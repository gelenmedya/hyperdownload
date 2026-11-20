mod support;

use support::*;

/// 3 MiB: dosya 0'ın son 1.5 MiB'ı + dosya 1'in ilk 1.5 MiB'ı. Hem parça sınırlarını hem de
/// dosya sınırını (sınır parçası iki dosyaya aittir) keser.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roundtrip_across_piece_and_file_boundary() {
    let sw = Swarm::start_default().await;
    let (_h, store) = sw.leech_all().await;
    let f0_len = DEFAULT_LAYOUT[0];
    let half = 3 * MIB / 2;

    let mut got0 = vec![0u8; half as usize];
    store.pread_exact(0, f0_len - half, &mut got0).unwrap();
    assert_eq!(got0, sw.seed_bytes(0, f0_len - half..f0_len));

    let mut got1 = vec![0u8; half as usize];
    store.pread_exact(1, 0, &mut got1).unwrap();
    assert_eq!(got1, sw.seed_bytes(1, 0..half));

    // Bir pread birden çok parçayı da kapsayabilmeli.
    let mut wide = vec![0u8; (3 * MIB) as usize];
    store.pread_exact(0, 5 * MIB + 17, &mut wide).unwrap();
    assert_eq!(wide, sw.seed_bytes(0, 5 * MIB + 17..8 * MIB + 17));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evict_outside_keep() {
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let (h, store) = sw.leech_all().await;
    assert!(store.bytes_on_disk() >= 64 * MIB);

    let rep = store.set_keep(0, &[10 * MIB..20 * MIB]);
    assert!(store.bytes_on_disk() <= 12 * MIB, "disk: {}", store.bytes_on_disk());
    assert_eq!(rep.bytes_on_disk, store.bytes_on_disk());

    let dir = sw.window_root.join(h.shared().info_hash.as_string());
    let on_disk = pieces_on_disk(&dir);
    assert!(!on_disk.is_empty());
    assert!(on_disk.iter().all(|p| (10..=20).contains(p)), "diskte: {on_disk:?}");
    assert_eq!(rep.evicted.len() as u64, 64 - on_disk.len() as u64);

    // Silinen aralıktan okuma hata verir, sıfır doldurmaz.
    let mut buf = vec![0u8; 4096];
    assert!(store.pread_exact(0, 2 * MIB, &mut buf).is_err());
    // Korunan aralık okunabilir.
    let mut keep = vec![0u8; MIB as usize];
    store.pread_exact(0, 12 * MIB, &mut keep).unwrap();
    assert_eq!(keep, sw.seed_bytes(0, 12 * MIB..13 * MIB));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keep_boundary_pieces_retained() {
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let (h, store) = sw.leech_all().await;

    // 10 MiB + 1 parça 10'a, 20 MiB - 1 parça 19'a değer.
    store.set_keep(0, &[10 * MIB + 1..20 * MIB - 1]);
    let dir = sw.window_root.join(h.shared().info_hash.as_string());
    assert_eq!(pieces_on_disk(&dir), (10..=19).collect::<Vec<u32>>());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multiple_keep_ranges_header_plus_window() {
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let (h, store) = sw.leech_all().await;
    // Kapsayıcı başlığı (ilk 1 MiB) + pencere.
    store.set_keep(0, &[0..MIB, 30 * MIB..32 * MIB]);
    let dir = sw.window_root.join(h.shared().info_hash.as_string());
    assert_eq!(pieces_on_disk(&dir), vec![0, 30, 31]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forget_then_refetch() {
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let (h, store) = sw.leech_all().await;

    store.set_keep(0, &[30 * MIB..40 * MIB]);
    let mut buf = vec![0u8; 16];
    assert!(store.pread_exact(0, 12 * MIB, &mut buf).is_err());

    // Parça 10..20 silindi; librqbit'e "bunlara artık sahip değilsin" de.
    let pieces: Vec<u32> = (10..20).collect();
    store.forget_pieces(&h, &pieces).unwrap();

    let got = read_via_stream(&h, 0, 12 * MIB..14 * MIB, 10).await.expect("refetch");
    assert_eq!(got, sw.seed_bytes(0, 12 * MIB..14 * MIB));
    // Tekrar indirilen parçalar diskte.
    store.pread_exact(0, 12 * MIB, &mut buf).unwrap();
}

/// Kontrol: `forget_pieces` olmadan librqbit parçaya hâlâ "sahibim" der, bu yüzden tahliye
/// edilmiş aralık yeniden indirilmez ve akıştan okuma başarısız olur.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evicted_range_stays_unreadable_without_forget() {
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let (h, store) = sw.leech_all().await;
    store.set_keep(0, &[30 * MIB..40 * MIB]);
    assert!(read_via_stream(&h, 0, 12 * MIB..13 * MIB, 3).await.is_err());
}

/// Tahliye indirme sürerken (kısmi parçalar varken) çalışırsa hiçbir şey bozulmaz: torrent
/// yine de biter ve korunan aralık bayt bayt doğrudur.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eviction_during_download_does_not_corrupt() {
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let h = sw.add_leecher().await;
    let store = sw.registry.store(&h.shared().info_hash.as_string()).unwrap();

    let evictor = {
        let store = store.clone();
        tokio::spawn(async move {
            // Pencere dışını sürekli tahliye et; indirme bitene dek.
            for _ in 0..2000 {
                store.set_keep(0, &[20 * MIB..30 * MIB]);
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(90), h.wait_until_completed())
        .await
        .expect("download timeout")
        .expect("download");
    evictor.await.unwrap();
    store.set_keep(0, &[20 * MIB..30 * MIB]);

    let mut got = vec![0u8; (10 * MIB) as usize];
    store.pread_exact(0, 20 * MIB, &mut got).unwrap();
    assert_eq!(got, sw.seed_bytes(0, 20 * MIB..30 * MIB));
    let on_disk = pieces_on_disk(&sw.window_root.join(h.shared().info_hash.as_string()));
    assert!(on_disk.iter().all(|p| (20..=30).contains(p)), "diskte: {on_disk:?}");
}

/// Yalnızca `set_keep` verilen dosyanın parçaları tahliye edilir; başka dosyanın (ör. dış
/// altyazı) verisi ve paylaşılan sınır parçası kalır.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_keep_does_not_touch_other_files() {
    let sw = Swarm::start_default().await; // dosya 0: 40.5 MiB, dosya 1: 23.5 MiB
    let (h, store) = sw.leech_all().await;
    let dir = sw.window_root.join(h.shared().info_hash.as_string());

    store.set_keep(0, &[10 * MIB..20 * MIB]);
    let on_disk = pieces_on_disk(&dir);
    // 10..=19 (pencere) + dosya 1'e değen 40..=63 (40: paylaşılan sınır parçası).
    let mut want: Vec<u32> = (10..20).collect();
    want.extend(40..64);
    assert_eq!(on_disk, want);
    let mut b = vec![0u8; MIB as usize];
    store.pread_exact(1, 0, &mut b).unwrap();
    assert_eq!(b, sw.seed_bytes(1, 0..MIB));

    // Dosya 1 de kayıt verince paylaşılan parça serbest kalır.
    store.set_keep(1, &[20 * MIB..23 * MIB]);
    let on_disk = pieces_on_disk(&dir);
    let mut want: Vec<u32> = (10..20).collect();
    want.extend(60..64);
    assert_eq!(on_disk, want);
}

/// Tahliye edilmiş parçayı isteyen üçüncü eş: okuma hatası yalnızca o eş bağlantısını düşürür;
/// izleyicinin torrent'i Live kalır ve korunan aralık akıtılabilir.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peer_requesting_evicted_piece_does_not_break_torrent() {
    let sw = Swarm::start(&SINGLE_LAYOUT).await;
    let (h, store) = sw.leech_all().await;
    store.set_keep(0, &[30 * MIB..40 * MIB]);

    let (_sess3, h3) = sw.add_third_peer().await;
    let _ = h3; // eş, izleyiciden parça 0..'ı ister (tahliye edilmiş) ve hata alır
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    // Üçüncü eşin istekleri işlensin diye biraz bekle; bu sırada torrent Live kalmalı.
    while std::time::Instant::now() < deadline {
        assert!(matches!(h.stats().state, librqbit::TorrentStatsState::Live), "{:?}", h.stats().state);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    // Üçüncü eş tamamlayamaz (tahliye edilmiş parçalar sunulamaz).
    // Hata yolu gerçekten işledi: eşin bağlantısı düştü (dead), hiçbir veri alamadı.
    let live = h3.stats().live.expect("third peer live stats");
    assert!(live.snapshot.peer_stats.dead >= 1, "{:?}", live.snapshot.peer_stats);
    assert!(!h3.stats().finished);
    let got = read_via_stream(&h, 0, 31 * MIB..33 * MIB, 10).await.expect("kept range streams");
    assert_eq!(got, sw.seed_bytes(0, 31 * MIB..33 * MIB));
}
