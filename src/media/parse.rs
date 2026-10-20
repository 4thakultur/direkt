//! Release-style filename parsing.
//!
//! Turns names like `Show.Name.S02E05.1080p.WEB.x264-GRP.mkv` or
//! `Movie.Title.2017.1080p.BluRay.mkv` into a [`MediaGuess`]. Parsing is
//! deterministic and conservative: a name without a recognizable structure
//! (episode marker, release year, or quality tag) yields `None` rather than a
//! guess.

use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Movie,
    Tv,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Tv => "tv",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaGuess {
    pub kind: MediaKind,
    /// Title as written in the name, separators replaced by spaces.
    pub title: String,
    pub year: Option<u16>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
}

/// Extensions stripped before parsing: video, subtitles, archives, and split
/// archive parts.
const EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "mov", "wmv", "ts", "m2ts", "webm", "mpg", "mpeg", "flv", "iso",
    "srt", "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz",
];

/// Tokens that start the technical part of a release name.
const RELEASE_TAGS: &[&str] = &[
    "480p",
    "576p",
    "720p",
    "1080p",
    "1080i",
    "2160p",
    "4k",
    "uhd",
    "hdr",
    "hdr10",
    "dv",
    "web",
    "web-dl",
    "webdl",
    "webrip",
    "bluray",
    "blu-ray",
    "bdrip",
    "brrip",
    "dvdrip",
    "hdtv",
    "hdrip",
    "remux",
    "x264",
    "x265",
    "h264",
    "h265",
    "hevc",
    "avc",
    "xvid",
    "10bit",
    "proper",
    "repack",
    "multi",
    "french",
    "truefrench",
    "vostfr",
    "vff",
    "vfq",
    "german",
    "dual",
    "internal",
    "extended",
    "unrated",
    "imax",
    "complete",
    "integrale",
];

static SEASON_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^s(\d{1,2})(?:e(\d{1,3}))?(?:-?e\d{1,3})*$").expect("valid regex")
});
static CROSS_EPISODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{1,2})x(\d{2,3})$").expect("valid regex"));
static YEAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[(\[]?((?:19|20)\d{2})[)\]]?$").expect("valid regex"));
static LEADING_GROUP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\[[^\]]*\]\s*").expect("valid regex"));
static PART_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[._ -]part\d{1,3}$").expect("valid regex"));

/// Longest title worth looking up; anything longer is not a release name.
const MAX_TITLE_LEN: usize = 120;

pub fn parse_filename(name: &str) -> Option<MediaGuess> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = PART_SUFFIX.replace(strip_extensions(base), "");
    let stem = LEADING_GROUP.replace(&stem, "");
    let spaced: String = stem
        .chars()
        .map(|c| if matches!(c, '.' | '_') { ' ' } else { c })
        .collect();
    let tokens: Vec<&str> = spaced.split_whitespace().collect();

    for (i, token) in tokens.iter().enumerate().skip(1) {
        let (season, episode) = if let Some(c) = SEASON_EPISODE.captures(token) {
            (
                c[1].parse().ok(),
                c.get(2).and_then(|m| m.as_str().parse().ok()),
            )
        } else if let Some(c) = CROSS_EPISODE.captures(token) {
            (c[1].parse().ok(), c[2].parse().ok())
        } else if token.eq_ignore_ascii_case("season")
            && let Some(n) = tokens.get(i + 1).and_then(|t| t.parse().ok())
        {
            (Some(n), None)
        } else {
            continue;
        };
        let mut title = &tokens[..i];
        // "Doctor.Who.2005.S01E01": the year belongs to the show.
        let mut year = None;
        if let [rest @ .., last] = title
            && !rest.is_empty()
            && let Some(y) = parse_year(last)
        {
            year = Some(y);
            title = rest;
        }
        return guess(MediaKind::Tv, title, year, season, episode);
    }

    let cut = tokens
        .iter()
        .skip(1)
        .position(|t| is_release_tag(t))
        .map(|p| p + 1)
        .unwrap_or(tokens.len());
    let year_at = (1..cut).rev().find(|&i| parse_year(tokens[i]).is_some());
    match year_at {
        Some(i) => guess(
            MediaKind::Movie,
            &tokens[..i],
            parse_year(tokens[i]),
            None,
            None,
        ),
        // Without a year, only a release tag shows this is media at all.
        None if cut < tokens.len() => guess(MediaKind::Movie, &tokens[..cut], None, None, None),
        None => None,
    }
}

