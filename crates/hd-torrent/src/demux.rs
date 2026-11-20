//! Pure-Rust demux layer (no ffmpeg) on top of `filmcraft-matroska` 0.9 and `filmcraft-isobmff` 0.9.
//!
//! * [`probe`]: container + track metadata from MKV/WebM or MP4/MOV, reading only headers
//!   (MKV: EBML header, SeekHead, Info, Tracks, Cues; MP4: moov).
//! * [`PacketReader`]: sequential single-track packet reader that seeks through Cues (MKV) or the
//!   sample tables (MP4) instead of scanning the file.
//!
//! Dependencies: `anyhow`, `filmcraft-matroska = "0.9"`, `filmcraft-isobmff = "0.9"`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

use anyhow::{Context, Result, anyhow, bail};
use filmcraft_isobmff as mp4;
use filmcraft_matroska as mkv;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
    Other,
}

/// ISO/H.273 renk tanımı (`colr nclx` alanları).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColourInfo {
    pub primaries: u16,
    pub transfer: u16,
    pub matrix: u16,
    pub full_range: bool,
}

/// Kapsayıcının (MKV Colour / MP4 colr) bildirdiği, kısmen eksik olabilen renk bilgisi.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ContainerColour {
    primaries: Option<u16>,
    transfer: Option<u16>,
    matrix: Option<u16>,
    full_range: Option<bool>,
}

/// H.273'te 0 (ayrılmış) ve 2 (belirtilmemiş) "bilinmiyor" sayılır.
fn known(v: Option<u16>) -> Option<u16> {
    v.filter(|v| !matches!(v, 0 | 2))
}

/// Kapsayıcı bilgisi varsa onu kullanır (asla ezmez); yalnızca kapsayıcı aktarım (transfer)
/// bilgisi vermediyse SPS VUI'ye düşer. Kapsayıcının transferi VUI ile aynıysa eksik
/// primaries/matrix VUI'den tamamlanır.
fn resolve_colour(c: ContainerColour, vui: Option<crate::hls::sps::VuiColour>) -> Option<ColourInfo> {
    let vui = vui.map(|v| ColourInfo {
        primaries: v.primaries as u16,
        transfer: v.transfer as u16,
        matrix: v.matrix as u16,
        full_range: v.full_range,
    });
    let Some(transfer) = known(c.transfer) else {
        // Kapsayıcı transfer vermedi: transfer yalnızca VUI'den (bilinmiyorsa hiç). Diğer alanlar
        // `kapsayıcı.or(VUI)`; kapsayıcının bilinen primaries/matrix'i VUI ile çelişirse
        // (örn. BT.709 vs BT.2020+PQ) HDR iddia edilmez: colour = None (hdr de None kalır).
        let v = vui.filter(|v| known(Some(v.transfer)).is_some())?;
        let agree = |c: Option<u16>, v: u16| known(c).is_none_or(|c| known(Some(v)).is_none_or(|v| v == c));
        if !agree(c.primaries, v.primaries) || !agree(c.matrix, v.matrix) {
            tracing::warn!(
                "kapsayıcı renk bilgisi SPS VUI ile çelişiyor (primaries {:?}/{}, matrix {:?}/{}); HDR sinyali verilmiyor",
                c.primaries, v.primaries, c.matrix, v.matrix
            );
            return None;
        }
        return Some(ColourInfo {
            primaries: known(c.primaries).unwrap_or(v.primaries),
            transfer: v.transfer,
            matrix: known(c.matrix).unwrap_or(v.matrix),
            full_range: c.full_range.unwrap_or(v.full_range),
        });
    };
    let same = vui.filter(|v| v.transfer == transfer);
    Some(ColourInfo {
        primaries: known(c.primaries).or(same.map(|v| v.primaries))?,
        transfer,
        matrix: known(c.matrix).or(same.map(|v| v.matrix))?,
        full_range: c.full_range.or(same.map(|v| v.full_range)).unwrap_or(false),
    })
}

/// Video izinde `colour` ve (kapsayıcı vermediyse) `hdr` alanlarını kesinleştirir. Yalnızca DV
/// profil 5 (`dvh1`, geriye uyumsuz; renk RPU'dan gelir) ve bl_compat 0 atlanır; P8.x (bl_compat 1/2/4), P7 ve
/// düz HEVC temel katmanının renk bilgisi normal çözülür.
pub(crate) fn finalize_video_colour(ti: &mut TrackInfo, c: ContainerColour) {
    if ti.dv_profile == Some(5) || ti.dv_bl_compat == Some(0) {
        ti.colour = None;
        return;
    }
    let vui = crate::hls::sps::vui_colour(&ti.codec, &ti.codec_private);
    ti.colour = resolve_colour(c, vui);
    if known(c.transfer).is_none() {
        ti.hdr = ti.colour.and_then(|x| hdr_from_transfer(x.transfer as u64));
    }
}

#[derive(Clone, Debug)]
pub struct TrackInfo {
    /// Track number used to pull packets (MKV TrackNumber / MP4 track_ID).
    pub id: u64,
    pub kind: TrackKind,
    /// Normalized codec name ("h264", "hevc", "av1", "vp9", "aac", "ac3", "eac3", "truehd", "dts",
    /// "flac", "opus", "vorbis", "mp3", "pcm", "srt", "ass", "pgs", ...).
    pub codec: String,
    /// Raw codec id ("A_TRUEHD", "V_MPEGH/ISO/HEVC", or the MP4 sample-entry fourcc).
    pub codec_id: String,
    #[allow(dead_code)]
    pub codec_private: Vec<u8>,
    pub language: Option<String>,
    pub name: Option<String>,
    pub default: bool,
    pub channels: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// "HDR10" (transfer 16 / PQ) or "HLG" (transfer 18).
    pub hdr: Option<String>,
    /// Çözümlenmiş renk tanımı (kapsayıcı, yoksa SPS VUI); HLS init'inde `colr` kutusu için.
    pub colour: Option<ColourInfo>,
    pub dv_profile: Option<u8>,
    /// Dolby Vision düzeyi (DOVIDecoderConfigurationRecord `dv_level`, 1..=13); HLS codec dizesi için.
    pub dv_level: Option<u8>,
    pub dv_bl_compat: Option<u8>,
    /// Ham DOVI kod çözücü yapılandırma kaydı (24 bayt: MKV `BlockAdditionMapping` ExtraData ya da
    /// MP4 `dvcC`/`dvvC`/`dvwC` kutusunun yükü); HLS/fMP4 kutusu için.
    pub dv_config: Option<Vec<u8>>,
    /// `dv_config`in geldiği kutu türü: `dvcC`, `dvvC` ya da `dvwC`.
    pub dv_config_fourcc: Option<[u8; 4]>,
    /// Kare/çerçeve başına varsayılan süre (ns): MKV `DefaultDuration`, MP4 ilk örnek süresi.
    pub default_duration_ns: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    /// "matroska", "webm", "mp4" or "mov".
    pub container: &'static str,
    pub duration_secs: Option<f64>,
    /// MKV Segment Info `Title` (yoksa/boşsa `None`); film adı çıkarımında kullanılır.
    pub title: Option<String>,
    pub tracks: Vec<TrackInfo>,
}

#[derive(Clone, Debug)]
pub struct Packet {
    pub pts_secs: f64,
    pub data: Vec<u8>,
    #[allow(dead_code)]
    pub keyframe: bool,
    /// Packet duration in seconds when the container records it (altyazı ipuçları için).
    pub duration_secs: Option<f64>,
}

// ---------------------------------------------------------------------------------------------
// Byte source adapter (one RefCell<R>, usable as both crates' ByteSource)
// ---------------------------------------------------------------------------------------------

struct Src<R> {
    inner: RefCell<(R, u64)>, // (reader, current position or u64::MAX if unknown)
    len: u64,
}

