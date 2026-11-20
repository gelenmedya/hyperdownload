//! Test yardımcısı: aynı süreçte yerel bir tohumlayıcı oturum + `WindowStorage` kullanan bir
//! izleyici oturum, 127.0.0.1 üzerinde. DHT, izleyici (tracker) ve LSD kapalı; izleyici,
//! tohumlayıcıyı `initial_peers` ile bulur. İnternet gerekmez. Geçici klasörler `Swarm`
//! düşünce silinir.
//!
//! Task 5/6/17 testleri de bunu kullanır: `Swarm::start(...)`, `swarm.leech_all().await`.
#![allow(dead_code)]

use std::io::SeekFrom;
use std::net::SocketAddr;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::storage::StorageFactoryExt;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerMode, ListenerOptions,
    ManagedTorrent, Session, SessionOptions, create_torrent,
};
use rand::{RngCore, SeedableRng, rngs::StdRng};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use hd_torrent::engine::{Engine, EngineConfig, StorageMode};
use hd_torrent::window_store::{WindowRegistry, WindowStorageFactory, WindowStore};

pub const MIB: u64 = 1024 * 1024;
pub const PIECE: u32 = MIB as u32;
/// Varsayılan düzen: 64 MiB toplam, parça sınırına oturmayan iki dosya.
/// Tek dosyalı 64 MiB düzen (tahliye testleri: başka dosya karışmaz).
pub const SINGLE_LAYOUT: [u64; 1] = [64 * MIB];
pub const DEFAULT_LAYOUT: [u64; 2] = [40 * MIB + 512 * 1024, 23 * MIB + 512 * 1024];

pub struct Swarm {
    pub seeder: Arc<Session>,
    pub leecher: Arc<Session>,
    pub registry: Arc<WindowRegistry>,
    pub seeder_addr: SocketAddr,
    pub torrent_bytes: Bytes,
    /// Tohumlayıcı dosyaları (torrent dosya sırasıyla).
    pub seed_files: Vec<PathBuf>,
    /// Tohumlayıcının 'ı (içerik burada).
    pub seed_out: PathBuf,
    /// İzleyicinin `<window>` kökü (`registry` buraya yazar).
    pub window_root: PathBuf,
    _tmp: TempDir,
}

fn session_opts(factory: Option<librqbit::storage::BoxStorageFactory>) -> SessionOptions {
    SessionOptions {
        dht: None,
        disable_trackers: true,
        disable_local_service_discovery: true,
        ipv4_only: true,
        fastresume: false,
        persistence: None,
        listen: Some(ListenerOptions {
            mode: ListenerMode::TcpOnly,
            listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            enable_upnp_port_forwarding: false,
            ipv4_only: true,
            ..Default::default()
        }),
        default_storage_factory: factory,
        ..Default::default()
    }
}

/// Deterministik sözde rastgele veri.
pub fn random_bytes(len: u64, seed: u64) -> Vec<u8> {
    let mut v = vec![0u8; len as usize];
    StdRng::seed_from_u64(seed).fill_bytes(&mut v);
    v
}

impl Swarm {
    /// `layout`: dosya boyutları (bayt). Parça uzunluğu 1 MiB.
    pub async fn start(layout: &[u64]) -> Swarm {
        let files: Vec<Vec<u8>> =
            layout.iter().enumerate().map(|(i, len)| random_bytes(*len, 1000 + i as u64)).collect();
        Self::start_with_files(&files).await
    }

    /// Verilen dosya içerikleriyle (ör. gerçek bir MKV).
    pub async fn start_with_files(files: &[Vec<u8>]) -> Swarm {
        Self::start_with_files_ext(files, "bin").await
    }

    /// Tanınan video uzantılı (ör. `mkv`) rastgele dosyalarla: motorun "ana video" kuralı çalışır.
    pub async fn start_video(layout: &[u64]) -> Swarm {
        let files: Vec<Vec<u8>> =
            layout.iter().enumerate().map(|(i, len)| random_bytes(*len, 1000 + i as u64)).collect();
        Self::start_with_files_ext(&files, "mkv").await
    }

    pub async fn start_with_files_ext(files: &[Vec<u8>], ext: &str) -> Swarm {
        let named: Vec<(String, Vec<u8>)> = files.iter().enumerate().map(|(i, d)| (format!("f{i}.{ext}"), d.clone())).collect();
        Self::start_named(&named).await
    }

