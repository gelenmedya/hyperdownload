//! Dolby Atmos tespiti, bit akışından (iz adından asla): E-AC-3 JOC ve TrueHD 16 kanallı sunum.
//!
//! # Bit düzeni kaynakları
//!
//! **E-AC-3** (ETSI TS 102 366 V1.4.1, Ek E, §E.1.2.2 `bsi()`): `syncword(16)=0x0B77`,
//! `strmtyp(2)`, `substreamid(3)`, `frmsiz(11)`, `fscod(2)` (3 ise `fscod2(2)`, `numblkscod=3`; aksi
//! halde `numblkscod(2)`), `acmod(3)`, `lfeon(1)`, `bsid(5)` ... `addbsie(1)`, `addbsil(6)`,
//! `addbsi((addbsil+1)*8)`. BSI alan sırası `filmcraft-ac3` 0.9'un `eac3::read_bsi` işlevindeki
//! (aynı standarda göre yazılmış, `addbsi`yi atlayan) ayrıştırmayla bire bir karşılaştırıldı.
//! `addbsi`nin ilk baytı: 7 ayrılmış bit + `flag_ec3_extension_type_a` (LSB); bayrak 1 ise
//! ikinci bayt `complexity_index_type_a` (ETSI TS 103 420 §C.3; FFmpeg `ac3_parser.c`
//! `eac3_extension_type_a` / `complexity_index_type_a`). JOC (Atmos) <=> bayrak 1.
//! AC-3 (`bsid <= 10`) hiçbir zaman JOC taşımaz.
//!
//! **TrueHD** (Dolby TrueHD / MLP `major_sync_info`; `truehd` 0.7.2 `structs/sync.rs`
//! `MajorSyncInfo::read` ve `process::PresentationMap`, FFmpeg `mlp_parse.c`
//! `ff_mlp_read_major_sync`): erişim biriminin başında 4 baytlık `access_unit_length(16) +
//! input_timing(16)` başlığı, ardından büyük senkron: `format_sync(32)=0xF8726FBA`,
//! `format_info(32)`, `signature(16)=0xB752`, `flags(16)`, `reserved(16)`, `variable_rate(1)`,
//! `peak_data_rate(15)`, `substreams(4)`, `extended_substream_info(4)`, `substream_info(8)`.
//! `truehd` kasası sunum haritasını `PresentationMap::with_substream_info` ile türetir: 4.
//! (16 kanallı) sunum `substream_info` bit 7'si ile bildirilir. Atmos'u şu ikisinin birlikte
//! doğrulanması olarak kabul ediyoruz: `substreams >= 4` VE 16 kanallı sunum bildirilmiş.
//!
//! **Emin olunmayan nokta:** "16 kanallı sunum (alt akış 3) = Atmos" eşlemesi `truehd` kasasının
//! belgesine/sunum haritasına dayanır; Dolby'nin kapalı belirtimine karşı elle doğrulanmadı ve
//! gerçek bir Atmos TrueHD klibiyle sınanmadı. Büyük senkronun CRC'si denetlenmez (imza 0xB752
//! denetlenir). Şüphede `None` döner; yanlış "Atmos" demek yerine `Unknown`.

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use truehd::process::PresentationMap;

use crate::demux::{PacketReader, TrackInfo, TrackKind};

/// Bir ses izinin Atmos durumu. `Unknown`: henüz taranmadı / paket okunamadı.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum AtmosState {
    Yes,
    No,
    Unknown,
}

/// Varsayılan taranan paket sayısı.
pub const DEFAULT_MAX_PACKETS: usize = 64;

// ---------------------------------------------------------------------------------------------
// E-AC-3
// ---------------------------------------------------------------------------------------------

/// MSB-first bit okuyucu; sonu aşan okuma `overrun` işaretler.
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8]) -> Self {
        Self { d, pos: 0 }
    }
    fn read(&mut self, n: usize) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = self.d.get(self.pos >> 3).copied().unwrap_or(0);
            v = (v << 1) | ((byte >> (7 - (self.pos & 7))) & 1) as u32;
            self.pos += 1;
        }
        v
    }
    fn bit(&mut self) -> bool {
        self.read(1) == 1
    }
    fn skip(&mut self, n: usize) {
        self.pos += n;
    }
    fn overrun(&self) -> bool {
        self.pos > self.d.len() * 8
    }
}

/// E-AC-3 bağımsız alt akışının BSI özeti (`dec3` kutusunu kurmak için).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Eac3Bsi {
    pub fscod: u8,
    pub bsid: u8,
    pub bsmod: u8,
    pub acmod: u8,
    pub lfeon: bool,
    /// Bu bağımsız çerçevenin bayt boyutu.
    pub frame_bytes: usize,
    pub numblks: usize,
    pub joc: bool,
    pub complexity: Option<u8>,
}

/// Sentezlenen alt akış kümesi: ilk bağımsız çerçeve + ardından gelen bağımlı çerçeveler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Eac3Config {
    pub bsi: Eac3Bsi,
    pub num_dep_sub: u8,
    /// `dec3` `chan_loc` (9 bit); bağımlı akış yoksa 0.
    pub chan_loc: u16,
    /// Bağımsız + bağımlı çerçevelerin toplam hızı, kb/s (yuvarlanmış).
    pub data_rate_kbps: u32,
}

fn fscod_hz(fscod: u8) -> u32 {
    [48000, 44100, 32000, 24000][fscod as usize & 3]
}

/// Verinin başındaki E-AC-3 erişim birimini (bağımsız çerçeve + bağımlı çerçeveler) çözer.
pub(crate) fn eac3_config(data: &[u8]) -> Option<Eac3Config> {
    if data.len() < 7 || data[0] != 0x0B || data[1] != 0x77 || data[2] >> 6 != 0 {
        return None; // senkron yok ya da ilk çerçeve bağımsız (strmtyp 0) değil
    }
    let bsi = parse_eac3_bsi(data)?;
    if bsi.bsid <= 10 || bsi.fscod == 3 || bsi.frame_bytes < 8 || bsi.frame_bytes > data.len() {
        return None; // fscod 3 (fscod2 yarı hızlar) desteklenmiyor
    }
    let mut total = bsi.frame_bytes;
    let mut off = bsi.frame_bytes;
    let mut num_dep = 0u8;
    let mut chanmap_acc = 0u16;
    while off + 7 <= data.len() && data[off] == 0x0B && data[off + 1] == 0x77 && data[off + 2] >> 6 == 1 {
        let words = ((((data[off + 2] & 7) as usize) << 8) | data[off + 3] as usize) + 1;
        if off + words * 2 > data.len() {
            break;
        }
        num_dep = num_dep.saturating_add(1);
        total += words * 2;
        if let Some(cm) = dep_chanmap(&data[off..]) {
            chanmap_acc |= cm;
        }
        off += words * 2;
    }
    let bits = total as u64 * 8 * fscod_hz(bsi.fscod) as u64 / (bsi.numblks as u64 * 256);
    // dec3 chan_loc: chanmap bit k (MSB=0), k=5..13 -> chan_loc bit 13-k.
    let chan_loc = (chanmap_acc >> 2) & 0x1FF;
    Some(Eac3Config { bsi, num_dep_sub: num_dep.min(15), chan_loc, data_rate_kbps: ((bits + 500) / 1000) as u32 })
}

