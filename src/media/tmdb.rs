//! The Movie Database (TMDB): movie and TV series search.
//!
//! The API key travels as a query parameter (v3 key) or bearer token (v4
//! read access token); errors never carry request URLs.

use super::fetch::{Found, LookupError, get_json};
use super::parse::{MediaGuess, normalize_title};
use crate::config::Secret;
use crate::domain::MediaType;
use reqwest::Client;
use serde::Deserialize;
use url::Url;

pub const API_BASE: &str = "https://api.themoviedb.org/3/";
pub const IMAGE_BASE: &str = "https://image.tmdb.org/t/p/w780/";

/// Search results examined for an exact title match.
const MAX_CANDIDATES: usize = 10;

#[derive(Clone)]
pub struct Tmdb {
    client: Client,
    api_base: Url,
    image_base: Url,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchResult>,
}

#[derive(Deserialize)]
struct SearchResult {
    /// `title` for movies, `name` for TV shows.
    #[serde(alias = "name")]
    title: Option<String>,
    #[serde(alias = "original_name")]
    original_title: Option<String>,
    /// `release_date` for movies, `first_air_date` for TV shows.
    #[serde(alias = "first_air_date")]
    release_date: Option<String>,
    backdrop_path: Option<String>,
    poster_path: Option<String>,
}

impl SearchResult {
    fn year(&self) -> Option<u16> {
        self.release_date.as_deref()?.get(..4)?.parse().ok()
    }

    fn title_matches(&self, wanted: &str) -> bool {
        [&self.title, &self.original_title]
            .into_iter()
            .flatten()
            .any(|t| normalize_title(t) == wanted)
    }

    /// The wide backdrop if there is one (it fits the cover tile), else the
    /// poster.
    fn image_path(&self) -> Option<&str> {
        [&self.backdrop_path, &self.poster_path]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .find(|p| valid_image_path(p))
    }
}

/// TMDB image paths look like `/kqjL17yufvn9OVLyXYpvtyrFfak.jpg`.
fn valid_image_path(path: &str) -> bool {
    let Some(name) = path.strip_prefix('/') else {
        return false;
    };
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    (1..=64).contains(&stem.len())
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && matches!(ext, "jpg" | "jpeg" | "png")
}

/// Pick a result for `guess`: the title must match exactly once normalized,
/// and the year, when known, within one (release dates differ by country).
fn choose<'a>(guess: &MediaGuess, results: &'a [SearchResult]) -> Option<&'a SearchResult> {
    let wanted = normalize_title(&guess.title);
    results
        .iter()
        .take(MAX_CANDIDATES)
        .filter(|r| r.title_matches(&wanted))
        .filter(|r| match (guess.year, r.year()) {
            (Some(want), Some(got)) => want.abs_diff(got) <= 1,
            (Some(_), None) => false,
            (None, _) => true,
        })
        .find(|r| r.image_path().is_some())
}

impl Tmdb {
    pub fn new(client: Client) -> Self {
        Self::with_bases(
            client,
            Url::parse(API_BASE).expect("valid URL"),
            Url::parse(IMAGE_BASE).expect("valid URL"),
        )
    }

    /// Point at other servers (tests).
    pub fn with_bases(client: Client, api_base: Url, image_base: Url) -> Self {
        Self {
            client,
            api_base,
            image_base,
        }
    }

    /// A confident match for a movie or TV guess, or `None`.
    pub async fn search(
        &self,
        guess: &MediaGuess,
        key: &Secret,
    ) -> Result<Option<Found>, LookupError> {
        let (endpoint, year_param) = match guess.kind {
            MediaType::Tv => ("search/tv", "first_air_date_year"),
            _ => ("search/movie", "year"),
        };
        let mut url = self
            .api_base
            .join(endpoint)
            .map_err(|_| LookupError::Failed("invalid API URL".into()))?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("query", &guess.title)
                .append_pair("include_adult", "false");
            if let Some(year) = guess.year {
                query.append_pair(year_param, &year.to_string());
            }
            if !is_read_access_token(key.expose()) {
                query.append_pair("api_key", key.expose());
            }
        }
        let mut request = self.client.get(url);
        if is_read_access_token(key.expose()) {
            request = request.bearer_auth(key.expose());
        }
        let body: SearchResponse = get_json(request).await?;
        let Some(result) = choose(guess, &body.results) else {
            return Ok(None);
        };
        let (Some(path), Some(title)) = (result.image_path(), result.title.clone()) else {
            return Ok(None);
        };
        let image = self
            .image_base
            .join(path.trim_start_matches('/'))
            .map_err(|_| LookupError::Failed("invalid image path".into()))?;
        Ok(Some(Found {
            title,
            year: result.year(),
            image,
        }))
    }
}

