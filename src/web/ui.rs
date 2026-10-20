//! Server-rendered web UI (askama templates + htmx).
//!
//! Every action goes through the same services as the JSON API. Live parts of
//! the page (download list, download details) poll small HTML fragments, and
//! actions respond with a flash message plus an `HX-Trigger` that refreshes
//! them.

use super::{AppState, clear_session_cookie, client_ip, read_cookie, session_cookie};
use crate::domain::{Download, DownloadEvent, PostState, TransferState};
use crate::engine::LiveSnapshot;
use crate::error::{AppError, ErrorKind};
use crate::store::ApiToken;
use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::http::{Extensions, HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

const HTMX_JS: &str = include_str!("../../static/htmx-2.0.4.min.js");
const APP_CSS: &str = include_str!("../../static/app.css");
const LIST_LIMIT: i64 = 200;

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
        .route("/logout", post(logout))
        .route("/ui/rows", get(rows_fragment))
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
            Some(t) => format!("{} / {}", human_bytes(downloaded), human_bytes(t)),
            None if downloaded > 0 => human_bytes(downloaded),
            None => String::new(),
        };
        let rate = live.map(|l| l.bytes_per_sec).filter(|&r| r > 0);
        let speed = rate.map(|r| format!("{}/s", human_bytes(r)));
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
        Self {
            id: d.id.to_string(),
            name: d.filename.clone().unwrap_or_else(|| d.display_url.clone()),
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

/// List filters shown as tabs: (key, label, states).
const FILTERS: &[(&str, &str, &[TransferState])] = &[
    ("all", "All", &[]),
    (
        "active",
        "Active",
        &[
            TransferState::Queued,
            TransferState::Downloading,
            TransferState::Retrying,
        ],
    ),
    ("paused", "Paused", &[TransferState::Paused]),
    (
        "attention",
        "Needs attention",
        &[TransferState::Blocked, TransferState::Failed],
    ),
    ("completed", "Completed", &[TransferState::Completed]),
    ("cancelled", "Cancelled", &[TransferState::Cancelled]),
];

#[derive(Deserialize)]
struct FilterQuery {
    f: Option<String>,
}

fn filter_states(key: &str) -> (&'static str, &'static [TransferState]) {
    FILTERS
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(k, _, states)| (*k, *states))
        .unwrap_or(("all", &[]))
}

struct Tab {
    key: &'static str,
    label: &'static str,
    active: bool,
}

#[derive(Template)]
#[template(path = "downloads.html")]
struct DownloadsPage {
    tabs: Vec<Tab>,
    filter: &'static str,
    rows: Vec<DownloadView>,
    error: Option<String>,
    submit: SubmitForm,
}

#[derive(Template)]
#[template(path = "_rows.html")]
struct RowsFragment {
    rows: Vec<DownloadView>,
    error: Option<String>,
}

#[derive(Template, Default)]
#[template(path = "_submit.html")]
struct SubmitForm {
    value: String,
    message: Option<String>,
    error: Option<String>,
}

async fn load_rows(
    state: &AppState,
    states: &[TransferState],
) -> (Vec<DownloadView>, Option<String>) {
    match state.downloads.list(states, LIST_LIMIT, 0).await {
        Ok(list) => (
            list.into_iter()
                .map(|d| {
                    let live = state.downloads.live(d.id);
                    DownloadView::new(d, live)
                })
                .collect(),
            None,
        ),
        Err(e) => (Vec::new(), Some(e.message)),
    }
}

async fn downloads_page(State(state): State<AppState>, Query(q): Query<FilterQuery>) -> Response {
    let (filter, states) = filter_states(q.f.as_deref().unwrap_or("all"));
    let (rows, error) = load_rows(&state, states).await;
    render(&DownloadsPage {
        tabs: FILTERS
            .iter()
            .map(|(key, label, _)| Tab {
                key,
                label,
                active: *key == filter,
            })
            .collect(),
        filter,
        rows,
        error,
        submit: SubmitForm::default(),
    })
}

async fn rows_fragment(State(state): State<AppState>, Query(q): Query<FilterQuery>) -> Response {
    let (_, states) = filter_states(q.f.as_deref().unwrap_or("all"));
    let (rows, error) = load_rows(&state, states).await;
    render(&RowsFragment { rows, error })
}

#[derive(Deserialize)]
struct SubmitBody {
    url: String,
}

fn with_refresh(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("hx-trigger", HeaderValue::from_static("downloads-changed"));
    response
}

async fn submit_download(State(state): State<AppState>, Form(body): Form<SubmitBody>) -> Response {
    match state.downloads.submit(&body.url).await {
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
        "retry" => service.retry(id).await.map(|_| "Queued for retry"),
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
    tokens: Vec<TokenView>,
    new_token: Option<String>,
    message: Option<String>,
    error: Option<String>,
}

async fn settings_with(state: &AppState, mut page: SettingsPage) -> SettingsPage {
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
    use crate::web::api::parse_states;

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
    fn unknown_filter_falls_back_to_all() {
        assert_eq!(filter_states("bogus").0, "all");
        assert_eq!(filter_states("active").1.len(), 3);
    }

    #[test]
    fn filters_are_valid_states() {
        for (_, _, states) in FILTERS {
            let joined: Vec<_> = states.iter().map(|s| s.as_str()).collect();
            assert_eq!(parse_states(Some(&joined.join(","))).unwrap(), *states);
        }
    }
}