impl<R: Read + Seek> Src<R> {
    fn new(mut r: R) -> std::io::Result<Self> {
        let len = r.seek(SeekFrom::End(0))?;
        Ok(Self { inner: RefCell::new((r, u64::MAX)), len })
    }
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let end = offset.checked_add(buf.len() as u64);
        if end.is_none_or(|e| e > self.len) {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let mut g = self.inner.borrow_mut();
        let (r, pos) = &mut *g;
        if *pos != offset {
            r.seek(SeekFrom::Start(offset))?;
        }
        match r.read_exact(buf) {
            Ok(()) => {
                *pos = offset + buf.len() as u64;
                Ok(())
            }
            Err(e) => {
                *pos = u64::MAX;
                Err(e)
            }
        }
    }
}

impl<R: Read + Seek> mkv::ByteSource for Src<R> {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.read_exact_at(offset, buf)
    }
}
impl<R: Read + Seek> mp4::ByteSource for Src<R> {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.read_exact_at(offset, buf)
    }
}

// ---------------------------------------------------------------------------------------------
// Container detection
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Mkv,
    Mp4,
}

fn detect<R: Read + Seek>(r: &mut R) -> Result<Kind> {
    r.seek(SeekFrom::Start(0))?;
    let mut h = [0u8; 12];
    let mut n = 0;
    while n < h.len() {
        let k = r.read(&mut h[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    r.seek(SeekFrom::Start(0))?;
    if n >= 4 && h[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Ok(Kind::Mkv);
    }
    if n >= 8 && matches!(&h[4..8], b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"pnot") {
        return Ok(Kind::Mp4);
    }
    bail!("unrecognised container (not Matroska/WebM or MP4/MOV)")
}

// ---------------------------------------------------------------------------------------------
// probe
// ---------------------------------------------------------------------------------------------

/// Detect the container (by magic) and read track metadata. Reads only headers: for MKV the EBML
/// header, Info, Tracks and (via SeekHead) Cues; for MP4 the `moov` box.
pub fn probe<R: Read + Seek>(mut r: R) -> Result<MediaInfo> {
    match detect(&mut r)? {
        Kind::Mkv => probe_mkv(r),
        Kind::Mp4 => probe_mp4(r),
    }
}

fn hdr_from_transfer(t: u64) -> Option<String> {
    match t {
        16 => Some("HDR10".into()),
        18 => Some("HLG".into()),
        _ => None,
    }
}

fn mkv_codec_name(t: &mkv::Track) -> String {
    use mkv::Codec::*;
    match &t.codec {
        SubRip => "srt".into(),
        Ass { .. } => "ass".into(),
        Pcm { .. } => "pcm".into(),
        Other(id) => {
            let u = id.to_ascii_uppercase();
            let name = match u.as_str() {
                "A_TRUEHD" => "truehd",
                "A_MLP" => "mlp",
                "A_DTS" | "A_DTS/EXPRESS" | "A_DTS/LOSSLESS" => "dts",
                "A_MS/ACM" => "acm",
                "S_HDMV/PGS" => "pgs",
                "S_HDMV/TEXTST" => "textst",
                "S_VOBSUB" => "vobsub",
                "S_DVBSUB" => "dvbsub",
                "S_TEXT/USF" => "usf",
                "V_MPEG2" => "mpeg2",
                "V_MPEG4/ISO/ASP" | "V_MPEG4/ISO/SP" | "V_MPEG4/ISO/AP" => "mpeg4",
                "V_MPEG4/MS/V3" => "msmpeg4v3",
                "V_THEORA" => "theora",
                "V_VC1" | "V_MS/VFW/FOURCC" => "vc1",
                "A_QUICKTIME" => "qdm2",
                _ if u.starts_with("A_PCM/") => "pcm",
                _ => return id.to_ascii_lowercase(),
            };
            name.into()
        }
        other => other.name(),
    }
}

/// DOVIDecoderConfigurationRecord alanları (Dolby Vision Streams Within the ISO Base Media File
/// Format, §3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DoviConfig {
    pub profile: u8,
    pub level: u8,
    pub bl_compat: u8,
    pub rpu: bool,
    pub el: bool,
    pub bl: bool,
}

/// Ham kayıt + geldiği kutu türü.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DoviEntry {
    cfg: DoviConfig,
    raw: Vec<u8>,
    fourcc: [u8; 4],
}

/// 'dvcC' / 'dvvC' / 'dvwC' (MKV BlockAddIDType ya da MP4 kutu türü).
fn dovi_fourcc(ty: u32) -> Option<[u8; 4]> {
    let f = ty.to_be_bytes();
    matches!(&f, b"dvcC" | b"dvvC" | b"dvwC").then_some(f)
}

/// Dolby Vision config from BlockAdditionMapping (type 'dvcC' / 'dvvC' / 'dvwC'), per track number.
/// The matroska crate skips BlockAdditionMapping, so the Tracks element is re-read and parsed here.
fn mkv_dolby_vision<S: mkv::ByteSource + ?Sized>(src: &S, seg_start: u64) -> HashMap<u64, DoviEntry> {
    let out = HashMap::new();
    let len = src.len();
    let mut pos = seg_start;
    // Walk level-1 headers until Tracks (always before the first Cluster in practice).
    for _ in 0..64 {
        if pos >= len {
            return out;
        }
        let mut hb = [0u8; 12];
        let n = ((len - pos) as usize).min(12);
        if src.read_at(pos, &mut hb[..n]).is_err() {
            return out;
        }
        let Some((id, il)) = mkv::ebml::read_id(&hb[..n]) else { return out };
        let Some((size, sl)) = mkv::ebml::read_size(&hb[il..n]) else { return out };
        let Some(sz) = size.known() else { return out };
        let data = pos + (il + sl) as u64;
        if id == 0x1F43_B675 {
            return out; // Cluster: Tracks not found before media data
        }
        if id == 0x1654_AE6B {
            if sz > 16 << 20 || data + sz > len {
                return out;
            }
            let mut buf = vec![0u8; sz as usize];
            if src.read_at(data, &mut buf).is_err() {
                return out;
            }
            return dv_from_tracks(&buf);
        }
        pos = data + sz;
    }
    out
}

fn dv_from_tracks(tracks: &[u8]) -> HashMap<u64, DoviEntry> {
    let mut out = HashMap::new();
    for te in mkv::ebml::Children::new(tracks).filter(|e| e.id == 0xAE) {
        let mut number = None;
        let mut dv = None;
        for c in mkv::ebml::Children::new(te.data) {
            match c.id {
                0xD7 => number = Some(mkv::ebml::uint(c.data)),
                0x41E4 => {
                    let (mut ty, mut extra) = (0u64, &[][..]);
                    for m in mkv::ebml::Children::new(c.data) {
                        match m.id {
                            0x41E7 => ty = mkv::ebml::uint(m.data),
                            0x41ED => extra = m.data,
                            _ => {}
                        }
                    }
                    if let (Some(fourcc), Some(cfg)) = (u32::try_from(ty).ok().and_then(dovi_fourcc), parse_dovi_config(extra)) {
                        dv = Some(DoviEntry { cfg, raw: extra.to_vec(), fourcc });
                    }
                }
                _ => {}
            }
        }
        if let (Some(n), Some(d)) = (number, dv) {
            out.insert(n, d);
        }
    }
    out
}

/// DOVIDecoderConfigurationRecord: dv_version_major(8) dv_version_minor(8) dv_profile(7)
/// dv_level(6) rpu_present(1) el_present(1) bl_present(1) dv_bl_signal_compatibility_id(4) ...
pub fn parse_dovi_config(b: &[u8]) -> Option<DoviConfig> {
    if b.len() < 5 {
        return None;
    }
    Some(DoviConfig {
        profile: b[2] >> 1,
        level: ((b[2] & 1) << 5) | (b[3] >> 3),
        rpu: b[3] & 4 != 0,
        el: b[3] & 2 != 0,
        bl: b[3] & 1 != 0,
        bl_compat: b[4] >> 4,
    })
}

/// MP4 görsel örnek girdisinin (`dvh1`/`dvhe`/`dvav`/`dva1`/`dav1`...) ham yükünden (8 baytlık kutu
/// başlığından sonrası) `dvcC`/`dvvC`/`dvwC` alt kutusunu bulur. Görsel örnek girdisinin sabit
/// alanları 78 bayttır.
fn mp4_dovi_from_entry(raw: &[u8]) -> Option<DoviEntry> {
    let mut p = 78usize;
    while p + 8 <= raw.len() {
        let size = u32::from_be_bytes(raw[p..p + 4].try_into().ok()?) as usize;
        let ty: [u8; 4] = raw[p + 4..p + 8].try_into().ok()?;
        let (hdr, total) = match size {
            0 => (8, raw.len() - p),
            1 => (16, u64::from_be_bytes(raw.get(p + 8..p + 16)?.try_into().ok()?) as usize),
            n => (8, n),
        };
        if total < hdr || p.saturating_add(total) > raw.len() {
            return None;
        }
        if matches!(&ty, b"dvcC" | b"dvvC" | b"dvwC") {
            let body = &raw[p + hdr..p + total];
            return parse_dovi_config(body).map(|cfg| DoviEntry { cfg, raw: body.to_vec(), fourcc: ty });
        }
        p += total;
    }
    None
}

fn apply_dovi(ti: &mut TrackInfo, d: &DoviEntry) {
    ti.dv_profile = Some(d.cfg.profile);
    ti.dv_level = Some(d.cfg.level);
    ti.dv_bl_compat = Some(d.cfg.bl_compat);
    ti.dv_config = Some(d.raw.clone());
    ti.dv_config_fourcc = Some(d.fourcc);
}

fn opt_str(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn mkv_track_info(t: &mkv::Track, dv: &HashMap<u64, DoviEntry>) -> TrackInfo {
    let kind = match t.kind {
        mkv::TrackKind::Video => TrackKind::Video,
        mkv::TrackKind::Audio => TrackKind::Audio,
        mkv::TrackKind::Subtitle => TrackKind::Subtitle,
        _ => TrackKind::Other,
    };
    let lang = opt_str(t.language_bcp47.as_deref().unwrap_or(&t.language)).filter(|l| l != "und");
    let mut ti = TrackInfo {
        id: t.number,
        kind,
        codec: mkv_codec_name(t),
        codec_id: t.codec_id.clone(),
        codec_private: t.codec_private.clone(),
        language: lang,
        name: opt_str(&t.name),
        default: t.default,
        channels: None,
        sample_rate: None,
        bit_depth: None,
        width: None,
        height: None,
        hdr: None,
        colour: None,
        dv_profile: None,
        dv_level: None,
        dv_bl_compat: None,
        dv_config: None,
        dv_config_fourcc: None,
        default_duration_ns: t.default_duration_ns.filter(|d| *d > 0),
    };
    if let Some(a) = &t.audio {
        ti.channels = Some(a.channels as u32);
        ti.sample_rate = Some(a.output_sampling_frequency.unwrap_or(a.sampling_frequency).round() as u32);
        ti.bit_depth = a.bit_depth.map(|b| b as u32);
    }
    if let Some(v) = &t.video {
        ti.width = Some(v.pixel_width);
        ti.height = Some(v.pixel_height);
        if let Some(c) = &v.colour {
            ti.hdr = c.transfer_characteristics.and_then(hdr_from_transfer);
            ti.bit_depth = c.bits_per_channel.filter(|b| *b > 0).map(|b| b as u32);
        }
        if ti.bit_depth.is_none() && matches!(t.codec, mkv::Codec::Hevc { .. }) {
            // hvcC byte 17: bitDepthLumaMinus8 in the low 3 bits
            ti.bit_depth = t.codec_private.get(17).map(|b| (b & 7) as u32 + 8);
        }
        if let Some(d) = dv.get(&t.number) {
            apply_dovi(&mut ti, d);
        }
        let cc = v.colour.as_ref().map(|c| ContainerColour {
            primaries: c.primaries.and_then(|x| u16::try_from(x).ok()),
            transfer: c.transfer_characteristics.and_then(|x| u16::try_from(x).ok()),
            matrix: c.matrix_coefficients.and_then(|x| u16::try_from(x).ok()),
            full_range: match c.range {
                Some(1) => Some(false),
                Some(2) => Some(true),
                _ => None,
            },
        });
        finalize_video_colour(&mut ti, cc.unwrap_or_default());
    }
    ti
}

fn probe_mkv<R: Read + Seek>(r: R) -> Result<MediaInfo> {
    let src = Src::new(r)?;
    // index:false => stop at the first Cluster, follow SeekHead for Cues; no whole-file scan.
    let opts = mkv::OpenOptions { index: false, verify_crc: false };
    let f = mkv::open_with(&src, &opts).map_err(|e| anyhow!("matroska: {e}"))?;
    let dv = mkv_dolby_vision(&src, f.segment_data_start);
    Ok(MediaInfo {
        container: if f.is_webm() { "webm" } else { "matroska" },
        duration_secs: f.duration_ns().map(|n| n as f64 / 1e9),
        title: f.info.title.as_deref().and_then(opt_str),
        tracks: f.tracks.iter().map(|t| mkv_track_info(t, &dv)).collect(),
    })
}

fn mp4_codec(t: &mp4::Track) -> (String, String) {
    let Some(e) = t.entries.first() else { return ("unknown".into(), String::new()) };
    let fourcc = e.format.to_string_lossy();
    let name = match &e.codec {
        mp4::CodecConfig::Pcm(_) => "pcm".to_string(),
        mp4::CodecConfig::Unknown { fourcc: f, .. } => {
            let s = f.to_string_lossy();
            match s.as_str() {
                "dvh1" | "dvhe" => "hevc".into(),
                "dvav" | "dva1" => "h264".into(),
                "dav1" => "av1".into(),
                "mp4a" => "aac".into(),
                "mlpa" => "truehd".into(),
                "dtsc" | "dtsh" | "dtsl" | "dtse" => "dts".into(),
                "tx3g" => "mov_text".into(),
                "wvtt" => "webvtt".into(),
                _ => s.trim().to_ascii_lowercase(),
            }
        }
        other => other.name().to_string(),
    };
    (name, fourcc)
}

fn mp4_codec_private(c: &mp4::CodecConfig) -> Vec<u8> {
    use mp4::CodecConfig::*;
    match c {
        Avc(c) => c.to_bytes(),
        Hevc(c) => c.to_bytes(),
        Av1(c) => c.to_bytes(),
        Vp9(c) => c.to_bytes(),
        Aac(c) => c.asc.clone(),
        Opus(c) => c.to_bytes(),
        Alac { cookie } => cookie.clone(),
        Ac3 { dac3 } => dac3.clone(),
        Eac3 { dec3 } => dec3.clone(),
        Unknown { raw, .. } => raw.clone(),
        _ => Vec::new(),
    }
}

fn mp4_track_info(t: &mp4::Track) -> TrackInfo {
    let kind = match t.kind {
        mp4::TrackKind::Video => TrackKind::Video,
        mp4::TrackKind::Audio => TrackKind::Audio,
        mp4::TrackKind::Subtitle => TrackKind::Subtitle,
        _ => TrackKind::Other,
    };
    let (codec, codec_id) = mp4_codec(t);
    let e = t.entries.first();
    let mut ti = TrackInfo {
        id: t.id as u64,
        kind,
        codec,
        codec_id,
        codec_private: e.map(|e| mp4_codec_private(&e.codec)).unwrap_or_default(),
        language: opt_str(&t.language).filter(|l| l != "und" && l != "und\0"),
        name: opt_str(&t.handler_name).filter(|n| !n.eq_ignore_ascii_case("SoundHandler") && !n.eq_ignore_ascii_case("VideoHandler")),
        default: t.enabled,
        channels: None,
        sample_rate: None,
        bit_depth: None,
        width: None,
        height: None,
        hdr: None,
        colour: None,
        dv_profile: None,
        dv_level: None,
        dv_bl_compat: None,
        dv_config: None,
        dv_config_fourcc: None,
        default_duration_ns: t.samples.first().filter(|_| t.timescale > 0 && t.kind == mp4::TrackKind::Video).map(|x| x.duration as u64 * 1_000_000_000 / t.timescale as u64).filter(|d| *d > 0),
    };
    if let Some(a) = t.audio() {
        ti.channels = Some(a.channels);
        ti.sample_rate = Some(a.sample_rate.round() as u32);
        ti.bit_depth = (a.sample_size > 0 && matches!(e.map(|e| &e.codec), Some(mp4::CodecConfig::Pcm(_)))).then_some(a.sample_size as u32);
        if let Some(mp4::CodecConfig::Aac(c)) = e.map(|e| &e.codec) {
            ti.channels = Some(c.channels()).filter(|c| *c > 0).or(ti.channels);
        }
    }
    if let Some(v) = t.video() {
        ti.width = Some(v.width as u32);
        ti.height = Some(v.height as u32);
        if let Some(mp4::ColorInfo::Nclx { transfer, .. } | mp4::ColorInfo::Nclc { transfer, .. }) = &v.color {
            ti.hdr = hdr_from_transfer(*transfer as u64);
        }
        if let Some(mp4::CodecConfig::Hevc(h)) = e.map(|e| &e.codec) {
            ti.bit_depth = Some(h.bit_depth_luma as u32).filter(|b| *b > 0);
        }
        // filmcraft-isobmff 0.9 parses no Dolby Vision boxes: only entries it keeps raw (`dvh1`,
        // `dvhe`, `dvav`, `dva1`, `dav1`) can be searched for dvcC/dvvC/dvwC. An `hvc1`/`avc1`
        // entry with a dvcC sibling (profile 8 compatible signalling) is not reachable -> None.
        if let Some(mp4::CodecConfig::Unknown { raw, .. }) = e.map(|e| &e.codec)
            && let Some(d) = mp4_dovi_from_entry(raw)
        {
            apply_dovi(&mut ti, &d);
        }
        let cc = match &v.color {
            Some(mp4::ColorInfo::Nclx { primaries, transfer, matrix, full_range }) => ContainerColour {
                primaries: Some(*primaries),
                transfer: Some(*transfer),
                matrix: Some(*matrix),
                full_range: Some(*full_range),
            },
            Some(mp4::ColorInfo::Nclc { primaries, transfer, matrix }) => ContainerColour {
                primaries: Some(*primaries),
                transfer: Some(*transfer),
                matrix: Some(*matrix),
                full_range: None,
            },
            _ => ContainerColour::default(),
        };
        finalize_video_colour(&mut ti, cc);
    }
    ti
}

fn probe_mp4<R: Read + Seek>(r: R) -> Result<MediaInfo> {
    let src = Src::new(r)?;
    let f = mp4::open(&src).map_err(|e| anyhow!("isobmff: {e}"))?;
    let duration = (f.timescale > 0 && f.duration > 0).then(|| f.duration as f64 / f.timescale as f64);
    Ok(MediaInfo {
        container: if f.is_quicktime { "mov" } else { "mp4" },
        duration_secs: duration,
        title: None,
        tracks: f.tracks.iter().map(mp4_track_info).collect(),
    })
}

// ---------------------------------------------------------------------------------------------
// Time index (seconds <-> file byte), used by the sliding-window storage policy
// ---------------------------------------------------------------------------------------------

/// Video time -> absolute file byte map. `points` are (secs, byte), both strictly increasing.
/// Empty when the container carries no usable index (callers then fall back to the average
/// bit rate).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimeIndex {
    pub points: Vec<(f64, u64)>,
}

impl TimeIndex {
    /// Absolute byte for `secs`, linear between points; beyond the ends (and with no points) the
    /// average bit rate `avg_bps` (bytes/s) extrapolates. Clamped to `0..=file_len`.
    pub fn byte_at(&self, secs: f64, file_len: u64, avg_bps: f64) -> u64 {
        let secs = secs.max(0.0);
        let v = match (self.points.first(), self.points.last()) {
            (Some(&(t0, b0)), Some(&(t1, b1))) => {
                if secs <= t0 {
                    b0 as f64 - (t0 - secs) * avg_bps
                } else if secs >= t1 {
                    b1 as f64 + (secs - t1) * avg_bps
                } else {
                    let i = self.points.partition_point(|p| p.0 <= secs);
                    let (ta, ba) = self.points[i - 1];
                    let (tb, bb) = self.points[i];
                    ba as f64 + (secs - ta) / (tb - ta) * (bb - ba) as f64
                }
            }
            _ => secs * avg_bps,
        };
        (v.max(0.0) as u64).min(file_len)
    }

    /// Inverse of [`byte_at`](Self::byte_at).
    pub fn secs_at(&self, byte: u64, avg_bps: f64) -> f64 {
        let avg = avg_bps.max(1.0);
        let byte_f = byte as f64;
        let v = match (self.points.first(), self.points.last()) {
            (Some(&(t0, b0)), Some(&(t1, b1))) => {
                if byte <= b0 {
                    t0 - (b0 - byte) as f64 / avg
                } else if byte >= b1 {
                    t1 + (byte - b1) as f64 / avg
                } else {
                    let i = self.points.partition_point(|p| p.1 <= byte);
                    let (ta, ba) = self.points[i - 1];
                    let (tb, bb) = self.points[i];
                    ta + (byte_f - ba as f64) / (bb - ba) as f64 * (tb - ta)
                }
            }
            _ => byte_f / avg,
        };
        v.max(0.0)
    }

    /// Bytes before the first indexed media (container header: EBML/Tracks/Cues or `moov`).
    pub fn header_end(&self) -> Option<u64> {
        self.points.first().map(|p| p.1)
    }

    fn push(&mut self, secs: f64, byte: u64) {
        if let Some(&(t, b)) = self.points.last() {
            if secs <= t || byte <= b {
                return;
            }
        }
        self.points.push((secs, byte));
    }
}

/// Build the video time index. MKV: video `Cues` (`CueClusterPosition` -> absolute byte). MP4: video
/// sync samples. Empty when the container has no index; errors only for unreadable containers.
pub fn time_index<R: Read + Seek>(mut r: R) -> Result<TimeIndex> {
    match detect(&mut r)? {
        Kind::Mkv => time_index_mkv(r),
        Kind::Mp4 => time_index_mp4(r),
    }
}

fn time_index_mkv<R: Read + Seek>(r: R) -> Result<TimeIndex> {
    let src = Src::new(r)?;
    let opts = mkv::OpenOptions { index: false, verify_crc: false };
    let f = mkv::open_with(&src, &opts).map_err(|e| anyhow!("matroska: {e}"))?;
    let video: Vec<u64> = f.tracks.iter().filter(|t| t.kind == mkv::TrackKind::Video).map(|t| t.number).collect();
    let scale = f.info.timestamp_scale as f64 / 1e9;
    let mut ti = TimeIndex::default();
    for c in &f.cues {
        let pos = c
            .positions
            .iter()
            .find(|p| video.contains(&p.track))
            .or_else(|| if video.is_empty() { c.positions.first() } else { None });
        let Some(p) = pos else { continue };
        let Some(abs) = f.segment_data_start.checked_add(p.cluster_position) else { continue };
        ti.push(c.time as f64 * scale, abs);
    }
    Ok(ti)
}

fn time_index_mp4<R: Read + Seek>(r: R) -> Result<TimeIndex> {
    let src = Src::new(r)?;
    let f = mp4::open(&src).map_err(|e| anyhow!("isobmff: {e}"))?;
    let mut ti = TimeIndex::default();
    let Some(t) = f.tracks.iter().find(|t| t.kind == mp4::TrackKind::Video) else { return Ok(ti) };
    let scale = t.timescale.max(1) as f64;
    let mut pts: Vec<(i64, u64)> = t.samples.iter().filter(|s| s.is_sync).map(|s| (s.pts.saturating_add(t.edit_offset), s.offset)).collect();
    // Presentation order; both time and byte must grow.
    pts.sort_by_key(|p| p.0);
    for (p, off) in pts {
        ti.push(p.max(0) as f64 / scale, off);
    }
    Ok(ti)
}

// ---------------------------------------------------------------------------------------------
// Metadata locator: where the container's index lives (header, Cues, moov), tolerant of
// bytes that are not on disk yet
// ---------------------------------------------------------------------------------------------

/// Byte ranges that must be selected/kept for the demuxer to read the container index.
/// `complete == false`: more bytes are needed to learn the final layout; `ranges` then also holds
/// the small probe range the next call needs (select it, then call again).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetaLoc {
    pub ranges: Vec<std::ops::Range<u64>>,
    pub complete: bool,
}

