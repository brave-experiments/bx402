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
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};

mod config;
mod dispatch;
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
        .route(
            WEB_SEARCH_PATH,
            get(search).layer(middleware::from_fn(dispatch::dispatch)),
        )
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
    use crate::dispatch::Rail;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_config(base_url: String) -> Config {
        Config {
            brave_search_api_key: "secret-key".to_string(),
            brave_search_api_base_url: base_url,
        }
    }

    /// The request headers a [`Rail`] state sends. Values are arbitrary; dispatch keys
    /// only on header presence.
    fn headers_for(rail: Rail) -> Vec<(&'static str, &'static str)> {
        match rail {
            Rail::None => vec![],
            Rail::X402 => vec![("payment-signature", "test-proof")],
            Rail::Mpp => vec![("authorization", "Payment test-cred")],
            Rail::Both => vec![
                ("payment-signature", "test-proof"),
                ("authorization", "Payment test-cred"),
            ],
        }
    }

    /// Drive `app()` for a `GET` against `uri`, carrying the payment headers for `rail` so
    /// the request reaches the dispatch gate in the chosen state.
    async fn get_with(config: Config, uri: &str, rail: Rail) -> axum::response::Response {
        let mut request = Request::builder().uri(uri);
        for (name, value) in headers_for(rail) {
            request = request.header(name, value);
        }
        app(config)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[test]
    fn banner_includes_name_and_version() {
        let banner = banner();
        assert!(banner.starts_with("bx402 v"));
        assert!(banner.contains(env!("CARGO_PKG_VERSION")));
    }

    #[tokio::test]
    async fn health_returns_200() {
        let response = get_with(
            test_config("http://upstream.invalid".to_string()),
            "/health",
            Rail::None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn forwards_query_and_key_then_relays_body() {
        let upstream = MockServer::start().await;
        let upstream_body = serde_json::json!({ "web": { "results": [] } });

        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .and(query_param("q", "rust"))
            .and(header("X-Subscription-Token", "secret-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&upstream_body))
            .expect(1) // also asserts the query + header matched
            .mount(&upstream)
            .await;

        let response = get_with(
            test_config(upstream.uri()),
            "/res/v1/web/search?q=rust",
            Rail::X402,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body, upstream_body);
    }

    #[tokio::test]
    async fn upstream_5xx_is_relayed_byte_for_byte() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .respond_with(ResponseTemplate::new(500).set_body_string("brave is down"))
            .mount(&upstream)
            .await;

        let response = get_with(
            test_config(upstream.uri()),
            "/res/v1/web/search?q=rust",
            Rail::X402,
        )
        .await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), "brave is down");
    }

    #[tokio::test]
    async fn unreachable_upstream_becomes_502() {
        // Nothing listens on port 1, so reqwest returns a transport error,
        // which the handler maps to 502 via `AppError::Upstream` — distinct from
        // an upstream that responds with a 5xx (relayed as-is, test above).
        let response = get_with(
            test_config("http://127.0.0.1:1".to_string()),
            "/res/v1/web/search?q=rust",
            Rail::X402,
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn dispatch_routes_by_payment_headers() {
        struct Case {
            name: &'static str,
            rail: Rail,
            expected: StatusCode,
        }
        // The upstream is unreachable, so cold/collision requests short-circuit in the
        // dispatch layer before it, while the has-payment requests pass the gate and the
        // handler 502s trying to reach it. A 502 therefore confirms dispatch let them
        // through to `search`.
        let cases = [
            Case {
                name: "cold",
                rail: Rail::None,
                expected: StatusCode::PAYMENT_REQUIRED,
            },
            Case {
                name: "collision",
                rail: Rail::Both,
                expected: StatusCode::BAD_REQUEST,
            },
            Case {
                name: "x402 through",
                rail: Rail::X402,
                expected: StatusCode::BAD_GATEWAY,
            },
            Case {
                name: "mpp through",
                rail: Rail::Mpp,
                expected: StatusCode::BAD_GATEWAY,
            },
        ];
        for Case {
            name,
            rail,
            expected,
        } in cases
        {
            let response = get_with(
                test_config("http://127.0.0.1:1".to_string()),
                "/res/v1/web/search?q=rust",
                rail,
            )
            .await;
            assert_eq!(response.status(), expected, "case: {name}");
        }
    }
}
