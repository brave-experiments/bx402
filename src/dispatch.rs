//! Payment-rail dispatch: classify each request by its payment headers.
//!
//! The payment handshake is dual-rail, so every request falls into one of four
//! states, decided purely by which payment headers are present:
//!
//! - **cold** (no payment proof): answered with the dual-rail `402`
//! - **x402**: an x402 payment attempt
//! - **MPP**: an MPP payment attempt
//! - **collision** (both rails at once): rejected with `400`

// TODO: remove once the request path uses these.
#![allow(dead_code)]

use axum::http::{HeaderMap, header};

/// x402 V2 carries its payment proof in the `PAYMENT-SIGNATURE` request header.
/// V1's `X-PAYMENT` is deliberately not recognized: the service is V2-only, so a
/// V1 client carries no payment we accept and falls through to the cold `402`.
const X402_V2_PAYMENT_HEADER: &str = "payment-signature";

/// The payment rail a request is attempting, determined solely by which payment
/// headers it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rail {
    /// No payment proof: a cold request, answered with the dual-rail `402`.
    None,
    /// An x402 attempt (`PAYMENT-SIGNATURE` present).
    X402,
    /// An MPP attempt (`Authorization` present).
    Mpp,
    /// Both rails at once: a collision, rejected with `400`.
    Both,
}

/// Classify a request by which payment headers it carries.
fn classify(headers: &HeaderMap) -> Rail {
    let x402 = headers.contains_key(X402_V2_PAYMENT_HEADER);
    let mpp = headers.contains_key(header::AUTHORIZATION);
    match (x402, mpp) {
        (false, false) => Rail::None,
        (true, false) => Rail::X402,
        (false, true) => Rail::Mpp,
        (true, true) => Rail::Both,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
