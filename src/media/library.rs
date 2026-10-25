//! Where a completed download belongs in the media library.
//!
//! [`plan`] decides from the file name, the type chosen on the confirmation
//! step, and the title and year found by the metadata lookup. It follows the
//! layout Jellyfin and Plex scan without help:
//!
//! ```text
//! <movies>/Blade Runner 2049 (2017)/Blade Runner 2049 (2017).mkv
//! <tv>/The Office (2005)/Season 03/The Office (2005) - S03E10.mkv
//! ```
//!
//! Like arrangerr, which it is ported from, it is deterministic and
//! conservative: a movie needs a year, an episode needs a season and episode
//! number, and a name that says otherwise (a "movie" named `S01E01`) is
//! [`Placement::Unidentified`] rather than a guess. Unidentified videos go to
//! the fallback folder, if one is set.

use super::parse::{MediaGuess, detect, guess_as};
use crate::domain::MediaType;
use crate::fsutil::sanitize_filename;
use std::path::PathBuf;

/// Extensions of video files a media server plays.
const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "mov", "wmv", "ts", "m2ts", "webm", "mpg", "mpeg", "flv",
];
const ARCHIVE_EXTENSIONS: &[&str] = &["zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "iso"];
/// Longest title kept in a folder or file name, in bytes.
const MAX_TITLE_BYTES: usize = 150;

/// The library folders. Absent folders are not organized into.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryPaths {
    pub movies: Option<PathBuf>,
    pub tv: Option<PathBuf>,
    /// Where videos that cannot be identified go.
    pub unsorted: Option<PathBuf>,
}

impl LibraryPaths {
    /// Whether a movie or TV library is set; without one nothing is moved.
    pub fn any_library(&self) -> bool {
        self.movies.is_some() || self.tv.is_some()
    }
}

/// A title and year from the metadata lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub title: String,
    pub year: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// Move the file to `root/folders.../file_name`.
    Library {
        root: PathBuf,
        folders: Vec<String>,
        file_name: String,
        /// What it was identified as, e.g. `The Office (2005) - S03E10`.
        label: String,
    },
    /// A video that could not be identified with confidence.
    Unidentified { reason: String },
    /// Not organized; the file stays where it is. `note` says why, when the
    /// user may want to know (types without a library need no note).
    Skip { note: Option<String> },
}

/// The movie or TV guess to look metadata up for, when the download has no
/// title yet (it was submitted without the confirmation step).
pub fn lookup_guess(file_name: &str, media_type: Option<MediaType>) -> Option<MediaGuess> {
    if !is_video(file_name) {
        return None;
    }
    guess_as(file_name, video_kind(file_name, media_type).ok()?)
}

/// Where `file_name` goes. `metadata` is the lookup's title for the type the
/// download will be organized as.
pub fn plan(
    file_name: &str,
    media_type: Option<MediaType>,
    metadata: Option<&Metadata>,
    paths: &LibraryPaths,
) -> Placement {
    let skip = |note: &str| Placement::Skip {
        note: Some(note.to_owned()),
    };
    if let Some(t @ (MediaType::Game | MediaType::Music | MediaType::Software)) = media_type {
        tracing::debug!(media_type = t.as_str(), "no library for this type");
        return Placement::Skip { note: None };
    }
    let Some(ext) = extension(file_name) else {
        return skip("not a video file, so it stays in the download folder");
    };
    if !VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        return if is_archive(&ext) {
            skip("this kind of archive is not extracted, so it stays in the download folder")
        } else {
            skip("not a video file, so it stays in the download folder")
        };
    }
    let kind = match video_kind(file_name, media_type) {
        Ok(kind) => kind,
        Err(reason) => return Placement::Unidentified { reason },
    };
    let root = match kind {
        MediaType::Movie => paths.movies.clone(),
        _ => paths.tv.clone(),
    };
    let Some(root) = root else {
        return skip(match kind {
            MediaType::Movie => "no movie library folder is set",
            _ => "no TV library folder is set",
        });
    };
    let parsed = guess_as(file_name, kind);
    let placed = match kind {
        MediaType::Movie => movie(parsed, metadata),
        _ => episode(parsed, metadata),
    };
    match placed {
        Ok((folders, stem, label)) => Placement::Library {
            root,
            folders,
            file_name: format!("{stem}.{ext}"),
            label,
        },
        Err(reason) => Placement::Unidentified {
            reason: reason.to_owned(),
        },
    }
}

