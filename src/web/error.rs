//! HTTP error responses and extractors that report rejections in the API's
//! error format.

use crate::error::{AppError, ErrorKind};
use axum::Json;
use axum::extract::rejection::{FormRejection, JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Error response body: `{"error": {"kind": "...", "message": "..."}}`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ErrorDetail {
    pub kind: ErrorKind,
    /// Human-readable, safe to display.
    pub message: String,
}

#[derive(Debug)]
pub struct ApiError {
    pub error: AppError,
    /// Overrides the status derived from the error kind (used for request
    /// rejections such as 413 or 415).
    pub status: Option<StatusCode>,
}

pub fn status_for(kind: ErrorKind) -> StatusCode {
    match kind {
        ErrorKind::InvalidRequest => StatusCode::BAD_REQUEST,
        ErrorKind::UnsupportedProvider => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorKind::Unauthorized => StatusCode::UNAUTHORIZED,
        ErrorKind::Forbidden => StatusCode::FORBIDDEN,
        ErrorKind::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        ErrorKind::NotFound => StatusCode::NOT_FOUND,
        ErrorKind::Conflict => StatusCode::CONFLICT,
        ErrorKind::Persistence => StatusCode::SERVICE_UNAVAILABLE,
        // A remote service (e.g. a file host during sign-in) refused or failed.
        ErrorKind::AuthenticationFailed => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorKind::ProviderRestricted => StatusCode::SERVICE_UNAVAILABLE,
        ErrorKind::Network | ErrorKind::RemoteUnavailable => StatusCode::BAD_GATEWAY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        self.status.unwrap_or_else(|| status_for(self.error.kind))
    }
}

impl From<AppError> for ApiError {
    fn from(error: AppError) -> Self {
        Self {
            error,
            status: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(kind = %self.error.kind, message = %self.error.message, "request failed");
        }
        let body = ErrorBody {
            error: ErrorDetail {
                kind: self.error.kind,
                message: self.error.message,
            },
        };
        let mut response = (status, Json(body)).into_response();
        if status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
        }
        response
    }
}

macro_rules! rejection_into_api_error {
    ($($rejection:ty),*) => {$(
        impl From<$rejection> for ApiError {
            fn from(rejection: $rejection) -> Self {
                let status = rejection.status();
                let kind = if status == StatusCode::NOT_FOUND {
                    ErrorKind::NotFound
                } else {
                    ErrorKind::InvalidRequest
                };
                Self {
                    error: AppError::new(kind, rejection.body_text()),
                    status: Some(status),
                }
            }
        }
    )*};
}

rejection_into_api_error!(JsonRejection, PathRejection, QueryRejection, FormRejection);

/// `Json` extractor whose rejections use the API error format.
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct ApiJson<T>(pub T);

/// `Path` extractor whose rejections use the API error format.
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
pub struct ApiPath<T>(pub T);

/// `Query` extractor whose rejections use the API error format.
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
pub struct ApiQuery<T>(pub T);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_maps_to_an_error_status() {
        for kind in [
            ErrorKind::InvalidRequest,
            ErrorKind::UnsupportedProvider,
            ErrorKind::Unauthorized,
            ErrorKind::Forbidden,
            ErrorKind::RateLimited,
            ErrorKind::NotFound,
            ErrorKind::Conflict,
            ErrorKind::Persistence,
            ErrorKind::Internal,
            ErrorKind::Network,
        ] {
            let status = status_for(kind);
            assert!(
                status.is_client_error() || status.is_server_error(),
                "{kind}"
            );
        }
        assert_eq!(status_for(ErrorKind::Conflict), StatusCode::CONFLICT);
    }
}
