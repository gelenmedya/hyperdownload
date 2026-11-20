//! Kayan pencere politikası (iOS, `StorageMode::Window`): oynatma konumundan tutulacak bayt
//! aralığını hesaplar ve seçim + depolama tahliyesini uygular.
//!
//! librqbit dosyaları bütün olarak seçtiği için pencere dışı baytları indirip sonra silmek
//! bant genişliği israfı olur; bu yüzden `apply` önce parça düzeyinde seçimi
//! (`ManagedTorrent::set_selected_pieces`, vendor yaması) pencereye daraltır, sonra
//! `WindowStore::set_keep` ile dışarıda kalanları siler ve silinenleri librqbit'e unutturur.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use librqbit::ManagedTorrent;

use crate::demux::{MetaLoc, TimeIndex, locate_meta};
use crate::window_store::{EvictReport, WindowStore};

pub const WINDOW_BEHIND_SECS: f64 = 300.0;
pub const WINDOW_AHEAD_SECS: f64 = 300.0;
/// Boştaki (henüz oynatılmamış) torrent'te devam konumu çevresinde tutulan pencere: oynatma bir
/// konum bildirene dek (`report`) ±300 sn yerine bu küçük pencere indirilir (4K'da film başına ~GB tasarrufu).
pub const IDLE_BEHIND_SECS: f64 = 10.0;
pub const IDLE_AHEAD_SECS: f64 = 60.0;
/// Kapsayıcı başlığı için üst sınır (ilk Cluster/`moov` öncesi baytlar).
pub const HEADER_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Zaman dizini yokken korunan başlık boyu.
pub const HEADER_FALLBACK_BYTES: u64 = 8 * 1024 * 1024;
/// Oynatma konumu bu kadar sn'den fazla sıçrarsa bir tick eski+yeni pencere birlikte korunur.
pub const JUMP_SECS: f64 = 30.0;
/// Seçim değişmese de tahliye bu aralıkla yeniden süpürülür (pencere dışına düşüp sonradan
/// tamamlanan uçuştaki parçalar için).
const KEEP_REFRESH: Duration = Duration::from_secs(4);
/// Zaman dizini yokken pencere her yönde bu oranda genişletilir (VBR payı).
const NO_INDEX_MARGIN: f64 = 0.20;

/// `pos_secs - 300 .. pos_secs + 300` aralığının dosya baytları, `0..file_len` ile kırpılmış.
pub fn keep_range(pos_secs: f64, idx: &TimeIndex, file_len: u64, avg_bps: f64) -> Range<u64> {
    keep_range_with(pos_secs, idx, file_len, avg_bps, WINDOW_BEHIND_SECS, WINDOW_AHEAD_SECS)
}

/// `keep_range` ile aynı, geri/ileri süreleri verilebilir (boşta bakım için küçük pencere).
pub fn keep_range_with(pos_secs: f64, idx: &TimeIndex, file_len: u64, avg_bps: f64, behind: f64, ahead: f64) -> Range<u64> {
    let a = idx.byte_at(pos_secs - behind, file_len, avg_bps);
    let b = idx.byte_at(pos_secs + ahead, file_len, avg_bps);
    a..b.max(a)
}

/// Her zaman korunan kapsayıcı başlığı: `0..min(ilk indeksli bayt, 32 MiB, dosya)`.
pub fn header_range(idx: &TimeIndex, file_len: u64) -> Range<u64> {
    let end = idx.header_end().unwrap_or(HEADER_FALLBACK_BYTES);
    0..end.min(HEADER_MAX_BYTES).min(file_len)
}

/// Dosya düzeyinde meta aralıkları: kapsayıcı başlığı + indeks bölgesi (MKV Cues / MP4 `moov`,
/// dosyanın sonunda olsa da). Zaman dizini yüklenince ilk indeksli bayta kadar başlık da eklenir.
pub fn meta_ranges(loc: &MetaLoc, idx: &TimeIndex, file_len: u64) -> Vec<Range<u64>> {
    let mut v: Vec<Range<u64>> = loc.ranges.iter().map(|r| r.start.min(file_len)..r.end.min(file_len)).collect();
    if v.is_empty() || !idx.points.is_empty() {
        v.push(header_range(idx, file_len));
    }
    v
}

