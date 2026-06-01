//! `bx402` — a micropayment proxy for the Brave Search API over x402 and MPP.
//!
//! The name combines `bx` (Brave Search CLI) with HTTP `402 Payment Required`,
//! the status code behind the per-request payment handshake.
//!

use std::sync::Arc;

use axum::{
    Router,
    extract::{RawQuery, State},
    http::{
        HeaderMap, StatusCode,
        header::{ACCEPT, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
    routing::get,
};

mod config;
mod error;
pub use config::Config;
pub use error::AppError;

/// Shared application state, cloned into each request handler.
///
/// `reqwest::Client` is `Arc`-internal, so cloning it is cheap and shares one
/// connection pool. `Config` is wrapped in `Arc` so handlers share it without
/// copying the strings per request.
#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    config: Arc<Config>,
}

/// Path of the Brave web-search endpoint, used both as our route and as the
/// upstream path we forward to.
const WEB_SEARCH_PATH: &str = "/res/v1/web/search";

/// Human-readable service banner, printed on startup.
pub fn banner() -> String {
    format!("bx402 v{}", env!("CARGO_PKG_VERSION"))
}

/// Build the HTTP application.
///
/// Returns a `Router` rather than serving it, so tests can drive the same
/// router as the binary via `tower::ServiceExt::oneshot` without binding a
/// socket. Takes [`Config`] by value so tests can point the proxy at a mock
/// upstream instead of the live Brave Search API.
pub fn app(config: Config) -> Router {
    let state = AppState {
        client: reqwest::Client::new(),
        config: Arc::new(config),
    };
    Router::new()
        .route("/health", get(health))
        .route(WEB_SEARCH_PATH, get(search))
        .with_state(state)
}

/// Liveness probe — returns `200 OK` with an empty body if the server is up.
async fn health() -> impl IntoResponse {
    StatusCode::OK
}

/// Proxy `GET /res/v1/web/search` to the Brave Search API.
///
/// Forwards the query string verbatim, attaches the API key as
/// a header, then relays the upstream status, content type, and
/// body back to the caller byte-for-byte.
async fn search(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Response, AppError> {
    let url = format!(
        "{}{WEB_SEARCH_PATH}",
        state.config.brave_search_api_base_url
    );
    let url = match query.as_deref().filter(|q| !q.is_empty()) {
        Some(query) => format!("{url}?{query}"),
        None => url,
    };

    let upstream = state
        .client
        .get(url)
        .header("X-Subscription-Token", &state.config.brave_search_api_key)
        .header(ACCEPT, "application/json")
        .send()
        .await?;

    let status = upstream.status();
    let content_type = upstream.headers().get(CONTENT_TYPE).cloned();
    let body = upstream.bytes().await?;

    let mut headers = HeaderMap::new();
    if let Some(content_type) = content_type {
        headers.insert(CONTENT_TYPE, content_type);
    }

    Ok((status, headers, body).into_response())
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
        let config = Config {
            brave_search_api_key: "test-key".to_string(),
            brave_search_api_base_url: "http://upstream.invalid".to_string(),
        };
        let response = app(config)
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
