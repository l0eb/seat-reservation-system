use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Why a request lost to the current state. Sent to clients as the 409 body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    SeatTaken,
    PerUserLimit,
    IdempotencyMismatch,
    AlreadyCancelled,
}

impl Conflict {
    pub fn as_str(self) -> &'static str {
        match self {
            Conflict::SeatTaken => "seat_taken",
            Conflict::PerUserLimit => "per_user_limit",
            Conflict::IdempotencyMismatch => "idempotency_mismatch",
            Conflict::AlreadyCancelled => "already_cancelled",
        }
    }
}

#[derive(Debug)]
pub enum AppError {
    Unauthorized(&'static str),
    Forbidden(&'static str),
    NotFound(&'static str),
    Conflict(Conflict),
    Validation(String),
    Internal(anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg.to_string()),
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.to_string()),
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.to_string()),
            AppError::Conflict(reason) => (StatusCode::CONFLICT, reason.as_str().to_string()),
            AppError::Validation(msg) => (StatusCode::UNPROCESSABLE_ENTITY, msg),
            AppError::Internal(err) => {
                tracing::error!(error = %err, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        AppError::Internal(err)
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        AppError::Internal(err.into())
    }
}
