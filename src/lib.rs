//! `bx402` — a micropayment proxy for the Brave Search API over x402 and MPP.
//!
//! The name combines `bx` (Brave Search CLI) with HTTP `402 Payment Required`,
//! the status code behind the per-request payment handshake.
//!

use axum::{Router, http::StatusCode, response::IntoResponse, routing::get};

mod config;
pub use config::Config;

/// Human-readable service banner, printed on startup.
pub fn banner() -> String {
    format!("bx402 v{}", env!("CARGO_PKG_VERSION"))
}

/// Build the HTTP application.
///
/// Returns a `Router` rather than serving it, so tests can drive the same
/// router as the binary via `tower::ServiceExt::oneshot` without binding a
/// socket.
pub fn app() -> Router {
    Router::new().route("/health", get(health))
}

/// Liveness probe — returns `200 OK` with an empty body if the server is up.
async fn health() -> impl IntoResponse {
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[test]
    fn banner_includes_name_and_version() {
        let banner = banner();
        assert!(banner.starts_with("bx402 v"));
        assert!(banner.contains(env!("CARGO_PKG_VERSION")));
    }

    #[tokio::test]
    async fn health_returns_200() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
