import { createRequire } from "node:module";
import { MockAgent } from "undici";
import { afterEach, describe, expect, it } from "vitest";
import { app, banner } from "../src/app.js";
import { ENDPOINTS } from "../src/endpoints.js";
import { Metrics } from "../src/metrics.js";
import { searchClient } from "../src/search.js";
import {
  assertNotRecorded,
  assertRecorded,
  decodeChallenge,
  mockFacilitator,
  paymentSignature,
  restoreNetwork,
  restoreS3,
  screenerAnswering,
  screenerBlocking,
  testConfig,
} from "./support.js";

const { version } = createRequire(import.meta.url)("../../package.json") as {
  version: string;
};

const UPSTREAM = "http://upstream.invalid";

let agent: MockAgent | undefined;

/**
 * A dispatcher that answers only what a test declares, so a request the test did
 * not set up fails loudly instead of reaching the network.
 */
function mockUpstream(): MockAgent {
  agent = new MockAgent();
  agent.disableNetConnect();
  return agent;
}

afterEach(async () => {
  await agent?.close();
  agent = undefined;
  restoreNetwork();
  restoreS3();
});

/** The headers a paying x402 client sends for `path`. */
function paid(path: string): { headers: Record<string, string> } {
  return { headers: { "payment-signature": paymentSignature(path) } };
}

/**
 * `paid`, naming `from` as the payer so the screener has an address to check.
 * The scheme payload only needs `authorization.from`; the mock facilitator
 * accepts the rest.
 */
function paidFrom(path: string, from: string): { headers: Record<string, string> } {
  return {
    headers: {
      "payment-signature": paymentSignature(path, { authorization: { from } }),
    },
  };
}

/**
 * Drive one paid x402 request against a facilitator behaving as `valid` and
 * `settles` and an upstream answering `upstreamStatus`, handing back what the
 * request recorded.
 */
async function paidX402(
  valid: boolean,
  settles: boolean,
  upstreamStatus: number,
): Promise<{ response: Response; metrics: Metrics }> {
  const mock = mockUpstream();
  mock
    .get(UPSTREAM)
    .intercept({ method: "GET", path: "/res/v1/web/search?q=rust" })
    .reply(upstreamStatus, {}, { headers: { "content-type": "application/json" } });
  mockFacilitator(valid, settles);
  const metrics = new Metrics();
  const response = await app(testConfig({ mpp: undefined }), undefined, metrics, mock).request(
    "/res/v1/web/search?q=rust",
    paid("/res/v1/web/search"),
  );
  await agent?.close();
  agent = undefined;
  restoreNetwork();
  return { response, metrics };
}

/**
 * Assert exactly one payment on `rail` for the web search endpoint ended in
 * `outcome`.
 */
async function assertPaymentOutcome(
  metrics: Metrics,
  rail: string,
  outcome: string,
): Promise<void> {
  await assertRecorded(
    metrics,
    `bx402_payments_total{rail="${rail}",endpoint="/res/v1/web/search",outcome="${outcome}"} 1`,
  );
}

