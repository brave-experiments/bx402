import { createRequire } from "node:module";
import { Hono } from "hono";
import type { Dispatcher } from "undici";
import type { Config } from "./config.js";
import { ENDPOINTS } from "./endpoints.js";
import { AppError } from "./error.js";
import { search, searchClient } from "./search.js";

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

/** The methods every route serves, and the value of the `Allow` header. */
const ALLOWED_METHODS = ["GET", "HEAD"];

/** Human-readable service banner, printed on startup. */
export function banner(): string {
  return `bx402 v${manifest.version}`;
}

/**
 * Build the HTTP application.
 *
 * Returns the app rather than serving it, so tests drive the same routes as the
 * binary without binding a socket. The upstream connection pool is passed in so
 * a test can hand over a mock dispatcher instead of reaching the network.
 */
export function app(config: Config, client: Dispatcher = searchClient()): Hono {
  const hono = new Hono();

  // Liveness probe: 200 with an empty body while the server is up.
  hono.on(ALLOWED_METHODS, HEALTH_PATH, (c) => c.body(null, 200));

  for (const endpoint of ENDPOINTS) {
    hono.on(ALLOWED_METHODS, endpoint.path, (c) => proxy(c.req.raw, config, client));
  }

  // Registered after the served methods, so it answers only a method those did
  // not match. A path we do not serve falls through to the 404 below instead.
  for (const path of [HEALTH_PATH, ...ENDPOINTS.map((endpoint) => endpoint.path)]) {
    hono.all(path, () => methodNotAllowed());
  }

  // An unlisted path is a 404 with an empty body, never a payable route.
  hono.notFound(() => new Response(null, { status: 404 }));

  return hono;
}

/**
 * Proxy a paid Brave Search API endpoint upstream.
 *
 * Relays the upstream status, content type, and body back to the caller byte for
 * byte. The path is taken from the request and forwarded unchanged. Only paths
 * in the catalog are routed here, so an unlisted path is a 404 from the router
 * and never reaches this handler. That is what keeps the proxy closed: a caller
 * cannot name an arbitrary upstream path.
 */
async function proxy(request: Request, config: Config, client: Dispatcher): Promise<Response> {
  const url = new URL(request.url);
  try {
    const upstream = await search(client, config, url.pathname, rawQuery(request.url));
    const headers = new Headers();
    if (upstream.contentType !== undefined) {
      headers.set("content-type", upstream.contentType);
    }
    // A HEAD carries the headers of the GET and none of the body, and a status
    // that forbids a body must not be given one.
    const body =
      request.method === "HEAD" || upstream.body.length === 0
        ? null
        : new Uint8Array(upstream.body);
    return new Response(body, { status: upstream.status, headers });
  } catch (err: unknown) {
    if (err instanceof AppError) {
      return err.toResponse();
    }
    throw err;
  }
}

/**
 * The query string exactly as the client sent it. Taken off the raw URL rather
 * than through `URLSearchParams`, which would re-encode `+` and reorder nothing
 * for free.
 */
export function rawQuery(url: string): string {
  const start = url.indexOf("?");
  return start === -1 ? "" : url.slice(start + 1);
}

/** The router's answer to a method a served path does not offer. */
function methodNotAllowed(): Response {
  return new Response(null, {
    status: 405,
    headers: { allow: ALLOWED_METHODS.join(",") },
  });
}
