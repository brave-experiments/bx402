//! `bx402` — a micropayment proxy for the Brave Search API over x402 and MPP.
//!
//! The name combines `bx` (Brave Search CLI) with HTTP `402 Payment Required`,
//! the status code behind the per-request payment handshake.
//!

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Router,
    body::Bytes,
    extract::{RawQuery, State},
    http::{
        HeaderMap, HeaderValue, StatusCode, Uri,
        header::{ACCEPT, CONTENT_TYPE},
    },
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};

mod config;
mod dispatch;
mod endpoints;
mod error;
mod metrics;
mod mpp;
mod screener;
mod x402;
pub use config::{Config, MppConfig, X402Config};
pub use error::AppError;
pub use metrics::{Metrics, serve as serve_metrics};
pub use screener::{RestrictedAddressScreener, Status, init as init_screener};

/// Shared application state, cloned into each request handler.
///
/// `reqwest::Client` is `Arc`-internal, so cloning it is cheap and shares one
/// connection pool. `Config` is wrapped in `Arc` so handlers share it without
/// copying the strings per request.
#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    config: Arc<Config>,
    metrics: Arc<Metrics>,
}

/// Liveness probe path, kept in one place so the route and its metric label
/// cannot drift apart.
pub(crate) const HEALTH_PATH: &str = "/health";

/// The paid path tests reach for when any one endpoint will do. Production
/// routing reads every path from the catalog instead.
#[cfg(test)]
const WEB_SEARCH_PATH: &str = "/res/v1/web/search";

/// How long to wait for the upstream connection to establish before giving up.
const SEARCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Overall deadline for one upstream search, so a stalled Brave Search API cannot
/// pin the request task. A timeout relays as a `502`, like any transport failure.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Human-readable service banner, printed on startup.
pub fn banner() -> String {
    format!("bx402 v{}", env!("CARGO_PKG_VERSION"))
}

/// Build the HTTP application.
///
/// Returns a `Router` rather than serving it, so tests can drive the same router as
/// the binary via `tower::ServiceExt::oneshot` without binding a socket. Takes
/// [`Config`] by value so tests can point the proxy at mock upstreams instead of the
/// live Brave Search API and facilitator. Async because the MPP rail resolves its
/// chain from the RPC endpoint during construction.
pub async fn app(
    config: Config,
    screener: Option<RestrictedAddressScreener>,
    metrics: Arc<Metrics>,
) -> Result<Router, AppError> {
    let context = dispatch::context(&config, screener, metrics.clone()).await?;
    let state = AppState {
        // Build fails only if the TLS backend cannot initialize. That is a startup
        // fault like a bad URL or bucket, so it aborts startup rather than panicking.
        client: reqwest::Client::builder()
            .connect_timeout(SEARCH_CONNECT_TIMEOUT)
            .timeout(SEARCH_TIMEOUT)
            .build()
            .map_err(|err| AppError::InvalidConfig(format!("search client: {err}")))?,
        config: Arc::new(config),
        metrics: metrics.clone(),
    };
    // `route_layer` runs the dispatch gate only when the method matches, so
    // an unsupported method gets the plain 405 instead of a payable 402
    // whose search would then be refused.
    let paid = get(search).route_layer(middleware::from_fn_with_state(context, dispatch::dispatch));
    let mut router = Router::new().route(HEALTH_PATH, get(health));
    for endpoint in endpoints::ENDPOINTS {
        router = router.route(endpoint.path, paid.clone());
    }
    let router = router
        // Span and log each request; failures log at `error`, so a 5xx is visible.
        .layer(tower_http::trace::TraceLayer::new_for_http())
        // Outermost, so the timing covers everything the service does and the
        // count includes requests that match no route.
        .layer(middleware::from_fn_with_state(metrics, metrics::measure))
        .with_state(state);
    Ok(router)
}

/// Liveness probe — returns `200 OK` with an empty body if the server is up.
async fn health() -> impl IntoResponse {
    StatusCode::OK
}

