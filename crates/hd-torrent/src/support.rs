//! iPhone destek raporu: dosyadaki her iz için "içinde ne var" ve "iPhone'da nasıl çalar", ayrıca
//! sonraki görevlerin (HLS paketleyici) kullanacağı paketleme planı.

use std::collections::HashMap;

use serde::Serialize;

use crate::atmos::AtmosState;
use crate::demux::{MediaInfo, TrackInfo, TrackKind};

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Full,
    Degraded,
    Unsupported,
}

/// HLS paketleme planı (Görev 10/11 kullanır).
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum HlsPlan {
    Copy,
    DecodeToFlac,
    StripDvEnhancement,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DeviceCaps {
    pub av1_hw: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct TrackSupport {
    pub track_id: u64,
    pub kind: &'static str,
    pub label: String,
    pub in_file: String,
    pub on_iphone: String,
    pub level: Level,
    pub atmos: Option<AtmosState>,
    pub hls: Option<HlsPlan>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Summary {
    pub dolby_vision: Option<String>,
    pub hdr: Option<String>,
    pub atmos: AtmosState,
    pub playable: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct SupportReport {
    pub tracks: Vec<TrackSupport>,
    pub summary: Summary,
}

fn codec_name(codec: &str) -> String {
    match codec {
        "eac3" => "E-AC-3".into(),
        "ac3" => "AC-3".into(),
        "truehd" => "TrueHD".into(),
        "dts" => "DTS".into(),
        "aac" => "AAC".into(),
        "flac" => "FLAC".into(),
        "opus" => "Opus".into(),
        "mp3" => "MP3".into(),
        "vorbis" => "Vorbis".into(),
        "pcm" => "PCM".into(),
        "hevc" => "HEVC".into(),
        "h264" => "H.264".into(),
        "av1" => "AV1".into(),
        "vp9" => "VP9".into(),
        "vp8" => "VP8".into(),
        "vc1" => "VC-1".into(),
        "mpeg2" => "MPEG-2".into(),
        "mpeg4" => "MPEG-4 Part 2".into(),
        "srt" => "SRT".into(),
        "ass" => "ASS".into(),
        "webvtt" => "WebVTT".into(),
        "pgs" => "PGS".into(),
        "vobsub" => "VobSub".into(),
        other => other.to_ascii_uppercase(),
    }
}

/// 2 -> "2.0", 6 -> "5.1", 8 -> "7.1", diğer -> "N kanal"; bilinmiyorsa boş.
fn channels_str(ch: Option<u32>) -> String {
    match ch {
        None | Some(0) => String::new(),
        Some(1) => "1.0".into(),
        Some(2) => "2.0".into(),
        Some(6) => "5.1".into(),
        Some(8) => "7.1".into(),
        Some(n) => format!("{n} kanal"),
    }
}

fn label(t: &TrackInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(l) = t.language.as_deref().map(str::trim) {
        if (2..=3).contains(&l.len()) && l.chars().all(|c| c.is_ascii_alphabetic()) && !l.eq_ignore_ascii_case("und") {
            parts.push(l.to_ascii_uppercase());
        }
    }
    if let Some(n) = t.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        parts.push(n.to_string());
    }
    let mut codec = codec_name(&t.codec);
    if t.kind == TrackKind::Audio {
        let ch = channels_str(t.channels);
        if !ch.is_empty() {
            codec = format!("{codec} {ch}");
        }
    }
    parts.push(codec);
    parts.join(" · ")
}

fn dv_string(t: &TrackInfo) -> Option<String> {
    let p = t.dv_profile?;
    Some(match t.dv_bl_compat {
        Some(c) => format!("Profile {p}.{c}"),
        None => format!("Profile {p}"),
    })
}

fn in_file(t: &TrackInfo) -> String {
    let mut s = codec_name(&t.codec);
    match t.kind {
        TrackKind::Video => {
            if let (Some(w), Some(h)) = (t.width, t.height) {
                s.push_str(&format!(" {w}×{h}"));
            }
            if let Some(d) = dv_string(t) {
                s.push_str(&format!(" Dolby Vision {d}"));
            }
            if let Some(h) = &t.hdr {
                s.push_str(&format!(" {h}"));
            }
        }
        TrackKind::Audio => {
            let ch = channels_str(t.channels);
            if !ch.is_empty() {
                s.push_str(&format!(" {ch}"));
            }
            if let Some(sr) = t.sample_rate {
                s.push_str(&format!(" {} kHz", sr as f64 / 1000.0));
            }
            if let Some(b) = t.bit_depth {
                s.push_str(&format!(" {b}-bit"));
            }
        }
        _ => {}
    }
    s
}

/// "{head} {ch}{tail}"; kanal bilinmiyorsa fazladan boşluk bırakmaz.
fn with_ch(head: &str, ch: &str, tail: &str) -> String {
    if ch.is_empty() { format!("{head}{tail}") } else { format!("{head} {ch}{tail}") }
}

type Verdict = (Level, String, Option<HlsPlan>);

fn video(t: &TrackInfo, _caps: DeviceCaps) -> Verdict {
    match t.codec.as_str() {
        "h264" | "hevc" => match t.dv_profile {
            Some(7) => (
                Level::Degraded,
                "HDR10 taban katmanı (DV P7 iPhone'da çalmaz)".into(),
                Some(HlsPlan::StripDvEnhancement),
            ),
            Some(5) | Some(8) => (Level::Full, "Tam Dolby Vision".into(), Some(HlsPlan::Copy)),
            _ => {
                let txt = match t.hdr.as_deref() {
                    Some("HDR10") => "Tam HDR10",
                    Some("HLG") => "Tam HLG",
                    _ => "Tam",
                };
                (Level::Full, txt.into(), Some(HlsPlan::Copy))
            }
        },
        // AV1: HLS `av01` codec dizesi henüz üretilmiyor (Ruling R27); `av1_hw` ne olursa olsun desteklenmez.
        "av1" => (Level::Unsupported, "AV1 henüz desteklenmiyor".into(), None),
        _ => (Level::Unsupported, "iPhone'da oynatılamaz (görüntü dönüştürülmez)".into(), None),
    }
}

fn audio(t: &TrackInfo, atmos: AtmosState) -> Verdict {
    let ch = channels_str(t.channels);
    match t.codec.as_str() {
        "eac3" => {
            let txt = match atmos {
                AtmosState::Yes => "Gerçek Atmos · AirPods'ta uzamsal".to_string(),
                AtmosState::No => with_ch("Dolby Digital Plus", &ch, ""),
                AtmosState::Unknown => "Atmos belirleniyor…".to_string(),
            };
            (Level::Full, txt, Some(HlsPlan::Copy))
        }
        // MP3 HLS ses başlatma segmenti yok (segment::audio_init'te mp3 kolu yok) → desteklenmez.
        "mp3" => (Level::Unsupported, "iPhone'da desteklenmiyor (MP3 HLS)".into(), None),
        "ac3" | "aac" | "flac" | "opus" => (Level::Full, "Tam".into(), Some(HlsPlan::Copy)),
        "truehd" if atmos == AtmosState::Yes => (
            Level::Degraded,
            with_ch("Kayıpsız", &ch, ", Atmos nesneleri çalmaz"),
            Some(HlsPlan::DecodeToFlac),
        ),
        "truehd" => (Level::Full, with_ch("Kayıpsız", &ch, ""), Some(HlsPlan::DecodeToFlac)),
        "dts" => (
            Level::Full,
            with_ch("Kayıpsız/çok kanallı", &ch, " (DTS:X nesneleri çalmaz)"),
            Some(HlsPlan::DecodeToFlac),
        ),
        _ => (Level::Unsupported, "iPhone'da oynatılamaz".into(), None),
    }
}

fn subtitle(t: &TrackInfo) -> Verdict {
    match t.codec.as_str() {
        // `vtt_segment` yalnızca srt/ass işler; diğer metin biçimleri 500 verirdi.
        "srt" | "ass" => (Level::Full, "Tam".into(), None),
        "ssa" | "webvtt" | "mov_text" => (Level::Unsupported, "Bu altyazı biçimi henüz desteklenmiyor".into(), None),
        "pgs" | "vobsub" | "dvbsub" => (Level::Unsupported, "Resim altyazı desteklenmiyor".into(), None),
        _ => (Level::Unsupported, "iPhone'da oynatılamaz".into(), None),
    }
}

pub fn assess(info: &MediaInfo, atmos: &HashMap<u64, AtmosState>, caps: DeviceCaps) -> Vec<TrackSupport> {
    info.tracks
        .iter()
        .filter_map(|t| {
            let (kind, atmos_state, (level, on_iphone, hls)) = match t.kind {
                TrackKind::Video => ("video", None, video(t, caps)),
                TrackKind::Audio => {
                    let st = atmos.get(&t.id).copied().unwrap_or(AtmosState::Unknown);
                    let reports = matches!(t.codec.as_str(), "eac3" | "truehd");
                    ("audio", reports.then_some(st), audio(t, st))
                }
                TrackKind::Subtitle => ("subtitle", None, subtitle(t)),
                TrackKind::Other => return None,
            };
            Some(TrackSupport {
                track_id: t.id,
                kind,
                label: label(t),
                in_file: in_file(t),
                on_iphone,
                level,
                atmos: atmos_state,
                hls,
            })
        })
        .collect()
}

fn summarize(info: &MediaInfo, tracks: &[TrackSupport]) -> Summary {
    let video_t = info.tracks.iter().find(|t| t.kind == TrackKind::Video);
    let ok = |kind: &str| tracks.iter().any(|t| t.kind == kind && t.level != Level::Unsupported);
    let has_audio = tracks.iter().any(|t| t.kind == "audio");
    let states: Vec<AtmosState> = tracks.iter().filter_map(|t| t.atmos).collect();
    let atmos = if states.contains(&AtmosState::Yes) {
        AtmosState::Yes
    } else if states.contains(&AtmosState::Unknown) {
        AtmosState::Unknown
    } else {
        AtmosState::No
    };
    Summary {
        dolby_vision: video_t.filter(|t| matches!(t.codec.as_str(), "h264" | "hevc") && matches!(t.dv_profile, Some(5) | Some(8))).and_then(dv_string),
        hdr: video_t.and_then(|t| t.hdr.clone()),
        atmos,
        playable: ok("video") && (!has_audio || ok("audio")),
    }
}

pub fn report(info: &MediaInfo, atmos: &HashMap<u64, AtmosState>, caps: DeviceCaps) -> SupportReport {
    let tracks = assess(info, atmos, caps);
    let summary = summarize(info, &tracks);
    SupportReport { tracks, summary }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr(id: u64, kind: TrackKind, codec: &str) -> TrackInfo {
        TrackInfo {
            id,
            kind,
            codec: codec.into(),
            codec_id: String::new(),
            codec_private: vec![],
            language: None,
            name: None,
            default: false,
            channels: None,
            sample_rate: None,
            bit_depth: None,
            width: None,
            height: None,
            hdr: None,
            colour: None,
            dv_profile: None,
            dv_bl_compat: None,
            dv_level: None,
            dv_config: None,
            dv_config_fourcc: None,
        default_duration_ns: None,
        }
    }
    fn vid(codec: &str, hdr: Option<&str>, dv: Option<u8>) -> TrackInfo {
        let mut t = tr(1, TrackKind::Video, codec);
        t.hdr = hdr.map(Into::into);
        t.dv_profile = dv;
        t
    }
    fn aud(codec: &str, ch: u32) -> TrackInfo {
        let mut t = tr(2, TrackKind::Audio, codec);
        t.channels = Some(ch);
        t
    }
    fn one(t: TrackInfo, a: AtmosState, caps: DeviceCaps) -> TrackSupport {
        let id = t.id;
        let info = MediaInfo { container: "matroska", duration_secs: None, title: None, tracks: vec![t] };
        let mut m = HashMap::new();
        m.insert(id, a);
        assess(&info, &m, caps).remove(0)
    }
    fn chk(t: TrackInfo, a: AtmosState, caps: DeviceCaps, lv: Level, txt: &str, hls: Option<HlsPlan>) {
        let r = one(t.clone(), a, caps);
        assert_eq!((r.level, r.on_iphone.as_str(), r.hls), (lv, txt, hls), "{} {:?}", t.codec, a);
    }
    const NO: DeviceCaps = DeviceCaps { av1_hw: false };

    #[test]
    fn video_rules() {
        use HlsPlan::*;
        use Level::*;
        let u = AtmosState::Unknown;
        chk(vid("h264", None, None), u, NO, Full, "Tam", Some(Copy));
        chk(vid("hevc", None, None), u, NO, Full, "Tam", Some(Copy));
        chk(vid("hevc", Some("HDR10"), None), u, NO, Full, "Tam HDR10", Some(Copy));
        chk(vid("hevc", Some("HLG"), None), u, NO, Full, "Tam HLG", Some(Copy));
        chk(vid("hevc", Some("HDR10"), Some(5)), u, NO, Full, "Tam Dolby Vision", Some(Copy));
        chk(vid("hevc", Some("HDR10"), Some(8)), u, NO, Full, "Tam Dolby Vision", Some(Copy));
        chk(vid("hevc", Some("HDR10"), Some(7)), u, NO, Degraded, "HDR10 taban katmanı (DV P7 iPhone'da çalmaz)", Some(StripDvEnhancement));
        chk(vid("av1", None, None), u, DeviceCaps { av1_hw: true }, Unsupported, "AV1 henüz desteklenmiyor", None);
        chk(vid("av1", None, None), u, NO, Unsupported, "AV1 henüz desteklenmiyor", None);
        for c in ["vp9", "vc1", "mpeg2", "mpeg4"] {
            chk(vid(c, None, None), u, NO, Unsupported, "iPhone'da oynatılamaz (görüntü dönüştürülmez)", None);
        }
    }

    #[test]
    fn audio_rules() {
        use AtmosState::*;
        use HlsPlan::*;
        use Level::*;
        chk(aud("eac3", 6), Yes, NO, Full, "Gerçek Atmos · AirPods'ta uzamsal", Some(Copy));
        chk(aud("eac3", 6), No, NO, Full, "Dolby Digital Plus 5.1", Some(Copy));
        chk(aud("eac3", 6), Unknown, NO, Full, "Atmos belirleniyor…", Some(Copy));
        for c in ["ac3", "aac", "flac", "opus"] {
            chk(aud(c, 2), No, NO, Full, "Tam", Some(Copy));
        }
        chk(aud("mp3", 2), No, NO, Unsupported, "iPhone'da desteklenmiyor (MP3 HLS)", None);
        chk(aud("truehd", 8), Yes, NO, Degraded, "Kayıpsız 7.1, Atmos nesneleri çalmaz", Some(DecodeToFlac));
        chk(aud("truehd", 6), No, NO, Full, "Kayıpsız 5.1", Some(DecodeToFlac));
        chk(aud("dts", 6), No, NO, Full, "Kayıpsız/çok kanallı 5.1 (DTS:X nesneleri çalmaz)", Some(DecodeToFlac));
        chk(aud("dts", 3), No, NO, Full, "Kayıpsız/çok kanallı 3 kanal (DTS:X nesneleri çalmaz)", Some(DecodeToFlac));
        chk(aud("vorbis", 2), No, NO, Unsupported, "iPhone'da oynatılamaz", None);
    }

    #[test]
    fn subtitle_rules() {
        use Level::*;
        let u = AtmosState::Unknown;
        for c in ["srt", "ass"] {
            chk(tr(3, TrackKind::Subtitle, c), u, NO, Full, "Tam", None);
        }
        for c in ["ssa", "webvtt", "mov_text"] {
            chk(tr(3, TrackKind::Subtitle, c), u, NO, Unsupported, "Bu altyazı biçimi henüz desteklenmiyor", None);
        }
        for c in ["pgs", "vobsub"] {
            chk(tr(3, TrackKind::Subtitle, c), u, NO, Unsupported, "Resim altyazı desteklenmiyor", None);
        }
    }

    #[test]
    fn labels() {
        let mut t = aud("eac3", 6);
        t.language = Some("en".into());
        assert_eq!(one(t.clone(), AtmosState::No, NO).label, "EN · E-AC-3 5.1");
        t.language = Some("uk".into());
        t.name = Some("Dub, LeDoyen".into());
        assert_eq!(one(t.clone(), AtmosState::No, NO).label, "UK · Dub, LeDoyen · E-AC-3 5.1");
        t.language = Some("und".into());
        t.name = None;
        assert_eq!(one(t, AtmosState::No, NO).label, "E-AC-3 5.1");
    }

    fn info(tracks: Vec<TrackInfo>) -> MediaInfo {
        MediaInfo { container: "matroska", duration_secs: None, title: None, tracks }
    }

    #[test]
    fn summary_atmos_yes_if_any_track_yes() {
        let mut a2 = aud("truehd", 8);
        a2.id = 3;
        let i = info(vec![vid("hevc", None, None), aud("eac3", 6), a2]);
        let m = HashMap::from([(2, AtmosState::No), (3, AtmosState::Yes)]);
        let r = report(&i, &m, NO);
        assert_eq!(r.summary.atmos, AtmosState::Yes);
        assert!(r.summary.playable);
    }

    #[test]
    fn unknown_never_reported_as_yes() {
        let i = info(vec![vid("hevc", None, None), aud("eac3", 6)]);
        let r = report(&i, &HashMap::new(), NO);
        assert_eq!(r.summary.atmos, AtmosState::Unknown);
        assert_eq!(r.tracks[1].atmos, Some(AtmosState::Unknown));
        let m = HashMap::from([(2, AtmosState::No)]);
        assert_eq!(report(&i, &m, NO).summary.atmos, AtmosState::No);
    }

    #[test]
    fn other_dv_profiles_never_claim_dv() {
        use HlsPlan::*;
        use Level::*;
        let u = AtmosState::Unknown;
        for p in [4u8, 9, 10, 20] {
            chk(vid("hevc", Some("HDR10"), Some(p)), u, NO, Full, "Tam HDR10", Some(Copy));
            chk(vid("hevc", None, Some(p)), u, NO, Full, "Tam", Some(Copy));
        }
    }

    #[test]
    fn summary_dolby_vision_only_when_playable_as_dv() {
        let dv = |codec: &str, p: u8, caps| {
            let mut v = vid(codec, Some("HDR10"), Some(p));
            v.dv_bl_compat = Some(1);
            report(&info(vec![v]), &HashMap::new(), caps).summary.dolby_vision
        };
        assert_eq!(dv("hevc", 8, NO).as_deref(), Some("Profile 8.1"));
        assert_eq!(dv("hevc", 5, NO).as_deref(), Some("Profile 5.1"));
        assert_eq!(dv("hevc", 7, NO), None);
        assert_eq!(dv("hevc", 4, NO), None);
        assert_eq!(dv("av1", 10, NO), None);
        assert_eq!(dv("av1", 10, DeviceCaps { av1_hw: true }), None);
    }

    #[test]
    fn unknown_channels_leave_no_stray_spaces_or_commas() {
        let t = |codec: &str| {
            let mut t = tr(2, TrackKind::Audio, codec);
            t.channels = None;
            t
        };
        let r = one(t("truehd"), AtmosState::Yes, NO);
        assert_eq!(r.on_iphone, "Kayıpsız, Atmos nesneleri çalmaz");
        assert_eq!(one(t("truehd"), AtmosState::No, NO).on_iphone, "Kayıpsız");
        assert_eq!(one(t("dts"), AtmosState::No, NO).on_iphone, "Kayıpsız/çok kanallı (DTS:X nesneleri çalmaz)");
        assert_eq!(one(t("eac3"), AtmosState::No, NO).on_iphone, "Dolby Digital Plus");
    }

    #[test]
    fn playable_and_dv_summary() {
        let i = info(vec![vid("hevc", None, None), aud("vorbis", 2)]);
        assert!(!report(&i, &HashMap::new(), NO).summary.playable);
        assert!(report(&info(vec![vid("h264", None, None)]), &HashMap::new(), NO).summary.playable);
        assert!(!report(&info(vec![vid("vp9", None, None), aud("aac", 2)]), &HashMap::new(), NO).summary.playable);
        let mut v = vid("hevc", Some("HDR10"), Some(8));
        v.dv_bl_compat = Some(1);
        let s = report(&info(vec![v]), &HashMap::new(), NO).summary;
        assert_eq!(s.dolby_vision.as_deref(), Some("Profile 8.1"));
        assert_eq!(s.hdr.as_deref(), Some("HDR10"));
    }

    /// iOS `SupportDecodingTests` fixture'ı: `ios/torwatchTests/Fixtures/support_report.json`.
    /// Gerçek `report()` çıktısıdır; yeniden üretmek için `TORWATCH_WRITE_FIXTURE=1 cargo test -p torwatch-core support_report_fixture`.
    #[test]
    fn support_report_fixture() {
        let mut v = vid("hevc", Some("HDR10"), Some(8));
        v.width = Some(3840);
        v.height = Some(2160);
        v.bit_depth = Some(10);
        v.dv_bl_compat = Some(1);
        let a = |id: u64, codec: &str, ch: u32, lang: &str| {
            let mut t = aud(codec, ch);
            t.id = id;
            t.language = Some(lang.into());
            t.sample_rate = Some(48000);
            t
        };
        let eac3 = a(2, "eac3", 6, "en");
        let thd = a(3, "truehd", 8, "en");
        let ac3 = a(4, "ac3", 6, "tr");
        let mut srt = tr(5, TrackKind::Subtitle, "srt");
        srt.language = Some("tr".into());
        let mut pgs = tr(6, TrackKind::Subtitle, "pgs");
        pgs.language = Some("en".into());
        let i = info(vec![v, eac3, thd, ac3, srt, pgs]);
        let m = HashMap::from([(2, AtmosState::Yes), (3, AtmosState::Yes), (4, AtmosState::No)]);
        let json = serde_json::to_string_pretty(&report(&i, &m, NO)).unwrap() + "\n";
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ios/torwatchTests/Fixtures/support_report.json");
        if std::env::var_os("TORWATCH_WRITE_FIXTURE").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &json).unwrap();
        }
        // (the fixture is shared with torwatch's iOS tests; HyperDownload has no iOS app: without the file there is nothing to compare)
        let Ok(on_disk) = std::fs::read_to_string(&path) else { return };
        assert_eq!(on_disk.replace("\r\n", "\n"), json);
    }
}
