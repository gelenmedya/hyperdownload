//! Kayan pencere depolaması (iOS): librqbit için parça başına dosya tutan, parçaları tahliye
//! edebilen depolama arka ucu.
//!
//! Disk düzeni: `<kök>/<info_hash>/<parça_no>.bin`. Bir parçanın silinmesi = dosyasının
//! silinmesi. librqbit'in (dosya, ofset) adresleri `init`/`create` anında kurulan dosya
//! tablosuyla küresel torrent ofsetine, oradan parça numarasına çevrilir.
//!
//! # Eşzamanlılık
//!
//! librqbit parçaları 16 KiB'lık parçacıklarla, farklı iş parçacıklarından ve sırasız yazar;
//! tamamlanınca SHA-1 için okur, doğrulanınca `on_piece_completed` çağırır. Tahliye bu
//! yazma/doğrulama hattına karışmamalıdır. Kural: **yalnızca `complete` kümesindeki parçalar
//! silinebilir**; bir parça ancak librqbit doğrulayıp `on_piece_completed` çağırınca bu kümeye
//! girer. Yazılmakta olan ya da doğrulanmayı bekleyen (kısmi) parça kümede değildir, bu yüzden
//! asla silinmez. Tersine, tamamlanmış bir parçaya yeniden yazılmaya başlanırsa (geri alınıp
//! yeniden indirilirken) `pwrite_all` onu kümeden çıkarır; böylece yeni yazım sürerken tahliye
//! ona dokunamaz. Küme `Mutex` ile korunur; `set_keep` ve `pwrite_all` aynı kilitte sıralanır.
//! Dosya G/Ç'sinin kendisi kilit dışındadır (pread/pwrite paralel). Silinen bir parçayı o anda
//! okuyan okuyucu ya eski veriyi ya da `Err` alır, asla sıfır/çöp görmez.
//!
//! Tahliyeden sonra librqbit hâlâ parçaya "sahibim" der; yeniden indirme için
//! [`WindowStore::forget_pieces`] gerekir (vendored `ManagedTorrent::unmark_pieces`).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::ops::{Range, RangeInclusive};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, bail};
use librqbit::storage::{BoxStorageFactory, StorageFactory, StorageFactoryExt, TorrentStorage};
use librqbit::{ManagedTorrent, ManagedTorrentShared, TorrentMetadata};
use librqbit_core::lengths::{Lengths, ValidPieceIndex};

/// Tahliye sonucu.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvictReport {
    /// Bu çağrıda silinen parçalar (artan sırada).
    pub evicted: Vec<u32>,
    /// Çağrı sonrası bu torrent'in diskte kapladığı bayt.
    pub bytes_on_disk: u64,
}

/// Torrent başına depoları (info hash → `WindowStore`) tutar. `Engine` bir tane sahiplenir.
pub struct WindowRegistry {
    root: PathBuf,
    stores: Mutex<HashMap<String, Arc<WindowStore>>>,
}

