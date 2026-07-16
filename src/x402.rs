//! The x402 payment rail: everything specific to x402 lives here.
//!
//! See `mpp.rs` for the MPP rail and `dispatch.rs` for the neutral router that
//! classifies each request and delegates to whichever rail it is paying on.

use axum::{
    extract::Request,
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header::HeaderName},
    middleware::Next,
    response::Response,
};

use serde_json::{Value, json};
use x402_axum::facilitator_client::FacilitatorClient;

use crate::error::json_error;
use crate::screener::RestrictedAddressScreener;
use crate::{AppError, Config};
use x402_chain_eip155::{KnownNetworkEip155, V2Eip155Exact, chain::ChecksummedAddress};
use x402_types::{
    networks::USDC,
    proto::{self, v2},
    util::Base64Bytes,
};

/// x402 V2 carries its payment proof in the `PAYMENT-SIGNATURE` request header.
/// V1's `X-PAYMENT` is deliberately not recognized: the service is V2-only, so a
/// V1 client carries no payment we accept and falls through to the cold `402`.
const V2_PAYMENT_HEADER: &str = "payment-signature";

/// x402 V2 returns the settlement receipt in the `Payment-Response` response header
/// as base64-encoded JSON, the dual of the `PAYMENT-SIGNATURE` request header.
const PAYMENT_RECEIPT_HEADER: &str = "payment-response";

/// The EVM treasury address that receives x402 payments (`payTo`).
const PAY_TO_EVM: &str = "0xbd9420A98a7Bd6B89765e5715e169481602D9c3d";

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
    let pay_to: ChecksummedAddress = PAY_TO_EVM.parse().expect("PAY_TO_EVM is a valid address");
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
        extensions: Default::default(),
    };
    serde_json::to_value(body).expect("PaymentRequired envelope serializes")
}

/// The x402 facilitator client, newtyped so the rest of the crate names this module's
/// type, not the SDK's, and `dispatch` can carry it as plain axum state.
///
/// Returning `impl Facilitator + …` would hide the SDK just as well, but axum state
/// must be a type we can name. The way to name an opaque type is a TAIT alias
/// (`type Client = impl Facilitator + …`), and TAIT is not stable on our pinned
/// toolchain, so a concrete newtype it is.
#[derive(Clone)]
pub(crate) struct Client(FacilitatorClient);

/// Build the x402 facilitator client from config. A bad `X402_FACILITATOR_URL` is a
/// startup misconfiguration, surfaced as [`AppError`].
pub(crate) fn client(config: &Config) -> Result<Client, AppError> {
    FacilitatorClient::try_from(config.x402_facilitator_url.as_str())
        .map(Client)
        .map_err(|err| AppError::InvalidConfig(format!("X402_FACILITATOR_URL: {err}")))
}

/// Drive the x402 pay flow for a request that carries a payment proof: verify, run
/// the search, then settle, each step gating the next. It fails closed, so a caller
/// is never charged for a response they don't get nor served one they didn't pay for:
///
/// * payment missing, malformed, or rejected: `402`, before any upstream call.
/// * facilitator unreachable on verify: `502`.
/// * search fails (4xx or 5xx): relayed as is, settlement skipped.
/// * settlement fails: `502`, the response body withheld.
pub(crate) async fn handle(
    Client(facilitator_client): Client,
    screener: Option<RestrictedAddressScreener>,
    req: Request,
    next: Next,
) -> Response {
    let Some((request, payer)) = decode_payment(req.headers()) else {
        return payment_rejected("malformed x402 payment payload");
    };

    // Screen the payer before any facilitator or upstream call, so a blocked signer
    // touches neither.
    if let Some(screener) = &screener
        && let Err(denied) = screener
            .require_allowed(payer, payment_rejected(GENERIC_REJECTION))
            .await
    {
        return denied;
    }

    // Verify before doing any work. A facilitator we cannot reach is our failure,
    // not the client's, so it is a 502 rather than a 402.
    match facilitator_client.verify(&request).await {
        Ok(response) if is_valid(&response) => {}
        Ok(_) => return payment_rejected(GENERIC_REJECTION),
        Err(err) => {
            tracing::error!(error = ?err, "x402 facilitator verify failed");
            return gateway_error("payment facilitator unavailable");
        }
    }

    let response = next.run(req).await;
    if !response.status().is_success() {
        return response;
    }

    // `SettleRequest` is an alias of `VerifyRequest`, so the value we verified
    // settles unchanged. Withhold the (already produced) body unless it settles.
    match facilitator_client.settle(&request).await {
        Ok(receipt) if settled(&receipt) => attach_receipt(response, &receipt),
        Ok(receipt) => {
            tracing::error!(?receipt, "x402 facilitator reported settlement failure");
            gateway_error(SETTLE_FAILED)
        }
        Err(err) => {
            tracing::error!(error = ?err, "x402 facilitator settle failed");
            gateway_error(SETTLE_FAILED)
        }
    }
}

