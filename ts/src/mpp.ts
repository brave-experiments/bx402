/**
 * The MPP payment rail: everything specific to the Machine Payments Protocol
 * lives here.
 *
 * See `x402.ts` for the x402 rail and `dispatch.ts` for the neutral router that
 * classifies each request and delegates to whichever rail it is paying on.
 */

/**
 * MPP carries its credential in the `Authorization` request header. Dispatch
 * keys on presence alone, so any value counts as an attempt on this rail.
 */
const CREDENTIAL_HEADER = "authorization";

/** What this rail calls itself in metrics. */
export const RAIL = "mpp";

/** Whether the request carries an MPP credential. */
export function hasCredential(headers: Headers): boolean {
  return headers.has(CREDENTIAL_HEADER);
}
