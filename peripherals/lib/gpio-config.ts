// ==================== GPIO: header map, tool parsers, config.txt editing ====================
//
// Pure functions (unit-tested): the 40-pin header layout, parsers for `pinctrl get` and
// libgpiod's `gpioinfo` (v2 and v1 output), and section-aware reading/editing of
// /boot/firmware/config.txt with a unified diff for review.
//
// config.txt rules that shape the editor: later lines win; [filter] lines start conditional
// sections that last until the next filter, and [all] resets to unconditional. We only ever
// edit lines in the effective [all] context (the top of the file and after any [all]) and
// append new lines under an [all] block, so a [cm4] or [pi5] section is never touched; if a
// conditional section also sets the key, the caller is told it can override the change.
//
// Also: the extra BCM2711 controllers (I²C3–6, UART2–5, i2c-gpio), which GPIOs every
// interface takes and where two collide, and (not pure) onboardUarts() for tty → UART names.

import { readdirSync, readFileSync, realpathSync } from "node:fs";

// ---------- 40-pin header ----------

export type HeaderPin = { pin: number; kind: "3v3" | "5v" | "gnd" | "gpio"; gpio?: number };

const LAYOUT: (number | "3v3" | "5v" | "gnd")[] = [
  "3v3", "5v", 2, "5v", 3, "gnd", 4, 14, "gnd", 15, 17, 18, 27, "gnd", 22, 23, "3v3", 24, 10, "gnd",
  9, 25, 11, 8, "gnd", 7, 0, 1, 5, "gnd", 6, 12, 13, "gnd", 19, 16, 26, 20, "gnd", 21,
];

export const HEADER: HeaderPin[] = LAYOUT.map((x, i) => (typeof x === "number" ? { pin: i + 1, kind: "gpio", gpio: x } : { pin: i + 1, kind: x }));
export const GPIO_TO_PIN = new Map(HEADER.filter((h) => h.kind === "gpio").map((h) => [h.gpio!, h.pin]));

// ---------- pinctrl ----------

export interface PinState {
  gpio: number;
  fsel: string; // ip | op | a0..a8 | no
  func: string; // input | output | SDA1 | TXD1 | …
  level: 0 | 1 | null;
  pull: "up" | "down" | "none" | null;
  drive: "high" | "low" | null; // for outputs
  name: string; // board name, e.g. "ID_SDA/GPIO0"
}

/**
 * `pinctrl get` lines:
 *    2: a0    pu | hi // GPIO2 = SDA1
 *   42: op -- pu | lo // STATUS_LED_G_CLK/GPIO42 = output
 *   20: op dh pn | hi // GPIO20 = output
 *  100: (null)    -- | -- // BT_ON/FWGPIO0 = (null)
 */
export function parsePinctrl(text: string): PinState[] {
  const out: PinState[] = [];
  for (const l of text.split("\n")) {
    const m = /^\s*(\d+):\s+(\S+)\s+(?:(dh|dl|--)\s+)?(pu|pd|pn|--)\s*\|\s*(hi|lo|--)\s*\/\/\s*(.*?)\s*=\s*(.*?)\s*$/.exec(l);
    if (!m) continue;
    const [, g, fsel, drive, pull, lev, name, func] = m;
    out.push({
      gpio: +g,
      fsel,
      func,
      level: lev === "hi" ? 1 : lev === "lo" ? 0 : null,
      pull: pull === "pu" ? "up" : pull === "pd" ? "down" : pull === "pn" ? "none" : null,
      drive: drive === "dh" ? "high" : drive === "dl" ? "low" : null,
      name,
    });
  }
  return out;
}

// ---------- gpioinfo ----------

export interface LineInfo {
  chip: string;
  offset: number;
  name: string;
  direction: "input" | "output";
  consumer: string | null;
  flags: string[];
}

/**
 * libgpiod v2:  `\tline   4:\t"GPIO4"  \toutput drive=open-drain consumer="onewire@4"`
 * libgpiod v1:  `\tline   4:    "GPIO4" "onewire@4" output active-high [used open-drain]`
 */
