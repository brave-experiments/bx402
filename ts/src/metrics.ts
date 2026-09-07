/**
 * Prometheus metrics: the registry, and the endpoint that exposes it.
 *
 * Metrics are served from a listener of their own rather than from the router
 * that takes public traffic. The page names every paid endpoint, how often
 * payments are refused, and how much has been charged, so keeping it off the
 * public port is a property of the process rather than of a proxy rule somewhere
 * upstream.
 */

import { serve } from "@hono/node-server";
import { Counter, Gauge, Histogram, openMetricsContentType, Registry } from "@prometheus-io/client";
import { Hono, type MiddlewareHandler } from "hono";
import { find } from "./endpoints.js";
import { log } from "./log.js";
import { VERSION } from "./version.js";

/**
 * Address the metrics listener binds. Fixed rather than configurable, like the
 * main bind, and above 1024 so the unprivileged container user can bind it.
 */
const BIND_HOSTNAME = "0.0.0.0";
const BIND_PORT = 8090;

/**
 * Buckets for every duration we record, in seconds.
 *
 * The boundaries straddle the deadlines this service enforces, so a histogram
 * answers directly how close requests are running to them: two seconds for a
 * screen, five for the upstream connect, fifteen for a whole upstream search.
 * The steps between one and five seconds are where paid requests actually land,
 * because settling on chain dominates the time.
 */
const DURATION_BUCKETS = [0.05, 0.1, 0.25, 0.5, 1, 2, 3, 5, 8, 15, 30];

/**
 * Catch-all label value, used wherever the caller supplies something outside the
 * set we serve, so a request cannot mint a series of its own.
 */
const OTHER = "other";

/** Liveness probe path, repeated here so the label cannot drift from the route. */
const HEALTH_PATH = "/health";

/** Why a request was answered with a challenge instead of served. */
export const challenge = {
  /** The request carried no payment proof at all. */
  NO_PAYMENT: "no_payment",
  /** The proof was for a rail this deployment has turned off. */
  RAIL_DISABLED: "rail_disabled",
  /** The request carried proof for both rails at once. */
  COLLISION: "collision",
} as const;

/**
 * How a payment ended: the complete set of outcomes, whichever rail reports
 * them. Some values only ever come from one rail; they live together so the
 * whole vocabulary can be read, and checked against, in one place.
 *
 * Every refusal looks the same to the client on purpose, so this is the only
 * place the reasons stay apart.
 */
export const outcome = {
  /** Paid, and the caller got what they paid for. */
  SETTLED: "settled",
  /** The proof could not be read at all. */
  MALFORMED: "malformed",
  /** The proof was readable but accepted no offer we made for this path. */
  NO_OFFER: "no_offer",
  /** The proof was readable and is a kind this rail does not take. */
  UNSUPPORTED: "unsupported",
  /** The payment was read and understood, and did not verify. */
  REFUSED: "refused",
  /** The payer did not clear address screening. */
  SCREENED_OUT: "screened_out",
  /** We could not reach the facilitator or the chain. */
  NETWORK_UNAVAILABLE: "network_unavailable",
  /** Verified, then could not be settled. */
  SETTLE_FAILED: "settle_failed",
  /** Paid for, but the upstream search failed, so nothing was charged. */
  UPSTREAM_FAILED: "upstream_failed",
} as const;

/**
 * What a screen decided.
 *
 * The three refusals reach the rails as one indistinguishable error, so this is
 * the only place they stay apart.
 */
export const screening = {
  /** Not on the restricted list. */
  ALLOWED: "allowed",
  /** On the restricted list. */
  BLOCKED: "blocked",
  /** The payment carried nothing to screen. */
  UNIDENTIFIED: "unidentified",
  /**
   * The screen gave no answer: a timeout, a permissions problem, or any other
   * failure to reach the list.
   */
  ERROR: "error",
} as const;