    /// Dosya adı + içerik (ad sırası torrent dosya sırasını belirler): ör. `f0.mkv` + `f1.srt`.
    pub async fn start_named(files: &[(String, Vec<u8>)]) -> Swarm {
        let tmp = tempfile::tempdir().expect("tempdir");
        let content = tmp.path().join("content").join("media");
        std::fs::create_dir_all(&content).unwrap();
        let mut seed_files = Vec::new();
        for (name, data) in files.iter() {
            let p = content.join(name);
            std::fs::write(&p, data).unwrap();
            seed_files.push(p);
        }
        let created = create_torrent(
            &content,
            CreateTorrentOptions {
                name: Some("media"),
                trackers: vec![],
                piece_length: Some(PIECE),
            },
            &BlockingSpawner::new(2),
        )
        .await
        .expect("create_torrent");
        let torrent_bytes = created.as_bytes().expect("torrent bytes");

        let seeder = Session::new_with_opts(tmp.path().join("seed-out"), session_opts(None))
            .await
            .expect("seeder session");
        let added = seeder
            .add_torrent(
                AddTorrent::from_bytes(torrent_bytes.clone()),
                Some(AddTorrentOptions {
                    output_folder: Some(created.output_folder.to_string_lossy().into_owned()),
                    overwrite: true,
                    disable_trackers: true,
                    ..Default::default()
                }),
            )
            .await
            .expect("seeder add");
        let h = added.into_handle().expect("seeder handle");
        tokio::time::timeout(Duration::from_secs(60), h.wait_until_completed())
            .await
            .expect("seeder check timeout")
            .expect("seeder check");
        let seeder_addr = seeder.listen_addr().expect("seeder listen addr");

        let window_root = tmp.path().join("leech-data").join("window");
        let registry = Arc::new(WindowRegistry::new(window_root.clone()));
        let leecher = Session::new_with_opts(
            tmp.path().join("leech-out"),
            session_opts(Some(WindowStorageFactory::new(registry.clone()).boxed())),
        )
        .await
        .expect("leecher session");

        Swarm {
            seeder,
            leecher,
            registry,
            seeder_addr,
            torrent_bytes,
            seed_files,
            seed_out: created.output_folder.clone(),
            window_root,
            _tmp: tmp,
        }
    }

    pub async fn start_default() -> Swarm {
        Self::start(&DEFAULT_LAYOUT).await
    }

    /// Torrent'i izleyiciye ekler (tüm dosyalar seçili) ve başlatma doğrulaması bitene dek
    /// bekler. İndirme kendiliğinden başlar; bitişi `leech_all` bekler.
    pub async fn add_leecher(&self) -> Arc<ManagedTorrent> {
        let r = self
            .leecher
            .add_torrent(
                AddTorrent::from_bytes(self.torrent_bytes.clone()),
                Some(AddTorrentOptions {
                    initial_peers: Some(vec![self.seeder_addr]),
                    disable_trackers: true,
                    ..Default::default()
                }),
            )
            .await
            .expect("leecher add");
        let h = r.into_handle().expect("leecher handle");
        tokio::time::timeout(Duration::from_secs(30), h.wait_until_initialized())
            .await
            .expect("leecher init timeout")
            .expect("leecher init");
        h
    }

    /// Duraklatılmış ekler: ilk doğrulama biter, indirme başlamaz. Test, seçimi (`apply_window`)
    /// indirme başlamadan ayarlayıp `leecher.unpause` ile başlatır.
    pub async fn add_leecher_paused(&self) -> (Arc<ManagedTorrent>, Arc<WindowStore>) {
        let r = self
            .leecher
            .add_torrent(
                AddTorrent::from_bytes(self.torrent_bytes.clone()),
                Some(AddTorrentOptions {
                    initial_peers: Some(vec![self.seeder_addr]),
                    disable_trackers: true,
                    paused: true,
                    ..Default::default()
                }),
            )
            .await
            .expect("leecher add");
        let h = r.into_handle().expect("leecher handle");
        tokio::time::timeout(Duration::from_secs(30), h.wait_until_initialized())
            .await
            .expect("leecher init timeout")
            .expect("leecher init");
        let store = self
            .registry
            .store(&h.shared().info_hash.as_string())
            .expect("window store registered");
        (h, store)
    }

    /// Ekler ve tüm veriyi indirir; `(handle, store)` döner.
    pub async fn leech_all(&self) -> (Arc<ManagedTorrent>, Arc<WindowStore>) {
        let h = self.add_leecher().await;
        tokio::time::timeout(Duration::from_secs(90), h.wait_until_completed())
            .await
            .expect("download timeout")
            .expect("download");
        let store = self
            .registry
            .store(&h.shared().info_hash.as_string())
            .expect("window store registered");
        (h, store)
    }