export function parseGpioinfo(text: string): LineInfo[] {
  const out: LineInfo[] = [];
  let chip = "";
  for (const l of text.split("\n")) {
    const c = /^(gpiochip\d+)\b/.exec(l);
    if (c) {
      chip = c[1];
      continue;
    }
    const v2 = /^\s*line\s+(\d+):\s+"([^"]*)"\s+(input|output)(.*)$/.exec(l);
    if (v2) {
      const rest = v2[4];
      const cons = /consumer="([^"]*)"/.exec(rest);
      const flags = rest.replace(/consumer="[^"]*"/, "").trim().split(/\s+/).filter(Boolean);
      out.push({ chip, offset: +v2[1], name: v2[2], direction: v2[3] as any, consumer: cons ? cons[1] : null, flags });
      continue;
    }
    const v1 = /^\s*line\s+(\d+):\s+(?:"([^"]*)"|unnamed)\s+(?:"([^"]*)"|unused)\s+(input|output)\s+(\S+)(?:\s+\[([^\]]*)\])?/.exec(l);
    if (v1) out.push({ chip, offset: +v1[1], name: v1[2] || "", direction: v1[4] as any, consumer: v1[3] ?? null, flags: [v1[5], ...(v1[6] ? v1[6].split(/\s+/) : [])].filter((f) => f && f !== "used") });
  }
  return out;
}

// ---------- config.txt ----------

export interface CfgLine {
  i: number;
  text: string;
  section: string; // "all" or the active filter(s), e.g. "cm4"
  active: boolean; // not a comment / blank
  key?: string;
  value?: string;
}

export function parseConfig(text: string): CfgLine[] {
  const out: CfgLine[] = [];
  let section = "all";
  text.split("\n").forEach((raw, i) => {
    const t = raw.replace(/\r$/, "");
    const s = t.trim();
    const sec = /^\[([^\]]+)\]/.exec(s);
    if (sec) {
      const f = sec[1].trim().toLowerCase();
      section = f; // a new filter replaces the previous one; [all] resets
      out.push({ i, text: t, section, active: false });
      return;
    }
    const kv = /^([A-Za-z0-9_]+)\s*=\s*(.*?)\s*$/.exec(s);
    out.push({ i, text: t, section, active: !!kv, key: kv?.[1], value: kv?.[2] });
  });
  return out;
}

/** "i2c_arm=on,i2c_arm_baudrate=400000" → [["i2c_arm","on"],["i2c_arm_baudrate","400000"]] */
function params(v: string): [string, string | null][] {
  return v.split(",").map((p) => {
    const [k, ...rest] = p.split("=");
    return [k.trim(), rest.length ? rest.join("=").trim() : null];
  });
}
const truthy = (v: string | null) => v == null || /^(on|true|yes|1|okay)$/i.test(v);
const I2C_NAMES = ["i2c_arm", "i2c"];
const BAUD_NAMES = ["i2c_arm_baudrate", "i2c_baudrate"];

// ---------- extra controllers (BCM2711: Pi 4, Pi 400, CM4) ----------
//
// From /boot/firmware/overlays/README (checked on desky 2026-10-18): each of these "BCM2711
// only" overlays enables one more controller. The I²C ones take one of two pin pairs (the
// first listed here is the overlay's default) and a baudrate (default 100000); the UARTs
// put TX/RX on the first two pins and, with `ctsrts`, CTS/RTS on the next two. Pi 5 has
// differently named overlays (…-pi5) on other pins, so none of this is offered there.

