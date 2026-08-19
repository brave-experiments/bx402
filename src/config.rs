//! Runtime configuration, read from the environment at startup.

use std::env;

use crate::AppError;

/// Default base URL when `BRAVE_SEARCH_API_BASE_URL` is unset.
const DEFAULT_BRAVE_SEARCH_API_BASE_URL: &str = "https://api.search.brave.com";

/// Settings for the x402 rail.
#[cfg_attr(test, derive(Clone))]
pub struct X402Config {
    /// Base URL of the x402 facilitator that verifies and settles payments.
    /// Docs: <https://docs.x402.org/core-concepts/facilitator>
    pub facilitator_url: String,
}

/// Settings for the MPP rail.
#[cfg_attr(test, derive(Clone))]
pub struct MppConfig {
    /// Tempo RPC endpoint the MPP rail verifies and settles payments against. The
    /// chain is discovered by querying the endpoint at startup. Testnet chains
    /// require `ALLOW_TESTNET`.
    pub rpc_url: String,
    /// Secret that marks MPP challenges as ours. Challenge ids are HMACs under this
    /// key, so only a credential answering a challenge this service issued verifies.
    pub secret_key: String,
}

/// Runtime configuration, read once from the environment at startup.
#[cfg_attr(test, derive(Clone))]
pub struct Config {
    /// Brave Search API key, forwarded upstream as `X-Subscription-Token`.
    pub brave_search_api_key: String,
    /// Base URL of the Brave Search API. Overridable so tests can point at a
    /// mock server; defaults to the public endpoint.
    pub brave_search_api_base_url: String,
    /// Settings for the x402 rail.
    pub x402: X402Config,
    /// Settings for the MPP rail.
    pub mpp: MppConfig,
    /// S3 bucket holding the restricted-address list. `None` turns screening off,
    /// the default for local and testnet runs.
    pub restricted_address_s3_bucket: Option<String>,
    /// Accept testnet networks. Off in production, so faucet-money rails never
    /// pay for real API traffic there.
    pub allow_testnet: bool,
}

impl Config {
    /// Read configuration from the process environment. Each field comes from one
    /// variable:
    ///
    /// * `BRAVE_SEARCH_API_KEY` (required): forwarded upstream as `X-Subscription-Token`.
    /// * `X402_FACILITATOR_URL` (required): base URL of the x402 facilitator.
    /// * `MPP_RPC_URL` (required): Tempo RPC endpoint for the MPP rail.
    /// * `MPP_SECRET_KEY` (required): HMAC secret binding MPP challenges to this service.
    /// * `BRAVE_SEARCH_API_BASE_URL` (optional): defaults to the public Brave Search API endpoint.
    /// * `RESTRICTED_ADDRESS_S3_BUCKET` (optional): unset or empty turns screening off.
    /// * `ALLOW_TESTNET` (optional): `true` permits testnet networks, with each
    ///   rail deciding what that admits.
    ///
    /// An absent required variable yields [`AppError::MissingConfig`]; a present but
    /// non-Unicode one yields [`AppError::InvalidConfig`].
    pub fn from_env() -> Result<Self, AppError> {
        let brave_search_api_key = require_var("BRAVE_SEARCH_API_KEY")?;
        let brave_search_api_base_url = env::var("BRAVE_SEARCH_API_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BRAVE_SEARCH_API_BASE_URL.to_string());
        let x402 = X402Config {
            facilitator_url: require_var("X402_FACILITATOR_URL")?,
        };
        let mpp = MppConfig {
            rpc_url: require_var("MPP_RPC_URL")?,
            secret_key: require_var("MPP_SECRET_KEY")?,
        };
        let restricted_address_s3_bucket = env::var("RESTRICTED_ADDRESS_S3_BUCKET")
            .ok()
            .filter(|bucket| !bucket.is_empty());
        let allow_testnet = env::var("ALLOW_TESTNET").is_ok_and(|value| value == "true");
        Ok(Self {
            brave_search_api_key,
            brave_search_api_base_url,
            x402,
            mpp,
            restricted_address_s3_bucket,
            allow_testnet,
        })
    }
}

#[cfg(test)]
impl Config {
    /// A config whose every endpoint is parseable but unreachable, shared by the
    /// test modules across the crate; each test overrides the fields it exercises.
    pub(crate) fn for_tests() -> Self {
        Self {
            brave_search_api_key: "secret-key".to_string(),
            brave_search_api_base_url: "http://upstream.invalid".to_string(),
            x402: X402Config {
                facilitator_url: "http://facilitator.invalid".to_string(),
            },
            mpp: MppConfig {
                rpc_url: "http://tempo.invalid".to_string(),
                secret_key: "test-secret".to_string(),
            },
            restricted_address_s3_bucket: None,
            allow_testnet: true,
        }
    }

    /// The same config with the x402 facilitator pointed at `url`.
    pub(crate) fn with_facilitator_url(mut self, url: String) -> Self {
        self.x402.facilitator_url = url;
        self
    }

    /// The same config with the MPP rail pointed at `url`.
    pub(crate) fn with_mpp_rpc_url(mut self, url: String) -> Self {
        self.mpp.rpc_url = url;
        self
    }
}

/// Read a required environment variable, distinguishing the two ways it can fail:
/// absent is [`AppError::MissingConfig`], present but non-Unicode is
/// [`AppError::InvalidConfig`].
fn require_var(name: &'static str) -> Result<String, AppError> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Err(AppError::MissingConfig(name)),
        Err(env::VarError::NotUnicode(_)) => Err(AppError::InvalidConfig(format!(
            "{name}: not valid Unicode"
        ))),
    }
}
