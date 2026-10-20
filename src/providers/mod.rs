//! Download providers.
//!
//! A provider recognizes the URLs it supports and resolves a submitted URL
//! into a directly downloadable HTTP resource. Host-specific behavior such as
//! link resolution, authentication and error interpretation belongs here. The
//! queue and transfer lifecycle in [`crate::engine`] stays provider-agnostic.

mod http;
mod onefichier;

pub use http::HttpProvider;
pub use onefichier::OneFichierProvider;

use crate::error::{AppError, TransferError};
use crate::settings::SecretCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use url::Url;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Input to [`Provider::resolve`].
#[derive(Debug, Clone, Copy)]
pub struct ResolveRequest<'a> {
    /// The URL as stored at submission (already canonicalized).
    pub url: &'a Url,
    /// Password for a protected file, if the user supplied one.
    pub password: Option<&'a str>,
}

/// A resource the engine can fetch with a plain HTTP GET.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedResource {
    /// Direct URL to fetch.
    pub url: Url,
    /// Filename suggested by the provider, used when the server's response
    /// does not supply one. Untrusted; sanitized before use.
    pub filename_hint: Option<String>,
    /// Whether the provider allows resuming with byte ranges. The engine still
    /// checks that the server actually honors ranges and validators.
    pub resumable: bool,
    /// The provider guarantees that every resolution of this download refers
    /// to the same file, even though the direct URL may change (e.g. a new
    /// token per attempt). The engine may then resume without an ETag or
    /// Last-Modified validator, provided the total size matches exactly.
    pub stable_identity: bool,
    /// The direct URL must return a file. An HTML response is an error page
    /// and is treated as a failed attempt instead of being saved.
    pub reject_html: bool,
}

pub trait Provider: Send + Sync {
    /// Stable identifier, persisted with each download.
    fn id(&self) -> &'static str;

    /// Whether this provider handles `url`.
    fn matches(&self, url: &Url) -> bool;

    /// Validate a submitted URL and return the form to store. Called once at
    /// submission so malformed links are rejected before any request is made.
    fn canonicalize(&self, url: &Url) -> Result<Url, AppError> {
        Ok(url.clone())
    }

    /// Whether this provider can use a download password.
    fn supports_password(&self) -> bool {
        false
    }

    /// Resolve a submitted URL into a downloadable resource. Errors carry a
    /// [`Disposition`](crate::error::Disposition): a provider can signal
    /// temporary restrictions (retry), required user action (block), or
    /// permanent failure.
    fn resolve<'a>(
        &'a self,
        request: ResolveRequest<'a>,
    ) -> BoxFuture<'a, Result<ResolvedResource, TransferError>>;
}

/// The set of available providers, consulted in order. Specific hosters go
/// first; the generic HTTP provider is the fallback.
#[derive(Clone)]
pub struct Providers {
    list: Vec<Arc<dyn Provider>>,
}

impl Providers {
    pub fn new(list: Vec<Arc<dyn Provider>>) -> Self {
        Self { list }
    }

    /// All built-in providers. `client` must be the policy-enforcing client
    /// from [`crate::http_client::build_client`].
    pub fn standard(client: reqwest::Client, onefichier_key: SecretCell) -> Self {
        Self::new(vec![
            Arc::new(OneFichierProvider::new(client, onefichier_key)),
            Arc::new(HttpProvider),
        ])
    }

    pub fn for_url(&self, url: &Url) -> Option<&Arc<dyn Provider>> {
        self.list.iter().find(|p| p.matches(url))
    }

    pub fn get(&self, id: &str) -> Option<&Arc<dyn Provider>> {
        self.list.iter().find(|p| p.id() == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn providers() -> Providers {
        let client = crate::http_client::build_client(Default::default()).unwrap();
        Providers::standard(client, SecretCell::default())
    }

    #[test]
    fn hosters_take_precedence_over_http() {
        let providers = providers();
        let p = |s: &str| providers.for_url(&Url::parse(s).unwrap()).map(|p| p.id());
        assert_eq!(p("https://example.com/file.zip"), Some("http"));
        assert_eq!(p("https://1fichier.com/?abcde12345"), Some("1fichier"));
        assert_eq!(
            p("https://www.alterupload.com/?abcde12345"),
            Some("1fichier")
        );
        // A direct CDN link from 1fichier is an ordinary HTTP download.
        assert_eq!(p("https://a-12.1fichier.com/c123456"), Some("http"));
        assert_eq!(providers.get("http").unwrap().id(), "http");
        assert!(providers.get("nope").is_none());
    }

    #[test]
    fn non_http_urls_have_no_provider() {
        let url = Url::parse("ftp://example.com/file.zip").unwrap();
        assert!(providers().for_url(&url).is_none());
    }
}
