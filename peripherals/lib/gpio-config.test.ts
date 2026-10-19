import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { editConfig, GPIO_TO_PIN, HEADER, parseConfig, parseGpioinfo, parsePinctrl, pinConflicts, pinUsage, readInterfaces, unifiedDiff } from "./gpio-config.ts";

const fx = (n: string) => readFileSync(`${import.meta.dir}/gpio-fixtures/${n}`, "utf8");
const DESKY = fx("config-desky.txt"); // /boot/firmware/config.txt on desky, 2026-10-17

describe("header + parsers", () => {
  test("40-pin layout", () => {
    expect(HEADER.length).toBe(40);
    expect(HEADER.filter((h) => h.kind === "gnd").length).toBe(8);
    expect(HEADER.filter((h) => h.kind === "gpio").length).toBe(28);
    expect(GPIO_TO_PIN.get(4)).toBe(7);
    expect(GPIO_TO_PIN.get(21)).toBe(40);
  });

  test("pinctrl get (desky capture)", () => {
    const p = parsePinctrl(fx("pinctrl-desky.txt"));
    expect(p.length).toBe(58);
    expect(p[2]).toMatchObject({ gpio: 2, fsel: "a0", func: "SDA1", pull: "up", level: 1 });
    expect(p[4]).toMatchObject({ gpio: 4, fsel: "ip", func: "input", pull: "none" });
    expect(p[14]).toMatchObject({ fsel: "a5", func: "TXD1" });
    expect(p[42]).toMatchObject({ fsel: "op", func: "output", drive: null, level: 0 });
    expect(parsePinctrl("20: op dh pn | hi // GPIO20 = output")[0]).toMatchObject({ drive: "high", pull: "none", level: 1 });
    expect(parsePinctrl("100: (null)    -- | -- // BT_ON/FWGPIO0 = (null)")[0]).toMatchObject({ gpio: 100, level: null, pull: null });
  });

  test("gpioinfo v2 (desky capture) and v1", () => {
    const l = parseGpioinfo(fx("gpioinfo-v2-desky.txt"));
    const c0 = l.filter((x) => x.chip === "gpiochip0");
    expect(c0.length).toBe(58);
    expect(c0[4]).toMatchObject({ name: "GPIO4", direction: "output", consumer: "onewire@4", flags: ["drive=open-drain"] });
    expect(c0[2].consumer).toBe("kernel");
    expect(c0[17].consumer).toBeNull();
    expect(c0[42].consumer).toBe("ACT");
    const v1 = parseGpioinfo('gpiochip0 - 54 lines:\n\tline   4:     "GPIO4"  "onewire@4"  output  active-high [used open-drain]\n\tline   5:     "GPIO5"       unused   input  active-high ');
    expect(v1[0]).toMatchObject({ offset: 4, consumer: "onewire@4", direction: "output" });
    expect(v1[1]).toMatchObject({ offset: 5, consumer: null });
  });
});

