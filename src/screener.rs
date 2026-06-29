//! Screens payers against a list of restricted addresses.
//!
//! The list is an S3 bucket. Each prohibited address is stored as a key, base64url
//! encoded. Checking membership is one `HeadObject`:
//!
//! - key present (`200`): on the list
//! - key absent (`404`): not on the list
//!
//! The screener errs on the side of caution. Only a `404` means "not on the list". Any
//! other error (timeout, misconfiguration, S3 outage) blocks the payment rather than
//! letting it through unchecked.
//!
//! The module is chain agnostic. It screens the identifier string exactly as given and
//! knows nothing about the address itself. It returns a plain outcome, never an HTTP
//! response; the rail turns that outcome into a status code.
//!
//! Canonicalization belongs to the caller, because it differs per chain:
//!
//! - EVM: addresses are case-insensitive hex, so the rail lowercases (`{:#x}`).
//! - Solana: addresses are case-sensitive base58, so the rail passes them as is.
//!
//! One screener backs every chain this way. Adapted from the Go reference
//! (`brave-intl/compliance-ops`).

// Unused until the x402 rail wires it in; dropped then.
#![allow(dead_code)]

use aws_sdk_s3::operation::head_object::HeadObjectError as SdkErr;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Screens identifiers against the restricted-address S3 bucket. `Clone` is cheap: the
/// inner `aws_sdk_s3::Client` is `Arc`-backed, like `reqwest::Client`.
#[derive(Clone)]
pub(crate) struct RestrictedAddressScreener {
    client: aws_sdk_s3::Client,
    bucket: String,
}

/// The two definite answers a screen can give.
///
/// When the screener cannot give a definite answer, it returns [`ScreenError`] instead,
/// and the caller must still deny the payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Screening {
    /// On the restricted list. Deny the payment.
    Blocked,
    /// Not on the list.
    Allowed,
}

/// The screen could not give a definite answer: a timeout, a misconfiguration,
/// `AccessDenied`, or any other S3 failure.
///
/// The caller must treat this as a denial. The underlying SDK error is kept as the
/// source for logs and is never shown to clients.
#[derive(Debug, thiserror::Error)]
#[error("address screening unavailable")]
pub(crate) struct ScreenError(#[source] Box<dyn std::error::Error + Send + Sync>);

impl RestrictedAddressScreener {
    pub(crate) fn new(client: aws_sdk_s3::Client, bucket: String) -> Self {
        Self { client, bucket }
    }

    /// Screen one identifier, exactly as given.
    ///
    /// The caller must pass the already-canonical form (casing differs per chain; see the
    /// module docs). The key is `base64url(identifier)`, and one `HeadObject` decides:
    ///
    /// - `200`: on the list, returns [`Screening::Blocked`]
    /// - `404 NotFound`: not on the list, returns [`Screening::Allowed`]
    /// - anything else: returns [`ScreenError`], so the caller blocks
    pub(crate) async fn screen(&self, identifier: &str) -> Result<Screening, ScreenError> {
        let key = URL_SAFE_NO_PAD.encode(identifier.as_bytes());
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            // Key exists, so the address is on the list.
            Ok(_) => Ok(Screening::Blocked),
            // A 404 is the only way to be allowed.
            Err(err) if err.as_service_error().is_some_and(SdkErr::is_not_found) => {
                Ok(Screening::Allowed)
            }
            // Any other failure blocks the payment.
            Err(err) => Err(ScreenError(Box::new(err))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region, retry::RetryConfig};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a screener pointed at a wiremock server standing in for S3.
    ///
    /// The SDK signs every request, even against a fake, so the client needs:
    ///
    /// - static test credentials
    /// - an explicit region
    /// - `force_path_style`, so requests are `HEAD /<bucket>/<key>` (a virtual-host URL
    ///   could not resolve to the mock)
    ///
    /// Retries are off so the error paths return promptly and deterministically.
    fn screener_for(endpoint: String) -> RestrictedAddressScreener {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::new("test", "test", None, None, "test"))
            .endpoint_url(endpoint)
            .force_path_style(true)
            .retry_config(RetryConfig::disabled())
            .build();
        RestrictedAddressScreener::new(
            aws_sdk_s3::Client::from_conf(config),
            "restricted".to_string(),
        )
    }

    /// Screen `"0xanything"` against a mock S3 that answers every `HEAD` with `status`.
    async fn screen_against(status: u16) -> Result<Screening, ScreenError> {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        screener_for(server.uri()).screen("0xanything").await
    }

    #[tokio::test]
    async fn present_key_is_blocked() {
        assert_eq!(screen_against(200).await.unwrap(), Screening::Blocked);
    }

    #[tokio::test]
    async fn absent_key_is_allowed() {
        assert_eq!(screen_against(404).await.unwrap(), Screening::Allowed);
    }

    /// The screener never changes the casing of an identifier. Casing is the caller's
    /// job. So two casings of one address are two different keys: the exact stored string
    /// matches, a different casing does not.
    #[tokio::test]
    async fn screens_the_identifier_verbatim() {
        // A list entry stored in one exact casing (here, EIP-55 checksummed).
        let stored = "0xAb5801a7D398351b8bE11C439e05C5B3259aeC9B";
        let other_casing = stored.to_ascii_lowercase();
        let stored_key = URL_SAFE_NO_PAD.encode(stored.as_bytes());

        let server = MockServer::start().await;
        // Only the exact stored key exists; every other key 404s.
        Mock::given(method("HEAD"))
            .and(wiremock::matchers::path(format!(
                "/restricted/{stored_key}"
            )))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let screener = screener_for(server.uri());
        assert_eq!(
            screener.screen(stored).await.unwrap(),
            Screening::Blocked,
            "the exact stored string matches",
        );
        assert_eq!(
            screener.screen(&other_casing).await.unwrap(),
            Screening::Allowed,
            "a different casing is a different key; the screener never normalizes",
        );
    }

    #[tokio::test]
    async fn server_error_denies() {
        assert!(
            screen_against(500).await.is_err(),
            "a non-404 error must not resolve to Allowed",
        );
    }

    #[tokio::test]
    async fn unreachable_s3_denies() {
        // Nothing listens on port 1: the request fails at the transport layer, which is
        // not a definite NotFound, so the screener denies.
        let screener = screener_for("http://127.0.0.1:1".to_string());
        assert!(screener.screen("0xanything").await.is_err());
    }
}
