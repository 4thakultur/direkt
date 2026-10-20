//! Server-rendered web UI (askama templates + htmx).
//!
//! Every action goes through the same services as the JSON API. Live parts of
//! the page (download list, download details) poll small HTML fragments, and
//! actions respond with a flash message plus an `HX-Trigger` that refreshes
//! them.

use super::{
    AppState, PENDING_LOGIN_COOKIE, SESSION_COOKIE, clear_pending_login_cookie,
    clear_session_cookie, client_ip, pending_login_cookie, read_cookie, session_cookie,
};
use crate::auth::{LoginOutcome, TwoFactorStatus};
use crate::domain::{Download, DownloadEvent, MediaType, PostState, TransferState};
use crate::engine::{HISTORY_SECS, LiveSnapshot, ThroughputSnapshot};
use crate::error::{AppError, ErrorKind};
use crate::service::Preview;
use crate::settings::{
    FolderSetting, LibrarySettings, MiscSettings, MiscUpdate, OneFichierSettings, SECRET_MASK,
    SpeedUnit, TmdbSettings, parse_speed_limit,
};
use crate::store::ApiToken;
use askama::Template;
use axum::Router;
use axum::extract::{Form, FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::fmt::Write as _;
use uuid::Uuid;

const HTMX_JS: &str = include_str!("../../static/htmx-2.0.4.min.js");
const CLASSIC_CSS: &str = include_str!("../../static/classic.css");
const STEVE_CSS: &str = include_str!("../../static/steve.css");
const APP_JS: &str = include_str!("../../static/app.js");
/// Most downloads shown in the queue sections (everything not finished).
const QUEUE_LIMIT: i64 = 500;
/// Most finished downloads shown per history section.
const HISTORY_LIMIT: i64 = 50;

pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/login", get(login_page).post(login_submit))
        .route("/login/code", get(login_code_page).post(login_code_submit))
        .route("/setup", get(setup_page).post(setup_submit))
        .route("/static/htmx-2.0.4.min.js", get(htmx_js))
        .route("/static/classic.css", get(|| css(CLASSIC_CSS)))
        .route("/static/steve.css", get(|| css(STEVE_CSS)))
        .route("/static/app.js", get(app_js))
}

pub fn protected_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(downloads_page))
        .route("/downloads/{id}", get(download_page))
        .route("/settings", get(settings_page))
        .route("/settings/api", get(api_page))
        .route("/settings/tokens", post(create_token))
        .route("/settings/tokens/{id}/revoke", post(revoke_token))
        .route("/settings/password", post(change_password))
        .route("/settings/two-factor/setup", post(start_two_factor))
        .route("/settings/two-factor/enable", post(enable_two_factor))
        .route("/settings/two-factor/cancel", post(cancel_two_factor))
        .route("/settings/two-factor/disable", post(disable_two_factor))
        .route(
            "/settings/two-factor/recovery-codes",
            post(new_recovery_codes),
        )
        .route("/settings/theme", post(change_theme))
        .route("/settings/tmdb", post(save_tmdb))
        .route("/settings/tmdb/remove", post(remove_tmdb))
        .route("/settings/misc", post(save_misc))
        .route("/artwork/{id}", get(artwork_image))
        .route("/settings/onefichier", post(save_onefichier))
        .route("/settings/onefichier/remove", post(remove_onefichier))
        .route("/settings/onefichier/sign-in", post(sign_in_onefichier))
        .route("/settings/onefichier/sign-out", post(sign_out_onefichier))
        .route(
            "/settings/onefichier/cdn/refresh",
            post(refresh_onefichier_cdn),
        )
        .route("/logout", post(logout))
        .route("/ui/dashboard", get(dashboard_fragment))
        .route("/ui/submit", get(submit_fragment))
        .route("/ui/downloads/preview", post(preview_download))
        .route("/ui/previews/{id}/type", post(retype_preview))
        .route("/ui/downloads", post(confirm_download))
        .route("/ui/downloads/{id}/detail", get(detail_fragment))
        .route(
            "/ui/cancelled/delete",
            get(|| confirm_delete_all(TransferState::Cancelled))
                .post(|state| delete_all(state, TransferState::Cancelled)),
        )
        .route(
            "/ui/completed/delete",
            get(|| confirm_delete_all(TransferState::Completed))
                .post(|state| delete_all(state, TransferState::Completed)),
        )
        .route("/ui/downloads/{id}/{action}", post(download_action))
        .route("/ui/downloads/{id}/{action}/confirm", get(confirm_action))
        .route("/ui/modal/close", get(close_modal))
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

fn render(template: &impl Template) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "template rendering failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

fn render_status(status: StatusCode, template: &impl Template) -> Response {
    let mut response = render(template);
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    response
}

/// The look of the web UI. Chosen per browser on the settings page and kept in
/// a cookie, so a phone and a desktop can differ. Every full page renders with
/// it; fragments only use classes both stylesheets style.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Theme {
    /// "Beyond": black and red, flat.
    Classic,
    /// Light and dark, after Apple's iOS, macOS and apple.com.
    #[default]
    Steve,
}

const THEME_COOKIE: &str = "direkt_theme";

impl Theme {
    const ALL: [Theme; 2] = [Theme::Steve, Theme::Classic];

    /// The cookie value, `data-theme` attribute and stylesheet name.
    fn id(self) -> &'static str {
        match self {
            Theme::Classic => "classic",
            Theme::Steve => "steve",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Theme::Classic => "Beyond",
            Theme::Steve => "Steve",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Theme::Classic => "Black and red, flat and dense.",
            Theme::Steve => "Light and dark, after iOS and macOS.",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.id() == value)
    }

    fn cookie(self, secure: bool) -> HeaderValue {
        let cookie = format!(
            "{THEME_COOKIE}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000{}",
            self.id(),
            if secure { "; Secure" } else { "" }
        );
        HeaderValue::from_str(&cookie).expect("theme ids are ASCII")
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Theme {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(read_cookie(&parts.headers, THEME_COOKIE)
            .and_then(Theme::parse)
            .unwrap_or_default())
    }
}

fn status_for(err: &AppError) -> StatusCode {
    super::error::status_for(err.kind)
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn human_duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// How long ago `t` was, coarsely; older than a day shows the date.
fn human_ago(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    match (now - t).num_seconds().max(0) {
        0..60 => "just now".into(),
        s @ 60..3600 => format!("{} min ago", s / 60),
        s @ 3600..86400 => format!("{} h ago", s / 3600),
        _ => t.format("%Y-%m-%d").to_string(),
    }
}

fn fmt_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

fn state_label(state: TransferState) -> &'static str {
    match state {
        TransferState::Queued => "Queued",
        TransferState::Downloading => "Downloading",
        TransferState::Retrying => "Retrying",
        TransferState::Paused => "Paused",
        TransferState::Blocked => "Needs attention",
        TransferState::Completed => "Completed",
        TransferState::Failed => "Failed",
        TransferState::Cancelled => "Cancelled",
    }
}

/// `noted`: post-processing left a note, which for a completed one means
/// the file went to the fallback folder.
fn post_label(state: PostState, noted: bool) -> Option<&'static str> {
    match state {
        PostState::None => None,
        PostState::Pending => Some("Organizing pending"),
        PostState::Processing => Some("Organizing"),
        PostState::Completed if noted => Some("Needs sorting"),
        PostState::Completed => Some("Organized"),
        PostState::Failed => Some("Organizing failed"),
        PostState::Skipped => None,
    }
}

