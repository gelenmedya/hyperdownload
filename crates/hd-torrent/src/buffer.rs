//! Akıllı tamponlama.
//!
//! librqbit her akış için konumundan itibaren sabit 32 MB'ı önceliklendirir; 4K bir dosyada bu
//! birkaç saniyedir. Burada oynatma konumundan sonraki ilk eksik parçaya ("cephe") ayrı akışlar
//! konumlanır, böylece öncelik penceresi cepheyle birlikte ileri kayar. Pencerenin ne kadar ileri
//! uzanacağı indirme hızının bit hızına oranından ve eş sayısındaki düşüşten hesaplanır.
//! İstemci aynı uç noktadan "başlamak / sürdürmek için ne kadar tampon gerekli" bilgisini alır.

use std::collections::{HashMap, VecDeque};
use std::io::SeekFrom;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use librqbit::api::TorrentIdOrHash;
use librqbit::{Api, ManagedTorrent};
use serde::Serialize;
use tokio::io::AsyncSeekExt;

use crate::demux::TimeIndex;
use crate::engine::StorageMode;
use crate::window_policy::{WINDOW_AHEAD_SECS, WindowCtl};
use crate::window_store::{WindowRegistry, WindowStore};

/// Önden okuma akışı başına librqbit'in önceliklendirdiği bayt (PER_STREAM_BUF_DEFAULT).
const STREAM_WINDOW: u64 = 32 * 1024 * 1024;
/// Her akış bir engelleme izni tutar; çalışma zamanını aç bırakmamak için üst sınır.
const MAX_PREFETCH_STREAMS: usize = 2;
const TICK: Duration = Duration::from_millis(500);
const IDLE_STOP: Duration = Duration::from_secs(45);
const MAP_BUCKETS: usize = 160;

#[derive(Clone, Copy)]
struct Playhead {
    pos: u64,
    /// Oynatma konumu (sn); pencere politikası bunu kullanır.
    secs: f64,
    bitrate: f64,
    updated: Instant,
}

/// İndirme hızının iki üstel ortalaması ve eş geçmişi (yalnızca önden okuma görevi yazar).
#[derive(Default)]
struct Rate {
    fast: f64,
    slow: f64,
    samples: u32,
    peers: VecDeque<(Instant, u32)>,
}

impl Rate {
    fn push(&mut self, bytes_per_sec: f64, peers: u32) {
        if self.samples == 0 {
            self.fast = bytes_per_sec;
            self.slow = bytes_per_sec;
        } else {
            self.fast += 0.30 * (bytes_per_sec - self.fast);
            self.slow += 0.06 * (bytes_per_sec - self.slow);
        }
        self.samples += 1;
        let now = Instant::now();
        self.peers.push_back((now, peers));
        while self
            .peers
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(30))
        {
            self.peers.pop_front();
        }
    }

    /// Temkinli hız: hız düşerken hızlı ortalama, dalgalanırken yavaş ortalama belirler.
    fn effective(&self) -> f64 {
        if self.samples < 3 {
            return self.fast * 0.7;
        }
        self.fast.min(self.slow) * 0.9
    }

    /// Son 30 sn içinde eş sayısı en yüksek değerin %70'inin altına düştü mü?
    fn peers_dropping(&self) -> bool {
        let max = self.peers.iter().map(|(_, p)| *p).max().unwrap_or(0);
        let cur = self.peers.back().map(|(_, p)| *p).unwrap_or(0);
        max >= 4 && (cur as f64) < max as f64 * 0.7
    }
}

struct Entry {
    playhead: Playhead,
    rate: Rate,
    task_alive: bool,
    /// Torrent kaldırıldı: önden okuma akışları hemen kapanır.
    cancelled: bool,
    /// Pencere kipi: video zaman dizini (yüklenene dek `None`).
    index: Option<Arc<TimeIndex>>,
    /// Pencere denetleyicisi: önden okuma görevi ve `ensure_range` paylaşır.
    ctl: Arc<Mutex<WindowCtl>>,
    /// Hafif pencere bakımı bu girişi oluşturdu/sahiplendi: önden okuma bitse de giriş yaşar
    /// (torrent kaldırılana dek), böylece oynatma aynı `WindowCtl` ile kesintisiz sürer.
    maintained: bool,
    /// Son bakım tick'i seçimi başarıyla uyguladı (başarısızlıkta false).
    applied: bool,
    last_maint: Option<Instant>,
}

