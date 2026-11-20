//! The engine against a small HTTP server of the test's own on 127.0.0.1: ranges or none, a lost connection, a pause and a resume
//! (also after a "restart"), a file changed on the server, a refusal.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hd_engine::{Download, Phase, Request, part_of, state_of};

/// What the test server does.
#[derive(Default)]
struct Rules {
    ranges: bool,
    /// Sent as the file's name.
    disposition: Option<String>,
    etag: Mutex<Option<String>>,
    /// Each answer stops after this many bytes, once per answer number below `cut_answers` (a lost connection).
    cut_after: Option<u64>,
    cut_answers: usize,
    /// Bytes a second for each answer (0: as fast as it goes).
    rate: u64,
    /// Answers with this status and nothing else.
    status: Option<u16>,
}

struct Server {
    port: u16,
    body: Arc<Vec<u8>>,
    rules: Arc<Rules>,
    /// Bytes of the file sent in all.
    sent: Arc<AtomicU64>,
    answers: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn body(len: usize) -> Vec<u8> {
    // a pattern that is not the same at any two nearby places: a byte written at a wrong offset shows
    let mut x: u32 = 2_463_534_242;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 24) as u8
        })
        .collect()
}

fn serve(len: usize, rules: Rules) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let s = Server { port, body: Arc::new(body(len)), rules: Arc::new(rules), sent: Arc::default(), answers: Arc::default(), stop: Arc::default() };
    let (body, rules, sent, answers, stop) = (s.body.clone(), s.rules.clone(), s.sent.clone(), s.answers.clone(), s.stop.clone());
    thread::spawn(move || {
        for conn in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(conn) = conn else { continue };
            let (body, rules, sent, answers) = (body.clone(), rules.clone(), sent.clone(), answers.clone());
            thread::spawn(move || answer(conn, &body, &rules, &sent, &answers));
        }
    });
    s
}

fn answer(conn: TcpStream, body: &[u8], rules: &Rules, sent: &AtomicU64, answers: &AtomicUsize) {
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut range: Option<(u64, Option<u64>)> = None;
    let mut if_range: Option<String> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            let v = v.trim();
            match k.trim().to_ascii_lowercase().as_str() {
                "range" => {
                    if let Some((a, b)) = v.strip_prefix("bytes=").and_then(|r| r.split_once('-')) {
                        range = Some((a.parse().unwrap(), if b.is_empty() { None } else { Some(b.parse().unwrap()) }));
                    }
                }
                "if-range" => if_range = Some(v.to_string()),
                _ => {}
            }
        }
    }
    let n = answers.fetch_add(1, Ordering::SeqCst);
    let mut out = conn;
    if let Some(code) = rules.status {
        let _ = write!(out, "HTTP/1.1 {code} No\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    let etag = rules.etag.lock().unwrap().clone();
    let len = body.len() as u64;
    // a range only when ranges are done and the file is the one the client started (If-Range)
    let honoured = rules.ranges && range.is_some() && (if_range.is_none() || if_range == etag);
    let (status, from, to) = match range {
        Some((a, b)) if honoured => (206, a, b.map_or(len - 1, |b| b.min(len - 1))),
        _ => (200, 0, len - 1),
    };
    let mut head = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n", if status == 206 { "206" } else { "200" }, to + 1 - from);
    if status == 206 {
        head.push_str(&format!("Content-Range: bytes {from}-{to}/{len}\r\n"));
    }
    if rules.ranges {
        head.push_str("Accept-Ranges: bytes\r\n");
    }
    if let Some(e) = &etag {
        head.push_str(&format!("ETag: {e}\r\n"));
    }
    if let Some(d) = &rules.disposition {
        head.push_str(&format!("Content-Disposition: {d}\r\n"));
    }
    head.push_str("\r\n");
    if out.write_all(head.as_bytes()).is_err() {
        return;
    }
    let mut at = from;
    let started = Instant::now();
    let limit = if n < rules.cut_answers { rules.cut_after } else { None };
    while at <= to {
        let chunk_end = (at + 16 * 1024).min(to + 1);
        let chunk_end = match limit {
            Some(l) if chunk_end - from > l => from + l,
            _ => chunk_end,
        };
        if chunk_end <= at {
            // the connection is "lost"
            return;
        }
        if out.write_all(&body[at as usize..chunk_end as usize]).is_err() {
            return;
        }
        sent.fetch_add(chunk_end - at, Ordering::SeqCst);
        at = chunk_end;
        if rules.rate > 0 {
            let due = Duration::from_secs_f64((at - from) as f64 / rules.rate as f64);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                thread::sleep(wait);
            }
        }
    }
    let _ = out.flush();
}

