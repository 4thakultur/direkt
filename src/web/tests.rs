//! HTTP-level tests for the API, authentication, and UI, driving the router
//! in-process. The engine is not started, so submitted downloads stay queued
//! and no network access happens.

use super::*;
use crate::domain::PostState;
use crate::engine::{Engine, EngineSettings};
use crate::net::NetworkPolicy;
use crate::providers::Providers;
use axum::body::Body;
use axum::http::Request as HttpRequest;
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";

struct TestApp {
    router: Router,
    auth: AuthService,
    store: Store,
    _dir: tempfile::TempDir,
    /// Base URL of a local file server, for apps built with `local_files`.
    files: String,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.body))
    }

    fn error_kind(&self) -> String {
        self.json()["error"]["kind"].as_str().unwrap().to_owned()
    }

    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .map(|v| v.to_str().unwrap())
            .unwrap_or("")
    }
}

impl TestApp {
    async fn new() -> Self {
        Self::build(None).await
    }

    async fn build(env_onefichier_key: Option<crate::config::Secret>) -> Self {
        Self::build_with(env_onefichier_key, false).await
    }

    /// `local_files`: allow loopback downloads and start a local file server,
    /// so previews can inspect links without leaving the machine.
    async fn build_with(
        env_onefichier_key: Option<crate::config::Secret>,
        local_files: bool,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().await.unwrap();
        let auth = AuthService::new(store.clone(), dir.path()).await.unwrap();
        let (policy, files) = if local_files {
            (
                NetworkPolicy::new(vec!["127.0.0.0/8".parse().unwrap()]),
                file_server().await,
            )
        } else {
            (NetworkPolicy::default(), String::new())
        };
        let client = crate::http_client::build_client(policy.clone()).unwrap();
        let account = crate::providers::unconfigured_onefichier_account().await;
        let limits = crate::engine::Limits::new(1);
        let settings = crate::settings::SettingsService::load(
            store.clone(),
            crate::settings::EnvKeys {
                onefichier: env_onefichier_key,
                tmdb: None,
                ..Default::default()
            },
            account.clone(),
            limits.clone(),
        )
        .await
        .unwrap();
        let providers = Providers::new(vec![
            Arc::new(LockedProvider),
            Arc::new(crate::providers::OneFichierProvider::new(
                client.clone(),
                settings.onefichier_key(),
                account,
            )),
            Arc::new(crate::providers::HttpProvider::new(client.clone())),
        ]);
        let engine_settings = EngineSettings {
            download_dir: dir.path().join("dl"),
            incomplete_dir: dir.path().join("inc"),
            max_concurrent: 1,
            max_retries: 1,
            retry_base_delay: Duration::from_secs(1),
            retry_max_delay: Duration::from_secs(1),
            min_free_bytes: 0,
        };
        let engine = Engine::with_limits(
            store.clone(),
            providers.clone(),
            client.clone(),
            policy.clone(),
            engine_settings,
            limits,
        );
        // TMDB and Steam are mocked; the TMDB key starts unset.
        let (metadata, _) = crate::media::artwork_tests::mock_server().await;
        let artwork = crate::media::artwork_tests::service(
            &metadata,
            dir.path().join("artwork"),
            settings.tmdb_key(),
        );
        let downloads =
            DownloadService::new(store.clone(), engine, providers, policy, artwork.clone());
        let router = router(AppState {
            downloads,
            auth: auth.clone(),
            settings,
            artwork,
            store: store.clone(),
            secure_cookies: false,
        });
        Self {
            router,
            auth,
            store,
            _dir: dir,
            files,
        }
    }

    /// Complete setup and return a session cookie header value.
    async fn with_admin() -> (Self, String) {
        Self::with_admin_and_env_key(None).await
    }

    async fn with_admin_and_env_key(key: Option<crate::config::Secret>) -> (Self, String) {
        Self::admin(Self::build(key).await).await
    }

    /// With an administrator and a local file server.
    async fn with_admin_local() -> (Self, String) {
        Self::admin(Self::build_with(None, true).await).await
    }

    async fn admin(app: Self) -> (Self, String) {
        let token = app.auth.read_setup_token();
        let session = app
            .auth
            .setup(IpAddr::V4(Ipv4Addr::LOCALHOST), &token, "admin", PASSWORD)
            .await
            .unwrap();
        let cookie = format!("{SESSION_COOKIE}={}", session.token);
        (app, cookie)
    }

    async fn send(&self, request: HttpRequest<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn get(&self, uri: &str, auth: &[(&str, &str)]) -> Reply {
        let mut builder = HttpRequest::get(uri);
        for (k, v) in auth {
            builder = builder.header(*k, *v);
        }
        self.send(builder.body(Body::empty()).unwrap()).await
    }

    async fn post_json(&self, uri: &str, auth: &[(&str, &str)], body: Value) -> Reply {
        let mut builder = HttpRequest::post(uri).header(header::CONTENT_TYPE, "application/json");
        for (k, v) in auth {
            builder = builder.header(*k, *v);
        }
        self.send(builder.body(Body::from(body.to_string())).unwrap())
            .await
    }

    async fn post_form(&self, uri: &str, headers: &[(&str, &str)], form: &str) -> Reply {
        let mut builder = HttpRequest::post(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        self.send(builder.body(Body::from(form.to_owned())).unwrap())
            .await
    }
}

/// A hoster with password-protected files at `https://locked.test/…`. Its
/// links are inspected without any request, unlike 1fichier's.
struct LockedProvider;

impl crate::providers::Provider for LockedProvider {
    fn id(&self) -> &'static str {
        "locked"
    }

    fn matches(&self, url: &url::Url) -> bool {
        url.host_str() == Some("locked.test")
    }

    fn supports_password(&self) -> bool {
        true
    }

    fn resolve<'a>(
        &'a self,
        _request: crate::providers::ResolveRequest<'a>,
    ) -> crate::providers::BoxFuture<
        'a,
        Result<crate::providers::ResolvedResource, crate::error::TransferError>,
    > {
        Box::pin(async {
            Err(crate::error::TransferError::block(
                crate::error::ErrorKind::RemoteUnavailable,
                "test hoster",
            ))
        })
    }
}

