//! Medya yardımcıları: dosya türleri, otomatik seçim, altyazı dönüştürme (SRT/ASS -> WebVTT, Türkçe kod sayfası). Harici oynatıcılar masaüstü crate'indedir.

use std::path::Path;

use serde::Serialize;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Video,
    Audio,
    Subtitle,
    Other,
}

const VIDEO: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "mov", "webm", "ts", "m2ts", "mts", "wmv", "flv", "mpg", "mpeg",
    "ogv", "3gp", "vob",
];
const AUDIO: &[&str] = &["mp3", "flac", "m4a", "aac", "ogg", "opus", "wav", "alac", "wma"];
const SUBTITLE: &[&str] = &["srt", "vtt", "ass", "ssa"];

fn ext(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn kind_of(name: &str) -> FileKind {
    let e = ext(name);
    if VIDEO.contains(&e.as_str()) {
        FileKind::Video
    } else if AUDIO.contains(&e.as_str()) {
        FileKind::Audio
    } else if SUBTITLE.contains(&e.as_str()) {
        FileKind::Subtitle
    } else {
        FileKind::Other
    }
}

/// Chromium'un (WebView2) doğrudan açabildiği kapsayıcılar.
const BROWSER_FRIENDLY: &[&str] = &["mkv", "mp4", "m4v", "webm", "mov"];

/// Ana video: aynı içeriğin birden çok sürümü varsa tarayıcıda oynayan kapsayıcı öne alınır.
/// "sample" adlı ve en büyüğün %15'inden küçük dosyalar (tanıtım/örnek) aday olmaz.
pub fn main_video<'a>(files: impl Iterator<Item = (usize, &'a str, u64)> + Clone) -> Option<usize> {
    let videos = files.filter(|(_, n, _)| kind_of(n) == FileKind::Video);
    let largest = videos.clone().map(|(_, _, l)| l).max()?;
    videos
        .filter(|(_, n, l)| *l * 100 >= largest * 15 && !n.to_ascii_lowercase().contains("sample"))
        .max_by_key(|(_, n, l)| (BROWSER_FRIENDLY.contains(&ext(n).as_str()), *l))
        .map(|(i, _, _)| i)
}

/// Gerçek bir seçim gerekir mi? Yalnızca ≥2 aday video varsa (örnek/tanıtım dosyaları aday değildir).
/// Tek video + altyazı/nfo gibi eklentiler için `pick_files` önerisi sorulmadan uygulanır.
pub fn needs_choice(files: &[(String, u64)]) -> bool {
    let videos: Vec<(&str, u64)> = files
        .iter()
        .filter(|(n, _)| kind_of(n) == FileKind::Video)
        .map(|(n, l)| (n.as_str(), *l))
        .collect();
    let Some(largest) = videos.iter().map(|v| v.1).max() else { return false };
    videos
        .iter()
        .filter(|(n, l)| *l * 100 >= largest * 15 && !n.to_ascii_lowercase().contains("sample"))
        .count()
        >= 2
}

/// Yeni torrent'te indirilecekler: ana video + tüm altyazılar. Video yoksa hepsi (None).
pub fn pick_files(files: &[(String, u64)]) -> Option<Vec<usize>> {
    let main = main_video(files.iter().enumerate().map(|(i, (n, l))| (i, n.as_str(), *l)))?;
    let mut out = vec![main];
    out.extend(
        files
            .iter()
            .enumerate()
            .filter(|(i, (n, _))| *i != main && kind_of(n) == FileKind::Subtitle)
            .map(|(i, _)| i),
    );
    Some(out)
}

/// Tarayıcı oynatıcısı için içerik türü. Matroska'yı Chromium WebM ayrıştırıcısıyla açar.
pub fn mime_for(name: &str) -> &'static str {
    match ext(name).as_str() {
        "mp4" | "m4v" | "mov" => "video/mp4",
        "mkv" | "webm" => "video/webm",
        "ts" | "m2ts" | "mts" => "video/mp2t",
        "avi" => "video/x-msvideo",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" | "aac" => "audio/mp4",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "vtt" => "text/vtt",
        "srt" => "application/x-subrip",
        _ => "application/octet-stream",
    }
}

// ---------- altyazı ----------

/// UTF-8 değilse Türkçe Windows kod sayfası (1254) varsayılır: Türk altyazılarının çoğu böyledir.
pub fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return encoding_rs::UTF_16LE.decode(bytes).0.into_owned();
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return encoding_rs::UTF_16BE.decode(bytes).0.into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => encoding_rs::WINDOWS_1254.decode(bytes).0.into_owned(),
    }
}

pub fn to_vtt(name: &str, bytes: &[u8]) -> String {
    let text = decode_text(bytes).replace("\r\n", "\n").replace('\r', "\n");
    match ext(name).as_str() {
        "vtt" => text,
        "ass" | "ssa" => ass_to_vtt(&text),
        _ => srt_to_vtt(&text),
    }
}

