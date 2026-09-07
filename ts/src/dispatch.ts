/**
 * Payment-rail dispatch: classify each request by its payment headers.
 *
 * The payment handshake is dual-rail, so every request falls into one of four
 * states, decided purely by which payment headers are present:
 *
 * - **cold** (no payment proof): answered with the `402` challenge
 * - **x402** (`PAYMENT-SIGNATURE`): run through the x402 verify/settle flow
 * - **MPP** (`Authorization`): run through the MPP verify flow
 * - **collision** (both rails at once): rejected with `400`
 *
 * A deployment can disable a rail through `ENABLED_RAILS`. An attempt on a
 * disabled rail is answered with the cold `402`, which advertises only the rails
 * that can actually take a payment.
 */

import type { MiddlewareHandler } from "hono";
import type { Config } from "./config.js";
import { AppError } from "./error.js";
import { challenge, endpointLabel, type Metrics } from "./metrics.js";
import * as mpp from "./mpp.js";
import type { RestrictedAddressScreener } from "./screener.js";
import * as x402 from "./x402.js";

/**
 * The payment rail a request is attempting, determined solely by which payment
 * headers it carries.
 */
export type Rail =
  /** No payment proof: a cold request, answered with the `402` challenge. */
  | "none"
  /** An x402 attempt (`PAYMENT-SIGNATURE` present). */
  | "x402"
  /** An MPP attempt (`Authorization` present). */
  | "mpp"
  /** Both rails at once: a collision, rejected with `400`. */
  | "both";

/**
 * Classify a request by which payment headers it carries. The router names no
 * headers itself; it asks each rail module whether its proof is present.
 */
export function classify(headers: Headers): Rail {
  const hasX402 = x402.hasPayment(headers);
  const hasMpp = mpp.hasCredential(headers);
  if (hasX402 && hasMpp) {
    return "both";
  }
  if (hasX402) {
    return "x402";
  }
  return hasMpp ? "mpp" : "none";
}

/**
 * The dispatch state: one field per payment rail, built once at startup. A rail
 * is absent when the deployment disables it.
 */
export interface Context {
  x402: x402.Client | undefined;
  /** Payer screener, shared by every rail. Absent when screening is off. */
  screener: RestrictedAddressScreener | undefined;
  /** Where every rail records what happened. */
  metrics: Metrics;
}

/**
 * Assemble the dispatch context from config and the already-built screener (the
 * screener is built asynchronously at startup, so it is passed in rather than
 * built here).
 */
export function context(
  config: Config,
  screener: RestrictedAddressScreener | undefined,
  metrics: Metrics,
): Context {
  return {
    x402: config.x402 === undefined ? undefined : x402.client(config.x402, config.allowTestnet),
    screener,
    metrics,
  };
}

/**
 * Build the cold `402` from the rails' challenges, one header each, minted fresh
 * for this request. The body stays empty, since V2 clients read only the
 * headers.
 *
 * A rail that cannot produce its challenge is left out, so the `402` still
 * advertises whatever the other rail offers.
 */
export function cold402(ctx: Context, resource: string, method: string): Response {
  const headers = new Headers();
  const challenges = [
    ctx.x402 === undefined ? undefined : x402.challenge(ctx.x402, resource, method),
  ];
  for (const entry of challenges) {
    if (entry !== undefined) {
      headers.set(entry[0], entry[1]);
    }
  }
  return new Response(null, { status: 402, headers });
}

/** Collision `400`: both rails presented at once. Reuses the error envelope. */
export function collision400(): Response {
  return AppError.badRequest("send exactly one payment rail, not both").toResponse();
}

/**
 * Dispatch middleware for the paid routes: classify the request by its payment
 * headers and route each state to its rail. The router decides which rail runs,
 * never how a rail verifies.
 */
