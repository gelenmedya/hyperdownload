//! Yeniden açılışta kalıcılık (Task 14b): torrent'ler ve pencereleri korunur; geri yüklenenler
//! de `add_managed` ile eklenenlerle aynı kurala (duraklatılmış başla, pencere uygulanınca devam)
//! tabidir.

mod support;

use std::sync::Arc;
use std::time::Duration;

use librqbit::ManagedTorrent;
use support::*;
use hd_torrent::engine::Engine;
use hd_torrent::history::HistoryEntry;

fn downloaded(h: &Arc<ManagedTorrent>) -> u64 {
    h.stats().live.map(|l| l.snapshot.downloaded_and_checked_bytes).unwrap_or(0)
}

/// Bilinen süre: boşta pencere (-10 sn, +60 sn) ~ 4 MiB; başlık 8 MiB.
fn seed_history(engine: &Arc<Engine>, hash: &str) {
    engine.history.upsert(HistoryEntry {
        info_hash: hash.to_string(),
        name: "x".into(),
        source: format!("magnet:?xt=urn:btih:{hash}"),
        file: 0,
        position_secs: 0.0,
        duration_secs: Some(960.0),
        audio_track: None,
        subtitle_track: None,
        updated_unix: 1,
    });
}

/// Motoru kapatır ve bırakır (sabit DHT portu serbest kalsın).
async fn stop(engine: Arc<Engine>) {
    engine.shutdown().await;
    drop(engine);
    tokio::time::sleep(Duration::from_millis(500)).await;
}

fn window_dir(data: &std::path::Path, hash: &str) -> std::path::PathBuf {
    data.join("window").join(hash)
}

fn have(engine: &Arc<Engine>, id: usize) -> Vec<bool> {
    librqbit::api::Api::new(engine.session.clone(), None)
        .api_dump_haves(librqbit::api::TorrentIdOrHash::Id(id))
        .map(|(bf, n)| (0..n as usize).map(|i| bf.get(i).map(|b| *b).unwrap_or(false)).collect())
        .unwrap()
}

/// Motor 1'de pencere iner; yeniden açılışta torrent listede, parçalar diskte ve (tohumlayıcı
/// kapalıyken bile) pencere içi aralık bayt bayt okunabilir.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_keeps_torrent_and_window() {
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let hash = sw.info_hash();
    seed_history(&engine, &hash);
    let id = engine.add_managed(sw.torrent_bytes.clone(), None, vec![sw.seeder_addr]).await.unwrap();
    let h = engine.handle(id).unwrap();
    assert!(wait_for(60, || h.stats().finished).await, "pencere inmedi");
    drop(h);
    let dir = window_dir(data.path(), &hash);
    let before = pieces_on_disk(&dir);
    assert!(!before.is_empty());
    stop(engine).await;
    sw.seeder.stop().await; // tohumlayıcı kapalı

    let engine = sw.window_engine(data.path()).await;
    let list = engine.list();
    assert_eq!(list.len(), 1, "torrent geri gelmeli");
    assert_eq!(list[0].info_hash, hash);
    let id = list[0].id;
    let h = engine.handle(id).unwrap();
    assert!(
        wait_for(60, || engine.window_blocked(id).is_none() && h.stats().finished).await,
        "pencere doğrulanıp torrent devam etmedi"
    );
    assert_eq!(pieces_on_disk(&dir), before, "pencere parçaları korunmalı");
    let store = engine.window.store(&hash).expect("depo");
    let mut got = vec![0u8; MIB as usize];
    store.pread_exact(0, 0, &mut got).expect("pencere içi okunabilmeli");
    assert_eq!(got, sw.seed_bytes(0, 0..MIB));
    assert_eq!(downloaded(&h), 0, "ağdan bir şey inmemeli");
}

