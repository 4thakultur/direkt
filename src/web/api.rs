//! JSON API (`/api/v1`). The OpenAPI document is served at
//! `/api/openapi.json`.

use super::error::{ApiJson, ApiPath, ApiQuery};
use super::{ApiError, AppState, ErrorBody};
use crate::domain::{Download, DownloadError, DownloadEvent, MediaType, PostState, TransferState};
use crate::engine::LiveSnapshot;
use crate::error::{AppError, ErrorKind};
use crate::service::Preview;
use crate::settings::{OneFichierSettings, TmdbSettings};
use crate::store::ApiToken;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{IntoParams, Modify, OpenApi, ToSchema};
use uuid::Uuid;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/downloads", get(list_downloads).post(create_download))
        .route("/downloads/cancelled", delete(delete_cancelled_downloads))
        .route("/downloads/completed", delete(delete_completed_downloads))
        .route("/downloads/{id}", get(get_download).delete(delete_download))
        .route("/downloads/{id}/events", get(download_events))
        .route("/downloads/{id}/pause", post(pause_download))
        .route("/downloads/{id}/resume", post(resume_download))
        .route("/downloads/{id}/cancel", post(cancel_download))
        .route("/downloads/{id}/retry", post(retry_download))
        .route("/previews", post(create_preview))
        .route("/previews/{id}", put(retype_preview))
        .route("/stats", get(get_stats))
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/{id}", delete(revoke_token))
        .route(
            "/settings/onefichier",
            get(get_onefichier)
                .put(put_onefichier)
                .delete(delete_onefichier),
        )
        .route(
            "/settings/onefichier/account",
            post(sign_in_onefichier).delete(sign_out_onefichier),
        )
        .route(
            "/settings/onefichier/cdn/refresh",
            post(refresh_onefichier_cdn),
        )
        .route(
            "/settings/tmdb",
            get(get_tmdb).put(put_tmdb).delete(delete_tmdb),
        )
}

type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------

