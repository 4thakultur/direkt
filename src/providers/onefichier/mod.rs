//! 1fichier.com premium downloads.
//!
//! Two ways to authenticate, configured under Settings:
//! * **Account sign-in** (preferred when configured): email and password,
//!   used like a browser session. See [`website`] and [`account`].
//! * **API key** through the official API. See [`api`].
//!
//! API mode: each transfer attempt makes exactly one API call,
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
//! blocks, and the latter is limited to one call per 5 minutes per user. The
//! confirmation step reads a file's name and size from its public page
//! instead: one throttled request, like a browser visit.

use super::{BoxFuture, Provider, ResolveRequest, ResolvedResource, ResourceInfo};
use crate::error::{AppError, ErrorKind, TransferError};
use crate::settings::SecretCell;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

mod account;
mod api;
mod website;

pub use account::{AccountView, CdnSource, CdnView, OneFichierAccount, SignInOutcome};
#[cfg(test)]
pub(crate) use website::{BROWSER_USER_AGENT, Website};

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

/// Spacing between requests to 1fichier (API and site). The documented API
/// limit is 3/s; 1fichier's admin told JDownloader developers that 1/s is fine.
pub(crate) const DEFAULT_MIN_INTERVAL: Duration = Duration::from_secs(1);
const MAX_PASSWORD_CHARS: usize = 256;

/// Serializes and spaces requests to a host.
pub(crate) struct Throttle {
    min_interval: Duration,
    last: tokio::sync::Mutex<Option<Instant>>,
}

impl Throttle {
    pub(crate) fn new(min_interval: Duration) -> Arc<Self> {
        Arc::new(Self {
            min_interval,
            last: tokio::sync::Mutex::new(None),
        })
    }

    /// Run `request` no sooner than `min_interval` after the previous one.
    /// The lock is held across the request on purpose, so requests are
    /// strictly sequential.
    pub(crate) async fn run<T>(&self, request: impl Future<Output = T>) -> T {
        let mut last = self.last.lock().await;
        if let Some(previous) = *last {
            tokio::time::sleep(self.min_interval.saturating_sub(previous.elapsed())).await;
        }
        let result = request.await;
        *last = Some(Instant::now());
        result
    }
}

pub struct OneFichierProvider {
    api: api::ApiClient,
    /// Read on every attempt, so a key changed in the settings applies at once.
    api_key: SecretCell,
    account: Arc<OneFichierAccount>,
}

impl OneFichierProvider {
    /// `api_client` is the policy-enforcing client that follows redirects.
    pub fn new(
        api_client: reqwest::Client,
        api_key: SecretCell,
        account: Arc<OneFichierAccount>,
    ) -> Self {
        Self::with_api_base(
            api_client,
            api_key,
            account,
            Url::parse(api::API_BASE).expect("constant URL"),
        )
    }

    /// Use a different API endpoint (tests).
    pub(crate) fn with_api_base(
        api_client: reqwest::Client,
        api_key: SecretCell,
        account: Arc<OneFichierAccount>,
        api_base: Url,
    ) -> Self {
        Self {
            api: api::ApiClient {
                client: api_client,
                base: api_base,
                throttle: account.throttle(),
            },
            api_key,
            account,
        }
    }
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

    fn uses_server_account(&self) -> bool {
        true
    }

    fn resolve<'a>(
        &'a self,
        request: ResolveRequest<'a>,
    ) -> BoxFuture<'a, Result<ResolvedResource, TransferError>> {
        Box::pin(async move {
            if request
                .password
                .is_some_and(|p| p.chars().count() > MAX_PASSWORD_CHARS)
            {
                return Err(TransferError::block(
                    ErrorKind::InvalidRequest,
                    "download password is too long",
                ));
            }
            let link = if self.account.is_signed_in() {
                self.account
                    .direct_link(request.url, request.password)
                    .await?
            } else if let Some(api_key) = self.api_key.get() {
                // Ask to spend CDN credits when the last check showed the
                // account uses them.
                let use_credits = self
                    .account
                    .cdn_view()
                    .is_some_and(|c| c.in_use == Some(true) && c.balance.is_some());
                self.api
                    .get_token(&api_key, request.url, request.password, use_credits)
                    .await?
            } else {
                return Err(TransferError::block(
                    ErrorKind::AuthenticationFailed,
                    "1fichier downloads need a premium account: sign in under Settings \
                     (or add an API key), then retry",
                ));
            };
            Ok(ResolvedResource {
                url: link,
                filename_hint: None,
                resumable: true,
                stable_identity: true,
                reject_html: true,
            })
        })
    }

    fn inspect<'a>(
        &'a self,
        request: ResolveRequest<'a>,
    ) -> BoxFuture<'a, Result<ResourceInfo, TransferError>> {
        Box::pin(async move {
            let info = self.account.file_info(request.url).await?;
            Ok(ResourceInfo {
                filename: info.name,
                size: info.size,
                size_approximate: true,
            })
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::error::Disposition;
    use api::{FLOOD_WAIT, IP_LOCK_WAIT, UNKNOWN_ERROR_WAIT, classify_api_error};
    use reqwest::StatusCode;

    /// A provider with no sign-in and no API key.
    pub(crate) async fn unconfigured_account() -> Arc<OneFichierAccount> {
        let store = crate::store::Store::open_in_memory().await.unwrap();
        let client =
            crate::http_client::build_no_redirect_client(Default::default(), BROWSER_USER_AGENT)
                .unwrap();
        let website = Website::new(client, Throttle::new(Duration::ZERO));
        // Unreachable API base: tests must never contact the real API.
        let api_base = Url::parse("http://127.0.0.1:9/v1/").unwrap();
        Arc::new(
            OneFichierAccount::load(store, website, api_base)
                .await
                .unwrap(),
        )
    }

    async fn provider() -> OneFichierProvider {
        let client = crate::http_client::build_client(Default::default()).unwrap();
        OneFichierProvider::new(client, SecretCell::default(), unconfigured_account().await)
    }

    #[tokio::test]
    async fn canonicalizes_file_links() {
        let p = provider().await;
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

    #[tokio::test]
    async fn matches_only_file_domains() {
        let p = provider().await;
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
        let p = provider().await;
        let url = Url::parse("https://1fichier.com/?abcde12345").unwrap();
        let err = p
            .resolve(ResolveRequest {
                url: &url,
                password: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err.disposition, Disposition::Block);
        assert!(err.message.contains("Settings"));
    }
}