/// Oturumda karşılığı olmayan `<hash>` dizinleri ve artık dosyalar silinir; geçerli torrent'in
/// dizini, `history.json` ve `settings.json` kalır.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_removes_only_orphan_window_dirs() {
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let hash = sw.info_hash();
    seed_history(&engine, &hash);
    let id = engine.add_managed(sw.torrent_bytes.clone(), None, vec![sw.seeder_addr]).await.unwrap();
    let h = engine.handle(id).unwrap();
    assert!(wait_for(60, || h.stats().finished).await);
    drop(h);
    stop(engine).await;

    std::fs::write(data.path().join("settings.json"), b"{}").unwrap();
    let orphan = data.path().join("window").join("deadbeef");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join("0.bin"), b"leftover").unwrap();
    std::fs::write(data.path().join("window").join("stray.bin"), b"x").unwrap();

    let engine = sw.window_engine(data.path()).await;
    assert!(!orphan.exists(), "yetim dizin silinmeli");
    assert!(!data.path().join("window").join("stray.bin").exists());
    assert!(window_dir(data.path(), &hash).exists(), "geçerli torrent dizini kalmalı");
    assert!(!pieces_on_disk(&window_dir(data.path(), &hash)).is_empty());
    assert!(data.path().join("history.json").exists());
    assert!(data.path().join("settings.json").exists());
    assert_eq!(engine.list().len(), 1);
}

/// Oynatılabilir video yoksa geri yüklenen torrent de duraklatılmış kalır ve ileti görünür.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restored_torrent_without_video_stays_paused() {
    let _g = engine_lock().await;
    let sw = Swarm::start(&SINGLE_LAYOUT).await; // .bin: video değil
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let id = engine.add_managed(sw.torrent_bytes.clone(), None, vec![sw.seeder_addr]).await.unwrap();
    assert_eq!(engine.view(id, false).unwrap().error.as_deref(), Some("Bu torrentte oynatılabilir video yok"));
    stop(engine).await;

    let engine = sw.window_engine(data.path()).await;
    let list = engine.list();
    assert_eq!(list.len(), 1);
    let id = list[0].id;
    let h = engine.handle(id).unwrap();
    assert!(
        wait_for(20, || engine.view(id, false).unwrap().error.as_deref()
            == Some("Bu torrentte oynatılabilir video yok"))
        .await,
        "ileti görünmeli"
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(h.is_paused(), "duraklatılmış kalmalı");
    assert_eq!(downloaded(&h), 0);
    assert!(engine.window_blocked(id).is_some(), "kullanıcı başlatamamalı");
}

/// "Kaldır" (close) davranışı değişmez: veri silinir, geçmiş kalır, yeniden açılışta torrent dönmez.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_torrent_does_not_come_back_after_restart() {
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let hash = sw.info_hash();
    seed_history(&engine, &hash);
    let id = engine.add_managed(sw.torrent_bytes.clone(), None, vec![sw.seeder_addr]).await.unwrap();
    let h = engine.handle(id).unwrap();
    assert!(wait_for(60, || h.stats().finished).await);
    drop(h);
    engine.close(id).await.unwrap();
    stop(engine).await;

    let engine = sw.window_engine(data.path()).await;
    assert!(engine.list().is_empty(), "kapatılan torrent geri gelmemeli");
    assert!(!window_dir(data.path(), &hash).exists());
    assert_eq!(engine.history.list().len(), 1, "geçmiş kalmalı");
}

/// Bozuk parça dosyasına güvenilmez: yeniden açılışta o parça "var" sayılmaz ve dosyası silinir;
/// sağlam parçalar korunur.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_piece_file_is_not_trusted_after_restart() {
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let hash = sw.info_hash();
    seed_history(&engine, &hash);
    let id = engine.add_managed(sw.torrent_bytes.clone(), None, vec![sw.seeder_addr]).await.unwrap();
    let h = engine.handle(id).unwrap();
    assert!(wait_for(60, || h.stats().finished).await);
    drop(h);
    let dir = window_dir(data.path(), &hash);
    let before = pieces_on_disk(&dir);
    assert!(before.len() >= 2, "{before:?}");
    stop(engine).await;

    let bad = before[1];
    let path = dir.join(format!("{bad}.bin"));
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    let engine = sw.window_engine(data.path()).await;
    let id = engine.list()[0].id;
    assert!(wait_for(60, || engine.window_blocked(id).is_none()).await, "pencere uygulanmadı");
    let bf = have(&engine, id);
    assert!(!bf[bad as usize], "bozuk parça var sayıldı");
    assert!(bf[before[0] as usize], "sağlam parça kaybolmuş");
    assert!(!path.exists(), "bozuk parça dosyası silinmeli");
}

