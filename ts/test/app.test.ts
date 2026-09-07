import { createRequire } from "node:module";
import { MockAgent } from "undici";
import { afterEach, describe, expect, it } from "vitest";
import { app, banner } from "../src/app.js";
import { Metrics } from "../src/metrics.js";
import { searchClient } from "../src/search.js";
import { assertNotRecorded, assertRecorded, testConfig } from "./support.js";

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
});

describe("app", () => {
  it("banner_includes_name_and_version", () => {
    expect(banner().startsWith("bx402 v")).toBe(true);
    expect(banner()).toContain(version);
  });

  it("health_returns_200", async () => {
    const response = await app(testConfig(), new Metrics()).request("/health");
    expect(response.status).toBe(200);
  });

  it("an_unsold_endpoint_is_404_not_a_payable_402", async () => {
    // The Answers API is deliberately not sold.
    const response = await app(testConfig(), new Metrics()).request("/res/v1/chat/completions");
    expect(response.status).toBe(404);
  });

  it("unsupported_method_is_405_not_a_payable_402", async () => {
    // A POST must get the plain 405, not a challenge whose payment would buy a 405.
    const response = await app(testConfig(), new Metrics()).request("/res/v1/web/search?q=rust", {
      method: "POST",
    });

    expect(response.status).toBe(405);
    expect(response.headers.get("allow")).toBe("GET,HEAD");
    expect(response.headers.get("payment-required")).toBeNull();
  });

  it("forwards_query_and_key_then_relays_body", async () => {
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

    const response = await app(testConfig(), new Metrics(), mock).request(
      "/res/v1/web/search?q=rust",
    );

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual(upstreamBody);
    // The interceptor matched, which is what asserts the query and the key.
    mock.assertNoPendingInterceptors();
  });

  it("upstream_5xx_is_relayed_byte_for_byte", async () => {
    const mock = mockUpstream();
    mock
      .get(UPSTREAM)
      .intercept({ method: "GET", path: "/res/v1/web/search?q=rust" })
      .reply(500, "brave is down");

    const response = await app(testConfig(), new Metrics(), mock).request(
      "/res/v1/web/search?q=rust",
    );

    expect(response.status).toBe(500);
    expect(await response.text()).toBe("brave is down");
  });

  it("unreachable_upstream_becomes_502", async () => {
    // Nothing listens on port 1, so the connection is refused, which the handler
    // maps to 502. That is distinct from an upstream that answers with a 5xx,
    // relayed as it is by the test above.
    const config = testConfig({ braveSearchApiBaseUrl: "http://127.0.0.1:1" });
    const response = await app(config, new Metrics(), searchClient()).request(
      "/res/v1/web/search?q=rust",
    );

    expect(response.status).toBe(502);
    expect(await response.json()).toEqual({ error: "upstream error" });
  });

  it("every_request_is_counted_with_its_endpoint_method_and_status", async () => {
    const metrics = new Metrics();
    const response = await app(testConfig(), metrics).request("/health");
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
    const response = await app(testConfig(), metrics).request("/res/v1/chat/completions");
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

    const metrics = new Metrics();
    const response = await app(testConfig(), metrics, mock).request("/res/v1/web/search?q=rust");
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
    const config = testConfig({ braveSearchApiBaseUrl: "http://127.0.0.1:1" });
    const metrics = new Metrics();
    const response = await app(config, metrics, searchClient()).request(
      "/res/v1/web/search?q=rust",
    );
    expect(response.status).toBe(502);

    await assertRecorded(
      metrics,
      'bx402_upstream_requests_total{endpoint="/res/v1/web/search",status="connect"} 1',
    );
  });
});
