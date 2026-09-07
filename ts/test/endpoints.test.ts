import { describe, expect, it } from "vitest";
import { ENDPOINTS, find, UTILITY_RATE } from "../src/endpoints.js";

describe("endpoints", () => {
  it("every_path_is_listed_once", () => {
    const distinct = new Set(ENDPOINTS.map((endpoint) => endpoint.path));
    expect(distinct.size).toBe(ENDPOINTS.length);
  });

  it("the_utility_rate_applies_to_suggest_and_spellcheck", () => {
    const utility = ENDPOINTS.filter((endpoint) => endpoint.priceBaseUnits === UTILITY_RATE).map(
      (endpoint) => endpoint.path,
    );
    expect(utility).toEqual(["/res/v1/suggest/search", "/res/v1/spellcheck/search"]);
  });

  it("find_matches_a_served_path_exactly", () => {
    const found = find("/res/v1/images/search");
    expect(found?.priceBaseUnits).toBe(5_000);

    // The Answers API is not sold, and a prefix of a served path is not a served path.
    expect(find("/res/v1/chat/completions")).toBeUndefined();
    expect(find("/res/v1/images")).toBeUndefined();
  });
});
