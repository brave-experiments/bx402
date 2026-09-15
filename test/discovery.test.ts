import { validate } from "mppx/discovery";
import { afterEach, describe, expect, it } from "vitest";
import type { Config } from "../src/config.js";
import { type DiscoveryDocument, document, GUIDE_PATH } from "../src/discovery.js";
import { context } from "../src/dispatch.js";
import { ENDPOINTS } from "../src/endpoints.js";
import { Metrics } from "../src/metrics.js";
import { VERSION } from "../src/version.js";
import { accepts } from "../src/x402.js";
import {
  mockTempoRpc,
  restoreNetwork,
  TEST_CHAIN_ID,
  testConfig,
  WEB_SEARCH_PATH,
} from "./support.js";

/** Tempo mainnet, for the one deployment row that refuses testnets. */
const MAINNET = 4217;

/**
 * The document a deployment with `overrides` serves, its MPP endpoint canned
 * to report `chain`. The build itself is the production path.
 */
async function documentFor(
  overrides: Partial<Config> = {},
  chain = TEST_CHAIN_ID,
): Promise<DiscoveryDocument> {
  const config = testConfig(overrides);
  if (config.mpp !== undefined) {
    mockTempoRpc(chain);
  }
  return document(await context(config, undefined, new Metrics()));
}

/** The offers stated for one path. */
function offersOf(doc: DiscoveryDocument, path: string) {
  return doc.paths[path]?.get["x-payment-info"]?.offers ?? [];
}

describe("discovery", () => {
  afterEach(restoreNetwork);

  it("the_document_passes_the_spec_validator", async () => {
    interface Deployment {
      /** Label printed if the assertion fails. */
      name: string;
      overrides: Partial<Config>;
      chain?: number;
    }
    const deployments: Deployment[] = [
      { name: "both rails with the testnet", overrides: {} },
      { name: "both rails mainnet only", overrides: { allowTestnet: false }, chain: MAINNET },
      { name: "x402 only", overrides: { mpp: undefined } },
      { name: "mpp only", overrides: { x402: undefined } },
      { name: "no rails", overrides: { x402: undefined, mpp: undefined } },
    ];
    for (const { name, overrides, chain } of deployments) {
      const doc = await documentFor(overrides, chain);
      // Empty rather than "no errors": the only warning the validator emits
      // is for a write method without a request body, and the document
      // publishes GET alone, so a warning is a signal too.
      expect(validate(doc), `case: ${name}`).toEqual([]);
    }
  });

  it("every_offer_carries_only_the_fields_the_spec_defines", async () => {
    const doc = await documentFor();
    const offers = Object.values(doc.paths).flatMap(
      (path) => path.get["x-payment-info"]?.offers ?? [],
    );
    expect(offers.length).toBeGreaterThan(0);
    for (const offer of offers) {
      // The standing guard that the treasury address and the network id never
      // leak into the document. The spec validator is loose and accepts
      // unknown fields, so it would not catch either.
      expect(Object.keys(offer).sort()).toEqual(
        ["amount", "currency", "description", "intent", "method"].sort(),
      );
    }
  });

  it("every_paid_path_is_listed_with_a_402_and_its_price", async () => {
    const doc = await documentFor();
    for (const endpoint of ENDPOINTS) {
      const operation = doc.paths[endpoint.path]?.get;
      expect(operation, endpoint.path).toBeDefined();
      expect(operation?.responses["402"], endpoint.path).toBeDefined();
      const offers = offersOf(doc, endpoint.path);
      expect(offers.length, endpoint.path).toBeGreaterThan(0);
      for (const offer of offers) {
        expect(offer.amount, endpoint.path).toBe(String(endpoint.priceBaseUnits));
      }
    }
  });

  it("the_amount_is_in_base_units_not_the_decimal_charge", async () => {
    const doc = await documentFor();
    // The MPP charge behind the web search offers reads "0.005": a document
    // restating that would advertise a millionth of the price.
    expect(offersOf(doc, WEB_SEARCH_PATH).map((offer) => offer.amount)).toEqual([
      "5000",
      "5000",
      "5000",
    ]);
    expect(offersOf(doc, "/res/v1/suggest/search").map((offer) => offer.amount)).toEqual([
      "500",
      "500",
      "500",
    ]);
  });

  it("the_document_states_the_required_top_level_fields", async () => {
    const doc = await documentFor();
    expect(doc.openapi).toBe("3.1.0");
    expect(doc.info.title).toBe("bx402");
    expect(doc.info.version).toBe(VERSION);
    expect(Object.keys(doc.paths)).toHaveLength(ENDPOINTS.length);
    // Relative, so a reader resolves it against wherever the document was
    // fetched from, an origin the service itself never learns.
    expect(doc["x-service-info"].docs.llms).toBe(GUIDE_PATH);
  });

  it("a_disabled_rail_is_not_advertised", async () => {
    const x402Only = await documentFor({ mpp: undefined });
    expect(offersOf(x402Only, WEB_SEARCH_PATH).map((offer) => offer.method)).toEqual([
      "x402",
      "x402",
    ]);

    const mppOnly = await documentFor({ x402: undefined });
    expect(offersOf(mppOnly, WEB_SEARCH_PATH).map((offer) => offer.method)).toEqual(["tempo"]);

    // With no rails nothing is advertised that cannot be paid: the extension
    // is absent, never an empty list, while the 402 stays declared.
    const none = await documentFor({ x402: undefined, mpp: undefined });
    const operation = none.paths[WEB_SEARCH_PATH]?.get;
    expect(operation?.["x-payment-info"]).toBeUndefined();
    expect(operation?.responses["402"]).toBeDefined();
  });

  it("the_testnet_offer_leads_like_the_cold_402", async () => {
    const doc = await documentFor();
    const stated = offersOf(doc, WEB_SEARCH_PATH)
      .filter((offer) => offer.method === "x402")
      .map((offer) => offer.currency);
    // Asserted against `accepts` rather than pasted addresses, which are
    // already pinned once in the x402 tests.
    const advertised = (accepts(true).get(WEB_SEARCH_PATH) ?? []).map((entry) => entry.asset);
    expect(advertised).toHaveLength(2);
    expect(stated).toEqual(advertised);
  });

  it("every_offer_describes_its_chain", async () => {
    const doc = await documentFor();
    for (const endpoint of ENDPOINTS) {
      const descriptions = offersOf(doc, endpoint.path).map((offer) => offer.description);
      for (const description of descriptions) {
        expect(description, endpoint.path).not.toBe("");
      }
      // Three offers, three distinct sentences. Offers identical but for an
      // unlabelled contract address are what made Parallel's document
      // unreadable.
      expect(new Set(descriptions).size, endpoint.path).toBe(descriptions.length);
    }

    // The canned deployment settles on moderato, and the document must say
    // so rather than read as mainnet.
    const tempo = offersOf(doc, WEB_SEARCH_PATH).find((offer) => offer.method === "tempo");
    expect(tempo?.description).toContain("Moderato");
  });
});
