//! Outbound HTTP client that enforces the [`NetworkPolicy`] on every hop.
//!
//! Three layers stop requests to forbidden destinations:
//!
//! 1. A DNS resolver that drops forbidden addresses and fails if none remain.
//!    The connector dials exactly the addresses it returns, so a hostname
//!    cannot be re-resolved to a different address after the check (DNS
//!    rebinding).
//! 2. [`check_destination`], which runs before the first request and on every
//!    redirect hop. hyper never asks the resolver about IP-literal hosts, so
//!    those have to be checked here.
//! 3. [`check_peer`], which checks the address the response actually came from.
//!    This is defence in depth.
//!
//! System proxy settings are ignored on purpose: with a proxy, the resolver
//! would check the proxy's address instead of the destination's.

use crate::error::{ErrorKind, TransferError};
use crate::net::NetworkPolicy;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{Client, StatusCode, redirect};
use std::error::Error as StdError;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use url::{Host, Url};

const MAX_REDIRECTS: usize = 10;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum time without receiving any data before a transfer is aborted.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The destination is not permitted by the network policy.
#[derive(Debug, thiserror::Error)]
#[error("destination address is not allowed by the network policy")]
pub struct BlockedDestination;

#[derive(Debug, thiserror::Error)]
#[error("too many redirects")]
struct TooManyRedirects;

struct PolicyResolver {
    policy: NetworkPolicy,
}

impl Resolve for PolicyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = self.policy.clone();
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let resolved: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed: Vec<SocketAddr> = resolved
                .iter()
                .copied()
                .filter(|addr| policy.is_allowed(addr.ip()))
                .collect();
            if allowed.is_empty() {
                tracing::warn!(
                    host,
                    resolved = resolved.len(),
                    "blocked request: host resolves only to forbidden addresses"
                );
                return Err(Box::new(BlockedDestination) as Box<dyn StdError + Send + Sync>);
            }
            Ok(Box::new(allowed.into_iter()) as Addrs)
        })
    }
}

pub fn build_client(policy: NetworkPolicy) -> reqwest::Result<Client> {
    let redirect_policy = policy.clone();
    Client::builder()
        .dns_resolver(PolicyResolver { policy })
        .no_proxy()
        .redirect(redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error(TooManyRedirects);
            }
            match check_destination(attempt.url(), &redirect_policy) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(e),
            }
        }))
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .user_agent(concat!("Direkt/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Check a URL's scheme and, for IP-literal hosts, its address. Domain names
/// are checked by the resolver at connection time.
pub fn check_destination(url: &Url, policy: &NetworkPolicy) -> Result<(), BlockedDestination> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(BlockedDestination);
    }
    let ip = match url.host() {
        Some(Host::Ipv4(v4)) => IpAddr::V4(v4),
        Some(Host::Ipv6(v6)) => IpAddr::V6(v6),
        Some(Host::Domain(_)) => return Ok(()),
        None => return Err(BlockedDestination),
    };
    if policy.is_allowed(ip) {
        Ok(())
    } else {
        Err(BlockedDestination)
    }
}

/// Verify the address a response was actually received from.
pub fn check_peer(peer: Option<SocketAddr>, policy: &NetworkPolicy) -> Result<(), TransferError> {
    match peer {
        Some(addr) if !policy.is_allowed(addr.ip()) => Err(blocked_error()),
        _ => Ok(()),
    }
}

fn blocked_error() -> TransferError {
    TransferError::fail(
        ErrorKind::InvalidRequest,
        "destination resolves to a private, local, or reserved address, which is not allowed",
    )
}

fn chain_contains<T: StdError + 'static>(err: &(dyn StdError + 'static)) -> bool {
    let mut current = Some(err);
    while let Some(e) = current {
        if e.is::<T>() {
            return true;
        }
        // `io::Error::source` skips the error it wraps; look inside explicitly.
        if let Some(inner) = e
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref())
            && chain_contains::<T>(inner)
        {
            return true;
        }
        current = e.source();
    }
    false
}

/// Classify a reqwest error from sending a request or reading its body.
pub fn classify_reqwest_error(err: &reqwest::Error) -> TransferError {
    if chain_contains::<BlockedDestination>(err) {
        return blocked_error();
    }
    if chain_contains::<TooManyRedirects>(err) {
        return TransferError::fail(ErrorKind::RemoteUnavailable, "too many redirects");
    }
    let message = if err.is_timeout() {
        "connection timed out"
    } else if err.is_connect() {
        "could not connect to server"
    } else if err.is_body() || err.is_decode() {
        "connection interrupted during transfer"
    } else {
        "network error"
    };
    // Log the source chain only: reqwest's own message includes the full URL.
    let detail = format!("{}", ErrorChain(err));
    tracing::debug!(error = %detail, "network error");
    TransferError::retry(ErrorKind::Network, message)
}

