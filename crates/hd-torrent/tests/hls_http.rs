//! HLS HTTP uçları: yerel dosya kaynağı + sentetik MKV (AVC + AC-3 + SRT), torrent yok.

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use filmcraft_isobmff as mp4;
use filmcraft_matroska as mkv;
use hd_torrent::atmos::AtmosState;
use hd_torrent::demux::{self, MediaInfo, TimeIndex};
use hd_torrent::hls::routes::{Fut, HlsCore, HlsHost, MediaSource, ReadSeek, routes_with};

/// 64 baytlık AC-3 (bsid 8) senkron çerçevesi başlığı.
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

/// 30 sn: 25 fps AVC (2 sn'de bir anahtar kare), 48 kHz AC-3 5.1, bir SRT altyazı izi.
fn movie() -> Vec<u8> {
    let mut v = mkv::TrackSpec::new(mkv::TrackKind::Video, "V_MPEG4/ISO/AVC");
    v.video_size = Some((64, 64));
    v.default_duration_ns = Some(40_000_000);
    v.codec_private = avcc();
    let mut a = mkv::TrackSpec::new(mkv::TrackKind::Audio, "A_AC3");
    a.audio = Some((48000.0, 6, None));
    a.default_duration_ns = Some(32_000_000);
    a.language = Some("tur".into());
    let mut s = mkv::TrackSpec::new(mkv::TrackKind::Subtitle, "S_TEXT/UTF8");
    s.language = Some("eng".into());
    let opts = mkv::MuxOptions { cluster_duration_ns: 1_000_000_000, ..Default::default() };
    let mut w = mkv::MkvWriter::new(Cursor::new(Vec::new()), vec![v, a, s], opts).unwrap();
    // (pts ms, iz, anahtar, veri, süre ns) — zamana göre sıralı
    let mut fr: Vec<(u64, usize, bool, Vec<u8>, Option<u64>)> = Vec::new();
    for i in 0..750u64 {
        fr.push((i * 40, 0, i % 50 == 0, vec![i as u8; 40], None));
    }
    for k in 0..937u64 {
        fr.push((k * 32, 1, true, ac3_frame(), None));
    }
    for k in 0..10u64 {
        fr.push((k * 3000 + 500, 2, true, format!("Satir {k}").into_bytes(), Some(2_000_000_000)));
    }
    fr.sort_by_key(|f| (f.0, f.1));
    for (ms, t, key, d, dur) in fr {
        w.write_frame(t, (ms * 1_000_000) as i64, key, &d, dur).unwrap();
    }
    w.finish().unwrap().into_inner()
}

struct CountingSource {
    path: PathBuf,
    len: u64,
    opens: Arc<AtomicUsize>,
}

impl MediaSource for CountingSource {
    fn open(&self) -> anyhow::Result<Box<dyn ReadSeek>> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        // Eşzamanlılık testi için üretimi biraz yavaşlat.
        std::thread::sleep(Duration::from_millis(60));
        Ok(Box::new(std::fs::File::open(&self.path)?))
    }
    fn len(&self) -> u64 {
        self.len
    }
}

struct TestHost {
    files: HashMap<(usize, usize), PathBuf>,
    names: Vec<(usize, String)>,
    opens: Arc<AtomicUsize>,
    focus: Mutex<Vec<(f64, Range<u64>)>>,
    keep: Mutex<Vec<(f64, Range<u64>)>>,
    empty_index: bool,
}

