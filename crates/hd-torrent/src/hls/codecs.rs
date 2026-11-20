//! HLS `CODECS` / `SUPPLEMENTAL-CODECS` dizeleri (RFC 6381, ISO 14496-15, Dolby Vision profilleri).

use crate::demux::TrackInfo;
use crate::support::HlsPlan;

/// `hvcC` kaydından `hvc1.<profil>.<uyumluluk>.<L|H><seviye>.<kısıtlar>` (ör. `hvc1.2.4.L153.B0`).
pub fn hevc_codec_string(hvcc: &[u8]) -> String {
    if hvcc.len() < 13 {
        return "hvc1".into();
    }
    let space = match hvcc[1] >> 6 {
        1 => "A",
        2 => "B",
        3 => "C",
        _ => "",
    };
    let tier = if hvcc[1] & 0x20 != 0 { 'H' } else { 'L' };
    let profile = hvcc[1] & 0x1F;
    let compat = u32::from_be_bytes([hvcc[2], hvcc[3], hvcc[4], hvcc[5]]).reverse_bits();
    let level = hvcc[12];
    let mut n = 6;
    while n > 1 && hvcc[6 + n - 1] == 0 {
        n -= 1;
    }
    let constraints: Vec<String> = hvcc[6..6 + n].iter().map(|b| format!("{b:X}")).collect();
    format!("hvc1.{space}{profile}.{compat:X}.{tier}{level}.{}", constraints.join("."))
}

/// `avcC` kaydından `avc1.PPCCLL` (ör. `avc1.640033`).
pub fn avc_codec_string(avcc: &[u8]) -> String {
    if avcc.len() < 4 {
        return "avc1".into();
    }
    format!("avc1.{:02x}{:02x}{:02x}", avcc[1], avcc[2], avcc[3])
}

/// Dolby Vision codec dizesi: `dvh1.08.06` (`dvhe` için `dvh1 = false`).
pub fn dv_codec_string(profile: u8, level: u8, dvh1: bool) -> String {
    format!("{}.{profile:02}.{level:02}", if dvh1 { "dvh1" } else { "dvhe" })
}

/// Bir ses izinin HLS `CODECS` karşılığı. `DecodeToFlac` → `fLaC`.
pub fn audio_codec_string(track: &TrackInfo, plan: Option<HlsPlan>) -> Option<String> {
    if plan == Some(HlsPlan::DecodeToFlac) {
        return Some("fLaC".into());
    }
    Some(match track.codec.as_str() {
        "eac3" => "ec-3".into(),
        "ac3" => "ac-3".into(),
        "aac" => format!("mp4a.40.{}", aac_aot(&track.codec_private)),
        "mp3" => "mp4a.6B".into(),
        "flac" => "fLaC".into(),
        "opus" => "Opus".into(),
        _ => return None,
    })
}

/// AudioSpecificConfig'in ilk 5 biti (31 ise 32 + sonraki 6 bit); geçersizse AAC-LC (2).
fn aac_aot(asc: &[u8]) -> u8 {
    let Some(&b0) = asc.first() else { return 2 };
    let mut aot = b0 >> 3;
    if aot == 31 {
        let Some(&b1) = asc.get(1) else { return 2 };
        aot = 32 + (((b0 & 7) << 3) | (b1 >> 5));
    }
    if (1..=45).contains(&aot) { aot } else { 2 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hvcc(profile_byte: u8, compat: u32, constraint: [u8; 6], level: u8) -> Vec<u8> {
        let mut v = vec![1, profile_byte];
        v.extend_from_slice(&compat.to_be_bytes());
        v.extend_from_slice(&constraint);
        v.push(level);
        v.extend_from_slice(&[0; 10]);
        v
    }

    #[test]
    fn hevc_codec_string_known_vector() {
        // Main10, Main tier, L5.1 (153), kısıt B0 -> hvc1.2.4.L153.B0
        assert_eq!(hevc_codec_string(&hvcc(0x02, 0x2000_0000, [0xB0, 0, 0, 0, 0, 0], 153)), "hvc1.2.4.L153.B0");
    }

    #[test]
    fn hevc_codec_string_main_and_high_tier() {
        // Main (compat bit1+2 = 0x60000000 -> ters çevrilmiş 0x6), high tier.
        assert_eq!(hevc_codec_string(&hvcc(0x21, 0x6000_0000, [0x90, 0x01, 0, 0, 0, 0], 120)), "hvc1.1.6.H120.90.1");
        // Tüm kısıtlar sıfır: en az bir bayt yazılır.
        assert_eq!(hevc_codec_string(&hvcc(0x01, 0x6000_0000, [0; 6], 93)), "hvc1.1.6.L93.0");
    }

    #[test]
    fn avc_codec_string_known_vector() {
        assert_eq!(avc_codec_string(&[1, 0x64, 0x00, 0x33, 0xFF]), "avc1.640033");
    }

    #[test]
    fn dv_strings() {
        assert_eq!(dv_codec_string(8, 6, true), "dvh1.08.06");
        assert_eq!(dv_codec_string(5, 6, true), "dvh1.05.06");
        assert_eq!(dv_codec_string(8, 10, false), "dvhe.08.10");
    }

    #[test]
    fn audio_codec_strings() {
        use crate::demux::TrackKind;
        let mk = |codec: &str, cp: Vec<u8>| TrackInfo {
            id: 2,
            kind: TrackKind::Audio,
            codec: codec.into(),
            codec_id: String::new(),
            codec_private: cp,
            language: None,
            name: None,
            default: false,
            channels: Some(2),
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
        };
        let c = |t: &TrackInfo, p| audio_codec_string(t, p);
        assert_eq!(c(&mk("eac3", vec![]), Some(HlsPlan::Copy)).as_deref(), Some("ec-3"));
        assert_eq!(c(&mk("ac3", vec![]), Some(HlsPlan::Copy)).as_deref(), Some("ac-3"));
        assert_eq!(c(&mk("aac", vec![0x12, 0x10]), Some(HlsPlan::Copy)).as_deref(), Some("mp4a.40.2"));
        assert_eq!(c(&mk("aac", vec![0x2B, 0x92]), Some(HlsPlan::Copy)).as_deref(), Some("mp4a.40.5"));
        assert_eq!(c(&mk("aac", vec![]), Some(HlsPlan::Copy)).as_deref(), Some("mp4a.40.2"));
        assert_eq!(c(&mk("flac", vec![]), Some(HlsPlan::Copy)).as_deref(), Some("fLaC"));
        assert_eq!(c(&mk("opus", vec![]), Some(HlsPlan::Copy)).as_deref(), Some("Opus"));
        assert_eq!(c(&mk("truehd", vec![]), Some(HlsPlan::DecodeToFlac)).as_deref(), Some("fLaC"));
        assert_eq!(c(&mk("dts", vec![]), Some(HlsPlan::DecodeToFlac)).as_deref(), Some("fLaC"));
        assert_eq!(c(&mk("vorbis", vec![]), None), None);
    }
}
