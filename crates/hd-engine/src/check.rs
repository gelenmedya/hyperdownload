//! What is done to a finished file: its SHA-256 checked when the caller knows it, and the "Mark of the Web" browsers put on their
//! downloads (the `Zone.Identifier` stream: zone 3, the internet, with the address it came from), which Defender, SmartScreen and
//! Office look at.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

pub fn sha256_of(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The text of the `Zone.Identifier` stream for a file from `url` (reached from `referer`).
pub fn zone_text(url: &str, referer: Option<&str>) -> String {
    let mut s = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
    // (a link with a user name or a password in it does not go into the file)
    let clean = |u: &str| (!u.contains('@') || !u.split("//").nth(1).is_some_and(|rest| rest.split('/').next().is_some_and(|h| h.contains('@')))).then(|| u.to_string());
    if let Some(r) = referer.and_then(clean) {
        s.push_str(&format!("ReferrerUrl={r}\r\n"));
    }
    if let Some(h) = clean(url) {
        s.push_str(&format!("HostUrl={h}\r\n"));
    }
    s
}

/// Writes the Mark of the Web (NTFS only; elsewhere there is nothing to write to, and nothing is said).
pub fn mark_of_the_web(path: &Path, url: &str, referer: Option<&str>) {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        let _ = std::fs::write(stream, zone_text(url, referer));
    }
    #[cfg(not(windows))]
    {
        let _ = (path, url, referer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_names_the_internet_zone_and_where_the_file_came_from() {
        let t = zone_text("https://example.com/a.zip", Some("https://example.com/page"));
        assert_eq!(t, "[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=https://example.com/page\r\nHostUrl=https://example.com/a.zip\r\n");
        // a link that carries a user name and password is not written down
        let t = zone_text("https://user:pass@example.com/a.zip", None);
        assert_eq!(t, "[ZoneTransfer]\r\nZoneId=3\r\n");
    }

    #[test]
    fn a_files_sha256() {
        let p = std::env::temp_dir().join(format!("hd-sha-{}", std::process::id()));
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(sha256_of(&p).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let _ = std::fs::remove_file(&p);
    }
}
