/**
 * Screens payers against a list of restricted addresses.
 *
 * The list is an S3 bucket. Each prohibited address is stored as a key, base64url
 * encoded. Checking membership is one `HeadObject`:
 *
 * - key present (`200`): on the list
 * - key absent (`404`): not on the list
 *
 * The screener errs on the side of caution. Only a `404` means "not on the list".
 * Any other error (timeout, misconfiguration, S3 outage) blocks the payment
 * rather than letting it through unchecked.
 *
 * The module is chain agnostic. It screens the identifier string exactly as
 * given and knows nothing about the address itself. The screen returns a plain
 * outcome, and one shared helper turns that outcome into the caller's refusal, so
 * every rail denies the same way.
 *
 * Canonicalization belongs to the caller, because it differs per chain:
 *
 * - EVM: addresses are case-insensitive hex, so the rail lowercases them.
 * - Solana: addresses are case-sensitive base58, so the rail passes them as is.
 *
 * One screener backs every chain this way. Adapted from the Go reference
 * (`brave-intl/compliance-ops`).
 */

import { HeadObjectCommand, S3Client } from "@aws-sdk/client-s3";
import { NodeHttpHandler } from "@smithy/node-http-handler";
import type { Config } from "./config.js";
import { serviceUnavailable } from "./error.js";
import { log } from "./log.js";
import { type Metrics, screening } from "./metrics.js";

/** Canary key used to probe the bucket at startup. */
export const CANARY_KEY = "bx402.canary";

/**
 * How long a screen may take before it counts as unavailable. Bounds the paid
 * request path; the S3 client's own timeout is a looser startup backstop.
 */
const SCREEN_TIMEOUT_MS = 2_000;

/**
 * How long the startup probe may take. Kept generous because the first call also
 * resolves credentials.
 */
const STARTUP_TIMEOUT_MS = 10_000;

/**
 * The two definite answers a screen can give.
 *
 * When the screener cannot give a definite answer it throws `ScreenError`
 * instead, and the caller must still deny the payment.
 */
export type Screening = "blocked" | "allowed";

/**
 * The screen could not give a definite answer: a timeout, a misconfiguration,
 * an access denial, or any other S3 failure.
 *
 * The caller must treat this as a denial. The underlying SDK error is kept as the
 * cause for logs and is never shown to clients.
 */
export class ScreenError extends Error {
  constructor(cause: unknown) {
    super("address screening unavailable", { cause });
    this.name = "ScreenError";
  }
}

/** Screens identifiers against the restricted-address S3 bucket. */
export class RestrictedAddressScreener {
  constructor(
    private readonly client: S3Client,
    private readonly bucket: string,
    private readonly metrics: Metrics,
  ) {}

  /**
   * Screen one identifier, exactly as given, within the screening deadline.
   *
   * The caller must pass the already-canonical form (casing differs per chain;
   * see the module docs). The identifier is base64url-encoded into the S3 key. A
   * lookup that outlives the deadline is a `ScreenError` like any other failure,
   * so the caller denies it the same way.
   */
  async screen(identifier: string): Promise<Screening> {
    const key = Buffer.from(identifier, "utf8").toString("base64url");
    return this.headKey(key, SCREEN_TIMEOUT_MS);
  }

  /**
   * Screen `identifier` and decide whether the payment proceeds. A returned
   * response is the refusal to send instead of serving the request; `undefined`
   * means the payment proceeds.
   *
   * A payment with no identifier to screen is refused like a listed one, with
   * the rail's `rejected` response. A screening failure is logged and refused
   * with the shared `503`.
   */
  async requireAllowed(
    identifier: string | undefined,
    rejected: Response,
  ): Promise<Response | undefined> {
    if (identifier === undefined) {
      this.metrics.recordScreening(screening.UNIDENTIFIED);
      return rejected;
    }
    let outcome: Screening;
    try {
      outcome = await this.screen(identifier);
    } catch (err: unknown) {
      this.metrics.recordScreening(screening.ERROR);
      log.error(`address screening failed: ${describe(err)}`);
      return serviceUnavailable();
    }
    if (outcome === "blocked") {
      this.metrics.recordScreening(screening.BLOCKED);
      return rejected;
    }
    this.metrics.recordScreening(screening.ALLOWED);
    return undefined;
  }

