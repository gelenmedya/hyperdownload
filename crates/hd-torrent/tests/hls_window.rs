//! HLS uçları Window kipinde, gerçek (yerel) sürüyle: master, init, uzak segmentler, ses, gömülü ve
//! harici altyazı. İnen/seçili parçalar meta ∪ pencere ∪ keep-alive ∪ harici altyazı ile sınırlı
//! kalır; v/a istekleri dönüşümlü gelse de playhead geri gitmez ve hiçbir istek aç kalmaz.

mod support;

use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::ops::Range;
use std::time::{Duration, Instant};

use filmcraft_isobmff as mp4;
use filmcraft_matroska as mkv;
use support::*;
use hd_torrent::demux;

const TOTAL_SECS: u64 = 1200;

fn ac3_frame() -> Vec<u8> {
    let mut v = vec![0x0B, 0x77, 0x00, 0x00, 0x0E, 8 << 3, 0xE0];
    v.resize(64, 0);
    v
}

fn avcc() -> Vec<u8> {
    let sps = [0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x00, 0xA0, 0x47, 0xFE, 0xC8];
    let pps = [0x68, 0xCE, 0x3C, 0x80];
    let mut v = vec![1, 0x42, 0xC0, 0x1E, 0xFF, 0xE1];
    v.extend((sps.len() as u16).to_be_bytes());
    v.extend(sps);
    v.push(1);
    v.extend((pps.len() as u16).to_be_bytes());
    v.extend(pps);
    v
}

/// 1200 sn: 2 sn'de bir 40 KB anahtar kare (~24 MB), AC-3 5.1, 5 sn'de bir SRT ipucu; 6 sn'lik kümeler.
fn movie() -> Vec<u8> {
    let mut v = mkv::TrackSpec::new(mkv::TrackKind::Video, "V_MPEG4/ISO/AVC");
    v.video_size = Some((64, 64));
    v.default_duration_ns = Some(2_000_000_000);
    v.codec_private = avcc();
    let mut a = mkv::TrackSpec::new(mkv::TrackKind::Audio, "A_AC3");
    a.audio = Some((48000.0, 6, None));
    a.default_duration_ns = Some(32_000_000);
    a.language = Some("tur".into());
    let mut s = mkv::TrackSpec::new(mkv::TrackKind::Subtitle, "S_TEXT/UTF8");
    s.language = Some("eng".into());
    let opts = mkv::MuxOptions { cluster_duration_ns: 6_000_000_000, ..Default::default() };
    let mut w = mkv::MkvWriter::new(Cursor::new(Vec::new()), vec![v, a, s], opts).unwrap();
    let mut fr: Vec<(u64, usize, Vec<u8>, Option<u64>)> = Vec::new();
    for i in 0..TOTAL_SECS / 2 {
        let mut d = vec![0u8; 40_000];
        for (k, b) in d.iter_mut().enumerate() {
            *b = (k as u64 * 31 + i * 7) as u8;
        }
        fr.push((i * 2000, 0, d, None));
    }
    for k in 0..TOTAL_SECS * 1000 / 32 {
        fr.push((k * 32, 1, ac3_frame(), None));
    }
    for k in 0..TOTAL_SECS / 5 {
        fr.push((k * 5000, 2, format!("Altyazi {k}").into_bytes(), Some(2_000_000_000)));
    }
    fr.sort_by_key(|f| (f.0, f.1));
    for (ms, t, d, dur) in fr {
        w.write_frame(t, (ms * 1_000_000) as i64, true, &d, dur).unwrap();
    }
    w.finish().unwrap().into_inner()
}

struct Reply {
    status: u16,
    body: Vec<u8>,
}

fn http_get(addr: SocketAddr, path: &str) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(40))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head.lines().next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    let chunked = head.to_ascii_lowercase().contains("transfer-encoding: chunked");
    let mut body = raw[split + 4..].to_vec();
    if chunked {
        let mut out = Vec::new();
        let mut b = &body[..];
        loop {
            let nl = b.windows(2).position(|w| w == b"\r\n").unwrap();
            let n = usize::from_str_radix(std::str::from_utf8(&b[..nl]).unwrap().trim(), 16).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&b[nl + 2..nl + 2 + n]);
            b = &b[nl + 2 + n + 2..];
        }
        body = out;
    }
    Reply { status, body }
}

/// İstek bir üst sınır içinde 200 dönmeli (aç kalma yok).
async fn get_ok(addr: SocketAddr, path: &str) -> Vec<u8> {
    let p = path.to_string();
    let t0 = Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(45), tokio::task::spawn_blocking(move || http_get(addr, &p)))
        .await
        .unwrap_or_else(|_| panic!("{path}: zaman aşımı"))
        .unwrap();
    assert_eq!(r.status, 200, "{path}: {}", String::from_utf8_lossy(&r.body[..r.body.len().min(300)]));
    assert!(t0.elapsed() < Duration::from_secs(40), "{path}: {:?}", t0.elapsed());
    r.body
}

