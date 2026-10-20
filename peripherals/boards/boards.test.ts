import { describe, expect, test } from "bun:test";
import { BOARDS, boardInfo, bootConfigOf, describe as describeBoard, detect, resolveChips, type DetectInput } from "./index.ts";
import { dmiName } from "./x86.ts";

const NO_DMI = { sysVendor: "", productName: "", productVersion: "", boardVendor: "", boardName: "" };
const arm = (compatible: string[], model = ""): DetectInput => ({ arch: "arm64", compatible, model, dmi: NO_DMI, wsl: false });
const pc = (dmi: Partial<DetectInput["dmi"]>, wsl = false): DetectInput => ({ arch: "x64", compatible: [], model: "", dmi: { ...NO_DMI, ...dmi }, wsl });

// /proc/device-tree/compatible and model for each board, from the kernel dts files the
// board files cite (and, for desky, read off the machine).
const FIXTURES: [string, DetectInput, string][] = [
  ["rpi", arm(["raspberrypi,4-model-b", "brcm,bcm2711"], "Raspberry Pi 4 Model B Rev 1.1"), "Raspberry Pi 4 Model B Rev 1.1"], // desky
  ["rpi", arm(["raspberrypi,5-model-b", "brcm,bcm2712"], "Raspberry Pi 5 Model B Rev 1.0"), "Raspberry Pi 5 Model B Rev 1.0"],
  ["rpi", arm(["raspberrypi,model-zero-2-w", "brcm,bcm2837"], "Raspberry Pi Zero 2 W Rev 1.0"), "Raspberry Pi Zero 2 W Rev 1.0"],
  ["rpi", arm(["raspberrypi,4-compute-module", "brcm,bcm2711"], "Raspberry Pi Compute Module 4 Rev 1.0"), "Raspberry Pi Compute Module 4 Rev 1.0"],
  // Orange Pi: mainline strings, then Orange Pi's own kernel's.
  ["orangepi-5", arm(["xunlong,orangepi-5", "rockchip,rk3588s"], "Xunlong Orange Pi 5"), "Xunlong Orange Pi 5"],
  ["orangepi-5", arm(["rockchip,rk3588s-orangepi-5", "rockchip,rk3588"], "Orange Pi 5"), "Orange Pi 5"],
  ["orangepi-5b", arm(["xunlong,orangepi-5b", "rockchip,rk3588s"], "Xunlong Orange Pi 5B"), "Xunlong Orange Pi 5B"],
  ["orangepi-5-plus", arm(["xunlong,orangepi-5-plus", "rockchip,rk3588"], "Xunlong Orange Pi 5 Plus"), "Xunlong Orange Pi 5 Plus"],
  ["orangepi-5-plus", arm(["rockchip,rk3588-orangepi-5-plus", "rockchip,rk3588"], "RK3588 OPi 5 Plus"), "RK3588 OPi 5 Plus"],
  ["orangepi-5-other", arm(["xunlong,orangepi-5-max", "rockchip,rk3588"], "Xunlong Orange Pi 5 Max"), "Xunlong Orange Pi 5 Max"],
  ["orangepi-5-other", arm(["rockchip,rk3588s-orangepi-5-pro", "rockchip,rk3588"], "RK3588S OPi 5 Pro"), "RK3588S OPi 5 Pro"],
  ["orangepi-3b", arm(["xunlong,orangepi-3b-v2.1", "xunlong,orangepi-3b", "rockchip,rk3566"], "Xunlong Orange Pi 3B v2.1"), "Xunlong Orange Pi 3B v2.1"],
  ["orangepi-3b", arm(["rockchip,rk3566-orangepi-3b", "rockchip,rk3566"], "Rockchip RK3566 OPi 3B"), "Rockchip RK3566 OPi 3B"],
  ["orangepi-zero3", arm(["xunlong,orangepi-zero3", "allwinner,sun50i-h618"], "OrangePi Zero3"), "OrangePi Zero3"],
  ["orangepi-zero3", arm(["xunlong,orangepi-zero3", "allwinner,sun50i-h616"], "OrangePi Zero3"), "OrangePi Zero3"],
  ["orangepi-zero2w", arm(["xunlong,orangepi-zero2w", "allwinner,sun50i-h618"], "OrangePi Zero 2W"), "OrangePi Zero 2W"],
  ["rock-5b", arm(["radxa,rock-5b", "rockchip,rk3588"], "Radxa ROCK 5B"), "Radxa ROCK 5B"],
  ["rock-5b-plus", arm(["radxa,rock-5b-plus", "rockchip,rk3588"], "Radxa ROCK 5B+"), "Radxa ROCK 5B+"],
  ["beagley-ai", arm(["beagle,am67a-beagley-ai", "ti,j722s"], "BeagleBoard.org BeagleY-AI"), "BeagleBoard.org BeagleY-AI"],
  ["beaglebone-ai64", arm(["beagle,j721e-beagleboneai64", "ti,j721e"], "BeagleBoard.org BeagleBone AI-64"), "BeagleBoard.org BeagleBone AI-64"],
  ["le-potato", arm(["libretech,aml-s905x-cc", "amlogic,s905x", "amlogic,meson-gxl"], "Libre Computer AML-S905X-CC"), "Libre Computer AML-S905X-CC (Le Potato)"],
  ["le-potato", arm(["libretech,aml-s905x-cc-v2", "amlogic,s905x", "amlogic,meson-gxl"], "Libre Computer AML-S905X-CC V2"), "Libre Computer AML-S905X-CC V2 (Sweet Potato)"],
  ["roc-rk3328-cc", arm(["firefly,roc-rk3328-cc", "rockchip,rk3328"], "Firefly ROC-RK3328-CC"), "Firefly ROC-RK3328-CC (Renegade)"],
  ["x86", pc({ sysVendor: "ASUSTeK COMPUTER INC.", productName: "MINIPC PN51-E1", boardVendor: "ASUSTeK COMPUTER INC.", boardName: "PN51-E1" }), "ASUSTeK COMPUTER INC. MINIPC PN51-E1"],
  ["x86", pc({ sysVendor: "Intel(R) Client Systems", productName: "NUC8i5BEH" }), "Intel(R) Client Systems NUC8i5BEH"],
  ["x86", pc({ sysVendor: "To be filled by O.E.M.", productName: "To be filled by O.E.M.", boardVendor: "AZW", boardName: "SEi" }), "AZW SEi"],
  ["x86", pc({ sysVendor: "Default string", productName: "Default string" }), "x86-64 PC"],
  ["x86", pc({}, true), "Windows Subsystem for Linux"], // bbtest-trixie: no DMI at all
  ["generic", arm(["pine64,rockpro64-v2.1", "pine64,rockpro64", "rockchip,rk3399"], "Pine64 RockPro64 v2.1"), "Pine64 RockPro64 v2.1"],
  ["generic", { arch: "riscv64", compatible: ["sifive,hifive-unmatched-a00"], model: "SiFive HiFive Unmatched A00", dmi: NO_DMI, wsl: false }, "SiFive HiFive Unmatched A00"],
];

