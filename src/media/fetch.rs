//! HTTP plumbing shared by the metadata sources. Requests go through the
//! policy-enforcing client; errors never carry request URLs, which may
//! contain an API key.

use reqwest::{Client, StatusCode};
use std::time::Duration;
use url::Url;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest artwork accepted; a backdrop or Steam header is usually < 200 KiB.
pub const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
/// Largest search response accepted (a page of results is a few KiB).
pub const MAX_SEARCH_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LookupError {
    #[error("the metadata service rejected the API key")]
    Unauthorized,
    #[error("the metadata service is rate limiting requests")]
    RateLimited,
    #[error("metadata lookup failed: {0}")]
    Failed(String),
}

/// A confident match from a metadata source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub title: String,
    pub year: Option<u16>,
    pub image: Url,
}

/// Sniff the image type from its first bytes.
pub fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else {
        None
    }
}

pub async fn get_json<T: serde::de::DeserializeOwned>(
    request: reqwest::RequestBuilder,
) -> Result<T, LookupError> {
    let response = request
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(request_error)?;
    check_status(response.status())?;
    let bytes = read_capped(response, MAX_SEARCH_BYTES).await?;
    serde_json::from_slice(&bytes).map_err(|_| LookupError::Failed("unexpected response".into()))
}

pub async fn fetch_image(client: &Client, url: Url) -> Result<Vec<u8>, LookupError> {
    let response = client
        .get(url)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(request_error)?;
    check_status(response.status())?;
    let bytes = read_capped(response, MAX_IMAGE_BYTES).await?;
    if image_type(&bytes).is_none() {
        return Err(LookupError::Failed("not a JPEG or PNG image".into()));
    }
    Ok(bytes)
}

/// Read a response body, failing once it exceeds `max` bytes.
async fn read_capped(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>, LookupError> {
    if response.content_length().is_some_and(|n| n > max as u64) {
        return Err(LookupError::Failed("response too large".into()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if bytes.len() + chunk.len() > max {
            return Err(LookupError::Failed("response too large".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn check_status(status: StatusCode) -> Result<(), LookupError> {
    match status {
        s if s.is_success() => Ok(()),
        StatusCode::UNAUTHORIZED => Err(LookupError::Unauthorized),
        StatusCode::TOO_MANY_REQUESTS => Err(LookupError::RateLimited),
        s => Err(LookupError::Failed(format!("HTTP {}", s.as_u16()))),
    }
}

fn request_error(err: reqwest::Error) -> LookupError {
    // The classified message never includes the URL (and so the key).
    LookupError::Failed(crate::http_client::classify_reqwest_error(&err).message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_types_are_sniffed() {
        assert_eq!(image_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(image_type(b"<html>"), None);
    }
}