/// `Movie` or `Tv`: the chosen type, or for downloads without one, what the
/// name says.
fn video_kind(file_name: &str, media_type: Option<MediaType>) -> Result<MediaType, String> {
    match media_type {
        Some(t @ (MediaType::Movie | MediaType::Tv)) => Ok(t),
        Some(MediaType::Unsorted) => Err("its type is Unsorted".into()),
        Some(t) => Err(format!("{} files are not organized", t.label())),
        None => match detect(file_name).media_type {
            t @ (MediaType::Movie | MediaType::Tv) => Ok(t),
            _ => Err("the name is not that of a movie or an episode".into()),
        },
    }
}

type Placed = (Vec<String>, String, String);

/// `Title (Year)/Title (Year)`.
fn movie(parsed: Option<MediaGuess>, metadata: Option<&Metadata>) -> Result<Placed, &'static str> {
    if parsed.as_ref().is_some_and(|g| g.season.is_some()) {
        return Err("the name looks like a TV episode, not a movie");
    }
    let (title, year) = title_and_year(parsed.as_ref(), metadata).ok_or("no title in the name")?;
    let year = year.ok_or("no release year in the name, and no metadata match")?;
    let name = folder_name(&format!("{title} ({year})")).ok_or("no usable title")?;
    Ok((vec![name.clone()], name.clone(), name))
}

/// `Show (Year)/Season 01/Show (Year) - S01E02`, without the year if it is
/// not known.
fn episode(
    parsed: Option<MediaGuess>,
    metadata: Option<&Metadata>,
) -> Result<Placed, &'static str> {
    let (season, episode) = parsed
        .as_ref()
        .and_then(|g| Some((g.season?, g.episode?)))
        .ok_or("no season and episode number in the name")?;
    let last = parsed.as_ref().and_then(|g| g.last_episode);
    let (title, year) = title_and_year(parsed.as_ref(), metadata).ok_or("no title in the name")?;
    let show = match year {
        Some(year) => format!("{title} ({year})"),
        None => title,
    };
    let show = folder_name(&show).ok_or("no usable title")?;
    let mut code = format!("S{season:02}E{episode:02}");
    if let Some(last) = last {
        code.push_str(&format!("-E{last:02}"));
    }
    let label = format!("{show} - {code}");
    Ok((
        vec![show.clone(), format!("Season {season:02}")],
        label.clone(),
        label,
    ))
}

/// The metadata title, else the parsed one in title case; the metadata year,
/// else the parsed one.
fn title_and_year(
    parsed: Option<&MediaGuess>,
    metadata: Option<&Metadata>,
) -> Option<(String, Option<u16>)> {
    let parsed_year = parsed.and_then(|g| g.year);
    match metadata {
        Some(m) if !m.title.trim().is_empty() => {
            Some((truncate(m.title.trim()), m.year.or(parsed_year)))
        }
        _ => parsed.map(|g| (truncate(&title_case(&g.title)), parsed_year)),
    }
}