/// Tail kept when an MKV has no SeekHead entry for its Cues.
pub const TAIL_FALLBACK_BYTES: u64 = 16 * 1024 * 1024;
const MKV_HEAD_PROBE: u64 = 256 * 1024;
const MKV_SEEKHEAD: u32 = 0x114D_9B74;
const MKV_CLUSTER: u32 = 0x1F43_B675;
const MKV_SEGMENT: u32 = 0x1853_8067;
const MKV_CUES: u32 = 0x1C53_BB6B;
const MKV_TAGS: u32 = 0x1254_C367;
const MKV_CHAPTERS: u32 = 0x1043_A770;

/// EBML element ID at the start of `b`: (id with marker bits, length).
fn ebml_id(b: &[u8]) -> Option<(u32, usize)> {
    let n = b.first()?.leading_zeros() as usize + 1;
    if n > 4 || b.len() < n {
        return None;
    }
    Some((b[..n].iter().fold(0u32, |a, x| (a << 8) | *x as u32), n))
}

/// EBML data size: (value or None when "unknown", length).
fn ebml_size(b: &[u8]) -> Option<(Option<u64>, usize)> {
    let n = b.first()?.leading_zeros() as usize + 1;
    if n > 8 || b.len() < n {
        return None;
    }
    let mut v = (b[0] as u64) & (0xFFu64 >> n);
    let mut all_ones = v == (0xFFu64 >> n);
    for x in &b[1..n] {
        v = (v << 8) | *x as u64;
        all_ones &= *x == 0xFF;
    }
    Some((if all_ones { None } else { Some(v) }, n))
}

