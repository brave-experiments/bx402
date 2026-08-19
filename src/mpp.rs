//! The MPP payment rail — everything specific to MPP lives here.
//!
//! See `x402.rs` for the x402 rail and `dispatch.rs` for the neutral router that
//! classifies each request and delegates to whichever rail it is paying on.
//!
//! MPP (the Machine Payments Protocol) pays in stablecoins on the Tempo chain.
//! Unlike x402 there is no facilitator service behind this rail: the `mpp` SDK
//! verifies a credential and settles it on Tempo in the same call.

use std::time::Duration;

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
#[cfg(test)]
use mpp::protocol::methods::tempo::{CHAIN_ID as TEMPO_CHAIN_ID, MODERATO_CHAIN_ID};
use mpp::protocol::methods::tempo::{PATH_USD, TEMPO_TX_TYPE_ID, TempoNetwork};
use mpp::server::{ErrorCode, Mpp, TempoChargeMethod, TempoConfig, TempoProvider, tempo};
use tempo_primitives::transaction::AASigned;

use crate::error::json_error;
use crate::screener::RestrictedAddressScreener;
use crate::{AppError, config::MppConfig};

#[cfg(test)]
use crate::Config;

/// The settlement receipt returns to the client in the `Payment-Receipt` response
/// header, the dual of the `Authorization` request header.
const PAYMENT_RECEIPT_HEADER: &str = "payment-receipt";

/// The realm named in every MPP challenge and echoed back in every credential.
const REALM: &str = "bx402";

/// The EVM treasury address that receives MPP payments (the challenge recipient).
const PAY_TO_EVM: &str = "0xbd9420A98a7Bd6B89765e5715e169481602D9c3d";

/// Flat price per request in base units of pathUSD (6 decimals, so `5_000` =
/// 0.005). The x402 rail charges the same 0.005 through its own
/// `PRICE_USDC_BASE_UNITS`. A price change edits both consts.
const PRICE_USD_BASE_UNITS: u64 = 5_000;

/// How long to wait for the startup chain-id query before giving up.
const CHAIN_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Build the MPP client for the configured RPC endpoint.
///
/// * The chain is whatever the endpoint reports at startup, never assumed
///   from the URL. An unsupported chain is refused, and a testnet chain is
///   served only when the testnet is allowed.
/// * The charge currency is pinned to pathUSD, the same TIP-20 address on
///   every Tempo network, overriding the SDK default of USDC on mainnet.
/// * An unreachable endpoint or an unusable `MPP_SECRET_KEY` is a startup
///   misconfiguration, surfaced as [`AppError`].
pub(crate) async fn client(rail: &MppConfig, allow_testnet: bool) -> Result<Client, AppError> {
    let chain_id = get_chain_id(&rail.rpc_url).await?;
    // Exhaustive so a new SDK network variant fails the build here, forcing
    // its testnet classification to be decided.
    let testnet = match TempoNetwork::from_chain_id(chain_id) {
        Some(TempoNetwork::Moderato) => true,
        Some(TempoNetwork::Mainnet) => false,
        None => {
            return Err(AppError::InvalidConfig(format!(
                "MPP: unsupported Tempo chain {chain_id}"
            )));
        }
    };
    if testnet && !allow_testnet {
        return Err(AppError::InvalidConfig(format!(
            "MPP_RPC_URL: chain {chain_id} is a testnet; set ALLOW_TESTNET=true to accept it"
        )));
    }
    let builder = tempo(TempoConfig {
        recipient: PAY_TO_EVM,
    })
    .rpc_url(&rail.rpc_url)
    .chain_id(chain_id)
    .currency(PATH_USD)
    .realm(REALM)
    .secret_key(&rail.secret_key);
    let handler =
        Handler::create(builder).map_err(|err| AppError::InvalidConfig(format!("MPP: {err}")))?;
    let charge = ChargeRequest {
        amount: PRICE_USD_BASE_UNITS.to_string(),
        currency: PATH_USD.to_string(),
        recipient: Some(PAY_TO_EVM.to_string()),
        method_details: Some(json!({ "chainId": chain_id })),
        ..Default::default()
    };
    Ok(Client { handler, charge })
}

