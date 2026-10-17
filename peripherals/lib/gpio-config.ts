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

export interface Interfaces {
  i2c: { enabled: boolean; baudrate: number | null };
  spi: { enabled: boolean };
  uart: { enabled: boolean };
  onewire: { enabled: boolean; pin: number | null; pullup: boolean };
  /** Keys also set in conditional sections (may override the [all] value on matching boards). */
  conditional: { section: string; line: string }[];
}

export function readInterfaces(text: string): Interfaces {
  const lines = parseConfig(text);
  const r: Interfaces = { i2c: { enabled: false, baudrate: null }, spi: { enabled: false }, uart: { enabled: false }, onewire: { enabled: false, pin: null, pullup: false }, conditional: [] };
  for (const l of lines) {
    if (!l.active) continue;
    const relevant = (l.key === "dtparam" && params(l.value!).some(([k]) => [...I2C_NAMES, ...BAUD_NAMES, "spi"].includes(k))) || l.key === "enable_uart" || (l.key === "dtoverlay" && /^w1-gpio/.test(l.value!));
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

  if (append.length) {
    while (out.length && out[out.length - 1].trim() === "") out.pop();
    const lastSection = [...parseConfig(out.join("\n"))].reverse().find((l) => /^\s*\[/.test(l.text));
    const inAll = !lastSection || /^\s*\[all\]/i.test(lastSection.text);
    out.push("", ...(inAll ? [] : ["[all]"]), MARK, ...append, "");
  }
  return out.join("\n").replace(/\n/g, eol);
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