export const EXTRA_I2C: Record<string, { choices: Record<string, [number, number]>; def: string }> = {
  i2c3: { choices: { pins_4_5: [4, 5], pins_2_3: [2, 3] }, def: "pins_4_5" },
  i2c4: { choices: { pins_8_9: [8, 9], pins_6_7: [6, 7] }, def: "pins_8_9" },
  i2c5: { choices: { pins_12_13: [12, 13], pins_10_11: [10, 11] }, def: "pins_12_13" },
  i2c6: { choices: { pins_22_23: [22, 23], pins_0_1: [0, 1] }, def: "pins_22_23" },
};
export const EXTRA_UART: Record<string, { tx: number; rx: number; cts: number; rts: number }> = {
  uart2: { tx: 0, rx: 1, cts: 2, rts: 3 },
  uart3: { tx: 4, rx: 5, cts: 6, rts: 7 },
  uart4: { tx: 8, rx: 9, cts: 10, rts: 11 },
  uart5: { tx: 12, rx: 13, cts: 14, rts: 15 },
};
/** i2c-gpio (bit-banged, any pins): the overlay's default pins. */
export const I2C_GPIO_DEFAULTS = { sda: 23, scl: 24 };
/** Bus numbers a Pi 4's own controllers (and the HDMI DDC buses) use; i2c-gpio must pick another. */
const RESERVED_BUSES = [1, 2, 3, 4, 5, 6, 10, 20, 21, 22];
const overlayName = (v: string) => params(v)[0][0];
const isManagedOverlay = (n: string) => /^w1-gpio/.test(n) || n in EXTRA_I2C || n in EXTRA_UART || n === "i2c-gpio";

export interface Interfaces {
  i2c: { enabled: boolean; baudrate: number | null };
  spi: { enabled: boolean };
  uart: { enabled: boolean };
  onewire: { enabled: boolean; pin: number | null; pullup: boolean };
  /** dtoverlay=i2c3…i2c6; pins is the pins_a_b param in effect. */
  extraI2c: Record<string, { enabled: boolean; pins: string; baudrate: number | null }>;
  /** dtoverlay=uart2…uart5. */
  extraUart: Record<string, { enabled: boolean; ctsrts: boolean }>;
  /** dtoverlay=i2c-gpio (only the last one in [all] is read or edited). */
  i2cGpio: { enabled: boolean; sda: number; scl: number; bus: number | null; delayUs: number | null };
  /** Keys also set in conditional sections (may override the [all] value on matching boards). */
  conditional: { section: string; line: string }[];
}

export function readInterfaces(text: string): Interfaces {
  const lines = parseConfig(text);
  const r: Interfaces = {
    i2c: { enabled: false, baudrate: null }, spi: { enabled: false }, uart: { enabled: false }, onewire: { enabled: false, pin: null, pullup: false },
    extraI2c: Object.fromEntries(Object.entries(EXTRA_I2C).map(([n, x]) => [n, { enabled: false, pins: x.def, baudrate: null }])),
    extraUart: Object.fromEntries(Object.keys(EXTRA_UART).map((n) => [n, { enabled: false, ctsrts: false }])),
    i2cGpio: { enabled: false, ...I2C_GPIO_DEFAULTS, bus: null, delayUs: null },
    conditional: [],
  };
  for (const l of lines) {
    if (!l.active) continue;
    const relevant = (l.key === "dtparam" && params(l.value!).some(([k]) => [...I2C_NAMES, ...BAUD_NAMES, "spi"].includes(k))) || l.key === "enable_uart" || (l.key === "dtoverlay" && isManagedOverlay(overlayName(l.value!)));
    if (!relevant) continue;
    if (l.section !== "all") {
      r.conditional.push({ section: l.section, line: l.text.trim() });
      continue;
    }
    if (l.key === "dtparam")
      for (const [k, v] of params(l.value!)) {
        if (I2C_NAMES.includes(k)) r.i2c.enabled = truthy(v);
        if (BAUD_NAMES.includes(k) && v) r.i2c.baudrate = parseInt(v, 10) || null;
        if (k === "spi") r.spi.enabled = truthy(v);
      }
    if (l.key === "enable_uart") r.uart.enabled = l.value === "1";
    if (l.key === "dtoverlay") {
      const [name, ...ps] = params(l.value!);
      const num = (k: string) => {
        const v = parseInt(ps.find(([x]) => x === k)?.[1] ?? "", 10);
        return isNaN(v) ? null : v;
      };
      if (name[0] in EXTRA_I2C) {
        const x = EXTRA_I2C[name[0]];
        const pins = ps.filter(([k, v]) => k in x.choices && truthy(v)).pop()?.[0] || x.def;
        r.extraI2c[name[0]] = { enabled: true, pins, baudrate: num("baudrate") };
        continue;
      }
      if (name[0] in EXTRA_UART) {
        r.extraUart[name[0]] = { enabled: true, ctsrts: ps.some(([k, v]) => (k === "ctsrts" || k === "rs485") && truthy(v)) };
        continue;
      }
      if (name[0] === "i2c-gpio") {
        r.i2cGpio = { enabled: true, sda: num("i2c_gpio_sda") ?? I2C_GPIO_DEFAULTS.sda, scl: num("i2c_gpio_scl") ?? I2C_GPIO_DEFAULTS.scl, bus: num("bus"), delayUs: num("i2c_gpio_delay_us") };
        continue;
      }
      r.onewire.enabled = true;
      r.onewire.pullup = name[0] === "w1-gpio-pullup" || ps.some(([k, v]) => k === "pullup" && truthy(v));
      const pin = ps.find(([k]) => k === "gpiopin");
      r.onewire.pin = pin?.[1] ? parseInt(pin[1], 10) : 4;
    }
  }
  return r;
}