/**
 * The steps of a payment worth timing separately. Which steps a rail reports
 * depends on whether it can check a payment without moving money.
 */
export const step = {
  /** Checking a payment without moving money. */
  VERIFY: "verify",
  /** Moving the money. */
  SETTLE: "settle",
  /** Checking and moving in one call, where the rail offers no dry run. */
  CHARGE: "charge",
} as const;

/**
 * Everything the service records, and the registry that renders it.
 *
 * Built once at startup and shared, so all recording lands in the one registry
 * the endpoint reads. Every test builds its own and reads exact values out of
 * it, rather than sharing a global the whole process sees.
 */
export class Metrics {
  private readonly registry: Registry;
  private readonly requests: Counter<"endpoint" | "method" | "status">;
  private readonly requestDuration: Histogram<"endpoint" | "method">;
  private readonly upstreamRequests: Counter<"endpoint" | "status">;
  private readonly upstreamDuration: Histogram<"endpoint">;
  private readonly challenges: Counter<"endpoint" | "reason">;
  private readonly payments: Counter<"rail" | "endpoint" | "outcome">;
  private readonly paymentStepDuration: Histogram<"rail" | "step">;
  private readonly chargedBaseUnits: Counter<"rail" | "endpoint">;
  private readonly screenings: Counter<"outcome">;

  /**
   * Build the registry and register every metric. Names carry the `bx402`
   * prefix, and the encoder appends `_total` to counters as OpenMetrics
   * requires.
   */
  constructor() {
    this.registry = new Registry();
    this.registry.setContentType(openMetricsContentType);
    const registers = [this.registry];

    new Gauge({
      name: "bx402_build_info",
      help: "Version of the running service.",
      labelNames: ["version"],
      registers,
    }).set({ version: VERSION }, 1);

    this.requests = new Counter<"endpoint" | "method" | "status">({
      name: "bx402_http_requests",
      help: "Requests answered, by endpoint and status.",
      labelNames: ["endpoint", "method", "status"],
      registers,
    });
    this.requestDuration = new Histogram<"endpoint" | "method">({
      name: "bx402_http_request_duration_seconds",
      help: "Time to answer a request, payment included.",
      labelNames: ["endpoint", "method"],
      buckets: DURATION_BUCKETS,
      registers,
    });
    this.upstreamRequests = new Counter<"endpoint" | "status">({
      name: "bx402_upstream_requests",
      help: "Calls to the Brave Search API, by endpoint and outcome.",
      labelNames: ["endpoint", "status"],
      registers,
    });
    this.upstreamDuration = new Histogram<"endpoint">({
      name: "bx402_upstream_duration_seconds",
      help: "Time for one Brave Search API call, response body included.",
      labelNames: ["endpoint"],
      buckets: DURATION_BUCKETS,
      registers,
    });
    this.challenges = new Counter<"endpoint" | "reason">({
      name: "bx402_challenges",
      help: "Payment challenges issued, by endpoint and reason.",
      labelNames: ["endpoint", "reason"],
      registers,
    });
    this.payments = new Counter<"rail" | "endpoint" | "outcome">({
      name: "bx402_payments",
      help: "Payments attempted, by rail, endpoint and how they ended.",
      labelNames: ["rail", "endpoint", "outcome"],
      registers,
    });
    this.paymentStepDuration = new Histogram<"rail" | "step">({
      name: "bx402_payment_step_duration_seconds",
      help: "Time for one step of a payment, by rail.",
      labelNames: ["rail", "step"],
      buckets: DURATION_BUCKETS,
      registers,
    });
    this.chargedBaseUnits = new Counter<"rail" | "endpoint">({
      name: "bx402_charged_base_units",
      help: "Base units of currency charged for settled payments.",
      labelNames: ["rail", "endpoint"],
      registers,
    });
    this.screenings = new Counter<"outcome">({
      name: "bx402_screenings",
      help: "Address screens performed, by what they decided.",
      labelNames: ["outcome"],
      registers,
    });
  }