type Shared = Arc<Mutex<Entry>>;

#[derive(Default)]
pub struct Buffering {
    entries: Mutex<HashMap<(usize, usize), Shared>>,
    /// `Some`: `StorageMode::Window` (iOS). `None`: `StorageMode::Disk`, davranış değişmez.
    window: Option<Arc<WindowRegistry>>,
}

#[derive(Serialize)]
pub struct BufferReport {
    pub file_len: u64,
    pub pos: u64,
    /// Konumdan itibaren kesintisiz inmiş bayt ve bunun süre karşılığı.
    pub ahead_bytes: u64,
    pub ahead_secs: f64,
    /// Başlamak/sürdürmek için gereken tampon (sn) ve bunun tahmini bekleme süresi.
    pub need_secs: f64,
    pub eta_secs: Option<f64>,
    pub ready: bool,
    pub complete: bool,
    pub rate: f64,
    pub bitrate: f64,
    pub ratio: f64,
    pub window_secs: f64,
    pub peers: u32,
    pub peers_dropping: bool,
    /// Pencere kipinde bu torrent'in diskte kapladığı bayt (Disk kipinde 0).
    pub window_bytes: u64,
    /// Dosya boyunca indirilmiş oran (0-255), arama çubuğu için.
    pub map: Vec<u8>,
}

/// Dosyanın parça düzeni ve sahip olunan parçalar.
pub(crate) struct Layout {
    offset: u64,
    pub(crate) len: u64,
    piece_len: u64,
    have: Vec<bool>,
    first_piece: u64,
}

impl Layout {
    pub(crate) fn read(session: &Arc<librqbit::Session>, h: &ManagedTorrent, id: usize, file: usize) -> anyhow::Result<Self> {
        let meta = h.metadata.load_full().ok_or_else(|| anyhow::anyhow!("meta veri yok"))?;
        let fi = meta
            .file_infos
            .get(file)
            .ok_or_else(|| anyhow::anyhow!("dosya yok"))?;
        let piece_len = meta.info.lengths().default_piece_length() as u64;
        let (offset, len) = (fi.offset_in_torrent, fi.len);
        let first_piece = offset / piece_len;
        let last_piece = (offset + len).div_ceil(piece_len).max(first_piece + 1);
        let have = match Api::new(session.clone(), None).api_dump_haves(TorrentIdOrHash::Id(id)) {
            Ok((bf, _)) => (first_piece..last_piece)
                .map(|p| bf.get(p as usize).map(|b| *b).unwrap_or(false))
                .collect(),
            // Duraklatılmış/başlatılıyor: tamamlanmışsa hepsi var say.
            Err(_) => vec![h.stats().finished; (last_piece - first_piece) as usize],
        };
        Ok(Self { offset, len, piece_len, have, first_piece })
    }

    /// Dosya içi `[a, b)` aralığının tüm parçaları indirilmiş mi.
    pub(crate) fn has_range(&self, a: u64, b: u64) -> bool {
        let mut pos = a;
        while pos < b.min(self.len) {
            if !self.has_piece_at(pos) {
                return false;
            }
            let abs_piece = (self.offset + pos) / self.piece_len;
            pos = (abs_piece + 1) * self.piece_len - self.offset;
        }
        true
    }

    fn has_piece_at(&self, file_pos: u64) -> bool {
        let p = (self.offset + file_pos) / self.piece_len - self.first_piece;
        self.have.get(p as usize).copied().unwrap_or(false)
    }

    /// file_pos'tan sonraki ilk eksik baytın dosya içi konumu (yoksa dosya sonu).
    fn frontier(&self, file_pos: u64) -> u64 {
        let mut pos = file_pos.min(self.len);
        while pos < self.len && self.has_piece_at(pos) {
            let abs_piece = (self.offset + pos) / self.piece_len;
            pos = ((abs_piece + 1) * self.piece_len - self.offset).min(self.len);
        }
        pos
    }

    /// pos'tan sonra eksik ilk bayt; yoksa None. (Cephe ötesindeki ikinci boşluk için.)
    fn next_missing_after(&self, pos: u64) -> Option<u64> {
        let mut p = pos;
        while p < self.len {
            if !self.has_piece_at(p) {
                return Some(p);
            }
            let abs_piece = (self.offset + p) / self.piece_len;
            p = ((abs_piece + 1) * self.piece_len - self.offset).min(self.len);
        }
        None
    }

