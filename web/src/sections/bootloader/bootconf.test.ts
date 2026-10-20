import { describe, expect, it } from "vitest";
import { blRest, blSet, blValues, orderAdd, orderHex, orderItems, orderWarnings } from "./bootconf";

const CONF = "[all]\nBOOT_UART=0\nBOOT_ORDER=0xf41\n\n[gpio4=0]\nBOOT_ORDER=0xf14\n\n[config.txt]\ndtoverlay=x\n";

describe("boot.conf editing", () => {
  it("reads only the unconditional keys", () => {
    expect(blValues(CONF)).toEqual({ BOOT_UART: "0", BOOT_ORDER: "0xf41" });
  });

  it("rewrites a key in place and leaves conditional sections alone", () => {
    const t = blSet(CONF, "BOOT_ORDER", "0xf461");
    expect(blValues(t).BOOT_ORDER).toBe("0xf461");
    expect(t).toContain("[gpio4=0]\nBOOT_ORDER=0xf14");
    expect(blRest(t)).toBe(blRest(CONF));
  });

  it("adds a new key after the first block and removes keys", () => {
    const t = blSet(CONF, "PARTITION_WALK", "1");
    expect(t.split("\n").indexOf("PARTITION_WALK=1")).toBe(3);
    expect(blValues(blSet(t, "BOOT_UART", null))).not.toHaveProperty("BOOT_UART");
  });

  it("reads BOOT_ORDER right to left", () => {
    expect(orderItems("0xf41")).toEqual(["1", "4", "f"]);
    expect(orderHex(["1", "4", "f"])).toBe("0xf41");
    expect(orderItems("bogus")).toBeNull();
    expect(orderAdd(["1", "f"], "4")).toEqual(["1", "4", "f"]);
  });

  it("warns about orders that can't work", () => {
    expect(orderWarnings(["1", "4"], { chip: "2711", cm: false })[0]).toMatch(/No Restart/);
    expect(orderWarnings(["6", "f"], { chip: "2711", cm: false })).toContain("The Pi 4 has no NVMe boot.");
    expect(orderWarnings(["6", "f"], { chip: "2711", cm: true })).toEqual([]);
  });
});
