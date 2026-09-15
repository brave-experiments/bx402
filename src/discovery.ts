/**
 * Service discovery: the machine-readable description of what this service
 * sells and how to pay for it.
 *
 * The shapes here follow MPP's payment discovery draft, which fixes where the
 * document lives: a discovery-aware client fetches `/openapi.json` and looks
 * nowhere else. The document links a buyer's guide in prose at `/llms.txt`.
 * Discovery is advisory; the runtime `402` challenge stays authoritative for
 * what a request must pay.
 *
 * This module is protocol-neutral: it names no rail's header, asset, or
 * chain. See `x402.ts` and `mpp.ts` for what each rail advertises.
 */

/** Where the discovery document is served. The spec fixes this exact path. */
export const DISCOVERY_PATH = "/openapi.json";

/** Where the buyer's guide is served, the path the document names under `docs.llms`. */
export const GUIDE_PATH = "/llms.txt";

/**
 * Cache lifetime for the discovery responses: the spec's recommended five
 * minutes for a service whose capabilities change infrequently. `public`
 * because the bodies are identical for every caller.
 */
export const CACHE_CONTROL = "public, max-age=300";

/**
 * One payment offer, as the spec's offer object defines it: exactly these
 * five fields and no others. In particular there is no recipient and no
 * network field, so the chain and token are named in `description`, the one
 * spec-defined place to state them in words.
 */
export interface Offer {
  /** What the payment buys. Every offer here charges for one request. */
  intent: "charge";
  /** The payment method identifier the matching challenge carries. */
  method: string;
  /** Price in base units of `currency`, as a decimal integer string. */
  amount: string;
  /** Address of the token the price is stated in. */
  currency: string;
  /** The token and chain in words, since no field states them. */
  description: string;
}