/// `range` (dosya içi, yarı açık) baytlarına değen parçaları `out`ta işaretler.
fn pieces_of(range: &Range<u64>, offset: u64, piece_len: u64, out: &mut [bool]) {
    if range.start >= range.end {
        return;
    }
    let first = (offset + range.start) / piece_len;
    let last = (offset + range.end).div_ceil(piece_len); // dışlayıcı
    for p in first..last.min(out.len() as u64) {
        out[p as usize] = true;
    }
}

/// Seçim vektörü: diğer seçili dosyalar (dosya düzeyinde) + video dosyasının `ranges` parçaları.
fn compute_selection(
    handle: &ManagedTorrent,
    video_file: usize,
    ranges: &[&Range<u64>],
    side: &HashSet<usize>,
) -> anyhow::Result<Vec<bool>> {
    let meta = handle.metadata.load_full().context("meta veri yok")?;
    let lengths = meta.info.lengths();
    let total = lengths.total_pieces() as usize;
    let piece_len = lengths.default_piece_length() as u64;
    let vf = meta.file_infos.get(video_file).context("video dosyası yok")?;
    let mut sel = vec![false; total];
    let only = handle.only_files();
    let names: Vec<String> = meta.info.iter_file_details().map(|d| d.filename.to_string()).collect();
    for (i, fi) in meta.file_infos.iter().enumerate() {
        if i == video_file || fi.attrs.padding {
            continue;
        }
        // Kullanıcının seçtiği diğer videolar, oynatılana dek indirilmez (pencere kipi hiçbir videoyu
        // bütün olarak indirmez); altyazı/ses gibi küçük eklentiler dosya düzeyinde seçilir.
        if names.get(i).is_some_and(|n| crate::media::kind_of(n) == crate::media::FileKind::Video) {
            continue;
        }
        // Yan dosyalar (HLS harici altyazıları) denetleyici tarafından açıkça tutulur.
        if side.contains(&i) || only.as_ref().is_none_or(|o| o.contains(&i)) {
            for p in fi.piece_range_usize() {
                if let Some(s) = sel.get_mut(p) {
                    *s = true;
                }
            }
        }
    }
    for r in ranges {
        pieces_of(r, vf.offset_in_torrent, piece_len, &mut sel);
    }
    Ok(sel)
}

/// Bir (torrent, video dosyası) için pencere denetleyicisi: meta konumu, zaman dizini ve uygulanan
/// son seçim. Engelleyen işler burada; `Buffering` bunu `spawn_blocking` içinde çağırır.
pub struct WindowCtl {
    pub loc: MetaLoc,
    pub index: Option<Arc<TimeIndex>>,
    tries: u32,
    next_try: Instant,
    last_sel: Option<Vec<bool>>,
    last_keep_at: Instant,
    last_secs: Option<f64>,
    last_keep: Option<Range<u64>>,
    /// Tanılama/test: `set_selected_pieces` ve `set_keep` çağrı sayıları.
    pub vendor_calls: u32,
    pub keep_calls: u32,
    /// Geçici "canlı tut" aralıkları (HLS ses/altyazı segmentleri, `ensure_range` aralığı): TTL
    /// dolana dek her tick'e eklenir; oynatma konumunu taşımaz.
    extras: Vec<(Range<u64>, Instant)>,
    /// Son tick'in tuttuğu aralıklar (keep-alive uygularken mevcut pencere tahliye edilmesin).
    last_keeps: Vec<Range<u64>>,
    /// Dosya düzeyinde seçili küçük yan dosyalar (harici altyazılar).
    side_files: HashSet<usize>,
    /// `(video dosyasının torrent içi başlangıcı, parça uzunluğu)`: `is_selected_at` için.
    geom: Option<(u64, u64)>,
    /// Dizin okunabilir bayt üzerinde ayrıştırılamadı (desteklenmeyen/bozuk kapsayıcı).
    index_unreadable: bool,
    /// Eksik bayt yüzünden değil, kapsayıcı ayrıştırılamadığı için başarısız denemeler.
    unparseable_tries: u32,
}

