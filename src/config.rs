//! Runtime configuration, read from the environment at startup.

use std::env;

/// Default base URL when `BRAVE_SEARCH_API_BASE_URL` is unset.
const DEFAULT_BRAVE_SEARCH_API_BASE_URL: &str = "https://api.search.brave.com";

/// Runtime configuration, read once from the environment at startup.
pub struct Config {
    /// Brave Search API key, forwarded upstream as `X-Subscription-Token`.
    pub brave_search_api_key: String,
    /// Base URL of the Brave Search API. Overridable so tests can point at a
    /// mock server; defaults to the public endpoint.
    pub brave_search_api_base_url: String,
}

impl Config {
    /// Read configuration from the process environment.
    ///
    /// `BRAVE_SEARCH_API_KEY` is required; a missing or non-Unicode value is an
    /// error. `BRAVE_SEARCH_API_BASE_URL` is optional and defaults to
    /// [`DEFAULT_BRAVE_SEARCH_API_BASE_URL`]. Returns a human-readable message
    /// on failure.
    pub fn from_env() -> Result<Self, String> {
        let brave_search_api_key = env::var("BRAVE_SEARCH_API_KEY")
            .map_err(|_| "BRAVE_SEARCH_API_KEY must be set".to_string())?;
        let brave_search_api_base_url = env::var("BRAVE_SEARCH_API_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BRAVE_SEARCH_API_BASE_URL.to_string());
        Ok(Self {
            brave_search_api_key,
            brave_search_api_base_url,
        })
    }
}
