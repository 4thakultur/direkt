//! Server-rendered web UI (askama templates + htmx).
//!
//! Every action goes through the same services as the JSON API. Live parts of
//! the page (download list, download details) poll small HTML fragments, and
//! actions respond with a flash message plus an `HX-Trigger` that refreshes
//! them.

use super::{AppState, clear_session_cookie, client_ip, read_cookie, session_cookie};
use crate::domain::{Download, DownloadEvent, PostState, TransferState};
use crate::engine::{HISTORY_SECS, LiveSnapshot, ThroughputSnapshot};
use crate::error::{AppError, ErrorKind};
use crate::settings::{OneFichierSettings, SECRET_MASK};
use crate::store::ApiToken;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, State};
use axum::http::{Extensions, HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::fmt::Write as _;
use uuid::Uuid;

const HTMX_JS: &str = include_str!("../../static/htmx-2.0.4.min.js");
const APP_CSS: &str = include_str!("../../static/app.css");
/// Most downloads shown in the queue sections (everything not finished).
const QUEUE_LIMIT: i64 = 500;
/// Most finished downloads shown per history section.
const HISTORY_LIMIT: i64 = 50;

pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/login", get(login_page).post(login_submit))
        .route("/setup", get(setup_page).post(setup_submit))
        .route("/static/htmx-2.0.4.min.js", get(htmx_js))
        .route("/static/app.css", get(app_css))
}

pub fn protected_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(downloads_page))
        .route("/downloads/{id}", get(download_page))
        .route("/settings", get(settings_page))
        .route("/settings/tokens", post(create_token))
        .route("/settings/tokens/{id}/revoke", post(revoke_token))
        .route("/settings/password", post(change_password))
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
        .route("/ui/downloads", post(submit_download))
        .route("/ui/downloads/{id}/detail", get(detail_fragment))
        .route("/ui/downloads/{id}/{action}", post(download_action))
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

fn human_speed(bytes_per_sec: u64) -> String {
    format!("{}/s", human_bytes(bytes_per_sec))
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

fn post_label(state: PostState) -> Option<&'static str> {
    match state {
        PostState::None => None,
        PostState::Pending => Some("Organizing pending"),
        PostState::Processing => Some("Organizing"),
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
    /// Downloading without a known size: show an indeterminate bar.
    indeterminate: bool,
    /// Short label on the cover tile, e.g. the file extension.
    badge: String,
    /// Cover tile color, `0..TILE_HUES`.
    hue: u8,
    /// When it finished, for the completed list.
    finished: Option<String>,
}

/// Number of `.hue-N` cover tile colors in the stylesheet.
const TILE_HUES: u128 = 8;

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
    fn new(d: Download, live: Option<LiveSnapshot>) -> Self {
        use TransferState::*;
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
        let speed = rate.map(human_speed);
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
            id: d.id.to_string(),
            badge: tile_badge(&name),
            hue: (d.id.as_u128() % TILE_HUES) as u8,
            indeterminate: d.state == Downloading && percent.is_none(),
            finished: d.completed_at.map(|t| human_ago(t, Utc::now())),
            name,
            display_url: d.display_url,
            provider: d.provider,
            state: d.state.as_str(),
            state_label: state_label(d.state),
            post_label: post_label(d.post_state),
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

async fn htmx_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        HTMX_JS,
    )
}

async fn app_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        APP_CSS,
    )
}

// ---------------------------------------------------------------------------
// Login and setup
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    error: Option<String>,
    username: String,
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login_page(State(state): State<AppState>) -> Response {
    match state.auth.needs_setup().await {
        Ok(true) => Redirect::to("/setup").into_response(),
        _ => render(&LoginPage {
            error: None,
            username: String::new(),
        }),
    }
}