impl TestApp {
    /// Click Download on `url` and return the confirmation's preview id.
    async fn preview_id(&self, auth: &[(&str, &str)], url: &str) -> String {
        let r = self
            .post_form(
                "/ui/downloads/preview",
                auth,
                &format!("url={}", encode(url)),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        preview_id_in(&r.body)
    }
}

fn preview_id_in(html: &str) -> String {
    html.split("name=\"preview_id\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("no confirmation menu in: {html}"))
        .to_owned()
}

fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// Serves `/files/{name}` as a 14,100,000-byte file that honours `Range`,
/// and 404 for anything else.
async fn file_server() -> String {
    use axum::routing::get;
    async fn file(headers: HeaderMap) -> Response {
        const SIZE: u64 = 14_100_000;
        if headers.contains_key(header::RANGE) {
            (
                StatusCode::PARTIAL_CONTENT,
                [(header::CONTENT_RANGE, format!("bytes 0-0/{SIZE}"))],
                "x",
            )
                .into_response()
        } else {
            (StatusCode::OK, [(header::CONTENT_LENGTH, SIZE.to_string())]).into_response()
        }
    }
    let app = Router::new().route("/files/{name}", get(file));
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn cookie_header(cookie: &str) -> [(&'static str, &str); 1] {
    [("cookie", cookie)]
}

// ---------------------------------------------------------------------------
// Public endpoints and headers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_is_public() {
    let app = TestApp::new().await;
    let r = app.get("/healthz", &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["status"], "ok");
}

#[tokio::test]
async fn security_headers_on_every_response() {
    let app = TestApp::new().await;
    for uri in ["/healthz", "/login", "/api/v1/downloads", "/nope"] {
        let r = app.get(uri, &[]).await;
        assert!(
            r.header("content-security-policy")
                .contains("default-src 'none'"),
            "{uri}"
        );
        assert_eq!(r.header("x-content-type-options"), "nosniff", "{uri}");
        assert_eq!(r.header("x-frame-options"), "DENY", "{uri}");
        assert_eq!(r.header("referrer-policy"), "same-origin", "{uri}");
        assert_eq!(r.header("cache-control"), "no-store", "{uri}");
    }
}

#[tokio::test]
async fn static_assets() {
    let app = TestApp::new().await;
    let js = app.get("/static/htmx-2.0.4.min.js", &[]).await;
    assert_eq!(js.status, StatusCode::OK);
    assert!(js.header("content-type").starts_with("text/javascript"));
    assert!(js.header("cache-control").contains("immutable"));
    let css = app.get("/static/style.css", &[]).await;
    assert!(css.header("content-type").starts_with("text/css"));
    let app_js = app.get("/static/app.js", &[]).await;
    assert_eq!(app_js.status, StatusCode::OK);
    assert!(app_js.header("content-type").starts_with("text/javascript"));
    for (path, content_type) in [
        ("/favicon.ico", "image/x-icon"),
        ("/static/favicon.svg", "image/svg+xml"),
        ("/static/apple-touch-icon.png", "image/png"),
    ] {
        let icon = app.get(path, &[]).await;
        assert_eq!(icon.status, StatusCode::OK, "{path}");
        assert_eq!(icon.header("content-type"), content_type, "{path}");
        assert!(icon.header("cache-control").starts_with("public"), "{path}");
    }
}

#[tokio::test]
async fn openapi_document_is_valid() {
    let app = TestApp::new().await;
    let r = app.get("/api/openapi.json", &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    let doc = r.json();
    assert!(doc["openapi"].as_str().unwrap().starts_with("3."));
    for path in [
        "/api/v1/downloads",
        "/api/v1/downloads/{id}",
        "/api/v1/downloads/{id}/events",
        "/api/v1/downloads/{id}/pause",
        "/api/v1/downloads/{id}/resume",
        "/api/v1/downloads/{id}/cancel",
        "/api/v1/downloads/{id}/retry",
        "/api/v1/downloads/cancelled",
        "/api/v1/downloads/completed",
        "/api/v1/stats",
        "/api/v1/tokens",
        "/api/v1/tokens/{id}",
    ] {
        assert!(doc["paths"].get(path).is_some(), "missing {path}");
    }
    assert!(doc["components"]["securitySchemes"]["bearer"].is_object());
    assert!(doc["components"]["schemas"]["DownloadDto"].is_object());
    assert!(doc["components"]["schemas"]["RetryDownload"].is_object());
    assert!(doc["paths"].get("/api/v1/settings/onefichier").is_some());
    assert!(
        doc["paths"]
            .get("/api/v1/settings/onefichier/account")
            .is_some()
    );
    assert!(doc["components"]["schemas"]["CreateDownload"]["properties"]["password"].is_object());
}

#[tokio::test]
async fn unknown_api_paths_return_json_404() {
    let app = TestApp::new().await;
    let r = app.get("/api/v1/nope", &[]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.error_kind(), "not_found");
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn api_requires_authentication() {
    let (app, _) = TestApp::with_admin().await;
    for (uri, auth) in [
        ("/api/v1/downloads", vec![]),
        (
            "/api/v1/downloads",
            vec![("authorization", "Bearer dkt_wrong")],
        ),
        (
            "/api/v1/downloads",
            vec![("authorization", "Basic YWRtaW46eA==")],
        ),
        (
            "/api/v1/downloads",
            vec![("cookie", "direkt_session=forged")],
        ),
    ] {
        let r = app.get(uri, &auth).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{auth:?}");
        assert_eq!(r.error_kind(), "unauthorized");
        assert_eq!(r.header("www-authenticate"), "Bearer");
    }
    let r = app
        .post_json(
            "/api/v1/downloads",
            &[],
            serde_json::json!({"url": "https://example.com/x"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn ui_redirects_to_login_or_setup() {
    let app = TestApp::new().await;
    let r = app.get("/", &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.header("location"), "/login");
    // Before setup, /login forwards to /setup.
    let r = app.get("/login", &[]).await;
    assert_eq!(r.header("location"), "/setup");
    let r = app.get("/setup", &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("setup-token"));
    // htmx requests get a client-side redirect instead.
    let r = app.get("/ui/dashboard", &[("hx-request", "true")]).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.header("hx-redirect"), "/login");
}

#[tokio::test]
async fn setup_through_the_ui() {
    let app = TestApp::new().await;
    let r = app
        .post_form(
            "/setup",
            &[],
            &format!(
                "setup_token=wrong&username=admin&password={PASSWORD}&password_confirm={PASSWORD}"
            ),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.body.contains("invalid setup token"));

    let token = app.auth.read_setup_token();
    let r = app
        .post_form(
            "/setup",
            &[],
            &format!("setup_token={token}&username=admin&password={PASSWORD}&password_confirm=different1234"),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("passwords do not match"));

    let r = app
        .post_form(
            "/setup",
            &[],
            &format!(
                "setup_token={token}&username=admin&password={PASSWORD}&password_confirm={PASSWORD}"
            ),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let cookie = r.header("set-cookie").to_owned();
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));
    assert!(!cookie.contains("Secure"), "secure_cookies is off");

    let session = cookie.split(';').next().unwrap();
    let r = app.get("/", &cookie_header(session)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Downloads"));

    // Setup is closed now.
    assert_eq!(app.get("/setup", &[]).await.header("location"), "/login");
}

#[tokio::test]
async fn login_and_logout() {
    let (app, _) = TestApp::with_admin().await;
    let r = app
        .post_form("/login", &[], "username=admin&password=wrong+password")
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.body.contains("invalid username or password"));

    let r = app
        .post_form(
            "/login",
            &[],
            &format!("username=admin&password={PASSWORD}"),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let session = r.header("set-cookie").split(';').next().unwrap().to_owned();
    assert_eq!(
        app.get("/api/v1/downloads", &cookie_header(&session))
            .await
            .status,
        StatusCode::OK
    );

    let r = app.post_form("/logout", &cookie_header(&session), "").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.header("set-cookie").contains("Max-Age=0"));
    assert_eq!(
        app.get("/api/v1/downloads", &cookie_header(&session))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

/// The authenticator app's code, `offset` steps from now.
fn app_code(secret: &str, offset: i64) -> String {
    let key = crate::totp::decode_base32(secret).unwrap();
    crate::totp::code_at(
        &key,
        crate::totp::step_at(chrono::Utc::now().timestamp()) + offset,
    )
}

#[tokio::test]
async fn two_factor_sign_in_through_the_web_ui() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);

    let r = app
        .post_form(
            "/settings/two-factor/setup",
            &auth,
            "password=wrong+password",
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("password is incorrect"));

    let r = app
        .post_form(
            "/settings/two-factor/setup",
            &auth,
            &format!("password={}", encode(PASSWORD)),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("<svg class=\"qr\""));
    assert!(
        r.body
            .contains("href=\"otpauth://totp/Direkt:admin?secret=")
    );
    let secret: String = r
        .body
        .split("class=\"totp-key\">")
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .unwrap()
        .replace(' ', "");

    let r = app
        .post_form(
            "/settings/two-factor/enable",
            &auth,
            &format!("code={}", app_code(&secret, 5)),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = app
        .post_form(
            "/settings/two-factor/enable",
            &auth,
            &format!("code={}", app_code(&secret, 0)),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("class=\"secret recovery-codes\""));
    assert_eq!(
        app.get("/settings", &auth).await.status,
        StatusCode::OK,
        "the session that turned it on stays signed in"
    );

    // The password alone starts a pending sign-in, not a session.
    let r = app
        .post_form(
            "/login",
            &[],
            &format!("username=admin&password={}", encode(PASSWORD)),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.header("location"), "/login/code");
    let pending_cookie = r.header("set-cookie");
    assert!(pending_cookie.starts_with("direkt_2fa="));
    assert!(pending_cookie.contains("Path=/login"));
    assert!(pending_cookie.contains("HttpOnly"));
    let pending = pending_cookie.split(';').next().unwrap().to_owned();
    assert_eq!(
        app.get("/api/v1/downloads", &cookie_header(&pending))
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "a pending sign-in is not a session"
    );

    let r = app.get("/login/code", &[]).await;
    assert_eq!(r.header("location"), "/login");
    let r = app.get("/login/code", &cookie_header(&pending)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Two-factor code"));

    let r = app
        .post_form("/login/code", &cookie_header(&pending), "code=12")
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.body.contains("invalid code"));

    let r = app
        .post_form(
            "/login/code",
            &cookie_header(&pending),
            &format!("code={}", app_code(&secret, 1)),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.header("location"), "/");
    let cookies: Vec<&str> = r
        .headers
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert!(cookies.iter().any(|c| c.starts_with("direkt_2fa=;")));
    let session = cookies
        .iter()
        .find(|c| c.starts_with("direkt_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    assert_eq!(
        app.get("/api/v1/downloads", &cookie_header(session))
            .await
            .status,
        StatusCode::OK
    );

    // Turning it off needs the password and a code.
    let r = app
        .post_form(
            "/settings/two-factor/disable",
            &auth,
            &format!("password={}&code=000000x", encode(PASSWORD)),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = app
        .post_form(
            "/settings/two-factor/disable",
            &auth,
            &format!(
                "password={}&code={}",
                encode(PASSWORD),
                app_code(&secret, -1)
            ),
        )
        .await;
    // The previous step's code is older than the one already used.
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{}", r.body);
    assert!(r.body.contains("already used"));
}

#[tokio::test]
async fn two_factor_forms_reject_cross_site_requests() {
    let (app, cookie) = TestApp::with_admin().await;
    let r = app
        .post_form(
            "/settings/two-factor/setup",
            &[
                ("cookie", cookie.as_str()),
                ("sec-fetch-site", "cross-site"),
            ],
            &format!("password={}", encode(PASSWORD)),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app
        .post_form(
            "/login/code",
            &[("sec-fetch-site", "cross-site")],
            "code=123456",
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn login_page_escapes_user_input() {
    let (app, _) = TestApp::with_admin().await;
    let r = app
        .post_form(
            "/login",
            &[],
            "username=%3Cscript%3Ealert(1)%3C%2Fscript%3E&password=x",
        )
        .await;
    assert!(!r.body.contains("<script>alert(1)"));
    assert!(r.body.contains("&#60;script&#62;") || r.body.contains("&lt;script&gt;"));
}

#[tokio::test]
async fn api_tokens_work_and_can_be_revoked() {
    let (app, cookie) = TestApp::with_admin().await;
    let r = app
        .post_json(
            "/api/v1/tokens",
            &cookie_header(&cookie),
            serde_json::json!({"name": "script"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let created = r.json();
    let secret = created["secret"].as_str().unwrap().to_owned();
    let id = created["id"].as_str().unwrap().to_owned();
    let bearer = format!("Bearer {secret}");

    let r = app
        .get("/api/v1/downloads", &[("authorization", &bearer)])
        .await;
    assert_eq!(r.status, StatusCode::OK);

    let listed = app
        .get("/api/v1/tokens", &[("authorization", &bearer)])
        .await
        .json();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert!(
        listed[0].get("secret").is_none(),
        "secrets are never listed"
    );
    assert!(!listed[0]["last_used_at"].is_null());

    let r = app
        .send(
            HttpRequest::delete(format!("/api/v1/tokens/{id}"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app
        .get("/api/v1/downloads", &[("authorization", &bearer)])
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cross_site_cookie_requests_are_rejected() {
    let (app, cookie) = TestApp::with_admin().await;
    let body = serde_json::json!({"url": "https://example.com/file"});
    for headers in [
        vec![
            ("cookie", cookie.as_str()),
            ("origin", "https://evil.example"),
            ("host", "direkt.lan"),
        ],
        vec![
            ("cookie", cookie.as_str()),
            ("sec-fetch-site", "cross-site"),
        ],
        vec![("cookie", cookie.as_str()), ("sec-fetch-site", "same-site")],
    ] {
        let r = app
            .post_json("/api/v1/downloads", &headers, body.clone())
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{headers:?}");
        assert_eq!(r.error_kind(), "forbidden");
    }
    // Same origin is fine.
    for headers in [
        vec![
            ("cookie", cookie.as_str()),
            ("origin", "http://direkt.lan:9877"),
            ("host", "direkt.lan:9877"),
        ],
        vec![
            ("cookie", cookie.as_str()),
            ("sec-fetch-site", "same-origin"),
        ],
    ] {
        let r = app
            .post_json("/api/v1/downloads", &headers, body.clone())
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{headers:?}");
    }
    // Cross-site GETs (navigation) are fine.
    let r = app
        .get(
            "/",
            &[("cookie", &cookie), ("sec-fetch-site", "cross-site")],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn cross_site_login_attempts_are_rejected_without_counting() {
    let (app, _) = TestApp::with_admin().await;
    let form = format!("username=admin&password={PASSWORD}");
    // Far more than the limiter allows: none of them may count.
    for _ in 0..10 {
        let r = app
            .post_form(
                "/login",
                &[("sec-fetch-site", "cross-site")],
                "username=admin&password=wrong+password",
            )
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
    }
    let r = app
        .post_form(
            "/login",
            &[("origin", "https://evil.example"), ("host", "direkt.lan")],
            &form,
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "no login CSRF");
    let r = app
        .post_form(
            "/setup",
            &[("sec-fetch-site", "cross-site")],
            "setup_token=x&username=a&password=b&password_confirm=b",
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // `Origin: null` is what a form post sends under `no-referrer`.
    let r = app
        .post_form(
            "/login",
            &[("origin", "null"), ("host", "direkt.lan")],
            &form,
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "opaque origin");

    // Plain HTTP on a LAN address: browsers send no `Sec-Fetch-Site` there,
    // only `Origin`.
    let r = app
        .post_form(
            "/login",
            &[
                ("origin", "http://192.168.1.20:9877"),
                ("host", "192.168.1.20:9877"),
            ],
            &form,
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "same origin over HTTP");

    let r = app
        .post_form("/login", &[("sec-fetch-site", "same-origin")], &form)
        .await;
    assert_eq!(
        r.status,
        StatusCode::SEE_OTHER,
        "the real user is not locked out"
    );
}

// ---------------------------------------------------------------------------
// Downloads API
// ---------------------------------------------------------------------------

#[tokio::test]
async fn submit_and_inspect_downloads() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let r = app
        .post_json(
            "/api/v1/downloads",
            &auth,
            serde_json::json!({"url": "https://example.com/dl/movie.mkv?token=secret"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let d = r.json();
    assert_eq!(d["state"], "queued");
    assert_eq!(d["post_state"], "none");
    assert_eq!(d["provider"], "http");
    assert_eq!(d["display_url"], "https://example.com/dl/movie.mkv?…");
    assert!(!r.body.contains("secret"), "the full URL is never returned");
    let id = d["id"].as_str().unwrap().to_owned();

    let got = app.get(&format!("/api/v1/downloads/{id}"), &auth).await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.json()["id"], id.as_str());

    let events = app
        .get(&format!("/api/v1/downloads/{id}/events"), &auth)
        .await
        .json();
    assert_eq!(events[0]["kind"], "submitted");

    let list = app
        .get("/api/v1/downloads?state=queued,paused&limit=10", &auth)
        .await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(list.json()["items"].as_array().unwrap().len(), 1);
    let none = app
        .get("/api/v1/downloads?state=completed", &auth)
        .await
        .json();
    assert!(none["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn download_actions_follow_the_state_machine() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let id = app
        .post_json(
            "/api/v1/downloads",
            &auth,
            serde_json::json!({"url": "https://example.com/f"}),
        )
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let r = app
        .post_json(&format!("/api/v1/downloads/{id}/retry"), &auth, Value::Null)
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.error_kind(), "conflict");

    let r = app
        .post_json(&format!("/api/v1/downloads/{id}/pause"), &auth, Value::Null)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["state"], "paused");

    let r = app
        .post_json(
            &format!("/api/v1/downloads/{id}/resume"),
            &auth,
            Value::Null,
        )
        .await;
    assert_eq!(r.json()["state"], "queued");

    let r = app
        .post_json(
            &format!("/api/v1/downloads/{id}/cancel"),
            &auth,
            Value::Null,
        )
        .await;
    assert_eq!(r.json()["state"], "cancelled");

    let r = app
        .post_json(&format!("/api/v1/downloads/{id}/retry"), &auth, Value::Null)
        .await;
    assert_eq!(r.json()["state"], "queued");
}

#[tokio::test]
async fn invalid_requests_get_actionable_errors() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);

    let cases: Vec<(Value, &str)> = vec![
        (
            serde_json::json!({"url": "ftp://example.com/x"}),
            "unsupported URL scheme",
        ),
        (
            serde_json::json!({"url": "http://192.168.1.10/x"}),
            "private",
        ),
        (
            serde_json::json!({"url": "http://169.254.169.254/latest/meta-data"}),
            "private",
        ),
        (
            serde_json::json!({"url": "https://user:pw@example.com/x"}),
            "credentials",
        ),
        (serde_json::json!({"url": ""}), "must not be empty"),
    ];
    for (body, expected) in cases {
        let r = app
            .post_json("/api/v1/downloads", &auth, body.clone())
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(r.error_kind(), "invalid_request");
        assert!(
            r.json()["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected),
            "{}",
            r.body
        );
    }

    // Malformed JSON, wrong fields, wrong content type.
    let r = app
        .send(
            HttpRequest::post("/api/v1/downloads")
                .header("cookie", &cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.error_kind(), "invalid_request");
    let r = app
        .post_json("/api/v1/downloads", &auth, serde_json::json!({"link": "x"}))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.error_kind(), "invalid_request");
    assert!(r.body.contains("url is required"));
    let r = app
        .send(
            HttpRequest::post("/api/v1/downloads")
                .header("cookie", &cookie)
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(r.error_kind(), "invalid_request");

    // Bad path and query parameters.
    let r = app.get("/api/v1/downloads/not-a-uuid", &auth).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.error_kind(), "invalid_request");
    let r = app
        .get(
            &format!("/api/v1/downloads/{}", uuid::Uuid::new_v4()),
            &auth,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = app.get("/api/v1/downloads?state=bogus", &auth).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("unknown state 'bogus'"));
    let r = app.get("/api/v1/downloads?limit=0", &auth).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = app.get("/api/v1/downloads?limit=abc", &auth).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.error_kind(), "invalid_request");
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

#[tokio::test]
async fn only_cancelled_downloads_can_be_deleted() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let mut ids = Vec::new();
    for n in 0..3 {
        let r = app
            .post_json(
                "/api/v1/downloads",
                &auth,
                serde_json::json!({"url": format!("https://example.com/{n}")}),
            )
            .await;
        ids.push(r.json()["id"].as_str().unwrap().to_owned());
    }
    let delete = |uri: String| {
        let mut builder = HttpRequest::delete(uri);
        for (k, v) in &auth {
            builder = builder.header(*k, *v);
        }
        app.send(builder.body(Body::empty()).unwrap())
    };

    let r = delete(format!("/api/v1/downloads/{}", ids[0])).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "queued downloads are kept");
    assert!(r.body.contains("only cancelled"), "{}", r.body);

    for id in &ids[..2] {
        app.post_json(
            &format!("/api/v1/downloads/{id}/cancel"),
            &auth,
            Value::Null,
        )
        .await;
    }
    let r = delete(format!("/api/v1/downloads/{}", ids[0])).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app
        .get(&format!("/api/v1/downloads/{}", ids[0]), &auth)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // The dashboard offers both buttons; the UI endpoint clears the rest.
    let dash = app.get("/ui/dashboard", &auth).await;
    assert!(dash.body.contains("/ui/cancelled/delete"), "{}", dash.body);
    assert!(
        dash.body
            .contains(&format!("/ui/downloads/{}/delete", ids[1]))
    );
    let r = app.post_form("/ui/cancelled/delete", &auth, "").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body
            .contains("Removed 1 cancelled download from the list<"),
        "{}",
        r.body
    );
    assert_eq!(r.header("hx-trigger"), "downloads-changed");

    let r = delete("/api/v1/downloads/cancelled".into()).await;
    assert_eq!(r.json()["deleted"], 0);
    let r = app.get("/api/v1/downloads", &auth).await;
    let left = r.json();
    let left: Vec<_> = left["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].clone())
        .collect();
    assert_eq!(left, [Value::from(ids[2].clone())]);
}

impl TestApp {
    /// A completed download whose file is `path`, written with some content.
    async fn completed_download(&self, path: &std::path::Path) -> String {
        use crate::domain::TransferState::{Completed, Downloading};
        use crate::store::{NewDownload, TransitionDetails};
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "data").unwrap();
        let now = chrono::Utc::now();
        self.store
            .insert_download(
                NewDownload {
                    url: "https://example.com/movie.mkv".into(),
                    display_url: "https://example.com/movie.mkv".into(),
                    provider: "http".into(),
                    ..Default::default()
                },
                now,
            )
            .await
            .unwrap();
        let d = self.store.claim_next(now).await.unwrap().unwrap();
        assert_eq!(d.state, Downloading);
        self.store
            .set_file_path(d.id, &path.to_string_lossy(), now)
            .await
            .unwrap();
        self.store
            .transition(
                d.id,
                Downloading,
                Completed,
                TransitionDetails::default(),
                now,
            )
            .await
            .unwrap();
        d.id.to_string()
    }

    fn download_dir(&self) -> std::path::PathBuf {
        self._dir.path().join("dl")
    }
}

#[tokio::test]
async fn extracted_archives_can_be_deleted_but_organized_files_cannot() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let dl = app.download_dir();
    let archive = dl.join("Show.S01.1080p.zip");
    let video = dl.join("Some.Movie.2019.mkv");
    let archive_id = app.completed_download(&archive).await;
    let video_id = app.completed_download(&video).await;
    // Both organized: the archive's videos were extracted into the library
    // and it stayed; the movie itself was moved there.
    for id in [&archive_id, &video_id] {
        let id = uuid::Uuid::parse_str(id).unwrap();
        let d = app.store.get_download(id).await.unwrap().unwrap();
        let now = chrono::Utc::now();
        if d.post_state == PostState::None {
            app.store
                .transition_post(id, PostState::None, PostState::Pending, None, now)
                .await
                .unwrap();
        }
        app.store
            .transition_post(id, PostState::Pending, PostState::Processing, None, now)
            .await
            .unwrap();
        app.store
            .transition_post(id, PostState::Processing, PostState::Completed, None, now)
            .await
            .unwrap();
    }
    let dash = app.get("/ui/dashboard", &auth).await;
    assert!(
        dash.body
            .contains(&format!("/ui/downloads/{archive_id}/delete-file/confirm"))
    );
    assert!(
        !dash
            .body
            .contains(&format!("/ui/downloads/{video_id}/delete-file/confirm"))
    );
    let dialog = app
        .get(
            &format!("/ui/downloads/{archive_id}/delete-file/confirm"),
            &auth,
        )
        .await;
    assert!(
        dialog.body.contains("extracted into your library stay"),
        "{}",
        dialog.body
    );
    let r = app
        .post_form(
            &format!("/ui/downloads/{archive_id}/delete-file"),
            &auth,
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(!archive.exists());
}

#[tokio::test]
async fn completed_downloads_can_be_removed_with_or_without_their_file() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let dl = app.download_dir();
    let kept = dl.join("kept.mkv");
    let gone = dl.join("gone.mkv");
    let outside = app._dir.path().join("outside.mkv");
    let keep_id = app.completed_download(&kept).await;
    let gone_id = app.completed_download(&gone).await;
    let outside_id = app.completed_download(&outside).await;

    // Buttons open an in-page dialog, never a browser alert.
    let dash = app.get("/ui/dashboard", &auth).await;
    assert!(!dash.body.contains("hx-confirm"));
    for action in ["delete", "delete-file"] {
        let confirm = format!("/ui/downloads/{keep_id}/{action}/confirm");
        assert!(dash.body.contains(&confirm), "{}", dash.body);
        let dialog = app.get(&confirm, &auth).await;
        assert_eq!(dialog.status, StatusCode::OK);
        assert!(
            dialog.body.contains("role=\"alertdialog\""),
            "{}",
            dialog.body
        );
        let post = format!("hx-post=\"/ui/downloads/{keep_id}/{action}\"");
        assert!(dialog.body.contains(&post), "{}", dialog.body);
    }
    let dialog = app
        .get(
            &format!("/ui/downloads/{keep_id}/delete-file/confirm"),
            &auth,
        )
        .await;
    assert!(dialog.body.contains("kept.mkv"), "shows the file path");
    let dialog = app.get("/ui/completed/delete", &auth).await;
    assert!(
        dialog.body.contains("Their files stay on disk"),
        "{}",
        dialog.body
    );
    assert!(dialog.body.contains("hx-post=\"/ui/completed/delete\""));
    let r = app
        .get(&format!("/ui/downloads/{keep_id}/bogus/confirm"), &auth)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = app.get("/ui/modal/close", &auth).await;
    assert_eq!((r.status, r.body.as_str()), (StatusCode::OK, ""));
    let detail = app.get(&format!("/downloads/{keep_id}"), &auth).await;
    assert!(!detail.body.contains("/delete"), "not on the detail page");

    let r = app
        .post_form(&format!("/ui/downloads/{keep_id}/delete"), &auth, "")
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Removed from the list"), "{}", r.body);
    assert!(
        r.body.contains("id=\"modal\" hx-swap-oob"),
        "the result closes the dialog"
    );
    assert!(kept.exists(), "removing from the list keeps the file");

    let r = app
        .post_form(&format!("/ui/downloads/{gone_id}/delete-file"), &auth, "")
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("File deleted"), "{}", r.body);
    assert!(!gone.exists());
    let r = app
        .get(&format!("/api/v1/downloads/{gone_id}"), &auth)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // A recorded path outside the download directory is never deleted, and
    // the download stays listed.
    let r = app
        .post_form(
            &format!("/ui/downloads/{outside_id}/delete-file"),
            &auth,
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        r.body.contains("not in the download directory"),
        "{}",
        r.body
    );
    assert!(outside.exists());
    let r = app
        .get(&format!("/api/v1/downloads/{outside_id}"), &auth)
        .await;
    assert_eq!(r.status, StatusCode::OK);

    let r = app.post_form("/ui/completed/delete", &auth, "").await;
    assert!(
        r.body
            .contains("Removed 1 completed download from the list"),
        "{}",
        r.body
    );
    assert!(outside.exists(), "clearing the list keeps files");
    let r = app.get("/api/v1/downloads", &auth).await;
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn delete_file_via_api() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let file = app.download_dir().join("movie.mkv");
    let id = app.completed_download(&file).await;
    let mut builder = HttpRequest::delete(format!("/api/v1/downloads/{id}?delete_file=true"));
    for (k, v) in &auth {
        builder = builder.header(*k, *v);
    }
    let r = app.send(builder.body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.body);
    assert!(!file.exists());
}

#[tokio::test]
async fn ui_submit_and_actions() {
    let (app, cookie) = TestApp::with_admin_local().await;
    let auth = cookie_header(&cookie);

    let url = format!("{}/files/movie.mkv", app.files);
    let id = app.preview_id(&auth, &url).await;
    let r = app
        .post_form(
            "/ui/downloads",
            &auth,
            &format!("preview_id={id}&media_type=unsorted"),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(!r.body.contains("movie.mkv"), "no message: {}", r.body);
    assert_eq!(
        r.header("hx-trigger"),
        "downloads-changed, download-started"
    );

    let rows = app.get("/ui/dashboard", &auth).await;
    assert!(rows.body.contains("movie.mkv"));
    assert!(rows.body.contains("Pause"));
    let id = rows
        .body
        .split("/downloads/")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned();

    let r = app
        .post_form(&format!("/ui/downloads/{id}/pause"), &auth, "")
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Paused"));
    let r = app
        .post_form(&format!("/ui/downloads/{id}/pause"), &auth, "")
        .await;
    assert!(r.body.contains("Paused"), "pausing twice is harmless");
    let r = app
        .post_form(&format!("/ui/downloads/{id}/retry"), &auth, "")
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert!(r.body.contains("cannot retry"));

    let detail = app.get(&format!("/downloads/{id}"), &auth).await;
    assert_eq!(detail.status, StatusCode::OK);
    assert!(detail.body.contains("History"));
    let missing = app
        .get(&format!("/downloads/{}", uuid::Uuid::new_v4()), &auth)
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ui_submit_errors_keep_input_escaped() {
    let (app, cookie) = TestApp::with_admin().await;
    let r = app
        .post_form(
            "/ui/downloads/preview",
            &cookie_header(&cookie),
            "url=%22%3E%3Cscript%3Ealert(1)%3C%2Fscript%3E",
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("invalid URL"));
    assert!(!r.body.contains("<script>alert(1)"));
}

#[tokio::test]
async fn ui_token_management() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let r = app.post_form("/settings/tokens", &auth, "name=phone").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body.contains(r#"id="tokens" open>"#),
        "the section the action came from is expanded"
    );
    assert!(!r.body.contains(r#"id="password" open>"#));
    let secret = r
        .body
        .split("<code>")
        .find(|s| s.starts_with("dkt_"))
        .and_then(|s| s.split("</code>").next())
        .expect("secret shown once")
        .to_owned();
    let bearer = format!("Bearer {secret}");
    assert_eq!(
        app.get("/api/v1/downloads", &[("authorization", &bearer)])
            .await
            .status,
        StatusCode::OK
    );

    let page = app.get("/settings", &auth).await;
    assert!(page.body.contains("phone"));
    assert!(!page.body.contains(&secret), "secret is not shown again");
    assert!(
        !page.body.contains(" open>"),
        "settings sections start collapsed"
    );
}

#[tokio::test]
async fn api_reference_page() {
    let app = TestApp::new().await;
    let r = app.get("/settings/api", &[]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "requires sign-in");

    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let settings = app.get("/settings", &auth).await;
    assert!(settings.body.contains(r#"href="/settings/api""#));

    let page = app.get("/settings/api", &auth).await;
    assert_eq!(page.status, StatusCode::OK);
    for text in [
        "/api/v1/downloads/{id}/retry",
        r#"id="op-create_download""#,
        r#"id="schema-DownloadDto""#,
        r##"href="#schema-DownloadDto""##,
        "<code>insufficient_disk_space</code>",
        "Authorization: Bearer $TOKEN",
    ] {
        assert!(page.body.contains(text), "missing {text}");
    }
    // Example JSON is escaped, never raw markup.
    assert!(
        page.body
            .contains("{&#34;url&#34;:&#34;https://example.com/files/movie.mkv&#34;}")
    );
}

#[tokio::test]
async fn file_password_is_asked_on_the_confirmation_menu() {
    let (app, cookie) = TestApp::with_admin_local().await;
    let auth = cookie_header(&cookie);

    // Plain links take no password: no field, and a sent one is refused.
    let url = format!("{}/files/movie.mkv", app.files);
    let r = app
        .post_form(
            "/ui/downloads/preview",
            &auth,
            &format!("url={}", encode(&url)),
        )
        .await;
    assert!(!r.body.contains("name=\"password\""), "{}", r.body);
    let id = preview_id_in(&r.body);
    let r = app
        .post_form(
            "/ui/downloads",
            &auth,
            &format!("preview_id={id}&media_type=movie&password=hunter2-secret"),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("does not use a download password"));

    // A hoster that takes one: the link form has no field, the menu does.
    let page = app.get("/", &auth).await;
    assert!(
        !page.body.contains("name=\"password\""),
        "not on the link form"
    );
    let url = "https://locked.test/file/movie.mkv";
    let r = app
        .post_form(
            "/ui/downloads/preview",
            &auth,
            &format!("url={}", encode(url)),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("id=\"file-password\" name=\"password\""));
    let id = preview_id_in(&r.body);
    let r = app
        .post_form(
            "/ui/downloads",
            &auth,
            &format!("preview_id={id}&media_type=movie&password=hunter2-secret"),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(!r.body.contains("hunter2-secret"));

    let list = app.get("/api/v1/downloads", &auth).await;
    assert!(!list.body.contains("hunter2-secret"));
    let id = list.json()["items"][0]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let stored = app.store.get_download(id).await.unwrap().unwrap();
    assert_eq!(stored.download_password.as_deref(), Some("hunter2-secret"));

    // The API takes it with preview_id too.
    let r = app
        .post_json("/api/v1/previews", &auth, serde_json::json!({"url": url}))
        .await;
    assert_eq!(r.json()["accepts_password"], true);
    let preview = r.json()["id"].as_str().unwrap().to_owned();
    let r = app
        .post_json(
            "/api/v1/downloads",
            &auth,
            serde_json::json!({"preview_id": preview, "password": "other-secret"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let id = r.json()["id"].as_str().unwrap().parse().unwrap();
    let stored = app.store.get_download(id).await.unwrap().unwrap();
    assert_eq!(stored.download_password.as_deref(), Some("other-secret"));
}

#[tokio::test]
async fn download_passwords_are_write_only() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let r = app
        .post_json(
            "/api/v1/downloads",
            &auth,
            serde_json::json!({"url": "https://1fichier.com/?abcde12345", "password": "hunter2-secret"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.json()["provider"], "1fichier");
    let id = r.json()["id"].as_str().unwrap().to_owned();
    for uri in [
        format!("/api/v1/downloads/{id}"),
        format!("/api/v1/downloads/{id}/events"),
        "/api/v1/downloads".to_owned(),
        "/ui/dashboard".to_owned(),
        format!("/downloads/{id}"),
    ] {
        let r = app.get(&uri, &auth).await;
        assert!(
            !r.body.contains("hunter2-secret"),
            "{uri} leaks the password"
        );
    }

    // Plain HTTP links reject passwords.
    let r = app
        .post_json(
            "/api/v1/downloads",
            &auth,
            serde_json::json!({"url": "https://example.com/f", "password": "x"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    // Retry accepts an optional password; unknown fields are rejected.
    app.post_json(&format!("/api/v1/downloads/{id}/pause"), &auth, Value::Null)
        .await;
    app.post_json(
        &format!("/api/v1/downloads/{id}/cancel"),
        &auth,
        Value::Null,
    )
    .await;
    let r = app
        .post_json(
            &format!("/api/v1/downloads/{id}/retry"),
            &auth,
            serde_json::json!({"password": "new-secret"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(!r.body.contains("new-secret"));
    let r = app
        .post_json(
            &format!("/api/v1/downloads/{id}/retry"),
            &auth,
            serde_json::json!({"pasword": "typo"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

const ONEFICHIER_KEY: &str = "AbCdEfGhIjKlMnOpQrStUvWxYz012345";

#[tokio::test]
async fn onefichier_key_via_api_is_masked() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let r = app.get("/api/v1/settings/onefichier", &auth).await;
    assert_eq!(r.json()["configured"], false);
    assert_eq!(r.json()["source"], "none");

    let put = |body: Value| {
        let cookie = cookie.clone();
        let app = &app;
        async move {
            app.send(
                HttpRequest::put("/api/v1/settings/onefichier")
                    .header("cookie", cookie)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
        }
    };
    let r = put(serde_json::json!({"api_key": "bad key"})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.error_kind(), "invalid_request");

    let r = put(serde_json::json!({"api_key": ONEFICHIER_KEY})).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["configured"], true);
    assert_eq!(r.json()["source"], "settings");
    assert_eq!(r.json()["api_key"], crate::settings::SECRET_MASK);
    assert!(!r.body.contains(ONEFICHIER_KEY));
    let r = app.get("/api/v1/settings/onefichier", &auth).await;
    assert!(!r.body.contains(ONEFICHIER_KEY));
    assert_eq!(r.json()["api_key"], crate::settings::SECRET_MASK);

    let r = app
        .send(
            HttpRequest::delete("/api/v1/settings/onefichier")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["configured"], false);
    assert!(r.json()["api_key"].is_null());
}

#[tokio::test]
async fn onefichier_key_via_settings_page() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let page = app.get("/settings", &auth).await;
    assert!(page.body.contains("Paste your 1fichier API key"));
    assert!(!page.body.contains("Remove key"));

    let r = app
        .post_form(
            "/settings/onefichier",
            &auth,
            &format!("api_key={ONEFICHIER_KEY}"),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("1fichier API key saved"));
    assert!(
        !r.body.contains(ONEFICHIER_KEY),
        "the key is never rendered"
    );
    let mask = crate::settings::SECRET_MASK;
    assert!(
        r.body.contains(&format!("value=\"{mask}\"")),
        "field shows the mask"
    );
    assert!(r.body.contains("Remove key"));

    // Submitting the pre-filled mask keeps the key.
    let r = app
        .post_form(
            "/settings/onefichier",
            &auth,
            &format!("api_key={}", mask.replace('*', "%2A")),
        )
        .await;
    assert!(r.body.contains("unchanged"), "{}", r.body);
    assert_eq!(
        app.get("/api/v1/settings/onefichier", &auth).await.json()["configured"],
        true
    );

    // Typing next to the mask is rejected rather than saved.
    let r = app
        .post_form("/settings/onefichier", &auth, "api_key=%2A%2A%2Aabc")
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("does not look like a 1fichier API key"));

    let r = app
        .post_form("/settings/onefichier/remove", &auth, "")
        .await;
    assert!(r.body.contains("1fichier API key removed"));
    assert_eq!(
        app.get("/api/v1/settings/onefichier", &auth).await.json()["configured"],
        false
    );
}

#[tokio::test]
async fn environment_key_locks_the_setting() {
    let (app, cookie) =
        TestApp::with_admin_and_env_key(Some(crate::config::Secret::new(ONEFICHIER_KEY))).await;
    let auth = cookie_header(&cookie);
    let r = app.get("/api/v1/settings/onefichier", &auth).await;
    assert_eq!(r.json()["source"], "environment");
    assert_eq!(r.json()["editable"], false);
    let page = app.get("/settings", &auth).await;
    assert!(page.body.contains("DIREKT_ONEFICHIER_API_KEY"));
    assert!(page.body.contains(" disabled"));
    assert!(!page.body.contains(ONEFICHIER_KEY));
    let r = app
        .post_form(
            "/settings/onefichier",
            &auth,
            "api_key=ZyXwVuTsRqPoNmLkJiHgFeDcBa987654",
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn onefichier_account_endpoints_validate_input() {
    // Only inputs rejected before contacting 1fichier: these tests must never
    // reach the real site.
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let r = app.get("/api/v1/settings/onefichier", &auth).await;
    assert_eq!(r.json()["account"]["signed_in"], false);

    let r = app
        .post_json(
            "/api/v1/settings/onefichier/account",
            &auth,
            serde_json::json!({"email": "not-an-email", "password": "x"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = app
        .post_json(
            "/api/v1/settings/onefichier/account",
            &auth,
            serde_json::json!({"email": "me@example.com", "password": crate::settings::SECRET_MASK}),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::BAD_REQUEST,
        "no stored password to reuse"
    );
    let r = app
        .post_json(
            "/api/v1/settings/onefichier/account",
            &auth,
            serde_json::json!({"email": "me@example.com", "password": "x", "code": "12ab"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    let r = app
        .send(
            HttpRequest::delete("/api/v1/settings/onefichier/account")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);

    let page = app.get("/settings", &auth).await;
    assert!(
        page.body
            .contains("action=\"/settings/onefichier/sign-in\"")
    );
    let r = app
        .post_form(
            "/settings/onefichier/sign-in",
            &auth,
            "email=bad&password=x",
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("email address of your 1fichier account"));
}

#[tokio::test]
async fn onefichier_cdn_section() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let page = app.get("/settings", &auth).await;
    assert!(page.body.contains("CDN credits"));
    assert!(page.body.contains("Not checked yet"));
    assert!(
        !page.body.contains("Refresh credits"),
        "nothing to refresh with"
    );

    let r = app
        .post_json(
            "/api/v1/settings/onefichier/cdn/refresh",
            &auth,
            Value::Null,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert!(app.get("/api/v1/settings/onefichier", &auth).await.json()["cdn"].is_null());

    // With an API key the button appears.
    app.post_form(
        "/settings/onefichier",
        &auth,
        &format!("api_key={ONEFICHIER_KEY}"),
    )
    .await;
    assert!(
        app.get("/settings", &auth)
            .await
            .body
            .contains("Refresh credits")
    );
}

const TMDB_KEY: &str = "0123456789abcdef0123456789abcdef";

#[tokio::test]
async fn tmdb_key_via_api_and_settings_page() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let r = app.get("/api/v1/settings/tmdb", &auth).await;
    assert_eq!(r.json()["configured"], false);

    let r = app
        .send(
            HttpRequest::put("/api/v1/settings/tmdb")
                .header("cookie", &cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"api_key": TMDB_KEY}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["api_key"], crate::settings::SECRET_MASK);
    assert!(!r.body.contains(TMDB_KEY));

    let page = app.get("/settings", &auth).await;
    assert!(page.body.contains("TMDB key set"));
    assert!(!page.body.contains(TMDB_KEY), "the key is never rendered");

    let r = app.post_form("/settings/tmdb", &auth, "api_key=bad").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("does not look like a TMDB API key"));
    assert!(r.body.contains("id=\"artwork\" open"), "section stays open");

    let r = app.post_form("/settings/tmdb/remove", &auth, "").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("TMDB API key removed"));
    let r = app.get("/api/v1/settings/tmdb", &auth).await;
    assert_eq!(r.json()["configured"], false);
}

const EDGERUNNERS: &str = "Cyberpunk.Edgerunners.S02.MULTi.AD.1080p.NF.WEB.x264-BYOR.zip";

async fn set_tmdb_key(app: &TestApp, auth: &[(&str, &str)]) {
    let key = crate::media::artwork_tests::KEY;
    let r = app
        .post_form("/settings/tmdb", auth, &format!("api_key={key}"))
        .await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn confirmation_menu_flow() {
    let (app, cookie) = TestApp::with_admin_local().await;
    let auth = cookie_header(&cookie);
    set_tmdb_key(&app, &auth).await;
    let url = format!("{}/files/{EDGERUNNERS}", app.files);

    // Download: the menu shows what was looked up, nothing is queued yet.
    let r = app
        .post_form(
            "/ui/downloads/preview",
            &auth,
            &format!("url={}", encode(&url)),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let menu = &r.body;
    assert!(menu.contains("Cyberpunk: Edgerunners"), "looked-up title");
    assert!(menu.contains("(2022)"));
    assert!(menu.contains(EDGERUNNERS), "file name");
    assert!(menu.contains("Direct link"), "provider");
    assert!(menu.contains("13.4 MiB"), "size");
    assert!(menu.contains("<img src=\"/artwork/"), "artwork");
    assert!(
        menu.contains(r#"<option value="tv" selected>TV series (detected)</option>"#),
        "type pre-filled"
    );
    let id = preview_id_in(menu);
    let dash = app.get("/ui/dashboard", &auth).await;
    assert!(
        !dash.body.contains("Edgerunners"),
        "not queued before confirming"
    );

    // Changing the type looks it up again: nothing for Unsorted.
    let r = app
        .post_form(
            &format!("/ui/previews/{id}/type"),
            &auth,
            &format!("preview_id={id}&media_type=unsorted"),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("<span>ZIP</span>"), "file-type tile");
    assert!(!r.body.contains("<img src=\"/artwork/"));
    assert!(r.body.contains(r#"<option value="unsorted" selected>"#));

    // Back to TV series, then start the download.
    app.post_form(
        &format!("/ui/previews/{id}/type"),
        &auth,
        &format!("preview_id={id}&media_type=tv"),
    )
    .await;
    let r = app
        .post_form(
            "/ui/downloads",
            &auth,
            &format!("preview_id={id}&media_type=tv"),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(!r.body.contains("preview_id"), "menu closed");

    let dash = app.get("/ui/dashboard", &auth).await;
    assert!(dash.body.contains(EDGERUNNERS));
    assert!(dash.body.contains("TV series"));
    let art = dash
        .body
        .split("<img src=\"/artwork/")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("artwork on the dashboard")
        .to_owned();
    let r = app.get(&format!("/artwork/{art}"), &auth).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-type"), "image/jpeg");
    let r = app.get(&format!("/artwork/{art}"), &[]).await;
    assert_ne!(r.status, StatusCode::OK, "artwork requires a login");

    // A confirmation works once.
    let r = app
        .post_form(
            "/ui/downloads",
            &auth,
            &format!("preview_id={id}&media_type=tv"),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(r.body.contains("expired"));
}

#[tokio::test]
async fn preview_without_tmdb_key_says_so() {
    let (app, cookie) = TestApp::with_admin_local().await;
    let auth = cookie_header(&cookie);
    let url = format!("{}/files/{EDGERUNNERS}", app.files);
    let r = app
        .post_form(
            "/ui/downloads/preview",
            &auth,
            &format!("url={}", encode(&url)),
        )
        .await;
    assert!(r.body.contains("Add a TMDB API key"), "{}", r.body);
    assert!(r.body.contains("<span>ZIP</span>"));

    // Games need no key (Steam).
    let url = format!("{}/files/Hades.II-TENOKE.zip", app.files);
    let r = app
        .post_form(
            "/ui/downloads/preview",
            &auth,
            &format!("url={}", encode(&url)),
        )
        .await;
    assert!(
        r.body.contains(r#"<option value="game" selected>"#),
        "{}",
        r.body
    );
    assert!(r.body.contains("<img src=\"/artwork/"));
}

#[tokio::test]
async fn preview_errors_and_api() {
    let (app, cookie) = TestApp::with_admin_local().await;
    let auth = cookie_header(&cookie);
    let missing = format!("{}/nope/file.zip", app.files);
    let r = app
        .post_form(
            "/ui/downloads/preview",
            &auth,
            &format!("url={}", encode(&missing)),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY);
    assert!(r.body.contains("not found"), "{}", r.body);
    assert!(
        r.body.contains(&format!("value=\"{missing}\"")),
        "link kept"
    );

    set_tmdb_key(&app, &auth).await;
    let url = format!("{}/files/{EDGERUNNERS}", app.files);
    let r = app
        .post_json("/api/v1/previews", &auth, serde_json::json!({"url": url}))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let p = r.json();
    assert_eq!(p["media_type"], "tv");
    assert_eq!(p["detected_type"], "tv");
    assert_eq!(p["size_bytes"], 14_100_000);
    assert_eq!(p["title"], "Cyberpunk: Edgerunners");
    let id = p["id"].as_str().unwrap().to_owned();

    let r = app
        .send(
            HttpRequest::put(format!("/api/v1/previews/{id}"))
                .header("cookie", &cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"media_type":"movie"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(r.json()["media_type"], "movie");
    assert!(r.json()["artwork_id"].is_null(), "no movie by that name");

    let r = app
        .post_json(
            "/api/v1/downloads",
            &auth,
            serde_json::json!({"preview_id": id, "media_type": "tv"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let d = r.json();
    assert_eq!(d["media_type"], "tv");
    assert_eq!(d["title"], "Cyberpunk: Edgerunners");
    assert!(d["artwork_id"].is_string());
    assert_eq!(d["filename"], EDGERUNNERS);

    let r = app
        .post_json("/api/v1/downloads", &auth, serde_json::json!({}))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn theme_is_chosen_per_browser() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);

    // Dark by default, on the app and on the login page.
    let page = app.get("/", &auth).await;
    assert!(page.body.contains("data-theme=\"dark\""));
    assert!(
        page.body
            .contains("<meta name=\"color-scheme\" content=\"dark\">")
    );
    assert!(page.body.contains("href=\"/static/style.css\""));
    assert!(
        app.get("/login", &[])
            .await
            .body
            .contains("data-theme=\"dark\"")
    );

    // Switching answers with the new theme and remembers it in a cookie.
    let r = app.post_form("/settings/theme", &auth, "theme=light").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("data-theme=\"light\""));
    assert!(
        r.body
            .contains("value=\"light\" class=\"theme-choice\" aria-pressed=\"true\"")
    );
    let set = r.header("set-cookie");
    assert!(set.starts_with("direkt_theme=light;"), "{set}");
    assert!(set.contains("HttpOnly"), "{set}");

    let both = format!("{cookie}; direkt_theme=light");
    for uri in ["/", "/settings"] {
        let page = app.get(uri, &cookie_header(&both)).await;
        assert!(page.body.contains("data-theme=\"light\""), "{uri}");
        assert!(
            page.body
                .contains("<meta name=\"color-scheme\" content=\"light\">"),
            "{uri}"
        );
    }
    let login = app.get("/login", &[("cookie", "direkt_theme=light")]).await;
    assert!(login.body.contains("data-theme=\"light\""));

    // An unknown value in the cookie falls back; one in the form is refused.
    let odd = format!("{cookie}; direkt_theme=../evil");
    assert!(
        app.get("/", &cookie_header(&odd))
            .await
            .body
            .contains("data-theme=\"dark\"")
    );
    let r = app.post_form("/settings/theme", &auth, "theme=evil").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("unknown theme"));
    assert!(r.headers.get("set-cookie").is_none());

    // Changing the theme needs a session.
    let r = app.post_form("/settings/theme", &[], "theme=light").await;
    assert_ne!(r.status, StatusCode::OK);
}

impl TestApp {
    async fn patch_json(&self, uri: &str, cookie: &str, body: Value) -> Reply {
        self.send(
            HttpRequest::patch(uri)
                .header("cookie", cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }
}

#[tokio::test]
async fn delete_archives_setting_via_page_and_api() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let page = app.get("/settings", &auth).await;
    assert!(page.body.contains("Delete archives after extraction"));
    assert!(
        page.body
            .contains(r#"name="delete_extracted_archives" value="on">"#),
        "off by default"
    );
    let r = app
        .post_form("/settings/archives", &auth, "delete_extracted_archives=on")
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("id=\"library\" open"), "section stays open");
    assert!(
        r.body
            .contains(r#"name="delete_extracted_archives" value="on" checked"#)
    );
    let misc = app.get("/api/v1/settings/misc", &auth).await.json();
    assert_eq!(misc["delete_extracted_archives"], true);

    // Saving the miscellaneous section leaves it alone.
    let r = app
        .post_form(
            "/settings/misc",
            &auth,
            "speed_unit=binary&speed_limit=&speed_limit_unit=MiB&max_concurrent=1",
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let misc = app.get("/api/v1/settings/misc", &auth).await.json();
    assert_eq!(misc["delete_extracted_archives"], true);

    let r = app.post_form("/settings/archives", &auth, "").await;
    assert_eq!(r.status, StatusCode::OK);
    let misc = app.get("/api/v1/settings/misc", &auth).await.json();
    assert_eq!(misc["delete_extracted_archives"], false);
}

#[tokio::test]
async fn misc_settings_via_page_and_api() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let page = app.get("/settings", &auth).await;
    assert!(page.body.contains("Miscellaneous"));
    assert!(page.body.contains("No speed limit"));
    assert!(
        page.body
            .contains(r#"name="speed_unit" value="binary" checked"#)
    );
    assert!(
        page.body
            .contains(r#"name="confirm_downloads" value="on" checked"#)
    );

    let r = app
        .post_form(
            "/settings/misc",
            &auth,
            "speed_unit=decimal&speed_limit=2%2C5&speed_limit_unit=MB&max_concurrent=4\
             &confirm_downloads=on",
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("id=\"misc\" open"), "section stays open");
    assert!(r.body.contains("Limited to 2.5 MB/s"));
    assert!(r.body.contains(r#"value="2.5""#), "limit pre-filled");
    assert!(
        r.body
            .contains(r#"<option value="MB" selected>MB/s</option>"#)
    );

    let misc = app.get("/api/v1/settings/misc", &auth).await.json();
    assert_eq!(misc["speed_unit"], "decimal");
    assert_eq!(misc["speed_limit"], 2_500_000);
    assert_eq!(misc["max_concurrent"], 4);
    assert_eq!(misc["default_max_concurrent"], 1);
    assert_eq!(misc["confirm_downloads"], true);

    let dash = app.get("/ui/dashboard", &auth).await;
    assert!(dash.body.contains("Limited to 2.5 MB/s"));
    assert!(dash.body.contains(r#"data-speed="0 B/s""#));

    // Invalid input is reported in the section and changes nothing.
    for (form, message) in [
        (
            "speed_unit=bits&speed_limit=fast&speed_limit_unit=MB&max_concurrent=4",
            "enter the speed limit as a number",
        ),
        (
            "speed_unit=bits&speed_limit=1&speed_limit_unit=kB&max_concurrent=4",
            "at least 10 kB/s",
        ),
        (
            "speed_unit=bits&speed_limit=&speed_limit_unit=MB&max_concurrent=99",
            "between 1 and 32",
        ),
        (
            "speed_unit=parsecs&speed_limit=&speed_limit_unit=MB&max_concurrent=2",
            "unknown unit",
        ),
    ] {
        let r = app.post_form("/settings/misc", &auth, form).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{form}");
        assert!(r.body.contains(message), "{form}: {}", r.body);
        assert!(r.body.contains("id=\"misc\" open"));
    }
    assert_eq!(
        app.get("/api/v1/settings/misc", &auth).await.json(),
        misc,
        "unchanged"
    );

    // The API changes only the fields it is sent; 0 removes the limit.
    let r = app
        .patch_json(
            "/api/v1/settings/misc",
            &cookie,
            serde_json::json!({"speed_limit": 0, "speed_unit": "bits"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let after = r.json();
    assert_eq!(after["speed_limit"], Value::Null);
    assert_eq!(after["speed_unit"], "bits");
    assert_eq!(after["max_concurrent"], 4);
    let r = app
        .patch_json(
            "/api/v1/settings/misc",
            &cookie,
            serde_json::json!({"max_concurrent": 0}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.error_kind(), "invalid_request");
    let r = app
        .patch_json(
            "/api/v1/settings/misc",
            &cookie,
            serde_json::json!({"turbo": true}),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "unknown field");
}

#[tokio::test]
async fn download_starts_without_confirmation_when_turned_off() {
    let (app, cookie) = TestApp::with_admin_local().await;
    let auth = cookie_header(&cookie);
    // The checkbox is left out of the form when unticked.
    let r = app
        .post_form(
            "/settings/misc",
            &auth,
            "speed_unit=binary&speed_limit=&speed_limit_unit=MiB&max_concurrent=1",
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(
        !r.body
            .contains(r#"name="confirm_downloads" value="on" checked"#)
    );

    let url = format!("{}/files/{EDGERUNNERS}", app.files);
    let r = app
        .post_form(
            "/ui/downloads/preview",
            &auth,
            &format!("url={}", encode(&url)),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(!r.body.contains("preview_id"), "no confirmation menu");
    assert!(r.header("hx-trigger").contains("download-started"));

    let list = app.get("/api/v1/downloads", &auth).await.json();
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["media_type"], "tv", "the detected type");
    assert_eq!(items[0]["filename"], EDGERUNNERS);

    // A bad link still shows its error on the form.
    let r = app
        .post_form("/ui/downloads/preview", &auth, "url=ftp%3A%2F%2Fx")
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("flash error"));
}

#[tokio::test]
async fn library_folders_are_read_only() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let r = app.get("/api/v1/settings/library", &auth).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["movies"]["path"], Value::Null);
    assert_eq!(r.json()["movies"]["variable"], "DIREKT_MOVIES_DIR");

    let page = app.get("/settings", &auth).await;
    assert!(page.body.contains("Media library"));
    assert!(page.body.contains("DIREKT_TV_DIR"));
    assert!(
        !page.body.contains("action=\"/settings/library\""),
        "no form"
    );

    // Nothing over HTTP can change where Direkt writes.
    let r = app
        .patch_json(
            "/api/v1/settings/library",
            &cookie,
            serde_json::json!({ "movies": "/etc" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    let r = app
        .post_form("/settings/library", &auth, "movies=%2Fetc")
        .await;
    assert_ne!(r.status, StatusCode::OK);
    let r = app.get("/api/v1/settings/library", &auth).await;
    assert_eq!(r.json()["movies"]["path"], Value::Null);
}

#[tokio::test]
async fn skipped_downloads_can_be_organized_again() {
    use crate::domain::PostState;
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);
    let id = app
        .completed_download(&app.download_dir().join("movie.mkv"))
        .await;

    // Waiting already: nothing to do.
    let r = app
        .post_json(
            &format!("/api/v1/downloads/{id}/organize"),
            &auth,
            Value::Null,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    let uuid = uuid::Uuid::parse_str(&id).unwrap();
    app.store
        .transition_post(
            uuid,
            PostState::Pending,
            PostState::Skipped,
            None,
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    let r = app
        .post_json(
            &format!("/api/v1/downloads/{id}/organize"),
            &auth,
            Value::Null,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.json()["post_state"], "pending");
    assert_eq!(r.json()["state"], "completed");

    app.store
        .transition_post(
            uuid,
            PostState::Pending,
            PostState::Skipped,
            None,
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    let detail = app.get(&format!("/ui/downloads/{id}/detail"), &auth).await;
    assert!(detail.body.contains("Organize again"));
    let r = app
        .post_form(&format!("/ui/downloads/{id}/organize"), &auth, "")
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Queued for organizing"));
}

// ---------------------------------------------------------------------------
// Accounts and permissions
// ---------------------------------------------------------------------------

/// What new accounts choose at their first sign-in.
const USER_PASSWORD: &str = "Tidal-orchard-lamp-42";

fn session_from(r: &Reply) -> String {
    r.header("set-cookie").split(';').next().unwrap().to_owned()
}

impl TestApp {
    /// An administrator creates an account; returns its id and temporary
    /// password.
    async fn create_account(&self, admin: &str, body: Value) -> (i64, String) {
        let r = self
            .post_json("/api/v1/users", &cookie_header(admin), body)
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
        let v = r.json();
        (
            v["user"]["id"].as_i64().unwrap(),
            v["temporary_password"].as_str().unwrap().to_owned(),
        )
    }

    async fn log_in(&self, username: &str, password: &str) -> Reply {
        self.post_form(
            "/login",
            &[],
            &format!("username={username}&password={}", encode(password)),
        )
        .await
    }

    /// First sign-in: the temporary password, then one of their own.
    /// Returns the session cookie.
    async fn first_sign_in(&self, username: &str, temporary: &str) -> String {
        let r = self.log_in(username, temporary).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        let r = self
            .post_form(
                "/welcome/password",
                &cookie_header(&session_from(&r)),
                &format!(
                    "new_password={0}&new_password_confirm={0}",
                    encode(USER_PASSWORD)
                ),
            )
            .await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        assert_eq!(r.header("location"), "/welcome");
        session_from(&r)
    }

    /// A signed-in account with `permissions`: its id and session cookie.
    async fn user_with(&self, admin: &str, name: &str, permissions: &[&str]) -> (i64, String) {
        let (id, temporary) = self
            .create_account(
                admin,
                serde_json::json!({"username": name, "permissions": permissions}),
            )
            .await;
        (id, self.first_sign_in(name, &temporary).await)
    }

    /// Submit a direct link through the API; returns the download id.
    async fn submit_as(&self, cookie: &str, url: &str) -> String {
        let r = self
            .post_json(
                "/api/v1/downloads",
                &cookie_header(cookie),
                serde_json::json!({ "url": url }),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
        r.json()["id"].as_str().unwrap().to_owned()
    }
}

#[tokio::test]
async fn first_sign_in_replaces_the_temporary_password() {
    let (app, admin) = TestApp::with_admin().await;
    let (_, temporary) = app
        .create_account(&admin, serde_json::json!({"username": "alice"}))
        .await;
    let users = app.get("/api/v1/users", &cookie_header(&admin)).await;
    assert!(!users.body.contains(&temporary), "shown once");
    let users = users.json();
    let alice = &users.as_array().unwrap()[1];
    assert_eq!(alice["must_change_password"], true);
    assert_eq!(
        alice["permissions"],
        serde_json::json!(["submit_downloads", "premium_providers"])
    );

    let r = app.log_in("alice", &temporary).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let cookie = session_from(&r);
    let auth = cookie_header(&cookie);

    // Everything leads to the first-sign-in page until it is done.
    let r = app.get("/", &auth).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.header("location"), "/welcome");
    let r = app
        .get(
            "/ui/dashboard",
            &[("cookie", &cookie), ("hx-request", "true")],
        )
        .await;
    assert_eq!(r.header("hx-redirect"), "/welcome");
    let r = app.get("/api/v1/downloads", &auth).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(
        r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("temporary password")
    );
    let page = app.get("/welcome", &auth).await;
    assert!(page.body.contains("Welcome, alice"), "{}", page.body);

    let choose = |password: &str, confirm: &str| {
        format!(
            "new_password={}&new_password_confirm={}",
            encode(password),
            encode(confirm)
        )
    };
    let r = app
        .post_form("/welcome/password", &auth, &choose("short", "short"))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("at least 12"), "{}", r.body);
    let r = app
        .post_form("/welcome/password", &auth, &choose(&temporary, &temporary))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("not the temporary one"), "{}", r.body);
    let r = app
        .post_form(
            "/welcome/password",
            &auth,
            &choose(USER_PASSWORD, "Something-else-42"),
        )
        .await;
    assert!(r.body.contains("do not match"));

    let r = app
        .post_form(
            "/welcome/password",
            &auth,
            &choose(USER_PASSWORD, USER_PASSWORD),
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let new_cookie = session_from(&r);
    assert_eq!(
        app.get("/api/v1/me", &auth).await.status,
        StatusCode::UNAUTHORIZED,
        "the session that used the temporary password ended"
    );
    let new_auth = cookie_header(&new_cookie);
    let r = app.get("/welcome", &new_auth).await;
    assert_eq!(r.header("location"), "/", "nothing left to do");
    let me = app.get("/api/v1/me", &new_auth).await.json();
    assert_eq!(me["username"], "alice");
    assert_eq!(me["role"], "user");
    assert_eq!(me["must_change_password"], false);
    assert_eq!(app.get("/", &new_auth).await.status, StatusCode::OK);

    assert_eq!(
        app.log_in("alice", &temporary).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.log_in("alice", USER_PASSWORD).await.status,
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn required_two_factor_is_set_up_at_first_sign_in() {
    let (app, admin) = TestApp::with_admin().await;
    let (_, temporary) = app
        .create_account(
            &admin,
            serde_json::json!({"username": "bob", "require_two_factor": true}),
        )
        .await;
    let cookie = app.first_sign_in("bob", &temporary).await;
    let auth = cookie_header(&cookie);
    assert_eq!(app.get("/", &auth).await.header("location"), "/welcome");

    let page = app.get("/welcome", &auth).await;
    assert!(page.body.contains("Set up two-factor authentication"));
    let key = |body: &str| {
        body.split("class=\"totp-key\">")
            .nth(1)
            .and_then(|s| s.split('<').next())
            .unwrap()
            .replace(' ', "")
    };
    let secret = key(&page.body);
    assert_eq!(
        key(&app.get("/welcome", &auth).await.body),
        secret,
        "a reload keeps the secret"
    );

    let r = app
        .post_form("/welcome/two-factor", &auth, "code=000000")
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = app
        .post_form(
            "/welcome/two-factor",
            &auth,
            &format!("code={}", app_code(&secret, 0)),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("Save your recovery codes"));
    assert_eq!(app.get("/", &auth).await.status, StatusCode::OK);

    let settings = app.get("/settings", &auth).await;
    assert!(settings.body.contains("can't be turned off"));
    assert!(!settings.body.contains("/settings/two-factor/disable"));

    let r = app.log_in("bob", USER_PASSWORD).await;
    assert_eq!(r.header("location"), "/login/code", "a code is now needed");
}

#[tokio::test]
async fn permissions_are_enforced() {
    let (app, admin) = TestApp::with_admin().await;
    let (_, viewer) = app.user_with(&admin, "viewer", &[]).await;
    let auth = cookie_header(&viewer);

    // No downloads.
    let r = app
        .post_json(
            "/api/v1/downloads",
            &auth,
            serde_json::json!({"url": "https://example.com/file.bin"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.error_kind(), "forbidden");
    let r = app
        .post_form(
            "/ui/downloads/preview",
            &auth,
            "url=https%3A%2F%2Fexample.com%2Ffile.bin",
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let page = app.get("/", &auth).await;
    assert!(page.body.contains("can't add downloads"));
    assert!(!page.body.contains("name=\"url\""));

    // No server settings, tokens or accounts.
    for path in ["/api/v1/settings/misc", "/api/v1/tokens", "/api/v1/users"] {
        let r = app.get(path, &auth).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(r.error_kind(), "forbidden");
    }
    for (path, form) in [
        ("/settings/misc", "speed_unit=binary&max_concurrent=3"),
        ("/settings/tokens", "name=x"),
        ("/settings/tmdb/remove", ""),
        ("/users", "username=eve"),
    ] {
        let r = app.post_form(path, &auth, form).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{path}");
    }
    assert_eq!(app.get("/users", &auth).await.status, StatusCode::FORBIDDEN);
    let settings = app.get("/settings", &auth).await;
    for hidden in [
        r#"id="onefichier""#,
        r#"id="misc""#,
        r#"id="tokens""#,
        "href=\"/users\"",
    ] {
        assert!(!settings.body.contains(hidden), "{hidden}");
    }
    for shown in [r#"id="password""#, r#"id="two-factor""#] {
        assert!(settings.body.contains(shown), "{shown}");
    }
    assert_eq!(
        app.get("/api/v1/me", &auth).await.json()["permissions"],
        serde_json::json!([])
    );

    // Direct links only, without premium hosts.
    let (_, direct) = app.user_with(&admin, "direct", &["submit_downloads"]).await;
    let r = app
        .post_json(
            "/api/v1/downloads",
            &cookie_header(&direct),
            serde_json::json!({"url": "https://1fichier.com/?abcde12345"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert!(r.body.contains("premium"));
    app.submit_as(&direct, "https://example.com/file.bin").await;

    // Administrators see the accounts link.
    let settings = app.get("/settings", &cookie_header(&admin)).await;
    assert!(settings.body.contains("href=\"/users\""));
}

#[tokio::test]
async fn downloads_belong_to_their_owner() {
    let (app, admin) = TestApp::with_admin().await;
    let a = app.submit_as(&admin, "https://example.com/a.bin").await;
    let (alice_id, alice) = app.user_with(&admin, "alice", &["submit_downloads"]).await;
    let b = app.submit_as(&alice, "https://example.com/b.bin").await;
    let alice_auth = cookie_header(&alice);

    let list = app.get("/api/v1/downloads", &alice_auth).await.json();
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], b.as_str());
    assert_eq!(items[0]["owner"], "alice");
    for path in [
        format!("/api/v1/downloads/{a}"),
        format!("/api/v1/downloads/{a}/events"),
    ] {
        assert_eq!(
            app.get(&path, &alice_auth).await.status,
            StatusCode::NOT_FOUND
        );
    }
    let r = app
        .post_json(
            &format!("/api/v1/downloads/{a}/pause"),
            &alice_auth,
            Value::Null,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "existence is not revealed");
    let r = app
        .post_json(
            &format!("/api/v1/downloads/{b}/pause"),
            &alice_auth,
            Value::Null,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);

    let list = app
        .get("/api/v1/downloads", &cookie_header(&admin))
        .await
        .json();
    let owners: Vec<_> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["owner"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(owners, ["alice", "admin"]);

    // Seeing everyone's is not managing them.
    let (watcher_id, watcher) = app
        .user_with(&admin, "watcher", &["view_all_downloads"])
        .await;
    let watcher_auth = cookie_header(&watcher);
    let list = app.get("/api/v1/downloads", &watcher_auth).await.json();
    assert_eq!(list["items"].as_array().unwrap().len(), 2);
    let r = app
        .post_json(
            &format!("/api/v1/downloads/{b}/resume"),
            &watcher_auth,
            Value::Null,
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let dash = app.get("/ui/dashboard", &watcher_auth).await;
    assert!(dash.body.contains("by alice"), "{}", dash.body);
    assert!(!dash.body.contains(&format!("/ui/downloads/{b}/resume")));
    let dash = app.get("/ui/dashboard", &alice_auth).await;
    assert!(dash.body.contains(&format!("/ui/downloads/{b}/resume")));

    let r = app
        .patch_json(
            &format!("/api/v1/users/{watcher_id}"),
            &admin,
            serde_json::json!({"permissions": ["manage_all_downloads"]}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        r.json()["permissions"],
        serde_json::json!(["view_all_downloads", "manage_all_downloads"])
    );
    let r = app
        .post_json(
            &format!("/api/v1/downloads/{b}/resume"),
            &watcher_auth,
            Value::Null,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "takes effect at once");

    // Deleting files needs its own permission.
    let r = app
        .post_json(
            &format!("/api/v1/downloads/{b}/cancel"),
            &alice_auth,
            Value::Null,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = app
        .send(
            HttpRequest::delete(format!("/api/v1/downloads/{b}?delete_file=true"))
                .header("cookie", &alice)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // Clearing a list only clears the account's own.
    let r = app
        .post_json(
            &format!("/api/v1/downloads/{a}/cancel"),
            &cookie_header(&admin),
            Value::Null,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = app
        .send(
            HttpRequest::delete("/api/v1/downloads/cancelled")
                .header("cookie", &alice)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.json()["deleted"], 1);
    assert_eq!(
        app.get(&format!("/api/v1/downloads/{a}"), &cookie_header(&admin))
            .await
            .status,
        StatusCode::OK
    );

    // A deleted account's downloads stay, without an owner.
    let c = app.submit_as(&alice, "https://example.com/c.bin").await;
    let r = app
        .send(
            HttpRequest::delete(format!("/api/v1/users/{alice_id}"))
                .header("cookie", &admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let d = app
        .get(&format!("/api/v1/downloads/{c}"), &cookie_header(&admin))
        .await
        .json();
    assert_eq!(d["owner_id"], Value::Null);
    assert_eq!(
        app.get("/api/v1/me", &alice_auth).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn accounts_are_managed_through_the_web_ui() {
    let (app, admin) = TestApp::with_admin().await;
    let auth = cookie_header(&admin);
    let r = app
        .post_form(
            "/users",
            &auth,
            "username=carol&role=user&perm_submit_downloads=on&require_two_factor=on",
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("Temporary Password for carol"));
    let temporary = |body: &str| {
        body.split("<code id=\"issued-password\">")
            .nth(1)
            .and_then(|s| s.split('<').next())
            .unwrap()
            .to_owned()
    };
    let first = temporary(&r.body);
    let list = app.get("/users", &auth).await;
    assert!(list.body.contains("Waiting for first sign-in"));
    assert!(!list.body.contains(&first), "shown once");
    let r = app
        .post_form("/users", &auth, "username=CAROL&role=user")
        .await;
    assert!(r.body.contains("already taken"), "{}", r.body);
    assert!(
        r.body.contains("value=\"CAROL\""),
        "the form keeps its input"
    );

    let users = app.get("/api/v1/users", &auth).await.json();
    let carol = &users.as_array().unwrap()[1];
    assert_eq!(carol["require_two_factor"], true);
    let id = carol["id"].as_i64().unwrap();

    let r = app
        .post_form(
            &format!("/users/{id}"),
            &auth,
            "role=user&perm_view_all_downloads=on",
        )
        .await;
    assert!(r.body.contains("Saved"), "{}", r.body);
    let carol = app.get(&format!("/api/v1/users/{id}"), &auth).await.json();
    assert_eq!(
        carol["permissions"],
        serde_json::json!(["view_all_downloads"])
    );
    assert_eq!(carol["require_two_factor"], false);

    let r = app
        .post_form(&format!("/users/{id}/reset-password"), &auth, "")
        .await;
    assert!(r.body.contains("Temporary Password for carol"));
    let second = temporary(&r.body);
    assert_eq!(
        app.log_in("carol", &first).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.log_in("carol", &second).await.status,
        StatusCode::SEE_OTHER
    );

    let r = app
        .post_form(&format!("/users/{id}/status"), &auth, "disabled=true")
        .await;
    assert!(r.body.contains("Account disabled"));
    let r = app.log_in("carol", &second).await;
    assert!(r.body.contains("disabled"), "{}", r.body);

    // Administrators can't lock themselves out.
    let r = app
        .post_form("/users/1/status", &auth, "disabled=true")
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = app
        .patch_json(
            "/api/v1/users/1",
            &admin,
            serde_json::json!({"role": "user"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    let dialog = app.get(&format!("/users/{id}/delete"), &auth).await;
    assert!(dialog.body.contains("Delete account?"));
    let r = app
        .post_form(&format!("/users/{id}/delete"), &auth, "")
        .await;
    assert_eq!(r.header("hx-redirect"), "/users");
    assert_eq!(
        app.get(&format!("/api/v1/users/{id}"), &auth).await.status,
        StatusCode::NOT_FOUND
    );
}
