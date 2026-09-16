/**
 * In-memory claims over payment replay keys.
 *
 * A claim records the first use of a key until it expires, so concurrent
 * requests carrying the same payment can be told apart from the first one.
 * Claims live in one process, the same scope the MPP rail's store uses, and
 * every entry expires, so the map stays bounded by the paid request rate
 * times the claim window.
 */

/** How often lapsed entries are swept out, in milliseconds. */
const SWEEP_INTERVAL_MS = 60_000;

/** Proof of holding a claim. Only the holder can release its key. */
export type ClaimToken = symbol;

export class ClaimStore {
  /** Each claimed key mapped to when its claim lapses and who holds it. */
  private readonly claims = new Map<string, { expires: number; token: ClaimToken }>();

  /** When the next sweep is due, so the map is walked at most once per interval. */
  private sweepDue = 0;

  /**
   * Record the first use of `key` until `expires`, an epoch millisecond.
   * The token to release it with when this call recorded the key,
   * `undefined` when the key is already claimed and its claim has not
   * lapsed. The check and the write run with no await between them, so on
   * Node's single event loop two callers can never both claim a key.
   */
  tryClaim(key: string, expires: number): ClaimToken | undefined {
    const now = Date.now();
    this.sweep(now);
    const current = this.claims.get(key);
    if (current !== undefined && current.expires > now) {
      return undefined;
    }
    const token = Symbol(key);
    this.claims.set(key, { expires, token });
    return token;
  }

  /**
   * Release the claim on `key` held with `token`. A token releases only the
   * claim it was handed out for, so a caller whose claim lapsed cannot drop
   * the key's current holder.
   */
  release(key: string, token: ClaimToken): void {
    if (this.claims.get(key)?.token === token) {
      this.claims.delete(key);
    }
  }

  /** The number of claims held, for tests that watch the map stay bounded. */
  get size(): number {
    return this.claims.size;
  }

  /**
   * Drop lapsed entries, at most once per interval so the walk stays off the
   * per-request path. Between sweeps the expiry check in `tryClaim` treats a
   * lapsed entry as absent.
   */
  private sweep(now: number): void {
    if (now < this.sweepDue) {
      return;
    }
    this.sweepDue = now + SWEEP_INTERVAL_MS;
    for (const [key, { expires }] of this.claims) {
      if (expires <= now) {
        this.claims.delete(key);
      }
    }
  }
}
