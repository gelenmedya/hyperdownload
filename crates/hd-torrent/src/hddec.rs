//! hddec.rs - pure-Rust PCM decoders for Dolby TrueHD (`truehd` 0.7) and DTS core
//! (`oxideav-dts` 0.0.2, default-features = false).
//!
//! Cargo deps:
//!   truehd = "0.7"
//!   oxideav-dts = { version = "0.0.2", default-features = false }
//!   anyhow = "1"
//!
//! Output of every decoder: interleaved f32 in [-1,1), channels sorted in WAV /
//! WAVEFORMATEXTENSIBLE order (ascending channel-mask bit), `format()` gives the mask.

use anyhow::{anyhow, bail, Result};
use std::panic::{catch_unwind, AssertUnwindSafe};

pub use crate::audio::{AudioFormat, PcmDecoder};

pub fn new_truehd_decoder() -> Result<Box<dyn PcmDecoder>> {
    Ok(Box::new(TrueHdDecoder::new()))
}

pub fn new_dts_decoder() -> Result<Box<dyn PcmDecoder>> {
    Ok(Box::new(DtsDecoder::new()))
}

// ---------------------------------------------------------------------------------------
// WAV channel mask bits
// ---------------------------------------------------------------------------------------
const FL: u32 = 0x1;
const FR: u32 = 0x2;
const FC: u32 = 0x4;
const LFE: u32 = 0x8;
const BL: u32 = 0x10;
const BR: u32 = 0x20;
const FLC: u32 = 0x40;
const FRC: u32 = 0x80;
const BC: u32 = 0x100;
const SL: u32 = 0x200;
const SR: u32 = 0x400;
const TC: u32 = 0x800;
const TFL: u32 = 0x1000;
const TFC: u32 = 0x2000;
const TFR: u32 = 0x4000;
const TBL: u32 = 0x8000;
const TBR: u32 = 0x20000;

/// Given the WAV bit each *source* channel wants (0 = no natural bit), assign unique bits
/// (spilling collisions / unknowns into free bits) and return (mask, perm) where
/// `perm[k]` = source channel index that goes to output slot k (ascending bit order).
fn build_layout(wanted: &[u32]) -> (u32, Vec<usize>) {
    let mut used = 0u32;
    let mut assigned = vec![0u32; wanted.len()];
    for (i, &w) in wanted.iter().enumerate() {
        if w != 0 && used & w == 0 {
            assigned[i] = w;
            used |= w;
        }
    }
    let mut free: Vec<u32> = (0..18).map(|b| 1u32 << b).filter(|b| used & b == 0).collect();
    free.reverse();
    for a in assigned.iter_mut().filter(|a| **a == 0) {
        // more than 18 channels cannot happen (<=16 in TrueHD)
        let b = free.pop().unwrap_or(0);
        *a = b;
        used |= b;
    }
    let mut idx: Vec<usize> = (0..wanted.len()).collect();
    idx.sort_by_key(|&i| assigned[i]);
    (used, idx)
}

// ---------------------------------------------------------------------------------------
// TrueHD
// ---------------------------------------------------------------------------------------
use truehd::process::{decode::Decoder as ThdDecoder, extract::Extractor, parse::Parser};
use truehd::structs::channel::ChannelLabel;

/// Presentation index we ask for. TrueHD presentations: 0 = 2ch, 1 = 6ch, 2 = 8ch,
/// 3 = 16ch (Atmos object bed, 31EC 32-bit). We request 2: if the stream carries only
/// 2/6 channel presentations the crate resolves "2" to the highest independent one
/// below it (6ch -> 5.1), and the 16-ch Atmos substream is never even parsed/decoded.
const WANT_PRESENTATION: usize = 2;

struct TrueHdDecoder {
    ex: Extractor,
    parser: Parser,
    dec: ThdDecoder,
    synced: bool, // false -> drop AUs until next major sync
    fmt: Option<AudioFormat>,
    layout: Option<(Vec<usize>, Vec<ChannelLabel>)>, // (perm, labels the perm was built for)
}

impl TrueHdDecoder {
    fn new() -> Self {
        let mut s = Self {
            ex: Extractor::default(),
            parser: Parser::default(),
            dec: ThdDecoder::default(),
            synced: false,
            fmt: None,
            layout: None,
        };
        s.fresh();
        s
    }

    fn fresh(&mut self) {
        self.ex = Extractor::default();
        self.parser = Parser::default();
        self.parser.set_check_fifo(false); // conformance model only; saves time
        let mut req = [false; truehd::process::MAX_PRESENTATIONS];
        req[WANT_PRESENTATION] = true;
        self.parser.set_required_presentations(&req);
        self.dec = ThdDecoder::default();
        self.synced = false;
    }

    fn label_bit(l: ChannelLabel) -> u32 {
        use ChannelLabel::*;
        match l {
            L => FL,
            R => FR,
            C => FC,
            LFE => self::LFE,
            Ls => SL,
            Rs => SR,
            Lb => BL,
            Rb => BR,
            Lsc => FLC,
            Rsc => FRC,
            Cb => BC,
            Tc => TC,
            Tfl => TFL,
            Tfr => TFR,
            Tfc => TFC,
            Tbl => TBL,
            Tbr => TBR,
            // Tsl/Tsr, Lsd/Rsd, Lw/Rw, LFE2: no natural WAV bit / collide -> spilled
            _ => 0,
        }
    }

