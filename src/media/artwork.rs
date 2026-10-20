//! Cover artwork and titles for the confirmation step.
//!
//! [`ArtworkService::find`] looks a [`MediaGuess`] up (TMDB for movies and TV
//! series, Steam for games), downloads the image, and caches it under
//! `data_dir/artwork/<id>` with its title in `<id>.json`. The web UI serves
//! cached images itself, so browsers never contact the metadata services.
//!
//! Files are written to a temporary name and renamed into place, so an
//! interrupted write never leaves a truncated image behind. Misses are
//! remembered in memory for a while, so repeated previews don't repeat
//! requests.

use super::fetch::{Found, LookupError, fetch_image, image_type};
use super::parse::{MediaGuess, normalize_title};
use super::steam::Steam;
use super::tmdb::Tmdb;
use crate::domain::MediaType;
use crate::settings::SecretCell;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// How long a title with no match is left alone.
const MISS_RETRY: Duration = Duration::from_secs(24 * 3600);
/// Retry delay after a network or service error.
const ERROR_RETRY: Duration = Duration::from_secs(10 * 60);
/// Retry delay after TMDB rejected the key (a new key retries at once).
const AUTH_RETRY: Duration = Duration::from_secs(3600);
/// Lookups running at once, so API clients cannot flood the services.
const MAX_CONCURRENT: usize = 2;
/// Remembered misses; the oldest are dropped beyond this.
const MAX_MISSES: usize = 1024;
/// Artwork ids are this many hex digits of a SHA-256.
const ID_LEN: usize = 32;

#[derive(Clone)]
pub struct ArtworkService {
    inner: Arc<Inner>,
}

struct Inner {
    dir: PathBuf,
    client: reqwest::Client,
    tmdb: Tmdb,
    steam: Steam,
    tmdb_key: SecretCell,
    misses: Mutex<HashMap<String, Miss>>,
    permits: Semaphore,
}

#[derive(Debug, Clone, Copy)]
struct Miss {
    retry_at: Instant,
    /// For TMDB lookups: the key version the miss was found with.
    key_version: u64,
}

/// What a lookup found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Found(Artwork),
    NotFound,
    /// Movie and TV lookups need a TMDB key, and none is set.
    NeedsKey,
    /// The service failed; a later preview tries again.
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artwork {
    pub id: String,
    /// The title as the metadata service writes it.
    pub title: String,
    pub year: Option<u16>,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    title: String,
    year: Option<u16>,
}

impl ArtworkService {
    /// `client` must be the policy-enforcing client.
    pub fn new(dir: PathBuf, client: reqwest::Client, tmdb_key: SecretCell) -> Self {
        let tmdb = Tmdb::new(client.clone());
        let steam = Steam::new(client.clone());
        Self::with_sources(dir, client, tmdb, steam, tmdb_key)
    }

