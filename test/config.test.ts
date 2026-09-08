import { describe, expect, it } from "vitest";
import { type EnabledRails, parseEnabledRails } from "../src/config.js";

interface Case {
  /** Label printed if the assertion fails. */
  name: string;
  /** The `ENABLED_RAILS` value to parse. */
  value: string;
  /** The flags the value should parse to, or `undefined` when it must be rejected. */
  expected: EnabledRails | undefined;
}

describe("config", () => {
  it("parse_enabled_rails_accepts_only_known_rails", () => {
    const cases: Case[] = [
      { name: "both rails", value: "x402,mpp", expected: { x402: true, mpp: true } },
      { name: "both rails, either order", value: "mpp,x402", expected: { x402: true, mpp: true } },
      { name: "x402 only", value: "x402", expected: { x402: true, mpp: false } },
      { name: "mpp only", value: "mpp", expected: { x402: false, mpp: true } },
      {
        name: "whitespace around tokens",
        value: " x402 , mpp ",
        expected: { x402: true, mpp: true },
      },
      { name: "duplicate rail", value: "x402,x402", expected: { x402: true, mpp: false } },
      { name: "none turns every rail off", value: "none", expected: { x402: false, mpp: false } },
      { name: "none with whitespace", value: " none ", expected: { x402: false, mpp: false } },
      { name: "none is only valid alone", value: "none,x402", expected: undefined },
      { name: "empty", value: "", expected: undefined },
      { name: "only whitespace", value: "  ", expected: undefined },
      { name: "unknown rail", value: "btc", expected: undefined },
      { name: "rail names are lowercase", value: "X402", expected: undefined },
      { name: "trailing comma", value: "x402,", expected: undefined },
    ];

    for (const { name, value, expected } of cases) {
      if (expected !== undefined) {
        expect(parseEnabledRails(value), `case: ${name}`).toEqual(expected);
        continue;
      }
      let thrown: unknown;
      try {
        parseEnabledRails(value);
      } catch (err: unknown) {
        thrown = err;
      }
      expect(thrown, `case: ${name}`).toBeInstanceOf(Error);
      const message = (thrown as Error).message;
      expect(message, `case: ${name}, error was: ${message}`).toContain("ENABLED_RAILS");
    }
  });
});