/// Ayrıştırılamayan kapsayıcı için bu kadar denemeden sonra vazgeçilir (hata durumu korunur).
const MAX_UNPARSEABLE_TRIES: u32 = 20;

/// `keep_alive` aralıklarının yaşam süresi.
pub const KEEP_ALIVE_TTL: Duration = Duration::from_secs(30);

impl Default for WindowCtl {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowCtl {
    pub fn new() -> Self {
        Self {
            loc: MetaLoc::default(),
            index: None,
            tries: 0,
            next_try: Instant::now(),
            last_sel: None,
            last_keep_at: Instant::now(),
            last_secs: None,
            last_keep: None,
            vendor_calls: 0,
            keep_calls: 0,
            extras: Vec::new(),
            last_keeps: Vec::new(),
            side_files: HashSet::new(),
            geom: None,
            index_unreadable: false,
            unparseable_tries: 0,
        }
    }

    /// Uygulanan son seçimi unutur: bir sonraki `apply` seçimi (örn. `only_files` daraltıldıktan
    /// sonra) yeniden gönderir.
    pub fn reset_selection(&mut self) {
        self.last_sel = None;
    }

    /// Süresi dolmamış keep-alive aralıkları.
    fn live_extras(&mut self) -> Vec<Range<u64>> {
        self.extras.retain(|(_, t)| t.elapsed() < KEEP_ALIVE_TTL);
        self.extras.iter().map(|(r, _)| r.clone()).collect()
    }

    pub fn add_extra(&mut self, range: Range<u64>) {
        if range.start < range.end {
            self.extras.push((range, Instant::now()));
        }
    }

    /// Mevcut pencere + canlı keep-alive aralıklarıyla seçimi hemen uygular (tahliye süpürmesi zorlanır).
    fn apply_now(&mut self, handle: &ManagedTorrent, store: &Arc<WindowStore>, file: usize, len: u64) -> anyhow::Result<()> {
        self.refresh_meta(store, file, len);
        let idx = self.index.clone().unwrap_or_default();
        let meta = meta_ranges(&self.loc, &idx, len);
        let mut keeps = self.last_keeps.clone();
        keeps.extend(self.live_extras());
        self.last_keep_at = Instant::now() - KEEP_REFRESH;
        self.apply(handle, store, file, &meta, &keeps)?;
        Ok(())
    }

    /// `range`i `KEEP_ALIVE_TTL` boyunca seçili tutar (mevcut pencereyle birlikte); oynatma
    /// konumunu TAŞIMAZ, mevcut pencereyi tahliye ETMEZ.
    pub fn keep_alive(&mut self, handle: &ManagedTorrent, store: &Arc<WindowStore>, file: usize, len: u64, range: Range<u64>) -> anyhow::Result<()> {
        self.add_extra(range.start.min(len)..range.end.min(len));
        self.apply_now(handle, store, file, len)
    }

    /// Dosya düzeyinde seçili yan dosya ekler (harici altyazı) ve seçimi hemen uygular.
    pub fn add_side_file(&mut self, handle: &ManagedTorrent, store: &Arc<WindowStore>, file: usize, len: u64, side: usize) -> anyhow::Result<()> {
        if self.side_files.insert(side) {
            self.apply_now(handle, store, file, len)?;
        }
        Ok(())
    }

    /// HLS için dizin durumu: yüklendiyse o; okunabilir baytlarda ayrıştırılamadıysa BOŞ dizin
    /// ("denendi, yok" -> 422); hâlâ bekleniyorsa `None`.
    pub fn settled_index(&self) -> Option<Arc<TimeIndex>> {
        self.index.clone().or_else(|| self.index_unreadable.then(|| Arc::new(TimeIndex::default())))
    }

