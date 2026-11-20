//! The name a download is saved under: the server's (`Content-Disposition`), else the URL's last part, made safe for Windows, and
//! free in the folder.

use std::path::{Path, PathBuf};

/// The file name a `Content-Disposition` header gives: `filename*=UTF-8''…` (percent-encoded) wins over `filename="…"`.
pub fn from_disposition(header: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for part in split_params(header) {
        let Some((key, value)) = part.split_once('=') else { continue };
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => {
                let v = value.trim().trim_matches('"');
                // charset'language'value
                let mut it = v.splitn(3, '\'');
                if let (Some(charset), Some(_lang), Some(rest)) = (it.next(), it.next(), it.next()) {
                    let bytes = percent_decode(rest);
                    extended = if charset.eq_ignore_ascii_case("utf-8") {
                        String::from_utf8(bytes).ok()
                    } else {
                        // ISO-8859-1: each byte is its own character
                        Some(bytes.into_iter().map(char::from).collect())
                    };
                }
            }
            "filename" => plain = Some(unquote(value.trim())),
            _ => {}
        }
    }
    extended.or(plain).filter(|n| !n.trim().is_empty())
}

/// The parameters of a header, split at the semicolons that are not inside quotes.
fn split_params(header: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in header.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => {
                cur.push(c);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            ';' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn unquote(v: &str) -> String {
    match v.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(inner) => {
            let mut out = String::new();
            let mut esc = false;
            for c in inner.chars() {
                if esc {
                    out.push(c);
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else {
                    out.push(c);
                }
            }
            out
        }
        None => v.to_string(),
    }
}

/// `%41` → `A`; a `%` without two hex digits stays as it is.
pub fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// The last part of a URL's path, decoded (`https://x/a/b%20c.zip?d=1` → `b c.zip`).
pub fn from_url(url: &str) -> Option<String> {
    let no_frag = url.split('#').next().unwrap_or(url);
    let no_query = no_frag.split('?').next().unwrap_or(no_frag);
    let path = no_query.split_once("://").map_or(no_query, |(_, rest)| rest.split_once('/').map_or("", |(_, p)| p));
    let last = path.rsplit('/').find(|s| !s.is_empty())?;
    String::from_utf8(percent_decode(last)).ok().filter(|n| !n.trim().is_empty())
}

/// A name Windows takes: no `<>:"/\|?*` or control characters, no trailing dots or spaces, not a device name (`CON`, `NUL`, `COM1`…),
/// at most 200 characters (the folder's path still needs room). `download` when nothing is left.
pub fn safe(name: &str) -> String {
    let mut s: String = name.chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect();
    let trimmed = s.trim().trim_end_matches(['.', ' ']).to_string();
    s = trimmed;
    if s.chars().count() > 200 {
        // keep the extension
        let (stem, ext) = split_ext(&s);
        let keep = 200usize.saturating_sub(ext.chars().count());
        s = stem.chars().take(keep).collect::<String>() + ext;
    }
    let stem = split_ext(&s).0.to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    if device {
        s.insert(0, '_');
    }
    if s.is_empty() { "download".to_string() } else { s }
}

/// `name.tar.gz` → (`name.tar`, `.gz`); a name without a dot, or a dot first: no extension.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// `dir\name` when it is free, else `dir\name (2).ext`, `(3)`… (neither the file nor its unfinished part may be there).
pub fn free_path(dir: &Path, name: &str, taken: impl Fn(&Path) -> bool) -> PathBuf {
    let first = dir.join(name);
    if !taken(&first) {
        return first;
    }
    let (stem, ext) = split_ext(name);
    (2..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !taken(p)).unwrap_or(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_servers_name_wins_and_its_utf8_form_over_the_plain_one() {
        assert_eq!(from_disposition(r#"attachment; filename="report 2026.pdf""#).as_deref(), Some("report 2026.pdf"));
        assert_eq!(from_disposition("attachment; filename=plain.zip").as_deref(), Some("plain.zip"));
        assert_eq!(from_disposition(r#"attachment; filename="fallback.txt"; filename*=UTF-8''%C3%B6zet%20d%C3%BC.txt"#).as_deref(), Some("özet dü.txt"));
        assert_eq!(from_disposition(r#"attachment; filename="a;b.txt""#).as_deref(), Some("a;b.txt"), "a semicolon inside quotes");
        assert_eq!(from_disposition(r#"attachment; filename="say \"hi\".txt""#).as_deref(), Some(r#"say "hi".txt"#));
        assert_eq!(from_disposition("inline"), None);
    }

    #[test]
    fn the_urls_last_part_decoded() {
        assert_eq!(from_url("https://example.com/files/b%20c.zip?token=1#x").as_deref(), Some("b c.zip"));
        assert_eq!(from_url("https://example.com/dir/").as_deref(), Some("dir"));
        assert_eq!(from_url("https://example.com/"), None);
        assert_eq!(from_url("https://example.com"), None);
    }

    #[test]
    fn a_name_windows_takes() {
        assert_eq!(safe(r#"a<b>c:d"e/f\g|h?i*.txt"#), "a_b_c_d_e_f_g_h_i_.txt");
        assert_eq!(safe("name. . "), "name");
        assert_eq!(safe("CON.txt"), "_CON.txt");
        assert_eq!(safe("com1"), "_com1");
        assert_eq!(safe("COMPUTER.txt"), "COMPUTER.txt");
        assert_eq!(safe("   "), "download");
        let long = "x".repeat(300) + ".iso";
        let s = safe(&long);
        assert_eq!(s.chars().count(), 200);
        assert!(s.ends_with(".iso"));
    }

    #[test]
    fn a_taken_name_gets_a_number_before_its_extension() {
        let dir = Path::new(r"C:\d");
        let taken = [PathBuf::from(r"C:\d\a.zip"), PathBuf::from(r"C:\d\a (2).zip")];
        assert_eq!(free_path(dir, "a.zip", |p| taken.iter().any(|t| t == p)), PathBuf::from(r"C:\d\a (3).zip"));
        assert_eq!(free_path(dir, "b.zip", |p| taken.iter().any(|t| t == p)), PathBuf::from(r"C:\d\b.zip"));
        assert_eq!(free_path(dir, "noext", |_p| _p == Path::new(r"C:\d\noext")), PathBuf::from(r"C:\d\noext (2)"));
    }
}
