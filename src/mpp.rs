//! The MPP payment rail — everything specific to MPP lives here.
//!
//! See `x402.rs` for the x402 rail and `dispatch.rs` for the neutral router that
//! classifies each request and delegates to whichever rail it is paying on.
//!
//! MPP (the Machine Payments Protocol) pays in stablecoins on the Tempo chain.
//! Unlike x402 there is no facilitator service behind this rail: the `mpp` SDK
//! verifies a credential and settles it on Tempo in the same call.

use axum::{
    Json,
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;

use mpp::protocol::core::PaymentCredential;
use mpp::server::{ErrorCode, Mpp, TempoChargeMethod, TempoConfig, TempoProvider, tempo};

use crate::{AppError, Config};

/// MPP's `WWW-Authenticate` challenge value, using the `Payment` scheme (the client
/// replies with `Authorization: Payment <credential>`). Real params come from `mpp-rs`.
const CHALLENGE: &str = r#"Payment realm="bx402""#;

/// The realm named in every MPP challenge and echoed back in every credential.
const REALM: &str = "bx402";

/// The EVM treasury address that receives MPP payments (the challenge recipient).
const PAY_TO_EVM: &str = "0xbd9420A98a7Bd6B89765e5715e169481602D9c3d";

/// The concrete SDK handler behind [`Client`]: the Tempo charge method over the
/// SDK's own RPC provider, named once so signatures stay readable.
type Handler = Mpp<TempoChargeMethod<TempoProvider>>;

/// Returns `true` if the request carries an MPP credential (an `Authorization` header).
pub(crate) fn has_credential(headers: &HeaderMap) -> bool {
    headers.contains_key(header::AUTHORIZATION)
}

/// The MPP contribution to the cold `402`: the `WWW-Authenticate` response header
/// (name and challenge value) the client answers on the MPP rail. Out of the cold
/// `402` while the static value advertises no real charge.
#[expect(dead_code, reason = "the cold 402 does not advertise the MPP rail")]
pub(crate) fn challenge() -> (HeaderName, HeaderValue) {
    (
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static(CHALLENGE),
    )
}

/// The MPP payment handler, newtyped so the rest of the crate names this module's
/// type, not the SDK's, and `dispatch` can carry it as plain axum state.
#[derive(Clone)]
pub(crate) struct Client(Handler);

/// Build the MPP handler from config. A bad `MPP_RPC_URL` or unusable
/// `MPP_SECRET_KEY` is a startup misconfiguration, surfaced as [`AppError`]. The
/// Tempo chain and challenge currency both follow from the RPC URL.
pub(crate) fn client(config: &Config) -> Result<Client, AppError> {
    let builder = tempo(TempoConfig {
        recipient: PAY_TO_EVM,
    })
    .rpc_url(&config.mpp_rpc_url)
    .realm(REALM)
    .secret_key(&config.mpp_secret_key);
    Handler::create(builder)
        .map(Client)
        .map_err(|err| AppError::InvalidConfig(format!("MPP: {err}")))
}

/// Drive the MPP pay flow for a request that carries a credential.
///
/// MPP has no settle-free verification: `verify_credential` checks the signed
/// transfer and settles it on Tempo in one call, so the charge lands before the
/// search runs:
///
/// * credential missing, malformed, not a signed transaction, or rejected: `402`,
///   before any upstream call.
/// * Tempo RPC unreachable: `502`.
/// * verified and settled: the search runs.
///
/// A credential only verifies against a challenge this service issued: the SDK
/// recomputes the challenge id (an HMAC under our secret key) and checks the
/// echoed charge against the bound currency and recipient.
pub(crate) async fn handle(Client(handler): Client, req: Request, next: Next) -> Response {
    let Some(credential) = credential(req.headers()) else {
        return payment_rejected();
    };

    if !pays_by_transaction(&credential) {
        return payment_rejected();
    }

    // A Tempo RPC we cannot reach is our failure, not the client's, so it is a 502
    // rather than a 402.
    match handler.verify_credential(&credential).await {
        Ok(_) => next.run(req).await,
        Err(err) if err.code == Some(ErrorCode::NetworkError) => {
            tracing::error!(error = ?err, "mpp verify failed: tempo rpc unreachable");
            gateway_error("payment network unavailable")
        }
        Err(_) => payment_rejected(),
    }
}

/// Returns `true` when the credential pays with a signed transaction that this
/// service broadcasts during verification. A hash credential says the client
/// already broadcast the transfer itself, settling before anything was checked,
/// so it does not pay here.
fn pays_by_transaction(credential: &PaymentCredential) -> bool {
    credential
        .charge_payload()
        .is_ok_and(|payload| payload.is_transaction())
}

/// Parse the MPP credential from the `Authorization` header. Returns `None` if the
/// header is absent, not UTF-8, or not the `Payment <credential>` form.
fn credential(headers: &HeaderMap) -> Option<PaymentCredential> {
    let header = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    PaymentCredential::from_header(header).ok()
}

/// A `402` refusing the payment. Every refusal carries the same message, so a
/// missing, malformed, non-transaction, and rejected credential all read alike.
fn payment_rejected() -> Response {
    (
        StatusCode::PAYMENT_REQUIRED,
        Json(json!({ "error": "mpp payment did not verify" })),
    )
        .into_response()
}

/// A `502` for a payment we could not verify because Tempo was unreachable.
fn gateway_error(detail: &str) -> Response {
    (StatusCode::BAD_GATEWAY, Json(json!({ "error": detail }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(rpc_url: &str) -> Config {
        Config {
            brave_search_api_key: "key".to_string(),
            brave_search_api_base_url: "http://upstream.invalid".to_string(),
            x402_facilitator_url: "http://facilitator.invalid".to_string(),
            mpp_rpc_url: rpc_url.to_string(),
            mpp_secret_key: "test-secret".to_string(),
            restricted_address_s3_bucket: None,
        }
    }

    #[test]
    fn client_rejects_an_unparseable_rpc_url() {
        let err = client(&test_config("not a url")).map(|_| ()).unwrap_err();
        assert!(matches!(err, AppError::InvalidConfig(_)));
    }

    /// A minimal challenge echo; the payload gate reads only the payload beside it.
    fn echo() -> mpp::protocol::core::ChallengeEcho {
        mpp::protocol::core::ChallengeEcho {
            id: "id".into(),
            realm: REALM.into(),
            method: "tempo".into(),
            intent: "charge".into(),
            request: mpp::protocol::core::Base64UrlJson::from_raw("e30"),
            expires: None,
            digest: None,
            opaque: None,
        }
    }

    #[test]
    fn only_a_signed_transaction_payload_pays() {
        use mpp::protocol::core::PaymentPayload;

        let cases = [
            (
                "transaction",
                json!(PaymentPayload::transaction("0xsigned")),
                true,
            ),
            ("hash", json!(PaymentPayload::hash("0xhash")), false),
            ("proof", json!(PaymentPayload::proof("0xsig")), false),
            ("arbitrary json", json!({ "type": "mystery" }), false),
        ];
        for (name, payload, expected) in cases {
            let credential = PaymentCredential::new(echo(), payload);
            assert_eq!(pays_by_transaction(&credential), expected, "case: {name}");
        }
    }

    #[test]
    fn credential_requires_the_payment_scheme() {
        let cases = [
            ("bearer token", "Bearer abc123"),
            ("payment but not a credential", "Payment not-base64-json"),
            ("empty", ""),
        ];
        for (name, value) in cases {
            let headers: HeaderMap = [(header::AUTHORIZATION, value.parse().unwrap())]
                .into_iter()
                .collect();
            assert!(credential(&headers).is_none(), "case: {name}");
        }
    }
}