/// (id, header_len, data_size) of the element at the start of `b`.
fn ebml_elem(b: &[u8]) -> Option<(u32, usize, Option<u64>)> {
    let (id, il) = ebml_id(b)?;
    let (sz, sl) = ebml_size(&b[il..])?;
    Some((id, il + sl, sz))
}

fn be_uint(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |a, x| (a << 8) | *x as u64)
}

/// Locate the metadata of the file behind `r` (`len` bytes). `r` may fail for ranges that are not
/// on disk; that only makes the result incomplete. Never errors.
pub fn locate_meta<R: Read + Seek>(mut r: R, len: u64) -> MetaLoc {
    let mut read_at = |off: u64, n: u64| -> Option<Vec<u8>> {
        let n = n.min(len.saturating_sub(off)) as usize;
        if n == 0 {
            return None;
        }
        let mut v = vec![0u8; n];
        r.seek(SeekFrom::Start(off)).ok()?;
        r.read_exact(&mut v).ok()?;
        Some(v)
    };
    let Some(magic) = read_at(0, 12) else {
        return MetaLoc { ranges: vec![0..4096.min(len)], complete: false };
    };
    if magic[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        locate_mkv(&mut read_at, len)
    } else {
        locate_mp4(&mut read_at, len)
    }
}

fn locate_mkv(read_at: &mut dyn FnMut(u64, u64) -> Option<Vec<u8>>, len: u64) -> MetaLoc {
    let probe = MKV_HEAD_PROBE.min(len);
    let Some(buf) = read_at(0, probe) else {
        return MetaLoc { ranges: vec![0..probe], complete: false };
    };
    let mut loc = MetaLoc::default();
    // EBML header, then the Segment.
    let Some((_, hl, Some(sz))) = ebml_elem(&buf) else { return MetaLoc { ranges: vec![0..probe], complete: true } };
    let mut pos = hl + sz as usize;
    let seg = match buf.get(pos..).and_then(ebml_elem) {
        Some((MKV_SEGMENT, hl2, _)) => pos + hl2,
        _ => return MetaLoc { ranges: vec![0..probe], complete: true },
    };
    pos = seg;
    let mut header_end = buf.len();
    let mut wanted: Vec<u64> = Vec::new(); // absolute starts of Cues/Tags/Chapters
    while let Some((id, hl, size)) = buf.get(pos..).and_then(ebml_elem) {
        if id == MKV_CLUSTER {
            header_end = pos;
            break;
        }
        let Some(size) = size else { break };
        if id == MKV_SEEKHEAD {
            let body = buf.get(pos + hl..(pos + hl + size as usize).min(buf.len())).unwrap_or(&[]);
            let mut p = 0;
            while let Some((cid, chl, Some(csz))) = body.get(p..).and_then(ebml_elem) {
                if cid == 0x4DBB {
                    let inner = body.get(p + chl..(p + chl + csz as usize).min(body.len())).unwrap_or(&[]);
                    let (mut sid, mut spos, mut q) = (0u32, None, 0);
                    while let Some((iid, ihl, Some(isz))) = inner.get(q..).and_then(ebml_elem) {
                        let d = inner.get(q + ihl..(q + ihl + isz as usize).min(inner.len())).unwrap_or(&[]);
                        match iid {
                            0x53AB => sid = be_uint(d) as u32,
                            0x53AC => spos = Some(be_uint(d)),
                            _ => {}
                        }
                        q += ihl + isz as usize;
                    }
                    if let (true, Some(sp)) = (matches!(sid, MKV_CUES | MKV_TAGS | MKV_CHAPTERS), spos) {
                        wanted.push((seg as u64).saturating_add(sp));
                    }
                }
                p += chl + csz as usize;
            }
        }
        pos += hl + size as usize;
    }
    let header_end = (header_end as u64).min(len);
    loc.ranges.push(0..header_end);
    let mut complete = true;
    let mut has_cues = false;
    for start in wanted {
        if start < header_end {
            has_cues = true; // inside the header already
            continue;
        }
        match read_at(start, 16).as_deref().and_then(ebml_elem) {
            Some((id, hl, Some(sz))) => {
                has_cues |= id == MKV_CUES;
                loc.ranges.push(start..(start + hl as u64 + sz).min(len));
            }
            Some(_) => loc.ranges.push(start..(start + TAIL_FALLBACK_BYTES).min(len)),
            None => {
                complete = false;
                loc.ranges.push(start..(start + 64 * 1024).min(len));
            }
        }
    }
    if !has_cues && complete {
        // No SeekHead entry for Cues: assume they trail the file.
        loc.ranges.push(len.saturating_sub(TAIL_FALLBACK_BYTES).max(header_end)..len);
    }
    loc.complete = complete;
    loc
}

