//! Media organization: moves completed downloads into the media library.
//!
//! One task works through completed downloads whose post-processing is
//! `pending`, oldest first and one at a time, so moves never compete for the
//! disk. The engine wakes it when a transfer completes. For each download it:
//!
//! 1. claims it (`pending -> processing`, compare-and-swap);
//! 2. plans the destination with [`library::plan`], using the title and year
//!    from the confirmation step, or a metadata lookup for downloads
//!    submitted without one (an outage only means the parsed title is used);
//! 3. records the destination, then moves the file there without ever
//!    replacing an existing file;
//! 4. records the outcome: `completed` with the new path, `skipped` (no
//!    library for it), or `failed` with the reason. The file stays in the
//!    download folder unless the move succeeded.
//!
//! Archives (`.zip`, `.rar`, `.7z`, split volumes) of movies and series are
//! extracted with 7-Zip into a private folder next to them, and each video
//! inside is organized as above. The archive itself stays in the download
//! folder. A later volume of a split archive only wakes the download of the
//! first volume, which extracts the whole set once every part is there.
//!
//! The transfer state is never changed. A restart during a move puts the
//! download back to `pending`; the recorded destination lets the next run
//! recognize a move that finished just before. A restart during extraction
//! redoes it; videos an earlier attempt already placed are recognized in the
//! library (same name and size) and not counted as collisions.

use super::ArtworkService;
use super::Lookup;
use super::archive::{self, ArchiveError, Role, SevenZip};
use super::library::{self, LibraryPaths, Metadata, Placement};
use super::parse;
use crate::domain::{Download, DownloadError, MediaType, PostState};
use crate::error::ErrorKind;
use crate::fsutil;
use crate::settings::{FlagCell, LibraryCell};
use crate::store::{PostDetails, Store};
use chrono::Utc;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// How often the queue is checked without a wake-up.
const IDLE_POLL: Duration = Duration::from_secs(60);
/// Pause after a database error, so a failing database is not hammered.
const ERROR_PAUSE: Duration = Duration::from_secs(5);
/// How long organization waits for a metadata lookup.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(20);
/// How long shutdown waits for a move in progress.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);
/// Attempts at a free name in the fallback folder.
const MAX_NAME_RACES: usize = 5;
/// Free space left over after extracting an archive.
const EXTRACT_SPACE_MARGIN: u64 = 256 * 1024 * 1024;
/// Deepest folder searched for videos in an extracted archive.
const MAX_EXTRACT_DEPTH: usize = 8;
/// Most organized names listed in an event message.
const MAX_LISTED: usize = 30;

#[derive(Clone)]
pub struct Organizer {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    library: LibraryCell,
    artwork: ArtworkService,
    /// Extracts archives; without it they stay in the download folder.
    sevenzip: Option<SevenZip>,
    /// Delete an archive once all its videos are placed.
    delete_archives: FlagCell,
    wake: Arc<Notify>,
    shutdown: CancellationToken,
}

/// What became of a download.
#[derive(Debug)]
enum Outcome {
    Organized {
        path: PathBuf,
        label: String,
    },
    /// Not identified; moved to the fallback folder.
    Fallback {
        path: PathBuf,
        reason: String,
    },
    /// An archive was extracted and its videos organized.
    Extracted {
        /// What each video was organized as.
        organized: Vec<String>,
        /// Where they are now, in the library.
        placed: Vec<PathBuf>,
        /// Videos moved to the fallback folder.
        fallback: Vec<PathBuf>,
        /// The archive's files (all volumes) to delete once this is
        /// recorded; empty when archives are kept.
        delete: Vec<PathBuf>,
    },
    Skipped {
        note: Option<String>,
    },
    Failed {
        kind: ErrorKind,
        message: String,
    },
    /// Shutdown stopped it; it stays `processing` and is redone on restart.
    Interrupted,
}

impl Outcome {
    fn failed(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::Failed {
            kind,
            message: message.into(),
        }
    }

    fn transition(self) -> (PostState, PostDetails) {
        let note = |kind, message: String| {
            Some(DownloadError {
                kind,
                message: capitalize(&message),
            })
        };
        match self {
            Self::Organized { path, label } => (
                PostState::Completed,
                PostDetails {
                    message: Some(format!("organized as {label}: {}", path.display())),
                    file_path: Some(path.to_string_lossy().into_owned()),
                    error: None,
                },
            ),
            Self::Fallback { path, reason } => (
                PostState::Completed,
                PostDetails {
                    message: Some(format!(
                        "not identified ({reason}); moved to {}",
                        path.display()
                    )),
                    error: note(
                        ErrorKind::MediaIdentification,
                        format!("not identified ({reason}); moved to the fallback folder"),
                    ),
                    file_path: Some(path.to_string_lossy().into_owned()),
                },
            ),
            Self::Extracted {
                organized,
                placed,
                fallback,
                delete,
            } => {
                let fallback_count = fallback.len();
                let mut message = format!(
                    "extracted {} from the archive",
                    count(organized.len() + fallback_count, "video")
                );
                if !organized.is_empty() {
                    message.push_str(&format!("; organized as {}", list(&organized)));
                }
                if fallback_count > 0 {
                    message.push_str(&format!(
                        "; {} not identified, moved to the fallback folder",
                        count(fallback_count, "video")
                    ));
                }
                // Once the archive is deleted, the download's file is where
                // its videos went; otherwise it is still the archive.
                let file_path = if delete.is_empty() {
                    None
                } else {
                    message.push_str(&format!(
                        "; deleting the archive ({})",
                        count(delete.len(), "file")
                    ));
                    location(if placed.is_empty() {
                        &fallback
                    } else {
                        &placed
                    })
                    .map(|p| p.to_string_lossy().into_owned())
                };
                (
                    PostState::Completed,
                    PostDetails {
                        message: Some(message),
                        error: (fallback_count > 0)
                            .then(|| {
                                note(
                                    ErrorKind::MediaIdentification,
                                    format!(
                                        "{} from the archive not identified; moved to the \
                                         fallback folder",
                                        count(fallback_count, "video")
                                    ),
                                )
                            })
                            .flatten(),
                        file_path,
                    },
                )
            }
            Self::Skipped { note: reason } => (
                PostState::Skipped,
                PostDetails {
                    message: Some(match &reason {
                        Some(r) => format!("not organized: {r}"),
                        None => "not organized: no library for this type".into(),
                    }),
                    error: reason.and_then(|r| note(ErrorKind::MediaIdentification, r)),
                    file_path: None,
                },
            ),
            Self::Failed { kind, message } => (
                PostState::Failed,
                PostDetails {
                    error: note(kind, message),
                    ..Default::default()
                },
            ),
            Self::Interrupted => unreachable!("an interrupted download is not recorded"),
        }
    }
}