impl WindowRegistry {
    /// `root`: parça klasörlerinin üst dizini (`<data_dir>/window`).
    pub fn new(root: PathBuf) -> Self {
        Self { root, stores: Mutex::new(HashMap::new()) }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 40 karakterlik onaltılık info hash ile kayıtlı depo.
    pub fn store(&self, info_hash: &str) -> Option<Arc<WindowStore>> {
        lock(&self.stores).get(info_hash).cloned()
    }

    /// Depoyu kayıttan çıkarır ve parça klasörünü siler (torrent kaldırılırken).
    pub fn remove(&self, info_hash: &str) {
        lock(&self.stores).remove(info_hash);
        let _ = std::fs::remove_dir_all(self.root.join(info_hash));
    }

    /// Açılış temizliği: `keep` kümesinde olmayan her şeyi (oturumda karşılığı olmayan `<info_hash>`
    /// dizinleri ve artık dosyalar) siler. Silinenlerin sayısını döner.
    pub fn remove_orphans(&self, keep: &std::collections::HashSet<String>) -> usize {
        let Ok(rd) = std::fs::read_dir(&self.root) else { return 0 };
        let mut n = 0;
        for e in rd.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir && keep.contains(&name) {
                continue;
            }
            let path = e.path();
            let r = if is_dir { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
            match r {
                Ok(()) => n += 1,
                Err(err) => tracing::warn!("yetim pencere girdisi silinemedi: {path:?}: {err}"),
            }
        }
        n
    }

    fn get_or_create(
        &self,
        info_hash: &str,
        lengths: Lengths,
        files: Vec<FileSpan>,
    ) -> anyhow::Result<Arc<WindowStore>> {
        let mut g = lock(&self.stores);
        if let Some(s) = g.get(info_hash) {
            // Hata sonrası yeniden başlatma: aynı depo (tamamlanmış parça kümesi) korunur.
            return Ok(s.clone());
        }
        // Önceki oturumdan kalan parça dosyaları SİLİNMEZ (yeniden açılışta pencere korunur);
        // librqbit'in ilk doğrulaması hangilerinin sağlam olduğunu belirler, `adopt_verified` onları
        // `complete` kümesine alır ve geri kalanını siler.
        let dir = self.root.join(info_hash);
        let store = Arc::new(WindowStore::new(dir, lengths, files)?);
        g.insert(info_hash.to_owned(), store.clone());
        Ok(store)
    }
}

/// `SessionOptions.default_storage_factory` için fabrika.
#[derive(Clone)]
pub struct WindowStorageFactory {
    registry: Arc<WindowRegistry>,
}

impl WindowStorageFactory {
    pub fn new(registry: Arc<WindowRegistry>) -> Self {
        Self { registry }
    }
}

impl StorageFactory for WindowStorageFactory {
    type Storage = WindowStorage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<WindowStorage> {
        let files = metadata
            .file_infos
            .iter()
            .map(|f| FileSpan { offset: f.offset_in_torrent, len: f.len })
            .collect();
        let store = self.registry.get_or_create(
            &shared.info_hash.as_string(),
            *metadata.lengths(),
            files,
        )?;
        Ok(WindowStorage { store })
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

#[derive(Debug, Clone, Copy)]
struct FileSpan {
    offset: u64,
    len: u64,
}

#[derive(Default)]
struct Inner {
    /// librqbit'in doğrulayıp `on_piece_completed` ile bildirdiği (yani silinebilir) parçalar.
    complete: std::collections::BTreeSet<u32>,
    /// Dosya başına son `set_keep` parça aralıkları (kaydı olmayan dosya her şeyi tutar).
    keeps: HashMap<usize, Vec<RangeInclusive<u32>>>,
}

/// Tek torrent'in parça dosyaları.
pub struct WindowStore {
    dir: PathBuf,
    lengths: Lengths,
    files: Vec<FileSpan>,
    inner: Mutex<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(unix)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(f, buf, off)
}
#[cfg(windows)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(f, buf, off)
}
#[cfg(unix)]
fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(f, buf, off)
}
#[cfg(windows)]
fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(f, buf, off)
}

