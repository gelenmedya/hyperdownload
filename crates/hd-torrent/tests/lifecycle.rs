//! Film yaşam döngüsü: kapatınca sil, geçmiş, açılışta temizlik, uzak geri sarma (Engine düzeyi).

mod support;

use std::sync::Arc;

use support::*;
use hd_torrent::engine::{Engine, PositionUpdate};

fn pos(file: usize, secs: f64) -> PositionUpdate {
    PositionUpdate { file, secs, duration_secs: None, audio_track: Some(Some(2)), subtitle_track: Some(Some(7)) }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_deletes_all_data() {
    let _g = engine_lock().await;
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let (id, h) = sw.engine_add(&engine).await;
    let hash = h.shared().info_hash.as_string();

    // Oynat: baştan biraz oku.
    let got = read_via_stream(&h, 0, 0..MIB, 30).await.unwrap();
    assert_eq!(got, sw.seed_bytes(0, 0..MIB));
    let dir = data.path().join("window").join(&hash);
    assert!(dir.exists(), "pencere dizini oluşmalıydı");

    engine.position(id, pos(0, 123.5)).await.unwrap();
    engine.close(id).await.unwrap();

    assert!(!dir.exists(), "close sonrası pencere dizini silinmeli");
    assert!(engine.handle(id).is_err(), "torrent oturumdan çıkmalı");
    let hist = engine.history.list();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].info_hash, hash);
    assert_eq!(hist[0].position_secs, 123.5);
    assert_eq!(hist[0].audio_track, Some(2));
    assert!(hist[0].source.contains(&hash), "kaynak: {}", hist[0].source);
    // Kalıcı: dosyadan yeniden yüklenir.
    assert!(data.path().join("history.json").exists());
    let again = hd_torrent::history::History::load(data.path());
    assert_eq!(again.list().len(), 1);
}

