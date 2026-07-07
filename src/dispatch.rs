//! Payment-rail dispatch: classify each request by its payment headers.
//!
//! The payment handshake is dual-rail, so every request falls into one of four
//! states, decided purely by which payment headers are present:
//!
//! * **cold** (no payment proof): answered with the `402` challenge
//! * **x402** (`PAYMENT-SIGNATURE`): run through the x402 verify/settle flow
//! * **MPP** (`Authorization`): run through the MPP verify flow
//! * **collision** (both rails at once): rejected with `400`

use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::screener::RestrictedAddressScreener;
use crate::{AppError, Config, mpp, x402};

/// The payment rail a request is attempting, determined solely by which payment
/// headers it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rail {
    /// No payment proof: a cold request, answered with the `402` challenge.
    None,
    /// An x402 attempt (`PAYMENT-SIGNATURE` present).
    X402,
    /// An MPP attempt (`Authorization` present).
    Mpp,
    /// Both rails at once: a collision, rejected with `400`.
    Both,
}

/// Classify a request by which payment headers it carries. The router names no
/// headers itself; it asks each rail module whether its proof is present.
fn classify(headers: &HeaderMap) -> Rail {
    match (x402::has_payment(headers), mpp::has_credential(headers)) {
        (false, false) => Rail::None,
        (true, false) => Rail::X402,
        (false, true) => Rail::Mpp,
        (true, true) => Rail::Both,
    }
}

/// Build the cold `402` from the rails' challenges:
///
/// * x402: the V2 payment requirements as the JSON body, echoing `resource`
///   (the requested endpoint) back to the client.
/// * MPP: the `WWW-Authenticate: Payment` challenge header, minted fresh for
///   each `402` (omitted if it cannot be built).
fn cold_402(mpp: &mpp::Client, resource: &str) -> Response {
    let mut response = (
        StatusCode::PAYMENT_REQUIRED,
        Json(x402::challenge(resource)),
    )
        .into_response();
    // A rail that cannot produce its challenge is left out, so the 402 still
    // advertises whatever the other rail offers.
    if let Some((name, value)) = mpp::challenge(mpp) {
        response.headers_mut().insert(name, value);
    }
    response
}

/// Collision `400`: both rails presented at once. Reuses the `AppError` envelope.
fn collision_400() -> Response {
    AppError::BadRequest("send exactly one payment rail, not both".into()).into_response()
}

/// The dispatch middleware's state: one field per payment rail, built once at
/// startup and cloned into each request.
#[derive(Clone)]
pub(crate) struct Context {
    pub(crate) x402: x402::Client,
    pub(crate) mpp: mpp::Client,
    /// Payer screener, shared by every rail. `None` when screening is not configured.
    pub(crate) screener: Option<RestrictedAddressScreener>,
}

/// Assemble the dispatch context from config and the already-built screener (the
/// screener is built asynchronously at startup, so it is passed in rather than built
/// here).
pub(crate) fn context(
    config: &Config,
    screener: Option<RestrictedAddressScreener>,
) -> Result<Context, AppError> {
    Ok(Context {
        x402: x402::client(config)?,
        mpp: mpp::client(config)?,
        screener,
    })
}

/// Dispatch middleware for the paid route: classify the request by its payment
/// headers and route each state to its rail. The router decides which rail runs,
/// never how a rail verifies.
pub(crate) async fn dispatch(State(ctx): State<Context>, req: Request, next: Next) -> Response {
    match classify(req.headers()) {
        Rail::None => cold_402(&ctx.mpp, &absolute_uri(&req)),
        Rail::Both => collision_400(),
        Rail::X402 => x402::handle(ctx.x402, ctx.screener, req, next).await,
        Rail::Mpp => mpp::handle(ctx.mpp, ctx.screener, req, next).await,
    }
}

/// Reconstruct the absolute URL the client requested (`scheme://host/path`, no
/// query string) for the cold `402`'s `resource`. The query is dropped so the
/// resource names the endpoint, not one specific query; a request with no host
/// gets the bare path.
fn absolute_uri(req: &Request) -> String {
    let path = req.uri().path();
    let Some(host) = host(req) else {
        return path.to_string();
    };
    format!("{}://{host}{path}", scheme(req))
}

/// The host the client addressed: the `Host` header when non-empty, else the
/// URI's authority.
fn host(req: &Request) -> Option<&str> {
    header_str(req, header::HOST)
        .filter(|host| !host.is_empty())
        .or_else(|| req.uri().authority().map(|a| a.as_str()))
}