fn locate_mp4(read_at: &mut dyn FnMut(u64, u64) -> Option<Vec<u8>>, len: u64) -> MetaLoc {
    let mut loc = MetaLoc::default();
    let mut off = 0u64;
    for _ in 0..256 {
        if off.saturating_add(8) > len {
            break;
        }
        let probe = off..off.saturating_add(32).min(len);
        let Some(h) = read_at(off, 16) else {
            loc.ranges.push(probe);
            return loc; // incomplete
        };
        let (mut size, mut _hl) = (u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64, 8u64);
        if size == 1 && h.len() >= 16 {
            size = u64::from_be_bytes(h[8..16].try_into().unwrap());
            _hl = 16;
        } else if size == 0 {
            size = len - off;
        }
        if &h[4..8] == b"moov" {
            loc.ranges.push(off..off.saturating_add(size).min(len));
            loc.complete = true;
            return loc;
        }
        loc.ranges.push(probe);
        if size < 8 {
            break;
        }
        off = off.saturating_add(size);
    }
    loc.complete = true;
    loc
}

// ---------------------------------------------------------------------------------------------
// PacketReader
// ---------------------------------------------------------------------------------------------

/// Sequential packet reader for ONE track.
///
/// MKV: positions through Cues. Most muxers cue only the video track, so the reader seeks with the
/// first track that has cue entries and then discards packets of the wanted track that end before
/// `start_secs` (non-video tracks only; video starts at the preceding keyframe). If the file has
/// no Cues at all it falls back to a forward scan from the first Cluster (reads everything before
/// `start_secs`). Packets of other tracks interleaved in the file are read (the crate always
/// materialises them) and dropped.
///
/// MP4: sample tables are in memory, so seeking is free and only the wanted samples are read.
pub struct PacketReader<R> {
    inner: Inner<R>,
}

