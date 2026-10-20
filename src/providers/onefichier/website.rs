//! 1fichier website flow with an account session (email/password sign-in),
//! following the behavior of JDownloader's 1fichier plugin in website mode.
//!
//! * Sign-in: `POST https://1fichier.com/login.pl` with `mail`, `pass`,
//!   `lt=on` (long session), `other=on` (cookie on the other 1fichier domains)
//!   and `valider=ok`. `purge` (delete all other sessions, checked by default
//!   in the browser form) is deliberately never sent. Success sets the `SID`
//!   cookie. Accounts with two-factor authentication get a second form with a
//!   `tfa` field.
//! * Download: `GET <file link>` with the session, without following
//!   redirects. A premium account gets a redirect to the direct link. Failing
//!   that, `POST did=0&dlssl=SSL+Download`, or the page's password form for
//!   protected files, also answers with a redirect.
//! * Pages are requested in English (`LG=en` cookie, `lg=en` parameter)
//!   because error detection matches English messages.

use super::Throttle;
use crate::config::Secret;
use crate::error::{ErrorKind, TransferError};
use crate::http_client::classify_reqwest_error;
use regex::Regex;
use reqwest::StatusCode;
use reqwest::header::{self, HeaderMap};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use url::Url;

/// 1fichier's pages are served to browsers; present as one.
pub(crate) const BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";
const SITE_BASE: &str = "https://1fichier.com/";
const PAGE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
const SESSION_COOKIE: &str = "SID";

/// The web pages of 1fichier, accessed with a session cookie.
pub(crate) struct Website {
    client: reqwest::Client,
    base: Url,
    /// Tests: serve file pages from `base` instead of the link's own domain.
    file_pages_from_base: bool,
    throttle: Arc<Throttle>,
}

/// Result of submitting the login form.
pub(crate) enum LoginOutcome {
    Session(Secret),
    TwoFactor(PendingTwoFactor),
}

/// The second login step of an account with two-factor authentication.
#[derive(Clone)]
pub(crate) struct PendingTwoFactor {
    action: Url,
    fields: Vec<(String, String)>,
    /// Cookies 1fichier set during the first step, sent back with the code.
    cookies: Vec<(String, String)>,
}

/// CDN credits as read from 1fichier.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CdnCredits {
    /// Display text of the balance; `None` if the account shows no credits.
    pub balance: Option<String>,
    /// Whether the account is set to spend credits on its own downloads.
    pub in_use: Option<bool>,
}

pub(crate) enum LinkOutcome {
    Link(Url),
    /// The page was served without a valid session.
    SessionExpired,
}

struct Page {
    status: StatusCode,
    url: Url,
    location: Option<String>,
    cookies: Vec<(String, String)>,
    body: String,
}

impl Page {
    fn session(&self) -> Option<Secret> {
        self.cookies
            .iter()
            .find(|(name, _)| name == SESSION_COOKIE)
            .map(|(_, value)| Secret::new(value.clone()))
    }

    fn logged_in(&self) -> bool {
        self.body.contains("/logout.pl")
    }

    /// The redirect target, if this is a redirect to a downloadable link.
    fn redirect(&self) -> Option<Url> {
        if !self.status.is_redirection() {
            return None;
        }
        let target = self.url.join(self.location.as_deref()?).ok()?;
        matches!(target.scheme(), "http" | "https").then_some(target)
    }
}

