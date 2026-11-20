//! HLS segment üretimi: video baytları değiştirilmeden (kopya) fMP4'e sarılır, DV P7 iPhone'da
//! çalmadığı için gerekirse DV katman NAL'ları (62/63) ayıklanır; ses kopyalanır ya da çözülüp
//! FLAC'a kodlanır; altyazı WebVTT olur. HTTP burada yok (Task 12).

use std::io::{Read, Seek};

use anyhow::{Result, anyhow, bail};

use crate::audio::{self, PcmDecoder};
use crate::demux::{Packet, PacketReader, TrackInfo};
use crate::hls::flac::{self, FlacEncoder};
use crate::hls::fmp4::{self, DoviBox, Entry, Sample};
use crate::hls::{HLS_TIME_OFFSET_SECS, sps};
use crate::support::HlsPlan;

/// Bir HLS segmentinin zaman aralığı (saniye) ve sırası.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub index: u32,
    pub start: f64,
    pub end: f64,
}

/// Tüm HLS izleri tek izli fMP4'tür.
const TRACK_ID: u32 = 1;
/// Video zaman ölçeği.
const VIDEO_TS: u32 = 90_000;
/// Süresi bilinmeyen altyazı ipucuna varsayılan süre (sn).
const DEFAULT_CUE_SECS: f64 = 3.0;
/// Altyazı okurken `span.start`tan geriye bakılan en uzun ipucu süresi (sn).
const CUE_LOOKBACK_SECS: f64 = 8.0;

// ---------------------------------------------------------------------------------------------
// Video
// ---------------------------------------------------------------------------------------------

/// Yeniden sıralama gecikmesine eklenen pay (2 ms): Matroska ms'ye yuvarlanmış pts'lerinden gelen
/// sapma `cto < 0` üretmesin.
pub const REORDER_MARGIN_TICKS: u64 = 180;

/// Bir kare süresi (90 kHz tik): izin varsayılan süresi, yoksa 24 fps.
fn frame_ticks(track: &TrackInfo) -> u64 {
    track
        .default_duration_ns
        .filter(|d| *d > 0)
        .map(|d| (d as u128 * VIDEO_TS as u128 + 500_000_000) as u64 / 1_000_000_000)
        .filter(|t| *t > 0)
        .unwrap_or(VIDEO_TS as u64 / 24)
}

/// İz için TEK sabit yeniden sıralama gecikmesi `D` (90 kHz tik); her segmentte aynıdır, böylece
/// `dts = sıralı_pts - D` segmentler arasında kesintisiz kalır (Task 11/12 de kullanabilir).
/// Kaynak: HEVC `sps_max_num_reorder_pics[en yüksek tid]` / AVC VUI `max_num_reorder_frames`
/// (kare sayısı x kare süresi); SPS'te yoksa 4 kare. Üzerine [`REORDER_MARGIN_TICKS`] eklenir.
pub fn reorder_delay_ticks(track: &TrackInfo) -> u64 {
    let ft = frame_ticks(track);
    let frames = sps::max_num_reorder(&track.codec, &track.codec_private).map_or(4, |n| n as u64);
    frames * ft + REORDER_MARGIN_TICKS
}

/// `HLS_TIME_OFFSET_SECS` 90 kHz tik cinsinden.
fn offset_ticks() -> i64 {
    (HLS_TIME_OFFSET_SECS * VIDEO_TS as f64).round() as i64
}

/// `HLS_TIME_OFFSET_SECS` örnek cinsinden.
fn offset_samples(rate: u32) -> u64 {
    (HLS_TIME_OFFSET_SECS * rate as f64).round() as u64
}

/// NAL uzunluk öneki boyutu: avcC/hvcC `lengthSizeMinusOne` (MKV blokları bu boyutla önekli).
fn nal_len_size(track: &TrackInfo) -> usize {
    let cp = &track.codec_private;
    match track.codec.as_str() {
        "h264" if cp.len() > 4 => (cp[4] & 3) as usize + 1,
        "hevc" if cp.len() > 21 => (cp[21] & 3) as usize + 1,
        _ => 4,
    }
}

pub fn video_init(track: &TrackInfo, plan: HlsPlan) -> Result<Vec<u8>> {
    if track.codec_private.is_empty() {
        bail!("video izinde codec yapılandırması (avcC/hvcC) yok");
    }
    let w = track.width.ok_or_else(|| anyhow!("video genişliği yok"))? as u16;
    let h = track.height.ok_or_else(|| anyhow!("video yüksekliği yok"))? as u16;
    let entry = match track.codec.as_str() {
        "h264" => Entry::Avc { avcc: track.codec_private.clone(), w, h, colr: track.colour },
        "hevc" => {
            // P7 + ayıklama: DV kutusu yazılmaz, HDR10 taban katmanı `hvc1` olarak kalır.
            // DV kutusu yalnızca profil 5, ya da bl_compat'ı bilinen (1/2/4) profil 8 için yazılır
            // (oynatma listesindeki codec dizesiyle tutarlı); diğerleri düz `hvc1`.
            let dv_known = matches!(
                (track.dv_profile, track.dv_bl_compat),
                (Some(5), _) | (Some(8), Some(1 | 2 | 4))
            );
            let dv = match (plan, &track.dv_config, track.dv_config_fourcc) {
                (HlsPlan::StripDvEnhancement, ..) => None,
                (_, Some(payload), Some(fourcc)) if dv_known => Some(DoviBox { fourcc, payload: payload.clone() }),
                _ => None,
            };
            let dvh1 = dv.is_some() && track.dv_profile == Some(5);
            // `colr nclx` (hvcC'den sonra, DV kutusundan önce): HDR10/HLG için iPhone'un SDR gibi
            // çizmemesi gerekir. DV profil 5 (`dvh1`, geriye uyum yok) için yazılmaz: renk
            // uzayı DV RPU'dan gelir ve `colr` yanıltıcı olurdu (demux DV izlerinde zaten
            // `colour` doldurmaz).
            let colr = if dvh1 { None } else { track.colour };
            Entry::Hevc { hvcc: track.codec_private.clone(), w, h, dv, dvh1, colr }
        }
        other => bail!("HLS için desteklenmeyen video kodeği: {other}"),
    };
    Ok(fmp4::init_segment(TRACK_ID, VIDEO_TS, &entry))
}

/// Uzunluk önekli HEVC örneğinden Dolby Vision katman NAL'larını (tür 62 RPU, 63 EL) atar.
fn strip_dv_nals(data: &[u8], len_size: usize) -> Result<Vec<u8>> {
    if !(1..=4).contains(&len_size) {
        bail!("geçersiz NAL uzunluk boyutu: {len_size}");
    }
    let mut out = Vec::with_capacity(data.len());
    let mut pos = 0usize;
    while pos < data.len() {
        let hdr = data.get(pos..pos + len_size).ok_or_else(|| anyhow!("kesik NAL uzunluk öneki"))?;
        let n = hdr.iter().fold(0usize, |a, &b| (a << 8) | b as usize);
        let end = pos + len_size + n;
        if end > data.len() {
            bail!("NAL uzunluğu örnek sınırını aşıyor ({n} bayt @{pos})");
        }
        // tür = (ilk bayt >> 1) & 0x3F; boş NAL'lar olduğu gibi kalır
        let drop = n > 0 && matches!((data[pos + len_size] >> 1) & 0x3F, 62 | 63);
        if !drop {
            out.extend_from_slice(&data[pos..end]);
        }
        pos = end;
    }
    Ok(out)
}

/// Segmentin paketleri (kod çözme sırasında) ve bir sonraki segmentin anahtar karesinin pts'i
/// (varsa): `span.start` anahtar karesinden başlar, pts >= `span.end` olan sonraki anahtar kareye
/// ya da akış sonuna kadar sürer. Açık GOP öncül resimleri (anahtar karenin ardından kodlanan ama
/// pts'i anahtar karenin altında kalanlar) atılır: segment başında `sıralı_pts[0] == anahtar pts`
/// olur ve önceki segmentle zaman çakışması çıkmaz. KABUL EDİLEN SINIRLAMA: açık GOP'lu içerikte bu
/// öncül resimler hiçbir segmentte yer almaz (anahtar kare başına birkaç karelik görüntü boşluğu).
fn read_video_packets<R: Read + Seek>(r: R, track: &TrackInfo, span: &Span) -> Result<(Vec<Packet>, Option<f64>)> {
    // +1 ms: `span.start` kayan nokta gürültüsüyle anahtar karenin birkaç ns altında kalsa bile
    // bir önceki anahtar kareye düşülmesin.
    let mut rd = PacketReader::open(r, track.id, span.start + 1e-3)?;
    let mut out: Vec<Packet> = Vec::new();
    let mut next_key = None;
    while let Some(p) = rd.next()? {
        if out.is_empty() {
            if !p.keyframe {
                continue;
            }
        } else if p.keyframe && p.pts_secs >= span.end - 1e-4 {
            next_key = Some(p.pts_secs);
            break;
        } else if p.pts_secs < out[0].pts_secs - 1e-6 {
            continue; // açık GOP öncül resmi
        }
        out.push(p);
    }
    if out.is_empty() {
        bail!("video segmenti boş ({:.3}..{:.3} sn)", span.start, span.end);
    }
    Ok((out, next_key))
}