enum Inner<R> {
    Mkv(MkvReader<R>),
    Mp4(Mp4Reader<R>),
}

/// Safety margin used when seeking an audio/other track through a video cue.
const SEEK_BACKOFF_NS: i64 = 500_000_000;

struct MkvReader<R> {
    d: mkv::Demuxer<mkv::ReadSeekSource<R>>,
    track_number: u64,
    /// Discard packets whose end is before this (ns); `i64::MIN` for video / start==0.
    drop_before_ns: i64,
    /// Video without Cues and start>0: forward scan up to this time (ns), keeping the last GOP.
    scan_video_to_ns: Option<i64>,
    buffered: std::collections::VecDeque<Packet>,
}

struct Mp4Reader<R> {
    src: Src<R>,
    file: mp4::Mp4File,
    track: usize,
    next: usize,
    scale: f64,
}

impl<R: Read + Seek> PacketReader<R> {
    pub fn open(mut r: R, track_id: u64, start_secs: f64) -> Result<Self> {
        let start_secs = if start_secs.is_finite() { start_secs.max(0.0) } else { 0.0 };
        match detect(&mut r)? {
            Kind::Mkv => Ok(Self { inner: Inner::Mkv(MkvReader::open(r, track_id, start_secs)?) }),
            Kind::Mp4 => Ok(Self { inner: Inner::Mp4(Mp4Reader::open(r, track_id, start_secs)?) }),
        }
    }

    /// Next packet of the selected track, or `None` at end of stream.
    pub fn next(&mut self) -> Result<Option<Packet>> {
        match &mut self.inner {
            Inner::Mkv(m) => m.next(),
            Inner::Mp4(m) => m.next(),
        }
    }
}

fn dur_secs(ns: impl TryInto<i64>) -> Option<f64> {
    ns.try_into().ok().filter(|n| *n > 0).map(|n| n as f64 / 1e9)
}

impl<R: Read + Seek> MkvReader<R> {
    fn open(r: R, track_number: u64, start_secs: f64) -> Result<Self> {
        let opts = mkv::OpenOptions { index: false, verify_crc: false };
        let mut d = mkv::Demuxer::with_options(mkv::ReadSeekSource::new(r)?, opts).map_err(|e| anyhow!("matroska: {e}"))?;
        let want = d
            .file()
            .track_by_number(track_number)
            .ok_or_else(|| anyhow!("no MKV track with number {track_number}"))?;
        let is_video = d.tracks()[want].kind == mkv::TrackKind::Video;
        let mut drop_before_ns = i64::MIN;
        let mut scan_video_to_ns = None;
        if start_secs > 0.0 {
            let ns = (start_secs * 1e9) as i64;
            // Pick a track that has Cue entries (prefer the wanted one, else any).
            let cue_track = {
                let f = d.file();
                let has = |n: u64| f.cues.iter().any(|c| c.positions.iter().any(|p| p.track == n));
                if has(track_number) {
                    Some(want)
                } else {
                    f.tracks.iter().position(|t| has(t.number))
                }
            };
            match cue_track {
                Some(ct) => {
                    // Seeking through another track's cue (usually video) positions the stream at
                    // that track's keyframe *block*; audio blocks of the same cluster that were
                    // muxed just before it would be skipped. Back off a little so the wanted
                    // audio frame is always after the landing point (costs <= 1 extra GOP).
                    let at = if ct == want { ns } else { (ns - SEEK_BACKOFF_NS).max(0) };
                    d.seek(ct, at).map_err(|e| anyhow!("matroska seek: {e}"))?;
                }
                None => { /* no Cues: forward scan from the first Cluster (already positioned) */ }
            }
            if !is_video {
                drop_before_ns = ns;
            } else if cue_track.is_none() {
                scan_video_to_ns = Some(ns);
            }
        }
        Ok(Self { d, track_number, drop_before_ns, scan_video_to_ns, buffered: Default::default() })
    }

    fn next(&mut self) -> Result<Option<Packet>> {
        if let Some(limit) = self.scan_video_to_ns {
            // Forward scan: buffer from the last keyframe with pts <= limit, stop at the first
            // packet past the limit (bounded memory: one GOP).
            self.scan_video_to_ns = None;
            while let Some(p) = self.d.next_packet().map_err(|e| anyhow!("matroska: {e}"))? {
                if p.track_number != self.track_number {
                    continue;
                }
                let pk = Packet { pts_secs: p.pts_ns as f64 / 1e9, data: p.data, keyframe: p.keyframe, duration_secs: dur_secs(p.duration_ns) };
                if p.pts_ns > limit && !self.buffered.is_empty() && (p.keyframe || p.pts_ns > limit) {
                    self.buffered.push_back(pk);
                    break;
                }
                if p.keyframe {
                    self.buffered.clear();
                }
                self.buffered.push_back(pk);
            }
        }
        if let Some(p) = self.buffered.pop_front() {
            return Ok(Some(p));
        }
        loop {
            let Some(p) = self.d.next_packet().map_err(|e| anyhow!("matroska: {e}"))? else {
                return Ok(None);
            };
            if p.track_number != self.track_number {
                continue;
            }
            if self.drop_before_ns != i64::MIN && p.pts_ns.saturating_add(p.duration_ns as i64) <= self.drop_before_ns {
                continue;
            }
            return Ok(Some(Packet { pts_secs: p.pts_ns as f64 / 1e9, data: p.data, keyframe: p.keyframe, duration_secs: dur_secs(p.duration_ns) }));
        }
    }
}

impl<R: Read + Seek> Mp4Reader<R> {
    fn open(r: R, track_id: u64, start_secs: f64) -> Result<Self> {
        let src = Src::new(r)?;
        let file = mp4::open(&src).map_err(|e| anyhow!("isobmff: {e}"))?;
        let track = file
            .tracks
            .iter()
            .position(|t| t.id as u64 == track_id)
            .ok_or_else(|| anyhow!("no MP4 track with id {track_id}"))?;
        let t = &file.tracks[track];
        let scale = t.timescale.max(1) as f64;
        let mut next = 0usize;
        if start_secs > 0.0 {
            let ticks = (start_secs * scale) as i64;
            if let Some(i) = t.sample_at_presentation_time(ticks) {
                next = if t.kind == mp4::TrackKind::Video { t.sync_sample_before(i) } else { i };
            }
        }
        Ok(Self { src, file, track, next, scale })
    }