/// `1 video`, `3 videos`.
fn count(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// `a, b, c`, shortened when long.
fn list(items: &[String]) -> String {
    let mut out = items[..items.len().min(MAX_LISTED)].join(", ");
    if items.len() > MAX_LISTED {
        out.push_str(&format!(" and {} more", items.len() - MAX_LISTED));
    }
    out
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

impl Organizer {
    /// `wake` is notified when there may be work: [`Engine::completions`].
    /// Without `sevenzip`, archives are not extracted; with
    /// `delete_archives` on, they are deleted once extracted.
    ///
    /// [`Engine::completions`]: crate::engine::Engine::completions
    pub fn new(
        store: Store,
        library: LibraryCell,
        artwork: ArtworkService,
        sevenzip: Option<SevenZip>,
        delete_archives: FlagCell,
        wake: Arc<Notify>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                library,
                artwork,
                sevenzip,
                delete_archives,
                wake,
                shutdown: CancellationToken::new(),
            }),
        }
    }

    pub fn start(&self) -> JoinHandle<()> {
        tokio::spawn(run(self.inner.clone()))
    }

    /// Stop after the download being organized, waiting a while for it. A
    /// move still running then is finished or redone after the restart.
    pub async fn shutdown(&self, handle: JoinHandle<()>) {
        self.inner.shutdown.cancel();
        match tokio::time::timeout(SHUTDOWN_GRACE, handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = %e, "media organizer terminated abnormally"),
            Err(_) => {
                tracing::warn!("media organization did not stop in time; it resumes on restart")
            }
        }
    }

    /// Organize the oldest pending download. Returns `false` when none is
    /// pending.
    #[cfg(test)]
    async fn organize_next(&self) -> bool {
        match self.inner.store.next_post_pending().await.unwrap() {
            Some(id) => {
                self.inner.organize(id).await.unwrap();
                true
            }
            None => false,
        }
    }
}

async fn run(inner: Arc<Inner>) {
    tracing::info!("media organizer started");
    loop {
        if inner.shutdown.is_cancelled() {
            break;
        }
        let pause = match inner.store.next_post_pending().await {
            Ok(Some(id)) => match inner.organize(id).await {
                Ok(()) => continue,
                Err(()) => ERROR_PAUSE,
            },
            Ok(None) => IDLE_POLL,
            Err(e) => {
                tracing::error!(error = %e, "failed to read the organization queue");
                ERROR_PAUSE
            }
        };
        tokio::select! {
            () = inner.shutdown.cancelled() => break,
            () = inner.wake.notified() => {}
            () = tokio::time::sleep(pause) => {}
        }
    }
}

impl Inner {
    /// `Err` means the database failed; the caller pauses before retrying.
    async fn organize(&self, id: Uuid) -> Result<(), ()> {
        let download = match self
            .store
            .transition_post(
                id,
                PostState::Pending,
                PostState::Processing,
                None,
                Utc::now(),
            )
            .await
        {
            Ok(d) => d,
            // Deleted or changed in the meantime: nothing to do.
            Err(e) if matches!(e.kind, ErrorKind::Conflict | ErrorKind::NotFound) => return Ok(()),
            Err(e) => {
                tracing::error!(%id, error = %e, "could not start organizing");
                return Err(());
            }
        };
        let outcome = self.place(&download).await;
        match &outcome {
            Outcome::Interrupted => {
                tracing::info!(%id, "organizing interrupted by shutdown; it is redone on restart");
                return Ok(());
            }
            Outcome::Extracted {
                organized,
                fallback,
                ..
            } => {
                tracing::info!(%id, organized = organized.len(), fallback = fallback.len(), "archive extracted and organized");
            }
            Outcome::Organized { path, label } => {
                tracing::info!(%id, %label, path = %path.display(), "download organized");
            }
            Outcome::Fallback { path, reason } => {
                tracing::info!(%id, %reason, path = %path.display(), "download not identified; moved to the fallback folder");
            }
            Outcome::Skipped { note } => tracing::info!(%id, ?note, "download not organized"),
            Outcome::Failed { message, .. } => {
                tracing::warn!(%id, error = %message, "organizing failed");
            }
        }
        let delete = match &outcome {
            Outcome::Extracted { delete, .. } => delete.clone(),
            _ => Vec::new(),
        };
        let (to, details) = outcome.transition();
        if let Err(e) = self
            .store
            .transition_post_with(id, PostState::Processing, to, details, Utc::now())
            .await
        {
            // Restart recovery puts it back to `pending`; the recorded
            // destination shows whether the move happened.
            tracing::error!(%id, error = %e, "failed to record the organization result");
            return Err(());
        }
        // Only after the result is recorded: a restart before this keeps the
        // archive, never loses track of it.
        if !delete.is_empty() {
            delete_archive(id, delete).await;
        }
        Ok(())
    }

