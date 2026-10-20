//! Release-style filename parsing.
//!
//! [`detect`] suggests a [`MediaType`] for a file name and, for movies, TV
//! series and games, a [`MediaGuess`] to look up: names like
//! `Show.Name.S02E05.1080p.WEB.x264-GRP.mkv`, `Movie.Title.2017.1080p.mkv` or
//! `Game.Title.v1.2-CODEX.zip`. Parsing is deterministic and conservative: a
//! name without a recognizable structure is `Unsorted` rather than a guess.
//! [`guess_as`] redoes the guess for a type the user picked.

use crate::domain::MediaType;
use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaGuess {
    /// `Movie`, `Tv` or `Game`: the types that have a lookup.
    pub kind: MediaType,
    /// Title as written in the name, separators replaced by spaces.
    pub title: String,
    pub year: Option<u16>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    /// Last episode of a multi-episode file (`S01E01E02`, `S01E01-E03`).
    pub last_episode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub media_type: MediaType,
    pub guess: Option<MediaGuess>,
}

const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "mov", "wmv", "ts", "m2ts", "webm", "mpg", "mpeg", "flv", "srt",
];
const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "m4a", "ogg", "opus", "wav", "alac", "ape"];
const SOFTWARE_EXTENSIONS: &[&str] = &["exe", "msi", "dmg", "deb", "rpm", "appimage", "apk"];
const GAME_EXTENSIONS: &[&str] = &["nsp", "xci"];
/// Containers that say nothing about the content.
const ARCHIVE_EXTENSIONS: &[&str] = &["iso", "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz"];

/// Tokens that start the technical part of a movie or TV release name.
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

/// Tags only video releases carry; they rule out music.
const VIDEO_TAGS: &[&str] = &[
    "480p", "576p", "720p", "1080p", "2160p", "x264", "x265", "h264", "h265", "hevc", "bluray",
    "webrip", "hdtv", "remux",
];

/// Scene groups and repackers that release games.
const GAME_GROUPS: &[&str] = &[
    "codex",
    "plaza",
    "skidrow",
    "reloaded",
    "cpy",
    "empress",
    "tenoke",
    "rune",
    "flt",
    "doge",
    "tinyiso",
    "darksiders",
    "razor1911",
    "prophet",
    "hoodlum",
    "chronos",
    "simplex",
    "fitgirl",
    "dodi",
    "elamigos",
    "kaos",
    "goldberg",
    "gog",
    "steamrip",
    "anadius",
    "nsw",
];

const MUSIC_WORDS: &[&str] = &[
    "flac",
    "mp3",
    "alac",
    "320kbps",
    "lossless",
    "discography",
    "24bit",
    "16bit",
    "vinyl",
];

const SOFTWARE_WORDS: &[&str] = &[
    "x64",
    "x86",
    "amd64",
    "arm64",
    "win64",
    "win32",
    "windows",
    "macos",
    "linux",
    "portable",
    "setup",
    "installer",
    "multilingual",
];

static SEASON_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^s(\d{1,2})(?:e(\d{1,3}))?(?:-?e(\d{1,3}))*$").expect("valid regex")
});
static CROSS_EPISODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{1,2})x(\d{2,3})$").expect("valid regex"));
static YEAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[(\[]?((?:19|20)\d{2})[)\]]?$").expect("valid regex"));
static LEADING_GROUP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\[[^\]]*\]\s*").expect("valid regex"));
static PART_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[._ -]part\d{1,3}$").expect("valid regex"));
/// Tokens that end a game title: versions, builds, updates, edition notes.
static GAME_STOP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:v\d.*|build|update|repack|multi\d*|incl|dlcs?|\+|[\[(].*)$")
        .expect("valid regex")
});

/// Longest title worth looking up; anything longer is not a release name.
const MAX_TITLE_LEN: usize = 120;

/// A file name split for parsing.
struct Name {
    /// The last extension that says what the content is (not an archive).
    content_ext: Option<String>,
    /// Whitespace-separated tokens, `.` and `_` read as spaces.
    tokens: Vec<String>,
    /// Lowercase words, also split on `-` and brackets, for tag matching.
    words: Vec<String>,
}

