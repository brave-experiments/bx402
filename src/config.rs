//! Runtime configuration, read from the environment at startup.

use std::env;

use crate::AppError;

/// Default base URL when `BRAVE_SEARCH_API_BASE_URL` is unset.
const DEFAULT_BRAVE_SEARCH_API_BASE_URL: &str = "https://api.search.brave.com";

/// Which payment rails the deployment turns on. This is the deployment-level
/// toggle from `ENABLED_RAILS`, not the per-request `dispatch::Rail`. The
/// default is every rail off, the starting point the parser adds to.
#[derive(Default)]
struct EnabledRails {
    x402: bool,
    mpp: bool,
}

/// Read `ENABLED_RAILS`. Unset enables both rails.
fn enabled_rails() -> Result<EnabledRails, AppError> {
    match env::var("ENABLED_RAILS") {
        Ok(value) => parse_enabled_rails(&value),
        Err(env::VarError::NotPresent) => Ok(EnabledRails {
            x402: true,
            mpp: true,
        }),
        Err(env::VarError::NotUnicode(_)) => Err(AppError::InvalidConfig(
            "ENABLED_RAILS: not valid Unicode".into(),
        )),
    }
}

/// Parse an `ENABLED_RAILS` value: a comma-separated list of rail names, each
/// `x402` or `mpp`. Anything else is refused, so a typo can never silently
/// disable a rail.
fn parse_enabled_rails(value: &str) -> Result<EnabledRails, AppError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::InvalidConfig(
            "ENABLED_RAILS: empty; unset it to enable both rails, or list a subset of x402,mpp"
                .into(),
        ));
    }
    let mut rails = EnabledRails::default();
    for token in value.split(',') {
        match token.trim() {
            "x402" => rails.x402 = true,
            "mpp" => rails.mpp = true,
            other => {
                return Err(AppError::InvalidConfig(format!(
                    "ENABLED_RAILS: unknown rail {other:?}, expected a comma-separated subset of x402,mpp"
                )));
            }
        }
    }
    Ok(rails)
}

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
    /// Settings for the x402 rail. `None` when `ENABLED_RAILS` leaves the rail out.
    pub x402: Option<X402Config>,
    /// Settings for the MPP rail. `None` when `ENABLED_RAILS` leaves the rail out.
    pub mpp: Option<MppConfig>,
    /// S3 bucket holding the restricted-address list. `None` turns screening off,
    /// the default for local and testnet runs.
    pub restricted_address_s3_bucket: Option<String>,
    /// Accept testnet networks. Off in production, so faucet-money rails never
    /// pay for real API traffic there.
    pub allow_testnet: bool,
}

impl Config {
    /// Read configuration from the process environment:
    ///
    /// * `BRAVE_SEARCH_API_KEY` (required): forwarded upstream as `X-Subscription-Token`.
    /// * `ENABLED_RAILS` (optional): comma-separated subset of `x402,mpp` naming the
    ///   rails to serve; unset enables both. A disabled rail's variables are not read.
    /// * `X402_FACILITATOR_URL` (required when the x402 rail is enabled): base URL of
    ///   the x402 facilitator.
    /// * `MPP_RPC_URL` (required when the MPP rail is enabled): Tempo RPC endpoint for
    ///   the MPP rail.
    /// * `MPP_SECRET_KEY` (required when the MPP rail is enabled): HMAC secret binding
    ///   MPP challenges to this service.
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
        let rails = enabled_rails()?;
        let x402 = if rails.x402 {
            Some(X402Config {
                facilitator_url: require_var("X402_FACILITATOR_URL")?,
            })
        } else {
            None
        };
        let mpp = if rails.mpp {
            Some(MppConfig {
                rpc_url: require_var("MPP_RPC_URL")?,
                secret_key: require_var("MPP_SECRET_KEY")?,
            })
        } else {
            None
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
            x402: Some(X402Config {
                facilitator_url: "http://facilitator.invalid".to_string(),
            }),
            mpp: Some(MppConfig {
                rpc_url: "http://tempo.invalid".to_string(),
                secret_key: "test-secret".to_string(),
            }),
            restricted_address_s3_bucket: None,
            allow_testnet: true,
        }
    }

    /// The x402 rail the test config enables.
    pub(crate) fn x402_rail(&self) -> &X402Config {
        self.x402
            .as_ref()
            .expect("the test config enables the x402 rail")
    }

    /// The MPP rail the test config enables.
    pub(crate) fn mpp_rail(&self) -> &MppConfig {
        self.mpp
            .as_ref()
            .expect("the test config enables the MPP rail")
    }

    /// The same config with the x402 facilitator pointed at `url`.
    pub(crate) fn with_facilitator_url(mut self, url: String) -> Self {
        self.x402 = Some(X402Config {
            facilitator_url: url,
        });
        self
    }

    /// The same config with the MPP rail pointed at `url`.
    pub(crate) fn with_mpp_rpc_url(mut self, url: String) -> Self {
        self.mpp
            .as_mut()
            .expect("the test config enables the MPP rail")
            .rpc_url = url;
        self
    }

    /// The same config with the x402 rail disabled.
    pub(crate) fn without_x402(mut self) -> Self {
        self.x402 = None;
        self
    }

    /// The same config with the MPP rail disabled.
    pub(crate) fn without_mpp(mut self) -> Self {
        self.mpp = None;
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        /// Label printed if the assertion fails.
        name: &'static str,
        /// The `ENABLED_RAILS` value to parse.
        value: &'static str,
        /// The `(x402, mpp)` flags the value should parse to, or `None` when
        /// the value must be rejected.
        expected: Option<(bool, bool)>,
    }

    #[test]
    fn parse_enabled_rails_accepts_only_known_rails() {
        let cases = [
            Case {
                name: "both rails",
                value: "x402,mpp",
                expected: Some((true, true)),
            },
            Case {
                name: "both rails, either order",
                value: "mpp,x402",
                expected: Some((true, true)),
            },
            Case {
                name: "x402 only",
                value: "x402",
                expected: Some((true, false)),
            },
            Case {
                name: "mpp only",
                value: "mpp",
                expected: Some((false, true)),
            },
            Case {
                name: "whitespace around tokens",
                value: " x402 , mpp ",
                expected: Some((true, true)),
            },
            Case {
                name: "duplicate rail",
                value: "x402,x402",
                expected: Some((true, false)),
            },
            Case {
                name: "empty",
                value: "",
                expected: None,
            },
            Case {
                name: "only whitespace",
                value: "  ",
                expected: None,
            },
            Case {
                name: "unknown rail",
                value: "btc",
                expected: None,
            },
            Case {
                name: "rail names are lowercase",
                value: "X402",
                expected: None,
            },
            Case {
                name: "trailing comma",
                value: "x402,",
                expected: None,
            },
        ];
        for Case {
            name,
            value,
            expected,
        } in cases
        {
            let parsed = parse_enabled_rails(value).map(|rails| (rails.x402, rails.mpp));
            match expected {
                Some(flags) => assert_eq!(parsed.ok(), Some(flags), "case: {name}"),
                None => {
                    let message = parsed.expect_err(name).to_string();
                    assert!(
                        message.contains("ENABLED_RAILS"),
                        "case: {name}, error was: {message}"
                    );
                }
            }
        }
    }
}