  /**
   * Look up one exact S3 key with `HeadObject`:
   *
   * - `200`: key present, returns `blocked`
   * - `404 NotFound`: key absent, returns `allowed`
   * - anything else: throws `ScreenError`, so the caller blocks
   */
  async headKey(key: string, timeoutMs: number): Promise<Screening> {
    try {
      await this.client.send(new HeadObjectCommand({ Bucket: this.bucket, Key: key }), {
        abortSignal: AbortSignal.timeout(timeoutMs),
      });
      // Key exists, so the address is on the list.
      return "blocked";
    } catch (err: unknown) {
      // A 404 is the only way to be allowed.
      if (isNotFound(err)) {
        return "allowed";
      }
      // Any other failure blocks the payment.
      throw new ScreenError(err);
    }
  }
}

/** Whether an S3 failure is the definite "no such key" answer. */
function isNotFound(err: unknown): boolean {
  if (typeof err !== "object" || err === null) {
    return false;
  }
  const name = "name" in err ? err.name : undefined;
  const status =
    "$metadata" in err && typeof err.$metadata === "object" && err.$metadata !== null
      ? (err.$metadata as { httpStatusCode?: number }).httpStatusCode
      : undefined;
  return name === "NotFound" || status === 404;
}

/** The message and cause chain of a failure, for one log line. */
function describe(err: unknown): string {
  if (!(err instanceof Error)) {
    return String(err);
  }
  return err.cause === undefined ? err.message : `${err.message}: ${describe(err.cause)}`;
}

/** Outcome of `initScreener`, for the startup log line. */
export type Status = { enabled: true; bucket: string } | { enabled: false };

/** The startup log line for a screening status. */
export function statusLine(status: Status): string {
  return status.enabled
    ? `✓ enabled (bucket=${status.bucket})`
    : "✗ disabled (RESTRICTED_ADDRESS_S3_BUCKET not set)";
}

/**
 * Build the screener from config and prove it works before serving traffic.
 *
 * - bucket unset: screening off, no screener
 * - bucket set: builds the AWS client and probes the bucket once. A reachable
 *   bucket yields a screener; any probe failure aborts startup, so a
 *   misconfigured screener never serves traffic.
 */
export async function initScreener(
  config: Config,
  metrics: Metrics,
): Promise<{ screener: RestrictedAddressScreener | undefined; status: Status }> {
  const bucket = config.restrictedAddressS3Bucket;
  if (bucket === undefined) {
    return { screener: undefined, status: { enabled: false } };
  }
  // Cap the S3 call so a stalled connection cannot hang startup.
  const client = new S3Client({
    requestHandler: new NodeHttpHandler({
      connectionTimeout: STARTUP_TIMEOUT_MS,
      requestTimeout: STARTUP_TIMEOUT_MS,
    }),
  });
  return initWith(client, bucket, metrics);
}

/**
 * The probe, split out so tests can inject a client pointed at a mock bucket.
 * Only reached when a bucket is configured, so it always yields a screener on
 * success.
 */
export async function initWith(
  client: S3Client,
  bucket: string,
  metrics: Metrics,
): Promise<{ screener: RestrictedAddressScreener; status: Status }> {
  const screener = new RestrictedAddressScreener(client, bucket, metrics);
  // A reachable bucket (404, or even a 200) proves credentials and permissions
  // work. On failure the real cause travels as the cause of this error.
  try {
    await screener.headKey(CANARY_KEY, STARTUP_TIMEOUT_MS);
  } catch (err: unknown) {
    throw new Error(`restricted address screening probe failed for bucket ${bucket}`, {
      cause: err,
    });
  }
  return { screener, status: { enabled: true, bucket } };
}