impl Name {
    fn new(name: &str) -> Self {
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        let (stem, content_ext) = strip_extensions(base);
        let stem = PART_SUFFIX.replace(stem, "");
        let stem = LEADING_GROUP.replace(&stem, "");
        let spaced: String = stem
            .chars()
            .map(|c| if matches!(c, '.' | '_') { ' ' } else { c })
            .collect();
        let tokens: Vec<String> = spaced.split_whitespace().map(str::to_owned).collect();
        let words = spaced
            .split(|c: char| c.is_whitespace() || matches!(c, '-' | '[' | ']' | '(' | ')'))
            .filter(|w| !w.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        Self {
            content_ext,
            tokens,
            words,
        }
    }

    fn ext_in(&self, list: &[&str]) -> bool {
        self.content_ext
            .as_deref()
            .is_some_and(|e| list.contains(&e))
    }

    fn has_word(&self, list: &[&str]) -> bool {
        self.words.iter().any(|w| list.contains(&w.as_str()))
    }

    fn tokens(&self) -> Vec<&str> {
        self.tokens.iter().map(String::as_str).collect()
    }
}

pub fn detect(filename: &str) -> Detection {
    let name = Name::new(filename);
    let found = |media_type, guess| Detection { media_type, guess };
    if name.ext_in(VIDEO_EXTENSIONS) {
        return match release_guess(&name) {
            Some(g) => found(g.kind, Some(g)),
            None => found(MediaType::Unsorted, None),
        };
    }
    if name.ext_in(AUDIO_EXTENSIONS) {
        return found(MediaType::Music, None);
    }
    if name.ext_in(GAME_EXTENSIONS) || name.has_word(GAME_GROUPS) {
        let guess = game_guess(&name);
        return found(MediaType::Game, guess);
    }
    if name.ext_in(SOFTWARE_EXTENSIONS) {
        return found(MediaType::Software, None);
    }
    if let Some(g) = tv_guess(&name.tokens()) {
        return found(MediaType::Tv, Some(g));
    }
    if name.has_word(MUSIC_WORDS) && !name.has_word(VIDEO_TAGS) {
        return found(MediaType::Music, None);
    }
    if let Some(g) = movie_guess(&name.tokens()) {
        return found(MediaType::Movie, Some(g));
    }
    if name.has_word(SOFTWARE_WORDS) {
        return found(MediaType::Software, None);
    }
    found(MediaType::Unsorted, None)
}

/// A movie or TV guess, as the structure of the name says.
pub fn parse_filename(filename: &str) -> Option<MediaGuess> {
    release_guess(&Name::new(filename))
}

/// The guess to look up when the user says the file is `kind`. Falls back to
/// the name up to its first release tag, version or year.
pub fn guess_as(filename: &str, kind: MediaType) -> Option<MediaGuess> {
    if !matches!(kind, MediaType::Movie | MediaType::Tv | MediaType::Game) {
        return None;
    }
    let name = Name::new(filename);
    let parsed = match kind {
        MediaType::Game => game_guess(&name),
        _ => release_guess(&name),
    };
    if let Some(g) = parsed {
        return Some(MediaGuess { kind, ..g });
    }
    let title = title_until(&name.tokens(), |t| {
        is_release_tag(t) || parse_year(t).is_some()
    });
    guess(kind, &title, None, None, None)
}

fn release_guess(name: &Name) -> Option<MediaGuess> {
    let tokens = name.tokens();
    tv_guess(&tokens).or_else(|| movie_guess(&tokens))
}

fn tv_guess(tokens: &[&str]) -> Option<MediaGuess> {
    for (i, token) in tokens.iter().enumerate().skip(1) {
        let (season, episode, last) = if let Some(c) = SEASON_EPISODE.captures(token) {
            let number = |i: usize| c.get(i).and_then(|m| m.as_str().parse::<u32>().ok());
            (c[1].parse().ok(), number(2), number(3))
        } else if let Some(c) = CROSS_EPISODE.captures(token) {
            (c[1].parse().ok(), c[2].parse().ok(), None)
        } else if token.eq_ignore_ascii_case("season")
            && let Some(n) = tokens.get(i + 1).and_then(|t| t.parse().ok())
        {
            (Some(n), None, None)
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
        let last_episode = last.filter(|&l| episode.is_some_and(|e| l > e));
        return guess(MediaType::Tv, title, year, season, episode)
            .map(|g| MediaGuess { last_episode, ..g });
    }
    None
}

fn movie_guess(tokens: &[&str]) -> Option<MediaGuess> {
    let cut = tokens
        .iter()
        .skip(1)
        .position(|t| is_release_tag(t))
        .map(|p| p + 1)
        .unwrap_or(tokens.len());
    let year_at = (1..cut).rev().find(|&i| parse_year(tokens[i]).is_some());
    match year_at {
        Some(i) => guess(
            MediaType::Movie,
            &tokens[..i],
            parse_year(tokens[i]),
            None,
            None,
        ),
        // Without a year, only a release tag shows this is media at all.
        None if cut < tokens.len() => guess(MediaType::Movie, &tokens[..cut], None, None, None),
        None => None,
    }
}

/// The game title: the name up to its version, build, or bracketed notes,
/// without a trailing `-GROUP`.
fn game_guess(name: &Name) -> Option<MediaGuess> {
    let title = title_until(&name.tokens(), |_| false);
    guess(MediaType::Game, &title, None, None, None)
}

/// Leading tokens up to a game stop token, a group tag, or `stop`. A trailing
/// `-GROUP` on a token is dropped ("II-TENOKE" ends the title as "II").
fn title_until<'a>(tokens: &[&'a str], stop: impl Fn(&str) -> bool) -> Vec<&'a str> {
    let mut title = Vec::new();
    for (i, &token) in tokens.iter().enumerate() {
        let leading_group = token.split('-').next().is_some_and(is_game_group);
        if i > 0 && (GAME_STOP.is_match(token) || leading_group || stop(token)) {
            break;
        }
        if let Some((head, group)) = token.rsplit_once('-')
            && is_game_group(group)
        {
            if !head.is_empty() {
                title.push(head);
            }
            break;
        }
        title.push(token);
    }
    title
}