    fn map(&self) -> Vec<u8> {
        let n = self.have.len().max(1);
        (0..MAP_BUCKETS)
            .map(|b| {
                let a = b * n / MAP_BUCKETS;
                let z = ((b + 1) * n / MAP_BUCKETS).max(a + 1).min(n);
                let got = self.have[a..z].iter().filter(|x| **x).count();
                (got * 255 / (z - a)) as u8
            })
            .collect()
    }
}

/// Hız/bit hızı oranına göre önden okuma hedefi (sn). Hız yetmiyorsa dosyanın tamamı.
fn window_secs(ratio: f64, dropping: bool, remaining_secs: f64) -> f64 {
    let base = if ratio >= 3.0 {
        45.0
    } else if ratio >= 1.8 {
        90.0
    } else if ratio >= 1.2 {
        180.0
    } else if ratio >= 1.0 {
        420.0
    } else {
        remaining_secs
    };
    let w = if dropping { base * 1.6 } else { base };
    w.min(remaining_secs.max(1.0))
}

/// Takılmadan oynatmak için gereken tampon (sn). Hız bit hızının altındaysa açığı kapatacak kadar.
fn need_secs(ratio: f64, remaining_secs: f64, stalls: u32, dropping: bool, mode: StorageMode) -> f64 {
    let base: f64 = if ratio >= 1.25 {
        5.0
    } else if ratio >= 1.0 {
        12.0
    } else {
        // Kalan süre boyunca biriken açık: remaining * (1 - ratio), artı güvenlik payı.
        remaining_secs * (1.0 - ratio.max(0.0)) * 1.1 + 10.0
    };
    let factor = 1.0 + 0.6 * stalls.min(5) as f64 + if dropping { 0.5 } else { 0.0 };
    let need = (base * factor).min(remaining_secs.max(0.0));
    match mode {
        StorageMode::Disk => need,
        // Pencere ileriye yalnızca WINDOW_AHEAD_SECS kadar tutulur; daha fazla tampon beklenemez.
        StorageMode::Window => need.min(WINDOW_AHEAD_SECS - 20.0),
    }
}

impl Buffering {
    /// `window`: pencere kipinde torrent depolarının kaydı (`Engine.window`); Disk kipinde `None`.
    pub fn new(window: Option<Arc<WindowRegistry>>) -> Self {
        Self { entries: Mutex::default(), window }
    }

    /// Kaldırılan torrent'in önden okuma görevlerini durdurur (açık akışlarını bırakır).
    pub fn stop_torrent(&self, id: usize) {
        self.entries.lock().unwrap().retain(|(tid, _), e| {
            if *tid == id {
                e.lock().unwrap().cancelled = true;
                false
            } else {
                true
            }
        });
    }

    /// İstemcinin oynatma konumunu kaydeder, önden okumayı (gerekirse) başlatır ve durum döner.
    pub fn report(
        self: &Arc<Self>,
        session: &Arc<librqbit::Session>,
        h: Arc<ManagedTorrent>,
        id: usize,
        file: usize,
        t: f64,
        duration: Option<f64>,
        bitrate: Option<f64>,
        stalls: u32,
    ) -> anyhow::Result<BufferReport> {
        let layout = Layout::read(session, &h, id, file)?;
        let wstore = self.window_store(&h);
        let mode = if wstore.is_some() { StorageMode::Window } else { StorageMode::Disk };
        let len = layout.len.max(1);
        let bitrate = bitrate
            .filter(|b| *b > 1000.0)
            .or_else(|| duration.filter(|d| *d > 1.0).map(|d| len as f64 / d))
            .unwrap_or(len as f64 / 5400.0);
        let mut pos = match duration {
            Some(d) if d > 0.0 => ((t / d).clamp(0.0, 1.0) * len as f64) as u64,
            _ => (t * bitrate) as u64,
        }
        .min(len);

        let shared = {
            let mut map = self.entries.lock().unwrap();
            map.retain(|_, e| { let g = e.lock().unwrap(); g.task_alive || g.maintained });
            map.entry((id, file))
                .or_insert_with(|| {
                    Arc::new(Mutex::new(Entry {
                        playhead: Playhead { pos, secs: t, bitrate, updated: Instant::now() },
                        rate: Rate::default(),
                        task_alive: false,
                        cancelled: false,
                        index: None,
                        ctl: Arc::new(Mutex::new(WindowCtl::new())),
                        maintained: false,
                        applied: false,
                        last_maint: None,
                    }))
                })
                .clone()
        };

        let (rate, dropping, spawn) = {
            let mut e = shared.lock().unwrap();
            // Pencere kipinde konum zaman dizininden (varsa) bayta çevrilir.
            if let Some(idx) = e.index.as_ref().filter(|_| mode == StorageMode::Window) {
                pos = idx.byte_at(t, len, bitrate);
            }
            e.playhead = Playhead { pos, secs: t, bitrate, updated: Instant::now() };
            let spawn = !e.task_alive;
            e.task_alive = true;
            (e.rate.effective(), e.rate.peers_dropping(), spawn)
        };
        if spawn {
            tokio::spawn(prefetch_task(shared.clone(), h.clone(), file, layout.len, session.clone(), id, wstore.clone()));
        }

        Ok(self.build(&h, &layout, wstore.as_ref(), mode, pos, bitrate, rate, dropping, stalls))
    }

