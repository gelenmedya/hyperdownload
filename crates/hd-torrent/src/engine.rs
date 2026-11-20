//! Torrent motoru: librqbit oturumu, ekleme akışı (önce meta veri, sonra yalnızca izlenecek
//! dosyalar), durum özeti ve ayarlar.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv6Addr, SocketAddr};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use bytes::Bytes;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ListOnlyResponse, ListenerMode,
    ListenerOptions, ManagedTorrent, Session, SessionOptions, SessionPersistenceConfig,
    TorrentStatsState,
};
use serde::{Deserialize, Serialize};

use crate::history::HistoryEntry;
use crate::media::{self, FileKind};
use crate::window_store::WindowStorageFactory;
use librqbit::storage::StorageFactoryExt;

/// Atmos taraması: başarısız taramalar arasındaki taban bekleme ve bir taramanın toplam süre sınırı.
const ATMOS_RETRY_GAP: Duration = Duration::from_secs(5);
const ATMOS_SCAN_TIMEOUT: Duration = Duration::from_secs(30);

/// Arayüz penceresine giden istekler (öne getir, tam ekran).
#[derive(Debug, Clone)]
pub enum UiEvent {
    Focus,
    Fullscreen(bool),
    /// Gömülü başlık çubuğundan gelen pencere komutu (min, max, close, drag, resize:<yön>).
    Window(String),
}

pub type UiNotify = Arc<dyn Fn(UiEvent) + Send + Sync>;

/// Depolama kipi. `Disk`: librqbit dosya sistemi arka ucu (masaüstü). `Window`: parça başına
/// dosya + kayan pencere tahliyesi (iOS), bkz. `window_store`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMode {
    Disk,
    Window,
}

/// Motor yapılandırması (platform tarafından verilir).
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub data_dir: PathBuf,
    pub default_download_dir: PathBuf,
    pub storage: StorageMode,
    pub listen_port: u16,
}

/// `/api/state` yanıtına eklenecek platforma özgü alanlar (ör. harici oynatıcılar).
pub type StateExtras = Box<dyn Fn() -> serde_json::Value + Send + Sync>;

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub download_dir: PathBuf,
    /// Meta veri gelince indirmeye başlamadan dosya seçimini kullanıcıya sor.
    #[serde(default)]
    pub ask_files: bool,
    /// Harici oynatıcıda (mpv) sesi çözmeden AV alıcısına gönder: TrueHD Atmos, DTS:X, DD+ Atmos.
    #[serde(default)]
    pub passthrough: bool,
    /// HyperDownload: ask for the files of every torrent that has more than one (torwatch asked only when it had two videos or more).
    #[serde(default)]
    pub ask_all_files: bool,
}

#[derive(Clone, Serialize)]
pub struct ChoiceFile {
    pub idx: usize,
    pub name: String,
    pub len: u64,
    pub kind: FileKind,
}

/// Kullanıcının seçmesini bekleyen dosya listesi (önerilen seçim işaretli gelir).
#[derive(Clone, Serialize)]
pub struct PendingChoice {
    pub files: Vec<ChoiceFile>,
    pub suggested: Vec<usize>,
}

/// Meta verisi henüz çözülmemiş (DHT/izleyici bekleyen) ekleme.
#[derive(Clone, Serialize)]
pub struct Pending {
    pub key: u64,
    pub name: String,
    pub error: Option<String>,
    pub choice: Option<PendingChoice>,
    #[serde(skip)]
    started: Instant,
    pub elapsed: u64,
}

#[derive(Serialize)]
pub struct FileView {
    pub idx: usize,
    pub name: String,
    pub path: String,
    pub len: u64,
    pub progress: u64,
    pub included: bool,
    pub kind: FileKind,
}

#[derive(Serialize)]
pub struct TorrentView {
    pub id: usize,
    pub info_hash: String,
    pub name: String,
    pub state: &'static str,
    pub error: Option<String>,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub torrent_bytes: u64,
    pub finished: bool,
    pub down: f64,
    pub up: f64,
    pub eta: Option<u64>,
    pub peers: u32,
    pub seen_peers: u32,
    pub file_count: usize,
    pub main_file: Option<usize>,
    pub output_folder: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileView>>,
    /// Temizlenmiş film adı (bkz. `title::display_title`); ana dosya yoksa ham ad.
    pub display_title: String,
    pub year: Option<u16>,
    /// "UHD Remux", "1080p BluRay" gibi; bulunamazsa `None`.
    pub quality: Option<String>,
}

pub struct Engine {
    pub session: Arc<Session>,
    settings: RwLock<Settings>,
    settings_path: PathBuf,
    /// `(kayıtlı, etkin)` indirme klasörü; kayıtlı klasör aynı kaldığı sürece `settings()` etkini gösterir.
    dir_override: Option<(PathBuf, PathBuf)>,
    /// Kayıtlı indirme klasörü açılamadığı için varsayılana düşüldüyse kullanıcıya iletisi.
    download_dir_fallback: Option<String>,
    data_dir: PathBuf,
    pending: Mutex<Vec<Pending>>,
    /// HyperDownload: which torrent an addition (by its pending key) became, so that a caller following the key finds the torrent.
    resolved: Mutex<HashMap<u64, usize>>,
    choosers: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Vec<usize>>>>,
    pending_seq: AtomicU64,
    focus_seq: AtomicU64,
    focus_id: Mutex<Option<usize>>,
    pub restart_needed: std::sync::atomic::AtomicBool,
    pub port: OnceLock<u16>,
    pub state_extras: OnceLock<StateExtras>,
    pub storage: StorageMode,
    /// Pencere kipindeki parça depoları (Disk kipinde boş kalır).
    pub window: Arc<crate::window_store::WindowRegistry>,
    pub ui: UiNotify,
    pub buffering: Arc<crate::buffer::Buffering>,
    media_cache: Mutex<HashMap<(String, usize), Arc<crate::demux::MediaInfo>>>,
    /// Ses izi başına Atmos sonuçları (kesin sonuçlar kalıcı), bkz. [`Engine::atmos`].
    atmos_cache: crate::atmos::AtmosCache,
    /// HLS dosya durumları ve segment önbelleği (`close`/`remove`'da silinir).
    pub hls: Arc<crate::hls::routes::HlsCore>,
    /// İzleme geçmişi (`<data_dir>/history.json`).
    pub history: crate::history::History,
    /// info hash -> eklenen kaynak (magnet URI / base64 .torrent); geçmiş girişi için.
    sources: Mutex<HashMap<String, String>>,
    /// Window kipinde bizim duraklattığımız ve pencere seçimi uygulanana dek tutulan torrent'ler.
    held: Mutex<HashSet<usize>>,
    /// Window kipinde oynatılamaz bulunan torrent'ler (id -> kullanıcıya gösterilecek ileti).
    window_errors: Mutex<HashMap<usize, String>>,
    /// Window kipinde parça dosyaları ilk doğrulama sonucuyla eşlenmiş torrent'ler.
    adopted: Mutex<HashSet<usize>>,
    /// `settle_window` çağrılarını sıraya koyar (eşzamanlı benimseme/başlatma olmasın).
    settle_locks: Mutex<HashMap<usize, Arc<tokio::sync::Mutex<()>>>>,
    /// Kullanıcının duraklattığı torrent'ler (info hash; `paused.json` ile kalıcı). Bizim iç
    /// `held` duraklatmamızdan ayrıdır: yeniden açılışta bunlar pencere uygulansa da duraklatılmış kalır.
    user_paused: Mutex<HashSet<String>>,
    /// `adopt_window`un ilk başarısız denemesi (zaman aşımı iletisi için).
    adopt_started: Mutex<HashMap<usize, std::time::Instant>>,
}