/// Bağımlı çerçevenin `chanmap` alanı (yoksa `None`).
fn dep_chanmap(f: &[u8]) -> Option<u16> {
    let mut r = Bits::new(f);
    r.skip(16 + 2 + 3 + 11);
    r.skip(2 + 2); // fscod, numblkscod (ya da fscod2)
    let acmod = r.read(3);
    r.skip(1 + 5 + 5); // lfeon, bsid, dialnorm
    if r.bit() {
        r.skip(8);
    }
    if acmod == 0 {
        r.skip(5);
        if r.bit() {
            r.skip(8);
        }
    }
    let present = r.bit();
    let cm = r.read(16) as u16;
    if r.overrun() || !present { None } else { Some(cm) }
}

/// `Some((joc, complexity))`: ayrıştırıldı; `None`: ayrıştırılamadı (kesik / tanınmayan).
fn eac3_scan(data: &[u8]) -> Option<(bool, Option<u8>)> {
    let mut off = 0usize;
    // Bir paket birden çok senkron çerçeve taşıyabilir (bağımsız + bağımlı alt akışlar):
    // bağımlı (strmtyp 1) çerçeveleri atla, ilk bağımsız çerçevenin BSI'sini oku.
    while off + 7 <= data.len() {
        let f = &data[off..];
        if f[0] != 0x0B || f[1] != 0x77 {
            return None;
        }
        let bsid = f[5] >> 3;
        if bsid <= 10 {
            return Some((false, None)); // AC-3 (bsid 6..8, 9/10 yarım hız): JOC yok
        }
        if bsid > 16 {
            return None;
        }
        let strmtyp = f[2] >> 6;
        if strmtyp == 3 {
            return None; // ayrılmış akış türü
        }
        if strmtyp == 1 {
            let words = ((((f[2] & 7) as usize) << 8) | f[3] as usize) + 1;
            off += words * 2;
            continue;
        }
        return parse_eac3_bsi(f).map(|b| (b.joc, b.complexity));
    }
    None
}

/// §E.1.2.2 `bsi()` — `addbsi`ye kadar; bkz. modül belgesi.
fn parse_eac3_bsi(f: &[u8]) -> Option<Eac3Bsi> {
    let mut r = Bits::new(f);
    r.skip(16); // syncword
    let strmtyp = r.read(2);
    r.skip(3); // substreamid
    let frmsiz = r.read(11) as usize;
    let fscod = r.read(2);
    let numblkscod = if fscod == 3 {
        if r.read(2) == 3 {
            return None; // fscod2 ayrılmış
        }
        3
    } else {
        r.read(2)
    };
    let nblocks = [1, 2, 3, 6][numblkscod as usize & 3];
    let acmod = r.read(3);
    let lfeon = r.bit();
    let bsid = r.read(5) as u8;
    r.skip(5); // dialnorm
    if r.bit() {
        r.skip(8); // compr
    }
    if acmod == 0 {
        r.skip(5); // dialnorm2
        if r.bit() {
            r.skip(8); // compr2
        }
    }
    if strmtyp == 1 && r.bit() {
        r.skip(16); // chanmap
    }
    if r.bit() {
        // mixmdate
        if acmod > 2 {
            r.skip(2); // dmixmod
        }
        if acmod & 1 != 0 && acmod > 2 {
            r.skip(6); // ltrtcmixlev, lorocmixlev
        }
        if acmod & 4 != 0 {
            r.skip(6); // ltrtsurmixlev, lorosurmixlev
        }
        if lfeon && r.bit() {
            r.skip(5); // lfemixlevcod
        }
        if strmtyp == 0 {
            if r.bit() {
                r.skip(6); // pgmscl
            }
            if acmod == 0 && r.bit() {
                r.skip(6); // pgmscl2
            }
            if r.bit() {
                r.skip(6); // extpgmscl
            }
            match r.read(2) {
                1 => r.skip(5),
                2 => r.skip(12),
                3 => {
                    let len = r.read(5) as usize;
                    r.skip(8 * (len + 2)); // mixdata
                }
                _ => {}
            }
            if acmod < 2 {
                if r.bit() {
                    r.skip(14); // panmean, paninfo
                }
                if acmod == 0 && r.bit() {
                    r.skip(14); // panmean2, paninfo2
                }
            }
            if r.bit() {
                // frmmixcfginfoe
                if numblkscod == 0 {
                    r.skip(5);
                } else {
                    for _ in 0..nblocks {
                        if r.bit() {
                            r.skip(5);
                        }
                    }
                }
            }
        }
    }
    let mut bsmod = 0u8;
    if r.bit() {
        // infomdate
        bsmod = r.read(3) as u8;
        r.skip(2); // copyrightb, origbs
        if acmod == 2 {
            r.skip(4); // dsurmod, dheadphonmod
        }
        if acmod >= 6 {
            r.skip(2); // dsurexmod
        }
        if r.bit() {
            r.skip(8); // audprodie: mixlevel, roomtyp, adconvtyp
        }
        if acmod == 0 && r.bit() {
            r.skip(8); // audprodi2e
        }
        if fscod < 3 {
            r.skip(1); // sourcefscod
        }
    }
    if strmtyp == 0 && numblkscod != 3 {
        r.skip(1); // convsync
    }
    if strmtyp == 2 && (numblkscod == 3 || r.bit()) {
        r.skip(6); // frmsizecod
    }
    let addbsie = r.bit();
    if r.overrun() {
        return None; // addbsie bitine ulaşamadık: kesik
    }
    let mk = |joc: bool, complexity: Option<u8>| Eac3Bsi {
        fscod: fscod as u8,
        bsid,
        bsmod,
        acmod: acmod as u8,
        lfeon,
        frame_bytes: (frmsiz + 1) * 2,
        numblks: nblocks,
        joc,
        complexity,
    };
    if !addbsie {
        return Some(mk(false, None));
    }
    let addbsil = r.read(6) as usize;
    // addbsi: ilk bayt = 7 ayrılmış bit + flag_ec3_extension_type_a; bayrak 1 ise ikinci bayt
    // complexity_index_type_a.
    r.skip(7);
    let flag = r.bit();
    let complexity = if flag && addbsil >= 1 { Some(r.read(8) as u8) } else { None };
    if r.overrun() {
        return None;
    }
    // addbsil == 0 → tek bayt addbsi: complexity_index_type_a yok; Atmos'u asla yanlış iddia etme.
    Some(mk(flag && addbsil >= 1, complexity))
}

