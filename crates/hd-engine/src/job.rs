//! One download: a leader thread that looks at the file, makes the pieces and keeps the books, and a worker thread per connection that
//! fetches a piece and writes it in its place. A worker that has finished its piece takes half of the biggest piece left.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::net::{self, Fail};
use crate::pieces::{self, Piece, Saved};
use crate::{DEFAULT_CONNECTIONS, PART_EXT, Request, names};

/// Where a download is.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// Looking at the file (its size, its name).
    Starting,
    Running,
    Paused,
    Done(PathBuf),
    Failed(String),
    Cancelled,
}

/// A look at a download.
#[derive(Clone, Debug)]
pub struct Status {
    pub phase: Phase,
    /// The file's name, once known.
    pub name: Option<String>,
    pub total: Option<u64>,
    pub done: u64,
    /// Bytes a second, over the last seconds.
    pub speed: u64,
    pub connections: usize,
    /// Whether a pause keeps what was fetched (the server answers ranges and said the size).
    pub resumable: bool,
}

struct Book {
    phase: Phase,
    url: String,
    name: Option<String>,
    /// The finished file's path, chosen once the name is known.
    target: Option<PathBuf>,
    total: Option<u64>,
    ranges: bool,
    validator: Option<String>,
    pieces: Vec<Piece>,
    busy: Vec<bool>,
    /// The bytes of a download without ranges (one stream).
    streamed: u64,
    workers: usize,
    /// The last error a worker met, and how many tries in a row brought nothing.
    last_error: Option<String>,
    fruitless: u32,
    speed: u64,
}

struct Shared {
    req: Request,
    book: Mutex<Book>,
    changed: Condvar,
    pause: AtomicBool,
    cancel: AtomicBool,
}

/// A running (or paused, or finished) download. Clones steer the same download.
#[derive(Clone)]
pub struct Download {
    shared: Arc<Shared>,
}

/// After this many tries in a row that brought nothing the download fails.
const MAX_FRUITLESS: u32 = 6;
const BUF: usize = 256 * 1024;

impl Download {
    /// Starts a new download.
    pub fn start(req: Request) -> Download {
        let d = Download::new(req);
        d.spawn_leader(None);
        d
    }

    /// Goes on with a download from its `<file>.hdpart.json`: what was fetched is kept when the file on the server is still the same
    /// (its size and its `ETag` or date); else it starts again. `req.url` and `req.dir` may be left empty (the saved ones are used).
    pub fn continue_from(state: &Path, mut req: Request) -> std::io::Result<Download> {
        let saved: Saved = serde_json::from_slice(&std::fs::read(state)?).map_err(std::io::Error::other)?;
        req.url = saved.url.clone();
        if req.dir.as_os_str().is_empty() {
            req.dir = state.parent().map(Path::to_path_buf).unwrap_or_default();
        }
        let d = Download::new(req);
        d.spawn_leader(Some(saved));
        Ok(d)
    }

    /// A download from its `<file>.hdpart.json`, paused: it shows what it has and goes on at [`Download::resume`] (a program that
    /// starts again lists its unfinished downloads without fetching anything yet).
    pub fn paused_from(state: &Path, mut req: Request) -> std::io::Result<Download> {
        let saved: Saved = serde_json::from_slice(&std::fs::read(state)?).map_err(std::io::Error::other)?;
        req.url = saved.url.clone();
        if req.dir.as_os_str().is_empty() {
            req.dir = state.parent().map(Path::to_path_buf).unwrap_or_default();
        }
        let d = Download::new(req);
        {
            let mut b = d.shared.book.lock().unwrap();
            b.phase = Phase::Paused;
            b.name = Some(saved.name.clone());
            b.target = Some(d.shared.req.dir.join(&saved.name));
            b.total = saved.total;
            b.ranges = true;
            b.validator = saved.validator.clone();
            b.busy = vec![false; saved.pieces.len()];
            b.pieces = saved.pieces;
        }
        Ok(d)
    }

    /// The file it is (or will be) saved as, once known.
    pub fn target(&self) -> Option<PathBuf> {
        self.shared.book.lock().unwrap().target.clone()
    }