    /// Salt okunur durum: oynatma konumunu KAYDETMEZ, önden okuma BAŞLATMAZ. Ana ekran kartları
    /// bunu yoklar; kimsenin izlemediği torrent için okuma-ileri başlamamalıdır.
    pub fn peek(
        &self,
        session: &Arc<librqbit::Session>,
        h: Arc<ManagedTorrent>,
        id: usize,
        file: usize,
        t: f64,
        duration: Option<f64>,
        bitrate: Option<f64>,
        stalls: u32,
    ) -> anyhow::Result<BufferReport> {
        let layout = Layout::read(session, &h, id, file)?;
        let wstore = self.window_store(&h);
        let mode = if wstore.is_some() { StorageMode::Window } else { StorageMode::Disk };
        let len = layout.len.max(1);
        let existing = self.entries.lock().unwrap().get(&(id, file)).cloned();
        let bitrate = bitrate
            .filter(|b| *b > 1000.0)
            .or_else(|| duration.filter(|d| *d > 1.0).map(|d| len as f64 / d))
            .unwrap_or(len as f64 / 5400.0);
        let mut pos = match duration {
            Some(d) if d > 0.0 => ((t / d).clamp(0.0, 1.0) * len as f64) as u64,
            _ => (t * bitrate) as u64,
        }
        .min(len);
        let (rate, dropping) = match &existing {
            Some(s) => {
                let e = s.lock().unwrap();
                if let Some(idx) = e.index.as_ref().filter(|_| mode == StorageMode::Window) {
                    pos = idx.byte_at(t, len, bitrate);
                }
                (e.rate.effective(), e.rate.peers_dropping())
            }
            None => (0.0, false),
        };
        Ok(self.build(&h, &layout, wstore.as_ref(), mode, pos, bitrate, rate, dropping, stalls))
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        &self,
        h: &Arc<ManagedTorrent>,
        layout: &Layout,
        wstore: Option<&Arc<WindowStore>>,
        mode: StorageMode,
        pos: u64,
        bitrate: f64,
        rate: f64,
        dropping: bool,
        stalls: u32,
    ) -> BufferReport {
        let stats = h.stats();
        let live_rate = stats
            .live
            .as_ref()
            .map(|l| l.download_speed.mbps * 1024.0 * 1024.0)
            .unwrap_or(0.0);
        let rate = if rate > 0.0 { rate } else { live_rate * 0.7 };
        let peers = stats.live.as_ref().map(|l| l.snapshot.peer_stats.live).unwrap_or(0);

        let frontier = layout.frontier(pos);
        let ahead_bytes = frontier - pos;
        let complete = frontier >= layout.len;
        let ahead_secs = ahead_bytes as f64 / bitrate;
        let remaining_secs = (layout.len - pos) as f64 / bitrate;
        let ratio = rate / bitrate;
        let need = need_secs(ratio, remaining_secs, stalls, dropping, mode);
        let ready = complete || ahead_secs >= need;
        let eta_secs = if ready {
            Some(0.0)
        } else if rate > 1.0 {
            // Tampon oynatma dururken (rate) dolar.
            Some((need - ahead_secs).max(0.0) * bitrate / rate)
        } else {
            None
        };

        BufferReport {
            file_len: layout.len,
            pos,
            ahead_bytes,
            ahead_secs,
            need_secs: need,
            eta_secs,
            ready,
            complete,
            rate,
            bitrate,
            ratio,
            window_secs: window_secs(ratio, dropping, remaining_secs),
            peers,
            peers_dropping: dropping,
            window_bytes: wstore.map(|s| s.bytes_on_disk()).unwrap_or(0),
            map: layout.map(),
        }
    }
}

