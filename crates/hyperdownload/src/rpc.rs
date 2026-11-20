//! The aria2-compatible JSON-RPC (https://aria2.github.io/manual/en/html/aria2c.html#rpc-interface): what rustyfiles (or any aria2
//! front end) speaks to the program. The first parameter of every call is `"token:<secret>"`. Numbers in answers are strings, as
//! aria2 sends them. HyperDownload's own calls start with `hd.`.

use std::path::PathBuf;
use std::sync::Arc;

use hd_engine::{Phase, Request};
use hd_torrent::engine::{AddSource, TorrentView};
use serde_json::{Value, json};

use crate::hub::{self, ADDING, Hub, Named, TORRENT, gid_of, gid_text};

/// A JSON-RPC error: (code, message).
type Fail = (i64, String);

fn bad(msg: impl Into<String>) -> Fail {
    (1, msg.into())
}

/// One request (or a batch: an array of them). `None` for a notification (no id).
pub async fn handle(hub: &Arc<Hub>, body: Value) -> Value {
    match body {
        Value::Array(calls) => {
            let mut out = Vec::new();
            for c in calls {
                out.push(one(hub, c).await);
            }
            Value::Array(out)
        }
        call => one(hub, call).await,
    }
}

async fn one(hub: &Arc<Hub>, call: Value) -> Value {
    let id = call.get("id").cloned().unwrap_or(Value::Null);
    let method = call.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
    let mut params: Vec<Value> = call.get("params").and_then(Value::as_array).cloned().unwrap_or_default();
    // the secret first; system.listMethods is answered without it, as aria2 does
    let authorised = match params.first().and_then(Value::as_str).and_then(|t| t.strip_prefix("token:")) {
        Some(t) if constant_time_eq(t.as_bytes(), hub.token.as_bytes()) => {
            params.remove(0);
            true
        }
        _ => false,
    };
    let result = if method == "system.listMethods" {
        Ok(json!(METHODS))
    } else if !authorised {
        Err((1, "Unauthorized".to_string()))
    } else {
        dispatch(hub, &method, &params).await
    };
    match result {
        Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

const METHODS: &[&str] = &[
    "aria2.addUri",
    "aria2.addTorrent",
    "aria2.remove",
    "aria2.forceRemove",
    "aria2.pause",
    "aria2.forcePause",
    "aria2.unpause",
    "aria2.tellStatus",
    "aria2.getFiles",
    "aria2.tellActive",
    "aria2.tellWaiting",
    "aria2.tellStopped",
    "aria2.changeOption",
    "aria2.getGlobalStat",
    "aria2.removeDownloadResult",
    "aria2.getVersion",
    "aria2.shutdown",
    "aria2.forceShutdown",
    "system.listMethods",
    "hd.choices",
    "hd.choose",
    "hd.streamUrl",
];

fn param_gid(params: &[Value], i: usize) -> Result<u64, Fail> {
    params.get(i).and_then(Value::as_str).and_then(gid_of).ok_or_else(|| bad("a GID is needed"))
}

fn opt_str(options: &Value, key: &str) -> Option<String> {
    options.get(key).and_then(Value::as_str).map(str::to_string)
}

async fn dispatch(hub: &Arc<Hub>, method: &str, params: &[Value]) -> Result<Value, Fail> {
    let empty = json!({});
    match method {
        "aria2.getVersion" => Ok(json!({ "version": env!("CARGO_PKG_VERSION"), "enabledFeatures": ["BitTorrent", "HTTPS", "HyperDownload"] })),
        "aria2.addUri" => {
            let uris = params.first().and_then(Value::as_array).ok_or_else(|| bad("addUri needs a list of URIs"))?;
            let uri = uris.first().and_then(Value::as_str).ok_or_else(|| bad("no URI"))?.trim().to_string();
            let options = params.get(1).unwrap_or(&empty);
            let dir = opt_str(options, "dir").map(PathBuf::from).unwrap_or_else(|| hub.downloads.clone());
            if uri.to_ascii_lowercase().starts_with("magnet:") {
                let key = hub.torrent.add(AddSource::Url(uri));
                return Ok(json!(gid_text(ADDING | key)));
            }
            if !(uri.starts_with("http://") || uri.starts_with("https://")) {
                return Err(bad("only http, https and magnet links"));
            }
            // aria2's "header" option: ["Cookie: …", "User-Agent: …"]
            let mut cookie = None;
            let mut user_agent = opt_str(options, "user-agent");
            if let Some(headers) = options.get("header").and_then(Value::as_array) {
                for h in headers.iter().filter_map(Value::as_str) {
                    if let Some((k, v)) = h.split_once(':') {
                        match k.trim().to_ascii_lowercase().as_str() {
                            "cookie" => cookie = Some(v.trim().to_string()),
                            "user-agent" => user_agent = Some(v.trim().to_string()),
                            _ => {}
                        }
                    }
                }
            }
            let connections = opt_str(options, "split").or_else(|| opt_str(options, "max-connection-per-server")).and_then(|s| s.parse().ok()).unwrap_or(0);
            let _ = hub::ensure_dir(&dir);
            // aria2's "checksum=sha-256=<hex>"
            let sha256 = opt_str(options, "checksum").and_then(|c| c.split_once('=').filter(|(k, _)| k.eq_ignore_ascii_case("sha-256")).map(|(_, v)| v.to_string()));
            let gid = hub.add_http(Request { url: uri, dir, name: opt_str(options, "out"), referer: opt_str(options, "referer"), cookie, user_agent, connections, sha256 });
            Ok(json!(gid_text(gid)))
        }
        "aria2.addTorrent" => {
            use base64::Engine as _;
            let b64 = params.first().and_then(Value::as_str).ok_or_else(|| bad("addTorrent needs the torrent file in base64"))?;
            let data = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| bad(format!("not base64: {e}")))?;
            let key = hub.torrent.add(AddSource::Bytes { name: "torrent".into(), data: data.into() });
            Ok(json!(gid_text(ADDING | key)))
        }
        "aria2.tellStatus" => status_of(hub, param_gid(params, 0)?).ok_or_else(|| bad("no such download")),
        "aria2.getFiles" => status_of(hub, param_gid(params, 0)?).and_then(|s| s.get("files").cloned()).ok_or_else(|| bad("no such download")),
        "aria2.tellActive" => Ok(Value::Array(all(hub).into_iter().filter(|s| s["status"] == "active").collect())),
        "aria2.tellWaiting" => Ok(window(all(hub).into_iter().filter(|s| s["status"] == "paused" || s["status"] == "waiting").collect(), params)),
        "aria2.tellStopped" => Ok(window(all(hub).into_iter().filter(|s| matches!(s["status"].as_str(), Some("complete" | "error" | "removed"))).collect(), params)),
        "aria2.pause" | "aria2.forcePause" => {
            let gid = param_gid(params, 0)?;
            match hub::named(gid) {
                Named::Http(g) => hub.http(g).ok_or_else(|| bad("no such download"))?.pause(),
                Named::Torrent(id) => hub.torrent.pause_torrent(id).await.map_err(|e| bad(e.to_string()))?,
                Named::Adding(key) => match hub.torrent.resolved(key) {
                    Some(id) => hub.torrent.pause_torrent(id).await.map_err(|e| bad(e.to_string()))?,
                    None => return Err(bad("the torrent is still being added")),
                },
            }
            hub.save();
            Ok(json!(gid_text(gid)))
        }
        "aria2.unpause" => {
            let gid = param_gid(params, 0)?;
            match hub::named(gid) {
                Named::Http(g) => hub.http(g).ok_or_else(|| bad("no such download"))?.resume(),
                Named::Torrent(id) => hub.torrent.start_torrent(id).await.map_err(|e| bad(e.to_string()))?,
                Named::Adding(key) => match hub.torrent.resolved(key) {
                    Some(id) => hub.torrent.start_torrent(id).await.map_err(|e| bad(e.to_string()))?,
                    None => return Err(bad("the torrent is still being added")),
                },
            }
            Ok(json!(gid_text(gid)))
        }
        "aria2.remove" | "aria2.forceRemove" => {
            let gid = param_gid(params, 0)?;
            match hub::named(gid) {
                // (an HTTP download's unfinished file goes; a finished file stays, as in aria2)
                Named::Http(g) => hub.http(g).ok_or_else(|| bad("no such download"))?.cancel(),
                Named::Torrent(id) => {
                    hub.torrent.remove(id, false).await.map_err(|e| bad(e.to_string()))?;
                }
                Named::Adding(key) => match hub.torrent.resolved(key) {
                    Some(id) => {
                        hub.torrent.remove(id, false).await.map_err(|e| bad(e.to_string()))?;
                    }
                    None => hub.torrent.dismiss_pending(key),
                },
            }
            hub.save();
            Ok(json!(gid_text(gid)))
        }
        "aria2.removeDownloadResult" => {
            let gid = param_gid(params, 0)?;
            if hub.forget(gid) { Ok(json!("OK")) } else { Err(bad("the download is still going")) }
        }
        "aria2.changeOption" => {
            let gid = param_gid(params, 0)?;
            let options = params.get(1).unwrap_or(&empty);
            if let (Named::Torrent(id), Some(sel)) = (hub::named(gid), opt_str(options, "select-file")) {
                // aria2 counts files from 1
                let files: Vec<usize> = sel.split(',').filter_map(|n| n.trim().parse::<usize>().ok()).filter(|n| *n > 0).map(|n| n - 1).collect();
                hub.torrent.select_files(id, &files, true).await.map_err(|e| bad(e.to_string()))?;
            }
            Ok(json!("OK"))
        }
        "aria2.getGlobalStat" => {
            let list = all(hub);
            let sum = |k: &str| list.iter().filter_map(|s| s[k].as_str()?.parse::<u64>().ok()).sum::<u64>().to_string();
            let count = |f: &dyn Fn(&str) -> bool| list.iter().filter(|s| f(s["status"].as_str().unwrap_or_default())).count().to_string();
            Ok(json!({
                "downloadSpeed": sum("downloadSpeed"),
                "uploadSpeed": sum("uploadSpeed"),
                "numActive": count(&|s| s == "active"),
                "numWaiting": count(&|s| s == "paused" || s == "waiting"),
                "numStopped": count(&|s| matches!(s, "complete" | "error" | "removed")),
                "numStoppedTotal": count(&|s| matches!(s, "complete" | "error" | "removed")),
            }))
        }
        "aria2.shutdown" | "aria2.forceShutdown" => {
            let hub = hub.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                crate::shutdown(&hub).await;
            });
            Ok(json!("OK"))
        }
        // ---- HyperDownload's own
        // the torrents whose file list waits for a choice: [{gid, name, files: [{index, path, length}], suggested: [index]}]
        "hd.choices" => Ok(Value::Array(
            hub.torrent
                .pending()
                .into_iter()
                .filter_map(|p| {
                    let c = p.choice?;
                    Some(json!({
                        "gid": gid_text(ADDING | p.key),
                        "name": p.name,
                        "files": c.files.iter().map(|f| json!({ "index": (f.idx + 1).to_string(), "path": f.name, "length": f.len.to_string() })).collect::<Vec<_>>(),
                        "suggested": c.suggested.iter().map(|i| (i + 1).to_string()).collect::<Vec<_>>(),
                    }))
                })
                .collect(),
        )),
        "hd.choose" => {
            let gid = param_gid(params, 0)?;
            let Named::Adding(key) = hub::named(gid) else { return Err(bad("not a torrent waiting for a choice")) };
            let files: Vec<usize> =
                params.get(1).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().and_then(|s| s.parse::<usize>().ok()).or(v.as_u64().map(|n| n as usize))).filter(|n| *n > 0).map(|n| n - 1).collect()).unwrap_or_default();
            hub.torrent.choose(key, files).map_err(|e| bad(e.to_string()))?;
            Ok(json!("OK"))
        }
        // where a player streams a file of a torrent while it downloads (the program's own address, with the secret)
        "hd.streamUrl" => {
            let gid = param_gid(params, 0)?;
            let file = params.get(1).and_then(|v| v.as_str().and_then(|s| s.parse::<usize>().ok()).or(v.as_u64().map(|n| n as usize))).filter(|n| *n > 0).ok_or_else(|| bad("a file index (from 1) is needed"))?;
            let Named::Torrent(id) = hub::named(gid) else { return Err(bad("only a torrent's files stream")) };
            Ok(json!(format!("{}?token={}", hub.torrent.stream_url(id, file - 1), hub.token)))
        }
        _ => Err((1, format!("No such method: {method}"))),
    }
}