/// Video segmenti. Zamanlama (Matroska'da yalnızca pts var):
/// * pts'ler 90 kHz tike yuvarlanır ve sıralanır; `dts[i] = sıralı_pts[i] + KAYMA - D`, burada `D` iz
///   başına SABİT yeniden sıralama gecikmesi ([`reorder_delay_ticks`]) ve `KAYMA` =
///   [`HLS_TIME_OFFSET_SECS`] (dts asla negatif olmaz; kırpma yok). `cto = pts - sıralı_pts + D`;
///   herhangi biri < 0 ise hata (D yetersiz).
/// * Son örnek süresi, bir sonraki segmentin anahtar karesine (yoksa `span.end`) kadar:
///   `sonraki_anahtar_pts - son_sıralı_pts`; böylece `tfdt[n+1] == tfdt[n] + Σ süre` TAM eşitlik.
pub fn video_segment<R: Read + Seek>(r: R, track: &TrackInfo, plan: HlsPlan, span: &Span) -> Result<Vec<u8>> {
    let (pkts, next_key) = read_video_packets(r, track, span)?;
    let n = pkts.len();
    let strip = plan == HlsPlan::StripDvEnhancement && track.codec == "hevc";
    let len_size = nal_len_size(track);

    let d = reorder_delay_ticks(track) as i64;
    let off = offset_ticks();
    let ticks: Vec<i64> = pkts.iter().map(|p| (p.pts_secs * VIDEO_TS as f64).round() as i64).collect();
    let mut sorted = ticks.clone();
    sorted.sort_unstable();
    let mut dts: Vec<i64> = Vec::with_capacity(n);
    for (i, &s) in sorted.iter().enumerate() {
        let raw = s + off - d;
        if raw < 0 {
            bail!("dts negatif: yeniden sıralama gecikmesi kayma payından büyük");
        }
        dts.push(if i == 0 { raw } else { raw.max(dts[i - 1] + 1) });
    }
    let mut ctos: Vec<i32> = Vec::with_capacity(n);
    for i in 0..n {
        let c = ticks[i] + off - dts[i];
        if c < 0 {
            bail!("negatif kompozisyon ofseti ({c} tik): yeniden sıralama gecikmesi ({d}) yetersiz");
        }
        ctos.push(c as i32);
    }
    let end_ticks = next_key.map_or((span.end * VIDEO_TS as f64).round() as i64, |k| (k * VIDEO_TS as f64).round() as i64);
    let mut durs: Vec<u32> = (0..n.saturating_sub(1)).map(|i| (dts[i + 1] - dts[i]) as u32).collect();
    let last = end_ticks - sorted[n - 1];
    let last_dur = if last > 0 { last as u32 } else { durs.last().copied().unwrap_or(frame_ticks(track) as u32) };
    durs.push(last_dur);

    let mut datas: Vec<std::borrow::Cow<[u8]>> = Vec::with_capacity(n);
    for p in pkts.iter() {
        datas.push(if strip {
            std::borrow::Cow::Owned(strip_dv_nals(&p.data, len_size)?)
        } else {
            std::borrow::Cow::Borrowed(&p.data[..])
        });
    }
    let samples: Vec<Sample> =
        (0..n).map(|i| Sample { data: &datas[i][..], duration: durs[i], cto: ctos[i], sync: pkts[i].keyframe }).collect();
    Ok(fmp4::media_segment(span.index + 1, TRACK_ID, dts[0] as u64, &samples))
}

// ---------------------------------------------------------------------------------------------
// Ses
// ---------------------------------------------------------------------------------------------

/// FLAC'a çevirilecek akışın (örnekleme hızı, kanal): ilk çerçeveyi çözmeyi dener (çözücünün
/// gerçek çıktı biçimi), olmazsa (ör. TrueHD ilk AU'da ana senkron yok) iz bilgisine düşer.
fn flac_format(track: &TrackInfo, first_frame: &[u8]) -> Result<(u32, u16)> {
    let decoded = audio::decoder_for(&track.codec).ok().and_then(|mut d| {
        let mut pcm = Vec::new();
        d.decode(first_frame, &mut pcm).ok()?;
        d.format().filter(|_| !pcm.is_empty())
    });
    match decoded {
        Some(f) => {
            if let Some(c) = track.channels
                && c as u16 != f.channels
            {
                tracing::warn!("çözülen kanal sayısı ({}) kapsayıcıdakinden ({c}) farklı; çözücüye güvenilir", f.channels);
            }
            Ok((f.sample_rate, f.channels))
        }
        None => Ok((
            track.sample_rate.ok_or_else(|| anyhow!("ses izinde örnekleme hızı yok"))?,
            track.channels.ok_or_else(|| anyhow!("ses izinde kanal sayısı yok"))? as u16,
        )),
    }
}

/// MKV `fLaC` codec_private: ya `fLaC` + meta bloklar ya da yalnızca 34 baytlık STREAMINFO.
fn flac_streaminfo_from_private(cp: &[u8]) -> Option<Vec<u8>> {
    if cp.len() >= 42 && &cp[..4] == b"fLaC" {
        // ilk meta blok STREAMINFO (tür 0, 34 bayt)
        return Some(cp[8..42].to_vec());
    }
    (cp.len() == 34).then(|| cp.to_vec())
}

/// STREAMINFO'daki örnek başına bit sayısı: bayt 12'nin son biti + bayt 13'ün üst 4 biti, artı 1.
fn flac_bits_from_streaminfo(si: &[u8]) -> Option<u16> {
    if si.len() < 14 {
        return None;
    }
    Some((((si[12] & 1) as u16) << 4 | (si[13] >> 4) as u16) + 1)
}

pub fn audio_init(track: &TrackInfo, plan: HlsPlan, first_frame: &[u8]) -> Result<Vec<u8>> {
    if plan == HlsPlan::DecodeToFlac {
        let (rate, ch) = flac_format(track, first_frame)?;
        let enc = FlacEncoder::new(rate, ch)?;
        let entry = Entry::Flac { streaminfo: enc.streaminfo(), channels: ch, rate, bits: flac::BITS };
        return Ok(fmp4::init_segment(TRACK_ID, rate, &entry));
    }
    let rate = track.sample_rate.ok_or_else(|| anyhow!("ses izinde örnekleme hızı yok"))?;
    let ch = track.channels.ok_or_else(|| anyhow!("ses izinde kanal sayısı yok"))? as u16;
    let entry = match track.codec.as_str() {
        "ac3" => Entry::Ac3 { dac3: fmp4::build_dac3(first_frame)?, channels: ch, rate },
        "eac3" => Entry::Eac3 { dec3: fmp4::build_dec3(first_frame)?, channels: ch, rate },
        "aac" => Entry::Aac { asc: track.codec_private.clone(), channels: ch, rate },
        "opus" => Entry::Opus { dops: fmp4::opus_dops_from_head(&track.codec_private)?, channels: ch, rate: 48_000 },
        "flac" => Entry::Flac {
            streaminfo: flac_streaminfo_from_private(&track.codec_private)
                .ok_or_else(|| anyhow!("FLAC STREAMINFO bulunamadı"))?,
            channels: ch,
            rate,
            // Bit derinliği STREAMINFO'dan (kapsayıcı varsayılanı değil).
            bits: flac_bits_from_streaminfo(&flac_streaminfo_from_private(&track.codec_private).unwrap_or_default())
                .or(track.bit_depth.map(|b| b as u16))
                .unwrap_or(16),
        },
        other => bail!("HLS'te kopyalanamayan ses kodeği: {other} (FLAC'a çevrilmeli)"),
    };
    let timescale = if track.codec == "opus" { 48_000 } else { rate };
    Ok(fmp4::init_segment(TRACK_ID, timescale, &entry))
}