    fn next(&mut self) -> Result<Option<Packet>> {
        let t = &self.file.tracks[self.track];
        let Some(s) = t.samples.get(self.next) else { return Ok(None) };
        let pts = s.pts.saturating_add(t.edit_offset);
        let key = s.is_sync;
        let dur = (s.duration > 0).then(|| s.duration as f64 / self.scale);
        let data = self.file.read_sample(&self.src, self.track, self.next).map_err(|e| anyhow!("isobmff: {e}")).context("reading sample")?;
        self.next += 1;
        Ok(Some(Packet { pts_secs: pts as f64 / self.scale, data, keyframe: key, duration_secs: dur }))
    }
}

#[cfg(test)]
mod tests {
    use crate::hls::sps::VuiColour;

    #[test]
    fn colour_resolution_prefers_container() {
        use super::*;
        let vui = |t| Some(VuiColour { primaries: 9, transfer: t, matrix: 9, full_range: false });
        // kapsayıcı yok -> VUI
        let r = resolve_colour(ContainerColour::default(), vui(16)).unwrap();
        assert_eq!((r.primaries, r.transfer, r.matrix, r.full_range), (9, 16, 9, false));
        assert_eq!(resolve_colour(ContainerColour::default(), None), None);
        // belirtilmemiş (2) VUI -> bilinmiyor
        let u = Some(VuiColour { primaries: 2, transfer: 2, matrix: 2, full_range: false });
        assert_eq!(resolve_colour(ContainerColour::default(), u), None);
        // kapsayıcı transfer 1 -> VUI (PQ) ezmez
        let c = ContainerColour { primaries: Some(1), transfer: Some(1), matrix: Some(1), full_range: None };
        let r = resolve_colour(c, vui(16)).unwrap();
        assert_eq!((r.primaries, r.transfer, r.matrix), (1, 1, 1));
        // yalnız transfer var: eksikler, transfer VUI ile aynıysa VUI'den
        let c = ContainerColour { transfer: Some(16), ..Default::default() };
        assert_eq!(resolve_colour(c, vui(16)).map(|x| (x.primaries, x.matrix)), Some((9, 9)));
        assert_eq!(resolve_colour(c, vui(18)), None);
        assert_eq!(resolve_colour(c, None), None);
        // kapsayıcı yalnız primaries (BT.2020) + VUI PQ: birleşir
        let c = ContainerColour { primaries: Some(9), ..Default::default() };
        let r = resolve_colour(c, vui(16)).unwrap();
        assert_eq!((r.primaries, r.transfer, r.matrix), (9, 16, 9));
        let c = ContainerColour { matrix: Some(9), full_range: Some(true), ..Default::default() };
        assert_eq!(resolve_colour(c, vui(18)).map(|x| (x.transfer, x.full_range)), Some((18, true)));
        // çelişki (kapsayıcı BT.709 primaries, VUI BT.2020+PQ): conservatif None
        let c = ContainerColour { primaries: Some(1), ..Default::default() };
        assert_eq!(resolve_colour(c, vui(16)), None);
        let c = ContainerColour { matrix: Some(1), ..Default::default() };
        assert_eq!(resolve_colour(c, vui(16)), None);
        // VUI transfer belirtilmemiş: colr/hdr yok
        let c = ContainerColour { primaries: Some(9), ..Default::default() };
        assert_eq!(resolve_colour(c, vui(2)), None);
    }

    use super::*;

    #[test]
    fn huge_box_sizes_do_not_overflow() {
        // Görsel örnek girdisi: 78 bayt sabit alan + 64 bit boyutlu (u64::MAX) kutu.
        let mut raw = vec![0u8; 78];
        raw.extend_from_slice(&1u32.to_be_bytes());
        raw.extend_from_slice(b"dvcC");
        raw.extend_from_slice(&u64::MAX.to_be_bytes());
        assert!(mp4_dovi_from_entry(&raw).is_none());
        // locate_mp4: ilk kutu u64::MAX boyutlu (ftyp): taşmadan biter.
        let mut hdr = Vec::new();
        hdr.extend_from_slice(&1u32.to_be_bytes());
        hdr.extend_from_slice(b"ftyp");
        hdr.extend_from_slice(&u64::MAX.to_be_bytes());
        let mut rd = |o: u64, n: u64| -> Option<Vec<u8>> {
            let o = o as usize;
            (o < hdr.len()).then(|| hdr[o..(o + n as usize).min(hdr.len())].to_vec())
        };
        let loc = locate_mp4(&mut rd, 1 << 20);
        assert!(loc.complete);
    }

    #[test]
    fn dovi_config_parse() {
        // profile 8, level 6, bl compat 1: byte2 = (8<<1)|0, byte3 = level 6 << 3 | bl, byte4 = 1<<4
        let c = parse_dovi_config(&[1, 0, 16, 0x31, 0x10]).unwrap();
        assert_eq!((c.profile, c.level, c.bl_compat), (8, 6, 1));
        assert_eq!(parse_dovi_config(&[1, 0]), None);
    }

    #[test]
    fn dovi_from_synthetic_tracks() {
        // TrackEntry{ TrackNumber=1, BlockAdditionMapping{ Type='dvcC', ExtraData } }
        let extra = [1u8, 0, 16, 0x31, 0x10, 0, 0, 0];
        let mut map = Vec::new();
        map.extend_from_slice(&[0x41, 0xE7, 0x84, 0x64, 0x76, 0x63, 0x43]);
        map.extend_from_slice(&[0x41, 0xED, 0x88]);
        map.extend_from_slice(&extra);
        let mut te = vec![0xD7, 0x81, 0x01, 0x41, 0xE4, 0x80 | map.len() as u8];
        te.extend_from_slice(&map);
        let mut tr = vec![0xAE, 0x80 | te.len() as u8];
        tr.extend_from_slice(&te);
        let d = dv_from_tracks(&tr).remove(&1).unwrap();
        assert_eq!((d.cfg.profile, d.cfg.level, d.cfg.bl_compat), (8, 6, 1));
        assert_eq!(d.raw, extra);
        assert_eq!(&d.fourcc, b"dvcC");
    }

    #[test]
    fn dovi_mp4_entry_box() {
        let rec = [1u8, 0, 0x14, 0x35, 0x00]; // profile 10 (dav1), level 6, rpu+bl
        let mut raw = vec![0u8; 78];
        // hvcC-benzeri başka bir alt kutu, sonra dvvC
        raw.extend_from_slice(&[0, 0, 0, 12, b'p', b'a', b's', b'p', 0, 0, 0, 1]);
        raw.extend_from_slice(&(8 + rec.len() as u32).to_be_bytes());
        raw.extend_from_slice(b"dvvC");
        raw.extend_from_slice(&rec);
        let d = mp4_dovi_from_entry(&raw).unwrap();
        assert_eq!((d.cfg.profile, d.cfg.level), (10, 6));
        assert_eq!(&d.fourcc, b"dvvC");
        assert_eq!(d.raw, rec);
        assert!(mp4_dovi_from_entry(&raw[..90]).is_none());
        assert!(mp4_dovi_from_entry(&[0u8; 78]).is_none());
    }