/// aria2's `offset, num` paging of tellWaiting and tellStopped.
fn window(list: Vec<Value>, params: &[Value]) -> Value {
    let offset = params.first().and_then(Value::as_i64).unwrap_or(0).max(0) as usize;
    let num = params.get(1).and_then(Value::as_u64).unwrap_or(1000) as usize;
    Value::Array(list.into_iter().skip(offset).take(num).collect())
}

/// Every download, as tellStatus answers.
fn all(hub: &Hub) -> Vec<Value> {
    let mut out: Vec<Value> = hub.http_jobs().into_iter().map(|(gid, d, dir)| http_status(gid, &d.status(), &d.url(), &dir, d.target())).collect();
    for e in hub.ended() {
        let s = hd_engine::Status { phase: e.phase.clone(), name: e.target.as_ref().and_then(|t| t.file_name()).map(|n| n.to_string_lossy().into_owned()), total: None, done: 0, speed: 0, connections: 0, resumable: false };
        out.push(http_status(e.gid, &s, &e.url, &e.dir, e.target.clone()));
    }
    for t in hub.torrent.list() {
        out.push(torrent_status(&t, None));
    }
    for p in hub.torrent.pending() {
        out.push(adding_status(&p));
    }
    out
}

fn status_of(hub: &Hub, gid: u64) -> Option<Value> {
    match hub::named(gid) {
        Named::Http(g) => match hub.http(g) {
            Some(d) => Some(http_status(g, &d.status(), &d.url(), &hub.http_dir(g).unwrap_or_default(), d.target())),
            None => all(hub).into_iter().find(|s| s["gid"] == gid_text(g)),
        },
        Named::Torrent(id) => hub.torrent.view(id, true).map(|t| torrent_status(&t, Some(()))),
        // (an addition that has become a torrent answers as that torrent: the caller follows one GID from the link to the end)
        Named::Adding(key) => match hub.torrent.resolved(key) {
            Some(id) => hub.torrent.view(id, true).map(|t| torrent_status(&t, Some(()))),
            None => hub.torrent.pending().into_iter().find(|p| p.key == key).map(|p| adding_status(&p)),
        },
    }
}

