//! Küçük fMP4 (CMAF) yazıcı: HLS için başlatma ve medya segmentleri.
//!
//! Video/ses baytları değiştirilmeden (kopya) taşınır. filmcraft-isobmff yazıcısı `ec-3`/`ac-3`/
//! `dvcC`/`dvvC` yazamadığı ve bağımsız segmentlerde keyfi `sequence_number`/`baseMediaDecodeTime`
//! veremediği için kutular burada elle yazılır. Biçim: ISO/IEC 14496-12 (ftyp, moov/mvex, moof
//! `default-base-is-moof`, tfdt v1, trun v1), ETSI TS 102 366 Ek F (`dac3`/`dec3`; JOC uzantısı
//! `flag_ec3_extension_type_a` + `complexity_index_type_a`), Dolby Vision ISOBMFF (`dvcC` P<=7,
//! `dvvC` P8-10), FLAC-in-ISOBMFF (`dfLa`), Opus-in-ISOBMFF (`dOps`).

use anyhow::{Result, anyhow, bail};

use crate::atmos::eac3_config;
use crate::demux::ColourInfo;

/// Dolby Vision yapılandırma kutusu: `dvcC` (profil <= 7) ya da `dvvC` (8+); yük ham kayıttır.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoviBox {
    pub fourcc: [u8; 4],
    pub payload: Vec<u8>,
}

/// Parçanın örnek girişi (codec yapılandırması dahil).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// `avc1` + `avcC`.
    Avc { avcc: Vec<u8>, w: u16, h: u16, colr: Option<ColourInfo> },
    /// `hvc1` (dv None ya da P8 uyumlu) ya da `dvh1` (P5, `dvh1 = true`) + `hvcC` + DV kutusu.
    Hevc { hvcc: Vec<u8>, w: u16, h: u16, dv: Option<DoviBox>, dvh1: bool, colr: Option<ColourInfo> },
    /// `ec-3` + `dec3` (yük: [`build_dec3`]).
    Eac3 { dec3: Vec<u8>, channels: u16, rate: u32 },
    /// `ac-3` + `dac3` (yük: [`build_dac3`]).
    Ac3 { dac3: Vec<u8>, channels: u16, rate: u32 },
    /// `mp4a` + `esds` (AudioSpecificConfig'ten).
    Aac { asc: Vec<u8>, channels: u16, rate: u32 },
    /// `fLaC` + `dfLa`; `streaminfo` 34 baytlık STREAMINFO gövdesi.
    Flac { streaminfo: Vec<u8>, channels: u16, rate: u32, bits: u16 },
    /// `Opus` + `dOps` (yük: [`opus_dops_from_head`] çıktısı, ISOBMFF/büyük-endian).
    Opus { dops: Vec<u8>, channels: u16, rate: u32 },
}

impl Entry {
    fn is_video(&self) -> bool {
        matches!(self, Entry::Avc { .. } | Entry::Hevc { .. })
    }
}

/// Bir medya örneği (kopya baytlar).
pub struct Sample<'a> {
    pub data: &'a [u8],
    pub duration: u32,
    /// Kompozisyon ofseti (pts - dts), işaretli.
    pub cto: i32,
    pub sync: bool,
}

// ---------------------------------------------------------------------------------------------
// Kutu yardımcıları
// ---------------------------------------------------------------------------------------------

fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + payload.len());
    v.extend(((8 + payload.len()) as u32).to_be_bytes());
    v.extend(kind);
    v.extend(payload);
    v
}

fn fbx(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + payload.len());
    p.push(version);
    p.extend(&flags.to_be_bytes()[1..]);
    p.extend(payload);
    bx(kind, &p)
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn matrix() -> Vec<u8> {
    MATRIX.iter().flat_map(|m| m.to_be_bytes()).collect()
}

// ---------------------------------------------------------------------------------------------
// Başlatma segmenti
// ---------------------------------------------------------------------------------------------

