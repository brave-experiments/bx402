//! The MPP payment rail — everything specific to MPP lives here.
//!
//! See `x402.rs` for the x402 rail and `dispatch.rs` for the neutral router that
//! classifies each request and delegates to whichever rail it is paying on.
//!
//! MPP (the Machine Payments Protocol) pays in stablecoins on the Tempo chain.
//! Unlike x402 there is no facilitator service behind this rail: the `mpp` SDK
//! verifies a credential and settles it on Tempo in the same call.

use axum::{
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    middleware::Next,
    response::Response,
};
use serde_json::json;

use alloy_primitives::Bytes;
use mpp::protocol::core::{PaymentCredential, PaymentPayload, Receipt};
use mpp::protocol::intents::ChargeRequest;
use mpp::protocol::methods::tempo::TEMPO_TX_TYPE_ID;
use mpp::server::{ErrorCode, Mpp, TempoChargeMethod, TempoConfig, TempoProvider, tempo};
use tempo_primitives::transaction::AASigned;

use crate::error::{json_error, service_unavailable};
use crate::screener::{RestrictedAddressScreener, Screening};
use crate::{AppError, Config};

/// The settlement receipt returns to the client in the `Payment-Receipt` response
/// header, the dual of the `Authorization` request header.
const PAYMENT_RECEIPT_HEADER: &str = "payment-receipt";

/// The realm named in every MPP challenge and echoed back in every credential.
const REALM: &str = "bx402";

/// The EVM treasury address that receives MPP payments (the challenge recipient).
const PAY_TO_EVM: &str = "0xbd9420A98a7Bd6B89765e5715e169481602D9c3d";

/// Flat price per request in base units of the challenge currency (6 decimals, so
/// `5_000` = 0.005). The currency follows the configured network: pathUSD on the
/// Moderato testnet, USDC on Tempo mainnet, each worth one dollar. The x402 rail
/// charges the same 0.005 through its own `PRICE_USDC_BASE_UNITS`; a price change
/// edits both consts.
const PRICE_USD_BASE_UNITS: u64 = 5_000;

/// The concrete SDK handler behind [`Client`]: the Tempo charge method over the
/// SDK's own RPC provider, named once so signatures stay readable.
type Handler = Mpp<TempoChargeMethod<TempoProvider>>;

/// Returns `true` if the request carries an MPP credential (an `Authorization` header).
pub(crate) fn has_credential(headers: &HeaderMap) -> bool {
    headers.contains_key(header::AUTHORIZATION)
}

/// The MPP contribution to the cold `402`: a fresh `WWW-Authenticate: Payment`
/// challenge carrying the charge a credential must answer (the `Payment` scheme;
/// the client replies with `Authorization: Payment <credential>`). Minted per
/// request because every challenge is HMAC-signed and expires. `None` if the
/// challenge cannot be built or encoded, leaving the `402` advertising x402 alone.
pub(crate) fn challenge(client: &Client) -> Option<(HeaderName, HeaderValue)> {
    let value = client
        .handler
        .charge_challenge_with_options(&client.charge, None, None)
        .and_then(|challenge| challenge.to_header())
        .ok()
        .and_then(|value| HeaderValue::from_str(&value).ok());
    let Some(value) = value else {
        tracing::error!("mpp challenge could not be built");
        return None;
    };
    Some((header::WWW_AUTHENTICATE, value))
}

/// The MPP payment handler and the charge it collects. A wrapper, so the rest
/// of the crate names this module's type rather than the SDK's, and `dispatch`
/// can carry it as plain axum state.
#[derive(Clone)]
pub(crate) struct Client {
    handler: Handler,
    /// Built once at startup. Challenges advertise this exact value and
    /// credentials are verified against it, so the two can never disagree.
    charge: ChargeRequest,
}