fn http_status(gid: u64, s: &hd_engine::Status, url: &str, dir: &std::path::Path, target: Option<PathBuf>) -> Value {
    let status = match &s.phase {
        Phase::Starting | Phase::Running => "active",
        Phase::Paused => "paused",
        Phase::Done(_) => "complete",
        Phase::Failed(_) => "error",
        Phase::Cancelled => "removed",
    };
    let error = if let Phase::Failed(e) = &s.phase { Some(e.clone()) } else { None };
    let total = s.total.unwrap_or(if matches!(s.phase, Phase::Done(_)) { s.done } else { 0 });
    let path = target.map(|t| t.display().to_string()).unwrap_or_default();
    json!({
        "gid": gid_text(gid),
        "status": status,
        "totalLength": total.to_string(),
        "completedLength": s.done.to_string(),
        "uploadLength": "0",
        "downloadSpeed": s.speed.to_string(),
        "uploadSpeed": "0",
        "connections": s.connections.to_string(),
        "dir": dir.display().to_string(),
        "errorCode": if error.is_some() { "1" } else { "0" },
        "errorMessage": error.unwrap_or_default(),
        "files": [{ "index": "1", "path": path, "length": total.to_string(), "completedLength": s.done.to_string(), "selected": "true", "uris": [{ "uri": url, "status": "used" }] }],
        "hdKind": "http",
        "hdResumable": s.resumable,
    })
}

