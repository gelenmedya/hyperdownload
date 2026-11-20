//! HLS segment planı ve oynatma listeleri (ana + medya).
//! Listeler MEDYA zamanındadır (Ruling R24): `EXTINF` süreleri `HLS_TIME_OFFSET_SECS`'ten etkilenmez.

use crate::atmos::AtmosState;
use crate::demux::{DoviConfig, MediaInfo, TrackInfo};
use crate::hls::codecs::{audio_codec_string, avc_codec_string, dv_codec_string, hevc_codec_string};
use crate::hls::segment::Span;
use crate::support::{HlsPlan, Level, TrackSupport};

/// Plan/ana liste üretim hataları. `Display` kullanıcıya gösterilecek Türkçe iletidir.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// Anahtar kare dizini (MKV Cues / MP4 sync örnekleri) yok; sınırlar uydurulamaz (Ruling R26).
    NoKeyframeIndex,
    NoPlayableVideo,
    UnsupportedVideoCodec,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PlanError::NoKeyframeIndex => "Bu dosyada anahtar kare dizini yok; iPhone'da oynatılamıyor",
            PlanError::NoPlayableVideo => "Oynatılabilir video izi yok",
            PlanError::UnsupportedVideoCodec => "Bu video codec'i iPhone'da oynatılamıyor",
        })
    }
}

impl std::error::Error for PlanError {}

pub const TARGET_SEGMENT_SECS: f64 = 6.0;

/// Harici altyazı kimlikleri ad alanlıdır: `EXTERNAL_SUB_ID_BASE + dosya_indeksi`.
pub const EXTERNAL_SUB_ID_BASE: u64 = 1_000_000;

#[derive(Clone, Debug)]
pub struct ExternalSub {
    pub id: u64,
    pub name: String,
    pub lang: Option<String>,
}

/// Bundan kısa kuyruk span'ı öncekine katılır (sn).
const MIN_TAIL_SECS: f64 = 0.5;

/// Segment planı. `keyframes`: video anahtar kare zamanları (`TimeIndex.points`); sıralanır, tekrarlar atılır.
/// Anahtar kareler en az `TARGET_SEGMENT_SECS` olacak şekilde birleştirilir. Her span başlangıcı (ilki hariç)
/// bir anahtar karedir; ilk span 0'dan başlar, son span `duration`da biter, hiçbir span sıfır süreli değildir
/// ve 0.5 sn'den kısa kuyruk öncekine katılır.
///
/// Anahtar kare listesi boşsa `PlanError::NoKeyframeIndex` (Ruling R26): sınırlar uydurulmaz.
pub fn plan(keyframes: &[f64], duration: f64) -> Result<Vec<Span>, PlanError> {
    let mut kfs: Vec<f64> = keyframes.iter().copied().filter(|k| k.is_finite() && *k >= 0.0).collect();
    if kfs.is_empty() {
        return Err(PlanError::NoKeyframeIndex);
    }
    if !duration.is_finite() || duration <= 1e-3 {
        return Ok(Vec::new());
    }
    let mut starts: Vec<f64> = vec![0.0];
    kfs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    for k in kfs {
        let cur = *starts.last().unwrap();
        if k - cur >= TARGET_SEGMENT_SECS && duration - k >= MIN_TAIL_SECS {
            starts.push(k);
        }
    }
    Ok(starts
        .iter()
        .enumerate()
        .map(|(i, &start)| Span { index: i as u32, start, end: starts.get(i + 1).copied().unwrap_or(duration) })
        .collect())
}

