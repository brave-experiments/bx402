import { generateKeyPairSync } from "node:crypto";
import type { HTTPFacilitatorClient } from "@x402/core/server";
import type { PaymentPayload } from "@x402/core/types";
import { afterEach, describe, expect, it } from "vitest";
import { Metrics } from "../src/metrics.js";
import {
  accepts,
  challenge,
  client,
  decodePayment,
  handle,
  offers,
  PAYMENT_REQUIRED_HEADER,
} from "../src/x402.js";
import { decodeChallenge, mockOrigin, restoreNetwork, testConfig } from "./support.js";

/** The offers advertised for one paid path. */
function offersFor(allowTestnet: boolean, path: string) {
  const offers = accepts(allowTestnet).get(path);
  if (offers === undefined) {
    throw new Error(`${path} is a paid endpoint`);
  }
  return offers;
}

/** Headers carrying `payload` as the base64 `PAYMENT-SIGNATURE`. */
function paymentHeaders(payload: unknown): Headers {
  return new Headers({
    "payment-signature": Buffer.from(JSON.stringify(payload), "utf8").toString("base64"),
  });
}

/** A well-formed eip3009 authorization, the least a decodable payment carries. */
const AUTHORIZATION = {
  from: `0x${"AB".repeat(20)}`,
  nonce: `0x${"CD".repeat(32)}`,
};

/**
 * A CDP API key secret that really signs: 64 base64 bytes of Ed25519 seed plus
 * public key, the shape the CDP SDK detects and signs tokens with.
 */
function testCdpSecret(): string {
  const { publicKey, privateKey } = generateKeyPairSync("ed25519");
  const jwk = privateKey.export({ format: "jwk" }) as { d?: string };
  const pub = publicKey.export({ format: "jwk" }) as { x?: string };
  return Buffer.concat([
    Buffer.from(jwk.d ?? "", "base64url"),
    Buffer.from(pub.x ?? "", "base64url"),
  ]).toString("base64");
}

/** The base URL of the Coinbase-hosted facilitator. */
const CDP_FACILITATOR_URL = "https://api.cdp.coinbase.com/platform/v2/x402";