/// Shared message for every refused payment, so refusals are indistinguishable.
const GENERIC_REJECTION: &str = "x402 payment did not verify";

/// Shared message for a payment we could not settle, whether the facilitator declined it
/// or was unreachable, so the client cannot tell the two apart.
const SETTLE_FAILED: &str = "x402 payment could not be settled";

/// Decode the client's base64 JSON payment payload from `PAYMENT-SIGNATURE` into the
/// facilitator's verify/settle request (wrapping it with our advertised
/// [`requirements`]) and the payer to screen. Returns `None` if the header is absent or
/// not the base64 JSON required.
///
/// The payer is `Some` only for the eip3009 payload we advertise; a payload without
/// `authorization.from` (e.g. a permit2 shape) yields `None`, which the caller rejects
/// before any facilitator call when screening is on.
fn decode_payment(headers: &HeaderMap) -> Option<(proto::VerifyRequest, Option<String>)> {
    let header = headers.get(V2_PAYMENT_HEADER)?.to_str().ok()?;
    let decoded = Base64Bytes::from(header.as_bytes()).decode().ok()?;
    let payload: Value = serde_json::from_slice(&decoded).ok()?;
    let payer = payer_address(&payload);
    let body = json!({
        "x402Version": 2,
        "paymentPayload": payload,
        "paymentRequirements": requirements(),
    });
    let raw = serde_json::value::to_raw_value(&body).ok()?;
    Some((proto::VerifyRequest::from(raw), payer))
}

/// The payer to screen: the eip3009 `authorization.from`, lowercased to the screener's
/// canonical form (EVM addresses are case-insensitive hex). `None` when the payload has
/// no such field.
fn payer_address(payload: &Value) -> Option<String> {
    let from = payload
        .get("payload")?
        .get("authorization")?
        .get("from")?
        .as_str()?;
    Some(from.to_ascii_lowercase())
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

/// A settle response confirms settlement when `success` is true. Like the verify
/// response, the SDK returns the `POST /settle` body as an untyped `serde_json::Value`,
/// so we read the field by key. Shape:
///
/// ```json
/// { "success": true,  "payer": "0x09e0…2c1f", "transaction": "0x4f2b…9ad3", "network": "eip155:84532" }
/// { "success": false, "error_reason": "insufficient_funds", "network": "eip155:84532" }
/// ```
fn settled(response: &proto::SettleResponse) -> bool {
    response.0.get("success").and_then(Value::as_bool) == Some(true)
}

/// Attach the settlement receipt as the base64 `Payment-Response` header the client
/// reads back, leaving the response body untouched.
fn attach_receipt(mut response: Response, receipt: &proto::SettleResponse) -> Response {
    let encoded = Base64Bytes::encode(receipt.0.to_string());
    if let Ok(value) = HeaderValue::from_str(&encoded.to_string()) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(PAYMENT_RECEIPT_HEADER), value);
    }
    response
}

/// A `402` telling the client their x402 payment was missing, malformed, or rejected.
fn payment_rejected(detail: &str) -> Response {
    json_error(StatusCode::PAYMENT_REQUIRED, detail)
}

/// A `502` for a payment we could neither verify nor settle through the facilitator.
fn gateway_error(detail: &str) -> Response {
    json_error(StatusCode::BAD_GATEWAY, detail)
}

/// Decode a base64 `Payment-Response` receipt back to JSON, the test-side inverse of
/// [`attach_receipt`], so the crate's tests read receipts through this module too.
#[cfg(test)]
pub(crate) fn decode_receipt(encoded: &str) -> Value {
    let bytes = Base64Bytes::from(encoded.as_bytes())
        .decode()
        .expect("receipt is valid base64");
    serde_json::from_slice(&bytes).expect("receipt is JSON")
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
        assert_eq!(reqs.pay_to, PAY_TO_EVM);
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
                "payTo": "0xbd9420A98a7Bd6B89765e5715e169481602D9c3d",
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
