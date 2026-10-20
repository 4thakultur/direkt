//! Submitted-URL validation and outbound network policy (SSRF protection).
//!
//! Validation happens in two places:
//!
//! 1. At submission time, [`validate_submission_url`] rejects malformed URLs,
//!    unsupported schemes, embedded credentials, and IP-literal hosts that the
//!    [`NetworkPolicy`] forbids.
//! 2. At connection time, every resolved address and every redirect target is
//!    checked again with [`NetworkPolicy::is_allowed`]. A hostname that passed
//!    step 1 is never trusted on its own, because DNS can change between
//!    submission and connection (DNS rebinding).

use crate::error::{AppError, AppResult};
use ipnet::IpNet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use url::{Host, Url};

const MAX_URL_LEN: usize = 8 * 1024;

/// Decides which remote IP addresses Direkt may connect to.
///
/// Public unicast addresses are always allowed. Loopback, private, link-local
/// (including the cloud metadata endpoint), CGNAT, and other special-purpose
/// ranges are denied unless they fall inside an explicitly configured
/// `allowed_private_networks` entry. Unspecified, broadcast, and multicast
/// addresses are always denied.
#[derive(Debug, Clone, Default)]
pub struct NetworkPolicy {
    allowed_private: Vec<IpNet>,
}

impl NetworkPolicy {
    pub fn new(allowed_private: Vec<IpNet>) -> Self {
        Self { allowed_private }
    }

    pub fn is_allowed(&self, ip: IpAddr) -> bool {
        let ip = canonicalize(ip);
        if never_allowed(ip) {
            return false;
        }
        if is_global(ip) {
            return true;
        }
        self.allowed_private.iter().any(|net| net.contains(&ip))
    }
}

/// Map IPv6 encodings of IPv4 addresses (mapped, compatible, NAT64, 6to4) to
/// the embedded IPv4 address so they cannot be used to bypass IPv4 rules.
fn canonicalize(ip: IpAddr) -> IpAddr {
    let IpAddr::V6(v6) = ip else { return ip };
    if let Some(v4) = v6.to_ipv4_mapped() {
        return IpAddr::V4(v4);
    }
    let seg = v6.segments();
    let embedded_tail = || {
        Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            seg[6] as u8,
            (seg[7] >> 8) as u8,
            seg[7] as u8,
        )
    };
    // Deprecated IPv4-compatible ::a.b.c.d (but not :: or ::1).
    if seg[..6].iter().all(|&s| s == 0) && !(seg[6] == 0 && seg[7] <= 1) {
        return IpAddr::V4(embedded_tail());
    }
    // NAT64 well-known prefix 64:ff9b::/96.
    if seg[0] == 0x64 && seg[1] == 0xff9b && seg[2..6].iter().all(|&s| s == 0) {
        return IpAddr::V4(embedded_tail());
    }
    // 6to4 2002:AABB:CCDD::/48 embeds the IPv4 address in segments 1-2.
    if seg[0] == 0x2002 {
        return IpAddr::V4(Ipv4Addr::new(
            (seg[1] >> 8) as u8,
            seg[1] as u8,
            (seg[2] >> 8) as u8,
            seg[2] as u8,
        ));
    }
    ip
}

fn never_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_unspecified() || v4.is_broadcast() || v4.is_multicast(),
        IpAddr::V6(v6) => v6.is_unspecified() || v6.is_multicast(),
    }
}

fn is_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_global_v4(v4),
        IpAddr::V6(v6) => is_global_v6(v6),
    }
}

fn is_global_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0                                   // "this network" 0.0.0.0/8
        || ip.is_loopback()                    // 127.0.0.0/8
        || ip.is_private()                     // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()                  // 169.254/16 (incl. cloud metadata)
        || (a == 100 && (b & 0xc0) == 64)      // CGNAT 100.64.0.0/10
        || (a == 192 && b == 0 && c == 0)      // IETF protocol assignments 192.0.0.0/24
        || ip.is_documentation()               // 192.0.2/24, 198.51.100/24, 203.0.113/24
        || (a == 198 && (b & 0xfe) == 18)      // benchmarking 198.18.0.0/15
        || a >= 240                            // reserved 240/4 and broadcast
        || ip.is_multicast())
}

fn is_global_v6(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (seg[0] & 0xfe00) == 0xfc00          // unique local fc00::/7
        || (seg[0] & 0xffc0) == 0xfe80          // link-local fe80::/10
        || (seg[0] & 0xffc0) == 0xfec0          // deprecated site-local fec0::/10
        || (seg[0] == 0x2001 && seg[1] == 0x0db8) // documentation 2001:db8::/32
        || (seg[0] == 0x2001 && seg[1] == 0)    // Teredo 2001::/32
        || (seg[0] == 0x0100 && seg[1..4] == [0, 0, 0]) // discard-only 100::/64
        || seg[0] == 0) // remaining ::/16 special space
}

