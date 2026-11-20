//! Saf Rust ses hattı: tarayıcının çözemediği ses izleri (AC-3, E-AC-3, TrueHD, DTS) torrent
//! akışından okunur, çözülür ve 24-bit çok kanallı WAV olarak ayrı bir akışla verilir. Görüntü
//! orijinal akıştan tarayıcıda oynar; arayüz <audio> öğesini görüntüye senkron tutar.

use std::collections::HashMap;
use std::io::{Read, Seek};

use bytes::Bytes;
use serde_json::json;

use crate::atmos::AtmosState;
use crate::demux::{self, MediaInfo, PacketReader, TrackInfo, TrackKind};

/// Ortak çözücü arayüzü: paket (Matroska bloğu) alır, WAV kanal sırasında f32 PCM üretir.
pub use crate::ac3dec::{AudioFormat, PcmDecoder};

/// Bu motorun Rust'ta çözebildiği kodekler (arayüz uyumlu moda geçmek için kullanır).
pub const RUST_AUDIO_CODECS: &[&str] = &["ac3", "eac3", "truehd", "dts"];

pub fn decoder_for(codec: &str) -> anyhow::Result<Box<dyn PcmDecoder>> {
    match codec {
        "ac3" | "eac3" => Ok(crate::ac3dec::new_ac3_decoder()),
        "truehd" | "mlp" => crate::hddec::new_truehd_decoder(),
        "dts" => crate::hddec::new_dts_decoder(),
        other => anyhow::bail!("bu ses kodeği Rust çözücüde yok: {other}"),
    }
}

/// Akış başlığı: uzunluk bilinmediği için RIFF/data boyutları en büyük değer.
fn wav_header(f: AudioFormat) -> Vec<u8> {
    const BITS: u16 = 24;
    let block_align = f.channels as u32 * (BITS as u32 / 8);
    let mut h = Vec::with_capacity(68);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&u32::MAX.to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&40u32.to_le_bytes());
    h.extend_from_slice(&0xFFFEu16.to_le_bytes()); // WAVE_FORMAT_EXTENSIBLE
    h.extend_from_slice(&f.channels.to_le_bytes());
    h.extend_from_slice(&f.sample_rate.to_le_bytes());
    h.extend_from_slice(&(f.sample_rate * block_align).to_le_bytes());
    h.extend_from_slice(&(block_align as u16).to_le_bytes());
    h.extend_from_slice(&BITS.to_le_bytes());
    h.extend_from_slice(&22u16.to_le_bytes());
    h.extend_from_slice(&BITS.to_le_bytes()); // geçerli bit
    h.extend_from_slice(&f.channel_mask.to_le_bytes());
    // KSDATAFORMAT_SUBTYPE_PCM
    h.extend_from_slice(&[
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
    ]);
    h.extend_from_slice(b"data");
    h.extend_from_slice(&u32::MAX.to_le_bytes());
    h
}