/// TMDB accepts a v3 API key (32 hex digits) as a query parameter, or a v4
/// read access token (a JWT) as a bearer token.
fn is_read_access_token(key: &str) -> bool {
    key.len() > 32 && key.contains('.')
}

/// Check that a value looks like a TMDB v3 API key or v4 read access token.
pub fn validate_key(key: &str) -> Result<(), String> {
    let v3 = key.len() == 32 && key.bytes().all(|b| b.is_ascii_hexdigit());
    let v4 = (64..=2048).contains(&key.len())
        && key.bytes().filter(|&b| b == b'.').count() == 2
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    if v3 || v4 {
        Ok(())
    } else {
        Err(
            "this does not look like a TMDB API key: expected the 32-character API key \
             or the API read access token from your TMDB account's API settings"
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(title: &str, date: &str, backdrop: Option<&str>) -> SearchResult {
        SearchResult {
            title: Some(title.into()),
            original_title: None,
            release_date: Some(date.into()),
            backdrop_path: backdrop.map(Into::into),
            poster_path: None,
        }
    }

    fn guess(kind: MediaType, title: &str, year: Option<u16>) -> MediaGuess {
        MediaGuess {
            kind,
            title: title.into(),
            year,
            season: None,
            episode: None,
            last_episode: None,
        }
    }

    #[test]
    fn choose_requires_exact_title_and_year() {
        let results = [
            result("Cyberpunk 2077: The Movie", "2020-01-01", Some("/a.jpg")),
            result("Cyberpunk: Edgerunners", "2022-09-13", Some("/b.jpg")),
        ];
        let g = guess(MediaType::Tv, "Cyberpunk Edgerunners", None);
        assert_eq!(
            choose(&g, &results).and_then(|r| r.image_path()),
            Some("/b.jpg")
        );

        let g = guess(MediaType::Movie, "Cyberpunk Edgerunners", Some(2022));
        assert!(choose(&g, &results).is_some());
        let g = guess(MediaType::Movie, "Cyberpunk Edgerunners", Some(2010));
        assert!(choose(&g, &results).is_none(), "year mismatch");
        let g = guess(MediaType::Movie, "Cyberpunk", None);
        assert!(choose(&g, &results).is_none(), "partial title");

        let no_image = [result("Cyberpunk: Edgerunners", "2022-09-13", None)];
        let g = guess(MediaType::Tv, "Cyberpunk Edgerunners", None);
        assert!(choose(&g, &no_image).is_none());
    }

    #[test]
    fn original_titles_and_posters_count() {
        let r = SearchResult {
            title: Some("The Intouchables".into()),
            original_title: Some("Intouchables".into()),
            release_date: Some("2011-11-02".into()),
            backdrop_path: Some("../../etc/passwd".into()),
            poster_path: Some("/poster.jpg".into()),
        };
        assert!(r.title_matches("intouchables"));
        assert_eq!(r.image_path(), Some("/poster.jpg"), "unsafe path skipped");
    }

    #[test]
    fn image_paths_are_validated() {
        assert!(valid_image_path("/kqjL17yufvn9OVLyXYpvtyrFfak.jpg"));
        assert!(valid_image_path("/a-b_c.png"));
        for bad in [
            "a.jpg",
            "/../x.jpg",
            "/a/b.jpg",
            "/a.gif",
            "/.jpg",
            "/a.jpg?x=1",
            "//evil.com/a.jpg",
        ] {
            assert!(!valid_image_path(bad), "{bad}");
        }
    }

    #[test]
    fn keys_are_validated() {
        assert!(validate_key("0123456789abcdef0123456789ABCDEF").is_ok());
        let jwt = format!(
            "eyJ{}.eyJ{}.{}",
            "a".repeat(30),
            "b".repeat(80),
            "c".repeat(40)
        );
        assert!(validate_key(&jwt).is_ok());
        assert!(is_read_access_token(&jwt));
        assert!(!is_read_access_token("0123456789abcdef0123456789ABCDEF"));
        assert!(validate_key("short").is_err());
        assert!(validate_key("0123456789abcdef0123456789abcdeg").is_err());
        assert!(validate_key(&format!("{jwt} ")).is_err());
    }
}