/// `v.m3u8`den segment başlangıç zamanları.
fn starts_of(playlist: &str) -> Vec<f64> {
    let mut t = 0.0;
    let mut v = Vec::new();
    for l in playlist.lines() {
        if let Some(d) = l.strip_prefix("#EXTINF:") {
            v.push(t);
            t += d.trim_end_matches(',').parse::<f64>().unwrap();
        }
    }
    v
}

fn overlaps(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hls_window_mode_end_to_end() {
    let _g = engine_lock().await;
    let file = movie();
    let len0 = file.len() as u64;
    assert!(len0 > 20 * MIB, "{len0}");
    let ti = demux::time_index(Cursor::new(file.clone())).unwrap();
    let loc = demux::locate_meta(Cursor::new(file.clone()), len0);
    let srt = b"1\n00:00:01,000 --> 00:00:02,000\nDis altyazi\n".to_vec();
    let len1 = srt.len() as u64;
    let sw = Swarm::start_named(&[("f0.mkv".into(), file.clone()), ("f1.srt".into(), srt)]).await;
    let data = tempfile::tempdir().unwrap();
    let engine = sw.window_engine(data.path()).await;
    let (id, h) = sw.engine_add(&engine).await;
    let hash = h.shared().info_hash.as_string();
    let dir = data.path().join("window").join(&hash);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = hd_torrent::server::router(engine.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("/hls/{id}/0");

    // Gözlenen playhead'ler ve keep-alive aralıkları (saniye) — izin verilen kümeyi bunlar kurar.
    let mut heads: Vec<f64> = vec![0.0];
    let mut keeps: Vec<(f64, f64)> = Vec::new();

    // 1) master (zaman dizini hazır olana dek bekler)
    let master = String::from_utf8(get_ok(addr, &format!("{base}/master.m3u8")).await).unwrap();
    println!("--- master ---\n{master}");
    assert!(master.contains("#EXT-X-STREAM-INF"));
    assert_eq!(master.matches("TYPE=AUDIO").count(), 1, "{master}");
    assert_eq!(master.matches("TYPE=SUBTITLES").count(), 2, "gömülü + harici: {master}");
    let aid: u64 = master.split("URI=\"a").nth(1).unwrap().split(".m3u8").next().unwrap().parse().unwrap();
    let sid: u64 = master.split("URI=\"s").nth(1).unwrap().split(".m3u8").next().unwrap().parse().unwrap();
    assert!(sid < 1_000_000);
    let pl = String::from_utf8(get_ok(addr, &format!("{base}/v.m3u8")).await).unwrap();
    let starts = starts_of(&pl);
    assert!(starts.len() > 100, "{}", starts.len());
    let n_far = starts.iter().position(|s| *s >= 900.0).unwrap();

    // 2) video init + UZAK segment (pencere dışı): çözümlenir
    let init = get_ok(addr, &format!("{base}/v/init.mp4")).await;
    let seg = get_ok(addr, &format!("{base}/v/{n_far}.m4s")).await;
    heads.push(starts[n_far]);
    let mut all = init.clone();
    all.extend_from_slice(&seg);
    let f = mp4::open(all).expect("video init + segment çözümlenir");
    let t = f.tracks.iter().find(|t| t.kind == mp4::TrackKind::Video).unwrap();
    assert!(!t.samples.is_empty() && t.samples[0].is_sync);
    let ph = engine.buffering.playhead_secs(id, 0).unwrap();
    assert!((ph - starts[n_far]).abs() < 1.0, "playhead {ph} != {}", starts[n_far]);

    // 3) ses init + ses segmenti (playhead taşınmaz; pencere dışı geri bir segment dahil)
    let ainit = get_ok(addr, &format!("{base}/a{aid}/init.mp4")).await;
    assert!(ainit.windows(4).any(|w| w == b"ac-3") && ainit.windows(4).any(|w| w == b"dac3"));
    keeps.push((starts[n_far] - 6.0, starts[n_far] + 12.0));
    let aseg = get_ok(addr, &format!("{base}/a{aid}/{n_far}.m4s")).await;
    let mut all = ainit.clone();
    all.extend_from_slice(&aseg);
    let f = mp4::open(all).expect("ses init + segment çözümlenir");
    let t = f.tracks.iter().find(|t| t.kind == mp4::TrackKind::Audio).unwrap();
    assert!(!t.samples.is_empty());
    let ph2 = engine.buffering.playhead_secs(id, 0).unwrap();
    assert!((ph2 - ph).abs() < 0.01, "ses playhead'i taşıdı: {ph} -> {ph2}");

    // Pencere (900±300) dışındaki geri ses segmenti: keep-alive ile gelir, playhead yerinde.
    let n_back = starts.iter().position(|s| *s >= 120.0).unwrap();
    let back = get_ok(addr, &format!("{base}/a{aid}/{n_back}.m4s")).await;
    assert!(back.windows(4).any(|w| w == b"moof"));
    keeps.push((starts[n_back] - 3.0, starts[n_back + 1] + 3.0));
    let ph3 = engine.buffering.playhead_secs(id, 0).unwrap();
    assert!((ph3 - ph).abs() < 0.01, "geri ses playhead'i taşıdı: {ph3}");

    // 4) gömülü altyazı (uzak + geri) ve harici altyazı
    let vtt = String::from_utf8(get_ok(addr, &format!("{base}/s{sid}/{n_far}.vtt")).await).unwrap();
    assert!(vtt.starts_with("WEBVTT\nX-TIMESTAMP-MAP="), "{vtt}");
    assert!(vtt.contains("Altyazi 180"), "{vtt}");
    let vtt2 = String::from_utf8(get_ok(addr, &format!("{base}/s{sid}/{n_back}.vtt")).await).unwrap();
    assert!(vtt2.starts_with("WEBVTT"));
    keeps.push((starts[n_far] - 3.0, starts[n_far + 1] + 3.0));
    keeps.push((starts[n_back] - 3.0, starts[n_back + 1] + 3.0));
    let ext = String::from_utf8(get_ok(addr, &format!("{base}/s1000001/0.vtt")).await).unwrap();
    assert!(ext.contains("00:00:01.000 --> 00:00:02.000\nDis altyazi"), "{ext}");

    // 5) dönüşümlü v/a/s istekleri: playhead yalnız video ile ve geri gitmeden ilerler
    let mut last = engine.buffering.playhead_secs(id, 0).unwrap();
    for k in 1..=6usize {
        let vn = n_far + k;
        get_ok(addr, &format!("{base}/v/{vn}.m4s")).await;
        let p = engine.buffering.playhead_secs(id, 0).unwrap();
        assert!((p - starts[vn]).abs() < 1.0, "video {vn}: playhead {p}");
        assert!(p >= last - 0.01, "playhead geri gitti: {last} -> {p}");
        last = p;
        heads.push(starts[vn]);
        // ses ve altyazı, videonun gerisinde/ilerisinde, sırasız
        for an in [vn - 4, vn + 2] {
            get_ok(addr, &format!("{base}/a{aid}/{an}.m4s")).await;
            get_ok(addr, &format!("{base}/s{sid}/{an}.vtt")).await;
            keeps.push((starts[an] - 3.0, starts[an + 1] + 3.0));
            let q = engine.buffering.playhead_secs(id, 0).unwrap();
            assert!((q - last).abs() < 0.01, "ses/altyazı playhead'i taşıdı: {last} -> {q}");
        }
    }

    // 6) inen parçalar: meta ∪ pencere(ler) ∪ keep-alive ∪ harici altyazı dosyası
    let byte = |secs: f64| ti.byte_at(secs.max(0.0), len0, len0 as f64 / TOTAL_SECS as f64);
    let mut allowed: Vec<Range<u64>> = loc.ranges.clone();
    allowed.push(0..ti.header_end().unwrap_or(0).max(1));
    for p in &heads {
        allowed.push(byte(p - 300.0)..byte(p + 300.0) + MIB);
    }
    for (a, b) in &keeps {
        allowed.push(byte(*a)..byte(*b));
    }
    allowed.push(len0..len0 + len1);
    let piece_range = |p: u32| p as u64 * MIB..(p as u64 + 1) * MIB;
    let outside = |pieces: &[u32]| -> Vec<u32> {
        pieces.iter().copied().filter(|p| !allowed.iter().any(|r| overlaps(r, &piece_range(*p)))).collect()
    };
    // Eski pencerenin tahliyesi bir sonraki süpürmede olur: sınırlı süre içinde oturmalı.
    let settled = wait_for(40, || outside(&pieces_on_disk(&dir)).is_empty()).await;
    let disk = pieces_on_disk(&dir);
    assert!(settled, "izinli kümenin dışında parça var: {:?} (disk {:?})", outside(&disk), disk);
    // İlk pencere (0..300 sn) ile uzak pencere arasındaki bölge (330..590 sn) hiç inmemiş/tutulmamış olmalı.
    let mid = byte(330.0)..byte(590.0);
    let mid_pieces: Vec<u32> = disk.iter().copied().filter(|p| overlaps(&mid, &piece_range(*p)) && !keeps.iter().any(|(a, b)| overlaps(&(byte(*a)..byte(*b)), &piece_range(*p)))).collect();
    assert!(mid_pieces.is_empty(), "ara bölge parçaları: {mid_pieces:?}");
    // Uzak pencere gerçekten indi.
    assert!(disk.contains(&((byte(starts[n_far]) / MIB) as u32)), "uzak segment parçası yok: {disk:?}");
}
