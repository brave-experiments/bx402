import { describe, expect, it } from "vitest";
import { accepts, challenge, client, decodePayment, PAYMENT_REQUIRED_HEADER } from "../src/x402.js";
import { decodeChallenge, testConfig } from "./support.js";

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

describe("x402", () => {
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

  it("challenge_emits_the_full_payment_required_payload", () => {
    // Every offer field is spelled out so a change to the SDK's defaults fails
    // here instead of silently moving the charge.
    const config = testConfig();
    const built = client(
      config.x402 ?? { facilitatorUrl: "http://facilitator.invalid" },
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
