import { createRequire } from "node:module";
import { Hono } from "hono";

/**
 * Package metadata, read from the manifest so the version has one source of
 * truth. The manifest sits two levels up from `ts/src` and from the compiled
 * `ts/dist` alike, so the same path works in both.
 */
const manifest = createRequire(import.meta.url)("../../package.json") as {
  version: string;
};

/**
 * Liveness probe path, kept in one place so the route and its metric label
 * cannot drift apart.
 */
export const HEALTH_PATH = "/health";

/** Human-readable service banner, printed on startup. */
export function banner(): string {
  return `bx402 v${manifest.version}`;
}

/**
 * Build the HTTP application.
 *
 * Returns the app rather than serving it, so tests drive the same routes as the
 * binary without binding a socket.
 */
export function app(): Hono {
  const hono = new Hono();
  // Liveness probe: 200 with an empty body while the server is up.
  hono.get(HEALTH_PATH, (c) => c.body(null, 200));
  return hono;
}
