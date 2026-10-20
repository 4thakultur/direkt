//! 1fichier.com premium downloads through the official API.
//!
//! Each transfer attempt makes exactly one API call,
//! `POST https://api.1fichier.com/v1/download/get_token.cgi`, authenticated
//! with `Authorization: Bearer <API key>` and the body
//! `{"url": "<file link>", "pass": "<password>"}`. The response carries a
//! direct link (`{"status":"OK","url":...}`) that is valid for 5 minutes. The
//! engine then downloads it like any HTTP file. 1fichier supports Range
//! requests and allows resuming for 30 minutes by requesting a fresh token,
//! which is what a retry or resume does.
//!
//! API usage rules (from <https://1fichier.com/api.html>):
//! * Token generation is limited to customers (Premium, Premium Light, Premium
//!   GOLD, Access, CDN).
//! * At most 3 requests per second; more returns HTTP 429.
//! * Repeated error responses (401, 403, 404, 410) lead to a temporary ban
//!   of the IP address and the account. Permanent errors are therefore never
//!   retried automatically.
//!
//! The error messages handled below follow those observed and handled by
//! JDownloader's 1fichier plugin. `file/info.cgi` and `user/info.cgi` are
//! deliberately not called: JDownloader reports that the former causes IP
//! blocks, and the latter is limited to one call per 5 minutes per user.

use super::{BoxFuture, Provider, ResolveRequest, ResolvedResource};
use crate::config::Secret;
use crate::error::{AppError, ErrorKind, TransferError};
use crate::http_client::classify_reqwest_error;
use reqwest::StatusCode;
use serde::Deserialize;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use url::Url;

/// Every domain 1fichier serves file links on (official list, domain IDs 0-10).
const DOMAINS: [&str; 11] = [
    "1fichier.com",
    "alterupload.com",
    "cjoint.net",
    "desfichiers.com",
    "dfichiers.com",
    "megadl.fr",
    "mesfichiers.org",
    "piecejointe.net",
    "pjointe.com",
    "tenvoi.com",
    "dl4free.com",
];

const API_BASE: &str = "https://api.1fichier.com/v1/";
/// Spacing between API calls. The documented limit is 3/s; 1fichier's admin
/// told JDownloader developers that 1/s is fine.
const DEFAULT_MIN_INTERVAL: Duration = Duration::from_secs(1);
const API_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_PASSWORD_CHARS: usize = 256;

const FLOOD_WAIT: Duration = Duration::from_secs(5 * 60);
const IP_LOCK_WAIT: Duration = Duration::from_secs(60 * 60);
const UNKNOWN_ERROR_WAIT: Duration = Duration::from_secs(5 * 60);

pub struct OneFichierProvider {
    client: reqwest::Client,
    api_key: Option<Secret>,
    api_base: Url,
    min_interval: Duration,
    /// Time of the last API call. Held across the call itself so requests
    /// are serialized and spaced, as the API's rate limit requires.
    last_call: Mutex<Option<Instant>>,
}

impl OneFichierProvider {
    pub fn new(client: reqwest::Client, api_key: Option<Secret>) -> Self {
        Self::with_endpoint(
            client,
            api_key,
            Url::parse(API_BASE).expect("constant URL"),
            DEFAULT_MIN_INTERVAL,
        )
    }

    /// Use a different API endpoint and call spacing (tests).
    pub(crate) fn with_endpoint(
        client: reqwest::Client,
        api_key: Option<Secret>,
        api_base: Url,
        min_interval: Duration,
    ) -> Self {
        Self {
            client,
            api_key,
            api_base,
            min_interval,
            last_call: Mutex::new(None),
        }
    }

