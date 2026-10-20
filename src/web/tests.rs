//! HTTP-level tests for the API, authentication, and UI, driving the router
//! in-process. The engine is not started, so submitted downloads stay queued
//! and no network access happens.

use super::*;
use crate::engine::{Engine, EngineSettings};
use crate::net::NetworkPolicy;
use crate::providers::Providers;
use axum::body::Body;
use axum::http::Request as HttpRequest;
use http_body_util::BodyExt;
use serde_json::Value;
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
        let settings = crate::settings::SettingsService::load(
            store.clone(),
            crate::settings::EnvKeys {
                onefichier: env_onefichier_key,
                tmdb: None,
            },
            account.clone(),
        )
        .await
        .unwrap();
        let providers = Providers::standard(client.clone(), settings.onefichier_key(), account);
        let engine_settings = EngineSettings {
            download_dir: dir.path().join("dl"),
            incomplete_dir: dir.path().join("inc"),
            max_concurrent: 1,
            max_retries: 1,
            retry_base_delay: Duration::from_secs(1),
            retry_max_delay: Duration::from_secs(1),
            min_free_bytes: 0,
        };
        let engine = Engine::new(
            store.clone(),
            providers.clone(),
            client.clone(),
            policy.clone(),
            engine_settings,
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
    let css = app.get("/static/app.css", &[]).await;
    assert!(css.header("content-type").starts_with("text/css"));
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

    let dash = app.get("/ui/dashboard", &auth).await;
    assert!(
        dash.body
            .contains(&format!("/ui/downloads/{keep_id}/delete\""))
    );
    assert!(
        dash.body
            .contains(&format!("/ui/downloads/{keep_id}/delete-file"))
    );
    assert!(dash.body.contains("/ui/completed/delete"));
    let detail = app.get(&format!("/downloads/{keep_id}"), &auth).await;
    assert!(!detail.body.contains("/delete"), "not on the detail page");

    let r = app
        .post_form(&format!("/ui/downloads/{keep_id}/delete"), &auth, "")
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Removed from the list"), "{}", r.body);
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
    assert!(r.body.contains("Queued movie.mkv"), "{}", r.body);
    assert_eq!(r.header("hx-trigger"), "downloads-changed");

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
    assert!(r.body.contains(&format!("Queued {EDGERUNNERS}")));
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
