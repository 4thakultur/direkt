//! HTTP layer: JSON API, web UI, authentication middleware, and security
//! headers. Handlers stay thin; business rules live in the services.

pub mod api;
mod error;
pub mod server;
mod ui;

pub use error::{ApiError, ErrorBody};

use crate::auth::{AuthService, SESSION_TTL, Session};
use crate::error::{AppError, ErrorKind};
use crate::media::ArtworkService;
use crate::service::DownloadService;
use crate::settings::SettingsService;
use crate::store::Store;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

pub const SESSION_COOKIE: &str = "direkt_session";

#[derive(Clone)]
pub struct AppState {
    pub downloads: DownloadService,
    pub auth: AuthService,
    pub settings: SettingsService,
    pub artwork: ArtworkService,
    pub store: Store,
    pub secure_cookies: bool,
}

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .nest("/api/v1", api::routes())
        .merge(ui::protected_routes())
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .merge(protected)
        .merge(ui::public_routes().route_layer(middleware::from_fn(reject_cross_site)))
        .route("/healthz", get(health))
        .route("/api/openapi.json", get(api::openapi_json))
        .fallback(not_found)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Response {
    match state.store.ping().await {
        Ok(()) => Json(json!({ "status": "ok" })).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "error", "database": "unavailable" })),
        )
            .into_response(),
    }
}

async fn not_found(request: Request) -> Response {
    if request.uri().path().starts_with("/api/") {
        ApiError::from(AppError::not_found("no such endpoint")).into_response()
    } else {
        (StatusCode::NOT_FOUND, "Not found").into_response()
    }
}

/// Accept either an API bearer token or a valid session cookie.
async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(value) = request.headers().get(header::AUTHORIZATION) {
        let token = value
            .to_str()
            .ok()
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim);
        return match token {
            Some(token) => match state.auth.api_token_valid(token).await {
                Ok(true) => next.run(request).await,
                Ok(false) => unauthenticated(&request, "invalid API token"),
                Err(e) => ApiError::from(e).into_response(),
            },
            None => unauthenticated(&request, "unsupported Authorization scheme; use Bearer"),
        };
    }

    if let Some(session) = read_cookie(request.headers(), SESSION_COOKIE) {
        match state.auth.session_valid(session).await {
            Ok(true) => {
                return reject_cross_site(request, next).await;
            }
            Ok(false) => {}
            Err(e) => return ApiError::from(e).into_response(),
        }
    }
    unauthenticated(&request, "authentication required")
}

fn unauthenticated(request: &Request, message: &str) -> Response {
    if request.uri().path().starts_with("/api/") {
        return ApiError::from(AppError::unauthorized(message)).into_response();
    }
    if request.headers().contains_key("hx-request") {
        // htmx follows this header instead of swapping the response.
        return (StatusCode::UNAUTHORIZED, [("hx-redirect", "/login")]).into_response();
    }
    Redirect::to("/login").into_response()
}

/// Refuse state-changing requests that a browser sent from another site.
/// Guards cookie-authenticated requests, and also the login and setup forms,
/// so another site cannot make a visitor's browser use up their sign-in
/// attempts (locking them out) or sign them in to an attacker's session.
async fn reject_cross_site(request: Request, next: Next) -> Response {
    if !same_origin(request.method(), request.headers()) {
        tracing::warn!(path = %request.uri().path(), "rejected cross-site request");
        return ApiError::from(AppError::new(
            ErrorKind::Forbidden,
            "cross-site request rejected",
        ))
        .into_response();
    }
    next.run(request).await
}

/// Defence in depth against CSRF for cookie-authenticated requests, on top of
/// the `SameSite=Strict` cookie. Unsafe methods must come from this origin.
fn same_origin(method: &Method, headers: &HeaderMap) -> bool {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return true;
    }
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        return site == "same-origin" || site == "none";
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
        let origin_host = origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"));
        return origin_host.is_some() && origin_host == host;
    }
    // No browser provenance headers: a non-browser client or a very old
    // browser. SameSite=Strict still keeps the cookie off cross-site requests.
    true
}

pub(crate) fn read_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
}

pub(crate) fn session_cookie(session: &Session, secure: bool) -> HeaderValue {
    let cookie = format!(
        "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        session.token,
        SESSION_TTL.as_secs(),
        if secure { "; Secure" } else { "" }
    );
    HeaderValue::from_str(&cookie).expect("session token is hex")
}

pub(crate) fn clear_session_cookie(secure: bool) -> HeaderValue {
    let cookie = format!(
        "{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    );
    HeaderValue::from_str(&cookie).expect("static cookie")
}

/// The connecting client's address, or `0.0.0.0` if unknown (tests).
pub(crate) fn client_ip(request_extensions: &axum::http::Extensions) -> IpAddr {
    request_extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

async fn security_headers(request: Request, next: Next) -> Response {
    let is_static = request.uri().path().starts_with("/static/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let mut set = |name: HeaderName, value: &'static str| {
        if !headers.contains_key(&name) {
            headers.insert(name, HeaderValue::from_static(value));
        }
    };
    set(header::CONTENT_SECURITY_POLICY, CSP);
    set(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(header::X_FRAME_OPTIONS, "DENY");
    set(header::REFERRER_POLICY, "no-referrer");
    if !is_static {
        set(header::CACHE_CONTROL, "no-store");
    }
    response
}

#[cfg(test)]
mod tests;
