/**
 * The x402 payment rail: everything specific to x402 lives here.
 *
 * See `mpp.ts` for the MPP rail and `dispatch.ts` for the neutral router that
 * classifies each request and delegates to whichever rail it is paying on.
 */

import { isDeepStrictEqual } from "node:util";
import { createCdpAuthHeaders } from "@coinbase/x402";
import type { FacilitatorConfig } from "@x402/core/http";
import { encodePaymentRequiredHeader, encodePaymentResponseHeader } from "@x402/core/http";
import { HTTPFacilitatorClient } from "@x402/core/server";
import type { PaymentPayload, PaymentRequirements } from "@x402/core/types";
import { getDefaultAsset } from "@x402/evm";
import type { X402Config } from "./config.js";
import { ENDPOINTS, find } from "./endpoints.js";
import { AppError, jsonError } from "./error.js";
import { log } from "./log.js";
import { type Metrics, outcome, step } from "./metrics.js";
import type { RestrictedAddressScreener } from "./screener.js";

/**
 * x402 V2 carries its payment proof in the `PAYMENT-SIGNATURE` request header.
 * V1's `X-PAYMENT` is deliberately not recognized: the service is V2-only, so a
 * V1 client carries no payment we accept and falls through to the cold `402`.
 */
const V2_PAYMENT_HEADER = "payment-signature";

/**
 * x402 V2 returns the settlement receipt in the `Payment-Response` response
 * header as base64-encoded JSON, the dual of the `PAYMENT-SIGNATURE` request
 * header.
 */
const PAYMENT_RECEIPT_HEADER = "payment-response";

/**
 * x402 V2 serves the cold `402` requirements in the `Payment-Required` response
 * header as base64-encoded JSON. Header-only clients read nothing else.
 */
export const PAYMENT_REQUIRED_HEADER = "payment-required";

/** The EVM treasury address that receives x402 payments (`payTo`). */
const PAY_TO_EVM = "0xbd9420A98a7Bd6B89765e5715e169481602D9c3d";

/** What this rail calls itself in metrics. */
export const RAIL = "x402";

/** Base mainnet and the Base Sepolia testnet, in CAIP-2 form. */
const BASE = "eip155:8453" as const;
const BASE_SEPOLIA = "eip155:84532" as const;

/** How the advertised asset moves. Every offer we make is an EIP-3009 transfer. */
const ASSET_TRANSFER_METHOD = "eip3009";

/** How long an offer stays payable, in seconds. */
const MAX_TIMEOUT_SECONDS = 300;

/** The error line every cold `402` envelope carries. */
const PAYMENT_REQUIRED = "Payment required";

/**
 * Shared message for a payment we could not read at all, whether the header, its
 * base64, its JSON, or the offer it names is unusable.
 */
const MALFORMED_PAYMENT = "malformed x402 payment payload";

/** Shared message for every refused payment, so refusals are indistinguishable. */
const GENERIC_REJECTION = "x402 payment did not verify";

/**
 * Shared message for a payment we could not settle, whether the facilitator
 * declined it or was unreachable, so the client cannot tell the two apart.
 */
const SETTLE_FAILED = "x402 payment could not be settled";

/** Whether the request carries an x402 V2 payment proof. */
export function hasPayment(headers: Headers): boolean {
  return headers.has(V2_PAYMENT_HEADER);
}

/**
 * Build the list of payment offers we advertise and verify against. The same
 * entries seed both the cold `402` and payment verification, so there is one
 * source of truth for what we charge: real USDC on Base mainnet, plus faucet
 * USDC on Base Sepolia when the testnet is allowed.
 *
 * The order states the deployment's preference. A deployment that allows the
 * testnet leads with it, so clients that take the first offer they support pay
 * with faucet money rather than the real thing.
 */