/// A download and its current progress.
#[derive(Debug, Serialize, ToSchema)]
pub struct DownloadDto {
    pub id: Uuid,
    /// Submitted URL with the query string redacted.
    pub display_url: String,
    pub provider: String,
    pub state: TransferState,
    pub post_state: PostState,
    pub filename: Option<String>,
    pub total_bytes: Option<i64>,
    pub downloaded_bytes: i64,
    /// Current transfer speed; present only while downloading.
    pub bytes_per_sec: Option<u64>,
    pub retry_count: i64,
    /// When a `retrying` download will be attempted again.
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// Last transfer error.
    pub error: Option<DownloadError>,
    /// Last post-processing error. Never affects the transfer state.
    pub post_error: Option<DownloadError>,
    /// Location of the completed file.
    pub file_path: Option<String>,
    /// Chosen when the download was confirmed.
    pub media_type: Option<MediaType>,
    /// Title found by the metadata lookup.
    pub title: Option<String>,
    /// Cached artwork, served at `/artwork/{id}` (session login).
    pub artwork_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl DownloadDto {
    pub fn new(d: Download, live: Option<LiveSnapshot>) -> Self {
        let downloaded_bytes = live
            .map(|l| i64::try_from(l.downloaded_bytes).unwrap_or(i64::MAX))
            .unwrap_or(d.downloaded_bytes);
        Self {
            id: d.id,
            display_url: d.display_url,
            provider: d.provider,
            state: d.state,
            post_state: d.post_state,
            filename: d.filename,
            total_bytes: d.total_bytes,
            downloaded_bytes,
            bytes_per_sec: live.map(|l| l.bytes_per_sec),
            retry_count: d.retry_count,
            next_attempt_at: d.next_attempt_at,
            error: d.error,
            post_error: d.post_error,
            file_path: d.file_path,
            media_type: d.media_type,
            title: d.title,
            artwork_id: d.artwork_id,
            created_at: d.created_at,
            updated_at: d.updated_at,
            completed_at: d.completed_at,
        }
    }
}

/// Aggregate transfer throughput since the service started.
#[derive(Debug, Serialize, ToSchema)]
pub struct StatsDto {
    /// Bytes per second during the last second.
    pub current_bytes_per_sec: u64,
    /// Highest one-second rate since start.
    pub peak_bytes_per_sec: u64,
    /// Bytes transferred since start.
    pub total_bytes: u64,
    /// Free space in the download directory; absent until first checked.
    pub free_bytes: Option<u64>,
    /// Bytes per second for each of the last seconds (up to five minutes),
    /// oldest first.
    pub history: Vec<u64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DownloadList {
    pub items: Vec<DownloadDto>,
    pub limit: i64,
    pub offset: i64,
}

/// Either `url` (queue directly) or `preview_id` (queue a preview from
/// `POST /api/v1/previews`, with its password and lookup results).
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateDownload {
    /// An http or https URL.
    #[schema(example = "https://example.com/files/movie.mkv")]
    pub url: Option<String>,
    /// Password for a password-protected file (1fichier). Write-only: never
    /// returned or logged.
    pub password: Option<String>,
    /// A preview to confirm, instead of `url` and `password`.
    pub preview_id: Option<Uuid>,
    /// The download's type. With `preview_id`, defaults to the preview's.
    pub media_type: Option<MediaType>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatePreview {
    /// An http or https URL.
    #[schema(example = "https://1fichier.com/?abcde12345")]
    pub url: String,
    /// Password for a password-protected file. Kept with the preview on the
    /// server and never returned.
    pub password: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RetypePreview {
    pub media_type: MediaType,
}

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RetryDownload {
    /// New download password to use from this retry on.
    pub password: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Comma-separated transfer states to include, e.g. `queued,downloading`.
    /// Omit for all states.
    pub state: Option<String>,
    /// Maximum number of items (1-500, default 50).
    pub limit: Option<i64>,
    /// Number of items to skip.
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateToken {
    /// A label to recognize the token by, e.g. the integration using it.
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedToken {
    #[serde(flatten)]
    pub token: ApiToken,
    /// The bearer token. Shown only once; store it securely.
    pub secret: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Submit a URL for download.
#[utoipa::path(
    post, path = "/api/v1/downloads", tag = "downloads",
    request_body = CreateDownload,
    responses(
        (status = 201, description = "Download queued", body = DownloadDto),
        (status = 400, description = "Invalid or forbidden URL", body = ErrorBody),
        (status = 401, description = "Not authenticated", body = ErrorBody),
        (status = 422, description = "No provider supports the URL", body = ErrorBody),
    )
)]
async fn create_download(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateDownload>,
) -> ApiResult<impl IntoResponse> {
    let download = match (body.preview_id, body.url) {
        (Some(id), None) => state.downloads.confirm(id, body.media_type).await?,
        (None, Some(url)) => {
            state
                .downloads
                .submit(&url, body.password.as_deref(), body.media_type)
                .await?
        }
        (Some(_), Some(_)) => {
            return Err(AppError::invalid("send either url or preview_id, not both").into());
        }
        (None, None) => return Err(AppError::invalid("url is required").into()),
    };
    Ok((StatusCode::CREATED, Json(DownloadDto::new(download, None))))
}

/// Look a link up before downloading it: provider, file name, size,
/// suggested type, and a title and artwork when found. Confirm it with
/// `POST /api/v1/downloads` and `preview_id` within an hour.
#[utoipa::path(
    post, path = "/api/v1/previews", tag = "downloads",
    request_body = CreatePreview,
    responses(
        (status = 200, body = Preview),
        (status = 400, description = "Invalid or forbidden URL", body = ErrorBody),
        (status = 422, description = "No provider supports the URL", body = ErrorBody),
        (status = 502, description = "The file is gone", body = ErrorBody),
    )
)]
async fn create_preview(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreatePreview>,
) -> ApiResult<Json<Preview>> {
    Ok(Json(
        state
            .downloads
            .preview(&body.url, body.password.as_deref())
            .await?,
    ))
}

/// Change a preview's type; the title and artwork are looked up again.
#[utoipa::path(
    put, path = "/api/v1/previews/{id}", tag = "downloads",
    request_body = RetypePreview,
    responses(
        (status = 200, body = Preview),
        (status = 404, description = "Unknown or expired preview", body = ErrorBody),
    )
)]
async fn retype_preview(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<RetypePreview>,
) -> ApiResult<Json<Preview>> {
    Ok(Json(state.downloads.retype(id, body.media_type).await?))
}

/// List downloads, newest first.
#[utoipa::path(
    get, path = "/api/v1/downloads", tag = "downloads",
    params(ListQuery),
    responses(
        (status = 200, body = DownloadList),
        (status = 400, description = "Invalid filter", body = ErrorBody),
        (status = 401, description = "Not authenticated", body = ErrorBody),
    )
)]
async fn list_downloads(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> ApiResult<Json<DownloadList>> {
    let states = parse_states(query.state.as_deref())?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(AppError::invalid(format!("limit must be between 1 and {MAX_LIMIT}")).into());
    }
    let offset = query.offset.unwrap_or(0);
    if offset < 0 {
        return Err(AppError::invalid("offset must not be negative").into());
    }
    let items = state
        .downloads
        .list(&states, limit, offset)
        .await?
        .into_iter()
        .map(|d| {
            let live = state.downloads.live(d.id);
            DownloadDto::new(d, live)
        })
        .collect();
    Ok(Json(DownloadList {
        items,
        limit,
        offset,
    }))
}

