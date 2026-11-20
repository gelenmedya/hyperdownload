//! HLS HTTP uçları: `/hls/{id}/{file}/...` (iOS AVPlayer `http://127.0.0.1:<port>` üzerinden açar).
//!
//! URL düzeni (hepsi `/hls/{id}/{file}/` altında):
//!
//! | yol                              | içerik                                  | Content-Type                    |
//! |----------------------------------|-----------------------------------------|---------------------------------|
//! | `master.m3u8`                    | ana oynatma listesi                     | `application/vnd.apple.mpegurl` |
//! | `v.m3u8`, `a{track}.m3u8`        | video / ses medya listesi               | `application/vnd.apple.mpegurl` |
//! | `s{id}.m3u8`                     | altyazı medya listesi (gömülü ya da harici) | `application/vnd.apple.mpegurl` |
//! | `v/init.mp4`, `v/{n}.m4s`        | video başlatma / segment                | `video/mp4`                     |
//! | `a{track}/init.mp4`, `.../{n}.m4s` | ses başlatma / segment                | `audio/mp4`                     |
//! | `s{id}/{n}.vtt`                  | WebVTT segmenti                         | `text/vtt`                      |
//!
//! Harici altyazı kimlikleri `EXTERNAL_SUB_ID_BASE + dosya_indeksi` (tek segment, `s{id}/0.vtt`).
//!
//! Motor bağlantısı [`HlsHost`] üzerindendir (testlerde yerel dosya kaynağı); dosya başına durum
//! (kapsayıcı bilgisi, zaman dizini, segment planı, destek raporu) ilk istekte kurulup
//! [`HlsCore`]'da önbelleğe alınır, `close`/`remove`'da silinir.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub use crate::atmos::ReadSeek;
use crate::atmos::AtmosState;
use crate::demux::{self, MediaInfo, PacketReader, TimeIndex, TrackInfo};
use crate::engine::Engine;
use crate::hls::codecs::audio_codec_string;
use crate::hls::playlist::{self, EXTERNAL_SUB_ID_BASE, ExternalSub, PlanError, TARGET_SEGMENT_SECS};
use crate::hls::segment::{self, Span};
use crate::media::{self, FileKind};
use crate::support::{self, DeviceCaps, HlsPlan, Level, TrackSupport};

/// Bir okumanın veri gelmesini bekleyeceği en uzun süre; aşılınca 503.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// İlk istekte dosya bilgisinin (kapsayıcı + zaman dizini) gelmesi için en uzun bekleme.
pub const STATE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bir segment isteğinin toplam üst sınırı (okuma zaman aşımından biraz uzun).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);
/// Önbellekteki en çok segment sayısı ve bellek tavanı.
const CACHE_SEGMENTS: usize = 8;
const CACHE_BYTES: usize = 64 * 1024 * 1024;
/// Harici altyazı dosyası üst sınırı.
const MAX_EXTERNAL_SUB: u64 = 5 * 1024 * 1024;
/// Aynı anda segment üreten engelleyici iş sayısı.
const GEN_PERMITS: usize = 4;
/// Aynı aralık için tekrar odaklama/keep-alive yapılmayan süre.
const FOCUS_DEDUPE: Duration = Duration::from_secs(5);
/// Segment aralığına eklenen pay (sn): ses geri sarma payı ve sonraki anahtar kare.
const RANGE_PAD_SECS: f64 = 3.0;
/// `io::Error` iletisinde zaman aşımı işareti (anyhow zincirinde ayıklanır).
const TIMEOUT_MARK: &str = "veri zaman aşımı";
/// Pencere dışı (seçili olmayan) parça okuma reddi.
const OUTSIDE_MARK: &str = "pencere dışı okuma";
const OUTSIDE_MSG: &str = "Pencere bu konuma henüz taşınmadı, biraz sonra tekrar dene";
const WAIT_MSG: &str = "Film bilgisi henüz gelmedi, biraz sonra tekrar dene";
const DATA_MSG: &str = "Veri henüz gelmedi, biraz sonra tekrar dene";

// ---------------------------------------------------------------------------------------------
// Kaynak soyutlaması
// ---------------------------------------------------------------------------------------------

/// Bir medya dosyasının bayt kaynağı (torrent dosyası ya da test için yerel dosya).
pub trait MediaSource: Send + Sync {
    /// Yeni bir okuyucu açar. Torrent kaynağında okumalar veri gelene kadar BLOKLAR; bu yüzden
    /// yalnızca `spawn_blocking` içinden çağrılmalıdır.
    fn open(&self) -> anyhow::Result<Box<dyn ReadSeek>>;
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// `open` ile aynı, her okuma için `t` bekleme sınırıyla (aşılırsa zaman aşımı hatası).
    fn open_timeout(&self, _t: Duration) -> anyhow::Result<Box<dyn ReadSeek>> {
        self.open()
    }
}

/// Yerel dosya kaynağı.
pub struct FileSource {
    path: PathBuf,
    len: u64,
}

impl FileSource {
    pub fn new(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let len = std::fs::metadata(&path)?.len();
        Ok(Self { path, len })
    }
}

impl MediaSource for FileSource {
    fn open(&self) -> anyhow::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(std::fs::File::open(&self.path)?))
    }
    fn len(&self) -> u64 {
        self.len
    }
}

thread_local! {
    /// Bu iş parçacığındaki `BlockingReader`ın torrent verisi için harcadığı toplam bekleme (ns);
    /// `segment()` üretim başında sıfırlar, sonunda okur (üretim tek `spawn_blocking` iş parçacığında koşar).
    static WAIT_NS: Cell<u64> = const { Cell::new(0) };
}

fn add_wait(since: Instant) {
    let ns = since.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    WAIT_NS.with(|c| c.set(c.get().saturating_add(ns)));
}

/// Bir okumanın başlayacağı bayt seçili (indirilebilir) bir parçada mı?
pub type ReadGate = Arc<dyn Fn(u64) -> bool + Send + Sync>;