describe("board detection", () => {
  for (const [id, input, name] of FIXTURES)
    test(`${input.model || input.dmi.productName || (input.wsl ? "WSL" : "PC")} → ${id}`, () => {
      const def = detect(input);
      expect(def.id).toBe(id);
      expect(describeBoard(def, input).name).toBe(name);
    });

  test("only the Pi 4 family counts as tested among Pis", () => {
    const pi4 = FIXTURES[0][1], pi5 = FIXTURES[1][1];
    expect(describeBoard(detect(pi4), pi4).tested).toBe(true);
    expect(describeBoard(detect(pi5), pi5).tested).toBe(false);
  });

  test("ids are unique and generic is the fallback", () => {
    expect(new Set(BOARDS.map((b) => b.id)).size).toBe(BOARDS.length);
    expect(detect(arm([])).id).toBe("generic");
  });

  test("boardInfo shape", () => {
    const i = FIXTURES[0][1];
    const info = boardInfo(describeBoard(detect(i), i));
    expect(info).toMatchObject({ id: "rpi", soc: "BCM2711", tested: true, capabilities: { gpio: "rpi", bootConfig: { kind: "config.txt" }, health: "vcgencmd", headers: [{ id: "j8", pins: 40 }] } });
  });

  test("boot config is whichever file this image has", () => {
    // A board that may run Armbian or its vendor's image (as the Orange Pis do).
    const opi = { ...BOARDS.find((b) => b.id === "generic")!, bootConfig: { files: [{ kind: "armbianEnv" as const, path: "/boot/armbianEnv.txt" }, { kind: "orangepiEnv" as const, path: "/boot/orangepiEnv.txt" }] } };
    expect(bootConfigOf(opi, (p) => p === "/boot/orangepiEnv.txt")).toEqual({ kind: "orangepiEnv", path: "/boot/orangepiEnv.txt" });
    expect(bootConfigOf(opi, (p) => p.startsWith("/boot/"))?.kind).toBe("armbianEnv"); // Armbian's first
    expect(bootConfigOf(opi, () => false)).toBeNull();
    expect(bootConfigOf(BOARDS.find((b) => b.id === "rpi")!, () => false)?.kind).toBe("config.txt");
    expect(bootConfigOf(BOARDS.find((b) => b.id === "x86")!)).toBeNull();
  });

  test("overlay catalogs only name pins that are GPIOs on the board's first header", () => {
    for (const b of BOARDS)
      for (const o of b.bootConfig?.overlays || []) {
        const h = b.headers[0];
        for (const p of o.pins) expect(`${b.id} ${o.name} pin ${p}: ${h.pins.find((x) => x.pin === p)?.kind}`).toBe(`${b.id} ${o.name} pin ${p}: gpio`);
      }
  });

  test("dmiName drops placeholders and repeated vendors", () => {
    expect(dmiName(pc({ sysVendor: "LENOVO", productName: "LENOVO ThinkCentre" }))).toBe("LENOVO ThinkCentre");
    expect(dmiName(pc({ sysVendor: "System manufacturer", productName: "System Product Name", boardVendor: "ASUSTeK COMPUTER INC.", boardName: "PRIME B450M-A" }))).toBe("ASUSTeK COMPUTER INC. PRIME B450M-A");
  });
});

