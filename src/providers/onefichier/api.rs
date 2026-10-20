//! The official 1fichier API (premium API key).

use super::website::CdnCredits;
use super::{Throttle, clean_message};
use crate::config::Secret;
use crate::error::{ErrorKind, TransferError};
use crate::http_client::classify_reqwest_error;
use reqwest::StatusCode;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

pub(super) const API_BASE: &str = "https://api.1fichier.com/v1/";
const API_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

pub(super) const FLOOD_WAIT: Duration = Duration::from_secs(5 * 60);
pub(super) const IP_LOCK_WAIT: Duration = Duration::from_secs(60 * 60);
pub(super) const UNKNOWN_ERROR_WAIT: Duration = Duration::from_secs(5 * 60);

pub(super) struct ApiClient {
    pub client: reqwest::Client,
    pub base: Url,
    pub throttle: Arc<Throttle>,
}

impl ApiClient {
    pub async fn get_token(
        &self,
        api_key: &Secret,
        file_url: &Url,
        password: Option<&str>,
    ) -> Result<Url, TransferError> {
        let mut body = serde_json::json!({ "url": file_url.as_str() });
        if let Some(password) = password {
            body["pass"] = password.into();
        }
        let endpoint = self
            .base
            .join("download/get_token.cgi")
            .expect("relative path joins");

        let request = self
            .client
            .post(endpoint)
            .bearer_auth(api_key.expose())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .timeout(API_TIMEOUT);
        let response = self
            .throttle
            .run(request.send())
            .await
            .map_err(|e| classify_reqwest_error(&e))?;

        let status = response.status();
        let bytes = read_limited(response).await?;
        let parsed: Option<TokenResponse> = serde_json::from_slice(&bytes).ok();
        match parsed {
            Some(r) if r.status.eq_ignore_ascii_case("OK") => {
                let link = r.url.as_deref().and_then(|u| Url::parse(u).ok());
                match link {
                    Some(link) if matches!(link.scheme(), "http" | "https") => Ok(link),
                    _ => Err(TransferError::retry_after(
                        ErrorKind::ProviderRestricted,
                        "1fichier API returned no usable download link",
                        Some(UNKNOWN_ERROR_WAIT),
                    )),
                }
            }
            Some(r) => Err(classify_api_error(status, r.message.as_deref())),
            None => Err(classify_api_error(status, None)),
        }
    }
}

#[derive(Deserialize)]
struct UserInfoResponse {
    status: Option<String>,
    message: Option<String>,
    /// CDN credits in GB, as a number (sometimes a string).
    cdn: Option<serde_json::Value>,
    use_cdn: Option<serde_json::Value>,
}

impl ApiClient {
    /// CDN credits from `user/info.cgi`. 1fichier allows this call once per
    /// minute per IP and once per 5 minutes per account.
    pub async fn cdn_credits(&self, api_key: &Secret) -> Result<CdnCredits, TransferError> {
        let endpoint = self
            .base
            .join("user/info.cgi")
            .expect("relative path joins");
        let request = self
            .client
            .post(endpoint)
            .bearer_auth(api_key.expose())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body("{}")
            .timeout(API_TIMEOUT);
        let response = self
            .throttle
            .run(request.send())
            .await
            .map_err(|e| classify_reqwest_error(&e))?;
        let status = response.status();
        let bytes = read_limited(response).await?;
        let parsed: Option<UserInfoResponse> = serde_json::from_slice(&bytes).ok();
        match parsed {
            Some(r)
                if status.is_success()
                    && !r
                        .status
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case("KO")) =>
            {
                let gigabytes = r.cdn.as_ref().and_then(json_number);
                Ok(CdnCredits {
                    balance: gigabytes.filter(|g| *g > 0.0).map(|g| format!("{g:.2} GB")),
                    in_use: r.use_cdn.as_ref().and_then(json_number).map(|v| v >= 1.0),
                })
            }
            Some(r) => Err(classify_api_error(status, r.message.as_deref())),
            None => Err(classify_api_error(status, None)),
        }
    }
}

fn json_number(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    status: String,
    url: Option<String>,
    message: Option<String>,
}

async fn read_limited(mut response: reqwest::Response) -> Result<Vec<u8>, TransferError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| classify_reqwest_error(&e))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(TransferError::retry(
                ErrorKind::Network,
                "1fichier API response is too large",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Map a failed API call to an engine action. Order matters: more specific
/// messages are checked before their substrings (e.g. "Flood detected: IP
/// Locked" before "IP Locked").
pub(super) fn classify_api_error(status: StatusCode, message: Option<&str>) -> TransferError {
    let raw = message.map(clean_message).unwrap_or_default();
    let m = raw.to_ascii_lowercase();
    let detail = if raw.is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        format!("1fichier: {raw}")
    };
    let has = |needle: &str| m.contains(needle);

    if has("flood detected") || status == StatusCode::TOO_MANY_REQUESTS {
        return TransferError::retry_after(
            ErrorKind::ProviderRestricted,
            format!("1fichier API rate limit reached; waiting before trying again ({detail})"),
            Some(FLOOD_WAIT),
        );
    }
    if has("not authenticated") || has("no such user") {
        return TransferError::block(
            ErrorKind::AuthenticationFailed,
            format!("1fichier rejected the API key; check it under Settings ({detail})"),
        );
    }
    if has("owner locked") {
        return TransferError::block(
            ErrorKind::AuthenticationFailed,
            format!("the 1fichier account is locked ({detail})"),
        );
    }
    if has("must be a customer") {
        return TransferError::block(
            ErrorKind::AuthenticationFailed,
            format!("1fichier API downloads need an active premium subscription ({detail})"),
        );
    }
    if has("password not provided") {
        return TransferError::block(
            ErrorKind::AuthenticationFailed,
            "this 1fichier file is password protected or access restricted; retry with its password",
        );
    }
    if has("invalid password") {
        return TransferError::block(
            ErrorKind::AuthenticationFailed,
            "1fichier rejected the download password; retry with the correct password",
        );
    }
    if has("ip locked") {
        return TransferError::retry_after(
            ErrorKind::ProviderRestricted,
            format!("1fichier temporarily locked this IP address ({detail})"),
            Some(IP_LOCK_WAIT),
        );
    }
    if has("allowed at a time") {
        return TransferError::retry_after(
            ErrorKind::ProviderRestricted,
            format!("the 1fichier account is in use from too many locations ({detail})"),
            Some(FLOOD_WAIT),
        );
    }
    if has("resource not allowed") {
        return TransferError::fail(
            ErrorKind::RemoteUnavailable,
            format!("access to this 1fichier file is restricted by its owner ({detail})"),
        );
    }
    if has("resource not found") || status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
        return TransferError::fail(
            ErrorKind::RemoteUnavailable,
            format!("file not found on 1fichier; it was deleted or the link is wrong ({detail})"),
        );
    }
    if status.is_server_error() {
        return TransferError::retry(
            ErrorKind::Network,
            format!("1fichier API is temporarily unavailable ({detail})"),
        );
    }
    TransferError::retry_after(
        ErrorKind::ProviderRestricted,
        format!("1fichier API error ({detail})"),
        Some(UNKNOWN_ERROR_WAIT),
    )
}
