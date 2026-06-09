//! The x402 payment rail: everything specific to x402 lives here.
//!
//! See `mpp.rs` for the MPP rail and `dispatch.rs` for the neutral router that
//! classifies each request and delegates to whichever rail it is paying on.

use axum::{
    Json,
    extract::Request,
    http::{HeaderMap, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use x402_axum::facilitator_client::FacilitatorClient;

use crate::{AppError, Config};
use x402_chain_eip155::{KnownNetworkEip155, V2Eip155Exact, chain::ChecksummedAddress};
use x402_types::{
    facilitator::Facilitator,
    networks::USDC,
    proto::{self, v2},
    util::Base64Bytes,
};

/// x402 V2 carries its payment proof in the `PAYMENT-SIGNATURE` request header.
/// V1's `X-PAYMENT` is deliberately not recognized: the service is V2-only, so a
/// V1 client carries no payment we accept and falls through to the cold `402`.
const V2_PAYMENT_HEADER: &str = "payment-signature";

/// The treasury address that receives x402 payments (`payTo`). The zero address
/// is a placeholder; set a real treasury before settling on a live network.
const PAY_TO: &str = "0x0000000000000000000000000000000000000000";

/// Flat price per request, in USDC base units (6 decimals, so `5_000` = 0.005 USDC).
/// One rate for every request today; pricing may later vary by endpoint or by rail.
const PRICE_USDC_BASE_UNITS: u64 = 5_000;

/// Returns `true` if the request carries an x402 V2 payment proof.
pub(crate) fn has_payment(headers: &HeaderMap) -> bool {
    headers.contains_key(V2_PAYMENT_HEADER)
}

/// Build the x402 V2 payment requirements we advertise and verify against. The same
/// value seeds both the cold `402` body and payment verification at a later stage,
/// so there is one source of truth for what we charge.
fn requirements() -> v2::PaymentRequirements {
    let pay_to: ChecksummedAddress = PAY_TO.parse().expect("PAY_TO is a valid address");
    let usdc = USDC::base_sepolia();
    V2Eip155Exact::price_tag(pay_to, usdc.amount(PRICE_USDC_BASE_UNITS)).requirements
}

/// Label for each paid endpoint, keyed by request path.
fn endpoint_description(path: &str) -> &'static str {
    match path {
        crate::WEB_SEARCH_PATH => "Brave Search API - Web / Search",
        _ => "Brave Search API",
    }
}

/// x402's part of the cold `402`: the V2 `PaymentRequired` envelope for `resource`.
pub(crate) fn challenge(resource: &str) -> Value {
    let uri = resource.parse::<Uri>().ok();
    let path = uri.as_ref().map(|u| u.path()).unwrap_or(resource);
    let body = v2::PaymentRequired {
        x402_version: v2::X402Version2,
        error: Some("Payment required".to_string()),
        resource: Some(v2::ResourceInfo {
            url: resource.to_string(),
            description: Some(endpoint_description(path).to_string()),
            mime_type: Some("application/json".to_string()),
        }),
        accepts: vec![requirements()],
    };
    serde_json::to_value(body).expect("PaymentRequired envelope serializes")
}

/// The x402 facilitator client as a single crate-local bound: the SDK's
/// [`Facilitator`] interface plus the `Clone + Send + Sync + 'static` the axum layer
/// needs. A blanket impl means any real facilitator (and the wiremock-backed client
/// in tests) satisfies it, and the rest of the crate names this instead of the SDK
/// trait.
pub(crate) trait Client: Facilitator + Clone + Send + Sync + 'static {}
impl<F> Client for F where F: Facilitator + Clone + Send + Sync + 'static {}

/// Build the real x402 facilitator client from config, returned as an opaque
/// [`Client`] so the rest of the crate depends on this module, not the SDK. A bad
/// `X402_FACILITATOR_URL` is a startup misconfiguration, surfaced as [`AppError`].
pub(crate) fn client(config: &Config) -> Result<impl Client, AppError> {
    FacilitatorClient::try_from(config.x402_facilitator_url.as_str())
        .map_err(|err| AppError::InvalidConfig(format!("X402_FACILITATOR_URL: {err}")))
}

/// Drive the x402 pay flow for a request that carries a payment proof: verify the
/// payment, then run the search. A payment that does not verify is rejected before any
/// upstream call:
///
/// * payment missing, malformed, or rejected: `402`, before any upstream call.
/// * facilitator unreachable on verify: `502`.
pub(crate) async fn handle<F: Client>(client: F, req: Request, next: Next) -> Response {
    let request = match verify_request(req.headers()) {
        Some(request) => request,
        None => return payment_rejected("malformed x402 payment payload"),
    };

    // Verify before doing any work. A facilitator we cannot reach is our failure,
    // not the client's, so it is a 502 rather than a 402.
    match client.verify(&request).await {
        Ok(response) if is_valid(&response) => {}
        Ok(_) => return payment_rejected("x402 payment did not verify"),
        Err(_) => return gateway_error("payment facilitator unavailable"),
    }

    next.run(req).await
}

/// Decode the client's base64 JSON payment payload from `PAYMENT-SIGNATURE` and wrap
/// it with our advertised [`requirements`] into the facilitator's verify request.
/// Returns `None` if the header is absent or not the base64 JSON required.
fn verify_request(headers: &HeaderMap) -> Option<proto::VerifyRequest> {
    let header = headers.get(V2_PAYMENT_HEADER)?.to_str().ok()?;
    let decoded = Base64Bytes::from(header.as_bytes()).decode().ok()?;
    let payload: Value = serde_json::from_slice(&decoded).ok()?;
    let body = json!({
        "x402Version": 2,
        "paymentPayload": payload,
        "paymentRequirements": requirements(),
    });
    let raw = serde_json::value::RawValue::from_string(body.to_string()).ok()?;
    Some(proto::VerifyRequest::from(raw))
}

/// A verify response confirms the payment when `isValid` is true. The SDK returns the
/// `POST /verify` body as an untyped `serde_json::Value` (not a struct), so we read the
/// field by key. Shape:
///
/// ```json
/// { "isValid": true,  "payer": "0x09e0…2c1f" }
/// { "isValid": false, "invalidReason": "insufficient_funds", "payer": "0x09e0…2c1f" }
/// ```
fn is_valid(response: &proto::VerifyResponse) -> bool {
    response.0.get("isValid").and_then(Value::as_bool) == Some(true)
}

/// A `402` telling the client their x402 payment was missing, malformed, or rejected.
fn payment_rejected(detail: &str) -> Response {
    (
        StatusCode::PAYMENT_REQUIRED,
        Json(json!({ "error": detail })),
    )
        .into_response()
}

/// A `502` for a payment we could not verify through the facilitator.
fn gateway_error(detail: &str) -> Response {
    (StatusCode::BAD_GATEWAY, Json(json!({ "error": detail }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requirements_match_the_pricing_constants() {
        let reqs = requirements();

        assert_eq!(reqs.scheme, "exact");
        assert_eq!(reqs.network.to_string(), "eip155:84532"); // Base Sepolia
        assert_eq!(reqs.amount, PRICE_USDC_BASE_UNITS.to_string()); // decimal base units
        assert_eq!(reqs.pay_to, PAY_TO);
    }

    #[test]
    fn challenge_emits_the_full_payment_required_payload() {
        let body = challenge("https://bx402.example.com/res/v1/web/search");

        let expected = json!({
            "x402Version": 2,
            "error": "Payment required",
            "resource": {
                "url": "https://bx402.example.com/res/v1/web/search",
                "description": "Brave Search API - Web / Search",
                "mimeType": "application/json"
            },
            "accepts": [{
                "scheme": "exact",
                "network": "eip155:84532",
                "amount": "5000",
                "asset": "0x036CbD53842c5426634e7929541eC2318f3dCF7e",
                "payTo": "0x0000000000000000000000000000000000000000",
                "maxTimeoutSeconds": 300,
                "extra": {
                    "assetTransferMethod": "eip3009",
                    "name": "USDC",
                    "version": "2"
                }
            }]
        });

        assert_eq!(body, expected);
    }
}
