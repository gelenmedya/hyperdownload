//! Every download the program holds: the HTTP ones (`hd-engine`) and the torrents (`hd-torrent`, torwatch's engine), each known
//! by a GID as aria2 names them (16 hex digits). The HTTP downloads are listed in `jobs.json` so that a program started again shows
//! the unfinished ones (paused) and the finished ones until they are cleared; the torrents keep their own session.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use hd_engine::{Download, Phase, Request, state_of};
use hd_torrent::engine::Engine;
use serde::{Deserialize, Serialize};

/// The high bits of a GID say what it names: a torrent by its session id, or a torrent still being added (its metadata coming) by
/// its key. An HTTP download's GID is a plain counter.
pub const TORRENT: u64 = 0x7470 << 48;
pub const ADDING: u64 = 0x6164 << 48;
const KIND: u64 = 0xffff << 48;

pub fn gid_text(gid: u64) -> String {
    format!("{gid:016x}")
}

pub fn gid_of(text: &str) -> Option<u64> {
    u64::from_str_radix(text.trim(), 16).ok()
}

/// What a GID names.
pub enum Named {
    Http(u64),
    Torrent(usize),
    Adding(u64),
}

pub fn named(gid: u64) -> Named {
    match gid & KIND {
        TORRENT => Named::Torrent((gid & !KIND) as usize),
        ADDING => Named::Adding(gid & !KIND),
        _ => Named::Http(gid),
    }
}

pub struct HttpJob {
    pub gid: u64,
    pub download: Download,
    pub dir: PathBuf,
}

/// A line of `jobs.json`.
#[derive(Serialize, Deserialize)]
struct Listed {
    gid: u64,
    url: String,
    dir: PathBuf,
    /// The finished file, or the file it will be.
    target: Option<PathBuf>,
    /// "paused", "complete", "error" or "removed"
    status: String,
    #[serde(default)]
    error: Option<String>,
}

pub struct Hub {
    pub token: String,
    pub data: PathBuf,
    /// Where a download goes when nothing else is said.
    pub downloads: PathBuf,
    pub torrent: Arc<Engine>,
    http: Mutex<Vec<HttpJob>>,
    /// HTTP downloads that ended in an earlier run (they have no `Download` any more): shown until cleared.
    ended: Mutex<Vec<EndedJob>>,
    next: AtomicU64,
}

/// A finished (or failed) HTTP download from an earlier run.
#[derive(Clone)]
pub struct EndedJob {
    pub gid: u64,
    pub url: String,
    pub dir: PathBuf,
    pub target: Option<PathBuf>,
    pub phase: Phase,
}

impl Hub {
    pub fn new(token: String, data: PathBuf, downloads: PathBuf, torrent: Arc<Engine>) -> Hub {
        let hub = Hub { token, data, downloads, torrent, http: Mutex::new(Vec::new()), ended: Mutex::new(Vec::new()), next: AtomicU64::new(1) };
        hub.restore();
        hub
    }

    fn jobs_file(&self) -> PathBuf {
        self.data.join("jobs.json")
    }

    /// The HTTP downloads of the last run: the unfinished ones paused (from their state beside the file), the rest as they ended.
    fn restore(&self) {
        let Ok(bytes) = std::fs::read(self.jobs_file()) else { return };
        let Ok(list) = serde_json::from_slice::<Vec<Listed>>(&bytes) else { return };
        let mut top = 0;
        for l in list {
            top = top.max(l.gid);
            let resumable = l.target.as_deref().map(state_of).filter(|s| s.exists());
            match (l.status.as_str(), resumable) {
                ("paused" | "active", Some(state)) => {
                    if let Ok(d) = Download::paused_from(&state, Request { dir: l.dir.clone(), ..Default::default() }) {
                        self.http.lock().unwrap().push(HttpJob { gid: l.gid, download: d, dir: l.dir });
                    }
                }
                ("complete", _) => self.ended.lock().unwrap().push(EndedJob { gid: l.gid, url: l.url, dir: l.dir, target: l.target.clone(), phase: Phase::Done(l.target.unwrap_or_default()) }),
                ("removed", _) => {}
                _ => self.ended.lock().unwrap().push(EndedJob {
                    gid: l.gid,
                    url: l.url,
                    dir: l.dir,
                    target: l.target,
                    phase: Phase::Failed(l.error.unwrap_or_else(|| "the download was not finished".into())),
                }),
            }
        }
        self.next.store(top + 1, Ordering::SeqCst);
    }

    /// Writes `jobs.json` (after any change, and now and then while downloads run).
    pub fn save(&self) {
        let mut list: Vec<Listed> = Vec::new();
        for j in self.http.lock().unwrap().iter() {
            let s = j.download.status();
            let (status, error) = match &s.phase {
                Phase::Done(_) => ("complete", None),
                Phase::Failed(e) => ("error", Some(e.clone())),
                Phase::Cancelled => ("removed", None),
                _ => ("paused", None),
            };
            list.push(Listed { gid: j.gid, url: j.download.url(), dir: j.dir.clone(), target: j.download.target(), status: status.into(), error });
        }
        for e in self.ended.lock().unwrap().iter() {
            let (status, error) = match &e.phase {
                Phase::Done(_) => ("complete", None),
                Phase::Failed(m) => ("error", Some(m.clone())),
                _ => ("removed", None),
            };
            list.push(Listed { gid: e.gid, url: e.url.clone(), dir: e.dir.clone(), target: e.target.clone(), status: status.into(), error });
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(&list) {
            let tmp = self.data.join("jobs.json.tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, self.jobs_file());
            }
        }
    }

    pub fn add_http(&self, req: Request) -> u64 {
        let gid = self.next.fetch_add(1, Ordering::SeqCst);
        let dir = req.dir.clone();
        let download = Download::start(req);
        self.http.lock().unwrap().push(HttpJob { gid, download, dir });
        self.save();
        gid
    }

    pub fn http(&self, gid: u64) -> Option<Download> {
        self.http.lock().unwrap().iter().find(|j| j.gid == gid).map(|j| j.download.clone())
    }

    pub fn http_dir(&self, gid: u64) -> Option<PathBuf> {
        self.http.lock().unwrap().iter().find(|j| j.gid == gid).map(|j| j.dir.clone())
    }

    pub fn http_jobs(&self) -> Vec<(u64, Download, PathBuf)> {
        self.http.lock().unwrap().iter().map(|j| (j.gid, j.download.clone(), j.dir.clone())).collect()
    }

    pub fn ended(&self) -> Vec<EndedJob> {
        self.ended.lock().unwrap().clone()
    }

    /// Forgets a download that has ended (aria2's removeDownloadResult). False when it is still going.
    pub fn forget(&self, gid: u64) -> bool {
        let mut http = self.http.lock().unwrap();
        if let Some(i) = http.iter().position(|j| j.gid == gid) {
            if matches!(http[i].download.status().phase, Phase::Done(_) | Phase::Failed(_) | Phase::Cancelled) {
                http.remove(i);
                drop(http);
                self.save();
                return true;
            }
            return false;
        }
        drop(http);
        let mut ended = self.ended.lock().unwrap();
        let before = ended.len();
        ended.retain(|e| e.gid != gid);
        let gone = ended.len() != before;
        drop(ended);
        if gone {
            self.save();
        }
        gone
    }
}

/// `%LOCALAPPDATA%\HyperDownload`, where the program keeps its lists, its torrent session and `rpc.json`.
pub fn default_data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("HyperDownload")
}

/// The user's Downloads folder.
pub fn default_downloads() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("Downloads")
}

pub fn ensure_dir(p: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(p)
}
