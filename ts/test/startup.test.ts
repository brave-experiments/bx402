import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { beforeAll, describe, expect, it } from "vitest";

/** The repository root, two levels up from this file. */
const ROOT = join(import.meta.dirname, "..", "..");

/** The built entrypoint these tests run, the same one the container starts. */
const ENTRYPOINT = join(ROOT, "ts", "dist", "main.js");

/**
 * Start the service with `env` and hand back what it printed and how it exited.
 *
 * Run from the temp directory, because the entrypoint loads a `.env` beside its
 * working directory and a local one would otherwise supply the very settings a
 * test is proving are missing. The environment is built from scratch for the
 * same reason.
 */
function startWith(env: Record<string, string>): { code: number; stderr: string } {
  try {
    execFileSync("node", [ENTRYPOINT], {
      cwd: tmpdir(),
      env: { PATH: process.env.PATH ?? "", ...env },
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    });
  } catch (err: unknown) {
    const failure = err as { status?: number; stderr?: string };
    return { code: failure.status ?? 0, stderr: failure.stderr ?? "" };
  }
  throw new Error("the service started when it should have refused to");
}

beforeAll(() => {
  // These tests drive the built entrypoint rather than the modules, so build it
  // first. Compiling here keeps the test self-contained instead of depending on
  // whatever a previous command happened to leave in `dist`.
  execFileSync("pnpm", ["exec", "tsc", "-p", "tsconfig.build.json"], {
    cwd: ROOT,
    stdio: "ignore",
  });
}, 120_000);

describe("startup", () => {
  it("missing_api_key_reports_clear_message_and_exits_nonzero", () => {
    const { code, stderr } = startWith({});

    expect(code).toBe(1);
    expect(stderr, `stderr was: ${stderr}`).toContain(
      "missing required configuration: BRAVE_SEARCH_API_KEY",
    );
  });

  it("bad_enabled_rails_reports_clear_message_and_exits_nonzero", () => {
    // The API key is set explicitly, so the failure is the rails value and not
    // the setting the test above covers.
    const { code, stderr } = startWith({
      BRAVE_SEARCH_API_KEY: "test-key",
      ENABLED_RAILS: "btc",
    });

    expect(code).toBe(1);
    expect(stderr, `stderr was: ${stderr}`).toContain("invalid configuration: ENABLED_RAILS");
  });
});