/// E-AC-3 paketi/çerçevesi için: `Some(true)` JOC (Atmos) bayrağı var, `Some(false)` ayrıştırıldı
/// ve bayrak yok (AC-3 dahil), `None` ayrıştırılamadı.
pub fn eac3_joc(frame: &[u8]) -> Option<bool> {
    eac3_scan(frame).map(|(joc, _)| joc)
}

/// JOC `complexity_index_type_a` (dec3 kutusunun `complexity_index_type_a` alanı için).
/// JOC yoksa ya da ayrıştırılamadıysa `None`.
pub fn eac3_joc_complexity(frame: &[u8]) -> Option<u8> {
    eac3_scan(frame).and_then(|(joc, c)| if joc { c } else { None })
}

// ---------------------------------------------------------------------------------------------
// TrueHD
// ---------------------------------------------------------------------------------------------

/// TrueHD erişim birimi: büyük senkron varsa `Some(16 kanallı sunum var mı)`, yoksa `None`.
/// Büyük senkron erişim biriminin 4 baytlık başlığından sonra (ofset 4) ya da ham akışta
/// (ofset 0) aranır; gövdede tarama yapılmaz (sahte eşleşme riski).
pub fn truehd_atmos(frame: &[u8]) -> Option<bool> {
    [4usize, 0].into_iter().find_map(|off| major_sync_at(frame, off))
}

fn major_sync_at(b: &[u8], off: usize) -> Option<bool> {
    // sync(4) format_info(4) signature(2) flags(2) reserved(2) vbr+peak(2) substreams|ext(1) substream_info(1)
    let m = b.get(off..off + 18)?;
    if m[..3] != [0xF8, 0x72, 0x6F] {
        return None;
    }
    if m[8] != 0xB7 || m[9] != 0x52 {
        return None; // imza tutmuyor: gerçek büyük senkron değil
    }
    match m[3] {
        0xBB => return Some(false), // MLP (FBB): Atmos olamaz
        0xBA => {}
        _ => return None,
    }
    let substreams = m[16] >> 4;
    let ext = m[16] & 0x0F;
    let substream_info = m[17];
    let map = PresentationMap::with_substream_info(substream_info, ext & 3);
    let has_16ch = map.masks[3] >> 3 != 0;
    Some(substreams >= 4 && has_16ch)
}

// ---------------------------------------------------------------------------------------------
// Paket tarama
// ---------------------------------------------------------------------------------------------

/// İzin ilk `max_packets` paketini okuyup ilk kesin sonucu döner (başlangıç 0 sn).
pub fn detect<R: Read + Seek>(r: R, track: &TrackInfo, max_packets: usize) -> AtmosState {
    detect_at(r, track, 0.0, max_packets)
}

/// [`detect`], ama `start_secs` konumundan başlar (Cues/örnek tablosu ile atlanır). `eac3`/`truehd`
/// dışındaki kodekler `No`. Hiçbiri kesin değilse (ya da okuma hatasında) `Unknown`.
pub fn detect_at<R: Read + Seek>(r: R, track: &TrackInfo, start_secs: f64, max_packets: usize) -> AtmosState {
    let probe: fn(&[u8]) -> Option<bool> = match track.codec.as_str() {
        "eac3" => eac3_joc,
        "truehd" => truehd_atmos,
        _ => return AtmosState::No,
    };
    let Ok(mut packets) = PacketReader::open(r, track.id, start_secs.max(0.0)) else {
        return AtmosState::Unknown;
    };
    for _ in 0..max_packets {
        match packets.next() {
            Ok(Some(p)) => {
                if let Some(v) = probe(&p.data) {
                    return if v { AtmosState::Yes } else { AtmosState::No };
                }
            }
            _ => break,
        }
    }
    AtmosState::Unknown
}


// ---------------------------------------------------------------------------------------------
// Sınırlı okuyucu + tarama sürücüsü
// ---------------------------------------------------------------------------------------------

pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

/// Yalnızca doğrulanmış (diskte var) bayt aralıklarını okutan `Read + Seek` sarmalayıcı: dışarıdaki
/// okuma bloklamak yerine `UnexpectedEof` verir; `cancel` kurulunca her okuma hata verir.
pub struct HaveReader<R> {
    inner: R,
    len: u64,
    pos: u64,
    cancel: Arc<AtomicBool>,
    /// `[başlangıç, bitiş)` bayt aralığı tamamen mevcut mu.
    avail: Box<dyn Fn(u64, u64) -> bool + Send>,
}

impl<R: Read + Seek> HaveReader<R> {
    pub fn new(inner: R, len: u64, cancel: Arc<AtomicBool>, avail: Box<dyn Fn(u64, u64) -> bool + Send>) -> Self {
        Self { inner, len, pos: 0, cancel, avail }
    }
}

impl<R: Read + Seek> Read for HaveReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("tarama iptal edildi"));
        }
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        if n == 0 {
            return Ok(0);
        }
        if !(self.avail)(self.pos, self.pos + n as u64) {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "bayt aralığı diskte yok"));
        }
        self.inner.seek(SeekFrom::Start(self.pos))?;
        let k = self.inner.read(&mut buf[..n])?;
        self.pos += k as u64;
        Ok(k)
    }
}

impl<R: Read + Seek> Seek for HaveReader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let np = match from {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::End(d) => self.len as i128 + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if np < 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        self.pos = np as u64;
        Ok(self.pos)
    }
}

pub struct ScanParams {
    /// Denenecek başlangıç konumları (sn), sırayla; ilk kesin sonuç kazanır.
    pub starts: Vec<f64>,
    pub max_packets: usize,
    /// Tüm tarama için toplam süre.
    pub timeout: Duration,
}