fn torrent_status(t: &TorrentView, _with_files: Option<()>) -> Value {
    let status = match (t.state, t.finished) {
        ("error", _) => "error",
        ("paused", true) => "complete",
        ("paused", false) => "paused",
        (_, true) => "complete",
        _ => "active",
    };
    let files: Vec<Value> = t
        .files
        .as_ref()
        .map(|fs| fs.iter().map(|f| json!({ "index": (f.idx + 1).to_string(), "path": f.path, "length": f.len.to_string(), "completedLength": f.progress.to_string(), "selected": f.included.to_string() })).collect())
        .unwrap_or_default();
    json!({
        "gid": gid_text(TORRENT | t.id as u64),
        "status": status,
        "totalLength": t.total_bytes.to_string(),
        "completedLength": t.progress_bytes.to_string(),
        "downloadSpeed": ((t.down * 1024.0 * 1024.0) as u64).to_string(),
        "uploadSpeed": ((t.up * 1024.0 * 1024.0) as u64).to_string(),
        "connections": t.peers.to_string(),
        "numSeeders": "0",
        "infoHash": t.info_hash,
        "dir": t.output_folder,
        "errorCode": if t.error.is_some() { "1" } else { "0" },
        "errorMessage": t.error.clone().unwrap_or_default(),
        "bittorrent": { "info": { "name": t.name } },
        "files": files,
        "hdKind": "torrent",
        "hdTitle": t.display_title,
        "hdMainFile": t.main_file.map(|i| (i + 1).to_string()),
        "hdEta": t.eta,
    })
}

fn adding_status(p: &hd_torrent::engine::Pending) -> Value {
    json!({
        "gid": gid_text(ADDING | p.key),
        "status": if p.error.is_some() { "error" } else { "waiting" },
        "totalLength": "0",
        "completedLength": "0",
        "downloadSpeed": "0",
        "uploadSpeed": "0",
        "connections": "0",
        "errorCode": if p.error.is_some() { "1" } else { "0" },
        "errorMessage": p.error.clone().unwrap_or_default(),
        "bittorrent": { "info": { "name": p.name } },
        "files": [],
        "hdKind": "torrent",
        // the file list is waiting for the user's choice (hd.choices / hd.choose)
        "hdChoice": p.choice.is_some(),
    })
}