fn is_game_group(word: &str) -> bool {
    GAME_GROUPS.iter().any(|g| g.eq_ignore_ascii_case(word))
}

/// Strip up to two extensions ("name.mkv.001", "name.tar.gz"), returning the
/// stem and the content extension, lowercase.
fn strip_extensions(mut name: &str) -> (&str, Option<String>) {
    let mut content_ext = None;
    for _ in 0..2 {
        let Some((stem, ext)) = name.rsplit_once('.') else {
            break;
        };
        let ext = ext.to_ascii_lowercase();
        let split_part = ext.len() == 3 && ext.bytes().all(|b| b.is_ascii_digit());
        let rar_part =
            ext.len() == 3 && ext.starts_with('r') && ext[1..].bytes().all(|b| b.is_ascii_digit());
        let container = split_part || rar_part || ARCHIVE_EXTENSIONS.contains(&ext.as_str());
        let content = [
            VIDEO_EXTENSIONS,
            AUDIO_EXTENSIONS,
            SOFTWARE_EXTENSIONS,
            GAME_EXTENSIONS,
        ]
        .iter()
        .any(|list| list.contains(&ext.as_str()));
        if stem.is_empty() || !(container || content) {
            break;
        }
        if content && content_ext.is_none() {
            content_ext = Some(ext);
        }
        name = stem;
    }
    (name, content_ext)
}

fn parse_year(token: &str) -> Option<u16> {
    YEAR.captures(token).and_then(|c| c[1].parse().ok())
}

fn is_release_tag(token: &str) -> bool {
    let token = token.trim_matches(|c| matches!(c, '[' | ']' | '(' | ')'));
    RELEASE_TAGS.iter().any(|t| t.eq_ignore_ascii_case(token))
}