fn load_json_set(path: &Path) -> HashSet<String> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok())
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn load_json_map(path: &Path) -> HashMap<String, String> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Pencere bakımının sonucu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Maintained {
    /// Seçim pencereye daraltıldı (ya da oynatma yönetiyor).
    Applied,
    /// Henüz uygulanmadı (başka bir tick sürüyor); tekrar denenecek.
    Pending,
    /// Oynatılabilir video yok: hiçbir şey indirilmemeli.
    NoVideo,
}

/// Torrent'in ana video dosyası (önce indirilecekler arasından; `view` ile aynı kural).
fn main_video_file(h: &ManagedTorrent) -> Option<usize> {
    let meta = h.metadata.load_full()?;
    let only = h.only_files();
    let names: Vec<(usize, String, u64)> = meta
        .info
        .iter_file_details()
        .enumerate()
        .map(|(i, d)| {
            let p = d.filename.to_string();
            (i, p.rsplit(['/', '\\']).next().unwrap_or(&p).to_string(), d.len)
        })
        .collect();
    let pick = |only_included: bool| {
        media::main_video(
            names
                .iter()
                .filter(|f| !only_included || only.as_ref().is_none_or(|o| o.contains(&f.0)))
                .map(|f| (f.0, f.1.as_str(), f.2)),
        )
    };
    pick(true).or_else(|| pick(false))
}

fn load_settings(path: &Path, default_dir: &Path) -> Settings {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| Settings {
            download_dir: default_dir.to_path_buf(),
            ask_files: false,
            passthrough: false,
            ask_all_files: false,
        })
}

