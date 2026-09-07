import type { Config } from "../src/config.js";

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