/// f32 -> işaretli 24-bit tamsayı (i32 içinde). 24-bit kaynaklar (TrueHD) için kayıpsızdır:
/// f32 mantisi 24 bit.
pub(crate) fn f32_to_s24(s: f32) -> i32 {
    (s as f64 * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32
}

fn push_s24(out: &mut Vec<u8>, samples: &[f32]) {
    out.reserve(samples.len() * 3);
    for &s in samples {
        out.extend_from_slice(&f32_to_s24(s).to_le_bytes()[..3]);
    }
}

/// Akışın başlangıç hizalaması (`stream_wav` ve HLS FLAC segmentleri ortak kullanır): ilk çözülen
/// paket `offset_secs` (= paket pts - istenen başlangıç) kadar erken/geç ise, çıktının başından
/// atılacak (`skip`) ya da başına eklenecek sessiz (`pad`) örnek sayısını verir. İkisi de
/// iç içe geçmiş (interleaved) örnek sayısıdır; `pad` en çok 10 sn ile sınırlıdır.
pub(crate) fn start_alignment(offset_secs: f64, fmt: AudioFormat, pcm_len: usize) -> (usize, usize) {
    let ch = fmt.channels as usize;
    if offset_secs < 0.0 {
        let drop = ((-offset_secs * fmt.sample_rate as f64).round() as usize * ch).min(pcm_len);
        (drop - drop % ch, 0)
    } else if offset_secs > 0.0 {
        let pad = (offset_secs * fmt.sample_rate as f64).round() as usize * ch;
        (0, pad.min(fmt.sample_rate as usize * ch * 10))
    } else {
        (0, 0)
    }
}

/// Seçilen izi `start` saniyesinden itibaren çözüp WAV parçaları gönderir. WAV tam olarak
/// `start` anından başlar: önceki örnekler atılır, eksik baş kısım sessizlikle doldurulur.
/// Alıcı kapanınca (oynatıcı atladı/kapandı) sessizce biter.
pub fn stream_wav<R: Read + Seek>(
    reader: R,
    track: &TrackInfo,
    start: f64,
    send: &mut dyn FnMut(Bytes) -> bool,
) -> anyhow::Result<()> {
    let mut packets = PacketReader::open(reader, track.id, start)?;
    let mut dec = decoder_for(&track.codec)?;
    let mut pcm: Vec<f32> = Vec::with_capacity(16 * 1024);
    let mut started: Option<AudioFormat> = None;
    let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut errors = 0u32;

    while let Some(pkt) = packets.next()? {
        pcm.clear();
        if let Err(e) = dec.decode(&pkt.data, &mut pcm) {
            errors += 1;
            tracing::debug!("ses paketi çözülemedi ({errors}): {e:#}");
            if errors > 200 {
                anyhow::bail!("ses çözülemiyor: {e:#}");
            }
            continue;
        }
        let Some(fmt) = dec.format() else { continue };
        if pcm.is_empty() {
            continue;
        }
        let mut slice: &[f32] = &pcm;
        match started {
            None => {
                buf.extend_from_slice(&wav_header(fmt));
                let (skip, pad) = start_alignment(pkt.pts_secs - start, fmt, slice.len());
                slice = &slice[skip..];
                if pad > 0 {
                    push_s24(&mut buf, &vec![0.0; pad]);
                }
                started = Some(fmt);
            }
            Some(f) if f != fmt => {
                tracing::warn!("ses biçimi akış ortasında değişti: {f:?} -> {fmt:?}");
                continue;
            }
            Some(_) => {}
        }
        push_s24(&mut buf, slice);
        if buf.len() >= 128 * 1024 && !send(Bytes::from(std::mem::take(&mut buf))) {
            return Ok(());
        }
    }
    if !buf.is_empty() {
        send(Bytes::from(buf));
    }
    Ok(())
}

/// Arayüzün beklediği kodek özeti (eski ffprobe özetiyle aynı biçim).
pub fn info_json(info: &MediaInfo, file_len: u64) -> serde_json::Value {
    let video = info.tracks.iter().find(|t| matches!(t.kind, TrackKind::Video));
    let audio: Vec<_> = info
        .tracks
        .iter()
        .filter(|t| matches!(t.kind, TrackKind::Audio))
        .enumerate()
        .map(|(n, t)| {
            json!({
                "n": n,
                "id": t.id,
                "codec": t.codec,
                "codec_id": t.codec_id,
                "channels": t.channels,
                "sample_rate": t.sample_rate,
                "bits": t.bit_depth,
                "lang": t.language,
                "title": t.name,
                "default": t.default,
                "rust": RUST_AUDIO_CODECS.contains(&t.codec.as_str()),
            })
        })
        .collect();
    let subtitles: Vec<_> = info
        .tracks
        .iter()
        .filter(|t| matches!(t.kind, TrackKind::Subtitle))
        .map(|t| json!({ "id": t.id, "codec": t.codec, "lang": t.language, "title": t.name }))
        .collect();
    json!({
        "container": info.container,
        "duration": info.duration_secs,
        "bit_rate": info.duration_secs.filter(|d| *d > 0.0).map(|d| ((file_len as f64 * 8.0) / d) as u64),
        "video": video.map(|v| json!({
            "codec": v.codec,
            "width": v.width,
            "height": v.height,
            "bits": v.bit_depth,
            "hdr": v.hdr,
            "dv_profile": v.dv_profile,
            "dv_level": v.dv_level,
            "dv_compat": v.dv_bl_compat,
        })),
        "audio": audio,
        "subtitles": subtitles,
    })
}

/// `info_json` çıktısındaki her ses izine `"atmos": "yes"|"no"|"unknown"` ekler (iz `id`'sine göre).
/// Haritada olmayan iz `unknown` sayılır. Mevcut alanlara dokunmaz (arayüz uyumu).
pub fn merge_atmos(info: &mut serde_json::Value, atmos: &HashMap<u64, AtmosState>) {
    let Some(audio) = info.get_mut("audio").and_then(|a| a.as_array_mut()) else { return };
    for t in audio {
        let state = t.get("id").and_then(|i| i.as_u64()).and_then(|id| atmos.get(&id)).copied().unwrap_or(AtmosState::Unknown);
        if let Some(o) = t.as_object_mut() {
            o.insert("atmos".into(), serde_json::to_value(state).unwrap_or(json!("unknown")));
        }
    }
}

pub fn probe<R: Read + Seek>(reader: R) -> anyhow::Result<MediaInfo> {
    demux::probe(reader)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_atmos_adds_state_per_audio_track() {
        let mut v = json!({"container":"mkv","audio":[{"n":0,"id":2,"codec":"eac3"},{"n":1,"id":3,"codec":"truehd"},{"n":2,"id":4,"codec":"aac"}]});
        let m = HashMap::from([(2u64, AtmosState::Yes), (3, AtmosState::No)]);
        merge_atmos(&mut v, &m);
        assert_eq!(v["audio"][0]["atmos"], "yes");
        assert_eq!(v["audio"][1]["atmos"], "no");
        assert_eq!(v["audio"][2]["atmos"], "unknown");
        assert_eq!(v["audio"][0]["codec"], "eac3");
        assert_eq!(v["container"], "mkv");
    }

    #[test]
    fn wav_header_is_extensible_24bit() {
        let h = wav_header(AudioFormat { sample_rate: 48_000, channels: 6, channel_mask: 0x3F });
        assert_eq!(h.len(), 68);
        assert_eq!(&h[20..22], &0xFFFEu16.to_le_bytes());
        assert_eq!(u16::from_le_bytes([h[22], h[23]]), 6);
        assert_eq!(u16::from_le_bytes([h[34], h[35]]), 24);
        assert_eq!(u32::from_le_bytes([h[40], h[41], h[42], h[43]]), 0x3F);
    }

    #[test]
    fn s24_roundtrip_is_exact_for_24bit_values() {
        let ints = [-8_388_608i32, -1, 0, 1, 1234567, 8_388_607];
        let f: Vec<f32> = ints.iter().map(|&i| i as f32 / 8_388_608.0).collect();
        let mut out = Vec::new();
        push_s24(&mut out, &f);
        for (k, &i) in ints.iter().enumerate() {
            let b = &out[k * 3..k * 3 + 3];
            let v = i32::from_le_bytes([b[0], b[1], b[2], if b[2] & 0x80 != 0 { 0xFF } else { 0 }]);
            assert_eq!(v, i);
        }
    }
}
