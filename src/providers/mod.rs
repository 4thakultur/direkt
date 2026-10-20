//! Download providers.
//!
//! A provider recognizes the URLs it supports and resolves a submitted URL
//! into a directly downloadable HTTP resource. Host-specific behavior such as
//! link resolution, authentication and error interpretation belongs here. The
//! queue and transfer lifecycle in [`crate::engine`] stays provider-agnostic.

mod http;

pub use http::HttpProvider;

use crate::error::TransferError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use url::Url;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

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
}

pub trait Provider: Send + Sync {
    /// Stable identifier, persisted with each download.
    fn id(&self) -> &'static str;

    /// Whether this provider handles `url`.
    fn matches(&self, url: &Url) -> bool;

    /// Resolve a submitted URL into a downloadable resource. Errors carry a
    /// [`Disposition`](crate::error::Disposition): a provider can signal
    /// temporary restrictions (retry), required user action (block), or
    /// permanent failure.
    fn resolve<'a>(
        &'a self,
        url: &'a Url,
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

    pub fn for_url(&self, url: &Url) -> Option<&Arc<dyn Provider>> {
        self.list.iter().find(|p| p.matches(url))
    }

    pub fn get(&self, id: &str) -> Option<&Arc<dyn Provider>> {
        self.list.iter().find(|p| p.id() == id)
    }
}

impl Default for Providers {
    fn default() -> Self {
        Self::new(vec![Arc::new(HttpProvider)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_is_the_fallback() {
        let providers = Providers::default();
        let url = Url::parse("https://example.com/file.zip").unwrap();
        assert_eq!(providers.for_url(&url).unwrap().id(), "http");
        assert_eq!(providers.get("http").unwrap().id(), "http");
        assert!(providers.get("nope").is_none());
    }

    #[test]
    fn non_http_urls_have_no_provider() {
        let providers = Providers::default();
        let url = Url::parse("ftp://example.com/file.zip").unwrap();
        assert!(providers.for_url(&url).is_none());
    }
}