    /// Video dosyasının baytı `pos`un parçası şu an seçili mi? (librqbit akış okuyucusu seçili
    /// olmayan parçaları da öncelikle indirir; HLS okumaları bununla sınırlanır.)
    pub fn is_selected_at(&self, pos: u64) -> bool {
        let (Some((off, plen)), Some(sel)) = (self.geom, self.last_sel.as_ref()) else { return false };
        sel.get(((off + pos) / plen) as usize).copied().unwrap_or(false)
    }

    /// Meta konumunu (tamamlanana dek) ve zaman dizinini (yüklenene dek) yeniden dener. Boş
    /// dizine kalıcı kilitlenmez: 0.5 sn'den 5 sn'ye üstel geri çekilmeyle sürekli denenir.
    pub fn refresh_meta(&mut self, store: &Arc<WindowStore>, file: usize, len: u64) {
        if !self.loc.complete {
            self.loc = locate_meta(StoreReader::new(store.clone(), file, len), len);
        }
        if self.index.is_none() && self.unparseable_tries < MAX_UNPARSEABLE_TRIES && Instant::now() >= self.next_try {
            let miss = Arc::new(AtomicBool::new(false));
            let rd = StoreReader::new(store.clone(), file, len).with_miss_flag(miss.clone());
            match crate::demux::time_index(rd) {
                Ok(i) => self.index = Some(Arc::new(i)),
                Err(_) if !miss.load(Ordering::Relaxed) => {
                    // Hiçbir okuma eksik bayt yüzünden başarısız olmadı ama kapsayıcı ayrıştırılamadı
                    // (desteklenmeyen/bozuk): sonsuza dek beklemek yerine "dizin yok" (boş) kalıcıdır.
                    // Pencere payı (dizinsiz %20) bozulmasın diye `index` None kalır; yalnızca HLS'e bildirilir.
                    self.index_unreadable = true;
                    self.unparseable_tries += 1;
                    self.tries += 1;
                    let ms = (500u64 << self.tries.min(4)).min(5000);
                    self.next_try = Instant::now() + Duration::from_millis(ms);
                }
                Err(_) => {
                    self.tries += 1;
                    let ms = (500u64 << self.tries.min(4)).min(5000);
                    self.next_try = Instant::now() + Duration::from_millis(ms);
                }
            }
        }
    }

    /// Bir tick: meta/indeksi tazele, pencereyi hesapla ve uygula. Dönen aralık, önden okumanın
    /// geçmemesi gereken `keep` (yeni konuma ait).
    pub fn tick(
        &mut self,
        handle: &ManagedTorrent,
        store: &Arc<WindowStore>,
        file: usize,
        len: u64,
        secs: f64,
        avg_bps: f64,
    ) -> anyhow::Result<Range<u64>> {
        self.tick_with(handle, store, file, len, secs, avg_bps, WINDOW_BEHIND_SECS, WINDOW_AHEAD_SECS)
    }

    /// Boşta bakım: `IDLE_*` küçük pencere.
    pub fn tick_idle(
        &mut self,
        handle: &ManagedTorrent,
        store: &Arc<WindowStore>,
        file: usize,
        len: u64,
        secs: f64,
        avg_bps: f64,
    ) -> anyhow::Result<Range<u64>> {
        self.tick_with(handle, store, file, len, secs, avg_bps, IDLE_BEHIND_SECS, IDLE_AHEAD_SECS)
    }

