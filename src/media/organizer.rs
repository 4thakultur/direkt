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
//! The transfer state is never changed. A restart during a move puts the
//! download back to `pending`; the recorded destination lets the next run
//! recognize a move that finished just before.

use super::ArtworkService;
use super::Lookup;
use super::library::{self, LibraryPaths, Metadata, Placement};
use crate::domain::{Download, DownloadError, MediaType, PostState};
use crate::error::ErrorKind;
use crate::fsutil;
use crate::settings::LibraryCell;
use crate::store::{PostDetails, Store};
use chrono::Utc;
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

#[derive(Clone)]
pub struct Organizer {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    library: LibraryCell,
    artwork: ArtworkService,
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
    Skipped {
        note: Option<String>,
    },
    Failed {
        kind: ErrorKind,
        message: String,
    },
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
        }
    }
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
    ///
    /// [`Engine::completions`]: crate::engine::Engine::completions
    pub fn new(
        store: Store,
        library: LibraryCell,
        artwork: ArtworkService,
        wake: Arc<Notify>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                library,
                artwork,
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
        let metadata = self.metadata(d, &file_name).await;
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
    async fn metadata(&self, d: &Download, file_name: &str) -> Option<Metadata> {
        if let (Some(title), Some(MediaType::Movie | MediaType::Tv)) = (&d.title, d.media_type) {
            return Some(Metadata {
                title: title.clone(),
                year: d.year,
            });
        }
        let guess = library::lookup_guess(file_name, d.media_type)?;
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

    async fn move_to_library(
        &self,
        id: Uuid,
        source: PathBuf,
        root: PathBuf,
        folders: Vec<String>,
        file_name: String,
        label: String,
    ) -> Outcome {
        let which = if folders.len() > 1 { "TV" } else { "movie" };
        let root_check = root.clone();
        match blocking(move || Ok(std::fs::metadata(&root_check)?.is_dir())).await {
            Ok(true) => {}
            _ => {
                return Outcome::failed(
                    ErrorKind::Filesystem,
                    format!(
                        "the {which} library folder {} is missing; check the path (and the \
                         volume mounts, in Docker)",
                        root.display()
                    ),
                );
            }
        }
        let parts = folders.clone();
        let dir = match blocking(move || {
            let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
            fsutil::create_dirs_in(&root, &parts)
        })
        .await
        {
            Ok(dir) => dir,
            Err(e) => {
                return Outcome::failed(
                    ErrorKind::Filesystem,
                    format!("could not create the library folder: {e}"),
                );
            }
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

fn move_failed(e: &io::Error) -> Outcome {
    let message = match e.kind() {
        io::ErrorKind::NotFound => {
            "the downloaded file disappeared before it could be moved".into()
        }
        io::ErrorKind::PermissionDenied => format!(
            "Direkt is not allowed to write to the library folder ({e}); check its owner and \
             permissions"
        ),
        _ => format!("could not move the file: {e}"),
    };
    Outcome::failed(ErrorKind::Filesystem, message)
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
    }

    impl Setup {
        async fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            for dir in ["dl", "movies", "tv", "unsorted"] {
                fs::create_dir(root.path().join(dir)).unwrap();
            }
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
                Arc::new(Notify::new()),
            );
            Self {
                store,
                organizer,
                library,
                root,
                tmdb_key,
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
            let path = self.path("dl").join(name);
            fs::write(&path, b"video").unwrap();
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

        let archive = s
            .completed("Show.S02.1080p.WEB.zip", Some(MediaType::Tv), None)
            .await;
        let d = s.organize(archive).await;
        assert_eq!(d.post_state, PostState::Skipped);
        assert!(d.post_error.unwrap().message.contains("rchives"));
        assert!(s.path("dl/Show.S02.1080p.WEB.zip").is_file());

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
}
