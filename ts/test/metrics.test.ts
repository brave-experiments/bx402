import { describe, expect, it } from "vitest";
import { Metrics } from "../src/metrics.js";
import { VERSION } from "../src/version.js";

describe("metrics", () => {
  it("the_exposition_carries_the_running_version", async () => {
    const rendered = await new Metrics().render();
    expect(rendered).toContain(`bx402_build_info{version="${VERSION}"} 1`);
  });

  it("the_exposition_ends_the_way_openmetrics_requires", async () => {
    const rendered = await new Metrics().render();
    expect(rendered.endsWith("# EOF\n")).toBe(true);
  });
});
