//! Generic provider for ordinary HTTP/HTTPS direct-download URLs.

use super::{BoxFuture, Provider, ResolveRequest, ResolvedResource, ResourceInfo};
use crate::engine::{content_disposition_filename, filename_from_url, parse_content_range};
use crate::error::TransferError;
use crate::http_client::{classify_reqwest_error, classify_status};
use reqwest::StatusCode;
use reqwest::header;
use std::time::Duration;
use url::Url;

const INSPECT_TIMEOUT: Duration = Duration::from_secs(20);

pub struct HttpProvider {
    /// The policy-enforcing client (its resolver and redirect checks apply to
    /// inspection requests too).
    client: reqwest::Client,
}

impl HttpProvider {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

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

    /// Ask for the first byte only: the headers carry the name and size, and
    /// the body is never read.
    fn inspect<'a>(
        &'a self,
        request: ResolveRequest<'a>,
    ) -> BoxFuture<'a, Result<ResourceInfo, TransferError>> {
        Box::pin(async move {
            let response = self
                .client
                .get(request.url.clone())
                .header(header::RANGE, "bytes=0-0")
                .timeout(INSPECT_TIMEOUT)
                .send()
                .await
                .map_err(|e| classify_reqwest_error(&e))?;
            let status = response.status();
            let headers = response.headers();
            let size = match status {
                StatusCode::PARTIAL_CONTENT => parse_content_range(headers).and_then(|r| r.total),
                s if s.is_success() => response.content_length(),
                s => return Err(classify_status(s, None)),
            };
            let filename = headers
                .get(header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .and_then(content_disposition_filename)
                .and_then(|n| crate::fsutil::sanitize_filename(&n))
                .or_else(|| filename_from_url(response.url()))
                .or_else(|| filename_from_url(request.url));
            Ok(ResourceInfo {
                filename,
                size,
                size_approximate: false,
            })
        })
    }
}
