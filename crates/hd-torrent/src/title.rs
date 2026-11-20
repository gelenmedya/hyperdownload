//! Film adı çıkarımı: ham torrent/dosya adından "Thor: Love and Thunder", 2022, "UHD Remux".
//!
//! Sıra: (a) MKV Segment Info `Title` Latin harfliyse, (b) torrent adında `/` ile ayrılmış Latin kısım
//! (parantez/köşeli parantez sonrası atılır), (c) temizlenmiş dosya adı (noktalar boşluk olur; yıl ve
//! kalite/kodek etiketlerinden sonrası atılır).

use crate::media::{kind_of, FileKind};

/// Ad ayrıştırıcının "sürüm etiketi" saydığı küçük harfli sözcükler; ilk etikette başlık biter.
const TAGS: &[&str] = &[
    "2160p", "1440p", "1080p", "1080i", "720p", "576p", "480p", "4k", "uhd", "bluray", "blu-ray", "bdrip", "brrip", "bdremux",
    "remux", "hdr", "hdr10", "hdr10+", "hdr10plus", "dv", "dovi", "x264", "x265", "h264", "h265", "hevc", "avc", "truehd",
    "atmos", "web-dl", "webdl", "webrip", "hdtv", "dvdrip", "dts", "dts-hd", "ddp5", "aac", "ac3", "eac3", "flac",
    "10bit", "imax", "repack", "proper", "extended", "unrated", "remastered", "multi", "dual",
];

fn is_year(tok: &str) -> bool {
    tok.len() == 4 && tok.bytes().all(|b| b.is_ascii_digit()) && matches!(&tok[..2], "19" | "20")
}

/// Sözcüğün başındaki/sonundaki noktalama atılıp küçük harfe çevrilir; tire ile bağlı ek (`x265-GRP`) etikettir.
fn is_tag(tok: &str) -> bool {
    let t = tok.trim_matches(|c: char| !c.is_alphanumeric() && c != '+').to_lowercase();
    if t.is_empty() {
        return false;
    }
    TAGS.contains(&t.as_str()) || t.split('-').next().is_some_and(|h| h != t && TAGS.contains(&h))
}

/// "WEB.DL" / "WEB.Rip" noktayla bölününce: "web" yalnız ardından "dl"/"rip" geliyorsa etikettir ("Charlotte's Web" korunur).
fn is_web_dl(toks: &[String], i: usize) -> bool {
    toks[i].eq_ignore_ascii_case("web") && toks.get(i + 1).is_some_and(|n| n.eq_ignore_ascii_case("dl") || n.eq_ignore_ascii_case("rip"))
}

