//! Window kipinde hafif pencere bakımı (R16/R17) ve salt okunur `peek` (Engine düzeyi).

mod support;

use std::sync::Arc;

use support::*;
use hd_torrent::engine::Engine;

fn downloaded(h: &Arc<librqbit::ManagedTorrent>) -> u64 {
    h.stats().live.map(|l| l.snapshot.downloaded_and_checked_bytes).unwrap_or(0)
}

/// Bilinen süre: 64 MiB / 960 sn ~ 68 KB/sn; boşta pencere (-10 sn, +60 sn) ~ 4 MiB, normal pencere
/// (±300 sn) ~ 20 MiB. Geçmişe yazılan süre bakımın bit hızını belirler.
fn seed_history(engine: &Arc<Engine>, hash: &str) {
    engine.history.upsert(hd_torrent::history::HistoryEntry {
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

/// 64 MiB'lik torrent eklenir, hiç oynatılmaz: yalnızca başlık + devam konumu (0) çevresindeki
/// KÜÇÜK boşta pencere iner (R17); film bütünüyle inmez ve pencere deposunda yer kaplamaz.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn added_torrent_downloads_only_initial_window() {
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    seed_history(&engine, &sw.info_hash());
    let id = engine
        .add_managed(sw.torrent_bytes.clone(), None, vec![sw.seeder_addr])
        .await
        .expect("add_managed");
    let h = engine.handle(id).unwrap();
    let dir = data.path().join("window").join(h.shared().info_hash.as_string());

    // Seçili pencere inince torrent "bitmiş" sayılır; sonra birkaç sn daha bekle.
    assert!(wait_for(60, || h.stats().finished).await, "başlangıç penceresi inmedi");
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;

    // Başlık (zaman dizini yokken 8 MiB) + boşta keep (~4 MiB +%20, başlığın içinde) + parça payı.
    // Normal ±300 sn pencere ~24 MiB olurdu.
    let limit = 10 * MIB;
    let got = downloaded(&h);
    let on_disk = engine.window.store(&h.shared().info_hash.as_string()).unwrap().bytes_on_disk();
    assert!(got > 0, "hiç inmedi");
    assert!(got <= limit, "indirilen: {got}");
    assert!(on_disk <= limit, "disk: {on_disk}");
    let pieces = pieces_on_disk(&dir);
    assert!(pieces.iter().all(|p| (*p as u64) < 10), "diskte uzak parça var: {pieces:?}");
    // Önden okuma akışı açılmadı.
    assert!(!engine.buffering.is_active(id, 0));
}

/// Oynatma konum bildirince (`report`) boşta pencere olağan ±300 sn pencereye genişler.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_window_expands_when_played() {
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    seed_history(&engine, &sw.info_hash());
    let id = engine
        .add_managed(sw.torrent_bytes.clone(), None, vec![sw.seeder_addr])
        .await
        .expect("add_managed");
    let h = engine.handle(id).unwrap();
    assert!(wait_for(60, || h.stats().finished).await, "başlangıç penceresi inmedi");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let idle = downloaded(&h);
    assert!(idle <= 10 * MIB, "boşta: {idle}");

    engine.buffering.report(&engine.session, h.clone(), id, 0, 0.0, Some(960.0), None, 0).expect("report");
    assert!(
        wait_for(60, || downloaded(&h) >= 18 * MIB).await,
        "oynatma penceresi genişlemedi: {}",
        downloaded(&h)
    );
    assert!(engine.buffering.is_active(id, 0));
}

/// `peek` konumu kaydetmez ve önden okuma görevi başlatmaz; `report` başlatır.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn buffer_peek_does_not_spawn_prefetch() {
    let _g = engine_lock().await;
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let (id, h) = sw.engine_add(&engine).await;

    let r = engine
        .buffering
        .peek(&engine.session, h.clone(), id, 0, 500.0, Some(3600.0), None, 0)
        .expect("peek");
    assert_eq!(r.file_len, 64 * MIB);
    assert!(r.pos > 0, "peek konumu hesaplar");
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(!engine.buffering.is_active(id, 0), "peek önden okuma başlatmamalı");
    assert_ne!(engine.buffering.playhead_secs(id, 0), Some(500.0), "peek konumu kaydetmemeli");

    engine
        .buffering
        .report(&engine.session, h.clone(), id, 0, 500.0, Some(3600.0), None, 0)
        .expect("report");
    assert!(engine.buffering.is_active(id, 0), "report önden okumayı başlatır");
    assert_eq!(engine.buffering.playhead_secs(id, 0), Some(500.0));
}