async fn login_submit(
    State(state): State<AppState>,
    extensions: Extensions,
    Form(form): Form<LoginForm>,
) -> Response {
    match state
        .auth
        .login(client_ip(&extensions), &form.username, &form.password)
        .await
    {
        Ok(session) => (
            [(
                header::SET_COOKIE,
                session_cookie(&session, state.secure_cookies),
            )],
            Redirect::to("/"),
        )
            .into_response(),
        Err(e) => render_status(
            status_for(&e),
            &LoginPage {
                error: Some(e.message),
                username: form.username,
            },
        ),
    }
}

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupPage {
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

async fn setup_page(State(state): State<AppState>) -> Response {
    match state.auth.needs_setup().await {
        Ok(false) => Redirect::to("/login").into_response(),
        _ => render(&SetupPage {
            error: None,
            token_path: setup_token_path(&state),
            username: String::new(),
        }),
    }
}

async fn setup_submit(
    State(state): State<AppState>,
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
                error: Some(e.message),
                token_path: setup_token_path(&state),
                username: form.username,
            },
        ),
    }
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = read_cookie(&headers, super::SESSION_COOKIE) {
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
    /// SVG path data for the filled area and its outline.
    area: String,
    line: String,
}

/// Graph height in SVG units; the width is one unit per second of history.
const GRAPH_HEIGHT: f64 = 100.0;
/// Lowest graph scale, so an idle graph does not amplify noise.
const MIN_GRAPH_SCALE: u64 = 64 * 1024;