    pub fn url(&self) -> String {
        self.shared.book.lock().unwrap().url.clone()
    }

    fn new(req: Request) -> Download {
        let url = req.url.clone();
        Download { shared: Arc::new(Shared { req, book: Mutex::new(Book::new(url)), changed: Condvar::new(), pause: AtomicBool::new(false), cancel: AtomicBool::new(false) }) }
    }

    pub fn status(&self) -> Status {
        let b = self.shared.book.lock().unwrap();
        Status {
            phase: b.phase.clone(),
            name: b.name.clone(),
            total: b.total,
            done: b.done(),
            speed: if b.phase == Phase::Running { b.speed } else { 0 },
            connections: b.workers,
            resumable: b.ranges && b.total.is_some(),
        }
    }

    /// Stops the connections; what was fetched stays (when the server answers ranges). [`Download::resume`] goes on.
    pub fn pause(&self) {
        let b = self.shared.book.lock().unwrap();
        if matches!(b.phase, Phase::Running | Phase::Starting) {
            self.shared.pause.store(true, Ordering::SeqCst);
        }
    }

    /// Goes on after a pause, or tries again after a failure.
    pub fn resume(&self) {
        let saved = {
            let mut b = self.shared.book.lock().unwrap();
            if !matches!(b.phase, Phase::Paused | Phase::Failed(_)) {
                return;
            }
            b.phase = Phase::Starting;
            b.fruitless = 0;
            b.last_error = None;
            b.saved()
        };
        self.shared.pause.store(false, Ordering::SeqCst);
        self.spawn_leader(saved);
    }

    /// Stops it and removes the unfinished file.
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::SeqCst);
        let mut b = self.shared.book.lock().unwrap();
        // (a paused or failed download has no leader to clean up after it: done here)
        if matches!(b.phase, Phase::Paused | Phase::Failed(_)) {
            if let Some(t) = &b.target {
                remove_part(t);
            }
            b.phase = Phase::Cancelled;
            self.shared.changed.notify_all();
        }
    }

    /// Waits until it is no longer starting or running.
    pub fn wait(&self) -> Status {
        let mut b = self.shared.book.lock().unwrap();
        while matches!(b.phase, Phase::Starting | Phase::Running) {
            b = self.shared.changed.wait(b).unwrap();
        }
        drop(b);
        self.status()
    }

    fn spawn_leader(&self, saved: Option<Saved>) {
        let shared = self.shared.clone();
        thread::spawn(move || {
            let end = lead(&shared, saved);
            let mut b = shared.book.lock().unwrap();
            b.phase = end;
            b.workers = 0;
            shared.changed.notify_all();
        });
    }
}

impl Book {
    fn new(url: String) -> Book {
        Book {
            phase: Phase::Starting,
            url,
            name: None,
            target: None,
            total: None,
            ranges: false,
            validator: None,
            pieces: Vec::new(),
            busy: Vec::new(),
            streamed: 0,
            workers: 0,
            last_error: None,
            fruitless: 0,
            speed: 0,
        }
    }

    fn done(&self) -> u64 {
        if self.pieces.is_empty() { self.streamed } else { self.pieces.iter().map(|p| p.done).sum() }
    }

    /// What lets this download go on later (only one with ranges and a known size).
    fn saved(&self) -> Option<Saved> {
        (self.ranges && self.total.is_some() && !self.pieces.is_empty()).then(|| Saved {
            url: self.url.clone(),
            name: self.name.clone().unwrap_or_default(),
            total: self.total,
            pieces: self.pieces.clone(),
            validator: self.validator.clone(),
        })
    }
}

/// `<target>.hdpart`, the file while it downloads.
pub fn part_of(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".");
    s.push(PART_EXT);
    PathBuf::from(s)
}

/// `<target>.hdpart.json`, what lets it go on later.
pub fn state_of(target: &Path) -> PathBuf {
    let mut s = part_of(target).into_os_string();
    s.push(".json");
    PathBuf::from(s)
}

fn remove_part(target: &Path) {
    let _ = std::fs::remove_file(part_of(target));
    let _ = std::fs::remove_file(state_of(target));
}