impl Engine {
    pub async fn start(cfg: EngineConfig, ui: UiNotify) -> anyhow::Result<Arc<Self>> {
        let data_dir = cfg.data_dir.as_path();
        let settings_path = data_dir.join("settings.json");
        let settings = load_settings(&settings_path, &cfg.default_download_dir);
        // Kayıtlı `download_dir` (settings.json) ASLA değiştirilmez/üzerine yazılmaz; oturum için ayrı
        // bir etkin klasör kullanılır. iOS'ta kapsayıcı UUID'si her kurulum/güncellemede değişir:
        // kayıtlı mutlak yol yabancı kapsayıcıyı gösterir (EPERM), bu yüzden Window kipinde hiç
        // güvenilmez. Diğer kiplerde kayıtlı klasör açılamazsa varsayılana düşülür (kalıcı değil).
        let persisted_dir = settings.download_dir.clone();
        let mut effective_dir =
            if cfg.storage == StorageMode::Window { cfg.default_download_dir.clone() } else { persisted_dir.clone() };
        let mut download_dir_fallback = None;
        if let Err(e) = std::fs::create_dir_all(&effective_dir) {
            tracing::warn!("indirme klasörü oluşturulamadı ({effective_dir:?}): {e}; varsayılana dönülüyor");
            download_dir_fallback = Some(format!(
                "İndirme klasörü kullanılamıyor ({}); geçici olarak varsayılan klasör kullanılıyor",
                effective_dir.display()
            ));
            effective_dir = cfg.default_download_dir.clone();
            std::fs::create_dir_all(&effective_dir)
                .with_context(|| format!("klasör oluşturulamadı: {effective_dir:?}"))?;
        }
        let dir_override = (effective_dir != persisted_dir).then(|| (persisted_dir, effective_dir.clone()));

        let window = Arc::new(crate::window_store::WindowRegistry::new(data_dir.join("window")));
        let window_mode = cfg.storage == StorageMode::Window;
        // Window kipinde de torrent'ler ve pencere parçaları yeniden açılışta korunur (Task 14b);
        // yalnızca oturumda karşılığı olmayan dizinler aşağıda (oturum kurulunca) temizlenir.
        if window_mode {
            let root = data_dir.join("window");
            std::fs::create_dir_all(&root)
                .with_context(|| format!("pencere klasörü oluşturulamadı: {root:?}"))?;
        }
        let make_opts = |port: u16| SessionOptions {
            // Pencere kipinde fastresume KAPALI: pencere silmeleri have-bitfield'ını bayatlatır; geri
            // yüklenen torrent ilk doğrulamadan (parça dosyalarını hash'leyerek) geçer.
            fastresume: !window_mode,
            persistence: Some(SessionPersistenceConfig::Json { folder: Some(data_dir.join("session")) }),
            // Geri yüklenenler duraklatılmış başlar; pencere uygulanınca bakım döngüsü başlatır.
            restore_paused: window_mode,
            default_storage_factory: window_mode
                .then(|| WindowStorageFactory::new(window.clone()).boxed()),
            listen: Some(ListenerOptions {
                mode: ListenerMode::TcpAndUtp,
                listen_addr: SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)),
                enable_upnp_port_forwarding: true,
                ..Default::default()
            }),
            client_name_and_version: Some(format!("torwatch {}", env!("CARGO_PKG_VERSION"))),
            ..Default::default()
        };

        // Sabit port gelen bağlantılar (ve UPnP) için daha iyi; doluysa rastgele port.
        let session = match Session::new_with_opts(effective_dir.clone(), make_opts(cfg.listen_port)).await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("{} portu açılamadı ({e:#}), rastgele port deneniyor", cfg.listen_port);
                Session::new_with_opts(effective_dir.clone(), make_opts(0)).await?
            }
        };

        // Geri yüklenen torrent'ler: pencere seçimi uygulanana dek tutulur (`add_managed` kuralı) ve
        // oturumda karşılığı olmayan pencere dizinleri silinir.
        let mut restored = HashSet::new();
        if window_mode {
            let found: Vec<(usize, String)> =
                session.with_torrents(|it| it.map(|(id, h)| (id, h.info_hash().as_string())).collect());
            let hashes: HashSet<String> = found.iter().map(|(_, h)| h.clone()).collect();
            restored.extend(found.iter().map(|(id, _)| *id));
            let n = window.remove_orphans(&hashes);
            if n > 0 {
                tracing::info!("{n} yetim pencere girdisi silindi");
            }
        }

        let buffering = Arc::new(crate::buffer::Buffering::new(window_mode.then(|| window.clone())));
        let engine = Arc::new(Self {
            session,
            settings: RwLock::new(settings),
            dir_override,
            download_dir_fallback,
            data_dir: data_dir.to_path_buf(),
            settings_path,
            pending: Mutex::new(Vec::new()),
            resolved: Mutex::new(HashMap::new()),
            choosers: Mutex::new(HashMap::new()),
            pending_seq: AtomicU64::new(1),
            focus_seq: AtomicU64::new(0),
            focus_id: Mutex::new(None),
            restart_needed: Default::default(),
            port: OnceLock::new(),
            state_extras: OnceLock::new(),
            storage: cfg.storage,
            window,
            ui,
            buffering,
            media_cache: Default::default(),
            atmos_cache: Default::default(),
            hls: Default::default(),
            history: crate::history::History::load(data_dir),
            sources: Mutex::new(load_json_map(&data_dir.join("sources.json"))),
            user_paused: Mutex::new(load_json_set(&data_dir.join("paused.json"))),
            adopt_started: Default::default(),
            held: Mutex::new(restored),
            window_errors: Default::default(),
            adopted: Default::default(),
            settle_locks: Default::default(),
        });
        if window_mode {
            engine.spawn_window_maintainer();
        }
        Ok(engine)
    }

    /// Oturumu durdurur (torrent'ler duraklatılır, ağ görevleri biter). Veriler ve oturum kaydı
    /// yerinde kalır; aynı `data_dir` ile yeniden başlatınca torrent'ler geri gelir.
    pub async fn shutdown(&self) {
        self.session.stop().await;
    }

    pub fn settings(&self) -> Settings {
        let mut s = self.settings.read().unwrap().clone();
        if let Some((persisted, effective)) = &self.dir_override {
            if s.download_dir == *persisted {
                s.download_dir = effective.clone();
            }
        }
        s
    }

    /// Kayıtlı indirme klasörü kullanılamadığında geçici olarak varsayılana düşüldüyse iletisi.
    pub fn download_dir_fallback(&self) -> Option<&str> {
        self.download_dir_fallback.as_deref()
    }

    pub fn set_download_dir(&self, dir: PathBuf) -> anyhow::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let mut s = self.settings.write().unwrap();
        if s.download_dir != dir {
            s.download_dir = dir;
            self.restart_needed.store(true, Ordering::Relaxed);
        }
        crate::history::write_atomic(&self.settings_path, &serde_json::to_vec_pretty(&*s)?)?;
        Ok(())
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port.get().copied().unwrap_or(0))
    }

    pub fn stream_url(&self, id: usize, file: usize) -> String {
        format!("{}/api/torrents/{id}/stream/{file}", self.base_url())
    }

    /// Arayüze "bu torrent'i aç" sinyali (yeni eklenen veya dışarıdan gelen).
    fn focus(&self, id: usize) {
        *self.focus_id.lock().unwrap() = Some(id);
        self.focus_seq.fetch_add(1, Ordering::Relaxed);
        (self.ui)(UiEvent::Focus);
    }

    pub fn focus_state(&self) -> (u64, Option<usize>) {
        (
            self.focus_seq.load(Ordering::Relaxed),
            *self.focus_id.lock().unwrap(),
        )
    }

    pub fn pending(&self) -> Vec<Pending> {
        let mut list = self.pending.lock().unwrap();
        // Hatalı eklemeler bir süre görünür kalır, sonra kendiliğinden düşer.
        list.retain(|p| p.error.is_none() || p.started.elapsed() < Duration::from_secs(30));
        list.iter()
            .cloned()
            .map(|mut p| {
                p.elapsed = p.started.elapsed().as_secs();
                p
            })
            .collect()
    }

    pub fn dismiss_pending(&self, key: u64) {
        self.pending.lock().unwrap().retain(|p| p.key != key);
        // Bekleyen seçim varsa kanal kapanır, ekleme "iptal" ile biter.
        self.choosers.lock().unwrap().remove(&key);
    }

    pub fn set_ask_files(&self, on: bool) -> anyhow::Result<()> {
        self.update_settings(|s| s.ask_files = on)
    }

    /// HyperDownload: ask for the files of every torrent with more than one.
    pub fn set_ask_all_files(&self, on: bool) -> anyhow::Result<()> {
        self.update_settings(|s| s.ask_all_files = on)
    }

    /// The torrent the addition with this pending key became, once it has.
    pub fn resolved(&self, key: u64) -> Option<usize> {
        self.resolved.lock().unwrap().get(&key).copied()
    }

    pub fn set_passthrough(&self, on: bool) -> anyhow::Result<()> {
        self.update_settings(|s| s.passthrough = on)
    }

    fn update_settings(&self, f: impl FnOnce(&mut Settings)) -> anyhow::Result<()> {
        let mut s = self.settings.write().unwrap();
        f(&mut s);
        crate::history::write_atomic(&self.settings_path, &serde_json::to_vec_pretty(&*s)?)?;
        Ok(())
    }

    /// Seçim penceresinden gelen dosyalar; beklemedeki eklemeyi sürdürür.
    pub fn choose(&self, key: u64, files: Vec<usize>) -> anyhow::Result<()> {
        anyhow::ensure!(!files.is_empty(), "en az bir dosya seçilmeli");
        let tx = self.choosers.lock().unwrap().remove(&key).context("seçim artık beklemiyor")?;
        if let Some(p) = self.pending.lock().unwrap().iter_mut().find(|p| p.key == key) {
            p.choice = None;
        }
        tx.send(files).map_err(|_| anyhow::anyhow!("ekleme iptal edilmiş"))
    }

    async fn ask_choice(&self, key: u64, files: &[(String, u64)], suggested: Option<Vec<usize>>) -> anyhow::Result<Vec<usize>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.choosers.lock().unwrap().insert(key, tx);
        let choice = PendingChoice {
            files: files
                .iter()
                .enumerate()
                .map(|(idx, (path, len))| ChoiceFile {
                    idx,
                    name: path.clone(),
                    len: *len,
                    kind: media::kind_of(path),
                })
                .collect(),
            suggested: suggested.unwrap_or_else(|| (0..files.len()).collect()),
        };
        if let Some(p) = self.pending.lock().unwrap().iter_mut().find(|p| p.key == key) {
            p.choice = Some(choice);
        }
        (self.ui)(UiEvent::Focus);
        rx.await.map_err(|_| anyhow::anyhow!("İptal edildi"))
    }

    /// Ekleme arka planda sürer: magnet'te meta veri birkaç saniye ile dakikalar arası sürebilir.
    pub fn add(self: &Arc<Self>, source: AddSource) -> u64 {
        let key = self.pending_seq.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().unwrap().push(Pending {
            key,
            name: source.display_name(),
            error: None,
            choice: None,
            started: Instant::now(),
            elapsed: 0,
        });
        let this = self.clone();
        tokio::spawn(async move {
            let res = this.add_inner(key, source.clone()).await;
            let mut list = this.pending.lock().unwrap();
            match res {
                Ok(id) => {
                    this.resolved.lock().unwrap().insert(key, id);
                    this.record_source(id, &source);
                    list.retain(|p| p.key != key);
                    drop(list);
                    this.focus(id);
                }
                Err(e) => {
                    tracing::warn!("torrent eklenemedi: {e:#}");
                    if let Some(p) = list.iter_mut().find(|p| p.key == key) {
                        p.error = Some(friendly_error(&e));
                        p.started = Instant::now();
                    }
                }
            }
        });
        key
    }

    async fn add_inner(&self, key: u64, source: AddSource) -> anyhow::Result<usize> {
        let listing = self
            .session
            .add_torrent(
                source.to_add(),
                Some(AddTorrentOptions {
                    list_only: true,
                    ..Default::default()
                }),
            )
            .await?;
        let ListOnlyResponse {
            info,
            seen_peers,
            torrent_bytes,
            ..
        } = match listing {
            AddTorrentResponse::ListOnly(r) => r,
            AddTorrentResponse::AlreadyManaged(id, _) | AddTorrentResponse::Added(id, _) => {
                return Ok(id);
            }
        };

        let files: Vec<(String, u64)> = info
            .iter_file_details()
            .map(|d| (d.filename.to_string(), d.len))
            .collect();
        let suggested = media::pick_files(&files);
        // Yalnızca ≥2 aday video varsa sor; tek video + altyazı/nfo için öneri doğrudan uygulanır.
        let s = self.settings();
        let only_files = if s.ask_files && files.len() > 1 && (s.ask_all_files || media::needs_choice(&files)) {
            let chosen = self.ask_choice(key, &files, suggested).await?;
            (chosen.len() < files.len()).then_some(chosen)
        } else {
            suggested
        };

        self.add_managed(torrent_bytes, only_files, seen_peers).await
    }

    /// Meta verisi hazır torrent'i oturuma ekler. Window kipinde duraklatılmış eklenir, pencere
    /// bakımı (yalnızca meta + devam konumu çevresi) kurulur ve sonra başlatılır; böylece film
    /// oynatılmadan da bütün dosya indirilmez.
    pub async fn add_managed(
        &self,
        torrent_bytes: Bytes,
        only_files: Option<Vec<usize>>,
        initial_peers: Vec<SocketAddr>,
    ) -> anyhow::Result<usize> {
        let window = self.storage == StorageMode::Window;
        let res = self
            .session
            .add_torrent(
                AddTorrent::from_bytes(torrent_bytes),
                Some(AddTorrentOptions {
                    only_files,
                    overwrite: true,
                    initial_peers: (!initial_peers.is_empty()).then_some(initial_peers),
                    paused: window,
                    ..Default::default()
                }),
            )
            .await?;
        match res {
            AddTorrentResponse::Added(id, h) => {
                if window {
                    // Pencere seçimi uygulanana dek duraklatılmış kalır (tam seçimle ASLA başlamaz);
                    // uygulanamazsa bakım döngüsü yeniden dener ve başarınca başlatır.
                    self.held.lock().unwrap().insert(id);
                    let ready = tokio::time::timeout(Duration::from_secs(30), h.wait_until_initialized()).await;
                    if matches!(ready, Ok(Ok(()))) {
                        self.settle_window(id).await;
                    } else {
                        tracing::warn!("torrent başlatılamadı; duraklatılmış tutuluyor, bakım yeniden deneyecek");
                    }
                }
                Ok(id)
            }
            AddTorrentResponse::AlreadyManaged(id, _) => Ok(id),
            AddTorrentResponse::ListOnly(_) => anyhow::bail!("beklenmeyen yanıt"),
        }
    }

    /// Pencere seçimi uygulanmadığı için bizim duraklattığımız torrent mi? Böyle bir torrent
    /// kullanıcı tarafından başlatılamaz (tam seçimle indirmeye düşerdi).
    pub fn window_blocked(&self, id: usize) -> Option<String> {
        if let Some(m) = self.window_errors.lock().unwrap().get(&id) {
            return Some(m.clone());
        }
        self.held.lock().unwrap().contains(&id).then(|| "Torrent hazırlanıyor, biraz bekle".to_string())
    }

    /// Window kipinde hafif pencere bakımı: ana video için seçimi meta + devam konumu çevresine
    /// daraltır (bkz. [`crate::buffer::Buffering::maintain`]). Disk kipinde no-op.
    pub async fn maintain_window(&self, id: usize) -> anyhow::Result<Maintained> {
        if self.storage != StorageMode::Window {
            return Ok(Maintained::Applied);
        }
        let h = self.handle(id)?;
        anyhow::ensure!(h.metadata.load_full().is_some(), "meta veri yok");
        let Some(file) = self.window_video_file(&h) else { return Ok(Maintained::NoVideo) };
        let (secs, dur) = self
            .history
            .get(&h.info_hash().as_string())
            .filter(|e| e.file == file)
            .map(|e| (e.position_secs, e.duration_secs))
            .unwrap_or((0.0, None));
        Ok(if self.buffering.maintain(h, id, file, secs, dur).await? { Maintained::Applied } else { Maintained::Pending })
    }

    /// Pencerelenecek video: geçmişteki dosya geçerli bir videoysa o (en son oynatılan), değilse ana video.
    fn window_video_file(&self, h: &ManagedTorrent) -> Option<usize> {
        let meta = h.metadata.load_full()?;
        let hist = self.history.get(&h.info_hash().as_string()).map(|e| e.file);
        if let Some(f) = hist {
            let valid = meta.file_infos.get(f).is_some_and(|fi| !fi.attrs.padding)
                && meta
                    .info
                    .iter_file_details()
                    .nth(f)
                    .is_some_and(|d| media::kind_of(&d.filename.to_string()) == media::FileKind::Video);
            if valid {
                return Some(f);
            }
        }
        main_video_file(h)
    }

    /// Bakımı dener ve sonucu işler: uygulandıysa tutulan torrent başlatılır; oynatılabilir video
    /// yoksa torrent duraklatılır (hiçbir şey indirilmez) ve kullanıcıya bir ileti gösterilir;
    /// aksi halde duraklatılmış kalır, döngü yeniden dener. Tam seçimle indirmeye ASLA düşmez.
    async fn settle_window(&self, id: usize) {
        // Torrent başına kilit: bir torrent'in uzun bakımı diğerlerini bekletmez.
        let lock = self.settle_locks.lock().unwrap().entry(id).or_default().clone();
        let _serial = lock.lock().await;
        if !self.adopt_window(id) {
            return; // ilk doğrulama sürüyor; hiçbir şey başlatılmaz
        }
        match self.maintain_window(id).await {
            Ok(Maintained::Applied) => {
                self.window_errors.lock().unwrap().remove(&id);
                if self.held.lock().unwrap().remove(&id) {
                    if let Ok(h) = self.handle(id) {
                        // Kullanıcının duraklattığı torrent pencere/benimseme sonrası da duraklatılmış kalır.
                        let user_paused = self.user_paused.lock().unwrap().contains(&h.info_hash().as_string());
                        if !user_paused {
                            if let Err(e) = self.session.unpause(&h).await {
                                tracing::warn!("torrent başlatılamadı: {e:#}");
                            }
                        }
                    }
                }
            }
            Ok(Maintained::NoVideo) => {
                // Yalnızca bizim tuttuğumuz (add_managed ile eklenen, duraklatılmış) torrent'ler: seçim
                // hiç uygulanmaz, torrent duraklatılmış kalır ve kullanıcıya ileti gösterilir. (Oturuma
                // doğrudan eklenenlere dokunulmaz; üretimde tüm eklemeler add_managed'dan geçer.)
                if self.held.lock().unwrap().contains(&id) {
                    self.window_errors
                        .lock()
                        .unwrap()
                        .insert(id, "Bu torrentte oynatılabilir video yok".into());
                }
            }
            Ok(Maintained::Pending) => {}
            Err(e) => tracing::debug!("pencere bakımı: {e:#}"),
        }
    }

    /// Yeniden açılışta: ilk doğrulama bitince (torrent duraklatılmış durumda) hash'i tutan
    /// parça dosyalarını `WindowStore`un `complete` (tahliye edilebilir) kümesine alır, geri
    /// kalanını siler. `false`: doğrulama henüz bitmedi. Oturumda canlı eklenmiş (indirme süren)
    /// torrent'lerde yapılacak bir şey yoktur (`on_piece_completed` zaten işler).
    fn adopt_window(&self, id: usize) -> bool {
        if self.adopted.lock().unwrap().contains(&id) {
            return true;
        }
        let Ok(h) = self.handle(id) else { return false };
        match h.stats().state {
            TorrentStatsState::Paused => {
                let hash = h.info_hash().as_string();
                let Some(store) = self.window.store(&hash) else { return true };
                let Ok((bf, n)) = librqbit::api::Api::new(self.session.clone(), None)
                    .api_dump_haves(librqbit::api::TorrentIdOrHash::Id(id))
                else {
                    self.adopt_failed(id, "parça bilgisi okunamadı");
                    return false;
                };
                let have: Vec<bool> = (0..n as usize).map(|i| bf.get(i).map(|b| *b).unwrap_or(false)).collect();
                let (kept, removed) = store.adopt_verified(&have);
                tracing::info!(%hash, kept, removed, "pencere parçaları doğrulandı");
            }
            TorrentStatsState::Live => {}
            TorrentStatsState::Error => {
                self.adopt_failed(id, "torrent hata durumunda");
                return false;
            }
            _ => return false,
        }
        self.adopted.lock().unwrap().insert(id);
        self.adopt_started.lock().unwrap().remove(&id);
        true
    }

    /// Benimseme ~30 sn boyunca ilerleyemezse log + kullanıcıya Türkçe ileti (bir kez).
    fn adopt_failed(&self, id: usize, why: &str) {
        let since = *self.adopt_started.lock().unwrap().entry(id).or_insert_with(std::time::Instant::now);
        if since.elapsed() >= Duration::from_secs(30) {
            let mut errs = self.window_errors.lock().unwrap();
            if !errs.contains_key(&id) {
                tracing::warn!(id, "pencere doğrulaması tamamlanamadı: {why}");
                errs.insert(id, "Torrent doğrulanamadı; kaldırıp yeniden ekle".into());
            }
        }
    }

    /// Window kipinde oturumdaki her torrent için bakımı sürdürür (eklenme yolundan bağımsız;
    /// ör. oturumda başka yoldan eklenenler).
    fn spawn_window_maintainer(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let Some(e) = weak.upgrade() else { break };
                let ids: Vec<usize> = e.session.with_torrents(|it| it.map(|(id, _)| id).collect());
                for id in ids {
                    e.settle_window(id).await;
                }
            }
        });
    }

    /// Dosyayı indirme kümesine ekler (oynatmadan önce) veya kümeyi baştan belirler.
    pub async fn select_files(&self, id: usize, files: &[usize], replace: bool) -> anyhow::Result<()> {
        let handle = self.handle(id)?;
        let mut set: HashSet<usize> = if replace {
            HashSet::new()
        } else {
            match handle.only_files() {
                Some(v) => v.into_iter().collect(),
                None => return Ok(()), // zaten hepsi indiriliyor
            }
        };
        let before = set.len();
        set.extend(files.iter().copied());
        if !replace && set.len() == before {
            return Ok(());
        }
        if set.is_empty() {
            anyhow::bail!("en az bir dosya seçilmeli");
        }
        handle.wait_until_initialized().await?;
        self.session.update_only_files(&handle, &set).await?;
        // Daraltılan seçim pencere denetleyicisinde önbellekte kalmasın: bir sonraki tick yeniden gönderir.
        let b = self.buffering.clone();
        let _ = tokio::task::spawn_blocking(move || b.reset_selection(id)).await;
        Ok(())
    }

    pub fn handle(&self, id: usize) -> anyhow::Result<Arc<ManagedTorrent>> {
        self.session
            .get(librqbit::api::TorrentIdOrHash::Id(id))
            .context("torrent bulunamadı")
    }

    pub fn list(&self) -> Vec<TorrentView> {
        let ids: Vec<usize> = self
            .session
            .with_torrents(|it| it.map(|(id, _)| id).collect());
        let mut out: Vec<TorrentView> = ids.into_iter().filter_map(|id| self.view(id, false)).collect();
        out.sort_by(|a, b| b.id.cmp(&a.id));
        out
    }

    pub fn view(&self, id: usize, with_files: bool) -> Option<TorrentView> {
        let h = self.handle(id).ok()?;
        let stats = h.stats();
        let shared = h.shared();
        let only = h.only_files();
        let meta = h.metadata.load_full();

        let mut file_list: Vec<(usize, String, String, u64)> = Vec::new();
        if let Some(m) = meta.as_ref() {
            for (idx, d) in m.info.iter_file_details().enumerate() {
                let path = d.filename.to_string();
                let name = path
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(&path)
                    .to_string();
                file_list.push((idx, name, path, d.len));
            }
        }
        let included = |idx: usize| only.as_ref().map(|o| o.contains(&idx)).unwrap_or(true);
        // Önce indirilenler arasından, yoksa tümünden ana video.
        let candidates = |only_included: bool| {
            file_list
                .iter()
                .filter(move |f| !only_included || included(f.0))
                .map(|f| (f.0, f.1.as_str(), f.3))
        };
        let main_file = media::main_video(candidates(true)).or_else(|| media::main_video(candidates(false)));
        let torrent_bytes = file_list.iter().map(|f| f.3).sum();

        let torrent_name = h.name().unwrap_or_else(|| shared.info_hash.as_string());
        // MKV başlığı yalnızca önbellekteyse kullanılır (burada ağ/okuma yapılmaz).
        let (display_title, year, quality) = match main_file.and_then(|f| file_list.iter().find(|x| x.0 == f)) {
            Some((idx, fname, _, _)) => {
                let mkv_title = self
                    .media_cache
                    .lock()
                    .unwrap()
                    .get(&(shared.info_hash.as_string(), *idx))
                    .and_then(|i| i.title.clone());
                crate::title::display_title(&torrent_name, fname, mkv_title.as_deref())
            }
            None => (torrent_name.clone(), None, None),
        };

        let (down, up, eta, peers, seen) = match &stats.live {
            Some(l) => (
                l.download_speed.mbps,
                l.upload_speed.mbps,
                l.time_remaining.as_ref().and_then(|t| {
                    serde_json::to_value(t)
                        .ok()
                        .and_then(|v| v.get("duration")?.get("secs")?.as_u64())
                }),
                l.snapshot.peer_stats.live,
                l.snapshot.peer_stats.seen,
            ),
            None => (0.0, 0.0, None, 0, 0),
        };
        let state = match stats.state {
            TorrentStatsState::Initializing { .. } => "initializing",
            TorrentStatsState::Live => "live",
            TorrentStatsState::Paused => "paused",
            TorrentStatsState::Error => "error",
        };

        let files = with_files.then(|| {
            file_list
                .iter()
                .map(|(idx, name, path, len)| FileView {
                    idx: *idx,
                    name: name.clone(),
                    path: path.clone(),
                    len: *len,
                    progress: stats.file_progress.get(*idx).copied().unwrap_or(0),
                    included: included(*idx),
                    kind: media::kind_of(name),
                })
                .collect()
        });

        Some(TorrentView {
            id,
            info_hash: shared.info_hash.as_string(),
            name: torrent_name,
            state,
            error: stats.error.clone().or_else(|| self.window_errors.lock().unwrap().get(&id).cloned()),
            progress_bytes: stats.progress_bytes,
            total_bytes: stats.total_bytes,
            torrent_bytes,
            finished: stats.finished,
            down,
            up,
            eta,
            peers,
            seen_peers: seen,
            file_count: file_list.len(),
            main_file,
            output_folder: h.output_folder().to_string_lossy().into_owned(),
            files,
            display_title,
            year,
            quality,
        })
    }

    /// Kapsayıcı bilgisi (iz listesi, kodekler); torrent akışı üzerinden saf Rust okunur, önbelleğe alınır.
    pub async fn media_info(&self, id: usize, file: usize) -> anyhow::Result<Arc<crate::demux::MediaInfo>> {
        let h = self.handle(id)?;
        let key = (h.info_hash().as_string(), file);
        if let Some(i) = self.media_cache.lock().unwrap().get(&key) {
            return Ok(i.clone());
        }
        self.select_files(id, &[file], false).await?;
        let reader = tokio_util::io::SyncIoBridge::new(h.stream(file).await?);
        let info = tokio::time::timeout(
            Duration::from_secs(90),
            tokio::task::spawn_blocking(move || crate::audio::probe(reader)),
        )
        .await
        .context("kapsayıcı bilgisi zaman aşımına uğradı")???;
        let info = Arc::new(info);
        self.media_cache.lock().unwrap().insert(key, info.clone());
        Ok(info)
    }

    /// Ses izi başına Atmos durumu (iz numarası -> durum), bit akışından tespit edilir.
    ///
    /// Asla engellemez: sonuçlar önbellekten döner; kesin olmayan izler `Unknown` kalır ve arka
    /// planda taranır (varsayılan ses izi önce, sonra diğerleri). Tarama yalnızca diskte doğrulanmış
    /// baytları okur (bitfield + Window kipinde parça deposu); eksik bayt bloklamaz, hata verir ve
    /// iz `Unknown` kalır, hiçbir parça indirtilmez. Önce dosya başından, olmazsa oynatma
    /// konumundan (Window kipinde/geçmişten devamda baş tahliye edilmiş olabilir) denenir.
    /// Kesin sonuçlar (info hash, dosya) başına kalıcıdır; `Unknown` olanlar artan aralıklarla ve
    /// artan paket sayısıyla yeniden denenir, oynatma konumu değişince sayaç sıfırlanır. Tarama
    /// zaman sınırlıdır ve torrent kapanınca iptal edilir. Kapsayıcı bilgisi henüz yoksa boş harita
    /// döner ve onu da arka planda getirir.
    pub fn atmos(self: &Arc<Self>, id: usize, file: usize) -> HashMap<u64, crate::atmos::AtmosState> {
        use crate::atmos::AtmosState;
        let Ok(h) = self.handle(id) else { return HashMap::new() };
        let key = (h.info_hash().as_string(), file);
        let info = self.media_cache.lock().unwrap().get(&key).cloned();
        let states = info.as_ref().map(|i| self.atmos_cache.states(&key, &i.tracks)).unwrap_or_default();
        let unresolved = info.is_none() || states.values().any(|s| *s == AtmosState::Unknown);
        if unresolved && tokio::runtime::Handle::try_current().is_ok() {
            let pos = self.buffering.playhead_secs(id, file).unwrap_or(0.0);
            if let Some(ticket) = self.atmos_cache.try_begin(&key, Instant::now(), ATMOS_RETRY_GAP, pos) {
                let me = self.clone();
                tokio::spawn(async move { me.atmos_scan(id, file, key, ticket, pos).await });
            }
        }
        states
    }

    async fn atmos_scan(self: Arc<Self>, id: usize, file: usize, key: crate::atmos::CacheKey, ticket: crate::atmos::ScanTicket, pos: f64) {
        use crate::atmos::{HaveReader, ReadSeek, ScanGuard, ScanParams};
        let _end = ScanGuard::new(&self.atmos_cache, &key); // scanning her yolda temizlenir
        let prepared = async {
            let info = self.media_info(id, file).await?;
            let h = self.handle(id)?;
            let layout = Arc::new(crate::buffer::Layout::read(&self.session, &h, id, file)?);
            let store = (self.storage == StorageMode::Window).then(|| self.window.store(&key.0)).flatten();
            let path = self.file_path(id, file);
            anyhow::Ok((info, layout, store, path))
        };
        let (info, layout, store, path) = match prepared.await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("atmos taraması hazırlanamadı: {e:#}");
                return;
            }
        };
        if ticket.cancel.load(Ordering::Relaxed) {
            return;
        }
        let len = layout.len;
        let open = |cancel: &Arc<std::sync::atomic::AtomicBool>| -> Option<HaveReader<Box<dyn ReadSeek>>> {
            let inner: Box<dyn ReadSeek> = match &store {
                Some(s) => Box::new(crate::window_policy::StoreReader::new(s.clone(), file, len)),
                None => Box::new(std::fs::File::open(path.as_ref()?).ok()?),
            };
            let l = layout.clone();
            Some(HaveReader::new(inner, len, cancel.clone(), Box::new(move |a, b| l.has_range(a, b))))
        };
        // Oynatma konumu bilinen yerdeyse önce orası (veri kesin orada), sonra baş.
        let starts = if pos > 1.0 { vec![pos, 0.0] } else { vec![0.0] };
        let params = ScanParams {
            starts,
            max_packets: crate::atmos::DEFAULT_MAX_PACKETS << ticket.attempt.min(4),
            timeout: ATMOS_SCAN_TIMEOUT,
        };
        crate::atmos::run_scan(&self.atmos_cache, &key, self.atmos_cache.pending(&key, &info.tracks), &params, ticket.cancel, open).await;
    }

    pub async fn probe(&self, id: usize, file: usize) -> anyhow::Result<serde_json::Value> {
        let len = self
            .view(id, true)
            .and_then(|v| v.files)
            .and_then(|f| f.into_iter().find(|x| x.idx == file).map(|x| x.len))
            .unwrap_or(0);
        Ok(crate::audio::info_json(&*self.media_info(id, file).await?, len))
    }

    /// Tanılama özeti (`GET /api/diag`): pencere baytları, torrent başına durum, motor bilgisi.
    /// `data_dir_bytes` (özyinelemeli klasör boyutu) async çalışma zamanını tıkamamak için
    /// engelleyici iş parçacığında hesaplanır.
    pub async fn diag(&self) -> serde_json::Value {
        let mut total = 0u64;
        let mut per = Vec::new();
        for v in self.list() {
            let store = (self.storage == StorageMode::Window).then(|| self.window.store(&v.info_hash)).flatten();
            let (window_bytes, pieces_kept, keep_ranges) = match &store {
                Some(s) => (s.bytes_on_disk(), s.pieces_on_disk(), s.keep_range_count()),
                None => (0, 0, 0),
            };
            total += window_bytes;
            let playhead = v.main_file.and_then(|f| self.buffering.playhead_secs(v.id, f));
            per.push(serde_json::json!({
                "id": v.id,
                "name": v.name,
                "window_bytes": window_bytes,
                "pieces_kept": pieces_kept,
                "peers": v.peers,
                "down": v.down,
                "up": v.up,
                "state": v.state,
                "playhead_secs": playhead,
                "keep_alive_ranges": keep_ranges,
            }));
        }
        let dir = self.data_dir.clone();
        let data_dir_bytes = tokio::task::spawn_blocking(move || dir_size(&dir)).await.unwrap_or(0);
        serde_json::json!({
            "window_bytes_total": total,
            "per_torrent": per,
            "engine": {
                "version": env!("CARGO_PKG_VERSION"),
                "storage_mode": match self.storage { StorageMode::Window => "window", StorageMode::Disk => "disk" },
                "port": self.port.get().copied(),
                "data_dir_bytes": data_dir_bytes,
            },
            // Son 30 HLS segment üretimi (tür, süre, veri bekleme, CPU payı, bayt); eski istemciler yok sayar.
            "hls_recent": self.hls.recent_metrics(),
        })
    }

    /// İndirme klasörünün bulunduğu diskte kullanılabilir alan.
    pub fn disk(&self) -> serde_json::Value {
        let dir = self.settings().download_dir;
        serde_json::json!({
            "free": fs4::available_space(&dir).ok(),
            "total": fs4::total_space(&dir).ok(),
            "drive": dir.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()),
        })
    }

    /// Torrent'i kaldırır; dosyalar da silinecekse diskten gerçekten gittiğini doğrular ve
    /// boşalan alanı ölçer. Başka bir programın kilitlediği dosyalar arka planda yeniden denenir.
    pub async fn remove(self: &Arc<Self>, id: usize, delete_files: bool) -> anyhow::Result<serde_json::Value> {
        let view = self.view(id, true).context("torrent bulunamadı")?;
        let paths: Vec<PathBuf> = if delete_files {
            (0..view.file_count).filter_map(|f| self.file_path(id, f)).filter(|p| p.exists()).collect()
        } else {
            Vec::new()
        };
        let root = PathBuf::from(&view.output_folder);
        let download_dir = self.settings().download_dir;
        let before = fs4::available_space(&download_dir).unwrap_or(0);

        // Bizim açık tuttuğumuz akışlar (önden okuma) silmeden önce kapanır.
        self.buffering.stop_torrent(id);
        self.held.lock().unwrap().remove(&id);
        self.settle_locks.lock().unwrap().remove(&id);
        self.adopted.lock().unwrap().remove(&id);
        self.set_user_paused(&view.info_hash, false);
        self.window_errors.lock().unwrap().remove(&id);
        self.adopt_started.lock().unwrap().remove(&id);
        self.atmos_cache.cancel_hash(&view.info_hash);
        self.hls.invalidate(id);
        self.media_cache.lock().unwrap().retain(|(hash, _), _| *hash != view.info_hash);
        self.session
            .delete(librqbit::api::TorrentIdOrHash::Id(id), delete_files)
            .await?;

        let mut leftover = Vec::new();
        for p in &paths {
            if p.exists() && std::fs::remove_file(p).is_err() && p.exists() {
                leftover.push(p.clone());
            }
        }
        // Çok dosyalı torrent'in boş kalan klasörü (indirme klasörünün kendisi değil).
        if delete_files && root != download_dir && root.starts_with(&download_dir) {
            let _ = remove_empty_dirs(&root);
        }
        if !leftover.is_empty() {
            let left = leftover.clone();
            tokio::spawn(async move {
                // Kilit (oynatıcı, antivirüs taraması) kalkınca silinir: 10 dakika boyunca dene.
                let mut left = left;
                for _ in 0..120 {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    left.retain(|p| p.exists() && std::fs::remove_file(p).is_err() && p.exists());
                    if left.is_empty() {
                        break;
                    }
                }
                if !left.is_empty() {
                    tracing::warn!("silinemeyen dosyalar: {left:?}");
                }
            });
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let after = fs4::available_space(&download_dir).unwrap_or(0);
        Ok(serde_json::json!({
            "deleted_files": delete_files,
            "expected_bytes": view.progress_bytes,
            "freed_bytes": after.saturating_sub(before),
            "leftover": leftover.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
            "disk": self.disk(),
        }))
    }


    // ---------- film yaşam döngüsü ----------

    fn record_source(&self, id: usize, source: &AddSource) {
        if let Ok(h) = self.handle(id) {
            let mut g = self.sources.lock().unwrap();
            g.insert(h.info_hash().as_string(), source.history_source());
            // Yeniden açılışta .torrent ile eklenen torrent'in gerçek kaynağı kaybolmasın.
            self.save_sources(&g);
        }
    }

    fn save_sources(&self, g: &HashMap<String, String>) {
        match serde_json::to_vec(g) {
            Ok(b) => {
                if let Err(e) = crate::history::write_atomic(&self.data_dir.join("sources.json"), &b) {
                    tracing::warn!("sources.json yazılamadı: {e}");
                }
            }
            Err(e) => tracing::warn!("sources.json serileştirilemedi: {e}"),
        }
    }

    /// Kaynak kaydını siler (geçmiş kendi kaynağını tutar). Torrent hâlâ oturumdaysa dokunmaz.
    pub(crate) fn forget_source(&self, hash: &str) {
        let live = self
            .session
            .with_torrents(|it| it.map(|(_, t)| t.info_hash().as_string()).any(|h| h == hash));
        if live {
            return;
        }
        let mut g = self.sources.lock().unwrap();
        if g.remove(hash).is_some() {
            self.save_sources(&g);
        }
    }

    /// Geçmişten girişi siler ve artık gereksiz kaynak kaydını temizler.
    pub fn history_remove(&self, hash: &str) {
        self.history.remove(hash);
        self.forget_source(hash);
    }

    fn set_user_paused(&self, hash: &str, paused: bool) {
        let mut g = self.user_paused.lock().unwrap();
        let changed = if paused { g.insert(hash.to_string()) } else { g.remove(hash) };
        if changed {
            match serde_json::to_vec(&g.iter().collect::<Vec<_>>()) {
                Ok(b) => {
                    if let Err(e) = crate::history::write_atomic(&self.data_dir.join("paused.json"), &b) {
                        tracing::warn!("paused.json yazılamadı: {e}");
                    }
                }
                Err(e) => tracing::warn!("paused.json serileştirilemedi: {e}"),
            }
        }
    }

    /// Kullanıcı duraklatması: kalıcı olarak hatırlanır (yeniden açılışta duraklatılmış kalır).
    pub async fn pause_torrent(&self, id: usize) -> anyhow::Result<()> {
        let h = self.handle(id)?;
        self.set_user_paused(&h.info_hash().as_string(), true);
        self.session.pause(&h).await
    }

    /// Kullanıcı başlatması. Pencere hazırlanırken (tutulan torrent) başlatılamaz.
    pub async fn start_torrent(&self, id: usize) -> anyhow::Result<()> {
        let h = self.handle(id)?;
        if let Some(msg) = self.window_blocked(id) {
            anyhow::bail!(msg);
        }
        self.set_user_paused(&h.info_hash().as_string(), false);
        self.session.unpause(&h).await
    }

    /// Oynatma konumu bildirimi: geçmişi günceller ve tamponlama playhead'ini taşır.
    pub async fn position(&self, id: usize, u: PositionUpdate) -> anyhow::Result<()> {
        let h = self.handle(id)?;
        let hash = h.info_hash().as_string();
        let prev = self.history.get(&hash);
        // Alan JSON'da yoksa kayıtlı değer korunur; açık `null` temizler.
        let duration_secs = u.duration_secs.or(prev.as_ref().and_then(|p| p.duration_secs));
        let audio_track = u.audio_track.unwrap_or(prev.as_ref().and_then(|p| p.audio_track));
        let subtitle_track = u.subtitle_track.unwrap_or(prev.as_ref().and_then(|p| p.subtitle_track));
        self.history.upsert(HistoryEntry {
            info_hash: hash.clone(),
            name: h.name().unwrap_or_else(|| hash.clone()),
            source: self.source_for(&hash),
            file: u.file,
            position_secs: u.secs.max(0.0),
            duration_secs,
            audio_track,
            subtitle_track,
            updated_unix: crate::history::now_unix(),
        });
        let bitrate = self.buffering.known_bitrate(id, u.file);
        self.buffering
            .report(&self.session, h, id, u.file, u.secs.max(0.0), duration_secs, bitrate, 0)?;
        Ok(())
    }

    fn source_for(&self, hash: &str) -> String {
        self.sources
            .lock()
            .unwrap()
            .get(hash)
            .cloned()
            .or_else(|| self.history.get(hash).map(|e| e.source))
            .unwrap_or_else(|| format!("magnet:?xt=urn:btih:{hash}"))
    }

    /// Filmi kapatır: geçmişte girişi garanti eder, torrenti oturumdan çıkarır. Window kipinde
    /// `window/<info_hash>/` silinir. Disk kipinde (masaüstü) indirilen dosyalara DOKUNULMAZ:
    /// yalnızca torrent durdurulup oturumdan çıkarılır.
    pub async fn close(self: &Arc<Self>, id: usize) -> anyhow::Result<()> {
        let h = self.handle(id)?;
        let hash = h.info_hash().as_string();
        if self.history.get(&hash).is_none() {
            let main = self.view(id, false).and_then(|v| v.main_file).unwrap_or(0);
            self.history.upsert(HistoryEntry {
                info_hash: hash.clone(),
                name: h.name().unwrap_or_else(|| hash.clone()),
                source: self.source_for(&hash),
                file: main,
                position_secs: 0.0,
                duration_secs: None,
                audio_track: None,
                subtitle_track: None,
                updated_unix: crate::history::now_unix(),
            });
        }
        self.buffering.stop_torrent(id);
        self.held.lock().unwrap().remove(&id);
        self.settle_locks.lock().unwrap().remove(&id);
        self.adopted.lock().unwrap().remove(&id);
        self.set_user_paused(&hash, false);
        self.window_errors.lock().unwrap().remove(&id);
        self.adopt_started.lock().unwrap().remove(&id);
        self.atmos_cache.cancel_hash(&hash);
        self.hls.invalidate(id);
        self.media_cache.lock().unwrap().retain(|(k, _), _| *k != hash);
        self.session.delete(librqbit::api::TorrentIdOrHash::Id(id), false).await?;
        drop(h);
        // Geçmiş girişi kendi kaynağını tutar; sources.json'daki kayıt artık gereksiz.
        self.forget_source(&hash);
        if self.storage == StorageMode::Window {
            // Uçuştaki bir yazma klasörü kısa süre meşgul edebilir: birkaç kez dene.
            let dir = self.window.root().join(&hash);
            for _ in 0..20 {
                self.window.remove(&hash);
                if !dir.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if dir.exists() {
                tracing::warn!("pencere klasörü silinemedi: {dir:?}");
            }
        }
        Ok(())
    }

    /// Oynatma konumunu `range`e taşır ve pencereyi bir sonraki tick'i beklemeden uygular
    /// (pencerenin dışına geri/ileri sarma). Disk kipinde no-op.
    pub async fn ensure_range(&self, id: usize, file: usize, range: Range<u64>) -> anyhow::Result<()> {
        if self.storage != StorageMode::Window {
            return Ok(());
        }
        let h = self.handle(id)?;
        self.buffering.ensure_range(&self.session, h, id, file, range).await
    }

    /// Dosyanın diskteki yolu (klasörde göster / harici oynatıcı için).
    pub fn file_path(&self, id: usize, file: usize) -> Option<PathBuf> {
        let h = self.handle(id).ok()?;
        let meta = h.metadata.load_full()?;
        let d = meta.info.iter_file_details().nth(file)?;
        let mut p = h.output_folder().to_path_buf();
        for c in d.filename.iter_components() {
            p.push(&*c);
        }
        Some(p)
    }

    pub fn file_name(&self, id: usize, file: usize) -> Option<String> {
        let h = self.handle(id).ok()?;
        let meta = h.metadata.load_full()?;
        let d = meta.info.iter_file_details().nth(file)?;
        let s = d.filename.to_string();
        Some(s.rsplit(['/', '\\']).next().unwrap_or(&s).to_string())
    }
}

/// Klasör ağacındaki dosyaların toplam baytı (sembolik bağlara girmez).
fn dir_size(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.filter_map(|e| e.ok())
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// Klasör ağacında yalnızca boş klasörleri siler (dosya içeren hiçbir şeye dokunmaz).
fn remove_empty_dirs(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        if p.is_dir() {
            let _ = remove_empty_dirs(&p);
        }
    }
    std::fs::remove_dir(dir)
}

fn friendly_error(e: &anyhow::Error) -> String {
    let s = format!("{e:#}");
    if s.contains("not a valid magnet") || s.contains("error decoding torrent") {
        "Geçerli bir magnet linki veya .torrent dosyası değil".into()
    } else if s.contains("no known way to resolve peers") {
        "Eş bulunamadı: linkte izleyici yok ve DHT kapalı".into()
    } else {
        s
    }
}

/// Eklenecek kaynak: magnet / http(s) .torrent adresi ya da dosya içeriği.
#[derive(Clone)]
pub enum AddSource {
    Url(String),
    Bytes { name: String, data: Bytes },
}

impl AddSource {
    fn to_add(&self) -> AddTorrent<'static> {
        match self {
            AddSource::Url(u) => AddTorrent::from_url(u.clone()),
            AddSource::Bytes { data, .. } => AddTorrent::from_bytes(data.clone()),
        }
    }

    /// Geçmişe yazılacak kaynak: magnet/adres olduğu gibi, .torrent ise base64.
    fn history_source(&self) -> String {
        use base64::Engine as _;
        match self {
            AddSource::Url(u) => u.clone(),
            AddSource::Bytes { data, .. } => base64::engine::general_purpose::STANDARD.encode(data),
        }
    }

    fn display_name(&self) -> String {
        match self {
            AddSource::Url(u) if u.starts_with("magnet:") => u
                .split(['?', '&'])
                .find_map(|kv| kv.strip_prefix("dn="))
                .map(|dn| percent_decode(&dn.replace('+', " ")))
                .unwrap_or_else(|| "Magnet linki".into()),
            AddSource::Url(u) => u.rsplit('/').next().unwrap_or(u).to_string(),
            AddSource::Bytes { name, .. } => name.clone(),
        }
    }

    /// Komut satırı / ikinci örnekten gelen argüman: magnet, adres, bilgi özeti veya dosya yolu.
    pub fn from_arg(arg: &str) -> anyhow::Result<Self> {
        let arg = arg.trim().trim_matches('"');
        let lower = arg.to_ascii_lowercase();
        if lower.starts_with("magnet:") || lower.starts_with("http://") || lower.starts_with("https://") {
            return Ok(Self::Url(arg.to_string()));
        }
        if arg.len() == 40 && arg.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(Self::Url(format!("magnet:?xt=urn:btih:{arg}")));
        }
        let path = Path::new(arg);
        let data = std::fs::read(path).with_context(|| format!("dosya okunamadı: {arg}"))?;
        Ok(Self::Bytes {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| arg.into()),
            data: data.into(),
        })
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Yoksa `None`, açık `null` ise `Some(None)`, değer varsa `Some(Some(v))`.
fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// `POST /api/torrents/{id}/position` gövdesi. İz alanları: yok = kayıtlı seçimi koru,
/// `null` = temizle, sayı = ayarla.
#[derive(Debug, Clone, Deserialize)]
pub struct PositionUpdate {
    pub file: usize,
    pub secs: f64,
    #[serde(default)]
    pub duration_secs: Option<f64>,
    #[serde(default, deserialize_with = "double_option")]
    pub audio_track: Option<Option<u64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub subtitle_track: Option<Option<u64>>,
}
