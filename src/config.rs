//! Runtime configuration, read from the environment at startup.

use std::env;

use crate::AppError;

/// Default base URL when `BRAVE_SEARCH_API_BASE_URL` is unset.
const DEFAULT_BRAVE_SEARCH_API_BASE_URL: &str = "https://api.search.brave.com";

/// Runtime configuration, read once from the environment at startup.
pub struct Config {
    /// Brave Search API key, forwarded upstream as `X-Subscription-Token`.
    pub brave_search_api_key: String,
    /// Base URL of the Brave Search API. Overridable so tests can point at a
    /// mock server; defaults to the public endpoint.
    pub brave_search_api_base_url: String,
    /// Base URL of the x402 facilitator that verifies and settles payments.
    /// Docs: <https://docs.x402.org/core-concepts/facilitator>
    pub x402_facilitator_url: String,
}

impl Config {
    /// Read configuration from the process environment. Each field comes from one
    /// variable:
    ///
    /// * `BRAVE_SEARCH_API_KEY` (required): forwarded upstream as `X-Subscription-Token`.
    /// * `X402_FACILITATOR_URL` (required): base URL of the x402 facilitator.
    /// * `BRAVE_SEARCH_API_BASE_URL` (optional): defaults to the public Brave Search API endpoint.
    ///
    /// An absent required variable yields [`AppError::MissingConfig`]; a present but
    /// non-Unicode one yields [`AppError::InvalidConfig`].
    pub fn from_env() -> Result<Self, AppError> {
        let brave_search_api_key = require_var("BRAVE_SEARCH_API_KEY")?;
        let brave_search_api_base_url = env::var("BRAVE_SEARCH_API_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BRAVE_SEARCH_API_BASE_URL.to_string());
        let x402_facilitator_url = require_var("X402_FACILITATOR_URL")?;
        Ok(Self {
            brave_search_api_key,
            brave_search_api_base_url,
            x402_facilitator_url,
        })
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