pub(crate) fn parse_states(raw: Option<&str>) -> Result<Vec<TransferState>, AppError> {
    let Some(raw) = raw.filter(|r| !r.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(|s| {
            TransferState::parse(s.trim())
                .ok_or_else(|| AppError::invalid(format!("unknown state '{}'", s.trim())))
        })
        .collect()
}

/// Get one download.
#[utoipa::path(
    get, path = "/api/v1/downloads/{id}", tag = "downloads",
    params(("id" = Uuid, Path, description = "Download ID")),
    responses(
        (status = 200, body = DownloadDto),
        (status = 404, body = ErrorBody),
    )
)]
async fn get_download(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<DownloadDto>> {
    let download = state.downloads.get(id).await?;
    Ok(Json(DownloadDto::new(download, state.downloads.live(id))))
}

/// Event history of a download, oldest first.
#[utoipa::path(
    get, path = "/api/v1/downloads/{id}/events", tag = "downloads",
    params(("id" = Uuid, Path, description = "Download ID")),
    responses(
        (status = 200, body = Vec<DownloadEvent>),
        (status = 404, body = ErrorBody),
    )
)]
async fn download_events(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Vec<DownloadEvent>>> {
    Ok(Json(state.downloads.events(id).await?))
}

/// Transfer throughput and free disk space.
#[utoipa::path(
    get, path = "/api/v1/stats", tag = "downloads",
    responses(
        (status = 200, body = StatsDto),
        (status = 401, description = "Not authenticated", body = ErrorBody),
    )
)]
async fn get_stats(State(state): State<AppState>) -> Json<StatsDto> {
    let t = state.downloads.throughput();
    Json(StatsDto {
        current_bytes_per_sec: t.current,
        peak_bytes_per_sec: t.peak,
        total_bytes: t.total,
        free_bytes: t.free_bytes,
        history: t.samples,
    })
}

/// Pause a queued, retrying, or running download. Partial data is kept.
#[utoipa::path(
    post, path = "/api/v1/downloads/{id}/pause", tag = "downloads",
    params(("id" = Uuid, Path, description = "Download ID")),
    responses(
        (status = 200, body = DownloadDto),
        (status = 404, body = ErrorBody),
        (status = 409, description = "Not pausable in its current state", body = ErrorBody),
    )
)]
async fn pause_download(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<DownloadDto>> {
    Ok(Json(DownloadDto::new(
        state.downloads.pause(id).await?,
        None,
    )))
}

/// Resume a paused download.
#[utoipa::path(
    post, path = "/api/v1/downloads/{id}/resume", tag = "downloads",
    params(("id" = Uuid, Path, description = "Download ID")),
    responses(
        (status = 200, body = DownloadDto),
        (status = 404, body = ErrorBody),
        (status = 409, description = "Not paused", body = ErrorBody),
    )
)]
async fn resume_download(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<DownloadDto>> {
    Ok(Json(DownloadDto::new(
        state.downloads.resume(id).await?,
        None,
    )))
}

/// Cancel a download that has not completed and delete its partial data.
#[utoipa::path(
    post, path = "/api/v1/downloads/{id}/cancel", tag = "downloads",
    params(("id" = Uuid, Path, description = "Download ID")),
    responses(
        (status = 200, body = DownloadDto),
        (status = 404, body = ErrorBody),
        (status = 409, description = "Already completed or failed", body = ErrorBody),
    )
)]
async fn cancel_download(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<DownloadDto>> {
    Ok(Json(DownloadDto::new(
        state.downloads.cancel(id).await?,
        None,
    )))
}

#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeleteQuery {
    /// Also delete a completed download's file from disk. Without it the file
    /// stays and only the entry is removed.
    #[serde(default)]
    pub delete_file: bool,
}