/// Bir ses paketinin örnek sayısı, bit akışından (çerçeve başlığı): AC-3 1536; E-AC-3
/// `numblks x 256`; AAC 1024; Opus TOC (48 kHz); FLAC başlıktaki blok boyutu. Çözülemezse
/// `fallback` (pts farkından türetilen) kullanılır.
fn frame_samples(codec: &str, d: &[u8], fallback: u32) -> u32 {
    let v = match codec {
        "ac3" => Some(1536),
        "eac3" => eac3_samples(d),
        "aac" => Some(1024),
        "opus" => opus_samples(d),
        "flac" => flac_block_size(d),
        _ => None,
    };
    v.filter(|&n| n > 0).unwrap_or(fallback)
}

fn eac3_samples(d: &[u8]) -> Option<u32> {
    if d.len() < 5 || d[0] != 0x0B || d[1] != 0x77 {
        return None;
    }
    let fscod = d[4] >> 6;
    let numblks = if fscod == 3 { 6 } else { [1, 2, 3, 6][((d[4] >> 4) & 3) as usize] };
    Some(256 * numblks)
}

fn opus_samples(d: &[u8]) -> Option<u32> {
    let toc = *d.first()?;
    let cfg = (toc >> 3) as usize;
    let per_frame = match cfg {
        0..=11 => [480, 960, 1920, 2880][cfg % 4],
        12..=15 => [480, 960][cfg % 2],
        _ => [120, 240, 480, 960][cfg % 4],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => (*d.get(1)? & 0x3F) as u32,
    };
    Some(per_frame * frames)
}

fn flac_block_size(d: &[u8]) -> Option<u32> {
    if d.len() < 6 || d[0] != 0xFF || d[1] & 0xFE != 0xF8 {
        return None;
    }
    match d[2] >> 4 {
        1 => Some(192),
        c @ 2..=5 => Some(576 << (c - 2)),
        c @ (6 | 7) => {
            // UTF-8 kodlu çerçeve/örnek numarasından sonra 8 ya da 16 bit (blok-1)
            let first = d[4];
            let utf = if first < 0x80 { 1 } else { first.leading_ones() as usize };
            let p = 4 + utf;
            if c == 6 {
                Some(*d.get(p)? as u32 + 1)
            } else {
                Some(u16::from_be_bytes([*d.get(p)?, *d.get(p + 1)?]) as u32 + 1)
            }
        }
        c => Some(256 << (c - 8)),
    }
}

pub fn audio_segment<R: Read + Seek>(r: R, track: &TrackInfo, plan: HlsPlan, span: &Span) -> Result<Vec<u8>> {
    if plan == HlsPlan::DecodeToFlac {
        let mut dec = audio::decoder_for(&track.codec)?;
        return flac_segment(r, track, span, dec.as_mut());
    }
    let rate = if track.codec == "opus" {
        48_000
    } else {
        track.sample_rate.ok_or_else(|| anyhow!("ses izinde örnekleme hızı yok"))?
    };
    let mut rd = PacketReader::open(r, track.id, span.start)?;
    let mut pkts: Vec<Packet> = Vec::new();
    while let Some(p) = rd.next()? {
        if p.pts_secs < span.start - 1e-6 {
            continue;
        }
        if p.pts_secs >= span.end - 1e-6 {
            break;
        }
        pkts.push(p);
    }
    if pkts.is_empty() {
        bail!("ses segmenti boş ({:.3}..{:.3} sn)", span.start, span.end);
    }
    let samples: Vec<Sample> = pkts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            // yedek süre: bir sonraki paketle pts farkı (son paket için span sonu)
            let next = pkts.get(i + 1).map(|q| q.pts_secs).unwrap_or(span.end);
            let fb = ((next - p.pts_secs).max(0.0) * rate as f64).round() as u32;
            Sample { data: &p.data, duration: frame_samples(&track.codec, &p.data, fb), cto: 0, sync: true }
        })
        .collect();
    // Tam kümülatif örnek indeksi: sabit çerçeve boyutlu kodeklerde (AC-3/E-AC-3/AAC) ilk pts en
    // yakın çerçeve ızgarasına oturtulur (Matroska ms yuvarlaması çerçeve süresinden küçüktür);
    // böylece bir sonraki segmentin tabanı = bu taban + Σ süre TAM eşit olur. Akışın gerçek
    // başlangıç kayması varsa en fazla yarım çerçeve (<= 16 ms) tutarlı biçimde kayar.
    let fr = samples[0].duration as f64;
    let pos = pkts[0].pts_secs * rate as f64;
    let base = if matches!(track.codec.as_str(), "ac3" | "eac3" | "aac") && fr > 0.0 {
        ((pos / fr).round() * fr) as u64
    } else {
        pos.round() as u64
    } + offset_samples(rate);
    Ok(fmp4::media_segment(span.index + 1, TRACK_ID, base, &samples))
}

/// Segmentin örnek/kanal sayısı: `round(end*hız) - round(start*hız)` (ardışık segmentler boşluksuz).
fn frames_in(span: &Span, rate: u32) -> usize {
    let r = rate as f64;
    ((span.end * r).round() - (span.start * r).round()).max(0.0) as usize
}

/// TrueHD/DTS/AC-3 -> PCM -> 24-bit FLAC çerçeveleri; segment tam olarak
/// `[round(start*hız), round(end*hız))` örnek aralığını kapsar. Başlangıç hizalaması
/// [`audio::stream_wav`] ile ortaktır (`audio::start_alignment`): erken örnekler atılır, boşluk
/// sessizlikle dolar; akış erken biterse son kısım sessizlikle tamamlanır.
/// Konum uydurması için okunan ilk paket sayısı.
const FIT_PACKETS: usize = 16;
/// Matroska zaman damgası kesinliği: pts, gerçek değerin en çok ±0,5 ms yuvarlanmışıdır.
const MKV_TS_HALF_MS: f64 = 0.5e-3;

/// Çoklu paket uydurması: `pts[i] = ms_yuvarla(t0 + (i - j) * AU / hız)` (j = ilk çözülen paketin
/// indeksi, AU = çözücünün paket başına çıktı uzunluğu). Her paket için t0 aralığı
/// `[pts_i - 0,5 ms - (i-j)*AU/hız, pts_i + 0,5 ms - (i-j)*AU/hız]` kesiştirilir. Kesişim boş ise
/// (akışta boşluk/atlama) `None`. Aksi halde t0, AU ızgarasına (`m * AU` örnek, akış örnek 0'dan
/// başlar) oturtulur; ızgara noktası kesişimin içindeyse o, değilse (ızgara dışı başlangıç kayması)
/// kesişim orta noktası kullanılır. Dönüş: ilk çözülen paketin saniye cinsinden konumu.
fn fit_stream_position(pts: &[f64], j: usize, au: usize, rate: u32) -> Option<f64> {
    if pts.len() < 2 || au == 0 {
        return None;
    }
    let r = rate as f64;
    let step = au as f64 / r;
    let (mut lo, mut hi) = (f64::NEG_INFINITY, f64::INFINITY);
    for (i, &p) in pts.iter().enumerate() {
        let b = p - (i as f64 - j as f64) * step;
        lo = lo.max(b - MKV_TS_HALF_MS - 1e-9);
        hi = hi.min(b + MKV_TS_HALF_MS + 1e-9);
    }
    if lo > hi {
        return None;
    }
    let mid = (lo + hi) / 2.0;
    let on_grid = (mid * r / au as f64).round() * au as f64 / r;
    Some(if (lo..=hi).contains(&on_grid) { on_grid } else { mid })
}