/// Displays an error with its sources, without the request URL.
struct ErrorChain<'a>(&'a reqwest::Error);

impl std::fmt::Display for ErrorChain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut err: &(dyn StdError + 'static) = self.0;
        // reqwest's own Display includes the URL; start from its source.
        let mut first = true;
        while let Some(source) = err.source() {
            if !first {
                f.write_str(": ")?;
            }
            write!(f, "{source}")?;
            first = false;
            err = source;
        }
        if first {
            f.write_str("request failed")?;
        }
        Ok(())
    }
}

/// Classify an unsuccessful HTTP status.
pub fn classify_status(status: StatusCode, retry_after: Option<Duration>) -> TransferError {
    let code = status.as_u16();
    match code {
        429 => TransferError::retry_after(
            ErrorKind::ProviderRestricted,
            "server is rate limiting requests (HTTP 429)",
            retry_after,
        ),
        408 | 500 | 502 | 503 | 504 => TransferError::retry_after(
            ErrorKind::Network,
            format!("server temporarily unavailable (HTTP {code})"),
            retry_after,
        ),
        401 | 403 => TransferError::fail(
            ErrorKind::AuthenticationFailed,
            format!("server denied access (HTTP {code})"),
        ),
        404 | 410 => TransferError::fail(
            ErrorKind::RemoteUnavailable,
            format!("file not found on server (HTTP {code})"),
        ),
        _ if status.is_server_error() => {
            TransferError::retry(ErrorKind::Network, format!("server error (HTTP {code})"))
        }
        _ => TransferError::fail(
            ErrorKind::RemoteUnavailable,
            format!("server rejected the request (HTTP {code})"),
        ),
    }
}

/// Parse a `Retry-After` header (delay in seconds or an HTTP date).
pub fn parse_retry_after(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    (at.with_timezone(&chrono::Utc) - now).to_std().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Disposition;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn destination_checks() {
        let policy = NetworkPolicy::default();
        assert!(check_destination(&url("https://example.com/x"), &policy).is_ok());
        assert!(check_destination(&url("https://1.1.1.1/x"), &policy).is_ok());
        assert!(check_destination(&url("http://127.0.0.1/x"), &policy).is_err());
        assert!(check_destination(&url("http://[::1]/x"), &policy).is_err());
        assert!(check_destination(&url("http://169.254.169.254/"), &policy).is_err());
        assert!(check_destination(&url("ftp://example.com/x"), &policy).is_err());
        assert!(check_destination(&url("file:///etc/passwd"), &policy).is_err());
    }

    #[tokio::test]
    async fn resolver_drops_forbidden_addresses() {
        let resolver = PolicyResolver {
            policy: NetworkPolicy::default(),
        };
        let err = resolver
            .resolve("localhost".parse().unwrap())
            .await
            .err()
            .expect("localhost must be blocked");
        assert!(err.is::<BlockedDestination>());

        let resolver = PolicyResolver {
            policy: NetworkPolicy::new(vec!["127.0.0.0/8".parse().unwrap()]),
        };
        let addrs: Vec<_> = resolver
            .resolve("localhost".parse().unwrap())
            .await
            .unwrap()
            .collect();
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|a| a.ip().is_loopback() && a.is_ipv4()));
    }

    #[test]
    fn status_classification() {
        let d = |code: u16| classify_status(StatusCode::from_u16(code).unwrap(), None).disposition;
        assert_eq!(d(503), Disposition::Retry { after: None });
        assert_eq!(d(500), Disposition::Retry { after: None });
        assert_eq!(d(507), Disposition::Retry { after: None });
        assert_eq!(d(429), Disposition::Retry { after: None });
        assert_eq!(d(404), Disposition::Fail);
        assert_eq!(d(403), Disposition::Fail);
        assert_eq!(d(400), Disposition::Fail);
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS, None).kind,
            ErrorKind::ProviderRestricted
        );
        assert_eq!(
            classify_status(
                StatusCode::SERVICE_UNAVAILABLE,
                Some(Duration::from_secs(9))
            )
            .disposition,
            Disposition::Retry {
                after: Some(Duration::from_secs(9))
            }
        );
    }

    #[test]
    fn retry_after_parsing() {
        let now = chrono::DateTime::parse_from_rfc3339("2015-10-21T07:28:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:30:00 GMT", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:00:00 GMT", now),
            None
        );
        assert_eq!(parse_retry_after("soon", now), None);
    }
}
