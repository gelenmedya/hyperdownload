//! HyperDownload's file choice: a torrent with more than one file waits for the choice (also when its files are not videos), the
//! addition is followed to the torrent it became, and only the chosen file is downloaded.

mod support;

use std::sync::Arc;

use hd_torrent::engine::{AddSource, Engine, EngineConfig, StorageMode};
use support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_torrent_of_two_files_waits_for_the_choice_and_downloads_only_the_chosen_one() {
    let _g = engine_lock().await;
    let files = vec![("a.bin".to_string(), random_bytes(3 * MIB, 1)), ("b.bin".to_string(), random_bytes(2 * MIB, 2))];
    let sw = Swarm::start_named(&files).await;
    let data = tempfile::tempdir().unwrap();
    let cfg = EngineConfig { data_dir: data.path().to_path_buf(), default_download_dir: data.path().join("dl"), storage: StorageMode::Disk, listen_port: 0 };
    let e: Arc<Engine> = Engine::start(cfg, Arc::new(|_| {})).await.expect("engine");
    e.set_ask_files(true).unwrap();
    e.set_ask_all_files(true).unwrap();

    let key = e.add(AddSource::Bytes { name: "t.torrent".into(), data: sw.torrent_bytes.clone() });
    assert!(wait_for(30, || e.pending().iter().any(|p| p.key == key && p.choice.is_some())).await, "the file list waits for a choice");
    let choice = e.pending().into_iter().find(|p| p.key == key).unwrap().choice.unwrap();
    assert_eq!(choice.files.len(), 2);
    assert!(e.resolved(key).is_none(), "not a torrent yet");

    e.choose(key, vec![1]).unwrap();
    assert!(wait_for(30, || e.resolved(key).is_some()).await, "the addition became a torrent");
    let id = e.resolved(key).unwrap();
    let included: Vec<bool> = e.view(id, true).unwrap().files.unwrap().iter().map(|f| f.included).collect();
    assert_eq!(included, vec![false, true]);

    // a peer that has it all dials the engine (no trackers, no DHT in the test)
    let addr = e.session.listen_addr().expect("engine listens");
    let _seed = sw.add_seeder_dialing(std::net::SocketAddr::from(([127, 0, 0, 1], addr.port()))).await;
    let want = files[1].1.len() as u64;
    assert!(
        wait_for(90, || e.view(id, true).and_then(|v| v.files).is_some_and(|f| f[1].progress == want)).await,
        "the chosen file came"
    );
    let v = e.view(id, true).unwrap();
    let dir = std::path::PathBuf::from(&v.output_folder);
    assert_eq!(std::fs::read(dir.join("b.bin")).unwrap(), files[1].1);
    let a_len = std::fs::metadata(dir.join("a.bin")).map(|m| m.len()).unwrap_or(0);
    assert!(a_len == 0 || v.files.unwrap()[0].progress < files[0].1.len() as u64, "the file not chosen was not downloaded");
    e.shutdown().await;
}