impl Website {
    /// `client` must come from [`crate::http_client::build_no_redirect_client`].
    pub(crate) fn new(client: reqwest::Client, throttle: Arc<Throttle>) -> Self {
        Self {
            client,
            base: Url::parse(SITE_BASE).expect("constant URL"),
            file_pages_from_base: false,
            throttle,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_base(client: reqwest::Client, base: Url, throttle: Arc<Throttle>) -> Self {
        Self {
            client,
            base,
            file_pages_from_base: true,
            throttle,
        }
    }

    pub(crate) async fn login(
        &self,
        email: &str,
        password: &str,
    ) -> Result<LoginOutcome, TransferError> {
        let fields = [
            ("mail", email),
            ("pass", password),
            ("lt", "on"),
            ("other", "on"),
            ("valider", "ok"),
        ];
        let page = self
            .request(
                self.client
                    .post(self.url("login.pl"))
                    .body(form_body(&fields)),
                &[],
            )
            .await?;
        if let Some(session) = page.session()
            && self.session_valid(&session).await?
        {
            return Ok(LoginOutcome::Session(session));
        }
        if let Some(form) = find_form(&page.body, |f| f.has_input("tfa")) {
            let action = page
                .url
                .join(form.action.as_deref().unwrap_or(""))
                .map_err(|_| unexpected_login_page())?;
            return Ok(LoginOutcome::TwoFactor(PendingTwoFactor {
                action,
                fields: form.fields_except(&["tfa"]),
                cookies: page.cookies,
            }));
        }
        Err(login_failure(&page.body))
    }

    pub(crate) async fn submit_two_factor(
        &self,
        pending: &PendingTwoFactor,
        code: &str,
    ) -> Result<Secret, TransferError> {
        let mut fields: Vec<(&str, &str)> = pending
            .fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        fields.push(("tfa", code));
        let page = self
            .request(
                self.client
                    .post(pending.action.clone())
                    .body(form_body(&fields)),
                &pending.cookies,
            )
            .await?;
        if let Some(session) = page.session()
            && self.session_valid(&session).await?
        {
            return Ok(session);
        }
        if find_form(&page.body, |f| f.has_input("tfa")).is_some() {
            return Err(TransferError::block(
                ErrorKind::AuthenticationFailed,
                "1fichier rejected the two-factor code; sign in again with a new code",
            ));
        }
        Err(login_failure(&page.body))
    }

    /// Whether `session` is still signed in.
    pub(crate) async fn session_valid(&self, session: &Secret) -> Result<bool, TransferError> {
        let page = self
            .request(
                self.client.get(self.url("console/index.pl")),
                &session_cookie(session),
            )
            .await?;
        Ok(page.status == StatusCode::OK && page.logged_in())
    }

    /// End of the premium subscription (`YYYY-MM-DD`), or `None` for an
    /// account without a subscription.
    pub(crate) async fn premium_until(
        &self,
        session: &Secret,
    ) -> Result<Option<String>, TransferError> {
        static VALID_UNTIL: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?i)subscription is valid until\s*<[^<]*>(\d{4}-\d{2}-\d{2})").unwrap()
        });
        let page = self
            .request(
                self.client.get(self.url("console/abo.pl")),
                &session_cookie(session),
            )
            .await?;
        Ok(VALID_UNTIL.captures(&page.body).map(|c| c[1].to_owned()))
    }

    /// Ask the file page for a direct download link.
    pub(crate) async fn direct_link(
        &self,
        session: &Secret,
        file_url: &Url,
        password: Option<&str>,
    ) -> Result<LinkOutcome, TransferError> {
        let cookies = session_cookie(session);
        let page_url = self.file_page_url(file_url);
        let page = self
            .request(self.client.get(page_url.clone()), &cookies)
            .await?;
        if let Some(link) = page.redirect() {
            return Ok(classify_redirect(link));
        }
        classify_file_page(&page)?;
        if !page.logged_in() {
            return Ok(LinkOutcome::SessionExpired);
        }

        let mut post_url = page_url.clone();
        post_url.query_pairs_mut().append_pair("lg", "en");
        let page = if let Some(form) = find_download_password_form(&page.body) {
            let password = password.ok_or_else(|| {
                TransferError::block(
                    ErrorKind::AuthenticationFailed,
                    "this 1fichier file is password protected; retry with its password",
                )
            })?;
            let mut fields = form.fields_except(&["pass", "did", "save"]);
            fields.push(("pass".into(), password.into()));
            fields.push(("did".into(), "1".into()));
            let fields: Vec<(&str, &str)> = fields
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let action = match form.action.as_deref() {
                Some(a) if !a.is_empty() => page.url.join(a).unwrap_or(post_url),
                _ => post_url,
            };
            let reply = self
                .request(self.client.post(action).body(form_body(&fields)), &cookies)
                .await?;
            if reply.redirect().is_none() && find_download_password_form(&reply.body).is_some() {
                return Err(TransferError::block(
                    ErrorKind::AuthenticationFailed,
                    "1fichier rejected the download password; retry with the correct password",
                ));
            }
            reply
        } else {
            let fields = [("did", "0"), ("dlssl", "SSL Download")];
            self.request(
                self.client.post(post_url).body(form_body(&fields)),
                &cookies,
            )
            .await?
        };
        if let Some(link) = page.redirect() {
            return Ok(classify_redirect(link));
        }
        classify_file_page(&page)?;
        if !page.logged_in() {
            return Ok(LinkOutcome::SessionExpired);
        }
        Err(TransferError::retry_after(
            ErrorKind::ProviderRestricted,
            "1fichier did not provide a download link (unrecognized page)",
            Some(Duration::from_secs(5 * 60)),
        ))
    }

    pub(crate) fn throttle(&self) -> Arc<Throttle> {
        self.throttle.clone()
    }

    pub(crate) fn client(&self) -> reqwest::Client {
        self.client.clone()
    }

    /// CDN credits from the account's settings page: the balance as 1fichier
    /// writes it (e.g. "12.5 GB"), and whether downloads use them.
    pub(crate) async fn cdn_credits(&self, session: &Secret) -> Result<CdnCredits, TransferError> {
        static BALANCE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?i)>\s*Your account have ([^<>"]*?) of[^<]*credits"#).unwrap()
        });
        let page = self
            .request(
                self.client
                    .get(self.url("en/console/params.pl"))
                    .header("X-Requested-With", "XMLHttpRequest"),
                &session_cookie(session),
            )
            .await?;
        if page.status.is_redirection() || !page.status.is_success() {
            return Err(TransferError::retry(
                ErrorKind::ProviderRestricted,
                format!(
                    "could not open the 1fichier account settings (HTTP {})",
                    page.status.as_u16()
                ),
            ));
        }
        let balance = BALANCE
            .captures(&page.body)
            .map(|c| super::clean_message(&c[1]))
            .filter(|b| !b.is_empty());
        Ok(CdnCredits {
            balance,
            in_use: checkbox_checked(&page.body, "own_credit"),
        })
    }

    fn url(&self, path: &str) -> Url {
        self.base.join(path).expect("relative path joins")
    }

    fn file_page_url(&self, file_url: &Url) -> Url {
        if !self.file_pages_from_base {
            return file_url.clone();
        }
        let mut url = self.base.clone();
        url.set_query(file_url.query());
        url
    }

    /// Send a request (spaced by the shared throttle) and read the page.
    async fn request(
        &self,
        builder: reqwest::RequestBuilder,
        cookies: &[(String, String)],
    ) -> Result<Page, TransferError> {
        let mut cookie_header = String::from("LG=en");
        for (name, value) in cookies {
            cookie_header.push_str(&format!("; {name}={value}"));
        }
        let mut request = builder
            .header(header::COOKIE, cookie_header)
            .header(
                header::ACCEPT,
                "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8",
            )
            .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.5")
            .timeout(PAGE_TIMEOUT)
            .build()
            .map_err(|e| classify_reqwest_error(&e))?;
        if request.method() == reqwest::Method::POST {
            request.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/x-www-form-urlencoded"),
            );
        }
        let mut response = self
            .throttle
            .run(self.client.execute(request))
            .await
            .map_err(|e| classify_reqwest_error(&e))?;

        let status = response.status();
        let url = response.url().clone();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut jar: Vec<(String, String)> = cookies.to_vec();
        for (name, value) in set_cookies(response.headers()) {
            jar.retain(|(n, _)| *n != name);
            if let Some(value) = value {
                jar.push((name, value));
            }
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| classify_reqwest_error(&e))?
        {
            if body.len() + chunk.len() > MAX_PAGE_BYTES {
                return Err(TransferError::retry(
                    ErrorKind::Network,
                    "1fichier page is unexpectedly large",
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Page {
            status,
            url,
            location,
            cookies: jar,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }
}

fn session_cookie(session: &Secret) -> Vec<(String, String)> {
    vec![(SESSION_COOKIE.to_owned(), session.expose().to_owned())]
}

/// A redirect to the login page means the session is gone; anything else is
/// the download link.
fn classify_redirect(link: Url) -> LinkOutcome {
    if link.path().ends_with("/login.pl") {
        LinkOutcome::SessionExpired
    } else {
        LinkOutcome::Link(link)
    }
}

fn form_body(fields: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish()
}

/// Cookies set by a response: `(name, Some(value))`, or `(name, None)` for a
/// deletion (empty value, `deleted`, `Max-Age=0`, or an expiry in the past).
fn set_cookies(headers: &HeaderMap) -> Vec<(String, Option<String>)> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|cookie| {
            let mut parts = cookie.split(';');
            let (name, value) = parts.next()?.split_once('=')?;
            let (name, value) = (name.trim(), value.trim());
            let expired = parts.any(|attr| {
                let attr = attr.trim();
                let lower = attr.to_ascii_lowercase();
                if let Some(age) = lower.strip_prefix("max-age=") {
                    return age.trim().parse::<i64>().is_ok_and(|a| a <= 0);
                }
                if lower.starts_with("expires=") {
                    let date = &attr["expires=".len()..];
                    return chrono::DateTime::parse_from_rfc2822(date.trim())
                        .or_else(|_| {
                            chrono::DateTime::parse_from_str(
                                date.trim(),
                                "%a, %d-%b-%Y %H:%M:%S GMT",
                            )
                        })
                        .is_ok_and(|t| t < chrono::Utc::now());
                }
                false
            });
            let deleted = expired || value.is_empty() || value.eq_ignore_ascii_case("deleted");
            Some((name.to_owned(), (!deleted).then(|| value.to_owned())))
        })
        .collect()
}

fn unexpected_login_page() -> TransferError {
    TransferError::retry_after(
        ErrorKind::ProviderRestricted,
        "1fichier returned an unexpected sign-in page",
        Some(Duration::from_secs(5 * 60)),
    )
}

/// Why a sign-in did not succeed.
fn login_failure(body: &str) -> TransferError {
    let has = |pattern: &str| contains_ci(body, pattern);
    if has("login try per") && has("is not allowed") {
        return TransferError::retry_after(
            ErrorKind::ProviderRestricted,
            "1fichier: too many sign-in attempts; wait a few minutes before trying again",
            Some(Duration::from_secs(60)),
        );
    }
    if has("following many identification errors") {
        return TransferError::retry_after(
            ErrorKind::ProviderRestricted,
            "1fichier temporarily locked sign-ins after repeated failures (about 1 hour)",
            Some(Duration::from_secs(60 * 60)),
        );
    }
    TransferError::block(
        ErrorKind::AuthenticationFailed,
        "1fichier rejected the email or password",
    )
}

/// Map an error page served instead of a download link. `Ok(())` means no
/// known error was found.
fn classify_file_page(page: &Page) -> Result<(), TransferError> {
    static FILE_NOT_FOUND: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)>\s*File not found").unwrap());
    static SOFTWARE_ERROR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)>\s*Software error:<").unwrap());
    static PROTECTED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)>\s*Access to this file is protected|>\s*This file is protected").unwrap()
    });
    static TOO_FAST: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)>\s*Your requests are too fast").unwrap());
    static PROFESSIONAL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)>\s*Premium status must not be used on professional services").unwrap()
    });
    static MAINTENANCE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)>\s*Our services are in maintenance\.\s*Please come back after").unwrap()
    });
    static WAIT_MINUTES: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)you must wait (?:at least|up to)\s*(\d+)\s*minutes between each downloads|>\s*You must wait\s*(\d+)\s*minutes|>\s*Vous devez attendre encore\s*(\d+)\s*minutes",
        )
        .unwrap()
    });
    static IP_LOCKED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)>\s*IP Locked|>\s*Will be unlocked within 1h\.").unwrap()
    });
    static NOT_PREMIUM: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)Warning \! Without premium status, you can download only").unwrap()
    });
    static SHARED_CONNECTION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)>\s*You can use your account only for downloading from|>\s*Our services are not compatible with massively shared internet access|>\s*Be carefull? to not use simultaneously your IPv4 and IPv6 IP",
        )
        .unwrap()
    });
    static ONE_AT_A_TIME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)>\s*You already downloading (?:some|a) file|>\s*You can download only one file at a time|>\s*Please wait a few seconds before downloading new ones|>\s*You must wait for another download|>\s*Votre adresse IP ouvre trop de connexions vers le serveur",
        )
        .unwrap()
    });

    let body = page.body.as_str();
    let wait = |minutes: u64, message: &str| {
        TransferError::retry_after(
            ErrorKind::ProviderRestricted,
            format!("1fichier: {message}"),
            Some(Duration::from_secs(minutes * 60)),
        )
    };

    if FILE_NOT_FOUND.is_match(body) {
        return Err(TransferError::fail(
            ErrorKind::RemoteUnavailable,
            "file not found on 1fichier; it was deleted or the link is wrong",
        ));
    }
    if PROTECTED.is_match(body) {
        return Err(TransferError::fail(
            ErrorKind::RemoteUnavailable,
            "access to this 1fichier file is restricted by its owner",
        ));
    }
    if NOT_PREMIUM.is_match(body) {
        return Err(TransferError::block(
            ErrorKind::AuthenticationFailed,
            "the signed-in 1fichier account has no active premium subscription",
        ));
    }
    if contains_ci(body, "Your account will be unlock") {
        return Err(wait(60, "account temporarily locked for security reasons"));
    }
    if SHARED_CONNECTION.is_match(body) {
        return Err(wait(
            10,
            "the account is in use from another internet connection",
        ));
    }
    if page.status == StatusCode::FORBIDDEN && PROFESSIONAL.is_match(body) {
        return Err(wait(
            30,
            "premium downloads are refused from VPNs, proxies and hosting providers",
        ));
    }
    if IP_LOCKED.is_match(body) {
        return Err(wait(60, "this IP address is temporarily locked"));
    }
    if let Some(c) = WAIT_MINUTES.captures(body) {
        let minutes = (1..=3)
            .find_map(|i| c.get(i))
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .unwrap_or(5)
            .clamp(1, 24 * 60);
        return Err(wait(minutes, "waiting time required between downloads"));
    }
    if ONE_AT_A_TIME.is_match(body) {
        return Err(wait(
            5,
            "another download from this account or IP is still running",
        ));
    }
    if TOO_FAST.is_match(body) {
        return Err(TransferError::retry_after(
            ErrorKind::ProviderRestricted,
            "1fichier: requests too fast",
            Some(Duration::from_secs(30)),
        ));
    }
    if SOFTWARE_ERROR.is_match(body)
        || contains_ci(body, "Can't connect DB")
        || contains_ci(body, "Connexion à la base de données impossible")
    {
        return Err(wait(10, "temporary server error"));
    }
    if page.status == StatusCode::SERVICE_UNAVAILABLE && MAINTENANCE.is_match(body) {
        return Err(wait(20, "the service is in maintenance"));
    }
    match page.status.as_u16() {
        403 => Err(wait(15, "server refused the request (HTTP 403)")),
        404 => Err(wait(30, "server error (HTTP 404)")),
        s if s >= 500 => Err(TransferError::retry(
            ErrorKind::Network,
            format!("1fichier is temporarily unavailable (HTTP {s})"),
        )),
        _ => Ok(()),
    }
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