/// Hafif pencere bakımının tick aralığı (oynatma yokken).
const MAINT_EVERY: Duration = Duration::from_secs(2);

impl Buffering {
    /// Bu (torrent, dosya) için önden okuma görevi çalışıyor mu (yani oynatma etkin mi)?
    pub fn is_active(&self, id: usize, file: usize) -> bool {
        let e = self.entries.lock().unwrap().get(&(id, file)).cloned();
        e.is_some_and(|e| e.lock().unwrap().task_alive)
    }

    /// Hafif pencere bakımı (yalnızca `StorageMode::Window`; Disk kipinde no-op).
    ///
    /// Window kipinde eklenen torrent'te librqbit, biri seçim daraltana dek video dosyasının tamamını
    /// indirir. Bu bakım seçimi hemen daraltır: yalnızca meta aralıkları (başlık + Cues/moov) ve
    /// `resume_secs` çevresindeki pencere. Önden okuma akışı AÇMAZ. Oynatma konum bildirince
    /// (`report`) giriş aynı `WindowCtl` ile olağan yola devredilir; önden okuma çalışırken bakım
    /// bir şey yapmaz. `resume_secs`/`duration` yalnızca giriş ilk kez oluşurken kullanılır.
    pub async fn maintain(
        self: &Arc<Self>,
        h: Arc<ManagedTorrent>,
        id: usize,
        file: usize,
        resume_secs: f64,
        duration: Option<f64>,
    ) -> anyhow::Result<bool> {
        // Pencere kipinde depo henüz yoksa uygulanmış sayma: Pending (tekrar denenecek).
        let Some(store) = self.window_store(&h) else {
            return Ok(false);
        };
        // Aynı torrent'in başka bir dosyası oynatılıyorsa onun penceresi geçerlidir; seçimi ezme.
        let playing_other = {
            let map = self.entries.lock().unwrap();
            map.iter().any(|((tid, _), e)| *tid == id && e.lock().unwrap().task_alive)
        };
        if playing_other {
            return Ok(true);
        }
        let meta = h.metadata.load_full().ok_or_else(|| anyhow::anyhow!("meta veri yok"))?;
        let len = meta.file_infos.get(file).ok_or_else(|| anyhow::anyhow!("dosya yok"))?.len.max(1);
        let shared = {
            let mut map = self.entries.lock().unwrap();
            map.entry((id, file))
                .or_insert_with(|| {
                    let bitrate = duration
                        .filter(|d| *d > 1.0)
                        .map(|d| len as f64 / d)
                        .unwrap_or(len as f64 / 5400.0);
                    let secs = resume_secs.max(0.0);
                    Arc::new(Mutex::new(Entry {
                        playhead: Playhead { pos: ((secs * bitrate) as u64).min(len), secs, bitrate, updated: Instant::now() },
                        rate: Rate::default(),
                        task_alive: false,
                        cancelled: false,
                        index: None,
                        ctl: Arc::new(Mutex::new(WindowCtl::new())),
                        maintained: true,
                        applied: false,
                        last_maint: None,
                    }))
                })
                .clone()
        };
        let (secs, bps, ctl) = {
            let mut e = shared.lock().unwrap();
            if e.task_alive {
                return Ok(true);
            }
            if e.cancelled || e.last_maint.is_some_and(|t| t.elapsed() < MAINT_EVERY) {
                return Ok(e.applied);
            }
            e.maintained = true;
            e.last_maint = Some(Instant::now());
            (e.playhead.secs, e.playhead.bitrate, e.ctl.clone())
        };
        let (hh, st) = (h.clone(), store);
        let (keep, idx) = tokio::task::spawn_blocking(move || {
            let mut g = ctl.lock().unwrap();
            let keep = g.tick_idle(&hh, &st, file, len, secs, bps);
            if keep.is_ok() {
                // Çok videolu torrent: bu dosya dışındaki videoların kayıtlı pencereleri bırakılır.
                for other in st.kept_files().into_iter().filter(|f| *f != file) {
                    let rep = st.set_keep(other, &[]);
                    if let Err(e) = st.forget_pieces(&hh, &rep.evicted) {
                        tracing::warn!("diğer video penceresi unutturulamadı: {e:#}");
                    }
                }
            }
            (keep, g.index.clone())
        })
        .await?;
        let ok = keep.is_ok();
        {
            let mut e = shared.lock().unwrap();
            e.applied = ok;
            if idx.is_some() {
                e.index = idx;
            }
        }
        keep?;
        Ok(true)
    }