/// Eşzamansız akışı (librqbit `FileStream`) bloklayan `Read + Seek`'e çevirir. `deadline`
/// ÜRETİM başına toplamdır (her okumanın payı kalan süredir); aşılınca `TimedOut` döner.
///
/// librqbit `FileStream::poll_read` eksik parçada yalnızca bekler, ama kayıtlı akışlar
/// `acquire_piece`ta `priority_pieces` olarak SEÇİMDEN BAĞIMSIZ öncelikle istenir (vendor/librqbit
/// `piece_tracker.rs`): pencere dışı bir okuma o parçayı indirtir. Bu yüzden `gate`, okuma
/// konumunun parçası seçili değilse okumayı REDDEDER (akışa hiç dokunmaz).
struct BlockingReader<S> {
    s: S,
    rt: tokio::runtime::Handle,
    deadline: Instant,
    len: u64,
    pos: u64,
    gate: Option<ReadGate>,
}

impl<S> BlockingReader<S> {
    fn left(&self) -> std::io::Result<Duration> {
        let d = self.deadline.saturating_duration_since(Instant::now());
        if d.is_zero() {
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, TIMEOUT_MARK))
        } else {
            Ok(d)
        }
    }
}

impl<S: tokio::io::AsyncRead + tokio::io::AsyncSeek + Unpin> Read for BlockingReader<S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.len && !buf.is_empty() {
            if let Some(g) = &self.gate {
                if !g(self.pos) {
                    return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, OUTSIDE_MARK));
                }
            }
        }
        let t = self.left()?;
        let started = Instant::now();
        let res = self.rt.block_on(tokio::time::timeout(t, self.s.read(buf)));
        add_wait(started);
        match res {
            Ok(r) => {
                let n = r?;
                self.pos += n as u64;
                Ok(n)
            }
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, TIMEOUT_MARK)),
        }
    }
}

impl<S: tokio::io::AsyncRead + tokio::io::AsyncSeek + Unpin> Seek for BlockingReader<S> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let t = self.left()?;
        let started = Instant::now();
        let res = self.rt.block_on(tokio::time::timeout(t, self.s.seek(pos)));
        add_wait(started);
        match res {
            Ok(r) => {
                self.pos = r?;
                Ok(self.pos)
            }
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, TIMEOUT_MARK)),
        }
    }
}

/// Torrent içindeki dosya (librqbit akış okuyucusu; eksik parçalar için bekler).
pub struct TorrentSource {
    handle: Arc<librqbit::ManagedTorrent>,
    file: usize,
    len: u64,
    rt: tokio::runtime::Handle,
    gate: Option<ReadGate>,
}

impl MediaSource for TorrentSource {
    fn open(&self) -> anyhow::Result<Box<dyn ReadSeek>> {
        self.open_timeout(READ_TIMEOUT)
    }
    fn len(&self) -> u64 {
        self.len
    }
    fn open_timeout(&self, t: Duration) -> anyhow::Result<Box<dyn ReadSeek>> {
        let stream = self.rt.block_on(self.handle.clone().stream(self.file))?;
        Ok(Box::new(BlockingReader {
            s: stream,
            rt: self.rt.clone(),
            deadline: Instant::now() + t,
            len: self.len,
            pos: 0,
            gate: self.gate.clone(),
        }))
    }
}

// ---------------------------------------------------------------------------------------------
// Motor soyutlaması
// ---------------------------------------------------------------------------------------------

pub type Fut<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'a>>;

/// Uçların motordan ihtiyaç duyduğu her şey (testlerde yerel dosyayla uygulanır).
pub trait HlsHost: Send + Sync + 'static {
    /// Dosyanın bayt kaynağı; torrent/dosya yoksa (ya da meta veri henüz yoksa) `None`.
    /// Tokio bağlamında çağrılmalıdır.
    fn source(&self, id: usize, file: usize) -> anyhow::Result<Option<Arc<dyn MediaSource>>>;
    fn media_info(&self, id: usize, file: usize) -> Fut<'_, Arc<MediaInfo>>;
    /// Video anahtar kare zaman dizini; hazır olana dek bekler (çağıran zaman sınırı koyar).
    fn time_index(&self, id: usize, file: usize) -> Fut<'_, Arc<TimeIndex>>;
    fn atmos(&self, id: usize, file: usize) -> HashMap<u64, AtmosState>;
    /// Aynı torrent'teki tüm dosyalar `(indeks, ad)`; harici altyazıları çağıran süzer.
    fn files(&self, id: usize) -> Vec<(usize, String)>;
    /// Küçük bir yan dosyayı (harici altyazı) indirmeye seçer. Window kipinde seçim pencere
    /// denetleyicisine (`video_file`) kayıtlıdır; denetleyici onu ezmez.
    fn prepare_file(&self, id: usize, video_file: usize, side_file: usize) -> Fut<'_, ()>;
    /// Son bildirilen oynatma konumu (sn).
    fn playhead_secs(&self, id: usize, file: usize) -> Option<f64>;
    /// VİDEO segment isteği: oynatma konumunu `t`ye çeker ve pencereyi `range` baytına taşır.
    fn focus(&self, id: usize, file: usize, t: f64, duration: f64, bitrate: f64, range: Range<u64>) -> Fut<'_, ()>;
    /// Ses/altyazı segment isteği: `range` seçili ve tutulu kalır; oynatma konumu TAŞINMAZ,
    /// mevcut pencere tahliye edilmez.
    fn keep_alive(&self, id: usize, file: usize, t: f64, duration: f64, bitrate: f64, range: Range<u64>) -> Fut<'_, ()>;
}

/// [`Engine`] için [`HlsHost`].
pub struct EngineHost(pub Arc<Engine>);

impl HlsHost for EngineHost {
    fn source(&self, id: usize, file: usize) -> anyhow::Result<Option<Arc<dyn MediaSource>>> {
        let Ok(h) = self.0.handle(id) else { return Ok(None) };
        let Some(meta) = h.metadata.load_full() else { return Ok(None) };
        let Some(fi) = meta.file_infos.get(file) else { return Ok(None) };
        let len = fi.len;
        // Window kipinde okumalar yalnızca seçili parçalarla sınırlanır (bkz. `BlockingReader`).
        // Küçük yan dosyalar (harici altyazı, `prepare_file` ile dosya düzeyinde seçilir) sınırlanmaz.
        let gate: Option<ReadGate> = (self.0.storage == crate::engine::StorageMode::Window && len > MAX_EXTERNAL_SUB).then(|| {
            let e = self.0.clone();
            Arc::new(move |pos| e.buffering.pos_selected(id, file, pos)) as ReadGate
        });
        Ok(Some(Arc::new(TorrentSource { handle: h, file, len, rt: tokio::runtime::Handle::current(), gate })))
    }