/// A download prepared for display.
struct DownloadView {
    id: String,
    name: String,
    display_url: String,
    provider: String,
    state: &'static str,
    state_label: &'static str,
    post_label: Option<&'static str>,
    percent: Option<u64>,
    progress: String,
    speed: Option<String>,
    eta: Option<String>,
    note: Option<String>,
    post_note: Option<String>,
    file_path: Option<String>,
    created: String,
    updated: String,
    completed: Option<String>,
    can_pause: bool,
    can_resume: bool,
    can_retry: bool,
    can_cancel: bool,
    /// Remove from the list (cancelled or completed). Offered on the
    /// dashboard only: the detail page would outlive the row.
    can_delete: bool,
    /// Delete a completed download's file as well. Dashboard only.
    can_delete_file: bool,
    /// Organize again, after organizing failed or was skipped.
    can_organize: bool,
    /// Downloading without a known size: show an indeterminate bar.
    indeterminate: bool,
    /// Short label on the cover tile, e.g. the file extension.
    badge: String,
    /// Cached artwork id, shown instead of the badge.
    artwork: Option<String>,
    /// The type chosen at confirmation, e.g. "TV series".
    media_label: Option<&'static str>,
    /// Title found by the metadata lookup.
    title: Option<String>,
    /// When it finished, for the completed list.
    finished: Option<String>,
}

/// The file extension in capitals if it looks like one, else `FILE`.
fn tile_badge(name: &str) -> String {
    name.rsplit_once('.')
        .filter(|(stem, ext)| {
            !stem.is_empty()
                && (1..=4).contains(&ext.len())
                && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .map(|(_, ext)| ext.to_ascii_uppercase())
        .unwrap_or_else(|| "FILE".into())
}

/// The last path segment of a URL, as a stand-in name until the provider
/// reports the real file name.
fn url_file_name(display_url: &str) -> Option<String> {
    let url = url::Url::parse(display_url).ok()?;
    let last = url.path_segments()?.rfind(|s| !s.is_empty())?;
    let name = percent_encoding::percent_decode_str(last).decode_utf8_lossy();
    (!name.trim().is_empty()).then(|| name.into_owned())
}

impl DownloadView {
    fn new(d: Download, live: Option<LiveSnapshot>, unit: SpeedUnit) -> Self {
        use TransferState::*;
        let deletable_completed = d.state == Completed && d.post_state != PostState::Processing;
        // An organized file is in the media library, which Direkt never
        // deletes from.
        let can_delete_file =
            deletable_completed && d.file_path.is_some() && d.post_state != PostState::Completed;
        let can_organize =
            d.state == Completed && matches!(d.post_state, PostState::Failed | PostState::Skipped);
        let downloaded = live
            .map(|l| l.downloaded_bytes)
            .unwrap_or(u64::try_from(d.downloaded_bytes).unwrap_or(0));
        let total = d.total_bytes.and_then(|t| u64::try_from(t).ok());
        let percent = total
            .filter(|&t| t > 0)
            .map(|t| (downloaded.min(t) * 100) / t);
        let progress = match total {
            Some(t) if d.state == Completed => human_bytes(t),
            Some(t) => format!("{} / {}", human_bytes(downloaded), human_bytes(t)),
            None if downloaded > 0 => human_bytes(downloaded),
            None => String::new(),
        };
        let rate = live.map(|l| l.bytes_per_sec).filter(|&r| r > 0);
        let speed = rate.map(|r| unit.format(r));
        let eta = match (rate, total) {
            (Some(r), Some(t)) if t > downloaded => Some(human_duration((t - downloaded) / r)),
            _ => None,
        };
        let note = match (d.state, &d.error, d.next_attempt_at) {
            (Retrying, Some(e), Some(at)) => {
                let wait = (at - Utc::now()).num_seconds().max(0) as u64;
                Some(format!(
                    "{} — retry {} in {}",
                    e.message,
                    d.retry_count,
                    human_duration(wait)
                ))
            }
            (Queued | Downloading | Completed, _, _) => None,
            (_, Some(e), _) => Some(e.message.clone()),
            _ => None,
        };
        let name = d
            .filename
            .clone()
            .or_else(|| url_file_name(&d.display_url))
            .unwrap_or_else(|| d.display_url.clone());
        Self {
            artwork: d.artwork_id,
            media_label: d.media_type.map(MediaType::label),
            title: d.title,
            id: d.id.to_string(),
            badge: tile_badge(&name),
            indeterminate: d.state == Downloading && percent.is_none(),
            finished: d.completed_at.map(|t| human_ago(t, Utc::now())),
            name,
            display_url: d.display_url,
            provider: d.provider,
            state: d.state.as_str(),
            state_label: state_label(d.state),
            post_label: post_label(d.post_state, d.post_error.is_some()),
            percent,
            progress,
            speed,
            eta,
            note,
            post_note: d.post_error.map(|e| e.message),
            file_path: d.file_path,
            created: fmt_time(d.created_at),
            updated: fmt_time(d.updated_at),
            completed: d.completed_at.map(fmt_time),
            can_pause: matches!(d.state, Queued | Downloading | Retrying),
            can_resume: d.state == Paused,
            can_retry: matches!(d.state, Failed | Blocked | Cancelled),
            can_cancel: matches!(d.state, Queued | Downloading | Retrying | Paused | Blocked),
            can_delete: d.state == Cancelled || deletable_completed,
            can_delete_file,
            can_organize,
        }
    }
}

struct EventView {
    at: String,
    kind: String,
    message: String,
}

impl From<DownloadEvent> for EventView {
    fn from(e: DownloadEvent) -> Self {
        Self {
            at: fmt_time(e.at),
            kind: e.kind,
            message: e.message,
        }
    }
}

// ---------------------------------------------------------------------------
// Static assets
// ---------------------------------------------------------------------------

/// A cached artwork image. Ids are content-addressed, so browsers may keep it.
async fn artwork_image(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.artwork.image(&id).await {
        Ok(Some((bytes, content_type))) => (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "private, max-age=604800"),
            ],
            bytes,
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "Not found").into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "reading cached artwork failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

async fn htmx_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        HTMX_JS,
    )
}

async fn css(stylesheet: &'static str) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        stylesheet,
    )
}

async fn app_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        APP_JS,
    )
}

// ---------------------------------------------------------------------------
// Login and setup
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    theme: Theme,
    error: Option<String>,
    username: String,
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login_page(State(state): State<AppState>, theme: Theme) -> Response {
    match state.auth.needs_setup().await {
        Ok(true) => Redirect::to("/setup").into_response(),
        _ => render(&LoginPage {
            theme,
            error: None,
            username: String::new(),
        }),
    }
}

async fn login_submit(
    State(state): State<AppState>,
    theme: Theme,
    extensions: Extensions,
    Form(form): Form<LoginForm>,
) -> Response {
    match state
        .auth
        .login(client_ip(&extensions), &form.username, &form.password)
        .await
    {
        Ok(LoginOutcome::Session(session)) => (
            [(
                header::SET_COOKIE,
                session_cookie(&session, state.secure_cookies),
            )],
            Redirect::to("/"),
        )
            .into_response(),
        Ok(LoginOutcome::CodeRequired(token)) => (
            [(
                header::SET_COOKIE,
                pending_login_cookie(&token, state.secure_cookies),
            )],
            Redirect::to("/login/code"),
        )
            .into_response(),
        Err(e) => render_status(
            status_for(&e),
            &LoginPage {
                theme,
                error: Some(e.message),
                username: form.username,
            },
        ),
    }
}

