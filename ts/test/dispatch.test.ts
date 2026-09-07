import { describe, expect, it } from "vitest";
import {
  absoluteUri,
  absoluteUriFrom,
  classify,
  type Rail,
  type UriParts,
} from "../src/dispatch.js";

interface ClassifyCase {
  /** Label printed if the assertion fails. */
  name: string;
  /** Request headers to send, as name and value pairs. */
  headers: [string, string][];
  /** The rail `classify` should return for those headers. */
  expected: Rail;
}

interface UriCase {
  name: string;
  uri: string;
  headers: [string, string][];
  expected: string;
}

/** The pieces `absoluteUriFrom` reads, taken from a target and its headers. */
function partsOf(uri: string, headers: [string, string][]): UriParts {
  const map = new Map(headers.map(([name, value]) => [name.toLowerCase(), value]));
  return {
    target: uri,
    host: map.get("host"),
    forwardedProto: map.get("x-forwarded-proto"),
    uriScheme: undefined,
    uriAuthority: undefined,
  };
}

describe("dispatch", () => {
  it("classify_by_payment_headers", () => {
    const cases: ClassifyCase[] = [
      { name: "cold", headers: [], expected: "none" },
      { name: "x402 v2", headers: [["payment-signature", "sig"]], expected: "x402" },
      { name: "mpp", headers: [["authorization", "cred"]], expected: "mpp" },
      {
        name: "both",
        headers: [
          ["payment-signature", "sig"],
          ["authorization", "cred"],
        ],
        expected: "both",
      },
      // x402 V1 wire (`X-PAYMENT`) is not accepted, so it reads as no payment.
      { name: "x402 v1 ignored", headers: [["x-payment", "sig"]], expected: "none" },
      // A V1 header alongside MPP is therefore an MPP attempt, not a collision.
      {
        name: "x402 v1 + mpp",
        headers: [
          ["x-payment", "sig"],
          ["authorization", "cred"],
        ],
        expected: "mpp",
      },
      // Header names are case-insensitive, so the client's casing must never
      // change classification.
      {
        name: "mixed-case names",
        headers: [
          ["Payment-Signature", "sig"],
          ["AUTHORIZATION", "cred"],
        ],
        expected: "both",
      },
    ];
    for (const { name, headers, expected } of cases) {
      expect(classify(new Headers(headers)), `case: ${name}`).toBe(expected);
    }
  });

  it("absolute_uri_rebuilds_the_requested_url", () => {
    const cases: UriCase[] = [
      {
        name: "forwarded proto and host, query kept",
        uri: "/res/v1/web/search?q=rust",
        headers: [
          ["host", "bx402.example.com"],
          ["x-forwarded-proto", "https"],
        ],
        expected: "https://bx402.example.com/res/v1/web/search?q=rust",
      },
      {
        name: "no host falls back to path and query",
        uri: "/res/v1/web/search?q=rust",
        headers: [],
        expected: "/res/v1/web/search?q=rust",
      },
      // A client refuses to pay a challenge naming a different URL than it asked
      // for, so the query comes back exactly as sent. Re-encoding `+` as `%2B`
      // (or the reverse) would break that comparison.
      {
        name: "query repeated byte for byte",
        uri: "/res/v1/web/search?q=base+sepolia&count=2",
        headers: [["host", "localhost:8080"]],
        expected: "http://localhost:8080/res/v1/web/search?q=base+sepolia&count=2",
      },
      {
        name: "scheme defaults to http",
        uri: "/res/v1/web/search",
        headers: [["host", "localhost:8080"]],
        expected: "http://localhost:8080/res/v1/web/search",
      },
      // A non-canonical `Host` comes back in the form URL parsers produce.
      {
        name: "host is lowercased",
        uri: "/res/v1/web/search",
        headers: [
          ["host", "API.bx402.io"],
          ["x-forwarded-proto", "HTTPS"],
        ],
        expected: "https://api.bx402.io/res/v1/web/search",
      },
      {
        name: "default https port dropped",
        uri: "/res/v1/web/search",
        headers: [
          ["host", "api.bx402.io:443"],
          ["x-forwarded-proto", "https"],
        ],
        expected: "https://api.bx402.io/res/v1/web/search",
      },
      {
        name: "default http port dropped",
        uri: "/res/v1/web/search",
        headers: [["host", "api.bx402.io:80"]],
        expected: "http://api.bx402.io/res/v1/web/search",
      },
      // Behind chained proxies each hop appends to `X-Forwarded-Proto`; the
      // first entry is the scheme the client actually used.
      {
        name: "forwarded proto list reads the first hop",
        uri: "/res/v1/web/search",
        headers: [
          ["host", "api.bx402.io"],
          ["x-forwarded-proto", "https, http"],
        ],
        expected: "https://api.bx402.io/res/v1/web/search",
      },
      {
        name: "empty forwarded proto falls back to http",
        uri: "/res/v1/web/search",
        headers: [
          ["host", "api.bx402.io"],
          ["x-forwarded-proto", ""],
        ],
        expected: "http://api.bx402.io/res/v1/web/search",
      },
      {
        name: "non-default port kept",
        uri: "/res/v1/web/search",
        headers: [["host", "api.bx402.io:443"]],
        expected: "http://api.bx402.io:443/res/v1/web/search",
      },
    ];
    for (const { name, uri, headers, expected } of cases) {
      expect(absoluteUriFrom(partsOf(uri, headers)), `case: ${name}`).toBe(expected);
    }
  });

  // The fetch API always carries an authority on the request URL, so this checks
  // the wiring the vectors above cannot reach through a live request.
  it("absolute_uri_reads_a_live_request", () => {
    const request = new Request("http://ignored.invalid/res/v1/web/search?q=a+b", {
      headers: { host: "API.bx402.io:443", "x-forwarded-proto": "https" },
    });
    expect(absoluteUri(request)).toBe("https://api.bx402.io/res/v1/web/search?q=a+b");
  });
});