    fn handle_frame(&mut self, frame: &truehd::process::extract::Frame, out: &mut Vec<f32>) -> Result<()> {
        if !self.synced {
            if !frame.is_major_sync() {
                return Ok(());
            }
            self.synced = true;
        }
        let au = self.parser.parse(frame)?;
        let d = self.dec.decode_presentation(&au, WANT_PRESENTATION)?;
        if d.is_duplicate {
            return Ok(());
        }
        let ch = d.channel_count;
        if ch == 0 || ch > 16 {
            bail!("truehd: bad channel count {ch}");
        }
        // (re)build channel layout when labels change
        let need = match &self.layout {
            Some((perm, labels)) => perm.len() != ch || *labels != d.channel_labels,
            None => true,
        };
        if need {
            let wanted: Vec<u32> = (0..ch)
                .map(|i| d.channel_labels.get(i).map_or(0, |l| Self::label_bit(*l)))
                .collect();
            let (mask, perm) = build_layout(&wanted);
            self.fmt = Some(AudioFormat {
                sample_rate: d.sampling_frequency,
                channels: ch as u16,
                channel_mask: mask,
            });
            self.layout = Some((perm, d.channel_labels.clone()));
        }
        let (perm, _) = self.layout.as_ref().unwrap();
        const K: f32 = 1.0 / 8_388_608.0; // 24-bit
        out.reserve(d.sample_length * ch);
        for s in 0..d.sample_length {
            let row = &d.pcm_data[s];
            for &c in perm {
                out.push(row[c] as f32 * K);
            }
        }
        Ok(())
    }
}

impl PcmDecoder for TrueHdDecoder {
    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<()> {
        use truehd::utils::errors::ExtractError;
        let start = out.len();
        let r = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            self.ex.push_bytes(packet);
            let mut first_err: Option<anyhow::Error> = None;
            loop {
                // Extractor is an Iterator; it ends with InsufficientData / None
                match self.ex.next() {
                    None => break,
                    Some(Err(ExtractError::InsufficientData)) => break,
                    Some(Err(_)) => continue, // damaged bytes: extractor resyncs itself
                    Some(Ok(frame)) => {
                        let mark = out.len();
                        if let Err(e) = self.handle_frame(&frame, out) {
                            out.truncate(mark);
                            // stream state is unreliable now: restart at next major sync
                            self.parser.reset_for_next_major_sync();
                            self.dec.reset_for_next_major_sync();
                            self.synced = false;
                            first_err.get_or_insert(e);
                        }
                    }
                }
            }
            match first_err {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }));
        match r {
            Ok(r) => r,
            Err(_) => {
                out.truncate(start);
                self.fresh();
                Err(anyhow!("truehd decoder panicked; state reset"))
            }
        }
    }

    fn format(&self) -> Option<AudioFormat> {
        self.fmt
    }

    fn reset(&mut self) {
        self.fresh();
        // keep self.fmt / layout: same stream, WAV header already sent
    }
}

// ---------------------------------------------------------------------------------------
// DTS (core only; DTS-HD extension substreams / XLL are ignored)
// ---------------------------------------------------------------------------------------
use oxideav_dts::{
    parse_frame_header, unpack_14bit_to_16bit, AmodeArrangement, CoreStreamDecoder, DtsFrameHeader,
    FourteenBitByteOrder, SyncWordEncoding,
};

struct DtsDecoder {
    stream: Option<CoreStreamDecoder>,
    fmt: Option<AudioFormat>,
    perm: Vec<usize>,
}

impl DtsDecoder {
    fn new() -> Self {
        Self { stream: None, fmt: None, perm: vec![] }
    }

    fn sync_of(b: &[u8]) -> Option<SyncWordEncoding> {
        if b.len() < 4 {
            return None;
        }
        match [b[0], b[1], b[2], b[3]] {
            [0x7F, 0xFE, 0x80, 0x01] => Some(SyncWordEncoding::RawBigEndian),
            [0xFE, 0x7F, 0x01, 0x80] => Some(SyncWordEncoding::RawLittleEndian),
            [0x1F, 0xFF, 0xE8, 0x00] => Some(SyncWordEncoding::FourteenBitBigEndian),
            [0xFF, 0x1F, 0x00, 0xE8] => Some(SyncWordEncoding::FourteenBitLittleEndian),
            _ => None,
        }
    }

