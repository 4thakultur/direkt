//! Cover artwork for downloads, found from the file name.
//!
//! [`ArtworkService::lookup`] never waits on the network: it answers from an
//! in-memory index and queues unknown names for a single background worker,
//! which parses the name, asks TMDB, and caches the image under
//! `data_dir/artwork/<id>`. The web UI serves cached images itself, so
//! browsers never contact TMDB. Names that cannot be parsed or matched get no
//! artwork, and the UI keeps its file-type tile.
//!
//! Images are written to a temporary file and renamed into place, so an
//! interrupted write never leaves a truncated image behind.

use super::parse::{MediaGuess, normalize_title, parse_filename};
use super::tmdb::{Tmdb, TmdbError, image_type};
use crate::settings::SecretCell;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Lookups waiting for the worker. When full, names are retried on a later
/// render.
const QUEUE_CAPACITY: usize = 128;
/// How long a name TMDB has no match for is left alone.
const MISS_RETRY: Duration = Duration::from_secs(24 * 3600);
/// Retry delay after a network or TMDB error.
const ERROR_RETRY: Duration = Duration::from_secs(10 * 60);
/// Retry delay after TMDB rejected the key (a new key retries immediately).
const AUTH_RETRY: Duration = Duration::from_secs(3600);
/// Pause after TMDB rate limits us.
const RATE_LIMIT_PAUSE: Duration = Duration::from_secs(10);
/// Artwork ids are this many hex digits of a SHA-256.
const ID_LEN: usize = 32;

#[derive(Clone)]
pub struct ArtworkService {
    shared: Arc<Shared>,
    jobs: mpsc::Sender<Job>,
}

struct Shared {
    dir: PathBuf,
    key: SecretCell,
    entries: Mutex<HashMap<String, Entry>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Pending,
    Found,
    /// No artwork; look again after `retry_at` or once the key changes.
    Missing {
        retry_at: Instant,
        key_version: u64,
    },
}

struct Job {
    id: String,
    guess: MediaGuess,
}

impl ArtworkService {
    /// Start the lookup worker. It stops when the last service handle is
    /// dropped.
    pub fn start(dir: PathBuf, tmdb: Tmdb, key: SecretCell) -> (Self, JoinHandle<()>) {
        let (jobs, rx) = mpsc::channel(QUEUE_CAPACITY);
        let shared = Arc::new(Shared {
            dir,
            key,
            entries: Mutex::default(),
        });
        let worker = tokio::spawn(run_worker(shared.clone(), tmdb, rx));
        (Self { shared, jobs }, worker)
    }

    /// The artwork id for a file name, if its artwork is cached. Unknown
    /// names are queued for lookup.
    pub fn lookup(&self, filename: &str) -> Option<String> {
        let guess = parse_filename(filename)?;
        let id = artwork_id(&guess);
        let mut entries = self.shared.lock();
        match entries.get(&id) {
            Some(Entry::Found) => return Some(id),
            Some(Entry::Pending) => return None,
            Some(&Entry::Missing {
                retry_at,
                key_version,
            }) if Instant::now() < retry_at && key_version == self.shared.key.version() => {
                return None;
            }
            _ => {}
        }
        match self.jobs.try_send(Job {
            id: id.clone(),
            guess,
        }) {
            Ok(()) => {
                entries.insert(id, Entry::Pending);
            }
            Err(_) => tracing::debug!("artwork queue full; lookup deferred"),
        }
        None
    }

