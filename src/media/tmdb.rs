//! The Movie Database (TMDB): movie and TV series search.
//!
//! Release names often carry a localized title ("Avengers.L_ère.d_Ultron"),
//! while search results carry the English one ("Avengers: Age of Ultron").
//! When no result's title matches, the translations of the best few
//! candidates are checked too.
//!
//! The API key travels as a query parameter (v3 key) or bearer token (v4
//! read access token); errors never carry request URLs.

use super::fetch::{Found, LookupError, get_json};
use super::parse::{MediaGuess, normalize_title};
use crate::config::Secret;
use crate::domain::MediaType;
use reqwest::Client;
use serde::Deserialize;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;
use url::Url;

pub const API_BASE: &str = "https://api.themoviedb.org/3/";
pub const IMAGE_BASE: &str = "https://image.tmdb.org/t/p/w780/";

/// Search results examined for an exact title match.
const MAX_CANDIDATES: usize = 10;
/// Candidates whose translated titles are fetched when no title matches.
const MAX_TRANSLATION_LOOKUPS: usize = 3;
/// Leading articles a release name may drop ("Avengers" for "The Avengers").
const ARTICLES: &[&str] = &[
    "the", "a", "an", "le", "la", "les", "l", "el", "los", "las", "il", "der", "das",
];

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
    id: u64,
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

    fn title_fit(&self, wanted: &Wanted) -> Option<Fit> {
        [&self.title, &self.original_title]
            .into_iter()
            .flatten()
            .filter_map(|t| wanted.fit(t))
            .max()
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

#[derive(Deserialize)]
struct TranslationsResponse {
    #[serde(default)]
    translations: Vec<Translation>,
}

#[derive(Deserialize)]
struct Translation {
    #[serde(default)]
    data: TranslationData,
}

#[derive(Deserialize, Default)]
struct TranslationData {
    /// `title` for movies, `name` for TV shows.
    #[serde(alias = "name")]
    title: Option<String>,
}

/// How closely a title matches the release name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Fit {
    /// Equal once a leading article is dropped; trusted only with a year.
    Article,
    /// Equal ignoring case, accents, punctuation and spacing.
    Exact,
}

/// The release name's title, reduced for comparison.
struct Wanted {
    exact: String,
    /// Without a leading article; only set when the guess has a year.
    article: Option<String>,
}

impl Wanted {
    fn new(guess: &MediaGuess) -> Self {
        Self {
            exact: match_key(&guess.title),
            article: guess.year.map(|_| match_key(strip_article(&guess.title))),
        }
    }

    fn fit(&self, title: &str) -> Option<Fit> {
        if self.exact.is_empty() {
            None
        } else if match_key(title) == self.exact {
            Some(Fit::Exact)
        } else if self
            .article
            .as_ref()
            .is_some_and(|a| !a.is_empty() && match_key(strip_article(title)) == *a)
        {
            Some(Fit::Article)
        } else {
            None
        }
    }
}

/// A title reduced for loose comparison: [`normalize_title`] without
/// accents or spaces, so "L ère d Ultron" (from `L_ère.d_Ultron`) equals
/// "L'Ère d'Ultron".
fn match_key(title: &str) -> String {
    normalize_title(title)
        .nfd()
        .filter(|&c| !is_combining_mark(c) && !c.is_whitespace())
        .collect()
}

/// `title` without a leading article, when more words follow.
fn strip_article(title: &str) -> &str {
    let trimmed = title.trim_start();
    let end = trimmed
        .find(|c: char| !c.is_alphanumeric())
        .unwrap_or(trimmed.len());
    let (first, rest) = trimmed.split_at(end);
    let rest = rest.trim_start_matches(|c: char| !c.is_alphanumeric());
    if !rest.is_empty() && ARTICLES.iter().any(|a| a.eq_ignore_ascii_case(first)) {
        rest
    } else {
        title
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

/// Results that could be `guess`, in TMDB's order: the year, when known,
/// within one (release dates differ by country), and an image to show.
fn candidates<'a>(
    guess: &MediaGuess,
    results: &'a [SearchResult],
) -> impl Iterator<Item = &'a SearchResult> {
    results
        .iter()
        .take(MAX_CANDIDATES)
        .filter(move |r| match (guess.year, r.year()) {
            (Some(want), Some(got)) => want.abs_diff(got) <= 1,
            (Some(_), None) => false,
            (None, _) => true,
        })
        .filter(|r| r.image_path().is_some())
}

