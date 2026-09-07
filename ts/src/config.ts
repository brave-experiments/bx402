/**
 * Runtime configuration, read from the environment at startup.
 */

import { AppError } from "./error.js";

/** Default base URL when `BRAVE_SEARCH_API_BASE_URL` is unset. */
const DEFAULT_BRAVE_SEARCH_API_BASE_URL = "https://api.search.brave.com";

/**
 * Which payment rails the deployment turns on. This is the deployment-level
 * toggle from `ENABLED_RAILS`, not the per-request rail a client picks. The
 * default is every rail off, the starting point the parser adds to.
 */
export interface EnabledRails {
  x402: boolean;
  mpp: boolean;
}

/**
 * Parse an `ENABLED_RAILS` value: a comma-separated list of rail names, each
 * `x402` or `mpp`, or the word `none` alone to serve no rails at all. Anything
 * else is refused, so a typo can never silently disable a rail.
 */
export function parseEnabledRails(value: string): EnabledRails {
  const trimmed = value.trim();
  if (trimmed === "") {
    throw AppError.invalidConfig(
      "ENABLED_RAILS: empty; unset it to enable both rails, or list a subset of x402,mpp",
    );
  }
  // The switch for taking every rail offline at once. Only valid alone: naming a
  // rail next to `none` is a contradiction, not a list.
  if (trimmed === "none") {
    return { x402: false, mpp: false };
  }
  const rails: EnabledRails = { x402: false, mpp: false };
  for (const token of trimmed.split(",")) {
    switch (token.trim()) {
      case "x402":
        rails.x402 = true;
        break;
      case "mpp":
        rails.mpp = true;
        break;
      default:
        throw AppError.invalidConfig(
          `ENABLED_RAILS: unknown rail ${JSON.stringify(token.trim())}, ` +
            "expected none or a comma-separated subset of x402,mpp",
        );
    }
  }
  return rails;
}

/** Settings for the x402 rail. */
export interface X402Config {
  /**
   * Base URL of the x402 facilitator that verifies and settles payments.
   * Docs: https://docs.x402.org/core-concepts/facilitator
   */
  facilitatorUrl: string;
}

/** Settings for the MPP rail. */
export interface MppConfig {
  /**
   * Tempo RPC endpoint the MPP rail verifies and settles payments against. The
   * chain is discovered by querying the endpoint at startup. Testnet chains
   * require `ALLOW_TESTNET`.
   */
  rpcUrl: string;
  /**
   * Secret that marks MPP challenges as ours. Challenge ids are HMACs under this
   * key, so only a credential answering a challenge this service issued verifies.
   */
  secretKey: string;
}

/** Runtime configuration, read once from the environment at startup. */
export interface Config {
  /** Brave Search API key, forwarded upstream as `X-Subscription-Token`. */
  braveSearchApiKey: string;
  /**
   * Base URL of the Brave Search API. Overridable so tests can point at a mock
   * upstream; defaults to the public endpoint.
   */
  braveSearchApiBaseUrl: string;
  /** Settings for the x402 rail, absent when `ENABLED_RAILS` leaves the rail out. */
  x402: X402Config | undefined;
  /** Settings for the MPP rail, absent when `ENABLED_RAILS` leaves the rail out. */
  mpp: MppConfig | undefined;
  /**
   * S3 bucket holding the restricted-address list. Absent turns screening off,
   * the default for local and testnet runs.
   */
  restrictedAddressS3Bucket: string | undefined;
  /**
   * Accept testnet networks. Off in production, so faucet-money rails never pay
   * for real API traffic there.
   */
  allowTestnet: boolean;
}

/**
 * Read configuration from the process environment:
 *
 * - `BRAVE_SEARCH_API_KEY` (required): forwarded upstream as `X-Subscription-Token`.
 * - `ENABLED_RAILS` (optional): comma-separated subset of `x402,mpp` naming the
 *   rails to serve, or `none` to serve no rails; unset enables both. A disabled
 *   rail's variables are not read.
 * - `X402_FACILITATOR_URL` (required when the x402 rail is enabled): base URL of
 *   the x402 facilitator.
 * - `MPP_RPC_URL` (required when the MPP rail is enabled): Tempo RPC endpoint.
 * - `MPP_SECRET_KEY` (required when the MPP rail is enabled): HMAC secret binding
 *   MPP challenges to this service.
 * - `BRAVE_SEARCH_API_BASE_URL` (optional): defaults to the public endpoint.
 * - `RESTRICTED_ADDRESS_S3_BUCKET` (optional): unset or empty turns screening off.
 * - `ALLOW_TESTNET` (optional): `true` permits testnet networks, with each rail
 *   deciding what that admits.
 *
 * An absent required variable throws a missing-configuration error. The Rust
 * service also had an invalid-Unicode case per variable; Node hands every
 * environment variable over as a string, so that case cannot arise here.
 */
export function configFromEnv(env: NodeJS.ProcessEnv = process.env): Config {
  const braveSearchApiKey = requireVar(env, "BRAVE_SEARCH_API_KEY");
  const braveSearchApiBaseUrl = env.BRAVE_SEARCH_API_BASE_URL ?? DEFAULT_BRAVE_SEARCH_API_BASE_URL;
  // Unset enables both rails.
  const rails =
    env.ENABLED_RAILS === undefined
      ? { x402: true, mpp: true }
      : parseEnabledRails(env.ENABLED_RAILS);
  const bucket = env.RESTRICTED_ADDRESS_S3_BUCKET;
  return {
    braveSearchApiKey,
    braveSearchApiBaseUrl,
    x402: rails.x402 ? { facilitatorUrl: requireVar(env, "X402_FACILITATOR_URL") } : undefined,
    mpp: rails.mpp
      ? {
          rpcUrl: requireVar(env, "MPP_RPC_URL"),
          secretKey: requireVar(env, "MPP_SECRET_KEY"),
        }
      : undefined,
    restrictedAddressS3Bucket: bucket === undefined || bucket === "" ? undefined : bucket,
    allowTestnet: env.ALLOW_TESTNET === "true",
  };
}

/** Read a required environment variable, or fail startup naming it. */
function requireVar(env: NodeJS.ProcessEnv, name: string): string {
  const value = env[name];
  if (value === undefined) {
    throw AppError.missingConfig(name);
  }
  return value;
}
