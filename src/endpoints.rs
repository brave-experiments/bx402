//! The paid surface: which Brave Search API endpoints we proxy and what each costs.
//!
//! One table, read by the router and by both payment rails, so the path we serve,
//! the price we advertise, and the price we verify against can never drift apart.
//! The table is protocol-neutral: it names no rail and imports nothing from one.

/// Brave's Web Search and LLM Context rate, $5.00 per 1,000 requests.
const SEARCH_RATE: u64 = 5_000;

/// Brave's Autosuggest and Spellcheck rate, $5.00 per 10,000 requests.
const UTILITY_RATE: u64 = 500;

/// One paid endpoint: the path we serve, what it costs, and how we label it.
pub(crate) struct Endpoint {
    /// Path we accept and forward upstream unchanged, so our route and Brave's
    /// are the same string.
    pub(crate) path: &'static str,
    /// Price for one request, in base units of the rail's currency.
    ///
    /// One number serves both rails because USDC and pathUSD both carry 6
    /// decimals, so `5_000` is $0.005 on either. A rail with a different scale
    /// would have to convert rather than read this directly.
    pub(crate) price_base_units: u64,
    /// Label for this endpoint in the payment challenge.
    pub(crate) description: &'static str,
}

/// Every endpoint a client can pay for.
///
/// Prices come from Brave's published rates. The rate card names only Web Search
/// and LLM Context, Autosuggest, and Spellcheck; the other search endpoints are
/// charged the Web rate, which never bills under the published tier.
///
/// The Answers API is absent on purpose. It is metered per query and per token,
/// which a fixed price cannot express.
pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        path: "/res/v1/web/search",
        price_base_units: SEARCH_RATE,
        description: "Brave Search API - Web / Search",
    },
    Endpoint {
        path: "/res/v1/llm/context",
        price_base_units: SEARCH_RATE,
        description: "Brave Search API - LLM Context",
    },
    Endpoint {
        path: "/res/v1/news/search",
        price_base_units: SEARCH_RATE,
        description: "Brave Search API - News / Search",
    },
    Endpoint {
        path: "/res/v1/videos/search",
        price_base_units: SEARCH_RATE,
        description: "Brave Search API - Video / Search",
    },
    Endpoint {
        path: "/res/v1/images/search",
        price_base_units: SEARCH_RATE,
        description: "Brave Search API - Image / Search",
    },
    Endpoint {
        path: "/res/v1/summarizer/search",
        price_base_units: SEARCH_RATE,
        description: "Brave Search API - Summarizer / Search",
    },
    Endpoint {
        path: "/res/v1/local/place_search",
        price_base_units: SEARCH_RATE,
        description: "Brave Search API - Place / Search",
    },
    Endpoint {
        path: "/res/v1/local/pois",
        price_base_units: SEARCH_RATE,
        description: "Brave Search API - Local / POIs",
    },
    Endpoint {
        path: "/res/v1/local/descriptions",
        price_base_units: SEARCH_RATE,
        description: "Brave Search API - Local / Descriptions",
    },
    Endpoint {
        path: "/res/v1/suggest/search",
        price_base_units: UTILITY_RATE,
        description: "Brave Search API - Autosuggest",
    },
    Endpoint {
        path: "/res/v1/spellcheck/search",
        price_base_units: UTILITY_RATE,
        description: "Brave Search API - Spellcheck",
    },
];

/// The endpoint served at `path`, or `None` for a path we do not sell.
pub(crate) fn find(path: &str) -> Option<&'static Endpoint> {
    ENDPOINTS.iter().find(|endpoint| endpoint.path == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_path_is_listed_once() {
        let distinct: std::collections::HashSet<_> =
            ENDPOINTS.iter().map(|endpoint| endpoint.path).collect();
        assert_eq!(distinct.len(), ENDPOINTS.len());
    }

    #[test]
    fn the_utility_rate_applies_to_suggest_and_spellcheck() {
        let utility: Vec<_> = ENDPOINTS
            .iter()
            .filter(|endpoint| endpoint.price_base_units == UTILITY_RATE)
            .map(|endpoint| endpoint.path)
            .collect();
        assert_eq!(
            utility,
            ["/res/v1/suggest/search", "/res/v1/spellcheck/search"]
        );
    }

    #[test]
    fn find_matches_a_served_path_exactly() {
        let found = find("/res/v1/images/search").expect("images is served");
        assert_eq!(found.price_base_units, SEARCH_RATE);

        // The Answers API is not sold, and a prefix of a served path is not a
        // served path.
        assert!(find("/res/v1/chat/completions").is_none());
        assert!(find("/res/v1/images").is_none());
    }
}
