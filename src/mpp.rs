//! The MPP payment rail — everything specific to MPP lives here.
//!
//! See `x402.rs` for the x402 rail and `dispatch.rs` for the neutral router that
//! classifies each request and delegates to whichever rail it is paying on.

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};

/// MPP's `WWW-Authenticate` challenge value, using the `Payment` scheme (the client
/// replies with `Authorization: Payment <credential>`). Real params come from `mpp-rs`.
const CHALLENGE: &str = r#"Payment realm="bx402""#;

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
