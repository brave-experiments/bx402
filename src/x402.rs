//! The x402 payment rail — everything specific to x402 lives here.
//!
//! See `mpp.rs` for the MPP rail and `dispatch.rs` for the neutral router that
//! classifies each request and delegates to whichever rail it is paying on.

use axum::http::HeaderMap;
use serde_json::{Value, json};

/// x402 V2 carries its payment proof in the `PAYMENT-SIGNATURE` request header.
/// V1's `X-PAYMENT` is deliberately not recognized: the service is V2-only, so a
/// V1 client carries no payment we accept and falls through to the cold `402`.
const V2_PAYMENT_HEADER: &str = "payment-signature";

/// Returns `true` if the request carries an x402 V2 payment proof.
pub(crate) fn has_payment(headers: &HeaderMap) -> bool {
    headers.contains_key(V2_PAYMENT_HEADER)
}

/// The x402 contribution to the cold dual-rail `402`: the V2 payment requirements
/// the client's wallet signs against, returned as the JSON response body.
pub(crate) fn challenge() -> Value {
    json!({
        "x402Version": 2,
        "accepts": [
            {
                "scheme": "exact",
                "network": "eip155:8453",
                "asset": "USDC",
                "maxAmountRequired": "0",
                "payTo": "0x0000000000000000000000000000000000000000",
                "resource": crate::WEB_SEARCH_PATH,
                "description": "Brave Search web query",
                "mimeType": "application/json",
                "maxTimeoutSeconds": 60
            }
        ]
    })
}