// ---------------------------------------------------------------------------
// Minimal HTML form extraction
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Form {
    action: Option<String>,
    inputs: Vec<(String, String)>,
}

impl Form {
    fn has_input(&self, name: &str) -> bool {
        self.inputs.iter().any(|(n, _)| n == name)
    }

    fn fields_except(&self, excluded: &[&str]) -> Vec<(String, String)> {
        self.inputs
            .iter()
            .filter(|(n, _)| !excluded.contains(&n.as_str()))
            .cloned()
            .collect()
    }
}

fn find_form(html: &str, predicate: impl Fn(&Form) -> bool) -> Option<Form> {
    forms(html).into_iter().find(predicate)
}

/// The password form of a protected file (not the site's login form).
fn find_download_password_form(html: &str) -> Option<Form> {
    find_form(html, |f| {
        f.has_input("pass") && !f.action.as_deref().is_some_and(|a| a.contains("login.pl"))
    })
}

fn forms(html: &str) -> Vec<Form> {
    static FORM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<form\b([^>]*)>(.*?)</form>").unwrap());
    static INPUT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<(?:input|button)\b([^>]*)>").unwrap());
    FORM.captures_iter(html)
        .map(|c| {
            let attrs = attributes(&c[1]);
            let action = attrs
                .iter()
                .find(|(k, _)| k == "action")
                .map(|(_, v)| v.clone());
            let inputs = INPUT
                .captures_iter(&c[2])
                .filter_map(|i| {
                    let attrs = attributes(&i[1]);
                    let get =
                        |key: &str| attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
                    let kind = get("type").unwrap_or_default().to_ascii_lowercase();
                    // Unchecked boxes and buttons are not submitted.
                    if matches!(kind.as_str(), "submit" | "button" | "image" | "reset")
                        || (matches!(kind.as_str(), "checkbox" | "radio")
                            && get("checked").is_none())
                    {
                        return None;
                    }
                    Some((get("name")?, get("value").unwrap_or_default()))
                })
                .collect();
            Form { action, inputs }
        })
        .collect()
}