fn write_state(target: &Path, saved: &Saved) {
    if let Ok(bytes) = serde_json::to_vec(saved) {
        let mut tmp = state_of(target).into_os_string();
        tmp.push(".tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, state_of(target));
        }
    }
}

#[cfg(windows)]
fn write_at(f: &File, mut buf: &[u8], mut at: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_write(buf, at)?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        at += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn write_at(f: &File, buf: &[u8], at: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, at)
}

fn connections(req: &Request) -> usize {
    if req.connections == 0 { DEFAULT_CONNECTIONS } else { req.connections }
}

/// The leader: everything from the first look to the finished file. Returns how it ended.
fn lead(shared: &Arc<Shared>, saved: Option<Saved>) -> Phase {
    let agent = net::agent();
    let req = &shared.req;
    let stopped = || shared.cancel.load(Ordering::SeqCst) || shared.pause.load(Ordering::SeqCst);

    // ---- the first look
    let probe = loop {
        let url = shared.book.lock().unwrap().url.clone();
        match net::probe(&agent, &Request { url, ..req.clone() }) {
            Ok(p) => break p,
            Err(f @ Fail::Refused(_)) => return Phase::Failed(f.to_string()),
            Err(Fail::Again(e)) => {
                let tries = {
                    let mut b = shared.book.lock().unwrap();
                    b.fruitless += 1;
                    b.last_error = Some(e.clone());
                    b.fruitless
                };
                if tries >= MAX_FRUITLESS {
                    return Phase::Failed(e);
                }
                if rest(shared, tries) {
                    return stop_phase(shared, None);
                }
            }
        }
    };

    // ---- the name, the file, the pieces
    let same = saved.as_ref().is_some_and(|s| probe.ranges && s.total == probe.total && (s.validator.is_none() || s.validator == probe.validator));
    let name = match (&saved, &req.name) {
        (Some(s), _) if same && !s.name.is_empty() => s.name.clone(),
        (_, Some(n)) => names::safe(n),
        _ => names::safe(probe.name.as_deref().unwrap_or("download")),
    };
    let target = if same { req.dir.join(&name) } else { names::free_path(&req.dir, &name, |p| p.exists() || part_of(p).exists()) };
    let file = match OpenOptions::new().create(true).write(true).truncate(!same).open(part_of(&target)) {
        Ok(f) => Arc::new(f),
        Err(e) => return Phase::Failed(format!("{}: {e}", part_of(&target).display())),
    };
    if let Some(total) = probe.total.filter(|_| probe.ranges) {
        // (the whole size at once: the pieces are written where they belong)
        let _ = file.set_len(total);
    }
    let pieces = match (&saved, probe.total) {
        (Some(s), _) if same => s.pieces.clone(),
        (_, Some(total)) if probe.ranges => pieces::plan(total, connections(req)),
        _ => Vec::new(),
    };
    {
        let mut b = shared.book.lock().unwrap();
        b.url = probe.url.clone();
        b.name = target.file_name().map(|n| n.to_string_lossy().into_owned());
        b.target = Some(target.clone());
        b.total = probe.total;
        b.ranges = probe.ranges;
        b.validator = probe.validator.clone();
        b.busy = vec![false; pieces.len()];
        b.pieces = pieces;
        b.streamed = 0;
        b.fruitless = 0;
        b.phase = Phase::Running;
        shared.changed.notify_all();
    }

    // ---- no ranges: the first answer is the download, in one stream
    if !probe.ranges {
        let Some(mut body) = probe.body else { return Phase::Failed("the server sent no file".into()) };
        shared.book.lock().unwrap().workers = 1;
        let mut buf = vec![0u8; BUF];
        let mut at = 0u64;
        let mut meter = Meter::new();
        loop {
            if stopped() {
                // (nothing can be kept without ranges: a resume starts it again)
                drop(file);
                return stop_phase(shared, Some(&target));
            }
            match body.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(e) = write_at(&file, &buf[..n], at) {
                        return Phase::Failed(e.to_string());
                    }
                    at += n as u64;
                    let mut b = shared.book.lock().unwrap();
                    b.streamed = at;
                    b.speed = meter.add(at);
                }
                Err(e) => {
                    drop(file);
                    remove_part(&target);
                    return Phase::Failed(format!("the connection was lost, and this server cannot go on from where it was: {e}"));
                }
            }
        }
        if probe.total.is_some_and(|t| t != at) {
            drop(file);
            remove_part(&target);
            return Phase::Failed("the file arrived shorter than the server said".into());
        }
        return finish(shared, file, &target);
    }
    drop(probe.body);

    // ---- ranges: the workers
    let count = shared.book.lock().unwrap().pieces.len();
    let mut handles = Vec::new();
    for _ in 0..connections(req).min(count.max(1)) {
        let (shared, file, agent) = (shared.clone(), file.clone(), agent.clone());
        handles.push(thread::spawn(move || work(&shared, &file, &agent)));
    }
    shared.book.lock().unwrap().workers = handles.len();
    // the books: the speed, the state on the disk now and then
    let mut meter = Meter::new();
    let mut saved_at = Instant::now();
    loop {
        let (finished, done, saved) = {
            let b = shared.book.lock().unwrap();
            let (mut b, _) = shared.changed.wait_timeout(b, Duration::from_millis(250)).unwrap();
            let done = b.done();
            b.speed = meter.add(done);
            (b.workers == 0 || b.pieces.iter().all(Piece::finished), done, b.saved())
        };
        let _ = done;
        if saved_at.elapsed() >= Duration::from_secs(2) {
            if let Some(s) = saved {
                write_state(&target, &s);
            }
            saved_at = Instant::now();
        }
        if finished {
            break;
        }
    }
    for h in handles {
        let _ = h.join();
    }
    let (finished, error, saved) = {
        let b = shared.book.lock().unwrap();
        (b.pieces.iter().all(Piece::finished), b.last_error.clone(), b.saved())
    };
    if finished {
        return finish(shared, file, &target);
    }
    if let Some(s) = &saved {
        write_state(&target, s);
    }
    drop(file);
    if stopped() {
        return stop_phase(shared, Some(&target));
    }
    Phase::Failed(error.unwrap_or_else(|| "the download stopped".into()))
}

