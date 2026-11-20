//! SPS'ten yeniden sıralama derinliği: HEVC `sps_max_num_reorder_pics[en yüksek tid]`, AVC VUI
//! `max_num_reorder_frames` (yalnızca `bitstream_restriction_flag` varsa).

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn bit(&mut self) -> Option<u32> {
        let b = *self.d.get(self.pos / 8)?;
        let v = (b >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(v as u32)
    }
    fn bits(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u64;
        }
        Some(v as u32)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        let np = self.pos.checked_add(n)?;
        (np <= self.d.len().checked_mul(8)?).then(|| self.pos = np)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 32 {
                return None;
            }
        }
        u32::try_from((1u64 << zeros) - 1 + self.bits(zeros)? as u64).ok()
    }
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}

/// Emülasyon önleme baytlarını (00 00 03) kaldırır.
fn unescape(d: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(d.len());
    let mut zeros = 0;
    for &b in d {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Kodek yapılandırma kaydındaki (avcC/hvcC) ilk SPS NAL'ı.
fn first_sps(codec: &str, cp: &[u8]) -> Option<Vec<u8>> {
    match codec {
        "h264" => {
            if cp.len() < 8 || cp[5] & 0x1F == 0 {
                return None;
            }
            let n = u16::from_be_bytes([cp[6], cp[7]]) as usize;
            Some(cp.get(8..8 + n)?.to_vec())
        }
        "hevc" => {
            let mut p = 23usize;
            for _ in 0..*cp.get(22)? {
                let ty = cp.get(p)? & 0x3F;
                let cnt = u16::from_be_bytes([*cp.get(p + 1)?, *cp.get(p + 2)?]) as usize;
                p += 3;
                for _ in 0..cnt {
                    let n = u16::from_be_bytes([*cp.get(p)?, *cp.get(p + 1)?]) as usize;
                    if ty == 33 {
                        return Some(cp.get(p + 2..p + 2 + n)?.to_vec());
                    }
                    p += 2 + n;
                }
            }
            None
        }
        _ => None,
    }
}

/// VUI `colour_description` alanları (H.273 kodları) ve `video_full_range_flag`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VuiColour {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

/// SPS'ten çıkarılanlar; her alan kendi bölümüne kadar ayrıştırma başarılıysa dolar.
#[derive(Default)]
struct SpsInfo {
    reorder: Option<u32>,
    colour: Option<VuiColour>,
}

fn parse_sps(codec: &str, codec_private: &[u8]) -> SpsInfo {
    let mut out = SpsInfo::default();
    let Some(nal) = first_sps(codec, codec_private) else { return out };
    // Hatalı/kesik SPS'te `?` ile erken dönülür; o ana kadar dolan alanlar korunur.
    match codec {
        "hevc" => {
            if let Some(r) = nal.get(2..) {
                let _ = hevc_parse(&unescape(r), &mut out);
            }
        }
        _ => {
            if let Some(r) = nal.get(1..) {
                let _ = avc_parse(&unescape(r), &mut out);
            }
        }
    }
    out
}

/// Yeniden sıralanan en çok resim sayısı; bilinmiyorsa `None`.
pub fn max_num_reorder(codec: &str, codec_private: &[u8]) -> Option<u32> {
    parse_sps(codec, codec_private).reorder
}

/// SPS VUI'sindeki renk tanımı (`colour_description_present_flag` varsa); yoksa `None`.
pub fn vui_colour(codec: &str, codec_private: &[u8]) -> Option<VuiColour> {
    parse_sps(codec, codec_private).colour
}

/// `video_signal_type_present_flag` alt yapısı (HEVC ve AVC aynı): `video_format`(3),
/// `video_full_range_flag`(1), `colour_description_present_flag`(1), 3x8 bit.
fn video_signal_type(b: &mut Bits) -> Option<Option<VuiColour>> {
    b.skip(3)?;
    let full_range = b.bit()? == 1;
    if b.bit()? == 0 {
        return Some(None);
    }
    let (primaries, transfer, matrix) = (b.bits(8)? as u8, b.bits(8)? as u8, b.bits(8)? as u8);
    Some(Some(VuiColour { primaries, transfer, matrix, full_range }))
}

fn hevc_parse(r: &[u8], out: &mut SpsInfo) -> Option<()> {
    let mut b = Bits { d: r, pos: 0 };
    b.skip(4)?; // vps id
    let max_sub = b.bits(3)? as usize;
    b.skip(1)?;
    // profile_tier_level: genel 88 bit + seviye 8 bit
    b.skip(96)?;
    let mut pp = [false; 8];
    let mut lp = [false; 8];
    for i in 0..max_sub {
        pp[i] = b.bit()? == 1;
        lp[i] = b.bit()? == 1;
    }
    if max_sub > 0 {
        b.skip(2 * (8 - max_sub))?;
    }
    for i in 0..max_sub {
        if pp[i] {
            b.skip(88)?;
        }
        if lp[i] {
            b.skip(8)?;
        }
    }
    b.ue()?; // sps id
    if b.ue()? == 3 {
        b.skip(1)?;
    }
    b.ue()?;
    b.ue()?;
    if b.bit()? == 1 {
        for _ in 0..4 {
            b.ue()?;
        }
    }
    b.ue()?;
    b.ue()?;
    let log2_poc_lsb = b.ue()?.checked_add(4)? as usize;
    let present = b.bit()? == 1;
    let mut reorder = 0;
    for _ in (if present { 0 } else { max_sub })..=max_sub {
        b.ue()?;
        reorder = b.ue()?;
        b.ue()?;
    }
    out.reorder = Some(reorder);
    // Renk bilgisi için VUI'ye kadar ilerle.
    for _ in 0..4 {
        b.ue()?; // log2 min/diff luma coding + transform block
    }
    b.ue()?; // max_transform_hierarchy_depth_inter
    b.ue()?; // ..._intra
    if b.bit()? == 1 && b.bit()? == 1 {
        hevc_scaling_list_data(&mut b)?;
    }
    b.skip(2)?; // amp, sao
    if b.bit()? == 1 {
        b.skip(8)?;
        b.ue()?;
        b.ue()?;
        b.skip(1)?;
    }
    let n_sets = b.ue()?;
    if n_sets > 64 {
        return None;
    }
    let mut num_delta = Vec::with_capacity(n_sets as usize);
    for i in 0..n_sets as usize {
        let n = hevc_st_rps(&mut b, i, &num_delta)?;
        num_delta.push(n);
    }
    if b.bit()? == 1 {
        let n = b.ue()?;
        if n > 32 {
            return None;
        }
        for _ in 0..n {
            b.skip(log2_poc_lsb)?;
            b.skip(1)?;
        }
    }
    b.skip(2)?; // temporal_mvp, strong_intra_smoothing
    if b.bit()? == 0 {
        return Some(()); // VUI yok
    }
    if b.bit()? == 1 && b.bits(8)? == 255 {
        b.skip(32)?;
    }
    if b.bit()? == 1 {
        b.skip(1)?;
    }
    if b.bit()? == 1 {
        out.colour = video_signal_type(&mut b)?;
    }
    Some(())
}

fn hevc_scaling_list_data(b: &mut Bits) -> Option<()> {
    for size_id in 0..4 {
        let step = if size_id == 3 { 3 } else { 1 };
        for _ in (0..6).step_by(step) {
            if b.bit()? == 0 {
                b.ue()?;
            } else {
                let coefs = 64.min(1usize << (4 + (size_id << 1)));
                if size_id > 1 {
                    b.se()?;
                }
                for _ in 0..coefs {
                    b.se()?;
                }
            }
        }
    }
    Some(())
}

/// `st_ref_pic_set(idx)` (SPS içinde); dönüş: `NumDeltaPocs[idx]`.
fn hevc_st_rps(b: &mut Bits, idx: usize, prev: &[u32]) -> Option<u32> {
    let inter = idx != 0 && b.bit()? == 1;
    if inter {
        b.skip(1)?;
        b.ue()?;
        let n_ref = *prev.get(idx - 1)?;
        let mut kept = 0u32;
        for _ in 0..=n_ref {
            let used = b.bit()? == 1;
            if used || b.bit()? == 1 {
                kept += 1;
            }
        }
        Some(kept)
    } else {
        let neg = b.ue()?;
        let pos = b.ue()?;
        if neg > 16 || pos > 16 {
            return None;
        }
        for _ in 0..neg + pos {
            b.ue()?;
            b.skip(1)?;
        }
        Some(neg + pos)
    }
}

fn skip_hrd(b: &mut Bits) -> Option<()> {
    let cnt = b.ue()?.checked_add(1)?;
    b.skip(8)?;
    for _ in 0..cnt {
        b.ue()?;
        b.ue()?;
        b.skip(1)?;
    }
    b.skip(20)
}

fn avc_parse(r: &[u8], out: &mut SpsInfo) -> Option<()> {
    let mut b = Bits { d: r, pos: 0 };
    let profile = b.bits(8)?;
    b.skip(16)?;
    b.ue()?; // sps id
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let cf = b.ue()?;
        if cf == 3 {
            b.skip(1)?;
        }
        b.ue()?;
        b.ue()?;
        b.skip(1)?;
        if b.bit()? == 1 {
            for j in 0..(if cf != 3 { 8 } else { 12 }) {
                if b.bit()? == 1 {
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..(if j < 6 { 16 } else { 64 }) {
                        if next != 0 {
                            next = last.wrapping_add(b.se()?).wrapping_add(256).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    b.ue()?; // log2_max_frame_num
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.skip(1)?;
            b.se()?;
            b.se()?;
            for _ in 0..b.ue()? {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()?; // max_num_ref_frames
    b.skip(1)?;
    b.ue()?;
    b.ue()?;
    if b.bit()? == 0 {
        b.skip(1)?;
    }
    b.skip(1)?;
    if b.bit()? == 1 {
        for _ in 0..4 {
            b.ue()?;
        }
    }
    if b.bit()? == 0 {
        return Some(()); // VUI yok
    }
    if b.bit()? == 1 && b.bits(8)? == 255 {
        b.skip(32)?;
    }
    if b.bit()? == 1 {
        b.skip(1)?;
    }
    if b.bit()? == 1 {
        out.colour = video_signal_type(&mut b)?;
    }
    if b.bit()? == 1 {
        b.ue()?;
        b.ue()?;
    }
    if b.bit()? == 1 {
        b.skip(65)?;
    }
    let nal_hrd = b.bit()? == 1;
    if nal_hrd {
        skip_hrd(&mut b)?;
    }
    let vcl_hrd = b.bit()? == 1;
    if vcl_hrd {
        skip_hrd(&mut b)?;
    }
    if nal_hrd || vcl_hrd {
        b.skip(1)?;
    }
    b.skip(1)?; // pic_struct_present
    if b.bit()? == 0 {
        return Some(()); // bitstream_restriction yok
    }
    b.skip(1)?;
    for _ in 0..4 {
        b.ue()?;
    }
    out.reorder = Some(b.ue()?);
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    pub struct W {
        pub bits: Vec<u8>,
    }
    impl W {
        pub fn put(&mut self, n: usize, v: u64) {
            for i in (0..n).rev() {
                self.bits.push(if i < 64 { ((v >> i) & 1) as u8 } else { 0 });
            }
        }
        pub fn ue(&mut self, v: u32) {
            let x = v as u64 + 1;
            let len = 64 - x.leading_zeros() as usize;
            self.put(len - 1, 0);
            self.put(len, x);
        }
        pub fn bytes(mut self) -> Vec<u8> {
            self.put(1, 1); // rbsp_stop_bit
            while self.bits.len() % 8 != 0 {
                self.bits.push(0);
            }
            self.bits.chunks(8).map(|c| c.iter().fold(0u8, |a, b| (a << 1) | b)).collect()
        }
    }

    fn hevc_sps(reorder: u32) -> Vec<u8> {
        let mut w = W::default();
        w.put(4, 0);
        w.put(3, 0); // max_sub_layers_minus1
        w.put(1, 1);
        w.put(96, 0); // profile_tier_level
        w.ue(0); // sps id
        w.ue(1); // chroma 4:2:0
        w.ue(64);
        w.ue(64);
        w.put(1, 0); // conformance window
        w.ue(0);
        w.ue(0);
        w.ue(4);
        w.put(1, 1); // sub_layer_ordering_info_present
        w.ue(4); // max_dec_pic_buffering_minus1
        w.ue(reorder);
        w.ue(0);
        let mut nal = vec![0x42, 0x01];
        nal.extend(w.bytes());
        nal
    }

    fn hvcc_with(sps: &[u8]) -> Vec<u8> {
        let mut v = vec![1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x78, 0xF0, 0, 0xFC, 0xFD, 0xF8, 0xF8, 0, 0, 0x0F, 1];
        v.push(33);
        v.extend(1u16.to_be_bytes());
        v.extend((sps.len() as u16).to_be_bytes());
        v.extend(sps);
        v
    }

    #[test]
    fn hevc_reorder_from_sps() {
        for r in [0, 2, 4] {
            assert_eq!(max_num_reorder("hevc", &hvcc_with(&hevc_sps(r))), Some(r));
        }
        assert_eq!(max_num_reorder("hevc", &[1, 2, 3]), None);
    }

    fn avc_sps(reorder: Option<u32>) -> Vec<u8> {
        let mut w = W::default();
        w.put(8, 66); // baseline: kroma alanı yok
        w.put(16, 0);
        w.ue(0);
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(0); // poc type 0
        w.ue(0); // log2_max_poc_lsb_minus4
        w.ue(4); // max_num_ref_frames
        w.put(1, 0);
        w.ue(39);
        w.ue(29);
        w.put(1, 1); // frame_mbs_only
        w.put(1, 1); // direct_8x8
        w.put(1, 0); // cropping
        match reorder {
            None => w.put(1, 0),
            Some(r) => {
                w.put(1, 1); // vui
                w.put(1, 0); // aspect
                w.put(1, 0); // overscan
                w.put(1, 0); // video signal
                w.put(1, 0); // chroma loc
                w.put(1, 0); // timing
                w.put(1, 0);
                w.put(1, 0); // hrd
                w.put(1, 0); // pic struct
                w.put(1, 1); // bitstream_restriction
                w.put(1, 1);
                w.ue(2);
                w.ue(1);
                w.ue(16);
                w.ue(16);
                w.ue(r);
                w.ue(4);
            }
        }
        let mut nal = vec![0x67];
        nal.extend(w.bytes());
        nal
    }

    fn avcc_with(sps: &[u8]) -> Vec<u8> {
        let mut v = vec![1, 66, 0, 30, 0xFF, 0xE1];
        v.extend((sps.len() as u16).to_be_bytes());
        v.extend(sps);
        v.push(0);
        v
    }

    #[test]
    fn avc_reorder_from_vui() {
        assert_eq!(max_num_reorder("h264", &avcc_with(&avc_sps(Some(2)))), Some(2));
        assert_eq!(max_num_reorder("h264", &avcc_with(&avc_sps(Some(0)))), Some(0));
        assert_eq!(max_num_reorder("h264", &avcc_with(&avc_sps(None))), None);
    }

    /// Tam HEVC SPS (VUI'ye kadar): ölçek listesi + 2 st_ref_pic_set (biri inter) ile.
    /// `vui`: None = VUI yok; Some(None) = VUI var, renk tanımı yok; Some(Some(..)) = renk tanımlı.
    fn hevc_sps_full(vui: Option<Option<(u8, u8, u8, bool)>>, scaling: bool) -> Vec<u8> {
        let mut w = W::default();
        w.put(4, 0);
        w.put(3, 0);
        w.put(1, 1);
        w.put(96, 0);
        w.ue(0);
        w.ue(1);
        w.ue(3840);
        w.ue(2160);
        w.put(1, 0);
        w.ue(2); // bit depth luma 10
        w.ue(2);
        w.ue(4); // log2_max_poc_lsb_minus4
        w.put(1, 1);
        w.ue(4);
        w.ue(2); // reorder
        w.ue(0);
        w.ue(0);
        w.ue(3);
        w.ue(0);
        w.ue(3);
        w.ue(1);
        w.ue(1);
        if scaling {
            w.put(1, 1); // scaling_list_enabled
            w.put(1, 1); // data present
            for size_id in 0..4 {
                let step = if size_id == 3 { 3 } else { 1 };
                for m in (0..6).step_by(step) {
                    if m % 2 == 0 {
                        w.put(1, 0);
                        w.ue(0); // pred matrix delta
                    } else {
                        w.put(1, 1);
                        if size_id > 1 {
                            w.ue(0); // dc coef se(0)
                        }
                        for _ in 0..64.min(1usize << (4 + (size_id << 1))) {
                            w.ue(1); // se(+1)
                        }
                    }
                }
            }
        } else {
            w.put(1, 0);
        }
        w.put(1, 1); // amp
        w.put(1, 1); // sao
        w.put(1, 1); // pcm
        w.put(8, 0x77);
        w.ue(0);
        w.ue(0);
        w.put(1, 1);
        w.ue(2); // 2 st_ref_pic_sets
        w.ue(1); // set0: 1 neg
        w.ue(1); //       1 pos
        w.ue(0);
        w.put(1, 1);
        w.ue(0);
        w.put(1, 1);
        w.put(1, 1); // set1: inter
        w.put(1, 0);
        w.ue(0);
        w.put(1, 0); // j=0: not used
        w.put(1, 1); //      use_delta
        w.put(1, 1); // j=1: used
        w.put(1, 1); // j=2: used
        w.put(1, 1); // long_term_ref_pics_present
        w.ue(1);
        w.put(8, 0xA5); // lt_ref_pic_poc_lsb_sps (log2_max_poc_lsb = 8)
        w.put(1, 1);
        w.put(1, 1); // tmvp
        w.put(1, 1); // strong intra
        match vui {
            None => w.put(1, 0),
            Some(c) => {
                w.put(1, 1);
                w.put(1, 1); // aspect
                w.put(8, 255);
                w.put(32, 0x0001_0001);
                w.put(1, 1); // overscan present
                w.put(1, 0);
                w.put(1, 1); // video_signal_type_present
                w.put(3, 5);
                match c {
                    None => {
                        w.put(1, 0);
                        w.put(1, 0);
                    }
                    Some((p, t, m, full)) => {
                        w.put(1, full as u64);
                        w.put(1, 1);
                        w.put(8, p as u64);
                        w.put(8, t as u64);
                        w.put(8, m as u64);
                    }
                }
                w.put(1, 0);
            }
        }
        let mut nal = vec![0x42, 0x01];
        nal.extend(w.bytes());
        nal
    }

    #[test]
    fn hevc_vui_colour_pq() {
        for scaling in [false, true] {
            let cp = hvcc_with(&hevc_sps_full(Some(Some((9, 16, 9, false))), scaling));
            assert_eq!(
                vui_colour("hevc", &cp),
                Some(VuiColour { primaries: 9, transfer: 16, matrix: 9, full_range: false })
            );
            assert_eq!(max_num_reorder("hevc", &cp), Some(2));
        }
        let hlg = hvcc_with(&hevc_sps_full(Some(Some((9, 18, 9, true))), false));
        assert_eq!(vui_colour("hevc", &hlg).map(|c| (c.transfer, c.full_range)), Some((18, true)));
        assert_eq!(vui_colour("hevc", &hvcc_with(&hevc_sps_full(Some(None), false))), None);
        assert_eq!(vui_colour("hevc", &hvcc_with(&hevc_sps_full(None, false))), None);
    }

    #[test]
    fn vui_colour_robust_to_garbage() {
        let cp = hvcc_with(&hevc_sps_full(Some(Some((9, 16, 9, false))), true));
        // her kesme noktasında panik olmamalı
        for cut in 0..cp.len() {
            let _ = vui_colour("hevc", &cp[..cut]);
            let _ = max_num_reorder("hevc", &cp[..cut]);
        }
        let mut junk = hvcc_with(&[0x42, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(vui_colour("hevc", &junk), None);
        junk.clear();
        assert_eq!(vui_colour("hevc", &junk), None);
        assert_eq!(vui_colour("h264", &[1, 2, 3]), None);
    }

    #[test]
    fn avc_garbage_and_truncation_no_panic() {
        // High profile, scaling matrix present, hostile se() deltas (se ~ +/-2^31)
        let mut w = W::default();
        w.put(8, 100);
        w.put(16, 0);
        w.ue(0);
        w.ue(1); // chroma 4:2:0
        w.ue(0);
        w.ue(0);
        w.put(1, 0);
        w.put(1, 1); // seq_scaling_matrix_present
        for _ in 0..8 {
            w.put(1, 1);
            for _ in 0..64 {
                w.ue(u32::MAX - 1);
            }
        }
        let mut nal = vec![0x67];
        nal.extend(w.bytes());
        let cp = avcc_with(&nal);
        let _ = vui_colour("h264", &cp);
        let ok = avcc_with(&avc_sps(Some(1)));
        for cut in 0..=ok.len() {
            let _ = vui_colour("h264", &ok[..cut]);
            let _ = max_num_reorder("h264", &ok[..cut]);
        }
        let mut x = 0x1234_5678u32;
        for _ in 0..200 {
            let mut junk = vec![0x67, 100];
            for _ in 0..40 {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                junk.push((x >> 24) as u8);
            }
            let _ = vui_colour("h264", &avcc_with(&junk));
        }
    }

    #[test]
    fn avc_vui_colour() {
        let mut w = W::default();
        w.put(8, 66);
        w.put(16, 0);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(4);
        w.put(1, 0);
        w.ue(39);
        w.ue(29);
        w.put(1, 1);
        w.put(1, 1);
        w.put(1, 0);
        w.put(1, 1); // vui
        w.put(1, 0); // aspect
        w.put(1, 0); // overscan
        w.put(1, 1); // video signal
        w.put(3, 5);
        w.put(1, 0);
        w.put(1, 1);
        w.put(8, 9);
        w.put(8, 18);
        w.put(8, 9);
        w.put(1, 0); // chroma loc
        w.put(1, 0); // timing
        w.put(1, 0);
        w.put(1, 0);
        w.put(1, 0);
        w.put(1, 0);
        let mut nal = vec![0x67];
        nal.extend(w.bytes());
        assert_eq!(
            vui_colour("h264", &avcc_with(&nal)),
            Some(VuiColour { primaries: 9, transfer: 18, matrix: 9, full_range: false })
        );
        assert_eq!(vui_colour("h264", &avcc_with(&avc_sps(Some(1)))), None);
    }

    /// PQ (primaries 9, transfer 16, matrix 9, limited) VUI'li hvcC.
    pub fn hvcc_pq_for_test() -> Vec<u8> {
        hvcc_with(&hevc_sps_full(Some(Some((9, 16, 9, false))), false))
    }

    pub fn hvcc_for_test(reorder: u32) -> Vec<u8> {
        hvcc_with(&hevc_sps(reorder))
    }
}

#[cfg(test)]
pub(crate) use tests::hvcc_for_test;
#[cfg(test)]
pub(crate) use tests::hvcc_pq_for_test;
