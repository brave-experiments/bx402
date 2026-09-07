import { createRequire } from "node:module";
import { describe, expect, it } from "vitest";
import { app, banner } from "../src/app.js";

const { version } = createRequire(import.meta.url)("../../package.json") as {
  version: string;
};

describe("app", () => {
  it("banner_includes_name_and_version", () => {
    expect(banner().startsWith("bx402 v")).toBe(true);
    expect(banner()).toContain(version);
  });

  it("health_returns_200", async () => {
    const response = await app().request("/health");
    expect(response.status).toBe(200);
  });
});