fn strip_extensions(mut name: &str) -> &str {
    // At most two: "name.mkv.001", "name.tar.gz".
    for _ in 0..2 {
        let Some((stem, ext)) = name.rsplit_once('.') else {
            break;
        };
        let split_part = ext.len() == 3 && ext.bytes().all(|b| b.is_ascii_digit());
        let rar_part = ext.len() == 3
            && ext.as_bytes()[0].eq_ignore_ascii_case(&b'r')
            && ext[1..].bytes().all(|b| b.is_ascii_digit());
        let known = EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext));
        if stem.is_empty() || !(known || split_part || rar_part) {
            break;
        }
        name = stem;
    }
    name
}

fn parse_year(token: &str) -> Option<u16> {
    YEAR.captures(token).and_then(|c| c[1].parse().ok())
}

fn is_release_tag(token: &str) -> bool {
    let token = token.trim_matches(|c| matches!(c, '[' | ']' | '(' | ')'));
    RELEASE_TAGS.iter().any(|t| t.eq_ignore_ascii_case(token))
}

fn guess(
    kind: MediaKind,
    title: &[&str],
    year: Option<u16>,
    season: Option<u32>,
    episode: Option<u32>,
) -> Option<MediaGuess> {
    let title = title
        .join(" ")
        .trim_matches(|c: char| c == '-' || c.is_whitespace())
        .to_owned();
    if title.is_empty() || title.len() > MAX_TITLE_LEN || !title.chars().any(char::is_alphanumeric)
    {
        return None;
    }
    Some(MediaGuess {
        kind,
        title,
        year,
        season,
        episode,
    })
}

/// A title reduced for comparison: lowercase alphanumeric words.
pub fn normalize_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    for word in title
        .replace('&', " and ")
        .replace(['\'', '\u{2019}'], "")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(word.chars().flat_map(char::to_lowercase));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tv(title: &str, year: Option<u16>, season: u32, episode: Option<u32>) -> Option<MediaGuess> {
        Some(MediaGuess {
            kind: MediaKind::Tv,
            title: title.into(),
            year,
            season: Some(season),
            episode,
        })
    }

    fn movie(title: &str, year: Option<u16>) -> Option<MediaGuess> {
        Some(MediaGuess {
            kind: MediaKind::Movie,
            title: title.into(),
            year,
            season: None,
            episode: None,
        })
    }

    #[test]
    fn tv_names() {
        let cases = [
            (
                "Cyberpunk.Edgerunners.S02.MULTi.AD.1080p.NF.WEB.x264-BYOR.zip",
                tv("Cyberpunk Edgerunners", None, 2, None),
            ),
            (
                "The.Office.US.S03E10.720p.HDTV.mkv",
                tv("The Office US", None, 3, Some(10)),
            ),
            (
                "Doctor.Who.2005.S01E01.Rose.mkv",
                tv("Doctor Who", Some(2005), 1, Some(1)),
            ),
            ("Show_Name_2x05_HDTV.avi", tv("Show Name", None, 2, Some(5))),
            ("Show.S01E01E02.mkv", tv("Show", None, 1, Some(1))),
            (
                "[Group] Some Show Season 2 [1080p].mkv",
                tv("Some Show", None, 2, None),
            ),
        ];
        for (name, expected) in cases {
            assert_eq!(parse_filename(name), expected, "{name}");
        }
    }

    #[test]
    fn movie_names() {
        let cases = [
            (
                "Blade.Runner.2049.2017.1080p.BluRay.x264.mkv",
                movie("Blade Runner 2049", Some(2017)),
            ),
            ("2012.2009.720p.mkv", movie("2012", Some(2009))),
            ("Spider-Man (2002).mkv", movie("Spider-Man", Some(2002))),
            ("Inception.1080p.BluRay.mkv", movie("Inception", None)),
            (
                "Some.Movie.2019.MULTi.1080p.WEB.part01.rar",
                movie("Some Movie", Some(2019)),
            ),
            ("Some.Movie.2019.mkv.001", movie("Some Movie", Some(2019))),
        ];
        for (name, expected) in cases {
            assert_eq!(parse_filename(name), expected, "{name}");
        }
    }

    #[test]
    fn non_media_names_are_not_guessed() {
        for name in [
            "ubuntu-24.04-desktop-amd64.iso",
            "archive.zip",
            "report.pdf",
            "2019.mkv",
            "S01E01.mkv",
            "setup.exe",
            "",
        ] {
            assert_eq!(parse_filename(name), None, "{name}");
        }
    }

    #[test]
    fn titles_normalize_for_comparison() {
        assert_eq!(
            normalize_title("Cyberpunk: Edgerunners"),
            "cyberpunk edgerunners"
        );
        assert_eq!(normalize_title("Spider-Man"), "spider man");
        assert_eq!(normalize_title("Law & Order"), "law and order");
        assert_eq!(normalize_title("Grey's Anatomy"), "greys anatomy");
        assert_eq!(normalize_title("  Amélie  "), "amélie");
    }
}