/// Paused, or cancelled (a cancel removes the unfinished file).
fn stop_phase(shared: &Shared, target: Option<&Path>) -> Phase {
    if shared.cancel.load(Ordering::SeqCst) {
        if let Some(t) = target {
            remove_part(t);
        }
        Phase::Cancelled
    } else {
        Phase::Paused
    }
}

/// Waits before try `tries` (1, 2, 4, 8 s); true when a pause or a cancel came meanwhile.
fn rest(shared: &Shared, tries: u32) -> bool {
    let until = Instant::now() + Duration::from_secs(1 << tries.clamp(1, 4).saturating_sub(1));
    while Instant::now() < until {
        if shared.cancel.load(Ordering::SeqCst) || shared.pause.load(Ordering::SeqCst) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

/// The finished file gets its name (a free one, if the name was taken meanwhile); the state beside it goes.
fn finish(shared: &Shared, file: Arc<File>, target: &Path) -> Phase {
    let _ = file.sync_all();
    drop(file);
    // the checksum the caller knows: a file that is not it is not given its name
    if let Some(want) = &shared.req.sha256 {
        match crate::check::sha256_of(&part_of(target)) {
            Ok(got) if got.eq_ignore_ascii_case(want.trim()) => {}
            Ok(got) => {
                remove_part(target);
                return Phase::Failed(format!("the file arrived different from the expected one (SHA-256 {got}, expected {want})"));
            }
            Err(e) => return Phase::Failed(e.to_string()),
        }
    }
    let to = if target.exists() {
        let name = target.file_name().unwrap_or_default().to_string_lossy().into_owned();
        names::free_path(target.parent().unwrap_or(Path::new("")), &name, |p| p.exists())
    } else {
        target.to_path_buf()
    };
    match std::fs::rename(part_of(target), &to) {
        Ok(()) => {
            let _ = std::fs::remove_file(state_of(target));
            // marked as coming from the internet, as browsers mark their downloads (Defender and SmartScreen look at it)
            crate::check::mark_of_the_web(&to, &shared.req.url, shared.req.referer.as_deref());
            let mut b = shared.book.lock().unwrap();
            b.name = to.file_name().map(|n| n.to_string_lossy().into_owned());
            b.target = Some(to.clone());
            Phase::Done(to)
        }
        Err(e) => Phase::Failed(format!("{}: {e}", to.display())),
    }
}

/// A worker: takes a piece (a free one, else half of the biggest), fetches it, until nothing is left or it is told to stop.
fn work(shared: &Arc<Shared>, file: &File, agent: &ureq::Agent) {
    let stopped = || shared.cancel.load(Ordering::SeqCst) || shared.pause.load(Ordering::SeqCst);
    let mut buf = vec![0u8; BUF];
    'pieces: loop {
        if stopped() {
            break;
        }
        // ---- a piece
        let (ix, url, validator, from, to) = {
            let mut b = shared.book.lock().unwrap();
            if b.fruitless >= MAX_FRUITLESS {
                break;
            }
            let free = (0..b.pieces.len()).find(|&i| !b.busy[i] && !b.pieces[i].finished());
            let ix = match free {
                Some(i) => i,
                None => match pieces::split_biggest(&mut b.pieces) {
                    Some(i) => {
                        b.busy.push(false);
                        i
                    }
                    None => break,
                },
            };
            b.busy[ix] = true;
            (ix, b.url.clone(), b.validator.clone(), b.pieces[ix].next(), b.pieces[ix].end)
        };
        let release = || {
            let mut b = shared.book.lock().unwrap();
            b.busy[ix] = false;
            shared.changed.notify_all();
        };
        // ---- its bytes
        let mut body = match net::fetch(agent, &url, &shared.req, from, to, validator.as_deref()) {
            Ok(r) => r,
            Err(f) => {
                let tries = {
                    let mut b = shared.book.lock().unwrap();
                    b.last_error = Some(f.to_string());
                    b.fruitless = if matches!(f, Fail::Refused(_)) { MAX_FRUITLESS } else { b.fruitless + 1 };
                    b.fruitless
                };
                release();
                if tries >= MAX_FRUITLESS || rest(shared, tries) {
                    break;
                }
                continue;
            }
        };
        loop {
            if stopped() {
                release();
                break 'pieces;
            }
            // (the piece may have been shortened by a worker that took half of it)
            let (at, end) = {
                let b = shared.book.lock().unwrap();
                (b.pieces[ix].next(), b.pieces[ix].end)
            };
            if at >= end {
                break;
            }
            let room = usize::try_from(end - at).unwrap_or(usize::MAX).min(buf.len());
            match body.read(&mut buf[..room]) {
                // the server ended early: the rest is asked for again
                Ok(0) => break,
                Ok(n) => {
                    if let Err(e) = write_at(file, &buf[..n], at) {
                        let mut b = shared.book.lock().unwrap();
                        b.last_error = Some(e.to_string());
                        b.fruitless = MAX_FRUITLESS;
                        b.busy[ix] = false;
                        shared.changed.notify_all();
                        break 'pieces;
                    }
                    let mut b = shared.book.lock().unwrap();
                    let p = &mut b.pieces[ix];
                    // (the end may have moved while writing: count only up to it)
                    p.done += (n as u64).min(p.end.saturating_sub(p.next()));
                    b.fruitless = 0;
                }
                Err(e) => {
                    shared.book.lock().unwrap().last_error = Some(e.to_string());
                    break;
                }
            }
        }
        release();
    }
    let mut b = shared.book.lock().unwrap();
    b.workers = b.workers.saturating_sub(1);
    shared.changed.notify_all();
}

/// Bytes a second over the last few seconds.
struct Meter {
    samples: std::collections::VecDeque<(Instant, u64)>,
}

impl Meter {
    fn new() -> Meter {
        Meter { samples: std::collections::VecDeque::new() }
    }

    fn add(&mut self, total: u64) -> u64 {
        let now = Instant::now();
        self.samples.push_back((now, total));
        while self.samples.len() > 2 && now.duration_since(self.samples[0].0) > Duration::from_secs(4) {
            self.samples.pop_front();
        }
        let (t0, b0) = self.samples[0];
        let secs = now.duration_since(t0).as_secs_f64();
        if secs < 0.2 { 0 } else { (total.saturating_sub(b0) as f64 / secs) as u64 }
    }
}