describe("header maps", () => {
  for (const b of BOARDS)
    for (const h of b.headers)
      test(`${b.id} ${h.name}`, () => {
        expect([13, 26, 40, 46]).toContain(h.pins.length);
        // Physical pins 1..N in order, each once.
        expect(h.pins.map((p) => p.pin)).toEqual(h.pins.map((_, i) => i + 1));
        const gpio = h.pins.filter((p) => p.kind === "gpio");
        expect(gpio.length).toBeGreaterThan(0);
        expect(h.pins.some((p) => p.kind === "gnd")).toBe(true);
        if (b.gpio === "rpi") {
          const nums = gpio.map((p) => p.gpio);
          expect(new Set(nums).size).toBe(nums.length);
          return;
        }
        // Every GPIO pin names a known chip and a line, and no line appears twice.
        for (const p of gpio) {
          expect(b.chips?.[p.chip!]).toBeDefined();
          expect(Number.isInteger(p.line) && p.line! >= 0 && p.line! < 512).toBe(true);
          expect(p.soc).toBeTruthy();
        }
        const keys = gpio.map((p) => `${p.chip}:${p.line}`);
        expect(new Set(keys).size).toBe(keys.length);
      });

  test("every non-Pi board cites its pinout source", () => {
    for (const b of BOARDS) if (b.headers.length) expect(b.sources.length).toBeGreaterThan(0);
  });

  test("untested boards are marked", () => {
    for (const b of BOARDS) if (!["rpi", "x86", "generic"].includes(b.id)) expect(b.tested).toBe(false);
  });
});

describe("resolveChips", () => {
  test("matches by uAPI label or sysfs device, case-insensitively", () => {
    const def = { ...BOARDS.find((b) => b.id === "generic")!, chips: { aobus: ["aobus-banks"], periphs: ["periphs-banks"] } }; // Le Potato's
    const chips = [
      { name: "gpiochip0", label: "periphs-banks", device: "c8834000.bus:pinctrl@4b0" },
      { name: "gpiochip1", label: "aobus-banks", device: "c8100000.bus:pinctrl@14" },
      { name: "gpiochip2", label: "ftdi-cbus", device: "1-1:1.0" },
    ];
    const m = resolveChips(def, chips);
    expect(m.get("aobus")?.name).toBe("gpiochip1");
    expect(m.get("periphs")?.name).toBe("gpiochip0");
    expect(resolveChips({ ...def, chips: { x: ["C8834000.BUS:PINCTRL@4B0"] } }, chips).get("x")?.name).toBe("gpiochip0");
  });
});