export function accepts(allowTestnet: boolean): Map<string, PaymentRequirements[]> {
  const networks = allowTestnet ? [BASE_SEPOLIA, BASE] : [BASE];
  const table = new Map<string, PaymentRequirements[]>();
  for (const endpoint of ENDPOINTS) {
    const offers = networks.map((network) => {
      const asset = getDefaultAsset(network);
      return {
        scheme: "exact",
        network,
        amount: String(endpoint.priceBaseUnits),
        asset: asset.asset,
        payTo: PAY_TO_EVM,
        maxTimeoutSeconds: MAX_TIMEOUT_SECONDS,
        extra: {
          assetTransferMethod: ASSET_TRANSFER_METHOD,
          name: asset.name,
          version: asset.version,
        },
      } as PaymentRequirements;
    });
    table.set(endpoint.path, offers);
  }
  return table;
}

/**
 * The route binding mppx needs before it will sign, under the key it reads.
 *
 * Both members are required: dropping either fails the payment with "requires
 * route binding", even though nothing reads inside `schema`. We do not verify
 * the binding. It only feeds the client's nonce, and the facilitator is what
 * checks the signature.
 */
function routeExtensions(method: string): Record<string, unknown> {
  return {
    mppx: {
      info: { method },
      schema: { type: "object" },
    },
  };
}

/**
 * The x402 facilitator client and the payment offers we accept, wrapped so the
 * rest of the service names this module's type rather than the SDK's.
 */
export interface Client {
  facilitator: HTTPFacilitatorClient;
  /**
   * Offers per paid path, built once at startup. The cold `402` for a path
   * advertises exactly that path's entries and a payment must accept one of
   * them, so the two can never disagree and no path is payable at another's
   * price.
   */
  accepts: Map<string, PaymentRequirements[]>;
}

/**
 * The host CDP credentials sign for. The signed tokens name this host and the
 * CDP verify and settle paths, so they authenticate nowhere else.
 */
const CDP_FACILITATOR_HOST = "api.cdp.coinbase.com";

/**
 * Build the x402 facilitator client from the rail's settings. A bad
 * `X402_FACILITATOR_URL` is a startup misconfiguration.
 */