    /// Torrent'in tüm girişlerinde uygulanan son seçimi unutturur: `only_files` daraltıldıktan sonra
    /// bir sonraki tick seçimi yeniden gönderir. Engelleyicidir (denetleyici kilidi); `spawn_blocking`
    /// içinden çağır.
    pub fn reset_selection(&self, id: usize) {
        let ctls: Vec<Arc<Mutex<WindowCtl>>> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|((tid, _), _)| *tid == id)
            .map(|(_, e)| e.lock().unwrap().ctl.clone())
            .collect();
        for c in ctls {
            c.lock().unwrap().reset_selection();
        }
    }

    /// Bu (torrent, dosya) için son bildirilen oynatma konumu (sn).
    pub fn playhead_secs(&self, id: usize, file: usize) -> Option<f64> {
        let e = self.entries.lock().unwrap().get(&(id, file))?.clone();
        let t = e.lock().unwrap().playhead.secs;
        Some(t)
    }

    /// Pencere denetleyicisinin kurduğu video zaman dizini (Window kipi; henüz yoksa `None`; kapsayıcı
    /// ayrıştırılamıyorsa boş dizin).
    pub fn time_index(&self, id: usize, file: usize) -> Option<Arc<TimeIndex>> {
        let ctl = self.ctl_of(id, file)?;
        let g = ctl.try_lock().ok()?; // bir tick sürüyorsa sonraki yoklamada
        g.settled_index()
    }

    /// Bu (torrent, dosya) için son bilinen bit hızı (bayt/sn).
    pub fn known_bitrate(&self, id: usize, file: usize) -> Option<f64> {
        let e = self.entries.lock().unwrap().get(&(id, file))?.clone();
        let b = e.lock().unwrap().playhead.bitrate;
        Some(b)
    }

    /// Oynatma konumunu `range.start` baytına taşır ve pencereyi 500 ms'lik tick'i beklemeden
    /// uygular: yeni aralığın parçaları seçilir, eski pencere tahliye edilir. Disk kipinde no-op.
    pub async fn ensure_range(
        self: &Arc<Self>,
        session: &Arc<librqbit::Session>,
        h: Arc<ManagedTorrent>,
        id: usize,
        file: usize,
        range: std::ops::Range<u64>,
    ) -> anyhow::Result<()> {
        let Some(store) = self.window_store(&h) else {
            return Ok(());
        };
        let len = Layout::read(session, &h, id, file)?.len.max(1);
        let bitrate = self.known_bitrate(id, file).unwrap_or(len as f64 / 5400.0);
        let existing = self.entries.lock().unwrap().get(&(id, file)).cloned();
        let index = existing.as_ref().and_then(|e| e.lock().unwrap().index.clone());
        let start = range.start.min(len);
        let secs = index.as_ref().map(|i| i.secs_at(start, bitrate)).unwrap_or(start as f64 / bitrate);
        // Konumu kaydeder, önden okumayı başlatır (yoksa); bayt konumu açıkça verilen değerdir.
        self.report(session, h.clone(), id, file, secs, None, Some(bitrate), 0)?;
        let shared = self.entries.lock().unwrap().get(&(id, file)).cloned();
        let Some(shared) = shared else { return Ok(()) };
        let ctl = {
            let mut e = shared.lock().unwrap();
            e.playhead.pos = start;
            e.ctl.clone()
        };
        let hh = h.clone();
        let extra = range.start.min(len)..range.end.min(len);
        let (keep, idx) = tokio::task::spawn_blocking(move || {
            let mut g = ctl.lock().unwrap();
            // Aralığın tamamı (bitiş dahil) seçili kalsın: pencere dizinsiz kısa olabilir.
            g.add_extra(extra);
            let keep = g.tick(&hh, &store, file, len, secs, bitrate);
            (keep, g.index.clone())
        })
        .await?;
        keep?;
        if idx.is_some() {
            shared.lock().unwrap().index = idx;
        }
        Ok(())
    }

    fn ctl_of(&self, id: usize, file: usize) -> Option<Arc<Mutex<WindowCtl>>> {
        let e = self.entries.lock().unwrap().get(&(id, file))?.clone();
        let c = e.lock().unwrap().ctl.clone();
        Some(c)
    }

    /// Oynatma konumunu TAŞIMADAN `range`i (ses/altyazı segmenti) `KEEP_ALIVE_TTL` boyunca seçili
    /// tutar; mevcut pencere tahliye edilmez. Giriş (oynatma) yoksa `false`: çağıran `ensure_range`e düşer.
    pub async fn keep_alive(&self, h: Arc<ManagedTorrent>, file: usize, len: u64, id: usize, range: std::ops::Range<u64>) -> anyhow::Result<bool> {
        let Some(store) = self.window_store(&h) else { return Ok(true) };
        let Some(ctl) = self.ctl_of(id, file) else { return Ok(false) };
        tokio::task::spawn_blocking(move || ctl.lock().unwrap().keep_alive(&h, &store, file, len, range)).await??;
        Ok(true)
    }

    /// Küçük bir yan dosyayı (harici altyazı) dosya düzeyinde seçili tutar; denetleyici onu ezmez.
    pub async fn add_side_file(&self, h: Arc<ManagedTorrent>, file: usize, id: usize, side: usize) -> anyhow::Result<bool> {
        let Some(store) = self.window_store(&h) else { return Ok(true) };
        let Some(ctl) = self.ctl_of(id, file) else { return Ok(false) };
        let len = h.metadata.load_full().and_then(|m| m.file_infos.get(file).map(|f| f.len)).unwrap_or(0);
        tokio::task::spawn_blocking(move || ctl.lock().unwrap().add_side_file(&h, &store, file, len, side)).await??;
        Ok(true)
    }

    /// Video dosyasının baytı `pos` şu an seçili bir parçada mı? Girişsiz/Disk kipinde `true`.
    pub fn pos_selected(&self, id: usize, file: usize, pos: u64) -> bool {
        if self.window.is_none() {
            return true;
        }
        match self.ctl_of(id, file) {
            Some(c) => c.lock().unwrap().is_selected_at(pos),
            None => false,
        }
    }

    fn window_store(&self, h: &ManagedTorrent) -> Option<Arc<WindowStore>> {
        self.window.as_ref()?.store(&h.shared().info_hash.as_string())
    }
}