    async fn place(&self, d: &Download) -> Outcome {
        let Some(source) = d.file_path.as_deref().map(PathBuf::from) else {
            return Outcome::failed(
                ErrorKind::Filesystem,
                "the downloaded file's location is unknown",
            );
        };
        if let Some(target) = d.post_target.as_deref().map(PathBuf::from) {
            let (src, dst, total) = (source.clone(), target.clone(), d.total_bytes);
            if blocking(move || Ok(moved_before_restart(&src, &dst, total)))
                .await
                .unwrap_or(false)
            {
                return Outcome::Organized {
                    path: target,
                    label: "the move that finished before a restart".into(),
                };
            }
        }
        let src = source.clone();
        match blocking(move || fs_kind(&src)).await {
            Ok(Some(kind)) if kind.is_file() => {}
            Ok(_) => {
                return Outcome::failed(
                    ErrorKind::Filesystem,
                    format!(
                        "the downloaded file is missing or not a regular file: {}",
                        source.display()
                    ),
                );
            }
            Err(e) => {
                return Outcome::failed(
                    ErrorKind::Filesystem,
                    format!("cannot read the downloaded file: {e}"),
                );
            }
        }
        let paths = self.library.get();
        if !paths.any_library() {
            return Outcome::Skipped {
                note: Some(
                    "no media library folders are set (DIREKT_MOVIES_DIR, DIREKT_TV_DIR)".into(),
                ),
            };
        }
        let Some(file_name) = source.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return Outcome::failed(ErrorKind::Filesystem, "the downloaded file has no name");
        };
        let typed_without_library = matches!(
            d.media_type,
            Some(MediaType::Game | MediaType::Music | MediaType::Software)
        );
        if !typed_without_library && let Some(role) = archive::role(&file_name) {
            return self
                .place_archive(d, source, &file_name, role, &paths)
                .await;
        }
        let metadata = self.metadata(d, d.media_type, &file_name).await;
        match library::plan(&file_name, d.media_type, metadata.as_ref(), &paths) {
            Placement::Library {
                root,
                folders,
                file_name,
                label,
            } => {
                self.move_to_library(d.id, source, root, folders, file_name, label)
                    .await
            }
            Placement::Unidentified { reason } => {
                self.move_to_fallback(d.id, source, &paths, &file_name, reason)
                    .await
            }
            Placement::Skip { note } => Outcome::Skipped { note },
        }
    }

    /// The title and year to name the file by: from the confirmation step,
    /// or looked up now for downloads submitted without it.
    async fn metadata(
        &self,
        d: &Download,
        media_type: Option<MediaType>,
        file_name: &str,
    ) -> Option<Metadata> {
        if let (Some(title), Some(MediaType::Movie | MediaType::Tv)) = (&d.title, d.media_type) {
            return Some(Metadata {
                title: title.clone(),
                year: d.year,
            });
        }
        let guess = library::lookup_guess(file_name, media_type)?;
        // A title found for another type (e.g. Steam, for a game) says
        // nothing here; only movies and series are looked up.
        match tokio::time::timeout(LOOKUP_TIMEOUT, self.artwork.find(&guess)).await {
            Ok(Lookup::Found(found)) => Some(Metadata {
                title: found.title,
                year: found.year,
            }),
            Ok(Lookup::Unavailable(reason)) => {
                tracing::warn!(id = %d.id, %reason, "metadata lookup failed; using the file name");
                None
            }
            Ok(Lookup::NotFound | Lookup::NeedsKey) => None,
            Err(_) => {
                tracing::warn!(id = %d.id, "metadata lookup timed out; using the file name");
                None
            }
        }
    }

    /// Extract the archive `source` and organize the videos in it.
    async fn place_archive(
        &self,
        d: &Download,
        source: PathBuf,
        file_name: &str,
        role: Role,
        paths: &LibraryPaths,
    ) -> Outcome {
        if let Role::Volume { first } = role {
            self.wake_first_volume(d.id, &source.with_file_name(&first))
                .await;
            return Outcome::Skipped {
                note: Some(format!(
                    "part of the archive {first}, which is extracted with that download"
                )),
            };
        }
        // Downloads submitted without a type: what the archive's name says.
        let media_type = d
            .media_type
            .unwrap_or_else(|| parse::detect(file_name).media_type);
        let missing_library = match media_type {
            MediaType::Movie if paths.movies.is_none() => Some("no movie library folder is set"),
            MediaType::Tv if paths.tv.is_none() => Some("no TV library folder is set"),
            MediaType::Movie | MediaType::Tv | MediaType::Unsorted => None,
            // A game or program is used as it is: never unpacked into a library.
            _ => return Outcome::Skipped { note: None },
        };
        if let Some(note) = missing_library {
            return Outcome::Skipped {
                note: Some(note.into()),
            };
        }
        let Some(sevenzip) = &self.sevenzip else {
            return Outcome::Skipped {
                note: Some(
                    "archives are not extracted because 7-Zip (the 7z command) is not \
                     installed, so it stays in the download folder"
                        .into(),
                ),
            };
        };
        let entries = match sevenzip.list(&source, &self.shutdown).await {
            Ok(entries) => entries,
            Err(e) => return archive_failed(e),
        };
        let has_video = entries.iter().any(|e| {
            !e.is_dir
                && library::is_video(e.path.rsplit(['/', '\\']).next().unwrap_or_default())
                && !archive::is_sample(&e.path)
        });
        if !has_video {
            return Outcome::Skipped {
                note: Some(
                    "the archive holds no videos, so it stays in the download folder".into(),
                ),
            };
        }
        let Some(folder) = source.parent().map(Path::to_path_buf) else {
            return Outcome::failed(ErrorKind::Filesystem, "the archive has no folder");
        };
        let needed: u64 = entries.iter().map(|e| e.size).sum();
        let space_folder = folder.clone();
        match blocking(move || fsutil::available_space(&space_folder)).await {
            Ok(free) if free < needed.saturating_add(EXTRACT_SPACE_MARGIN) => {
                return Outcome::failed(
                    ErrorKind::InsufficientDiskSpace,
                    format!(
                        "extracting the archive needs {} but only {} is free in {}",
                        gib(needed),
                        gib(free),
                        folder.display()
                    ),
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(id = %d.id, error = %e, "could not check free space"),
        }
        // Next to the archive, so moving out of it into a library on the same
        // filesystem is a rename.
        let staging = folder.join(format!(".direkt-extract-{}", d.id));
        let fresh = staging.clone();
        if let Err(e) = blocking(move || fresh_dir(&fresh)).await {
            return Outcome::failed(
                ErrorKind::Filesystem,
                format!(
                    "could not create the extraction folder {}: {e}",
                    staging.display()
                ),
            );
        }
        tracing::info!(id = %d.id, entries = entries.len(), bytes = needed, "extracting archive");
        let mut outcome = match sevenzip.extract(&source, &staging, &self.shutdown).await {
            Ok(()) => {
                self.organize_extracted(d, &source, &staging, Some(media_type), paths)
                    .await
            }
            Err(e) => archive_failed(e),
        };
        // An interrupted extraction is cleared by the next attempt; killing
        // 7-Zip may not have finished yet.
        if !matches!(outcome, Outcome::Interrupted) {
            let leftover = staging.clone();
            if let Err(e) = blocking(move || remove_dir(&leftover)).await {
                tracing::warn!(
                    id = %d.id,
                    path = %staging.display(),
                    error = %e,
                    "could not remove the extraction folder"
                );
            }
        }
        if let Outcome::Extracted { delete, .. } = &mut outcome
            && self.delete_archives.get()
        {
            let archive = source.clone();
            match blocking(move || archive_volumes(&archive)).await {
                Ok(files) => *delete = files,
                Err(e) => tracing::warn!(
                    id = %d.id,
                    error = %e,
                    "could not list the archive's parts; it is kept"
                ),
            }
        }
        outcome
    }

    /// Organize the videos extracted into `staging`, one by one.
    async fn organize_extracted(
        &self,
        d: &Download,
        archive: &Path,
        staging: &Path,
        media_type: Option<MediaType>,
        paths: &LibraryPaths,
    ) -> Outcome {
        let (root, like) = (staging.to_path_buf(), archive.to_path_buf());
        let videos = match blocking(move || find_videos(&root, &like)).await {
            Ok(videos) => videos,
            Err(e) => {
                return Outcome::failed(
                    ErrorKind::Filesystem,
                    format!("could not read the extracted files: {e}"),
                );
            }
        };
        if videos.is_empty() {
            return Outcome::Skipped {
                note: Some(
                    "the archive holds no videos, so it stays in the download folder".into(),
                ),
            };
        }
        let total = videos.len();
        // Episodes in a season pack may be named `S01E03.mkv`: their show is
        // then the one the archive is named after.
        let show = archive
            .file_name()
            .and_then(|n| parse::guess_as(&n.to_string_lossy(), MediaType::Tv))
            .map(|g| g.title)
            .filter(|_| media_type == Some(MediaType::Tv));
        let (mut organized, mut placed, mut fallback) = (Vec::new(), Vec::new(), Vec::new());
        let mut failures: Vec<(ErrorKind, String)> = Vec::new();
        let mut skipped = None;
        // Lookups by the name they were made for: episodes of one show share
        // a title, so one lookup serves them all.
        let mut lookups: Vec<(String, Option<Metadata>)> = Vec::new();
        for video in videos {
            if self.shutdown.is_cancelled() {
                return Outcome::Interrupted;
            }
            let name = video
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let key = library::lookup_guess(&name, media_type).map(|g| g.title.to_lowercase());
            let metadata = match lookups.iter().find(|(k, _)| Some(k) == key.as_ref()) {
                Some((_, found)) => found.clone(),
                None => {
                    let found = self.metadata(d, media_type, &name).await;
                    if let Some(key) = key {
                        lookups.push((key, found.clone()));
                    }
                    found
                }
            };
            let mut placement = library::plan(&name, media_type, metadata.as_ref(), paths);
            if let (Placement::Unidentified { .. }, Some(show)) = (&placement, &show) {
                let named = format!("{show}.{name}");
                let retry = library::plan(&named, media_type, metadata.as_ref(), paths);
                if matches!(retry, Placement::Library { .. }) {
                    placement = retry;
                }
            }
            match placement {
                Placement::Library {
                    root,
                    folders,
                    file_name,
                    label,
                } => {
                    let dir = match library_dir(root, folders).await {
                        Ok(dir) => dir,
                        Err(message) => {
                            failures.push((ErrorKind::Filesystem, message));
                            continue;
                        }
                    };
                    let dest = dir.join(&file_name);
                    let (from, to) = (video.clone(), dest.clone());
                    match blocking(move || move_extracted(&from, &to)).await {
                        Ok(()) => {
                            organized.push(label);
                            placed.push(dest);
                        }
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => failures.push((
                            ErrorKind::Filesystem,
                            format!(
                                "{} already exists in the library; Direkt never replaces files",
                                dest.display()
                            ),
                        )),
                        Err(e) => failures.push((ErrorKind::Filesystem, move_error(&e))),
                    }
                }
                Placement::Unidentified { reason } => {
                    let Some(dir) = paths.unsorted.clone() else {
                        failures.push((
                            ErrorKind::MediaIdentification,
                            format!(
                                "{name} was not identified ({reason}); set a fallback folder \
                                 with DIREKT_UNSORTED_DIR, or extract it yourself"
                            ),
                        ));
                        continue;
                    };
                    let from = video.clone();
                    match blocking(move || move_to_unsorted(&from, &dir)).await {
                        Ok(path) => fallback.push(path),
                        Err(e) => failures.push((
                            ErrorKind::Filesystem,
                            format!(
                                "{name} was not identified ({reason}), and could not be moved to \
                                 the fallback folder: {e}"
                            ),
                        )),
                    }
                }
                Placement::Skip { note } => skipped = note.or(skipped),
            }
        }
        if let Some((kind, first)) = failures.first().cloned() {
            let done = organized.len() + fallback.len();
            let more = match failures.len() {
                1 => String::new(),
                n => format!(" (and {} more)", n - 1),
            };
            let message = if done == 0 {
                format!(
                    "none of the {} from the archive could be organized: {first}{more}",
                    count(total, "video")
                )
            } else {
                format!(
                    "{} of {total} videos from the archive could not be organized: {first}{more}. \
                     The others are in the library; Organize again retries the rest",
                    failures.len()
                )
            };
            return Outcome::failed(kind, message);
        }
        if organized.is_empty() && fallback.is_empty() {
            return Outcome::Skipped { note: skipped };
        }
        Outcome::Extracted {
            organized,
            placed,
            fallback,
            delete: Vec::new(),
        }
    }

    /// A later volume of a split archive finished: if the first volume's
    /// download waits for the others, queue it again so it extracts.
    async fn wake_first_volume(&self, id: Uuid, first: &Path) {
        let waiting = match self.store.skipped_with_file(&first.to_string_lossy()).await {
            Ok(Some(waiting)) if waiting != id => waiting,
            Ok(_) => return,
            Err(e) => {
                tracing::warn!(%id, error = %e, "could not look up the archive's first part");
                return;
            }
        };
        match self
            .store
            .transition_post(
                waiting,
                PostState::Skipped,
                PostState::Pending,
                None,
                Utc::now(),
            )
            .await
        {
            Ok(_) => {
                tracing::info!(%id, first = %waiting, "archive part done; extracting the archive again")
            }
            Err(e) => tracing::warn!(%id, error = %e, "could not queue the archive's first part"),
        }
    }

    async fn move_to_library(
        &self,
        id: Uuid,
        source: PathBuf,
        root: PathBuf,
        folders: Vec<String>,
        file_name: String,
        label: String,
    ) -> Outcome {
        let dir = match library_dir(root, folders).await {
            Ok(dir) => dir,
            Err(message) => return Outcome::failed(ErrorKind::Filesystem, message),
        };
        let dest = dir.join(&file_name);
        match self.move_to(id, source.clone(), dest.clone()).await {
            Ok(()) => Outcome::Organized { path: dest, label },
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Outcome::failed(
                ErrorKind::Filesystem,
                format!(
                    "{} already exists in the library; Direkt never replaces files, so this \
                     download stays in {}",
                    dest.display(),
                    source.display()
                ),
            ),
            Err(e) => move_failed(&e),
        }
    }

    async fn move_to_fallback(
        &self,
        id: Uuid,
        source: PathBuf,
        paths: &LibraryPaths,
        file_name: &str,
        reason: String,
    ) -> Outcome {
        let Some(dir) = paths.unsorted.clone() else {
            return Outcome::failed(
                ErrorKind::MediaIdentification,
                format!(
                    "not identified ({reason}). Move the file yourself, or set a fallback \
                     folder with DIREKT_UNSORTED_DIR"
                ),
            );
        };
        for _ in 0..MAX_NAME_RACES {
            let (folder, name) = (dir.clone(), file_name.to_owned());
            let dest = match blocking(move || {
                if !std::fs::metadata(&folder)?.is_dir() {
                    return Err(io::Error::from(io::ErrorKind::NotFound));
                }
                fsutil::unique_destination(&folder, &name)
            })
            .await
            {
                Ok(dest) => dest,
                Err(e) => {
                    return Outcome::failed(
                        ErrorKind::Filesystem,
                        format!(
                            "not identified ({reason}), and the fallback folder {} cannot be \
                             used: {e}",
                            dir.display()
                        ),
                    );
                }
            };
            match self.move_to(id, source.clone(), dest.clone()).await {
                Ok(()) => return Outcome::Fallback { path: dest, reason },
                // Someone took the name in the meantime; pick another.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return move_failed(&e),
            }
        }
        Outcome::failed(
            ErrorKind::Filesystem,
            "could not find a free file name in the fallback folder",
        )
    }

    /// Record `dest`, then move the file there without replacing anything.
    async fn move_to(&self, id: Uuid, source: PathBuf, dest: PathBuf) -> io::Result<()> {
        match self
            .store
            .set_post_target(id, &dest.to_string_lossy(), Utc::now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return Err(io::Error::other("the download changed while organizing")),
            Err(e) => {
                tracing::error!(%id, error = %e, "could not record the organization target");
                return Err(io::Error::other("the database is unavailable"));
            }
        }
        blocking(move || fsutil::move_no_clobber(&source, &dest)).await
    }
}

/// The library folder for `folders` inside `root`, created as needed. The
/// root itself must exist.
async fn library_dir(root: PathBuf, folders: Vec<String>) -> Result<PathBuf, String> {
    let which = if folders.len() > 1 { "TV" } else { "movie" };
    let root_check = root.clone();
    match blocking(move || Ok(std::fs::metadata(&root_check)?.is_dir())).await {
        Ok(true) => {}
        _ => {
            return Err(format!(
                "the {which} library folder {} is missing; check the path (and the volume \
                 mounts, in Docker)",
                root.display()
            ));
        }
    }
    blocking(move || {
        let parts: Vec<&str> = folders.iter().map(String::as_str).collect();
        fsutil::create_dirs_in(&root, &parts)
    })
    .await
    .map_err(|e| format!("could not create the library folder: {e}"))
}

fn move_failed(e: &io::Error) -> Outcome {
    Outcome::failed(ErrorKind::Filesystem, move_error(e))
}

fn move_error(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => {
            "the downloaded file disappeared before it could be moved".into()
        }
        io::ErrorKind::PermissionDenied => format!(
            "Direkt is not allowed to write to the library folder ({e}); check its owner and \
             permissions"
        ),
        _ => format!("could not move the file: {e}"),
    }
}

