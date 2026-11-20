//! Yerel HTTP sunucusu (127.0.0.1): platformdan bağımsız JSON API ve Range destekli akış.
//! Arayüz dosyaları ve masaüstüne özel uçlar `torwatch-desktop` içindedir.

use std::io::SeekFrom;
use std::sync::Arc;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::engine::{AddSource, Engine, UiEvent};
use crate::media;

pub type St = State<Arc<Engine>>;

pub struct AppError(pub StatusCode, pub String);

impl<E: std::fmt::Display> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

pub fn not_found() -> AppError {
    AppError(StatusCode::NOT_FOUND, "bulunamadı".into())
}

pub type R<T> = Result<T, AppError>;

pub fn router(engine: Arc<Engine>) -> Router {
    let hls = crate::hls::routes::routes(engine.clone());
    Router::new()
        .route("/api/state", get(state))
        .route("/api/add", post(add_url))
        .route("/api/add-file", post(add_file))
        .route("/api/external", post(external_arg))
        .route("/api/pending/{key}", axum::routing::delete(dismiss_pending))
        .route("/api/pending/{key}/choose", post(choose))
        .route("/api/settings/ask-files", post(ask_files))
        .route("/api/settings/passthrough", post(passthrough))
        .route("/api/torrents/{id}", get(details).delete(remove))
        .route("/api/torrents/{id}/pause", post(pause))
        .route("/api/torrents/{id}/start", post(start))
        .route("/api/torrents/{id}/files", post(select_files))
        .route("/api/torrents/{id}/position", post(position))
        .route("/api/torrents/{id}/close", post(close))
        .route("/api/diag", get(diag))
        .route("/api/history", get(history))
        .route("/api/history/{info_hash}", axum::routing::delete(history_remove))
        .route("/api/torrents/{id}/stream/{file}", get(stream))
        .route("/api/torrents/{id}/subtitle/{file}", get(subtitle))
        .route("/api/torrents/{id}/buffer/{file}", get(buffer))
        .route("/api/torrents/{id}/probe/{file}", get(probe))
        .route("/api/torrents/{id}/support/{file}", get(support))
        .route("/api/torrents/{id}/audio/{file}", get(audio_stream))
        .route("/api/subtitle/convert", post(convert_subtitle))
        .with_state(engine)
        .merge(hls)
}

// ---------- durum ----------

async fn state(State(e): St) -> impl IntoResponse {
    let (focus_seq, focus_id) = e.focus_state();
    let stats = e.session.stats_snapshot();
    let stats = serde_json::to_value(&stats).unwrap_or_default();
    let mut out = json!({
        "torrents": e.list(),
        "pending": e.pending(),
        "session": {
            "down": stats["download_speed"]["mbps"],
            "up": stats["upload_speed"]["mbps"],
            "peers": stats["peers"]["live"],
        },
        "focus": { "seq": focus_seq, "id": focus_id },
        "settings": e.settings(),
        "download_dir_fallback": e.download_dir_fallback(),
        "disk": e.disk(),
        "restart_needed": e.restart_needed.load(std::sync::atomic::Ordering::Relaxed),
        "players": [],
        "rust_audio": crate::audio::RUST_AUDIO_CODECS,
        "version": env!("CARGO_PKG_VERSION"),
    });
    if let (Some(extra), Some(map)) = (e.state_extras.get().map(|f| f()), out.as_object_mut()) {
        if let Some(extra) = extra.as_object() {
            for (k, v) in extra {
                map.insert(k.clone(), v.clone());
            }
        }
    }
    Json(out)
}

async fn diag(State(e): St) -> impl IntoResponse {
    Json(e.diag().await)
}

async fn details(State(e): St, Path(id): Path<usize>) -> R<impl IntoResponse> {
    Ok(Json(e.view(id, true).ok_or_else(not_found)?))
}

// ---------- ekleme ----------

#[derive(Deserialize)]
struct AddBody {
    uri: String,
}