export function client(rail: X402Config, allowTestnet: boolean): Client {
  let url: URL;
  try {
    // Parsed only to reject a URL we could never call; the string is passed on
    // as configured, so the facilitator sees exactly the base it was given.
    url = new URL(rail.facilitatorUrl);
  } catch (err: unknown) {
    throw AppError.invalidConfig(
      `X402_FACILITATOR_URL: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
  // A signed CDP token sent elsewhere would let that host replay it against CDP
  // for the token's lifetime, so credentials pair only with the CDP host.
  if (rail.cdp !== undefined && url.host !== CDP_FACILITATOR_HOST) {
    throw AppError.invalidConfig(
      `CDP_API_KEY_ID is set but X402_FACILITATOR_URL does not point at ${CDP_FACILITATOR_HOST}`,
    );
  }
  const config: FacilitatorConfig = { url: rail.facilitatorUrl };
  if (rail.cdp !== undefined) {
    // The SDK types the hook as optional but always builds one; the guard only
    // satisfies the type checker.
    const createAuthHeaders = createCdpAuthHeaders(rail.cdp.apiKeyId, rail.cdp.apiKeySecret);
    if (createAuthHeaders !== undefined) {
      config.createAuthHeaders = createAuthHeaders;
    }
  }
  return {
    facilitator: new HTTPFacilitatorClient(config),
    accepts: accepts(allowTestnet),
  };
}

/**
 * x402's part of the cold `402`: the V2 `PaymentRequired` envelope for
 * `resource`, base64 encoded into the `Payment-Required` header, the V2
 * transport clients read. `undefined` if it cannot be encoded, leaving the `402`
 * advertising MPP alone.
 */
export function challenge(
  client: Client,
  resource: string,
  method: string,
): [string, string] | undefined {
  const path = pathOf(resource);
  // Advertise this endpoint's price and no other. A client that is offered every
  // price at once could pay the cheapest and call the dearest.
  const endpoint = find(path);
  const offers = client.accepts.get(path);
  if (endpoint === undefined || offers === undefined) {
    log.error(`no x402 offer for a paid path: ${path}`);
    return undefined;
  }
  const envelope = {
    x402Version: 2,
    error: PAYMENT_REQUIRED,
    resource: {
      url: resource,
      description: endpoint.description,
      mimeType: "application/json",
    },
    accepts: offers,
    extensions: routeExtensions(method),
  };
  try {
    return [PAYMENT_REQUIRED_HEADER, encodePaymentRequiredHeader(envelope)];
  } catch {
    log.error("x402 challenge could not be encoded as a header");
    return undefined;
  }
}

/** The path part of a resource URL, or the whole string when it is not a URL. */
function pathOf(resource: string): string {
  try {
    return new URL(resource, "http://placeholder.invalid").pathname;
  } catch {
    return resource;
  }
}

/**
 * Drive the x402 pay flow for a request that carries a payment proof: verify,
 * run the search, then settle, each step gating the next. A caller is never
 * charged for a response they do not get, nor served one they did not pay for:
 *
 * - payment missing, malformed, or rejected: `402`, before any upstream call.
 * - facilitator unreachable on verify: `502`.
 * - search fails (4xx or 5xx): relayed as is, settlement skipped.
 * - settlement fails: `502`, the response body withheld.
 */
export async function handle(
  client: Client,
  screener: RestrictedAddressScreener | undefined,
  metrics: Metrics,
  endpoint: string,
  headers: Headers,
  runSearch: () => Promise<Response>,
): Promise<Response> {
  // Every exit below records how the payment ended, so no path goes uncounted.
  const ended = (label: string, response: Response): Response => {
    metrics.recordPayment(RAIL, endpoint, label);
    return response;
  };

  const decoded = decodePayment(headers);
  if (decoded === undefined) {
    return ended(outcome.MALFORMED, paymentRejected(MALFORMED_PAYMENT));
  }
  const { payload, accepted, payer } = decoded;

  // The payer must accept an offer we advertised for the path it is calling, so
  // it can name neither its own price, asset, and recipient, nor another
  // endpoint's cheaper offer. Refused like any other payment we decline.
  const offer = client.accepts.get(endpoint)?.find((entry) => isDeepStrictEqual(entry, accepted));
  if (offer === undefined) {
    return ended(outcome.NO_OFFER, paymentRejected(GENERIC_REJECTION));
  }

  // Screen the payer before any facilitator or upstream call, so a blocked
  // signer touches neither.
  if (screener !== undefined) {
    const denied = await screener.requireAllowed(payer, paymentRejected(GENERIC_REJECTION));
    if (denied !== undefined) {
      return ended(outcome.SCREENED_OUT, denied);
    }
  }

  // Verify before doing any work. A facilitator we cannot reach is our failure,
  // not the client's, so it is a 502 rather than a 402.
  const verifyStarted = performance.now();
  let verified: { isValid: boolean };
  try {
    verified = await client.facilitator.verify(payload, offer);
  } catch (err: unknown) {
    metrics.recordPaymentStep(RAIL, step.VERIFY, seconds(verifyStarted));
    log.error(`x402 facilitator verify failed: ${describe(err)}`);
    return ended(outcome.NETWORK_UNAVAILABLE, gatewayError("payment facilitator unavailable"));
  }
  metrics.recordPaymentStep(RAIL, step.VERIFY, seconds(verifyStarted));
  if (verified.isValid !== true) {
    return ended(outcome.REFUSED, paymentRejected(GENERIC_REJECTION));
  }

  const response = await runSearch();
  if (!response.ok) {
    return ended(outcome.UPSTREAM_FAILED, response);
  }

  // The value we verified settles unchanged. Withhold the already produced body
  // unless it settles.
  const settleStarted = performance.now();
  let receipt: { success: boolean };
  try {
    receipt = await client.facilitator.settle(payload, offer);
  } catch (err: unknown) {
    metrics.recordPaymentStep(RAIL, step.SETTLE, seconds(settleStarted));
    log.error(`x402 facilitator settle failed: ${describe(err)}`);
    return ended(outcome.SETTLE_FAILED, gatewayError(SETTLE_FAILED));
  }
  metrics.recordPaymentStep(RAIL, step.SETTLE, seconds(settleStarted));
  if (receipt.success !== true) {
    log.error(`x402 facilitator reported settlement failure: ${JSON.stringify(receipt)}`);
    return ended(outcome.SETTLE_FAILED, gatewayError(SETTLE_FAILED));
  }

  metrics.recordPayment(RAIL, endpoint, outcome.SETTLED);
  // The price comes from the catalog, so what we count as earned is what we
  // advertised rather than anything the payer said.
  const sold = find(endpoint);
  if (sold !== undefined) {
    metrics.recordCharge(RAIL, endpoint, sold.priceBaseUnits);
  }
  return attachReceipt(response, receipt);
}

/**
 * Decode the client's base64 JSON payment from `PAYMENT-SIGNATURE` into the raw
 * payload, the offer the payer says it accepted, and the payer to screen.
 * `undefined` if the header is absent or not the base64 JSON required.
 *
 * The payer is present only for the eip3009 payload we advertise; a payload
 * without `authorization.from` (a permit2 shape, say) yields none, which the
 * caller rejects before any facilitator call when screening is on.
 */
export function decodePayment(headers: Headers):
  | {
      payload: PaymentPayload;
      accepted: PaymentRequirements;
      payer: string | undefined;
    }
  | undefined {
  const header = headers.get(V2_PAYMENT_HEADER);
  if (header === null) {
    return undefined;
  }
  let payload: Record<string, unknown>;
  try {
    payload = JSON.parse(Buffer.from(header, "base64").toString("utf8"));
  } catch {
    return undefined;
  }
  if (typeof payload !== "object" || payload === null) {
    return undefined;
  }
  const accepted = payload.accepted;
  if (typeof accepted !== "object" || accepted === null) {
    return undefined;
  }
  return {
    payload: payload as unknown as PaymentPayload,
    accepted: accepted as PaymentRequirements,
    payer: payerAddress(payload),
  };
}

/**
 * The payer to screen: the eip3009 `authorization.from`, lowercased to the
 * screener's canonical form (EVM addresses are case-insensitive hex).
 */
function payerAddress(payload: Record<string, unknown>): string | undefined {
  const scheme = payload.payload;
  if (typeof scheme !== "object" || scheme === null) {
    return undefined;
  }
  const authorization = (scheme as Record<string, unknown>).authorization;
  if (typeof authorization !== "object" || authorization === null) {
    return undefined;
  }
  const from = (authorization as Record<string, unknown>).from;
  return typeof from === "string" ? from.toLowerCase() : undefined;
}

/**
 * Attach the settlement receipt as the base64 `Payment-Response` header the
 * client reads back, leaving the response body untouched.
 */
function attachReceipt(response: Response, receipt: { success: boolean }): Response {
  const headers = new Headers(response.headers);
  try {
    headers.set(
      PAYMENT_RECEIPT_HEADER,
      // biome-ignore lint/suspicious/noExplicitAny: the SDK's settle response shape.
      encodePaymentResponseHeader(receipt as any),
    );
  } catch {
    return response;
  }
  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers,
  });
}

/** A `402` telling the client their x402 payment was missing, malformed, or rejected. */
function paymentRejected(detail: string): Response {
  return jsonError(402, detail);
}

/** A `502` for a payment we could neither verify nor settle through the facilitator. */
function gatewayError(detail: string): Response {
  return jsonError(502, detail);
}

/** Elapsed seconds since `started`, the unit every duration metric records. */
function seconds(started: number): number {
  return (performance.now() - started) / 1000;
}

/** The message of a failure, for one log line. */
function describe(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