fn archive_failed(e: ArchiveError) -> Outcome {
    let kind = match &e {
        ArchiveError::Cancelled => return Outcome::Interrupted,
        ArchiveError::MissingVolume => {
            return Outcome::Skipped {
                note: Some(
                    "waiting for the other parts of the archive; it is extracted when they \
                     have downloaded"
                        .into(),
                ),
            };
        }
        ArchiveError::Io(_) => ErrorKind::Filesystem,
        _ => ErrorKind::MediaIdentification,
    };
    let advice = match &e {
        ArchiveError::Encrypted => "; Direkt does not extract password-protected archives",
        ArchiveError::Unsafe(_) | ArchiveError::TooManyEntries => "; it was not extracted",
        _ => "",
    };
    Outcome::failed(kind, format!("{e}{advice}"))
}

/// `1.4 GiB`.
fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// Make `dir` an empty private folder, removing what an interrupted attempt
/// left there.
fn fresh_dir(dir: &Path) -> io::Result<()> {
    remove_dir(dir)?;
    fs::create_dir(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Remove `dir` and its contents, never following a symlink.
fn remove_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(dir),
        Ok(_) => fs::remove_file(dir),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// The videos in an extracted archive, by path, without samples. Only
/// regular files count: links are never followed. Each gets the permissions
/// of `archive` (which the download created), so a media server can read
/// it whatever modes the archive stored.
fn find_videos(root: &Path, archive: &Path) -> io::Result<Vec<PathBuf>> {
    let permissions = fs::metadata(archive)?.permissions();
    let mut videos = Vec::new();
    let mut dirs = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = dirs.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() && depth < MAX_EXTRACT_DEPTH {
                dirs.push((path, depth + 1));
            } else if kind.is_file() {
                let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy();
                let name = entry.file_name();
                if library::is_video(&name.to_string_lossy()) && !archive::is_sample(&relative) {
                    fs::set_permissions(&path, permissions.clone())?;
                    videos.push(path);
                }
            }
        }
    }
    videos.sort();
    Ok(videos)
}