/// The scheme the client used: `X-Forwarded-Proto` when a TLS-terminating proxy
/// sets it, else the URI's scheme, else `http`.
fn scheme(req: &Request) -> &str {
    header_str(req, "x-forwarded-proto")
        .or_else(|| req.uri().scheme_str())
        .unwrap_or("http")
}

/// Read a request header as a string, when present and valid UTF-8.
fn header_str(req: &Request, name: impl header::AsHeaderName) -> Option<&str> {
    req.headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;

    /// Build a `HeaderMap` from `(name, value)` pairs.
    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| (name.parse().unwrap(), value.parse().unwrap()))
            .collect()
    }

    struct Case {
        /// Label printed if the assertion fails.
        name: &'static str,
        /// Request headers to send, as `(name, value)` pairs.
        headers: Vec<(&'static str, &'static str)>,
        /// The rail `classify` should return for those headers.
        expected: Rail,
    }

    #[test]
    fn classify_by_payment_headers() {
        let cases = [
            Case {
                name: "cold",
                headers: vec![],
                expected: Rail::None,
            },
            Case {
                name: "x402 v2",
                headers: vec![("payment-signature", "sig")],
                expected: Rail::X402,
            },
            Case {
                name: "mpp",
                headers: vec![("authorization", "cred")],
                expected: Rail::Mpp,
            },
            Case {
                name: "both",
                headers: vec![("payment-signature", "sig"), ("authorization", "cred")],
                expected: Rail::Both,
            },
            // x402 V1 wire (`X-PAYMENT`) is not accepted, so it reads as no payment.
            Case {
                name: "x402 v1 ignored",
                headers: vec![("x-payment", "sig")],
                expected: Rail::None,
            },
            // A V1 header alongside MPP is therefore an MPP attempt, not a collision.
            Case {
                name: "x402 v1 + mpp",
                headers: vec![("x-payment", "sig"), ("authorization", "cred")],
                expected: Rail::Mpp,
            },
            // Header names are case-insensitive (HeaderMap normalizes them), so the
            // client's casing must never change classification.
            Case {
                name: "mixed-case names",
                headers: vec![("Payment-Signature", "sig"), ("AUTHORIZATION", "cred")],
                expected: Rail::Both,
            },
        ];
        for Case {
            name,
            headers,
            expected,
        } in cases
        {
            assert_eq!(classify(&header_map(&headers)), expected, "case: {name}");
        }
    }

    #[tokio::test]
    async fn cold_402_advertises_both_rails() {
        let config = Config {
            brave_search_api_key: "key".to_string(),
            brave_search_api_base_url: "http://upstream.invalid".to_string(),
            x402_facilitator_url: "http://facilitator.invalid".to_string(),
            mpp_rpc_url: "https://rpc.moderato.tempo.xyz".to_string(),
            mpp_secret_key: "test-secret".to_string(),
            restricted_address_s3_bucket: None,
        };
        let response = cold_402(
            &mpp::client(&config).unwrap(),
            "https://bx402.example.com/res/v1/web/search",
        );
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);

        // MPP rail: a `Payment` challenge in `WWW-Authenticate`.
        let challenge = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .expect("the MPP challenge is advertised")
            .to_str()
            .unwrap();
        assert!(challenge.starts_with("Payment "));

        // x402 rail: V2 payment requirements as a JSON body.
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let requirements: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(requirements["x402Version"], 2);
        assert!(requirements["accepts"].is_array());
    }

    /// Build a request carrying `headers`, for exercising `absolute_uri`.
    fn request_with(uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut builder = Request::builder().uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn absolute_uri_uses_forwarded_proto_and_host_and_drops_query() {
        let req = request_with(
            "/res/v1/web/search?q=rust",
            &[
                ("host", "bx402.example.com"),
                ("x-forwarded-proto", "https"),
            ],
        );
        assert_eq!(
            absolute_uri(&req),
            "https://bx402.example.com/res/v1/web/search"
        );
    }

    #[test]
    fn absolute_uri_without_a_host_falls_back_to_the_path() {
        let req = request_with("/res/v1/web/search?q=rust", &[]);
        assert_eq!(absolute_uri(&req), "/res/v1/web/search");
    }

    #[test]
    fn absolute_uri_defaults_scheme_to_http() {
        let req = request_with("/res/v1/web/search", &[("host", "localhost:8080")]);
        assert_eq!(
            absolute_uri(&req),
            "http://localhost:8080/res/v1/web/search"
        );
    }
}