/// Delete a cancelled or completed download and its event history.
#[utoipa::path(
    delete, path = "/api/v1/downloads/{id}", tag = "downloads",
    params(("id" = Uuid, Path, description = "Download ID"), DeleteQuery),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, body = ErrorBody),
        (status = 409, description = "Not cancelled or completed, or being organized", body = ErrorBody),
        (status = 500, description = "The file could not be deleted; the download is kept", body = ErrorBody),
    )
)]
async fn delete_download(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(query): ApiQuery<DeleteQuery>,
) -> ApiResult<StatusCode> {
    state.downloads.delete(id, query.delete_file).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, ToSchema)]
struct Deleted {
    deleted: usize,
}

/// Delete every cancelled download and its event history.
#[utoipa::path(
    delete, path = "/api/v1/downloads/cancelled", tag = "downloads",
    responses((status = 200, body = Deleted))
)]
async fn delete_cancelled_downloads(State(state): State<AppState>) -> ApiResult<Json<Deleted>> {
    let deleted = state.downloads.delete_all(TransferState::Cancelled).await?;
    Ok(Json(Deleted { deleted }))
}

/// Remove every completed download from the list. Files stay on disk.
#[utoipa::path(
    delete, path = "/api/v1/downloads/completed", tag = "downloads",
    responses((status = 200, body = Deleted))
)]
async fn delete_completed_downloads(State(state): State<AppState>) -> ApiResult<Json<Deleted>> {
    let deleted = state.downloads.delete_all(TransferState::Completed).await?;
    Ok(Json(Deleted { deleted }))
}

/// Re-queue a failed, blocked, or cancelled download. The optional body sets
/// a new download password first (e.g. after "password required").
#[utoipa::path(
    post, path = "/api/v1/downloads/{id}/retry", tag = "downloads",
    params(("id" = Uuid, Path, description = "Download ID")),
    request_body(content = Option<RetryDownload>, description = "Optional"),
    responses(
        (status = 200, body = DownloadDto),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 409, description = "Not retryable in its current state", body = ErrorBody),
    )
)]
async fn retry_download(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    body: axum::body::Bytes,
) -> ApiResult<Json<DownloadDto>> {
    let body: RetryDownload = if body.iter().all(u8::is_ascii_whitespace) {
        RetryDownload::default()
    } else {
        serde_json::from_slice::<Option<RetryDownload>>(&body)
            .map_err(|e| AppError::invalid(format!("invalid JSON body: {e}")))?
            .unwrap_or_default()
    };
    Ok(Json(DownloadDto::new(
        state.downloads.retry(id, body.password.as_deref()).await?,
        None,
    )))
}

/// List API tokens (secrets are never returned).
#[utoipa::path(
    get, path = "/api/v1/tokens", tag = "tokens",
    responses((status = 200, body = Vec<ApiToken>))
)]
async fn list_tokens(State(state): State<AppState>) -> ApiResult<Json<Vec<ApiToken>>> {
    Ok(Json(state.auth.list_api_tokens().await?))
}

/// Create an API token. The secret is in the response only.
#[utoipa::path(
    post, path = "/api/v1/tokens", tag = "tokens",
    request_body = CreateToken,
    responses(
        (status = 201, body = CreatedToken),
        (status = 400, body = ErrorBody),
    )
)]
async fn create_token(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CreateToken>,
) -> ApiResult<impl IntoResponse> {
    let (token, secret) = state.auth.create_api_token(&body.name).await?;
    Ok((StatusCode::CREATED, Json(CreatedToken { token, secret })))
}