fn folder(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join("hd-engine-tests").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn url(s: &Server, path: &str) -> String {
    format!("http://127.0.0.1:{}/{path}", s.port)
}

fn done_path(d: &Download) -> PathBuf {
    match d.wait().phase {
        Phase::Done(p) => p,
        other => panic!("not done: {other:?}"),
    }
}

const MB: usize = 1 << 20;

#[test]
fn a_file_comes_in_pieces_through_several_connections_and_takes_the_servers_name() {
    let s = serve(9 * MB + 123, Rules { ranges: true, disposition: Some(r#"attachment; filename="büyük dosya.bin""#.into()), ..Default::default() });
    let dir = folder("ranges");
    let d = Download::start(Request { url: url(&s, "file?x=1"), dir: dir.clone(), connections: 4, ..Default::default() });
    let p = done_path(&d);
    assert_eq!(p, dir.join("büyük dosya.bin"));
    assert_eq!(std::fs::read(&p).unwrap(), *s.body);
    assert!(s.answers.load(Ordering::SeqCst) >= 5, "the first look and at least four pieces");
    assert!(!part_of(&p).exists() && !state_of(&p).exists(), "nothing left beside it");
}

#[test]
fn a_server_without_ranges_sends_the_file_in_one_stream() {
    let s = serve(3 * MB + 7, Rules::default());
    let dir = folder("noranges");
    let d = Download::start(Request { url: url(&s, "dir/plain%20name.zip"), dir: dir.clone(), ..Default::default() });
    let p = done_path(&d);
    assert_eq!(p, dir.join("plain name.zip"), "the name from the URL");
    assert_eq!(std::fs::read(&p).unwrap(), *s.body);
    assert_eq!(s.answers.load(Ordering::SeqCst), 1, "the first answer was the download");
    assert!(!d.status().resumable);
}

#[test]
fn lost_connections_are_asked_for_again_from_where_they_stopped() {
    // the first six answers (the look and the pieces) break after 300 KB
    let s = serve(6 * MB, Rules { ranges: true, cut_after: Some(300 * 1024), cut_answers: 6, ..Default::default() });
    let dir = folder("lost");
    let d = Download::start(Request { url: url(&s, "lost.bin"), dir: dir.clone(), connections: 4, ..Default::default() });
    let p = done_path(&d);
    assert_eq!(std::fs::read(&p).unwrap(), *s.body);
    // nothing fetched twice beyond the broken answers' own bytes
    assert!(s.sent.load(Ordering::SeqCst) < 6 * MB as u64 + 6 * 300 * 1024 + MB as u64, "sent {}", s.sent.load(Ordering::SeqCst));
}

#[test]
fn a_pause_keeps_what_came_and_a_restart_goes_on_from_it() {
    // 8 MB at 4 MB/s a connection: long enough to pause in the middle
    let s = serve(8 * MB, Rules { ranges: true, etag: Mutex::new(Some("\"v1\"".into())), rate: 4 * MB as u64, ..Default::default() });
    let dir = folder("pause");
    let d = Download::start(Request { url: url(&s, "big.bin"), dir: dir.clone(), connections: 2, ..Default::default() });
    let started = Instant::now();
    while d.status().done < 2 * MB as u64 {
        assert!(started.elapsed() < Duration::from_secs(20), "no progress");
        thread::sleep(Duration::from_millis(20));
    }
    d.pause();
    let st = d.wait();
    assert_eq!(st.phase, Phase::Paused);
    assert!(st.resumable);
    let target = dir.join("big.bin");
    assert!(part_of(&target).exists() && state_of(&target).exists(), "the unfinished file and its state are kept");
    let kept = st.done;
    let sent_before = s.sent.load(Ordering::SeqCst);
    // "a restart": a new download from the state on the disk
    let again = Download::continue_from(&state_of(&target), Request::default()).unwrap();
    let p = done_path(&again);
    assert_eq!(p, target);
    assert_eq!(std::fs::read(&p).unwrap(), *s.body);
    let sent_after = s.sent.load(Ordering::SeqCst) - sent_before;
    assert!(sent_after <= 8 * MB as u64 - kept + 2 * 256 * 1024, "only the rest was fetched: {sent_after} after {kept}");
}

#[test]
fn a_file_changed_on_the_server_meanwhile_starts_again() {
    let s = serve(6 * MB, Rules { ranges: true, etag: Mutex::new(Some("\"v1\"".into())), rate: 4 * MB as u64, ..Default::default() });
    let dir = folder("changed");
    let d = Download::start(Request { url: url(&s, "c.bin"), dir: dir.clone(), connections: 2, ..Default::default() });
    while d.status().done < MB as u64 {
        thread::sleep(Duration::from_millis(20));
    }
    d.pause();
    d.wait();
    *s.rules.etag.lock().unwrap() = Some("\"v2\"".into());
    let target = dir.join("c.bin");
    let again = Download::continue_from(&state_of(&target), Request::default()).unwrap();
    let p = done_path(&again);
    assert_eq!(std::fs::read(&p).unwrap(), *s.body);
}

#[test]
fn a_resume_in_the_same_run_and_a_cancel_that_removes_the_unfinished_file() {
    let s = serve(6 * MB, Rules { ranges: true, rate: 4 * MB as u64, ..Default::default() });
    let dir = folder("resume");
    let d = Download::start(Request { url: url(&s, "r.bin"), dir: dir.clone(), connections: 2, ..Default::default() });
    while d.status().done < MB as u64 {
        thread::sleep(Duration::from_millis(20));
    }
    d.pause();
    assert_eq!(d.wait().phase, Phase::Paused);
    d.resume();
    let p = done_path(&d);
    assert_eq!(std::fs::read(&p).unwrap(), *s.body);

    let d = Download::start(Request { url: url(&s, "r.bin"), dir: dir.clone(), connections: 2, ..Default::default() });
    while d.status().done < MB as u64 {
        thread::sleep(Duration::from_millis(20));
    }
    d.cancel();
    assert_eq!(d.wait().phase, Phase::Cancelled);
    let second = dir.join("r (2).bin");
    assert!(!part_of(&second).exists() && !state_of(&second).exists(), "the cancelled download's files are gone");
}

#[test]
fn a_refusal_fails_at_once_and_a_taken_name_gets_a_number() {
    let s = serve(10, Rules { status: Some(404), ..Default::default() });
    let dir = folder("refused");
    let d = Download::start(Request { url: url(&s, "missing.bin"), dir: dir.clone(), ..Default::default() });
    let started = Instant::now();
    let st = d.wait();
    assert!(matches!(st.phase, Phase::Failed(ref e) if e.contains("404")), "{:?}", st.phase);
    assert!(started.elapsed() < Duration::from_secs(2), "no retries for a refusal");

    let s = serve(1000, Rules { ranges: true, ..Default::default() });
    std::fs::write(dir.join("same.bin"), b"old").unwrap();
    let d = Download::start(Request { url: url(&s, "same.bin"), dir: dir.clone(), ..Default::default() });
    assert_eq!(done_path(&d), dir.join("same (2).bin"));
    assert_eq!(std::fs::read(dir.join("same.bin")).unwrap(), b"old");
}

#[test]
fn a_program_started_again_lists_an_unfinished_download_paused_and_it_goes_on_when_asked() {
    let s = serve(6 * MB, Rules { ranges: true, rate: 4 * MB as u64, ..Default::default() });
    let dir = folder("listed");
    let d = Download::start(Request { url: url(&s, "l.bin"), dir: dir.clone(), connections: 2, ..Default::default() });
    while d.status().done < MB as u64 {
        thread::sleep(Duration::from_millis(20));
    }
    d.pause();
    let kept = d.wait().done;
    let target = dir.join("l.bin");
    let listed = Download::paused_from(&state_of(&target), Request::default()).unwrap();
    let st = listed.status();
    assert_eq!(st.phase, Phase::Paused);
    assert_eq!(st.done, kept, "it shows what it has");
    assert_eq!(listed.target(), Some(target.clone()));
    thread::sleep(Duration::from_millis(300));
    assert_eq!(listed.status().done, kept, "nothing is fetched before a resume");
    listed.resume();
    assert_eq!(done_path(&listed), target);
    assert_eq!(std::fs::read(&target).unwrap(), *s.body);
}