describe("app", () => {
  it("banner_includes_name_and_version", () => {
    expect(banner().startsWith("bx402 v")).toBe(true);
    expect(banner()).toContain(version);
  });

  it("health_returns_200", async () => {
    const response = await app(testConfig(), undefined, new Metrics()).request("/health");
    expect(response.status).toBe(200);
  });

  it("an_unsold_endpoint_is_404_not_a_payable_402", async () => {
    // The Answers API is deliberately not sold.
    const response = await app(testConfig(), undefined, new Metrics()).request(
      "/res/v1/chat/completions",
    );
    expect(response.status).toBe(404);
  });

  it("unsupported_method_is_405_not_a_payable_402", async () => {
    // A POST must get the plain 405, not a challenge whose payment would buy a 405.
    const response = await app(testConfig(), undefined, new Metrics()).request(
      "/res/v1/web/search?q=rust",
      {
        method: "POST",
      },
    );

    expect(response.status).toBe(405);
    expect(response.headers.get("allow")).toBe("GET,HEAD");
    expect(response.headers.get("payment-required")).toBeNull();
  });

  it("forwards_query_and_key_then_relays_body", async () => {
    mockFacilitator(true, true);
    const upstreamBody = { web: { results: [] } };
    const mock = mockUpstream();
    mock
      .get(UPSTREAM)
      .intercept({
        method: "GET",
        path: "/res/v1/web/search?q=rust",
        headers: { "X-Subscription-Token": "secret-key" },
      })
      .reply(200, upstreamBody, { headers: { "content-type": "application/json" } });

    const response = await app(testConfig(), undefined, new Metrics(), mock).request(
      "/res/v1/web/search?q=rust",
      paid("/res/v1/web/search"),
    );

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual(upstreamBody);
    // The interceptor matched, which is what asserts the query and the key.
    mock.assertNoPendingInterceptors();
  });

  it("upstream_5xx_is_relayed_byte_for_byte", async () => {
    mockFacilitator(true, true);
    const mock = mockUpstream();
    mock
      .get(UPSTREAM)
      .intercept({ method: "GET", path: "/res/v1/web/search?q=rust" })
      .reply(500, "brave is down");

    const response = await app(testConfig(), undefined, new Metrics(), mock).request(
      "/res/v1/web/search?q=rust",
      paid("/res/v1/web/search"),
    );

    expect(response.status).toBe(500);
    expect(await response.text()).toBe("brave is down");
  });

  it("unreachable_upstream_becomes_502", async () => {
    mockFacilitator(true, true);
    // Nothing listens on port 1, so the connection is refused, which the handler
    // maps to 502. That is distinct from an upstream that answers with a 5xx,
    // relayed as it is by the test above.
    const config = testConfig({ braveSearchApiBaseUrl: "http://127.0.0.1:1" });
    const response = await app(config, undefined, new Metrics(), searchClient()).request(
      "/res/v1/web/search?q=rust",
      paid("/res/v1/web/search"),
    );

    expect(response.status).toBe(502);
    expect(await response.json()).toEqual({ error: "upstream error" });
  });

  it("every_request_is_counted_with_its_endpoint_method_and_status", async () => {
    const metrics = new Metrics();
    const response = await app(testConfig(), undefined, metrics).request("/health");
    expect(response.status).toBe(200);

    await assertRecorded(
      metrics,
      'bx402_http_requests_total{endpoint="/health",method="GET",status="200"} 1',
    );
    await assertRecorded(
      metrics,
      'bx402_http_request_duration_seconds_count{endpoint="/health",method="GET"} 1',
    );
  });

  // A path we do not serve is still counted, and its raw text never reaches a
  // label, so requests for paths that do not exist cannot grow the series.
  it("an_unsold_path_is_counted_without_minting_a_label", async () => {
    const metrics = new Metrics();
    const response = await app(testConfig(), undefined, metrics).request(
      "/res/v1/chat/completions",
    );
    expect(response.status).toBe(404);

    await assertRecorded(
      metrics,
      'bx402_http_requests_total{endpoint="other",method="GET",status="404"} 1',
    );
    await assertNotRecorded(metrics, "chat/completions");
  });

  it("an_upstream_error_is_counted_under_its_status", async () => {
    const mock = mockUpstream();
    mock
      .get(UPSTREAM)
      .intercept({ method: "GET", path: "/res/v1/web/search?q=rust" })
      .reply(500, "brave is down");

    mockFacilitator(true, true);
    const metrics = new Metrics();
    const response = await app(testConfig(), undefined, metrics, mock).request(
      "/res/v1/web/search?q=rust",
      paid("/res/v1/web/search"),
    );
    expect(response.status).toBe(500);

    await assertRecorded(
      metrics,
      'bx402_upstream_requests_total{endpoint="/res/v1/web/search",status="500"} 1',
    );
    await assertRecorded(
      metrics,
      'bx402_upstream_duration_seconds_count{endpoint="/res/v1/web/search"} 1',
    );
  });

  // An upstream that never answers is counted too, under the kind of failure
  // rather than a status it never sent.
  it("an_unreachable_upstream_is_counted_as_a_transport_failure", async () => {
    mockFacilitator(true, true);
    const config = testConfig({ braveSearchApiBaseUrl: "http://127.0.0.1:1" });
    const metrics = new Metrics();
    const response = await app(config, undefined, metrics, searchClient()).request(
      "/res/v1/web/search?q=rust",
      paid("/res/v1/web/search"),
    );
    expect(response.status).toBe(502);

    await assertRecorded(
      metrics,
      'bx402_upstream_requests_total{endpoint="/res/v1/web/search",status="connect"} 1',
    );
  });
  it("app_rejects_an_unparseable_facilitator_url", () => {
    const config = testConfig({ x402: { facilitatorUrl: "not a url" }, mpp: undefined });
    expect(() => app(config, undefined, new Metrics())).toThrow("invalid configuration");
  });

  it("each_endpoint_answers_a_cold_402_at_its_own_price", async () => {
    const hono = app(testConfig({ mpp: undefined }), undefined, new Metrics());

    for (const endpoint of ENDPOINTS) {
      const response = await hono.request(endpoint.path);
      expect(response.status, endpoint.path).toBe(402);
      const header = response.headers.get("payment-required");
      expect(header, endpoint.path).not.toBeNull();
      const advertised = decodeChallenge(header as string) as {
        accepts: { amount: string }[];
      };
      expect(advertised.accepts[0]?.amount, endpoint.path).toBe(String(endpoint.priceBaseUnits));
    }
  });

  // Paying one endpoint's price does not buy a dearer one. The payment is well
  // formed and accepts an offer we really do advertise, just not for the path it
  // is sent to, so only the per-path lookup refuses it.
  it("a_cheap_endpoints_payment_does_not_buy_a_dear_one", async () => {
    const hono = app(testConfig({ mpp: undefined }), undefined, new Metrics());

    // Refused before the facilitator is consulted, which is why an unreachable
    // facilitator here still yields a 402 rather than a 502.
    const response = await hono.request("/res/v1/web/search?q=rust", {
      headers: { "payment-signature": paymentSignature("/res/v1/suggest/search") },
    });
    expect(response.status).toBe(402);
  });

  it("cold_402_advertises_the_absolute_request_url_as_resource", async () => {
    // End to end: a cold request through the real router must echo back the
    // exact URL it hit as `resource.url`, built from the proxy headers with the
    // query kept.
    const response = await app(testConfig({ mpp: undefined }), undefined, new Metrics()).request(
      "/res/v1/web/search?q=rust",
      { headers: { host: "api.bx402.io", "x-forwarded-proto": "https" } },
    );

    expect(response.status).toBe(402);
    const challenge = decodeChallenge(response.headers.get("payment-required") as string) as {
      resource: { url: string };
      extensions: { mppx: { info: { method: string } } };
    };
    expect(challenge.resource.url).toBe("https://api.bx402.io/res/v1/web/search?q=rust");
    // The route binding names the method the request arrived with.
    expect(challenge.extensions.mppx.info.method).toBe("GET");
  });

  it("x402_only_app_needs_no_rpc_and_cold_402s_an_mpp_attempt", async () => {
    const response = await app(testConfig({ mpp: undefined }), undefined, new Metrics()).request(
      "/res/v1/web/search?q=rust",
      { headers: { authorization: "Payment test-cred" } },
    );

    expect(response.status).toBe(402);
    expect(response.headers.get("payment-required")).not.toBeNull();
    expect(response.headers.get("www-authenticate")).toBeNull();
    expect(await response.text()).toBe("");
  });

  it("no_rails_app_402s_every_payment_attempt", async () => {
    const config = testConfig({ x402: undefined, mpp: undefined });
    const hono = app(config, undefined, new Metrics());

    const attempts: Record<string, string>[] = [
      {},
      { "payment-signature": "sig" },
      { authorization: "Payment test-cred" },
    ];
    for (const headers of attempts) {
      const response = await hono.request("/res/v1/web/search?q=rust", { headers });
      expect(response.status).toBe(402);
      expect(response.headers.get("payment-required")).toBeNull();
      expect(response.headers.get("www-authenticate")).toBeNull();
      expect(await response.text()).toBe("");
    }

    // The health probe stays green, so a load balancer keeps the service up.
    expect((await hono.request("/health")).status).toBe(200);
  });

  it("challenges_record_why_they_were_issued", async () => {
    const metrics = new Metrics();
    const hono = app(testConfig({ mpp: undefined }), undefined, metrics);

    const cases: [Record<string, string>, string][] = [
      [{}, "no_payment"],
      [{ "payment-signature": "sig", authorization: "cred" }, "collision"],
    ];
    for (const [headers, reason] of cases) {
      await hono.request("/res/v1/web/search?q=rust", { headers });
      await assertRecorded(
        metrics,
        `bx402_challenges_total{endpoint="/res/v1/web/search",reason="${reason}"} 1`,
      );
    }
  });

  // A payer on a rail this deployment turned off is counted apart from a caller
  // who simply did not pay, which is the only way to see clients stranded
  // against a disabled rail.
  it("a_payment_on_a_disabled_rail_is_counted_apart_from_a_cold_request", async () => {
    const metrics = new Metrics();
    const config = testConfig({ x402: undefined, mpp: undefined });

    const response = await app(config, undefined, metrics).request("/res/v1/web/search?q=rust", {
      headers: { "payment-signature": "sig" },
    });
    expect(response.status).toBe(402);

    await assertRecorded(
      metrics,
      'bx402_challenges_total{endpoint="/res/v1/web/search",reason="rail_disabled"} 1',
    );
  });

  it("a_settled_payment_records_its_outcome_price_and_step_timings", async () => {
    const { response, metrics } = await paidX402(true, true, 200);
    expect(response.status).toBe(200);

    await assertPaymentOutcome(metrics, "x402", "settled");
    // The catalog price, not anything the payer stated.
    await assertRecorded(
      metrics,
      'bx402_charged_base_units_total{rail="x402",endpoint="/res/v1/web/search"} 5000',
    );
    for (const step of ["verify", "settle"]) {
      await assertRecorded(
        metrics,
        `bx402_payment_step_duration_seconds_count{rail="x402",step="${step}"} 1`,
      );
    }
  });

  // Every way a payment can end badly is counted apart, even though the client
  // is told the same thing.
  it("each_way_a_payment_fails_records_its_own_outcome", async () => {
    const refused = await paidX402(false, true, 200);
    expect(refused.response.status).toBe(402);
    await assertPaymentOutcome(refused.metrics, "x402", "refused");

    // Verified, then the search failed: relayed as is, and never charged.
    const upstreamFailed = await paidX402(true, true, 500);
    expect(upstreamFailed.response.status).toBe(500);
    await assertPaymentOutcome(upstreamFailed.metrics, "x402", "upstream_failed");
    await assertNotRecorded(upstreamFailed.metrics, "bx402_charged_base_units_total{");

    // Verified, then settlement declined.
    const unsettled = await paidX402(true, false, 200);
    expect(unsettled.response.status).toBe(502);
    await assertPaymentOutcome(unsettled.metrics, "x402", "settle_failed");
  });

  it("verified_payment_runs_the_search_and_returns_a_settlement_receipt", async () => {
    const upstreamBody = { web: { results: [] } };
    const mock = mockUpstream();
    mock
      .get(UPSTREAM)
      .intercept({ method: "GET", path: "/res/v1/web/search?q=rust" })
      .reply(200, upstreamBody, { headers: { "content-type": "application/json" } });
    mockFacilitator(true, true);

    const response = await app(
      testConfig({ mpp: undefined }),
      undefined,
      new Metrics(),
      mock,
    ).request("/res/v1/web/search?q=rust", paid("/res/v1/web/search"));

    expect(response.status).toBe(200);
    // The settlement receipt rides back base64-encoded in `Payment-Response`.
    const receipt = decodeChallenge(response.headers.get("payment-response") as string);
    expect(receipt.success).toBe(true);
    // The upstream body is relayed unchanged underneath the receipt header.
    expect(await response.json()).toEqual(upstreamBody);
    // The search runs exactly once, after verification.
    mock.assertNoPendingInterceptors();
  });

  it("rejected_payment_returns_402_and_never_calls_upstream", async () => {
    const mock = mockUpstream();
    mockFacilitator(false, true);

    const response = await app(
      testConfig({ mpp: undefined }),
      undefined,
      new Metrics(),
      mock,
    ).request("/res/v1/web/search?q=rust", paid("/res/v1/web/search"));

    expect(response.status).toBe(402);
    // Nothing settled, so no receipt.
    expect(response.headers.get("payment-response")).toBeNull();
  });

  // Verify passes and the search succeeds, but settlement fails. The client did
  // not pay, so the produced body must be withheld behind a 502 rather than
  // served.
  it("unsettled_payment_withholds_a_successful_body", async () => {
    const mock = mockUpstream();
    mock
      .get(UPSTREAM)
      .intercept({ method: "GET", path: "/res/v1/web/search?q=rust" })
      .reply(200, { web: {} }, { headers: { "content-type": "application/json" } });
    mockFacilitator(true, false);

    const response = await app(
      testConfig({ mpp: undefined }),
      undefined,
      new Metrics(),
      mock,
    ).request("/res/v1/web/search?q=rust", paid("/res/v1/web/search"));

    expect(response.status).toBe(502);
    expect(response.headers.get("payment-response")).toBeNull();
    expect(await response.json()).toEqual({ error: "x402 payment could not be settled" });
  });

  it("blocked_signer_is_refused_before_any_call", async () => {
    // Clients send the checksummed form. The rail lowercases it, so the stored
    // key is lowercase.
    const from = "0xAb5801a7D398351b8bE11C439e05C5B3259aeC9B";
    const screener = screenerBlocking(from);
    // The search must never run for a blocked signer, and the facilitator is
    // unreachable, so reaching either would surface as something other than 402.
    const mock = mockUpstream();

    const response = await app(
      testConfig({ mpp: undefined }),
      screener,
      new Metrics(),
      mock,
    ).request("/res/v1/web/search?q=rust", paidFrom("/res/v1/web/search", from));

    expect(response.status).toBe(402);
  });

  it("allowed_signer_passes_through_to_search", async () => {
    // Every key is absent: the payer is not on the list.
    const screener = screenerAnswering(404);
    const mock = mockUpstream();
    mock
      .get(UPSTREAM)
      .intercept({ method: "GET", path: "/res/v1/web/search?q=rust" })
      .reply(200, { web: {} }, { headers: { "content-type": "application/json" } });
    mockFacilitator(true, true);

    const response = await app(
      testConfig({ mpp: undefined }),
      screener,
      new Metrics(),
      mock,
    ).request(
      "/res/v1/web/search?q=rust",
      paidFrom("/res/v1/web/search", "0x1111111111111111111111111111111111111111"),
    );

    expect(response.status).toBe(200);
    mock.assertNoPendingInterceptors();
  });

  it("unscreenable_signer_returns_503", async () => {
    // The bucket errors, so the payer cannot be screened: deny, do not serve.
    const screener = screenerAnswering(500);
    const mock = mockUpstream();

    const response = await app(
      testConfig({ mpp: undefined }),
      screener,
      new Metrics(),
      mock,
    ).request(
      "/res/v1/web/search?q=rust",
      paidFrom("/res/v1/web/search", "0x2222222222222222222222222222222222222222"),
    );

    expect(response.status).toBe(503);
  });

  it("a_blocked_payer_records_a_screened_out_payment", async () => {
    const from = "0xAb5801a7D398351b8bE11C439e05C5B3259aeC9B";
    const metrics = new Metrics();
    const screener = screenerBlocking(from, metrics);
    const mock = mockUpstream();

    const response = await app(testConfig({ mpp: undefined }), screener, metrics, mock).request(
      "/res/v1/web/search?q=rust",
      paidFrom("/res/v1/web/search", from),
    );

    expect(response.status).toBe(402);
    await assertPaymentOutcome(metrics, "x402", "screened_out");
  });
});