/// Revoke an API token.
#[utoipa::path(
    delete, path = "/api/v1/tokens/{id}", tag = "tokens",
    params(("id" = Uuid, Path, description = "Token ID")),
    responses(
        (status = 204, description = "Revoked"),
        (status = 404, body = ErrorBody),
    )
)]
async fn revoke_token(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<StatusCode> {
    state.auth.revoke_api_token(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 1fichier settings. The API key itself is never returned, only a mask.
#[utoipa::path(
    get, path = "/api/v1/settings/onefichier", tag = "settings",
    responses((status = 200, body = OneFichierSettings))
)]
async fn get_onefichier(State(state): State<AppState>) -> Json<OneFichierSettings> {
    Json(state.settings.onefichier())
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetOneFichierKey {
    /// Premium API key from 1fichier.com (My account → Settings).
    pub api_key: String,
}

/// Set the 1fichier API key. It applies to the next download attempt.
#[utoipa::path(
    put, path = "/api/v1/settings/onefichier", tag = "settings",
    request_body = SetOneFichierKey,
    responses(
        (status = 200, body = OneFichierSettings),
        (status = 400, description = "Not a valid API key", body = ErrorBody),
        (status = 409, description = "Set by environment variable", body = ErrorBody),
    )
)]
async fn put_onefichier(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<SetOneFichierKey>,
) -> ApiResult<Json<OneFichierSettings>> {
    Ok(Json(
        state.settings.set_onefichier_key(&body.api_key).await?,
    ))
}

/// Remove the stored 1fichier API key.
#[utoipa::path(
    delete, path = "/api/v1/settings/onefichier", tag = "settings",
    responses(
        (status = 200, body = OneFichierSettings),
        (status = 409, description = "Set by environment variable", body = ErrorBody),
    )
)]
async fn delete_onefichier(State(state): State<AppState>) -> ApiResult<Json<OneFichierSettings>> {
    Ok(Json(state.settings.clear_onefichier_key().await?))
}

/// TMDB settings (artwork lookup). The key itself is never returned.
#[utoipa::path(
    get, path = "/api/v1/settings/tmdb", tag = "settings",
    responses((status = 200, body = TmdbSettings))
)]
async fn get_tmdb(State(state): State<AppState>) -> Json<TmdbSettings> {
    Json(state.settings.tmdb())
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetTmdbKey {
    /// TMDB API key (32 hex digits) or API read access token, from
    /// themoviedb.org (Settings → API).
    pub api_key: String,
}

/// Set the TMDB key. Artwork lookups use it immediately, and names that found
/// nothing without it are looked up again.
#[utoipa::path(
    put, path = "/api/v1/settings/tmdb", tag = "settings",
    request_body = SetTmdbKey,
    responses(
        (status = 200, body = TmdbSettings),
        (status = 400, description = "Not a valid TMDB key", body = ErrorBody),
        (status = 409, description = "Set by environment variable", body = ErrorBody),
    )
)]
async fn put_tmdb(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<SetTmdbKey>,
) -> ApiResult<Json<TmdbSettings>> {
    Ok(Json(state.settings.set_tmdb_key(&body.api_key).await?))
}

