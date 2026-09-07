/**
 * The upstream proxy: one Brave Search API call per paid request.
 */

import { Agent, type Dispatcher, request } from "undici";
import type { Config } from "./config.js";
import { AppError, type UpstreamFailure } from "./error.js";

/** How long to wait for the upstream connection to establish before giving up. */
const CONNECT_TIMEOUT_MS = 5_000;

/**
 * Overall deadline for one upstream search, so a stalled Brave Search API cannot
 * pin the request. A timeout relays as a `502`, like any transport failure.
 */
const SEARCH_TIMEOUT_MS = 15_000;

/** One upstream response, read to the end. */
export interface UpstreamResponse {
  status: number;
  contentType: string | undefined;
  body: Buffer;
}

/**
 * The connection pool for upstream searches. Keeping one pool across requests
 * reuses TLS sessions, which is most of the latency on a short search.
 */
export function searchClient(): Agent {
  return new Agent({ connect: { timeout: CONNECT_TIMEOUT_MS } });
}

/**
 * Fetch one upstream search and read it to the end.
 *
 * Forwards the query string verbatim, attaches the API key as a header, then
 * hands back the upstream status, content type, and body for the caller to relay
 * byte for byte.
 */
export async function search(
  client: Dispatcher,
  config: Config,
  path: string,
  query: string,
): Promise<UpstreamResponse> {
  const url =
    query === ""
      ? `${config.braveSearchApiBaseUrl}${path}`
      : `${config.braveSearchApiBaseUrl}${path}?${query}`;

  let response: Dispatcher.ResponseData;
  try {
    response = await request(url, {
      dispatcher: client,
      method: "GET",
      headers: {
        "X-Subscription-Token": config.braveSearchApiKey,
        accept: "application/json",
      },
      signal: AbortSignal.timeout(SEARCH_TIMEOUT_MS),
    });
  } catch (err: unknown) {
    throw AppError.upstream(transportFailure(err));
  }

  let body: Buffer;
  try {
    body = Buffer.from(await response.body.arrayBuffer());
  } catch {
    // A body that starts and then fails part way through, which the Rust client
    // reported as a decode failure.
    throw AppError.upstream("decode");
  }

  const contentType = response.headers["content-type"];
  return {
    status: response.statusCode,
    contentType: Array.isArray(contentType) ? contentType[0] : contentType,
    body,
  };
}

/**
 * The kind of failure behind an upstream error, as a label value. A fixed set,
 * so a failing upstream cannot grow the number of series.
 */
function transportFailure(err: unknown): UpstreamFailure {
  const code = typeof err === "object" && err !== null && "code" in err ? String(err.code) : "";
  const name = err instanceof Error ? err.name : "";

  // A deadline, whether it struck while connecting, waiting for headers, or
  // reading the body.
  if (
    name === "TimeoutError" ||
    // The only abort this code arms is the deadline above.
    code === "UND_ERR_ABORTED" ||
    name === "HeadersTimeoutError" ||
    name === "BodyTimeoutError" ||
    code === "UND_ERR_CONNECT_TIMEOUT" ||
    code === "UND_ERR_HEADERS_TIMEOUT" ||
    code === "UND_ERR_BODY_TIMEOUT"
  ) {
    return "timeout";
  }
  // Nothing answered at the other end: refused, unresolvable, or unreachable.
  if (
    code === "ECONNREFUSED" ||
    code === "ENOTFOUND" ||
    code === "EAI_AGAIN" ||
    code === "EHOSTUNREACH" ||
    code === "ENETUNREACH" ||
    code === "ECONNRESET" ||
    code === "EPIPE"
  ) {
    return "connect";
  }
  return "transport";
}