fn guess(
    kind: MediaType,
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
        last_episode: None,
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
            kind: MediaType::Tv,
            title: title.into(),
            year,
            season: Some(season),
            episode,
            last_episode: None,
        })
    }

    fn movie(title: &str, year: Option<u16>) -> Option<MediaGuess> {
        Some(MediaGuess {
            kind: MediaType::Movie,
            title: title.into(),
            year,
            season: None,
            episode: None,
            last_episode: None,
        })
    }

    fn game(title: &str) -> Option<MediaGuess> {
        Some(MediaGuess {
            kind: MediaType::Game,
            title: title.into(),
            year: None,
            season: None,
            episode: None,
            last_episode: None,
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
            (
                "Show.S01E01E02.mkv",
                tv("Show", None, 1, Some(1)).map(|g| MediaGuess {
                    last_episode: Some(2),
                    ..g
                }),
            ),
            (
                "[Group] Some Show Season 2 [1080p].mkv",
                tv("Some Show", None, 2, None),
            ),
        ];
        for (name, expected) in cases {
            assert_eq!(parse_filename(name), expected, "{name}");
            assert_eq!(detect(name).media_type, MediaType::Tv, "{name}");
        }
    }

    #[test]
    fn multi_episode_names() {
        let last = |name: &str| parse_filename(name).and_then(|g| g.last_episode);
        assert_eq!(last("Show.S01E01E02.mkv"), Some(2));
        assert_eq!(last("Show.S01E01-E03.720p.mkv"), Some(3));
        assert_eq!(last("Show.S01E05.mkv"), None);
        assert_eq!(last("Show.S01E05E05.mkv"), None, "not a range");
        assert_eq!(last("Show.S01E05E02.mkv"), None, "backwards");
        let g = parse_filename("Show.S01E01E02.mkv").unwrap();
        assert_eq!((g.season, g.episode), (Some(1), Some(1)));
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
            (
                "Superman.2025.Multi.Truefrench.1080p.Bluray.Remux.AVC-BDHD.mkv",
                movie("Superman", Some(2025)),
            ),
        ];
        for (name, expected) in cases {
            assert_eq!(parse_filename(name), expected, "{name}");
            assert_eq!(detect(name).guess, expected, "{name}");
        }
    }

    #[test]
    fn game_names() {
        let cases = [
            ("Hades.II-TENOKE.zip", game("Hades II")),
            ("Cyberpunk.2077-CODEX.iso", game("Cyberpunk 2077")),
            (
                "Baldurs.Gate.3.v4.1.1.MULTi12-ElAmigos.rar",
                game("Baldurs Gate 3"),
            ),
            ("Elden Ring [FitGirl Repack].part02.rar", game("Elden Ring")),
            ("Stardew.Valley.Build.12345-GOG.zip", game("Stardew Valley")),
            ("Some.Game.NSW-VENOM.nsp", game("Some Game")),
        ];
        for (name, expected) in cases {
            let d = detect(name);
            assert_eq!(d.media_type, MediaType::Game, "{name}");
            assert_eq!(d.guess, expected, "{name}");
        }
    }

    #[test]
    fn music_and_software_names() {
        for name in [
            "Artist-Album-WEB-2023-FLAC.zip",
            "Some Artist - Discography (1999-2020) [MP3 320kbps].rar",
            "song.mp3",
        ] {
            assert_eq!(detect(name).media_type, MediaType::Music, "{name}");
        }
        assert_eq!(
            detect("Movie.2019.1080p.BluRay.FLAC.x264.mkv").media_type,
            MediaType::Movie,
            "audio codec tags on a movie"
        );
        for name in [
            "ubuntu-24.04-desktop-amd64.iso",
            "SomeTool.v2.1.x64.Portable.zip",
            "installer.msi",
        ] {
            assert_eq!(detect(name).media_type, MediaType::Software, "{name}");
        }
    }

    #[test]
    fn unstructured_names_are_unsorted() {
        for name in [
            "archive.zip",
            "report.pdf",
            "2019.mkv",
            "S01E01.mkv",
            "holiday.mp4",
            "",
        ] {
            let d = detect(name);
            assert_eq!(d.media_type, MediaType::Unsorted, "{name}");
            assert_eq!(d.guess, None, "{name}");
        }
    }

    #[test]
    fn guesses_follow_the_chosen_type() {
        let name = "Cyberpunk.Edgerunners.S02.MULTi.AD.1080p.NF.WEB.x264-BYOR.zip";
        let g = guess_as(name, MediaType::Movie).unwrap();
        assert_eq!(
            (g.kind, g.title.as_str()),
            (MediaType::Movie, "Cyberpunk Edgerunners")
        );
        assert_eq!(
            guess_as("Hades.II-TENOKE.zip", MediaType::Tv)
                .unwrap()
                .title,
            "Hades II"
        );
        assert_eq!(
            guess_as("holiday.mp4", MediaType::Movie).unwrap().title,
            "holiday"
        );
        assert_eq!(
            guess_as("Cool.Game.v1.0.zip", MediaType::Game)
                .unwrap()
                .title,
            "Cool Game"
        );
        assert_eq!(guess_as(name, MediaType::Music), None);
        assert_eq!(guess_as(name, MediaType::Unsorted), None);
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
        assert_eq!(normalize_title("Hades™ II"), "hades ii");
    }
}