describe("config.txt", () => {
  test("sections: [cm4]/[cm5] lines are conditional, later [all] resets", () => {
    const l = parseConfig(DESKY);
    expect(l.find((x) => x.key === "otg_mode")!.section).toBe("cm4");
    expect(l.find((x) => x.text.startsWith("dtoverlay=dwc2"))!.section).toBe("cm5");
    expect(l.find((x) => x.key === "enable_uart")!.section).toBe("all");
  });

  test("reads desky's interfaces", () => {
    const r = readInterfaces(DESKY);
    expect(r).toMatchObject({ i2c: { enabled: true, baudrate: null }, spi: { enabled: false }, uart: { enabled: true }, onewire: { enabled: true, pin: 4 } });
    expect(r.conditional).toEqual([]);
  });

  test("enable SPI un-comments the stock line in place", () => {
    const n = editConfig(DESKY, { spi: { enabled: true } });
    expect(n).toContain("\ndtparam=spi=on\n");
    expect(n).not.toContain("#dtparam=spi=on");
    expect(n.split("\n").length).toBe(DESKY.split("\n").length);
    expect(readInterfaces(n).spi.enabled).toBe(true);
    const d = unifiedDiff(DESKY, n);
    expect(d).toContain("-#dtparam=spi=on\n+dtparam=spi=on");
    expect(d.match(/^@@/gm)!.length).toBe(1);
  });

  test("I²C off + 400 kHz edits the existing line / appends in the trailing [all] block", () => {
    const n = editConfig(DESKY, { i2c: { enabled: false, baudrate: 400000 } });
    const r = readInterfaces(n);
    expect(r.i2c).toEqual({ enabled: false, baudrate: 400000 });
    expect(n).toContain("dtparam=i2c_arm=off");
    expect(n.trimEnd().endsWith("# Added by DeskyPi (GPIO panel → Interfaces)\ndtparam=i2c_arm_baudrate=400000")).toBe(true);
    // the file already ends in an [all] context, so no extra [all] header
    expect(n.match(/^\[all\]/gm)!.length).toBe(2);
  });

  test("1-Wire: move pin, then disable (comments it out)", () => {
    const moved = editConfig(DESKY, { onewire: { pin: 17 } });
    expect(moved).toContain("dtoverlay=w1-gpio,gpiopin=17");
    expect(readInterfaces(moved).onewire.pin).toBe(17);
    const off = editConfig(DESKY, { onewire: { enabled: false } });
    expect(off).toContain("#dtoverlay=w1-gpio,gpiopin=4");
    expect(readInterfaces(off).onewire.enabled).toBe(false);
    expect(() => editConfig(DESKY, { onewire: { enabled: true, pin: 40 } })).toThrow();
  });

  test("appends under a new [all] when the file ends inside a conditional section", () => {
    const txt = "dtparam=audio=on\n[pi4]\narm_boost=1\n";
    const n = editConfig(txt, { uart: { enabled: true }, onewire: { enabled: true } });
    expect(n).toBe("dtparam=audio=on\n[pi4]\narm_boost=1\n\n[all]\n# Added by DeskyPi (GPIO panel → Interfaces)\nenable_uart=1\ndtoverlay=w1-gpio,gpiopin=4\n");
    expect(readInterfaces(n)).toMatchObject({ uart: { enabled: true }, onewire: { enabled: true, pin: 4 } });
  });

  test("never edits a key inside a conditional section; reports it", () => {
    const txt = "dtparam=spi=on\n[cm4]\ndtparam=spi=off\n[all]\n";
    const r = readInterfaces(txt);
    expect(r.spi.enabled).toBe(true);
    expect(r.conditional).toEqual([{ section: "cm4", line: "dtparam=spi=off" }]);
    const n = editConfig(txt, { spi: { enabled: false } });
    expect(n).toBe("dtparam=spi=off\n[cm4]\ndtparam=spi=off\n[all]\n");
  });

  test("multi-param dtparam lines are edited param-wise; CRLF preserved", () => {
    const txt = "dtparam=i2c_arm=on,i2c_arm_baudrate=100000,spi=on\r\n";
    const n = editConfig(txt, { i2c: { baudrate: 1000000 } });
    expect(n).toBe("dtparam=i2c_arm=on,i2c_arm_baudrate=1000000,spi=on\r\n");
  });

  test("no-op changes produce no diff", () => {
    expect(unifiedDiff(DESKY, editConfig(DESKY, { i2c: { enabled: true }, uart: { enabled: true } }))).toBe("");
    expect(unifiedDiff(DESKY, editConfig(DESKY, { extraI2c: { i2c3: { enabled: false } }, extraUart: { uart2: { enabled: false } }, i2cGpio: { enabled: false } }))).toBe("");
  });
});