    fn build_laced(mode: mkv::LacingMode, varying: bool) -> Vec<u8> {
        let mut v = mkv::TrackSpec::new(mkv::TrackKind::Video, "V_MPEG4/ISO/AVC");
        v.video_size = Some((64, 64));
        v.default_duration_ns = Some(40_000_000);
        let mut a = mkv::TrackSpec::new(mkv::TrackKind::Audio, "A_AC3");
        a.audio = Some((48000.0, 2, None));
        a.default_duration_ns = Some(32_000_000);
        let opts = mkv::MuxOptions { lacing: mode, cluster_duration_ns: 1_000_000_000, ..Default::default() };
        let mut w = mkv::MkvWriter::new(std::io::Cursor::new(Vec::new()), vec![v, a], opts).unwrap();
        let (mut vi, mut ai) = (0u64, 0u64);
        // 20 s, interleaved by time; video keyframe every 25 frames (1 s)
        while vi < 500 || ai < 625 {
            if vi < 500 && (vi * 40 <= ai * 32 || ai >= 625) {
                w.write_frame(0, (vi * 40_000_000) as i64, vi % 25 == 0, &[vi as u8; 100], None).unwrap();
                vi += 1;
            } else {
                let len = if varying { 10 + (ai % 7) as usize * 3 } else { 24 };
                let mut d = vec![ai as u8; len];
                d[0] = (ai >> 8) as u8;
                d[1] = ai as u8;
                w.write_frame(1, (ai * 32_000_000) as i64, true, &d, None).unwrap();
                ai += 1;
            }
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn lacing_modes_yield_individual_frames_and_seek() {
        for (mode, varying) in [(mkv::LacingMode::Xiph, true), (mkv::LacingMode::Ebml, true), (mkv::LacingMode::FixedOrEbml, false), (mkv::LacingMode::None, false)] {
            let bytes = build_laced(mode, varying);
            let info = probe(std::io::Cursor::new(bytes.clone())).unwrap();
            assert_eq!(info.tracks.len(), 2);
            if mode != mkv::LacingMode::None {
                // make sure the stream really contains laced blocks
                let mut d = mkv::Demuxer::from_slice(&bytes).unwrap();
                let mut laced = 0;
                while let Some(p) = d.next_packet().unwrap() {
                    laced += (p.lace > 0) as u32;
                }
                assert!(laced > 100, "mode {mode:?} produced only {laced} laced frames");
            }
            assert_eq!(info.tracks[1].codec, "ac3");
            let mut r = PacketReader::open(std::io::Cursor::new(bytes.clone()), 2, 0.0).unwrap();
            let mut n = 0u64;
            while let Some(p) = r.next().unwrap() {
                let idx = ((p.data[0] as u64) << 8) | p.data[1] as u64;
                assert_eq!(idx, n, "frame order/content");
                assert!((p.pts_secs - n as f64 * 0.032).abs() < 1e-6, "pts {} vs {}", p.pts_secs, n as f64 * 0.032);
                n += 1;
            }
            assert_eq!(n, 625);
            // seek to 10.0 s: first packet is the frame containing 10.0 s (index 312, 9.984..10.016)
            let mut r = PacketReader::open(std::io::Cursor::new(bytes.clone()), 2, 10.0).unwrap();
            let p = r.next().unwrap().unwrap();
            let idx = ((p.data[0] as u64) << 8) | p.data[1] as u64;
            assert_eq!(idx, 312, "mode {mode:?}");
            let mut cnt = 1;
            while r.next().unwrap().is_some() {
                cnt += 1;
            }
            assert_eq!(cnt, 625 - 312);
            // video seek to 10.5 s lands on the keyframe at 10.0 s
            let mut r = PacketReader::open(std::io::Cursor::new(bytes), 1, 10.5).unwrap();
            let p = r.next().unwrap().unwrap();
            assert!(p.keyframe && (p.pts_secs - 10.0).abs() < 1e-6, "video start {}", p.pts_secs);
        }
    }

    #[test]
    fn time_index_from_cues() {
        let bytes = build_laced(mkv::LacingMode::None, false);
        let len = bytes.len() as u64;
        let ti = time_index(std::io::Cursor::new(bytes)).unwrap();
        assert!(ti.points.len() >= 19, "points: {}", ti.points.len());
        assert!(ti.points.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1));
        assert!(ti.points.iter().all(|p| p.1 < len));
        let b = ti.byte_at(10.0, len, 1.0);
        assert!((ti.secs_at(b, 1.0) - 10.0).abs() < 0.01);
        assert!(ti.header_end().unwrap() > 0);
    }

    /// Dosya: ftyp(24) + mdat(1000) + moov(100). `avail`: yalnızca bu kadar bayt okunabilir.
    fn moov_at_end(avail: usize) -> (std::io::Cursor<Vec<u8>>, u64) {
        let mut v = Vec::new();
        v.extend_from_slice(&24u32.to_be_bytes());
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(&[0u8; 16]);
        v.extend_from_slice(&1000u32.to_be_bytes());
        v.extend_from_slice(b"mdat");
        v.extend_from_slice(&[0u8; 992]);
        v.extend_from_slice(&100u32.to_be_bytes());
        v.extend_from_slice(b"moov");
        v.extend_from_slice(&[0u8; 92]);
        let len = v.len() as u64;
        v.truncate(avail);
        (std::io::Cursor::new(v), len)
    }

    #[test]
    fn mp4_moov_at_end_box_walk() {
        // Üst kutu başlıkları var ama mdat'ın ardındaki başlık henüz inmemiş: probe istenir.
        let (r, len) = moov_at_end(100);
        let loc = locate_meta(r, len);
        assert!(!loc.complete);
        assert_eq!(loc.ranges.last().unwrap().start, 1024, "{loc:?}");
        // Başlık okunabilince moov (dosya sonunda) aralığı döner.
        let (r, len) = moov_at_end(usize::MAX);
        let loc = locate_meta(r, len);
        assert!(loc.complete);
        assert_eq!(loc.ranges.last().unwrap(), &(1024..1124));
    }

    #[test]
    fn mkv_cues_at_end_are_located() {
        // Başlık sondaki Cues'tan 256 KiB'lik probe penceresinden uzak olsun diye iri kareler.
        let mut v = mkv::TrackSpec::new(mkv::TrackKind::Video, "V_MPEG4/ISO/AVC");
        v.video_size = Some((64, 64));
        v.default_duration_ns = Some(40_000_000);
        let opts = mkv::MuxOptions { cluster_duration_ns: 1_000_000_000, ..Default::default() };
        let mut w = mkv::MkvWriter::new(std::io::Cursor::new(Vec::new()), vec![v], opts).unwrap();
        for i in 0..500u64 {
            w.write_frame(0, (i * 40_000_000) as i64, i % 25 == 0, &vec![i as u8; 2000], None).unwrap();
        }
        let bytes = w.finish().unwrap().into_inner();
        let len = bytes.len() as u64;
        let full = locate_meta(std::io::Cursor::new(bytes.clone()), len);
        assert!(full.complete);
        let (head, cues) = (&full.ranges[0], &full.ranges[1]);
        assert!(head.end < cues.start && cues.end <= len, "{full:?}");
        // Yalnızca başlık okunabilirse Cues başlığı için probe aralığı istenir.
        struct Limited(std::io::Cursor<Vec<u8>>, u64);
        impl Read for Limited {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                if self.0.position() + b.len() as u64 > self.1 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                self.0.read(b)
            }
        }
        impl Seek for Limited {
            fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
                self.0.seek(p)
            }
        }
        let loc = locate_meta(Limited(std::io::Cursor::new(bytes), cues.start), len);
        assert!(!loc.complete);
        assert_eq!(loc.ranges.last().unwrap().start, cues.start);
    }

    #[test]
    fn garbage_is_error_not_panic() {
        assert!(probe(std::io::Cursor::new(vec![0u8; 100])).is_err());
        assert!(probe(std::io::Cursor::new(Vec::<u8>::new())).is_err());
        let mut v = vec![0x1A, 0x45, 0xDF, 0xA3, 0xFF, 0xFF, 0xFF];
        v.extend((0..500u32).map(|i| (i * 7) as u8));
        let _ = probe(std::io::Cursor::new(v));
        let mut v = b"\0\0\0\x18ftypisom".to_vec();
        v.extend((0..500u32).map(|i| (i * 13) as u8));
        let _ = probe(std::io::Cursor::new(v));
    }
}
