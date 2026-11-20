librqbit 9.1.0 (crates.io) kaynağı + tek yama: `ManagedTorrent::unmark_pieces(&[ValidPieceIndex])`.
Değişen yerler: chunk_tracker.rs, piece_tracker.rs, torrent_state/live/mod.rs, torrent_state/mod.rs
(hepsi "torwatch yaması" yorumuyla işaretli). Yukarı akışa girerse kaldırılabilir.

Yama 2 (Task 5, Ruling R13): `ManagedTorrent::set_selected_pieces(&[bool])` — parça düzeyinde seçim
(Window kipinde yalnızca oynatma penceresi indirilsin). Canlı ve duraklatılmış durumda çalışır;
`only_files`'a dokunmaz. Değişen yerler: chunk_tracker.rs (`set_selected_pieces`; `update_only_files`
gövdesi `replace_selected`e çıkarıldı, davranış aynı), piece_tracker.rs, torrent_state/live/mod.rs,
torrent_state/paused.rs, torrent_state/mod.rs ("torwatch yaması" yorumlu).

Yama 3 (Task 14b): `SessionOptions::restore_paused` — kalıcı oturumdan geri yüklenen torrent'ler kaydedilmiş
duraklatma durumundan bağımsız DURAKLATILMIŞ eklenir (eş bağlantısı kurulmaz; Window kipinde pencere
uygulanmadan hiçbir şey inmesin). Değişen yer: session.rs (alan, Default, geri yükleme döngüsü;
"torwatch yaması" yorumlu). Kasıtlı olarak yeni vendor kancası eklenmedi: ilk doğrulamanın sonucu
(`api_dump_haves`) zaten dışarıdan okunabilir, `WindowStore` tamamlanma bilgisini oradan alır.

Yama 3b (Task 14b): `session_persistence/json.rs` `update_db` — "FilesystemStorageFactory dışı depolama
desteklenmiyor" hatası kaldırıldı (yalnızca trace). Window kipinde `WindowStorageFactory` oturumun
`default_storage_factory`si olduğundan geri yüklemede aynı fabrika kullanılır; depolama türü kayda yazılmaz.

Yama 3c: `restore_paused` açıkken geri yüklenen torrent'in kayıtlı `output_folder`ı atılır (None); yol oturumun
varsayılan klasöründen yeniden türetilir (iOS kapsayıcı UUID'si kurulumda değişir, mutlak yol bayatlar).
