import { describe, expect, test } from "bun:test";
import { editOverlays, readOverlays } from "./boot-env.ts";
import { unifiedDiff } from "./gpio-config.ts";

// A stock Armbian armbianEnv.txt for a Rockchip board (the layout Armbian's build writes:
// verbosity, bootlogo, console, overlay_prefix, fdtfile, rootdev, rootfstype, usbstoragequirks).
const ARMBIAN = `verbosity=1
bootlogo=false
console=both
overlay_prefix=rockchip-rk3588
fdtfile=rockchip/rk3588-orangepi-5-plus.dtb
rootdev=UUID=6b3f1e2a-0c1d-4a55-9a41-3c2f8a1e7d10
rootfstype=ext4
usbstoragequirks=0x2537:0x1066:u,0x2537:0x1068:u
`;

// BeagleY-AI's extlinux.conf as shipped (docs.beagleboard.org), with a second label.
const EXTLINUX = `label microSD (default)
    kernel /Image
    append console=ttyS2,115200n8 root=/dev/mmcblk1p3 ro rootfstype=ext4 rootwait net.ifnames=0
    fdtdir /
    fdt /ti/k3-am67a-beagley-ai.dtb
    initrd /initrd.img

label recovery
    kernel /Image
    fdt /ti/k3-am67a-beagley-ai.dtb
    fdtoverlays /overlays/old.dtbo
`;

describe("armbianEnv.txt / orangepiEnv.txt", () => {
  test("read a stock file: no overlays yet, prefix known", () => {
    expect(readOverlays("env", ARMBIAN)).toEqual({ enabled: [], prefix: "rockchip-rk3588", user: [], label: null });
  });

  test("enable appends one overlays= line; round trip back to the original", () => {
    const on = editOverlays("env", ARMBIAN, { enable: ["i2c5-m3", "uart1-m1"] });
    expect(on).toBe(ARMBIAN.replace(/\n$/, "\noverlays=i2c5-m3 uart1-m1\n"));
    expect(readOverlays("env", on).enabled).toEqual(["i2c5-m3", "uart1-m1"]);
    const diff = unifiedDiff(ARMBIAN, on, "/boot/armbianEnv.txt");
    expect(diff.split("\n").filter((l) => /^[+-][^+-]/.test(l))).toEqual(["+overlays=i2c5-m3 uart1-m1"]);
    const off = editOverlays("env", editOverlays("env", on, { disable: ["uart1-m1"] }), { disable: ["i2c5-m3"] });
    expect(off).toBe(ARMBIAN);
  });

  test("the last overlays= wins and is the one edited; order and CRLF kept", () => {
    const t = "overlays=old\r\nfoo=1\r\noverlays=spi4-m0-cs1-spidev  pwm14-m0\r\nuser_overlays=my-sensor\r\n";
    expect(readOverlays("env", t)).toMatchObject({ enabled: ["spi4-m0-cs1-spidev", "pwm14-m0"], user: ["my-sensor"] });
    const n = editOverlays("env", t, { enable: ["i2c5-m3"], disable: ["spi4-m0-cs1-spidev"] });
    expect(n).toBe("overlays=old\r\nfoo=1\r\noverlays=pwm14-m0 i2c5-m3\r\nuser_overlays=my-sensor\r\n");
  });

  test("no change returns the same text; bad names are refused", () => {
    expect(editOverlays("env", ARMBIAN, { disable: ["nothing"] })).toBe(ARMBIAN);
    expect(() => editOverlays("env", ARMBIAN, { enable: ["a b"] })).toThrow();
    expect(() => editOverlays("env", ARMBIAN, { enable: ["x\nrootdev=/dev/sda1"] })).toThrow();
  });
});

describe("extlinux.conf", () => {
  test("reads the default (first) label only", () => {
    expect(readOverlays("extlinux", EXTLINUX)).toEqual({ enabled: [], prefix: null, user: [], label: "microSD (default)" });
  });

  test("adds fdtoverlays after fdt, indented the same; removing restores the file", () => {
    const p = "/overlays/k3-am67a-beagley-ai-spi0-2cs.dtbo";
    const on = editOverlays("extlinux", EXTLINUX, { enable: [p] });
    expect(on).toContain(`    fdt /ti/k3-am67a-beagley-ai.dtb\n    fdtoverlays ${p}\n    initrd`);
    expect(readOverlays("extlinux", on).enabled).toEqual([p]);
    // The recovery label's own fdtoverlays is untouched.
    expect(on).toContain("fdtoverlays /overlays/old.dtbo");
    expect(editOverlays("extlinux", on, { disable: [p] })).toBe(EXTLINUX);
  });

  test("a `default` line picks the label", () => {
    const t = `default recovery\n${EXTLINUX}`;
    expect(readOverlays("extlinux", t)).toMatchObject({ label: "recovery", enabled: ["/overlays/old.dtbo"] });
    expect(editOverlays("extlinux", t, { enable: ["/overlays/b.dtbo"] })).toContain("fdtoverlays /overlays/old.dtbo /overlays/b.dtbo");
  });
});
