//! HLS (fMP4/CMAF) üretimi: kutu yazıcı ve segment oluşturma.

pub mod codecs;
pub mod flac;
pub mod fmp4;
pub mod playlist;
pub mod routes;
pub mod segment;
pub mod sps;

/// Tüm video/ses `tfdt`/pts değerlerine eklenen sabit zaman kayması (sn). Böylece B-kare
/// yeniden sıralama gecikmesi (`dts = pts - D`) akış başında bile asla negatif olmaz.
/// Task 11 oynatma listeleri MEDYA zamanında kalır (EXTINF süreleri etkilenmez); kayma yalnızca
/// fMP4 zaman damgalarında ve WebVTT `X-TIMESTAMP-MAP` (MPEGTS = kayma x 90000) içindedir.
pub const HLS_TIME_OFFSET_SECS: f64 = 10.0;