/// Proxy a paid Brave Search API endpoint upstream.
///
/// Forwards the query string verbatim, attaches the API key as
/// a header, then relays the upstream status, content type, and
/// body back to the caller byte-for-byte.
///
/// The path is taken from the request and forwarded unchanged. Only paths in
/// the catalog are routed here, so an unlisted path is a `404` from the router
/// and never reaches this handler. That is what keeps the proxy closed: a
/// caller cannot name an arbitrary upstream path.
async fn search(
    State(state): State<AppState>,
    uri: Uri,
    RawQuery(query): RawQuery,
) -> Result<Response, AppError> {
    let url = format!("{}{}", state.config.brave_search_api_base_url, uri.path());
    let url = match query.as_deref().filter(|q| !q.is_empty()) {
        Some(query) => format!("{url}?{query}"),
        None => url,
    };

    // Time the whole exchange, body included, since the body is most of it.
    let endpoint = metrics::endpoint_label(uri.path());
    let started = Instant::now();
    let fetched = fetch(&state, url).await;
    let elapsed = started.elapsed();
    let upstream_status = match &fetched {
        Ok((status, _, _)) => status.as_str(),
        Err(err) => transport_failure(err),
    };
    state
        .metrics
        .record_upstream(endpoint, upstream_status, elapsed);

    let (status, content_type, body) = fetched?;

    let mut headers = HeaderMap::new();
    if let Some(content_type) = content_type {
        headers.insert(CONTENT_TYPE, content_type);
    }

    Ok((status, headers, body).into_response())
}

/// Fetch one upstream search and read it to the end.
async fn fetch(
    state: &AppState,
    url: String,
) -> Result<(StatusCode, Option<HeaderValue>, Bytes), reqwest::Error> {
    let upstream = state
        .client
        .get(url)
        .header("X-Subscription-Token", &state.config.brave_search_api_key)
        .header(ACCEPT, "application/json")
        .send()
        .await?;

    let status = upstream.status();
    let content_type = upstream.headers().get(CONTENT_TYPE).cloned();
    Ok((status, content_type, upstream.bytes().await?))
}

