import { describe, expect, it } from "vitest";
import { AppError } from "../src/error.js";

describe("error", () => {
  it("bad_request_maps_to_400", () => {
    expect(AppError.badRequest("q is required").toResponse().status).toBe(400);
  });

  it("missing_config_maps_to_500", () => {
    expect(AppError.missingConfig("BRAVE_SEARCH_API_KEY").toResponse().status).toBe(500);
  });

  it("invalid_config_maps_to_500", () => {
    expect(AppError.invalidConfig("X402_FACILITATOR_URL: bad").toResponse().status).toBe(500);
  });
});