/// Oynatılabilir video olmayan torrent Window kipinde hiçbir şey indirmez: duraklatılmış kalır ve
/// kullanıcıya ileti gösterilir.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn window_mode_torrent_without_video_downloads_nothing() {
    let _g = engine_lock().await;
    let sw = Swarm::start(&SINGLE_LAYOUT).await; // f0.bin: video değil
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let id = engine
        .add_managed(sw.torrent_bytes.clone(), None, vec![sw.seeder_addr])
        .await
        .expect("add_managed");
    let h = engine.handle(id).unwrap();
    // Bakım döngüsüne birkaç tur zaman tanı; hâlâ hiçbir şey inmemeli.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert_eq!(downloaded(&h), 0, "video olmayan torrent indirmemeli");
    let on_disk = engine.window.store(&h.shared().info_hash.as_string()).unwrap().bytes_on_disk();
    assert_eq!(on_disk, 0);
    let v = engine.view(id, false).unwrap();
    assert_eq!(v.error.as_deref(), Some("Bu torrentte oynatılabilir video yok"));
}

/// Tutulan (duraklatılmış) torrent yalnızca bakım uygulandıktan sonra başlar; tam seçimle
/// başlamaz. (Başlatma zaman aşımı/bakım hatasında `held` kümesinde kalır ve döngü yeniden dener;
/// `Applied` dışında hiçbir yol unpause etmez: bkz. `Engine::settle_window`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_torrent_starts_only_after_window_applied() {
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let id = engine
        .add_managed(sw.torrent_bytes.clone(), None, vec![sw.seeder_addr])
        .await
        .expect("add_managed");
    let h = engine.handle(id).unwrap();
    assert!(wait_for(60, || h.stats().finished).await);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(downloaded(&h) <= 10 * MIB, "indirilen: {}", downloaded(&h));
}

/// Kullanıcı birden çok video seçmiş olsa bile yalnızca ana video pencerelenir; diğeri
/// oynatılana dek indirilmez.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn other_selected_videos_are_not_downloaded_until_played() {
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&[30 * MIB, 30 * MIB]).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let id = engine
        .add_managed(sw.torrent_bytes.clone(), Some(vec![0, 1]), vec![sw.seeder_addr])
        .await
        .expect("add_managed");
    let h = engine.handle(id).unwrap();
    assert!(wait_for(60, || h.stats().finished).await, "pencere inmedi");
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    let got = downloaded(&h);
    assert!(got > 0 && got <= 12 * MIB, "iki videodan biri bütünüyle inmiş olabilir: {got}");
}

/// Çok videolu torrent: bakım `history.file` (geçerli bir video ise) pencereler; daha önce bakımı
/// yapılmış diğer videonun kayıtlı penceresi bırakılır ve parçaları diskten silinir.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maintenance_follows_history_file_and_releases_other_video() {
    let _g = engine_lock().await;
    // Dosya 0 (40 MiB, ana video) ve dosya 1 (30 MiB); parça 1 MiB: dosya 0 = 0..40, dosya 1 = 40..70.
    let sw = Swarm::start_video(&[40 * MIB, 30 * MIB]).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let hash = sw.info_hash();
    let id = engine
        .add_managed(sw.torrent_bytes.clone(), Some(vec![0, 1]), vec![sw.seeder_addr])
        .await
        .expect("add_managed");
    let h = engine.handle(id).unwrap();
    let store = engine.window.store(&hash).unwrap();
    // Geçmiş yok: ana video (dosya 0) pencerelenir.
    assert!(wait_for(60, || store.kept_files() == vec![0]).await, "dosya 0 pencerelenmedi: {:?}", store.kept_files());
    assert!(wait_for(60, || h.stats().finished).await);

    // Kullanıcı dosya 1'i oynamış: bakım artık dosya 1'i pencereler, dosya 0 bırakılır.
    engine.history.upsert(hd_torrent::history::HistoryEntry {
        info_hash: hash.clone(),
        name: "x".into(),
        source: format!("magnet:?xt=urn:btih:{hash}"),
        file: 1,
        position_secs: 0.0,
        duration_secs: Some(960.0),
        audio_track: None,
        subtitle_track: None,
        updated_unix: 1,
    });
    assert!(wait_for(60, || store.kept_files() == vec![1]).await, "kayıtlı pencereler: {:?}", store.kept_files());
    let dir = data.path().join("window").join(&hash);
    assert!(
        wait_for(30, || {
            let p = pieces_on_disk(&dir);
            !p.is_empty() && p.iter().all(|x| *x >= 40)
        })
        .await,
        "dosya 0 parçaları silinmedi: {:?}",
        pieces_on_disk(&dir)
    );
}

/// Pencere kipinde depo yoksa bakım `Pending` döner (uygulanmış sayılmaz).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maintain_without_window_store_is_pending() {
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&SINGLE_LAYOUT).await;
    let h = sw.add_leecher().await;
    let tmp = tempfile::tempdir().unwrap();
    let reg = Arc::new(hd_torrent::window_store::WindowRegistry::new(tmp.path().join("window")));
    let b = Arc::new(hd_torrent::buffer::Buffering::new(Some(reg)));
    assert!(!b.maintain(h, 0, 0, 0.0, None).await.unwrap(), "depo yokken Pending (false) beklenir");
}