    fn media_info(&self, id: usize, file: usize) -> Fut<'_, Arc<MediaInfo>> {
        Box::pin(self.0.media_info(id, file))
    }

    fn time_index(&self, id: usize, file: usize) -> Fut<'_, Arc<TimeIndex>> {
        Box::pin(async move {
            let e = &self.0;
            if e.storage == crate::engine::StorageMode::Window {
                // Pencere denetleyicisi indeksi (Cues/moov bayt aralıklarını seçip) kendi tick'inde kurar;
                // oynatma konumu kaydı o görevi başlatır. Hazır olana dek yoklanır.
                let h = e.handle(id)?;
                e.buffering.report(&e.session, h, id, file, e.buffering.playhead_secs(id, file).unwrap_or(0.0), None, e.buffering.known_bitrate(id, file), 0)?;
                loop {
                    if let Some(i) = e.buffering.time_index(id, file) {
                        // Boş dizin = "denendi, kapsayıcıda anahtar kare dizini yok" (plan 422 verir).
                        return Ok(i);
                    }
                    e.handle(id)?; // torrent kaldırıldıysa bekleme
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            let src = self.source(id, file)?.ok_or_else(|| anyhow::anyhow!("dosya yok"))?;
            let ti = tokio::task::spawn_blocking(move || demux::time_index(src.open_timeout(STATE_TIMEOUT)?)).await??;
            Ok(Arc::new(ti))
        })
    }

    fn atmos(&self, id: usize, file: usize) -> HashMap<u64, AtmosState> {
        self.0.atmos(id, file)
    }

    fn files(&self, id: usize) -> Vec<(usize, String)> {
        self.0
            .view(id, true)
            .and_then(|v| v.files)
            .map(|f| f.into_iter().map(|x| (x.idx, x.name)).collect())
            .unwrap_or_default()
    }

    fn prepare_file(&self, id: usize, video_file: usize, side_file: usize) -> Fut<'_, ()> {
        Box::pin(async move {
            let e = &self.0;
            if e.storage == crate::engine::StorageMode::Window {
                let h = e.handle(id)?;
                if e.buffering.add_side_file(h, video_file, id, side_file).await? {
                    return Ok(());
                }
            }
            e.select_files(id, &[side_file], false).await
        })
    }

    fn playhead_secs(&self, id: usize, file: usize) -> Option<f64> {
        self.0.buffering.playhead_secs(id, file)
    }

    fn focus(&self, id: usize, file: usize, t: f64, duration: f64, bitrate: f64, range: Range<u64>) -> Fut<'_, ()> {
        Box::pin(async move {
            let e = &self.0;
            let h = e.handle(id)?;
            e.buffering.report(&e.session, h, id, file, t, Some(duration), Some(bitrate), 0)?;
            e.ensure_range(id, file, range).await
        })
    }

    fn keep_alive(&self, id: usize, file: usize, t: f64, duration: f64, bitrate: f64, range: Range<u64>) -> Fut<'_, ()> {
        Box::pin(async move {
            let e = &self.0;
            if e.storage != crate::engine::StorageMode::Window {
                return Ok(());
            }
            let h = e.handle(id)?;
            let len = h.metadata.load_full().and_then(|m| m.file_infos.get(file).map(|f| f.len)).unwrap_or(0);
            if !e.buffering.keep_alive(h, file, len, id, range.clone()).await? {
                // Henüz oynatma girişi yok (ilk istek ses/altyazı): bu istek konumu belirler.
                self.focus(id, file, t, duration, bitrate, range).await?;
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Hatalar
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
pub struct HlsErr(pub StatusCode, pub String);

impl IntoResponse for HlsErr {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<PlanError> for HlsErr {
    /// R26: oynatılamaz dosya -> 422 + Türkçe ileti.
    fn from(e: PlanError) -> Self {
        HlsErr(StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
    }
}

fn not_found() -> HlsErr {
    HlsErr(StatusCode::NOT_FOUND, "bulunamadı".into())
}

fn unavailable(msg: &str) -> HlsErr {
    HlsErr(StatusCode::SERVICE_UNAVAILABLE, msg.into())
}

/// Segment üretim hatası: veri beklerken zaman aşımı 503, gerisi 500.
fn gen_err(e: anyhow::Error) -> HlsErr {
    let s = format!("{e:#}");
    if s.contains(OUTSIDE_MARK) {
        tracing::warn!("HLS pencere dışı okuma reddedildi: {s}");
        unavailable(OUTSIDE_MSG)
    } else if s.contains(TIMEOUT_MARK) {
        tracing::warn!("HLS segment verisi gelmedi: {s}");
        unavailable(DATA_MSG)
    } else {
        tracing::warn!("HLS segment üretilemedi: {s}");
        HlsErr(StatusCode::INTERNAL_SERVER_ERROR, s)
    }
}

// ---------------------------------------------------------------------------------------------
// Dosya durumu + önbellekler
// ---------------------------------------------------------------------------------------------

/// Bir (torrent, dosya) için ilk istekte kurulan durum.
pub struct FileHls {
    info: Arc<MediaInfo>,
    spans: Result<Vec<Span>, PlanError>,
    /// Tek destek kaynağı: master her istekte tazeler (Atmos netleşebilir), uç noktalar aynısını okur.
    support: RwLock<Arc<Vec<TrackSupport>>>,
    len: u64,
    duration: f64,
    source: Arc<dyn MediaSource>,
    /// Harici altyazılar: `(ExternalSub, dosya indeksi)`.
    externals: Vec<(ExternalSub, usize)>,
    time_index: Arc<TimeIndex>,
    /// İz başına ses başlatma segmentleri (ilk kareden bir kez üretilir, sonra sabit).
    audio_inits: tokio::sync::Mutex<HashMap<u64, Bytes>>,
}

type SegKey = (usize, usize, String);

#[derive(Default)]
struct SegCache {
    map: HashMap<SegKey, Bytes>,
    order: VecDeque<SegKey>,
    bytes: usize,
}

/// Son segment üretimlerinin sayısı (`/api/diag` -> `hls_recent`).
const METRICS_KEPT: usize = 30;

/// Bir segment üretiminin ölçümü. `cpu_ms` = toplam - veri bekleme (çözme/kodlama/paketleme payı).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SegMetric {
    /// "v" video, "a" ses, "s" altyazı
    pub kind: String,
    pub track: Option<u64>,
    pub n: usize,
    pub total_ms: f64,
    pub wait_ms: f64,
    pub cpu_ms: f64,
    pub bytes: usize,
}

impl SegMetric {
    /// `v/3`, `a2/3`, `s6/0` -> (tür, iz, segment no).
    fn parse_key(key: &str) -> Option<(String, Option<u64>, usize)> {
        let (head, n) = key.split_once('/')?;
        let kind = head.get(..1)?;
        let track = head[1..].parse().ok();
        Some((kind.to_string(), track, n.parse().ok()?))
    }

    fn new(key: &str, total: Duration, wait: Duration, bytes: usize) -> Option<Self> {
        let (kind, track, n) = Self::parse_key(key)?;
        let wait = wait.min(total);
        let ms = |d: Duration| (d.as_secs_f64() * 1000.0 * 10.0).round() / 10.0;
        Some(Self { kind, track, n, total_ms: ms(total), wait_ms: ms(wait), cpu_ms: ms(total - wait), bytes })
    }
}

/// Dosya durumları, segment önbelleği (son 8 segment, 64 MiB) ve tek-uçuş kilitleri.
pub struct HlsCore {
    files: Mutex<HashMap<(usize, usize), Arc<tokio::sync::OnceCell<Arc<FileHls>>>>>,
    cache: Mutex<SegCache>,
    inflight: Mutex<HashMap<SegKey, Arc<tokio::sync::Mutex<()>>>>,
    /// Engelleyen segment üretimi sınırı (`GEN_PERMITS`).
    permits: Arc<tokio::sync::Semaphore>,
    /// Yalnızca video üretimine ayrılmış 1 izin: ses/altyazı üretimi videoyu aç bırakamaz.
    video_permit: Arc<tokio::sync::Semaphore>,
    /// `invalidate` sayacı: uçuştaki bir üretim, arada geçersiz kılındıysa önbelleğe yazmaz.
    epoch: AtomicU64,
    /// Son odaklama/keep-alive zamanları (tekrarı önlemek için).
    recent: Mutex<HashMap<(usize, usize, char, usize), Instant>>,
    /// Son `METRICS_KEPT` segment üretimi (en eski önde).
    metrics: Mutex<VecDeque<SegMetric>>,
}

impl Default for HlsCore {
    fn default() -> Self {
        Self {
            files: Default::default(),
            cache: Default::default(),
            inflight: Default::default(),
            permits: Arc::new(tokio::sync::Semaphore::new(GEN_PERMITS)),
            video_permit: Arc::new(tokio::sync::Semaphore::new(1)),
            epoch: AtomicU64::new(0),
            recent: Default::default(),
            metrics: Default::default(),
        }
    }
}

impl HlsCore {
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self, m: SegMetric) {
        let mut q = self.metrics.lock().unwrap();
        q.push_back(m);
        while q.len() > METRICS_KEPT {
            q.pop_front();
        }
    }

    /// `/api/diag` için son üretimler (eskiden yeniye).
    pub fn recent_metrics(&self) -> Vec<SegMetric> {
        self.metrics.lock().unwrap().iter().cloned().collect()
    }

    /// Bu anahtar `FOCUS_DEDUPE` içinde işlendiyse `true`; değilse şimdi işlenmiş olarak işaretler.
    fn seen_recently(&self, k: (usize, usize, char, usize)) -> bool {
        let mut m = self.recent.lock().unwrap();
        m.retain(|_, t| t.elapsed() < Duration::from_secs(60));
        if m.get(&k).is_some_and(|t| t.elapsed() < FOCUS_DEDUPE) {
            return true;
        }
        m.insert(k, Instant::now());
        false
    }

    fn cache_has(&self, k: &SegKey) -> bool {
        self.cache.lock().unwrap().map.contains_key(k)
    }

    /// Engelleyen işi izin alarak `spawn_blocking`'te çalıştırır. `video` ise ortak izinlerden biri
    /// YA DA ayrılmış video izni yeter (ses/altyazı yalnızca ortak izinleri kullanır).
    async fn blocking<T, F>(&self, video: bool, f: F) -> Result<T, HlsErr>
    where
        T: Send + 'static,
        F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    {
        let shared = self.permits.clone();
        let permit = if video {
            let reserved = self.video_permit.clone();
            tokio::select! {
                p = shared.acquire_owned() => p,
                p = reserved.acquire_owned() => p,
            }
        } else {
            shared.acquire_owned().await
        };
        let _permit = permit.map_err(|e| HlsErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        match tokio::task::spawn_blocking(f).await {
            Ok(r) => r.map_err(gen_err),
            Err(e) => Err(HlsErr(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
        }
    }

    /// Torrent kapatılınca/kaldırılınca durumunu ve segmentlerini siler.
    pub fn invalidate(&self, id: usize) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.recent.lock().unwrap().retain(|k, _| k.0 != id);
        self.files.lock().unwrap().retain(|(i, _), _| *i != id);
        let mut c = self.cache.lock().unwrap();
        c.order.retain(|k| k.0 != id);
        c.map.retain(|k, _| k.0 != id);
        c.bytes = c.map.values().map(|b| b.len()).sum();
    }

    fn cache_get(&self, k: &SegKey) -> Option<Bytes> {
        let mut c = self.cache.lock().unwrap();
        let b = c.map.get(k)?.clone();
        // LRU: en yeniye taşı
        c.order.retain(|x| x != k);
        c.order.push_back(k.clone());
        Some(b)
    }

    fn cache_put(&self, k: SegKey, b: Bytes) {
        let mut c = self.cache.lock().unwrap();
        if let Some(old) = c.map.insert(k.clone(), b.clone()) {
            c.bytes -= old.len();
            c.order.retain(|x| *x != k);
        }
        c.bytes += b.len();
        c.order.push_back(k);
        while c.order.len() > 1 && (c.order.len() > CACHE_SEGMENTS || c.bytes > CACHE_BYTES) {
            if let Some(old) = c.order.pop_front() {
                if let Some(v) = c.map.remove(&old) {
                    c.bytes -= v.len();
                }
            }
        }
    }

    /// Önbellekte yoksa `gen`'i `spawn_blocking`'te çalıştırır; aynı anahtar için eşzamanlı
    /// istekler tek üretimi paylaşır. Üretim istemci kopsa da tamamlanıp önbelleğe girer.
    async fn segment<F>(self: &Arc<Self>, key: SegKey, gen_fn: F) -> Result<Bytes, HlsErr>
    where
        F: FnOnce() -> anyhow::Result<Vec<u8>> + Send + 'static,
    {
        if let Some(b) = self.cache_get(&key) {
            return Ok(b);
        }
        let core = self.clone();
        let task = tokio::spawn(async move {
            let lock = core.inflight.lock().unwrap().entry(key.clone()).or_default().clone();
            let guard = lock.lock().await;
            let res = if let Some(b) = core.cache_get(&key) {
                Ok(b)
            } else {
                let epoch = core.epoch.load(Ordering::SeqCst);
                let timed = move || {
                    WAIT_NS.with(|c| c.set(0));
                    let t0 = Instant::now();
                    let r = gen_fn();
                    let total = t0.elapsed();
                    let wait = Duration::from_nanos(WAIT_NS.with(|c| c.replace(0)));
                    r.map(|v| (v, total, wait))
                };
                core.blocking(key.2.starts_with("v/"), timed).await.map(|(v, total, wait)| {
                    if let Some(m) = SegMetric::new(&key.2, total, wait, v.len()) {
                        core.record(m);
                    }
                    let b = Bytes::from(v);
                    // Üretim sürerken torrent kapatıldıysa önbelleğe yazılmaz.
                    if core.epoch.load(Ordering::SeqCst) == epoch {
                        core.cache_put(key.clone(), b.clone());
                    }
                    b
                })
            };
            drop(guard);
            let mut m = core.inflight.lock().unwrap();
            if m.get(&key).is_some_and(|l| Arc::strong_count(l) <= 2) {
                m.remove(&key);
            }
            res
        });
        match tokio::time::timeout(REQUEST_TIMEOUT, task).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => Err(HlsErr(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"))),
            Err(_) => Err(unavailable(DATA_MSG)),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Uygulama
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct HlsApp {
    host: Arc<dyn HlsHost>,
    core: Arc<HlsCore>,
}

/// Motor için HLS uçları (`server::router` içinde birleştirilir).
pub fn routes(engine: Arc<Engine>) -> Router {
    let core = engine.hls.clone();
    routes_with(Arc::new(EngineHost(engine)), core)
}

/// Verilen ana makine ve önbellekle uçlar (testler yerel dosya kaynağı verir).
pub fn routes_with(host: Arc<dyn HlsHost>, core: Arc<HlsCore>) -> Router {
    Router::new().route("/hls/{id}/{file}/{*rest}", get(handle)).with_state(HlsApp { host, core })
}

#[derive(Debug, PartialEq, Eq)]
enum Req {
    Master,
    VideoPlaylist,
    AudioPlaylist(u64),
    SubPlaylist(u64),
    VideoInit,
    VideoSeg(usize),
    AudioInit(u64),
    AudioSeg(u64, usize),
    SubSeg(u64, usize),
}

fn parse_req(rest: &str) -> Option<Req> {
    let num = |s: &str| -> Option<u64> { (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten() };
    if let Some((dir, file)) = rest.split_once('/') {
        if file.contains('/') {
            return None;
        }
        if dir == "v" {
            return match file {
                "init.mp4" => Some(Req::VideoInit),
                f => Some(Req::VideoSeg(num(f.strip_suffix(".m4s")?)? as usize)),
            };
        }
        if let Some(t) = dir.strip_prefix('a') {
            let t = num(t)?;
            return match file {
                "init.mp4" => Some(Req::AudioInit(t)),
                f => Some(Req::AudioSeg(t, num(f.strip_suffix(".m4s")?)? as usize)),
            };
        }
        if let Some(t) = dir.strip_prefix('s') {
            return Some(Req::SubSeg(num(t)?, num(file.strip_suffix(".vtt")?)? as usize));
        }
        return None;
    }
    if rest == "master.m3u8" {
        return Some(Req::Master);
    }
    if rest == "v.m3u8" {
        return Some(Req::VideoPlaylist);
    }
    let stem = rest.strip_suffix(".m3u8")?;
    if let Some(t) = stem.strip_prefix('a') {
        return Some(Req::AudioPlaylist(num(t)?));
    }
    if let Some(t) = stem.strip_prefix('s') {
        return Some(Req::SubPlaylist(num(t)?));
    }
    None
}

fn playlist_resp(body: String) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl"), (header::CACHE_CONTROL, "no-cache")],
        body,
    )
        .into_response()
}

fn data_resp(content_type: &'static str, body: Bytes) -> Response {
    (StatusCode::OK, [(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "max-age=3600")], body).into_response()
}

async fn handle(State(app): State<HlsApp>, Path((id, file, rest)): Path<(usize, usize, String)>) -> Response {
    match app.dispatch(id, file, &rest).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

/// Dil ipucu: dosya adındaki belirteçlerden (`film.tr.srt`, `Film.English.srt`).
fn guess_lang(name: &str) -> Option<String> {
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s).to_lowercase();
    for tok in stem.split(|c: char| !c.is_alphanumeric()).rev() {
        let l = match tok {
            "tr" | "tur" | "turkish" | "türkçe" | "turkce" => "tur",
            "en" | "eng" | "english" => "eng",
            "de" | "ger" | "deu" | "german" => "ger",
            "fr" | "fre" | "fra" | "french" => "fre",
            "es" | "spa" | "spanish" => "spa",
            "it" | "ita" | "italian" => "ita",
            "ru" | "rus" | "russian" => "rus",
            "ar" | "ara" | "arabic" => "ara",
            _ => continue,
        };
        return Some(l.into());
    }
    None
}

impl HlsApp {
    async fn dispatch(&self, id: usize, file: usize, rest: &str) -> Result<Response, HlsErr> {
        let req = parse_req(rest).ok_or_else(not_found)?;
        let fs = self.file_state(id, file).await?;
        match req {
            Req::Master => {
                // Atmos tespiti sonradan netleşebilir: destek raporu her istekte tazelenir.
                let support = Arc::new(support::assess(&fs.info, &self.host.atmos(id, file), DeviceCaps::default()));
                *fs.support.write().unwrap() = support.clone();
                let dv = support
                    .iter()
                    .find(|s| s.kind == "video" && s.level != Level::Unsupported)
                    .and_then(|s| fs.info.tracks.iter().find(|t| t.id == s.track_id))
                    .and_then(|t| t.dv_config.as_deref())
                    .and_then(demux::parse_dovi_config);
                let exts: Vec<ExternalSub> = fs.externals.iter().map(|(e, _)| e.clone()).collect();
                let body = playlist::master(&fs.info, &support, dv.as_ref(), fs.len, &exts)?;
                fs.spans.as_ref().map_err(|e| HlsErr::from(*e))?;
                Ok(playlist_resp(body))
            }
            Req::VideoPlaylist => {
                fs.video()?;
                Ok(playlist_resp(playlist::media(fs.spans()?, "v", "m4s")))
            }
            Req::AudioPlaylist(t) => {
                fs.audio(t).ok_or_else(not_found)?;
                Ok(playlist_resp(playlist::media(fs.spans()?, &format!("a{t}"), "m4s")))
            }
            Req::SubPlaylist(t) => {
                if t >= EXTERNAL_SUB_ID_BASE {
                    fs.external(t).ok_or_else(not_found)?;
                    let end = fs.duration.max(1.0);
                    let span = [Span { index: 0, start: 0.0, end }];
                    Ok(playlist_resp(playlist::media(&span, &format!("s{t}"), "vtt")))
                } else {
                    fs.subtitle(t).ok_or_else(not_found)?;
                    Ok(playlist_resp(playlist::media(fs.spans()?, &format!("s{t}"), "vtt")))
                }
            }
            Req::VideoInit => {
                let (track, plan) = fs.video()?;
                let b = segment::video_init(&track, plan).map_err(gen_err)?;
                Ok(data_resp("video/mp4", Bytes::from(b)))
            }
            Req::VideoSeg(n) => {
                let (track, plan) = fs.video()?;
                let span = fs.span(n)?;
                let key = (id, file, format!("v/{n}"));
                // Yalnızca VİDEO segmentleri oynatma konumunu taşır (AVPlayer v/a/s'yi sırasız ister).
                if !self.core.cache_has(&key) && !self.core.seen_recently((id, file, 'v', n)) {
                    self.focus(id, file, &fs, &span).await;
                }
                let src = fs.source.clone();
                let b = self
                    .core
                    .segment(key, move || segment::video_segment(src.open_timeout(READ_TIMEOUT)?, &track, plan, &span))
                    .await?;
                Ok(data_resp("video/mp4", b))
            }
            Req::AudioInit(t) => {
                let (track, plan) = fs.audio(t).ok_or_else(not_found)?;
                let mut inits = fs.audio_inits.lock().await;
                if let Some(b) = inits.get(&t) {
                    return Ok(data_resp("audio/mp4", b.clone()));
                }
                // Başlatma segmenti herhangi bir ses karesinden üretilir. Window kipinde dosya başı
                // tahliye edilmiş olabilir: kare, OYNATILAN video segmentinin (yoksa 0. segmentin)
                // aralığından okunur ve önce o aralık seçili tutulur.
                let spans = fs.spans()?;
                let span = self
                    .host
                    .playhead_secs(id, file)
                    .and_then(|p| spans.iter().find(|s| p >= s.start && p < s.end))
                    .or_else(|| spans.first())
                    .copied()
                    .ok_or_else(not_found)?;
                self.keep_alive(id, file, &fs, &span).await;
                let src = fs.source.clone();
                let start = span.start;
                let b = tokio::time::timeout(
                    REQUEST_TIMEOUT,
                    self.core.blocking(false, move || {
                        let mut pr = PacketReader::open(src.open_timeout(READ_TIMEOUT)?, track.id, start)?;
                        let pkt = pr.next()?.ok_or_else(|| anyhow::anyhow!("ses izinde paket yok"))?;
                        segment::audio_init(&track, plan, &pkt.data)
                    }),
                )
                .await
                .map_err(|_| unavailable(DATA_MSG))??;
                let b = Bytes::from(b);
                inits.insert(t, b.clone());
                Ok(data_resp("audio/mp4", b))
            }
            Req::AudioSeg(t, n) => {
                let (track, plan) = fs.audio(t).ok_or_else(not_found)?;
                let span = fs.span(n)?;
                let key = (id, file, format!("a{t}/{n}"));
                if !self.core.cache_has(&key) && !self.core.seen_recently((id, file, 'k', n)) {
                    self.keep_alive(id, file, &fs, &span).await;
                }
                let src = fs.source.clone();
                let b = self
                    .core
                    .segment(key, move || segment::audio_segment(src.open_timeout(READ_TIMEOUT)?, &track, plan, &span))
                    .await?;
                Ok(data_resp("audio/mp4", b))
            }
            Req::SubSeg(t, n) => {
                let key = (id, file, format!("s{t}/{n}"));
                if t >= EXTERNAL_SUB_ID_BASE {
                    let (ext, idx) = fs.external(t).ok_or_else(not_found)?;
                    if n != 0 {
                        return Err(not_found());
                    }
                    let name = ext.name.clone();
                    let src = self.host.source(id, idx).map_err(gen_err)?.ok_or_else(not_found)?;
                    if src.len() > MAX_EXTERNAL_SUB {
                        return Err(HlsErr(StatusCode::PAYLOAD_TOO_LARGE, "altyazı dosyası çok büyük".into()));
                    }
                    if !self.core.cache_has(&key) {
                        if let Err(e) = self.host.prepare_file(id, file, idx).await {
                            tracing::warn!("harici altyazı seçilemedi: {e:#}");
                        }
                    }
                    let b = self
                        .core
                        .segment(key, move || {
                            let mut buf = Vec::with_capacity(src.len() as usize);
                            src.open_timeout(READ_TIMEOUT)?.take(MAX_EXTERNAL_SUB).read_to_end(&mut buf)?;
                            Ok(segment::external_vtt(&name, &buf).into_bytes())
                        })
                        .await?;
                    Ok(data_resp("text/vtt", b))
                } else {
                    let track = fs.subtitle(t).ok_or_else(not_found)?;
                    let span = fs.span(n)?;
                    if !self.core.cache_has(&key) && !self.core.seen_recently((id, file, 'k', n)) {
                        self.keep_alive(id, file, &fs, &span).await;
                    }
                    let src = fs.source.clone();
                    let b = self
                        .core
                        .segment(key, move || Ok(segment::vtt_segment(src.open_timeout(READ_TIMEOUT)?, &track, &span)?.into_bytes()))
                        .await?;
                    Ok(data_resp("text/vtt", b))
                }
            }
        }
    }

    /// Segmentin tahmini bayt aralığı (TimeIndex); sonraki anahtar kare için sonda, ses geri sarma
    /// payı için (`pad_start`) başta `RANGE_PAD_SECS` pay.
    fn span_range(fs: &FileHls, span: &Span, pad_start: bool) -> (f64, f64, Range<u64>) {
        let dur = fs.duration.max(1.0);
        let bitrate = (fs.len as f64 / dur).max(1.0);
        let pad = if pad_start { RANGE_PAD_SECS } else { 0.0 };
        let a = fs.time_index.byte_at(span.start - pad, fs.len, bitrate);
        let b = fs.time_index.byte_at(span.end + RANGE_PAD_SECS, fs.len, bitrate).max(a);
        (dur, bitrate, a..b)
    }

    /// VİDEO segment isteği oynatma konumunun yetkilisidir (R6): playhead `span.start`a çekilir,
    /// pencere ve önden okuma buna göre kayar. Hata segmenti engellemez.
    async fn focus(&self, id: usize, file: usize, fs: &FileHls, span: &Span) {
        // Başlangıç payı yok: `ensure_range` aralık başını oynatma konumu sayar.
        let (dur, bitrate, range) = Self::span_range(fs, span, false);
        if let Err(e) = self.host.focus(id, file, span.start, dur, bitrate, range).await {
            tracing::warn!("HLS oynatma konumu taşınamadı: {e:#}");
        }
    }

    /// Ses/altyazı: aralığı seçili tutar, oynatma konumunu taşımaz.
    async fn keep_alive(&self, id: usize, file: usize, fs: &FileHls, span: &Span) {
        let (dur, bitrate, range) = Self::span_range(fs, span, true);
        if let Err(e) = self.host.keep_alive(id, file, span.start, dur, bitrate, range).await {
            tracing::warn!("HLS aralığı canlı tutulamadı: {e:#}");
        }
    }

    async fn file_state(&self, id: usize, file: usize) -> Result<Arc<FileHls>, HlsErr> {
        let cell = self.core.files.lock().unwrap().entry((id, file)).or_default().clone();
        match tokio::time::timeout(STATE_TIMEOUT, cell.get_or_try_init(|| self.build(id, file))).await {
            Ok(r) => r.cloned(),
            Err(_) => Err(unavailable(WAIT_MSG)),
        }
    }

    async fn build(&self, id: usize, file: usize) -> Result<Arc<FileHls>, HlsErr> {
        let source = self.host.source(id, file).map_err(gen_err)?.ok_or_else(not_found)?;
        let wait = |e: anyhow::Error| {
            tracing::warn!("HLS dosya bilgisi alınamadı: {e:#}");
            unavailable(WAIT_MSG)
        };
        let info = self.host.media_info(id, file).await.map_err(wait)?;
        let ti = self.host.time_index(id, file).await.map_err(wait)?;
        let len = source.len();
        let kfs: Vec<f64> = ti.points.iter().map(|p| p.0).collect();
        let duration = info
            .duration_secs
            .filter(|d| d.is_finite() && *d > 0.0)
            .unwrap_or_else(|| kfs.last().map_or(0.0, |k| k + TARGET_SEGMENT_SECS));
        let spans = playlist::plan(&kfs, duration);
        let support = support::assess(&info, &self.host.atmos(id, file), DeviceCaps::default());
        let externals = self
            .host
            .files(id)
            .into_iter()
            .filter(|(i, name)| *i != file && media::kind_of(name) == FileKind::Subtitle)
            .map(|(i, name)| (ExternalSub { id: EXTERNAL_SUB_ID_BASE + i as u64, lang: guess_lang(&name), name }, i))
            .collect();
        Ok(Arc::new(FileHls {
            info,
            spans,
            support: RwLock::new(Arc::new(support)),
            len,
            duration,
            source,
            externals,
            time_index: ti,
            audio_inits: Default::default(),
        }))
    }
}

impl FileHls {
    fn support(&self) -> Arc<Vec<TrackSupport>> {
        self.support.read().unwrap().clone()
    }

    fn spans(&self) -> Result<&Vec<Span>, HlsErr> {
        self.spans.as_ref().map_err(|e| HlsErr::from(*e))
    }

    fn span(&self, n: usize) -> Result<Span, HlsErr> {
        self.spans()?.get(n).copied().ok_or_else(not_found)
    }

    /// Oynatılabilir ilk video izi (master ile aynı seçim) ve paketleme planı.
    fn video(&self) -> Result<(TrackInfo, HlsPlan), HlsErr> {
        let support = self.support();
        let s = support
            .iter()
            .find(|s| s.kind == "video" && s.level != Level::Unsupported)
            .ok_or(PlanError::NoPlayableVideo)?;
        let t = self.info.tracks.iter().find(|t| t.id == s.track_id).ok_or(PlanError::NoPlayableVideo)?;
        if !matches!(t.codec.as_str(), "h264" | "hevc") {
            return Err(PlanError::UnsupportedVideoCodec.into());
        }
        Ok((t.clone(), s.hls.ok_or(PlanError::NoPlayableVideo)?))
    }

    /// Master'da listelenen ses izi (aynı süzgeç) ve planı.
    fn audio(&self, track: u64) -> Option<(TrackInfo, HlsPlan)> {
        let support = self.support();
        let s = support.iter().find(|s| s.kind == "audio" && s.track_id == track && s.level != Level::Unsupported)?;
        let t = self.info.tracks.iter().find(|t| t.id == track)?;
        let plan = s.hls?;
        audio_codec_string(t, Some(plan))?;
        Some((t.clone(), plan))
    }

    /// Master'da listelenen gömülü altyazı izi.
    fn subtitle(&self, track: u64) -> Option<TrackInfo> {
        self.support()
            .iter()
            .find(|s| s.kind == "subtitle" && s.track_id == track && s.level == Level::Full)
            .and_then(|s| self.info.tracks.iter().find(|t| t.id == s.track_id))
            .cloned()
    }

    fn external(&self, id: u64) -> Option<(&ExternalSub, usize)> {
        self.externals.iter().find(|(e, _)| e.id == id).map(|(e, i)| (e, *i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_paths() {
        assert_eq!(parse_req("master.m3u8"), Some(Req::Master));
        assert_eq!(parse_req("v.m3u8"), Some(Req::VideoPlaylist));
        assert_eq!(parse_req("a12.m3u8"), Some(Req::AudioPlaylist(12)));
        assert_eq!(parse_req("s1000003.m3u8"), Some(Req::SubPlaylist(1_000_003)));
        assert_eq!(parse_req("v/init.mp4"), Some(Req::VideoInit));
        assert_eq!(parse_req("v/7.m4s"), Some(Req::VideoSeg(7)));
        assert_eq!(parse_req("a2/init.mp4"), Some(Req::AudioInit(2)));
        assert_eq!(parse_req("a2/0.m4s"), Some(Req::AudioSeg(2, 0)));
        assert_eq!(parse_req("s3/4.vtt"), Some(Req::SubSeg(3, 4)));
        for bad in ["", "x.m3u8", "a.m3u8", "a1/x.m4s", "v/-1.m4s", "v/1.vtt", "s1/1.m4s", "v/a/b.m4s", "../master.m3u8", "v/.m4s"] {
            assert_eq!(parse_req(bad), None, "{bad}");
        }
    }

    #[test]
    fn cache_is_lru_bounded_by_count_and_bytes() {
        let c = HlsCore::default();
        for i in 0..12 {
            c.cache_put((1, 0, format!("v/{i}")), Bytes::from(vec![0u8; 10]));
        }
        assert_eq!(c.cache.lock().unwrap().map.len(), CACHE_SEGMENTS);
        assert!(c.cache_get(&(1, 0, "v/0".into())).is_none());
        assert!(c.cache_get(&(1, 0, "v/11".into())).is_some());
        // bayt tavanı
        let c = HlsCore::default();
        c.cache_put((1, 0, "a".into()), Bytes::from(vec![0u8; 40 * 1024 * 1024]));
        c.cache_put((1, 0, "b".into()), Bytes::from(vec![0u8; 40 * 1024 * 1024]));
        assert!(c.cache_get(&(1, 0, "a".into())).is_none());
        assert!(c.cache_get(&(1, 0, "b".into())).is_some());
        c.invalidate(1);
        assert_eq!(c.cache.lock().unwrap().bytes, 0);
    }

    #[test]
    fn seg_metric_key_and_cpu_split() {
        assert_eq!(SegMetric::parse_key("v/3"), Some(("v".into(), None, 3)));
        assert_eq!(SegMetric::parse_key("a2/17"), Some(("a".into(), Some(2), 17)));
        assert_eq!(SegMetric::parse_key("s1000001/0"), Some(("s".into(), Some(1_000_001), 0)));
        assert_eq!(SegMetric::parse_key("garbage"), None);
        let m = SegMetric::new("a2/3", Duration::from_millis(1500), Duration::from_millis(400), 1234).unwrap();
        assert_eq!((m.kind.as_str(), m.track, m.n, m.bytes), ("a", Some(2), 3, 1234));
        assert_eq!((m.total_ms, m.wait_ms, m.cpu_ms), (1500.0, 400.0, 1100.0));
        // bekleme toplamdan büyük ölçülürse (saat kayması) CPU negatif olmaz
        let m = SegMetric::new("v/0", Duration::from_millis(10), Duration::from_millis(50), 1).unwrap();
        assert_eq!((m.wait_ms, m.cpu_ms), (10.0, 0.0));
    }

    #[test]
    fn metrics_ring_keeps_last_30_oldest_first() {
        let core = HlsCore::new();
        for n in 0..40 {
            core.record(SegMetric::new(&format!("v/{n}"), Duration::from_millis(5), Duration::ZERO, 1).unwrap());
        }
        let r = core.recent_metrics();
        assert_eq!(r.len(), METRICS_KEPT);
        assert_eq!((r[0].n, r[29].n), (10, 39));
    }

    #[tokio::test]
    async fn segment_generation_is_measured_with_wait_share() {
        let core = Arc::new(HlsCore::new());
        let key = (1usize, 0usize, "a2/5".to_string());
        let b = core
            .segment(key, || {
                // BlockingReader'ın beklemesini taklit eder: 30 ms "veri bekleme" + 30 ms "iş".
                let t = Instant::now();
                std::thread::sleep(Duration::from_millis(30));
                add_wait(t);
                std::thread::sleep(Duration::from_millis(30));
                Ok(vec![0u8; 100])
            })
            .await
            .unwrap();
        assert_eq!(b.len(), 100);
        let r = core.recent_metrics();
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].kind.as_str(), r[0].track, r[0].n, r[0].bytes), ("a", Some(2), 5, 100));
        assert!(r[0].wait_ms >= 25.0 && r[0].wait_ms < 55.0, "{:?}", r[0]);
        assert!(r[0].cpu_ms >= 25.0, "{:?}", r[0]);
        assert!(r[0].total_ms >= 55.0, "{:?}", r[0]);
        // önbellekten gelen ikinci istek yeni ölçüm eklemez
        core.segment((1, 0, "a2/5".into()), || Ok(vec![1])).await.unwrap();
        assert_eq!(core.recent_metrics().len(), 1);
    }

    #[test]
    fn language_guess() {
        assert_eq!(guess_lang("Film.2020.tr.srt").as_deref(), Some("tur"));
        assert_eq!(guess_lang("Film.English.srt").as_deref(), Some("eng"));
        assert_eq!(guess_lang("Film.srt"), None);
    }
}
