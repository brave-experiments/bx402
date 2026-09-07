import { createRequire } from "node:module";
import { MockAgent } from "undici";
import { afterEach, describe, expect, it } from "vitest";
import { app, banner } from "../src/app.js";
import { searchClient } from "../src/search.js";
import { testConfig } from "./support.js";

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
    const response = await app(testConfig()).request("/health");
    expect(response.status).toBe(200);
  });

  it("an_unsold_endpoint_is_404_not_a_payable_402", async () => {
    // The Answers API is deliberately not sold.
    const response = await app(testConfig()).request("/res/v1/chat/completions");
    expect(response.status).toBe(404);
  });

  it("unsupported_method_is_405_not_a_payable_402", async () => {
    // A POST must get the plain 405, not a challenge whose payment would buy a 405.
    const response = await app(testConfig()).request("/res/v1/web/search?q=rust", {
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

    const response = await app(testConfig(), mock).request("/res/v1/web/search?q=rust");

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

    const response = await app(testConfig(), mock).request("/res/v1/web/search?q=rust");

    expect(response.status).toBe(500);
    expect(await response.text()).toBe("brave is down");
  });

  it("unreachable_upstream_becomes_502", async () => {
    // Nothing listens on port 1, so the connection is refused, which the handler
    // maps to 502. That is distinct from an upstream that answers with a 5xx,
    // relayed as it is by the test above.
    const config = testConfig({ braveSearchApiBaseUrl: "http://127.0.0.1:1" });
    const response = await app(config, searchClient()).request("/res/v1/web/search?q=rust");

    expect(response.status).toBe(502);
    expect(await response.json()).toEqual({ error: "upstream error" });
  });
});
