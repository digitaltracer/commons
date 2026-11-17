use crate::sqlx;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use proto::api::ErrorBody;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("not signed in")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict(serde_json::Value),
    #[error("gone")]
    Gone(serde_json::Value),
    #[error("{0}")]
    TooLarge(String),
    #[error("too many attempts")]
    RateLimited,
    #[error("unsupported client")]
    UpgradeRequired,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.into())
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::Conflict(v) => {
                return (StatusCode::CONFLICT, Json(v.clone())).into_response();
            }
            ApiError::Gone(v) => return (StatusCode::GONE, Json(v.clone())).into_response(),
            ApiError::TooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
            ApiError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            ApiError::UpgradeRequired => (StatusCode::UPGRADE_REQUIRED, "upgrade_required"),
            ApiError::Internal(e) => {
                tracing::error!(error = ?e, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        };
        let message = match &self {
            ApiError::Internal(_) => "internal error".to_string(),
            other => other.to_string(),
        };
        (
            status,
            Json(ErrorBody {
                error: code.into(),
                message,
            }),
        )
            .into_response()
    }
}
