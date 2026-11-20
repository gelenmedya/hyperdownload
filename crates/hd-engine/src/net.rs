//! The requests: a first look at the file (its size, whether the server answers byte ranges, its name) and the fetch of one range.

use std::io::Read;
use std::time::Duration;

use ureq::ResponseExt as _;

use crate::Request;

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        // a stalled connection is given up and its piece asked for again
        .timeout_recv_body(Some(Duration::from_secs(30)))
        .max_redirects(10)
        .tls_config(ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build())
        .build()
        .into()
}

/// What the first answer told.
// (no Debug: the body has none)
pub(crate) struct Probe {
    /// The address after the redirects.
    pub url: String,
    pub total: Option<u64>,
    pub ranges: bool,
    pub name: Option<String>,
    pub validator: Option<String>,
    /// The body of that first answer when the server does not do ranges: it is the download itself (no second request).
    pub body: Option<ureq::BodyReader<'static>>,
}

/// An answer that is not a success.
#[derive(Debug)]
pub(crate) enum Fail {
    /// The server said no (4xx): asking again will not help.
    Refused(u16),
    /// Anything that may pass (a 5xx, a lost connection, a timeout).
    Again(String),
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::Refused(code) => write!(f, "the server refused the download (HTTP {code})"),
            Fail::Again(e) => f.write_str(e),
        }
    }
}

fn get(agent: &ureq::Agent, url: &str, req: &Request, range: Option<(u64, Option<u64>)>, validator: Option<&str>) -> Result<ureq::http::Response<ureq::Body>, Fail> {
    let mut r = agent.get(url);
    if let Some((from, to)) = range {
        let to = to.map(|t| t.to_string()).unwrap_or_default();
        r = r.header("Range", format!("bytes={from}-{to}"));
    }
    if let Some(v) = validator {
        // the range only if the file is still the one that was started; else the whole file (200), which the caller sees
        r = r.header("If-Range", v);
    }
    if let Some(v) = &req.referer {
        r = r.header("Referer", v);
    }
    if let Some(v) = &req.cookie {
        r = r.header("Cookie", v);
    }
    if let Some(v) = &req.user_agent {
        r = r.header("User-Agent", v);
    }
    r.call().map_err(|e| Fail::Again(e.to_string()))
}

/// A header's value. Bytes beyond ASCII are read as UTF-8 when they are that, else as Latin-1: servers do send a name like
/// `filename="büyük.zip"` raw, which `to_str` refuses.
fn header(resp: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    let bytes = resp.headers().get(name)?.as_bytes();
    Some(match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| char::from(b)).collect(),
    })
}

/// `bytes 0-0/12345` → 12345 (`*` for an unknown size → None).
pub(crate) fn total_of_content_range(v: &str) -> Option<u64> {
    v.rsplit('/').next()?.trim().parse().ok()
}

pub(crate) fn probe(agent: &ureq::Agent, req: &Request) -> Result<Probe, Fail> {
    let resp = get(agent, &req.url, req, Some((0, None)), None)?;
    let status = resp.status().as_u16();
    if (400..500).contains(&status) {
        return Err(Fail::Refused(status));
    }
    if !(200..300).contains(&status) {
        return Err(Fail::Again(format!("HTTP {status}")));
    }
    let url = resp.get_uri().to_string();
    let name = header(&resp, "content-disposition").and_then(|d| crate::names::from_disposition(&d)).or_else(|| crate::names::from_url(&url));
    let validator = header(&resp, "etag").filter(|e| !e.starts_with("W/")).or_else(|| header(&resp, "last-modified"));
    if status == 206 {
        let total = header(&resp, "content-range").and_then(|v| total_of_content_range(&v));
        return Ok(Probe { url, total, ranges: total.is_some(), name, validator, body: None });
    }
    let total = header(&resp, "content-length").and_then(|v| v.trim().parse().ok());
    Ok(Probe { url, total, ranges: false, name, validator, body: Some(resp.into_body().into_reader()) })
}

/// The body for `[from, to)` of the file. `Err` when the server does not answer that range as asked (it changed, or no ranges now).
pub(crate) fn fetch(agent: &ureq::Agent, url: &str, req: &Request, from: u64, to: u64, validator: Option<&str>) -> Result<impl Read + use<>, Fail> {
    let resp = get(agent, url, req, Some((from, Some(to.saturating_sub(1)))), validator)?;
    let status = resp.status().as_u16();
    if (400..500).contains(&status) && status != 416 {
        return Err(Fail::Refused(status));
    }
    if status != 206 {
        return Err(Fail::Again(format!("the server did not answer the range (HTTP {status})")));
    }
    let starts = header(&resp, "content-range").and_then(|v| v.strip_prefix("bytes ")?.split('-').next()?.trim().parse::<u64>().ok());
    if starts != Some(from) {
        return Err(Fail::Again("the server answered another range".into()));
    }
    Ok(resp.into_body().into_reader())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_size_from_a_content_range() {
        assert_eq!(super::total_of_content_range("bytes 0-0/12345"), Some(12345));
        assert_eq!(super::total_of_content_range("bytes 0-99/*"), None);
    }
}
