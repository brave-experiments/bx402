import { HeadObjectCommand, S3Client } from "@aws-sdk/client-s3";
import { type AwsStub, mockClient } from "aws-sdk-client-mock";
import { afterEach, describe, expect, it } from "vitest";
import { Metrics } from "../src/metrics.js";
import {
  CANARY_KEY,
  initScreener,
  initWith,
  RestrictedAddressScreener,
  type Screening,
  statusLine,
} from "../src/screener.js";
import {
  assertRecorded,
  notFound,
  s3Failure,
  TEST_BUCKET,
  testConfig,
  unreachableS3Client,
} from "./support.js";

// biome-ignore lint/suspicious/noExplicitAny: the mock's own input/output generics.
let s3: AwsStub<any, any, any> | undefined;

/** A mock S3 that answers every `HeadObject` the way `status` says. */
function s3Answering(status: number): S3Client {
  s3 = mockClient(S3Client);
  if (status === 200) {
    s3.on(HeadObjectCommand).resolves({});
  } else if (status === 404) {
    s3.on(HeadObjectCommand).rejects(notFound());
  } else {
    s3.on(HeadObjectCommand).rejects(s3Failure("S3Error", status));
  }
  return new S3Client({});
}

/** A screener over a mock S3 that answers every `HeadObject` with `status`. */
function screenerAnswering(status: number, metrics = new Metrics()): RestrictedAddressScreener {
  return new RestrictedAddressScreener(s3Answering(status), TEST_BUCKET, metrics);
}

/** The rejection response a rail would pass to `requireAllowed`. */
function rejected(): Response {
  return new Response(null, { status: 402 });
}

afterEach(() => {
  s3?.restore();
  s3 = undefined;
});

describe("screener", () => {
  it("require_allowed_maps_outcomes_to_refusals", async () => {
    // 200 is a list hit, 404 a miss, anything else means the list could not be
    // consulted.
    const cases: [number, number | undefined][] = [
      [200, 402],
      [404, undefined],
      [500, 503],
    ];
    for (const [s3Status, expected] of cases) {
      const screener = screenerAnswering(s3Status);
      const refusal = await screener.requireAllowed("0xanything", rejected());
      expect(refusal?.status, `s3 status: ${s3Status}`).toBe(expected);
      s3?.restore();
      s3 = undefined;
    }
  });

  // The three refusals are one indistinguishable failure to the rails, and one
  // generic response to the client. The metric is the only thing that tells a
  // listed payer apart from a screener we could not reach.
  it("every_screen_records_what_it_decided", async () => {
    // The same S3 statuses as the refusal test above, plus the payment that
    // carried nothing to screen.
    const cases: [number, string][] = [
      [404, "allowed"],
      [200, "blocked"],
      [500, "error"],
    ];
    for (const [s3Status, expected] of cases) {
      const metrics = new Metrics();
      const screener = screenerAnswering(s3Status, metrics);

      await screener.requireAllowed("0xanything", rejected());

      await assertRecorded(metrics, `bx402_screenings_total{outcome="${expected}"} 1`);
      s3?.restore();
      s3 = undefined;
    }

    // Nothing to screen is counted apart from a screen that ran.
    const metrics = new Metrics();
    const screener = new RestrictedAddressScreener(unreachableS3Client(), TEST_BUCKET, metrics);
    await screener.requireAllowed(undefined, rejected());
    await assertRecorded(metrics, 'bx402_screenings_total{outcome="unidentified"} 1');
  });

  it("require_allowed_refuses_a_missing_identifier", async () => {
    // Refused without consulting anything. The endpoint is unreachable, so
    // reaching it would surface as a 503 instead.
    const screener = new RestrictedAddressScreener(
      unreachableS3Client(),
      TEST_BUCKET,
      new Metrics(),
    );
    const refusal = await screener.requireAllowed(undefined, rejected());
    expect(refusal?.status).toBe(402);
  });

  it("present_key_is_blocked", async () => {
    await expect(screenerAnswering(200).screen("0xanything")).resolves.toBe<Screening>("blocked");
  });

  it("absent_key_is_allowed", async () => {
    await expect(screenerAnswering(404).screen("0xanything")).resolves.toBe<Screening>("allowed");
  });

  // The screener never changes the casing of an identifier. Casing is the
  // caller's job. So two casings of one address are two different keys: the
  // exact stored string matches, a different casing does not.
  it("screens_the_identifier_verbatim", async () => {
    // A list entry stored in one exact casing (here, EIP-55 checksummed).
    const stored = "0xAb5801a7D398351b8bE11C439e05C5B3259aeC9B";
    const otherCasing = stored.toLowerCase();
    const storedKey = Buffer.from(stored, "utf8").toString("base64url");

    s3 = mockClient(S3Client);
    // Only the exact stored key exists; every other key is absent. The general
    // answer is declared first so the exact-key one overrides it.
    s3.on(HeadObjectCommand).rejects(notFound());
    s3.on(HeadObjectCommand, { Bucket: TEST_BUCKET, Key: storedKey }).resolves({});

    const screener = new RestrictedAddressScreener(new S3Client({}), TEST_BUCKET, new Metrics());
    expect(await screener.screen(stored), "the exact stored string matches").toBe("blocked");
    expect(
      await screener.screen(otherCasing),
      "a different casing is a different key; the screener never normalizes",
    ).toBe("allowed");
  });

  it("server_error_denies", async () => {
    // A non-404 error must not resolve to allowed.
    await expect(screenerAnswering(500).screen("0xanything")).rejects.toThrow(
      "address screening unavailable",
    );
  });

  it("unreachable_s3_denies", async () => {
    // Nothing listens on port 1: the request fails at the transport layer, which
    // is not a definite not-found, so the screener denies.
    const screener = new RestrictedAddressScreener(
      unreachableS3Client(),
      TEST_BUCKET,
      new Metrics(),
    );
    await expect(screener.screen("0xanything")).rejects.toThrow("address screening unavailable");
  });

  it("init_disabled_when_bucket_unset", async () => {
    const { screener, status } = await initScreener(testConfig(), new Metrics());
    expect(screener).toBeUndefined();
    expect(status.enabled).toBe(false);
  });

  it("init_enabled_when_bucket_reachable", async () => {
    s3 = mockClient(S3Client);
    // The probe heads the literal canary key; a 404 there means the bucket is
    // reachable. The catch-all failure makes the test pass only if that exact
    // key was hit.
    s3.on(HeadObjectCommand).rejects(s3Failure("S3Error", 500));
    s3.on(HeadObjectCommand, { Bucket: TEST_BUCKET, Key: CANARY_KEY }).rejects(notFound());

    const { status } = await initWith(new S3Client({}), TEST_BUCKET, new Metrics());
    expect(status.enabled).toBe(true);
  });

  it("init_enabled_when_canary_present", async () => {
    const { status } = await initWith(s3Answering(200), TEST_BUCKET, new Metrics());
    expect(status.enabled).toBe(true);
  });

  it("init_fails_fast_on_probe_error", async () => {
    await expect(initWith(s3Answering(403), TEST_BUCKET, new Metrics())).rejects.toThrow(
      "restricted address screening probe failed",
    );
  });

  it("status_display_reads_clearly", () => {
    expect(statusLine({ enabled: true, bucket: "restricted-address-bucket" })).toBe(
      "✓ enabled (bucket=restricted-address-bucket)",
    );
    expect(statusLine({ enabled: false })).toBe(
      "✗ disabled (RESTRICTED_ADDRESS_S3_BUCKET not set)",
    );
  });
});