/// Validate a URL submitted by a user and return the parsed form.
pub fn validate_submission_url(raw: &str, policy: &NetworkPolicy) -> AppResult<Url> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AppError::invalid("URL must not be empty"));
    }
    if raw.len() > MAX_URL_LEN {
        return Err(AppError::invalid(format!(
            "URL is too long (maximum {MAX_URL_LEN} characters)"
        )));
    }
    let url = Url::parse(raw).map_err(|e| AppError::invalid(format!("invalid URL: {e}")))?;

    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(AppError::invalid(format!(
                "unsupported URL scheme '{other}'; only http and https are allowed"
            )));
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AppError::invalid(
            "URLs with embedded credentials are not allowed; configure provider credentials instead",
        ));
    }

    let ip = match url.host() {
        None => return Err(AppError::invalid("URL must include a host")),
        Some(Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
        Some(Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
        Some(Host::Domain(_)) => None,
    };
    if let Some(ip) = ip
        && !policy.is_allowed(ip)
    {
        return Err(AppError::invalid(
            "URL points to a private, local, or reserved address, which is not allowed",
        ));
    }

    Ok(url)
}

/// A representation of `url` safe for logs and UI: scheme, host, port, and
/// path only. Query strings and fragments often carry tokens and are dropped.
pub fn redact_url(url: &Url) -> String {
    let mut out = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
    if let Some(port) = url.port() {
        out.push_str(&format!(":{port}"));
    }
    out.push_str(url.path());
    if url.query().is_some() {
        out.push_str("?…");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn public_addresses_allowed() {
        let policy = NetworkPolicy::default();
        for addr in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700:4700::1111",
        ] {
            assert!(policy.is_allowed(ip(addr)), "{addr} should be allowed");
        }
    }

    #[test]
    fn special_addresses_denied_by_default() {
        let policy = NetworkPolicy::default();
        for addr in [
            "0.0.0.0",
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254", // cloud metadata
            "100.64.0.1",
            "192.0.0.1",
            "198.18.0.1",
            "192.0.2.1",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "::",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "2001::1",
            "100::1",
            // IPv4 smuggled through IPv6 encodings.
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::a00:1",   // NAT64 10.0.0.1
            "2002:7f00:1::1",   // 6to4 127.0.0.1
            "2002:a9fe:a9fe::", // 6to4 169.254.169.254
        ] {
            assert!(!policy.is_allowed(ip(addr)), "{addr} should be denied");
        }
    }

    #[test]
    fn mapped_public_v4_allowed() {
        let policy = NetworkPolicy::default();
        assert!(policy.is_allowed(ip("::ffff:8.8.8.8")));
        assert!(policy.is_allowed(ip("64:ff9b::808:808")));
    }

    #[test]
    fn allowlist_permits_only_listed_private_ranges() {
        let policy = NetworkPolicy::new(vec!["192.168.1.0/24".parse().unwrap()]);
        assert!(policy.is_allowed(ip("192.168.1.50")));
        assert!(policy.is_allowed(ip("::ffff:192.168.1.50")));
        assert!(!policy.is_allowed(ip("192.168.2.50")));
        assert!(!policy.is_allowed(ip("127.0.0.1")));
        assert!(!policy.is_allowed(ip("169.254.169.254")));
    }

    #[test]
    fn allowlist_cannot_enable_multicast_or_unspecified() {
        let policy =
            NetworkPolicy::new(vec!["0.0.0.0/0".parse().unwrap(), "::/0".parse().unwrap()]);
        assert!(policy.is_allowed(ip("127.0.0.1")));
        assert!(!policy.is_allowed(ip("0.0.0.0")));
        assert!(!policy.is_allowed(ip("224.0.0.1")));
        assert!(!policy.is_allowed(ip("255.255.255.255")));
        assert!(!policy.is_allowed(ip("::")));
    }

    #[test]
    fn accepts_ordinary_urls() {
        let policy = NetworkPolicy::default();
        for raw in [
            "https://example.com/file.mkv",
            "http://example.com:8080/a/b?token=x",
            "  https://cdn.example.org/x.zip  ",
            "https://8.8.8.8/file",
        ] {
            assert!(validate_submission_url(raw, &policy).is_ok(), "{raw}");
        }
    }

    #[test]
    fn rejects_bad_urls() {
        let policy = NetworkPolicy::default();
        for raw in [
            "",
            "not a url",
            "ftp://example.com/file",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "gopher://example.com/",
            "https://user:pass@example.com/file",
            "https://user@example.com/file",
            "http://127.0.0.1/",
            "http://localhost.:80@127.0.0.1/",
            "http://[::1]/",
            "http://[::ffff:7f00:1]/",
            "http://169.254.169.254/latest/meta-data/",
            "http://0x7f.1/",       // WHATWG parsing normalizes to 127.0.0.1
            "http://2130706433/",   // decimal 127.0.0.1
            "http://017700000001/", // octal 127.0.0.1
            "http://0/",
        ] {
            let err = validate_submission_url(raw, &policy).unwrap_err();
            assert_eq!(err.kind, crate::error::ErrorKind::InvalidRequest, "{raw}");
        }
    }

    #[test]
    fn rejects_overlong_urls() {
        let raw = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        assert!(validate_submission_url(&raw, &NetworkPolicy::default()).is_err());
    }

    #[test]
    fn redaction_drops_query_and_fragment() {
        let url = Url::parse("https://example.com:8443/dl/file.mkv?token=secret#frag").unwrap();
        let shown = redact_url(&url);
        assert_eq!(shown, "https://example.com:8443/dl/file.mkv?…");
        assert!(!shown.contains("secret"));

        let url = Url::parse("https://example.com/file.mkv").unwrap();
        assert_eq!(redact_url(&url), "https://example.com/file.mkv");
    }
}
