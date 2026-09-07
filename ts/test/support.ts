import { S3Client } from "@aws-sdk/client-s3";
import { expect } from "vitest";
import type { Config } from "../src/config.js";
import type { Metrics } from "../src/metrics.js";

/**
 * A config whose every endpoint is parseable but unreachable, shared by the test
 * files; each test overrides the fields it exercises.
 */
export function testConfig(overrides: Partial<Config> = {}): Config {
  return {
    braveSearchApiKey: "secret-key",
    braveSearchApiBaseUrl: "http://upstream.invalid",
    x402: { facilitatorUrl: "http://facilitator.invalid" },
    mpp: { rpcUrl: "http://tempo.invalid", secretKey: "test-secret" },
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