/// `try_begin` ile alınan taramanın bitişinde (her yolda, panik dahil) `scanning` bayrağını temizler.
pub struct ScanGuard<'a> {
    cache: &'a AtmosCache,
    key: &'a CacheKey,
}

impl<'a> ScanGuard<'a> {
    pub fn new(cache: &'a AtmosCache, key: &'a CacheKey) -> Self {
        Self { cache, key }
    }
}

impl Drop for ScanGuard<'_> {
    fn drop(&mut self) {
        self.cache.end(self.key);
    }
}

/// `pending` izlerini sırayla tarar (her iz için `starts` sırayla); kesin sonuçları önbelleğe
/// yazar. `open` her deneme için yeni bir okuyucu üretir (`None`: açılamadı, iz atlanır). Zaman
/// aşımında `cancel` kurulur (okuyucular hata verip bloklamayı bırakır) ve dönülür; kalan izler
/// `Unknown` kalır. `scanning` bayrağını çağıran [`ScanGuard`] temizler.
pub async fn run_scan<R, F>(cache: &AtmosCache, key: &CacheKey, pending: Vec<TrackInfo>, p: &ScanParams, cancel: Arc<AtomicBool>, open: F)
where
    R: Read + Seek + Send + 'static,
    F: Fn(&Arc<AtomicBool>) -> Option<R>,
{
    let deadline = tokio::time::Instant::now() + p.timeout;
    for t in pending {
        let mut state = AtmosState::Unknown;
        for &start in &p.starts {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let Some(r) = open(&cancel) else { break };
            let (tr, max) = (t.clone(), p.max_packets);
            let job = tokio::task::spawn_blocking(move || detect_at(r, &tr, start, max));
            match tokio::time::timeout_at(deadline, job).await {
                Ok(Ok(s)) => state = s,
                _ => {
                    cancel.store(true, Ordering::Relaxed);
                    return;
                }
            }
            if state != AtmosState::Unknown {
                break;
            }
        }
        cache.record(key, t.id, state);
    }
}
// ---------------------------------------------------------------------------------------------
// Önbellek (Engine kullanır)
// ---------------------------------------------------------------------------------------------

/// (info hash, dosya)
pub type CacheKey = (String, usize);

#[derive(Default)]
struct Entry {
    /// Yalnızca kesin sonuçlar (Yes/No); kalıcı.
    known: HashMap<u64, AtmosState>,
    scanning: bool,
    last_try: Option<Instant>,
    attempts: u32,
    /// Son taramanın konumu (sn); konum değişince deneme sayacı sıfırlanır.
    last_pos: f64,
    cancel: Option<Arc<AtomicBool>>,
}

/// Başlatılan taramanın bileti.
pub struct ScanTicket {
    pub cancel: Arc<AtomicBool>,
    /// 0'dan başlayan deneme sırası (okunacak paket sayısını büyütmek için).
    pub attempt: u32,
}

/// Dosya başına iz sonuçları ve tarama durumu. Kesin sonuçlar kalıcıdır; `Unknown` saklanmaz.
#[derive(Default)]
pub struct AtmosCache {
    inner: Mutex<HashMap<CacheKey, Entry>>,
}

fn needs_scan(t: &TrackInfo) -> bool {
    t.kind == TrackKind::Audio && matches!(t.codec.as_str(), "eac3" | "truehd")
}

