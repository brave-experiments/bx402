/**
 * Typed application errors, the response shapes every module answers with, and
 * the narrowing guard for reading values whose shape the sender controls.
 */

/**
 * Narrows a decoded JSON value to an object whose fields can be read. Arrays
 * pass like any other object; a caller's checks on the fields it reads are
 * what refuse them.
 */
export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

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
 * factory fixes the HTTP status and the client-visible detail, so handlers can
 * throw and let one catch site turn the error into a response.
 */
export class AppError extends Error {
  readonly kind: AppErrorKind;
  /** The HTTP status `toResponse` answers with. */
  readonly status: number;
  /** The client-visible wording `toResponse` carries. */
  readonly detail: string;
  /** Set only on an upstream error, naming what went wrong on the wire. */
  readonly failure: UpstreamFailure | undefined;

  private constructor(
    kind: AppErrorKind,
    message: string,
    status: number,
    detail: string,
    failure?: UpstreamFailure,
  ) {
    super(message);
    this.name = "AppError";
    this.kind = kind;
    this.status = status;
    this.detail = detail;
    this.failure = failure;
  }

  static upstream(failure: UpstreamFailure): AppError {
    return new AppError(
      "upstream",
      "upstream Brave Search API call failed",
      502,
      "upstream error",
      failure,
    );
  }

  static badRequest(detail: string): AppError {
    return new AppError("badRequest", `invalid request: ${detail}`, 400, detail);
  }

  static missingConfig(name: string): AppError {
    return new AppError(
      "missingConfig",
      `missing required configuration: ${name}`,
      500,
      "server misconfigured",
    );
  }

  static invalidConfig(detail: string): AppError {
    return new AppError(
      "invalidConfig",
      `invalid configuration: ${detail}`,
      500,
      "server misconfigured",
    );
  }

  /** Narrows an upstream error, whose `failure` field is always set. */
  isUpstream(): this is AppError & { failure: UpstreamFailure } {
    return this.kind === "upstream";
  }

  /**
   * The response a client sees. Only the fixed detail crosses the wire; the
   * full error is left for the caller to log.
   */
  toResponse(): Response {
    return jsonError(this.status, this.detail);
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