    /// primary-channel WAV bits in DTS AMODE order (Table 5-4); LFE is appended by caller
    fn amode_bits(a: AmodeArrangement) -> Option<Vec<u32>> {
        use AmodeArrangement::*;
        Some(match a {
            Mono => vec![FC],
            DualMono | Stereo | SumDifference | LtRt => vec![FL, FR],
            ClR => vec![FC, FL, FR],
            LrS => vec![FL, FR, BC],
            ClRS => vec![FC, FL, FR, BC],
            LrSlSr => vec![FL, FR, SL, SR],
            ClRSlSr => vec![FC, FL, FR, SL, SR],
            ClCrLRSlSr => vec![FLC, FRC, FL, FR, SL, SR],
            ClRLrRrOv => vec![FC, FL, FR, BL, BR, TC],
            CfCrLfRfLrRr => vec![FC, BC, FL, FR, BL, BR],
            ClCCrLRSlSr => vec![FLC, FC, FRC, FL, FR, SL, SR],
            ClCrLRSl1Sl2Sr1Sr2 => vec![FLC, FRC, FL, FR, SL, BL, SR, BR],
            ClCCrLRSlSSr => vec![FLC, FC, FRC, FL, FR, SL, BC, SR],
            _ => return None,
        })
    }

    fn do_decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<()> {
        let enc = match Self::sync_of(packet) {
            Some(e) => e,
            // DTS-HD packet that starts with an extension substream only (no core): skip
            None => return Ok(()),
        };
        // normalise to raw big-endian 16-bit domain
        let owned: Vec<u8>;
        let bytes: &[u8] = match enc {
            SyncWordEncoding::RawBigEndian => packet,
            SyncWordEncoding::RawLittleEndian => {
                let mut v = packet.to_vec();
                for p in v.chunks_exact_mut(2) {
                    p.swap(0, 1);
                }
                owned = v;
                &owned
            }
            SyncWordEncoding::FourteenBitBigEndian | SyncWordEncoding::FourteenBitLittleEndian => {
                let order = FourteenBitByteOrder::from_sync(enc).ok_or_else(|| anyhow!("dts: bad 14-bit sync"))?;
                owned = unpack_14bit_to_16bit(packet, order).map_err(|e| anyhow!("dts unpack14: {e}"))?;
                &owned
            }
            _ => return Ok(()),
        };
        let hdr: DtsFrameHeader = parse_frame_header(bytes).map_err(|e| anyhow!("dts header: {e}"))?;
        // A_DTS blocks of DTS-HD streams are core frame + EXSS data: cut to the core frame
        let fsz = hdr.frame_size_bytes as usize;
        if fsz > bytes.len() {
            bail!("dts: truncated frame ({} < {})", bytes.len(), fsz);
        }
        let bytes = &bytes[..fsz];

        let (coding, _) = oxideav_dts::decode_audio_coding_header_at(bytes, hdr.header_bit_length() as usize, hdr.crc_present)
            .map_err(|e| anyhow!("dts acoding header: {e}"))?;
        let npchs = coding.n_pchs;
        let has_lfe = hdr.lfe.is_present();

        let stream = match self.stream.take() {
            Some(s) if s.channel_count() == npchs => s,
            _ => CoreStreamDecoder::new(npchs),
        };
        let mut stream = stream;
        let res = stream.decode_frame(bytes, &hdr);
        let lfe = stream.take_last_lfe_pcm();
        self.stream = Some(stream);
        let mut planes = res.map_err(|e| anyhow!("dts decode: {e}"))?;
        if !lfe.is_empty() {
            planes.push(lfe);
        }
        let ch = planes.len();
        let n = planes.first().map_or(0, |p| p.len());
        if ch == 0 || n == 0 {
            return Ok(());
        }

        // layout (rebuild each frame is cheap; only publish format on change)
        let mut wanted = Self::amode_bits(hdr.amode_arrangement()).unwrap_or_else(|| vec![0; npchs]);
        if wanted.len() != npchs {
            wanted = vec![0; npchs];
        }
        if planes.len() > npchs {
            wanted.push(LFE);
        }
        let _ = has_lfe;
        let (mask, perm) = build_layout(&wanted);
        let fmt = AudioFormat {
            sample_rate: hdr.sample_rate_hz().ok_or_else(|| anyhow!("dts: reserved sample rate"))?,
            channels: ch as u16,
            channel_mask: mask,
        };
        self.fmt = Some(fmt);
        self.perm = perm;

        // Verified empirically: the crate returns i32 samples at 32-bit full scale
        // (value / 2^31 = float), independent of the stream's PCMR bit depth.
        let k = 1.0f32 / 2_147_483_648.0;
        out.reserve(n * ch);
        for s in 0..n {
            for &c in &self.perm {
                out.push(planes[c][s] as f32 * k);
            }
        }
        Ok(())
    }
}

impl PcmDecoder for DtsDecoder {
    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<()> {
        let start = out.len();
        match catch_unwind(AssertUnwindSafe(|| self.do_decode(packet, out))) {
            Ok(r) => {
                if r.is_err() {
                    out.truncate(start);
                }
                r
            }
            Err(_) => {
                out.truncate(start);
                self.stream = None;
                Err(anyhow!("dts decoder panicked; state reset"))
            }
        }
    }

    fn format(&self) -> Option<AudioFormat> {
        self.fmt
    }

    fn reset(&mut self) {
        self.stream = None; // fresh QMF history
    }
}