fn read_exact_at(f: &File, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match read_at(f, buf, off) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_all_at(f: &File, mut buf: &[u8], mut off: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match write_at(f, buf, off) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

impl WindowStore {
    fn new(dir: PathBuf, lengths: Lengths, files: Vec<FileSpan>) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("parça klasörü oluşturulamadı: {dir:?}"))?;
        Ok(Self { dir, lengths, files, inner: Mutex::new(Inner::default()) })
    }

    /// Test ve alt katmanlar için: `(ofset, uzunluk)` dosya tablosundan doğrudan depo kurar.
    pub fn with_layout(
        dir: PathBuf,
        lengths: Lengths,
        files: &[(u64, u64)],
    ) -> anyhow::Result<Self> {
        let files = files.iter().map(|&(offset, len)| FileSpan { offset, len }).collect();
        Self::new(dir, lengths, files)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn piece_path(&self, piece: u32) -> PathBuf {
        self.dir.join(format!("{piece}.bin"))
    }

    /// (dosya, ofset, uzunluk) → `(parça, parça içi ofset, parça parçası uzunluğu)` listesi.
    fn segments(
        &self,
        file_id: usize,
        offset: u64,
        len: usize,
    ) -> anyhow::Result<Vec<(u32, u64, usize)>> {
        let f = self.files.get(file_id).with_context(|| format!("geçersiz dosya {file_id}"))?;
        let end = offset.checked_add(len as u64).context("ofset taşması")?;
        if end > f.len {
            bail!("dosya {file_id} sınırı aşıldı: {offset}+{len} > {}", f.len);
        }
        let plen = self.lengths.default_piece_length() as u64;
        let mut abs = f.offset + offset;
        let mut left = len as u64;
        let mut out = Vec::new();
        while left > 0 {
            let piece = u32::try_from(abs / plen).context("parça no taşması")?;
            let in_piece = abs % plen;
            let n = left.min(plen - in_piece);
            out.push((piece, in_piece, n as usize));
            abs += n;
            left -= n;
        }
        Ok(out)
    }

    /// Eksik parçada `Err` döner (sıfır doldurmaz).
    pub fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let mut done = 0usize;
        for (piece, in_piece, n) in self.segments(file_id, offset, buf.len())? {
            let f = File::open(self.piece_path(piece))
                .with_context(|| format!("parça {piece} diskte yok"))?;
            read_exact_at(&f, &mut buf[done..done + n], in_piece)
                .with_context(|| format!("parça {piece} okunamadı"))?;
            done += n;
        }
        Ok(())
    }

    pub fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let mut done = 0usize;
        for (piece, in_piece, n) in self.segments(file_id, offset, buf.len())? {
            // Yeniden yazılan parça artık "tamamlanmış" değildir: tahliye ona dokunamaz.
            lock(&self.inner).complete.remove(&piece);
            let f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(self.piece_path(piece))
                .with_context(|| format!("parça {piece} açılamadı"))?;
            write_all_at(&f, &buf[done..done + n], in_piece)
                .with_context(|| format!("parça {piece} yazılamadı"))?;
            done += n;
        }
        Ok(())
    }

    /// librqbit parçayı doğruladı: artık tahliye edilebilir.
    pub fn mark_complete(&self, piece: u32) {
        lock(&self.inner).complete.insert(piece);
    }

    /// Yeniden açılışta: librqbit'in ilk doğrulaması sonucu (`have[i]`: parça i hash'i tutuyor)
    /// diskteki parça dosyalarıyla eşlenir. Doğrulanmış parçalar `complete` (tahliye edilebilir)
    /// kümesine girer; doğrulanmamış (bozuk/kısmi/yabancı) parça dosyaları silinir. YALNIZCA
    /// torrent duraklatılmışken (yazım yokken) çağrılmalıdır. `(benimsenen, silinen)` döner.
    pub fn adopt_verified(&self, have: &[bool]) -> (usize, usize) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return (0, 0) };
        let (mut kept, mut removed) = (0, 0);
        let mut g = lock(&self.inner);
        for e in rd.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            let piece = name.strip_suffix(".bin").and_then(|n| n.parse::<u32>().ok());
            match piece {
                Some(p) if have.get(p as usize).copied().unwrap_or(false) => {
                    g.complete.insert(p);
                    kept += 1;
                }
                _ => match std::fs::remove_file(e.path()) {
                    Ok(()) => removed += 1,
                    Err(err) => tracing::warn!("parça dosyası silinemedi: {name}: {err}"),
                },
            }
        }
        (kept, removed)
    }

    /// Diskteki parça dosyası sayısı (tanılama).
    pub fn pieces_on_disk(&self) -> usize {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return 0 };
        rd.filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".bin")).count()
    }

    /// Kayıtlı `set_keep` parça aralıklarının toplam sayısı (tanılama: tutulan/keep-alive aralıkları).
    pub fn keep_range_count(&self) -> usize {
        lock(&self.inner).keeps.values().map(|v| v.len()).sum()
    }

    /// Diskteki parça dosyalarının toplam baytı.
    pub fn bytes_on_disk(&self) -> u64 {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return 0 };
        rd.filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".bin"))
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum()
    }

    /// Boş olmayan `set_keep` kaydı olan dosyalar (çok videolu torrent'te bırakılan pencereler).
    pub fn kept_files(&self) -> Vec<usize> {
        let g = lock(&self.inner);
        let mut v: Vec<usize> = g.keeps.iter().filter(|(_, k)| !k.is_empty()).map(|(f, _)| *f).collect();
        v.sort_unstable();
        v
    }

    /// `file_id` dosyasının `keep` bayt aralıklarını (dosya ofsetleri, sonu hariç) kaydeder ve
    /// tamamlanmış parçalardan **tahliye edilebilir olanları** siler. Bir parça ancak şu
    /// koşulda silinir: değdiği HER dosya için bir `set_keep` kaydı vardır ve parça o kayıtların
    /// hiçbirinin aralığına değmez. Böylece (a) hiç `set_keep` verilmemiş dosyaların (ör. dış
    /// altyazı) parçaları ve (b) komşu dosyayla paylaşılan sınır parçaları, o dosya hâlâ
    /// ihtiyaç duyuyorsa silinmez. Bir aralığa değen parça asla silinmez. Kısmi (yazılmakta olan)
    /// parçalara dokunulmaz.
    pub fn set_keep(&self, file_id: usize, keep: &[Range<u64>]) -> EvictReport {
        let Some(f) = self.files.get(file_id).copied() else {
            return EvictReport { evicted: vec![], bytes_on_disk: self.bytes_on_disk() };
        };
        let plen = self.lengths.default_piece_length() as u64;
        let keep_pieces: Vec<RangeInclusive<u32>> = keep
            .iter()
            .filter_map(|r| {
                let end = r.end.min(f.len);
                if r.start >= end {
                    return None;
                }
                let first = (f.offset + r.start) / plen;
                let last = (f.offset + end - 1) / plen;
                Some(first as u32..=last as u32)
            })
            .collect();

        let mut evicted = Vec::new();
        {
            let mut g = lock(&self.inner);
            g.keeps.insert(file_id, keep_pieces);
            let victims: Vec<u32> = g
                .complete
                .iter()
                .copied()
                .filter(|&p| self.evictable(&g.keeps, p, plen))
                .collect();
            for p in victims {
                match std::fs::remove_file(self.piece_path(p)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => {
                        tracing::warn!("parça {p} silinemedi: {e}");
                        continue;
                    }
                }
                g.complete.remove(&p);
                evicted.push(p);
            }
        }
        EvictReport { evicted, bytes_on_disk: self.bytes_on_disk() }
    }

    /// Parçanın değdiği tüm dosyalar `keeps`'te kayıtlı ve hiçbiri parçayı tutmuyorsa true.
    fn evictable(&self, keeps: &HashMap<usize, Vec<RangeInclusive<u32>>>, piece: u32, plen: u64) -> bool {
        let p_start = piece as u64 * plen;
        let p_end = p_start + plen;
        for (id, f) in self.files.iter().enumerate() {
            if f.len == 0 || f.offset >= p_end || f.offset + f.len <= p_start {
                continue;
            }
            match keeps.get(&id) {
                None => return false,
                Some(k) if k.iter().any(|r| r.contains(&piece)) => return false,
                Some(_) => {}
            }
        }
        true
    }

    /// Silinmiş parçaları librqbit'in "sahibim" kümesinden çıkarır; seçili parçalar yeniden
    /// indirilebilir olur. Önce tüm indeksler doğrulanır. Her parça için (varsa) dosya silinir,
    /// sonra yerelde başarıyla silinenlerin hepsi librqbit'e bildirilir (sonraki bir parça hata
    /// verse de); hata en sonda döner. Sıra, yeniden indirme başladıktan sonra silme
    /// yapılmasını önler.
    pub fn forget_pieces(&self, handle: &ManagedTorrent, pieces: &[u32]) -> anyhow::Result<()> {
        let (done, first_err) = self.forget_local(pieces)?;
        handle.unmark_pieces(&done)?;
        first_err.map_or(Ok(()), Err)
    }

    /// `forget_pieces`'in yerel yarısı: indeksleri doğrular (geçersizse hiçbir şey silmeden Err),
    /// sonra parça dosyalarını siler. Silinenleri ve (varsa) ilk silme hatasını döner.
    fn forget_local(
        &self,
        pieces: &[u32],
    ) -> anyhow::Result<(Vec<ValidPieceIndex>, Option<anyhow::Error>)> {
        let mut valid: Vec<(u32, ValidPieceIndex)> = Vec::with_capacity(pieces.len());
        for &p in pieces {
            let v = self
                .lengths
                .validate_piece_index(p)
                .with_context(|| format!("geçersiz parça {p}"))?;
            valid.push((p, v));
        }
        let mut done: Vec<ValidPieceIndex> = Vec::with_capacity(valid.len());
        let mut first_err: Option<anyhow::Error> = None;
        let mut g = lock(&self.inner);
        for (p, v) in valid {
            g.complete.remove(&p);
            match std::fs::remove_file(self.piece_path(p)) {
                Ok(()) => done.push(v),
                Err(e) if e.kind() == io::ErrorKind::NotFound => done.push(v),
                Err(e) => {
                    first_err.get_or_insert(
                        anyhow::Error::new(e).context(format!("parça {p} silinemedi")),
                    );
                }
            }
        }
        Ok((done, first_err))
    }
}