export interface InterfaceChanges {
  i2c?: { enabled?: boolean; baudrate?: number };
  spi?: { enabled?: boolean };
  uart?: { enabled?: boolean };
  onewire?: { enabled?: boolean; pin?: number };
  extraI2c?: Record<string, { enabled?: boolean; pins?: string; baudrate?: number }>;
  extraUart?: Record<string, { enabled?: boolean; ctsrts?: boolean }>;
  i2cGpio?: { enabled?: boolean; sda?: number; scl?: number; bus?: number | null; delayUs?: number | null };
}

// ---------- pin usage ----------

export interface PinUse {
  gpio: number;
  owner: string; // "I²C1", "UART3", "1-Wire"…
  role: string; // "SDA", "TX", "CTS"…
}

export const i2cLabel = (n: string) => n.replace(/^i2c(\d)$/, "I²C$1");

/** Which header GPIOs each enabled interface takes. */
export function pinUsage(r: Interfaces): PinUse[] {
  const u: PinUse[] = [];
  const add = (owner: string, pins: [number, string][]) => pins.forEach(([gpio, role]) => u.push({ gpio, owner, role }));
  if (r.i2c.enabled) add("I²C1", [[2, "SDA"], [3, "SCL"]]);
  if (r.spi.enabled) add("SPI0", [[7, "CE1"], [8, "CE0"], [9, "MISO"], [10, "MOSI"], [11, "SCLK"]]);
  if (r.uart.enabled) add("UART serial0", [[14, "TX"], [15, "RX"]]);
  if (r.onewire.enabled) add("1-Wire", [[r.onewire.pin ?? 4, "data"]]);
  for (const [n, x] of Object.entries(r.extraI2c)) {
    if (!x.enabled) continue;
    const [sda, scl] = EXTRA_I2C[n].choices[x.pins] || EXTRA_I2C[n].choices[EXTRA_I2C[n].def];
    add(i2cLabel(n), [[sda, "SDA"], [scl, "SCL"]]);
  }
  for (const [n, x] of Object.entries(r.extraUart)) {
    if (!x.enabled) continue;
    const p = EXTRA_UART[n];
    add(n.toUpperCase(), [[p.tx, "TX"], [p.rx, "RX"], ...(x.ctsrts ? ([[p.cts, "CTS"], [p.rts, "RTS"]] as [number, string][]) : [])]);
  }
  if (r.i2cGpio.enabled) add("i2c-gpio", [[r.i2cGpio.sda, "SDA"], [r.i2cGpio.scl, "SCL"]]);
  return u;
}

/** GPIOs claimed by more than one interface, e.g. 1-Wire and UART3 TX both on GPIO4. */
export function pinConflicts(r: Interfaces): { gpio: number; users: string[] }[] {
  const by = new Map<number, string[]>();
  for (const p of pinUsage(r)) by.set(p.gpio, [...(by.get(p.gpio) || []), `${p.owner} ${p.role}`]);
  return [...by].filter(([, users]) => users.length > 1).map(([gpio, users]) => ({ gpio, users })).sort((a, b) => a.gpio - b.gpio);
}

const MARK = "# Added by DeskyPi (GPIO panel → Interfaces)";

