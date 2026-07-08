//! `bx402` — a micropayment proxy for the Brave Search API over x402 and MPP.
//!
//! The name combines `bx` (Brave Search CLI) with HTTP `402 Payment Required`,
//! the status code behind the per-request payment handshake.
//!

use std::sync::Arc;
use std::time::Duration;

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
mod mpp;
mod screener;
mod x402;
pub use config::Config;
pub use error::AppError;
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
}

/// Path of the Brave web-search endpoint, used both as our route and as the
/// upstream path we forward to.
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
/// live Brave Search API and facilitator.
pub fn app(
    config: Config,
    screener: Option<RestrictedAddressScreener>,
) -> Result<Router, AppError> {
    let context = dispatch::context(&config, screener)?;
    let state = AppState {
        // Build fails only if the TLS backend cannot initialize. That is a startup
        // fault like a bad URL or bucket, so it aborts startup rather than panicking.
        client: reqwest::Client::builder()
            .connect_timeout(SEARCH_CONNECT_TIMEOUT)
            .timeout(SEARCH_TIMEOUT)
            .build()
            .map_err(|err| AppError::InvalidConfig(format!("search client: {err}")))?,
        config: Arc::new(config),
    };
    let router = Router::new()
        .route("/health", get(health))
        .route(
            WEB_SEARCH_PATH,
            get(search).layer(middleware::from_fn_with_state(context, dispatch::dispatch)),
        )
        // Span and log each request; failures log at `error`, so a 5xx is visible.
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state);
    Ok(router)
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

    /// A `PAYMENT-SIGNATURE` value that base64-decodes to the JSON object `{}` — enough
    /// for the pay flow to parse a payload before the request reaches the facilitator.
    const PAYMENT_SIGNATURE: &str = "e30="; // base64("{}")

    fn config_with(base_url: String, facilitator_url: String) -> Config {
        Config {
            brave_search_api_base_url: base_url,
            x402_facilitator_url: facilitator_url,
            ..Config::for_tests()
        }
    }

    /// A config whose facilitator URL is parseable but unreachable, fine for the
    /// non-payment paths (cold, MPP, health) that never call the facilitator.
    fn test_config(base_url: String) -> Config {
        Config {
            brave_search_api_base_url: base_url,
            ..Config::for_tests()
        }
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
    /// (dispatch keys on presence), but the x402 `payment-signature` must be the
    /// base64 JSON the pay flow decodes before it reaches the facilitator.
    fn headers_for(rail: Rail) -> Vec<(&'static str, &'static str)> {
        match rail {
            Rail::None => vec![],
            Rail::X402 => vec![("payment-signature", PAYMENT_SIGNATURE)],
            Rail::Mpp => vec![("authorization", "Payment test-cred")],
            Rail::Both => vec![
                ("payment-signature", PAYMENT_SIGNATURE),
                ("authorization", "Payment test-cred"),
            ],
        }
    }

    /// Drive `app()` for a `GET` against `uri`, carrying the payment headers for `rail`
    /// so the request reaches the dispatch gate in the chosen state. A valid facilitator
    /// backs the x402 rail, so an x402 attempt verifies and settles.
    async fn get_with(config: Config, uri: &str, rail: Rail) -> axum::response::Response {
        let facilitator = mock_facilitator(true, true).await;
        let config = Config {
            x402_facilitator_url: facilitator.uri(),
            ..config
        };
        let mut request = Request::builder().uri(uri);
        for (name, value) in headers_for(rail) {
            request = request.header(name, value);
        }
        app(config, None)
            .unwrap()
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

    #[test]
    fn app_rejects_an_unparseable_facilitator_url() {
        let config = config_with(
            "http://upstream.invalid".to_string(),
            "not a url".to_string(),
        );
        assert!(matches!(app(config, None), Err(AppError::InvalidConfig(_))));
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
    async fn get_mpp(
        config: Config,
        screener: Option<RestrictedAddressScreener>,
        authorization: &str,
    ) -> axum::response::Response {
        let request = Request::builder()
            .uri(format!("{WEB_SEARCH_PATH}?q=rust"))
            .header("authorization", authorization)
            .body(Body::empty())
            .unwrap();
        app(config, screener)
            .unwrap()
            .oneshot(request)
            .await
            .unwrap()
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

    /// A config wired to an upstream that must never be called and an unreachable
    /// Tempo RPC, so a refusal is proven to happen before either is touched. A
    /// reached RPC surfaces as a 502; a reached upstream fails the mock's
    /// expectation when the returned server drops.
    async fn refusing_mpp_config() -> (Config, MockServer) {
        let upstream = untouched_upstream().await;
        let config = Config {
            mpp_rpc_url: "http://127.0.0.1:1".to_string(),
            ..test_config(upstream.uri())
        };
        (config, upstream)
    }

    #[tokio::test]
    async fn mpp_hash_credential_is_refused_before_any_search() {
        let (config, _upstream) = refusing_mpp_config().await;

        // The credential answers our real challenge, but its payload says the client
        // already broadcast the transfer itself, which this service does not accept.
        let credential = crate::mpp::hash_credential_header(&config);
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

    #[tokio::test]
    async fn cold_402_advertises_the_absolute_request_url_as_resource() {
        // End-to-end: a cold request through the real router must echo the endpoint
        // it hit back as `resource.url`, built from the proxy headers (scheme from
        // `X-Forwarded-Proto`, host from `Host`) with the query stripped.
        let request = Request::builder()
            .uri("/res/v1/web/search?q=rust")
            .header("host", "api.bx402.io")
            .header("x-forwarded-proto", "https")
            .body(Body::empty())
            .unwrap();
        let response = app(test_config("http://upstream.invalid".to_string()), None)
            .unwrap()
            .oneshot(request)
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["resource"]["url"],
            "https://api.bx402.io/res/v1/web/search"
        );
    }

    /// Build a `GET /res/v1/web/search?q=rust` carrying a decodable x402 payment proof.
    fn paid_request() -> Request<Body> {
        Request::builder()
            .uri("/res/v1/web/search?q=rust")
            .header("payment-signature", PAYMENT_SIGNATURE)
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
        let response = app(config_with(upstream.uri(), facilitator.uri()), None)
            .unwrap()
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
        let response = app(config_with(upstream.uri(), facilitator.uri()), None)
            .unwrap()
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
        let response = app(config_with(upstream.uri(), facilitator.uri()), None)
            .unwrap()
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
    /// an address to check. The payload only needs `payload.authorization.from`; the
    /// mock facilitator accepts the rest.
    fn paid_request_from(from: &str) -> Request<Body> {
        use base64::Engine;
        let payload = serde_json::json!({ "payload": { "authorization": { "from": from } } });
        let signature = base64::engine::general_purpose::STANDARD.encode(payload.to_string());
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
        let response = app(test_config(upstream.uri()), Some(screener))
            .unwrap()
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
        let response = app(
            config_with(upstream.uri(), facilitator.uri()),
            Some(screener),
        )
        .unwrap()
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

        let response = app(test_config(upstream.uri()), Some(screener))
            .unwrap()
            .oneshot(paid_request_from(
                "0x2222222222222222222222222222222222222222",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
