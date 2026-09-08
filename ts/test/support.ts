import { HeadObjectCommand, S3Client } from "@aws-sdk/client-s3";
import { type AwsStub, mockClient } from "aws-sdk-client-mock";
import type { Hono } from "hono";
import * as Secp256k1 from "ox/Secp256k1";
import { TxEnvelopeTempo } from "ox/tempo";
import { type Dispatcher, getGlobalDispatcher, MockAgent, setGlobalDispatcher } from "undici";
import { privateKeyToAccount } from "viem/accounts";
import { expect } from "vitest";
import { app } from "../src/app.js";
import type { Config } from "../src/config.js";
import { Metrics } from "../src/metrics.js";
import { RestrictedAddressScreener } from "../src/screener.js";
import { accepts } from "../src/x402.js";

/**
 * A config whose every endpoint is parseable but unreachable, shared by the test
 * files; each test overrides the fields it exercises.
 */
export function testConfig(overrides: Partial<Config> = {}): Config {
  return {
    braveSearchApiKey: "secret-key",
    braveSearchApiBaseUrl: "http://upstream.invalid",
    x402: { facilitatorUrl: "http://facilitator.invalid" },
    // The SDK refuses a secret key under 32 bytes, so this one is long enough to
    // build a handler with.
    mpp: { rpcUrl: "http://tempo.invalid", secretKey: "test-secret-of-at-least-32-bytes" },
    restrictedAddressS3Bucket: undefined,
    allowTestnet: true,
    ...overrides,
  };
}

/**
 * Assert `series` appears verbatim in what `metrics` has recorded. Shared by the
 * tests in every file that records, so each assertion names only the series it
 * cares about.
 */
export async function assertRecorded(metrics: Metrics, series: string): Promise<void> {
  const exposition = await metrics.render();
  expect(exposition, `missing \`${series}\` in:\n${exposition}`).toContain(series);
}

/** The inverse of `assertRecorded`, for proving something was never recorded. */
export async function assertNotRecorded(metrics: Metrics, fragment: string): Promise<void> {
  const exposition = await metrics.render();
  expect(exposition, `unexpected \`${fragment}\` in:\n${exposition}`).not.toContain(fragment);
}

/** The bucket every screener test looks up, so tests can build the exact key. */
export const TEST_BUCKET = "restricted-address-bucket";

/**
 * An S3 failure shaped the way the SDK reports one, so the screener's not-found
 * check sees what it would see against the real service.
 */
export function s3Failure(name: string, httpStatusCode: number): Error {
  const err = new Error(name);
  err.name = name;
  Object.assign(err, { $metadata: { httpStatusCode } });
  return err;
}

/** The SDK's answer for a key that is not in the bucket. */
export function notFound(): Error {
  return s3Failure("NotFound", 404);
}

/**
 * An S3 client pointed at an endpoint that nothing serves, with retries off so
 * the failure path returns promptly.
 */
export function unreachableS3Client(): S3Client {
  return new S3Client({
    region: "us-east-1",
    credentials: { accessKeyId: "test", secretAccessKey: "test" },
    endpoint: "http://127.0.0.1:1",
    forcePathStyle: true,
    maxAttempts: 1,
  });
}

/** The facilitator base URL the test config points at. */
export const TEST_FACILITATOR = "http://facilitator.invalid";

let previousDispatcher: Dispatcher | undefined;
let installed: MockAgent | undefined;

/**
 * The mock agent every stub shares, installed on the global dispatcher the first
 * time one is asked for. One agent rather than one per stub, so a dual-rail test
 * can stand in for the facilitator and the Tempo endpoint at the same time
 * without the two replacing each other.
 */
function mockNetwork(): MockAgent {
  if (installed === undefined) {
    installed = new MockAgent();
    installed.disableNetConnect();
    previousDispatcher = getGlobalDispatcher();
    setGlobalDispatcher(installed);
  }
  return installed;
}

/**
 * Stand in for the x402 facilitator, which the SDK's client calls over the
 * global dispatcher: `POST /verify` reports `valid`, `POST /settle` reports
 * `settles`. The two are independent so a test can drive any verify/settle
 * pairing. Call `restoreNetwork` afterwards.
 */
export function mockFacilitator(valid: boolean, settles: boolean): MockAgent {
  const agent = mockNetwork();
  const pool = agent.get(TEST_FACILITATOR);
  pool.intercept({ method: "POST", path: "/verify" }).reply(200, { isValid: valid }).persist();
  pool
    .intercept({ method: "POST", path: "/settle" })
    .reply(
      200,
      settles
        ? { success: true, transaction: "0xtxhash", network: "eip155:84532" }
        : {
            success: false,
            errorReason: "settlement_failed",
            transaction: "",
            network: "eip155:84532",
          },
    )
    .persist();
  return agent;
}

/** The Tempo RPC endpoint the test config points at. */
export const TEST_TEMPO_RPC = "http://tempo.invalid";

/**
 * Stand in for the Tempo RPC endpoint, answering one `eth_chainId` with `chainId`.
 *
 * Answered once and no more, so a later call finds nothing bound and fails as a
 * transport error. That is what a test needs to drive the unreachable-endpoint
 * path: the rail builds at startup, then the charge it tries has no endpoint to
 * reach.
 */