/// Whether the checkbox named `name` is checked, if it exists.
fn checkbox_checked(html: &str, name: &str) -> Option<bool> {
    static INPUT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<input\b([^>]*)>").unwrap());
    INPUT.captures_iter(html).find_map(|c| {
        let attrs = attributes(&c[1]);
        let has = |key: &str| attrs.iter().any(|(k, _)| k == key);
        let named = attrs.iter().any(|(k, v)| k == "name" && v == name);
        named.then(|| has("checked"))
    })
}

fn attributes(tag: &str) -> Vec<(String, String)> {
    static ATTR: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)([a-z_:][-a-z0-9_:.]*)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+)))?"#)
            .unwrap()
    });
    ATTR.captures_iter(tag)
        .map(|c| {
            let value = c
                .get(2)
                .or_else(|| c.get(3))
                .or_else(|| c.get(4))
                .map(|m| unescape(m.as_str()))
                .unwrap_or_default();
            (c[1].to_ascii_lowercase(), value)
        })
        .collect()
}

fn unescape(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Disposition;

    fn page(status: u16, body: &str) -> Page {
        Page {
            status: StatusCode::from_u16(status).unwrap(),
            url: Url::parse("https://1fichier.com/?abcde").unwrap(),
            location: None,
            cookies: Vec::new(),
            body: body.to_owned(),
        }
    }

    #[test]
    fn extracts_forms_and_fields() {
        let html = r#"
            <form action="/login.pl" method="post"><input name="mail" value=""><input type="password" name="pass"></form>
            <form method="post" action='/?abc&amp;x=1'>
              <input type="hidden" name="adz" value="1.2">
              <input type="password" name="pass" value="">
              <input type="checkbox" name="save">
              <input type="checkbox" name="kept" checked>
              <input type="submit" name="submit" value="Go">
            </form>"#;
        let forms = forms(html);
        assert_eq!(forms.len(), 2);
        assert_eq!(forms[0].action.as_deref(), Some("/login.pl"));
        let pw = find_download_password_form(html).unwrap();
        assert_eq!(pw.action.as_deref(), Some("/?abc&x=1"));
        assert_eq!(
            pw.inputs,
            vec![
                ("adz".to_owned(), "1.2".to_owned()),
                ("pass".to_owned(), String::new()),
                ("kept".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn parses_set_cookie() {
        let mut headers = HeaderMap::new();
        for c in [
            "SID=abc123; path=/; domain=.1fichier.com; HttpOnly",
            "LG=en; path=/",
            "OLD=x; Max-Age=0",
            "GONE=deleted; path=/",
            "PAST=1; expires=Thu, 01 Jan 1970 00:00:00 GMT",
        ] {
            headers.append(header::SET_COOKIE, c.parse().unwrap());
        }
        let cookies = set_cookies(&headers);
        assert_eq!(cookies[0], ("SID".into(), Some("abc123".into())));
        assert_eq!(cookies[1], ("LG".into(), Some("en".into())));
        assert_eq!(cookies[2].1, None);
        assert_eq!(cookies[3].1, None);
        assert_eq!(cookies[4].1, None);
    }

    #[test]
    fn classifies_error_pages() {
        let c = |status: u16, body: &str| {
            classify_file_page(&page(status, body)).map_err(|e| (e.kind, e.disposition))
        };
        let wait = |m: u64| Disposition::Retry {
            after: Some(Duration::from_secs(m * 60)),
        };
        assert_eq!(c(200, "<p>ok</p><a href=/logout.pl>"), Ok(()));
        assert_eq!(
            c(200, "<div> File not found !</div>"),
            Err((ErrorKind::RemoteUnavailable, Disposition::Fail))
        );
        assert_eq!(
            c(
                200,
                r#"<span">Warning ! Without premium status, you can download only one file"#
            ),
            Err((ErrorKind::AuthenticationFailed, Disposition::Block))
        );
        assert_eq!(
            c(200, "<p>You must wait 12 minutes</p>").unwrap_err().1,
            wait(12)
        );
        assert_eq!(
            c(
                200,
                "you must wait at least 3 minutes between each downloads"
            )
            .unwrap_err()
            .1,
            wait(3)
        );
        assert_eq!(c(200, "<b> IP Locked</b>").unwrap_err().1, wait(60));
        assert_eq!(
            c(
                200,
                "<p> Our services are not compatible with massively shared internet access"
            )
            .unwrap_err()
            .1,
            wait(10)
        );
        assert_eq!(
            c(
                403,
                "<p> Premium status must not be used on professional services"
            )
            .unwrap_err()
            .1,
            wait(30)
        );
        assert_eq!(c(403, "nope").unwrap_err().1, wait(15));
        assert_eq!(
            c(
                503,
                "<h1> Our services are in maintenance. Please come back after 2h"
            )
            .unwrap_err()
            .1,
            wait(20)
        );
        assert_eq!(
            c(502, "").unwrap_err(),
            (ErrorKind::Network, Disposition::Retry { after: None })
        );
    }

    #[test]
    fn classifies_login_failures() {
        let rate = login_failure("<div>More than 5 login try per 10 minutes is not allowed</div>");
        assert_eq!(
            rate.disposition,
            Disposition::Retry {
                after: Some(Duration::from_secs(60))
            }
        );
        let locked =
            login_failure("For security reasons, following many identification errors, ...");
        assert_eq!(
            locked.disposition,
            Disposition::Retry {
                after: Some(Duration::from_secs(3600))
            }
        );
        let bad = login_failure("<p>Invalid email address or password</p>");
        assert_eq!(bad.disposition, Disposition::Block);
        assert_eq!(bad.kind, ErrorKind::AuthenticationFailed);
    }

    #[test]
    fn reads_cdn_settings_markup() {
        let html = r#"<p> Your account have 12.34 GB of CDN credits</p>
            <input type="checkbox" checked="checked" name="own_credit" value="1">"#;
        static BALANCE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?i)>\s*Your account have ([^<>"]*?) of[^<]*credits"#).unwrap()
        });
        assert_eq!(&BALANCE.captures(html).unwrap()[1], "12.34 GB");
        assert_eq!(checkbox_checked(html, "own_credit"), Some(true));
        assert_eq!(
            checkbox_checked(r#"<input name="own_credit" type="checkbox">"#, "own_credit"),
            Some(false)
        );
        assert_eq!(checkbox_checked("<p>nothing</p>", "own_credit"), None);
    }

    #[test]
    fn login_redirects_are_not_links() {
        let link = Url::parse("https://1fichier.com/login.pl?e=1").unwrap();
        assert!(matches!(
            classify_redirect(link),
            LinkOutcome::SessionExpired
        ));
        let link = Url::parse("https://a-7.1fichier.com/c12345").unwrap();
        assert!(matches!(classify_redirect(link), LinkOutcome::Link(_)));
    }
}
