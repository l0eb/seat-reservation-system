use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::http::{header, StatusCode};
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
    /// The request couldn't be parsed; keeps the status axum chose.
    Rejected(StatusCode, String),
    /// Nothing was written; safe to retry with the same Idempotency-Key.
    Overloaded,
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
            AppError::Rejected(status, msg) => (status, msg),
            AppError::Overloaded => {
                let body = Json(json!({ "error": "overloaded" }));
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(header::RETRY_AFTER, "1")],
                    body,
                )
                    .into_response();
            }
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
        // Both mean the database is too busy right now, not that the request
        // is wrong: answer 503 so the client retries.
        let statement_timeout = err
            .as_database_error()
            .and_then(|e| e.code())
            .is_some_and(|code| code == "57014");
        if matches!(err, sqlx::Error::PoolTimedOut) || statement_timeout {
            tracing::warn!(error = %err, "database overloaded");
            return AppError::Overloaded;
        }
        AppError::Internal(err.into())
    }
}

macro_rules! from_rejection {
    ($($rejection:ty),*) => {$(
        impl From<$rejection> for AppError {
            fn from(rejection: $rejection) -> Self {
                AppError::Rejected(rejection.status(), rejection.body_text())
            }
        }
    )*};
}

from_rejection!(JsonRejection, QueryRejection, PathRejection);
