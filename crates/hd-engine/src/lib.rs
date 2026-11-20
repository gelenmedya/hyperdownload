//! HyperDownload's engine: a file fetched over HTTP or HTTPS through several connections at once (byte ranges), paused and continued,
//! and continued after a restart from what is kept beside it.
//!
//! A download is a [`Download`]: started with a [`Request`], it runs on threads of its own; [`Download::status`] says where it is, and
//! [`Download::pause`], [`Download::resume`] and [`Download::cancel`] steer it. While it runs the file is `<name>.hdpart` with
//! `<name>.hdpart.json` beside it ([`pieces::Saved`]); finished, it gets its name.

pub mod check;
pub mod names;
pub mod pieces;
mod job;
mod net;

pub use job::{Download, Phase, Segment, Status, part_of, state_of};

use std::path::PathBuf;

/// What to fetch and where to.
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub url: String,
    /// The folder the file goes into.
    pub dir: PathBuf,
    /// A name chosen by the caller; else the server's or the URL's.
    pub name: Option<String>,
    /// Sent as they are (a browser hands over the page and the site's cookies, without which some downloads are refused).
    pub referer: Option<String>,
    pub cookie: Option<String>,
    pub user_agent: Option<String>,
    /// Connections at once (at most); 0 is the default ([`DEFAULT_CONNECTIONS`]).
    pub connections: usize,
    /// Where the browser's own download had got to (after its redirects): tried once when the server refuses `url`.
    pub fallback_url: Option<String>,
    /// The file's SHA-256 when the caller knows it (aria2's `checksum=sha-256=…`): a file that differs fails.
    pub sha256: Option<String>,
}

pub const DEFAULT_CONNECTIONS: usize = 8;

/// The extension of the unfinished file.
pub const PART_EXT: &str = "hdpart";