/// `ftyp` (iso6/cmfc) + `moov` (+`mvex/trex`) — boş örnek tabloları.
pub fn init_segment(track_id: u32, timescale: u32, entry: &Entry) -> Vec<u8> {
    let video = entry.is_video();
    let ftyp = bx(b"ftyp", &cat(&[b"iso6", &1u32.to_be_bytes(), b"iso6", b"cmfc"]));

    let mut mvhd = Vec::new();
    mvhd.extend([0u8; 8]); // oluşturma/değiştirme
    mvhd.extend(timescale.to_be_bytes());
    mvhd.extend(0u32.to_be_bytes()); // duration (parçalı)
    mvhd.extend(0x0001_0000u32.to_be_bytes()); // rate
    mvhd.extend(0x0100u16.to_be_bytes()); // volume
    mvhd.extend([0u8; 10]);
    mvhd.extend(matrix());
    mvhd.extend([0u8; 24]);
    mvhd.extend((track_id + 1).to_be_bytes());
    let mvhd = fbx(b"mvhd", 0, 0, &mvhd);

    let (w, h) = match entry {
        Entry::Avc { w, h, .. } | Entry::Hevc { w, h, .. } => (*w as u32, *h as u32),
        _ => (0, 0),
    };
    let mut tkhd = Vec::new();
    tkhd.extend([0u8; 8]);
    tkhd.extend(track_id.to_be_bytes());
    tkhd.extend(0u32.to_be_bytes()); // reserved
    tkhd.extend(0u32.to_be_bytes()); // duration
    tkhd.extend([0u8; 8]);
    tkhd.extend(0u16.to_be_bytes()); // layer
    tkhd.extend(0u16.to_be_bytes()); // alternate_group
    tkhd.extend((if video { 0u16 } else { 0x0100 }).to_be_bytes());
    tkhd.extend(0u16.to_be_bytes());
    tkhd.extend(matrix());
    tkhd.extend((w << 16).to_be_bytes());
    tkhd.extend((h << 16).to_be_bytes());
    let tkhd = fbx(b"tkhd", 0, 3, &tkhd); // enabled | in_movie

    let mut mdhd = Vec::new();
    mdhd.extend([0u8; 8]);
    mdhd.extend(timescale.to_be_bytes());
    mdhd.extend(0u32.to_be_bytes());
    mdhd.extend(0x55C4u16.to_be_bytes()); // 'und'
    mdhd.extend(0u16.to_be_bytes());
    let mdhd = fbx(b"mdhd", 0, 0, &mdhd);

    let (handler, hname): (&[u8; 4], &[u8]) = if video { (b"vide", b"VideoHandler\0") } else { (b"soun", b"SoundHandler\0") };
    let hdlr = fbx(b"hdlr", 0, 0, &cat(&[&0u32.to_be_bytes(), handler, &[0u8; 12], hname]));

    let media_hd = if video { fbx(b"vmhd", 0, 1, &[0u8; 8]) } else { fbx(b"smhd", 0, 0, &[0u8; 4]) };
    let url = fbx(b"url ", 0, 1, &[]);
    let dref = fbx(b"dref", 0, 0, &cat(&[&1u32.to_be_bytes(), &url]));
    let dinf = bx(b"dinf", &dref);

    let stsd = fbx(b"stsd", 0, 0, &cat(&[&1u32.to_be_bytes(), &sample_entry(entry)]));
    let stts = fbx(b"stts", 0, 0, &0u32.to_be_bytes());
    let stsc = fbx(b"stsc", 0, 0, &0u32.to_be_bytes());
    let stsz = fbx(b"stsz", 0, 0, &[0u8; 8]);
    let stco = fbx(b"stco", 0, 0, &0u32.to_be_bytes());
    let stbl = bx(b"stbl", &cat(&[&stsd, &stts, &stsc, &stsz, &stco]));
    let minf = bx(b"minf", &cat(&[&media_hd, &dinf, &stbl]));
    let mdia = bx(b"mdia", &cat(&[&mdhd, &hdlr, &minf]));
    let trak = bx(b"trak", &cat(&[&tkhd, &mdia]));

    let mut trex = Vec::new();
    trex.extend(track_id.to_be_bytes());
    trex.extend(1u32.to_be_bytes()); // default_sample_description_index
    trex.extend([0u8; 12]); // duration, size, flags
    let mvex = bx(b"mvex", &fbx(b"trex", 0, 0, &trex));

    let moov = bx(b"moov", &cat(&[&mvhd, &trak, &mvex]));
    cat(&[&ftyp, &moov])
}