/// The kind of failure behind an upstream error, as a label value. A fixed set,
/// so a failing upstream cannot grow the number of series.
fn transport_failure(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else if err.is_decode() {
        "decode"
    } else {
        "transport"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::Rail;
    use crate::metrics::{assert_not_recorded, assert_recorded};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A `PAYMENT-SIGNATURE` accepting the first offer the test config advertises,
    /// with an empty scheme payload. Enough to pass decoding and reach the
    /// facilitator.
    fn payment_signature() -> String {
        x402::test_payment_signature(&Config::for_tests(), WEB_SEARCH_PATH, serde_json::json!({}))
    }

    fn config_with(base_url: String, facilitator_url: String) -> Config {
        test_config(base_url).with_facilitator_url(facilitator_url)
    }

    /// A config whose facilitator URL is parseable but unreachable, fine for the
    /// non-payment paths (cold, MPP, health) that never call the facilitator.
    fn test_config(base_url: String) -> Config {
        Config {
            brave_search_api_base_url: base_url,
            ..Config::for_tests()
        }
    }

    /// [`app`] with a registry no test reads, for the cases that assert on
    /// responses rather than on what was recorded. Returns the `Result` so the
    /// startup-failure cases can inspect it.
    async fn build_app(
        config: Config,
        screener: Option<RestrictedAddressScreener>,
    ) -> Result<axum::Router, AppError> {
        app(config, screener, Arc::new(Metrics::new())).await
    }

    /// [`build_app`], handing back the registry the app records into. Each call
    /// builds its own, so recorded values are exact and unaffected by other tests.
    async fn app_recording(
        config: Config,
        screener: Option<RestrictedAddressScreener>,
    ) -> (axum::Router, Arc<Metrics>) {
        let metrics = Arc::new(Metrics::new());
        let router = app(config, screener, metrics.clone()).await.unwrap();
        (router, metrics)
    }

    /// [`app_recording`] against `rpc`, the mock Tempo endpoint answering the
    /// startup chain query.
    async fn app_and_metrics(
        rpc: &MockServer,
        config: Config,
        screener: Option<RestrictedAddressScreener>,
    ) -> (axum::Router, Arc<Metrics>) {
        app_recording(config.with_mpp_rpc_url(rpc.uri()), screener).await
    }

    /// Build the app against a throwaway RPC, for tests that never verify an MPP
    /// payment and do not read what was recorded.
    async fn test_app(config: Config, screener: Option<RestrictedAddressScreener>) -> axum::Router {
        let (router, _metrics) = test_app_and_metrics(config, screener).await;
        router
    }

    /// [`test_app`], handing back the registry the app records into.
    async fn test_app_and_metrics(
        config: Config,
        screener: Option<RestrictedAddressScreener>,
    ) -> (axum::Router, Arc<Metrics>) {
        let rpc = crate::mpp::test_rpc().await;
        app_and_metrics(&rpc, config, screener).await
    }

    /// Start a wiremock server standing in for the x402 facilitator: `POST /verify`
    /// reports `valid`, `POST /settle` reports `settles` (with a canned receipt on
    /// success). The two are independent so a test can drive any verify/settle pairing.
    async fn mock_facilitator(valid: bool, settles: bool) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/verify"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "isValid": valid })),
            )
            .mount(&server)
            .await;
        let settle_body = if settles {
            serde_json::json!({ "success": true, "transaction": "0xtxhash" })
        } else {
            serde_json::json!({ "success": false, "error_reason": "settlement_failed" })
        };
        Mock::given(method("POST"))
            .and(path("/settle"))
            .respond_with(ResponseTemplate::new(200).set_body_json(settle_body))
            .mount(&server)
            .await;
        server
    }

    /// The request headers a [`Rail`] state sends. The MPP credential is arbitrary
    /// (dispatch keys on presence), but the x402 `payment-signature` must accept
    /// an advertised offer to pass decoding and reach the facilitator.
    fn headers_for(rail: Rail) -> Vec<(&'static str, String)> {
        match rail {
            Rail::None => vec![],
            Rail::X402 => vec![("payment-signature", payment_signature())],
            Rail::Mpp => vec![("authorization", "Payment test-cred".into())],
            Rail::Both => vec![
                ("payment-signature", payment_signature()),
                ("authorization", "Payment test-cred".into()),
            ],
        }
    }

    /// A `GET` against `uri` carrying the payment headers for `rail`, so the
    /// request reaches the dispatch gate in the chosen state.
    fn request_for(uri: &str, rail: Rail) -> Request<Body> {
        let mut request = Request::builder().uri(uri);
        for (name, value) in headers_for(rail) {
            request = request.header(name, value);
        }
        request.body(Body::empty()).unwrap()
    }

    /// Assert `response` is the cold `402`: an empty body, and a challenge
    /// header for exactly the rails in `advertises`.
    async fn assert_cold_402(response: axum::response::Response, advertises: &[Rail]) {
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(
            response.headers().contains_key("payment-required"),
            advertises.contains(&Rail::X402),
            "x402 challenge"
        );
        assert_eq!(
            response.headers().contains_key("www-authenticate"),
            advertises.contains(&Rail::Mpp),
            "mpp challenge"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(body.is_empty());
    }

    /// Drive `app()` for a `GET` against `uri`, carrying the payment headers for `rail`
    /// so the request reaches the dispatch gate in the chosen state. A valid facilitator
    /// backs the x402 rail, so an x402 attempt verifies and settles.
    async fn get_with(config: Config, uri: &str, rail: Rail) -> axum::response::Response {
        let (response, _metrics) = get_and_metrics(config, uri, rail).await;
        response
    }

    /// [`get_with`], handing back the registry the request recorded into.
    async fn get_and_metrics(
        config: Config,
        uri: &str,
        rail: Rail,
    ) -> (axum::response::Response, Arc<Metrics>) {
        let facilitator = mock_facilitator(true, true).await;
        let config = config.with_facilitator_url(facilitator.uri());
        let rpc = crate::mpp::test_rpc().await;
        let (app, metrics) = app_and_metrics(&rpc, config, None).await;
        let response = app.oneshot(request_for(uri, rail)).await.unwrap();
        (response, metrics)
    }

    #[test]
    fn banner_includes_name_and_version() {
        let banner = banner();
        assert!(banner.starts_with("bx402 v"));
        assert!(banner.contains(env!("CARGO_PKG_VERSION")));
    }

    #[tokio::test]
    async fn app_rejects_an_unparseable_facilitator_url() {
        let config = config_with(
            "http://upstream.invalid".to_string(),
            "not a url".to_string(),
        );
        // The facilitator is rejected before the MPP chain query, so no RPC is needed.
        assert!(matches!(
            build_app(config, None).await,
            Err(AppError::InvalidConfig(_))
        ));
    }

    #[tokio::test]
    async fn each_endpoint_answers_a_cold_402_at_its_own_price() {
        let config = test_config("http://upstream.invalid".to_string()).without_mpp();
        let app = build_app(config, None).await.unwrap();

        for endpoint in endpoints::ENDPOINTS {
            let response = app
                .clone()
                .oneshot(request_for(endpoint.path, Rail::None))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::PAYMENT_REQUIRED,
                "{}",
                endpoint.path
            );
            let header = response
                .headers()
                .get("payment-required")
                .expect("the x402 challenge is advertised");
            let challenge = x402::decode_challenge(header);
            let advertised = challenge["accepts"][0]["amount"].as_str().unwrap();
            assert_eq!(
                advertised,
                endpoint.price_base_units.to_string(),
                "{}",
                endpoint.path
            );
        }
    }

    /// Paying one endpoint's price does not buy a dearer one. The payment is
    /// well formed and accepts an offer we really do advertise, just not for the
    /// path it is sent to, so only the per-path lookup refuses it.
    #[tokio::test]
    async fn a_cheap_endpoints_payment_does_not_buy_a_dear_one() {
        let config = test_config("http://upstream.invalid".to_string()).without_mpp();
        let app = build_app(config, None).await.unwrap();

        let signature = x402::test_payment_signature(
            &Config::for_tests(),
            "/res/v1/suggest/search",
            serde_json::json!({}),
        );
        let request = Request::builder()
            .uri("/res/v1/web/search?q=rust")
            .header("payment-signature", signature)
            .body(Body::empty())
            .unwrap();

        // Refused before the facilitator is consulted, which is why an
        // unreachable facilitator here still yields a 402 rather than a 502.
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    }

    #[tokio::test]
    async fn an_unsold_endpoint_is_404_not_a_payable_402() {
        let config = test_config("http://upstream.invalid".to_string()).without_mpp();
        let app = build_app(config, None).await.unwrap();

        // The Answers API is deliberately not sold.
        let response = app
            .oneshot(request_for("/res/v1/chat/completions", Rail::None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn x402_only_app_needs_no_rpc_and_cold_402s_an_mpp_attempt() {
        // Built directly, with no mock RPC bound anywhere: a disabled MPP rail
        // must skip the startup chain query entirely.
        let config = test_config("http://upstream.invalid".to_string()).without_mpp();
        let app = build_app(config, None).await.unwrap();

        let response = app
            .oneshot(request_for("/res/v1/web/search?q=rust", Rail::Mpp))
            .await
            .unwrap();

        // The cold challenge, not the MPP rail's refusal.
        assert_cold_402(response, &[Rail::X402]).await;
    }

    #[tokio::test]
    async fn mpp_only_app_cold_402s_an_x402_attempt() {
        let config = test_config("http://upstream.invalid".to_string()).without_x402();
        let app = test_app(config, None).await;

        let response = app
            .clone()
            .oneshot(request_for("/res/v1/web/search?q=rust", Rail::X402))
            .await
            .unwrap();
        assert_cold_402(response, &[Rail::Mpp]).await;

        // Carrying both rails' headers stays a collision, disabled rail or not.
        let response = app
            .oneshot(request_for("/res/v1/web/search?q=rust", Rail::Both))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn no_rails_app_402s_every_payment_attempt() {
        // `ENABLED_RAILS=none`: the service stays up but nothing is payable.
        // Built with no facilitator and no RPC anywhere.
        let config = test_config("http://upstream.invalid".to_string())
            .without_x402()
            .without_mpp();
        let app = build_app(config, None).await.unwrap();

        // Cold, an MPP attempt, and an x402 attempt all get the bare 402, which
        // advertises nothing.
        for rail in [Rail::None, Rail::Mpp, Rail::X402] {
            let response = app
                .clone()
                .oneshot(request_for("/res/v1/web/search?q=rust", rail))
                .await
                .unwrap();
            assert_cold_402(response, &[]).await;
        }

        // The health probe stays green, so a load balancer keeps the service up.
        let response = app
            .oneshot(request_for("/health", Rail::None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
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
        // Settlement is gated on a successful search, so a relayed upstream error is
        // never charged: no settle call, no receipt. Only successful searches are billed.
        assert!(
            response.headers().get("payment-response").is_none(),
            "a failed search must not be settled, so it carries no receipt",
        );
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
    async fn every_request_is_counted_with_its_endpoint_method_and_status() {
        let (response, metrics) = get_and_metrics(
            test_config("http://upstream.invalid".to_string()),
            HEALTH_PATH,
            Rail::None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        assert_recorded(
            &metrics,
            r#"bx402_http_requests_total{endpoint="/health",method="GET",status="200"} 1"#,
        );
        assert_recorded(
            &metrics,
            r#"bx402_http_request_duration_seconds_count{endpoint="/health",method="GET"} 1"#,
        );
    }

    /// A path we do not serve is still counted, and its raw text never reaches a
    /// label, so requests for paths that do not exist cannot grow the series.
    #[tokio::test]
    async fn an_unsold_path_is_counted_without_minting_a_label() {
        let config = test_config("http://upstream.invalid".to_string()).without_mpp();
        let (app, metrics) = app_recording(config, None).await;

        let response = app
            .oneshot(request_for("/res/v1/chat/completions", Rail::None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        assert_recorded(
            &metrics,
            r#"bx402_http_requests_total{endpoint="other",method="GET",status="404"} 1"#,
        );
        assert_not_recorded(&metrics, "chat/completions");
    }

    #[tokio::test]
    async fn an_upstream_error_is_counted_under_its_status() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .respond_with(ResponseTemplate::new(500).set_body_string("brave is down"))
            .mount(&upstream)
            .await;

        let (response, metrics) = get_and_metrics(
            test_config(upstream.uri()),
            "/res/v1/web/search?q=rust",
            Rail::X402,
        )
        .await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        assert_recorded(
            &metrics,
            r#"bx402_upstream_requests_total{endpoint="/res/v1/web/search",status="500"} 1"#,
        );
        assert_recorded(
            &metrics,
            r#"bx402_upstream_duration_seconds_count{endpoint="/res/v1/web/search"} 1"#,
        );
    }

    /// An upstream that never answers is counted too, under the kind of failure
    /// rather than a status it never sent.
    #[tokio::test]
    async fn an_unreachable_upstream_is_counted_as_a_transport_failure() {
        let (response, metrics) = get_and_metrics(
            test_config("http://127.0.0.1:1".to_string()),
            "/res/v1/web/search?q=rust",
            Rail::X402,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        assert_recorded(
            &metrics,
            r#"bx402_upstream_requests_total{endpoint="/res/v1/web/search",status="connect"} 1"#,
        );
    }

    #[tokio::test]
    async fn challenges_record_why_they_were_issued() {
        let config = test_config("http://upstream.invalid".to_string());
        let rpc = crate::mpp::test_rpc().await;
        let (app, metrics) = app_and_metrics(&rpc, config, None).await;

        for (rail, reason) in [(Rail::None, "no_payment"), (Rail::Both, "collision")] {
            let _ = app
                .clone()
                .oneshot(request_for("/res/v1/web/search?q=rust", rail))
                .await
                .unwrap();
            assert_recorded(
                &metrics,
                &format!(
                    r#"bx402_challenges_total{{endpoint="/res/v1/web/search",reason="{reason}"}} 1"#
                ),
            );
        }
    }

    /// A payer on a rail this deployment turned off is counted apart from a
    /// caller who simply did not pay, which is the only way to see clients
    /// stranded against a disabled rail.
    #[tokio::test]
    async fn a_payment_on_a_disabled_rail_is_counted_apart_from_a_cold_request() {
        let config = test_config("http://upstream.invalid".to_string()).without_x402();
        let rpc = crate::mpp::test_rpc().await;
        let (app, metrics) = app_and_metrics(&rpc, config, None).await;

        let response = app
            .oneshot(request_for("/res/v1/web/search?q=rust", Rail::X402))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);

        assert_recorded(
            &metrics,
            r#"bx402_challenges_total{endpoint="/res/v1/web/search",reason="rail_disabled"} 1"#,
        );
    }

    /// Drive one paid x402 request against a facilitator behaving as
    /// `valid`/`settles` and an upstream answering `upstream_status`, handing
    /// back what the request recorded.
    async fn paid_x402(
        valid: bool,
        settles: bool,
        upstream_status: u16,
    ) -> (axum::response::Response, Arc<Metrics>) {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(WEB_SEARCH_PATH))
            .respond_with(ResponseTemplate::new(upstream_status).set_body_string("{}"))
            .mount(&upstream)
            .await;
        let facilitator = mock_facilitator(valid, settles).await;
        let config = config_with(upstream.uri(), facilitator.uri()).without_mpp();
        let (app, metrics) = app_recording(config, None).await;
        let response = app.oneshot(paid_request()).await.unwrap();
        (response, metrics)
    }

    #[tokio::test]
    async fn a_settled_payment_records_its_outcome_price_and_step_timings() {
        let (response, metrics) = paid_x402(true, true, 200).await;
        assert_eq!(response.status(), StatusCode::OK);

        assert_payment_outcome(&metrics, "x402", "settled");
        // The catalog price, not anything the payer stated.
        assert_recorded(
            &metrics,
            r#"bx402_charged_base_units_total{rail="x402",endpoint="/res/v1/web/search"} 5000"#,
        );
        for step in ["verify", "settle"] {
            assert_recorded(
                &metrics,
                &format!(
                    r#"bx402_payment_step_duration_seconds_count{{rail="x402",step="{step}"}} 1"#
                ),
            );
        }
    }

    /// Every way a payment can end badly is counted apart, even though the
    /// client is told the same thing.
    #[tokio::test]
    async fn each_way_a_payment_fails_records_its_own_outcome() {
        let (response, metrics) = paid_x402(false, true, 200).await;
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        assert_payment_outcome(&metrics, "x402", "refused");

        // Verified, then the search failed: relayed as is, and never charged.
        let (response, metrics) = paid_x402(true, true, 500).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_payment_outcome(&metrics, "x402", "upstream_failed");
        assert_not_recorded(&metrics, r#"bx402_charged_base_units_total{"#);

        // Verified, then settlement declined.
        let (response, metrics) = paid_x402(true, false, 200).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_payment_outcome(&metrics, "x402", "settle_failed");
    }

    #[tokio::test]
    async fn a_blocked_payer_records_a_screened_out_payment() {
        let from = "0xAb5801a7D398351b8bE11C439e05C5B3259aeC9B";
        let (_s3, screener) = screener_blocking(from).await;
        let upstream = untouched_upstream().await;

        let (app, metrics) =
            test_app_and_metrics(test_config(upstream.uri()), Some(screener)).await;

        let response = app.oneshot(paid_request_from(from)).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        assert_payment_outcome(&metrics, "x402", "screened_out");
    }

    /// Assert exactly one payment on `rail` for the web search endpoint ended in
    /// `outcome`.
    fn assert_payment_outcome(metrics: &Metrics, rail: &str, outcome: &str) {
        assert_recorded(
            metrics,
            &format!(
                r#"bx402_payments_total{{rail="{rail}",endpoint="/res/v1/web/search",outcome="{outcome}"}} 1"#
            ),
        );
    }

    #[tokio::test]
    async fn dispatch_routes_by_payment_headers() {
        struct Case {
            name: &'static str,
            rail: Rail,
            expected: StatusCode,
        }
        // The upstream is unreachable, so cold/collision requests short-circuit in
        // the dispatch layer and the malformed MPP credential is refused by its rail.
        // A verified x402 request passes the gate and the handler 502s trying to
        // reach the upstream, so a 502 confirms dispatch let it through to `search`.
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
                name: "mpp rejected: malformed credential",
                rail: Rail::Mpp,
                expected: StatusCode::PAYMENT_REQUIRED,
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

    /// Drive `app()` with an MPP `Authorization` header through the paid route.
    /// The RPC answers only the startup chain query, so a verify-time RPC call
    /// surfaces as a 502.
    async fn get_mpp(
        config: Config,
        screener: Option<RestrictedAddressScreener>,
        authorization: &str,
    ) -> axum::response::Response {
        let (response, _metrics) = get_mpp_and_metrics(config, screener, authorization).await;
        response
    }

    /// [`get_mpp`], handing back the registry the request recorded into.
    async fn get_mpp_and_metrics(
        config: Config,
        screener: Option<RestrictedAddressScreener>,
        authorization: &str,
    ) -> (axum::response::Response, Arc<Metrics>) {
        // The mock RPC is bound here, not inside a helper, so it stays alive for
        // the request. Dropping it early frees its port for another test's mock
        // to bind, and a verify meant to find nothing there reaches that instead.
        let rpc = crate::mpp::test_rpc().await;
        let (app, metrics) = app_and_metrics(&rpc, config, screener).await;
        let request = Request::builder()
            .uri(format!("{WEB_SEARCH_PATH}?q=rust"))
            .header("authorization", authorization)
            .body(Body::empty())
            .unwrap();
        (app.oneshot(request).await.unwrap(), metrics)
    }

    /// An upstream that must never be called, for tests asserting a payment is
    /// refused before the search runs. Keep the returned server alive for the
    /// whole test. Wiremock checks the zero-request expectation at the end,
    /// when the server goes out of scope.
    async fn untouched_upstream() -> MockServer {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&upstream)
            .await;
        upstream
    }

    /// A config for `get_mpp` tests proving a refusal happens before any
    /// network touch:
    /// * a reached RPC surfaces as a 502 (it answers only the startup query)
    /// * a reached upstream fails its zero-request expectation on drop
    async fn refusing_mpp_config() -> (Config, MockServer) {
        let upstream = untouched_upstream().await;
        (test_config(upstream.uri()), upstream)
    }

    #[tokio::test]
    async fn mpp_hash_credential_is_refused_before_any_search() {
        let (config, _upstream) = refusing_mpp_config().await;

        // The credential answers our real challenge, but its payload says the client
        // already broadcast the transfer itself, which this service does not accept.
        let credential = crate::mpp::hash_credential_header(&config).await;
        let response = get_mpp(config, None, &credential).await;

        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        // The rail's own refusal, not the cold 402 a misrouted request would get.
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "error": "mpp payment did not verify" })
        );
    }

    #[tokio::test]
    async fn mpp_unreachable_tempo_rpc_is_a_gateway_error() {
        let (config, _upstream) = refusing_mpp_config().await;

        // The credential is well formed, so verification fails only at the transport
        // layer: our failure rather than the client's, and never a free search.
        let (credential, _signer) = crate::mpp::signed_transaction_credential_header(&config).await;
        let response = get_mpp(config, None, &credential).await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn mpp_blocked_signer_never_reaches_tempo_or_the_search() {
        let (config, _upstream) = refusing_mpp_config().await;

        // The restricted list holds exactly the address that signed the transaction.
        let (credential, signer) = crate::mpp::signed_transaction_credential_header(&config).await;
        let (_s3, screener) = screener_blocking(&signer).await;

        let response = get_mpp(config, Some(screener), &credential).await;

        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    }

    /// The MPP rail refuses in several ways that look identical to the client,
    /// so each keeps its own outcome.
    #[tokio::test]
    async fn mpp_refusals_record_their_own_outcomes() {
        // A credential we cannot read at all.
        let (config, _upstream) = refusing_mpp_config().await;
        let (response, metrics) =
            get_mpp_and_metrics(config, None, "Payment not-a-credential").await;
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        assert_payment_outcome(&metrics, "mpp", "malformed");

        // Readable, and a kind of credential this rail does not take: the client
        // says it broadcast the transfer itself, before we could screen it.
        let (config, _upstream) = refusing_mpp_config().await;
        let credential = crate::mpp::hash_credential_header(&config).await;
        let (response, metrics) = get_mpp_and_metrics(config, None, &credential).await;
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        assert_payment_outcome(&metrics, "mpp", "unsupported");
    }

    #[tokio::test]
    async fn an_unreachable_tempo_rpc_records_the_charge_it_attempted() {
        let (config, _upstream) = refusing_mpp_config().await;
        let (credential, _signer) = crate::mpp::signed_transaction_credential_header(&config).await;
        let (response, metrics) = get_mpp_and_metrics(config, None, &credential).await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_payment_outcome(&metrics, "mpp", "network_unavailable");

        // The charge is timed even when it fails, so a slow chain and an
        // unreachable one are both visible.
        assert_recorded(
            &metrics,
            r#"bx402_payment_step_duration_seconds_count{rail="mpp",step="charge"} 1"#,
        );
    }

    #[tokio::test]
    async fn a_blocked_mpp_signer_records_a_screened_out_payment() {
        let (config, _upstream) = refusing_mpp_config().await;
        let (credential, signer) = crate::mpp::signed_transaction_credential_header(&config).await;
        let (_s3, screener) = screener_blocking(&signer).await;

        let (response, metrics) = get_mpp_and_metrics(config, Some(screener), &credential).await;

        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        assert_payment_outcome(&metrics, "mpp", "screened_out");
        // Refused before the charge, so nothing was attempted on chain.
        assert_not_recorded(&metrics, r#"step="charge""#);
    }

    #[tokio::test]
    async fn cold_402_advertises_the_absolute_request_url_as_resource() {
        // End-to-end: a cold request through the real router must echo back the exact
        // URL it hit as `resource.url`, built from the proxy headers (scheme from
        // `X-Forwarded-Proto`, host from `Host`) with the query kept.
        let request = Request::builder()
            .uri("/res/v1/web/search?q=rust")
            .header("host", "api.bx402.io")
            .header("x-forwarded-proto", "https")
            .body(Body::empty())
            .unwrap();
        let response = test_app(test_config("http://upstream.invalid".to_string()), None)
            .await
            .oneshot(request)
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        let challenge = x402::decode_challenge(
            response
                .headers()
                .get("payment-required")
                .expect("the x402 challenge is advertised"),
        );
        assert_eq!(
            challenge["resource"]["url"],
            "https://api.bx402.io/res/v1/web/search?q=rust"
        );
        // The route binding names the method the request arrived with.
        assert_eq!(challenge["extensions"]["mppx"]["info"]["method"], "GET");
    }

    #[tokio::test]
    async fn unsupported_method_is_405_not_a_payable_402() {
        // The dispatch gate runs only for methods the route serves. A POST must get
        // the plain 405, not a 402 challenge whose payment would buy a 405.
        let request = Request::builder()
            .method("POST")
            .uri("/res/v1/web/search?q=rust")
            .body(Body::empty())
            .unwrap();
        let response = test_app(test_config("http://upstream.invalid".to_string()), None)
            .await
            .oneshot(request)
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(response.headers().get("payment-required").is_none());
    }

    /// Build a `GET /res/v1/web/search?q=rust` carrying a decodable x402 payment proof.
    fn paid_request() -> Request<Body> {
        Request::builder()
            .uri("/res/v1/web/search?q=rust")
            .header("payment-signature", payment_signature())
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn verified_payment_runs_the_search_and_returns_a_settlement_receipt() {
        let upstream = MockServer::start().await;
        let upstream_body = serde_json::json!({ "web": { "results": [] } });
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&upstream_body))
            .expect(1) // the search runs exactly once, after verification
            .mount(&upstream)
            .await;

        let facilitator = mock_facilitator(true, true).await;
        let response = test_app(config_with(upstream.uri(), facilitator.uri()), None)
            .await
            .oneshot(paid_request())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // The settlement receipt rides back base64-encoded in `Payment-Response`.
        let receipt_header = response
            .headers()
            .get("payment-response")
            .expect("settled response carries a Payment-Response receipt")
            .to_str()
            .unwrap()
            .to_owned();
        let receipt = x402::decode_receipt(&receipt_header);
        assert_eq!(receipt["success"], true);

        // The upstream body is relayed unchanged underneath the receipt header.
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body, upstream_body);
    }

    #[tokio::test]
    async fn rejected_payment_returns_402_and_never_calls_upstream() {
        let upstream = untouched_upstream().await;
        let facilitator = mock_facilitator(false, true).await;
        let response = test_app(config_with(upstream.uri(), facilitator.uri()), None)
            .await
            .oneshot(paid_request())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        // Nothing settled, so no receipt.
        assert!(response.headers().get("payment-response").is_none());
    }

    #[tokio::test]
    async fn unsettled_payment_withholds_a_successful_body() {
        // Verify passes and the search succeeds, but settlement fails. The client did
        // not pay, so the produced body must be withheld behind a 502 rather than served.
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "web": {} })),
            )
            .mount(&upstream)
            .await;

        let facilitator = mock_facilitator(true, false).await;
        let response = test_app(config_with(upstream.uri(), facilitator.uri()), None)
            .await
            .oneshot(paid_request())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(response.headers().get("payment-response").is_none());
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "x402 payment could not be settled");
    }

    /// A paid `GET` whose payment payload names `from` as the payer, so the screener has
    /// an address to check. The scheme payload only needs `authorization.from`.
    /// The mock facilitator accepts the rest.
    fn paid_request_from(from: &str) -> Request<Body> {
        let signature = x402::test_payment_signature(
            &Config::for_tests(),
            WEB_SEARCH_PATH,
            serde_json::json!({ "authorization": { "from": from } }),
        );
        Request::builder()
            .uri("/res/v1/web/search?q=rust")
            .header("payment-signature", signature)
            .body(Body::empty())
            .unwrap()
    }

    /// The S3 key the screener will look up for `address`, mirroring the rail's
    /// lowercasing and the screener's encoding.
    fn screening_key(address: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(address.to_ascii_lowercase())
    }

    /// A screener whose restricted list holds exactly `address`, and whose key
    /// must be looked up exactly once. Keep the returned mock S3 alive for the
    /// whole test. Wiremock checks the expectation at the end, when the server
    /// goes out of scope.
    async fn screener_blocking(address: &str) -> (MockServer, RestrictedAddressScreener) {
        let s3 = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path(format!(
                "/{}/{}",
                crate::screener::TEST_BUCKET,
                screening_key(address)
            )))
            .respond_with(ResponseTemplate::new(200)) // key exists: on the list
            .expect(1)
            .mount(&s3)
            .await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&s3)
            .await;
        let screener = crate::screener::test_screener(s3.uri());
        (s3, screener)
    }

    #[tokio::test]
    async fn blocked_signer_is_refused_before_any_call() {
        // Clients send the checksummed form. The rail lowercases it, so the
        // stored key is lowercase.
        let from = "0xAb5801a7D398351b8bE11C439e05C5B3259aeC9B";
        let (_s3, screener) = screener_blocking(from).await;

        // The search must never run for a blocked signer.
        let upstream = untouched_upstream().await;

        // Facilitator is unreachable: a blocked signer must not reach it either.
        let response = test_app(test_config(upstream.uri()), Some(screener))
            .await
            .oneshot(paid_request_from(from))
            .await
            .unwrap();

        // Refused as a generic 402, like any rejected payment. The unreachable facilitator
        // would 502 if the request reached verify, so 402 proves the block happened first.
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    }

    #[tokio::test]
    async fn allowed_signer_passes_through_to_search() {
        // Every key 404s: the payer is not on the list.
        let (_s3, screener) = crate::screener::test_screener_answering(404).await;

        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(WEB_SEARCH_PATH))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "web": {} })),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let facilitator = mock_facilitator(true, true).await;
        let response = test_app(
            config_with(upstream.uri(), facilitator.uri()),
            Some(screener),
        )
        .await
        .oneshot(paid_request_from(
            "0x1111111111111111111111111111111111111111",
        ))
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unscreenable_signer_returns_503() {
        // The bucket errors, so the payer cannot be screened: deny, do not serve.
        let (_s3, screener) = crate::screener::test_screener_answering(500).await;
        let upstream = untouched_upstream().await;

        let response = test_app(test_config(upstream.uri()), Some(screener))
            .await
            .oneshot(paid_request_from(
                "0x2222222222222222222222222222222222222222",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
