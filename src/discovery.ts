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

import { readFileSync } from "node:fs";
import type { Context } from "./dispatch.js";
import { ENDPOINTS, type Endpoint } from "./endpoints.js";
import { AppError } from "./error.js";
import * as mpp from "./mpp.js";
import { VERSION } from "./version.js";
import * as x402 from "./x402.js";

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

/** Where the project documents itself for a human reader. */
const HOMEPAGE = "https://github.com/brave-experiments/bx402";

/** One paid path's `get` operation, as far as the document states it. */
interface Operation {
  summary: string;
  /**
   * Absent rather than empty when no rail is enabled: the extension requires
   * at least one offer, and a document must not advertise what cannot be
   * paid. The `402` declaration below stays either way.
   */
  "x-payment-info"?: { offers: Offer[] };
  responses: Record<string, { description: string }>;
}

/** The discovery document's shape, as far as this service states it. */
export interface DiscoveryDocument {
  openapi: string;
  info: { title: string; version: string };
  "x-service-info": {
    categories: string[];
    docs: { homepage: string; apiReference: string; llms: string };
  };
  paths: Record<string, { get: Operation }>;
}

/**
 * The discovery document: the spec's OpenAPI shape naming every paid path
 * and the offers the deployment's rails advertise for it.
 *
 * Composed from the same rail state the paid routes dispatch on, so the
 * document cannot drift from what a request is actually charged. It stays
 * advisory all the same: the runtime `402` challenge is authoritative, which
 * is why every operation declares that response.
 *
 * There is no `servers` block. The service never learns its public origin,
 * so paths stay relative to wherever the document was fetched from.
 */
export function document(ctx: Context): DiscoveryDocument {
  const paths = Object.fromEntries(
    ENDPOINTS.map((endpoint) => [endpoint.path, { get: operation(ctx, endpoint) }]),
  );
  return {
    openapi: "3.1.0",
    // The build version stands in for the API version, so a release bumps
    // the document even when the paid surface is unchanged.
    info: { title: "bx402", version: VERSION },
    "x-service-info": {
      categories: ["search"],
      // The guide link is relative for the same reason there is no `servers`
      // block: a reader that fetched the document has the origin to resolve
      // it against, and the service itself does not.
      docs: { homepage: HOMEPAGE, apiReference: `${HOMEPAGE}#endpoints`, llms: GUIDE_PATH },
    },
    paths,
  };
}

/**
 * One paid path's operation: each enabled rail's offers, in the order the
 * cold `402` lists the rails, with x402's testnet-first network order kept
 * inside its slice.
 */
function operation(ctx: Context, endpoint: Endpoint): Operation {
  const offers = [
    ...(ctx.x402 === undefined ? [] : x402.offers(ctx.x402, endpoint.path)),
    ...(ctx.mpp === undefined ? [] : mpp.offers(ctx.mpp, endpoint.path)),
  ];
  const stated: Operation = {
    summary: endpoint.description,
    responses: {
      "200": { description: "Successful response" },
      "402": { description: "Payment Required" },
    },
  };
  if (offers.length > 0) {
    stated["x-payment-info"] = { offers };
  }
  return stated;
}

/**
 * The buyer's guide, read from `llms.txt` beside the package manifest. The
 * build is `tsc` alone with no bundler, so the file cannot be imported as a
 * module; it sits one level up from `src` and from the compiled `dist` alike,
 * the same trick `version.ts` uses for the manifest.
 *
 * Called when the routes are built, never at module load. A missing or
 * unreadable file refuses startup: serving a 404 instead would mean a healthy
 * looking deployment whose document advertises a guide it does not have.
 */
export function guide(): string {
  try {
    return readFileSync(new URL("../llms.txt", import.meta.url), "utf8");
  } catch (err: unknown) {
    throw AppError.invalidConfig(
      `the buyer's guide llms.txt cannot be read: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
}
