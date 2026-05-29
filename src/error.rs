//! Typed application errors and their HTTP representation.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use thiserror::Error;

/// Every error condition the service surfaces to a client.
///
/// One variant per failure mode the caller treats differently — not per cause.
/// Each maps to a single HTTP status in the [`IntoResponse`] impl, so handlers
/// can return `Result<T, AppError>` and let `?` carry errors to the response.
#[derive(Debug, Error)]
pub enum AppError {
    /// The request was malformed — e.g. a missing or invalid query parameter.
    #[error("invalid request: {0}")]
    BadRequest(String),

    /// The server is misconfigured — a required setting was absent at startup.
    #[error("missing required configuration: {0}")]
    Config(&'static str),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::BadRequest(detail) => (StatusCode::BAD_REQUEST, detail.as_str()),
            AppError::Config(_) => (StatusCode::INTERNAL_SERVER_ERROR, "server misconfigured"),
        };
        // The full error (with its source chain) belongs in our logs; the
        // client only ever sees `message`.
        (status, Json(json!({ "error": message }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_request_maps_to_400() {
        let response = AppError::BadRequest("q is required".into()).into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn config_maps_to_500() {
        let response = AppError::Config("BRAVE_SEARCH_API_KEY").into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
