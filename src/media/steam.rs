//! Steam store search for video game artwork. Keyless: only the parsed game
//! title is sent. The store's 460×215 header image fits the cover tile.

use super::fetch::{Found, LookupError, get_json};
use super::parse::{MediaGuess, normalize_title};
use reqwest::Client;
use serde::Deserialize;
use url::Url;

pub const SEARCH_URL: &str = "https://store.steampowered.com/api/storesearch/";
pub const IMAGE_BASE: &str = "https://shared.akamai.steamstatic.com/store_item_assets/steam/apps/";

/// Search results examined for an exact title match.
const MAX_CANDIDATES: usize = 10;

#[derive(Clone)]
pub struct Steam {
    client: Client,
    search_url: Url,
    image_base: Url,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    items: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
    id: Option<u64>,
}

/// The first app whose name matches the title exactly once normalized
/// ("Hades™ II" matches "Hades II"; "Hades II Soundtrack" does not).
fn choose<'a>(guess: &MediaGuess, items: &'a [Item]) -> Option<(&'a str, u64)> {
    let wanted = normalize_title(&guess.title);
    items.iter().take(MAX_CANDIDATES).find_map(|item| {
        let name = item.name.as_deref()?;
        (item.kind.as_deref() == Some("app") && normalize_title(name) == wanted)
            .then_some((name, item.id?))
    })
}

impl Steam {
    pub fn new(client: Client) -> Self {
        Self::with_urls(
            client,
            Url::parse(SEARCH_URL).expect("valid URL"),
            Url::parse(IMAGE_BASE).expect("valid URL"),
        )
    }

    /// Point at other servers (tests).
    pub fn with_urls(client: Client, search_url: Url, image_base: Url) -> Self {
        Self {
            client,
            search_url,
            image_base,
        }
    }

    pub async fn search(&self, guess: &MediaGuess) -> Result<Option<Found>, LookupError> {
        let mut url = self.search_url.clone();
        url.query_pairs_mut()
            .append_pair("term", &guess.title)
            .append_pair("l", "english")
            .append_pair("cc", "US");
        let body: SearchResponse = get_json(self.client.get(url)).await?;
        let Some((name, id)) = choose(guess, &body.items) else {
            return Ok(None);
        };
        let image = self
            .image_base
            .join(&format!("{id}/header.jpg"))
            .map_err(|_| LookupError::Failed("invalid image URL".into()))?;
        Ok(Some(Found {
            title: name.to_owned(),
            year: None,
            image,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::MediaType;

    fn item(kind: &str, name: &str, id: u64) -> Item {
        Item {
            kind: Some(kind.into()),
            name: Some(name.into()),
            id: Some(id),
        }
    }

    #[test]
    fn choose_requires_an_exact_app_name() {
        let items = [
            item("app", "Hades II Original Soundtrack", 2),
            item("bundle", "Hades II", 3),
            item("app", "Hades™ II", 1145350),
        ];
        let guess = |title: &str| MediaGuess {
            kind: MediaType::Game,
            title: title.into(),
            year: None,
            season: None,
            episode: None,
            last_episode: None,
        };
        assert_eq!(
            choose(&guess("Hades II"), &items),
            Some(("Hades™ II", 1145350))
        );
        assert_eq!(choose(&guess("Hades"), &items), None);
    }
}
