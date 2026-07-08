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

use std::time::Duration;

use anyhow::Context;
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::timeout::TimeoutConfig;
use aws_sdk_s3::operation::head_object::HeadObjectError as SdkErr;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::Config;

/// Canary key used to probe the bucket at startup.
const CANARY_KEY: &str = "bx402.canary";

/// How long a screen may take before it counts as unavailable. Bounds the paid
/// request path; the S3 client's own timeout is a looser startup backstop.
const SCREEN_TIMEOUT: Duration = Duration::from_secs(2);

/// Screens identifiers against the restricted-address S3 bucket. `Clone` is cheap: the
/// inner `aws_sdk_s3::Client` is `Arc`-backed, like `reqwest::Client`.
#[derive(Clone)]
pub struct RestrictedAddressScreener {
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

    /// Screen one identifier, exactly as given, within [`SCREEN_TIMEOUT`].
    ///
    /// The caller must pass the already-canonical form (casing differs per chain; see
    /// the module docs). The identifier is base64url-encoded into the S3 key. A lookup
    /// that outlives the deadline is a [`ScreenError`] like any other failure, so the
    /// caller denies it the same way.
    pub(crate) async fn screen(&self, identifier: &str) -> Result<Screening, ScreenError> {
        let lookup = self.head_key(URL_SAFE_NO_PAD.encode(identifier.as_bytes()));
        match tokio::time::timeout(SCREEN_TIMEOUT, lookup).await {
            Ok(screened) => screened,
            Err(elapsed) => Err(ScreenError(Box::new(elapsed))),
        }
    }

    /// Look up one exact S3 key with `HeadObject`:
    ///
    /// - `200`: key present, returns [`Screening::Blocked`]
    /// - `404 NotFound`: key absent, returns [`Screening::Allowed`]
    /// - anything else: returns [`ScreenError`], so the caller blocks
    async fn head_key(&self, key: String) -> Result<Screening, ScreenError> {
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

/// Outcome of [`init`], for the startup log line.
pub enum Status {
    /// Screening is on, against this bucket.
    Enabled { bucket: String },
    /// Screening is off because no bucket is configured.
    Disabled,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Enabled { bucket } => write!(f, "✓ enabled (bucket={bucket})"),
            Status::Disabled => write!(f, "✗ disabled (RESTRICTED_ADDRESS_S3_BUCKET not set)"),
        }
    }
}

/// Build the screener from config and prove it works before serving traffic.
///
/// - bucket unset: returns `(None, Disabled)`, screening off
/// - bucket set: builds the AWS client and probes the bucket once. A reachable bucket
///   returns `(Some, Enabled)`; any probe failure is an error that aborts startup, so a
///   misconfigured screener never serves traffic.
pub async fn init(config: &Config) -> anyhow::Result<(Option<RestrictedAddressScreener>, Status)> {
    let Some(bucket) = config.restricted_address_s3_bucket.clone() else {
        return Ok((None, Status::Disabled));
    };
    // Cap the S3 call so a stalled connection can't hang startup. Kept generous because
    // the first call also resolves credentials.
    let aws = aws_config::defaults(BehaviorVersion::latest())
        .timeout_config(
            TimeoutConfig::builder()
                .operation_timeout(Duration::from_secs(10))
                .build(),
        )
        .load()
        .await;
    let (screener, status) = init_with(aws_sdk_s3::Client::new(&aws), bucket).await?;
    Ok((Some(screener), status))
}

/// The probe, split out so tests can inject a client pointed at a mock bucket. Only
/// reached when a bucket is configured, so it always yields a screener on success.
async fn init_with(
    client: aws_sdk_s3::Client,
    bucket: String,
) -> anyhow::Result<(RestrictedAddressScreener, Status)> {
    let screener = RestrictedAddressScreener::new(client, bucket.clone());
    // A reachable bucket (404, or even a 200) proves credentials and permissions work.
    // On failure, `?` carries the real cause (bad creds, IAM 403, timeout) up the chain.
    screener
        .head_key(CANARY_KEY.to_string())
        .await
        .with_context(|| {
            format!("restricted address screening probe failed for bucket {bucket}")
        })?;
    Ok((screener, Status::Enabled { bucket }))
}

/// Build an S3 client pointed at a wiremock server standing in for S3, shared by tests
/// across the crate.
///
/// The SDK signs every request, even against a fake, so the client needs:
///
/// - static test credentials
/// - an explicit region
/// - `force_path_style`, so requests are `HEAD /<bucket>/<key>` (a virtual-host URL
///   could not resolve to the mock)
///
/// Retries are off so the error paths return promptly and deterministically.
#[cfg(test)]
fn test_client(endpoint: String) -> aws_sdk_s3::Client {
    use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region, retry::RetryConfig};
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new("test", "test", None, None, "test"))
        .endpoint_url(endpoint)
        .force_path_style(true)
        .retry_config(RetryConfig::disabled())
        .build();
    aws_sdk_s3::Client::from_conf(config)
}

/// Build a screener pointed at a wiremock S3, for tests in other modules of the crate.
#[cfg(test)]
pub(crate) fn test_screener(endpoint: String, bucket: &str) -> RestrictedAddressScreener {
    RestrictedAddressScreener::new(test_client(endpoint), bucket.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn screener_for(endpoint: String) -> RestrictedAddressScreener {
        test_screener(endpoint, "restricted-address-bucket")
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
                "/restricted-address-bucket/{stored_key}"
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

    #[tokio::test]
    async fn init_disabled_when_bucket_unset() {
        let config = crate::Config::for_tests();
        let (screener, status) = init(&config).await.unwrap();
        assert!(screener.is_none());
        assert!(matches!(status, Status::Disabled));
    }

    #[tokio::test]
    async fn init_enabled_when_bucket_reachable() {
        let server = MockServer::start().await;
        // The probe HEADs the literal canary key; a 404 there means the bucket is
        // reachable. The 500 catch-all makes the test pass only if that exact key was hit.
        Mock::given(method("HEAD"))
            .and(wiremock::matchers::path(format!(
                "/restricted-address-bucket/{CANARY_KEY}"
            )))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let (_screener, status) = init_with(
            test_client(server.uri()),
            "restricted-address-bucket".to_string(),
        )
        .await
        .unwrap();
        assert!(matches!(status, Status::Enabled { .. }));
    }

    /// Run `init_with` against a mock S3 that answers every `HEAD` with `status`.
    async fn init_against(status: u16) -> anyhow::Result<(RestrictedAddressScreener, Status)> {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        init_with(
            test_client(server.uri()),
            "restricted-address-bucket".to_string(),
        )
        .await
    }

    #[tokio::test]
    async fn init_enabled_when_canary_present() {
        let (_screener, status) = init_against(200).await.unwrap();
        assert!(matches!(status, Status::Enabled { .. }));
    }

    #[tokio::test]
    async fn init_fails_fast_on_probe_error() {
        // `.err()` drops the `Ok` value, which is not `Debug`, unlike `unwrap_err`.
        let err = init_against(403)
            .await
            .err()
            .expect("a probe error must abort init");
        assert!(
            err.to_string()
                .contains("restricted address screening probe failed"),
            "error was: {err:?}",
        );
    }

    #[test]
    fn status_display_reads_clearly() {
        let enabled = Status::Enabled {
            bucket: "restricted-address-bucket".to_string(),
        };
        assert_eq!(
            enabled.to_string(),
            "✓ enabled (bucket=restricted-address-bucket)"
        );
        assert_eq!(
            Status::Disabled.to_string(),
            "✗ disabled (RESTRICTED_ADDRESS_S3_BUCKET not set)"
        );
    }
}