/// SVG paths (area, line) for samples right-aligned in a fixed window, and the
/// scale they are drawn against: the next power of two above the highest
/// sample, so labels stay round.
fn graph_paths(samples: &[u64]) -> (String, String, u64) {
    let width = (HISTORY_SECS - 1) as f64;
    let max = samples.iter().copied().max().unwrap_or(0);
    let scale = max.max(MIN_GRAPH_SCALE).next_power_of_two();
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

impl From<ThroughputSnapshot> for StatsView {
    fn from(t: ThroughputSnapshot) -> Self {
        let (area, line, scale) = graph_paths(&t.samples);
        Self {
            current: human_speed(t.current),
            peak: human_speed(t.peak),
            total: human_bytes(t.total),
            free: t.free_bytes.map(human_bytes).unwrap_or_else(|| "—".into()),
            scale: human_speed(scale),
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
}

#[derive(Template)]
#[template(path = "downloads.html")]
struct DownloadsPage {
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
    message: Option<String>,
    error: Option<String>,
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

    let view = |d: Download| {
        let live = service.live(d.id);
        DownloadView::new(d, live)
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
        title,
        rows,
        truncated,
    })
    .collect();
    Dashboard {
        stats: service.throughput().into(),
        downloading,
        queued,
        groups,
        error,
    }
}

async fn downloads_page(State(state): State<AppState>) -> Response {
    render(&DownloadsPage {
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

async fn submit_download(State(state): State<AppState>, Form(body): Form<SubmitBody>) -> Response {
    match state
        .downloads
        .submit(&body.url, body.password.as_deref())
        .await
    {
        Ok(d) => with_refresh(render(&SubmitForm {
            value: String::new(),
            message: Some(format!("Queued {}", d.display_url)),
            error: None,
        })),
        Err(e) => render_status(
            status_for(&e),
            &SubmitForm {
                value: body.url,
                message: None,
                error: Some(e.message),
            },
        ),
    }
}

#[derive(Template)]
#[template(path = "_flash.html")]
struct Flash {
    ok: bool,
    message: String,
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
        _ => Err(AppError::not_found("unknown action")),
    };
    match result {
        Ok(message) => with_refresh(render(&Flash {
            ok: true,
            message: message.to_owned(),
        })),
        Err(e) => with_refresh(render_status(
            status_for(&e),
            &Flash {
                ok: false,
                message: e.message,
            },
        )),
    }
}

#[derive(Template)]
#[template(path = "download.html")]
struct DownloadPage {
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
    Ok((
        DownloadView::new(download, live),
        events.into_iter().rev().map(EventView::from).collect(),
    ))
}

async fn download_page(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    match load_detail(&state, id).await {
        Ok((d, events)) => render(&DownloadPage { d, events }),
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
    onefichier: OneFichierSettings,
    cdn: Option<CdnDisplay>,
    /// The account has CDN credits it uses for its own downloads.
    has_credits: bool,
    tokens: Vec<TokenView>,
    new_token: Option<String>,
    message: Option<String>,
    error: Option<String>,
}

async fn settings_with(state: &AppState, mut page: SettingsPage) -> SettingsPage {
    page.onefichier = state.settings.onefichier();
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
    page
}

async fn settings_page(State(state): State<AppState>) -> Response {
    render(&settings_with(&state, SettingsPage::default()).await)
}

#[derive(Deserialize)]
struct TokenForm {
    name: String,
}

async fn create_token(State(state): State<AppState>, Form(form): Form<TokenForm>) -> Response {
    let page = match state.auth.create_api_token(&form.name).await {
        Ok((token, secret)) => SettingsPage {
            new_token: Some(secret),
            message: Some(format!(
                "Created token \u{201c}{}\u{201d}. Copy it now; it will not be shown again.",
                token.name
            )),
            ..Default::default()
        },
        Err(e) => SettingsPage {
            error: Some(e.message),
            ..Default::default()
        },
    };
    render(&settings_with(&state, page).await)
}

async fn revoke_token(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    let page = match state.auth.revoke_api_token(id).await {
        Ok(()) => SettingsPage {
            message: Some("Token revoked.".into()),
            ..Default::default()
        },
        Err(e) => SettingsPage {
            error: Some(e.message),
            ..Default::default()
        },
    };
    render(&settings_with(&state, page).await)
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
    settings_result(&state, result).await
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
    settings_result(&state, result).await
}

async fn sign_out_onefichier(State(state): State<AppState>) -> Response {
    let result = state
        .settings
        .sign_out_onefichier()
        .await
        .map(|_| "Signed out of 1fichier; the stored email and password were deleted.");
    settings_result(&state, result).await
}

async fn refresh_onefichier_cdn(State(state): State<AppState>) -> Response {
    let result = state
        .settings
        .refresh_onefichier_cdn()
        .await
        .map(|_| "1fichier CDN credits updated.");
    settings_result(&state, result).await
}

async fn remove_onefichier(State(state): State<AppState>) -> Response {
    let result = state
        .settings
        .clear_onefichier_key()
        .await
        .map(|_| "1fichier API key removed.");
    settings_result(&state, result).await
}

async fn settings_result(state: &AppState, result: Result<&str, AppError>) -> Response {
    match result {
        Ok(message) => render(
            &settings_with(
                state,
                SettingsPage {
                    message: Some(message.to_owned()),
                    ..Default::default()
                },
            )
            .await,
        ),
        Err(e) => {
            let status = status_for(&e);
            let page = SettingsPage {
                error: Some(e.message),
                ..Default::default()
            };
            render_status(status, &settings_with(state, page).await)
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
    extensions: Extensions,
    Form(form): Form<PasswordForm>,
) -> Response {
    if form.new_password != form.new_password_confirm {
        let page = SettingsPage {
            error: Some("new passwords do not match".into()),
            ..Default::default()
        };
        return render_status(StatusCode::BAD_REQUEST, &settings_with(&state, page).await);
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
                ..Default::default()
            };
            let mut response = render(&settings_with(&state, page).await);
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
                ..Default::default()
            };
            render_status(status, &settings_with(&state, page).await)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (area, line, scale) = graph_paths(&[]);
        assert!(area.is_empty());
        assert_eq!(line, "M0,100H299");
        assert_eq!(scale, MIN_GRAPH_SCALE);

        let mib = 1024 * 1024;
        let (area, line, scale) = graph_paths(&[0, 3 * mib]);
        assert_eq!(scale, 4 * mib, "rounded up to a power of two");
        assert!(line.starts_with("M298,100.0L299,"), "{line}");
        assert!(area.ends_with("L299,100L298,100Z"), "{area}");

        // Longer input than the window keeps only the newest samples.
        let many = vec![mib; HISTORY_SECS + 10];
        let (_, line, _) = graph_paths(&many);
        assert!(line.starts_with("M0,"));
        assert_eq!(line.matches('L').count(), HISTORY_SECS - 1);
    }
}
