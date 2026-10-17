// ==================== Serial ports ====================
//
// USB-serial adapters (FTDI, CP210x, CH34x, PL2303, CDC-ACM boards) and the onboard UART.
// DeskyPi is the single owner of each open port: it holds the fd and fans the received bytes
// out to every client at once (dashboard terminal, WebSocket, raw TCP, RFC 2217, REST/MCP
// agents), keeping a timestamped ring of recent traffic so a late joiner gets history.
// Any client may write; an optional lock reserves writing for one holder.
//
// I/O has no native addon (see lib/termios.ts): O_NONBLOCK fd, termios2 over bun:ffi,
// Bun.file(fd).stream() for reads, writeSync + a retry queue for writes.
//
// Ports open on demand, never just because they were plugged in (other drivers, e.g. CAN's
// slcan, may want the same tty). A port is held open while any of these is true: the user
// opened it (`want`), it is set to open on plug, or a bridge/WebSocket client holds a lease.
//
// Pure helpers (device naming, ring buffer, auto-baud scoring, flash command building and
// progress parsing) are exported for serial.test.ts.

import type { ServerWebSocket, Socket as NetSocket, TCPSocketListener, Subprocess } from "bun";
import { existsSync, mkdirSync, rmSync, realpathSync } from "node:fs";
import { hostname } from "node:os";
import { HttpError, body, readText, usbName, type Ctx, type Driver, type Props, type UdevEvent, type WsData } from "../core.ts";
import * as T from "../lib/termios.ts";
import { Rfc2217Session } from "../lib/rfc2217.ts";

// ---------- Device identification ----------

export type Family = "esp-usbjtag" | "esp" | "bridge" | "arduino" | "arduino-native" | "pico" | "stm32" | "jlink" | "cdc" | "onboard" | "other";

interface Known {
  vid: string;
  pid?: string;
  name: string;
  family: Family;
}

export const KNOWN: Known[] = [
  { vid: "303a", pid: "1001", name: "Espressif USB-JTAG/serial", family: "esp-usbjtag" },
  { vid: "303a", name: "Espressif native USB", family: "esp" },
  { vid: "10c4", pid: "ea60", name: "CP2102/CP2104", family: "bridge" },
  { vid: "10c4", pid: "ea70", name: "CP2105 dual UART", family: "bridge" },
  { vid: "10c4", pid: "ea71", name: "CP2108 quad UART", family: "bridge" },
  { vid: "1a86", pid: "7523", name: "CH340", family: "bridge" },
  { vid: "1a86", pid: "5523", name: "CH341", family: "bridge" },
  { vid: "1a86", pid: "55d4", name: "CH9102", family: "bridge" },
  { vid: "1a86", pid: "55d3", name: "CH343", family: "bridge" },
  { vid: "1a86", pid: "55d2", name: "CH342 dual UART", family: "bridge" },
  { vid: "0403", pid: "6001", name: "FTDI FT232R", family: "bridge" },
  { vid: "0403", pid: "6010", name: "FTDI FT2232", family: "bridge" },
  { vid: "0403", pid: "6011", name: "FTDI FT4232", family: "bridge" },
  { vid: "0403", pid: "6014", name: "FTDI FT232H", family: "bridge" },
  { vid: "0403", pid: "6015", name: "FTDI FT-X", family: "bridge" },
  { vid: "067b", pid: "2303", name: "Prolific PL2303", family: "bridge" },
  { vid: "067b", pid: "23a3", name: "Prolific PL2303GC", family: "bridge" },
  { vid: "2341", pid: "0043", name: "Arduino Uno R3", family: "arduino" },
  { vid: "2341", pid: "0001", name: "Arduino Uno", family: "arduino" },
  { vid: "2341", pid: "0010", name: "Arduino Mega 2560", family: "arduino" },
  { vid: "2341", pid: "0042", name: "Arduino Mega 2560 R3", family: "arduino" },
  { vid: "2341", pid: "0058", name: "Arduino Nano Every", family: "arduino" },
  { vid: "2341", pid: "8036", name: "Arduino Leonardo", family: "arduino-native" },
  { vid: "2341", pid: "8037", name: "Arduino Micro", family: "arduino-native" },
  { vid: "2341", pid: "0069", name: "Arduino Uno R4 Minima", family: "arduino-native" },
  { vid: "2341", pid: "1002", name: "Arduino Uno R4 WiFi", family: "esp" },
  { vid: "2341", name: "Arduino", family: "arduino" },
  { vid: "2a03", name: "Arduino (arduino.org)", family: "arduino" },
  { vid: "2e8a", pid: "000c", name: "Raspberry Pi Debug Probe UART", family: "cdc" },
  { vid: "2e8a", pid: "000a", name: "Raspberry Pi Pico (SDK serial)", family: "pico" },
  { vid: "2e8a", pid: "0005", name: "Raspberry Pi Pico (MicroPython)", family: "pico" },
  { vid: "2e8a", name: "Raspberry Pi RP2040/RP2350", family: "pico" },
  { vid: "0483", pid: "5740", name: "STM32 Virtual COM Port", family: "stm32" },
  { vid: "0483", pid: "374b", name: "ST-LINK/V2-1 VCP", family: "stm32" },
  { vid: "0483", pid: "3752", name: "ST-LINK/V2-1 VCP", family: "stm32" },
  { vid: "0483", pid: "374e", name: "ST-LINK/V3 VCP", family: "stm32" },
  { vid: "0483", pid: "3753", name: "ST-LINK/V3 VCP", family: "stm32" },
  { vid: "1366", name: "SEGGER J-Link VCOM", family: "jlink" },
  { vid: "1d50", pid: "6018", name: "Black Magic Probe", family: "cdc" },
  { vid: "16c0", pid: "0483", name: "Teensy USB serial", family: "cdc" },
  { vid: "0d28", pid: "0204", name: "DAPLink / micro:bit", family: "cdc" },
  { vid: "239a", name: "Adafruit board", family: "arduino" },
];

const USB_DRIVERS: Record<string, string> = { ftdi_sio: "FTDI", cp210x: "CP210x", ch341: "CH34x", pl2303: "PL2303", cdc_acm: "CDC-ACM", ch343: "CH343" };

export function identify(vid: string, pid: string): Known | null {
  vid = (vid || "").toLowerCase();
  pid = (pid || "").toLowerCase();
  return KNOWN.find((k) => k.vid === vid && k.pid === pid) || KNOWN.find((k) => k.vid === vid && !k.pid) || null;
}

export type ResetMode = "esp" | "esp-boot" | "dtr" | "touch1200";

/** Reset helpers worth offering for a family, most useful first. */
export function resetsFor(f: Family): ResetMode[] {
  switch (f) {
    case "esp-usbjtag":
    case "esp":
    case "bridge":
    case "other":
      return ["esp", "esp-boot", "dtr"];
    case "arduino":
      return ["dtr", "touch1200"];
    case "arduino-native":
    case "pico":
      return ["touch1200"];
    case "stm32":
    case "jlink":
    case "cdc":
      return ["dtr"];
    default:
      return [];
  }
}

export function flashToolsFor(f: Family): ("esptool" | "avrdude")[] {
  if (f === "arduino") return ["avrdude"];
  if (["esp-usbjtag", "esp"].includes(f)) return ["esptool"];
  if (["bridge", "other", "onboard"].includes(f)) return ["esptool", "avrdude"];
  return [];
}

type Step = { dtr?: boolean; rts?: boolean; ms?: number };

// Line sequences, in esptool's own order (keys are applied in insertion order, so a step
// like { rts: true, dtr: false } passes through RTS=DTR=1 the way esptool does).
// "Asserted" (true) pulls the adapter pin low. On the classic ESP dev board circuit DTR
// drives IO0 and RTS drives EN; with both asserted neither is pulled.
export const RESET_SEQ: Record<string, Step[]> = {
  // esptool ClassicReset
  "esp-boot": [{ dtr: false, rts: true }, { ms: 100 }, { dtr: true, rts: false }, { ms: 50 }, { dtr: false }],
  // esptool HardReset
  esp: [{ dtr: false }, { rts: true }, { ms: 100 }, { rts: false }],
  // esptool USBJTAGSerialReset: the built-in USB-Serial/JTAG peripheral decodes line states, not pins.
  "esp-boot-usbjtag": [{ rts: false, dtr: false }, { ms: 100 }, { dtr: true, rts: false }, { ms: 100 }, { rts: true, dtr: false }, { rts: true }, { ms: 100 }, { dtr: false, rts: false }],
  "esp-usbjtag": [{ dtr: false }, { rts: true }, { ms: 200 }, { rts: false }, { ms: 200 }],
  // avrdude's arduino programmer: drop DTR/RTS, then raise them; the board's capacitor turns
  // the edge into a reset pulse.
  dtr: [{ dtr: false, rts: false }, { ms: 250 }, { dtr: true, rts: true }, { ms: 50 }],
};

export function resetSequence(mode: ResetMode, family: Family): Step[] {
  if (family === "esp-usbjtag" && mode === "esp-boot") return RESET_SEQ["esp-boot-usbjtag"];
  if (family === "esp-usbjtag" && mode === "esp") return RESET_SEQ["esp-usbjtag"];
  return RESET_SEQ[mode] || [];
}

