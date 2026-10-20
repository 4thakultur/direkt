//! Generic provider for ordinary HTTP/HTTPS direct-download URLs.

use super::{BoxFuture, Provider, ResolveRequest, ResolvedResource};
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
        request: ResolveRequest<'a>,
    ) -> BoxFuture<'a, Result<ResolvedResource, TransferError>> {
        Box::pin(async move {
            Ok(ResolvedResource {
                url: request.url.clone(),
                filename_hint: None,
                resumable: true,
                stable_identity: false,
                reject_html: false,
            })
        })
    }
}
