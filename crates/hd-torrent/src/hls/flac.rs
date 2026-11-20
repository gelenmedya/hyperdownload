//! Saf Rust FLAC kodlayıcı sarmalayıcısı (flacenc): 24-bit, çok kanallı PCM -> sabit blok boyutlu
//! (4096) FLAC çerçeveleri. Her çerçeve bir fMP4 örneği olur; STREAMINFO `fLaC`/`dfLa` için üretilir.

use anyhow::{Result, anyhow, bail};
use flacenc::bitsink::MemSink;
use flacenc::component::{BitRepr, StreamInfo};
use flacenc::config::Encoder as Config;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Fill, FrameBuf};

/// Sabit blok boyutu (örnek/kanal).
pub const BLOCK_SIZE: usize = 4096;
/// Çıktı örnek derinliği.
pub const BITS: u16 = 24;

/// Çok kanallı FLAC çerçeve üreticisi.
pub struct FlacEncoder {
    cfg: Verified<Config>,
    info: StreamInfo,
    rate: u32,
    channels: u16,
}

impl FlacEncoder {
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self> {
        if !(1..=8).contains(&channels) {
            bail!("FLAC en çok 8 kanal destekler (kanal: {channels})");
        }
        let mut cfg = Config::default();
        cfg.block_size = BLOCK_SIZE;
        cfg.multithread = false;
        let cfg = cfg
            .into_verified()
            .map_err(|e| anyhow!("flacenc yapılandırması: {e:?}"))?;
        let info = StreamInfo::new(sample_rate as usize, channels as usize, BITS as usize)
            .map_err(|e| anyhow!("flac akış bilgisi: {e:?}"))?;
        Ok(Self { cfg, info, rate: sample_rate, channels })
    }

    /// Akışı tanımlayan ham 34 baytlık STREAMINFO gövdesi: sabit blok boyutu 4096 (en az = en çok),
    /// çerçeve boyutları ve toplam örnek sayısı bilinmiyor (0), MD5 sıfır.
    ///
    /// En az ≠ en çok yazmak (ör. 16/4096) Apple AVPlayer'da tüm oynatmayı -16170 ile düşürüyor
    /// (Codemagic AVFoundation probe, 2026-11-06). Şartnameye göre en az değer son bloğu saymaz;
    /// segment sonundaki kısa çerçeve bu yüzden sorun değil (probe'da da oynadı).
    pub fn streaminfo(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(34);
        v.extend((BLOCK_SIZE as u16).to_be_bytes());
        v.extend((BLOCK_SIZE as u16).to_be_bytes());
        v.extend([0u8; 6]); // min/max çerçeve boyutu: bilinmiyor
        // 20 bit örnekleme hızı | 3 bit (kanal-1) | 5 bit (bit-1) | 36 bit toplam örnek
        let packed: u64 = ((self.rate as u64 & 0xF_FFFF) << 44)
            | (((self.channels as u64 - 1) & 7) << 41)
            | (((BITS as u64 - 1) & 0x1F) << 36);
        v.extend(packed.to_be_bytes());
        v.extend([0u8; 16]); // MD5
        v
    }

    /// `pcm` (iç içe, kanal sırası WAV ile aynı, 24-bit işaretli tamsayılar) tek bir çerçeveye
    /// kodlanır; en çok [`BLOCK_SIZE`] örnek/kanal. `frame_number` FLAC üstbilgisine yazılır.
    pub fn encode_frame(&self, pcm: &[i32], frame_number: u32) -> Result<Vec<u8>> {
        let ch = self.channels as usize;
        if pcm.is_empty() || pcm.len() % ch != 0 || pcm.len() / ch > BLOCK_SIZE {
            bail!("geçersiz FLAC blok uzunluğu: {} örnek, {ch} kanal", pcm.len());
        }
        let mut fb = FrameBuf::with_size(ch, BLOCK_SIZE).map_err(|e| anyhow!("framebuf: {e:?}"))?;
        fb.fill_interleaved(pcm).map_err(|e| anyhow!("framebuf doldurma: {e:?}"))?;
        let frame = flacenc::encode_fixed_size_frame(&self.cfg, &fb, frame_number as usize, &self.info)
            .map_err(|e| anyhow!("flac kodlama: {e:?}"))?;
        let mut sink = MemSink::<u8>::with_capacity(frame.count_bits());
        frame.write(&mut sink).map_err(|e| anyhow!("flac yazma: {e:?}"))?;
        Ok(sink.as_slice().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(ch: usize, n: usize) -> Vec<i32> {
        let mut v = Vec::with_capacity(ch * n);
        for i in 0..n {
            for c in 0..ch {
                let t = i as f64 / 48_000.0;
                let x = (t * 2.0 * std::f64::consts::PI * (220.0 * (c + 1) as f64)).sin() * 0.8;
                v.push((x * 8_388_607.0) as i32);
            }
        }
        // uç değerler de kayıpsız geçmeli
        v[0] = -8_388_608;
        v[1] = 8_388_607;
        v
    }

    #[test]
    fn flac_roundtrip_8ch() {
        let ch = 8usize;
        let n = BLOCK_SIZE * 3 + 1000; // son çerçeve kısa
        let pcm = sine(ch, n);
        let enc = FlacEncoder::new(48_000, ch as u16).unwrap();
        let si = enc.streaminfo();
        assert_eq!(si.len(), 34);
        // Apple AVPlayer min≠max blok boyutlu STREAMINFO'yu reddediyor (CoreMediaErrorDomain
        // -16170; Codemagic AVFoundation probe 2026-11-06). FLAC şartnamesine göre de en küçük blok
        // boyutu son bloğu saymaz; segment sonundaki kısa çerçeve bunu bozmaz.
        assert_eq!(u16::from_be_bytes([si[0], si[1]]), 4096);
        assert_eq!(u16::from_be_bytes([si[2], si[3]]), 4096);
        let mut file = b"fLaC".to_vec();
        file.extend_from_slice(&[0x80, 0, 0, 34]); // son meta blok, STREAMINFO, 34 bayt
        file.extend_from_slice(&si);
        for (k, chunk) in pcm.chunks(BLOCK_SIZE * ch).enumerate() {
            file.extend(enc.encode_frame(chunk, k as u32).unwrap());
        }
        let mut r = claxon::FlacReader::new(std::io::Cursor::new(file)).unwrap();
        assert_eq!(r.streaminfo().channels, 8);
        assert_eq!(r.streaminfo().bits_per_sample, 24);
        assert_eq!(r.streaminfo().sample_rate, 48_000);
        let got: Vec<i32> = r.samples().map(|s| s.unwrap()).collect();
        assert_eq!(got.len(), pcm.len());
        // claxon kanal-ardışık değil, iç içe örnek verir
        assert!(got == pcm, "kayıpsız değil");
    }
}
