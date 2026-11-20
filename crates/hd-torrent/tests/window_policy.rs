//! Pencere politikası: parça düzeyinde seçim + tahliye (`apply_window`), yerel sürüde.

mod support;

use std::ops::Range;
use std::sync::Arc;

use librqbit::ManagedTorrent;
use support::*;
use hd_torrent::window_policy::apply_window;
use hd_torrent::window_store::WindowStore;

const HEADER: Range<u64> = 0..MIB;

fn win(a: u64, b: u64) -> Range<u64> {
    a * MIB..b * MIB
}

fn disk_pieces(sw: &Swarm, h: &Arc<ManagedTorrent>) -> Vec<u32> {
    pieces_on_disk(&sw.window_root.join(h.shared().info_hash.as_string()))
}

/// Paused ekler, pencereyi uygular, sonra indirmeyi başlatır (tam indirme hiç başlamaz).
async fn start_windowed(sw: &Swarm, keep: Range<u64>) -> (Arc<ManagedTorrent>, Arc<WindowStore>) {
    let (h, store) = sw.add_leecher_paused().await;
    apply_window(&h, &store, 0, HEADER, keep).expect("apply_window (paused)");
    sw.leecher.unpause(&h).await.expect("unpause");
    (h, store)
}

/// Buffering her tick'te pencereyi yeniden uygular (uçuştaki parçalar tamamlanınca da dışarıda
/// kalanlar silinir). Aynısını yapar: `cond` doğru olana dek her 100 ms'de uygular.
async fn settle(
    h: &Arc<ManagedTorrent>,
    store: &Arc<WindowStore>,
    keep: Range<u64>,
    cond: impl Fn() -> bool,
) -> bool {
    wait_for(60, || {
        apply_window(h, store, 0, HEADER, keep.clone()).expect("apply_window");
        cond()
    })
    .await
}