fn flac_segment<R: Read + Seek>(r: R, track: &TrackInfo, span: &Span, dec: &mut dyn PcmDecoder) -> Result<Vec<u8>> {
    let mut rd = PacketReader::open(r, track.id, span.start)?;
    // İlk paketler önceden okunur (konum uydurması için); sonra akış kaldığı yerden sürer.
    let mut pending: std::collections::VecDeque<Packet> = std::collections::VecDeque::new();
    while pending.len() < FIT_PACKETS {
        match rd.next()? {
            Some(p) => pending.push_back(p),
            None => break,
        }
    }
    let head_pts: Vec<f64> = pending.iter().map(|p| p.pts_secs).collect();
    let mut idx = 0usize; // işlenen paket indeksi (head_pts ile aynı sıra)
    let mut pcm: Vec<f32> = Vec::with_capacity(16 * 1024);
    let mut out: Vec<i32> = Vec::new();
    let mut fmt: Option<audio::AudioFormat> = None;
    let mut need = usize::MAX;
    let mut errors = 0u32;
    let mut skip_left = 0usize;
    loop {
        let pkt = match pending.pop_front() {
            Some(p) => p,
            None => match rd.next()? {
                Some(p) => p,
                None => break,
            },
        };
        let pkt_idx = idx;
        idx += 1;
        pcm.clear();
        if let Err(e) = dec.decode(&pkt.data, &mut pcm) {
            errors += 1;
            tracing::debug!("ses paketi çözülemedi ({errors}): {e:#}");
            if errors > 200 {
                bail!("ses çözülemiyor: {e:#}");
            }
            continue;
        }
        let Some(f) = dec.format() else { continue };
        if pcm.is_empty() {
            continue;
        }
        let slice: &[f32] = &pcm;
        match fmt {
            None => {
                // Kanal sayısı çözücüden gelir (init ile aynı kaynak); kapsayıcıyla farkı yalnızca uyarı.
                if let Some(c) = track.channels
                    && c as u16 != f.channels
                {
                    tracing::warn!("çözücü kanal sayısı ({}) kapsayıcıdakinden ({c}) farklı; çözücüye güvenilir", f.channels);
                }
                fmt = Some(f);
                need = frames_in(span, f.sample_rate) * f.channels as usize;
                // İlk çözülen paketin konumu: Matroska pts'i ms'ye yuvarlıdır, örnek ızgarası kesin.
                // İlk ~16 paketin pts'inden çoklu paket uydurmasıyla (bkz. `fit_stream_position`)
                // konum tek bir ızgara noktasına sabitlenir; böylece 40 örneklik TrueHD AU'larında
                // bile ardışık segmentler örnek-kesin birleşir. Uydurma başarısızsa (boşluk) eski
                // davranış: >= 512 örnekte tek paketle ızgaraya oturt, değilse ham pts.
                let rate = f.sample_rate as f64;
                let flen = slice.len() / f.channels as usize;
                let mut offset_secs = pkt.pts_secs - span.start;
                match fit_stream_position(&head_pts, pkt_idx, flen, f.sample_rate) {
                    Some(t0) if pkt_idx < head_pts.len() => {
                        offset_secs = ((t0 * rate).round() - (span.start * rate).round()) / rate;
                    }
                    fit => {
                        if fit.is_none() {
                            tracing::debug!("ses konum uydurması başarısız (pts boşluğu?); tek paket yedeği");
                        }
                        if flen >= 512 {
                            let snapped = (pkt.pts_secs * rate / flen as f64).round() * flen as f64;
                            offset_secs = (snapped - (span.start * rate).round()) / rate;
                        }
                    }
                }
                // `skip` bir paketten uzun olabilir (ms yuvarlaması yüzünden başlangıçtan önce biten bir
                // paket okunmuş olabilir): kalan atlama sonraki paketlere taşınır.
                let (skip, pad) = audio::start_alignment(offset_secs, f, usize::MAX);
                out.resize(pad.min(need), 0);
                skip_left = skip;
            }
            Some(g) if g.channels != f.channels => {
                bail!("ses kanal sayısı akış ortasında değişti: {} -> {}", g.channels, f.channels);
            }
            Some(g) if g != f => {
                tracing::warn!("ses biçimi akış ortasında değişti: {g:?} -> {f:?}");
                continue;
            }
            Some(_) => {}
        }
        let d = skip_left.min(slice.len());
        skip_left -= d;
        out.extend(slice[d..].iter().map(|&s| audio::f32_to_s24(s)));
        if out.len() >= need {
            break;
        }
    }
    let (rate, ch) = match fmt {
        Some(f) => (f.sample_rate, f.channels),
        None => (
            track.sample_rate.ok_or_else(|| anyhow!("ses izinde örnekleme hızı yok"))?,
            track.channels.ok_or_else(|| anyhow!("ses izinde kanal sayısı yok"))? as u16,
        ),
    };
    let total = frames_in(span, rate);
    if total == 0 {
        bail!("ses segmenti boş ({:.3}..{:.3} sn)", span.start, span.end);
    }
    out.resize(total * ch as usize, 0);
    let enc = FlacEncoder::new(rate, ch)?;
    let start_sample = (span.start * rate as f64).round() as u64;
    let frames: Vec<(Vec<u8>, u32)> = out
        .chunks(flac::BLOCK_SIZE * ch as usize)
        .enumerate()
        .map(|(k, c)| Ok((enc.encode_frame(c, k as u32)?, (c.len() / ch as usize) as u32)))
        .collect::<Result<_>>()?;
    let samples: Vec<Sample> = frames.iter().map(|(d, n)| Sample { data: d, duration: *n, cto: 0, sync: true }).collect();
    Ok(fmp4::media_segment(span.index + 1, TRACK_ID, start_sample + offset_samples(rate), &samples))
}

// ---------------------------------------------------------------------------------------------
// WebVTT
// ---------------------------------------------------------------------------------------------

/// `LOCAL:00:00:00.000` (ipucu zamanı 0) = `MPEGTS:<kayma x 90000>`: ipuçları medya zamanında
/// kalır, fMP4 zaman damgalarındaki [`HLS_TIME_OFFSET_SECS`] kaymasıyla hizalanır.
fn vtt_header() -> String {
    format!("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:{},LOCAL:00:00:00.000\n", offset_ticks())
}

fn vtt_time(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// Gömülü (MKV) SRT/ASS izinden `span` ile kesişen ipuçları. Zamanlar mutlaktır; segmentler
/// `X-TIMESTAMP-MAP=MPEGTS:<kayma x 90000>,LOCAL:00:00:00.000` ile fMP4 zaman çizgisine eşlenir. Segment sınırını
/// aşan ipuçları her iki segmentte de yer alır (WebVTT HLS kuralı).
pub fn vtt_segment<R: Read + Seek>(r: R, track: &TrackInfo, span: &Span) -> Result<String> {
    if !matches!(track.codec.as_str(), "srt" | "ass") {
        bail!("metin tabanlı olmayan altyazı WebVTT'ye çevrilemez: {}", track.codec);
    }
    // Segment başından önce başlayıp içeri uzanan ipuçları için geriye bak: Cues'tan sonraki
    // kümeye atlayan okuyucu aksi halde bunları kaçırır.
    let mut rd = PacketReader::open(r, track.id, (span.start - CUE_LOOKBACK_SECS).max(0.0))?;
    let mut out = format!("{}\n", vtt_header());
    while let Some(p) = rd.next()? {
        if p.pts_secs >= span.end {
            break;
        }
        let end = p.pts_secs + p.duration_secs.unwrap_or(DEFAULT_CUE_SECS);
        if end <= span.start {
            continue;
        }
        let raw = crate::media::decode_text(&p.data).replace("\r\n", "\n").replace('\r', "\n");
        let text = if track.codec == "ass" {
            // Matroska ASS bloğu: ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Metin
            match raw.splitn(9, ',').nth(8) {
                Some(t) => crate::media::clean_ass_text(t),
                None => continue,
            }
        } else {
            raw
        };
        let body: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).collect();
        if body.is_empty() {
            continue;
        }
        out.push_str(&format!("{} --> {}\n{}\n\n", vtt_time(p.pts_secs), vtt_time(end), body.join("\n")));
    }
    Ok(out)
}