describe("extra I²C buses and UARTs (BCM2711 overlays)", () => {
  test("desky has none; defaults come from the overlay README", () => {
    const r = readInterfaces(DESKY);
    expect(r.extraI2c.i2c3).toEqual({ enabled: false, pins: "pins_4_5", baudrate: null });
    expect(r.extraI2c.i2c6.pins).toBe("pins_22_23");
    expect(r.extraUart.uart5).toEqual({ enabled: false, ctsrts: false });
    expect(r.i2cGpio).toEqual({ enabled: false, sda: 23, scl: 24, bus: null, delayUs: null });
  });

  test("reads overlay lines, params and conditional copies", () => {
    const r = readInterfaces("dtoverlay=i2c4,pins_6_7,baudrate=400000\ndtoverlay=uart3,ctsrts\ndtoverlay=uart4,rs485\ndtoverlay=i2c-gpio,i2c_gpio_sda=17,i2c_gpio_scl=27,bus=11\n[cm4]\ndtoverlay=i2c5\n");
    expect(r.extraI2c.i2c4).toEqual({ enabled: true, pins: "pins_6_7", baudrate: 400000 });
    expect(r.extraUart.uart3.ctsrts).toBe(true);
    expect(r.extraUart.uart4.ctsrts).toBe(true); // rs485 implies ctsrts
    expect(r.extraI2c.i2c5.enabled).toBe(false);
    expect(r.i2cGpio).toEqual({ enabled: true, sda: 17, scl: 27, bus: 11, delayUs: null });
    expect(r.conditional).toEqual([{ section: "cm4", line: "dtoverlay=i2c5" }]);
  });

  test("enable appends under the DeskyPi block; disable comments out", () => {
    const n = editConfig(DESKY, { extraI2c: { i2c6: { enabled: true, baudrate: 400000 } }, extraUart: { uart5: { enabled: true } } });
    expect(n.trimEnd().endsWith("dtoverlay=i2c6,pins_22_23,baudrate=400000\ndtoverlay=uart5")).toBe(true);
    const r = readInterfaces(n);
    expect(r.extraI2c.i2c6).toEqual({ enabled: true, pins: "pins_22_23", baudrate: 400000 });
    expect(r.extraUart.uart5.enabled).toBe(true);
    const off = editConfig(n, { extraUart: { uart5: { enabled: false } } });
    expect(off).toContain("\n#dtoverlay=uart5");
    expect(readInterfaces(off).extraUart.uart5.enabled).toBe(false);
  });

  test("edits keep unmanaged params; unchanged state is not rewritten", () => {
    const txt = "dtoverlay=uart2,rs485_invert_rts\ndtoverlay=i2c3\n";
    expect(editConfig(txt, { extraUart: { uart2: { ctsrts: true } } })).toBe("dtoverlay=uart2,ctsrts,rs485_invert_rts\ndtoverlay=i2c3\n");
    expect(editConfig(txt, { extraI2c: { i2c3: { enabled: true, pins: "pins_4_5", baudrate: 100000 } } })).toBe(txt);
    expect(editConfig(txt, { extraI2c: { i2c3: { pins: "pins_2_3" } } })).toBe("dtoverlay=uart2,rs485_invert_rts\ndtoverlay=i2c3,pins_2_3\n");
  });

  test("i2c-gpio: pins, bus number, validation", () => {
    const n = editConfig("", { i2cGpio: { enabled: true, sda: 17, scl: 27, bus: 11 } });
    expect(n).toContain("dtoverlay=i2c-gpio,i2c_gpio_sda=17,i2c_gpio_scl=27,bus=11");
    expect(() => editConfig("", { i2cGpio: { enabled: true, bus: 1 } })).toThrow(/bus number/);
    expect(() => editConfig("", { i2cGpio: { enabled: true, sda: 5, scl: 5 } })).toThrow(/different/);
    expect(() => editConfig("", { extraI2c: { i2c3: { enabled: true, pins: "pins_8_9" } } })).toThrow(/pins_4_5/);
    expect(() => editConfig("", { extraUart: { uart9: { enabled: true } } })).toThrow(/Unknown UART/);
  });

  test("pin usage and conflicts", () => {
    const r = readInterfaces(DESKY);
    expect(pinUsage(r).map((u) => u.gpio)).toEqual([2, 3, 14, 15, 4]);
    expect(pinConflicts(r)).toEqual([]);
    // desky's 1-Wire is on GPIO4, which UART3 TX and I²C3's default pins also use
    const c = pinConflicts(readInterfaces(editConfig(DESKY, { extraUart: { uart3: { enabled: true } }, extraI2c: { i2c3: { enabled: true } } })));
    expect(c).toEqual([
      { gpio: 4, users: ["1-Wire data", "I²C3 SDA", "UART3 TX"] },
      { gpio: 5, users: ["I²C3 SCL", "UART3 RX"] },
    ]);
    // UART5 CTS/RTS land on the primary UART's pins
    expect(pinConflicts(readInterfaces(editConfig(DESKY, { extraUart: { uart5: { enabled: true, ctsrts: true } } }))).map((x) => x.gpio)).toEqual([14, 15]);
  });
});
