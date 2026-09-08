import { afterEach, describe, expect, it } from "vitest";
import type { Config, MppConfig } from "../src/config.js";
import { client } from "../src/mpp.js";
import { mockTempoRpc, restoreNetwork, testConfig } from "./support.js";

/** Tempo mainnet and the Moderato testnet, the two chains this rail serves. */
const MAINNET = 4217;
const MODERATO = 42431;

/** The path every test that needs one paid endpoint uses. */
const WEB_SEARCH_PATH = "/res/v1/web/search";

/** The rail settings out of a test config that enables MPP. */
function mppRail(config: Config): MppConfig {
  if (config.mpp === undefined) {
    throw new Error("the test config enables the MPP rail");
  }
  return config.mpp;
}

/**
 * Build the client against an endpoint that reports `chain`. Only the endpoint is
 * canned; the build itself is the production path.
 */
async function clientOn(config: Config, chain: number) {
  mockTempoRpc(chain);
  return client(mppRail(config), config.allowTestnet);
}

afterEach(() => {
  restoreNetwork();
});

describe("mpp", () => {
  it("client_requires_a_usable_endpoint", async () => {
    // Both die in the startup chain query, the first step of the build.
    for (const endpoint of ["not a url", "http://127.0.0.1:1"]) {
      const rail = { ...mppRail(testConfig()), rpcUrl: endpoint };
      await expect(client(rail, true), `case: ${endpoint}`).rejects.toThrow(
        "invalid configuration",
      );
    }
  });

  it("a_testnet_chain_requires_the_testnet_flag", async () => {
    const config = testConfig({ allowTestnet: false });
    await expect(clientOn(config, MODERATO)).rejects.toThrow("ALLOW_TESTNET");
    // Mainnet needs no flag.
    await expect(clientOn(config, MAINNET)).resolves.toBeDefined();
  });

  it("each_endpoint_is_charged_at_its_own_price", async () => {
    const built = await clientOn(testConfig(), MODERATO);
    expect(built.charges.get(WEB_SEARCH_PATH)?.amount).toBe("0.005");
    expect(built.charges.get("/res/v1/suggest/search")?.amount).toBe("0.0005");
  });

  it("the_charge_follows_the_chain_and_pins_the_price", async () => {
    for (const chain of [MODERATO, MAINNET]) {
      const built = await clientOn(testConfig(), chain);
      const charge = built.charges.get(WEB_SEARCH_PATH);
      // pathUSD on both chains, proving the SDK's mainnet USDC default is
      // overridden, and the recipient is our treasury either way.
      expect(charge, `${chain}`).toEqual({
        amount: "0.005",
        chainId: chain,
        currency: "0x20c0000000000000000000000000000000000000",
        decimals: 6,
        recipient: "0xbd9420A98a7Bd6B89765e5715e169481602D9c3d",
      });
    }
    // Any other chain is refused rather than served with a default token.
    await expect(clientOn(testConfig(), 1)).rejects.toThrow("unsupported Tempo chain 1");
  });
});