    #[allow(clippy::too_many_arguments)]
    fn tick_with(
        &mut self,
        handle: &ManagedTorrent,
        store: &Arc<WindowStore>,
        file: usize,
        len: u64,
        secs: f64,
        avg_bps: f64,
        behind: f64,
        ahead: f64,
    ) -> anyhow::Result<Range<u64>> {
        self.refresh_meta(store, file, len);
        let idx = self.index.clone().unwrap_or_default();
        let mut keep = keep_range_with(secs, &idx, len, avg_bps, behind, ahead);
        if self.index.is_none() {
            let m = ((keep.end - keep.start) as f64 * NO_INDEX_MARGIN) as u64;
            keep = keep.start.saturating_sub(m)..keep.end.saturating_add(m).min(len);
        }
        let meta = meta_ranges(&self.loc, &idx, len);
        let mut keeps = vec![keep.clone()];
        // Sıçrama: yeni konum bu tick'te öğrenildi; eski pencere bir tick daha korunur ki yeni
        // konum için akıştan yeni inmiş parçalar eski `keep` yüzünden silinmesin.
        if let (Some(ls), Some(lk)) = (self.last_secs, self.last_keep.clone()) {
            if (secs - ls).abs() > JUMP_SECS && lk != keep {
                keeps.push(lk);
            }
        }
        self.last_secs = Some(secs);
        self.last_keep = Some(keep.clone());
        self.last_keeps = keeps.clone();
        keeps.extend(self.live_extras());
        self.apply(handle, store, file, &meta, &keeps)?;
        Ok(keep)
    }

