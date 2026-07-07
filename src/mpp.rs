//! The MPP payment rail — everything specific to MPP lives here.
//!
//! See `x402.rs` for the x402 rail and `dispatch.rs` for the neutral router that
//! classifies each request and delegates to whichever rail it is paying on.
//!
//! MPP (the Machine Payments Protocol) pays in stablecoins on the Tempo chain.
//! Unlike x402 there is no facilitator service behind this rail: the `mpp` SDK
//! verifies a credential and settles it on Tempo in the same call.

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};

use mpp::server::{Mpp, TempoChargeMethod, TempoConfig, TempoProvider, tempo};

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
/// `402` while the rail cannot verify what this advertises.
#[expect(
    dead_code,
    reason = "the MPP rail is paused; dispatch re-adds this challenge to the cold 402 when MPP verification ships"
)]
pub(crate) fn challenge() -> (HeaderName, HeaderValue) {
    (
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static(CHALLENGE),
    )
}

/// The MPP payment handler, newtyped so the rest of the crate names this module's
/// type, not the SDK's, and `dispatch` can carry it as plain axum state.
#[derive(Clone)]
#[allow(dead_code, reason = "dispatch does not carry the MPP handler")]
pub(crate) struct Client(Handler);

/// Build the MPP handler from config. A bad `MPP_RPC_URL` or unusable
/// `MPP_SECRET_KEY` is a startup misconfiguration, surfaced as [`AppError`]. The
/// Tempo chain and challenge currency both follow from the RPC URL.
#[allow(dead_code, reason = "dispatch does not carry the MPP handler")]
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
}