/// librqbit'in `Box<dyn TorrentStorage>` olarak tuttuğu ince sarmalayıcı; durumu `WindowStore`
/// paylaşır (`take()` ve kayıt defteri aynı depoyu görür).
pub struct WindowStorage {
    store: Arc<WindowStore>,
}

impl TorrentStorage for WindowStorage {
    fn init(&mut self, _shared: &ManagedTorrentShared, _metadata: &TorrentMetadata) -> anyhow::Result<()> {
        std::fs::create_dir_all(self.store.dir())?;
        Ok(())
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.store.pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.store.pwrite_all(file_id, offset, buf)
    }

    fn remove_file(&self, _file_id: usize, _filename: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn remove_directory_if_empty(&self, _path: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn ensure_file_length(&self, _file_id: usize, _length: u64) -> anyhow::Result<()> {
        Ok(())
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(WindowStorage { store: self.store.clone() }))
    }

    fn on_piece_completed(&self, piece_index: ValidPieceIndex) -> anyhow::Result<()> {
        self.store.mark_complete(piece_index.get());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: u32 = 1024;

    fn store(files: &[(u64, u64)]) -> (tempfile::TempDir, WindowStore) {
        let tmp = tempfile::tempdir().unwrap();
        let total: u64 = files.iter().map(|f| f.1).sum();
        let s = WindowStore::with_layout(
            tmp.path().join("t"),
            Lengths::new(total, P).unwrap(),
            files,
        )
        .unwrap();
        (tmp, s)
    }

    #[test]
    fn partial_pieces_are_never_evicted() {
        let (_t, s) = store(&[(0, 10 * P as u64)]);
        s.pwrite_all(0, 0, &vec![1u8; P as usize]).unwrap(); // parça 0: tamam
        s.mark_complete(0);
        s.pwrite_all(0, 5 * P as u64, &[2u8; 100]).unwrap(); // parça 5: kısmi (doğrulanmadı)
        let rep = s.set_keep(0, &[8 * P as u64..9 * P as u64]);
        assert_eq!(rep.evicted, vec![0]);
        let mut b = [0u8; 100];
        s.pread_exact(0, 5 * P as u64, &mut b).unwrap();
        assert_eq!(b, [2u8; 100]);
    }

    #[test]
    fn rewriting_a_complete_piece_protects_it_from_eviction() {
        let (_t, s) = store(&[(0, 4 * P as u64)]);
        s.pwrite_all(0, 0, &vec![1u8; P as usize]).unwrap();
        s.mark_complete(0);
        // Geri alınıp yeniden indirilirken yazım başladı.
        s.pwrite_all(0, 0, &[9u8; 16]).unwrap();
        let rep = s.set_keep(0, &[3 * P as u64..4 * P as u64]);
        assert!(rep.evicted.is_empty());
        let mut b = [0u8; 16];
        s.pread_exact(0, 0, &mut b).unwrap();
        assert_eq!(b, [9u8; 16]);
    }

    #[test]
    fn missing_piece_is_error_not_zeroes_and_oob_is_rejected() {
        let (_t, s) = store(&[(0, 4 * P as u64)]);
        let mut b = [7u8; 8];
        assert!(s.pread_exact(0, 0, &mut b).is_err());
        assert_eq!(b, [7u8; 8]);
        assert!(s.pwrite_all(0, 4 * P as u64 - 4, &[0u8; 8]).is_err());
        assert!(s.pread_exact(3, 0, &mut b).is_err());
    }

    #[test]
    fn keep_maps_file_offsets_through_file_start() {
        // Dosya 1, 2.5 parça ofsetinde başlar.
        let (_t, s) = store(&[(0, 2 * P as u64 + P as u64 / 2), (2 * P as u64 + P as u64 / 2, 6 * P as u64)]);
        for p in 0..8u32 {
            std::fs::write(s.piece_path(p), vec![p as u8; P as usize]).unwrap();
            s.mark_complete(p);
        }
        // Dosya 1'in 0..1 baytı küresel 2.5*P'de: parça 2.
        let rep = s.set_keep(1, &[0..1]);
        // Dosya 0 için kayıt yok: onun parçaları (0, 1 ve paylaşılan 2) kalır.
        assert_eq!(rep.bytes_on_disk, 3 * P as u64);
        let mut left = Vec::new();
        for p in 0..8u32 {
            if s.piece_path(p).exists() {
                left.push(p);
            }
        }
        assert_eq!(left, vec![0, 1, 2]);
    }

    #[test]
    fn adopt_verified_keeps_verified_and_deletes_the_rest() {
        let (_t, s) = store(&[(0, 4 * P as u64)]);
        for p in 0..4u32 {
            std::fs::write(s.piece_path(p), vec![p as u8; P as usize]).unwrap();
        }
        std::fs::write(s.dir().join("junk.txt"), b"x").unwrap();
        let (kept, removed) = s.adopt_verified(&[true, false, true, false]);
        assert_eq!((kept, removed), (2, 3));
        assert!(s.piece_path(0).exists() && s.piece_path(2).exists());
        assert!(!s.piece_path(1).exists() && !s.piece_path(3).exists());
        // Benimsenenler artık tahliye edilebilir.
        let rep = s.set_keep(0, &[3 * P as u64..4 * P as u64]);
        assert_eq!(rep.evicted, vec![0, 2]);
    }

    #[test]
    fn remove_orphans_keeps_only_listed_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = WindowRegistry::new(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("aaaa")).unwrap();
        std::fs::create_dir_all(tmp.path().join("bbbb")).unwrap();
        std::fs::write(tmp.path().join("stray.bin"), b"x").unwrap();
        let keep = ["aaaa".to_string()].into_iter().collect();
        assert_eq!(reg.remove_orphans(&keep), 2);
        assert!(tmp.path().join("aaaa").exists());
        assert!(!tmp.path().join("bbbb").exists() && !tmp.path().join("stray.bin").exists());
    }

    #[test]
    fn forget_with_invalid_index_changes_nothing() {
        let (_t, s) = store(&[(0, 4 * P as u64)]);
        for p in 0..4u32 {
            std::fs::write(s.piece_path(p), vec![1u8; P as usize]).unwrap();
            s.mark_complete(p);
        }
        assert!(s.forget_local(&[1, 2, 99]).is_err());
        assert_eq!(s.bytes_on_disk(), 4 * P as u64);
        assert_eq!(lock(&s.inner).complete.len(), 4);
        // Geçerli indeksler tam silinir ve geri bildirilir.
        let (done, err) = s.forget_local(&[1, 2]).unwrap();
        assert_eq!(done.len(), 2);
        assert!(err.is_none());
        assert_eq!(s.bytes_on_disk(), 2 * P as u64);
    }
}