    pub fn with_sources(
        dir: PathBuf,
        client: reqwest::Client,
        tmdb: Tmdb,
        steam: Steam,
        tmdb_key: SecretCell,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                dir,
                client,
                tmdb,
                steam,
                tmdb_key,
                misses: Mutex::default(),
                permits: Semaphore::new(MAX_CONCURRENT),
            }),
        }
    }

    pub async fn find(&self, guess: &MediaGuess) -> Lookup {
        let inner = &self.inner;
        let id = artwork_id(guess);
        if let Some(artwork) = inner.cached(&id, guess).await {
            return Lookup::Found(artwork);
        }
        let uses_tmdb = matches!(guess.kind, MediaType::Movie | MediaType::Tv);
        let key = if uses_tmdb {
            match inner.tmdb_key.get() {
                Some(key) => Some(key),
                None => return Lookup::NeedsKey,
            }
        } else {
            None
        };
        let version = inner.tmdb_key.version();
        if inner.recent_miss(&id, uses_tmdb.then_some(version)) {
            return Lookup::NotFound;
        }
        let Ok(_permit) = inner.permits.acquire().await else {
            return Lookup::Unavailable("artwork lookups are shutting down".into());
        };
        let result = match &key {
            Some(key) => inner.tmdb.search(guess, key).await,
            None => inner.steam.search(guess).await,
        };
        let outcome = match result {
            Ok(Some(found)) => inner.download(&id, found).await,
            Ok(None) => Err((MISS_RETRY, None)),
            Err(LookupError::Unauthorized) => Err((
                AUTH_RETRY,
                Some("TMDB rejected the API key; check it under Settings → Artwork".into()),
            )),
            Err(e) => Err((ERROR_RETRY, Some(e.to_string()))),
        };
        match outcome {
            Ok(artwork) => Lookup::Found(artwork),
            Err((retry, message)) => {
                inner.remember_miss(id, retry, version);
                match message {
                    Some(m) => {
                        tracing::warn!(error = %m, "artwork lookup failed");
                        Lookup::Unavailable(m)
                    }
                    None => {
                        tracing::debug!(title = %guess.title, "no artwork match");
                        Lookup::NotFound
                    }
                }
            }
        }
    }

    /// Whether `id` names a cached image.
    pub async fn exists(&self, id: &str) -> bool {
        valid_id(id)
            && tokio::fs::try_exists(self.inner.dir.join(id))
                .await
                .unwrap_or(false)
    }

    /// A cached image and its content type.
    pub async fn image(&self, id: &str) -> io::Result<Option<(Vec<u8>, &'static str)>> {
        if !valid_id(id) {
            return Ok(None);
        }
        match tokio::fs::read(self.inner.dir.join(id)).await {
            Ok(bytes) => Ok(image_type(&bytes).map(|t| (bytes, t))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl Inner {
    async fn cached(&self, id: &str, guess: &MediaGuess) -> Option<Artwork> {
        if !tokio::fs::try_exists(self.dir.join(id))
            .await
            .unwrap_or(false)
        {
            return None;
        }
        let meta = tokio::fs::read(self.dir.join(format!("{id}.json")))
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Meta>(&bytes).ok());
        Some(Artwork {
            id: id.to_owned(),
            title: meta
                .as_ref()
                .map(|m| m.title.clone())
                .unwrap_or_else(|| guess.title.clone()),
            year: meta.and_then(|m| m.year),
        })
    }

    fn recent_miss(&self, id: &str, key_version: Option<u64>) -> bool {
        let misses = self.misses.lock().unwrap_or_else(|e| e.into_inner());
        misses.get(id).is_some_and(|m| {
            Instant::now() < m.retry_at && key_version.is_none_or(|v| v == m.key_version)
        })
    }

    fn remember_miss(&self, id: String, retry: Duration, key_version: u64) {
        let mut misses = self.misses.lock().unwrap_or_else(|e| e.into_inner());
        if misses.len() >= MAX_MISSES {
            let now = Instant::now();
            misses.retain(|_, m| m.retry_at > now);
            if misses.len() >= MAX_MISSES {
                misses.clear();
            }
        }
        misses.insert(
            id,
            Miss {
                retry_at: Instant::now() + retry,
                key_version,
            },
        );
    }

    async fn download(
        &self,
        id: &str,
        found: Found,
    ) -> Result<Artwork, (Duration, Option<String>)> {
        let bytes = fetch_image(&self.client, found.image)
            .await
            .map_err(|e| (ERROR_RETRY, Some(e.to_string())))?;
        let meta = serde_json::to_vec(&Meta {
            title: found.title.clone(),
            year: found.year,
        })
        .map_err(|e| (ERROR_RETRY, Some(e.to_string())))?;
        let saved = async {
            save(&self.dir, &format!("{id}.json"), &meta).await?;
            save(&self.dir, id, &bytes).await
        };
        if let Err(e) = saved.await {
            tracing::warn!(error = %e, dir = %self.dir.display(), "could not save artwork");
            return Err((ERROR_RETRY, Some("could not save the artwork".into())));
        }
        Ok(Artwork {
            id: id.to_owned(),
            title: found.title,
            year: found.year,
        })
    }
}

/// Stable id for a guess: show-, movie- or game-level, so every episode of
/// a show shares one image.
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

fn valid_id(id: &str) -> bool {
    id.len() == ID_LEN && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

async fn save(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let tmp = dir.join(format!(".{name}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    if let Err(e) = tokio::fs::rename(&tmp, dir.join(name)).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::Secret;
    use crate::net::NetworkPolicy;
    use axum::Router;
    use axum::extract::{Query, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use std::collections::HashMap as StdHashMap;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use url::Url;

    pub(crate) const KEY: &str = "0123456789abcdef0123456789abcdef";
    pub(crate) const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F'];

    #[derive(Default)]
    pub(crate) struct Mock {
        pub searches: AtomicUsize,
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

    async fn search_steam(
        State(mock): State<Arc<Mock>>,
        Query(q): Query<StdHashMap<String, String>>,
    ) -> Response {
        mock.searches.fetch_add(1, Ordering::SeqCst);
        let items = match q.get("term").map(String::as_str) {
            Some("Hades II") => serde_json::json!([
                { "type": "app", "name": "Hades II Original Soundtrack", "id": 2 },
                { "type": "app", "name": "Hades II", "id": 1145350 },
            ]),
            _ => serde_json::json!([]),
        };
        axum::Json(serde_json::json!({ "total": 2, "items": items })).into_response()
    }

    async fn image() -> Response {
        ([("content-type", "image/jpeg")], JPEG).into_response()
    }

    /// Mock TMDB and Steam on one local server; returns its base URL.
    pub(crate) async fn mock_server() -> (String, Arc<Mock>) {
        let mock = Arc::new(Mock::default());
        let app = Router::new()
            .route("/3/search/tv", get(search_tv))
            .route("/img/edgerunners.jpg", get(image))
            .route("/steam/search", get(search_steam))
            .route("/steamimg/1145350/header.jpg", get(image))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, mock)
    }

    /// An artwork service backed by the mock server.
    pub(crate) fn service(base: &str, dir: PathBuf, key: SecretCell) -> ArtworkService {
        let policy = NetworkPolicy::new(vec!["127.0.0.0/8".parse().unwrap()]);
        let client = crate::http_client::build_client(policy).unwrap();
        let url = |path: &str| Url::parse(&format!("{base}{path}")).unwrap();
        let tmdb = Tmdb::with_bases(client.clone(), url("/3/"), url("/img/"));
        let steam = Steam::with_urls(client.clone(), url("/steam/search"), url("/steamimg/"));
        ArtworkService::with_sources(dir, client, tmdb, steam, key)
    }

    fn guess(kind: MediaType, title: &str) -> MediaGuess {
        MediaGuess {
            kind,
            title: title.into(),
            year: None,
            season: Some(2),
            episode: None,
        }
    }

    #[tokio::test]
    async fn finds_caches_and_serves_artwork() {
        let (base, mock) = mock_server().await;
        let dir = tempfile::tempdir().unwrap();
        let artwork = service(
            &base,
            dir.path().into(),
            SecretCell::new(Some(Secret::new(KEY))),
        );
        let g = guess(MediaType::Tv, "Cyberpunk Edgerunners");
        let Lookup::Found(found) = artwork.find(&g).await else {
            panic!("artwork not found");
        };
        assert_eq!(found.title, "Cyberpunk: Edgerunners");
        assert_eq!(found.year, Some(2022));
        let (bytes, kind) = artwork.image(&found.id).await.unwrap().unwrap();
        assert_eq!((bytes.as_slice(), kind), (JPEG, "image/jpeg"));
        assert!(artwork.exists(&found.id).await);

        // Cached, title included: no second search.
        assert_eq!(artwork.find(&g).await, Lookup::Found(found));
        assert_eq!(mock.searches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn games_use_steam_without_a_key() {
        let (base, _mock) = mock_server().await;
        let dir = tempfile::tempdir().unwrap();
        let artwork = service(&base, dir.path().into(), SecretCell::default());
        let Lookup::Found(found) = artwork.find(&guess(MediaType::Game, "Hades II")).await else {
            panic!("game artwork not found");
        };
        assert_eq!(found.title, "Hades II");
        assert_eq!(
            artwork.find(&guess(MediaType::Game, "Unknown Game")).await,
            Lookup::NotFound
        );
    }

    #[tokio::test]
    async fn misses_are_remembered_and_keys_matter() {
        let (base, mock) = mock_server().await;
        let dir = tempfile::tempdir().unwrap();
        let key = SecretCell::default();
        let artwork = service(&base, dir.path().into(), key.clone());
        let g = guess(MediaType::Tv, "Cyberpunk Edgerunners");
        assert_eq!(artwork.find(&g).await, Lookup::NeedsKey);
        assert_eq!(
            mock.searches.load(Ordering::SeqCst),
            0,
            "no key, no request"
        );

        key.set(Some(Secret::new("ffffffffffffffffffffffffffffffff")));
        assert!(matches!(artwork.find(&g).await, Lookup::Unavailable(m) if m.contains("rejected")));
        assert_eq!(artwork.find(&g).await, Lookup::NotFound, "miss remembered");
        assert_eq!(mock.searches.load(Ordering::SeqCst), 1);

        key.set(Some(Secret::new(KEY)));
        assert!(
            matches!(artwork.find(&g).await, Lookup::Found(_)),
            "new key retries"
        );

        let unknown = guess(MediaType::Tv, "Unknown Show");
        assert_eq!(artwork.find(&unknown).await, Lookup::NotFound);
        assert_eq!(artwork.find(&unknown).await, Lookup::NotFound);
        assert_eq!(mock.searches.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn image_ids_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        let artwork = service(
            "http://127.0.0.1:9",
            dir.path().into(),
            SecretCell::default(),
        );
        for id in [
            "../direkt.db",
            "",
            "ABCDEF0123456789ABCDEF0123456789",
            ".x.tmp",
        ] {
            assert!(artwork.image(id).await.unwrap().is_none(), "{id}");
            assert!(!artwork.exists(id).await, "{id}");
        }
        assert!(
            artwork
                .image("0123456789abcdef0123456789abcdef")
                .await
                .unwrap()
                .is_none()
        );
    }
}
