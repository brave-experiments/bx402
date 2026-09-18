/**
 * The payment rails, composed for the router.
 *
 * This is the one module outside `x402.ts` and `mpp.ts` that names both rails;
 * everything downstream iterates whatever it hands over. The rail modules may
 * import types from here and nothing else, so at runtime the dependency runs
 * one way: the router reads this module, and this module reads the rails.
 */

import * as mpp from "./mpp.js";
import * as x402 from "./x402.js";

/** The name of a payment rail, as metrics and classification spell it. */
export type RailName = typeof x402.RAIL | typeof mpp.RAIL;

/**
 * Detection per rail, stated apart from the built clients: whether a request
 * attempts a rail depends only on its headers, and a disabled rail must still
 * be recognized so its attempt can be answered with the cold `402`.
 */
const DETECTORS = [
  { name: x402.RAIL, detects: x402.hasPayment },
  { name: mpp.RAIL, detects: mpp.hasCredential },
] as const;

/** Which rails' payment proofs the request carries, in advertisement order. */
export function detect(headers: Headers): RailName[] {
  return DETECTORS.filter((rail) => rail.detects(headers)).map((rail) => rail.name);
}
