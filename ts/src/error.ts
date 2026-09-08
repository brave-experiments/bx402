/**
 * Typed application errors, and the response shapes every module answers with.
 */

/**
 * The kind of failure behind an upstream error. A fixed set, so a failing
 * upstream cannot grow the number of metric series.
 */
export type UpstreamFailure = "timeout" | "connect" | "decode" | "transport";

/** Every error condition the service surfaces to a client. */
export type AppErrorKind =
  /**
   * The upstream Brave Search API call failed (connect, timeout, or transport
   * error). All of these become a `502 Bad Gateway` to our client.
   */
  | "upstream"
  /** The request was malformed, such as a missing or invalid query parameter. */
  | "badRequest"
  /** A required setting was absent at startup. */
  | "missingConfig"
  /**
   * A configured value could not initialize a startup dependency, such as an
   * unparseable URL. Like `missingConfig`, this aborts startup and never emits
   * a response.
   */
  | "invalidConfig";

/**
 * One error per failure mode the caller treats differently, not per cause. Each
 * kind maps to a single HTTP status in `toResponse`, so handlers can throw and
 * let one catch site turn the error into a response.
 */
export class AppError extends Error {
  readonly kind: AppErrorKind;
  /** Set only on an upstream error, naming what went wrong on the wire. */
  readonly failure: UpstreamFailure | undefined;

  private constructor(kind: AppErrorKind, message: string, failure?: UpstreamFailure) {
    super(message);
    this.name = "AppError";
    this.kind = kind;
    this.failure = failure;
  }

  static upstream(failure: UpstreamFailure): AppError {
    return new AppError("upstream", "upstream Brave Search API call failed", failure);
  }

  static badRequest(detail: string): AppError {
    return new AppError("badRequest", `invalid request: ${detail}`);
  }

  static missingConfig(name: string): AppError {
    return new AppError("missingConfig", `missing required configuration: ${name}`);
  }

  static invalidConfig(detail: string): AppError {
    return new AppError("invalidConfig", `invalid configuration: ${detail}`);
  }

  /**
   * The response a client sees. Only the mapped message crosses the wire; the
   * full error is left for the caller to log.
   */
  toResponse(): Response {
    switch (this.kind) {
      case "upstream":
        return jsonError(502, "upstream error");
      case "badRequest":
        // The detail is the part after the prefix the constructor added.
        return jsonError(400, this.message.slice("invalid request: ".length));
      case "missingConfig":
      case "invalidConfig":
        return jsonError(500, "server misconfigured");
    }
  }
}

/**
 * The one envelope for every error a client sees, shared by `AppError` and the
 * rails.
 */
export function jsonError(status: number, detail: string): Response {
  return Response.json({ error: detail }, { status });
}

/**
 * A response carrying no body, framed with an explicit zero length.
 *
 * Node's server falls back to chunked encoding for a null body, which tells a
 * client a body may still be coming. Saying the length outright keeps an empty
 * answer framed as one.
 */
export function emptyBody(status: number, headers?: Headers | Record<string, string>): Response {
  const stated = new Headers(headers);
  stated.set("content-length", "0");
  return new Response(null, { status, headers: stated });
}

/** A generic `503` for a payer that could not be screened, identical on every rail. */
export function serviceUnavailable(): Response {
  return jsonError(503, "service temporarily unavailable");
}
