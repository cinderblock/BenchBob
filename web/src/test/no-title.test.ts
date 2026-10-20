// Hover tooltips are invisible on touch screens, and the dashboard is used from phones, so
// no element may carry a title attribute: say it inline or behind a tap (ui/common Hint).
import { describe, expect, it } from "vitest";

const sources = import.meta.glob("../**/*.tsx", { query: "?raw", import: "default", eager: true }) as Record<string, string>;

describe("no title= tooltips", () => {
  it("has sources to check", () => expect(Object.keys(sources).length).toBeGreaterThan(20));
  for (const [file, src] of Object.entries(sources)) {
    if (file.endsWith(".test.tsx")) continue;
    it(file, () => {
      const hits = src.split("\n").filter((l) => /\stitle=[{"']/.test(l) || /\btitle:\s*["'`]/.test(l) && /setAttribute|\.title\s*=/.test(l));
      expect(hits).toEqual([]);
    });
  }
});