fn sample_entry(e: &Entry) -> Vec<u8> {
    match e {
        Entry::Avc { avcc, w, h, colr } => {
            let mut kids = vec![bx(b"avcC", avcc)];
            kids.extend(colr.map(|c| colr_box(&c)));
            visual(b"avc1", *w, *h, &kids)
        }
        Entry::Hevc { hvcc, w, h, dv, dvh1, colr } => {
            // Sıra: hvcC, colr, dvcC/dvvC (colr kodek yapılandırmasından hemen sonra, DV
            // kutusundan önce; alt kutu sırası ISO 14496-12 için serbest, okuyucular arar).
            let mut kids = vec![bx(b"hvcC", hvcc)];
            kids.extend(colr.map(|c| colr_box(&c)));
            if let Some(d) = dv {
                kids.push(bx(&d.fourcc, &d.payload));
            }
            visual(if *dvh1 { b"dvh1" } else { b"hvc1" }, *w, *h, &kids)
        }
        Entry::Eac3 { dec3, channels, rate } => audio(b"ec-3", *channels, 16, *rate, &[bx(b"dec3", dec3)]),
        Entry::Ac3 { dac3, channels, rate } => audio(b"ac-3", *channels, 16, *rate, &[bx(b"dac3", dac3)]),
        Entry::Aac { asc, channels, rate } => audio(b"mp4a", *channels, 16, *rate, &[fbx(b"esds", 0, 0, &esds(asc))]),
        Entry::Flac { streaminfo, channels, rate, bits } => {
            // dfLa: tam kutu + metadata blokları; STREAMINFO tek ve son blok (0x80 | tip 0).
            let mut p = vec![0x80];
            p.extend(&(streaminfo.len() as u32).to_be_bytes()[1..]);
            p.extend(streaminfo);
            audio(b"fLaC", *channels, *bits, *rate, &[fbx(b"dfLa", 0, 0, &p)])
        }
        Entry::Opus { dops, channels, rate } => audio(b"Opus", *channels, 16, *rate, &[bx(b"dOps", dops)]),
    }
}

/// ISO `colr` kutusu, `nclx` türü: primaries/transfer/matrix u16 + full_range_flag(1)+7 ayrılmış bit.
pub fn colr_box(c: &ColourInfo) -> Vec<u8> {
    let mut p = Vec::with_capacity(11);
    p.extend(b"nclx");
    p.extend(c.primaries.to_be_bytes());
    p.extend(c.transfer.to_be_bytes());
    p.extend(c.matrix.to_be_bytes());
    p.push(if c.full_range { 0x80 } else { 0 });
    bx(b"colr", &p)
}

fn visual(fourcc: &[u8; 4], w: u16, h: u16, kids: &[Vec<u8>]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend([0u8; 6]);
    p.extend(1u16.to_be_bytes()); // data_reference_index
    p.extend([0u8; 16]); // pre_defined, reserved, pre_defined[3]
    p.extend(w.to_be_bytes());
    p.extend(h.to_be_bytes());
    p.extend(0x0048_0000u32.to_be_bytes()); // 72 dpi
    p.extend(0x0048_0000u32.to_be_bytes());
    p.extend(0u32.to_be_bytes());
    p.extend(1u16.to_be_bytes()); // frame_count
    p.extend([0u8; 32]); // compressorname
    p.extend(0x0018u16.to_be_bytes()); // depth
    p.extend(0xFFFFu16.to_be_bytes()); // pre_defined = -1
    for k in kids {
        p.extend(k);
    }
    bx(fourcc, &p)
}

fn audio(fourcc: &[u8; 4], channels: u16, bits: u16, rate: u32, kids: &[Vec<u8>]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend([0u8; 6]);
    p.extend(1u16.to_be_bytes());
    p.extend([0u8; 8]); // sürüm, revizyon, satıcı
    p.extend(channels.to_be_bytes());
    p.extend(bits.to_be_bytes());
    p.extend(0u32.to_be_bytes()); // pre_defined, reserved
    // 16.16 örnekleme hızı; 65535 Hz üstü alana sığmaz (gerçek hız kodek kutusunda).
    p.extend((if rate <= 0xFFFF { rate << 16 } else { 0 }).to_be_bytes());
    for k in kids {
        p.extend(k);
    }
    bx(fourcc, &p)
}

/// Genişleyebilir uzunluklu (7 bit/bayt) MPEG-4 betimleyici.
fn descr(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    let mut n = body.len();
    let mut len = vec![(n & 0x7F) as u8];
    n >>= 7;
    while n > 0 {
        len.push(0x80 | (n & 0x7F) as u8);
        n >>= 7;
    }
    len.reverse();
    v.extend(len);
    v.extend(body);
    v
}

fn esds(asc: &[u8]) -> Vec<u8> {
    let mut dc = vec![0x40, 0x15, 0, 0, 0]; // MPEG-4 audio, AudioStream, bufferSizeDB
    dc.extend([0u8; 8]); // maxBitrate, avgBitrate
    dc.extend(descr(5, asc));
    let mut es = vec![0, 0, 0]; // ES_ID, bayraklar
    es.extend(descr(4, &dc));
    es.extend(descr(6, &[2])); // SLConfig: MP4
    descr(3, &es)
}

// ---------------------------------------------------------------------------------------------
// Medya segmenti
// ---------------------------------------------------------------------------------------------

const SYNC_FLAGS: u32 = 0x0200_0000; // depends_on = 2 (bağımsız)
const NON_SYNC_FLAGS: u32 = 0x0101_0000; // depends_on = 1, non-sync