/// Önden okuma: cepheye (ve gerekirse cephenin 32 MB ötesine) akış konumlar.
async fn prefetch_task(
    shared: Shared,
    h: Arc<ManagedTorrent>,
    file: usize,
    file_len: u64,
    session: Arc<librqbit::Session>,
    id: usize,
    wstore: Option<Arc<WindowStore>>,
) {
    let mut streams: Vec<std::pin::Pin<Box<dyn tokio::io::AsyncSeek + Send>>> = Vec::new();
    let ctl = shared.lock().unwrap().ctl.clone();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;

        let stats = h.stats();
        let (speed, peers) = stats
            .live
            .as_ref()
            .map(|l| (l.download_speed.mbps * 1024.0 * 1024.0, l.snapshot.peer_stats.live))
            .unwrap_or((0.0, 0));
        let (ph, rate, dropping) = {
            let mut e = shared.lock().unwrap();
            if e.cancelled || e.playhead.updated.elapsed() > IDLE_STOP {
                e.task_alive = false;
                break;
            }
            e.rate.push(speed, peers);
            (e.playhead, e.rate.effective(), e.rate.peers_dropping())
        };

        // Pencere kipi: seçimi ve diski pencereye uydur. Bitmiş (seçili parçaların hepsi inmiş)
        // torrent'te de çalışır: oynatma konumu kayınca seçim yeniden genişler.
        let mut keep_end = file_len;
        if let Some(store) = &wstore {
            // Engelleyen iş (diskten indeks/meta okuma, kilitler) ayrı iş parçacığında. Meta/indeks
            // yüklenemese bile pencere (bit hızı + %20 pay) uygulanır; indeks geri çekilmeyle denenir.
            let (c, st, hh) = (ctl.clone(), store.clone(), h.clone());
            let (secs, bps) = (ph.secs, ph.bitrate);
            let res = tokio::task::spawn_blocking(move || {
                let mut g = c.lock().unwrap();
                let keep = g.tick(&hh, &st, file, file_len, secs, bps);
                (keep, g.index.clone())
            })
            .await;
            match res {
                Ok((Ok(keep), index)) => {
                    keep_end = keep.end;
                    if index.is_some() {
                        shared.lock().unwrap().index = index;
                    }
                }
                Ok((Err(e), _)) => tracing::debug!("pencere uygulanamadı: {e:#}"),
                Err(e) => tracing::debug!("pencere görevi: {e}"),
            }
        }
        let stats = if wstore.is_some() { h.stats() } else { stats };

        if stats.finished {
            streams.clear();
            continue;
        }
        let Ok(layout) = Layout::read(&session, &h, id, file) else {
            continue;
        };

        let remaining_secs = (file_len.saturating_sub(ph.pos)) as f64 / ph.bitrate;
        let ratio = rate / ph.bitrate;
        let window = window_secs(ratio, dropping, remaining_secs);
        // Pencere kipinde cephe keep.end'i geçmez.
        let target = (ph.pos + (window * ph.bitrate) as u64).min(file_len).min(keep_end);

        // Konumlanacak noktalar: cephe; cephe yeterince öndeyse (>= 20 sn tampon) bir sonraki boşluk.
        let mut wanted: Vec<u64> = Vec::new();
        let frontier = layout.frontier(ph.pos);
        if frontier < target {
            wanted.push(frontier);
            let ahead_secs = (frontier - ph.pos) as f64 / ph.bitrate;
            if ahead_secs >= 20.0 {
                if let Some(next) = layout.next_missing_after(frontier + STREAM_WINDOW) {
                    if next < target {
                        wanted.push(next);
                    }
                }
            }
        }
        wanted.truncate(MAX_PREFETCH_STREAMS);

        streams.truncate(wanted.len());
        for (i, at) in wanted.iter().enumerate() {
            if i >= streams.len() {
                match h.clone().stream(file).await {
                    Ok(s) => streams.push(Box::pin(s)),
                    Err(e) => {
                        tracing::debug!("önden okuma akışı açılamadı: {e:#}");
                        break;
                    }
                }
            }
            if let Err(e) = streams[i].seek(SeekFrom::Start(*at)).await {
                tracing::debug!("önden okuma konumlanamadı: {e:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_link_needs_deficit_buffer() {
        // Hız bit hızının yarısı, 1 saat kaldı: açığın (~30 dk) üstünde tampon gerekir.
        let n = need_secs(0.5, 3600.0, 0, false, StorageMode::Disk);
        assert!(n > 1800.0 && n <= 3600.0, "{n}");
        // Hızlı bağlantıda birkaç saniye yeter, donmalar hedefi büyütür.
        assert_eq!(need_secs(4.0, 3600.0, 0, false, StorageMode::Disk), 5.0);
        assert!(need_secs(4.0, 3600.0, 2, false, StorageMode::Disk) > 10.0);
    }

    #[test]
    fn need_secs_capped_in_window_mode() {
        // Oran 0.5, 1 saat kaldı: Disk'te açık kadar (~30 dk), Window'da pencereyle sınırlı.
        let disk = need_secs(0.5, 3600.0, 0, false, StorageMode::Disk);
        assert!(disk > 1800.0, "{disk}");
        let win = need_secs(0.5, 3600.0, 0, false, StorageMode::Window);
        assert!(win <= 280.0, "{win}");
        // Küçük ihtiyaç değişmez.
        assert_eq!(need_secs(4.0, 3600.0, 0, false, StorageMode::Window), 5.0);
    }

    #[test]
    fn window_grows_when_tight() {
        assert!(window_secs(1.1, false, 5000.0) > window_secs(3.5, false, 5000.0));
        assert_eq!(window_secs(0.8, false, 1234.0), 1234.0);
        assert!(window_secs(2.0, true, 5000.0) > window_secs(2.0, false, 5000.0));
    }

    #[test]
    fn rate_is_conservative() {
        let mut r = Rate::default();
        for _ in 0..20 {
            r.push(10.0e6, 30);
        }
        r.push(1.0e6, 10);
        assert!(r.effective() < 9.0e6);
        assert!(r.peers_dropping());
    }
}