fn tokens(s: &str) -> Vec<String> {
    s.chars()
        .map(|c| match c {
            '.' | '_' | '(' | ')' | '[' | ']' | ',' | ';' => ' ',
            c => c,
        })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Her harf Latin (ASCII + Latin-1/Genişletilmiş-A/B) ve en az bir harf var mı?
fn is_latin(s: &str) -> bool {
    let mut letters = 0;
    for c in s.chars().filter(|c| c.is_alphabetic()) {
        let u = c as u32;
        let latin = c.is_ascii_alphabetic() || ((0xC0..=0x24F).contains(&u) && u != 0xD7 && u != 0xF7);
        if !latin {
            return false;
        }
        letters += 1;
    }
    letters > 0
}

struct Cleaned {
    title: String,
    year: Option<u16>,
}

/// "Dune.Part.Two.2024.2160p.UHD.BluRay.x265-GRP" -> ("Dune Part Two", 2024).
/// Başlık ilk etikete kadardır; o aralığın son sözcüğü yılsa (ilk sözcük hariç) yıl olur ve başlıktan çıkar
/// ("Blade.Runner.2049.2017" -> "Blade Runner 2049", 2017).
fn clean_release(s: &str) -> Cleaned {
    let toks = tokens(s);
    let q = (0..toks.len()).find(|&i| is_tag(&toks[i]) || is_web_dl(&toks, i)).unwrap_or(toks.len());
    let head = &toks[..q];
    let mut year = None;
    let mut end = head.len();
    if let Some(i) = head.iter().rposition(|t| is_year(t)).filter(|&i| i > 0) {
        year = head[i].parse().ok();
        end = i;
    } else if let Some(t) = toks[q..].iter().find(|t| is_year(t)) {
        year = t.parse().ok();
    }
    let mut title: Vec<&str> = head[..end].iter().map(String::as_str).collect();
    while title.last().is_some_and(|t| t.chars().all(|c| c == '-' || c == ':')) {
        title.pop();
    }
    Cleaned { title: title.join(" "), year }
}

/// Başlık "dosya adı gibi" mi (boşluksuz, noktalı/alt çizgili)?
fn looks_like_release(s: &str) -> bool {
    !s.contains(' ') && s.matches(['.', '_']).count() >= 2
}

/// `/` ile ayrılmış kısımlardan ilk Latin olanı; `(`/`[` sonrası atılır.
fn latin_segment(torrent_name: &str) -> Option<String> {
    if !torrent_name.contains('/') {
        return None;
    }
    torrent_name.split('/').find_map(|seg| {
        let cut = seg.find(['(', '[']).map_or(seg, |i| &seg[..i]);
        let cut = cut.trim().trim_end_matches(['-', ':', ',', ' ']).trim();
        (!cut.is_empty() && is_latin(cut)).then(|| cut.to_string())
    })
}

fn base_name(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

fn strip_ext(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if kind_of(name) == FileKind::Video => &name[..i],
        _ => name,
    }
}

/// "UHD Remux", "1080p BluRay", "WEB-DL" ... ; dosya adı öncelikli, eksik kalanlar torrent adından.
fn quality(file_name: &str, torrent_name: &str) -> Option<String> {
    #[derive(Default)]
    struct Facets {
        uhd: bool,
        res: Option<&'static str>,
        src: Option<&'static str>,
    }
    fn scan(s: &str) -> Facets {
        let low = s.to_lowercase().replace(['.', '_', ' '], "-");
        let toks: Vec<String> = tokens(s).iter().map(|t| t.to_lowercase()).collect();
        let has = |w: &str| toks.iter().any(|t| t == w || t.split('-').next() == Some(w));
        let mut f = Facets { uhd: has("uhd") || has("2160p") || has("4k"), ..Default::default() };
        f.res = ["1080p", "720p", "576p", "480p"].into_iter().find(|r| has(r));
        f.src = if has("remux") || has("bdremux") {
            Some("Remux")
        } else if low.contains("web-dl") || has("webdl") {
            Some("WEB-DL")
        } else if has("webrip") {
            Some("WEBRip")
        } else if has("bluray") || low.contains("blu-ray") {
            Some("BluRay")
        } else if has("bdrip") || has("brrip") {
            Some("BDRip")
        } else if has("hdtv") {
            Some("HDTV")
        } else if has("dvdrip") {
            Some("DVDRip")
        } else {
            None
        };
        f
    }
    let (f, t) = (scan(file_name), scan(torrent_name));
    let uhd = f.uhd || (f.res.is_none() && t.uhd);
    let res = if uhd { Some("UHD") } else { f.res.or(t.res) };
    let src = f.src.or(t.src);
    let q = [res, src].into_iter().flatten().collect::<Vec<_>>().join(" ");
    (!q.is_empty()).then_some(q)
}

/// `(başlık, yıl, kalite)`; sıra için modül belgesine bakın.
pub fn display_title(torrent_name: &str, file_name: &str, mkv_title: Option<&str>) -> (String, Option<u16>, Option<String>) {
    let file = base_name(file_name);
    let stem = strip_ext(file);
    let from_file = clean_release(stem);
    let mut year = from_file.year;

    let mkv = mkv_title.map(str::trim).filter(|t| !t.is_empty() && is_latin(t));
    let title = if let Some(t) = mkv {
        if looks_like_release(t) {
            let c = clean_release(t);
            year = year.or(c.year);
            if c.title.is_empty() {
                t.to_string()
            } else {
                c.title
            }
        } else {
            t.to_string()
        }
    } else if let Some(t) = latin_segment(torrent_name) {
        t
    } else if !from_file.title.is_empty() {
        from_file.title.clone()
    } else if !stem.is_empty() {
        stem.to_string()
    } else {
        torrent_name.trim().to_string()
    };

    if year.is_none() {
        year = tokens(torrent_name).iter().enumerate().find(|(i, t)| *i > 0 && is_year(t)).and_then(|(_, t)| t.parse().ok());
    }
    (title, year, quality(stem, torrent_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const THOR_TORRENT: &str = "Тор: Любовь и гром / Thor: Love and Thunder (Taika Waititi) [2022, США, фантастика, UHD Blu-ray Remux, 2160p, HDR10, Dolby Vision, Atmos]";
    const THOR_FILE: &str = "Thor.Love.and.Thunder.2022.UHD.BluRay.2160p.TrueHD.Atmos.7.1.DV.HEVC.REMUX-FraMeSToR.mkv";

    #[test]
    fn mixed_ru_en_torrent_name_gives_latin_part() {
        let (t, y, q) = display_title(THOR_TORRENT, "Тор.Любовь.и.гром.2022.mkv", None);
        assert_eq!(t, "Thor: Love and Thunder");
        assert_eq!(y, Some(2022));
        assert_eq!(q.as_deref(), Some("UHD Remux"));
    }

    #[test]
    fn latin_mkv_title_wins_over_everything() {
        let (t, y, q) = display_title(THOR_TORRENT, THOR_FILE, Some("Thor: Love and Thunder (2022)"));
        assert_eq!(t, "Thor: Love and Thunder (2022)");
        assert_eq!(y, Some(2022));
        assert_eq!(q.as_deref(), Some("UHD Remux"));
    }

    #[test]
    fn cyrillic_mkv_title_is_ignored() {
        let (t, ..) = display_title(THOR_TORRENT, THOR_FILE, Some("Тор: Любовь и гром"));
        assert_eq!(t, "Thor: Love and Thunder");
    }

    #[test]
    fn release_like_mkv_title_is_cleaned() {
        let (t, y, _) = display_title("x", "a.mkv", Some("Thor.Love.and.Thunder.2022.2160p.BluRay"));
        assert_eq!(t, "Thor Love and Thunder");
        assert_eq!(y, Some(2022));
    }

    #[test]
    fn empty_mkv_title_falls_through() {
        let (t, ..) = display_title(THOR_TORRENT, THOR_FILE, Some("  "));
        assert_eq!(t, "Thor: Love and Thunder");
    }

    #[test]
    fn dotted_file_name_is_cleaned() {
        let (t, y, q) = display_title(
            "Dune.Part.Two.2024.2160p.UHD.BluRay.x265-GRP",
            "Dune.Part.Two.2024.2160p.UHD.BluRay.x265-GRP.mkv",
            None,
        );
        assert_eq!(t, "Dune Part Two");
        assert_eq!(y, Some(2024));
        assert_eq!(q.as_deref(), Some("UHD BluRay"));
    }

    #[test]
    fn russian_torrent_with_latin_file_name() {
        let (t, y, q) = display_title("Тор Любовь и гром (2022) BDRip", "Thor.Love.and.Thunder.2022.1080p.BluRay.x264.mkv", None);
        assert_eq!(t, "Thor Love and Thunder");
        assert_eq!(y, Some(2022));
        assert_eq!(q.as_deref(), Some("1080p BluRay"));
    }

    #[test]
    fn all_cyrillic_keeps_cleaned_file_name() {
        let (t, y, q) = display_title("Тор Любовь и гром", "Тор.Любовь.и.гром.2022.BDRip.mkv", None);
        assert_eq!(t, "Тор Любовь и гром");
        assert_eq!(y, Some(2022));
        assert_eq!(q.as_deref(), Some("BDRip"));
    }

    #[test]
    fn title_that_looks_like_a_year() {
        let (t, y, _) = display_title("2012", "2012.2009.1080p.BluRay.mkv", None);
        assert_eq!((t.as_str(), y), ("2012", Some(2009)));
    }

    #[test]
    fn sequel_number_stays_in_title() {
        let (t, y, q) = display_title("x", "Blade.Runner.2049.2017.2160p.WEB-DL.DDP5.1.mkv", None);
        assert_eq!((t.as_str(), y), ("Blade Runner 2049", Some(2017)));
        assert_eq!(q.as_deref(), Some("UHD WEB-DL"));
    }

    #[test]
    fn parenthesised_year_and_no_tags() {
        let (t, y, q) = display_title("Film (1999)", "Film (1999).mkv", None);
        assert_eq!((t.as_str(), y, q), ("Film", Some(1999), None));
    }

    #[test]
    fn year_from_torrent_name_when_file_has_none() {
        let (t, y, _) = display_title("Фильм / Film (2001) [2001, BDRip]", "film.mkv", None);
        assert_eq!(t, "Film");
        assert_eq!(y, Some(2001));
    }

    #[test]
    fn segment_cut_at_bracket_and_hyphen() {
        assert_eq!(latin_segment("Фильм / The Film - [2010, 1080p]").as_deref(), Some("The Film"));
        assert_eq!(latin_segment("The Film [2010] / Фильм").as_deref(), Some("The Film"));
        assert_eq!(latin_segment("Фильм (2010)"), None);
        assert_eq!(latin_segment("Plain Name"), None);
    }

    #[test]
    fn latin_detection() {
        assert!(is_latin("Amélie: Le Fabuleux Destin"));
        assert!(!is_latin("Тор: Love"));
        assert!(!is_latin("2012"));
        assert!(!is_latin("東京物語"));
    }

    #[test]
    fn web_is_a_tag_only_before_dl() {
        let (t, y, q) = display_title("x", "Charlotte's.Web.2006.1080p.WEB.DL.mkv", None);
        assert_eq!((t.as_str(), y), ("Charlotte's Web", Some(2006)));
        assert_eq!(q.as_deref(), Some("1080p WEB-DL"));
        let (t, ..) = display_title("x", "Some.Movie.WEB.DL.mkv", None);
        assert_eq!(t, "Some Movie");
    }

    #[test]
    fn nothing_usable_falls_back_to_raw() {
        let (t, y, q) = display_title("abc", "", None);
        assert_eq!((t.as_str(), y, q), ("abc", None, None));
    }
}