    /// A cached image and its content type.
    pub async fn image(&self, id: &str) -> io::Result<Option<(Vec<u8>, &'static str)>> {
        if !valid_id(id) {
            return Ok(None);
        }
        match tokio::fs::read(self.shared.dir.join(id)).await {
            Ok(bytes) => Ok(image_type(&bytes).map(|t| (bytes, t))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn missing(&self, after: Duration, key_version: u64) -> Entry {
        Entry::Missing {
            retry_at: Instant::now() + after,
            key_version,
        }
    }
}

/// Stable id for a guess: show- or movie-level, so every episode of a show
/// shares one image.
fn artwork_id(guess: &MediaGuess) -> String {
    let key = format!(
        "{}\n{}\n{}",
        guess.kind.as_str(),
        normalize_title(&guess.title),
        guess.year.map(|y| y.to_string()).unwrap_or_default()
    );
    let mut id = hex::encode(Sha256::digest(key.as_bytes()));
    id.truncate(ID_LEN);
    id
}

/// The artwork id a file name would get (tests seed the cache with it).
#[cfg(test)]
pub(crate) fn id_for_name(filename: &str) -> Option<String> {
    parse_filename(filename).map(|g| artwork_id(&g))
}

fn valid_id(id: &str) -> bool {
    id.len() == ID_LEN && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

async fn run_worker(shared: Arc<Shared>, tmdb: Tmdb, mut rx: mpsc::Receiver<Job>) {
    while let Some(job) = rx.recv().await {
        let entry = process(&shared, &tmdb, &job).await;
        shared.lock().insert(job.id, entry);
    }
}

async fn process(shared: &Shared, tmdb: &Tmdb, job: &Job) -> Entry {
    let path = shared.dir.join(&job.id);
    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Entry::Found;
    }
    let version = shared.key.version();
    let Some(key) = shared.key.get() else {
        return shared.missing(MISS_RETRY, version);
    };
    match tmdb.find_artwork(&job.guess, &key).await {
        Ok(Some(bytes)) => match save(&shared.dir, &job.id, &bytes).await {
            Ok(()) => {
                tracing::debug!(title = %job.guess.title, "artwork cached");
                Entry::Found
            }
            Err(e) => {
                tracing::warn!(error = %e, dir = %shared.dir.display(), "could not save artwork");
                shared.missing(ERROR_RETRY, version)
            }
        },
        Ok(None) => {
            tracing::debug!(title = %job.guess.title, "no artwork match on TMDB");
            shared.missing(MISS_RETRY, version)
        }
        Err(TmdbError::Unauthorized) => {
            tracing::warn!("TMDB rejected the API key; check it on the settings page");
            shared.missing(AUTH_RETRY, version)
        }
        Err(TmdbError::RateLimited) => {
            tokio::time::sleep(RATE_LIMIT_PAUSE).await;
            shared.missing(ERROR_RETRY, version)
        }
        Err(e) => {
            tracing::warn!(error = %e, "artwork lookup failed");
            shared.missing(ERROR_RETRY, version)
        }
    }
}

async fn save(dir: &Path, id: &str, bytes: &[u8]) -> io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let tmp = dir.join(format!(".{id}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    if let Err(e) = tokio::fs::rename(&tmp, dir.join(id)).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Secret;
    use crate::net::NetworkPolicy;
    use axum::Router;
    use axum::extract::{Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use std::collections::HashMap as StdHashMap;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use url::Url;

    const KEY: &str = "0123456789abcdef0123456789abcdef";
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F'];
    const NAME: &str = "Cyberpunk.Edgerunners.S02.MULTi.AD.1080p.NF.WEB.x264-BYOR.zip";

    #[derive(Default)]
    struct Mock {
        searches: AtomicUsize,
    }

    async fn search_tv(
        State(mock): State<Arc<Mock>>,
        Query(q): Query<StdHashMap<String, String>>,
    ) -> Response {
        mock.searches.fetch_add(1, Ordering::SeqCst);
        if q.get("api_key").map(String::as_str) != Some(KEY) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let results = match q.get("query").map(String::as_str) {
            Some("Cyberpunk Edgerunners") => serde_json::json!([
                { "name": "Cyberpunk 2077", "first_air_date": "2020-01-01", "backdrop_path": "/wrong.jpg" },
                { "name": "Cyberpunk: Edgerunners", "first_air_date": "2022-09-13",
                  "backdrop_path": "/edgerunners.jpg", "poster_path": "/poster.jpg" },
            ]),
            _ => serde_json::json!([]),
        };
        axum::Json(serde_json::json!({ "page": 1, "results": results })).into_response()
    }

    async fn image(headers: HeaderMap) -> Response {
        assert!(headers.get("authorization").is_none());
        ([("content-type", "image/jpeg")], JPEG).into_response()
    }

    struct Fixture {
        service: ArtworkService,
        mock: Arc<Mock>,
        key: SecretCell,
        _dir: tempfile::TempDir,
    }

    async fn fixture(key: Option<&str>) -> Fixture {
        let mock = Arc::new(Mock::default());
        let app = Router::new()
            .route("/3/search/tv", get(search_tv))
            .route("/img/edgerunners.jpg", get(image))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let policy = NetworkPolicy::new(vec!["127.0.0.0/8".parse().unwrap()]);
        let client = crate::http_client::build_client(policy).unwrap();
        let tmdb = Tmdb::with_bases(
            client,
            Url::parse(&format!("{base}/3/")).unwrap(),
            Url::parse(&format!("{base}/img/")).unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        let key = SecretCell::new(key.map(Secret::new));
        let (service, _worker) =
            ArtworkService::start(dir.path().join("artwork"), tmdb, key.clone());
        Fixture {
            service,
            mock,
            key,
            _dir: dir,
        }
    }

    impl Fixture {
        /// Look up until the worker has settled the name.
        async fn settle(&self, name: &str) -> Option<String> {
            for _ in 0..200 {
                if let Some(id) = self.service.lookup(name) {
                    return Some(id);
                }
                let id = artwork_id(&parse_filename(name)?);
                if matches!(
                    self.service.shared.lock().get(&id),
                    Some(Entry::Missing { .. })
                ) {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("artwork lookup did not settle");
        }
    }

    #[tokio::test]
    async fn finds_caches_and_serves_artwork() {
        let f = fixture(Some(KEY)).await;
        assert_eq!(f.service.lookup(NAME), None, "first lookup is queued");
        let id = f.settle(NAME).await.expect("artwork found");
        assert!(valid_id(&id));
        let (bytes, kind) = f.service.image(&id).await.unwrap().unwrap();
        assert_eq!((bytes.as_slice(), kind), (JPEG, "image/jpeg"));

        // Another episode of the same show shares the image; no new search.
        let searches = f.mock.searches.load(Ordering::SeqCst);
        let other = "Cyberpunk.Edgerunners.S01E03.1080p.mkv";
        assert_eq!(f.settle(other).await.as_deref(), Some(id.as_str()));
        assert_eq!(f.mock.searches.load(Ordering::SeqCst), searches);
    }

    #[tokio::test]
    async fn unmatched_and_unparsable_names_get_none() {
        let f = fixture(Some(KEY)).await;
        assert_eq!(f.settle("Unknown.Show.S01E01.720p.mkv").await, None);
        assert_eq!(f.mock.searches.load(Ordering::SeqCst), 1);
        assert_eq!(f.service.lookup("Unknown.Show.S01E01.720p.mkv"), None);
        assert_eq!(f.mock.searches.load(Ordering::SeqCst), 1, "miss is cached");

        assert_eq!(f.service.lookup("backup.tar.gz"), None);
        assert_eq!(f.mock.searches.load(Ordering::SeqCst), 1, "not looked up");
    }

    #[tokio::test]
    async fn setting_a_key_retries_missing_artwork() {
        let f = fixture(None).await;
        assert_eq!(f.settle(NAME).await, None);
        assert_eq!(
            f.mock.searches.load(Ordering::SeqCst),
            0,
            "no key, no request"
        );

        f.key
            .set(Some(Secret::new("ffffffffffffffffffffffffffffffff")));
        assert_eq!(f.settle(NAME).await, None, "rejected key");
        assert_eq!(f.mock.searches.load(Ordering::SeqCst), 1);

        f.key.set(Some(Secret::new(KEY)));
        assert!(f.settle(NAME).await.is_some());
    }

    #[tokio::test]
    async fn image_ids_are_validated() {
        let f = fixture(Some(KEY)).await;
        for id in [
            "../direkt.db",
            "",
            "ABCDEF0123456789ABCDEF0123456789",
            ".x.tmp",
        ] {
            assert!(f.service.image(id).await.unwrap().is_none(), "{id}");
        }
        assert!(
            f.service
                .image("0123456789abcdef0123456789abcdef")
                .await
                .unwrap()
                .is_none()
        );
    }
}
