/**
 * The paid surface: which Brave Search API endpoints we proxy and what each costs.
 *
 * One table, read by the router and by both payment rails, so the path we serve,
 * the price we advertise, and the price we verify against can never drift apart.
 * The table is protocol-neutral: it names no rail and imports nothing from one.
 */

/** Brave's Web Search and LLM Context rate, $5.00 per 1,000 requests. */
const SEARCH_RATE = 5_000;

/** Brave's Autosuggest and Spellcheck rate, $5.00 per 10,000 requests. */
export const UTILITY_RATE = 500;

/** One paid endpoint: the path we serve, what it costs, and how we label it. */
export interface Endpoint {
  /**
   * Path we accept and forward upstream unchanged, so our route and Brave's are
   * the same string.
   */
  readonly path: string;
  /**
   * Price for one request, in base units of the rail's currency.
   *
   * One number serves both rails because USDC and pathUSD both carry 6 decimals,
   * so `5_000` is $0.005 on either. A rail with a different scale would have to
   * convert rather than read this directly.
   */
  readonly priceBaseUnits: number;
  /** Label for this endpoint in the payment challenge. */
  readonly description: string;
}

/**
 * Every endpoint a client can pay for.
 *
 * Prices come from Brave's published rates. The rate card names only Web Search
 * and LLM Context, Autosuggest, and Spellcheck; the other search endpoints are
 * charged the Web rate, which never bills under the published tier.
 *
 * The Answers API is absent on purpose. It is metered per query and per token,
 * which a fixed price cannot express.
 */
export const ENDPOINTS: readonly Endpoint[] = [
  {
    path: "/res/v1/web/search",
    priceBaseUnits: SEARCH_RATE,
    description: "Brave Search API - Web / Search",
  },
  {
    path: "/res/v1/llm/context",
    priceBaseUnits: SEARCH_RATE,
    description: "Brave Search API - LLM Context",
  },
  {
    path: "/res/v1/news/search",
    priceBaseUnits: SEARCH_RATE,
    description: "Brave Search API - News / Search",
  },
  {
    path: "/res/v1/videos/search",
    priceBaseUnits: SEARCH_RATE,
    description: "Brave Search API - Video / Search",
  },
  {
    path: "/res/v1/images/search",
    priceBaseUnits: SEARCH_RATE,
    description: "Brave Search API - Image / Search",
  },
  {
    path: "/res/v1/summarizer/search",
    priceBaseUnits: SEARCH_RATE,
    description: "Brave Search API - Summarizer / Search",
  },
  {
    path: "/res/v1/local/place_search",
    priceBaseUnits: SEARCH_RATE,
    description: "Brave Search API - Place / Search",
  },
  {
    path: "/res/v1/local/pois",
    priceBaseUnits: SEARCH_RATE,
    description: "Brave Search API - Local / POIs",
  },
  {
    path: "/res/v1/local/descriptions",
    priceBaseUnits: SEARCH_RATE,
    description: "Brave Search API - Local / Descriptions",
  },
  {
    path: "/res/v1/suggest/search",
    priceBaseUnits: UTILITY_RATE,
    description: "Brave Search API - Autosuggest",
  },
  {
    path: "/res/v1/spellcheck/search",
    priceBaseUnits: UTILITY_RATE,
    description: "Brave Search API - Spellcheck",
  },
];

/** The endpoint served at `path`, or `undefined` for a path we do not sell. */
export function find(path: string): Endpoint | undefined {
  return ENDPOINTS.find((endpoint) => endpoint.path === path);
}
