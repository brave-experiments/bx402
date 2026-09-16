import { afterEach, describe, expect, it, vi } from "vitest";
import { ClaimStore, type ClaimToken } from "../src/claims.js";

/** An expiry comfortably in the future, for claims that must hold. */
function later(): number {
  return Date.now() + 60_000;
}

/** Claim `key`, asserting the store handed the claim out. */
function claim(store: ClaimStore, key: string, expires: number): ClaimToken {
  const token = store.tryClaim(key, expires);
  if (token === undefined) {
    throw new Error(`${key} is already claimed`);
  }
  return token;
}

describe("claims", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("a_key_claims_once_and_a_second_claim_is_refused", () => {
    const store = new ClaimStore();
    claim(store, "key", later());
    expect(store.tryClaim("key", later())).toBeUndefined();
  });

  it("a_released_key_can_be_claimed_again", () => {
    const store = new ClaimStore();
    const token = claim(store, "key", later());
    store.release("key", token);
    claim(store, "key", later());
  });

  it("a_lapsed_claim_can_be_claimed_again", () => {
    const store = new ClaimStore();
    claim(store, "key", Date.now() - 1);
    claim(store, "key", later());
  });

  it("a_stale_token_does_not_release_the_current_claim", () => {
    // The first claim lapses and a second caller claims the key. When the
    // first caller then releases, the second claim must survive, or a third
    // caller could run concurrently with the second.
    const store = new ClaimStore();
    const stale = claim(store, "key", Date.now() - 1);
    claim(store, "key", later());
    store.release("key", stale);
    expect(store.tryClaim("key", later())).toBeUndefined();
  });

  it("lapsed_entries_are_swept_out_after_the_sweep_interval", () => {
    vi.useFakeTimers();
    const store = new ClaimStore();
    for (let i = 0; i < 100; i++) {
      claim(store, `lapsed-${i}`, Date.now() + 1);
    }
    // The sweep runs at most once per interval, so the lapsed entries linger
    // until the interval passes and the next claim walks the map.
    vi.advanceTimersByTime(61_000);
    claim(store, "fresh", later());
    expect(store.size).toBe(1);
  });

  it("distinct_keys_do_not_contend", () => {
    const store = new ClaimStore();
    claim(store, "one", later());
    claim(store, "two", later());
  });
});
