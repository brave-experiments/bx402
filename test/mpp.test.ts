import { Challenge, Credential, Receipt } from "mppx";
import { HttpRequestError } from "viem";
import { afterEach, describe, expect, it } from "vitest";
import type { Config, MppConfig } from "../src/config.js";
import { Metrics } from "../src/metrics.js";
import type { Client } from "../src/mpp.js";
import {
  challenge,
  client,
  credential,
  handle,
  signerAddress,
  transactionPayload,
} from "../src/mpp.js";
import {
  assertNotRecorded,
  assertRecorded,
  forgedTransaction,
  mockTempoRpc,
  restoreNetwork,
  testConfig,
} from "./support.js";

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

/**
 * A minimal challenge to sit beside a payload, so the payload gate reads only
 * what is next to it rather than anything this echo says.
 */
function echo(): Challenge.Challenge {
  return {
    id: "id",
    realm: "bx402",
    method: "tempo",
    intent: "charge",
    request: {},
  } as Challenge.Challenge;
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

  it("challenge_advertises_the_charge_credentials_answer", async () => {
    const built = await clientOn(testConfig(), MODERATO);
    const advertised = await challenge(built, WEB_SEARCH_PATH);
    expect(advertised?.[0]).toBe("www-authenticate");

    const parsed = Challenge.deserialize(advertised?.[1] as string);
    expect(parsed.realm).toBe("bx402");
    expect(parsed.method).toBe("tempo");
    expect(parsed.intent).toBe("charge");
    // Signed and time-boxed, so only a credential answering this challenge pays.
    expect(parsed.id).not.toBe("");
    expect(parsed.expires).toBeDefined();

    // The charge a credential is verified against, byte for byte. `amount` is in
    // base units here while the charge table holds the decimal the SDK scales
    // from, so this is what pins the price that actually reaches a payer.
    expect(parsed.request).toEqual({
      amount: "5000",
      currency: "0x20c0000000000000000000000000000000000000",
      methodDetails: { chainId: MODERATO },
      recipient: "0xbd9420A98a7Bd6B89765e5715e169481602D9c3d",
    });
  });

  it("challenge_needs_a_charge_for_the_path", async () => {
    const built = await clientOn(testConfig(), MODERATO);
    expect(await challenge(built, "/res/v1/chat/completions")).toBeUndefined();
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

  it("only_a_signed_transaction_payload_pays", () => {
    interface PayloadCase {
      /** Label printed if the assertion fails. */
      name: string;
      /** The credential payload to offer. */
      payload: unknown;
      /** Whether that payload is one this rail broadcasts. */
      expected: boolean;
    }
    const cases: PayloadCase[] = [
      {
        name: "transaction",
        payload: { type: "transaction", signature: "0xsigned" },
        expected: true,
      },
      { name: "hash", payload: { type: "hash", hash: "0xhash" }, expected: false },
      { name: "proof", payload: { type: "proof", signature: "0xsig" }, expected: false },
      { name: "arbitrary json", payload: { type: "mystery" }, expected: false },
    ];
    for (const { name, payload, expected } of cases) {
      const parsed = { challenge: echo(), payload } as Credential.Credential;
      expect(transactionPayload(parsed) !== undefined, `case: ${name}`).toBe(expected);
    }
  });

  it("signer_recovery_matches_the_signing_key", () => {
    // The recovery decodes the transaction independently of the SDK, so it must
    // land on exactly the key that signed it.
    const { transaction, signer } = forgedTransaction();
    expect(signerAddress({ type: "transaction", signature: transaction })).toBe(signer);
  });

  it("signer_recovery_requires_a_decodable_signed_transaction", () => {
    const cases: [string, string][] = [
      ["garbage hex", "0xno"],
      ["not hex at all", "zzz"],
      ["empty", ""],
      ["not a tempo transaction", "0x02f8"],
    ];
    for (const [name, signature] of cases) {
      expect(signerAddress({ type: "transaction", signature }), `case: ${name}`).toBeUndefined();
    }
  });

  it("credential_requires_the_payment_scheme", () => {
    const cases: [string, string][] = [
      ["bearer token", "Bearer abc123"],
      ["payment but not a credential", "Payment not-base64-json"],
      ["empty", ""],
    ];
    for (const [name, value] of cases) {
      const headers = new Headers({ authorization: value });
      expect(credential(headers), `case: ${name}`).toBeUndefined();
    }
    expect(credential(new Headers()), "case: no header").toBeUndefined();
  });

  /**
   * A client whose charge always fails with `failure`, so a test can drive the
   * one call that both checks and settles without a chain behind it.
   */
  async function refusingClient(failure: unknown): Promise<Client> {
    const built = await clientOn(testConfig(), MODERATO);
    return {
      ...built,
      handler: {
        ...built.handler,
        broadcastCredential: () => Promise.reject(failure),
      } as Client["handler"],
    };
  }

  /** Headers carrying a credential that parses and pays with a signed transaction. */
  async function payingHeaders(): Promise<Headers> {
    const { transaction } = forgedTransaction();
    const built = await clientOn(testConfig(), MODERATO);
    const advertised = await challenge(built, WEB_SEARCH_PATH);
    const minted = Challenge.deserialize(advertised?.[1] as string);
    return new Headers({
      authorization: Credential.serialize(
        Credential.from({
          challenge: minted,
          payload: { type: "transaction", signature: transaction },
        }),
      ),
    });
  }

  it("mpp_unreachable_tempo_rpc_is_a_gateway_error", async () => {
    // The credential is good; only the endpoint is gone. That is our failure and
    // not the payer's, so it must not read as "you did not pay".
    const metrics = new Metrics();
    const unreachable = new HttpRequestError({ url: "http://tempo.invalid" });
    const response = await handle(
      await refusingClient(unreachable),
      undefined,
      metrics,
      WEB_SEARCH_PATH,
      await payingHeaders(),
      () => Promise.reject(new Error("the search must never run")),
    );

    expect(response.status).toBe(502);
    expect(await response.json()).toEqual({ error: "payment network unavailable" });
    await assertRecorded(
      metrics,
      `bx402_payments_total{rail="mpp",endpoint="${WEB_SEARCH_PATH}",outcome="network_unavailable"} 1`,
    );
  });

  it("an_unreachable_tempo_rpc_records_the_charge_it_attempted", async () => {
    const metrics = new Metrics();
    await handle(
      await refusingClient(new HttpRequestError({ url: "http://tempo.invalid" })),
      undefined,
      metrics,
      WEB_SEARCH_PATH,
      await payingHeaders(),
      () => Promise.reject(new Error("the search must never run")),
    );

    // Timed even when it fails, so a dead endpoint shows up as attempted work
    // rather than as nothing at all.
    await assertRecorded(
      metrics,
      'bx402_payment_step_duration_seconds_count{rail="mpp",step="charge"} 1',
    );
  });

  it("a_refused_charge_is_not_a_gateway_error", async () => {
    // Anything that is not the endpoint being unreachable is the payer's
    // problem, and reads as a plain refusal.
    const metrics = new Metrics();
    const response = await handle(
      await refusingClient(new Error("Payment verification failed: amount mismatch")),
      undefined,
      metrics,
      WEB_SEARCH_PATH,
      await payingHeaders(),
      () => Promise.reject(new Error("the search must never run")),
    );

    expect(response.status).toBe(402);
    expect(await response.json()).toEqual({ error: "mpp payment did not verify" });
    await assertRecorded(
      metrics,
      `bx402_payments_total{rail="mpp",endpoint="${WEB_SEARCH_PATH}",outcome="refused"} 1`,
    );
    await assertNotRecorded(metrics, 'outcome="network_unavailable"');
  });

  it("attached_receipt_parses_back_from_the_header", async () => {
    // The money has already moved by the time the search runs, so the receipt
    // rides back on whatever the search returned.
    const settled = Receipt.from({
      method: "tempo",
      reference: "0xtxhash",
      status: "success",
      timestamp: new Date().toISOString(),
    });
    const built = await clientOn(testConfig(), MODERATO);
    const charged: Client = {
      ...built,
      handler: {
        ...built.handler,
        broadcastCredential: () => Promise.resolve(settled),
      } as Client["handler"],
    };

    const response = await handle(
      charged,
      undefined,
      new Metrics(),
      WEB_SEARCH_PATH,
      await payingHeaders(),
      () => Promise.resolve(new Response("upstream body", { status: 200 })),
    );

    expect(response.status).toBe(200);
    expect(await response.text()).toBe("upstream body");
    const parsed = Receipt.deserialize(response.headers.get("payment-receipt") as string);
    expect(parsed.status).toBe("success");
    expect(parsed.reference).toBe("0xtxhash");
  });

  it("a_settled_charge_is_recorded_at_the_catalog_price", async () => {
    const metrics = new Metrics();
    const built = await clientOn(testConfig(), MODERATO);
    const charged: Client = {
      ...built,
      handler: {
        ...built.handler,
        broadcastCredential: () =>
          Promise.resolve(
            Receipt.from({
              method: "tempo",
              reference: "0xtxhash",
              status: "success",
              timestamp: new Date().toISOString(),
            }),
          ),
      } as Client["handler"],
    };

    await handle(charged, undefined, metrics, WEB_SEARCH_PATH, await payingHeaders(), () =>
      Promise.resolve(new Response(null, { status: 200 })),
    );

    await assertRecorded(
      metrics,
      `bx402_payments_total{rail="mpp",endpoint="${WEB_SEARCH_PATH}",outcome="settled"} 1`,
    );
    // What we count as earned is what we advertised, never anything the payer said.
    await assertRecorded(
      metrics,
      `bx402_charged_base_units_total{rail="mpp",endpoint="${WEB_SEARCH_PATH}"} 5000`,
    );
  });
});
