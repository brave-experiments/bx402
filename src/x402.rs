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
use x402_chain_eip155::{
    KnownNetworkEip155, V2Eip155Exact,
    chain::{ChecksummedAddress, Eip155TokenDeployment},
};
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

/// Build the list of payment offers we advertise and verify against. The same
/// entries seed both the cold `402` body and payment verification, so there is
/// one source of truth for what we charge: real USDC on Base mainnet, plus
/// faucet USDC on Base Sepolia when the testnet is allowed.
fn accepts(config: &Config) -> Result<Vec<v2::PaymentRequirements>, AppError> {
    let pay_to: ChecksummedAddress = PAY_TO_EVM
        .parse()
        .map_err(|err| AppError::InvalidConfig(format!("x402 payTo: {err}")))?;
    let offer = |usdc: Eip155TokenDeployment| {
        V2Eip155Exact::price_tag(pay_to, usdc.amount(PRICE_USDC_BASE_UNITS)).requirements
    };
    let mut accepts = vec![offer(USDC::base())];
    if config.allow_testnet {
        accepts.push(offer(USDC::base_sepolia()));
    }
    Ok(accepts)
}

/// The error line every cold `402` carries, in the envelope and in the
/// fallback body alike.
const PAYMENT_REQUIRED: &str = "Payment required";

/// Label for each paid endpoint, keyed by request path.
fn endpoint_description(path: &str) -> &'static str {
    match path {
        crate::WEB_SEARCH_PATH => "Brave Search API - Web / Search",
        _ => "Brave Search API",
    }
}

/// x402's part of the cold `402`: the V2 `PaymentRequired` envelope for `resource`.
pub(crate) fn challenge(client: &Client, resource: &str) -> Value {
    let uri = resource.parse::<Uri>().ok();
    let path = uri.as_ref().map(|u| u.path()).unwrap_or(resource);
    let body = v2::PaymentRequired {
        x402_version: v2::X402Version2,
        error: Some(PAYMENT_REQUIRED.to_string()),
        resource: Some(v2::ResourceInfo {
            url: resource.to_string(),
            description: Some(endpoint_description(path).to_string()),
            mime_type: Some("application/json".to_string()),
        }),
        accepts: client.accepts.clone(),
        extensions: Default::default(),
    };
    serde_json::to_value(body).unwrap_or_else(|err| {
        tracing::error!(error = %err, "x402 challenge could not be serialized");
        json!({ "error": PAYMENT_REQUIRED })
    })
}

/// The x402 facilitator client and the payment offers we accept, wrapped so the
/// rest of the crate names this module's type, not the SDK's, and `dispatch`
/// can carry it as plain axum state.
///
/// Returning `impl Facilitator + …` would hide the SDK just as well, but axum state
/// must be a type we can name. The way to name an opaque type is a TAIT alias
/// (`type Client = impl Facilitator + …`), and TAIT is not stable on our pinned
/// toolchain, so a concrete struct it is.
#[derive(Clone)]
pub(crate) struct Client {
    facilitator: FacilitatorClient,
    /// Built once at startup. The cold `402` advertises exactly these entries
    /// and a payment must accept one of them verbatim, so the two can never
    /// disagree.
    accepts: Vec<v2::PaymentRequirements>,
}