/// A bad `MPP_RPC_URL` or unusable `MPP_SECRET_KEY` is a startup
/// misconfiguration, surfaced as [`AppError`]. The Tempo chain and challenge
/// currency both follow from the RPC URL.
pub(crate) fn client(config: &Config) -> Result<Client, AppError> {
    let builder = tempo(TempoConfig {
        recipient: PAY_TO_EVM,
    })
    .rpc_url(&config.mpp_rpc_url)
    .realm(REALM)
    .secret_key(&config.mpp_secret_key);
    let handler =
        Handler::create(builder).map_err(|err| AppError::InvalidConfig(format!("MPP: {err}")))?;
    let currency = handler
        .currency()
        .ok_or_else(|| AppError::InvalidConfig("MPP: no currency bound to the handler".into()))?;
    let charge = ChargeRequest {
        amount: PRICE_USD_BASE_UNITS.to_string(),
        currency: currency.to_string(),
        recipient: handler.recipient().map(str::to_string),
        method_details: handler.chain_id().map(|id| json!({ "chainId": id })),
        ..Default::default()
    };
    Ok(Client { handler, charge })
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
/// * verified and settled: the search runs, and the response carries the
///   `Payment-Receipt` header whatever its status, because the payment has
///   already settled.
///
/// A credential only verifies against a challenge this service issued: the SDK
/// recomputes the challenge id (an HMAC under our secret key) and checks the
/// echoed charge against [`Client::charge`], so a credential minted for another
/// amount, currency, or recipient is refused.
pub(crate) async fn handle(
    client: Client,
    screener: Option<RestrictedAddressScreener>,
    req: Request,
    next: Next,
) -> Response {
    let Some(credential) = credential(req.headers()) else {
        return payment_rejected();
    };

    let Some(payload) = transaction_payload(&credential) else {
        return payment_rejected();
    };

    // Screen the transfer's signer before verification, so a blocked payer's
    // transaction is never broadcast and no funds move. Without a screener there
    // is nothing to consult, and the signer is not recovered at all.
    if let Some(screener) = &screener
        && let Some(denied) = screen_signer(screener, signer_address(&payload)).await
    {
        return denied;
    }

    // A Tempo RPC we cannot reach is our failure, not the client's, so it is a 502
    // rather than a 402.
    let receipt = match client
        .handler
        .verify_credential_with_expected_request(&credential, &client.charge)
        .await
    {
        Ok(receipt) => receipt,
        Err(err) if err.code == Some(ErrorCode::NetworkError) => {
            tracing::error!(error = ?err, "mpp verify failed: tempo rpc unreachable");
            return gateway_error();
        }
        Err(_) => return payment_rejected(),
    };

    // The payment has already settled, so the receipt rides on whatever the search
    // returns: the client paid and gets their proof either way.
    attach_receipt(next.run(req).await, &receipt)
}

/// Attach the settlement receipt as the `Payment-Receipt` header the client reads
/// back, leaving the response body untouched.
fn attach_receipt(mut response: Response, receipt: &Receipt) -> Response {
    let value = receipt
        .to_header()
        .ok()
        .and_then(|value| HeaderValue::from_str(&value).ok());
    let Some(value) = value else {
        tracing::error!("mpp settlement receipt could not be encoded as a header");
        return response;
    };
    response
        .headers_mut()
        .insert(HeaderName::from_static(PAYMENT_RECEIPT_HEADER), value);
    response
}

/// Screen the transaction's signer; returns the refusal to send, or `None` to proceed:
///
/// - allowed: `None`
/// - blocked, or no signer to screen: generic `402`
/// - could not screen (error or timeout): `503`
async fn screen_signer(
    screener: &RestrictedAddressScreener,
    signer: Option<String>,
) -> Option<Response> {
    let Some(signer) = signer else {
        return Some(payment_rejected());
    };
    match screener.screen(&signer).await {
        Ok(Screening::Allowed) => None,
        Ok(Screening::Blocked) => Some(payment_rejected()),
        Err(err) => {
            tracing::error!(error = ?err, "signer screening failed");
            Some(service_unavailable())
        }
    }
}

/// The payer, recovered from the signed transaction's own signature: the address
/// the transfer draws from, independent of anything the credential envelope
/// claims. Lowercase 0x hex, the screener's canonical form for EVM addresses.
/// `None` when the payload does not carry a decodable signed Tempo transaction,
/// which verification would refuse anyway.
fn signer_address(payload: &PaymentPayload) -> Option<String> {
    let bytes = payload.signed_tx()?.parse::<Bytes>().ok()?;
    let tx_data = bytes.strip_prefix(&[TEMPO_TX_TYPE_ID]).unwrap_or(&bytes);
    let signed = AASigned::rlp_decode(&mut &tx_data[..]).ok()?;
    let signer = signed
        .signature()
        .recover_signer(&signed.signature_hash())
        .ok()?;
    Some(format!("{signer:#x}"))
}

/// The credential's payload, if it pays with a signed transaction that this
/// service broadcasts during verification. A hash credential says the client
/// already broadcast the transfer itself, settling before anything was checked,
/// so it does not pay here.
fn transaction_payload(credential: &PaymentCredential) -> Option<PaymentPayload> {
    let payload = credential.charge_payload().ok()?;
    payload.is_transaction().then_some(payload)
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
    json_error(StatusCode::PAYMENT_REQUIRED, "mpp payment did not verify")
}

/// A `502` for a payment we could not verify because Tempo was unreachable.
fn gateway_error() -> Response {
    json_error(StatusCode::BAD_GATEWAY, "payment network unavailable")
}

/// Mint the `Authorization` header value for a credential answering our own
/// challenge with `payload`: the test-side counterpart of [`credential`], using
/// the same client the app under test builds from `config`.
#[cfg(test)]
fn credential_header(config: &Config, payload: PaymentPayload) -> String {
    let client = client(config).expect("test config builds the mpp client");
    let challenge = client
        .handler
        .charge_challenge_with_options(&client.charge, None, None)
        .expect("the challenge builds");
    let credential = PaymentCredential::new(challenge.to_echo(), payload);
    mpp::protocol::core::format_authorization(&credential).expect("the credential formats")
}

/// A credential whose payload says the client already broadcast the transfer.
#[cfg(test)]
pub(crate) fn hash_credential_header(config: &Config) -> String {
    credential_header(config, PaymentPayload::hash("0xdeadbeef"))
}

/// A credential paying with the forged signed transaction. Returns the header and
/// the signer's lowercase address, so a test can put that exact address on the
/// restricted list.
#[cfg(test)]
pub(crate) async fn signed_transaction_credential_header(config: &Config) -> (String, String) {
    let (tx, signer) = forged_transaction().await;
    let header = credential_header(config, PaymentPayload::transaction(tx));
    (header, signer)
}

/// A minimal signed Tempo transaction from a fixed test key, as
/// `(transaction hex, signer address)`. The transfer is not a valid charge; it
/// only has to decode and carry a real signature. Signing at test time keeps the
/// bytes aligned with the current tempo-primitives encoding. The signer types
/// come from mpp's re-exports rather than a new dependency.
#[cfg(test)]
async fn forged_transaction() -> (String, String) {
    use alloy_primitives::{Address, B256, Signature, TxKind, U256, hex};
    use mpp::{PrivateKeySigner, Signer};
    use tempo_primitives::TempoTransaction;
    use tempo_primitives::transaction::{Call, TempoSignature};

    let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(0x01))
        .expect("the fixed test key is a valid secp256k1 scalar");
    // Recipient, chain, and gas are arbitrary: the transfer never verifies.
    let tx = TempoTransaction {
        chain_id: 42431,
        gas_limit: 100_000,
        calls: vec![Call {
            to: TxKind::Call(Address::repeat_byte(0x42)),
            value: U256::ZERO,
            input: Bytes::new(),
        }],
        ..Default::default()
    };

    // The signature hash covers only the transaction fields, so a placeholder
    // signature is enough to compute it.
    let placeholder = TempoSignature::from(Signature::new(U256::from(1), U256::from(1), false));
    let sig_hash = AASigned::new_unhashed(tx.clone(), placeholder).signature_hash();
    let signature = signer.sign_hash(&sig_hash).await.expect("signing succeeds");
    let signed = AASigned::new_unhashed(tx, TempoSignature::from(signature));

    let mut encoded = Vec::new();
    signed.eip2718_encode(&mut encoded);
    (
        hex::encode_prefixed(encoded),
        format!("{:#x}", signer.address()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    fn test_config(rpc_url: &str) -> Config {
        Config {
            mpp_rpc_url: rpc_url.to_string(),
            ..Config::for_tests()
        }
    }

    #[test]
    fn client_rejects_an_unparseable_rpc_url() {
        assert!(matches!(
            client(&test_config("not a url")),
            Err(AppError::InvalidConfig(_))
        ));
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
            assert_eq!(
                transaction_payload(&credential).is_some(),
                expected,
                "case: {name}"
            );
        }
    }

    #[test]
    fn challenge_advertises_the_charge_credentials_answer() {
        let client = client(&test_config("https://rpc.moderato.tempo.xyz")).unwrap();
        let (name, value) = challenge(&client).expect("the challenge builds");
        assert_eq!(name, header::WWW_AUTHENTICATE);

        // The header parses back to a signed, expiring challenge for the same
        // charge a credential is verified against.
        let parsed = mpp::protocol::core::parse_www_authenticate(value.to_str().unwrap()).unwrap();
        assert_eq!(parsed.realm, REALM);
        assert!(!parsed.id.is_empty());
        assert!(parsed.expires.is_some());

        let expected = &client.charge;
        let advertised: ChargeRequest = parsed.request.decode().unwrap();
        assert_eq!(advertised.amount, expected.amount);
        assert_eq!(advertised.currency, expected.currency);
        assert_eq!(advertised.recipient, expected.recipient);
        assert_eq!(advertised.method_details, expected.method_details);
    }

    #[test]
    fn the_charge_follows_the_network_and_pins_the_price() {
        // The SDK's default currency per network: pathUSD (the TIP-20 precompile) on
        // the Moderato testnet, USDC on Tempo mainnet.
        let cases = [
            (
                "https://rpc.moderato.tempo.xyz",
                "0x20c0000000000000000000000000000000000000",
                42431,
            ),
            (
                "https://rpc.tempo.xyz",
                "0x20C000000000000000000000b9537d11c60E8b50",
                4217,
            ),
        ];
        for (rpc_url, currency, chain_id) in cases {
            let request = client(&test_config(rpc_url)).unwrap().charge;

            assert_eq!(
                request.amount,
                PRICE_USD_BASE_UNITS.to_string(),
                "{rpc_url}"
            );
            assert_eq!(request.currency, currency, "{rpc_url}");
            assert_eq!(request.recipient.as_deref(), Some(PAY_TO_EVM), "{rpc_url}");
            assert_eq!(
                request.method_details,
                Some(json!({ "chainId": chain_id })),
                "{rpc_url}"
            );
        }
    }

    #[test]
    fn attached_receipt_parses_back_from_the_header() {
        let receipt = Receipt::success("tempo", "0xtxhash");
        let response = attach_receipt(().into_response(), &receipt);

        let header = response
            .headers()
            .get(PAYMENT_RECEIPT_HEADER)
            .expect("the receipt header is attached")
            .to_str()
            .unwrap();
        let parsed = Receipt::from_header(header).expect("the header parses back");
        assert!(parsed.is_success());
        assert_eq!(parsed.reference, "0xtxhash");
    }

    #[tokio::test]
    async fn signer_recovery_matches_the_signing_key() {
        // signer_address decodes the transaction independently of the SDK, so it
        // must recover exactly the key that signed it.
        let (tx, signer) = forged_transaction().await;
        assert_eq!(
            signer_address(&PaymentPayload::transaction(tx)).as_deref(),
            Some(signer.as_str())
        );
    }

    #[test]
    fn signer_recovery_requires_a_decodable_signed_transaction() {
        let cases = [
            ("garbage hex", PaymentPayload::transaction("0xno")),
            ("not hex at all", PaymentPayload::transaction("zzz")),
            ("empty", PaymentPayload::transaction("")),
            ("hash payload", PaymentPayload::hash("0xdeadbeef")),
        ];
        for (name, payload) in cases {
            assert!(signer_address(&payload).is_none(), "case: {name}");
        }
    }

    #[tokio::test]
    async fn screening_outcomes_map_to_responses() {
        // The screener HEADs the S3 bucket: 200 is a list hit, 404 a miss, anything
        // else means the list could not be consulted.
        let cases = [
            (200, Some(StatusCode::PAYMENT_REQUIRED)),
            (404, None),
            (500, Some(StatusCode::SERVICE_UNAVAILABLE)),
        ];
        for (s3_status, expected) in cases {
            let (_server, screener) = crate::screener::test_screener_answering(s3_status).await;

            let refusal = screen_signer(&screener, Some("0xsigner".to_string())).await;
            assert_eq!(
                refusal.map(|response| response.status()),
                expected,
                "s3 status: {s3_status}"
            );
        }
    }

    #[tokio::test]
    async fn screening_requires_a_recoverable_signer() {
        // With a screener configured, a payment whose signer cannot be recovered is
        // refused without consulting anything (the endpoint is unreachable).
        let screener = crate::screener::test_screener("http://127.0.0.1:1".to_string());
        let refusal = screen_signer(&screener, None).await;
        assert_eq!(
            refusal.map(|response| response.status()),
            Some(StatusCode::PAYMENT_REQUIRED)
        );
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
