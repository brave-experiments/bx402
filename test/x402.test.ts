import { generateKeyPairSync } from "node:crypto";
import type { PaymentPayload } from "@x402/core/types";
import { afterEach, describe, expect, it } from "vitest";
import { accepts, challenge, client, decodePayment, PAYMENT_REQUIRED_HEADER } from "../src/x402.js";
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

  it("decode_reads_the_offer_the_payer_accepted", () => {
    const entries = offersFor(true, "/res/v1/web/search");

    const decoded = decodePayment(paymentHeaders({ accepted: entries[0] }));
    expect(decoded?.accepted).toEqual(entries[0]);

    // A payload naming no offer at all cannot be read.
    expect(decodePayment(paymentHeaders({}))).toBeUndefined();
  });

  it("a_tampered_offer_matches_nothing_we_advertise", () => {
    // Here the payer grants itself a discount. The payload still decodes, so it
    // is the value comparison in `handle` that refuses it.
    const entries = offersFor(true, "/res/v1/web/search");
    const discounted = { ...entries[0], amount: "1" };

    const decoded = decodePayment(paymentHeaders({ accepted: discounted }));
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
    const [name, value] = entry as [string, string];
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
