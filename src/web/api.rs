//! JSON API (`/api/v1`). The OpenAPI document is served at
//! `/api/openapi.json`.

use super::error::{ApiJson, ApiPath, ApiQuery};
use super::{ApiError, AppState, ErrorBody};
use crate::domain::{Download, DownloadError, DownloadEvent, PostState, TransferState};
use crate::engine::LiveSnapshot;
use crate::error::{AppError, ErrorKind};
use crate::settings::OneFichierSettings;
use crate::store::ApiToken;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
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
        .route("/downloads/{id}", get(get_download))
        .route("/downloads/{id}/events", get(download_events))
        .route("/downloads/{id}/pause", post(pause_download))
        .route("/downloads/{id}/resume", post(resume_download))
        .route("/downloads/{id}/cancel", post(cancel_download))
        .route("/downloads/{id}/retry", post(retry_download))
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/{id}", delete(revoke_token))
        .route(
            "/settings/onefichier",
            get(get_onefichier)
                .put(put_onefichier)
                .delete(delete_onefichier),
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
            created_at: d.created_at,
            updated_at: d.updated_at,
            completed_at: d.completed_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DownloadList {
    pub items: Vec<DownloadDto>,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateDownload {
    /// An http or https URL.
    #[schema(example = "https://example.com/files/movie.mkv")]
    pub url: String,
    /// Password for a password-protected file (1fichier). Write-only: never
    /// returned or logged.
    pub password: Option<String>,
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
    let download = state
        .downloads
        .submit(&body.url, body.password.as_deref())
        .await?;
    Ok((StatusCode::CREATED, Json(DownloadDto::new(download, None))))
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
        create_download, list_downloads, get_download, download_events,
        pause_download, resume_download, cancel_download, retry_download,
        list_tokens, create_token, revoke_token,
        get_onefichier, put_onefichier, delete_onefichier,
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
