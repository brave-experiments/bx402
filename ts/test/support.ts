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