/// ISO 639-2 (3 harf) -> BCP-47; 2 harfli kodlar ve `pt-BR` gibi bölgeli etiketler (alt etiketler doğrulanır)
/// aynen geçer; boş/`und`/geçersiz -> `None`. En çok 35 karakter.
fn bcp47(lang: Option<&str>) -> Option<String> {
    let l = lang?.trim().to_ascii_lowercase().replace('_', "-");
    if l.is_empty() || l.len() > 35 || l == "und" {
        return None;
    }
    let mut parts = l.split('-');
    let primary = parts.next()?;
    if !(2..=3).contains(&primary.len()) || !primary.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let rest: Vec<&str> = parts.collect();
    if rest.iter().any(|p| p.is_empty() || p.len() > 8 || !p.chars().all(|c| c.is_ascii_alphanumeric())) {
        return None;
    }
    let primary = match primary {
        "eng" => "en",
        "rus" => "ru",
        "ukr" => "uk",
        "tur" => "tr",
        "fre" | "fra" => "fr",
        "ger" | "deu" => "de",
        "spa" => "es",
        "ita" => "it",
        "por" => "pt",
        "jpn" => "ja",
        "kor" => "ko",
        "chi" | "zho" => "zh",
        "ara" => "ar",
        "pol" => "pl",
        "dut" | "nld" => "nl",
        "swe" => "sv",
        "cze" | "ces" => "cs",
        "slk" | "slo" => "sk",
        "gre" | "ell" => "el",
        "hun" => "hu",
        "rum" | "ron" => "ro",
        "bul" => "bg",
        "heb" => "he",
        "hin" => "hi",
        "nor" | "nob" | "nno" => "no",
        "dan" => "da",
        "fin" => "fi",
        "vie" => "vi",
        "tha" => "th",
        "ind" => "id",
        "fas" | "per" => "fa",
        "hrv" => "hr",
        "srp" => "sr",
        other => other, // 2 harfli kod ya da tabloda olmayan 3 harfli kod: aynen
    };
    let mut out = primary.to_string();
    for p in rest {
        out.push('-');
        out.push_str(p);
    }
    Some(out)
}

/// Öznitelik değeri için: `"` -> `'`, kontrol karakterleri (CR/LF dahil) atılır.
fn q(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect::<String>().replace('"', "'")
}

/// Aynı adın ikinci ve sonraki rastlanışlarına ` (2)`, ` (3)` ekler.
fn unique_names(names: Vec<String>) -> Vec<String> {
    let mut seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    names
        .into_iter()
        .map(|n| {
            let c = seen.entry(n.clone()).or_insert(0);
            *c += 1;
            if *c == 1 { n } else { format!("{n} ({c})") }
        })
        .collect()
}

fn channels_attr(ch: Option<u32>, codec: &str, atmos: Option<AtmosState>, plan: Option<HlsPlan>) -> String {
    // JOC yalnızca E-AC-3 olduğu gibi kopyalanırken geçerli (FLAC'a çözülürse nesne meta verisi yok).
    if codec == "eac3" && atmos == Some(AtmosState::Yes) && plan == Some(HlsPlan::Copy) {
        return "16/JOC".into();
    }
    match ch {
        Some(n) if n > 0 => n.to_string(),
        _ => "2".into(),
    }
}