    async fn get_token(
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
            .api_base
            .join("download/get_token.cgi")
            .expect("relative path joins");

        let mut last_call = self.last_call.lock().await;
        if let Some(previous) = *last_call {
            tokio::time::sleep(self.min_interval.saturating_sub(previous.elapsed())).await;
        }
        let result = self
            .client
            .post(endpoint)
            .bearer_auth(api_key.expose())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .timeout(API_TIMEOUT)
            .send()
            .await;
        *last_call = Some(Instant::now());
        let response = result.map_err(|e| classify_reqwest_error(&e))?;
        drop(last_call);

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
fn classify_api_error(status: StatusCode, message: Option<&str>) -> TransferError {
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
            format!("1fichier rejected the API key; check providers.onefichier.api_key ({detail})"),
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

/// Make a server-provided message safe and short enough to show to users.
fn clean_message(message: &str) -> String {
    let cleaned: String = message
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    cleaned.trim().to_owned()
}

fn domain_of(url: &Url) -> Option<&'static str> {
    let host = url.host_str()?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    DOMAINS
        .iter()
        .copied()
        .find(|d| d.eq_ignore_ascii_case(host))
}

fn is_file_id(id: &str) -> bool {
    (5..=20).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

impl Provider for OneFichierProvider {
    fn id(&self) -> &'static str {
        "1fichier"
    }

    fn matches(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https") && domain_of(url).is_some()
    }

    /// Accept `http(s)://[www.]<domain>/?<id>[&...]` and return
    /// `https://<domain>/?<id>`. The original domain is kept: JDownloader
    /// notes that links on the alternate domains are not valid under
    /// 1fichier.com.
    fn canonicalize(&self, url: &Url) -> Result<Url, AppError> {
        let domain = domain_of(url).ok_or_else(|| AppError::invalid("not a 1fichier link"))?;
        let id = url
            .query()
            .and_then(|q| q.split('&').next())
            .filter(|_| url.path() == "/")
            .filter(|id| is_file_id(id))
            .ok_or_else(|| {
                AppError::invalid(format!(
                    "not a 1fichier file link; expected https://{domain}/?<file id> \
                     (5-20 lowercase letters and digits)"
                ))
            })?;
        Url::parse(&format!("https://{domain}/?{id}"))
            .map_err(|_| AppError::invalid("not a 1fichier file link"))
    }

    fn supports_password(&self) -> bool {
        true
    }

    fn resolve<'a>(
        &'a self,
        request: ResolveRequest<'a>,
    ) -> BoxFuture<'a, Result<ResolvedResource, TransferError>> {
        Box::pin(async move {
            let Some(api_key) = &self.api_key else {
                return Err(TransferError::block(
                    ErrorKind::AuthenticationFailed,
                    "1fichier downloads need a premium API key: set providers.onefichier.api_key \
                     or DIREKT_ONEFICHIER_API_KEY, restart, then retry",
                ));
            };
            if request
                .password
                .is_some_and(|p| p.chars().count() > MAX_PASSWORD_CHARS)
            {
                return Err(TransferError::block(
                    ErrorKind::InvalidRequest,
                    "download password is too long",
                ));
            }
            let link = self
                .get_token(api_key, request.url, request.password)
                .await?;
            Ok(ResolvedResource {
                url: link,
                filename_hint: None,
                resumable: true,
                stable_identity: true,
                reject_html: true,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Disposition;

    fn provider() -> OneFichierProvider {
        let client = crate::http_client::build_client(Default::default()).unwrap();
        OneFichierProvider::new(client, None)
    }

    #[test]
    fn canonicalizes_file_links() {
        let p = provider();
        let c = |s: &str| {
            p.canonicalize(&Url::parse(s).unwrap())
                .map(|u| u.to_string())
        };
        assert_eq!(
            c("https://1fichier.com/?abcde12345").unwrap(),
            "https://1fichier.com/?abcde12345"
        );
        assert_eq!(
            c("http://www.1fichier.com/?abcde").unwrap(),
            "https://1fichier.com/?abcde"
        );
        assert_eq!(
            c("https://1fichier.com/?abcde12345&af=123#frag").unwrap(),
            "https://1fichier.com/?abcde12345"
        );
        assert_eq!(
            c("https://ALTERUPLOAD.com/?xyz987").unwrap(),
            "https://alterupload.com/?xyz987"
        );
        assert_eq!(
            c("https://dl4free.com/?zzzzz").unwrap(),
            "https://dl4free.com/?zzzzz"
        );

        for bad in [
            "https://1fichier.com/",
            "https://1fichier.com/?abcd",                  // too short
            "https://1fichier.com/?abcdefghijklmnopqrstu", // too long
            "https://1fichier.com/?ABCDE12345",            // uppercase
            "https://1fichier.com/?abc-de",
            "https://1fichier.com/dir/?abcde12345",
            "https://1fichier.com/console/index.pl",
            "https://example.com/?abcde12345",
        ] {
            let err = c(bad).unwrap_err();
            assert_eq!(err.kind, ErrorKind::InvalidRequest, "{bad}");
        }
    }

    #[test]
    fn matches_only_file_domains() {
        let p = provider();
        let m = |s: &str| p.matches(&Url::parse(s).unwrap());
        for domain in DOMAINS {
            assert!(m(&format!("https://{domain}/?abcde")), "{domain}");
            assert!(m(&format!("https://www.{domain}/?abcde")), "{domain}");
        }
        assert!(
            !m("https://a-12.1fichier.com/c123"),
            "CDN links are plain HTTP"
        );
        assert!(!m("https://api.1fichier.com/v1/"));
        assert!(!m("https://not1fichier.com/?abcde"));
        assert!(!m("ftp://1fichier.com/?abcde"));
    }

    #[test]
    fn api_errors_are_classified() {
        use Disposition::*;
        let c = |status: u16, msg: &str| {
            let e = classify_api_error(StatusCode::from_u16(status).unwrap(), Some(msg));
            (e.kind, e.disposition)
        };
        let retry_in = |d: Duration| Retry { after: Some(d) };
        let auth_block = (ErrorKind::AuthenticationFailed, Block);
        let gone = (ErrorKind::RemoteUnavailable, Fail);

        assert_eq!(
            c(403, "Flood detected: IP Locked #38"),
            (ErrorKind::ProviderRestricted, retry_in(FLOOD_WAIT))
        );
        assert_eq!(
            c(403, "Flood detected: User Locked #12"),
            (ErrorKind::ProviderRestricted, retry_in(FLOOD_WAIT))
        );
        assert_eq!(
            c(429, ""),
            (ErrorKind::ProviderRestricted, retry_in(FLOOD_WAIT))
        );
        assert_eq!(c(401, "Not authenticated #247"), auth_block);
        assert_eq!(c(401, "No such user #18"), auth_block);
        assert_eq!(c(403, "Owner locked #3"), auth_block);
        assert_eq!(
            c(403, "Must be a customer (Premium, Access) #200"),
            auth_block
        );
        assert_eq!(
            c(403, "Password not provided. Resource not allowed #632"),
            auth_block
        );
        assert_eq!(
            c(403, "Invalid password. Resource not allowed #631"),
            auth_block
        );
        assert_eq!(
            c(403, "IP Locked #7"),
            (ErrorKind::ProviderRestricted, retry_in(IP_LOCK_WAIT))
        );
        assert_eq!(
            c(403, "Only 2 locations allowed at a time #9"),
            (ErrorKind::ProviderRestricted, retry_in(FLOOD_WAIT))
        );
        assert_eq!(c(403, "Resource not allowed #634"), gone);
        assert_eq!(c(404, "Resource not found #469"), gone);
        assert_eq!(c(410, ""), gone);
        assert_eq!(c(503, ""), (ErrorKind::Network, Retry { after: None }));
        assert_eq!(
            c(400, "Something new #1"),
            (ErrorKind::ProviderRestricted, retry_in(UNKNOWN_ERROR_WAIT))
        );

        let e = classify_api_error(StatusCode::NOT_FOUND, Some("Resource not found #469"));
        assert!(
            e.message.contains("1fichier: Resource not found #469"),
            "{}",
            e.message
        );
    }

    #[test]
    fn server_messages_are_sanitized() {
        let e = classify_api_error(
            StatusCode::BAD_REQUEST,
            Some(&format!("evil\n\u{1b}[31m{}", "x".repeat(500))),
        );
        assert!(!e.message.contains('\n') && !e.message.contains('\u{1b}'));
        assert!(e.message.len() < 300);
    }

    #[tokio::test]
    async fn missing_api_key_blocks_without_calling_the_api() {
        let p = provider();
        let url = Url::parse("https://1fichier.com/?abcde12345").unwrap();
        let err = p
            .resolve(ResolveRequest {
                url: &url,
                password: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err.disposition, Disposition::Block);
        assert!(err.message.contains("DIREKT_ONEFICHIER_API_KEY"));
    }
}