/** Apply interface changes to config.txt text; only [all]-context lines are edited. */
export function editConfig(text: string, ch: InterfaceChanges): string {
  const eol = text.includes("\r\n") ? "\r\n" : "\n";
  const out = text.replace(/\r\n/g, "\n").split("\n");
  const append: string[] = [];
  const allLines = () => parseConfig(out.join("\n")).filter((l) => l.section === "all");

  /** Last active dtparam line in [all] carrying one of `names`; edits that param in place. */
  const setParam = (names: string[], value: string | null, enable: boolean) => {
    const hits = allLines().filter((l) => l.active && l.key === "dtparam" && params(l.value!).some(([k]) => names.includes(k)));
    const hit = hits[hits.length - 1];
    if (hit) {
      const ps = params(hit.value!).map(([k, v]) => (names.includes(k) ? [k, value] : [k, v]) as [string, string | null]);
      const indent = /^\s*/.exec(out[hit.i])![0];
      out[hit.i] = `${indent}dtparam=${ps.map(([k, v]) => (v == null ? k : `${k}=${v}`)).join(",")}`;
      return;
    }
    if (!enable) return; // absent = default off
    // Prefer un-commenting the stock "#dtparam=spi=on" line where it sits.
    const commented = allLines().filter((l) => !l.active && new RegExp(`^\\s*#\\s*dtparam=(${names.join("|")})=`).test(l.text));
    const c = commented[commented.length - 1];
    if (c) out[c.i] = `dtparam=${names[0]}=${value}`;
    else append.push(`dtparam=${names[0]}=${value}`);
  };

  if (ch.i2c?.enabled != null) setParam(I2C_NAMES, ch.i2c.enabled ? "on" : "off", ch.i2c.enabled);
  if (ch.i2c?.baudrate != null) {
    const has = allLines().some((l) => l.active && l.key === "dtparam" && params(l.value!).some(([k]) => BAUD_NAMES.includes(k)));
    if (has || ch.i2c.baudrate !== 100000) setParam(BAUD_NAMES, String(ch.i2c.baudrate), true);
  }
  if (ch.spi?.enabled != null) setParam(["spi"], ch.spi.enabled ? "on" : "off", ch.spi.enabled);
  if (ch.uart?.enabled != null) {
    const hits = allLines().filter((l) => l.active && l.key === "enable_uart");
    const v = ch.uart.enabled ? "1" : "0";
    if (hits.length) out[hits[hits.length - 1].i] = `enable_uart=${v}`;
    else if (ch.uart.enabled) append.push(`enable_uart=1`);
  }
  if (ch.onewire?.enabled != null || ch.onewire?.pin != null) {
    const hits = allLines().filter((l) => l.active && l.key === "dtoverlay" && /^w1-gpio/.test(l.value!));
    const enable = ch.onewire.enabled ?? hits.length > 0;
    if (!enable) for (const h of hits) out[h.i] = `#${out[h.i].replace(/^\s*/, "")}`;
    else {
      const pin = ch.onewire.pin;
      if (pin != null && !(pin >= 2 && pin <= 27)) throw new Error("1-Wire pin must be GPIO2–27");
      const h = hits[hits.length - 1];
      if (h) {
        if (pin != null) {
          const [name, ...ps] = params(h.value!);
          const rest = ps.filter(([k]) => k !== "gpiopin");
          out[h.i] = `dtoverlay=${[name[0], `gpiopin=${pin}`, ...rest.map(([k, v]) => (v == null ? k : `${k}=${v}`))].join(",")}`;
        }
      } else append.push(`dtoverlay=w1-gpio,gpiopin=${pin ?? 4}`);
    }
  }

  // Extra controllers: one dtoverlay line each. The wanted state is the current one with the
  // change merged in; a line is only rewritten when that state differs, and params we don't
  // manage (the UARTs' rs485 options…) are kept.
  const cur = readInterfaces(text);
  const setOverlay = (name: string, enable: boolean, managed: (k: string) => boolean, want: [string, string | null][]) => {
    const hits = allLines().filter((l) => l.active && l.key === "dtoverlay" && overlayName(l.value!) === name);
    if (!enable) {
      for (const h of hits) out[h.i] = `#${out[h.i].replace(/^\s*/, "")}`;
      return;
    }
    const h = hits[hits.length - 1];
    const keep = h ? params(h.value!).slice(1).filter(([k]) => !managed(k)) : [];
    const line = `dtoverlay=${[name, ...[...want, ...keep].map(([k, v]) => (v == null ? k : `${k}=${v}`))].join(",")}`;
    if (h) out[h.i] = line;
    else append.push(line);
  };
  for (const [n, c] of Object.entries(ch.extraI2c || {})) {
    const x = EXTRA_I2C[n];
    if (!x) throw new Error(`Unknown I²C controller ${n} (expected ${Object.keys(EXTRA_I2C).join(", ")})`);
    const was = cur.extraI2c[n];
    const enabled = c.enabled ?? was.enabled, pins = c.pins ?? was.pins, baud = c.baudrate ?? was.baudrate;
    if (!(pins in x.choices)) throw new Error(`${n} pins must be one of ${Object.keys(x.choices).join(", ")}`);
    if (baud != null && !(baud >= 10000 && baud <= 3400000)) throw new Error(`${n} baudrate must be 10000–3400000 Hz`);
    if (enabled === was.enabled && (!enabled || (pins === was.pins && (baud ?? 100000) === (was.baudrate ?? 100000)))) continue;
    setOverlay(n, enabled, (k) => k in x.choices || k === "baudrate", [[pins, null], ...(baud != null && baud !== 100000 ? ([["baudrate", String(baud)]] as [string, string][]) : [])]);
  }
  for (const [n, c] of Object.entries(ch.extraUart || {})) {
    if (!EXTRA_UART[n]) throw new Error(`Unknown UART ${n} (expected ${Object.keys(EXTRA_UART).join(", ")})`);
    const was = cur.extraUart[n];
    const enabled = c.enabled ?? was.enabled, ctsrts = c.ctsrts ?? was.ctsrts;
    if (enabled === was.enabled && (!enabled || ctsrts === was.ctsrts)) continue;
    setOverlay(n, enabled, (k) => k === "ctsrts", ctsrts ? [["ctsrts", null]] : []);
  }
  if (ch.i2cGpio) {
    const c = ch.i2cGpio, was = cur.i2cGpio;
    const want = { enabled: c.enabled ?? was.enabled, sda: c.sda ?? was.sda, scl: c.scl ?? was.scl, bus: c.bus !== undefined ? c.bus : was.bus, delayUs: c.delayUs !== undefined ? c.delayUs : was.delayUs };
    const pinOk = (g: number) => Number.isInteger(g) && g >= 0 && g <= 27;
    if (want.enabled) {
      if (!pinOk(want.sda) || !pinOk(want.scl) || want.sda === want.scl) throw new Error("i2c-gpio SDA and SCL must be two different GPIOs 0–27");
      if (want.bus != null && (!Number.isInteger(want.bus) || want.bus < 1 || want.bus > 255 || RESERVED_BUSES.includes(want.bus))) throw new Error(`i2c-gpio bus number must be 1–255 and not ${RESERVED_BUSES.join("/")} (the Pi's own buses)`);
      if (want.delayUs != null && !(Number.isInteger(want.delayUs) && want.delayUs >= 1 && want.delayUs <= 100)) throw new Error("i2c-gpio clock delay must be 1–100 µs");
    }
    const same = want.enabled === was.enabled && (!want.enabled || (want.sda === was.sda && want.scl === was.scl && want.bus === was.bus && want.delayUs === was.delayUs));
    if (!same)
      setOverlay("i2c-gpio", want.enabled, (k) => ["i2c_gpio_sda", "i2c_gpio_scl", "bus", "i2c_gpio_delay_us"].includes(k), [
        ["i2c_gpio_sda", String(want.sda)],
        ["i2c_gpio_scl", String(want.scl)],
        ...(want.bus != null ? ([["bus", String(want.bus)]] as [string, string][]) : []),
        ...(want.delayUs != null ? ([["i2c_gpio_delay_us", String(want.delayUs)]] as [string, string][]) : []),
      ]);
  }

  if (append.length) {
    while (out.length && out[out.length - 1].trim() === "") out.pop();
    const lastSection = [...parseConfig(out.join("\n"))].reverse().find((l) => /^\s*\[/.test(l.text));
    const inAll = !lastSection || /^\s*\[all\]/i.test(lastSection.text);
    out.push("", ...(inAll ? [] : ["[all]"]), MARK, ...append, "");
  }
  return out.join("\n").replace(/\n/g, eol);
}

// ---------- live device tree ----------
// Not pure (reads /proc and /sys, synchronously: a few tiny files); shared by the GPIO and
// serial drivers.

/** Onboard UART ttys mapped to their controller through the device-tree aliases: ttyS0 → uart1, ttyAMA2 → uart3. */
export function onboardUarts(): { uart: string; tty: string; node: string; pins: string }[] {
  const tryRead = <T>(f: () => T, d: T) => {
    try {
      return f();
    } catch {
      return d;
    }
  };
  const aliases = new Map<string, string>();
  for (let i = 0; i <= 5; i++) {
    const v = tryRead(() => readFileSync(`/proc/device-tree/aliases/uart${i}`, "utf8"), "");
    if (v) aliases.set(v.replace(/\0/g, ""), `uart${i}`);
  }
  const out = [];
  for (const tty of tryRead(() => readdirSync("/sys/class/tty"), [] as string[])) {
    if (!/^tty(AMA|S)\d+$/.test(tty)) continue;
    const node = tryRead(() => realpathSync(`/sys/class/tty/${tty}/device/of_node`), "").replace(/^\/sys\/firmware\/devicetree\/base/, "");
    const uart = node && aliases.get(node);
    if (!uart) continue;
    const p = EXTRA_UART[uart];
    out.push({ uart, tty, node, pins: p ? `GPIO${p.tx}/${p.rx}` : "" });
  }
  return out.sort((a, b) => a.uart.localeCompare(b.uart));
}

// ---------- unified diff ----------

/** Line-based unified diff (LCS), `ctx` lines of context. Returns "" when equal. */
export function unifiedDiff(a: string, b: string, name = "config.txt", ctx = 3): string {
  if (a === b) return "";
  // A final newline is a line terminator, not an empty last line.
  const lines = (s: string) => {
    const l = s.replace(/\r\n/g, "\n").split("\n");
    if (l.length > 1 && l[l.length - 1] === "") l.pop();
    return l;
  };
  const A = lines(a);
  const B = lines(b);
  const n = A.length, m = B.length;
  const L = Array.from({ length: n + 1 }, () => new Int32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) L[i][j] = A[i] === B[j] ? L[i + 1][j + 1] + 1 : Math.max(L[i + 1][j], L[i][j + 1]);
  type Op = { t: " " | "-" | "+"; s: string; ai: number; bi: number };
  const ops: Op[] = [];
  let i = 0, j = 0;
  while (i < n || j < m) {
    if (i < n && j < m && A[i] === B[j]) ops.push({ t: " ", s: A[i], ai: i++, bi: j++ });
    else if (i < n && (j >= m || L[i + 1][j] >= L[i][j + 1])) ops.push({ t: "-", s: A[i], ai: i++, bi: j }); // deletions first
    else ops.push({ t: "+", s: B[j], ai: i, bi: j++ });
  }
  const out = [`--- ${name}`, `+++ ${name} (proposed)`];
  let k = 0;
  while (k < ops.length) {
    if (ops[k].t === " ") {
      k++;
      continue;
    }
    const start = Math.max(0, k - ctx);
    let end = k;
    // extend the hunk while changes are within 2*ctx of each other
    while (end < ops.length) {
      if (ops[end].t !== " ") {
        end++;
        continue;
      }
      let run = 0;
      while (end + run < ops.length && ops[end + run].t === " ") run++;
      if (end + run >= ops.length || run > 2 * ctx) {
        end = Math.min(ops.length, end + ctx);
        break;
      }
      end += run;
    }
    const h = ops.slice(start, end);
    const aStart = h[0].ai + 1, bStart = h[0].bi + 1;
    const aLen = h.filter((o) => o.t !== "+").length, bLen = h.filter((o) => o.t !== "-").length;
    out.push(`@@ -${aLen ? aStart : aStart - 1},${aLen} +${bLen ? bStart : bStart - 1},${bLen} @@`);
    for (const o of h) out.push(o.t + o.s);
    k = end;
  }
  return out.join("\n") + "\n";
}