/// Ana oynatma listesi. `file_len`: dosya boyutu (bayt; BANDWIDTH için). `externals`: harici altyazı dosyaları
/// (Görev 12 verir); kimlikleri `EXTERNAL_SUB_ID_BASE + dosya_indeksi` ile ad alanlıdır ve `s{id}.m3u8` olur.
pub fn master(
    info: &MediaInfo,
    support: &[TrackSupport],
    dv: Option<&DoviConfig>,
    file_len: u64,
    externals: &[ExternalSub],
) -> Result<String, PlanError> {
    let playable = |s: &&TrackSupport| s.level != Level::Unsupported;
    let track = |id: u64| info.tracks.iter().find(|t| t.id == id);

    // Video önce çözülür: oynatılabilir/desteklenen video yoksa yetim EXT-X-MEDIA üretilmez.
    let v = support
        .iter()
        .filter(|s| s.kind == "video")
        .filter(playable)
        .find_map(|s| track(s.track_id))
        .ok_or(PlanError::NoPlayableVideo)?;
    // Açık eşleme: h264 -> avc1, hevc -> hvc1/dvh1. AV1/VP9/MPEG-2 vb. için avc1'e düşülmez.
    // AV1 (`av01.P.LLT.DD`) henüz üretilmiyor: Görev 12 av1'i oynatılamaz saymalı.
    let base = match v.codec.as_str() {
        "hevc" => hevc_codec_string(&v.codec_private),
        "h264" => avc_codec_string(&v.codec_private),
        _ => return Err(PlanError::UnsupportedVideoCodec),
    };

    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n");

    // Ses: CODECS dizesi üretilemeyen iz atlanır (CODECS her kalan kodeği listeler).
    let auds: Vec<(&TrackSupport, &TrackInfo)> = support
        .iter()
        .filter(|s| s.kind == "audio")
        .filter(playable)
        .filter_map(|s| track(s.track_id).map(|t| (s, t)))
        .filter(|(s, t)| audio_codec_string(t, s.hls).is_some())
        .collect();
    let names = unique_names(auds.iter().map(|(s, _)| q(&s.label)).collect());
    let default_idx = auds.iter().position(|(_, t)| t.default).unwrap_or(0);
    let mut audio_codecs: Vec<String> = Vec::new();
    for (i, ((s, t), name)) in auds.iter().zip(&names).enumerate() {
        let lang = bcp47(t.language.as_deref()).map(|l| format!("LANGUAGE=\"{l}\",")).unwrap_or_default();
        out.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"{name}\",{lang}DEFAULT={},AUTOSELECT=YES,CHANNELS=\"{}\",URI=\"a{}.m3u8\"\n",
            if i == default_idx { "YES" } else { "NO" },
            channels_attr(t.channels, &t.codec, s.atmos, s.hls),
            s.track_id
        ));
        if let Some(c) = audio_codec_string(t, s.hls) {
            if !audio_codecs.contains(&c) {
                audio_codecs.push(c);
            }
        }
    }

    // Altyazı: yalnızca Full metin altyazıları + harici dosyalar.
    let mut subs: Vec<(String, Option<String>, u64)> = support
        .iter()
        .filter(|s| s.kind == "subtitle" && s.level == Level::Full)
        .map(|s| (q(&s.label), bcp47(track(s.track_id).and_then(|t| t.language.as_deref())), s.track_id))
        .collect();
    subs.extend(externals.iter().map(|e| (q(&e.name), bcp47(e.lang.as_deref()), e.id)));
    let sub_names = unique_names(subs.iter().map(|s| s.0.clone()).collect());
    for ((_, lang, id), name) in subs.iter().zip(&sub_names) {
        let lang = lang.as_ref().map(|l| format!("LANGUAGE=\"{l}\",")).unwrap_or_default();
        out.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"sub\",NAME=\"{name}\",{lang}DEFAULT=NO,AUTOSELECT=YES,URI=\"s{id}.m3u8\"\n"
        ));
    }

    // Video
    {
        let dvp = dv.map(|d| (d.profile, d.level, d.bl_compat)).or_else(|| {
            v.dv_profile.map(|p| (p, v.dv_level.unwrap_or(6) /* dv_level yoksa: DV 4K24 için en yaygın düzey (06) */, v.dv_bl_compat.unwrap_or(0)))
        });
        let mut range = match v.hdr.as_deref() {
            Some("HLG") => "HLG",
            Some("HDR10") => "PQ",
            _ => "SDR",
        };
        let (mut vcodec, mut supplemental) = (base, None);
        if v.codec == "hevc" {
            match dvp {
                Some((5, l, _)) => {
                    vcodec = dv_codec_string(5, l, true);
                    range = "PQ";
                }
                Some((8, l, c)) => {
                    let brand = match c {
                        1 => Some(("db1p", "PQ")),
                        2 => Some(("db2g", "SDR")),
                        4 => Some(("db4h", "HLG")),
                        _ => None,
                    };
                    if let Some((b, r)) = brand {
                        supplemental = Some(format!("{}/{b}", dv_codec_string(8, l, true)));
                        range = r;
                    }
                }
                Some((7, _, _)) => range = "PQ",
                _ => {}
            }
        }
        let mut codecs = vec![vcodec];
        codecs.extend(audio_codecs.iter().cloned());

        let dur = info.duration_secs.filter(|d| *d > 0.0);
        let avg = dur.map(|d| (file_len as f64 * 8.0 / d).round() as u64);
        let bw = avg.map(|a| (a as f64 * 1.2).round() as u64).unwrap_or(10_000_000).max(1);
        let mut line = format!("#EXT-X-STREAM-INF:BANDWIDTH={bw}");
        if let Some(a) = avg.filter(|a| *a > 0) {
            line.push_str(&format!(",AVERAGE-BANDWIDTH={a}"));
        }
        line.push_str(&format!(",CODECS=\"{}\"", codecs.join(",")));
        if let Some(s) = supplemental {
            line.push_str(&format!(",SUPPLEMENTAL-CODECS=\"{s}\""));
        }
        if let (Some(w), Some(h)) = (v.width, v.height) {
            line.push_str(&format!(",RESOLUTION={w}x{h}"));
        }
        if let Some(ns) = v.default_duration_ns.filter(|n| *n > 0) {
            line.push_str(&format!(",FRAME-RATE={:.3}", 1e9 / ns as f64));
        }
        line.push_str(&format!(",VIDEO-RANGE={range}"));
        if !auds.is_empty() {
            line.push_str(",AUDIO=\"aud\"");
        }
        if !subs.is_empty() {
            line.push_str(",SUBTITLES=\"sub\"");
        }
        out.push_str(&line);
        out.push_str("\nv.m3u8\n");
    }
    Ok(out)
}