impl HlsHost for TestHost {
    fn source(&self, id: usize, file: usize) -> anyhow::Result<Option<Arc<dyn MediaSource>>> {
        let Some(p) = self.files.get(&(id, file)) else { return Ok(None) };
        let len = std::fs::metadata(p)?.len();
        Ok(Some(Arc::new(CountingSource { path: p.clone(), len, opens: self.opens.clone() })))
    }
    fn media_info(&self, id: usize, file: usize) -> Fut<'_, Arc<MediaInfo>> {
        let p = self.files[&(id, file)].clone();
        Box::pin(async move { Ok(Arc::new(demux::probe(std::fs::File::open(p)?)?)) })
    }
    fn time_index(&self, id: usize, file: usize) -> Fut<'_, Arc<TimeIndex>> {
        let p = self.files[&(id, file)].clone();
        let empty = self.empty_index;
        Box::pin(async move {
            if empty {
                return Ok(Arc::new(TimeIndex::default()));
            }
            Ok(Arc::new(demux::time_index(std::fs::File::open(p)?)?))
        })
    }
    fn atmos(&self, _id: usize, _file: usize) -> HashMap<u64, AtmosState> {
        HashMap::new()
    }
    fn files(&self, _id: usize) -> Vec<(usize, String)> {
        self.names.clone()
    }
    fn prepare_file(&self, _id: usize, _video: usize, _side: usize) -> Fut<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn playhead_secs(&self, _id: usize, _file: usize) -> Option<f64> {
        None
    }
    fn focus(&self, _id: usize, _file: usize, t: f64, _duration: f64, _bitrate: f64, range: Range<u64>) -> Fut<'_, ()> {
        self.focus.lock().unwrap().push((t, range));
        Box::pin(async { Ok(()) })
    }
    fn keep_alive(&self, _id: usize, _file: usize, t: f64, _duration: f64, _bitrate: f64, range: Range<u64>) -> Fut<'_, ()> {
        self.keep.lock().unwrap().push((t, range));
        Box::pin(async { Ok(()) })
    }
}

struct Fixture {
    addr: SocketAddr,
    host: Arc<TestHost>,
    _dir: tempfile::TempDir,
}

struct Reply {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Basit HTTP/1.1 istemcisi (`Connection: close`).
fn http_get(addr: SocketAddr, path: &str) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines.next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    if matches!(&headers.get("transfer-encoding"), Some(t) if t.contains("chunked")) {
        body = dechunk(&body);
    }
    Reply { status, headers, body }
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let nl = b.windows(2).position(|w| w == b"\r\n").unwrap();
        let n = usize::from_str_radix(std::str::from_utf8(&b[..nl]).unwrap().trim(), 16).unwrap();
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&b[nl + 2..nl + 2 + n]);
        b = &b[nl + 2 + n + 2..];
    }
}

