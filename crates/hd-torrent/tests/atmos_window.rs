//! Window kipinde Atmos taraması: oynatma dosyanın ortasında başlayınca (baş diskte yok) E-AC-3 JOC
//! izi oynatma konumundan okunarak çözülür; eksik baytlar bloklamaz.

mod support;

use std::time::Duration;

use filmcraft_matroska as mkv;
use support::*;
use hd_torrent::atmos::AtmosState;
use hd_torrent::engine::PositionUpdate;

/// E-AC-3 5.1 bağımsız çerçeve (bsid 16) + addbsi[flag=1, complexity] — `atmos::testdata` yalnızca
/// birim testlerinde görünür olduğundan burada elle (bit bit) kurulur.
fn joc_frame(complexity: u8) -> Vec<u8> {
    let mut bits: Vec<bool> = Vec::new();
    let mut put = |n: usize, v: u32| (0..n).rev().for_each(|i| bits.push((v >> i) & 1 == 1));
    put(16, 0x0B77);
    put(2, 0); // strmtyp
    put(3, 0); // substreamid
    put(11, 47); // frmsiz (96 bayt)
    put(2, 0); // fscod
    put(2, 3); // numblkscod
    put(3, 7); // acmod
    put(1, 1); // lfeon
    put(5, 16); // bsid
    put(5, 27); // dialnorm
    put(1, 0); // compre
    put(1, 0); // mixmdate
    put(1, 0); // infomdate
    put(1, 1); // addbsie
    put(6, 1); // addbsil
    put(8, 1); // bayrak
    put(8, complexity as u32);
    let mut v = vec![0u8; bits.len().div_ceil(8)];
    for (i, b) in bits.iter().enumerate() {
        if *b {
            v[i / 8] |= 0x80 >> (i % 8);
        }
    }
    v.resize(96, 0);
    v
}

/// 240 kare x 100 KB (~24 MiB), kare başına 10 sn (2400 sn). Ses (E-AC-3 JOC) yalnızca 200. kareden
/// (2000 sn) sonra başlar: dosyanın başı tek başına izi çözemez. Cues sonda.
fn movie() -> Vec<u8> {
    let mut v = mkv::TrackSpec::new(mkv::TrackKind::Video, "V_MPEG4/ISO/AVC");
    v.video_size = Some((64, 64));
    let mut a = mkv::TrackSpec::new(mkv::TrackKind::Audio, "A_EAC3");
    a.audio = Some((48000.0, 6, None));
    let opts = mkv::MuxOptions { lacing: mkv::LacingMode::None, cluster_duration_ns: 10_000_000_000, ..Default::default() };
    let mut w = mkv::MkvWriter::new(std::io::Cursor::new(Vec::new()), vec![v, a], opts).unwrap();
    for i in 0..240u64 {
        let ts = (i * 10_000_000_000) as i64;
        let mut frame = vec![0u8; 100_000];
        for (k, b) in frame.iter_mut().enumerate() {
            *b = (k as u64 * 31 + i * 7) as u8;
        }
        w.write_frame(0, ts, true, &frame, None).unwrap();
        if i >= 200 {
            w.write_frame(1, ts, true, &joc_frame(16), None).unwrap();
        }
    }
    w.finish().unwrap().into_inner()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn atmos_resolves_from_playhead_when_head_is_not_on_disk() {
    let _g = engine_lock().await;
    let file = movie();
    let len = file.len() as u64;
    assert!(len > 20 * MIB, "{len}");
    let sw = Swarm::start_with_files(&[file]).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let (id, h) = sw.engine_add(&engine).await;
    let hash = h.shared().info_hash.as_string();
    let dir = data.path().join("window").join(&hash);

    // Oynatma ortada (2100 sn ~ %87,5) başlar: pencere orada, baş (ses henüz yok) diskte değil.
    engine
        .position(id, PositionUpdate { file: 0, secs: 2100.0, duration_secs: Some(2400.0), audio_track: None, subtitle_track: None })
        .await
        .unwrap();
    let mid = (len as f64 * 2100.0 / 2400.0) as u64 / MIB;
    assert!(
        wait_for(60, || pieces_on_disk(&dir).contains(&(mid as u32))).await,
        "oynatma penceresi inmedi: {:?}",
        pieces_on_disk(&dir)
    );
    assert!(!pieces_on_disk(&dir).contains(&5), "dosya ortası değil başı indi: {:?}", pieces_on_disk(&dir));

    // atmos() asla engellemez; ilk çağrı Unknown/boş döner, sonra Yes olur.
    let t0 = std::time::Instant::now();
    let first = engine.atmos(id, 0);
    assert!(t0.elapsed() < Duration::from_millis(500), "atmos() bloklandı");
    assert!(first.values().all(|s| *s != AtmosState::Yes));
    let resolved = wait_for(40, || engine.atmos(id, 0).values().any(|s| *s == AtmosState::Yes)).await;
    assert!(resolved, "Atmos çözülmedi: {:?}; disk: {:?}", engine.atmos(id, 0), pieces_on_disk(&dir));
}