/// Medya oynatma listesi. `prefix`: `v` | `a12` | `s3`; `ext`: `m4s` | `vtt`. fMP4 (`m4s`) için `EXT-X-MAP` eklenir.
pub fn media(spans: &[Span], prefix: &str, ext: &str) -> String {
    let max = spans.iter().map(|s| s.end - s.start).fold(0.0_f64, f64::max);
    let mut out = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n",
        (max.ceil() as u64).max(1)
    );
    if ext == "m4s" {
        out.push_str(&format!("#EXT-X-MAP:URI=\"{prefix}/init.mp4\"\n"));
    }
    for s in spans {
        out.push_str(&format!("#EXTINF:{:.3},\n{prefix}/{}.{ext}\n", s.end - s.start, s.index));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::TrackKind;
    use crate::support::HlsPlan;

    fn kf(n: usize, step: f64) -> Vec<f64> {
        (0..n).map(|i| i as f64 * step).collect()
    }

    #[test]
    fn plan_merges_keyframes_to_target() {
        // 2 sn aralıklı anahtar kareler -> 6 sn'lik segmentler.
        let s = plan(&kf(10, 2.0), 20.0).unwrap();
        assert_eq!(s.len(), 4);
        assert_eq!((s[0].start, s[0].end), (0.0, 6.0));
        assert_eq!((s[1].start, s[1].end), (6.0, 12.0));
        assert_eq!((s[3].start, s[3].end), (18.0, 20.0));
        for (i, sp) in s.iter().enumerate() {
            assert_eq!(sp.index as usize, i);
        }
        // Başlangıçlar anahtar karelere denk gelir.
        assert!(s.iter().skip(1).all(|sp| kf(10, 2.0).contains(&sp.start)));
    }

    #[test]
    fn plan_without_keyframes_is_error() {
        let e = plan(&[], 15.0).unwrap_err();
        assert_eq!(e, PlanError::NoKeyframeIndex);
        assert_eq!(e.to_string(), "Bu dosyada anahtar kare dizini yok; iPhone'da oynatılamıyor");
    }

    #[test]
    fn plan_non_finite_or_negative_keyframes_are_no_index() {
        assert_eq!(plan(&[f64::NAN], 15.0).unwrap_err(), PlanError::NoKeyframeIndex);
        assert_eq!(plan(&[f64::INFINITY, -1.0], 15.0).unwrap_err(), PlanError::NoKeyframeIndex);
    }

    #[test]
    fn plan_merges_short_tail_into_previous() {
        // Son anahtar kare 18.3'te, süre 18.6: 0.3 sn'lik kuyruk 12'den başlayan span'a katılır.
        let s = plan(&[0.0, 6.0, 12.0, 18.3], 18.6).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!((s[2].start, s[2].end), (12.0, 18.6));
        // 0.5 sn ve üstü kuyruk korunur.
        let s = plan(&[0.0, 6.0, 12.0, 18.0], 18.5).unwrap();
        assert_eq!(s.len(), 4);
        assert_eq!((s[3].start, s[3].end), (18.0, 18.5));
    }

    #[test]
    fn plan_never_emits_empty_segment() {
        // Tekrarlayan, sıfır, süre sonundaki ve süreyi aşan anahtar kareler.
        let s = plan(&[0.0, 0.0, 6.0, 6.0, 12.0, 20.0, 25.0], 20.0).unwrap();
        assert!(s.iter().all(|sp| sp.end - sp.start > 1e-3), "{s:?}");
        assert_eq!(s.last().unwrap().end, 20.0);
        for w in s.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
        assert!(plan(&[0.0], 0.0).unwrap().is_empty());
    }

    #[test]
    fn joc_only_when_copied() {
        let a = audio(2, "eac3", 6, "eng");
        let (info, s) = mk(
            vec![video(None, None), a],
            &[V, ("EN", Level::Full, Some(AtmosState::Yes), Some(HlsPlan::DecodeToFlac))],
        );
        let m = master(&info, &s, None, 1_000_000_000, &[]).unwrap();
        assert!(!m.contains("JOC") && m.contains("CHANNELS=\"6\""), "{m}");
    }

    #[test]
    fn master_errors_without_playable_or_supported_video() {
        let (info, s) = mk(vec![audio(2, "ac3", 6, "eng")], &[("EN", Level::Full, None, Some(HlsPlan::Copy))]);
        assert_eq!(master(&info, &s, None, 1, &[]).unwrap_err(), PlanError::NoPlayableVideo);
        let mut av1 = video(None, None);
        av1.codec = "av1".into();
        let (info, s) = mk(vec![av1], &[V]);
        assert_eq!(master(&info, &s, None, 1, &[]).unwrap_err(), PlanError::UnsupportedVideoCodec);
    }

    #[test]
    fn master_skips_audio_without_codec_string_and_sanitizes() {
        let (info, s) = mk(
            vec![video(None, None), audio(2, "vorbis", 2, "eng"), audio(3, "ac3", 6, "pt-BR")],
            &[V, ("X", Level::Full, None, Some(HlsPlan::Copy)), ("A\r\nB\"", Level::Full, None, Some(HlsPlan::Copy))],
        );
        let m = master(&info, &s, None, 1_000_000_000, &[]).unwrap();
        assert!(!m.contains("a2.m3u8"), "{m}");
        assert!(m.contains("NAME=\"AB'\",LANGUAGE=\"pt-br\""), "{m}");
        assert!(m.contains("CODECS=\"hvc1.2.4.L153.B0,ac-3\""), "{m}");
    }

    #[test]
    fn bcp47_table() {
        assert_eq!(bcp47(Some("nob")).as_deref(), Some("no"));
        assert_eq!(bcp47(Some("slo")).as_deref(), Some("sk"));
        assert_eq!(bcp47(Some("und")), None);
        assert_eq!(bcp47(Some("en-US")).as_deref(), Some("en-us"));
        assert_eq!(bcp47(Some("en-")), None);
        assert_eq!(bcp47(Some(&"a".repeat(40))), None);
    }

    // --- master ---
    fn base(id: u64, kind: TrackKind, codec: &str) -> TrackInfo {
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
    fn hvcc_main10() -> Vec<u8> {
        let mut v = vec![1, 0x02, 0x20, 0, 0, 0, 0xB0, 0, 0, 0, 0, 0, 153];
        v.extend_from_slice(&[0; 10]);
        v
    }
    fn video(dv: Option<(u8, u8, u8)>, hdr: Option<&str>) -> TrackInfo {
        let mut t = base(1, TrackKind::Video, "hevc");
        t.codec_private = hvcc_main10();
        t.width = Some(3840);
        t.height = Some(2160);
        t.hdr = hdr.map(Into::into);
        t.default_duration_ns = Some(41_708_333);
        if let Some((p, l, c)) = dv {
            t.dv_profile = Some(p);
            t.dv_level = Some(l);
            t.dv_bl_compat = Some(c);
        }
        t
    }
    fn audio(id: u64, codec: &str, ch: u32, lang: &str) -> TrackInfo {
        let mut t = base(id, TrackKind::Audio, codec);
        t.channels = Some(ch);
        t.language = Some(lang.into());
        t
    }
    type S = (&'static str, Level, Option<AtmosState>, Option<HlsPlan>);
    fn mk(tracks: Vec<TrackInfo>, sups: &[S]) -> (MediaInfo, Vec<TrackSupport>) {
        let s = tracks
            .iter()
            .zip(sups)
            .map(|(t, (label, level, atmos, hls))| TrackSupport {
                track_id: t.id,
                kind: match t.kind {
                    TrackKind::Video => "video",
                    TrackKind::Audio => "audio",
                    _ => "subtitle",
                },
                label: (*label).into(),
                in_file: String::new(),
                on_iphone: String::new(),
                level: *level,
                atmos: *atmos,
                hls: *hls,
            })
            .collect();
        (MediaInfo { container: "matroska", duration_secs: Some(7200.0), title: None, tracks }, s)
    }
    const V: S = ("V", Level::Full, None, Some(HlsPlan::Copy));

    #[test]
    fn master_dv_p81_supplemental_codecs() {
        let (info, s) = mk(
            vec![video(Some((8, 6, 1)), Some("HDR10")), audio(2, "eac3", 6, "eng")],
            &[V, ("EN", Level::Full, Some(AtmosState::No), Some(HlsPlan::Copy))],
        );
        let m = master(&info, &s, None, 36_000_000_000, &[]).unwrap();
        assert!(m.contains("SUPPLEMENTAL-CODECS=\"dvh1.08.06/db1p\""), "{m}");
        assert!(m.contains("VIDEO-RANGE=PQ"), "{m}");
        assert!(m.contains("CODECS=\"hvc1.2.4.L153.B0,ec-3\""), "{m}");
        assert!(m.contains("RESOLUTION=3840x2160"), "{m}");
        assert!(m.contains("FRAME-RATE=23.976"), "{m}");
        // 36e9 * 8 / 7200 = 40 Mbit/s ortalama, x1.2 = 48 Mbit/s
        assert!(m.contains("BANDWIDTH=48000000"), "{m}");
        assert!(m.contains("AVERAGE-BANDWIDTH=40000000"), "{m}");
        assert!(m.starts_with("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n"), "{m}");
    }

    #[test]
    fn master_dv_p81_uses_dovi_arg_over_track() {
        let (info, s) = mk(vec![video(None, None)], &[V]);
        let dv = DoviConfig { profile: 8, level: 9, bl_compat: 4, rpu: true, el: false, bl: true };
        let m = master(&info, &s, Some(&dv), 1_000_000_000, &[]).unwrap();
        assert!(m.contains("SUPPLEMENTAL-CODECS=\"dvh1.08.09/db4h\""), "{m}");
        assert!(m.contains("VIDEO-RANGE=HLG"), "{m}");
    }

    #[test]
    fn master_dv_p5_uses_dvh1() {
        let (info, s) = mk(vec![video(Some((5, 6, 0)), None)], &[V]);
        let m = master(&info, &s, None, 1_000_000_000, &[]).unwrap();
        assert!(m.contains("CODECS=\"dvh1.05.06\""), "{m}");
        assert!(!m.contains("SUPPLEMENTAL-CODECS"), "{m}");
        assert!(m.contains("VIDEO-RANGE=PQ"), "{m}");
    }

    #[test]
    fn master_p7_has_no_dv_codec() {
        let (info, s) = mk(
            vec![video(Some((7, 6, 6)), Some("HDR10"))],
            &[("V", Level::Degraded, None, Some(HlsPlan::StripDvEnhancement))],
        );
        let m = master(&info, &s, None, 1_000_000_000, &[]).unwrap();
        assert!(!m.contains("dvh1") && !m.contains("SUPPLEMENTAL"), "{m}");
        assert!(m.contains("hvc1.2.4.L153.B0"), "{m}");
        assert!(m.contains("VIDEO-RANGE=PQ"), "{m}");
    }

    #[test]
    fn master_joc_channels() {
        let (info, s) = mk(
            vec![video(None, None), audio(2, "eac3", 6, "eng"), audio(3, "eac3", 6, "tur")],
            &[
                V,
                ("EN Atmos", Level::Full, Some(AtmosState::Yes), Some(HlsPlan::Copy)),
                ("TR", Level::Full, Some(AtmosState::No), Some(HlsPlan::Copy)),
            ],
        );
        let m = master(&info, &s, None, 1_000_000_000, &[]).unwrap();
        assert!(m.contains("CHANNELS=\"16/JOC\""), "{m}");
        assert!(m.contains("CHANNELS=\"6\""), "{m}");
        assert!(m.contains("URI=\"a2.m3u8\"") && m.contains("URI=\"a3.m3u8\""), "{m}");
        assert!(m.contains("LANGUAGE=\"en\"") && m.contains("LANGUAGE=\"tr\""), "{m}");
        // CODECS içinde ec-3 bir kez.
        assert_eq!(m.matches("ec-3").count(), 1, "{m}");
    }

    #[test]
    fn audio_names_are_unique() {
        let a = ("EN \"x\"", Level::Full, None, Some(HlsPlan::Copy));
        let (info, s) = mk(
            vec![video(None, None), audio(2, "ac3", 6, "eng"), audio(3, "ac3", 6, "eng"), audio(4, "ac3", 6, "eng")],
            &[V, a, a, a],
        );
        let m = master(&info, &s, None, 1_000_000_000, &[]).unwrap();
        assert!(m.contains("NAME=\"EN 'x'\""), "{m}");
        assert!(m.contains("NAME=\"EN 'x' (2)\""), "{m}");
        assert!(m.contains("NAME=\"EN 'x' (3)\""), "{m}");
    }

    #[test]
    fn master_flac_subtitles_externals_and_default() {
        let a1 = audio(2, "truehd", 8, "rus");
        let mut a2 = audio(3, "aac", 2, "und");
        a2.default = true;
        a2.codec_private = vec![0x12, 0x10];
        let mut sub = base(4, TrackKind::Subtitle, "srt");
        sub.language = Some("ukr".into());
        let mut pgs = base(5, TrackKind::Subtitle, "pgs");
        pgs.language = Some("eng".into());
        let (info, s) = mk(
            vec![video(None, Some("HLG")), a1, a2, sub, pgs],
            &[
                V,
                ("RU TrueHD", Level::Degraded, Some(AtmosState::Yes), Some(HlsPlan::DecodeToFlac)),
                ("AAC", Level::Full, None, Some(HlsPlan::Copy)),
                ("UK", Level::Full, None, None),
                ("PGS", Level::Unsupported, None, None),
            ],
        );
        let ext = [ExternalSub { id: EXTERNAL_SUB_ID_BASE + 2, name: "Film.tr".into(), lang: Some("tr".into()) }];
        let m = master(&info, &s, None, 1_000_000_000, &ext).unwrap();
        assert!(m.contains("CODECS=\"hvc1.2.4.L153.B0,fLaC,mp4a.40.2\""), "{m}");
        assert!(m.contains("VIDEO-RANGE=HLG"), "{m}");
        assert!(m.contains("NAME=\"RU TrueHD\",LANGUAGE=\"ru\",DEFAULT=NO"), "{m}");
        assert!(m.contains("NAME=\"AAC\",DEFAULT=YES"), "{m}"); // und -> LANGUAGE yok
        assert!(m.contains("CHANNELS=\"8\""), "{m}");
        assert!(m.contains("TYPE=SUBTITLES,GROUP-ID=\"sub\",NAME=\"UK\",LANGUAGE=\"uk\""), "{m}");
        assert!(m.contains("URI=\"s4.m3u8\""), "{m}");
        assert!(m.contains("NAME=\"Film.tr\",LANGUAGE=\"tr\"") && m.contains("URI=\"s1000002.m3u8\""), "{m}");
        assert!(!m.contains("s5.m3u8"), "{m}");
        assert!(m.contains("AUDIO=\"aud\",SUBTITLES=\"sub\""), "{m}");
    }

    #[test]
    fn master_full_example_dv_atmos_truehd_srt() {
        let mut a1 = audio(2, "eac3", 6, "eng");
        a1.default = true;
        let a2 = audio(3, "truehd", 8, "eng");
        let mut sub = base(4, TrackKind::Subtitle, "srt");
        sub.language = Some("tur".into());
        let (info, s) = mk(
            vec![video(Some((8, 6, 1)), Some("HDR10")), a1, a2, sub],
            &[
                V,
                ("EN · E-AC-3 Atmos", Level::Full, Some(AtmosState::Yes), Some(HlsPlan::Copy)),
                ("EN · TrueHD 7.1", Level::Degraded, Some(AtmosState::No), Some(HlsPlan::DecodeToFlac)),
                ("TR · SRT", Level::Full, None, None),
            ],
        );
        let m = master(&info, &s, None, 36_000_000_000, &[]).unwrap();
        let expected = "#EXTM3U
#EXT-X-VERSION:7
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"EN · E-AC-3 Atmos\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,CHANNELS=\"16/JOC\",URI=\"a2.m3u8\"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"EN · TrueHD 7.1\",LANGUAGE=\"en\",DEFAULT=NO,AUTOSELECT=YES,CHANNELS=\"8\",URI=\"a3.m3u8\"
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"sub\",NAME=\"TR · SRT\",LANGUAGE=\"tr\",DEFAULT=NO,AUTOSELECT=YES,URI=\"s4.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=48000000,AVERAGE-BANDWIDTH=40000000,CODECS=\"hvc1.2.4.L153.B0,ec-3,fLaC\",SUPPLEMENTAL-CODECS=\"dvh1.08.06/db1p\",RESOLUTION=3840x2160,FRAME-RATE=23.976,VIDEO-RANGE=PQ,AUDIO=\"aud\",SUBTITLES=\"sub\"
v.m3u8
";
        assert_eq!(m, expected);
    }

    #[test]
    fn media_playlist_has_map_and_endlist() {
        let spans = [Span { index: 0, start: 0.0, end: 6.0 }, Span { index: 1, start: 6.0, end: 12.5 }];
        let m = media(&spans, "a12", "m4s");
        assert!(m.contains("#EXT-X-PLAYLIST-TYPE:VOD"), "{m}");
        assert!(m.contains("#EXT-X-TARGETDURATION:7"), "{m}");
        assert!(m.contains("#EXT-X-MAP:URI=\"a12/init.mp4\""), "{m}");
        assert!(m.contains("#EXTINF:6.000,\na12/0.m4s"), "{m}");
        assert!(m.contains("#EXTINF:6.500,\na12/1.m4s"), "{m}");
        assert!(m.trim_end().ends_with("#EXT-X-ENDLIST"), "{m}");
        let v = media(&spans, "s3", "vtt");
        assert!(!v.contains("EXT-X-MAP") && v.contains("s3/1.vtt"), "{v}");
    }
}
