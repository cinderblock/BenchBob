import { expect, test } from "bun:test";
import { compareVersions } from "./update.ts";

test("compareVersions orders releases, pre-release builds and post-release builds", () => {
  const ordered = ["0.0.0~git85.83b68de", "0.0.0~git86.a1b2c3d", "0.1.0~git90.ffffff0", "0.1.0", "0.1.0+git4.abc1234", "0.1.0+git12.0000000", "0.1.1", "0.2.0", "1.0.0"];
  for (let i = 0; i < ordered.length; i++) {
    for (let j = 0; j < ordered.length; j++) {
      expect(Math.sign(compareVersions(ordered[i], ordered[j]))).toBe(Math.sign(i - j));
    }
  }
});

test("compareVersions accepts a v prefix and treats unparseable versions as equal", () => {
  expect(compareVersions("v0.2.0", "0.1.0")).toBeGreaterThan(0);
  expect(compareVersions("dev", "0.1.0")).toBe(0);
});