/// Kayıtlı seçim tüm dosyayı kapsasa bile geri yüklenen torrent pencere uygulanmadan indirmez:
/// açılışta duraklatılmıştır; pencere uygulanıp başlayınca yalnızca pencere iner.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restored_torrent_never_downloads_outside_window() {
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let hash = sw.info_hash();
    seed_history(&engine, &hash);
    // Eş yok: hiçbir şey inmez; kalıcı kayıtta tam seçim (only_files yok) durur.
    let id = engine.add_managed(sw.torrent_bytes.clone(), None, vec![]).await.unwrap();
    assert!(engine.handle(id).is_ok());
    stop(engine).await;

    let engine = sw.window_engine(data.path()).await;
    let id = engine.list()[0].id;
    let h = engine.handle(id).unwrap();
    // Açılış döner dönmez: duraklatılmış ve tutulmuş.
    assert!(h.is_paused(), "geri yüklenen torrent duraklatılmış başlamalı");
    assert!(engine.window_blocked(id).is_some(), "pencere uygulanana dek tutulmalı");
    assert!(wait_for(30, || engine.window_blocked(id).is_none()).await, "pencere uygulanmadı");

    let port = engine.session.listen_addr().expect("listen").port();
    let _seed2 = sw.add_seeder_dialing(std::net::SocketAddr::from(([127, 0, 0, 1], port))).await;
    assert!(wait_for(60, || h.stats().finished).await, "pencere inmedi");
    tokio::time::sleep(Duration::from_secs(4)).await;
    let limit = 10 * MIB;
    assert!(downloaded(&h) > 0 && downloaded(&h) <= limit, "indirilen: {}", downloaded(&h));
    let pieces = pieces_on_disk(&window_dir(data.path(), &hash));
    assert!(pieces.iter().all(|p| (*p as u64) < 10), "uzak parça var: {pieces:?}");
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().filter_map(|e| e.ok()) {
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

/// Kayıtlı `download_dir` yabancı/yazılamaz bir mutlak yolsa (iOS kapsayıcı UUID'si değişti) motor
/// yine başlar ve varsayılan klasörü kullanır.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_persisted_download_dir_falls_back() {
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    // Düz bir dosyanın "altında" klasör oluşturulamaz: her platformda hata verir.
    let blocker = data.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let stale = blocker.join("Containers/OLD-UUID/downloads");
    let json = serde_json::json!({ "download_dir": stale, "ask_files": false, "passthrough": false });
    std::fs::write(data.path().join("settings.json"), serde_json::to_vec(&json).unwrap()).unwrap();

    let engine = sw.window_engine(data.path()).await;
    assert_eq!(engine.settings().download_dir, data.path().join("downloads"));
}

/// `data_dir` taşınınca (kapsayıcı UUID'si değişti) torrent'ler ve pencereler yeni konumda bulunur.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moved_data_dir_still_restores_torrents_and_windows() {
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&SINGLE_LAYOUT).await;
    let old = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(old.path()).await;
    let hash = sw.info_hash();
    seed_history(&engine, &hash);
    let id = engine.add_managed(sw.torrent_bytes.clone(), None, vec![sw.seeder_addr]).await.unwrap();
    let h = engine.handle(id).unwrap();
    assert!(wait_for(60, || h.stats().finished).await);
    drop(h);
    let before = pieces_on_disk(&window_dir(old.path(), &hash));
    assert!(!before.is_empty());
    stop(engine).await;
    sw.seeder.stop().await;

    let new = tempfile::tempdir().unwrap();
    copy_dir(old.path(), new.path());
    drop(old); // eski konum silinir

    let engine = sw.window_engine(new.path()).await;
    let list = engine.list();
    assert_eq!(list.len(), 1, "torrent yeni konumda bulunmalı");
    let id = list[0].id;
    let h = engine.handle(id).unwrap();
    assert!(
        wait_for(60, || engine.window_blocked(id).is_none() && h.stats().finished).await,
        "pencere doğrulanmadı"
    );
    assert_eq!(pieces_on_disk(&window_dir(new.path(), &hash)), before);
    assert!(
        std::path::Path::new(&list[0].output_folder).starts_with(new.path()),
        "çıktı yolu yeni konuma türetilmeli: {}",
        list[0].output_folder
    );
}