/// Pick a result for `guess` by its title or original title: the first exact
/// match, else the first match ignoring a leading article.
fn choose<'a>(guess: &MediaGuess, results: &'a [SearchResult]) -> Option<&'a SearchResult> {
    let wanted = Wanted::new(guess);
    let mut best: Option<(&SearchResult, Fit)> = None;
    for r in candidates(guess, results) {
        match r.title_fit(&wanted) {
            Some(Fit::Exact) => return Some(r),
            Some(Fit::Article) if best.is_none() => best = Some((r, Fit::Article)),
            _ => {}
        }
    }
    best.map(|(r, _)| r)
}

/// The translated title of `translations` that matches `wanted` best (the
/// first of equally good ones).
fn matching_translation(wanted: &Wanted, translations: TranslationsResponse) -> Option<String> {
    translations
        .translations
        .into_iter()
        .filter_map(|t| t.data.title)
        .filter_map(|t| wanted.fit(&t).map(|fit| (fit, t)))
        .min_by_key(|(fit, _)| std::cmp::Reverse(*fit))
        .map(|(_, t)| t)
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
        let mut url = self.api_url(endpoint)?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("query", &guess.title)
                .append_pair("include_adult", "false");
            if let Some(year) = guess.year {
                query.append_pair(year_param, &year.to_string());
            }
        }
        let body: SearchResponse = self.get(url, key).await?;
        if let Some(result) = choose(guess, &body.results)
            && let Some(title) = result.title.clone()
        {
            return self.found(result, title).map(Some);
        }
        let wanted = Wanted::new(guess);
        let kind = match guess.kind {
            MediaType::Tv => "tv",
            _ => "movie",
        };
        for result in candidates(guess, &body.results).take(MAX_TRANSLATION_LOOKUPS) {
            let url = self.api_url(&format!("{kind}/{}/translations", result.id))?;
            let translations: TranslationsResponse = self.get(url, key).await?;
            if let Some(title) = matching_translation(&wanted, translations) {
                return self.found(result, title).map(Some);
            }
        }
        Ok(None)
    }

    fn api_url(&self, path: &str) -> Result<Url, LookupError> {
        self.api_base
            .join(path)
            .map_err(|_| LookupError::Failed("invalid API URL".into()))
    }

    /// GET `url` with the key as a query parameter or bearer token.
    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        mut url: Url,
        key: &Secret,
    ) -> Result<T, LookupError> {
        if is_read_access_token(key.expose()) {
            get_json(self.client.get(url).bearer_auth(key.expose())).await
        } else {
            url.query_pairs_mut().append_pair("api_key", key.expose());
            get_json(self.client.get(url)).await
        }
    }

    /// `result` with `title` shown for it.
    fn found(&self, result: &SearchResult, title: String) -> Result<Found, LookupError> {
        let Some(path) = result.image_path() else {
            return Err(LookupError::Failed("no image".into()));
        };
        let image = self
            .image_base
            .join(path.trim_start_matches('/'))
            .map_err(|_| LookupError::Failed("invalid image path".into()))?;
        Ok(Found {
            title,
            year: result.year(),
            image,
        })
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
            id: 1,
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
            id: 1,
            title: Some("The Intouchables".into()),
            original_title: Some("Intouchables".into()),
            release_date: Some("2011-11-02".into()),
            backdrop_path: Some("../../etc/passwd".into()),
            poster_path: Some("/poster.jpg".into()),
        };
        let g = guess(MediaType::Movie, "Intouchables", None);
        assert_eq!(r.title_fit(&Wanted::new(&g)), Some(Fit::Exact));
        assert_eq!(r.image_path(), Some("/poster.jpg"), "unsafe path skipped");
    }

    #[test]
    fn titles_match_without_accents_punctuation_or_spacing() {
        assert_eq!(
            match_key("Avengers : L'Ère d'Ultron"),
            "avengersleredultron"
        );
        assert_eq!(match_key("Avengers L ère d Ultron"), "avengersleredultron");
        assert_eq!(match_key("Amélie"), match_key("Amelie"));
        let g = guess(MediaType::Movie, "Avengers L ère d Ultron", Some(2015));
        let wanted = Wanted::new(&g);
        assert_eq!(wanted.fit("Avengers : L'Ère d'Ultron"), Some(Fit::Exact));
        assert_eq!(wanted.fit("Avengers: Age of Ultron"), None);
    }

    #[test]
    fn leading_articles_need_a_year() {
        assert_eq!(strip_article("The Avengers"), "Avengers");
        assert_eq!(strip_article("L'Odyssée de Pi"), "Odyssée de Pi");
        assert_eq!(strip_article("The"), "The", "nothing follows");
        assert_eq!(strip_article("Theory"), "Theory");

        let results = [
            result(
                "The Avengers: A Visual Journey",
                "2012-09-25",
                Some("/v.jpg"),
            ),
            result("The Avengers", "2012-04-25", Some("/a.jpg")),
        ];
        let g = guess(MediaType::Movie, "Avengers", Some(2012));
        assert_eq!(
            choose(&g, &results).and_then(|r| r.image_path()),
            Some("/a.jpg")
        );
        let g = guess(MediaType::Movie, "Avengers", None);
        assert!(choose(&g, &results).is_none(), "no year, no article match");

        // An exact match wins over an earlier article match.
        let results = [
            result("The Thing", "1982-06-25", Some("/the.jpg")),
            result("Thing", "1982-01-01", Some("/thing.jpg")),
        ];
        let g = guess(MediaType::Movie, "Thing", Some(1982));
        assert_eq!(
            choose(&g, &results).and_then(|r| r.image_path()),
            Some("/thing.jpg")
        );
    }

    #[test]
    fn translations_match() {
        let body: TranslationsResponse = serde_json::from_value(serde_json::json!({
            "id": 99861,
            "translations": [
                { "iso_639_1": "en", "data": { "title": "" } },
                { "iso_639_1": "fr", "data": { "title": "Avengers : L'Ère d'Ultron" } },
                { "iso_639_1": "fr", "data": { "title": "Avengers : L'ère d'Ultron" } },
                { "iso_639_1": "de", "data": {} },
            ]
        }))
        .unwrap();
        let g = guess(MediaType::Movie, "Avengers L ère d Ultron", Some(2015));
        assert_eq!(
            matching_translation(&Wanted::new(&g), body).as_deref(),
            Some("Avengers : L'Ère d'Ultron")
        );
    }

    /// `DIREKT_TMDB_API_KEY=... cargo test real_tmdb -- --ignored`
    #[tokio::test]
    #[ignore = "requires internet access and a TMDB API key"]
    async fn real_tmdb_localized_titles() {
        let key = Secret::new(std::env::var("DIREKT_TMDB_API_KEY").expect("key set"));
        let client = crate::http_client::build_client(Default::default()).unwrap();
        let tmdb = Tmdb::new(client);
        for (name, title, year) in [
            (
                "Avengers.2012.MULTi.VFF.2160p.DV.BluRay.4KLight.TrueHD.Atmos.7.1.x265-J4CK.mkv",
                "The Avengers",
                2012,
            ),
            (
                "Avengers.L_ère.d_Ultron.2015.MULTi.VFF.2160p.DV.BluRay.x265-J4CK.mkv",
                "Avengers : L'Ère d'Ultron",
                2015,
            ),
        ] {
            let g = crate::media::parse::parse_filename(name).unwrap();
            let found = tmdb.search(&g, &key).await.unwrap().expect(name);
            assert_eq!((found.title.as_str(), found.year), (title, Some(year)));
        }
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
