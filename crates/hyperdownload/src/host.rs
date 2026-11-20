//! The native messaging host: what the browser extension talks to (`hyperdownload.exe --native-host`, started by Chrome or Edge with
//! the extension's origin as its argument). Messages are JSON, each after its length (4 bytes, little-endian), on stdin and stdout.
//!
//!   {"type":"ping"}                          -> {"ok":true,"version":"0.1.0"}
//!   {"type":"add","url":…,"name":…,"referer":…,"cookie":…,"userAgent":…}
//!                                            -> {"ok":true,"gid":"…"} or {"ok":false,"error":"…"}
//!
//! Where the program keeps its data, where downloads go and which rustyfiles shows them come from `host.json` beside the exe,
//! written by rustyfiles when it installs the add-on (`{"data": …, "downloads": …, "rustyfiles": …}`). The running program is found by
//! its `rpc.json` there, and started when it is not running; once a download is added, rustyfiles is asked to show it
//! (`rustyfiles.exe --hd-follow=<gid>`, which a running rustyfiles takes as a message to its window).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Places {
    data: PathBuf,
    downloads: PathBuf,
    rustyfiles: Option<PathBuf>,
}

fn places() -> Places {
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).unwrap_or_default();
    let v: Value = std::fs::read(exe_dir.join("host.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    let path = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(PathBuf::from);
    Places {
        data: path("data").unwrap_or_else(crate::hub::default_data_dir),
        downloads: path("downloads").unwrap_or_else(crate::hub::default_downloads),
        rustyfiles: path("rustyfiles").filter(|p| p.is_file()),
    }
}

fn read_message(input: &mut impl Read) -> Option<Value> {
    let mut len = [0u8; 4];
    input.read_exact(&mut len).ok()?;
    let n = u32::from_le_bytes(len) as usize;
    // (the browser sends at most 4 GB; anything over 1 MB is not one of ours)
    if n > 1 << 20 {
        return None;
    }
    let mut buf = vec![0u8; n];
    input.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn write_message(out: &mut impl Write, v: &Value) {
    let bytes = serde_json::to_vec(v).unwrap_or_default();
    let _ = out.write_all(&(bytes.len() as u32).to_le_bytes());
    let _ = out.write_all(&bytes);
    let _ = out.flush();
}

/// A JSON-RPC call to the running program.
fn call(port: u16, token: &str, method: &str, mut params: Vec<Value>) -> Result<Value, String> {
    params.insert(0, json!(format!("token:{token}")));
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string();
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).http_status_as_error(false).build().into();
    let text = agent
        .post(&format!("http://127.0.0.1:{port}/jsonrpc"))
        .header("Content-Type", "application/json")
        .send(body)
        .map_err(|e| e.to_string())?
        .into_body()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    match v.get("error") {
        Some(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("error").to_string()),
        None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
    }
}

fn running(data: &Path) -> Option<(u16, String)> {
    let v: Value = serde_json::from_slice(&std::fs::read(data.join("rpc.json")).ok()?).ok()?;
    let port = u16::try_from(v.get("port")?.as_u64()?).ok()?;
    let token = v.get("token")?.as_str()?.to_string();
    call(port, &token, "aria2.getVersion", vec![]).ok().map(|_| (port, token))
}

/// The running program, started first when it is not running.
fn connect(p: &Places) -> Result<(u16, String), String> {
    if let Some(r) = running(&p.data) {
        return Ok(r);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let _ = std::fs::create_dir_all(&p.data);
    let _ = std::fs::remove_file(p.data.join("rpc.json"));
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve").arg("--data").arg(&p.data).arg("--downloads").arg(&p.downloads);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // its own process, apart from the browser's (it goes on when the browser closes); no console
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
    }
    cmd.spawn().map_err(|e| e.to_string())?;
    let until = Instant::now() + Duration::from_secs(15);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(150));
        if let Some(r) = running(&p.data) {
            return Ok(r);
        }
    }
    Err("HyperDownload did not start".into())
}

fn add(p: &Places, msg: &Value) -> Value {
    let Some(url) = msg.get("url").and_then(Value::as_str).filter(|u| u.starts_with("http://") || u.starts_with("https://")) else {
        return json!({ "ok": false, "error": "no http or https link" });
    };
    let (port, token) = match connect(p) {
        Ok(r) => r,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    let mut headers = Vec::new();
    if let Some(c) = msg.get("cookie").and_then(Value::as_str).filter(|c| !c.is_empty()) {
        headers.push(json!(format!("Cookie: {c}")));
    }
    if let Some(ua) = msg.get("userAgent").and_then(Value::as_str).filter(|u| !u.is_empty()) {
        headers.push(json!(format!("User-Agent: {ua}")));
    }
    let mut options = json!({ "dir": p.downloads.display().to_string(), "header": headers });
    if let Some(n) = msg.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) {
        options["out"] = json!(n);
    }
    if let Some(r) = msg.get("referer").and_then(Value::as_str).filter(|r| !r.is_empty()) {
        options["referer"] = json!(r);
    }
    match call(port, &token, "aria2.addUri", vec![json!([url]), options]) {
        Ok(gid) => {
            // rustyfiles shows it as a card of its Status Center (a running one takes it as a message)
            if let (Some(rf), Some(g)) = (&p.rustyfiles, gid.as_str()) {
                let mut cmd = std::process::Command::new(rf);
                cmd.arg(format!("--hd-follow={g}"));
                cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x0800_0000 | 0x0000_0008 | 0x0100_0000);
                }
                let _ = cmd.spawn();
            }
            json!({ "ok": true, "gid": gid })
        }
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

/// Answers the browser's messages until it closes the pipe.
pub fn run() {
    let p = places();
    let (mut input, mut out) = (std::io::stdin().lock(), std::io::stdout().lock());
    while let Some(msg) = read_message(&mut input) {
        let reply = match msg.get("type").and_then(Value::as_str) {
            Some("ping") => json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }),
            Some("add") => add(&p, &msg),
            _ => json!({ "ok": false, "error": "unknown message" }),
        };
        write_message(&mut out, &reply);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_framed_by_their_length() {
        let mut buf = Vec::new();
        write_message(&mut buf, &json!({ "type": "ping" }));
        assert_eq!(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize, buf.len() - 4);
        let back = read_message(&mut std::io::Cursor::new(buf)).unwrap();
        assert_eq!(back["type"], "ping");
        // too long a frame is refused
        let mut big = (2u32 << 20).to_le_bytes().to_vec();
        big.extend(vec![b' '; 10]);
        assert!(read_message(&mut std::io::Cursor::new(big)).is_none());
    }
}