#[derive(Template)]
#[template(path = "login_code.html")]
struct LoginCodePage {
    theme: Theme,
    error: Option<String>,
}

#[derive(Deserialize)]
struct CodeForm {
    code: String,
}

/// Second sign-in step: the code from the authenticator app.
async fn login_code_page(
    State(state): State<AppState>,
    theme: Theme,
    headers: HeaderMap,
) -> Response {
    match read_cookie(&headers, PENDING_LOGIN_COOKIE) {
        Some(token) if state.auth.login_pending(token) => {
            render(&LoginCodePage { theme, error: None })
        }
        _ => Redirect::to("/login").into_response(),
    }
}

async fn login_code_submit(
    State(state): State<AppState>,
    theme: Theme,
    extensions: Extensions,
    headers: HeaderMap,
    Form(form): Form<CodeForm>,
) -> Response {
    let Some(token) = read_cookie(&headers, PENDING_LOGIN_COOKIE) else {
        return Redirect::to("/login").into_response();
    };
    match state
        .auth
        .complete_login(client_ip(&extensions), token, &form.code)
        .await
    {
        Ok(session) => {
            let mut response = Redirect::to("/").into_response();
            let headers = response.headers_mut();
            headers.append(
                header::SET_COOKIE,
                session_cookie(&session, state.secure_cookies),
            );
            headers.append(
                header::SET_COOKIE,
                clear_pending_login_cookie(state.secure_cookies),
            );
            response
        }
        // Expired or out of attempts: back to the password.
        Err(e) if !state.auth.login_pending(token) => {
            let mut response = render_status(
                status_for(&e),
                &LoginPage {
                    theme,
                    error: Some(e.message),
                    username: String::new(),
                },
            );
            response.headers_mut().append(
                header::SET_COOKIE,
                clear_pending_login_cookie(state.secure_cookies),
            );
            response
        }
        Err(e) => render_status(
            status_for(&e),
            &LoginCodePage {
                theme,
                error: Some(e.message),
            },
        ),
    }
}

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupPage {
    theme: Theme,
    error: Option<String>,
    token_path: String,
    username: String,
}

#[derive(Deserialize)]
struct SetupForm {
    setup_token: String,
    username: String,
    password: String,
    password_confirm: String,
}