export function dispatch(ctx: Context): MiddlewareHandler {
  return async (c, next) => {
    const url = new URL(c.req.url);
    const endpoint = endpointLabel(url.pathname);
    const rail = classify(c.req.raw.headers);

    // Whichever rail runs answers for itself; what is left here is the request
    // that never reached one, and the reason it did not.
    if (rail === "both") {
      ctx.metrics.recordChallenge(endpoint, challenge.COLLISION);
      c.res = collision400();
      return;
    }
    if (rail === "x402" && ctx.x402 !== undefined) {
      // Assigned rather than returned: once the search has run, the context is
      // finalized and a returned response would be dropped in favour of it.
      c.res = await x402.handle(
        ctx.x402,
        ctx.screener,
        ctx.metrics,
        endpoint,
        c.req.raw.headers,
        async () => {
          await next();
          return c.res;
        },
      );
      return;
    }
    // Nothing here can pay: no proof, or proof on a rail the deployment disables.
    const reason = rail === "none" ? challenge.NO_PAYMENT : challenge.RAIL_DISABLED;
    ctx.metrics.recordChallenge(endpoint, reason);
    c.res = cold402(ctx, absoluteUri(c.req.raw), c.req.method);
  };
}

/** The pieces of a request that decide the resource URL it names. */
export interface UriParts {
  /** Path and query, exactly as the client sent them. */
  target: string;
  /** The `Host` header, when the client sent one. */
  host: string | undefined;
  /** The `X-Forwarded-Proto` header, when a proxy set one. */
  forwardedProto: string | undefined;
  /** The scheme of the request URI itself, when it carries one. */
  uriScheme: string | undefined;
  /** The authority of the request URI itself, when it carries one. */
  uriAuthority: string | undefined;
}

/**
 * Reconstruct the absolute URL the client requested for the cold `402`'s
 * `resource`. The query is kept verbatim, because a client compares this against
 * the URL it asked for and will not pay a challenge naming a different one. The
 * scheme and host are normalized the way URL parsers normalize the requested URL
 * before that comparison (lowercased, default port dropped), so a client sending
 * `Host: API.bx402.io:443` still recognizes the challenge as its own request. A
 * request with no host gets the bare path and query.
 */
export function absoluteUriFrom(parts: UriParts): string {
  const host = firstNonEmpty(parts.host, parts.uriAuthority);
  if (host === undefined) {
    return parts.target;
  }
  const scheme = schemeOf(parts).toLowerCase();
  return `${scheme}://${normalizedHost(host, scheme)}${parts.target}`;
}

/** `absoluteUriFrom` for a live request. */
export function absoluteUri(request: Request): string {
  const url = new URL(request.url);
  return absoluteUriFrom({
    target: `${url.pathname}${url.search}`,
    host: request.headers.get("host") ?? undefined,
    forwardedProto: request.headers.get("x-forwarded-proto") ?? undefined,
    uriScheme: url.protocol.replace(":", ""),
    uriAuthority: url.host,
  });
}

/**
 * The scheme the client used: `X-Forwarded-Proto` when a TLS-terminating proxy
 * sets it, else the URI's scheme, else `http`. Each proxy hop may append its own
 * value to the header, making it a comma-separated list, so only the first entry
 * (the hop the client spoke to) is read.
 */
function schemeOf(parts: UriParts): string {
  const forwarded = parts.forwardedProto?.split(",")[0]?.trim();
  return firstNonEmpty(forwarded, parts.uriScheme) ?? "http";
}

/** Lowercase `host` and drop the port when it is the scheme's default. */
function normalizedHost(host: string, scheme: string): string {
  const defaultPort = scheme === "http" ? ":80" : scheme === "https" ? ":443" : undefined;
  if (defaultPort === undefined) {
    return host.toLowerCase();
  }
  const bare = host.endsWith(defaultPort) ? host.slice(0, -defaultPort.length) : host;
  return bare.toLowerCase();
}

/** The first of `values` that is present and not empty. */
function firstNonEmpty(...values: (string | undefined)[]): string | undefined {
  return values.find((value) => value !== undefined && value !== "");
}
