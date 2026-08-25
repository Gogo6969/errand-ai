//! The local API.
//!
//! This is not a side door bolted on for KinAI: it is the app's own transport.
//! The UI is a pure API client with no database access, which is the only
//! reliable way to keep an integration API honest, because the first client to
//! notice a broken endpoint is the app itself.
//!
//! Loopback only by default.

pub mod auth;
pub mod routes;
pub mod sse;
#[cfg(test)]
pub mod testkit;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// RFC 9457 style error body with a stable machine code. Errors say what went
/// wrong and what to do, never just a status.
#[derive(Debug, Serialize)]
pub struct ApiError {
    #[serde(skip)]
    pub status: StatusCode,
    pub code: &'static str,
    pub title: &'static str,
    pub detail: String,
}

impl ApiError {
    pub fn not_found(what: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            title: "Not found",
            detail: what.into(),
        }
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "insufficient_scope",
            title: "Token lacks the required scope",
            detail: detail.into(),
        }
    }

    pub fn conflict(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code,
            title: "Conflict",
            detail: detail.into(),
        }
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            title: "Bad request",
            detail: detail.into(),
        }
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            title: "Something went wrong inside Errand",
            detail: detail.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status;
        (status, Json(self)).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;
