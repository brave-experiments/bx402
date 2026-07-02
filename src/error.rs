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
    /// The upstream Brave Search API call failed (connect, timeout, or transport
    /// error). All of these become a `502 Bad Gateway` to our client.
    #[error("upstream Brave Search API call failed")]
    Upstream(#[from] reqwest::Error),

    /// The request was malformed — e.g. a missing or invalid query parameter.
    #[error("invalid request: {0}")]
    BadRequest(String),

    /// A required setting was absent at startup.
    #[error("missing required configuration: {0}")]
    MissingConfig(&'static str),

    /// A configured value could not initialize a startup dependency, such as an
    /// unparseable URL. Like [`AppError::MissingConfig`], this aborts startup and
    /// never emits a response.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream error"),
            AppError::BadRequest(detail) => (StatusCode::BAD_REQUEST, detail.as_str()),
            AppError::MissingConfig(_) | AppError::InvalidConfig(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "server misconfigured")
            }
        };
        // The client sees only `message`; the full error goes to our logs, a 5xx at
        // `error` and a 4xx at `debug`.
        if status.is_server_error() {
            tracing::error!(error = ?self, "request failed");
        } else {
            tracing::debug!(error = ?self, "request rejected");
        }
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
    fn missing_config_maps_to_500() {
        let response = AppError::MissingConfig("BRAVE_SEARCH_API_KEY").into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn invalid_config_maps_to_500() {
        let response = AppError::InvalidConfig("X402_FACILITATOR_URL: bad".into()).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