fn srt_to_vtt(text: &str) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for line in text.lines() {
        if line.contains("-->") {
            out.push_str(&line.replace(',', "."));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn ass_time(t: &str) -> Option<String> {
    // H:MM:SS.cc -> HH:MM:SS.mmm
    let (hms, cs) = t.trim().split_once('.')?;
    let mut parts = hms.split(':');
    let h: u32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let s: u32 = parts.next()?.parse().ok()?;
    let cs: u32 = cs.parse().ok()?;
    Some(format!("{h:02}:{m:02}:{s:02}.{:03}", cs * 10))
}

/// ASS metninden `{...}` geçersiz kılmalarını ve satır sonu kaçışlarını temizler.
pub(crate) fn clean_ass_text(raw: &str) -> String {
    let mut clean = String::new();
    let mut depth = 0;
    for c in raw.chars() {
        match c {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            _ if depth == 0 => clean.push(c),
            _ => {}
        }
    }
    clean.replace("\\N", "\n").replace("\\n", "\n").replace("\\h", " ")
}

fn ass_to_vtt(text: &str) -> String {
    let mut out = String::from("WEBVTT\n\n");
    let mut fields: Vec<String> = vec![];
    let mut in_events = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_events = l.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(f) = l.strip_prefix("Format:") {
            fields = f.split(',').map(|s| s.trim().to_ascii_lowercase()).collect();
        } else if let Some(d) = l.strip_prefix("Dialogue:") {
            let n = fields.len().max(10);
            let parts: Vec<&str> = d.splitn(n, ',').collect();
            let idx = |name: &str, def: usize| fields.iter().position(|f| f == name).unwrap_or(def);
            let (Some(start), Some(end), Some(raw)) = (
                parts.get(idx("start", 1)).and_then(|t| ass_time(t)),
                parts.get(idx("end", 2)).and_then(|t| ass_time(t)),
                parts.get(idx("text", n - 1)),
            ) else {
                continue;
            };
            let clean = clean_ass_text(raw);
            if clean.trim().is_empty() {
                continue;
            }
            out.push_str(&format!("{start} --> {end}\n{}\n\n", clean.trim()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_choice_only_for_multiple_videos() {
        let one = vec![
            ("A/film.mkv".to_string(), 1000),
            ("A/film.srt".to_string(), 2),
            ("A/info.nfo".to_string(), 1),
            ("A/sample.mkv".to_string(), 10),
        ];
        assert!(!needs_choice(&one));
        let two = vec![("A/e1.mkv".to_string(), 1000), ("A/e2.mkv".to_string(), 900), ("A/e1.srt".to_string(), 2)];
        assert!(needs_choice(&two));
        assert!(!needs_choice(&[("a.mp3".to_string(), 5), ("b.mp3".to_string(), 5)]));
        // İkinci video %15'in altındaysa (ör. küçük bonus) aday sayılmaz.
        let small = vec![("A/film.mkv".to_string(), 1000), ("A/bonus.mkv".to_string(), 100)];
        assert!(!needs_choice(&small));
    }

    #[test]
    fn picks_largest_video_and_subs() {
        let files = vec![
            ("Show/sample.mkv".to_string(), 10),
            ("Show/episode.mkv".to_string(), 1000),
            ("Show/episode.tr.srt".to_string(), 2),
            ("Show/info.nfo".to_string(), 1),
        ];
        assert_eq!(pick_files(&files), Some(vec![1, 2]));
        // Aynı filmin AVI ve MP4 sürümü: tarayıcıda oynayan MP4 seçilir.
        let alt = vec![("film.avi".to_string(), 330), ("film.mp4".to_string(), 62), ("film-sample.mkv".to_string(), 300)];
        assert_eq!(pick_files(&alt), Some(vec![1]));
        assert_eq!(pick_files(&[("a.txt".into(), 5)]), None);
    }

    #[test]
    fn srt_and_cp1254() {
        // "Güzel" in windows-1254: ü=0xFC
        let srt = b"1\r\n00:00:01,000 --> 00:00:02,500\r\nG\xFCzel\r\n";
        let vtt = to_vtt("a.srt", srt);
        assert!(vtt.starts_with("WEBVTT"));
        assert!(vtt.contains("00:00:01.000 --> 00:00:02.500"));
        assert!(vtt.contains("Güzel"));
    }

    #[test]
    fn ass_dialogue() {
        let ass = "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.50,0:00:03.00,Default,,0,0,0,,{\\i1}Merhaba,\\Ndünya\n";
        let vtt = to_vtt("a.ass", ass.as_bytes());
        assert!(vtt.contains("00:00:01.500 --> 00:00:03.000\nMerhaba,\ndünya"));
    }
}