    /// İkinci bir tohumlayıcı oturum: aynı içeriği tutar ve `target` adresine kendisi bağlanır
    /// (hedefte `initial_peers` bilgisi olmayan, yeniden başlatılmış torrent'ler için).
    pub async fn add_seeder_dialing(&self, target: SocketAddr) -> Arc<Session> {
        let out = self.seed_out.clone();
        let sess = Session::new_with_opts(self._tmp.path().join("seed2-out"), session_opts(None))
            .await
            .expect("seeder2 session");
        sess.add_torrent(
            AddTorrent::from_bytes(self.torrent_bytes.clone()),
            Some(AddTorrentOptions {
                output_folder: Some(out.to_string_lossy().into_owned()),
                overwrite: true,
                initial_peers: Some(vec![target]),
                disable_trackers: true,
                ..Default::default()
            }),
        )
        .await
        .expect("seeder2 add");
        sess
    }

    /// Üçüncü bir süreç içi eş: yalnızca izleyiciden (leecher) indirmeye çalışır, tohumlayıcıyı
    /// bilmez. Dönen oturum yaşadığı sürece eş bağlı kalır/yeniden dener.
    pub async fn add_third_peer(&self) -> (Arc<Session>, Arc<ManagedTorrent>) {
        let sess = Session::new_with_opts(self._tmp.path().join("third-out"), session_opts(None))
            .await
            .expect("third session");
        let r = sess
            .add_torrent(
                AddTorrent::from_bytes(self.torrent_bytes.clone()),
                Some(AddTorrentOptions {
                    initial_peers: Some(vec![self.leecher.listen_addr().expect("leecher addr")]),
                    disable_trackers: true,
                    ..Default::default()
                }),
            )
            .await
            .expect("third add");
        (sess, r.into_handle().expect("third handle"))
    }

    /// `Engine`'i Window kipinde başlatır (`data_dir` altında `window/`, `history.json`).
    pub async fn window_engine(&self, data_dir: &std::path::Path) -> Arc<Engine> {
        let cfg = EngineConfig {
            data_dir: data_dir.to_path_buf(),
            default_download_dir: data_dir.join("downloads"),
            storage: StorageMode::Window,
            listen_port: 0,
        };
        Engine::start(cfg, Arc::new(|_| {})).await.expect("engine start")
    }

    /// Torrent'i motorun oturumuna tohumlayıcıyla ekler; `(id, handle)` döner.
    pub async fn engine_add(&self, e: &Arc<Engine>) -> (usize, Arc<ManagedTorrent>) {
        let r = e
            .session
            .add_torrent(
                AddTorrent::from_bytes(self.torrent_bytes.clone()),
                Some(AddTorrentOptions {
                    initial_peers: Some(vec![self.seeder_addr]),
                    disable_trackers: true,
                    ..Default::default()
                }),
            )
            .await
            .expect("engine add");
        let h = r.into_handle().expect("engine handle");
        tokio::time::timeout(Duration::from_secs(30), h.wait_until_initialized())
            .await
            .expect("engine init timeout")
            .expect("engine init");
        (h.id(), h)
    }

    /// Torrent'in info hash'i (40 karakter, küçük harf).
    pub fn info_hash(&self) -> String {
        self.seeder
            .with_torrents(|it| it.map(|(_, h)| h.info_hash().as_string()).next())
            .expect("seeder torrent")
    }

    /// Tohumlayıcı dosyasından bayt aralığı (referans veri).
    pub fn seed_bytes(&self, file: usize, range: Range<u64>) -> Vec<u8> {
        use std::io::{Read, Seek};
        let mut f = std::fs::File::open(&self.seed_files[file]).unwrap();
        f.seek(SeekFrom::Start(range.start)).unwrap();
        let mut v = vec![0u8; (range.end - range.start) as usize];
        f.read_exact(&mut v).unwrap();
        v
    }
}

/// Bir dosya akışı açıp `range` baytlarını okur (en çok `secs` saniye bekler).
pub async fn read_via_stream(
    h: &Arc<ManagedTorrent>,
    file: usize,
    range: Range<u64>,
    secs: u64,
) -> anyhow::Result<Vec<u8>> {
    let mut s = h.clone().stream(file).await?;
    s.seek(SeekFrom::Start(range.start)).await?;
    let mut v = vec![0u8; (range.end - range.start) as usize];
    tokio::time::timeout(Duration::from_secs(secs), s.read_exact(&mut v)).await??;
    Ok(v)
}

/// Parça dosyalarının numaraları (sıralı), diskten okunur.
pub fn pieces_on_disk(dir: &std::path::Path) -> Vec<u32> {
    let mut v: Vec<u32> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                e.file_name()
                    .to_str()?
                    .strip_suffix(".bin")?
                    .parse::<u32>()
                    .ok()
            })
            .collect(),
        Err(_) => vec![],
    };
    v.sort_unstable();
    v
}

/// `cond` doğru olana dek en çok `secs` saniye yoklar.
pub async fn wait_for(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    cond()
}

/// `Engine` oturumları sabit DHT portunu (UDP) bağlar: Engine kullanan testler sırayla çalışır.
pub async fn engine_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static L: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    L.lock().await
}