    /// Seçimi/tahliyeyi uygular. Seçim değişmediyse `set_selected_pieces` çağrılmaz; tahliye
    /// süpürmesi de yalnızca `KEEP_REFRESH`te bir yapılır (`None` döner).
    pub fn apply(
        &mut self,
        handle: &ManagedTorrent,
        store: &WindowStore,
        video_file: usize,
        meta: &[Range<u64>],
        keeps: &[Range<u64>],
    ) -> anyhow::Result<Option<EvictReport>> {
        let all: Vec<&Range<u64>> = meta.iter().chain(keeps.iter()).collect();
        let sel = compute_selection(handle, video_file, &all, &self.side_files)?;
        if let Some(meta) = handle.metadata.load_full() {
            if let Some(vf) = meta.file_infos.get(video_file) {
                self.geom = Some((vf.offset_in_torrent, meta.info.lengths().default_piece_length() as u64));
            }
        }
        let changed = self.last_sel.as_ref() != Some(&sel);
        if !changed && self.last_keep_at.elapsed() < KEEP_REFRESH {
            return Ok(None);
        }
        if changed {
            // Önce seçim: tahliye edilecek parçalar yeniden istenmesin.
            handle.set_selected_pieces(&sel)?;
            self.vendor_calls += 1;
            self.last_sel = Some(sel);
        }
        let ranges: Vec<Range<u64>> = meta.iter().chain(keeps.iter()).cloned().collect();
        let rep = store.set_keep(video_file, &ranges);
        self.keep_calls += 1;
        self.last_keep_at = Instant::now();
        store.forget_pieces(handle, &rep.evicted)?;
        Ok(Some(rep))
    }
}

/// Tek seferlik uygulama (önbelleksiz): `header` + `keep` için seçim, tahliye ve unutturma.
pub fn apply_window(
    handle: &ManagedTorrent,
    store: &WindowStore,
    video_file: usize,
    header: Range<u64>,
    keep: Range<u64>,
) -> anyhow::Result<EvictReport> {
    let mut ctl = WindowCtl::new();
    Ok(ctl.apply(handle, store, video_file, &[header], &[keep])?.unwrap_or_default())
}

/// `WindowStore` üzerinden `Read + Seek` (zaman dizinini okumak için). Henüz inmemiş veya
/// silinmiş baytlar okuma hatası verir.
pub struct StoreReader {
    store: Arc<WindowStore>,
    file: usize,
    len: u64,
    pos: u64,
    /// Eksik/silinmiş bayt yüzünden bir okuma başarısız olunca kurulur.
    miss: Option<Arc<AtomicBool>>,
}

impl StoreReader {
    pub fn new(store: Arc<WindowStore>, file: usize, len: u64) -> Self {
        Self { store, file, len, pos: 0, miss: None }
    }
    pub fn with_miss_flag(mut self, f: Arc<AtomicBool>) -> Self {
        self.miss = Some(f);
        self
    }
}

impl Read for StoreReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        if n == 0 {
            return Ok(0);
        }
        self.store
            .pread_exact(self.file, self.pos, &mut buf[..n])
            .map_err(|e| {
                if let Some(m) = &self.miss {
                    m.store(true, Ordering::Relaxed);
                }
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, format!("{e:#}"))
            })?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for StoreReader {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let np = match from {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::End(d) => self.len as i128 + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if np < 0 {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        self.pos = np as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1_000_000;

    #[test]
    fn keep_range_uses_index() {
        let idx = TimeIndex { points: vec![(0.0, 0), (600.0, 600 * MB)] };
        assert_eq!(keep_range(300.0, &idx, 600 * MB, MB as f64), 0..600 * MB);
        let r = keep_range(0.0, &idx, 600 * MB, MB as f64);
        assert_eq!(r, 0..idx.byte_at(300.0, 600 * MB, MB as f64));
        assert_eq!(r.end, 300 * MB);
    }

    #[test]
    fn keep_range_falls_back_to_bitrate() {
        let idx = TimeIndex::default();
        let bps = 10.0 * MB as f64;
        let r = keep_range(1000.0, &idx, u64::MAX, bps);
        assert_eq!(r, (700 * 10 * MB)..(1300 * 10 * MB));
        assert_eq!(keep_range(1000.0, &idx, 8000 * MB, bps).end, 8000 * MB);
    }

    #[test]
    fn header_range_is_capped() {
        let idx = TimeIndex { points: vec![(0.0, 5 * MB), (10.0, 90 * MB)] };
        assert_eq!(header_range(&idx, 1000 * MB), 0..5 * MB);
        let far = TimeIndex { points: vec![(0.0, 500 * MB), (10.0, 900 * MB)] };
        assert_eq!(header_range(&far, 1000 * MB).end, HEADER_MAX_BYTES);
        assert_eq!(header_range(&TimeIndex::default(), 100 * MB).end, HEADER_FALLBACK_BYTES);
    }

    #[test]
    fn meta_ranges_include_tail_index_region() {
        let loc = MetaLoc { ranges: vec![0..1000, 900 * MB..901 * MB], complete: true };
        let v = meta_ranges(&loc, &TimeIndex::default(), 1000 * MB);
        assert_eq!(v, loc.ranges);
        let idx = TimeIndex { points: vec![(0.0, 2000), (5.0, 3000)] };
        let v = meta_ranges(&loc, &idx, 1000 * MB);
        assert_eq!(v.len(), 3);
        assert_eq!(v[2], 0..2000);
    }

    #[test]
    fn refresh_meta_gives_up_on_unparseable_container() {
        use librqbit_core::lengths::Lengths;
        const P: u32 = 1024;
        let tmp = tempfile::tempdir().unwrap();
        let len = 4 * P as u64;
        let store = Arc::new(
            WindowStore::with_layout(tmp.path().join("t"), Lengths::new(len, P).unwrap(), &[(0, len)]).unwrap(),
        );
        // Tüm baytlar mevcut ama tanınmayan kapsayıcı.
        for p in 0..4u32 {
            store.pwrite_all(0, p as u64 * P as u64, &vec![0xAB; P as usize]).unwrap();
            store.mark_complete(p);
        }
        let mut ctl = WindowCtl::new();
        for _ in 0..(MAX_UNPARSEABLE_TRIES + 10) {
            ctl.next_try = Instant::now();
            ctl.refresh_meta(&store, 0, len);
        }
        assert_eq!(ctl.unparseable_tries, MAX_UNPARSEABLE_TRIES);
        assert!(ctl.index.is_none());
        // Hata durumu korunur: HLS boş dizin görür.
        assert!(ctl.settled_index().is_some_and(|i| i.points.is_empty()));
    }

    #[test]
    fn reset_selection_forgets_last_selection() {
        let mut ctl = WindowCtl::new();
        ctl.last_sel = Some(vec![true, false]);
        ctl.reset_selection();
        assert!(ctl.last_sel.is_none());
    }

    #[test]
    fn pieces_of_ranges() {
        let mut s = vec![false; 64];
        pieces_of(&(10 * 1048576..20 * 1048576), 0, 1048576, &mut s);
        assert_eq!(s.iter().filter(|b| **b).count(), 10);
        assert!(s[10] && s[19] && !s[20]);
    }
}