  /** Record what one address screen decided. */
  recordScreening(outcomeLabel: string): void {
    this.screenings.inc({ outcome: outcomeLabel });
  }

  /** Record one challenge the service issued instead of serving the request. */
  recordChallenge(endpoint: string, reason: string): void {
    this.challenges.inc({ endpoint, reason });
  }

  /** Record how one payment ended. */
  recordPayment(rail: string, endpoint: string, outcomeLabel: string): void {
    this.payments.inc({ rail, endpoint, outcome: outcomeLabel });
  }

  /** Record how long one step of a payment took. */
  recordPaymentStep(rail: string, stepLabel: string, seconds: number): void {
    this.paymentStepDuration.observe({ rail, step: stepLabel }, seconds);
  }

  /**
   * Record the money a settled payment brought in, in the currency's base units.
   * Read from the catalog, so it is the price we advertised rather than anything
   * the payer stated.
   */
  recordCharge(rail: string, endpoint: string, baseUnits: number): void {
    this.chargedBaseUnits.inc({ rail, endpoint }, baseUnits);
  }

  /** Record one request the service answered. */
  recordRequest(endpoint: string, method: string, status: number, seconds: number): void {
    this.requests.inc({ endpoint, method, status: String(status) });
    this.requestDuration.observe({ endpoint, method }, seconds);
  }

  /**
   * Record one call to the Brave Search API. `status` is the response code, or
   * the kind of failure when no response arrived.
   */
  recordUpstream(endpoint: string, status: string, seconds: number): void {
    this.upstreamRequests.inc({ endpoint, status });
    this.upstreamDuration.observe({ endpoint }, seconds);
  }

  /** Render the current values as an OpenMetrics text exposition. */
  render(): Promise<string> {
    return this.registry.metrics();
  }
}

/**
 * The label for a request path: the paid endpoint it names, the health probe, or
 * `other`. Drawn from the catalog rather than the request, so a caller cannot
 * mint label values by asking for paths that do not exist.
 */
export function endpointLabel(path: string): string {
  if (path === HEALTH_PATH) {
    return HEALTH_PATH;
  }
  return find(path)?.path ?? OTHER;
}

/**
 * The label for a request method. Anything the service does not serve collapses
 * to one value, since the method is caller-supplied and otherwise unbounded.
 */
function methodLabel(method: string): string {
  switch (method) {
    case "GET":
    case "POST":
    case "HEAD":
      return method;
    default:
      return OTHER;
  }
}

/** Count and time every request, including those matching no route. */
export function measure(metrics: Metrics): MiddlewareHandler {
  return async (c, next) => {
    const endpoint = endpointLabel(new URL(c.req.url).pathname);
    const method = methodLabel(c.req.method);
    const started = performance.now();
    await next();
    metrics.recordRequest(endpoint, method, c.res.status, (performance.now() - started) / 1000);
  };
}

/**
 * Serve the metrics endpoint on its own listener until shutdown.
 *
 * A port already in use aborts startup rather than leaving the service running
 * unobserved.
 */
export function serveMetrics(metrics: Metrics): Promise<never> {
  const hono = new Hono();
  hono.get("/metrics", async (c) => {
    try {
      const exposition = await metrics.render();
      return c.body(exposition, 200, { "content-type": openMetricsContentType });
    } catch (err: unknown) {
      log.error(`rendering metrics failed: ${err instanceof Error ? err.message : String(err)}`);
      return c.body(null, 500);
    }
  });

  const server = serve(
    { fetch: hono.fetch, hostname: BIND_HOSTNAME, port: BIND_PORT },
    (address) => {
      log.info(`metrics listening on ${address.address}:${address.port}`);
    },
  );
  return new Promise<never>((_resolve, reject) => {
    server.on("error", reject);
  });
}