fn setup_token_path(state: &AppState) -> String {
    state
        .auth
        .pending_setup_token_path()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

async fn setup_page(State(state): State<AppState>, theme: Theme) -> Response {
    match state.auth.needs_setup().await {
        Ok(false) => Redirect::to("/login").into_response(),
        _ => render(&SetupPage {
            theme,
            error: None,
            token_path: setup_token_path(&state),
            username: String::new(),
        }),
    }
}

async fn setup_submit(
    State(state): State<AppState>,
    theme: Theme,
    extensions: Extensions,
    Form(form): Form<SetupForm>,
) -> Response {
    let result = if form.password != form.password_confirm {
        Err(AppError::invalid("passwords do not match"))
    } else {
        state
            .auth
            .setup(
                client_ip(&extensions),
                &form.setup_token,
                &form.username,
                &form.password,
            )
            .await
    };
    match result {
        Ok(session) => (
            [(
                header::SET_COOKIE,
                session_cookie(&session, state.secure_cookies),
            )],
            Redirect::to("/"),
        )
            .into_response(),
        Err(e) if e.kind == ErrorKind::Conflict => Redirect::to("/login").into_response(),
        Err(e) => render_status(
            status_for(&e),
            &SetupPage {
                theme,
                error: Some(e.message),
                token_path: setup_token_path(&state),
                username: form.username,
            },
        ),
    }
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = read_cookie(&headers, SESSION_COOKIE) {
        let _ = state.auth.logout(token).await;
    }
    (
        [(
            header::SET_COOKIE,
            clear_session_cookie(state.secure_cookies),
        )],
        Redirect::to("/login"),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Downloads
// ---------------------------------------------------------------------------

/// Network graph and counters for the dashboard header.
struct StatsView {
    current: String,
    peak: String,
    total: String,
    free: String,
    /// Speed at the top of the graph.
    scale: String,
    /// The overall speed limit, if one is set.
    limit: Option<String>,
    /// SVG path data for the filled area and its outline.
    area: String,
    line: String,
}

/// Graph height in SVG units; the width is one unit per second of history.
const GRAPH_HEIGHT: f64 = 100.0;
/// Lowest graph scale, so an idle graph does not amplify noise.
const MIN_GRAPH_SCALE: u64 = 64 * 1024;

/// SVG paths (area, line) for samples right-aligned in a fixed window, and the
/// scale they are drawn against: the highest sample rounded up to a value
/// that reads round in `unit`.
fn graph_paths(samples: &[u64], unit: SpeedUnit) -> (String, String, u64) {
    let width = (HISTORY_SECS - 1) as f64;
    let max = samples.iter().copied().max().unwrap_or(0);
    let scale = unit.round_up(max.max(MIN_GRAPH_SCALE));
    // Leave headroom so the line never touches the top edge.
    let y = |v: u64| GRAPH_HEIGHT - (v as f64 / scale as f64) * (GRAPH_HEIGHT * 0.94);
    let samples = &samples[samples.len().saturating_sub(HISTORY_SECS)..];
    if samples.is_empty() {
        let flat = format!("M0,{GRAPH_HEIGHT}H{width}");
        return (String::new(), flat, scale);
    }
    let first_x = width - (samples.len() - 1) as f64;
    let mut line = String::with_capacity(samples.len() * 12);
    for (i, &v) in samples.iter().enumerate() {
        let cmd = if i == 0 { 'M' } else { 'L' };
        let _ = write!(line, "{cmd}{:.0},{:.1}", first_x + i as f64, y(v));
    }
    let area = format!("{line}L{width},{GRAPH_HEIGHT}L{first_x},{GRAPH_HEIGHT}Z");
    (area, line, scale)
}

impl StatsView {
    fn new(t: ThroughputSnapshot, misc: &MiscSettings) -> Self {
        let unit = misc.speed_unit;
        let (area, line, scale) = graph_paths(&t.samples, unit);
        Self {
            current: unit.format(t.current),
            peak: unit.format(t.peak),
            total: human_bytes(t.total),
            free: t.free_bytes.map(human_bytes).unwrap_or_else(|| "—".into()),
            scale: unit.format(scale),
            limit: misc.speed_limit.map(|l| unit.format(l)),
            area,
            line,
        }
    }
}

/// A titled list of downloads below the "now downloading" cards.
struct Group {
    title: &'static str,
    rows: Vec<DownloadView>,
    /// The list was cut at its limit; older entries exist.
    truncated: bool,
    /// A button that empties the list.
    clear: Option<ClearAction>,
}

struct ClearAction {
    /// GET shows the confirmation dialog, POST empties the list.
    url: &'static str,
    label: &'static str,
}

#[derive(Template)]
#[template(path = "downloads.html")]
struct DownloadsPage {
    theme: Theme,
    dash: Dashboard,
    submit: SubmitForm,
}

#[derive(Template)]
#[template(path = "_dashboard.html")]
struct Dashboard {
    stats: StatsView,
    downloading: Vec<DownloadView>,
    /// How many are queued behind the running transfers.
    queued: usize,
    groups: Vec<Group>,
    error: Option<String>,
}

#[derive(Template, Default)]
#[template(path = "_submit.html")]
struct SubmitForm {
    value: String,
    preview: Option<PreviewView>,
    error: Option<String>,
}

/// The confirmation menu shown after clicking Download.
struct PreviewView {
    id: String,
    /// The looked-up title, or the file name.
    heading: String,
    /// The file name, when the heading is a looked-up title.
    filename: Option<String>,
    year: Option<u16>,
    provider: &'static str,
    size: String,
    artwork: Option<String>,
    badge: String,
    types: Vec<TypeOption>,
    notes: Vec<String>,
    /// Show the file password field.
    accepts_password: bool,
}

struct TypeOption {
    value: &'static str,
    label: &'static str,
    selected: bool,
    detected: bool,
}

fn provider_label(id: &str) -> &'static str {
    match id {
        "1fichier" => "1fichier",
        _ => "Direct link",
    }
}

impl From<Preview> for PreviewView {
    fn from(p: Preview) -> Self {
        let name = p.filename.clone().unwrap_or_else(|| p.display_url.clone());
        let (heading, filename) = match p.title {
            Some(title) => (title, Some(name.clone())),
            None => (name.clone(), None),
        };
        let size = match (p.size_bytes, p.size_approximate) {
            (Some(n), true) => format!("\u{2248} {}", human_bytes(n)),
            (Some(n), false) => human_bytes(n),
            (None, _) => "Unknown".into(),
        };
        Self {
            badge: tile_badge(&name),
            id: p.id.to_string(),
            heading,
            filename,
            year: p.year,
            provider: provider_label(&p.provider),
            size,
            artwork: p.artwork_id,
            types: MediaType::ALL
                .into_iter()
                .map(|t| TypeOption {
                    value: t.as_str(),
                    label: t.label(),
                    selected: t == p.media_type,
                    detected: t == p.detected_type,
                })
                .collect(),
            notes: p.notes,
            accepts_password: p.accepts_password,
        }
    }
}

async fn load_dashboard(state: &AppState) -> Dashboard {
    use TransferState::*;
    let service = &state.downloads;
    let (unfinished, completed, cancelled) = tokio::join!(
        service.list(
            &[Queued, Downloading, Retrying, Paused, Blocked, Failed],
            QUEUE_LIMIT,
            0
        ),
        service.list(&[Completed], HISTORY_LIMIT, 0),
        service.list(&[Cancelled], HISTORY_LIMIT, 0),
    );
    let mut error = None;
    let mut take = |result: Result<Vec<Download>, AppError>| match result {
        Ok(list) => list,
        Err(e) => {
            error.get_or_insert(e.message);
            Vec::new()
        }
    };
    let (unfinished, mut completed, cancelled) =
        (take(unfinished), take(completed), take(cancelled));
    let history_truncated = |list: &[Download]| list.len() as i64 >= HISTORY_LIMIT;
    let (completed_more, cancelled_more) =
        (history_truncated(&completed), history_truncated(&cancelled));
    completed.sort_by_key(|d| std::cmp::Reverse(d.completed_at));

    let misc = state.settings.misc();
    let view = |d: Download| {
        let live = service.live(d.id);
        DownloadView::new(d, live, misc.speed_unit)
    };
    let mut downloading = Vec::new();
    let mut up_next = Vec::new();
    let mut attention = Vec::new();
    let mut paused = Vec::new();
    // Oldest first, the order the engine claims queued work in.
    for d in unfinished.into_iter().rev() {
        let bucket = match d.state {
            Downloading => &mut downloading,
            Queued | Retrying => &mut up_next,
            Paused => &mut paused,
            _ => &mut attention,
        };
        bucket.push(view(d));
    }
    let queued = up_next.len();
    let groups = [
        ("Up next", up_next, false),
        ("Needs attention", attention, false),
        ("Paused", paused, false),
        (
            "Completed",
            completed.into_iter().map(view).collect(),
            completed_more,
        ),
        (
            "Cancelled",
            cancelled.into_iter().map(view).collect(),
            cancelled_more,
        ),
    ]
    .into_iter()
    .filter(|(_, rows, _)| !rows.is_empty())
    .map(|(title, rows, truncated)| Group {
        clear: match title {
            "Cancelled" => Some(ClearAction {
                url: "/ui/cancelled/delete",
                label: "Delete all",
            }),
            "Completed" => Some(ClearAction {
                url: "/ui/completed/delete",
                label: "Clear list",
            }),
            _ => None,
        },
        title,
        rows,
        truncated,
    })
    .collect();
    Dashboard {
        stats: StatsView::new(service.throughput(), &misc),
        downloading,
        queued,
        groups,
        error,
    }
}

async fn downloads_page(State(state): State<AppState>, theme: Theme) -> Response {
    render(&DownloadsPage {
        theme,
        dash: load_dashboard(&state).await,
        submit: SubmitForm::default(),
    })
}

async fn dashboard_fragment(State(state): State<AppState>) -> Response {
    render(&load_dashboard(&state).await)
}

#[derive(Deserialize)]
struct SubmitBody {
    url: String,
    #[serde(default)]
    password: Option<String>,
}

fn with_refresh(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("hx-trigger", HeaderValue::from_static("downloads-changed"));
    response
}

/// An empty submit form (Cancel on the confirmation menu).
async fn submit_fragment() -> Response {
    render(&SubmitForm::default())
}

/// Download clicked: look the link up and show the confirmation menu, or
/// start it right away when confirmation is turned off.
async fn preview_download(State(state): State<AppState>, Form(body): Form<SubmitBody>) -> Response {
    let password = body.password.as_deref().filter(|p| !p.is_empty());
    if !state.settings.misc().confirm_downloads {
        return match state.downloads.submit_detected(&body.url, password).await {
            Ok(_) => download_started(),
            Err(e) => submit_error(e, body.url),
        };
    }
    match state.downloads.preview(&body.url, password).await {
        Ok(preview) => render(&SubmitForm {
            value: body.url,
            preview: Some(preview.into()),
            ..Default::default()
        }),
        Err(e) => submit_error(e, body.url),
    }
}

#[derive(Deserialize)]
struct ConfirmBody {
    preview_id: Uuid,
    media_type: String,
    /// The link field, kept on screen if the confirmation fails.
    #[serde(default)]
    url: String,
    /// Only on providers that take one; empty means none.
    #[serde(default)]
    password: Option<String>,
}

fn parse_media_type(value: &str) -> Result<MediaType, AppError> {
    MediaType::parse(value).ok_or_else(|| AppError::invalid("unknown type"))
}

/// The type was changed on the confirmation menu: look it up again.
async fn retype_preview(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Form(body): Form<ConfirmBody>,
) -> Response {
    let result = match parse_media_type(&body.media_type) {
        Ok(t) => state.downloads.retype(id, t).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(preview) => render(&SubmitForm {
            value: body.url,
            preview: Some(preview.into()),
            ..Default::default()
        }),
        Err(e) => submit_error(e, body.url),
    }
}

/// Start download clicked on the confirmation menu.
async fn confirm_download(
    State(state): State<AppState>,
    Form(body): Form<ConfirmBody>,
) -> Response {
    let result = match parse_media_type(&body.media_type) {
        Ok(t) => {
            state
                .downloads
                .confirm(body.preview_id, Some(t), body.password.as_deref())
                .await
        }
        Err(e) => Err(e),
    };
    match result {
        Ok(_) => download_started(),
        Err(e) => submit_error(e, body.url),
    }
}

/// An empty form. No message: the new download in the list (on phones, the
/// slide to Activity) says it started.
fn download_started() -> Response {
    let mut response = render(&SubmitForm::default());
    response.headers_mut().insert(
        "hx-trigger",
        HeaderValue::from_static("downloads-changed, download-started"),
    );
    response
}

fn submit_error(e: AppError, url: String) -> Response {
    render_status(
        status_for(&e),
        &SubmitForm {
            value: url,
            error: Some(e.message),
            ..Default::default()
        },
    )
}

#[derive(Template)]
#[template(path = "_flash.html")]
struct Flash {
    ok: bool,
    message: String,
}

/// The result of a download action: a toast, and the confirmation dialog
/// (if one led here) closed.
#[derive(Template)]
#[template(path = "_action_result.html")]
struct ActionResult {
    ok: bool,
    message: String,
}

/// An in-page confirmation dialog, swapped into `#modal`.
#[derive(Template)]
#[template(path = "_confirm.html")]
struct ConfirmDialog {
    title: &'static str,
    message: String,
    /// Extra line, e.g. the file path about to be deleted.
    detail: Option<String>,
    /// POSTed when confirmed.
    action: String,
    confirm: &'static str,
    dismiss: &'static str,
}

async fn close_modal() -> Response {
    Html("").into_response()
}

async fn confirm_action(
    State(state): State<AppState>,
    Path((id, action)): Path<(Uuid, String)>,
) -> Response {
    let d = match state.downloads.get(id).await {
        Ok(d) => d,
        Err(e) => {
            return render_status(
                status_for(&e),
                &Flash {
                    ok: false,
                    message: e.message,
                },
            );
        }
    };
    let name = d.filename.clone().unwrap_or_else(|| d.display_url.clone());
    let (title, message, detail, confirm, dismiss) = match action.as_str() {
        "cancel" => (
            "Cancel download?",
            format!("\u{201c}{name}\u{201d} stops, and its partial data is deleted."),
            None,
            "Cancel download",
            "Keep downloading",
        ),
        "delete" => (
            "Remove from list?",
            format!("\u{201c}{name}\u{201d} is removed from the list. The file stays on disk."),
            None,
            "Remove",
            "Keep",
        ),
        "delete-file" => (
            "Delete file?",
            format!(
                "\u{201c}{name}\u{201d} is deleted from disk and removed from the list. \
                 This can\u{2019}t be undone."
            ),
            d.file_path.clone(),
            "Delete file",
            "Keep file",
        ),
        _ => {
            return render_status(
                StatusCode::NOT_FOUND,
                &Flash {
                    ok: false,
                    message: "unknown action".into(),
                },
            );
        }
    };
    render(&ConfirmDialog {
        title,
        message,
        detail,
        action: format!("/ui/downloads/{id}/{action}"),
        confirm,
        dismiss,
    })
}

async fn confirm_delete_all(which: TransferState) -> Response {
    let dialog = if which == TransferState::Completed {
        ConfirmDialog {
            title: "Clear the completed list?",
            message: "All completed downloads are removed from the list. Their files stay on \
                      disk."
                .into(),
            detail: None,
            action: "/ui/completed/delete".into(),
            confirm: "Clear list",
            dismiss: "Keep",
        }
    } else {
        ConfirmDialog {
            title: "Delete all cancelled downloads?",
            message: "They are removed from the list.".into(),
            detail: None,
            action: "/ui/cancelled/delete".into(),
            confirm: "Delete all",
            dismiss: "Keep",
        }
    };
    render(&dialog)
}

async fn download_action(
    State(state): State<AppState>,
    Path((id, action)): Path<(Uuid, String)>,
) -> Response {
    let service = &state.downloads;
    let result = match action.as_str() {
        "pause" => service.pause(id).await.map(|_| "Paused"),
        "resume" => service.resume(id).await.map(|_| "Resumed"),
        "retry" => service.retry(id, None).await.map(|_| "Queued for retry"),
        "cancel" => service.cancel(id).await.map(|_| "Cancelled"),
        "delete" => service
            .delete(id, false)
            .await
            .map(|()| "Removed from the list"),
        "delete-file" => service.delete(id, true).await.map(|()| "File deleted"),
        "organize" => service.organize(id).await.map(|_| "Queued for organizing"),
        _ => Err(AppError::not_found("unknown action")),
    };
    match result {
        Ok(message) => with_refresh(render(&ActionResult {
            ok: true,
            message: message.to_owned(),
        })),
        Err(e) => with_refresh(render_status(
            status_for(&e),
            &ActionResult {
                ok: false,
                message: e.message,
            },
        )),
    }
}

async fn delete_all(State(state): State<AppState>, which: TransferState) -> Response {
    let (status, flash) = match state.downloads.delete_all(which).await {
        Ok(n) => (
            StatusCode::OK,
            ActionResult {
                ok: true,
                message: format!(
                    "Removed {n} {} download{} from the list",
                    which.as_str(),
                    if n == 1 { "" } else { "s" }
                ),
            },
        ),
        Err(e) => (
            status_for(&e),
            ActionResult {
                ok: false,
                message: e.message,
            },
        ),
    };
    with_refresh(render_status(status, &flash))
}

#[derive(Template)]
#[template(path = "download.html")]
struct DownloadPage {
    theme: Theme,
    d: DownloadView,
    events: Vec<EventView>,
}

#[derive(Template)]
#[template(path = "_detail.html")]
struct DetailFragment {
    d: DownloadView,
    events: Vec<EventView>,
}

async fn load_detail(
    state: &AppState,
    id: Uuid,
) -> Result<(DownloadView, Vec<EventView>), AppError> {
    let download = state.downloads.get(id).await?;
    let events = state.downloads.events(id).await?;
    let live = state.downloads.live(id);
    let mut view = DownloadView::new(download, live, state.settings.misc().speed_unit);
    view.can_delete = false;
    view.can_delete_file = false;
    Ok((
        view,
        events.into_iter().rev().map(EventView::from).collect(),
    ))
}

async fn download_page(
    State(state): State<AppState>,
    theme: Theme,
    Path(id): Path<Uuid>,
) -> Response {
    match load_detail(&state, id).await {
        Ok((d, events)) => render(&DownloadPage { theme, d, events }),
        Err(e) => (status_for(&e), e.message).into_response(),
    }
}

async fn detail_fragment(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    match load_detail(&state, id).await {
        Ok((d, events)) => render(&DetailFragment { d, events }),
        Err(e) => render_status(
            status_for(&e),
            &Flash {
                ok: false,
                message: e.message,
            },
        ),
    }
}

// ---------------------------------------------------------------------------
// Settings: API tokens and password
// ---------------------------------------------------------------------------

struct TokenView {
    id: String,
    name: String,
    created: String,
    last_used: String,
}

impl From<ApiToken> for TokenView {
    fn from(t: ApiToken) -> Self {
        Self {
            id: t.id.to_string(),
            name: t.name,
            created: fmt_time(t.created_at),
            last_used: t
                .last_used_at
                .map(fmt_time)
                .unwrap_or_else(|| "never".into()),
        }
    }
}

#[derive(Template, Default)]
#[template(path = "settings.html")]
struct SettingsPage {
    theme: Theme,
    onefichier: OneFichierSettings,
    tmdb: TmdbSettings,
    library: LibraryView,
    misc: MiscView,
    cdn: Option<CdnDisplay>,
    /// The account has CDN credits it uses for its own downloads.
    has_credits: bool,
    tokens: Vec<TokenView>,
    new_token: Option<String>,
    two_factor: TwoFactorView,
    /// Recovery codes just made, shown this once.
    recovery_codes: Vec<String>,
    message: Option<String>,
    error: Option<String>,
    /// The section to show expanded: the one the last action came from.
    open: &'static str,
}

/// Collapsible sections of the settings page, by element id.
const SECTION_ONEFICHIER: &str = "onefichier";
const SECTION_TMDB: &str = "artwork";
const SECTION_TOKENS: &str = "tokens";
const SECTION_PASSWORD: &str = "password";
const SECTION_TWO_FACTOR: &str = "two-factor";
const SECTION_APPEARANCE: &str = "appearance";
const SECTION_MISC: &str = "misc";

async fn settings_with(state: &AppState, theme: Theme, mut page: SettingsPage) -> SettingsPage {
    page.theme = theme;
    page.onefichier = state.settings.onefichier();
    page.tmdb = state.settings.tmdb();
    page.library = LibraryView::from(state.settings.library());
    page.misc = MiscView::from(&state.settings.misc());
    page.cdn = page.onefichier.cdn.as_ref().map(CdnDisplay::from);
    page.has_credits = page
        .onefichier
        .cdn
        .as_ref()
        .is_some_and(|c| c.balance.is_some() && c.in_use != Some(false));
    match state.auth.list_api_tokens().await {
        Ok(tokens) => page.tokens = tokens.into_iter().map(TokenView::from).collect(),
        Err(e) => page.error = page.error.or(Some(e.message)),
    }
    match state.auth.two_factor_status().await {
        Ok(status) => page.two_factor = TwoFactorView::from(status),
        Err(e) => page.error = page.error.or(Some(e.message)),
    }
    page
}

async fn settings_page(State(state): State<AppState>, theme: Theme) -> Response {
    render(&settings_with(&state, theme, SettingsPage::default()).await)
}

#[derive(Template)]
#[template(path = "api.html")]
struct ApiPage {
    theme: Theme,
    api: &'static super::api_docs::ApiReference,
    /// This server's address as the browser reached it, for the examples.
    origin: String,
}

async fn api_page(State(state): State<AppState>, theme: Theme, headers: HeaderMap) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:9877");
    let scheme = if state.secure_cookies {
        "https"
    } else {
        "http"
    };
    render(&ApiPage {
        theme,
        api: &super::api_docs::REFERENCE,
        origin: format!("{scheme}://{host}"),
    })
}

#[derive(Deserialize)]
struct TokenForm {
    name: String,
}

async fn create_token(
    State(state): State<AppState>,
    theme: Theme,
    Form(form): Form<TokenForm>,
) -> Response {
    let page = match state.auth.create_api_token(&form.name).await {
        Ok((token, secret)) => SettingsPage {
            new_token: Some(secret),
            message: Some(format!(
                "Created token \u{201c}{}\u{201d}. Copy it now; it will not be shown again.",
                token.name
            )),
            open: SECTION_TOKENS,
            ..Default::default()
        },
        Err(e) => SettingsPage {
            error: Some(e.message),
            open: SECTION_TOKENS,
            ..Default::default()
        },
    };
    render(&settings_with(&state, theme, page).await)
}

async fn revoke_token(
    State(state): State<AppState>,
    theme: Theme,
    Path(id): Path<Uuid>,
) -> Response {
    let page = match state.auth.revoke_api_token(id).await {
        Ok(()) => SettingsPage {
            message: Some("Token revoked.".into()),
            open: SECTION_TOKENS,
            ..Default::default()
        },
        Err(e) => SettingsPage {
            error: Some(e.message),
            open: SECTION_TOKENS,
            ..Default::default()
        },
    };
    render(&settings_with(&state, theme, page).await)
}

/// CDN credits prepared for the settings page.
struct CdnDisplay {
    balance: String,
    usage: &'static str,
    source: &'static str,
    checked: String,
}

impl From<&crate::providers::CdnView> for CdnDisplay {
    fn from(c: &crate::providers::CdnView) -> Self {
        Self {
            balance: c.balance.clone().unwrap_or_else(|| "none".into()),
            usage: match c.in_use {
                Some(true) => "used for your downloads",
                Some(false) => "not used for your downloads",
                None => "usage unknown",
            },
            source: match c.source {
                crate::providers::CdnSource::Account => "account",
                crate::providers::CdnSource::ApiKey => "API key",
            },
            checked: fmt_time(c.checked_at),
        }
    }
}

#[derive(Deserialize)]
struct OneFichierForm {
    api_key: String,
}

async fn save_onefichier(
    State(state): State<AppState>,
    theme: Theme,
    Form(form): Form<OneFichierForm>,
) -> Response {
    let value = form.api_key.trim();
    let configured = state.settings.onefichier().configured;
    // The form is pre-filled with the mask; submitting it unchanged is a no-op.
    let result =
        if value == SECRET_MASK && configured {
            Ok("1fichier API key unchanged.")
        } else if value.is_empty() {
            Err(AppError::invalid("enter a 1fichier API key"))
        } else {
            state.settings.set_onefichier_key(value).await.map(
                |_| "1fichier API key saved. Retry any 1fichier downloads that need attention.",
            )
        };
    settings_result(&state, theme, result).await
}

#[derive(Deserialize)]
struct SignInForm {
    email: String,
    password: String,
    #[serde(default)]
    code: Option<String>,
}

async fn sign_in_onefichier(
    State(state): State<AppState>,
    theme: Theme,
    Form(form): Form<SignInForm>,
) -> Response {
    let result = state
        .settings
        .sign_in_onefichier(&form.email, &form.password, form.code.as_deref())
        .await
        .map(|two_factor_required| {
            if two_factor_required {
                "1fichier sent a two-factor request: enter the 6-digit code and sign in again \
                 within 10 minutes."
            } else {
                "Signed in to 1fichier. Retry any 1fichier downloads that need attention."
            }
        });
    settings_result(&state, theme, result).await
}

async fn sign_out_onefichier(State(state): State<AppState>, theme: Theme) -> Response {
    let result = state
        .settings
        .sign_out_onefichier()
        .await
        .map(|_| "Signed out of 1fichier; the stored email and password were deleted.");
    settings_result(&state, theme, result).await
}

async fn refresh_onefichier_cdn(State(state): State<AppState>, theme: Theme) -> Response {
    let result = state
        .settings
        .refresh_onefichier_cdn()
        .await
        .map(|_| "1fichier CDN credits updated.");
    settings_result(&state, theme, result).await
}

async fn remove_onefichier(State(state): State<AppState>, theme: Theme) -> Response {
    let result = state
        .settings
        .clear_onefichier_key()
        .await
        .map(|_| "1fichier API key removed.");
    settings_result(&state, theme, result).await
}

#[derive(Deserialize)]
struct TmdbForm {
    api_key: String,
}

async fn save_tmdb(
    State(state): State<AppState>,
    theme: Theme,
    Form(form): Form<TmdbForm>,
) -> Response {
    let value = form.api_key.trim();
    // The form is pre-filled with the mask; submitting it unchanged is a no-op.
    let result = if value == SECRET_MASK && state.settings.tmdb().configured {
        Ok("TMDB API key unchanged.")
    } else if value.is_empty() {
        Err(AppError::invalid("enter a TMDB API key"))
    } else {
        state
            .settings
            .set_tmdb_key(value)
            .await
            .map(|_| "TMDB API key saved. Artwork appears as downloads are shown.")
    };
    section_result(&state, theme, SECTION_TMDB, result).await
}

async fn remove_tmdb(State(state): State<AppState>, theme: Theme) -> Response {
    let result = state
        .settings
        .clear_tmdb_key()
        .await
        .map(|_| "TMDB API key removed. Artwork already found stays cached.");
    section_result(&state, theme, SECTION_TMDB, result).await
}

/// Render the settings page after a 1fichier action, with that section open.
async fn settings_result(
    state: &AppState,
    theme: Theme,
    result: Result<&str, AppError>,
) -> Response {
    section_result(state, theme, SECTION_ONEFICHIER, result).await
}

/// Render the settings page after an action, with its section open.
async fn section_result(
    state: &AppState,
    theme: Theme,
    open: &'static str,
    result: Result<&str, AppError>,
) -> Response {
    match result {
        Ok(message) => render(
            &settings_with(
                state,
                theme,
                SettingsPage {
                    message: Some(message.to_owned()),
                    open,
                    ..Default::default()
                },
            )
            .await,
        ),
        Err(e) => {
            let status = status_for(&e);
            let page = SettingsPage {
                error: Some(e.message),
                open,
                ..Default::default()
            };
            render_status(status, &settings_with(state, theme, page).await)
        }
    }
}

/// Miscellaneous settings prepared for the settings page.
#[derive(Default)]
struct MiscView {
    units: Vec<UnitOption>,
    /// The speed limit in `limit_units`' selected unit; empty for none.
    limit_value: String,
    limit_units: Vec<UnitOption>,
    /// The limit as the pill shows it.
    limit: Option<String>,
    max_concurrent: usize,
    default_max_concurrent: usize,
    confirm_downloads: bool,
}

struct UnitOption {
    value: &'static str,
    label: &'static str,
    description: &'static str,
    selected: bool,
}

impl From<&MiscSettings> for MiscView {
    fn from(m: &MiscSettings) -> Self {
        let unit = m.speed_unit;
        let (limit_value, limit_unit) = match m.speed_limit {
            Some(limit) => {
                let (value, u) = unit.limit_parts(limit);
                (value, Some(u))
            }
            None => (String::new(), None),
        };
        // Without a limit, offer the large unit (MB/s rather than kB/s).
        let limit_unit = limit_unit.unwrap_or(unit.limit_units()[1]);
        Self {
            units: SpeedUnit::ALL
                .into_iter()
                .map(|u| UnitOption {
                    value: u.as_str(),
                    label: u.label(),
                    description: u.description(),
                    selected: u == unit,
                })
                .collect(),
            limit_value,
            limit_units: unit
                .limit_units()
                .into_iter()
                .map(|u| UnitOption {
                    value: u.id,
                    label: u.id,
                    description: "",
                    selected: u == limit_unit,
                })
                .collect(),
            limit: m.speed_limit.map(|l| unit.format(l)),
            max_concurrent: m.max_concurrent,
            default_max_concurrent: m.default_max_concurrent,
            confirm_downloads: m.confirm_downloads,
        }
    }
}

#[derive(Deserialize)]
struct MiscForm {
    speed_unit: String,
    #[serde(default)]
    speed_limit: String,
    #[serde(default)]
    speed_limit_unit: String,
    max_concurrent: String,
    /// A checkbox: present only when ticked.
    #[serde(default)]
    confirm_downloads: Option<String>,
}

impl MiscForm {
    fn parse(&self) -> Result<MiscUpdate, AppError> {
        let speed_unit =
            SpeedUnit::parse(&self.speed_unit).ok_or_else(|| AppError::invalid("unknown unit"))?;
        let speed_limit = parse_speed_limit(&self.speed_limit, &self.speed_limit_unit)
            .map_err(AppError::invalid)?;
        let max_concurrent = self.max_concurrent.trim().parse().map_err(|_| {
            AppError::invalid("enter the number of simultaneous downloads as a whole number")
        })?;
        Ok(MiscUpdate {
            speed_unit: Some(speed_unit),
            speed_limit: Some(speed_limit),
            max_concurrent: Some(max_concurrent),
            confirm_downloads: Some(self.confirm_downloads.is_some()),
        })
    }
}

async fn save_misc(
    State(state): State<AppState>,
    theme: Theme,
    Form(form): Form<MiscForm>,
) -> Response {
    let result = match form.parse() {
        Ok(update) => state
            .settings
            .update_misc(update)
            .await
            .map(|_| "Settings saved. They apply to running downloads right away."),
        Err(e) => Err(e),
    };
    section_result(&state, theme, SECTION_MISC, result).await
}

/// Library folders prepared for the settings page (read-only).
#[derive(Default)]
struct LibraryView {
    folders: Vec<FolderView>,
    /// The pill: which libraries are set.
    summary: &'static str,
    /// Whether any set folder had a problem at startup.
    any_problem: bool,
}

struct FolderView {
    label: &'static str,
    hint: &'static str,
    variable: String,
    path: Option<String>,
    problem: Option<String>,
}

impl FolderView {
    fn new(label: &'static str, hint: &'static str, f: FolderSetting) -> Self {
        Self {
            label,
            hint,
            variable: f.variable,
            path: f.path,
            problem: f.problem,
        }
    }
}

impl From<LibrarySettings> for LibraryView {
    fn from(l: LibrarySettings) -> Self {
        let summary = match (l.movies.path.is_some(), l.tv.path.is_some()) {
            (true, true) => "Movies and TV",
            (true, false) => "Movies only",
            (false, true) => "TV only",
            (false, false) => "Off",
        };
        let any_problem = [&l.movies, &l.tv, &l.unsorted]
            .iter()
            .any(|f| f.problem.is_some());
        Self {
            folders: vec![
                FolderView::new("Movies", "Title (year)/Title (year).mkv", l.movies),
                FolderView::new(
                    "TV series",
                    "Show (year)/Season 01/Show (year) - S01E02.mkv",
                    l.tv,
                ),
                FolderView::new(
                    "Fallback",
                    "videos that can't be identified with confidence",
                    l.unsorted,
                ),
            ],
            summary,
            any_problem,
        }
    }
}

#[derive(Deserialize)]
struct PasswordForm {
    current_password: String,
    new_password: String,
    new_password_confirm: String,
}

async fn change_password(
    State(state): State<AppState>,
    theme: Theme,
    extensions: Extensions,
    Form(form): Form<PasswordForm>,
) -> Response {
    if form.new_password != form.new_password_confirm {
        let page = SettingsPage {
            error: Some("new passwords do not match".into()),
            open: SECTION_PASSWORD,
            ..Default::default()
        };
        return render_status(
            StatusCode::BAD_REQUEST,
            &settings_with(&state, theme, page).await,
        );
    }
    match state
        .auth
        .change_password(
            client_ip(&extensions),
            &form.current_password,
            &form.new_password,
        )
        .await
    {
        Ok(session) => {
            let page = SettingsPage {
                message: Some("Password changed. Other sessions have been logged out.".into()),
                open: SECTION_PASSWORD,
                ..Default::default()
            };
            let mut response = render(&settings_with(&state, theme, page).await);
            response.headers_mut().insert(
                header::SET_COOKIE,
                session_cookie(&session, state.secure_cookies),
            );
            response
        }
        Err(e) => {
            let status = status_for(&e);
            let page = SettingsPage {
                error: Some(e.message),
                open: SECTION_PASSWORD,
                ..Default::default()
            };
            render_status(status, &settings_with(&state, theme, page).await)
        }
    }
}

/// Two-factor authentication prepared for the settings page.
#[derive(Default)]
struct TwoFactorView {
    enabled: bool,
    recovery_codes_left: u32,
    enrollment: Option<EnrollmentView>,
}

struct EnrollmentView {
    /// The secret in groups of four, for typing into the app by hand.
    secret: String,
    uri: String,
    qr_size: usize,
    qr_path: String,
}

impl From<TwoFactorStatus> for TwoFactorView {
    fn from(s: TwoFactorStatus) -> Self {
        Self {
            enabled: s.enabled,
            recovery_codes_left: s.recovery_codes_left,
            enrollment: s.enrollment.map(|e| {
                let qr = crate::totp::qr_svg(&e.uri);
                EnrollmentView {
                    secret: e
                        .secret
                        .as_bytes()
                        .chunks(4)
                        .map(|c| std::str::from_utf8(c).expect("base32 is ASCII"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    qr_size: qr.as_ref().map_or(0, |q| q.size),
                    qr_path: qr.map(|q| q.path).unwrap_or_default(),
                    uri: e.uri,
                }
            }),
        }
    }
}

#[derive(Deserialize)]
struct ConfirmPasswordForm {
    password: String,
}

#[derive(Deserialize)]
struct DisableTwoFactorForm {
    password: String,
    code: String,
}

async fn start_two_factor(
    State(state): State<AppState>,
    theme: Theme,
    extensions: Extensions,
    Form(form): Form<ConfirmPasswordForm>,
) -> Response {
    let result = state
        .auth
        .start_two_factor_setup(client_ip(&extensions), &form.password)
        .await
        .map(|_| "Scan the QR code with your authenticator app, then enter the code it shows.");
    section_result(&state, theme, SECTION_TWO_FACTOR, result).await
}

async fn enable_two_factor(
    State(state): State<AppState>,
    theme: Theme,
    extensions: Extensions,
    headers: HeaderMap,
    Form(form): Form<CodeForm>,
) -> Response {
    let current = read_cookie(&headers, SESSION_COOKIE).unwrap_or_default();
    match state
        .auth
        .finish_two_factor_setup(client_ip(&extensions), &form.code, current)
        .await
    {
        Ok(codes) => {
            recovery_codes_page(
                &state,
                theme,
                "Two-factor authentication is on. New sign-ins need a code from \
             your authenticator app, and other sessions have been logged out.",
                codes,
            )
            .await
        }
        Err(e) => section_result(&state, theme, SECTION_TWO_FACTOR, Err(e)).await,
    }
}

async fn cancel_two_factor(State(state): State<AppState>, theme: Theme) -> Response {
    state.auth.cancel_two_factor_setup();
    let page = SettingsPage {
        open: SECTION_TWO_FACTOR,
        ..Default::default()
    };
    render(&settings_with(&state, theme, page).await)
}

async fn disable_two_factor(
    State(state): State<AppState>,
    theme: Theme,
    extensions: Extensions,
    Form(form): Form<DisableTwoFactorForm>,
) -> Response {
    let result = state
        .auth
        .disable_two_factor(client_ip(&extensions), &form.password, &form.code)
        .await
        .map(|()| "Two-factor authentication is off. Remove Direkt from your authenticator app.");
    section_result(&state, theme, SECTION_TWO_FACTOR, result).await
}

async fn new_recovery_codes(
    State(state): State<AppState>,
    theme: Theme,
    extensions: Extensions,
    Form(form): Form<ConfirmPasswordForm>,
) -> Response {
    match state
        .auth
        .new_recovery_codes(client_ip(&extensions), &form.password)
        .await
    {
        Ok(codes) => {
            recovery_codes_page(
                &state,
                theme,
                "New recovery codes made. The old ones no longer work.",
                codes,
            )
            .await
        }
        Err(e) => section_result(&state, theme, SECTION_TWO_FACTOR, Err(e)).await,
    }
}

async fn recovery_codes_page(
    state: &AppState,
    theme: Theme,
    message: &str,
    codes: Vec<String>,
) -> Response {
    let page = SettingsPage {
        message: Some(message.to_owned()),
        recovery_codes: codes,
        open: SECTION_TWO_FACTOR,
        ..Default::default()
    };
    render(&settings_with(state, theme, page).await)
}

#[derive(Deserialize)]
struct ThemeForm {
    theme: String,
}

/// Switch this browser's theme. The page answering the switch already uses it.
async fn change_theme(State(state): State<AppState>, Form(form): Form<ThemeForm>) -> Response {
    let Some(theme) = Theme::parse(&form.theme) else {
        let page = SettingsPage {
            error: Some("unknown theme".into()),
            open: SECTION_APPEARANCE,
            ..Default::default()
        };
        return render_status(
            StatusCode::BAD_REQUEST,
            &settings_with(&state, Theme::default(), page).await,
        );
    };
    let page = SettingsPage {
        open: SECTION_APPEARANCE,
        ..Default::default()
    };
    let mut response = render(&settings_with(&state, theme, page).await);
    response
        .headers_mut()
        .insert(header::SET_COOKIE, theme.cookie(state.secure_cookies));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn themes_round_trip_through_their_ids() {
        for theme in Theme::ALL {
            assert_eq!(Theme::parse(theme.id()), Some(theme));
        }
        assert_eq!(Theme::parse("Steve"), None);
        assert_eq!(Theme::parse(""), None);
        assert_eq!(Theme::default(), Theme::Steve);
    }

    #[test]
    fn formatting() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(human_duration(5), "5s");
        assert_eq!(human_duration(125), "2m 05s");
        assert_eq!(human_duration(7260), "2h 01m");
    }

    #[test]
    fn relative_times() {
        let now = Utc::now();
        assert_eq!(human_ago(now, now), "just now");
        assert_eq!(
            human_ago(now - chrono::Duration::minutes(5), now),
            "5 min ago"
        );
        assert_eq!(human_ago(now - chrono::Duration::hours(3), now), "3 h ago");
        assert_eq!(
            human_ago(now + chrono::Duration::minutes(5), now),
            "just now"
        );
    }

    #[test]
    fn tile_badges() {
        assert_eq!(tile_badge("Movie.2024.1080p.mkv"), "MKV");
        assert_eq!(tile_badge("archive.tar.gz"), "GZ");
        assert_eq!(tile_badge("noext"), "FILE");
        assert_eq!(tile_badge(".hidden"), "FILE");
        assert_eq!(tile_badge("weird.ext!"), "FILE");
        assert_eq!(tile_badge("long.extension"), "FILE");
        assert_eq!(tile_badge(""), "FILE");
    }

    #[test]
    fn url_file_names() {
        let name = |u| url_file_name(u);
        assert_eq!(
            name("https://h/a/Movie%20Name.mkv?…").as_deref(),
            Some("Movie Name.mkv")
        );
        assert_eq!(name("https://h/dir/").as_deref(), Some("dir"));
        assert_eq!(name("https://h/"), None);
        assert_eq!(name("https://1fichier.com/?…"), None);
        assert_eq!(name("not a url"), None);
    }

    #[test]
    fn graph_is_right_aligned_and_scaled() {
        let (area, line, scale) = graph_paths(&[], SpeedUnit::Binary);
        assert!(area.is_empty());
        assert_eq!(line, "M0,100H299");
        assert_eq!(scale, MIN_GRAPH_SCALE);

        let mib = 1024 * 1024;
        let (area, line, scale) = graph_paths(&[0, 3 * mib], SpeedUnit::Binary);
        assert_eq!(scale, 4 * mib, "rounded up to a power of two");
        assert!(line.starts_with("M298,100.0L299,"), "{line}");
        assert!(area.ends_with("L299,100L298,100Z"), "{area}");

        // Longer input than the window keeps only the newest samples.
        let many = vec![mib; HISTORY_SECS + 10];
        let (_, line, _) = graph_paths(&many, SpeedUnit::Binary);
        assert!(line.starts_with("M0,"));
        assert_eq!(line.matches('L').count(), HISTORY_SECS - 1);
    }
}
