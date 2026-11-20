//! İzleme geçmişi ("kaldığın yerden devam"): `<data_dir>/history.json`. Film verisi kapatınca
//! silinir; burada yalnızca yeniden açmak için kaynak (magnet / .torrent) ve konum tutulur.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Geçmişte en çok tutulan giriş sayısı (en eskiler düşer).
const MAX_ENTRIES: usize = 200;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub info_hash: String,
    pub name: String,
    /// Magnet URI'si ya da base64 .torrent baytları.
    pub source: String,
    pub file: usize,
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
    pub audio_track: Option<u64>,
    pub subtitle_track: Option<u64>,
    pub updated_unix: u64,
}

/// Dosyayı atomik yazar: geçici dosya + yeniden adlandırma. Yazma başarısız olursa eski içerik
/// olduğu gibi kalır (yarım dosya bırakılmaz).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

pub struct History {
    path: PathBuf,
    entries: Mutex<Vec<HistoryEntry>>,
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl History {
    /// `dir/history.json` yüklenir; yoksa ya da bozuksa boş başlar.
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("history.json");
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { path, entries: Mutex::new(entries) }
    }

    fn save(&self, entries: &[HistoryEntry]) {
        let write = || -> anyhow::Result<()> {
            if let Some(p) = self.path.parent() {
                std::fs::create_dir_all(p)?;
            }
            // Yarım yazılmış dosya bırakmamak için geçici dosya + yeniden adlandırma.
            write_atomic(&self.path, &serde_json::to_vec_pretty(entries)?)?;
            Ok(())
        };
        if let Err(e) = write() {
            tracing::warn!("geçmiş yazılamadı: {e:#}");
        }
    }

    /// Aynı info hash'li girişi değiştirir (yoksa ekler).
    pub fn upsert(&self, entry: HistoryEntry) {
        let mut g = self.entries.lock().unwrap();
        g.retain(|e| e.info_hash != entry.info_hash);
        g.push(entry);
        if g.len() > MAX_ENTRIES {
            g.sort_by_key(|e| e.updated_unix);
            let extra = g.len() - MAX_ENTRIES;
            g.drain(..extra);
        }
        self.save(&g);
    }

    pub fn remove(&self, info_hash: &str) {
        let mut g = self.entries.lock().unwrap();
        let before = g.len();
        g.retain(|e| e.info_hash != info_hash);
        if g.len() != before {
            self.save(&g);
        }
    }

    pub fn get(&self, info_hash: &str) -> Option<HistoryEntry> {
        self.entries.lock().unwrap().iter().find(|e| e.info_hash == info_hash).cloned()
    }

    /// Yeniden eskiye.
    pub fn list(&self) -> Vec<HistoryEntry> {
        let mut v = self.entries.lock().unwrap().clone();
        v.reverse(); // eşit zamanda en son eklenen önce
        v.sort_by(|a, b| b.updated_unix.cmp(&a.updated_unix));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_replaces_existing_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sources.json");
        write_atomic(&p, b"old").unwrap();
        write_atomic(&p, b"new-content").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new-content");
        assert!(!d.path().join("sources.json.tmp").exists());
    }

    #[test]
    fn failed_write_leaves_old_content() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("paused.json");
        std::fs::write(&p, b"old").unwrap();
        // Geçici dosya yolunu klasör yaparak yazmayı başarısız kıl.
        std::fs::create_dir(d.path().join("paused.json.tmp")).unwrap();
        assert!(write_atomic(&p, b"new").is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"old");
    }
}