/// close sonrası `sources.json` kaydı temizlenir; geçmiş kendi kaynağını korur.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_prunes_sources_json_but_history_keeps_source() {
    let _g = engine_lock().await;
    let sw = Swarm::start_video(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let hash = sw.info_hash();
    engine.add(hd_torrent::engine::AddSource::Bytes {
        name: "t.torrent".into(),
        data: sw.torrent_bytes.clone().into(),
    });
    let sources = || std::fs::read_to_string(data.path().join("sources.json")).unwrap_or_default();
    assert!(wait_for(30, || sources().contains(&hash)).await, "sources.json kaydı yok: {}", sources());
    let id = engine.list().first().expect("eklenen torrent").id;

    engine.close(id).await.unwrap();
    assert!(!sources().contains(&hash), "close sonrası kayıt silinmeli: {}", sources());
    let hist = engine.history.get(&hash).expect("geçmiş girişi");
    assert!(!hist.source.is_empty());
    assert!(!data.path().join("sources.json.tmp").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_removes_orphans_when_session_is_empty() {
    let _g = engine_lock().await;
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("history.json"), b"[]").unwrap();
    std::fs::write(data.path().join("settings.json"), b"{}").unwrap();
    let junk = data.path().join("window").join("deadbeef");
    std::fs::create_dir_all(&junk).unwrap();
    std::fs::write(junk.join("0.bin"), b"leftover").unwrap();
    std::fs::write(data.path().join("window").join("stray.bin"), b"x").unwrap();

    let _engine = sw.window_engine(data.path()).await;
    let left: Vec<_> = std::fs::read_dir(data.path().join("window")).unwrap().collect();
    assert!(left.is_empty(), "window/ boş olmalı: {left:?}");
    assert!(data.path().join("history.json").exists(), "history.json silinmemeli");
    assert!(data.path().join("settings.json").exists(), "settings.json silinmemeli");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seek_back_beyond_window_refetches() {
    let _g = engine_lock().await;
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine: Arc<Engine> = sw.window_engine(data.path()).await;
    let (id, h) = sw.engine_add(&engine).await;
    let hash = h.shared().info_hash.as_string();
    let dir = data.path().join("window").join(&hash);

    // Bit hızı öyle ki ±300 sn ~ ±5 MiB (+%20 pay): 64 MiB ~ 3840 sn.
    let bps = (5 * MIB) as f64 / 300.0;
    let dur = (64 * MIB) as f64 / bps;
    let at = |mib: u64| (mib * MIB) as f64 / bps;
    let report = |mib: u64| {
        engine
            .buffering
            .report(&engine.session, h.clone(), id, 0, at(mib), Some(dur), Some(bps), 0)
            .unwrap();
    };

    // Uzakta oynat: 50 MiB civarı iner, ortadaki (24 MiB civarı) hiç inmez.
    report(50);
    assert!(
        wait_for(60, || { let d = pieces_on_disk(&dir); (47..=52).all(|p| d.contains(&p)) }).await,
        "50 MiB penceresi inmedi: {:?}",
        pieces_on_disk(&dir)
    );
    let far = read_via_stream(&h, 0, 50 * MIB..51 * MIB, 30).await.unwrap();
    assert_eq!(far, sw.seed_bytes(0, 50 * MIB..51 * MIB));
    assert!(!pieces_on_disk(&dir).contains(&24));

    // Pencerenin çok gerisine sar: tick beklemeden ensure_range.
    engine.ensure_range(id, 0, 24 * MIB..25 * MIB).await.unwrap();
    let back = read_via_stream(&h, 0, 24 * MIB + 2 * MIB..24 * MIB + 3 * MIB, 15).await.unwrap();
    assert_eq!(back, sw.seed_bytes(0, 26 * MIB..27 * MIB));

    // Eski pencere evict edilir, disk sınırlı kalır.
    assert!(
        wait_for(15, || { let d = pieces_on_disk(&dir); !d.iter().any(|p| (47..=52).contains(p)) }).await,
        "eski pencere durmakta: {:?}",
        pieces_on_disk(&dir)
    );
}

fn upd(secs: f64, a: Option<Option<u64>>, s: Option<Option<u64>>) -> PositionUpdate {
    PositionUpdate { file: 0, secs, duration_secs: None, audio_track: a, subtitle_track: s }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn position_without_tracks_keeps_stored_selection() {
    let _g = engine_lock().await;
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let (id, h) = sw.engine_add(&engine).await;
    let hash = h.shared().info_hash.as_string();

    engine.position(id, upd(10.0, Some(Some(2)), Some(Some(7)))).await.unwrap();
    engine.position(id, upd(20.0, None, None)).await.unwrap();
    let e = engine.history.get(&hash).unwrap();
    assert_eq!(e.position_secs, 20.0);
    assert_eq!(e.audio_track, Some(2));
    assert_eq!(e.subtitle_track, Some(7));
    // JSON düzeyinde: alan yok -> None, null -> Some(None).
    let u: PositionUpdate = serde_json::from_str(r#"{"file":0,"secs":1.0}"#).unwrap();
    assert_eq!((u.audio_track, u.subtitle_track), (None, None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn position_null_clears_subtitle() {
    let _g = engine_lock().await;
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let (id, h) = sw.engine_add(&engine).await;
    let hash = h.shared().info_hash.as_string();

    engine.position(id, upd(10.0, Some(Some(2)), Some(Some(7)))).await.unwrap();
    let u: PositionUpdate = serde_json::from_str(r#"{"file":0,"secs":30.0,"subtitle_track":null}"#).unwrap();
    assert_eq!(u.subtitle_track, Some(None));
    engine.position(id, u).await.unwrap();
    let e = engine.history.get(&hash).unwrap();
    assert_eq!(e.subtitle_track, None);
    assert_eq!(e.audio_track, Some(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_keeps_last_position() {
    let _g = engine_lock().await;
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let (id, h) = sw.engine_add(&engine).await;
    let hash = h.shared().info_hash.as_string();

    engine.position(id, upd(77.0, Some(Some(3)), Some(Some(4)))).await.unwrap();
    engine.close(id).await.unwrap();
    let e = engine.history.get(&hash).unwrap();
    assert_eq!((e.position_secs, e.audio_track, e.subtitle_track), (77.0, Some(3), Some(4)));
}

/// `/api/diag` özeti: pencere doluyken baytlar > 0, film kapatılınca 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn diag_reports_window_bytes() {
    let _g = engine_lock().await;
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let (id, h) = sw.engine_add(&engine).await;

    let got = read_via_stream(&h, 0, 0..MIB, 30).await.unwrap();
    assert_eq!(got, sw.seed_bytes(0, 0..MIB));

    let d = engine.diag().await;
    let total = d["window_bytes_total"].as_u64().unwrap();
    assert!(total > 0, "pencere doluyken > 0: {d}");
    let t = &d["per_torrent"][0];
    assert_eq!(t["id"].as_u64().unwrap(), id as u64);
    assert_eq!(t["window_bytes"].as_u64().unwrap(), total);
    assert!(t["pieces_kept"].as_u64().unwrap() > 0);
    assert_eq!(d["engine"]["storage_mode"], "window");
    assert!(d["engine"]["data_dir_bytes"].as_u64().unwrap() >= total);
    assert!(d["engine"]["version"].is_string());

    engine.close(id).await.unwrap();
    let d = engine.diag().await;
    assert_eq!(d["window_bytes_total"].as_u64().unwrap(), 0, "kapatınca 0: {d}");
    assert!(d["per_torrent"].as_array().unwrap().is_empty());
}
