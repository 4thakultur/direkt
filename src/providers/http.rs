//! Generic provider for ordinary HTTP/HTTPS direct-download URLs.

use super::{BoxFuture, Provider, ResolvedResource};
use crate::error::TransferError;
use url::Url;

pub struct HttpProvider;

impl Provider for HttpProvider {
    fn id(&self) -> &'static str {
        "http"
    }

    fn matches(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https")
    }

    fn resolve<'a>(
        &'a self,
        url: &'a Url,
    ) -> BoxFuture<'a, Result<ResolvedResource, TransferError>> {
        Box::pin(async move {
            Ok(ResolvedResource {
                url: url.clone(),
                filename_hint: None,
                resumable: true,
            })
        })
    }
}