/// Build the x402 facilitator client from config. A bad `X402_FACILITATOR_URL` is a
/// startup misconfiguration, surfaced as [`AppError`].
pub(crate) fn client(config: &Config) -> Result<Client, AppError> {
    let facilitator = FacilitatorClient::try_from(config.x402_facilitator_url.as_str())
        .map_err(|err| AppError::InvalidConfig(format!("X402_FACILITATOR_URL: {err}")))?;
    Ok(Client {
        facilitator,
        accepts: accepts(config)?,
    })
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
    client: Client,
    screener: Option<RestrictedAddressScreener>,
    req: Request,
    next: Next,
) -> Response {
    let Some((request, payer)) = decode_payment(req.headers(), &client.accepts) else {
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
    match client.facilitator.verify(&request).await {
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
    match client.facilitator.settle(&request).await {
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
/// facilitator's verify/settle request and the payer to screen. Returns `None` if the
/// header is absent, not the base64 JSON required, or accepts an offer we do not
/// advertise.
///
/// The payload names the offer the payer accepted. Only an entry from `accepts`,
/// matched verbatim, is passed on for verification, so a payer cannot write their
/// own price, asset, or recipient.
///
/// The payer is `Some` only for the eip3009 payload we advertise; a payload without
/// `authorization.from` (e.g. a permit2 shape) yields `None`, which the caller rejects
/// before any facilitator call when screening is on.
fn decode_payment(
    headers: &HeaderMap,
    accepts: &[v2::PaymentRequirements],
) -> Option<(proto::VerifyRequest, Option<String>)> {
    let header = headers.get(V2_PAYMENT_HEADER)?.to_str().ok()?;
    let decoded = Base64Bytes::from(header.as_bytes()).decode().ok()?;
    let payload: Value = serde_json::from_slice(&decoded).ok()?;
    let chosen = payload.get("accepted")?;
    let accepted = accepts
        .iter()
        .find(|entry| serde_json::to_value(entry).ok().as_ref() == Some(chosen))?;
    let payer = payer_address(&payload);
    let body = json!({
        "x402Version": 2,
        "paymentPayload": payload,
        "paymentRequirements": accepted,
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

/// The `PAYMENT-SIGNATURE` value for a payment accepting the first offer built
/// from `config`, carrying `payload` as the scheme payload. The test-side
/// counterpart of [`decode_payment`].
#[cfg(test)]
pub(crate) fn test_payment_signature(config: &Config, payload: Value) -> String {
    let entries = accepts(config).expect("the test config advertises an offer");
    let body = json!({ "accepted": &entries[0], "payload": payload });
    Base64Bytes::encode(body.to_string()).to_string()
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
    fn without_the_testnet_flag_only_mainnet_is_offered() {
        let production = Config {
            allow_testnet: false,
            ..Config::for_tests()
        };
        let entries = accepts(&production).unwrap();
        let [only] = entries.as_slice() else {
            panic!("a production config offers only mainnet");
        };
        assert_eq!(only.network.to_string(), "eip155:8453"); // Base mainnet
    }

    /// A header map carrying `payload` as the base64 `PAYMENT-SIGNATURE`.
    fn payment_headers(payload: &Value) -> HeaderMap {
        let encoded = Base64Bytes::encode(payload.to_string());
        let mut headers = HeaderMap::new();
        headers.insert(V2_PAYMENT_HEADER, encoded.to_string().parse().unwrap());
        headers
    }

    #[test]
    fn decode_accepts_only_an_advertised_offer() {
        let entries = accepts(&Config::for_tests()).unwrap();

        let advertised = json!({ "accepted": &entries[0] });
        assert!(decode_payment(&payment_headers(&advertised), &entries).is_some());

        // A tampered offer (here, a self-granted discount) is refused.
        let mut discounted = serde_json::to_value(&entries[0]).unwrap();
        discounted["amount"] = json!("1");
        let tampered = json!({ "accepted": discounted });
        assert!(decode_payment(&payment_headers(&tampered), &entries).is_none());

        // So is a payload naming no offer at all.
        assert!(decode_payment(&payment_headers(&json!({})), &entries).is_none());
    }

    #[test]
    fn challenge_emits_the_full_payment_required_payload() {
        // Every offer field is spelled out so an upstream change to the SDK's
        // consts fails here instead of silently moving the charge.
        let client = client(&Config::for_tests()).unwrap();
        let body = challenge(&client, "https://bx402.example.com/res/v1/web/search");

        let expected = json!({
            "x402Version": 2,
            "error": "Payment required",
            "resource": {
                "url": "https://bx402.example.com/res/v1/web/search",
                "description": "Brave Search API - Web / Search",
                "mimeType": "application/json"
            },
            "accepts": [
                {
                    "scheme": "exact",
                    "network": "eip155:8453",
                    "amount": "5000",
                    "asset": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
                    "payTo": "0xbd9420A98a7Bd6B89765e5715e169481602D9c3d",
                    "maxTimeoutSeconds": 300,
                    "extra": {
                        "assetTransferMethod": "eip3009",
                        "name": "USD Coin",
                        "version": "2"
                    }
                },
                {
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
                }
            ]
        });

        assert_eq!(body, expected);
    }
}