fn downloaded(h: &Arc<ManagedTorrent>) -> u64 {
    h.stats().live.map(|l| l.snapshot.downloaded_and_checked_bytes).unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selection_limits_download_to_window() {
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let (h, store) = start_windowed(&sw, win(10, 20)).await;

    // Pencere (10 parça) + başlık (1 parça) inince torrent "bitmiş" sayılır.
    assert!(wait_for(60, || h.stats().finished).await, "pencere inmedi: {:?}", h.stats().progress_bytes);
    // Seçim dışına taşan bir indirme olsaydı bu sürede görünürdü.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let got = downloaded(&h);
    assert!(got <= 12 * MIB, "indirilen: {got}");
    assert!(store.bytes_on_disk() <= 12 * MIB, "disk: {}", store.bytes_on_disk());
    let on_disk = disk_pieces(&sw, &h);
    assert!(on_disk.iter().all(|p| *p == 0 || (10..=19).contains(p)), "diskte: {on_disk:?}");
    for p in 10..20 {
        assert!(on_disk.contains(&p), "parça {p} yok: {on_disk:?}");
    }
    let mut buf = vec![0u8; MIB as usize];
    store.pread_exact(0, 12 * MIB, &mut buf).unwrap();
    assert_eq!(buf, sw.seed_bytes(0, 12 * MIB..13 * MIB));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moving_window_evicts_and_reselects() {
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let (h, store) = start_windowed(&sw, win(10, 20)).await;
    assert!(wait_for(60, || h.stats().finished).await, "ilk pencere inmedi");

    let new_done = settle(&h, &store, win(40, 50), || {
        let d = disk_pieces(&sw, &h);
        (40..50).all(|p| d.contains(&p)) && !d.iter().any(|p| (10..20).contains(p))
    })
    .await;
    assert!(new_done, "yeni pencere inmedi: {:?}", disk_pieces(&sw, &h));
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let on_disk = disk_pieces(&sw, &h);
    assert!(on_disk.iter().all(|p| *p == 0 || (40..=50).contains(p)), "diskte: {on_disk:?}");
    assert!(!on_disk.iter().any(|p| (10..20).contains(p)), "eski pencere duruyor: {on_disk:?}");
    assert!(store.bytes_on_disk() <= 12 * MIB, "disk: {}", store.bytes_on_disk());
    // Seçim dışına taşmadı: toplam indirme iki pencere + başlık.
    assert!(downloaded(&h) <= 22 * MIB + MIB, "indirilen: {}", downloaded(&h));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seek_back_reselects_evicted_range() {
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let (h, store) = start_windowed(&sw, win(10, 20)).await;
    assert!(wait_for(60, || h.stats().finished).await, "ilk pencere inmedi");
    assert!(
        settle(&h, &store, win(40, 50), || { let d = disk_pieces(&sw, &h); (40..50).all(|p| d.contains(&p)) }).await
    );

    // Geri sar: 10..20 tekrar pencerede.
    let back = settle(&h, &store, win(10, 20), || {
        let d = disk_pieces(&sw, &h);
        (10..20).all(|p| d.contains(&p)) && !d.iter().any(|p| (40..50).contains(p))
    })
    .await;
    assert!(back, "geri sarılan aralık yeniden inmedi: {:?}", disk_pieces(&sw, &h));

    // Parça dosyası var olması okunabilir olduğu anlamına gelmez (kısmi yazım): süreyle bekle.
    let mut got = vec![0u8; (10 * MIB) as usize];
    let readable = wait_for(30, || store.pread_exact(0, 10 * MIB, &mut got).is_ok()).await;
    assert!(readable, "10..20 okunamadı");
    assert_eq!(got, sw.seed_bytes(0, win(10, 20)));
    // Tahliye de asenkron olabilir: son kontrol süreyle beklenir.
    let evicted = wait_for(30, || !disk_pieces(&sw, &h).iter().any(|p| (40..50).contains(p))).await;
    let on_disk = disk_pieces(&sw, &h);
    assert!(evicted, "40..50 duruyor: {on_disk:?}");
    assert!(store.bytes_on_disk() <= 12 * MIB, "disk: {}", store.bytes_on_disk());
}

/// 500 kare x 20 KB MKV (~10 MB): Cues dosyanın SONUNDA (mkvmerge gibi), SeekHead başta.
fn mkv_cues_at_end() -> Vec<u8> {
    use filmcraft_matroska as mkv;
    let mut v = mkv::TrackSpec::new(mkv::TrackKind::Video, "V_MPEG4/ISO/AVC");
    v.video_size = Some((64, 64));
    v.default_duration_ns = Some(40_000_000);
    let opts = mkv::MuxOptions { cluster_duration_ns: 1_000_000_000, ..Default::default() };
    let mut w = mkv::MkvWriter::new(std::io::Cursor::new(Vec::new()), vec![v], opts).unwrap();
    for i in 0..500u64 {
        w.write_frame(0, (i * 40_000_000) as i64, i % 25 == 0, &vec![i as u8; 20_000], None).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// Cues sondayken pencere yalnızca başı seçse bile (küçük bit hızı varsayımı), meta konumu
/// (SeekHead -> Cues) seçilir ve zaman dizini yüklenir; dizin yokken orta parçalar inmez.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cues_at_end_are_selected_and_index_loads() {
    use hd_torrent::window_policy::WindowCtl;
    let mkv = mkv_cues_at_end();
    let len = mkv.len() as u64;
    assert!(len > 8 * MIB, "{len}");
    let sw = Swarm::start_with_files(&[mkv]).await;
    let (h, store) = sw.add_leecher_paused().await;
    let mut ctl = WindowCtl::new();
    let mut unpaused = false;
    let mut loaded = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        // 1000 B/sn varsayımı: pencere ~360 KB (yalnızca parça 0).
        ctl.tick(&h, &store, 0, len, 0.0, 1000.0).expect("tick");
        if !unpaused {
            sw.leecher.unpause(&h).await.expect("unpause");
            unpaused = true;
        }
        if ctl.index.is_some() {
            loaded = true;
            break;
        }
        let d = disk_pieces(&sw, &h);
        assert!(!d.iter().any(|p| (2..=7).contains(p)), "dizin yokken orta parça indi: {d:?}");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(loaded, "zaman dizini yüklenmedi; loc={:?}", ctl.loc);
    let idx = ctl.index.clone().unwrap();
    assert!(idx.points.len() >= 19, "{}", idx.points.len());
    assert!(idx.points.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1));
    assert!(ctl.loc.ranges.iter().any(|r| r.start > 8 * MIB), "{:?}", ctl.loc);
}

/// Seçim değişmedikçe vendor çağrısı (kilit + O(parça) hesap + uyandırma) yapılmaz.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unchanged_window_does_not_reselect() {
    use hd_torrent::window_policy::WindowCtl;
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let (h, store) = sw.add_leecher_paused().await;
    let mut ctl = WindowCtl::new();
    for _ in 0..3 {
        ctl.apply(&h, &store, 0, &[HEADER], &[win(10, 20)]).unwrap();
    }
    assert_eq!((ctl.vendor_calls, ctl.keep_calls), (1, 1));
    // Aynı parçalara düşen küçük kayma de seçimi değiştirmez.
    ctl.apply(&h, &store, 0, &[HEADER], &[10 * MIB + 5..20 * MIB - 5]).unwrap();
    assert_eq!(ctl.vendor_calls, 1);
    ctl.apply(&h, &store, 0, &[HEADER], &[win(11, 21)]).unwrap();
    assert_eq!(ctl.vendor_calls, 2);
}