async fn fixture(with_external: bool, empty_index: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let movie_path = dir.path().join("film.mkv");
    std::fs::write(&movie_path, movie()).unwrap();
    let mut files = HashMap::new();
    files.insert((7, 0), movie_path);
    let mut names = vec![(0, "film.mkv".to_string())];
    if with_external {
        let srt = dir.path().join("film.tr.srt");
        std::fs::write(&srt, "1\n00:00:01,000 --> 00:00:02,000\nMerhaba\n").unwrap();
        files.insert((7, 1), srt);
        names.push((1, "film.tr.srt".into()));
        names.push((2, "readme.txt".into()));
    }
    let host = Arc::new(TestHost { files, names, opens: Arc::new(AtomicUsize::new(0)), focus: Mutex::new(Vec::new()), keep: Mutex::new(Vec::new()), empty_index });
    let app = routes_with(host.clone(), Arc::new(HlsCore::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Fixture { addr, host, _dir: dir }
}

async fn get(f: &Fixture, path: &str) -> Reply {
    let (addr, p) = (f.addr, path.to_string());
    tokio::task::spawn_blocking(move || http_get(addr, &p)).await.unwrap()
}

fn track_ids() -> (u64, u64, u64) {
    let info = demux::probe(Cursor::new(movie())).unwrap();
    let id = |k| info.tracks.iter().find(|t| t.kind == k).unwrap().id;
    (id(demux::TrackKind::Video), id(demux::TrackKind::Audio), id(demux::TrackKind::Subtitle))
}

#[tokio::test(flavor = "multi_thread")]
async fn master_lists_audio_and_subs() {
    let f = fixture(false, false).await;
    let (_, a, s) = track_ids();
    let r = get(&f, "/hls/7/0/master.m3u8").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.headers["content-type"], "application/vnd.apple.mpegurl");
    assert_eq!(r.headers["cache-control"], "no-cache");
    let m = r.text();
    println!("--- master.m3u8 ---\n{m}");
    assert!(m.contains("#EXT-X-STREAM-INF"), "{m}");
    assert_eq!(m.matches("TYPE=AUDIO").count(), 1, "{m}");
    assert_eq!(m.matches("TYPE=SUBTITLES").count(), 1, "{m}");
    assert!(m.contains(&format!("URI=\"a{a}.m3u8\"")), "{m}");
    assert!(m.contains(&format!("URI=\"s{s}.m3u8\"")), "{m}");
    assert!(m.contains("CODECS=\"avc1.42c01e,ac-3\""), "{m}");

    // Medya listeleri
    let v = get(&f, "/hls/7/0/v.m3u8").await;
    assert_eq!(v.status, 200);
    let v = v.text();
    println!("--- v.m3u8 ---\n{v}");
    assert!(v.contains("#EXT-X-MAP:URI=\"v/init.mp4\""), "{v}");
    assert!(v.contains("v/0.m4s") && v.contains("v/4.m4s") && !v.contains("v/5.m4s"), "{v}");
    let ap = get(&f, &format!("/hls/7/0/a{a}.m3u8")).await.text();
    assert!(ap.contains(&format!("#EXT-X-MAP:URI=\"a{a}/init.mp4\"")), "{ap}");
    let sp = get(&f, &format!("/hls/7/0/s{s}.m3u8")).await.text();
    assert!(sp.contains(&format!("s{s}/0.vtt")) && !sp.contains("EXT-X-MAP"), "{sp}");
}

#[tokio::test(flavor = "multi_thread")]
async fn video_segment_parses() {
    let f = fixture(false, false).await;
    let init = get(&f, "/hls/7/0/v/init.mp4").await;
    assert_eq!(init.status, 200);
    assert_eq!(init.headers["content-type"], "video/mp4");
    let seg = get(&f, "/hls/7/0/v/1.m4s").await;
    assert_eq!(seg.status, 200, "{}", seg.text());
    assert_eq!(seg.headers["content-type"], "video/mp4");
    assert_eq!(seg.headers["cache-control"], "max-age=3600");
    let mut all = init.body.clone();
    all.extend_from_slice(&seg.body);
    let file = mp4::open(all).expect("init + segment filmcraft-isobmff ile okunur");
    let t = file.tracks.iter().find(|t| t.kind == mp4::TrackKind::Video).expect("video izi");
    assert!(t.samples.len() >= 100, "6 sn x 25 fps: {}", t.samples.len());
    assert!(t.samples[0].is_sync);
    // Playhead segmentin başına çekildi (6 sn) ve bayt aralığı bildirildi.
    let focus = f.host.focus.lock().unwrap().clone();
    let (t0, range) = focus.last().expect("focus çağrıldı").clone();
    assert!((t0 - 6.0).abs() < 1e-6, "{t0}");
    assert!(range.end > range.start, "{range:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn audio_init_has_ac3() {
    let f = fixture(false, false).await;
    let (_, a, _) = track_ids();
    let init = get(&f, &format!("/hls/7/0/a{a}/init.mp4")).await;
    assert_eq!(init.status, 200, "{}", init.text());
    assert_eq!(init.headers["content-type"], "audio/mp4");
    assert!(init.body.windows(4).any(|w| w == b"ac-3"), "ac-3 örnek girdisi");
    assert!(init.body.windows(4).any(|w| w == b"dac3"));
    let seg = get(&f, &format!("/hls/7/0/a{a}/2.m4s")).await;
    assert_eq!(seg.status, 200, "{}", seg.text());
    assert_eq!(seg.headers["content-type"], "audio/mp4");
    assert!(seg.body.windows(4).any(|w| w == b"moof"));
    // Ses isteği playhead'i TAŞIMAZ; yalnızca keep-alive.
    assert!(f.host.focus.lock().unwrap().is_empty(), "ses focus çağırmamalı");
    assert!(!f.host.keep.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_track_404() {
    let f = fixture(false, false).await;
    for p in [
        "/hls/7/0/a99.m3u8",
        "/hls/7/0/a99/init.mp4",
        "/hls/7/0/a99/0.m4s",
        "/hls/7/0/s99.m3u8",
        "/hls/7/0/s99/0.vtt",
        "/hls/7/0/v/999.m4s",
        "/hls/7/0/zzz.m3u8",
        "/hls/7/5/master.m3u8",
        "/hls/8/0/master.m3u8",
    ] {
        let r = get(&f, p).await;
        assert_eq!(r.status, 404, "{p}: {}", r.text());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn subtitle_segment_and_external() {
    let f = fixture(true, false).await;
    let (_, _, s) = track_ids();
    let r = get(&f, &format!("/hls/7/0/s{s}/0.vtt")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.headers["content-type"], "text/vtt");
    let t = r.text();
    assert!(t.starts_with("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000"), "{t}");
    assert!(t.contains("Satir 0"), "{t}");

    // Harici: dosya 1 -> id 1_000_001; readme.txt listelenmez.
    let m = get(&f, "/hls/7/0/master.m3u8").await.text();
    println!("--- master.m3u8 (harici altyazılı) ---\n{m}");
    assert_eq!(m.matches("TYPE=SUBTITLES").count(), 2, "{m}");
    assert!(m.contains("URI=\"s1000001.m3u8\"") && m.contains("LANGUAGE=\"tr\""), "{m}");
    assert!(!m.contains("s1000002"), "{m}");
    let pl = get(&f, "/hls/7/0/s1000001.m3u8").await.text();
    assert!(pl.contains("s1000001/0.vtt") && pl.contains("#EXT-X-ENDLIST"), "{pl}");
    let v = get(&f, "/hls/7/0/s1000001/0.vtt").await;
    assert_eq!(v.status, 200, "{}", v.text());
    assert!(v.text().contains("00:00:01.000 --> 00:00:02.000\nMerhaba"), "{}", v.text());
    assert_eq!(get(&f, "/hls/7/0/s1000001/1.vtt").await.status, 404);
    assert_eq!(get(&f, "/hls/7/0/s1000002.m3u8").await.status, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_keyframe_index_is_422_with_turkish_error() {
    let f = fixture(false, true).await;
    let r = get(&f, "/hls/7/0/master.m3u8").await;
    assert_eq!(r.status, 422);
    let j: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(j["error"], "Bu dosyada anahtar kare dizini yok; iPhone'da oynatılamıyor");
    assert_eq!(get(&f, "/hls/7/0/v.m3u8").await.status, 422);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_requests_share_one_generation() {
    let f = fixture(false, false).await;
    // Dosya durumunu kur (açılışları say).
    assert_eq!(get(&f, "/hls/7/0/master.m3u8").await.status, 200);
    let before = f.host.opens.load(Ordering::SeqCst);
    let mut hs = Vec::new();
    for _ in 0..6 {
        let (addr, ) = (f.addr,);
        hs.push(tokio::task::spawn_blocking(move || http_get(addr, "/hls/7/0/v/2.m4s")));
    }
    let mut bodies = Vec::new();
    for h in hs {
        let r = h.await.unwrap();
        assert_eq!(r.status, 200);
        bodies.push(r.body);
    }
    assert!(bodies.windows(2).all(|w| w[0] == w[1]));
    assert_eq!(f.host.opens.load(Ordering::SeqCst) - before, 1, "tek üretim");
    // Önbellekten: yeni açılış yok.
    assert_eq!(get(&f, "/hls/7/0/v/2.m4s").await.status, 200);
    assert_eq!(f.host.opens.load(Ordering::SeqCst) - before, 1);
    // Önbellek isabeti ve aynı aralığın tekrarı odaklamayı yinelemez (dedupe): 6+1 istek, 1 focus.
    assert_eq!(f.host.focus.lock().unwrap().len(), 1);
}
