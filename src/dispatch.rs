//! Payment-rail dispatch: classify each request by its payment headers.
//!
//! The payment handshake is dual-rail, so every request falls into one of four
//! states, decided purely by which payment headers are present:
//!
//! - **cold** (no payment proof): answered with the dual-rail `402`
//! - **x402**: an x402 payment attempt
//! - **MPP**: an MPP payment attempt
//! - **collision** (both rails at once): rejected with `400`

use axum::{
    Json,
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::{AppError, mpp, x402};

/// The payment rail a request is attempting, determined solely by which payment
/// headers it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rail {
    /// No payment proof: a cold request, answered with the dual-rail `402`.
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

/// Build the cold dual-rail `402` by composing each rail's challenge: x402's V2
/// requirements in the JSON body, MPP's challenge in the `WWW-Authenticate` header.
/// The client retries on the rail it can pay.
fn cold_402() -> Response {
    (
        StatusCode::PAYMENT_REQUIRED,
        [mpp::challenge()],
        Json(x402::challenge()),
    )
        .into_response()
}

/// Collision `400`: both rails presented at once. Reuses the `AppError` envelope.
fn collision_400() -> Response {
    AppError::BadRequest("send exactly one payment rail, not both".into()).into_response()
}

/// Dispatch middleware for the paid route: classify by payment headers, then either
/// short-circuit a cold request with `402` or a rail collision with `400`, or pass a
/// payment attempt through to the handler. Verifying the payment is the rails' job.
pub(crate) async fn dispatch(req: Request, next: Next) -> Response {
    match classify(req.headers()) {
        Rail::None => cold_402(),
        Rail::Both => collision_400(),
        Rail::X402 | Rail::Mpp => next.run(req).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;
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
        let response = cold_402();
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);

        // MPP rail: a `WWW-Authenticate` challenge using the `Payment` scheme.
        let challenge = response.headers().get(header::WWW_AUTHENTICATE).unwrap();
        assert!(challenge.to_str().unwrap().starts_with("Payment"));

        // x402 rail: V2 payment requirements as a JSON body.
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let requirements: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(requirements["x402Version"], 2);
        assert!(requirements["accepts"].is_array());
    }
}
