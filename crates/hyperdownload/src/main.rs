//! HyperDownload, the download manager add-on of rustyfiles.
//!
//!   hyperdownload serve [--data <folder>] [--downloads <folder>]
//!       The program rustyfiles starts: a local server on 127.0.0.1 (a free port) with the aria2-compatible JSON-RPC at `/jsonrpc`
//!       and the torrent engine's streams under `/api/...`. Its port and its secret are written to `<data>\rpc.json`.
//!   hyperdownload get <url> [folder]          one download on the command line (to try the engine)
//!   hyperdownload continue <file.hdpart.json> goes on with one

mod host;
mod hub;
mod register;
mod rpc;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request as HttpRequest, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use hd_engine::{Download, Phase, Request};
use hd_torrent::engine::{Engine, EngineConfig, StorageMode};

use crate::hub::Hub;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        // the browser starts the host with the extension's origin as its argument
        Some("--native-host") => host::run(),
        Some("--register") => match register::register() {
            Ok(p) => println!("registered for Chrome, Edge, Brave and Chromium: {}", p.display()),
            Err(e) => {
                eprintln!("not registered: {e}");
                std::process::exit(1);
            }
        },
        Some("--unregister") => {
            register::unregister();
            println!("unregistered");
        }
        Some(a) if a.starts_with("chrome-extension://") => host::run(),
        Some("get") if args.len() >= 2 => {
            let dir = args.get(2).map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            follow(Download::start(Request { url: args[1].clone(), dir, ..Default::default() }));
        }
        Some("continue") if args.len() >= 2 => match Download::continue_from(std::path::Path::new(&args[1]), Request::default()) {
            Ok(d) => follow(d),
            Err(e) => {
                eprintln!("{}: {e}", args[1]);
                std::process::exit(2);
            }
        },
        _ => {
            eprintln!("hyperdownload serve [--data <folder>] [--downloads <folder>]\nhyperdownload get <url> [folder]\nhyperdownload continue <file.hdpart.json>");
            std::process::exit(2);
        }
    }
}

/// The value after `--name` among the arguments.
fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn serve(args: &[String]) {
    let data = arg_value(args, "--data").map(PathBuf::from).unwrap_or_else(hub::default_data_dir);
    let downloads = arg_value(args, "--downloads").map(PathBuf::from).unwrap_or_else(hub::default_downloads);
    if let Err(e) = hub::ensure_dir(&data) {
        eprintln!("{}: {e}", data.display());
        std::process::exit(1);
    }
    let rt = tokio::runtime::Runtime::new().expect("tokio");
    rt.block_on(async move {
        let engine = match Engine::start(
            EngineConfig { data_dir: data.join("torrent"), default_download_dir: downloads.clone(), storage: StorageMode::Disk, listen_port: 6891 },
            Arc::new(|_| {}),
        )
        .await
        {
            Ok(e) => e,
            Err(e) => {
                eprintln!("the torrent engine did not start: {e:#}");
                std::process::exit(1);
            }
        };
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.expect("a local port");
        let port = listener.local_addr().expect("its port").port();
        let _ = engine.port.set(port);
        // the file list of a torrent with more than one file waits for the user's choice (rustyfiles asks: hd.choices, hd.choose)
        let _ = engine.set_ask_files(true);
        let _ = engine.set_ask_all_files(true);
        let hub = Arc::new(Hub::new(secret(), data.clone(), downloads, engine.clone()));
        write_rpc_file(&data, port, &hub.token);

        // the list on the disk follows the downloads while they run
        let saver = hub.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                saver.save();
            }
        });

        let app = Router::new()
            .route("/jsonrpc", post(jsonrpc))
            .with_state(hub.clone())
            .merge(hd_torrent::server::router(engine))
            .layer(middleware::from_fn_with_state((hub.clone(), port), guard));
        eprintln!("hyperdownload {} on 127.0.0.1:{port}", env!("CARGO_PKG_VERSION"));
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("the server stopped: {e}");
        }
    });
}

/// 32 random bytes as hex: the secret every request carries.
fn secret() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("random bytes");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `<data>\rpc.json`: where rustyfiles finds the program (in the user's own local app data, which other users cannot read).
fn write_rpc_file(data: &std::path::Path, port: u16, token: &str) {
    let v = serde_json::json!({ "port": port, "token": token, "pid": std::process::id(), "version": env!("CARGO_PKG_VERSION") });
    let tmp = data.join("rpc.json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(&v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, data.join("rpc.json"));
    }
}

/// Ends the program cleanly: the lists written, the torrents paused, `rpc.json` removed.
pub async fn shutdown(hub: &Hub) {
    hub.save();
    for (_, d, _) in hub.http_jobs() {
        d.pause();
    }
    hub.torrent.shutdown().await;
    let _ = std::fs::remove_file(hub.data.join("rpc.json"));
    std::process::exit(0);
}

/// Every request: only from this machine's loopback by its own address (no other Host: a web page that names its own domain and
/// points it at 127.0.0.1 is turned away), never from a web page (no Origin), and with the secret: in the JSON-RPC body (as aria2),
/// or for the other routes as `?token=` or the `X-HD-Token` header.
async fn guard(State((hub, port)): State<(Arc<Hub>, u16)>, req: HttpRequest, next: Next) -> Response {
    let headers = req.headers();
    let host_ok = headers.get(header::HOST).and_then(|h| h.to_str().ok()).is_some_and(|h| h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"));
    if !host_ok || headers.contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if req.uri().path() != "/jsonrpc" && !token_given(headers, req.uri().query(), &hub.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(req).await
}

fn token_given(headers: &HeaderMap, query: Option<&str>, token: &str) -> bool {
    let in_header = headers.get("x-hd-token").and_then(|v| v.to_str().ok()) == Some(token);
    let in_query = query.is_some_and(|q| q.split('&').any(|kv| kv.strip_prefix("token=") == Some(token)));
    in_header || in_query
}

async fn jsonrpc(State(hub): State<Arc<Hub>>, body: bytes::Bytes) -> Response {
    match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => axum::Json(rpc::handle(&hub, v).await).into_response(),
        Err(e) => axum::Json(serde_json::json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("Parse error: {e}") } })).into_response(),
    }
}

/// The command line's download: its progress on one line until it ends.
fn follow(download: Download) {
    loop {
        let s = download.status();
        match &s.phase {
            Phase::Done(path) => {
                println!("\ndone: {}", path.display());
                return;
            }
            Phase::Failed(e) => {
                eprintln!("\nfailed: {e}");
                std::process::exit(1);
            }
            Phase::Paused | Phase::Cancelled => return,
            _ => {
                let total = s.total.map(|t| format!("{:.1} MB", t as f64 / 1e6)).unwrap_or_else(|| "?".into());
                eprint!("\r{} {:.1} MB / {total} · {:.1} MB/s · {} connections   ", s.name.as_deref().unwrap_or("…"), s.done as f64 / 1e6, s.speed as f64 / 1e6, s.connections);
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