/// Remove the stored TMDB key. Cached artwork stays.
#[utoipa::path(
    delete, path = "/api/v1/settings/tmdb", tag = "settings",
    responses(
        (status = 200, body = TmdbSettings),
        (status = 409, description = "Set by environment variable", body = ErrorBody),
    )
)]
async fn delete_tmdb(State(state): State<AppState>) -> ApiResult<Json<TmdbSettings>> {
    Ok(Json(state.settings.clear_tmdb_key().await?))
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OneFichierSignIn {
    /// Email address of the 1fichier account.
    pub email: String,
    /// Account password. Send `************************` to reuse the stored
    /// password of the same account.
    pub password: String,
    /// Two-factor code (6 digits), for accounts that use 2FA. Can be sent with
    /// the first request, or in a second request after
    /// `two_factor_required` was returned.
    pub code: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OneFichierSignInResult {
    /// The sign-in is waiting for a two-factor code: repeat the request with
    /// `code` within 10 minutes.
    pub two_factor_required: bool,
    pub settings: OneFichierSettings,
}

/// Sign in to 1fichier with email and password. Downloads then use the
/// account instead of the API key. The password is stored so Direkt can sign
/// in again when the session ends; it is never returned.
#[utoipa::path(
    post, path = "/api/v1/settings/onefichier/account", tag = "settings",
    request_body = OneFichierSignIn,
    responses(
        (status = 200, body = OneFichierSignInResult),
        (status = 400, description = "Missing or malformed input", body = ErrorBody),
        (status = 422, description = "1fichier rejected the credentials or code", body = ErrorBody),
        (status = 503, description = "1fichier is rate limiting sign-ins", body = ErrorBody),
    )
)]
async fn sign_in_onefichier(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<OneFichierSignIn>,
) -> ApiResult<Json<OneFichierSignInResult>> {
    let two_factor_required = state
        .settings
        .sign_in_onefichier(&body.email, &body.password, body.code.as_deref())
        .await?;
    Ok(Json(OneFichierSignInResult {
        two_factor_required,
        settings: state.settings.onefichier(),
    }))
}

/// Read the 1fichier CDN credit balance again, from the signed-in account or
/// with the API key. Allowed once every 5 minutes (1fichier's limit).
#[utoipa::path(
    post, path = "/api/v1/settings/onefichier/cdn/refresh", tag = "settings",
    responses(
        (status = 200, body = OneFichierSettings),
        (status = 409, description = "Neither signed in nor an API key configured", body = ErrorBody),
        (status = 429, description = "Checked less than 5 minutes ago", body = ErrorBody),
        (status = 503, description = "1fichier refused the request (e.g. flood protection)", body = ErrorBody),
    )
)]
async fn refresh_onefichier_cdn(
    State(state): State<AppState>,
) -> ApiResult<Json<OneFichierSettings>> {
    Ok(Json(state.settings.refresh_onefichier_cdn().await?))
}

/// Sign out of 1fichier and forget the stored email, password and session.
#[utoipa::path(
    delete, path = "/api/v1/settings/onefichier/account", tag = "settings",
    responses((status = 200, body = OneFichierSettings))
)]
async fn sign_out_onefichier(State(state): State<AppState>) -> ApiResult<Json<OneFichierSettings>> {
    Ok(Json(state.settings.sign_out_onefichier().await?))
}

// ---------------------------------------------------------------------------
// OpenAPI
// ---------------------------------------------------------------------------

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Direkt API",
        description = "Submit and manage downloads. Authenticate with an API token \
            (`Authorization: Bearer dkt_...`), created in the web UI under Settings. \
            Errors use the body `{\"error\": {\"kind\": ..., \"message\": ...}}`."
    ),
    paths(
        create_download, create_preview, retype_preview, list_downloads, get_download, download_events,
        pause_download, resume_download, cancel_download, retry_download,
        delete_download, delete_cancelled_downloads, delete_completed_downloads, get_stats,
        list_tokens, create_token, revoke_token,
        get_onefichier, put_onefichier, delete_onefichier,
        sign_in_onefichier, sign_out_onefichier, refresh_onefichier_cdn,
        get_tmdb, put_tmdb, delete_tmdb,
    ),
    components(schemas(ErrorBody, ErrorKind)),
    modifiers(&SecuritySchemes),
    security(("bearer" = []), ("session" = [])),
    tags(
        (name = "downloads", description = "Download queue"),
        (name = "tokens", description = "API token management"),
        (name = "settings", description = "Runtime settings"),
    )
)]
pub struct ApiDoc;

struct SecuritySchemes;

impl Modify for SecuritySchemes {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
        );
        components.add_security_scheme(
            "session",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new(super::SESSION_COOKIE))),
        );
    }
}

pub async fn openapi_json() -> impl IntoResponse {
    match ApiDoc::openapi().to_json() {
        Ok(json) => (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "failed to serialize OpenAPI document");
            ApiError::from(AppError::new(
                ErrorKind::Internal,
                "OpenAPI document unavailable",
            ))
            .into_response()
        }
    }
}