/** A URL-safe id from udev's ID_PATH: "platform-fd500000.pcie-pci-0000:01:00.0-usb-0:1.3:1.0" -> "usb-0-1.3-1.0". */
export function stableId(p: Props): string {
  const path = p.ID_PATH || "";
  const m = /-usb-(\S+)$/.exec(path);
  if (m) return "usb-" + m[1].replace(/:/g, "-");
  if (path) return path.replace(/^platform-/, "").replace(/[^A-Za-z0-9.]+/g, "-");
  if (p.ID_SERIAL) return "sn-" + p.ID_SERIAL.replace(/[^A-Za-z0-9.]+/g, "-") + (p.ID_USB_INTERFACE_NUM ? "-" + p.ID_USB_INTERFACE_NUM : "");
  return (p.DEVNAME || "tty").replace(/^\/dev\//, "").replace(/[^A-Za-z0-9.]+/g, "-");
}

// ---------- Ring buffer ----------

/** Recent rx with timestamps. Positions are absolute byte offsets since the port was first
 * opened, so a cursor stays meaningful as old data falls off the front. */
export class Ring {
  chunks: { t: number; pos: number; data: Uint8Array }[] = [];
  size = 0;
  end = 0;
  constructor(public cap = 256 * 1024) {}

  get start() {
    return this.end - this.size;
  }

  push(data: Uint8Array, t = Date.now()) {
    if (!data.length) return;
    const last = this.chunks[this.chunks.length - 1];
    // Coalesce a trickle (a byte per keystroke) so the chunk count stays bounded.
    if (last && last.data.length + data.length <= 4096 && t - last.t < 50) {
      const merged = new Uint8Array(last.data.length + data.length);
      merged.set(last.data);
      merged.set(data, last.data.length);
      last.data = merged;
    } else this.chunks.push({ t, pos: this.end, data: data.slice() });
    this.size += data.length;
    this.end += data.length;
    while (this.size > this.cap && this.chunks.length > 1) {
      const c = this.chunks.shift()!;
      this.size -= c.data.length;
    }
  }

  /** Bytes from `cursor` (clamped to what's still held) up to `max` bytes. */
  since(cursor: number, max = Infinity): { from: number; to: number; data: Uint8Array; truncated: boolean } {
    const from = Math.max(cursor, this.start);
    const to = Math.min(this.end, from + max);
    const out = new Uint8Array(Math.max(0, to - from));
    for (const c of this.chunks) {
      const cEnd = c.pos + c.data.length;
      if (cEnd <= from || c.pos >= to) continue;
      const a = Math.max(from, c.pos), b = Math.min(to, cEnd);
      out.set(c.data.subarray(a - c.pos, b - c.pos), a - from);
    }
    return { from, to, data: out, truncated: cursor < this.start };
  }

  /** Timestamped chunks covering the last `bytes` bytes, for history replay. */
  tail(bytes: number): { t: number; data: Uint8Array }[] {
    const from = Math.max(this.start, this.end - bytes);
    const out: { t: number; data: Uint8Array }[] = [];
    for (const c of this.chunks) {
      const cEnd = c.pos + c.data.length;
      if (cEnd <= from) continue;
      out.push({ t: c.t, data: c.pos >= from ? c.data : c.data.subarray(from - c.pos) });
    }
    return out;
  }

  clear() {
    this.chunks = [];
    this.size = 0;
  }
}

// ---------- Auto-baud ----------

export const AUTOBAUD_RATES = [1200, 2400, 4800, 9600, 14400, 19200, 38400, 57600, 74880, 115200, 230400, 250000, 460800, 921600, 1000000, 1500000, 2000000];

/** How much a sample looks like text from a correctly-timed UART, 0..1. At the wrong rate
 * the receiver sees framing errors (read as NUL), high-bit bytes and control junk. */
export function scoreSample(b: Uint8Array): number {
  if (b.length < 4) return 0;
  let printable = 0, wordy = 0, newlines = 0, nul = 0;
  for (const x of b) {
    if ((x >= 0x20 && x < 0x7f) || x === 9 || x === 10 || x === 13) printable++;
    if ((x >= 0x30 && x <= 0x39) || (x >= 0x41 && x <= 0x5a) || (x >= 0x61 && x <= 0x7a) || x === 0x20) wordy++;
    if (x === 10) newlines++;
    if (x === 0) nul++;
  }
  const n = b.length;
  const p = printable / n;
  const w = wordy / n;
  // Line structure: real logs break into lines of sensible length.
  const avgLine = newlines ? n / newlines : Infinity;
  const lines = n < 40 ? 0.75 : newlines && avgLine >= 2 && avgLine <= 200 ? 1 : 0.5;
  const s = p * p * (0.55 + 0.45 * Math.min(1, w / 0.6)) * (0.85 + 0.15 * lines) * (1 - Math.min(0.5, nul / n));
  return Math.max(0, Math.min(1, s));
}

export interface AutobaudResult {
  best: { baud: number; score: number; confidence: number; bytes: number } | null;
  results: { baud: number; bytes: number; score: number; sample: string }[];
}

/** Pick the best rate. Confidence needs a decent score, enough bytes, and a clear winner. */
export function pickBaud(samples: { baud: number; data: Uint8Array }[]): AutobaudResult {
  const results = samples.map((s) => ({ baud: s.baud, bytes: s.data.length, score: +scoreSample(s.data).toFixed(3), sample: printableSample(s.data, 48) }));
  const ranked = [...results].sort((a, b) => b.score - a.score || b.bytes - a.bytes);
  const top = ranked[0];
  if (!top || top.score === 0) return { best: null, results };
  const second = ranked[1]?.score || 0;
  const size = Math.min(1, top.bytes / 48);
  const margin = Math.min(1, ((top.score - second) / top.score) * 2);
  return { best: { baud: top.baud, score: top.score, bytes: top.bytes, confidence: +(top.score * size * margin).toFixed(2) }, results };
}

function printableSample(b: Uint8Array, n: number) {
  let s = "";
  for (const x of b.subarray(0, n)) s += x >= 0x20 && x < 0x7f ? String.fromCharCode(x) : x === 10 ? "⏎" : "·";
  return s;
}

// ---------- Flashing: command building + progress parsing ----------

export const AVR_PRESETS: Record<string, { label: string; part: string; programmer: string; baud: number }> = {
  uno: { label: "Arduino Uno (optiboot, 115200)", part: "atmega328p", programmer: "arduino", baud: 115200 },
  nano: { label: "Arduino Nano (new bootloader, 115200)", part: "atmega328p", programmer: "arduino", baud: 115200 },
  "nano-old": { label: "Arduino Nano (old bootloader, 57600)", part: "atmega328p", programmer: "arduino", baud: 57600 },
  mega: { label: "Arduino Mega 2560 (wiring, 115200)", part: "atmega2560", programmer: "wiring", baud: 115200 },
};

export const ESP_CHIPS = ["auto", "esp32", "esp32s2", "esp32s3", "esp32c2", "esp32c3", "esp32c6", "esp32h2", "esp32p4", "esp8266"];

export interface FlashFile {
  offset: number;
  path: string;
  name: string;
}

export interface FlashOpts {
  tool: "esptool" | "avrdude";
  action: "write" | "erase" | "info";
  files?: FlashFile[];
  baud?: number;
  chip?: string;
  preset?: string;
}

export function hex(n: number) {
  return "0x" + n.toString(16).padStart(5, "0");
}

export function flashCommand(port: string, o: FlashOpts): string[] {
  if (o.tool === "esptool") {
    const chip = o.chip || "auto";
    if (!ESP_CHIPS.includes(chip)) throw new HttpError(400, `Unknown chip "${chip}" (one of ${ESP_CHIPS.join(", ")})`);
    const baud = o.baud || (o.action === "write" ? 460800 : 115200);
    const base = ["/usr/bin/esptool", "--chip", chip, "--port", port, "--baud", String(baud)];
    if (o.action === "info") return [...base, "flash_id"];
    if (o.action === "erase") return [...base, "erase_flash"];
    if (!o.files?.length) throw new HttpError(400, "No firmware files to write");
    return [...base, "write_flash", "-z", ...o.files.flatMap((f) => [hex(f.offset), f.path])];
  }
  if (o.tool === "avrdude") {
    const pr = AVR_PRESETS[o.preset || "uno"];
    if (!pr) throw new HttpError(400, `Unknown avrdude preset "${o.preset}" (one of ${Object.keys(AVR_PRESETS).join(", ")})`);
    const base = ["/usr/bin/avrdude", "-p", pr.part, "-c", pr.programmer, "-P", port, "-b", String(o.baud || pr.baud)];
    if (o.action === "info") return base; // connects and reads the signature
    if (o.action === "erase") return [...base, "-e"];
    const f = o.files?.[0];
    if (!f) throw new HttpError(400, "No firmware file to write");
    if (o.files!.length > 1) throw new HttpError(400, "avrdude takes one file");
    // -D: the bootloader erases page by page; a chip erase isn't possible through it.
    return [...base, "-D", "-U", `flash:w:${f.path}:${f.name.toLowerCase().endsWith(".hex") ? "i" : "r"}`];
  }
  throw new HttpError(400, `Unknown flash tool "${(o as any).tool}" (esptool or avrdude)`);
}

/** Streaming parser for esptool/avrdude output: overall percentage, phase, chip facts, error. */
export class FlashProgress {
  progress = 0;
  phase = "starting";
  info: Record<string, string> = {};
  error = "";
  private buf = "";
  private filesDone = 0;
  private avrStage = 0; // 1 signature read, 2 write, 3 verify

  constructor(public tool: "esptool" | "avrdude", public nFiles = 1, public action = "write") {}

  /** Feed raw output; returns the lines it completed. esptool rewrites progress with \r when on a tty, so both end a line. */
  feed(text: string): string[] {
    this.buf += text;
    const out: string[] = [];
    let i;
    while ((i = this.buf.search(/[\r\n]/)) >= 0) {
      const line = this.buf.slice(0, i);
      this.buf = this.buf.slice(i + 1);
      if (line.trim()) {
        out.push(line);
        this.line(line);
      }
    }
    // avrdude draws its bar a '#' at a time with no line end until it finishes.
    if (this.tool === "avrdude" && /^(Reading|Writing) \|/.test(this.buf)) this.avrBar(this.buf, false);
    return out;
  }

  end(): string[] {
    const rest = this.buf.trim();
    this.buf = "";
    if (rest) this.line(rest);
    return rest ? [rest] : [];
  }

  private line(l: string) {
    let m: RegExpExecArray | null;
    if (this.tool === "esptool") {
      if (/^Connecting/.test(l)) this.phase = "connecting";
      else if ((m = /^Chip is (.+)/.exec(l))) (this.info.chip = m[1]), (this.phase = "connected");
      else if ((m = /^Detecting chip type\.\.\. (.+)/.exec(l))) this.info.chipType = m[1];
      else if ((m = /^Features: (.+)/.exec(l))) this.info.features = m[1];
      else if ((m = /^Crystal is (.+)/.exec(l))) this.info.crystal = m[1];
      else if ((m = /^MAC: (.+)/.exec(l))) this.info.mac = m[1];
      else if ((m = /^Manufacturer: (\w+)/.exec(l))) this.info.flashManufacturer = m[1];
      else if ((m = /^Device: (\w+)/.exec(l))) this.info.flashDevice = m[1];
      else if ((m = /^Detected flash size: (\S+)/.exec(l))) this.info.flashSize = m[1];
      else if (/^Erasing flash/.test(l)) this.phase = "erasing";
      else if (/^Chip erase completed/.test(l)) this.progress = 100;
      else if (/^(Compressed \d+ bytes|Flash will be erased)/.test(l)) this.phase = "writing";
      else if ((m = /^Writing at 0x[0-9a-f]+\.\.\. \((\d+) ?%\)/i.exec(l))) {
        this.phase = "writing";
        this.progress = Math.min(100, ((this.filesDone + +m[1] / 100) / this.nFiles) * 100);
      } else if (/^Wrote \d+ bytes/.test(l)) {
        this.filesDone++;
        this.progress = Math.min(100, (this.filesDone / this.nFiles) * 100);
      } else if (/^Hash of data verified/.test(l)) this.info.verified = "yes";
      else if (/^(Hard resetting|Leaving)/.test(l)) this.phase = "resetting";
      else if ((m = /A fatal error occurred: (.+)/.exec(l))) this.error = m[1];
      else if ((m = /^(\w+(?:Error|Exception)): (.+)/.exec(l)) && !this.error) this.error = `${m[1]}: ${m[2]}`;
      if (this.action !== "write" && this.phase === "connected" && this.action === "info") this.progress = Math.max(this.progress, 50);
    } else {
      if (/^(Reading|Writing) \|/.test(l)) this.avrBar(l, true);
      else if ((m = /Device signature = (0x[0-9a-f]+)(?: \(probably (\w+)\))?/i.exec(l))) {
        this.info.signature = m[1];
        if (m[2]) this.info.part = m[2];
      } else if (/bytes of flash verified/.test(l)) (this.progress = 100), (this.info.verified = "yes");
      else if ((m = /^avrdude(?: OS)? error: (.+)/.exec(l)) || (m = /^avrdude: (.*(?:not responding|can't open|failed|error).*)$/i.exec(l))) this.error = m[1];
    }
  }

  private avrBar(l: string, done: boolean) {
    const writing = l.startsWith("Writing");
    if (writing) this.avrStage = 2;
    else if (this.avrStage === 0 || (this.avrStage === 1 && done === false && this.phase !== "reading")) this.avrStage = this.avrStage || 1;
    if (!writing && this.avrStage >= 2) this.avrStage = 3;
    const pct = Math.min(100, ((l.split("|")[1] || "").match(/#/g)?.length || 0) * 2);
    if (this.action !== "write") {
      this.phase = "reading";
      this.progress = pct;
      return;
    }
    const [lo, hi, phase] = this.avrStage === 1 ? [0, 5, "connecting"] : this.avrStage === 2 ? [5, 75, "writing"] : [75, 100, "verifying"];
    this.phase = phase;
    this.progress = Math.max(this.progress, lo + ((hi - lo) * pct) / 100);
  }
}

/** Strip ANSI escape sequences for agents (colours, cursor moves, OSC titles). */
export function stripAnsi(s: string) {
  return s.replace(/\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]/g, "");
}

export function parseHex(s: string): Uint8Array {
  const clean = s.replace(/0x/gi, "").replace(/[\s,:]/g, "");
  if (!/^([0-9a-fA-F]{2})*$/.test(clean)) throw new HttpError(400, "hex must be pairs of hex digits (spaces, commas, colons and 0x prefixes are ignored)");
  return Uint8Array.from(clean.match(/../g) || [], (h) => parseInt(h, 16));
}

const EOLS: Record<string, string> = { none: "", cr: "\r", lf: "\n", crlf: "\r\n" };

/** Payload from {text|hex|base64, eol}. */
export function payload(a: { text?: string; hex?: string; base64?: string; eol?: string }): Uint8Array {
  const eol = a.eol == null ? "" : EOLS[String(a.eol).toLowerCase()];
  if (eol === undefined) throw new HttpError(400, "eol must be none, cr, lf or crlf");
  const parts: Uint8Array[] = [];
  if (a.text != null) parts.push(new TextEncoder().encode(String(a.text)));
  if (a.hex != null) parts.push(parseHex(String(a.hex)));
  if (a.base64 != null) parts.push(Uint8Array.from(Buffer.from(String(a.base64), "base64")));
  if (!parts.length) throw new HttpError(400, "Give text, hex or base64");
  if (eol) parts.push(new TextEncoder().encode(eol));
  return Buffer.concat(parts);
}

// ---------- Port model ----------

type Kind = "usb" | "onboard" | "manual";
type BridgeKind = "tcp" | "rfc2217";

interface NetClient {
  kind: BridgeKind;
  pending: Uint8Array[];
  pendingBytes: number;
  dropped: number;
  suspended: boolean;
  session?: Rfc2217Session;
  remote: string;
}

interface Bridge {
  listener: TCPSocketListener<NetClient> | null;
  clients: Set<NetSocket<NetClient>>;
  error: string;
  port: number;
}

interface FlashJob {
  id: string;
  tool: string;
  action: string;
  label: string;
  state: "preparing" | "running" | "done" | "error" | "cancelled";
  phase: string;
  progress: number;
  info: Record<string, string>;
  message: string;
  log: string[];
  started: number;
  ended?: number;
  cmd: string[];
  proc?: Subprocess;
}

interface Port {
  id: string;
  path: string;
  devpath: string;
  kind: Kind;
  name: string;
  vendor: string;
  model: string;
  vidpid: string;
  serial: string;
  usbDriver: string;
  byId: string;
  family: Family;
  console: boolean;
  present: boolean;

  fd: number | null;
  reader: ReadableStreamDefaultReader<Uint8Array> | null;
  state: "closed" | "opening" | "open" | "autobaud" | "flashing" | "resetting";
  error: string;
  want: boolean;
  leases: number;
  cfg: T.SerialConfig;
  lines: T.ModemLines | null;
  modemTimer: ReturnType<typeof setInterval> | null;
  brk: boolean;

  ring: Ring;
  rxBytes: number;
  txBytes: number;
  lastRx: number;
  txq: Uint8Array[];
  txqBytes: number;
  txTimer: ReturnType<typeof setTimeout> | null;
  tap: ((b: Uint8Array) => void) | null; // auto-baud diverts rx here
  waiters: Set<() => void>;

  ws: Set<ServerWebSocket<WsData>>;
  bridges: Record<BridgeKind, Bridge>;
  lock: { owner: string; token: string; since: number } | null;
  flash: FlashJob | null;
  autobaud: (AutobaudResult & { at: number; applied: boolean }) | null;
}

interface PortSettings {
  config?: T.SerialConfig;
  openOnPlug?: boolean;
  bridges?: Partial<Record<BridgeKind, boolean>>;
  bridgePorts?: Partial<Record<BridgeKind, number>>;
  slot?: number;
}

let ctx: Ctx;
const ports = new Map<string, Port>();
const OFFSET = parseInt(process.env.DESKYPI_PORT_OFFSET || "") || 0;
const FLASH_DIR = "/var/tmp/deskypi-serial";
const TXQ_MAX = 1 << 20;
const NET_PENDING_MAX = 1 << 20;
const MAX_WAIT_MS = 60000; // server.ts sets Bun.serve's idleTimeout to 120 s, so a minute-long wait is safe
const ENV = { ...process.env, PATH: `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:${process.env.PATH || ""}` };

function portSettings(id: string): PortSettings {
  return ctx.settings.get<Record<string, PortSettings>>("ports", {})[id] || {};
}

function savePortSettings(id: string, patch: Partial<PortSettings>) {
  const all = { ...ctx.settings.get<Record<string, PortSettings>>("ports", {}) };
  all[id] = { ...all[id], ...patch };
  ctx.settings.set("ports", all);
}

/** Deterministic, persisted bridge slot per device: the lowest free one, assigned on first sight. */
function slotFor(id: string): number {
  const all = ctx.settings.get<Record<string, PortSettings>>("ports", {});
  if (all[id]?.slot != null) return all[id].slot!;
  const used = new Set(Object.values(all).map((s) => s.slot));
  let slot = 0;
  while (used.has(slot)) slot++;
  savePortSettings(id, { slot });
  return slot;
}

function bridgePort(id: string, kind: BridgeKind): number {
  const explicit = portSettings(id).bridgePorts?.[kind];
  if (explicit) return explicit + OFFSET;
  const base = kind === "tcp" ? ctx.settings.get("tcpBase", 4000) : ctx.settings.get("rfc2217Base", 4100);
  return base + slotFor(id) + OFFSET;
}

function newPort(init: PortInit): Port {
  const st = portSettings(init.id);
  return {
    ...init,
    present: true,
    fd: null,
    reader: null,
    state: "closed",
    error: "",
    want: false,
    leases: 0,
    cfg: st.config ? T.normalizeConfig(st.config) : { ...T.DEFAULT_CONFIG },
    lines: null,
    modemTimer: null,
    brk: false,
    ring: new Ring(),
    rxBytes: 0,
    txBytes: 0,
    lastRx: 0,
    txq: [],
    txqBytes: 0,
    txTimer: null,
    tap: null,
    waiters: new Set(),
    ws: new Set(),
    bridges: { tcp: { listener: null, clients: new Set(), error: "", port: 0 }, rfc2217: { listener: null, clients: new Set(), error: "", port: 0 } },
    lock: null,
    flash: null,
    autobaud: null,
  };
}

// ---------- Discovery ----------

let cmdline = "";

function isUsbTty(p: Props) {
  if (p.SUBSYSTEM !== "tty" || !p.DEVNAME) return false;
  if (/^\/dev\/ttyGS/.test(p.DEVNAME)) return false; // gadget side, not an adapter
  return p.ID_BUS === "usb" || !!ctx.ancestor(p.DEVPATH, "usb", "usb_device");
}

type PortInit = Pick<Port, "id" | "path" | "devpath" | "kind" | "name" | "vendor" | "model" | "vidpid" | "serial" | "usbDriver" | "byId" | "family" | "console">;

function describeUsb(p: Props): PortInit {
  const dev = ctx.ancestor(p.DEVPATH, "usb", "usb_device") || {};
  const merged = { ...dev, ...p };
  const vid = (merged.ID_VENDOR_ID || "").toLowerCase();
  const pid = (merged.ID_MODEL_ID || "").toLowerCase();
  const known = identify(vid, pid);
  const { vendor, model } = usbName(merged);
  const usbDriver = p.ID_USB_DRIVER || "";
  const links = (p.DEVLINKS || "").split(/\s+/);
  let family: Family = known?.family || (usbDriver && usbDriver !== "cdc_acm" ? "bridge" : "other");
  // A generic Arduino-vendor id with an 8xxx pid is a native-USB board (Leonardo, SAMD…).
  if (vid === "2341" && !known?.pid && /^8/.test(pid)) family = "arduino-native";
  const iface = p.ID_USB_INTERFACE_NUM && p.ID_USB_INTERFACE_NUM !== "00" ? ` (port ${parseInt(p.ID_USB_INTERFACE_NUM, 16)})` : "";
  return {
    id: stableId(p),
    path: p.DEVNAME,
    devpath: p.DEVPATH,
    kind: "usb" as Kind,
    name: (known?.name || model || p.DEVNAME.replace("/dev/", "")) + iface,
    vendor,
    model,
    vidpid: vid && pid ? `${vid}:${pid}` : "",
    serial: merged.ID_SERIAL_SHORT || "",
    usbDriver: USB_DRIVERS[usbDriver] || usbDriver,
    byId: links.find((l) => l.startsWith("/dev/serial/by-id/")) || "",
    family,
    console: false,
  };
}

/** Is this tty a kernel console (cmdline console=…)? Then we never write to or reconfigure it. */
function isConsole(alias: string, real: string) {
  const names = [...cmdline.matchAll(/(?:^|\s)console=([^,\s]+)/g)].map((m) => m[1]);
  return names.includes(alias) || names.includes(real);
}

function rescan() {
  const seen = new Set<string>();
  const add = (d: PortInit) => {
    seen.add(d.id);
    const p = ports.get(d.id);
    if (!p) {
      const np = newPort(d);
      ports.set(d.id, np);
      if (d.kind === "usb") ctx.event(`Serial: ${d.name} on ${d.path}`, { id: d.id, vidpid: d.vidpid });
      plugged(np);
    } else if (!p.present || p.path !== d.path) {
      Object.assign(p, d, { present: true });
      if (d.kind === "usb") ctx.event(`Serial: ${d.name} back on ${d.path}`, { id: d.id });
      plugged(p);
    }
  };

  for (const d of ctx.devices(isUsbTty)) add(describeUsb(d));

  // Onboard UARTs by their stable aliases.
  for (const alias of ["serial0", "serial1"]) {
    const link = `/dev/${alias}`;
    if (!existsSync(link)) continue;
    let real = "";
    try {
      real = realpathSync(link).replace("/dev/", "");
    } catch {
      continue;
    }
    const cons = isConsole(alias, real);
    add({ id: alias, path: link, devpath: "", kind: "onboard", name: `Onboard UART ${alias} (${real})`, vendor: "", model: "", vidpid: "", serial: "", usbDriver: "", byId: "", family: "onboard", console: cons });
  }

  for (const path of ctx.settings.get<string[]>("manual", [])) {
    if (!existsSync(path)) continue;
    add({ id: manualId(path), path, devpath: "", kind: "manual", name: path, vendor: "", model: "", vidpid: "", serial: "", usbDriver: "", byId: "", family: "other", console: false });
  }

  for (const p of ports.values()) {
    if (seen.has(p.id) || !p.present) continue;
    // Manual ports (ptys) are only noticed missing when their stream ends; let that path decide.
    if (p.kind === "manual" && p.fd != null) continue;
    unplugged(p);
  }
  ctx.changed();
}

function manualId(path: string) {
  return "path" + path.replace(/[^A-Za-z0-9.]+/g, "-");
}

function plugged(p: Port) {
  syncBridges(p);
  if (p.want || portSettings(p.id).openOnPlug || p.leases > 0) {
    // udev has set group and mode by the time we hear the udev (not kernel) event, but
    // cdc_acm can still refuse the first open for a moment after enumeration.
    setTimeout(() => ensureOpen(p).catch((e) => ctx.log(`${p.id}: open on plug failed: ${e.message}`)), 300);
  }
}

function unplugged(p: Port) {
  if (!p.present) return;
  p.present = false;
  if (p.kind === "usb") ctx.event(`Serial: ${p.name} unplugged`, { id: p.id });
  closeFd(p, "unplugged");
  syncBridges(p);
  notify(p);
}

// ---------- Open / close / I/O ----------

function getPort(id: string): Port {
  const p = ports.get(id);
  if (!p) throw new HttpError(404, `No serial port "${id}". GET /api/serial lists them.`);
  return p;
}

function ensurePresent(p: Port) {
  if (!p.present) throw new HttpError(409, `${p.name} is unplugged`);
}

function ensureWritable(p: Port) {
  if (p.console) throw new HttpError(409, `${p.path} is the kernel console (cmdline.txt console=); DeskyPi only reads it`);
  if (p.state === "flashing") throw new HttpError(409, `${p.name} is being flashed`);
}

async function ensureOpen(p: Port): Promise<void> {
  if (p.fd != null) return;
  if (p.state === "flashing") throw new HttpError(409, `${p.name} is being flashed`);
  ensurePresent(p);
  let fd: number;
  try {
    fd = T.openPort(p.path, p.console);
  } catch (e: any) {
    p.error = `open ${p.path}: ${e.code || e.message}`;
    notify(p);
    throw new HttpError(e.code === "EBUSY" ? 409 : e.code === "EACCES" ? 403 : 500, p.error);
  }
  p.fd = fd;
  p.error = "";
  try {
    if (!p.console) {
      // Exclusive so nothing else opens the adapter behind our back. Not for manual paths:
      // the flag lives on the tty until its *last* fd closes, so with a pty's other end (or
      // anything else) still holding it, a DeskyPi that dies without clearing it would leave
      // the port refusing every non-root open, its own restart included. A USB adapter only
      // we hold resets on our close, even a crash.
      if (p.kind !== "manual")
        try {
          T.setExclusive(fd);
        } catch {}
      p.cfg = { ...T.configure(fd, p.cfg), flow: p.cfg.flow };
    } else p.cfg = T.decodeTermios(T.getTermios(fd));
  } catch (e: any) {
    T.closePort(fd);
    p.fd = null;
    p.error = e.message;
    notify(p);
    throw new HttpError(500, `${p.path}: ${e.message}`);
  }
  p.state = "open";
  p.brk = false;
  startModemPoll(p);
  startReader(p, fd);
  ctx.log(`${p.id}: opened ${p.path} ${T.configLabel(p.cfg)}`);
  notify(p);
}

function startReader(p: Port, fd: number) {
  const reader = Bun.file(fd).stream().getReader() as ReadableStreamDefaultReader<Uint8Array>;
  p.reader = reader;
  (async () => {
    let err = "";
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        if (value?.length) onRx(p, value);
      }
    } catch (e: any) {
      err = e?.code || e?.message || String(e);
    }
    // Ended without us asking: the device went away (USB unplug, pty master closed) or EIO.
    if (p.reader === reader) {
      ctx.log(`${p.id}: read ended${err ? ` (${err})` : ""}`);
      closeFd(p, err ? `read error: ${err}` : "device went away");
      if (!existsSync(p.path)) unplugged(p);
      else if (p.want || p.leases) setTimeout(() => ensureOpen(p).catch(() => {}), 1000);
    }
  })();
}

/** Release the fd. Resolves once it is really closed, so a tool spawned next can open the tty. */
function closeFd(p: Port, why = ""): Promise<void> {
  const fd = p.fd, reader = p.reader;
  p.reader = null;
  p.fd = null;
  if (p.modemTimer) clearInterval(p.modemTimer);
  if (p.txTimer) clearTimeout(p.txTimer);
  p.modemTimer = p.txTimer = null;
  p.txq = [];
  p.txqBytes = 0;
  p.lines = null;
  if (p.state !== "flashing") p.state = "closed";
  if (why && why !== "closed") p.error = why === "unplugged" ? "" : why;
  if (fd != null && !p.console)
    try {
      T.clearExclusive(fd);
    } catch {}
  for (const w of p.waiters) w();
  notify(p);
  // cancel() first so Bun's reader lets go of the fd before the number can be reused.
  if (!reader) return Promise.resolve(fd != null ? T.closePort(fd) : undefined);
  return reader
    .cancel()
    .catch(() => {})
    .then(() => {
      if (fd != null) T.closePort(fd);
    });
}

function onRx(p: Port, data: Uint8Array) {
  if (p.tap) return p.tap(data);
  p.ring.push(data);
  p.rxBytes += data.length;
  p.lastRx = Date.now();
  for (const ws of p.ws) {
    // A browser tab that can't keep up is skipped rather than buffered without bound.
    if (ws.getBufferedAmount() < 4 << 20) ws.sendBinary(data);
  }
  for (const kind of ["tcp", "rfc2217"] as const) for (const s of p.bridges[kind].clients) netSend(s, s.data.session ? s.data.session.data(data) : data);
  for (const w of p.waiters) w();
}

function lockCheck(p: Port, token?: string) {
  if (p.lock && p.lock.token !== token) throw new HttpError(423, `Writing to ${p.name} is locked by ${p.lock.owner}`);
}

function write(p: Port, data: Uint8Array, token?: string) {
  ensureWritable(p);
  lockCheck(p, token);
  if (p.fd == null) throw new HttpError(409, `${p.name} is not open`);
  if (p.txqBytes + data.length > TXQ_MAX) throw new HttpError(503, "Transmit queue full (the device isn't taking data; flow control?)");
  p.txq.push(data);
  p.txqBytes += data.length;
  pump(p);
}

function pump(p: Port) {
  if (p.txTimer || p.fd == null) return;
  // Auto-baud promises to listen only: hold anything queued until it finishes.
  if (p.state === "autobaud") {
    p.txTimer = setTimeout(() => ((p.txTimer = null), pump(p)), 50);
    return;
  }
  while (p.txq.length) {
    const b = p.txq[0];
    let n: number;
    try {
      n = T.writeSome(p.fd, b);
    } catch (e: any) {
      closeFd(p, `write error: ${e.code || e.message}`);
      return;
    }
    p.txBytes += n;
    p.txqBytes -= n;
    if (n < b.length) {
      // Kernel buffer full (EAGAIN or partial). No writable event for tty fds in Bun, so poll briefly.
      p.txq[0] = b.subarray(n);
      p.txTimer = setTimeout(() => ((p.txTimer = null), pump(p)), 5);
      return;
    }
    p.txq.shift();
  }
}

function startModemPoll(p: Port) {
  if (p.fd == null) return;
  try {
    p.lines = T.getModem(p.fd);
  } catch {
    p.lines = null; // ptys and some CDC devices have no modem lines
    return;
  }
  p.modemTimer = setInterval(() => {
    if (p.fd == null) return;
    let l: T.ModemLines;
    try {
      l = T.getModem(p.fd);
    } catch (e: any) {
      if (e.code === "EIO") closeFd(p, "device went away");
      return;
    }
    const o = p.lines;
    if (o && o.dtr === l.dtr && o.rts === l.rts && o.cts === l.cts && o.dsr === l.dsr && o.dcd === l.dcd && o.ri === l.ri) return;
    p.lines = l;
    for (const s of p.bridges.rfc2217.clients) s.data.session?.notifyModem();
    for (const ws of p.ws) if (!ws.data.state.quiet) ws.send(JSON.stringify({ type: "lines", lines: l }));
    ctx.changed();
  }, 200);
}

function setLines(p: Port, l: { dtr?: boolean; rts?: boolean }) {
  ensureWritable(p);
  if (p.fd == null) throw new HttpError(409, `${p.name} is not open`);
  try {
    T.setModem(p.fd, l);
  } catch (e: any) {
    throw new HttpError(409, `${p.name} has no controllable modem lines (${e.code || e.message})`);
  }
  try {
    p.lines = T.getModem(p.fd);
  } catch {}
  for (const ws of p.ws) if (!ws.data.state.quiet) ws.send(JSON.stringify({ type: "lines", lines: p.lines }));
  ctx.changed();
}

function applyConfig(p: Port, c: Record<string, any>, save: boolean): T.SerialConfig {
  ensureWritable(p);
  let cfg: T.SerialConfig;
  try {
    cfg = T.normalizeConfig(c, p.cfg);
  } catch (e: any) {
    throw new HttpError(400, e.message);
  }
  if (p.fd != null) {
    const eff = T.configure(p.fd, cfg);
    // ptys ignore data bits/parity; keep what was asked for so the UI and saved default don't drift.
    p.cfg = p.kind === "manual" ? cfg : eff;
  } else p.cfg = cfg;
  if (save) savePortSettings(p.id, { config: p.cfg });
  notify(p);
  return p.cfg;
}

async function sendBreak(p: Port, ms = 250) {
  ensureWritable(p);
  if (p.fd == null) throw new HttpError(409, `${p.name} is not open`);
  ms = Math.max(10, Math.min(5000, ms));
  T.setBreak(p.fd, true);
  p.brk = true;
  await Bun.sleep(ms);
  if (p.fd != null) T.setBreak(p.fd, false);
  p.brk = false;
}

async function reset(p: Port, mode: ResetMode) {
  ensureWritable(p);
  ensurePresent(p);
  if (mode === "touch1200") {
    // Native-USB Arduinos and the Pico SDK reboot into their bootloader when the port is
    // opened at 1200 baud and closed (DTR drops). The device usually re-enumerates.
    const wasOpen = p.fd != null;
    await closeFd(p, "closed");
    const fd = T.openPort(p.path);
    try {
      T.configure(fd, { ...p.cfg, baud: 1200 });
      try {
        T.setModem(fd, { dtr: false });
      } catch {}
    } finally {
      T.closePort(fd);
    }
    if (wasOpen) setTimeout(() => existsSync(p.path) && ensureOpen(p).catch(() => {}), 1500);
    return;
  }
  const seq = resetSequence(mode, p.family);
  if (!seq.length) throw new HttpError(400, `Unknown reset mode "${mode}" (esp, esp-boot, dtr, touch1200)`);
  await ensureOpen(p);
  const prev = p.state;
  p.state = "resetting";
  notify(p);
  try {
    for (const step of seq) {
      for (const [k, v] of Object.entries(step)) {
        if (k === "ms") await Bun.sleep(v as number);
        else if (p.fd != null) T.setModem(p.fd, { [k]: v });
      }
    }
  } catch (e: any) {
    throw new HttpError(409, `${p.name} has no controllable DTR/RTS (${e.code || e.message})`);
  } finally {
    if (p.state === "resetting") p.state = prev === "resetting" ? "open" : prev;
    if (p.fd != null)
      try {
        p.lines = T.getModem(p.fd);
      } catch {}
    notify(p);
  }
}

function release(p: Port) {
  // Drop a lease; close when nobody wants the port any more.
  p.leases = Math.max(0, p.leases - 1);
  if (!p.leases && !p.want && !portSettings(p.id).openOnPlug && p.fd != null && p.state === "open") closeFd(p, "closed");
}

// ---------- Auto-baud ----------

async function autobaud(p: Port, o: { rates?: number[]; ms?: number; apply?: boolean }) {
  ensureWritable(p);
  if (p.state === "autobaud") throw new HttpError(409, "Auto-baud already running");
  const rates = (o.rates?.length ? o.rates : AUTOBAUD_RATES).map(Number).filter((r) => Number.isInteger(r) && r >= 50);
  if (!rates.length) throw new HttpError(400, "No valid rates");
  // Keep the whole sweep inside one HTTP request (server.ts's idleTimeout).
  const ms = Math.max(100, Math.min(o.ms || 400, Math.floor(8500 / rates.length)));
  await ensureOpen(p);
  const saved = { ...p.cfg };
  const samples: { baud: number; data: Uint8Array }[] = [];
  let cur: Uint8Array[] = [];
  p.state = "autobaud";
  p.tap = (b) => cur.push(b);
  notify(p);
  try {
    for (const baud of rates) {
      if (p.fd == null) throw new HttpError(409, `${p.name} closed during auto-baud`);
      T.configure(p.fd, { ...saved, baud });
      T.flush(p.fd, "rx");
      cur = [];
      await Bun.sleep(ms);
      samples.push({ baud, data: Buffer.concat(cur) });
    }
  } catch (e) {
    p.tap = null;
    if (p.fd != null)
      try {
        T.configure(p.fd, saved);
      } catch {}
    if (p.state === "autobaud") p.state = p.fd != null ? "open" : "closed";
    notify(p);
    throw e;
  }
  const r = pickBaud(samples);
  const apply = !!(o.apply && r.best && r.best.confidence >= 0.5);
  // Restore (or apply) the rate and drop whatever arrived at the last trial rate before the
  // tap comes off, so no garbage lands in the ring.
  if (p.fd != null) {
    T.configure(p.fd, apply ? { ...saved, baud: r.best!.baud } : saved);
    T.flush(p.fd, "rx");
    await Bun.sleep(30);
  }
  p.tap = null;
  if (p.state === "autobaud") p.state = p.fd != null ? "open" : "closed";
  if (apply) p.cfg = { ...saved, baud: r.best!.baud };
  if (apply) savePortSettings(p.id, { config: p.cfg });
  p.autobaud = { ...r, at: Date.now(), applied: apply };
  notify(p);
  return { ...r, msPerRate: ms, applied: apply, config: p.cfg };
}

// ---------- Network bridges ----------

function listen(port: number, handlers: any): TCPSocketListener<NetClient> {
  try {
    return Bun.listen<NetClient>({ hostname: "::", port, socket: handlers });
  } catch (e: any) {
    if (e?.code === "EADDRINUSE") throw e;
    return Bun.listen<NetClient>({ hostname: "0.0.0.0", port, socket: handlers });
  }
}

function netSend(s: NetSocket<NetClient>, b: Uint8Array) {
  const c = s.data;
  if (!c || !b.length) return;
  if (c.pending.length || c.suspended) {
    if (c.pendingBytes + b.length > NET_PENDING_MAX) c.dropped += b.length;
    else c.pending.push(b), (c.pendingBytes += b.length);
    return;
  }
  const n = s.write(b);
  if (n < b.length) {
    c.pending.push(b.subarray(Math.max(0, n)));
    c.pendingBytes += b.length - Math.max(0, n);
  }
}

function netDrain(s: NetSocket<NetClient>) {
  const c = s.data;
  while (c.pending.length && !c.suspended) {
    const b = c.pending[0];
    const n = s.write(b);
    if (n < b.length) {
      c.pending[0] = b.subarray(Math.max(0, n));
      c.pendingBytes -= Math.max(0, n);
      return;
    }
    c.pending.shift();
    c.pendingBytes -= b.length;
  }
}

function startBridge(p: Port, kind: BridgeKind) {
  const br = p.bridges[kind];
  if (br.listener) return;
  br.port = bridgePort(p.id, kind);
  try {
    br.listener = listen(br.port, {
      open(s: NetSocket<NetClient>) {
        s.data = { kind, pending: [], pendingBytes: 0, dropped: 0, suspended: false, remote: s.remoteAddress };
        br.clients.add(s);
        if (kind === "rfc2217") {
          const sess = new Rfc2217Session(rfcHooks(p, s), (b) => s.write(b));
          s.data.session = sess;
          sess.start();
        }
        p.leases++;
        ensureOpen(p).catch((e) => {
          s.write(`DeskyPi: cannot open ${p.path}: ${e.message}\r\n`);
          s.end();
        });
        ctx.log(`${p.id}: ${kind} client ${s.remoteAddress}`);
        notify(p);
      },
      data(s: NetSocket<NetClient>, buf: Buffer) {
        const d = s.data.session ? s.data.session.feed(buf) : buf;
        if (!d.length || p.console || p.state === "flashing") return;
        if (p.lock) return; // locked to another writer; network clients can't hold the lock
        if (p.fd == null) return;
        try {
          write(p, new Uint8Array(d));
        } catch {}
      },
      drain: netDrain,
      close(s: NetSocket<NetClient>) {
        if (!br.clients.delete(s)) return;
        release(p);
        notify(p);
      },
      error(s: NetSocket<NetClient>) {
        s.end();
      },
    });
    br.error = "";
    ctx.log(`${p.id}: ${kind} bridge on :${br.port}`);
  } catch (e: any) {
    br.listener = null;
    br.error = `port ${br.port}: ${e.code || e.message}`;
    ctx.log(`${p.id}: ${kind} bridge failed: ${br.error}`);
  }
}

function stopBridge(p: Port, kind: BridgeKind) {
  const br = p.bridges[kind];
  for (const s of [...br.clients]) {
    br.clients.delete(s);
    p.leases = Math.max(0, p.leases - 1);
    s.end();
  }
  br.listener?.stop(true);
  br.listener = null;
}

function syncBridges(p: Port) {
  const want = portSettings(p.id).bridges || {};
  for (const kind of ["tcp", "rfc2217"] as const) {
    if (want[kind] && p.present) startBridge(p, kind);
    else stopBridge(p, kind);
  }
  if (!p.leases && !p.want && !portSettings(p.id).openOnPlug && p.fd != null && p.state === "open") closeFd(p, "closed");
}

function rfcHooks(p: Port, s: NetSocket<NetClient>) {
  const guard = <A extends any[], R>(fn: (...a: A) => R, dflt: R) => (...a: A): R => {
    try {
      return fn(...a);
    } catch (e: any) {
      ctx.log(`${p.id}: rfc2217: ${e.message}`);
      return dflt;
    }
  };
  return {
    config: () => p.cfg,
    // Remote changes are live but not saved as the port's default.
    setConfig: (c: Partial<T.SerialConfig>) => applyConfig(p, c, false),
    lines: () => p.lines,
    setLines: guard((l: { dtr?: boolean; rts?: boolean }) => setLines(p, l), undefined),
    breakState: () => p.brk,
    setBreak: guard((on: boolean) => {
      ensureWritable(p);
      if (p.fd != null) T.setBreak(p.fd, on);
      p.brk = on;
    }, undefined),
    purge: guard((which: "rx" | "tx" | "both") => {
      if (p.fd != null) T.flush(p.fd, which);
      if (which !== "rx") (p.txq = []), (p.txqBytes = 0);
    }, undefined),
    suspend: (on: boolean) => {
      s.data.suspended = on;
      if (!on) netDrain(s);
    },
    log: (m: string) => ctx.log(`${p.id}: rfc2217 ${s.remoteAddress}: ${m}`),
  };
}

function setBridge(p: Port, kind: BridgeKind, enabled: boolean, port?: number) {
  if (kind !== "tcp" && kind !== "rfc2217") throw new HttpError(400, 'kind must be "tcp" or "rfc2217"');
  if (port !== undefined && port !== null) {
    port = Number(port);
    if (!Number.isInteger(port) || port < 1024 || port > 65535) throw new HttpError(400, "port must be 1024-65535");
    const cur = portSettings(p.id).bridgePorts || {};
    savePortSettings(p.id, { bridgePorts: { ...cur, [kind]: port - OFFSET } });
    stopBridge(p, kind);
  }
  const cur = portSettings(p.id).bridges || {};
  savePortSettings(p.id, { bridges: { ...cur, [kind]: !!enabled } });
  syncBridges(p);
  notify(p);
  const br = p.bridges[kind];
  if (enabled && br.error) throw new HttpError(409, `Bridge failed: ${br.error}`);
  return bridgeStatus(p, kind, hostname());
}

function bridgeStatus(p: Port, kind: BridgeKind, host: string) {
  const br = p.bridges[kind];
  const port = br.port || bridgePort(p.id, kind);
  const enabled = !!portSettings(p.id).bridges?.[kind];
  return {
    enabled,
    port,
    listening: !!br.listener,
    clients: [...br.clients].map((s) => s.data?.remote),
    error: br.error,
    commands:
      kind === "tcp"
        ? [`nc ${host} ${port}`, `socat -,raw,echo=0 TCP:${host}:${port}`, `socat pty,link=/tmp/ttyDesky,raw,echo=0 TCP:${host}:${port}`]
        : [`python -m serial.tools.miniterm rfc2217://${host}:${port} ${p.cfg.baud}`, `esptool --port 'rfc2217://${host}:${port}?ign_set_control' flash_id`, `pio device monitor --port rfc2217://${host}:${port} -b ${p.cfg.baud}`],
  };
}

// ---------- Flashing ----------

let jobSeq = 0;

async function startFlash(p: Port, o: FlashOpts & { sources?: { offset: number; name: string; data?: Uint8Array; url?: string }[] }): Promise<FlashJob> {
  ensurePresent(p);
  ensureWritable(p);
  if (p.flash && (p.flash.state === "running" || p.flash.state === "preparing")) throw new HttpError(409, "Already flashing");
  if (!["write", "erase", "info"].includes(o.action)) throw new HttpError(400, "action must be write, erase or info");
  const srcs = o.action === "write" ? o.sources || [] : [];
  if (o.action === "write" && !srcs.length) throw new HttpError(400, "No firmware: upload a file or give files[] with url or base64");
  const id = `${Date.now().toString(36)}-${++jobSeq}`;
  const dir = `${FLASH_DIR}/${id}`;
  const files: FlashFile[] = srcs.map((s, i) => ({ offset: s.offset, name: s.name || `fw${i}.bin`, path: `${dir}/${i}-${(s.name || "fw.bin").replace(/[^A-Za-z0-9._-]/g, "_")}` }));
  // Validate the command now so a bad preset/chip is a 400, not a failed job.
  const cmd = flashCommand(p.path, { ...o, files });
  const job: FlashJob = { id, tool: o.tool, action: o.action, label: o.action === "write" ? files.map((f) => `${f.name}@${hex(f.offset)}`).join(", ") : o.action, state: "preparing", phase: "preparing", progress: 0, info: {}, message: "", log: [], started: Date.now(), cmd };
  p.flash = job;
  notify(p);
  runFlash(p, job, srcs, files, dir).catch((e) => finishJob(p, job, "error", e.message));
  return job;
}

function flashMsg(p: Port, job: FlashJob, line?: string) {
  const m = JSON.stringify({ type: "flash", job: jobView(job), line });
  for (const ws of p.ws) if (!ws.data.state.quiet) ws.send(m);
}

function jobView(j: FlashJob | null) {
  if (!j) return null;
  const { proc, log, ...rest } = j;
  return { ...rest, progress: Math.round(j.progress), logTail: log.slice(-12) };
}

async function runFlash(p: Port, job: FlashJob, srcs: { data?: Uint8Array; url?: string }[], files: FlashFile[], dir: string) {
  mkdirSync(dir, { recursive: true });
  try {
    for (let i = 0; i < srcs.length; i++) {
      let data = srcs[i].data;
      if (!data && srcs[i].url) {
        job.phase = `downloading ${files[i].name}`;
        flashMsg(p, job);
        const r = await fetch(srcs[i].url!);
        if (!r.ok) throw new Error(`Download ${srcs[i].url}: HTTP ${r.status}`);
        data = new Uint8Array(await r.arrayBuffer());
      }
      if (!data?.length) throw new Error(`${files[i].name} is empty`);
      await Bun.write(files[i].path, data);
    }
    if (job.state === "cancelled") return finishJob(p, job, "cancelled", "Cancelled");
    // Release the port so the tool can open it; bridges stay up but go quiet.
    const reopen = p.fd != null || p.want || p.leases > 0;
    p.state = "flashing"; // set first so nothing reopens it in the gap
    await closeFd(p, "closed");
    job.state = "running";
    job.phase = "starting";
    notify(p);
    const parser = new FlashProgress(job.tool as any, Math.max(1, files.length), job.action);
    const proc = ctx.spawn(job.cmd, { stdout: "pipe", stderr: "pipe", env: ENV });
    job.proc = proc;
    let lastPush = 0;
    const consume = async (stream: ReadableStream<Uint8Array>) => {
      const dec = new TextDecoder();
      for await (const chunk of stream) {
        const lines = parser.feed(dec.decode(chunk, { stream: true }));
        for (const l of lines) {
          job.log.push(l);
          if (job.log.length > 400) job.log.splice(0, job.log.length - 400);
        }
        job.progress = parser.progress;
        job.phase = parser.phase;
        job.info = parser.info;
        // Stream every line, but throttle bare progress ticks.
        const now = Date.now();
        if (lines.length || now - lastPush > 200) {
          lastPush = now;
          if (lines.length) for (const l of lines) flashMsg(p, job, l);
          else flashMsg(p, job);
        }
      }
    };
    // One parser for both streams: esptool talks on stdout, avrdude on stderr.
    await Promise.all([consume(proc.stdout as ReadableStream<Uint8Array>), consume(proc.stderr as ReadableStream<Uint8Array>)]);
    const code = await proc.exited;
    for (const l of parser.end()) job.log.push(l);
    job.info = parser.info;
    if (job.state === "cancelled") finishJob(p, job, "cancelled", "Cancelled");
    else if (code === 0) finishJob(p, job, "done", job.action === "info" ? infoSummary(job) : "Done");
    else finishJob(p, job, "error", parser.error || job.log.filter((l) => !/^\s/.test(l)).slice(-1)[0] || `exit ${code}`);
    p.state = "closed";
    notify(p);
    if (reopen) reopenAfterFlash(p);
  } finally {
    rmSync(dir, { recursive: true, force: true });
    if (p.state === "flashing") p.state = "closed";
  }
}

function infoSummary(job: FlashJob) {
  const i = job.info;
  return [i.chip || i.part, i.mac && `MAC ${i.mac}`, i.flashSize && `${i.flashSize} flash`, i.signature && `signature ${i.signature}`].filter(Boolean).join(" · ") || "Done";
}

function finishJob(p: Port, job: FlashJob, state: FlashJob["state"], message: string) {
  if (job.ended) return;
  job.state = state;
  job.message = message;
  job.ended = Date.now();
  if (state === "done") job.progress = 100;
  job.proc = undefined;
  ctx.event(`Serial: ${job.tool} ${job.action} on ${p.name}: ${state === "done" ? "done" : message}`, { id: p.id });
  flashMsg(p, job);
  notify(p);
}

/** ESP USB-JTAG and native-USB boards re-enumerate after a reset, so the node can vanish briefly. */
function reopenAfterFlash(p: Port, tries = 8) {
  setTimeout(() => {
    if (p.fd != null || p.state === "flashing") return;
    if (existsSync(p.path)) ensureOpen(p).catch(() => tries > 1 && reopenAfterFlash(p, tries - 1));
    else if (tries > 1) reopenAfterFlash(p, tries - 1);
  }, 600);
}

function parseOffset(v: any, dflt = 0): number {
  if (v === undefined || v === null || v === "") return dflt;
  const n = typeof v === "number" ? v : /^0x/i.test(String(v)) ? parseInt(String(v), 16) : parseInt(String(v), 10);
  if (!Number.isInteger(n) || n < 0) throw new HttpError(400, `Bad offset "${v}"`);
  return n;
}

/** Firmware sources from a request: multipart, raw body + query, or JSON {files:[{offset,url|base64,name}]}. */
async function flashRequest(req: Request, url: URL): Promise<FlashOpts & { sources: any[] }> {
  const q = url.searchParams;
  const ct = req.headers.get("content-type") || "";
  let o: Record<string, any> = Object.fromEntries(q);
  const sources: { offset: number; name: string; data?: Uint8Array; url?: string }[] = [];
  if (ct.startsWith("multipart/form-data")) {
    const fd = await req.formData();
    for (const [k, v] of fd.entries()) if (typeof v === "string" && k !== "offset") o[k] = v;
    const offsets = fd.getAll("offset").map(String);
    let i = 0;
    for (const [k, v] of fd.entries()) {
      if (typeof v === "string") continue;
      const f = v as File;
      // A file field named like an offset ("0x10000") carries its own; otherwise offsets[] pair up in order.
      const off = /^(0x[0-9a-f]+|\d+)$/i.test(k) ? parseOffset(k) : parseOffset(offsets[i++], 0);
      sources.push({ offset: off, name: f.name || k, data: new Uint8Array(await f.arrayBuffer()) });
    }
  } else if (ct.includes("application/json")) {
    o = { ...o, ...(await body(req)) };
    for (const f of o.files || []) {
      if (!f.url && !f.base64) throw new HttpError(400, "Each file needs url or base64");
      sources.push({ offset: parseOffset(f.offset, 0), name: f.name || (f.url ? String(f.url).split("/").pop()!.split("?")[0] : "firmware.bin"), url: f.url, data: f.base64 ? Uint8Array.from(Buffer.from(f.base64, "base64")) : undefined });
    }
  } else {
    const data = new Uint8Array(await req.arrayBuffer());
    if (data.length) sources.push({ offset: parseOffset(q.get("offset"), 0), name: q.get("name") || "firmware.bin", data });
  }
  return { tool: (o.tool || "esptool") as any, action: (o.action || (sources.length ? "write" : "info")) as any, baud: o.baud ? Number(o.baud) : undefined, chip: o.chip, preset: o.preset, sources };
}

// ---------- Status ----------

function portStatus(p: Port, host = hostname()) {
  const st = portSettings(p.id);
  return {
    id: p.id,
    path: p.path,
    byId: p.byId,
    kind: p.kind,
    name: p.name,
    vendor: p.vendor,
    model: p.model,
    vidpid: p.vidpid,
    serial: p.serial,
    usbDriver: p.usbDriver,
    family: p.family,
    resets: p.console ? [] : resetsFor(p.family),
    flashTools: p.console ? [] : flashToolsFor(p.family),
    console: p.console,
    present: p.present,
    state: p.state,
    open: p.fd != null,
    error: p.error,
    config: p.cfg,
    configLabel: T.configLabel(p.cfg),
    lines: p.lines,
    modem: !!p.lines,
    break: p.brk,
    rxBytes: p.rxBytes,
    txBytes: p.txBytes,
    lastRx: p.lastRx,
    cursor: p.ring.end,
    openOnPlug: !!st.openOnPlug,
    clients: { ws: p.ws.size, tcp: p.bridges.tcp.clients.size, rfc2217: p.bridges.rfc2217.clients.size },
    bridges: { tcp: bridgeStatus(p, "tcp", host), rfc2217: bridgeStatus(p, "rfc2217", host) },
    lock: p.lock ? { owner: p.lock.owner, since: p.lock.since } : null,
    flash: jobView(p.flash),
    autobaud: p.autobaud,
  };
}

function notify(p: Port) {
  if (p.ws.size) {
    const m = JSON.stringify({ type: "status", port: portStatus(p) });
    for (const ws of p.ws) if (!ws.data.state.quiet) ws.send(m);
  }
  ctx?.changed();
}

function listPorts(host?: string) {
  return [...ports.values()].sort((a, b) => (a.kind === "onboard" ? 1 : 0) - (b.kind === "onboard" ? 1 : 0) || a.path.localeCompare(b.path)).map((p) => portStatus(p, host));
}

function reqHost(req: Request) {
  return (req.headers.get("host") || hostname()).replace(/:\d+$/, "");
}

// ---------- Agent primitives ----------

function decodeOut(b: Uint8Array, raw: boolean) {
  const s = new TextDecoder().decode(b);
  return raw ? s : stripAnsi(s);
}

/** Write, then optionally wait for a regex (or just collect for timeout_ms) and return what came back. */
async function writeAndWait(p: Port, a: any) {
  const data = payload(a);
  if (p.fd == null) {
    ensureWritable(p);
    p.want = true; // an agent writing expects the port to stay open for the reply
    await ensureOpen(p);
  }
  let re: RegExp | null = null;
  if (a.wait) {
    try {
      re = new RegExp(String(a.wait), a.flags ?? "m");
    } catch (e: any) {
      throw new HttpError(400, `Bad wait regex: ${e.message}`);
    }
  }
  const timeout = Math.max(0, Math.min(MAX_WAIT_MS, Number(a.timeout_ms ?? (re ? 3000 : 0)) || 0));
  const raw = !!a.raw;
  const start = p.ring.end;
  write(p, data, a.token);
  const result = () => {
    const got = p.ring.since(start, 64 * 1024);
    const text = decodeOut(got.data, raw);
    return { got, text };
  };
  if (!timeout) return { written: data.length, cursor: p.ring.end };
  const t0 = Date.now();
  const match = await new Promise<RegExpExecArray | null>((resolve) => {
    const check = () => {
      if (re) {
        const m = re.exec(result().text);
        if (m) return done(m);
      }
      if (p.fd == null) done(null);
    };
    const timer = setTimeout(() => done(null), timeout);
    const done = (m: RegExpExecArray | null) => {
      clearTimeout(timer);
      p.waiters.delete(check);
      resolve(m);
    };
    p.waiters.add(check);
    check();
  });
  const { got, text } = result();
  return { written: data.length, matched: re ? !!match : undefined, match: match ? [...match] : undefined, elapsed_ms: Date.now() - t0, output: text, bytes: got.data.length, cursor: got.to };
}

async function readSince(p: Port, a: any) {
  const max = Math.max(1, Math.min(256 * 1024, Number(a.max_bytes) || 16 * 1024));
  // No cursor: the most recent max bytes.
  let cursor = a.cursor == null || a.cursor === "" ? Math.max(p.ring.start, p.ring.end - max) : Number(a.cursor);
  if (!Number.isFinite(cursor)) throw new HttpError(400, "cursor must be a number");
  const waitMs = Math.max(0, Math.min(MAX_WAIT_MS, Number(a.wait_ms) || 0));
  if (waitMs && p.ring.end <= cursor && p.fd != null) {
    await new Promise<void>((resolve) => {
      const w = () => {
        if (p.ring.end > cursor || p.fd == null) fin();
      };
      const timer = setTimeout(() => fin(), waitMs);
      const fin = () => {
        clearTimeout(timer);
        p.waiters.delete(w);
        resolve();
      };
      p.waiters.add(w);
    });
  }
  const got = p.ring.since(cursor, max);
  const fmt = a.format || "text";
  const data = fmt === "hex" ? Buffer.from(got.data).toString("hex").replace(/(..)(?!$)/g, "$1 ") : fmt === "base64" ? Buffer.from(got.data).toString("base64") : decodeOut(got.data, !!a.raw);
  return { id: p.id, from: got.from, cursor: got.to, more: got.to < p.ring.end, truncated: got.truncated, bytes: got.data.length, open: p.fd != null, format: fmt, data };
}

// ---------- WebSocket ----------

const WS_DOC = [
  "Live serial port. Binary frames are raw bytes both ways: what you send is written to the port, what you receive was read from it.",
  "Text frames from the server are JSON: {type:'hello', port, cursor}, {type:'history', chunks:[[t, base64]...]}, {type:'status', port}, {type:'lines', lines:{dtr,rts,cts,dsr,dcd,ri}}, {type:'flash', job, line?}, {type:'error', message}, {type:'result', for, ...}.",
  "Text frames to the server that are a JSON object with a `type` are control: {type:'write', text|hex|base64, eol?}, {type:'config', baud?, dataBits?, parity?, stopBits?, flow?, save?}, {type:'lines', dtr?, rts?}, {type:'break', ms?}, {type:'reset', mode:'esp'|'esp-boot'|'dtr'|'touch1200'}, {type:'open'}, {type:'close'}, {type:'lock', owner?}, {type:'unlock'}, {type:'status'}. Any other text frame is written to the port as UTF-8 (handy with websocat).",
  "Query: history=<bytes> of recent rx to replay first (default 65536; 0 for none), ts=1 to get history as JSON with timestamps instead of one binary frame, quiet=1 for no JSON at all (pure byte pipe), open=1 to open the port for as long as this connection lasts.",
].join(" ");

const sockets = [
  {
    path: "/ws/serial/:id",
    doc: WS_DOC,
    open(ws: ServerWebSocket<WsData>) {
      const p = ports.get(ws.data.params.id);
      if (!p) {
        ws.send(JSON.stringify({ type: "error", message: `No serial port "${ws.data.params.id}"` }));
        ws.close(4004, "no such port");
        return;
      }
      const q = ws.data.query;
      const quiet = q.get("quiet") === "1";
      ws.data.state = { id: p.id, quiet, lease: false, token: "" };
      p.ws.add(ws);
      if (!quiet) ws.send(JSON.stringify({ type: "hello", port: portStatus(p), cursor: p.ring.end }));
      const hist = Math.max(0, Math.min(p.ring.cap, Number(q.get("history") ?? 65536) || 0));
      if (hist) {
        const chunks = p.ring.tail(hist);
        if (q.get("ts") === "1" && !quiet) ws.send(JSON.stringify({ type: "history", chunks: chunks.map((c) => [c.t, Buffer.from(c.data).toString("base64")]) }));
        else if (chunks.length) ws.sendBinary(Buffer.concat(chunks.map((c) => c.data)));
      }
      if (q.get("open") === "1") {
        ws.data.state.lease = true;
        p.leases++;
        ensureOpen(p).catch((e) => ws.send(JSON.stringify({ type: "error", message: e.message })));
      }
      ctx.changed();
    },
    message(ws: ServerWebSocket<WsData>, msg: string | Buffer) {
      const p = ports.get(ws.data.state.id);
      if (!p) return;
      const fail = (e: any) => ws.send(JSON.stringify({ type: "error", message: e?.message || String(e) }));
      if (typeof msg !== "string") {
        try {
          write(p, new Uint8Array(msg), ws.data.state.token);
        } catch (e) {
          fail(e);
        }
        return;
      }
      let m: any = null;
      try {
        const j = JSON.parse(msg);
        if (j && typeof j === "object" && !Array.isArray(j) && typeof j.type === "string") m = j;
      } catch {}
      if (!m) {
        try {
          write(p, new TextEncoder().encode(msg), ws.data.state.token);
        } catch (e) {
          fail(e);
        }
        return;
      }
      const ok = (extra: any = {}) => ws.send(JSON.stringify({ type: "result", for: m.type, ok: true, ...extra }));
      (async () => {
        switch (m.type) {
          case "write":
            write(p, payload(m), ws.data.state.token);
            break;
          case "config":
            ok({ config: applyConfig(p, m, m.save !== false) });
            break;
          case "lines":
            setLines(p, { dtr: m.dtr, rts: m.rts });
            break;
          case "break":
            await sendBreak(p, Number(m.ms) || 250);
            break;
          case "reset":
            await reset(p, m.mode);
            ok();
            break;
          case "open":
            p.want = true;
            await ensureOpen(p);
            break;
          case "close":
            p.want = false;
            closeFd(p, "closed");
            break;
          case "lock": {
            if (p.lock && p.lock.token !== ws.data.state.token) throw new HttpError(423, `Locked by ${p.lock.owner}`);
            const token = crypto.randomUUID();
            p.lock = { owner: String(m.owner || "a dashboard tab"), token, since: Date.now() };
            ws.data.state.token = token;
            notify(p);
            break;
          }
          case "unlock":
            if (p.lock && (p.lock.token === ws.data.state.token || m.force)) p.lock = null;
            notify(p);
            break;
          case "status":
            ws.send(JSON.stringify({ type: "status", port: portStatus(p) }));
            break;
          default:
            throw new Error(`Unknown message type "${m.type}"`);
        }
      })().catch(fail);
    },
    close(ws: ServerWebSocket<WsData>) {
      const p = ports.get(ws.data.state?.id);
      if (!p) return;
      p.ws.delete(ws);
      if (p.lock && p.lock.token === ws.data.state.token) p.lock = null;
      if (ws.data.state.lease) release(p);
      notify(p);
    },
  },
];

// ---------- Routes ----------

const CONFIG_DOC = { baud: 115200, dataBits: 8, parity: "none|even|odd|mark|space", stopBits: 1, flow: "none|rtscts|xonxoff" };

const routes = [
  {
    method: "GET" as const,
    path: "/api/serial",
    doc: "List serial ports: USB adapters, the onboard UART and manually added paths, with state, config, modem lines, bridges and flash jobs",
    handler: (req: Request) => ({ ports: listPorts(reqHost(req)) }),
  },
  {
    method: "POST" as const,
    path: "/api/serial/manual",
    doc: { body: { path: "/dev/pts/3", remove: false }, description: "Add (or remove) a tty that udev doesn't report as USB serial: a pty, socat link, gadget port. Persisted." },
    handler: async (req: Request) => {
      const { path, remove } = await body(req);
      if (!path || !String(path).startsWith("/dev/")) throw new HttpError(400, "path must be a /dev/ node");
      const list = ctx.settings.get<string[]>("manual", []).filter((x) => x !== path);
      if (!remove) {
        if (!existsSync(path)) throw new HttpError(404, `${path} doesn't exist`);
        list.push(path);
      }
      ctx.settings.set("manual", list);
      if (remove) {
        const p = ports.get(manualId(path));
        if (p) {
          p.want = false;
          closeFd(p, "closed");
          for (const k of ["tcp", "rfc2217"] as const) stopBridge(p, k);
          ports.delete(p.id);
        }
      }
      rescan();
      return { ok: true, id: manualId(path) };
    },
  },
  {
    method: "GET" as const,
    path: "/api/serial/:id",
    doc: "One port's status",
    handler: (req: Request, a: any) => portStatus(getPort(a.id), reqHost(req)),
  },
  {
    method: "POST" as const,
    path: "/api/serial/:id/open",
    doc: { body: { ...CONFIG_DOC, "(all optional)": true }, description: "Open the port (DeskyPi holds it and fans data out to every client); an optional config is applied and saved as the default" },
    handler: async (req: Request, a: any) => {
      const p = getPort(a.id);
      const b = await body(req);
      if (Object.keys(b).length) applyConfig(p, b, true);
      p.want = true;
      await ensureOpen(p);
      return portStatus(p, reqHost(req));
    },
  },
  {
    method: "POST" as const,
    path: "/api/serial/:id/close",
    doc: "Close the port (bridges stay listening and reopen it when a client connects)",
    handler: (req: Request, a: any) => {
      const p = getPort(a.id);
      p.want = false;
      p.leases = [...p.ws].filter((w) => w.data.state.lease).length + p.bridges.tcp.clients.size + p.bridges.rfc2217.clients.size;
      if (!p.leases) closeFd(p, "closed");
      return portStatus(p, reqHost(req));
    },
  },
  {
    method: "POST" as const,
    path: "/api/serial/:id/config",
    doc: { body: { ...CONFIG_DOC, save: true }, description: "Change line settings (any baud rate). Applied live if open; save (default true) makes it the port's default" },
    handler: async (req: Request, a: any) => {
      const p = getPort(a.id);
      const b = await body(req);
      return { ok: true, config: applyConfig(p, b, b.save !== false) };
    },
  },
  {
    method: "POST" as const,
    path: "/api/serial/:id/settings",
    doc: { body: { openOnPlug: true }, description: "Per-port options: open automatically when plugged in (and at startup)" },
    handler: async (req: Request, a: any) => {
      const p = getPort(a.id);
      const b = await body(req);
      if (b.openOnPlug !== undefined) {
        savePortSettings(p.id, { openOnPlug: !!b.openOnPlug });
        if (b.openOnPlug && p.present) await ensureOpen(p).catch(() => {});
      }
      notify(p);
      return portStatus(p, reqHost(req));
    },
  },
  {
    method: "POST" as const,
    path: "/api/serial/:id/lines",
    doc: { body: { dtr: true, rts: false }, description: "Assert (true) or drop (false) DTR/RTS" },
    handler: async (req: Request, a: any) => {
      const p = getPort(a.id);
      const b = await body(req);
      setLines(p, { dtr: b.dtr, rts: b.rts });
      return { ok: true, lines: p.lines };
    },
  },
  {
    method: "POST" as const,
    path: "/api/serial/:id/break",
    doc: { body: { ms: 250 }, description: "Send a break (10-5000 ms)" },
    handler: async (req: Request, a: any) => {
      await sendBreak(getPort(a.id), Number((await body(req)).ms) || 250);
      return { ok: true };
    },
  },
  {
    method: "POST" as const,
    path: "/api/serial/:id/reset",
    doc: { body: { mode: "esp | esp-boot | dtr | touch1200" }, description: "Reset the target over DTR/RTS: esp (run), esp-boot (ROM bootloader; classic two-transistor circuit or ESP USB-JTAG sequence), dtr (Arduino auto-reset pulse), touch1200 (open at 1200 baud and close: native-USB Arduino / Pico bootloader)" },
    handler: async (req: Request, a: any) => {
      await reset(getPort(a.id), (await body(req)).mode);
      return { ok: true };
    },
  },
  {
    method: "POST" as const,
    path: "/api/serial/:id/write",
    doc: { body: { text: "help", hex: "(or) de ad be ef", base64: "(or)", eol: "none|cr|lf|crlf", wait: "regex, optional", flags: "m", timeout_ms: 3000, raw: false, token: "(lock token, if you hold the lock)" }, description: "Write to the port (opens it if closed). With wait, returns the output that arrived after the write up to the first regex match or timeout_ms (max 60000). raw keeps ANSI escapes." },
    handler: async (req: Request, a: any) => writeAndWait(getPort(a.id), await body(req)),
  },
  {
    method: "GET" as const,
    path: "/api/serial/:id/read",
    doc: "Received data from the ring buffer (~256 KB). ?cursor=N (absolute byte offset; omit for the latest), max_bytes, wait_ms (long-poll up to 60000), format=text|hex|base64|raw (raw returns the bytes; add download=1 for a log file)",
    handler: async (req: Request, a: any, url: URL) => {
      const p = getPort(a.id);
      const q = Object.fromEntries(url.searchParams);
      if (q.format === "raw") {
        const got = p.ring.since(q.cursor ? Number(q.cursor) : p.ring.start, Number(q.max_bytes) || Infinity);
        const headers: Record<string, string> = { "Content-Type": "application/octet-stream", "X-Serial-Cursor": String(got.to), "X-Serial-From": String(got.from) };
        if (q.download) headers["Content-Disposition"] = `attachment; filename="serial-${p.id}-${new Date().toISOString().replace(/[:.]/g, "-")}.log"`;
        return new Response(got.data, { headers });
      }
      return readSince(p, q);
    },
  },
  {
    method: "POST" as const,
    path: "/api/serial/:id/autobaud",
    doc: { body: { rates: [9600, 115200], ms: 400, apply: false }, description: "Guess the baud rate by listening only (never transmits): ~400 ms per rate, scored on printable/line-structured output. apply sets the best rate when confidence >= 0.5" },
    handler: async (req: Request, a: any) => autobaud(getPort(a.id), await body(req)),
  },
  {
    method: "POST" as const,
    path: "/api/serial/:id/bridge",
    doc: { body: { kind: "tcp | rfc2217", enabled: true, port: "(optional, persisted)" }, description: "Turn a network bridge on or off (persisted). Raw TCP: plain bytes. RFC 2217: Telnet COM-PORT-OPTION, so remote clients (pyserial rfc2217://, esptool) can change baud and lines. Default ports 4000+slot and 4100+slot, slot fixed per device" },
    handler: async (req: Request, a: any) => {
      const b = await body(req);
      const p = getPort(a.id);
      setBridge(p, b.kind, !!b.enabled, b.port);
      return bridgeStatus(p, b.kind, reqHost(req));
    },
  },
  {
    method: "POST" as const,
    path: "/api/serial/:id/lock",
    doc: { body: { owner: "my-script" }, description: "Reserve writing for the caller; returns a token to pass to /write. Other writers get 423 until unlocked" },
    handler: async (req: Request, a: any) => {
      const p = getPort(a.id);
      const b = await body(req);
      if (p.lock) throw new HttpError(423, `Already locked by ${p.lock.owner}`);
      p.lock = { owner: String(b.owner || "an API client"), token: crypto.randomUUID(), since: Date.now() };
      notify(p);
      return { ok: true, token: p.lock.token };
    },
  },
  {
    method: "POST" as const,
    path: "/api/serial/:id/unlock",
    doc: { body: { token: "...", force: false }, description: "Release the write lock (force breaks someone else's)" },
    handler: async (req: Request, a: any) => {
      const p = getPort(a.id);
      const b = await body(req);
      if (p.lock && p.lock.token !== b.token && !b.force) throw new HttpError(423, `Locked by ${p.lock.owner}; pass its token or force`);
      p.lock = null;
      notify(p);
      return { ok: true };
    },
  },
  {
    method: "POST" as const,
    path: "/api/serial/:id/flash",
    doc: {
      query: { tool: "esptool | avrdude", action: "write | erase | info", offset: "0x10000 (raw body)", chip: "auto", baud: 460800, preset: "uno | nano | nano-old | mega" },
      body: "Raw firmware bytes; or multipart with file fields plus offset fields in the same order (or file fields named by offset, e.g. 0x1000); or JSON {tool, action, files:[{offset, url | base64, name}]}",
      description: "Flash a board through the port. DeskyPi releases the port, runs the tool, streams progress (status + WebSocket 'flash' messages), then reopens. Returns at once with the job; poll GET .../flash",
    },
    handler: async (req: Request, a: any, url: URL) => {
      const p = getPort(a.id);
      return { ok: true, job: jobView(await startFlash(p, await flashRequest(req, url))) };
    },
  },
  {
    method: "GET" as const,
    path: "/api/serial/:id/flash",
    doc: "Current or last flash job, with the full tool log",
    handler: (req: Request, a: any) => {
      const p = getPort(a.id);
      return { job: jobView(p.flash), log: p.flash?.log || [] };
    },
  },
  {
    method: "POST" as const,
    path: "/api/serial/:id/flash/cancel",
    doc: "Stop a running flash job",
    handler: (req: Request, a: any) => {
      const j = getPort(a.id).flash;
      if (!j || (j.state !== "running" && j.state !== "preparing")) throw new HttpError(409, "Nothing is flashing");
      // While still downloading there's no process yet; runFlash checks the state before spawning.
      j.state = "cancelled";
      j.proc?.kill();
      return { ok: true };
    },
  },
];

// ---------- MCP tools ----------

const idProp = { id: { type: "string", description: "Port id from serial_list" } };

const tools = [
  {
    name: "serial_list",
    description: "List serial ports on DeskyPi (USB-serial adapters, dev boards, the onboard UART) with id, device path, chip/board name, open state, line settings, modem lines, bridge ports and any flash job. Call this first to get port ids.",
    inputSchema: { type: "object", properties: {} },
    handler: () => ({ ports: listPorts() }),
  },
  {
    name: "serial_configure",
    description: "Open, close or reconfigure a serial port: baud (any rate), data bits, parity, stop bits, flow control; set DTR/RTS; send a break; or reset the target (esp, esp-boot, dtr, touch1200). Settings are saved as the port's default unless save=false.",
    inputSchema: {
      type: "object",
      properties: {
        ...idProp,
        open: { type: "boolean", description: "true opens (and keeps open), false closes" },
        baud: { type: "integer" },
        dataBits: { type: "integer", enum: [5, 6, 7, 8] },
        parity: { type: "string", enum: ["none", "even", "odd", "mark", "space"] },
        stopBits: { type: "integer", enum: [1, 2] },
        flow: { type: "string", enum: ["none", "rtscts", "xonxoff"] },
        save: { type: "boolean", default: true },
        dtr: { type: "boolean" },
        rts: { type: "boolean" },
        break_ms: { type: "integer", description: "Send a break of this length" },
        reset: { type: "string", enum: ["esp", "esp-boot", "dtr", "touch1200"] },
        open_on_plug: { type: "boolean" },
      },
      required: ["id"],
    },
    handler: async (a: any) => {
      const p = getPort(a.id);
      const cfg = Object.fromEntries(["baud", "dataBits", "parity", "stopBits", "flow"].filter((k) => a[k] !== undefined).map((k) => [k, a[k]]));
      if (Object.keys(cfg).length) applyConfig(p, cfg, a.save !== false);
      if (a.open === true) (p.want = true), await ensureOpen(p);
      if (a.open === false) (p.want = false), closeFd(p, "closed");
      if (a.open_on_plug !== undefined) savePortSettings(p.id, { openOnPlug: !!a.open_on_plug });
      if (a.dtr !== undefined || a.rts !== undefined) {
        if (p.fd == null) (p.want = true), await ensureOpen(p);
        setLines(p, { dtr: a.dtr, rts: a.rts });
      }
      if (a.break_ms) await sendBreak(p, a.break_ms);
      if (a.reset) await reset(p, a.reset);
      return portStatus(p);
    },
  },
  {
    name: "serial_write",
    description:
      "Write to a serial port and optionally wait for a reply. Opens the port if needed (and leaves it open). Give text (use eol to append a line ending) or hex. With wait (a JavaScript regex), returns the output received after the write up to the first match, or whatever arrived by timeout_ms (max 60000). Without wait, timeout_ms just collects output for that long. ANSI escapes are stripped unless raw. Example: {id, text:'help', eol:'crlf', wait:'\\\\n> ?$'}.",
    inputSchema: {
      type: "object",
      properties: {
        ...idProp,
        text: { type: "string" },
        hex: { type: "string", description: "Bytes as hex, e.g. 'de ad be ef'" },
        eol: { type: "string", enum: ["none", "cr", "lf", "crlf"], default: "none" },
        wait: { type: "string", description: "Regex to wait for in the output" },
        flags: { type: "string", default: "m" },
        timeout_ms: { type: "integer", default: 3000, maximum: 60000 },
        raw: { type: "boolean", default: false },
      },
      required: ["id"],
    },
    handler: (a: any) => writeAndWait(getPort(a.id), a),
  },
  {
    name: "serial_read",
    description: "Read received serial data from DeskyPi's ring buffer (~256 KB of history, kept while the port is open). Pass the cursor from a previous call to get only newer data; omit it for the most recent output. wait_ms long-polls for new data. Returns {data, cursor, more, truncated}.",
    inputSchema: {
      type: "object",
      properties: {
        ...idProp,
        cursor: { type: "integer" },
        max_bytes: { type: "integer", default: 16384 },
        wait_ms: { type: "integer", maximum: 60000 },
        format: { type: "string", enum: ["text", "hex", "base64"], default: "text" },
        raw: { type: "boolean", default: false },
      },
      required: ["id"],
    },
    handler: (a: any) => readSince(getPort(a.id), a),
  },
  {
    name: "serial_autobaud",
    description: "Guess a port's baud rate by listening only (never transmits): tries common rates ~400 ms each and scores how much the data looks like text. The target must be sending something (reset it first if it only talks at boot). apply=true sets the winner when confidence >= 0.5.",
    inputSchema: { type: "object", properties: { ...idProp, rates: { type: "array", items: { type: "integer" } }, ms: { type: "integer" }, apply: { type: "boolean" } }, required: ["id"] },
    handler: (a: any) => autobaud(getPort(a.id), a),
  },
  {
    name: "serial_bridge",
    description: "Turn a port's network bridge on or off. kind=tcp is a raw byte socket (nc host port). kind=rfc2217 is RFC 2217 (pyserial rfc2217://host:port, esptool --port rfc2217://..., remote baud and DTR/RTS changes). Returns the port number and client commands.",
    inputSchema: { type: "object", properties: { ...idProp, kind: { type: "string", enum: ["tcp", "rfc2217"] }, enabled: { type: "boolean" }, port: { type: "integer" } }, required: ["id", "kind", "enabled"] },
    handler: (a: any) => setBridge(getPort(a.id), a.kind, a.enabled, a.port),
  },
  {
    name: "serial_flash",
    description:
      "Flash a board through its serial port. tool=esptool (ESP8266/ESP32 family): action write (files at offsets, e.g. bootloader 0x1000 / partitions 0x8000 / app 0x10000 for ESP32, or one merged image at 0x0), erase, or info (chip, MAC, flash size). tool=avrdude with preset uno | nano | nano-old | mega: write (.hex or .bin) or info. Files come from url or base64. Returns at once; poll serial_flash_status.",
    inputSchema: {
      type: "object",
      properties: {
        ...idProp,
        tool: { type: "string", enum: ["esptool", "avrdude"] },
        action: { type: "string", enum: ["write", "erase", "info"] },
        files: { type: "array", items: { type: "object", properties: { offset: { type: ["integer", "string"], description: "e.g. 65536 or '0x10000'" }, url: { type: "string" }, base64: { type: "string" }, name: { type: "string" } } } },
        chip: { type: "string", enum: ESP_CHIPS },
        baud: { type: "integer" },
        preset: { type: "string", enum: Object.keys(AVR_PRESETS) },
      },
      required: ["id", "tool", "action"],
    },
    handler: async (a: any) => {
      const sources = (a.files || []).map((f: any) => {
        if (!f.url && !f.base64) throw new HttpError(400, "Each file needs url or base64");
        return { offset: parseOffset(f.offset, 0), name: f.name || (f.url ? String(f.url).split("/").pop()!.split("?")[0] : "firmware.bin"), url: f.url, data: f.base64 ? Uint8Array.from(Buffer.from(f.base64, "base64")) : undefined };
      });
      return { job: jobView(await startFlash(getPort(a.id), { tool: a.tool, action: a.action, chip: a.chip, baud: a.baud, preset: a.preset, sources })) };
    },
  },
  {
    name: "serial_flash_status",
    description: "Progress and log of the current or last flash job on a port (state preparing/running/done/error/cancelled, percent, phase, chip info, error message).",
    inputSchema: { type: "object", properties: { ...idProp, log_lines: { type: "integer", default: 40 } }, required: ["id"] },
    handler: (a: any) => {
      const p = getPort(a.id);
      return { job: jobView(p.flash), log: (p.flash?.log || []).slice(-(a.log_lines || 40)) };
    },
  },
];

// ---------- Driver ----------

let rescanTimer: ReturnType<typeof setTimeout> | null = null;

const driver: Driver = {
  id: "serial",
  title: "Serial",
  order: 20,
  description: "USB-serial adapters and the onboard UART: browser terminal, auto-baud, RFC 2217/TCP/WebSocket bridges, ESP and AVR flashing",

  async init(c) {
    ctx = c;
    cmdline = await readText("/boot/firmware/cmdline.txt");
    mkdirSync(FLASH_DIR, { recursive: true });
    rescan();
  },

  async stop() {
    for (const p of ports.values()) {
      for (const k of ["tcp", "rfc2217"] as const) stopBridge(p, k);
      for (const ws of p.ws) ws.close(1001, "serial driver stopped");
      p.flash?.proc?.kill();
      closeFd(p, "closed");
    }
    ports.clear();
  },

  udev(ev: UdevEvent) {
    if (ev.props.SUBSYSTEM !== "tty") return;
    if (rescanTimer) clearTimeout(rescanTimer);
    rescanTimer = setTimeout(() => ((rescanTimer = null), rescan()), 200);
  },

  status() {
    const list = [...ports.values()];
    const shown = list.filter((p) => p.present && p.kind !== "onboard");
    const open = list.filter((p) => p.fd != null);
    const flashing = list.find((p) => p.state === "flashing");
    const summary = flashing
      ? `Flashing ${flashing.name} ${Math.round(flashing.flash?.progress || 0)}%`
      : open.length === 1
        ? `${open[0].name} open · ${T.configLabel(open[0].cfg)}`
        : shown.length || open.length
          ? `${shown.length} port${shown.length === 1 ? "" : "s"}${open.length ? ` · ${open.length} open` : ""}`
          : "";
    return { active: shown.length > 0 || open.length > 0, summary, offset: OFFSET, ports: listPorts() };
  },

  routes,
  sockets,
  tools,
};

export default driver;