/// Kullanıcının duraklattığı torrent yeniden açılışta duraklatılmış kalır (pencere + benimseme
/// yine uygulanır); kullanıcı başlatınca devam eder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn user_paused_torrent_stays_paused_after_restart() {
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let hash = sw.info_hash();
    seed_history(&engine, &hash);
    let id = engine.add_managed(sw.torrent_bytes.clone(), None, vec![sw.seeder_addr]).await.unwrap();
    let h = engine.handle(id).unwrap();
    assert!(wait_for(60, || h.stats().finished).await);
    drop(h);
    engine.pause_torrent(id).await.unwrap();
    let before = pieces_on_disk(&window_dir(data.path(), &hash));
    stop(engine).await;

    let engine = sw.window_engine(data.path()).await;
    let id = engine.list()[0].id;
    let h = engine.handle(id).unwrap();
    // Pencere uygulanır (tutma kalkar) ama torrent başlatılmaz.
    assert!(wait_for(60, || engine.window_blocked(id).is_none()).await, "pencere uygulanmadı");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(h.is_paused(), "kullanıcı duraklatması korunmalı");
    assert_eq!(engine.view(id, false).unwrap().state, "paused");
    // Benimseme yine yapıldı: parçalar duruyor ve have'te.
    assert_eq!(pieces_on_disk(&window_dir(data.path(), &hash)), before);
    assert!(have(&engine, id)[before[0] as usize]);
    // Kullanıcı başlatır.
    engine.start_torrent(id).await.unwrap();
    assert!(!h.is_paused());
}

/// Kayıtlı indirme klasörü açılamayan Disk kipi motoru varsayılana düşer; kayıtlı değer
/// settings.json'da KORUNUR (ask_files gibi bir ayar kaydı üzerine yazmaz) ve durum bildirilir.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disk_mode_unavailable_download_dir_falls_back_without_persisting() {
    use hd_torrent::engine::{EngineConfig, StorageMode};
    let _g = engine_lock().await;
    let data = tempfile::tempdir().unwrap();
    let blocker = data.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let stale = blocker.join("usb/Filmler");
    let json = serde_json::json!({ "download_dir": stale, "ask_files": false, "passthrough": false });
    std::fs::write(data.path().join("settings.json"), serde_json::to_vec(&json).unwrap()).unwrap();

    let cfg = EngineConfig {
        data_dir: data.path().to_path_buf(),
        default_download_dir: data.path().join("downloads"),
        storage: StorageMode::Disk,
        listen_port: 0,
    };
    let engine = Engine::start(cfg, Arc::new(|_| {})).await.expect("engine start");
    assert_eq!(engine.settings().download_dir, data.path().join("downloads"));
    let msg = engine.download_dir_fallback().expect("uyarı iletisi");
    assert!(msg.contains("varsayılan"), "{msg}");

    engine.set_ask_files(true).unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(data.path().join("settings.json")).unwrap()).unwrap();
    assert_eq!(saved["download_dir"], serde_json::json!(stale), "kayıtlı klasör korunmalı");
    assert_eq!(saved["ask_files"], serde_json::json!(true));
    stop(engine).await;
}

/// `.torrent` ile eklenen torrent'in gerçek kaynağı (base64) yeniden açılışta korunur.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn torrent_file_source_survives_restart() {
    use base64::Engine as _;
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    engine.add(hd_torrent::engine::AddSource::Bytes {
        name: "film.torrent".into(),
        data: sw.torrent_bytes.clone(),
    });
    assert!(wait_for(30, || !engine.list().is_empty()).await, "eklenmedi");
    stop(engine).await;

    let engine = sw.window_engine(data.path()).await;
    let id = engine.list()[0].id;
    engine.close(id).await.unwrap();
    let want = base64::engine::general_purpose::STANDARD.encode(&sw.torrent_bytes);
    assert_eq!(engine.history.list()[0].source, want);
}