/// `moof` (mfhd, traf: tfhd default-base-is-moof, tfdt v1, trun v1) + `mdat`.
pub fn media_segment(seq: u32, track_id: u32, base_dts: u64, samples: &[Sample]) -> Vec<u8> {
    let build = |data_offset: i32| {
        let mfhd = fbx(b"mfhd", 0, 0, &seq.to_be_bytes());
        let tfhd = fbx(b"tfhd", 0, 0x02_0000, &track_id.to_be_bytes());
        let tfdt = fbx(b"tfdt", 1, 0, &base_dts.to_be_bytes());
        let mut t = Vec::with_capacity(8 + 16 * samples.len());
        t.extend((samples.len() as u32).to_be_bytes());
        t.extend(data_offset.to_be_bytes());
        for s in samples {
            t.extend(s.duration.to_be_bytes());
            t.extend((s.data.len() as u32).to_be_bytes());
            t.extend((if s.sync { SYNC_FLAGS } else { NON_SYNC_FLAGS }).to_be_bytes());
            t.extend(s.cto.to_be_bytes());
        }
        // 0x001 data-offset, 0x100 duration, 0x200 size, 0x400 flags, 0x800 cto
        let trun = fbx(b"trun", 1, 0x0000_0F01, &t);
        let traf = bx(b"traf", &cat(&[&tfhd, &tfdt, &trun]));
        bx(b"moof", &cat(&[&mfhd, &traf]))
    };
    let moof_len = build(0).len();
    let moof = build((moof_len + 8) as i32);
    let total: usize = samples.iter().map(|s| s.data.len()).sum();
    let mut out = Vec::with_capacity(moof.len() + 8 + total);
    out.extend(&moof);
    out.extend(((8 + total) as u32).to_be_bytes());
    out.extend(b"mdat");
    for s in samples {
        out.extend(s.data);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// dec3 / dac3 / dOps
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct BitW {
    buf: Vec<u8>,
    n: usize,
}

impl BitW {
    fn put(&mut self, bits: usize, val: u32) {
        for i in (0..bits).rev() {
            if self.n % 8 == 0 {
                self.buf.push(0);
            }
            if (val >> i) & 1 == 1
                && let Some(last) = self.buf.last_mut()
            {
                *last |= 1 << (7 - self.n % 8);
            }
            self.n += 1;
        }
    }
}

/// E-AC-3 erişim biriminin (bağımsız çerçeve + bağımlı çerçeveler) başından `dec3` yükü
/// (ETSI TS 102 366 Ek F.6). JOC varsa `flag_ec3_extension_type_a = 1` + `complexity_index_type_a`.
/// Tek bağımsız alt akış (num_ind_sub = 0) yazılır.
pub fn build_dec3(first_frame: &[u8]) -> Result<Vec<u8>> {
    let c = eac3_config(first_frame).ok_or_else(|| anyhow!("E-AC-3 çerçevesi çözülemedi"))?;
    let b = &c.bsi;
    let mut w = BitW::default();
    w.put(13, c.data_rate_kbps.min(0x1FFF));
    w.put(3, 0); // num_ind_sub - 1
    w.put(2, b.fscod as u32);
    w.put(5, b.bsid as u32);
    w.put(1, 0); // reserved
    w.put(1, 0); // asvc
    w.put(3, b.bsmod as u32);
    w.put(3, b.acmod as u32);
    w.put(1, b.lfeon as u32);
    w.put(3, 0); // reserved
    w.put(4, c.num_dep_sub as u32);
    if c.num_dep_sub > 0 {
        w.put(9, c.chan_loc as u32);
    } else {
        w.put(1, 0);
    }
    if b.joc {
        w.put(7, 0);
        w.put(1, 1); // flag_ec3_extension_type_a
        w.put(8, b.complexity.unwrap_or(0) as u32);
    }
    Ok(w.buf)
}

/// AC-3 senkron çerçevesinden `dac3` yükü (24 bit; ETSI TS 102 366 Ek F.4).
pub fn build_dac3(first_frame: &[u8]) -> Result<Vec<u8>> {
    let f = first_frame;
    if f.len() < 8 || f[0] != 0x0B || f[1] != 0x77 {
        bail!("AC-3 senkron sözcüğü yok");
    }
    let fscod = f[4] >> 6;
    let frmsizecod = f[4] & 0x3F;
    let bsid = f[5] >> 3;
    let bsmod = f[5] & 7;
    if fscod == 3 || frmsizecod > 37 {
        bail!("AC-3 fscod/frmsizecod geçersiz");
    }
    if bsid > 8 {
        bail!("AC-3 değil (bsid {bsid}); E-AC-3 için build_dec3 kullanın");
    }
    let acmod = f[6] >> 5;
    let mut pos = 3; // f[6]'dan başlayan bit konumu (acmod sonrası)
    if acmod & 1 != 0 && acmod != 1 {
        pos += 2; // cmixlev
    }
    if acmod & 4 != 0 {
        pos += 2; // surmixlev
    }
    if acmod == 2 {
        pos += 2; // dsurmod
    }
    let lfeon = (u16::from_be_bytes([f[6], f[7]]) >> (15 - pos)) & 1;
    let mut w = BitW::default();
    w.put(2, fscod as u32);
    w.put(5, bsid as u32);
    w.put(3, bsmod as u32);
    w.put(3, acmod as u32);
    w.put(1, lfeon as u32);
    w.put(5, (frmsizecod >> 1) as u32);
    w.put(5, 0);
    Ok(w.buf)
}

/// MKV `OpusHead` (little-endian) -> ISOBMFF `dOps` yükü (Version 0, büyük-endian).
pub fn opus_dops_from_head(head: &[u8]) -> Result<Vec<u8>> {
    if head.len() < 19 || &head[..8] != b"OpusHead" {
        bail!("OpusHead değil");
    }
    let ch = head[9];
    let pre_skip = u16::from_le_bytes([head[10], head[11]]);
    let rate = u32::from_le_bytes([head[12], head[13], head[14], head[15]]);
    let gain = i16::from_le_bytes([head[16], head[17]]);
    let family = head[18];
    let mut v = vec![0, ch];
    v.extend(pre_skip.to_be_bytes());
    v.extend(rate.to_be_bytes());
    v.extend(gain.to_be_bytes());
    v.push(family);
    if family != 0 {
        let map = head.get(19..19 + 2 + ch as usize).ok_or_else(|| anyhow!("OpusHead kanal eşlemesi kesik"))?;
        v.extend(map);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atmos::testdata::{ac3_frame, eac3_dependent_frame, eac3_joc_frame, eac3_plain_frame};
    use filmcraft_isobmff as mp4;

    /// Bir kutu dizisini gezer: `(fourcc, yük)`.
    fn boxes(d: &[u8]) -> Vec<([u8; 4], &[u8])> {
        let mut v = Vec::new();
        let mut o = 0;
        while o + 8 <= d.len() {
            let sz = u32::from_be_bytes(d[o..o + 4].try_into().unwrap()) as usize;
            assert!(sz >= 8 && o + sz <= d.len(), "bozuk kutu @{o}");
            v.push((d[o + 4..o + 8].try_into().unwrap(), &d[o + 8..o + sz]));
            o += sz;
        }
        assert_eq!(o, d.len(), "kutu dizisi tam bitmeli");
        v
    }
    fn child<'a>(d: &'a [u8], k: &[u8; 4]) -> Option<&'a [u8]> {
        boxes(d).into_iter().find(|(f, _)| f == k).map(|(_, p)| p)
    }
    fn path<'a>(d: &'a [u8], p: &[&[u8; 4]]) -> &'a [u8] {
        let mut cur = d;
        for k in p {
            cur = child(cur, k).unwrap_or_else(|| panic!("kutu yok: {}", String::from_utf8_lossy(*k)));
        }
        cur
    }
    fn stsd(init: &[u8]) -> &[u8] {
        path(init, &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"])
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
    fn hvcc() -> Vec<u8> {
        let mut v = vec![1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x78, 0xF0, 0, 0xFC, 0xFD, 0xF8, 0xF8, 0, 0, 0x0F];
        v.push(0); // numOfArrays
        v
    }

    fn bufs() -> Vec<Vec<u8>> {
        vec![vec![1u8; 40], vec![2u8; 17], vec![3u8; 5], vec![4u8; 99]]
    }
    fn mk<'a>(bufs: &'a [Vec<u8>], dur: &[u32], cto: &[i32], sync: &[bool]) -> Vec<Sample<'a>> {
        bufs.iter().enumerate().map(|(i, b)| Sample { data: b, duration: dur[i], cto: cto[i], sync: sync[i] }).collect()
    }

    #[test]
    fn avc_init_and_segment_roundtrip() {
        let entry = Entry::Avc { avcc: avcc(), w: 1920, h: 1080, colr: None };
        let init = init_segment(1, 90000, &entry);
        let b = bufs();
        let seg1 = media_segment(1, 1, 0, &mk(&b[..2], &[3000, 3000], &[6000, 0], &[true, false]));
        let seg2 = media_segment(2, 1, 6000, &mk(&b[2..], &[3000, 3000], &[3000, -3000], &[true, false]));
        let mut file = init.clone();
        file.extend(&seg1);
        file.extend(&seg2);
        let f = mp4::open(file.clone()).expect("açılır");
        assert!(f.fragmented);
        assert_eq!(f.fragment_count, 2);
        assert_eq!(f.tracks.len(), 1);
        let t = &f.tracks[0];
        assert_eq!(t.id, 1);
        assert_eq!(t.timescale, 90000);
        assert_eq!(t.samples.len(), 4);
        for (i, x) in b.iter().enumerate() {
            assert_eq!(&f.read_sample(&file, 0, i).unwrap(), x, "örnek {i}");
            assert_eq!(t.samples[i].duration, 3000);
        }
        assert_eq!(t.samples[0].dts, 0);
        assert_eq!(t.samples[0].pts, 6000);
        assert_eq!(t.samples[1].dts, 3000);
        assert_eq!(t.samples[3].dts, 9000);
        assert_eq!(t.samples[3].pts, 9000 - 3000, "negatif cto");
        assert!(t.samples[0].is_sync && !t.samples[1].is_sync && t.samples[2].is_sync && !t.samples[3].is_sync);
        assert!(matches!(t.entries[0].codec, mp4::CodecConfig::Avc(_)));
        assert_eq!(t.width, 1920);
        // ftyp + moov yapısı
        let top = boxes(&init);
        assert_eq!(&top[0].0, b"ftyp");
        assert_eq!(&top[0].1[..4], b"iso6");
        assert!(top[0].1.windows(4).any(|w| w == b"cmfc"));
        assert_eq!(&top[1].0, b"moov");
        let stbl = path(&init, &[b"moov", b"trak", b"mdia", b"minf", b"stbl"]);
        for k in [b"stts", b"stsc", b"stsz", b"stco"] {
            assert!(child(stbl, k).is_some());
        }
        assert!(child(path(&init, &[b"moov"]), b"mvex").is_some());
    }

    #[test]
    fn segment_tfdt_and_seq() {
        let b = vec![vec![9u8; 10]];
        let seg = media_segment(7, 1, 90000 * 12, &mk(&b, &[3000], &[0], &[true]));
        let moof = path(&seg, &[b"moof"]);
        let mfhd = child(moof, b"mfhd").unwrap();
        assert_eq!(u32::from_be_bytes(mfhd[4..8].try_into().unwrap()), 7);
        let tfdt = path(moof, &[b"traf", b"tfdt"]);
        assert_eq!(tfdt[0], 1, "tfdt v1");
        assert_eq!(u64::from_be_bytes(tfdt[4..12].try_into().unwrap()), 90000 * 12);
        let tfhd = path(moof, &[b"traf", b"tfhd"]);
        assert_eq!(u32::from_be_bytes([0, tfhd[1], tfhd[2], tfhd[3]]) & 0x20000, 0x20000, "default-base-is-moof");
        let trun = path(moof, &[b"traf", b"trun"]);
        assert_eq!(trun[0], 1, "trun v1");

        let mut file = init_segment(1, 90000, &Entry::Avc { avcc: avcc(), w: 640, h: 360, colr: None });
        file.extend(&seg);
        let f = mp4::open(file).unwrap();
        assert_eq!(f.tracks[0].samples[0].dts, 90000 * 12);
        assert_eq!(f.tracks[0].samples[0].dts / 90000, 12);
    }

    #[test]
    fn colr_nclx_bytes_exact_and_order() {
        let pq = ColourInfo { primaries: 9, transfer: 16, matrix: 9, full_range: false };
        assert_eq!(
            colr_box(&pq),
            [0, 0, 0, 0x13, b'c', b'o', b'l', b'r', b'n', b'c', b'l', b'x', 0, 9, 0, 16, 0, 9, 0]
        );
        let full = ColourInfo { full_range: true, ..pq };
        assert_eq!(*colr_box(&full).last().unwrap(), 0x80);

        let dv = DoviBox { fourcc: *b"dvvC", payload: vec![0; 24] };
        let init = init_segment(1, 90000, &Entry::Hevc { hvcc: hvcc(), w: 3840, h: 2160, dv: Some(dv), dvh1: false, colr: Some(pq) });
        let entries = boxes(&stsd(&init)[8..]);
        let kids = boxes(&entries[0].1[78..]);
        let names: Vec<_> = kids.iter().map(|(k, _)| *k).collect();
        assert_eq!(names, vec![*b"hvcC", *b"colr", *b"dvvC"]);
        assert_eq!(kids[1].1, &colr_box(&pq)[8..]);
        let f = mp4::open(init).unwrap();
        match &f.tracks[0].video().unwrap().color {
            Some(mp4::ColorInfo::Nclx { primaries: 9, transfer: 16, matrix: 9, full_range: false }) => {}
            other => panic!("colr okunamadı: {other:?}"),
        }

        let init = init_segment(1, 90000, &Entry::Avc { avcc: avcc(), w: 640, h: 360, colr: Some(pq) });
        let kids = boxes(&boxes(&stsd(&init)[8..])[0].1[78..]);
        assert_eq!(kids.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![*b"avcC", *b"colr"]);
        assert!(mp4::open(init).is_ok());
    }

    #[test]
    fn dvcc_box_present_for_dv() {
        let payload = vec![1, 0, 0x2A, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let dv = DoviBox { fourcc: *b"dvvC", payload: payload.clone() };
        let init = init_segment(1, 90000, &Entry::Hevc { hvcc: hvcc(), w: 3840, h: 2160, dv: Some(dv), dvh1: false, colr: None });
        let entries = boxes(&stsd(&init)[8..]);
        assert_eq!(&entries[0].0, b"hvc1");
        // görsel örnek girişi: 78 bayt alan, ardından alt kutular
        let kids = boxes(&entries[0].1[78..]);
        let names: Vec<_> = kids.iter().map(|(k, _)| *k).collect();
        assert_eq!(names, vec![*b"hvcC", *b"dvvC"]);
        assert_eq!(kids[1].1, &payload[..]);

        let init = init_segment(1, 90000, &Entry::Hevc { hvcc: hvcc(), w: 3840, h: 2160, dv: Some(DoviBox { fourcc: *b"dvcC", payload }), dvh1: true, colr: None });
        let entries = boxes(&stsd(&init)[8..]);
        assert_eq!(&entries[0].0, b"dvh1");
        let kids = boxes(&entries[0].1[78..]);
        assert_eq!(&kids[1].0, b"dvcC");

        // DV yok: düz hvc1, dvvC yok
        let init = init_segment(1, 90000, &Entry::Hevc { hvcc: hvcc(), w: 1280, h: 720, dv: None, dvh1: false, colr: None });
        let entries = boxes(&stsd(&init)[8..]);
        assert_eq!(&entries[0].0, b"hvc1");
        assert_eq!(boxes(&entries[0].1[78..]).len(), 1);
        let f = mp4::open(init).unwrap();
        assert!(matches!(f.tracks[0].entries[0].codec, mp4::CodecConfig::Hevc(_)));
    }

    #[test]
    fn dec3_signals_joc() {
        let d = build_dec3(&eac3_joc_frame(16)).unwrap();
        // 13+3 bit hız/sayı, 24 bit bağımsız alt akış, 16 bit JOC uzantısı
        assert_eq!(d.len(), 2 + 3 + 2);
        let v = u16::from_be_bytes([d[0], d[1]]);
        assert_eq!(v >> 3, 24, "data_rate kb/s (96 bayt / 6 blok / 48 kHz)");
        assert_eq!(v & 7, 0, "num_ind_sub-1");
        let s = u32::from_be_bytes([0, d[2], d[3], d[4]]);
        assert_eq!(s >> 22, 0, "fscod 48k");
        assert_eq!((s >> 17) & 31, 16, "bsid");
        assert_eq!((s >> 12) & 7, 0, "bsmod");
        assert_eq!((s >> 9) & 7, 7, "acmod");
        assert_eq!((s >> 8) & 1, 1, "lfeon");
        assert_eq!((s >> 1) & 15, 0, "num_dep_sub");
        assert_eq!(d[5] & 1, 1, "flag_ec3_extension_type_a");
        assert_eq!(d[6], 16, "complexity_index_type_a");

        let p = build_dec3(&eac3_plain_frame()).unwrap();
        assert_eq!(p.len(), 5, "JOC yok -> uzantı yok");
    }

    #[test]
    fn dec3_counts_dependent_substreams() {
        let mut au = eac3_plain_frame();
        au.extend(eac3_dependent_frame());
        let d = build_dec3(&au).unwrap();
        assert_eq!(d.len(), 2 + 4);
        let s = u32::from_be_bytes([d[2], d[3], d[4], d[5]]);
        assert_eq!((s >> 9) & 15, 1, "num_dep_sub");
        assert_eq!(s & 0x1FF, 0, "chan_loc (sentetik bağımlı çerçevede chanmap yok)");
        assert!(build_dec3(&[0, 1, 2, 3]).is_err());
    }

    #[test]
    fn dac3_from_ac3_frame() {
        let d = build_dac3(&ac3_frame()).unwrap();
        assert_eq!(d.len(), 3);
        let v = u32::from_be_bytes([0, d[0], d[1], d[2]]);
        assert_eq!(v >> 22, 0, "fscod");
        assert_eq!((v >> 17) & 31, 8, "bsid");
        assert_eq!((v >> 14) & 7, 0, "bsmod");
        assert_eq!((v >> 11) & 7, 7, "acmod");
        assert_eq!((v >> 10) & 1, 0, "lfeon");
        assert_eq!((v >> 5) & 31, 7, "bit_rate_code = frmsizecod>>1");
        assert!(build_dac3(&eac3_joc_frame(16)).is_err(), "E-AC-3 dac3 olamaz");
    }

    #[test]
    fn audio_entries() {
        let dec3 = build_dec3(&eac3_joc_frame(16)).unwrap();
        let cases: Vec<(Entry, &[u8; 4], &[u8; 4])> = vec![
            (Entry::Eac3 { dec3: dec3.clone(), channels: 2, rate: 48000 }, b"ec-3", b"dec3"),
            (Entry::Ac3 { dac3: vec![0x10, 0x3D, 0xE0], channels: 6, rate: 48000 }, b"ac-3", b"dac3"),
            (Entry::Aac { asc: vec![0x12, 0x10], channels: 2, rate: 44100 }, b"mp4a", b"esds"),
            (Entry::Flac { streaminfo: vec![7; 34], channels: 2, rate: 44100, bits: 16 }, b"fLaC", b"dfLa"),
            (Entry::Opus { dops: vec![0, 2, 0, 0, 0, 0, 0xBB, 0x80, 0, 0, 0], channels: 2, rate: 48000 }, b"Opus", b"dOps"),
        ];
        for (e, fourcc, inner) in cases {
            let init = init_segment(2, 48000, &e);
            let ents = boxes(&stsd(&init)[8..]);
            assert_eq!(&ents[0].0, fourcc);
            let kids = boxes(&ents[0].1[28..]);
            assert_eq!(&kids[0].0, inner, "{}", String::from_utf8_lossy(fourcc));
            let hdlr = path(&init, &[b"moov", b"trak", b"mdia", b"hdlr"]);
            assert_eq!(&hdlr[8..12], b"soun");
            assert!(child(path(&init, &[b"moov", b"trak", b"mdia", b"minf"]), b"smhd").is_some());
            mp4::open(init).expect("filmcraft açar");
        }
        // dec3 baytları kutuda aynen
        let init = init_segment(2, 48000, &Entry::Eac3 { dec3: dec3.clone(), channels: 2, rate: 48000 });
        let ents = boxes(&stsd(&init)[8..]);
        assert_eq!(boxes(&ents[0].1[28..])[0].1, &dec3[..]);
        // FLAC dfLa: tam kutu + son-blok işaretli STREAMINFO başlığı
        let init = init_segment(2, 44100, &Entry::Flac { streaminfo: vec![7; 34], channels: 2, rate: 44100, bits: 16 });
        let ents = boxes(&stsd(&init)[8..]);
        let dfla = boxes(&ents[0].1[28..])[0].1;
        assert_eq!(&dfla[..8], &[0, 0, 0, 0, 0x80, 0, 0, 34]);
    }

    #[test]
    fn aac_audio_roundtrip_and_esds_asc() {
        let asc = vec![0x12, 0x10];
        let init = init_segment(2, 44100, &Entry::Aac { asc: asc.clone(), channels: 2, rate: 44100 });
        let ents = boxes(&stsd(&init)[8..]);
        let esds = boxes(&ents[0].1[28..])[0].1;
        assert!(esds.windows(4).any(|w| w == [5, 2, 0x12, 0x10]), "DecoderSpecificInfo");
        let b = vec![vec![0xAAu8; 21], vec![0xBBu8; 33]];
        let seg = media_segment(1, 2, 44100, &mk(&b, &[1024, 1024], &[0, 0], &[true, true]));
        let mut file = init;
        file.extend(&seg);
        let f = mp4::open(file.clone()).unwrap();
        assert!(f.fragmented);
        assert_eq!(f.tracks[0].samples[0].dts, 44100);
        assert_eq!(f.read_sample(&file, 0, 1).unwrap(), b[1]);
    }

    #[test]
    fn opus_dops_from_head() {
        let mut head = b"OpusHead".to_vec();
        head.extend([1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]); // sürüm, kanal, preskip 312, 48000 Hz, kazanç 0, aile 0
        let d = super::opus_dops_from_head(&head).unwrap();
        assert_eq!(d, vec![0, 2, 0x01, 0x38, 0, 0, 0xBB, 0x80, 0, 0, 0]);
        assert!(super::opus_dops_from_head(b"nope").is_err());
    }
}
