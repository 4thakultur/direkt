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
    _dir: tempfile::TempDir,
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
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().await.unwrap();
        let auth = AuthService::new(store.clone(), dir.path()).await.unwrap();
        let policy = NetworkPolicy::default();
        let client = crate::http_client::build_client(policy.clone()).unwrap();
        let settings = crate::settings::SettingsService::load(store.clone(), env_onefichier_key)
            .await
            .unwrap();
        let providers = Providers::standard(client.clone(), settings.onefichier_key());
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
            client,
            policy.clone(),
            engine_settings,
        );
        let downloads = DownloadService::new(store.clone(), engine, providers, policy);
        let router = router(AppState {
            downloads,
            auth: auth.clone(),
            settings,
            store,
            secure_cookies: false,
        });
        Self {
            router,
            auth,
            _dir: dir,
        }
    }

    /// Complete setup and return a session cookie header value.
    async fn with_admin() -> (Self, String) {
        Self::with_admin_and_env_key(None).await
    }

    async fn with_admin_and_env_key(key: Option<crate::config::Secret>) -> (Self, String) {
        let app = Self::build(key).await;
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
        "/api/v1/tokens",
        "/api/v1/tokens/{id}",
    ] {
        assert!(doc["paths"].get(path).is_some(), "missing {path}");
    }
    assert!(doc["components"]["securitySchemes"]["bearer"].is_object());
    assert!(doc["components"]["schemas"]["DownloadDto"].is_object());
    assert!(doc["components"]["schemas"]["RetryDownload"].is_object());
    assert!(doc["paths"].get("/api/v1/settings/onefichier").is_some());
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
    let r = app.get("/ui/rows", &[("hx-request", "true")]).await;
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
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.error_kind(), "invalid_request");
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
async fn ui_submit_and_actions() {
    let (app, cookie) = TestApp::with_admin().await;
    let auth = cookie_header(&cookie);

    let r = app
        .post_form(
            "/ui/downloads",
            &auth,
            "url=https%3A%2F%2Fexample.com%2Fmovie.mkv",
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Queued https://example.com/movie.mkv"));
    assert_eq!(r.header("hx-trigger"), "downloads-changed");

    let rows = app.get("/ui/rows?f=active", &auth).await;
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
            "/ui/downloads",
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
        "/ui/rows".to_owned(),
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