/// Upper-case the first letter of each word, as arrangerr does: release
/// names are often all lower case.
fn title_case(title: &str) -> String {
    title
        .split(' ')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate(title: &str) -> String {
    let mut cut = title.len().min(MAX_TITLE_BYTES);
    while !title.is_char_boundary(cut) {
        cut -= 1;
    }
    title[..cut].trim_end().to_owned()
}

/// A title made into one safe folder or file name: `Title: Subtitle` becomes
/// `Title - Subtitle`, and characters that are invalid on common filesystems
/// are dropped.
fn folder_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .replace(": ", " - ")
        .chars()
        .filter_map(|c| match c {
            ':' | '/' | '\\' => Some('-'),
            '*' | '?' | '"' | '<' | '>' | '|' => None,
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    sanitize_filename(&collapsed)
}

fn extension(file_name: &str) -> Option<String> {
    let (stem, ext) = file_name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

pub fn is_video(file_name: &str) -> bool {
    extension(file_name).is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.as_str()))
}

fn is_archive(ext: &str) -> bool {
    let numbered = |rest: &str| rest.len() >= 2 && rest.bytes().all(|b| b.is_ascii_digit());
    ARCHIVE_EXTENSIONS.contains(&ext)
        || numbered(ext)
        || ext.strip_prefix('r').is_some_and(numbered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> LibraryPaths {
        LibraryPaths {
            movies: Some("/media/movies".into()),
            tv: Some("/media/tv".into()),
            unsorted: Some("/media/unsorted".into()),
        }
    }

    /// The destination relative to the library root, or the placement.
    fn place(
        name: &str,
        media_type: Option<MediaType>,
        metadata: Option<(&str, Option<u16>)>,
    ) -> Result<String, Placement> {
        let metadata = metadata.map(|(title, year)| Metadata {
            title: title.into(),
            year,
        });
        match plan(name, media_type, metadata.as_ref(), &paths()) {
            Placement::Library {
                root,
                folders,
                file_name,
                ..
            } => {
                let mut path = root;
                path.extend(&folders);
                path.push(file_name);
                Ok(path.to_string_lossy().into_owned())
            }
            other => Err(other),
        }
    }

    fn unidentified(result: Result<String, Placement>) -> String {
        match result {
            Err(Placement::Unidentified { reason }) => reason,
            other => panic!("expected unidentified, got {other:?}"),
        }
    }

    #[test]
    fn movies_use_title_and_year() {
        let movie = Some(MediaType::Movie);
        assert_eq!(
            place("Blade.Runner.2049.2017.1080p.BluRay.x264.mkv", movie, None).unwrap(),
            "/media/movies/Blade Runner 2049 (2017)/Blade Runner 2049 (2017).mkv"
        );
        assert_eq!(
            place("the.matrix.1999.720p.MKV", movie, None).unwrap(),
            "/media/movies/The Matrix (1999)/The Matrix (1999).mkv",
            "title case and lower-case extension, as arrangerr"
        );
        // The metadata title and year win over the file name.
        assert_eq!(
            place(
                "Mission.Impossible.Dead.Reckoning.2023.mkv",
                movie,
                Some(("Mission: Impossible - Dead Reckoning Part One", Some(2023)))
            )
            .unwrap(),
            "/media/movies/Mission - Impossible - Dead Reckoning Part One (2023)/\
             Mission - Impossible - Dead Reckoning Part One (2023).mkv"
        );
        assert_eq!(
            place(
                "Inception.1080p.BluRay.mkv",
                movie,
                Some(("Inception", Some(2010)))
            )
            .unwrap(),
            "/media/movies/Inception (2010)/Inception (2010).mkv",
            "the year can come from the metadata only"
        );
    }

    #[test]
    fn episodes_use_show_season_and_episode() {
        let tv = Some(MediaType::Tv);
        assert_eq!(
            place("The.Office.US.S03E10.720p.HDTV.mkv", tv, None).unwrap(),
            "/media/tv/The Office US/Season 03/The Office US - S03E10.mkv"
        );
        assert_eq!(
            place("Doctor.Who.2005.S01E01.Rose.mkv", tv, None).unwrap(),
            "/media/tv/Doctor Who (2005)/Season 01/Doctor Who (2005) - S01E01.mkv"
        );
        assert_eq!(
            place(
                "Cyberpunk.Edgerunners.S01E02.1080p.NF.WEB.mkv",
                tv,
                Some(("Cyberpunk: Edgerunners", Some(2022)))
            )
            .unwrap(),
            "/media/tv/Cyberpunk - Edgerunners (2022)/Season 01/\
             Cyberpunk - Edgerunners (2022) - S01E02.mkv"
        );
        assert_eq!(
            place("show_name_2x05_hdtv.avi", tv, None).unwrap(),
            "/media/tv/Show Name/Season 02/Show Name - S02E05.avi"
        );
        assert_eq!(
            place("Show.S01E01E02.mkv", tv, None).unwrap(),
            "/media/tv/Show/Season 01/Show - S01E01-E02.mkv"
        );
    }

    #[test]
    fn untyped_downloads_go_by_their_name() {
        assert_eq!(
            place("Some.Show.S02E03.mkv", None, None).unwrap(),
            "/media/tv/Some Show/Season 02/Some Show - S02E03.mkv"
        );
        assert_eq!(
            place("Some.Movie.2019.mkv", None, None).unwrap(),
            "/media/movies/Some Movie (2019)/Some Movie (2019).mkv"
        );
        unidentified(place("holiday.mp4", None, None));
    }

    #[test]
    fn ambiguous_names_are_not_guessed() {
        let movie = Some(MediaType::Movie);
        let tv = Some(MediaType::Tv);
        assert!(unidentified(place("Inception.1080p.BluRay.mkv", movie, None)).contains("year"));
        assert!(unidentified(place("Show.S01E01.mkv", movie, None)).contains("TV episode"));
        assert!(unidentified(place("Some.Movie.2019.mkv", tv, None)).contains("season"));
        assert!(unidentified(place("Show.S02.Complete.mkv", tv, None)).contains("episode"));
        unidentified(place(
            "Some.Movie.2019.mkv",
            Some(MediaType::Unsorted),
            None,
        ));
        unidentified(place("holiday.mp4", movie, None));
    }

    #[test]
    fn other_files_stay_in_the_download_folder() {
        let skip_note = |name: &str, t: Option<MediaType>| match place(name, t, None) {
            Err(Placement::Skip { note }) => note,
            other => panic!("expected a skip for {name}, got {other:?}"),
        };
        for t in [MediaType::Game, MediaType::Music, MediaType::Software] {
            assert_eq!(skip_note("Some.Movie.2019.mkv", Some(t)), None);
        }
        for archive in [
            "Show.S02.1080p.zip",
            "Movie.2019.part01.rar",
            "Movie.2019.r00",
            "Movie.2019.mkv.001",
        ] {
            let note = skip_note(archive, Some(MediaType::Tv)).unwrap();
            assert!(note.contains("archive"), "{archive}: {note}");
        }
        assert!(
            skip_note("Show.S01E01.srt", Some(MediaType::Tv))
                .unwrap()
                .contains("not a video")
        );
        assert!(
            skip_note("noextension", None)
                .unwrap()
                .contains("not a video")
        );
    }

    #[test]
    fn missing_library_folders_skip() {
        let movies_only = LibraryPaths {
            tv: None,
            ..paths()
        };
        assert!(movies_only.any_library());
        let placement = plan("Show.S01E01.mkv", Some(MediaType::Tv), None, &movies_only);
        assert_eq!(
            placement,
            Placement::Skip {
                note: Some("no TV library folder is set".into())
            }
        );
        assert!(!LibraryPaths::default().any_library());
    }

    #[test]
    fn hostile_titles_stay_single_components() {
        let movie = Some(MediaType::Movie);
        for title in ["../../etc", "a/b\\c", "..", "Title\u{202e}vkm", "x\0y"] {
            match plan(
                "file.2019.mkv",
                movie,
                Some(&Metadata {
                    title: title.into(),
                    year: Some(2019),
                }),
                &paths(),
            ) {
                Placement::Library {
                    folders, file_name, ..
                } => {
                    for part in folders.iter().chain([&file_name]) {
                        assert_eq!(
                            std::path::Path::new(part).components().count(),
                            1,
                            "{title:?} -> {part:?}"
                        );
                        assert!(!part.starts_with('.'), "{title:?} -> {part:?}");
                        assert!(!part.contains('\u{202e}') && !part.contains('\0'));
                    }
                }
                Placement::Unidentified { .. } => {}
                Placement::Skip { .. } => panic!("{title:?} skipped"),
            }
        }
        let long = "Long ".repeat(100);
        match plan(
            "file.2019.mkv",
            movie,
            Some(&Metadata {
                title: long,
                year: Some(2019),
            }),
            &paths(),
        ) {
            Placement::Library { file_name, .. } => assert!(file_name.len() < 200),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn lookup_guesses_only_for_videos() {
        let g = lookup_guess("Some.Show.S02E03.mkv", None).unwrap();
        assert_eq!((g.kind, g.title.as_str()), (MediaType::Tv, "Some Show"));
        assert_eq!(
            lookup_guess("Some.Movie.2019.mkv", Some(MediaType::Movie))
                .unwrap()
                .year,
            Some(2019)
        );
        assert_eq!(
            lookup_guess("Some.Movie.2019.zip", Some(MediaType::Movie)),
            None
        );
        assert_eq!(
            lookup_guess("Some.Game-CODEX.mkv", Some(MediaType::Game)),
            None
        );
    }
}