/// Harici dosya altyazısı (tek segment): [`crate::media::to_vtt`] + zaman eşleme üstbilgisi.
pub fn external_vtt(name: &str, bytes: &[u8]) -> String {
    let vtt = crate::media::to_vtt(name, bytes);
    match vtt.strip_prefix("WEBVTT") {
        Some(rest) => {
            // "WEBVTT" satırının geri kalanını (varsa başlık metni) ve ilk boş satırı atla
            let body = rest.split_once('\n').map_or("", |x| x.1);
            format!("{}\n{}", vtt_header(), body.trim_start_matches('\n'))
        }
        None => format!("{}\n{vtt}", vtt_header()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ac3dec::AudioFormat;
    use crate::demux::{self, TrackKind};
    use filmcraft_matroska as mkv;
    use std::io::Cursor;

    // ---------- fMP4 ayrıştırma yardımcıları ----------

    fn boxes(d: &[u8]) -> Vec<([u8; 4], &[u8])> {
        let mut v = Vec::new();
        let mut o = 0;
        while o + 8 <= d.len() {
            let sz = u32::from_be_bytes(d[o..o + 4].try_into().unwrap()) as usize;
            assert!(sz >= 8 && o + sz <= d.len(), "bozuk kutu @{o}");
            v.push((d[o + 4..o + 8].try_into().unwrap(), &d[o + 8..o + sz]));
            o += sz;
        }
        assert_eq!(o, d.len());
        v
    }
    fn child<'a>(d: &'a [u8], k: &[u8; 4]) -> &'a [u8] {
        boxes(d).into_iter().find(|(f, _)| f == k).unwrap_or_else(|| panic!("kutu yok {k:?}")).1
    }

    struct Seg {
        base: u64,
        /// (süre, boyut, bayraklar, cto)
        samples: Vec<(u32, u32, u32, i32)>,
        mdat: Vec<u8>,
    }
    impl Seg {
        fn parse(d: &[u8]) -> Seg {
            let moof = child(d, b"moof");
            let traf = child(moof, b"traf");
            let tfdt = child(traf, b"tfdt");
            assert_eq!(tfdt[0], 1);
            let base = u64::from_be_bytes(tfdt[4..12].try_into().unwrap());
            let trun = child(traf, b"trun");
            let n = u32::from_be_bytes(trun[4..8].try_into().unwrap()) as usize;
            let mut samples = Vec::new();
            for i in 0..n {
                let e = &trun[12 + i * 16..12 + (i + 1) * 16];
                let g = |k: usize| u32::from_be_bytes(e[k * 4..k * 4 + 4].try_into().unwrap());
                samples.push((g(0), g(1), g(2), g(3) as i32));
            }
            Seg { base, samples, mdat: child(d, b"mdat").to_vec() }
        }
        fn sync(&self, i: usize) -> bool {
            self.samples[i].2 & 0x0001_0000 == 0
        }
        fn data(&self, i: usize) -> &[u8] {
            let off: usize = self.samples[..i].iter().map(|s| s.1 as usize).sum();
            &self.mdat[off..off + self.samples[i].1 as usize]
        }
        fn dts(&self) -> Vec<u64> {
            let mut t = self.base;
            self.samples
                .iter()
                .map(|s| {
                    let d = t;
                    t += s.0 as u64;
                    d
                })
                .collect()
        }
        fn end(&self) -> u64 {
            self.base + self.samples.iter().map(|s| s.0 as u64).sum::<u64>()
        }
    }

    // ---------- sentetik MKV'ler ----------

    fn track_of(bytes: &[u8], kind: TrackKind) -> TrackInfo {
        demux::probe(Cursor::new(bytes.to_vec())).unwrap().tracks.into_iter().find(|t| t.kind == kind).unwrap()
    }

    fn video_mkv(codec_id: &str, private: Vec<u8>, frames: &[(i64, bool, Vec<u8>)]) -> Vec<u8> {
        let mut v = mkv::TrackSpec::new(mkv::TrackKind::Video, codec_id);
        v.video_size = Some((64, 64));
        v.default_duration_ns = Some(40_000_000);
        v.codec_private = private;
        let opts = mkv::MuxOptions { cluster_duration_ns: 1_000_000_000, ..Default::default() };
        let mut w = mkv::MkvWriter::new(Cursor::new(Vec::new()), vec![v], opts).unwrap();
        for (pts_ms, key, d) in frames {
            w.write_frame(0, pts_ms * 1_000_000, *key, d, None).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn audio_mkv(frames: usize) -> Vec<u8> {
        audio_mkv_with("A_AC3", frames, 1536)
    }

    /// `frame_len` örneklik çerçeveler; pts Matroska gibi ms'ye yuvarlanır.
    fn audio_mkv_with(codec_id: &str, frames: usize, frame_len: u64) -> Vec<u8> {
        let mut a = mkv::TrackSpec::new(mkv::TrackKind::Audio, codec_id);
        a.audio = Some((48000.0, 2, None));
        a.default_duration_ns = Some(frame_len * 1_000_000_000 / 48_000);
        let opts = mkv::MuxOptions { cluster_duration_ns: 1_000_000_000, ..Default::default() };
        let mut w = mkv::MkvWriter::new(Cursor::new(Vec::new()), vec![a], opts).unwrap();
        for k in 0..frames as u64 {
            let mut d = vec![0xAAu8; 24];
            d[0] = (k >> 8) as u8;
            d[1] = k as u8;
            let ms = (k * frame_len + 24) / 48;
            w.write_frame(0, (ms * 1_000_000) as i64, true, &d, None).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    /// Sabit zaman kayması (tik / örnek).
    const OFF_T: i64 = 900_000;
    const OFF_S: u64 = 480_000;
    /// Yedek yeniden sıralama gecikmesi: 4 kare x 40 ms + 2 ms pay.
    const D: i64 = 14_400 + 180;

    fn hvcc() -> Vec<u8> {
        let mut v = vec![1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x78, 0xF0, 0, 0xFC, 0xFD, 0xF8, 0xF8, 0, 0, 0x0F];
        v.push(0);
        v
    }

    // ---------- testler ----------

    #[test]
    fn video_segment_copies_bytes() {
        let frames: Vec<_> = (0..250u32).map(|i| (i as i64 * 40, i % 25 == 0, vec![i as u8; 30 + (i % 5) as usize])).collect();
        let mkv = video_mkv("V_MPEG4/ISO/AVC", vec![], &frames);
        let tr = track_of(&mkv, TrackKind::Video);
        let span = Span { index: 2, start: 2.0, end: 4.0 };
        let out = video_segment(Cursor::new(mkv), &tr, HlsPlan::Copy, &span).unwrap();
        let seg = Seg::parse(&out);
        assert_eq!(seg.samples.len(), 50);
        assert_eq!(seg.base as i64, 180_000 + OFF_T - D);
        assert!(seg.sync(0));
        assert!((0..50).all(|i| seg.sync(i) == (i % 25 == 0)), "yalnızca anahtar kareler sync");
        for i in 0..50 {
            assert_eq!(seg.data(i), &frames[50 + i].2[..], "örnek {i}");
            assert_eq!(seg.samples[i].0, 3600);
            assert_eq!(seg.samples[i].3 as i64, D, "P-only: cto = D");
        }
    }

    /// 25 kareli GOP: I, sonra (P, B, B) x 8; pts = görüntüleme sırası x 40 ms.
    fn b_frames(gops: usize) -> Vec<(i64, bool, Vec<u8>)> {
        let mut v = Vec::new();
        let mut n = 0u8;
        for g in 0..gops as i64 {
            let d0 = 25 * g;
            let mut push = |disp: i64, key: bool| {
                n = n.wrapping_add(1);
                v.push((disp * 40, key, vec![n; 20]));
            };
            push(d0, true);
            for k in 1..=8 {
                push(d0 + 3 * k, false);
                push(d0 + 3 * k - 2, false);
                push(d0 + 3 * k - 1, false);
            }
        }
        v
    }

    #[test]
    fn video_segment_dts_monotonic_across_segments() {
        let frames = b_frames(4);
        let mkv = video_mkv("V_MPEG4/ISO/AVC", vec![], &frames);
        let tr = track_of(&mkv, TrackKind::Video);
        let mut prev: Option<Seg> = None;
        for g in 0..4u32 {
            let span = Span { index: g, start: g as f64, end: g as f64 + 1.0 };
            let seg = Seg::parse(&video_segment(Cursor::new(mkv.clone()), &tr, HlsPlan::Copy, &span).unwrap());
            assert_eq!(seg.samples.len(), 25);
            let dts = seg.dts();
            assert!(dts.windows(2).all(|w| w[0] < w[1]), "dts artmalı (segment {g}): {dts:?}");
            // dts + cto = gerçek sunum zamanı
            for (i, f) in frames[g as usize * 25..(g as usize + 1) * 25].iter().enumerate() {
                assert_eq!(dts[i] as i64 + seg.samples[i].3 as i64, f.0 * 90 + OFF_T, "segment {g} örnek {i}");
                assert_eq!(seg.data(i), &f.2[..]);
            }
            assert!(seg.samples.iter().all(|s| s.3 >= 0), "cto >= 0 (segment {g})");
            assert_eq!(seg.base as i64, g as i64 * 90_000 + OFF_T - D, "sabit gecikme D");
            if let Some(p) = &prev {
                assert!(seg.base >= p.end(), "segment {g}: {} < {}", seg.base, p.end());
                assert_eq!(seg.base, p.end());
            }
            prev = Some(seg);
        }
    }

    #[test]
    fn p7_strip_removes_nal_62_63() {
        // uzunluk önekli: tür 1 (0x02), tür 62 (0x7C), tür 63 (0x7E)
        let au = [
            &[0, 0, 0, 3, 0x02, 0x01, 0xAA][..],
            &[0, 0, 0, 4, 0x7C, 0x01, 1, 2][..],
            &[0, 0, 0, 3, 0x7E, 0x01, 9][..],
        ]
        .concat();
        assert_eq!(strip_dv_nals(&au, 4).unwrap(), vec![0, 0, 0, 3, 0x02, 0x01, 0xAA]);
        // 2 baytlık uzunluk boyutu
        let au2 = [&[0, 2, 0x02, 0x01][..], &[0, 3, 0x7E, 0x01, 5][..]].concat();
        assert_eq!(strip_dv_nals(&au2, 2).unwrap(), vec![0, 2, 0x02, 0x01]);
        // bozuk uzunluk hata verir
        assert!(strip_dv_nals(&[0, 0, 0, 9, 0x02], 4).is_err());

        // uçtan uca: HEVC MKV + P7 + StripDvEnhancement
        let k0 = au.clone();
        let p1 = vec![0, 0, 0, 3, 0x02, 0x01, 0xBB];
        let mkv = video_mkv("V_MPEGH/ISO/HEVC", hvcc(), &[(0, true, k0), (40, false, p1.clone())]);
        let mut tr = track_of(&mkv, TrackKind::Video);
        tr.dv_profile = Some(7);
        let mut cfg = vec![1, 0, 0x0E, 0x35, 0x10];
        cfg.resize(24, 0);
        tr.dv_config = Some(cfg);
        tr.dv_config_fourcc = Some(*b"dvcC");
        tr.dv_bl_compat = Some(1);
        let span = Span { index: 0, start: 0.0, end: 1.0 };
        let seg = Seg::parse(&video_segment(Cursor::new(mkv.clone()), &tr, HlsPlan::StripDvEnhancement, &span).unwrap());
        assert_eq!(seg.data(0), &[0, 0, 0, 3, 0x02, 0x01, 0xAA]);
        assert_eq!(seg.data(1), &p1[..]);
        // Copy planında baytlar aynen kalır
        let seg = Seg::parse(&video_segment(Cursor::new(mkv), &tr, HlsPlan::Copy, &span).unwrap());
        assert_eq!(seg.data(0), &au[..]);

        // başlatma segmenti: strip -> DV kutusu yok; copy -> var
        let has = |d: &[u8], k: &[u8]| d.windows(k.len()).any(|w| w == k);
        let init = video_init(&tr, HlsPlan::StripDvEnhancement).unwrap();
        assert!(has(&init, b"hvc1") && has(&init, b"hvcC") && !has(&init, b"dvcC") && !has(&init, b"dvh1"));
        // Copy: P7 için DV kutusu yok (bilinmeyen profil -> düz hvc1)
        let init = video_init(&tr, HlsPlan::Copy).unwrap();
        assert!(has(&init, b"hvc1") && !has(&init, b"dvcC"));
        // Profil 8 + bilinen bl_compat (1/2/4) -> dvcC; bilinmeyen/yok -> yok; profil 5 -> dvcC
        tr.dv_profile = Some(8);
        assert!(has(&video_init(&tr, HlsPlan::Copy).unwrap(), b"dvcC"));
        tr.dv_bl_compat = Some(6);
        assert!(!has(&video_init(&tr, HlsPlan::Copy).unwrap(), b"dvcC"));
        tr.dv_bl_compat = None;
        assert!(!has(&video_init(&tr, HlsPlan::Copy).unwrap(), b"dvcC"));
        tr.dv_profile = Some(5);
        assert!(has(&video_init(&tr, HlsPlan::Copy).unwrap(), b"dvcC"));
    }

    fn colour_mkv(private: Vec<u8>, colour: Option<mkv::Colour>) -> Vec<u8> {
        let mut v = mkv::TrackSpec::new(mkv::TrackKind::Video, "V_MPEGH/ISO/HEVC");
        v.video_size = Some((64, 64));
        v.default_duration_ns = Some(40_000_000);
        v.codec_private = private;
        v.colour = colour;
        let opts = mkv::MuxOptions { cluster_duration_ns: 1_000_000_000, ..Default::default() };
        let mut w = mkv::MkvWriter::new(Cursor::new(Vec::new()), vec![v], opts).unwrap();
        w.write_frame(0, 0, true, &[0, 0, 0, 2, 0x02, 0x01], None).unwrap();
        w.finish().unwrap().into_inner()
    }

    fn master_of(t: TrackInfo) -> String {
        use crate::support::{Level, TrackSupport};
        let s = TrackSupport {
            track_id: t.id,
            kind: "video",
            label: "V".into(),
            in_file: String::new(),
            on_iphone: String::new(),
            level: Level::Full,
            atmos: None,
            hls: Some(HlsPlan::Copy),
        };
        let info = demux::MediaInfo { container: "matroska", duration_secs: Some(60.0), title: None, tracks: vec![t] };
        crate::hls::playlist::master(&info, &[s], None, 1 << 30, &[]).unwrap()
    }

    #[test]
    fn pq_vui_without_container_colour_signals_hdr10_and_colr() {
        let has = |d: &[u8], k: &[u8]| d.windows(k.len()).any(|w| w == k);
        let tr = track_of(&colour_mkv(crate::hls::sps::hvcc_pq_for_test(), None), TrackKind::Video);
        assert_eq!(tr.hdr.as_deref(), Some("HDR10"));
        let pq = crate::demux::ColourInfo { primaries: 9, transfer: 16, matrix: 9, full_range: false };
        assert_eq!(tr.colour, Some(pq));
        assert!(master_of(tr.clone()).contains("VIDEO-RANGE=PQ"));

        let init = video_init(&tr, HlsPlan::Copy).unwrap();
        let colr: &[u8] = &[0, 0, 0, 0x13, b'c', b'o', b'l', b'r', b'n', b'c', b'l', b'x', 0, 9, 0, 16, 0, 9, 0];
        assert!(has(&init, colr));
        let f = filmcraft_isobmff::open(init).unwrap();
        assert!(matches!(
            &f.tracks[0].video().unwrap().color,
            Some(filmcraft_isobmff::ColorInfo::Nclx { primaries: 9, transfer: 16, matrix: 9, full_range: false })
        ));

        // DV profil 5 (dvh1): colr yazılmaz
        let mut p5 = tr.clone();
        p5.colour = Some(pq);
        p5.dv_profile = Some(5);
        p5.dv_config = Some(vec![0; 24]);
        p5.dv_config_fourcc = Some(*b"dvcC");
        let init = video_init(&p5, HlsPlan::Copy).unwrap();
        assert!(has(&init, b"dvh1") && !has(&init, b"colr"));
        // demux de P5 için colour'u temizler; P8.1 (HDR10 tabanı) için çözer
        crate::demux::finalize_video_colour(&mut p5, Default::default());
        assert_eq!(p5.colour, None);
        let mut p8 = tr.clone();
        p8.dv_profile = Some(8);
        p8.dv_bl_compat = Some(1);
        p8.dv_config = Some(vec![0; 24]);
        p8.dv_config_fourcc = Some(*b"dvvC");
        p8.colour = None;
        crate::demux::finalize_video_colour(&mut p8, Default::default());
        assert_eq!(p8.colour, Some(pq));
        let init = video_init(&p8, HlsPlan::Copy).unwrap();
        let (c, d) = (pos(&init, b"colr"), pos(&init, b"dvvC"));
        assert!(pos(&init, b"hvcC") < c && c < d, "sıra hvcC, colr, dvvC");
    }

    #[test]
    fn dv_bl_compat_zero_gets_no_colour() {
        let mut tr = track_of(&colour_mkv(crate::hls::sps::hvcc_pq_for_test(), None), TrackKind::Video);
        assert!(tr.colour.is_some());
        tr.dv_profile = Some(8);
        tr.dv_bl_compat = Some(0);
        crate::demux::finalize_video_colour(&mut tr, Default::default());
        assert_eq!(tr.colour, None);
        tr.dv_bl_compat = Some(1);
        crate::demux::finalize_video_colour(&mut tr, Default::default());
        assert!(tr.colour.is_some());
    }

    fn pos(d: &[u8], k: &[u8]) -> usize {
        d.windows(k.len()).position(|w| w == k).unwrap()
    }

    #[test]
    fn container_colour_is_never_overridden_by_vui() {
        // kapsayıcı BT.709 diyor, SPS PQ diyor: kapsayıcı kazanır -> SDR, colr 1/1/1
        let c = mkv::Colour {
            primaries: Some(1),
            transfer_characteristics: Some(1),
            matrix_coefficients: Some(1),
            range: Some(1),
            ..Default::default()
        };
        let tr = track_of(&colour_mkv(crate::hls::sps::hvcc_pq_for_test(), Some(c)), TrackKind::Video);
        assert_eq!(tr.hdr, None);
        assert_eq!(tr.colour, Some(crate::demux::ColourInfo { primaries: 1, transfer: 1, matrix: 1, full_range: false }));
        assert!(!master_of(tr).contains("VIDEO-RANGE=PQ"));
        // VUI'siz ve kapsayıcısız: colr yok
        let tr = track_of(&colour_mkv(hvcc(), None), TrackKind::Video);
        assert_eq!(tr.colour, None);
        let init = video_init(&tr, HlsPlan::Copy).unwrap();
        assert!(!init.windows(4).any(|w| w == b"colr"));
    }

    #[test]
    fn flac_bits_from_streaminfo_field() {
        let mut si = vec![0u8; 34];
        // 24 bit -> değer 23 = 0b10111: bayt 12 son bit = 1, bayt 13 üst 4 bit = 0b0111
        si[12] = 0x01;
        si[13] = 0x70;
        assert_eq!(flac_bits_from_streaminfo(&si), Some(24));
        si[12] = 0;
        si[13] = 0xF0;
        assert_eq!(flac_bits_from_streaminfo(&si), Some(16));
        assert_eq!(flac_bits_from_streaminfo(&si[..10]), None);
    }

    #[test]
    fn audio_copy_span_exact() {
        let mkv = audio_mkv(625);
        let tr = track_of(&mkv, TrackKind::Audio);
        let span = Span { index: 1, start: 2.0, end: 4.0 };
        let seg = Seg::parse(&audio_segment(Cursor::new(mkv), &tr, HlsPlan::Copy, &span).unwrap());
        let n = seg.samples.len();
        assert!(n == 62 || n == 63, "{n} çerçeve");
        assert!((seg.base - OFF_S) as f64 / 48000.0 >= 2.0);
        assert_eq!(seg.base, 96_768 + OFF_S);
        assert!(seg.samples.iter().all(|s| s.0 == 1536 && s.3 == 0));
        assert_eq!(seg.data(0)[..2], [0, 63]);
        assert!(((seg.end() - OFF_S) as f64 / 48000.0) <= 4.0 + 1e-9);
    }

    #[test]
    fn audio_init_copy_ac3_and_flac_fallback() {
        let mkv = audio_mkv(4);
        let tr = track_of(&mkv, TrackKind::Audio);
        let has = |d: &[u8], k: &[u8]| d.windows(k.len()).any(|w| w == k);
        let init = audio_init(&tr, HlsPlan::Copy, &crate::atmos::testdata::ac3_frame()).unwrap();
        assert!(has(&init, b"ac-3") && has(&init, b"dac3"));
        // çözülemeyen ilk çerçevede iz bilgisine düşer (48 kHz, 2 kanal)
        let init = audio_init(&tr, HlsPlan::DecodeToFlac, &crate::atmos::testdata::ac3_frame()).unwrap();
        assert!(has(&init, b"fLaC") && has(&init, b"dfLa"));
    }

    /// Her pakette 1536 örnek/kanal, 2 kanal: değer = mutlak örnek indeksi (kayıpsız izlenebilir).
    struct FakeDec(bool, usize);
    impl PcmDecoder for FakeDec {
        fn decode(&mut self, p: &[u8], out: &mut Vec<f32>) -> Result<()> {
            let idx = ((p[0] as usize) << 8) | p[1] as usize;
            for i in 0..self.1 {
                for c in 0..2 {
                    out.push((((idx * self.1 + i) * 2 + c) as f32) / 8_388_608.0);
                }
            }
            self.0 = true;
            Ok(())
        }
        fn format(&self) -> Option<AudioFormat> {
            self.0.then_some(AudioFormat { sample_rate: 48_000, channels: 2, channel_mask: 3 })
        }
        fn reset(&mut self) {}
    }

    #[test]
    fn flac_segment_covers_span_exactly() {
        let mkv = audio_mkv(625);
        let tr = track_of(&mkv, TrackKind::Audio);
        let span = Span { index: 1, start: 2.0, end: 4.0 };
        let seg = Seg::parse(&flac_segment(Cursor::new(mkv), &tr, &span, &mut FakeDec(false, 1536)).unwrap());
        assert_eq!(seg.base, 96_000 + OFF_S);
        assert_eq!(seg.samples.iter().map(|s| s.0 as u64).sum::<u64>(), 96_000);
        assert_eq!(seg.samples.len(), 24); // 23 x 4096 + 1792
        assert!(seg.samples[..23].iter().all(|s| s.0 == 4096));
        let enc = crate::hls::flac::FlacEncoder::new(48_000, 2).unwrap();
        let mut file = b"fLaC".to_vec();
        file.extend_from_slice(&[0x80, 0, 0, 34]);
        file.extend(enc.streaminfo());
        file.extend(&seg.mdat);
        let mut r = claxon::FlacReader::new(Cursor::new(file)).unwrap();
        let got: Vec<i32> = r.samples().map(|s| s.unwrap()).collect();
        assert_eq!(got.len(), 96_000 * 2);
        for (j, g) in got.iter().enumerate() {
            assert_eq!(*g as usize, 96_000 * 2 + j, "örnek {j}");
        }
    }

    #[test]
    fn vtt_segment_from_embedded_srt() {
        let mut s = mkv::TrackSpec::new(mkv::TrackKind::Subtitle, "S_TEXT/UTF8");
        s.language = Some("tur".into());
        let opts = mkv::MuxOptions { cluster_duration_ns: 1_000_000_000, ..Default::default() };
        let mut w = mkv::MkvWriter::new(Cursor::new(Vec::new()), vec![s], opts).unwrap();
        let cues = [(1_000u64, 2_500u64, "Önceki"), (2_000, 4_000, "Merhaba\nDünya"), (5_000, 6_000, "Sonra")];
        for (a, e, t) in cues {
            w.write_frame(0, (a * 1_000_000) as i64, true, t.as_bytes(), Some((e - a) * 1_000_000)).unwrap();
        }
        let mkv = w.finish().unwrap().into_inner();
        let tr = track_of(&mkv, TrackKind::Subtitle);
        let vtt = vtt_segment(Cursor::new(mkv), &tr, &Span { index: 1, start: 2.0, end: 4.0 }).unwrap();
        assert!(vtt.starts_with("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n"), "{vtt}");
        assert!(vtt.contains("00:00:02.000 --> 00:00:04.000\nMerhaba\nDünya\n"), "{vtt}");
        assert!(vtt.contains("00:00:01.000 --> 00:00:02.500\nÖnceki"), "kesişen ipucu: {vtt}");
        assert!(!vtt.contains("Sonra"), "{vtt}");
    }

    #[test]
    fn external_vtt_gets_timestamp_map() {
        let v = external_vtt("a.srt", b"1\n00:00:01,000 --> 00:00:02,000\nHi\n");
        assert!(v.starts_with("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n"), "{v}");
        assert!(v.contains("00:00:01.000 --> 00:00:02.000"));
    }

    /// Çerçeve yapıları: kodlama sırasında (pts_kare, anahtar_mı).
    fn gop_ipbb(d0: i64) -> Vec<(i64, bool)> {
        let mut v = vec![(d0, true)];
        for k in 1..=8 {
            v.extend([(d0 + 3 * k, false), (d0 + 3 * k - 2, false), (d0 + 3 * k - 1, false)]);
        }
        v
    }
    fn gop_ippp(d0: i64) -> Vec<(i64, bool)> {
        (0..15).map(|i| (d0 + i, i == 0)).collect()
    }
    fn gop_pyramid(d0: i64) -> Vec<(i64, bool)> {
        let mut v = vec![(d0, true)];
        for j in 1..=4 {
            v.extend([(d0 + 4 * j, false), (d0 + 4 * j - 2, false), (d0 + 4 * j - 3, false), (d0 + 4 * j - 1, false)]);
        }
        v
    }

    #[test]
    fn video_tfdt_continuous_with_varying_gop() {
        let mut order: Vec<(i64, bool)> = Vec::new();
        let mut keys: Vec<i64> = Vec::new(); // anahtar kare görüntüleme sırası
        let mut d0 = 0i64;
        for k in 0..6 {
            let g = match k % 3 {
                0 => gop_ipbb(d0),
                1 => gop_ippp(d0),
                _ => gop_pyramid(d0),
            };
            keys.push(d0);
            d0 += g.len() as i64;
            order.extend(g);
        }
        keys.push(d0);
        let frames: Vec<(i64, bool, Vec<u8>)> =
            order.iter().enumerate().map(|(i, (p, k))| (p * 40, *k, vec![i as u8; 12])).collect();
        let mkv = video_mkv("V_MPEG4/ISO/AVC", vec![], &frames);
        let tr = track_of(&mkv, TrackKind::Video);
        let mut prev: Option<Seg> = None;
        let mut idx = 0usize;
        for g in 0..6 {
            let (a, b) = (keys[g], keys[g + 1]);
            let span = Span { index: g as u32, start: a as f64 * 0.04, end: b as f64 * 0.04 };
            let seg = Seg::parse(&video_segment(Cursor::new(mkv.clone()), &tr, HlsPlan::Copy, &span).unwrap());
            assert_eq!(seg.samples.len() as i64, b - a, "segment {g}");
            let dts = seg.dts();
            for (i, d) in dts.iter().enumerate() {
                let pts = order[idx + i].0 * 3600;
                assert_eq!(*d as i64 + seg.samples[i].3 as i64, pts + OFF_T, "segment {g} örnek {i}");
                assert!(seg.samples[i].3 >= 0);
            }
            assert!(dts.windows(2).all(|w| w[0] < w[1]), "segment {g}");
            if let Some(p) = &prev {
                assert_eq!(seg.base, p.end(), "tfdt süreklilik: segment {g}");
            }
            idx += seg.samples.len();
            prev = Some(seg);
        }
    }

    #[test]
    fn first_segment_has_no_negative_dts() {
        let frames = b_frames(2);
        let mkv = video_mkv("V_MPEG4/ISO/AVC", vec![], &frames);
        let tr = track_of(&mkv, TrackKind::Video);
        let span = Span { index: 0, start: 0.0, end: 1.0 };
        let seg = Seg::parse(&video_segment(Cursor::new(mkv), &tr, HlsPlan::Copy, &span).unwrap());
        assert_eq!(seg.base as i64, OFF_T - D);
        assert!(seg.base > 0);
        assert!(seg.samples.iter().all(|s| s.3 >= 0));
        // süreler tam kare süresi: kırpma / 1 tik hilesi yok
        assert!(seg.samples.iter().all(|s| s.0 == 3600), "{:?}", seg.samples);
    }

    #[test]
    fn open_gop_leading_pictures_are_dropped() {
        // GOP0: I0 P1..P22 (kapalı). GOP1 (açık): K25, öncüller L23 L24 (pts < K), P28 B26 B27, P29 P30.
        let mut f: Vec<(i64, bool, Vec<u8>)> = vec![(0, true, vec![0; 8])];
        for i in 1..=22i64 {
            f.push((i * 40, false, vec![i as u8; 8]));
        }
        for (p, key, tag) in [(25, true, 100u8), (23, false, 101), (24, false, 102), (28, false, 103), (26, false, 104), (27, false, 105), (29, false, 106), (30, false, 107)] {
            f.push((p * 40, key, vec![tag; 8]));
        }
        let mkv = video_mkv("V_MPEG4/ISO/AVC", vec![], &f);
        let tr = track_of(&mkv, TrackKind::Video);
        let span = Span { index: 1, start: 1.0, end: 1.28 };
        let seg = Seg::parse(&video_segment(Cursor::new(mkv), &tr, HlsPlan::Copy, &span).unwrap());
        // öncüller (101, 102) atılır; kalan 6 örnek
        assert_eq!(seg.samples.len(), 6);
        assert!((0..6).all(|i| seg.data(i)[0] != 101 && seg.data(i)[0] != 102));
        assert_eq!(seg.base as i64, 25 * 3600 + OFF_T - D, "ilk dts = anahtar kare pts - D");
        assert!(seg.samples.iter().all(|s| s.3 >= 0));
        let dts = seg.dts();
        assert!(dts.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn reorder_delay_from_sps_or_fallback() {
        let mkv = video_mkv("V_MPEGH/ISO/HEVC", crate::hls::sps::hvcc_for_test(2), &[(0, true, vec![0, 0, 0, 1, 2])]);
        let tr = track_of(&mkv, TrackKind::Video);
        assert_eq!(reorder_delay_ticks(&tr), 2 * 3600 + 180, "SPS: 2 kare");
        let mkv = video_mkv("V_MPEG4/ISO/AVC", vec![], &[(0, true, vec![0, 0, 0, 1, 2])]);
        let tr = track_of(&mkv, TrackKind::Video);
        assert_eq!(reorder_delay_ticks(&tr), 14_400 + 180, "yedek: 4 x varsayılan süre");
    }

    #[test]
    fn audio_copy_segments_contiguous() {
        for (codec, frame) in [("A_AAC", 1024u64), ("A_AC3", 1536)] {
            let mkv = audio_mkv_with(codec, 400, frame);
            let tr = track_of(&mkv, TrackKind::Audio);
            let mut prev: Option<Seg> = None;
            for g in 0..3u32 {
                let span = Span { index: g, start: 1.0 + g as f64, end: 2.0 + g as f64 };
                let seg = Seg::parse(&audio_segment(Cursor::new(mkv.clone()), &tr, HlsPlan::Copy, &span).unwrap());
                assert!(seg.samples.iter().all(|s| s.0 as u64 == frame), "{codec}");
                assert_eq!((seg.base - OFF_S) % frame, 0, "{codec}: kare ızgarası");
                if let Some(p) = &prev {
                    assert_eq!(seg.base, p.end(), "{codec}: segment {g}");
                }
                prev = Some(seg);
            }
        }
    }

    #[test]
    fn flac_segments_join_sample_exact() {
        // 1024 örneklik çerçeveler, ms'ye yuvarlanmış pts: yakalama olmadan hizalama kayar
        let mkv = audio_mkv_with("A_AC3", 400, 1024);
        let tr = track_of(&mkv, TrackKind::Audio);
        let decode = |span: &Span| -> Vec<i32> {
            let seg = Seg::parse(&flac_segment(Cursor::new(mkv.clone()), &tr, span, &mut FakeDec(false, 1024)).unwrap());
            let enc = crate::hls::flac::FlacEncoder::new(48_000, 2).unwrap();
            let mut file = b"fLaC".to_vec();
            file.extend_from_slice(&[0x80, 0, 0, 34]);
            file.extend(enc.streaminfo());
            file.extend(&seg.mdat);
            claxon::FlacReader::new(Cursor::new(file)).unwrap().samples().map(|s| s.unwrap()).collect()
        };
        let a = decode(&Span { index: 0, start: 2.0, end: 3.0 });
        let b = decode(&Span { index: 1, start: 3.0, end: 4.0 });
        let all: Vec<i32> = a.into_iter().chain(b).collect();
        assert_eq!(all.len(), 96_000 * 2);
        for (j, g) in all.iter().enumerate() {
            assert_eq!(*g as usize, 96_000 * 2 + j, "örnek {j}");
        }
    }

    #[test]
    fn truehd_like_40_sample_frames_join_sample_exact() {
        // 40 örneklik AU'lar (48 kHz), ms'ye yuvarlanmış pts: ms hatası (24 örnek) AU'dan büyük;
        // çoklu paket uydurması olmadan sınırlarda kayma/tıkırtı olur.
        let mkv = audio_mkv_with("A_AC3", 4800, 40);
        let tr = track_of(&mkv, TrackKind::Audio);
        // sınırlar AU ızgarasına ve ms'ye denk gelmeyen değerlerde
        let bounds = [1.0123f64, 2.0377, 3.0551, 3.9999];
        let at = |t: f64| (t * 48_000.0).round() as u64;
        let mut all: Vec<i32> = Vec::new();
        for g in 0..3usize {
            let span = Span { index: g as u32, start: bounds[g], end: bounds[g + 1] };
            let seg = Seg::parse(&flac_segment(Cursor::new(mkv.clone()), &tr, &span, &mut FakeDec(false, 40)).unwrap());
            assert_eq!(seg.base, at(bounds[g]) + OFF_S);
            let enc = crate::hls::flac::FlacEncoder::new(48_000, 2).unwrap();
            let mut file = b"fLaC".to_vec();
            file.extend_from_slice(&[0x80, 0, 0, 34]);
            file.extend(enc.streaminfo());
            file.extend(&seg.mdat);
            all.extend(claxon::FlacReader::new(Cursor::new(file)).unwrap().samples().map(|s| s.unwrap()));
        }
        let first = at(bounds[0]) as usize;
        assert_eq!(all.len(), (at(bounds[3]) as usize - first) * 2);
        for (j, g) in all.iter().enumerate() {
            assert_eq!(*g as usize, first * 2 + j, "örnek {j}");
        }
    }
}