describe("x402", () => {
  afterEach(restoreNetwork);

  it("without_the_testnet_flag_only_mainnet_is_offered", () => {
    const entries = offersFor(false, "/res/v1/web/search");
    expect(entries).toHaveLength(1);
    expect(entries[0]?.network).toBe("eip155:8453"); // Base mainnet
  });

  it("each_endpoint_is_offered_at_its_own_price", () => {
    const web = offersFor(false, "/res/v1/web/search");
    const suggest = offersFor(false, "/res/v1/suggest/search");

    expect(web[0]?.amount).toBe("5000");
    expect(suggest[0]?.amount).toBe("500");

    // The cheap offer is not among the dear endpoint's, so accepting it there
    // finds no match in `handle` and the payment is refused.
    expect(web).not.toContainEqual(suggest[0]);
  });

  it("discovery_offers_restate_the_accepts_table", () => {
    const built = client({ facilitatorUrl: "https://x402.org/facilitator", cdp: undefined }, true);
    const entries = offersFor(true, "/res/v1/web/search");

    const stated = offers(built, "/res/v1/web/search");
    expect(stated).toHaveLength(entries.length);
    for (const [index, offer] of stated.entries()) {
      expect(offer.intent).toBe("charge");
      expect(offer.method).toBe("x402");
      expect(offer.amount).toBe(entries[index]?.amount);
      expect(offer.currency).toBe(entries[index]?.asset);
    }

    // The chain is named in words, testnet first like the cold 402, since the
    // offer object has no network field a reader could tell them apart by.
    expect(stated.map((offer) => offer.description)).toEqual([
      "USDC on Base Sepolia (testnet)",
      "USDC on Base",
    ]);
  });

  it("discovery_offers_are_empty_for_a_path_that_is_not_sold", () => {
    const built = client({ facilitatorUrl: "https://x402.org/facilitator", cdp: undefined }, true);
    expect(offers(built, "/res/v1/answers/search")).toEqual([]);
  });

  it("decode_reads_the_offer_the_payer_accepted", () => {
    const entries = offersFor(true, "/res/v1/web/search");

    const decoded = decodePayment(
      paymentHeaders({ accepted: entries[0], payload: { authorization: AUTHORIZATION } }),
    );
    expect(decoded?.accepted).toEqual(entries[0]);
    expect(decoded?.payer).toBe(AUTHORIZATION.from.toLowerCase());

    // A payload naming no offer at all cannot be read.
    expect(decodePayment(paymentHeaders({ payload: { authorization: AUTHORIZATION } }))).toBe(
      undefined,
    );
  });

  it("a_payment_without_a_full_authorization_cannot_be_read", () => {
    // Every offer we advertise is an eip3009 transfer, so a payload lacking a
    // plausible authorization is malformed for all of them. This includes a
    // nonce of the wrong size, which must never become a replay key.
    const entries = offersFor(true, "/res/v1/web/search");
    const decode = (payload: unknown) =>
      decodePayment(paymentHeaders({ accepted: entries[0], payload }));

    expect(decode({})).toBeUndefined();
    expect(decode({ authorization: { from: AUTHORIZATION.from } })).toBeUndefined();
    expect(decode({ authorization: { nonce: AUTHORIZATION.nonce } })).toBeUndefined();
    expect(
      decode({ authorization: { from: AUTHORIZATION.from, nonce: `0x${"cd".repeat(4096)}` } }),
    ).toBeUndefined();
  });

  it("the_claim_key_is_the_nonce_scoped_by_the_payer", () => {
    const entries = offersFor(true, "/res/v1/web/search");

    // The same payment under two encodings: key order differs, so the header
    // bytes differ, but both name one authorization.
    const first = decodePayment(
      paymentHeaders({ accepted: entries[0], payload: { authorization: AUTHORIZATION } }),
    );
    const second = decodePayment(
      paymentHeaders({ payload: { authorization: AUTHORIZATION }, accepted: entries[0] }),
    );

    expect(first?.claim.key).toBe(
      `${AUTHORIZATION.from.toLowerCase()}:${AUTHORIZATION.nonce.toLowerCase()}`,
    );
    expect(second?.claim.key).toBe(first?.claim.key);
  });

  it("the_claim_window_is_the_offer_timeout_not_the_clients_deadline", () => {
    // A sender must not shorten how long its payment is held against
    // duplicates, so a deadline just ahead does not move the expiry.
    const entries = offersFor(true, "/res/v1/web/search");
    const validBefore = String(Math.floor(Date.now() / 1000) + 3);
    const decoded = decodePayment(
      paymentHeaders({
        accepted: entries[0],
        payload: { authorization: { ...AUTHORIZATION, validBefore } },
      }),
    );

    const window = (entries[0]?.maxTimeoutSeconds ?? 0) * 1000;
    expect(decoded?.claim.expires).toBeGreaterThan(Date.now() + window - 5_000);
    expect(decoded?.claim.expires).toBeLessThanOrEqual(Date.now() + window);
  });

  it("decode_blanks_the_resource_url_the_client_echoed", () => {
    // What was searched is the payer's business. The facilitator verifies the
    // signature and the requirements, neither of which names the resource, so
    // the URL we relay leaves vacant instead of crossing to Coinbase.
    const entries = offersFor(true, "/res/v1/web/search");
    const decoded = decodePayment(
      paymentHeaders({
        x402Version: 2,
        resource: {
          url: "https://bx402.example.com/res/v1/web/search?q=private",
          description: "Brave Search API - Web / Search",
        },
        accepted: entries[0],
        payload: { authorization: AUTHORIZATION },
      }),
    );
    if (decoded === undefined) {
      throw new Error("the payment decodes");
    }
    const resource = (decoded.payload as { resource?: { url?: string; description?: string } })
      .resource;
    expect(resource?.url).toBe("");
    expect(resource?.description).toBe("Brave Search API - Web / Search");
    expect(decoded.accepted).toEqual(entries[0]);
  });

  it("decode_tolerates_a_payload_without_a_resource", () => {
    const entries = offersFor(true, "/res/v1/web/search");
    const decoded = decodePayment(
      paymentHeaders({ accepted: entries[0], payload: { authorization: AUTHORIZATION } }),
    );
    if (decoded === undefined) {
      throw new Error("the payment decodes");
    }
    expect((decoded.payload as { resource?: unknown }).resource).toBeUndefined();
  });

  it("the_verify_call_carries_a_vacant_resource_url", async () => {
    // A real client echoes the challenge's resource back, query string
    // included. The rail relays the decoded payload to the facilitator, so a
    // recording stand-in reads exactly what the SDK would serialize.
    const config = testConfig();
    const built = client(
      config.x402 ?? { facilitatorUrl: "http://facilitator.invalid", cdp: undefined },
      config.allowTestnet,
    );
    let forwarded: { resource?: { url?: string }; accepted?: unknown } | undefined;
    built.facilitator = {
      verify: async (payload: PaymentPayload) => {
        forwarded = payload as { resource?: { url?: string }; accepted?: unknown };
        return { isValid: true };
      },
      settle: async () => ({
        success: true,
        transaction: "0xtxhash",
        network: "eip155:84532",
      }),
    } as unknown as HTTPFacilitatorClient;

    const entries = offersFor(true, "/res/v1/web/search");
    const response = await handle(
      built,
      undefined,
      new Metrics(),
      "/res/v1/web/search",
      paymentHeaders({
        x402Version: 2,
        resource: {
          url: "https://bx402.example.com/res/v1/web/search?q=private",
          description: "Brave Search API - Web / Search",
        },
        accepted: entries[0],
        payload: { authorization: AUTHORIZATION },
      }),
      async () => new Response(JSON.stringify({ web: {} }), { status: 200 }),
    );

    expect(response.status).toBe(200);
    // The URL the client echoed leaves vacant; the offer it accepted arrives
    // unchanged.
    expect(forwarded?.resource?.url).toBe("");
    expect(forwarded?.accepted).toEqual(entries[0]);
  });

  it("a_tampered_offer_matches_nothing_we_advertise", () => {
    // Here the payer grants itself a discount. The payload still decodes, so it
    // is the value comparison in `handle` that refuses it.
    const entries = offersFor(true, "/res/v1/web/search");
    const discounted = { ...entries[0], amount: "1" };

    const decoded = decodePayment(
      paymentHeaders({ accepted: discounted, payload: { authorization: AUTHORIZATION } }),
    );
    expect(decoded).toBeDefined();
    expect(entries).not.toContainEqual(decoded?.accepted);
  });

  it("cdp_credentials_pair_only_with_the_cdp_facilitator", () => {
    // A signed CDP token sent to any other host could be replayed against CDP
    // while it lives, so that combination must never build.
    const cdp = { apiKeyId: "key-id", apiKeySecret: "key-secret" };
    expect(() => client({ facilitatorUrl: "https://x402.org/facilitator", cdp }, true)).toThrow(
      /api\.cdp\.coinbase\.com/,
    );
    expect(client({ facilitatorUrl: CDP_FACILITATOR_URL, cdp }, true)).toBeDefined();
  });

  it("cdp_credentials_sign_the_verify_call", async () => {
    const built = client(
      {
        facilitatorUrl: CDP_FACILITATOR_URL,
        cdp: { apiKeyId: "key-id", apiKeySecret: testCdpSecret() },
      },
      true,
    );

    let authorization: string | null = null;
    mockOrigin("https://api.cdp.coinbase.com")
      .intercept({ method: "POST", path: "/platform/v2/x402/verify" })
      .reply((request) => {
        authorization = new Headers(request.headers as Record<string, string>).get("authorization");
        return { statusCode: 200, data: { isValid: true } };
      });

    const offer = offersFor(true, "/res/v1/web/search")[0];
    if (offer === undefined) {
      throw new Error("the paid path offers nothing");
    }
    const result = await built.facilitator.verify({} as PaymentPayload, offer);
    expect(result.isValid).toBe(true);
    // The token itself is the CDP SDK's business; what is ours is that the
    // request went out bearing one.
    expect(authorization).toMatch(/^Bearer .+/);
  });

  it("challenge_emits_the_full_payment_required_payload", () => {
    // Every offer field is spelled out so a change to the SDK's defaults fails
    // here instead of silently moving the charge.
    const config = testConfig();
    const built = client(
      config.x402 ?? { facilitatorUrl: "http://facilitator.invalid", cdp: undefined },
      config.allowTestnet,
    );
    const entry = challenge(built, "https://bx402.example.com/res/v1/web/search?q=rust", "GET");
    expect(entry).toBeDefined();
    const { name, value } = entry as { name: string; value: string };
    expect(name).toBe(PAYMENT_REQUIRED_HEADER);

    expect(decodeChallenge(value)).toEqual({
      x402Version: 2,
      error: "Payment required",
      resource: {
        url: "https://bx402.example.com/res/v1/web/search?q=rust",
        description: "Brave Search API - Web / Search",
        mimeType: "application/json",
      },
      accepts: [
        {
          scheme: "exact",
          network: "eip155:84532",
          amount: "5000",
          asset: "0x036CbD53842c5426634e7929541eC2318f3dCF7e",
          payTo: "0xbd9420A98a7Bd6B89765e5715e169481602D9c3d",
          maxTimeoutSeconds: 300,
          extra: { assetTransferMethod: "eip3009", name: "USDC", version: "2" },
        },
        {
          scheme: "exact",
          network: "eip155:8453",
          amount: "5000",
          asset: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
          payTo: "0xbd9420A98a7Bd6B89765e5715e169481602D9c3d",
          maxTimeoutSeconds: 300,
          extra: { assetTransferMethod: "eip3009", name: "USD Coin", version: "2" },
        },
      ],
      extensions: {
        mppx: {
          info: { method: "GET" },
          schema: { type: "object" },
        },
      },
    });
  });
});