async fn add_url(State(e): St, Json(b): Json<AddBody>) -> R<impl IntoResponse> {
    let uri = b.uri.trim();
    if uri.is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "Magnet linki veya adres boş".into()));
    }
    let src = AddSource::from_arg(uri)
        .map_err(|_| AppError(StatusCode::BAD_REQUEST, "Geçerli bir magnet linki veya .torrent adresi değil".into()))?;
    Ok(Json(json!({ "pending": e.add(src) })))
}

#[derive(Deserialize)]
struct NameQuery {
    name: Option<String>,
}

async fn add_file(State(e): St, Query(q): Query<NameQuery>, body: Bytes) -> R<impl IntoResponse> {
    if body.is_empty() || body[0] != b'd' {
        return Err(AppError(StatusCode::BAD_REQUEST, "Bu bir .torrent dosyası değil".into()));
    }
    let key = e.add(AddSource::Bytes {
        name: q.name.unwrap_or_else(|| "Torrent dosyası".into()),
        data: body,
    });
    Ok(Json(json!({ "pending": key })))
}

#[derive(Deserialize)]
struct ExternalBody {
    arg: Option<String>,
}

/// İkinci örnek (çift tıklanan .torrent / tarayıcıdan magnet) buraya iletir.
async fn external_arg(State(e): St, Json(b): Json<ExternalBody>) -> R<impl IntoResponse> {
    (e.ui)(UiEvent::Focus);
    if let Some(arg) = b.arg.filter(|a| !a.trim().is_empty()) {
        let src = AddSource::from_arg(&arg)?;
        e.add(src);
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct ChooseBody {
    files: Vec<usize>,
}

async fn choose(State(e): St, Path(key): Path<u64>, Json(b): Json<ChooseBody>) -> R<impl IntoResponse> {
    e.choose(key, b.files)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ToggleBody {
    on: bool,
}

async fn ask_files(State(e): St, Json(b): Json<ToggleBody>) -> R<impl IntoResponse> {
    e.set_ask_files(b.on)?;
    Ok(Json(json!({ "settings": e.settings() })))
}

async fn passthrough(State(e): St, Json(b): Json<ToggleBody>) -> R<impl IntoResponse> {
    e.set_passthrough(b.on)?;
    Ok(Json(json!({ "settings": e.settings() })))
}

async fn dismiss_pending(State(e): St, Path(key): Path<u64>) -> impl IntoResponse {
    e.dismiss_pending(key);
    StatusCode::NO_CONTENT
}

// ---------- torrent eylemleri ----------

async fn pause(State(e): St, Path(id): Path<usize>) -> R<impl IntoResponse> {
    let h = e.handle(id)?;
    let _ = h;
    e.pause_torrent(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn start(State(e): St, Path(id): Path<usize>) -> R<impl IntoResponse> {
    e.handle(id)?;
    if let Some(msg) = e.window_blocked(id) {
        return Err(AppError(StatusCode::CONFLICT, msg));
    }
    e.start_torrent(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RemoveQuery {
    files: Option<u8>,
}

async fn remove(State(e): St, Path(id): Path<usize>, Query(q): Query<RemoveQuery>) -> R<impl IntoResponse> {
    tracing::info!(id, delete_files = q.files == Some(1), "torrent kaldırılıyor");
    Ok(Json(e.remove(id, q.files == Some(1)).await?))
}

#[derive(Deserialize)]
struct SelectBody {
    files: Vec<usize>,
    #[serde(default)]
    replace: bool,
}

async fn select_files(State(e): St, Path(id): Path<usize>, Json(b): Json<SelectBody>) -> R<impl IntoResponse> {
    e.select_files(id, &b.files, b.replace).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- akış ----------

fn parse_range(h: &HeaderMap, len: u64) -> Option<Result<(u64, u64), ()>> {
    let v = h.get(header::RANGE)?.to_str().ok()?;
    let spec = v.strip_prefix("bytes=")?.split(',').next()?.trim();
    let (s, en) = spec.split_once('-')?;
    let r = if s.is_empty() {
        // son N bayt
        let n: u64 = en.parse().ok()?;
        (len.saturating_sub(n), len)
    } else {
        let start: u64 = s.parse().ok()?;
        let end = if en.is_empty() { len } else { en.parse::<u64>().ok()?.saturating_add(1).min(len) };
        (start, end)
    };
    Some(if r.0 >= len || r.1 <= r.0 { Err(()) } else { Ok(r) })
}

async fn stream(State(e): St, Path((id, file)): Path<(usize, usize)>, headers: HeaderMap) -> R<Response> {
    let name = e.file_name(id, file).ok_or_else(not_found)?;
    let h = e.handle(id)?;
    let mut s = h.stream(file).await?;
    let len = s.len();

    let mut out = HeaderMap::new();
    out.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    out.insert(header::CONTENT_TYPE, HeaderValue::from_static(media::mime_for(&name)));
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));

    let (status, start, end) = match parse_range(&headers, len) {
        None => (StatusCode::OK, 0, len),
        Some(Ok((a, b))) => (StatusCode::PARTIAL_CONTENT, a, b),
        Some(Err(())) => {
            out.insert(header::CONTENT_RANGE, format!("bytes */{len}").parse().unwrap());
            return Ok((StatusCode::RANGE_NOT_SATISFIABLE, out).into_response());
        }
    };
    if status == StatusCode::PARTIAL_CONTENT {
        out.insert(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{len}", end - 1).parse().unwrap(),
        );
    }
    out.insert(header::CONTENT_LENGTH, (end - start).to_string().parse().unwrap());
    if start > 0 {
        s.seek(SeekFrom::Start(start)).await?;
    }
    let body = Body::from_stream(ReaderStream::with_capacity(s.take(end - start), 256 * 1024));
    Ok((status, out, body).into_response())
}

/// Torrent içindeki altyazıyı WebVTT olarak verir (SRT/ASS ve cp1254 dönüştürülür).
async fn subtitle(State(e): St, Path((id, file)): Path<(usize, usize)>) -> R<Response> {
    let name = e.file_name(id, file).ok_or_else(not_found)?;
    e.select_files(id, &[file], false).await?;
    let h = e.handle(id)?;
    let s = h.stream(file).await?;
    if s.len() > 20 * 1024 * 1024 {
        return Err(AppError(StatusCode::PAYLOAD_TOO_LARGE, "altyazı dosyası çok büyük".into()));
    }
    let mut buf = Vec::with_capacity(s.len() as usize);
    s.take(20 * 1024 * 1024).read_to_end(&mut buf).await?;
    Ok(vtt_response(media::to_vtt(&name, &buf)))
}

fn vtt_response(text: String) -> Response {
    ([(header::CONTENT_TYPE, "text/vtt; charset=utf-8")], text).into_response()
}

async fn convert_subtitle(Query(q): Query<NameQuery>, body: Bytes) -> impl IntoResponse {
    vtt_response(media::to_vtt(q.name.as_deref().unwrap_or("a.srt"), &body))
}

#[derive(Deserialize)]
struct BufferQuery {
    t: Option<f64>,
    dur: Option<f64>,
    br: Option<f64>,
    stalls: Option<u32>,
    /// 1: salt okunur — konum kaydedilmez, önden okuma başlatılmaz (ana ekran kartları).
    peek: Option<u8>,
}

/// Oynatma konumunu bildirir (önden okumayı sürdürür) ve tampon durumunu döner. `?peek=1` ile
/// yalnızca mevcut durum okunur (yan etkisiz).
async fn buffer(State(e): St, Path((id, file)): Path<(usize, usize)>, Query(q): Query<BufferQuery>) -> R<impl IntoResponse> {
    let h = e.handle(id)?;
    if q.peek.unwrap_or(0) != 0 {
        let report = e.buffering.peek(
            &e.session,
            h,
            id,
            file,
            q.t.unwrap_or(0.0).max(0.0),
            q.dur,
            q.br,
            q.stalls.unwrap_or(0),
        )?;
        return Ok(Json(report));
    }
    let report = e.buffering.report(
        &e.session,
        h,
        id,
        file,
        q.t.unwrap_or(0.0).max(0.0),
        q.dur,
        q.br,
        q.stalls.unwrap_or(0),
    )?;
    Ok(Json(report))
}

/// Dosyanın kapsayıcı/kodek bilgisi; saf Rust (MKV/MP4 başlıkları, yalnızca gereken baytlar okunur).
async fn probe(State(e): St, Path((id, file)): Path<(usize, usize)>) -> R<impl IntoResponse> {
    let mut out = e.probe(id, file).await?;
    // Dolby Atmos: bitstream taramasının (önbellekli, engellemeyen) sonucu iz bazında eklenir.
    crate::audio::merge_atmos(&mut out, &e.atmos(id, file));
    Ok(Json(out))
}

#[derive(Deserialize)]
struct SupportQuery {
    /// 1: bu iPhone AV1'i donanımda çözer. Varsayılan 0.
    av1: Option<u8>,
}

/// iPhone destek raporu: her iz için dosyadaki durum, iPhone'daki sonuç ve HLS paketleme planı.
async fn support(State(e): St, Path((id, file)): Path<(usize, usize)>, Query(q): Query<SupportQuery>) -> R<impl IntoResponse> {
    let info = e.media_info(id, file).await?;
    let atmos = e.atmos(id, file);
    let caps = crate::support::DeviceCaps { av1_hw: q.av1.unwrap_or(0) != 0 };
    Ok(Json(crate::support::report(&info, &atmos, caps)))
}

#[derive(Deserialize)]
struct AudioQuery {
    t: Option<f64>,
    /// Kapsayıcıdaki iz numarası (probe çıktısındaki `id`).
    track: Option<u64>,
}

/// Tarayıcının çözemediği ses izini Rust'ta çözüp 24-bit çok kanallı WAV akışı olarak verir.
async fn audio_stream(State(e): St, Path((id, file)): Path<(usize, usize)>, Query(q): Query<AudioQuery>) -> R<Response> {
    let info = e.media_info(id, file).await?;
    let track = info
        .tracks
        .iter()
        .find(|t| matches!(t.kind, crate::demux::TrackKind::Audio) && q.track.is_none_or(|n| t.id == n))
        .cloned()
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "ses izi yok".into()))?;
    crate::audio::decoder_for(&track.codec).map_err(|err| AppError(StatusCode::UNPROCESSABLE_ENTITY, format!("{err:#}")))?;
    let start = q.t.unwrap_or(0.0).max(0.0);
    let stream = e.handle(id)?.stream(file).await?;
    let reader = tokio_util::io::SyncIoBridge::new(stream);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(6);
    tokio::task::spawn_blocking(move || {
        let mut send = |b: Bytes| tx.blocking_send(Ok(b)).is_ok();
        if let Err(err) = crate::audio::stream_wav(reader, &track, start, &mut send) {
            tracing::warn!("ses akışı durdu: {err:#}");
        }
    });
    let body = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|x| (x, rx)) });
    Ok((
        [(header::CONTENT_TYPE, "audio/wav"), (header::CACHE_CONTROL, "no-store")],
        Body::from_stream(body),
    )
        .into_response())
}

// ---------- yaşam döngüsü ----------

/// Oynatma konumunu geçmişe yazar ve tamponlama playhead'ini taşır.
async fn position(State(e): St, Path(id): Path<usize>, Json(b): Json<crate::engine::PositionUpdate>) -> R<impl IntoResponse> {
    e.position(id, b).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Filmi kapatır: geçmişi yazar, torrenti oturumdan çıkarır; Window kipinde verisini siler.
async fn close(State(e): St, Path(id): Path<usize>) -> R<impl IntoResponse> {
    e.close(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn history(State(e): St) -> impl IntoResponse {
    Json(e.history.list())
}

async fn history_remove(State(e): St, Path(info_hash): Path<String>) -> impl IntoResponse {
    e.history_remove(&info_hash);
    StatusCode::NO_CONTENT
}