impl AtmosCache {
    /// Her ses izi için durum. Taranmamış `eac3`/`truehd` = `Unknown`; diğer kodekler `No`.
    pub fn states(&self, key: &CacheKey, tracks: &[TrackInfo]) -> HashMap<u64, AtmosState> {
        let g = self.inner.lock().unwrap();
        let known = g.get(key).map(|e| &e.known);
        tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio)
            .map(|t| {
                let s = known.and_then(|k| k.get(&t.id).copied()).unwrap_or(if needs_scan(t) { AtmosState::Unknown } else { AtmosState::No });
                (t.id, s)
            })
            .collect()
    }

    /// Taranması gereken izler; varsayılan ses izi önce, sonra dosya sırası.
    pub fn pending(&self, key: &CacheKey, tracks: &[TrackInfo]) -> Vec<TrackInfo> {
        let g = self.inner.lock().unwrap();
        let known = g.get(key).map(|e| &e.known);
        let mut v: Vec<TrackInfo> = tracks
            .iter()
            .filter(|t| needs_scan(t) && !known.is_some_and(|k| k.contains_key(&t.id)))
            .cloned()
            .collect();
        v.sort_by_key(|t| !t.default); // kararlı: varsayılan önde, geri kalan sıra korunur
        v
    }

    /// Taramayı başlatma izni: halen taranmıyorsa ve son denemeden beri bekleme süresi geçtiyse.
    /// Bekleme `min_gap` ile başlar, her başarısız denemede ikiye katlanır (en çok 16 kat). Oynatma
    /// konumu (`pos_secs`) 30 sn'den fazla değiştiyse sayaç ve bekleme sıfırlanır: yeni konumda
    /// farklı baytlar okunabilir.
    pub fn try_begin(&self, key: &CacheKey, now: Instant, min_gap: Duration, pos_secs: f64) -> Option<ScanTicket> {
        let mut g = self.inner.lock().unwrap();
        let e = g.entry(key.clone()).or_default();
        if e.scanning {
            return None;
        }
        if e.last_try.is_some() && (pos_secs - e.last_pos).abs() > 30.0 {
            e.attempts = 0;
            e.last_try = None;
        }
        let gap = min_gap * (1u32 << e.attempts.min(4));
        if e.last_try.is_some_and(|t| now.duration_since(t) < gap) {
            return None;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        e.scanning = true;
        e.last_try = Some(now);
        e.last_pos = pos_secs;
        e.cancel = Some(cancel.clone());
        let attempt = e.attempts;
        e.attempts += 1;
        Some(ScanTicket { cancel, attempt })
    }

    /// Bu info hash'in sürmekte olan taramalarını iptal eder (torrent kapatılırken/silinirken).
    pub fn cancel_hash(&self, hash: &str) {
        for ((h, _), e) in self.inner.lock().unwrap().iter() {
            if h == hash
                && let Some(c) = &e.cancel
            {
                c.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Kesin sonucu kalıcı kaydeder (`Unknown` yok sayılır).
    pub fn record(&self, key: &CacheKey, track_id: u64, state: AtmosState) {
        if state == AtmosState::Unknown {
            return;
        }
        self.inner.lock().unwrap().entry(key.clone()).or_default().known.insert(track_id, state);
    }

    pub fn end(&self, key: &CacheKey) {
        if let Some(e) = self.inner.lock().unwrap().get_mut(key) {
            e.scanning = false;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Test verisi (Görev 9 da kullanır)
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod testdata {
    use crate::demux::{TrackInfo, TrackKind};

    /// MSB-first bit yazıcı.
    #[derive(Default)]
    pub struct BitWriter {
        buf: Vec<u8>,
        nbits: usize,
    }

    impl BitWriter {
        pub fn new() -> Self {
            Self::default()
        }
        /// `val`in alt `n` bitini yazar.
        pub fn put(&mut self, n: usize, val: u32) -> &mut Self {
            for i in (0..n).rev() {
                if self.nbits % 8 == 0 {
                    self.buf.push(0);
                }
                let bit = ((val >> i) & 1) as u8;
                *self.buf.last_mut().unwrap() |= bit << (7 - self.nbits % 8);
                self.nbits += 1;
            }
            self
        }
        pub fn into_bytes(self) -> Vec<u8> {
            self.buf
        }
    }

    /// Bağımsız/bağımlı E-AC-3 senkron çerçevesi: 5.1 (acmod 7 + LFE), 6 blok, bsid 16.
    /// `addbsi` verilirse `addbsie=1`; çerçeve `total` bayta sıfırla doldurulur (ses verisi yok).
    pub fn eac3_frame(strmtyp: u8, addbsi: Option<&[u8]>, total: usize) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(16, 0x0B77);
        w.put(2, strmtyp as u32);
        w.put(3, 0); // substreamid
        w.put(11, (total / 2 - 1) as u32); // frmsiz
        w.put(2, 0); // fscod 48 kHz
        w.put(2, 3); // numblkscod = 6 blok
        w.put(3, 7); // acmod 3/2
        w.put(1, 1); // lfeon
        w.put(5, 16); // bsid
        w.put(5, 27); // dialnorm
        w.put(1, 0); // compre
        if strmtyp == 1 {
            w.put(1, 0); // chanmape
        }
        w.put(1, 0); // mixmdate
        w.put(1, 0); // infomdate
        // strmtyp 0 + numblkscod 3: convsync yok
        match addbsi {
            Some(a) => {
                w.put(1, 1);
                w.put(6, (a.len() - 1) as u32);
                for b in a {
                    w.put(8, *b as u32);
                }
            }
            None => {
                w.put(1, 0);
            }
        }
        let mut v = w.into_bytes();
        assert!(v.len() <= total, "çerçeve başlığı total'den büyük");
        v.resize(total, 0);
        v
    }

    /// JOC (Atmos) E-AC-3 çerçevesi: addbsi = [bayrak=1, complexity].
    pub fn eac3_joc_frame(complexity: u8) -> Vec<u8> {
        eac3_frame(0, Some(&[0x01, complexity]), 96)
    }

    /// Atmos'suz E-AC-3 çerçevesi (addbsie=0).
    pub fn eac3_plain_frame() -> Vec<u8> {
        eac3_frame(0, None, 96)
    }

    /// Bağımlı alt akış çerçevesi (7.1 uzantısı).
    pub fn eac3_dependent_frame() -> Vec<u8> {
        eac3_frame(1, None, 64)
    }

    /// AC-3 (bsid 8) senkron çerçevesi başlığı.
    pub fn ac3_frame() -> Vec<u8> {
        let mut v = vec![0x0B, 0x77, 0x00, 0x00, 0x0E, 8 << 3, 0xE0];
        v.resize(64, 0);
        v
    }

    /// TrueHD erişim birimi, büyük senkronlu: 4 baytlık AU başlığı + `major_sync_info` + dolgu.
    pub fn truehd_major_au(format_sync_last: u8, substreams: u8, ext: u8, substream_info: u8) -> Vec<u8> {
        let mut v = vec![0x80, 0x20, 0x01, 0x02]; // access_unit_length, input_timing
        v.extend_from_slice(&[0xF8, 0x72, 0x6F, format_sync_last]);
        v.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // format_info (48 kHz...)
        v.extend_from_slice(&[0xB7, 0x52]); // signature
        v.extend_from_slice(&[0x00, 0x00]); // flags
        v.extend_from_slice(&[0x00, 0x00]); // reserved
        v.extend_from_slice(&[0x8F, 0xFF]); // variable_rate + peak_data_rate
        v.push((substreams << 4) | (ext & 0x0F));
        v.push(substream_info);
        v.resize(64, 0);
        v
    }

    /// Büyük senkronsuz (küçük senkron / ara) TrueHD erişim birimi.
    pub fn truehd_minor_au() -> Vec<u8> {
        let mut v = vec![0x40, 0x10, 0x03, 0x04];
        v.resize(48, 0x5A);
        v
    }

    pub fn audio_track(id: u64, codec: &str, default: bool) -> TrackInfo {
        TrackInfo {
            id,
            kind: TrackKind::Audio,
            codec: codec.into(),
            codec_id: String::new(),
            codec_private: Vec::new(),
            language: None,
            name: None,
            default,
            channels: None,
            sample_rate: None,
            bit_depth: None,
            width: None,
            height: None,
            hdr: None,
            colour: None,
            dv_profile: None,
            dv_level: None,
            dv_bl_compat: None,
            dv_config: None,
            dv_config_fourcc: None,
        default_duration_ns: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testdata::*;
    use super::*;
    use crate::demux::{self, parse_dovi_config};
    use filmcraft_matroska as mkv;

    // ----- E-AC-3 -----

    #[test]
    fn eac3_joc_flag_set() {
        let f = eac3_joc_frame(16);
        assert_eq!(eac3_joc(&f), Some(true));
        assert_eq!(eac3_joc_complexity(&f), Some(16));
    }

    #[test]
    fn eac3_without_addbsi() {
        let f = eac3_plain_frame();
        assert_eq!(eac3_joc(&f), Some(false));
        assert_eq!(eac3_joc_complexity(&f), None);
    }

    #[test]
    fn eac3_addbsi_present_but_flag_clear() {
        let f = eac3_frame(0, Some(&[0x00, 0x00]), 96);
        assert_eq!(eac3_joc(&f), Some(false));
    }

    #[test]
    fn eac3_joc_flag_with_addbsil_zero_is_not_joc() {
        // addbsie=1, addbsil=0 (tek bayt), bayrak biti 1: complexity yok => JOC sayılmaz.
        let f = eac3_frame(0, Some(&[0x01]), 96);
        assert_eq!(eac3_joc(&f), Some(false));
        assert_eq!(eac3_joc_complexity(&f), None);
        let cfg = eac3_config(&f).unwrap();
        assert!(!cfg.bsi.joc);
        assert_eq!(cfg.bsi.complexity, None);
    }

    #[test]
    fn eac3_config_fscod3_is_none() {
        let mut f = eac3_plain_frame();
        // fscod bitleri: bayt 4'ün üst 2 biti
        f[4] |= 0xC0;
        assert!(eac3_config(&f).is_none());
    }

    #[test]
    fn eac3_truncated() {
        let f = eac3_joc_frame(16);
        // addbsi'ye kadar bile gelmeyen kesik çerçeve
        assert_eq!(eac3_joc(&f[..8]), None);
        // addbsi'nin ortasında kesik: header ~ 7 bayt + addbsie/addbsil/addbsi
        let hdr_end = f.iter().rposition(|b| *b != 0).unwrap();
        assert_eq!(eac3_joc(&f[..hdr_end]), None);
        assert_eq!(eac3_joc(&[]), None);
        assert_eq!(eac3_joc(&[0x0B]), None);
    }

    #[test]
    fn eac3_bad_sync_is_none() {
        let mut f = eac3_joc_frame(16);
        f[0] = 0;
        assert_eq!(eac3_joc(&f), None);
    }

    #[test]
    fn ac3_frame_is_not_joc() {
        assert_eq!(eac3_joc(&ac3_frame()), Some(false));
        assert_eq!(eac3_joc_complexity(&ac3_frame()), None);
    }

    #[test]
    fn eac3_joc_with_dependent_substream_after_independent() {
        let mut p = eac3_joc_frame(8);
        p.extend(eac3_dependent_frame());
        assert_eq!(eac3_joc(&p), Some(true));
        assert_eq!(eac3_joc_complexity(&p), Some(8));
    }

    #[test]
    fn eac3_dependent_first_then_independent_joc() {
        let mut p = eac3_dependent_frame();
        p.extend(eac3_joc_frame(12));
        assert_eq!(eac3_joc(&p), Some(true));
    }

    // ----- TrueHD -----

    #[test]
    fn truehd_major_sync_with_16ch_presentation() {
        // substream_info 0b11001100 + ext 1: kasanın kendi sunum haritası testindeki örnek
        // (6ch, 8ch ve alt akış 3'ü içeren 16ch sunum)
        let au = truehd_major_au(0xBA, 4, 1, 0b1100_1100);
        assert_eq!(truehd_atmos(&au), Some(true));
    }

    #[test]
    fn truehd_major_sync_8ch_only() {
        let au = truehd_major_au(0xBA, 3, 0, 0b0101_1000);
        assert_eq!(truehd_atmos(&au), Some(false));
    }

    #[test]
    fn truehd_inconsistent_signals_are_not_atmos() {
        // 16ch biti var ama 4 alt akış yok
        assert_eq!(truehd_atmos(&truehd_major_au(0xBA, 3, 1, 0b1100_1100)), Some(false));
        // 4 alt akış var ama 16ch sunum bildirilmemiş
        assert_eq!(truehd_atmos(&truehd_major_au(0xBA, 4, 0, 0b0101_1000)), Some(false));
    }

    #[test]
    fn truehd_no_major_sync() {
        assert_eq!(truehd_atmos(&truehd_minor_au()), None);
        assert_eq!(truehd_atmos(&[]), None);
        assert_eq!(truehd_atmos(&truehd_major_au(0xBA, 4, 1, 0xCC)[..12]), None);
    }

    #[test]
    fn truehd_bad_signature_is_none() {
        let mut au = truehd_major_au(0xBA, 4, 1, 0b1100_1100);
        au[12] = 0;
        assert_eq!(truehd_atmos(&au), None);
    }

    #[test]
    fn mlp_is_never_atmos() {
        assert_eq!(truehd_atmos(&truehd_major_au(0xBB, 4, 1, 0b1100_1100)), Some(false));
    }

    #[test]
    fn truehd_raw_sync_without_au_header() {
        let au = truehd_major_au(0xBA, 4, 1, 0b1100_1100);
        assert_eq!(truehd_atmos(&au[4..]), Some(true));
    }

    // ----- DV -----

    #[test]
    fn dovi_config_level() {
        let c = parse_dovi_config(&[1, 0, 16, 0x31, 0x10]).unwrap();
        assert_eq!((c.profile, c.level, c.bl_compat), (8, 6, 1));
        assert!(!c.rpu && !c.el && c.bl);
        assert_eq!(parse_dovi_config(&[1, 0]), None);
        // profile 5, level 9 (>=32 değil), rpu+bl: byte2 = 5<<1 | level>>5, byte3 = (level&31)<<3 | rpu<<2 | el<<1 | bl
        let c = parse_dovi_config(&[1, 0, 0x0A, (9 << 3) | 0b101, 0x00]).unwrap();
        assert_eq!((c.profile, c.level, c.bl_compat, c.rpu, c.el, c.bl), (5, 9, 0, true, false, true));
        // level 33 = 0b100001: üst bit byte2'nin LSB'sinde
        let c = parse_dovi_config(&[1, 0, 0x11, (1 << 3) | 1, 0x40]).unwrap();
        assert_eq!((c.profile, c.level, c.bl_compat), (8, 33, 4));
    }

    // ----- detect -----

    /// MKV: video + bir ses izi; `packets` ses paketleri.
    fn mkv_with_audio(codec_id: &str, packets: &[Vec<u8>]) -> Vec<u8> {
        let mut v = mkv::TrackSpec::new(mkv::TrackKind::Video, "V_MPEG4/ISO/AVC");
        v.video_size = Some((64, 64));
        let mut a = mkv::TrackSpec::new(mkv::TrackKind::Audio, codec_id);
        a.audio = Some((48000.0, 6, None));
        let opts = mkv::MuxOptions { lacing: mkv::LacingMode::None, cluster_duration_ns: 1_000_000_000, ..Default::default() };
        let mut w = mkv::MkvWriter::new(std::io::Cursor::new(Vec::new()), vec![v, a], opts).unwrap();
        for (i, p) in packets.iter().enumerate() {
            w.write_frame(0, i as i64 * 32_000_000, i == 0, &[i as u8; 50], None).unwrap();
            w.write_frame(1, i as i64 * 32_000_000, true, p, None).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn audio_of(bytes: &[u8]) -> TrackInfo {
        let info = demux::probe(std::io::Cursor::new(bytes.to_vec())).unwrap();
        info.tracks.into_iter().find(|t| t.kind == TrackKind::Audio).unwrap()
    }

    #[test]
    fn detect_eac3_joc_is_yes() {
        let b = mkv_with_audio("A_EAC3", &vec![eac3_joc_frame(16); 5]);
        let t = audio_of(&b);
        assert_eq!(t.codec, "eac3");
        assert_eq!(detect(std::io::Cursor::new(b), &t, DEFAULT_MAX_PACKETS), AtmosState::Yes);
    }

    #[test]
    fn detect_eac3_plain_is_no() {
        let b = mkv_with_audio("A_EAC3", &vec![eac3_plain_frame(); 5]);
        let t = audio_of(&b);
        assert_eq!(detect(std::io::Cursor::new(b), &t, DEFAULT_MAX_PACKETS), AtmosState::No);
    }

    #[test]
    fn detect_ac3_and_other_codecs_are_no() {
        let b = mkv_with_audio("A_AC3", &vec![ac3_frame(); 3]);
        let t = audio_of(&b);
        assert_eq!(detect(std::io::Cursor::new(b), &t, DEFAULT_MAX_PACKETS), AtmosState::No);
        let b = mkv_with_audio("A_AAC", &vec![vec![1, 2, 3]; 3]);
        let t = audio_of(&b);
        assert_eq!(detect(std::io::Cursor::new(b), &t, DEFAULT_MAX_PACKETS), AtmosState::No);
    }

    #[test]
    fn detect_truehd_finds_major_sync_after_minor_packets() {
        let mut pk = vec![truehd_minor_au(); 6];
        pk.push(truehd_major_au(0xBA, 4, 1, 0b1100_1100));
        let b = mkv_with_audio("A_TRUEHD", &pk);
        let t = audio_of(&b);
        assert_eq!(t.codec, "truehd");
        assert_eq!(detect(std::io::Cursor::new(b.clone()), &t, DEFAULT_MAX_PACKETS), AtmosState::Yes);
        // paket sınırı major sync'ten önce biterse kesin sonuç yok
        assert_eq!(detect(std::io::Cursor::new(b), &t, 5), AtmosState::Unknown);
    }

    #[test]
    fn detect_truehd_8ch_is_no() {
        let b = mkv_with_audio("A_TRUEHD", &[truehd_major_au(0xBA, 3, 0, 0b0101_1000)]);
        let t = audio_of(&b);
        assert_eq!(detect(std::io::Cursor::new(b), &t, DEFAULT_MAX_PACKETS), AtmosState::No);
    }

    #[test]
    fn detect_unreadable_is_unknown() {
        let t = audio_track(2, "eac3", true);
        assert_eq!(detect(std::io::Cursor::new(vec![0u8; 16]), &t, DEFAULT_MAX_PACKETS), AtmosState::Unknown);
    }

    #[test]
    fn atmos_state_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&AtmosState::Yes).unwrap(), "\"yes\"");
        assert_eq!(serde_json::to_string(&AtmosState::Unknown).unwrap(), "\"unknown\"");
    }

    // ----- önbellek -----

    fn key() -> CacheKey {
        ("abc".into(), 0)
    }

    #[test]
    fn cache_unscanned_is_unknown_and_other_codecs_no() {
        let c = AtmosCache::default();
        let tracks = vec![audio_track(2, "eac3", true), audio_track(3, "truehd", false), audio_track(4, "aac", false), audio_track(5, "ac3", false)];
        let s = c.states(&key(), &tracks);
        assert_eq!(s[&2], AtmosState::Unknown);
        assert_eq!(s[&3], AtmosState::Unknown);
        assert_eq!(s[&4], AtmosState::No);
        assert_eq!(s[&5], AtmosState::No);
    }

    #[test]
    fn cache_pending_default_first_and_skips_known() {
        let c = AtmosCache::default();
        let tracks = vec![audio_track(2, "truehd", false), audio_track(3, "eac3", true), audio_track(4, "eac3", false), audio_track(5, "aac", false)];
        let p: Vec<u64> = c.pending(&key(), &tracks).iter().map(|t| t.id).collect();
        assert_eq!(p, vec![3, 2, 4]);
        c.record(&key(), 3, AtmosState::Yes);
        let p: Vec<u64> = c.pending(&key(), &tracks).iter().map(|t| t.id).collect();
        assert_eq!(p, vec![2, 4]);
        assert_eq!(c.states(&key(), &tracks)[&3], AtmosState::Yes);
    }

    #[test]
    fn cache_ignores_unknown_and_keeps_known_permanently() {
        let c = AtmosCache::default();
        let tracks = vec![audio_track(2, "eac3", true)];
        c.record(&key(), 2, AtmosState::Unknown);
        assert_eq!(c.states(&key(), &tracks)[&2], AtmosState::Unknown);
        c.record(&key(), 2, AtmosState::No);
        assert!(c.pending(&key(), &tracks).is_empty());
        assert_eq!(c.states(&key(), &tracks)[&2], AtmosState::No);
        // başka dosya etkilenmez
        assert_eq!(c.states(&("abc".into(), 1), &tracks)[&2], AtmosState::Unknown);
    }

    #[test]
    fn cache_scan_is_exclusive_and_throttled() {
        let c = AtmosCache::default();
        let t0 = Instant::now();
        let gap = Duration::from_secs(10);
        let s = |dt: u64, pos: f64| c.try_begin(&key(), t0 + Duration::from_secs(dt), gap, pos);
        assert!(s(0, 0.0).is_some());
        assert!(s(60, 0.0).is_none(), "tarama sürerken ikinci başlamaz");
        c.end(&key());
        assert!(s(5, 0.0).is_none(), "aralık dolmadan yeniden denenmez");
        assert!(s(11, 0.0).is_none(), "ikinci denemeden önce bekleme 2 kat");
        let t = s(21, 0.0).unwrap();
        assert_eq!(t.attempt, 1);
        c.end(&key());
        // konum değişince sayaç ve bekleme sıfırlanır
        let t = s(22, 500.0).unwrap();
        assert_eq!(t.attempt, 0);
    }

    #[test]
    fn cancel_hash_flags_running_scan() {
        let c = AtmosCache::default();
        let t = c.try_begin(&key(), Instant::now(), Duration::from_secs(1), 0.0).unwrap();
        c.cancel_hash("other");
        assert!(!t.cancel.load(Ordering::Relaxed));
        c.cancel_hash("abc");
        assert!(t.cancel.load(Ordering::Relaxed));
    }

    // ----- sınırlı okuyucu + tarama sürücüsü -----

    #[test]
    fn have_reader_errors_outside_available_ranges() {
        let data: Vec<u8> = (0..100u8).collect();
        let mut r = HaveReader::new(std::io::Cursor::new(data), 100, Arc::new(AtomicBool::new(false)), Box::new(|a, b| b <= 40 || a >= 80));
        let mut buf = [0u8; 10];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(buf[9], 9);
        r.seek(SeekFrom::Start(35)).unwrap();
        assert_eq!(r.read(&mut buf).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        r.seek(SeekFrom::Start(85)).unwrap();
        r.read_exact(&mut buf).unwrap();
        assert_eq!(buf[0], 85);
        assert_eq!(r.seek(SeekFrom::End(0)).unwrap(), 100);
    }

    /// `head` ofsetinden sonrası "hiç gelmez": okuma iptale kadar bloklar.
    struct NeverDelivers {
        inner: std::io::Cursor<Vec<u8>>,
        head: u64,
        cancel: Arc<AtomicBool>,
    }
    impl Read for NeverDelivers {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            while self.inner.position() >= self.head {
                if self.cancel.load(Ordering::Relaxed) {
                    return Err(io::Error::other("iptal"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let room = (self.head - self.inner.position()) as usize;
            let n = buf.len().min(room);
            self.inner.read(&mut buf[..n])
        }
    }
    impl Seek for NeverDelivers {
        fn seek(&mut self, f: SeekFrom) -> io::Result<u64> {
            self.inner.seek(f)
        }
    }

    #[tokio::test]
    async fn scan_over_never_delivering_source_times_out_unknown_and_clears_scanning() {
        let b = mkv_with_audio("A_EAC3", &vec![eac3_joc_frame(16); 40]);
        let tracks = vec![audio_of(&b)];
        let cache = AtmosCache::default();
        let k = key();
        let tk = cache.try_begin(&k, Instant::now(), Duration::from_millis(1), 0.0).unwrap();
        let started = Instant::now();
        {
            let _g = ScanGuard::new(&cache, &k);
            let p = ScanParams { starts: vec![0.0], max_packets: 64, timeout: Duration::from_millis(300) };
            let bytes = b.clone();
            // yalnızca ilk 100 bayt "gelir"; paketler asla gelmez
            run_scan(&cache, &k, cache.pending(&k, &tracks), &p, tk.cancel.clone(), |c| {
                Some(NeverDelivers { inner: std::io::Cursor::new(bytes.clone()), head: 100, cancel: c.clone() })
            })
            .await;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "zaman aşımı işlemedi");
        assert_eq!(cache.states(&k, &tracks)[&tracks[0].id], AtmosState::Unknown);
        assert!(tk.cancel.load(Ordering::Relaxed), "zaman aşımında iptal kurulmalı");
        // scanning temizlendi: bekleme dolunca yeniden başlayabilir
        assert!(cache.try_begin(&k, Instant::now() + Duration::from_secs(1), Duration::from_millis(1), 0.0).is_some());
    }

    #[tokio::test]
    async fn run_scan_records_definite_result() {
        let b = mkv_with_audio("A_EAC3", &vec![eac3_joc_frame(16); 5]);
        let t = audio_of(&b);
        let cache = AtmosCache::default();
        let k = key();
        let tk = cache.try_begin(&k, Instant::now(), Duration::from_secs(1), 0.0).unwrap();
        let _g = ScanGuard::new(&cache, &k);
        let p = ScanParams { starts: vec![0.0], max_packets: 64, timeout: Duration::from_secs(10) };
        let bytes = b.clone();
        run_scan(&cache, &k, vec![t.clone()], &p, tk.cancel, |_| Some(std::io::Cursor::new(bytes.clone()))).await;
        assert_eq!(cache.states(&k, &[t.clone()])[&t.id], AtmosState::Yes);
    }

    // ----- elle yazılmış bit vektörü (ayrıştırıcıdan bağımsız) -----

    fn bits(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| *c == '0' || *c == '1').collect();
        let mut v = vec![0u8; s.len().div_ceil(8)];
        for (i, c) in s.chars().enumerate() {
            if c == '1' {
                v[i / 8] |= 0x80 >> (i % 8);
            }
        }
        v
    }

    #[test]
    fn eac3_hand_built_vector_with_mixing_and_info_metadata() {
        // ETSI TS 102 366 E.1.2.2 sırasıyla, her alan elle:
        let f = bits(concat!(
            "0000101101110111", // syncword
            "00", "000", "00000101111", // strmtyp, substreamid, frmsiz
            "00", "11", // fscod, numblkscod (6 blok)
            "111", "1", // acmod 3/2, lfeon
            "10000", "11011", // bsid 16, dialnorm
            "0", // compre
            "1", // mixmdate
            "01", // dmixmod (acmod>2)
            "100", "100", // ltrtcmixlev, lorocmixlev
            "100", "100", // ltrtsurmixlev, lorosurmixlev
            "1", "10101", // lfemixlevcode, lfemixlevcod
            "1", "101010", // pgmscle, pgmscl
            "1", "010101", // extpgmscle, extpgmscl
            "01", "11111", // mixdef=1: premixcmpsel, drcsrc, premixcmpscl (5 bit)
            "1", // frmmixcfginfoe (numblkscod 3 -> 6 blok)
            "1", "10101", "0", "0", "0", "0", "0", // blkmixcfginfoe[0..6]
            "1", // infomdate
            "000", "0", "1", // bsmod, copyrightb, origbs
            "00", // dsurexmod (acmod >= 6)
            "1", "10101010", // audprodie, mixlevel/roomtyp/adconvtyp
            "0", // sourcefscod
            "1", // addbsie
            "000001", // addbsil = 1 -> 2 bayt
            "0000000", "1", // 7 ayrılmış bit + flag_ec3_extension_type_a
            "00001100", // complexity_index_type_a = 12
        ));
        assert_eq!(eac3_joc(&f), Some(true));
        assert_eq!(eac3_joc_complexity(&f), Some(12));
    }

    #[test]
    fn eac3_reserved_strmtyp_3_is_none() {
        let mut f = eac3_joc_frame(16);
        f[2] |= 0xC0;
        assert_eq!(eac3_joc(&f), None);
    }
}