/// The chain id the RPC endpoint reports for itself (`eth_chainId`).
async fn get_chain_id(rpc_url: &str) -> Result<u64, AppError> {
    let invalid = |detail: String| AppError::InvalidConfig(format!("MPP_RPC_URL: {detail}"));

    let client = reqwest::Client::builder()
        .timeout(CHAIN_QUERY_TIMEOUT)
        .build()
        .map_err(|err| invalid(format!("chain query client: {err}")))?;

    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": [] });
    let body: serde_json::Value = client
        .post(rpc_url)
        .json(&request)
        .send()
        .await
        .and_then(|response| response.error_for_status())
        .map_err(|err| invalid(format!("eth_chainId query failed: {err}")))?
        .json()
        .await
        .map_err(|err| invalid(format!("eth_chainId response is not JSON: {err}")))?;

    // The chain id comes back as a hex string like "0xa5bf".
    body["result"]
        .as_str()
        .and_then(|hex| hex.strip_prefix("0x"))
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .ok_or_else(|| invalid(format!("eth_chainId returned no chain id: {body}")))
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
        && let Err(denied) = screener
            .require_allowed(signer_address(&payload), payment_rejected())
            .await
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

/// [`client`] built against a mock RPC reporting `chain`: tests construct
/// clients through the production path, only the endpoint is canned.
#[cfg(test)]
async fn client_on(config: &Config, chain: u64) -> Result<Client, AppError> {
    let rpc = make_tempo_rpc(chain).await;
    let config = config.clone().with_mpp_rpc_url(rpc.uri());
    client(config.mpp_rail(), config.allow_testnet).await
}

/// A mock Tempo RPC answering the startup `eth_chainId` query with `chain`,
/// exactly once. Later calls get a `404` while the port stays bound, so a
/// test holding the server has a dead RPC that nothing can rebind.
#[cfg(test)]
async fn make_tempo_rpc(chain: u64) -> wiremock::MockServer {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "eth_chainId" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 1, "result": format!("0x{chain:x}")
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    server
}

/// A mock RPC on Moderato, the tests' chain, answering the startup chain query.
#[cfg(test)]
pub(crate) async fn test_rpc() -> wiremock::MockServer {
    make_tempo_rpc(MODERATO_CHAIN_ID).await
}

/// Mint the `Authorization` header value for a credential answering our own
/// challenge with `payload`: the test-side counterpart of [`credential`], using
/// the same charge the app under test builds from `config`.
#[cfg(test)]
async fn credential_header(config: &Config, payload: PaymentPayload) -> String {
    let client = client_on(config, MODERATO_CHAIN_ID)
        .await
        .expect("test config builds the mpp client");
    let challenge = client
        .handler
        .charge_challenge_with_options(&client.charge, None, None)
        .expect("the challenge builds");
    let credential = PaymentCredential::new(challenge.to_echo(), payload);
    mpp::protocol::core::format_authorization(&credential).expect("the credential formats")
}

/// A credential whose payload says the client already broadcast the transfer.
#[cfg(test)]
pub(crate) async fn hash_credential_header(config: &Config) -> String {
    credential_header(config, PaymentPayload::hash("0xdeadbeef")).await
}

/// A credential paying with the forged signed transaction. Returns the header and
/// the signer's lowercase address, so a test can put that exact address on the
/// restricted list.
#[cfg(test)]
pub(crate) async fn signed_transaction_credential_header(config: &Config) -> (String, String) {
    let (tx, signer) = forged_transaction().await;
    let header = credential_header(config, PaymentPayload::transaction(tx)).await;
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

    #[tokio::test]
    async fn client_requires_a_usable_endpoint() {
        // Both die in the startup chain query, the first step of the build.
        for endpoint in ["not a url", "http://127.0.0.1:1"] {
            let config = Config::for_tests().with_mpp_rpc_url(endpoint.into());
            assert!(matches!(
                client(config.mpp_rail(), config.allow_testnet).await,
                Err(AppError::InvalidConfig(_))
            ));
        }
    }

    #[tokio::test]
    async fn a_testnet_chain_requires_the_testnet_flag() {
        let config = Config {
            allow_testnet: false,
            ..Config::for_tests()
        };
        let Err(err) = client_on(&config, MODERATO_CHAIN_ID).await else {
            panic!("a testnet chain must be refused when testnets are off");
        };
        assert!(
            err.to_string().contains("ALLOW_TESTNET"),
            "error was: {err}"
        );

        // Mainnet needs no flag.
        assert!(client_on(&config, TEMPO_CHAIN_ID).await.is_ok());
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

    #[tokio::test]
    async fn challenge_advertises_the_charge_credentials_answer() {
        let client = client_on(&Config::for_tests(), MODERATO_CHAIN_ID)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn the_charge_follows_the_chain_and_pins_the_price() {
        for chain in [MODERATO_CHAIN_ID, TEMPO_CHAIN_ID] {
            let request = client_on(&Config::for_tests(), chain).await.unwrap().charge;

            assert_eq!(request.amount, PRICE_USD_BASE_UNITS.to_string(), "{chain}");
            assert_eq!(
                request.currency, "0x20c0000000000000000000000000000000000000",
                "{chain}"
            );
            assert_eq!(request.recipient.as_deref(), Some(PAY_TO_EVM), "{chain}");
            assert_eq!(
                request.method_details,
                Some(json!({ "chainId": chain })),
                "{chain}"
            );
        }

        // Any other chain is refused rather than served with a default token.
        assert!(matches!(
            client_on(&Config::for_tests(), 1).await,
            Err(AppError::InvalidConfig(_))
        ));
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