export function mockTempoRpc(chainId: number): MockAgent {
  const agent = mockNetwork();
  agent
    .get(TEST_TEMPO_RPC)
    .intercept({ method: "POST", path: "/" })
    .reply(200, { jsonrpc: "2.0", id: 1, result: `0x${chainId.toString(16)}` });
  return agent;
}

/** Put the real global dispatcher back after any of the network stubs. */
export function restoreNetwork(): void {
  installed = undefined;
  if (previousDispatcher !== undefined) {
    setGlobalDispatcher(previousDispatcher);
    previousDispatcher = undefined;
  }
}

/**
 * The chain a test rail runs on, so tests read one name rather than a number.
 */
export const TEST_CHAIN_ID = 42431;

/**
 * Build the app the way a test needs it: an enabled MPP rail asks its endpoint
 * which chain it serves before the app exists, so stand in for that endpoint
 * first. A test that disables the rail gets no stub, which is what proves a
 * disabled rail never queries a chain.
 */
export async function buildApp(
  config: Config,
  screener: RestrictedAddressScreener | undefined,
  metrics: Metrics,
  dispatcher?: Dispatcher,
): Promise<Hono> {
  if (config.mpp !== undefined) {
    mockTempoRpc(TEST_CHAIN_ID);
  }
  return dispatcher === undefined
    ? app(config, screener, metrics)
    : app(config, screener, metrics, dispatcher);
}

/** The key every forged transaction is signed with, and the address it recovers to. */
const TEST_KEY = `0x${"01".repeat(32)}` as const;
export const TEST_SIGNER = privateKeyToAccount(TEST_KEY).address.toLowerCase();

/**
 * A real signed Tempo transaction, and the address that signed it.
 *
 * Signed at test time rather than pasted in, so the bytes stay aligned with the
 * encoding the current libraries produce. The recipient, chain, and gas are
 * arbitrary: the transfer never verifies, it only has to decode and carry a real
 * signature.
 */
export function forgedTransaction(): { transaction: string; signer: string } {
  const envelope = TxEnvelopeTempo.from({
    type: "tempo",
    chainId: TEST_CHAIN_ID,
    calls: [{ to: `0x${"42".repeat(20)}`, value: 0n, data: "0x" }],
    gas: 100_000n,
    maxFeePerGas: 1n,
    maxPriorityFeePerGas: 1n,
    nonce: 0n,
  });
  const signature = Secp256k1.sign({
    payload: TxEnvelopeTempo.getSignPayload(envelope),
    privateKey: TEST_KEY,
  });
  return {
    transaction: TxEnvelopeTempo.serialize(envelope, { signature }),
    signer: TEST_SIGNER,
  };
}

/**
 * The `PAYMENT-SIGNATURE` value for a payment accepting the first offer we
 * advertise for `path`, carrying `payload` as the scheme payload.
 */
export function paymentSignature(
  path: string,
  payload: Record<string, unknown> = {},
  allowTestnet = true,
): string {
  const offers = accepts(allowTestnet).get(path);
  if (offers === undefined || offers[0] === undefined) {
    throw new Error(`${path} is a paid endpoint`);
  }
  return Buffer.from(JSON.stringify({ accepted: offers[0], payload })).toString("base64");
}

/** Decode a base64 `Payment-Required` challenge header back to JSON. */
export function decodeChallenge(value: string): Record<string, unknown> {
  return JSON.parse(Buffer.from(value, "base64").toString("utf8"));
}

// biome-ignore lint/suspicious/noExplicitAny: the mock's own input/output generics.
let s3Stub: AwsStub<any, any, any> | undefined;

/** Take down whatever `mockS3*` installed. */
export function restoreS3(): void {
  s3Stub?.restore();
  s3Stub = undefined;
}

/** A mock S3 answering every `HeadObject` the way `status` says. */
export function mockS3Answering(status: number): S3Client {
  restoreS3();
  s3Stub = mockClient(S3Client);
  if (status === 200) {
    s3Stub.on(HeadObjectCommand).resolves({});
  } else if (status === 404) {
    s3Stub.on(HeadObjectCommand).rejects(notFound());
  } else {
    s3Stub.on(HeadObjectCommand).rejects(s3Failure("S3Error", status));
  }
  return new S3Client({});
}

/**
 * A mock S3 whose restricted list holds exactly `address`. The key is the
 * lowercased address, mirroring the rail's canonicalization and the screener's
 * encoding.
 */
export function mockS3Blocking(address: string): S3Client {
  restoreS3();
  s3Stub = mockClient(S3Client);
  const key = Buffer.from(address.toLowerCase(), "utf8").toString("base64url");
  // The general answer is declared first so the exact-key one overrides it.
  s3Stub.on(HeadObjectCommand).rejects(notFound());
  s3Stub.on(HeadObjectCommand, { Bucket: TEST_BUCKET, Key: key }).resolves({});
  return new S3Client({});
}

/** A screener over a mock S3 that answers every `HeadObject` with `status`. */
export function screenerAnswering(
  status: number,
  metrics = new Metrics(),
): RestrictedAddressScreener {
  return new RestrictedAddressScreener(mockS3Answering(status), TEST_BUCKET, metrics);
}

/** A screener whose restricted list holds exactly `address`. */
export function screenerBlocking(
  address: string,
  metrics = new Metrics(),
): RestrictedAddressScreener {
  return new RestrictedAddressScreener(mockS3Blocking(address), TEST_BUCKET, metrics);
}