/// Move an extracted video to `dest`, never replacing a file. A file of the
/// same size already at `dest` is this one, placed by an attempt that was
/// interrupted.
fn move_extracted(video: &Path, dest: &Path) -> io::Result<()> {
    match fsutil::move_no_clobber(video, dest) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && same_size(video, dest) => Ok(()),
        result => result,
    }
}

/// Move an extracted video into the fallback folder under its own (safe)
/// name, adding ` (1)`… when the name is taken by another file.
fn move_to_unsorted(video: &Path, dir: &Path) -> io::Result<PathBuf> {
    if !fs::metadata(dir)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} is not a folder", dir.display()),
        ));
    }
    let raw = video
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = fsutil::sanitize_filename(&raw)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the name is unusable"))?;
    let placed = dir.join(&name);
    if same_size(video, &placed) {
        return Ok(placed);
    }
    for _ in 0..MAX_NAME_RACES {
        let dest = fsutil::unique_destination(dir, &name)?;
        match fsutil::move_no_clobber(video, &dest) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            result => return result.map(|()| dest),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free file name",
    ))
}

/// The files of the archive whose first volume is `first`: it and its other
/// volumes, regular files in the same folder.
fn archive_volumes(first: &Path) -> io::Result<Vec<PathBuf>> {
    let (Some(dir), Some(name)) = (first.parent(), first.file_name()) else {
        return Ok(Vec::new());
    };
    let name = name.to_string_lossy();
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && archive::in_set(&entry.file_name().to_string_lossy(), &name)
        {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

/// Delete an extracted archive's files. They are in the download folder,
/// next to each other; anything else is refused by `remove_file_in`.
async fn delete_archive(id: Uuid, files: Vec<PathBuf>) {
    let Some(dir) = files
        .first()
        .and_then(|f| f.parent())
        .map(Path::to_path_buf)
    else {
        return;
    };
    let total = files.len();
    let result = blocking(move || {
        let mut failed = Vec::new();
        for file in &files {
            if let Err(e) = fsutil::remove_file_in(&dir, file) {
                failed.push(format!("{}: {e}", file.display()));
            }
        }
        Ok(failed)
    })
    .await;
    match result {
        Ok(failed) if failed.is_empty() => {
            tracing::info!(%id, files = total, "extracted archive deleted");
        }
        Ok(failed) => {
            tracing::warn!(%id, failed = ?failed, "could not delete all of the extracted archive");
        }
        Err(e) => tracing::warn!(%id, error = %e, "could not delete the extracted archive"),
    }
}

/// Where a download's videos went: the file, if one, else the folder that
/// holds them all (a season folder, for a season pack).
fn location(paths: &[PathBuf]) -> Option<PathBuf> {
    let (first, rest) = paths.split_first()?;
    if rest.is_empty() {
        return Some(first.clone());
    }
    let mut common = first.parent()?;
    for path in rest {
        while !path.starts_with(common) {
            common = common.parent()?;
        }
    }
    Some(common.to_path_buf())
}

/// Whether `other` is a regular file as large as `file`.
fn same_size(file: &Path, other: &Path) -> bool {
    match (fs::metadata(file), fs::symlink_metadata(other)) {
        (Ok(a), Ok(b)) => b.file_type().is_file() && a.len() == b.len(),
        _ => false,
    }
}

/// True if an earlier run moved the file to `target` but stopped before
/// recording it: the source is gone and the target is a whole regular file.
fn moved_before_restart(source: &Path, target: &Path, total_bytes: Option<i64>) -> bool {
    if !matches!(std::fs::symlink_metadata(source), Err(e) if e.kind() == io::ErrorKind::NotFound) {
        return false;
    }
    match std::fs::symlink_metadata(target) {
        Ok(meta) => {
            meta.file_type().is_file()
                && total_bytes.is_none_or(|total| i64::try_from(meta.len()) == Ok(total))
        }
        Err(_) => false,
    }
}

fn fs_kind(path: &Path) -> io::Result<Option<std::fs::FileType>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta.file_type())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Run blocking filesystem work off the async threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| io::Error::other("internal task failed"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TransferState;
    use crate::settings::SecretCell;
    use crate::store::{NewDownload, TransitionDetails};
    use std::fs;

    struct Setup {
        store: Store,
        organizer: Organizer,
        library: LibraryCell,
        root: tempfile::TempDir,
        /// TMDB lookups: unset by default.
        tmdb_key: SecretCell,
        /// Delete archives after extraction: off by default.
        delete_archives: FlagCell,
    }

    /// A stand-in for 7-Zip. An "archive" is a list of the paths it holds;
    /// extracting writes `video` to each. One starting with `MISSING` lacks
    /// a volume.
    const FAKE_7Z: &str = r#"#!/bin/sh
cmd=$1; shift
out=; archive=
while [ $# -gt 0 ]; do
  case "$1" in -o*) out="${1#-o}" ;; --) archive="$2"; break ;; esac
  shift
done
if [ "$(head -c 7 "$archive")" = MISSING ]; then
  echo "ERROR: Missing volume : next" >&2; exit 2
fi
if [ "$cmd" = l ]; then
  while IFS= read -r p; do printf 'Path = %s\nFolder = -\nSize = 5\n\n' "$p"; done < "$archive"
else
  while IFS= read -r p; do mkdir -p "$out/$(dirname "$p")"; printf video > "$out/$p"; done < "$archive"
fi
"#;

    impl Setup {
        /// With the stand-in 7-Zip.
        async fn new() -> Self {
            Self::with_sevenzip(|bin| {
                let program = bin.join("7z");
                fs::write(&program, FAKE_7Z).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
                }
                Some(SevenZip::at(program))
            })
            .await
        }

        async fn with_sevenzip(sevenzip: impl FnOnce(&Path) -> Option<SevenZip>) -> Self {
            let root = tempfile::tempdir().unwrap();
            for dir in ["dl", "movies", "tv", "unsorted", "bin"] {
                fs::create_dir(root.path().join(dir)).unwrap();
            }
            let sevenzip = sevenzip(&root.path().join("bin"));
            let delete_archives = FlagCell::default();
            let library = LibraryCell::new(LibraryPaths {
                movies: Some(root.path().join("movies")),
                tv: Some(root.path().join("tv")),
                unsorted: Some(root.path().join("unsorted")),
            });
            let store = Store::open_in_memory().await.unwrap();
            let (metadata, _) = crate::media::artwork_tests::mock_server().await;
            let tmdb_key = SecretCell::default();
            let artwork = crate::media::artwork_tests::service(
                &metadata,
                root.path().join("artwork"),
                tmdb_key.clone(),
            );
            let organizer = Organizer::new(
                store.clone(),
                library.clone(),
                artwork,
                sevenzip,
                delete_archives.clone(),
                Arc::new(Notify::new()),
            );
            Self {
                store,
                organizer,
                library,
                root,
                tmdb_key,
                delete_archives,
            }
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.path().join(rel)
        }

        /// A completed download of `name` (in the download folder).
        async fn completed(
            &self,
            name: &str,
            media_type: Option<MediaType>,
            title: Option<(&str, u16)>,
        ) -> Uuid {
            self.completed_with(name, b"video", media_type, title).await
        }

        /// A completed download of a (stand-in) archive holding `paths`.
        async fn archive(
            &self,
            name: &str,
            paths: &[&str],
            media_type: Option<MediaType>,
            title: Option<(&str, u16)>,
        ) -> Uuid {
            let listing = paths.join("\n") + "\n";
            self.completed_with(name, listing.as_bytes(), media_type, title)
                .await
        }

        async fn completed_with(
            &self,
            name: &str,
            contents: &[u8],
            media_type: Option<MediaType>,
            title: Option<(&str, u16)>,
        ) -> Uuid {
            let path = self.path("dl").join(name);
            fs::write(&path, contents).unwrap();
            let now = Utc::now();
            self.store
                .insert_download(
                    NewDownload {
                        url: "https://example.com/x".into(),
                        display_url: "https://example.com/x".into(),
                        provider: "http".into(),
                        media_type,
                        title: title.map(|(t, _)| t.to_owned()),
                        year: title.map(|(_, y)| y),
                        ..Default::default()
                    },
                    now,
                )
                .await
                .unwrap();
            let d = self.store.claim_next(now).await.unwrap().unwrap();
            self.store
                .set_file_path(d.id, &path.to_string_lossy(), now)
                .await
                .unwrap();
            self.store
                .transition(
                    d.id,
                    TransferState::Downloading,
                    TransferState::Completed,
                    TransitionDetails::default(),
                    now,
                )
                .await
                .unwrap();
            d.id
        }

        async fn organize(&self, id: Uuid) -> Download {
            assert!(self.organizer.organize_next().await);
            self.store.get_download(id).await.unwrap().unwrap()
        }
    }

    #[tokio::test]
    async fn movies_and_episodes_land_in_their_libraries() {
        let s = Setup::new().await;
        let movie = s
            .completed(
                "Blade.Runner.2049.2017.1080p.BluRay.x264.mkv",
                Some(MediaType::Movie),
                Some(("Blade Runner 2049", 2017)),
            )
            .await;
        let episode = s
            .completed(
                "The.Office.US.S03E10.720p.HDTV.mkv",
                Some(MediaType::Tv),
                None,
            )
            .await;

        let d = s.organize(movie).await;
        let expected = s.path("movies/Blade Runner 2049 (2017)/Blade Runner 2049 (2017).mkv");
        assert_eq!(d.post_state, PostState::Completed);
        assert_eq!(d.state, TransferState::Completed);
        assert_eq!(d.post_error, None);
        assert_eq!(d.file_path.as_deref(), Some(&*expected.to_string_lossy()));
        assert_eq!(d.post_target, None);
        assert_eq!(fs::read(&expected).unwrap(), b"video");
        assert!(
            !s.path("dl/Blade.Runner.2049.2017.1080p.BluRay.x264.mkv")
                .exists()
        );

        let d = s.organize(episode).await;
        let expected = s.path("tv/The Office US/Season 03/The Office US - S03E10.mkv");
        assert_eq!(d.post_state, PostState::Completed);
        assert!(expected.is_file());
        let events = s.store.list_events(episode).await.unwrap();
        let last = events.last().unwrap();
        assert_eq!(last.kind, "post_completed");
        assert!(
            last.message.contains("The Office US - S03E10"),
            "{}",
            last.message
        );

        assert!(!s.organizer.organize_next().await, "nothing left");
    }

    #[tokio::test]
    async fn existing_library_files_are_never_replaced() {
        let s = Setup::new().await;
        let dir = s.path("movies/Some Movie (2019)");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Some Movie (2019).mkv"), b"original").unwrap();
        let id = s
            .completed("Some.Movie.2019.mkv", Some(MediaType::Movie), None)
            .await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Failed);
        assert_eq!(d.state, TransferState::Completed, "the transfer stays done");
        let error = d.post_error.unwrap();
        assert_eq!(error.kind, ErrorKind::Filesystem);
        assert!(
            error.message.contains("already exists"),
            "{}",
            error.message
        );
        assert_eq!(
            fs::read(dir.join("Some Movie (2019).mkv")).unwrap(),
            b"original"
        );
        assert!(
            s.path("dl/Some.Movie.2019.mkv").is_file(),
            "the download is kept"
        );
        assert_eq!(
            d.file_path.as_deref(),
            Some(&*s.path("dl/Some.Movie.2019.mkv").to_string_lossy())
        );
    }

    #[tokio::test]
    async fn unidentified_videos_go_to_the_fallback_folder() {
        let s = Setup::new().await;
        fs::write(s.path("unsorted/holiday.mp4"), b"other").unwrap();
        let id = s.completed("holiday.mp4", None, None).await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Completed);
        let fallback = s.path("unsorted/holiday (1).mp4");
        assert_eq!(d.file_path.as_deref(), Some(&*fallback.to_string_lossy()));
        assert_eq!(fs::read(&fallback).unwrap(), b"video");
        assert_eq!(fs::read(s.path("unsorted/holiday.mp4")).unwrap(), b"other");
        let note = d.post_error.unwrap();
        assert_eq!(note.kind, ErrorKind::MediaIdentification);
        assert!(note.message.contains("fallback folder"), "{}", note.message);

        // Without a fallback folder, the file stays and the download says why.
        s.library.set(LibraryPaths {
            unsorted: None,
            ..s.library.get()
        });
        let id = s
            .completed("Show.S01E01.mkv", Some(MediaType::Movie), None)
            .await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Failed);
        let error = d.post_error.unwrap();
        assert_eq!(error.kind, ErrorKind::MediaIdentification);
        assert!(error.message.contains("TV episode"), "{}", error.message);
        assert!(s.path("dl/Show.S01E01.mkv").is_file());
    }

    #[tokio::test]
    async fn other_types_and_archives_are_skipped() {
        let s = Setup::new().await;
        let game = s
            .completed("Hades.II-TENOKE.zip", Some(MediaType::Game), None)
            .await;
        let d = s.organize(game).await;
        assert_eq!(d.post_state, PostState::Skipped);
        assert_eq!(d.post_error, None, "no note for types without a library");

        // Disk images are not extracted.
        let archive = s
            .completed("Some.Movie.2019.iso", Some(MediaType::Movie), None)
            .await;
        let d = s.organize(archive).await;
        assert_eq!(d.post_state, PostState::Skipped);
        assert!(d.post_error.unwrap().message.contains("rchive"));
        assert!(s.path("dl/Some.Movie.2019.iso").is_file());

        s.library.set(LibraryPaths::default());
        let id = s
            .completed("Some.Movie.2019.mkv", Some(MediaType::Movie), None)
            .await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Skipped);
        assert!(d.post_error.unwrap().message.contains("DIREKT_MOVIES_DIR"));
    }

    #[tokio::test]
    async fn missing_files_and_folders_fail_clearly() {
        let s = Setup::new().await;
        let id = s
            .completed("Some.Movie.2019.mkv", Some(MediaType::Movie), None)
            .await;
        fs::remove_file(s.path("dl/Some.Movie.2019.mkv")).unwrap();
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Failed);
        assert!(d.post_error.unwrap().message.contains("missing"));

        fs::remove_dir(s.path("tv")).unwrap();
        let id = s
            .completed("Show.S01E01.mkv", Some(MediaType::Tv), None)
            .await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Failed);
        let message = d.post_error.unwrap().message;
        assert!(message.contains("TV library folder"), "{message}");
        assert!(!s.path("tv").exists(), "the library root is never created");
    }

    #[tokio::test]
    async fn a_move_finished_before_a_restart_is_recognized() {
        let s = Setup::new().await;
        let id = s
            .completed("Some.Movie.2019.mkv", Some(MediaType::Movie), None)
            .await;
        // The previous run recorded the target, moved the file, and stopped.
        let target = s.path("movies/Some Movie (2019)/Some Movie (2019).mkv");
        let now = Utc::now();
        s.store
            .transition_post(id, PostState::Pending, PostState::Processing, None, now)
            .await
            .unwrap();
        s.store
            .set_post_target(id, &target.to_string_lossy(), now)
            .await
            .unwrap();
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::rename(s.path("dl/Some.Movie.2019.mkv"), &target).unwrap();
        let report = s.store.recover_interrupted(now).await.unwrap();
        assert_eq!(report.requeued_post_processing, 1);

        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Completed, "{:?}", d.post_error);
        assert_eq!(d.file_path.as_deref(), Some(&*target.to_string_lossy()));
    }

    #[tokio::test]
    async fn downloads_without_a_title_are_looked_up() {
        let s = Setup::new().await;
        s.tmdb_key.set(Some(crate::config::Secret::new(
            "0123456789abcdef0123456789abcdef",
        )));
        // The mock TMDB knows "Cyberpunk: Edgerunners" (2022).
        let id = s
            .completed("Cyberpunk.Edgerunners.S01E02.1080p.mkv", None, None)
            .await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Completed, "{:?}", d.post_error);
        assert!(
            s.path("tv/Cyberpunk - Edgerunners (2022)/Season 01/Cyberpunk - Edgerunners (2022) - S01E02.mkv")
                .is_file()
        );
    }

    #[tokio::test]
    async fn retried_after_failure() {
        let s = Setup::new().await;
        s.library.set(LibraryPaths::default());
        let id = s
            .completed("Some.Movie.2019.mkv", Some(MediaType::Movie), None)
            .await;
        assert_eq!(s.organize(id).await.post_state, PostState::Skipped);
        s.store
            .transition_post(id, PostState::Skipped, PostState::Pending, None, Utc::now())
            .await
            .unwrap();
        s.library.set(LibraryPaths {
            movies: Some(s.path("movies")),
            ..Default::default()
        });
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Completed);
        assert_eq!(d.post_error, None, "the old note is cleared");
    }

    const SPIDER_MAN: &str = "Your.Friendly.Neighborhood.Spider-Man.S01.MULTI.2160p.WEBRip.zip";

    /// The files under `dir`, relative to it.
    fn files_in(dir: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut dirs = vec![dir.to_path_buf()];
        while let Some(d) = dirs.pop() {
            for entry in fs::read_dir(&d).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else {
                    let rel = path.strip_prefix(dir).unwrap();
                    found.push(rel.to_string_lossy().into_owned());
                }
            }
        }
        found.sort();
        found
    }

    #[tokio::test]
    async fn season_packs_are_extracted_into_the_tv_library() {
        let s = Setup::new().await;
        let id = s
            .archive(
                SPIDER_MAN,
                &[
                    "Spider-Man.S01/Your.Friendly.Neighborhood.Spider-Man.S01E01.MULTI.2160p.mkv",
                    "Spider-Man.S01/Your.Friendly.Neighborhood.Spider-Man.S01E02.MULTI.2160p.mkv",
                    "Spider-Man.S01/Sample/spider-man.s01e01.sample.mkv",
                    "Spider-Man.S01/S01E03.mkv",
                    "Spider-Man.S01/info.nfo",
                ],
                Some(MediaType::Tv),
                Some(("Your Friendly Neighborhood Spider-Man", 2025)),
            )
            .await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Completed, "{:?}", d.post_error);
        assert_eq!(d.post_error, None);
        let show = "Your Friendly Neighborhood Spider-Man (2025)";
        assert_eq!(
            files_in(&s.path("tv")),
            [1, 2, 3].map(|e| format!("{show}/Season 01/{show} - S01E0{e}.mkv"))
        );
        // The archive stays, and stays the download's file; the extraction
        // folder is gone.
        let archive = s.path("dl").join(SPIDER_MAN);
        assert!(archive.is_file());
        assert_eq!(d.file_path.as_deref(), Some(&*archive.to_string_lossy()));
        assert_eq!(files_in(&s.path("dl")), [SPIDER_MAN]);
        let events = s.store.list_events(id).await.unwrap();
        let last = &events.last().unwrap().message;
        assert!(last.contains("extracted 3 videos"), "{last}");
        assert!(last.contains("S01E02"), "{last}");
    }

    #[tokio::test]
    async fn extracting_again_recognizes_videos_already_placed() {
        let s = Setup::new().await;
        // An interrupted attempt placed the first episode.
        let season = s.path("tv/Show/Season 01");
        fs::create_dir_all(&season).unwrap();
        fs::write(season.join("Show - S01E01.mkv"), b"video").unwrap();
        let id = s
            .archive(
                "Show.S01.1080p.rar",
                &["Show.S01E01.mkv", "Show.S01E02.mkv"],
                Some(MediaType::Tv),
                None,
            )
            .await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Completed, "{:?}", d.post_error);
        assert!(season.join("Show - S01E02.mkv").is_file());

        // A different file of that name is never replaced; the rest is placed.
        fs::write(season.join("Show - S01E03.mkv"), b"another release").unwrap();
        let id = s
            .archive(
                "Show.S01.part2.Extras.zip",
                &["Show.S01E03.mkv", "Show.S01E04.mkv"],
                Some(MediaType::Tv),
                None,
            )
            .await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Failed);
        let error = d.post_error.unwrap();
        assert!(error.message.contains("1 of 2 videos"), "{}", error.message);
        assert!(
            error.message.contains("already exists"),
            "{}",
            error.message
        );
        assert_eq!(
            fs::read(season.join("Show - S01E03.mkv")).unwrap(),
            b"another release"
        );
        assert!(season.join("Show - S01E04.mkv").is_file());
        assert!(s.path("dl/Show.S01.part2.Extras.zip").is_file());
        assert!(
            !files_in(&s.path("dl"))
                .iter()
                .any(|f| f.contains("direkt-extract")),
            "the extraction folder is removed"
        );
    }

    #[tokio::test]
    async fn unidentified_videos_in_archives_go_to_the_fallback_folder() {
        let s = Setup::new().await;
        let id = s
            .archive(
                "Some.Movie.2019.zip",
                &["Some.Movie.2019.mkv", "Bonus/holiday.mp4"],
                Some(MediaType::Movie),
                None,
            )
            .await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Completed, "{:?}", d.post_error);
        assert!(
            s.path("movies/Some Movie (2019)/Some Movie (2019).mkv")
                .is_file()
        );
        assert!(s.path("unsorted/holiday.mp4").is_file());
        let note = d.post_error.unwrap();
        assert_eq!(note.kind, ErrorKind::MediaIdentification);
        assert!(note.message.contains("fallback folder"), "{}", note.message);
    }

    #[tokio::test]
    async fn split_archives_are_extracted_once_every_part_is_there() {
        let s = Setup::new().await;
        // The first part arrives before the second.
        let first = s
            .completed_with("Show.S02.part1.rar", b"MISSING", Some(MediaType::Tv), None)
            .await;
        let d = s.organize(first).await;
        assert_eq!(d.post_state, PostState::Skipped);
        assert!(
            d.post_error.unwrap().message.contains("other parts"),
            "waiting for the rest"
        );

        // Now the set is complete (the stand-in reads the first part).
        fs::write(s.path("dl/Show.S02.part1.rar"), "Show.S02E01.mkv\n").unwrap();
        let second = s
            .completed("Show.S02.part2.rar", Some(MediaType::Tv), None)
            .await;
        let d = s.organize(second).await;
        assert_eq!(d.post_state, PostState::Skipped);
        assert!(
            d.post_error.unwrap().message.contains("Show.S02.part1.rar"),
            "points to the first part"
        );
        // ...which is queued again, and extracts the archive.
        let d = s.organize(first).await;
        assert_eq!(d.post_state, PostState::Completed, "{:?}", d.post_error);
        assert!(s.path("tv/Show/Season 02/Show - S02E01.mkv").is_file());
        assert!(!s.organizer.organize_next().await, "nothing left");
    }

    #[tokio::test]
    async fn archives_without_videos_or_7zip_stay() {
        let s = Setup::new().await;
        let id = s
            .archive(
                "Show.S01.Subtitles.zip",
                &["Show.S01E01.srt"],
                Some(MediaType::Tv),
                None,
            )
            .await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Skipped);
        assert!(d.post_error.unwrap().message.contains("no videos"));

        let s = Setup::with_sevenzip(|_| None).await;
        let id = s
            .archive(
                "Show.S01.zip",
                &["Show.S01E01.mkv"],
                Some(MediaType::Tv),
                None,
            )
            .await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Skipped);
        assert!(d.post_error.unwrap().message.contains("7-Zip"));
        assert!(s.path("dl/Show.S01.zip").is_file());
        assert!(files_in(&s.path("tv")).is_empty());
    }

    /// With a real 7-Zip, when one is installed (or `DIREKT_TEST_7Z` names
    /// it): a zip made by 7-Zip itself is extracted and organized.
    #[tokio::test]
    async fn real_7zip_extracts_season_packs() {
        let Some(sevenzip) = archive::tests::real() else {
            eprintln!("7-Zip not found; skipping");
            return;
        };
        let program = sevenzip.program().to_path_buf();
        let s = Setup::with_sevenzip(|_| Some(sevenzip)).await;
        let src = s.path("src/Snowfall.S01");
        fs::create_dir_all(src.join("Sample")).unwrap();
        for e in 1..=2 {
            fs::write(
                src.join(format!("Snowfall.2017.S01E0{e}.mkv")),
                vec![e; 4096],
            )
            .unwrap();
        }
        fs::write(src.join("Sample/snowfall.mkv"), b"sample").unwrap();
        let name = "Snowfall.2017.S01.MULTi.1080p.zip";
        let made = std::process::Command::new(&program)
            .args(["a", "-bso0", "-bsp0", "--"])
            .arg(s.path("dl").join(name))
            .arg(&src)
            .status()
            .unwrap();
        assert!(made.success());
        let id = s
            .completed_with(
                name,
                &fs::read(s.path("dl").join(name)).unwrap(),
                Some(MediaType::Tv),
                Some(("Snowfall", 2017)),
            )
            .await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Completed, "{:?}", d.post_error);
        assert_eq!(
            files_in(&s.path("tv")),
            [
                "Snowfall (2017)/Season 01/Snowfall (2017) - S01E01.mkv",
                "Snowfall (2017)/Season 01/Snowfall (2017) - S01E02.mkv",
            ]
        );
        assert_eq!(
            fs::read(s.path("tv/Snowfall (2017)/Season 01/Snowfall (2017) - S01E02.mkv")).unwrap(),
            vec![2; 4096]
        );
        assert_eq!(files_in(&s.path("dl")), [name]);
    }

    #[tokio::test]
    async fn archives_can_be_deleted_once_extracted() {
        let s = Setup::new().await;
        s.delete_archives.set(true);
        let id = s
            .archive(
                SPIDER_MAN,
                &["Show.S01E01.mkv", "Show.S01E02.mkv"],
                Some(MediaType::Tv),
                Some(("Your Friendly Neighborhood Spider-Man", 2025)),
            )
            .await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Completed, "{:?}", d.post_error);
        assert!(!s.path("dl").join(SPIDER_MAN).exists(), "archive deleted");
        // The download now points at the season folder.
        let season = s.path("tv/Your Friendly Neighborhood Spider-Man (2025)/Season 01");
        assert_eq!(d.file_path.as_deref(), Some(&*season.to_string_lossy()));
        assert_eq!(files_in(&season).len(), 2);
        let events = s.store.list_events(id).await.unwrap();
        let last = &events.last().unwrap().message;
        assert!(last.contains("deleting the archive (1 file)"), "{last}");

        // A single movie: the download points at the movie file.
        let id = s
            .archive(
                "Some.Movie.2019.7z",
                &["Some.Movie.2019.mkv", "Sample/sample.mkv"],
                Some(MediaType::Movie),
                None,
            )
            .await;
        let d = s.organize(id).await;
        let movie = s.path("movies/Some Movie (2019)/Some Movie (2019).mkv");
        assert_eq!(d.file_path.as_deref(), Some(&*movie.to_string_lossy()));
        assert!(!s.path("dl/Some.Movie.2019.7z").exists());
    }

    #[tokio::test]
    async fn every_part_of_a_split_archive_is_deleted() {
        let s = Setup::new().await;
        s.delete_archives.set(true);
        for other in ["Show.S02.part3.rar", "Other.part2.rar", "Show.S02.nfo"] {
            fs::write(s.path("dl").join(other), b"x").unwrap();
        }
        let second = s
            .completed("Show.S02.part2.rar", Some(MediaType::Tv), None)
            .await;
        s.organize(second).await;
        let first = s
            .archive(
                "Show.S02.part1.rar",
                &["Show.S02E01.mkv"],
                Some(MediaType::Tv),
                None,
            )
            .await;
        let d = s.organize(first).await;
        assert_eq!(d.post_state, PostState::Completed, "{:?}", d.post_error);
        assert_eq!(files_in(&s.path("dl")), ["Other.part2.rar", "Show.S02.nfo"]);
    }

    #[tokio::test]
    async fn archives_are_kept_when_a_video_could_not_be_placed() {
        let s = Setup::new().await;
        s.delete_archives.set(true);
        let season = s.path("tv/Show/Season 01");
        fs::create_dir_all(&season).unwrap();
        fs::write(season.join("Show - S01E01.mkv"), b"another release").unwrap();
        let id = s
            .archive(
                "Show.S01.zip",
                &["Show.S01E01.mkv", "Show.S01E02.mkv"],
                Some(MediaType::Tv),
                None,
            )
            .await;
        let d = s.organize(id).await;
        assert_eq!(d.post_state, PostState::Failed);
        assert!(
            s.path("dl/Show.S01.zip").is_file(),
            "kept for Organize again"
        );
        assert_eq!(
            d.file_path.as_deref(),
            Some(&*s.path("dl/Show.S01.zip").to_string_lossy())
        );
    }

    #[test]
    fn locations() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(location(&[]), None);
        assert_eq!(
            location(&[p("/tv/S/Season 01/a.mkv")]),
            Some(p("/tv/S/Season 01/a.mkv"))
        );
        assert_eq!(
            location(&[p("/tv/S/Season 01/a.mkv"), p("/tv/S/Season 01/b.mkv")]),
            Some(p("/tv/S/Season 01"))
        );
        assert_eq!(
            location(&[p("/tv/S/Season 01/a.mkv"), p("/tv/S/Season 02/b.mkv")]),
            Some(p("/tv/S"))
        );
    }
}
