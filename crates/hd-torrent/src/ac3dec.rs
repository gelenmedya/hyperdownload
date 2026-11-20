//! AC-3 / E-AC-3 -> interleaved f32 PCM (WAV channel order), on top of `filmcraft-ac3` 0.9.
//!
//! Notes (verified, see ac3dec.md):
//! * One Matroska block may hold several syncframes (E-AC-3: independent + dependent substreams);
//!   `decode` walks them all.
//! * E-AC-3 dependent substreams (7.1 extension channels) and extra programs are skipped: a 7.1
//!   stream is played as its 5.1 core (so `format()` reports 6 channels). Atmos/JOC objects are
//!   ignored for the same reason (the bed/core is what the crate decodes).
//! * dynrng (DRC) is ALWAYS applied by the crate (not configurable); `compr` and dialnorm ignored.
//! * AHT / enhanced coupling frames (`Error::Unsupported`) and corrupt frames are concealed with
//!   silence of the right length so A/V sync is kept; see `concealed_frames()`.

use anyhow::{Result, bail};
use filmcraft_ac3::{Decoder, parse_header};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    /// WAVE_FORMAT_EXTENSIBLE dwChannelMask.
    pub channel_mask: u32,
}

pub trait PcmDecoder: Send {
    /// Feed one compressed packet (Matroska block: one or more A_AC3 / A_EAC3 syncframes);
    /// append interleaved PCM (WAV channel order) to `out`.
    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<()>;
    /// Known after the first successfully decoded frame.
    fn format(&self) -> Option<AudioFormat>;
    /// Call after a seek.
    #[allow(dead_code)] // her istek yeni çözücü açar; atlama yeni akıştır
    fn reset(&mut self);
    /// Number of syncframes replaced by silence because they could not be decoded.
    #[allow(dead_code)]
    fn concealed_frames(&self) -> u64 {
        0
    }
}

// Speaker bits (WAVEFORMATEXTENSIBLE).
const FL: u32 = 0x1;
const FR: u32 = 0x2;
const FC: u32 = 0x4;
const LFE: u32 = 0x8;
const BC: u32 = 0x100;
const SL: u32 = 0x200;
const SR: u32 = 0x400;

/// Channel mask for the crate's output (WAV order: L R C LFE Ls Rs ...) per acmod/lfe.
pub fn channel_mask(acmod: u8, lfe: bool) -> u32 {
    let m = match acmod {
        0 | 2 => FL | FR,            // 1+1 dual mono (played as stereo) / 2.0
        1 => FC,                     // 1/0
        3 => FL | FR | FC,           // 3/0
        4 => FL | FR | BC,           // 2/1
        5 => FL | FR | FC | BC,      // 3/1
        6 => FL | FR | SL | SR,      // 2/2
        _ => FL | FR | FC | SL | SR, // 3/2
    };
    if lfe { m | LFE } else { m }
}

struct Ac3Pcm {
    dec: Decoder,
    fmt: Option<AudioFormat>,
    concealed: u64,
}

pub fn new_ac3_decoder() -> Box<dyn PcmDecoder> {
    Box::new(Ac3Pcm { dec: Decoder::new(), fmt: None, concealed: 0 })
}

impl Ac3Pcm {
    fn push_silence(&mut self, samples: usize, out: &mut Vec<f32>) {
        if let Some(f) = self.fmt {
            out.resize(out.len() + samples * f.channels as usize, 0.0);
            self.concealed += 1;
        }
    }
}

impl PcmDecoder for Ac3Pcm {
    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<()> {
        let mut pos = 0usize;
        let mut first_err: Option<anyhow::Error> = None;
        while pos + 7 <= packet.len() {
            // resync if the packet has junk / padding between frames
            if packet[pos] != 0x0B || packet[pos + 1] != 0x77 {
                match packet[pos + 1..].windows(2).position(|w| w == [0x0B, 0x77]) {
                    Some(p) => pos += p + 1,
                    None => break,
                }
                continue;
            }
            let h = match parse_header(&packet[pos..]) {
                Ok(h) => h,
                Err(e) => {
                    first_err.get_or_insert(e.into());
                    pos += 2;
                    continue;
                }
            };
            if h.frame_bytes == 0 || pos + h.frame_bytes > packet.len() {
                first_err.get_or_insert(anyhow::anyhow!("truncated syncframe"));
                break;
            }
            let frame = &packet[pos..pos + h.frame_bytes];
            pos += h.frame_bytes;
            if !h.is_primary() {
                continue; // dependent substream / other program: not decoded by the crate
            }
            match self.dec.decode(frame) {
                Ok(d) => {
                    let n = d.channels.len();
                    if n == 0 {
                        continue;
                    }
                    let spf = d.channels[0].len();
                    let fmt = AudioFormat {
                        sample_rate: h.sample_rate,
                        channels: n as u16,
                        channel_mask: channel_mask(h.acmod, h.lfeon),
                    };
                    match self.fmt {
                        None => self.fmt = Some(fmt),
                        Some(f) if f.channels != fmt.channels || f.sample_rate != fmt.sample_rate => {
                            // mid-stream layout change: the output WAV format is fixed, conceal
                            self.push_silence(spf, out);
                            first_err.get_or_insert(anyhow::anyhow!("audio format changed mid-stream"));
                            continue;
                        }
                        _ => {}
                    }
                    let base = out.len();
                    out.resize(base + spf * n, 0.0);
                    for (c, ch) in d.channels.iter().enumerate() {
                        for (i, s) in ch.iter().enumerate() {
                            out[base + i * n + c] = *s;
                        }
                    }
                }
                Err(e) => {
                    self.push_silence(h.samples(), out);
                    first_err.get_or_insert(e.into());
                }
            }
        }
        match first_err {
            // Errors were concealed (or are harmless); report only if nothing could be produced.
            Some(e) if self.fmt.is_none() => bail!(e),
            _ => Ok(()),
        }
    }

    fn format(&self) -> Option<AudioFormat> {
        self.fmt
    }

    fn reset(&mut self) {
        self.dec.reset();
    }

    fn concealed_frames(&self) -> u64 {
        self.concealed
    }
}
