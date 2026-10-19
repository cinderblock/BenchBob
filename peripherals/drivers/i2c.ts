// ==================== I²C ====================
//
// User I²C buses (the header's i2c-1, USB adapters, i2c-stub), an i2cdetect-style scan,
// identification of what answered, live decoded readings, and raw register tools.
// Chip knowledge lives in peripherals/lib/i2c-chips.ts; bus access (FFI ioctls) in
// peripherals/lib/i2c-dev.ts.
//
// Write policy: nothing here writes to a device unless a person or agent asked for it.
// Auto-polling only calls the read-only decoders; Measure and the per-device auto-measure
// opt-in are the only paths that may start conversions or wake chips, and raw writes
// require `confirm: true` over REST. Scans use i2cdetect's own safe defaults.

import { readdir, readlink, realpath } from "node:fs/promises";
import { body, HttpError, readText, type Ctx, type Driver, type Route, type Tool, type UdevEvent } from "../core.ts";
import { funcNames, hex, I2cError, LinuxI2cBus, type I2cIo, type Msg } from "../lib/i2c-dev.ts";
import { addressHints, CHIP_BY_ID, crossHints, Dev, identify, type ChipDef, type ChipState, type Readout } from "../lib/i2c-chips.ts";

const SYS_DEV = "/sys/class/i2c-dev";
const HIST_N = 60;
const SCAN_FIRST = 0x08;
const SCAN_LAST = 0x77;

/** Chips where register pointers are commands (a dump or blind register read would execute them). */
const POINTER_UNSAFE = new Set(["expander", "bh1750", "aht2x", "sht3x", "sht4x", "htu21d", "scd4x", "mcp4725", "tca9548", "ssd1306", "ds2482"]);

interface DevState {
  addr: number;
  claimed: string | null; // kernel driver bound to it (i2cdetect "UU")
  chip: ChipDef | null;
  cs: ChipState | null;
  readout: Readout | null;
  readAt: number;
  error: string;
  hist: Record<string, number[]>;
  busy: boolean;
  next: number;
  firstSeen: number;
  hint: string;
}

interface BusState {
  bus: number;
  name: string; // adapter name from sysfs
  label: string;
  kind: "header" | "usb" | "stub" | "ddc" | "other";
  key: string; // stable id for settings
  sysPath: string;
  hz: number | null; // clock-frequency from the device tree (header controllers)
  io: I2cIo | null;
  funcs: number;
  error: string;
  scanning: boolean;
  lastScan: number;
  scanMs: number;
  devices: Map<number, DevState>;
  lock: Promise<any>;
  nextScan: number;
}

let ctx: Ctx;
let running = false;
const buses = new Map<number, BusState>();
let tick: ReturnType<typeof setInterval> | null = null;
let refreshTimer: ReturnType<typeof setTimeout> | null = null;

// ---------- helpers ----------

const errMsg = (e: any) => (e instanceof I2cError ? e.message : e?.message || String(e));

/** Serialise everything that touches one bus: multi-step sequences sleep between steps. */
function withBus<T>(b: BusState, f: () => Promise<T> | T): Promise<T> {
  const p = b.lock.then(f, f);
  b.lock = p.catch(() => {});
  return p;
}

function parseBus(s: any): BusState {
  const n = parseInt(String(s ?? "").replace(/^i2c-/, ""), 10);
  const b = buses.get(n);
  if (!b) throw new HttpError(404, `No I²C bus "${s}". See GET /api/i2c (buses are numbered like the /dev/i2c-N nodes).`);
  if (!b.io) throw new HttpError(409, `i2c-${n} is not usable: ${b.error || "not open"}`);
  return b;
}

function parseAddr(s: any): number {
  const n = typeof s === "number" ? s : /^0x/i.test(String(s)) ? parseInt(String(s), 16) : parseInt(String(s), 10);
  if (!Number.isInteger(n) || n < 0x03 || n > 0x77) throw new HttpError(400, `Address must be 0x03–0x77 (got ${s})`);
  return n;
}

function byteArr(v: any, what: string): number[] {
  const a = Array.isArray(v) ? v : typeof v === "string" ? v.trim().split(/[\s,]+/).filter(Boolean) : null;
  if (!a) throw new HttpError(400, `${what} must be an array of bytes or a string like "0x00 0x1f"`);
  return a.map((x: any) => {
    const n = typeof x === "number" ? x : parseInt(String(x), /^0x/i.test(String(x)) ? 16 : 10);
    if (!Number.isInteger(n) || n < 0 || n > 255) throw new HttpError(400, `${what}: ${x} is not a byte`);
    return n;
  });
}

function intArg(v: any, what: string, lo: number, hi: number): number {
  const n = typeof v === "number" ? v : parseInt(String(v), /^0x/i.test(String(v)) ? 16 : 10);
  if (!Number.isInteger(n) || n < lo || n > hi) throw new HttpError(400, `${what} must be ${lo}–${hi}`);
  return n;
}

function devKey(b: BusState, addr: number) {
  return `dev:${b.key}@${hex(addr)}`;
}
function devCfg(b: BusState, addr: number): { poll?: boolean; auto?: boolean; interval?: number; opts?: Record<string, any>; label?: string } {
  return ctx.settings.get(devKey(b, addr), {} as any);
}
function setDevCfg(b: BusState, addr: number, patch: Record<string, any>) {
  ctx.settings.set(devKey(b, addr), { ...devCfg(b, addr), ...patch });
}
function busCfg(b: BusState): { autoScan?: number } {
  return ctx.settings.get(`bus:${b.key}`, {} as any);
}
const autoScanSec = (b: BusState) => busCfg(b).autoScan ?? (b.kind === "ddc" ? 0 : 30);

function chipName(d: DevState) {
  return d.cs?.probe.variant || d.chip?.name || "";
}

function pollable(d: DevState) {
  return !!(d.chip?.read && d.chip.poll !== false);
}

// ---------- bus discovery ----------

/** Classify an adapter from its sysfs name and device path. */
export function classify(name: string, path: string, compatible: string): { kind: BusState["kind"]; label: string } {
  if (/hdmi-i2c|brcmstb-i2c/.test(compatible) || /ddc|hdmi|gmbus|dpddc/i.test(name)) return { kind: "ddc", label: "HDMI DDC (display EDID)" };
  if (/i2c@7e804000/.test(name)) return { kind: "header", label: "Header pins 3/5 (GPIO2/3)" };
  // Pi 4 extra controllers (dtoverlay=i2c3…i2c6); which pin pair is in config.txt (GPIO → Interfaces).
  const extra = /i2c@7e205([68ac])00/.exec(name);
  if (extra) {
    const n = ({ "6": 3, "8": 4, a: 5, c: 6 } as Record<string, number>)[extra[1]];
    return { kind: "header", label: `Header I²C${n} (dtoverlay=i2c${n})` };
  }
  if (/(^|\s)i2c-gpio(\s|$)/.test(compatible)) return { kind: "header", label: "Header software I²C (i2c-gpio)" };
  if (/i2c@7e205000/.test(name)) return { kind: "other", label: "i2c0: GPIO0/1 (HAT ID EEPROM, header pins 27/28) or camera" };
  if (/i2c0mux|i2c@7e805000/.test(name)) return { kind: "other", label: "Camera / display (CSI/DSI)" };
  if (/SMBus stub/i.test(name)) return { kind: "stub", label: "i2c-stub (simulated chips)" };
  if (/CP2112/i.test(name)) return { kind: "usb", label: "USB: Silicon Labs CP2112" };
  if (/MCP2221/i.test(name)) return { kind: "usb", label: "USB: Microchip MCP2221" };
  if (/tiny-usb/i.test(name)) return { kind: "usb", label: "USB: i2c-tiny-usb" };
  if (/CH341/i.test(name)) return { kind: "usb", label: "USB: WCH CH341" };
  if (/\/usb\d/.test(path)) return { kind: "usb", label: `USB: ${name}` };
  return { kind: "other", label: name };
}

async function refreshBuses() {
  let names: string[] = [];
  try {
    names = await readdir(SYS_DEV);
  } catch {}
  const seen = new Set<number>();
  for (const n of names) {
    const m = /^i2c-(\d+)$/.exec(n);
    if (!m) continue;
    const bus = +m[1];
    seen.add(bus);
    if (buses.has(bus)) continue;
    const name = await readText(`${SYS_DEV}/${n}/name`);
    const sysPath = await realpath(`${SYS_DEV}/${n}/device`).catch(() => "");
    const compatible = (await readText(`${SYS_DEV}/${n}/device/of_node/compatible`)).replace(/\0/g, " ");
    const { kind, label } = classify(name, sysPath, compatible);
    const clk = new Uint8Array(await Bun.file(`${SYS_DEV}/${n}/device/of_node/clock-frequency`).arrayBuffer().catch(() => new ArrayBuffer(0)));
    const hz = clk.length >= 4 ? new DataView(clk.buffer).getUint32(0) : null;
    // Stable settings key: USB serial / port path, else the platform device, else the name.
    const udevPath = sysPath.replace(/^\/sys/, "") + `/i2c-dev/${n}`;
    const usb = ctx.ancestor(udevPath, "usb", "usb_device");
    const key = kind === "stub" ? "stub" : usb ? `usb:${usb.ID_SERIAL || usb.ID_PATH || usb.DEVPATH}` : sysPath.split("/").slice(-2, -1)[0] || name;
    const b: BusState = { bus, name, label, kind, key, sysPath, hz, io: null, funcs: 0, error: "", scanning: false, lastScan: 0, scanMs: 0, devices: new Map(), lock: Promise.resolve(), nextScan: 0 };
    try {
      b.io = new LinuxI2cBus(bus);
      b.funcs = b.io.funcs;
    } catch (e) {
      b.error = errMsg(e);
    }
    buses.set(bus, b);
    if (running && kind !== "ddc") ctx.event(`I²C bus i2c-${bus} appeared: ${label}`);
    if (kind !== "ddc" && b.io) b.nextScan = Date.now(); // scan on the next tick
  }
  for (const [bus, b] of buses) {
    if (seen.has(bus)) continue;
    b.io?.close();
    buses.delete(bus);
    if (b.kind !== "ddc") ctx.event(`I²C bus i2c-${bus} went away (${b.label})`);
  }
  ctx.changed();
}

// ---------- scan + identify ----------

/** Kernel driver bound to bus-addr, if any (i2cdetect's "UU"). */
async function boundDriver(bus: number, addr: number): Promise<string | null> {
  const base = `/sys/bus/i2c/devices/${bus}-${addr.toString(16).padStart(4, "0")}`;
  try {
    return (await readlink(`${base}/driver`)).split("/").pop() || "bound";
  } catch {
    return null;
  }
}

/** i2cdetect's auto mode: read-byte where quick-write can corrupt EEPROMs, quick-write elsewhere.
 * Like i2cdetect, any error means "nobody there": adapters disagree on the errno for a
 * missing ACK (bcm2835: EREMOTEIO for quick, EIO for read-byte; i2c-stub: ENODEV). Only
 * EBUSY (kernel driver) and timeouts / lost arbitration (a stuck bus) are passed up. */
function probeAddr(io: I2cIo, addr: number): boolean {
  const readFirst = (addr >= 0x30 && addr <= 0x37) || (addr >= 0x50 && addr <= 0x5f);
  const canQuick = !!(io.funcs & 0x10000);
  const canRecv = !!(io.funcs & 0x20000);
  try {
    if ((readFirst && canRecv) || !canQuick) io.receive(addr);
    else io.quick(addr, false);
    return true;
  } catch (e) {
    if (e instanceof I2cError && (e.errno === 16 || e.errno === 110 || e.errno === 11)) throw e;
    return false;
  }
}

async function scan(b: BusState, opts: { deep?: boolean; reidentify?: boolean } = {}) {
  if (!b.io || b.scanning) return;
  b.scanning = true;
  ctx.changed();
  const t0 = Date.now();
  try {
    await withBus(b, async () => {
      const present = new Map<number, string | null>();
      let faults = 0;
      for (let a = SCAN_FIRST; a <= SCAN_LAST; a++) {
        const drv = await boundDriver(b.bus, a);
        if (drv) {
          present.set(a, drv);
          continue;
        }
        try {
          if (probeAddr(b.io!, a)) present.set(a, null);
        } catch (e) {
          if (e instanceof I2cError && e.errno === 16) present.set(a, "kernel");
          else if (++faults >= 3) throw new Error(`bus fault: ${errMsg(e)} (is SDA or SCL held low?)`);
        }
      }
      // Departures
      for (const [a, d] of b.devices) {
        if (present.has(a)) continue;
        b.devices.delete(a);
        ctx.event(`I²C i2c-${b.bus} ${hex(a)}: ${chipName(d) || "device"} no longer answers`);
      }
      // Arrivals and re-identification, in ascending address order so cross-address probes
      // (PCA9685 All Call) can see what's below them.
      const found = new Map<number, string>();
      for (const [a, drv] of [...present].sort((x, y) => x[0] - y[0])) {
        let d = b.devices.get(a);
        const fresh = !d;
        if (!d) {
          d = { addr: a, claimed: drv, chip: null, cs: null, readout: null, readAt: 0, error: "", hist: {}, busy: false, next: 0, firstSeen: Date.now(), hint: "" };
          b.devices.set(a, d);
        }
        // A driver released the address (e.g. ds2482 unbound): identify what's really there.
        const released = !!d.claimed && !drv;
        d.claimed = drv;
        if (!drv && (fresh || released || opts.reidentify || opts.deep)) {
          const id = await identify(b.io!, a, { found, deep: opts.deep });
          d.chip = id.chip;
          d.cs = id.probe ? { probe: id.probe, opts: devCfg(b, a).opts || {} } : null;
          d.readout = null;
          d.hist = {};
          d.next = 0;
        }
        if (d.chip) found.set(a, d.chip.id);
        if (fresh && b.lastScan) ctx.event(`I²C i2c-${b.bus} ${hex(a)}: ${drv ? `claimed by ${drv}` : chipName(d) || "unknown device"} appeared`);
      }
      const hints = crossHints(found);
      for (const d of b.devices.values()) d.hint = hints.get(d.addr) || "";
    });
    b.error = "";
  } catch (e) {
    b.error = errMsg(e);
  } finally {
    b.scanning = false;
    b.lastScan = Date.now();
    b.scanMs = Date.now() - t0;
    const s = autoScanSec(b);
    b.nextScan = s > 0 ? Date.now() + s * 1000 : Infinity;
    ctx.changed();
  }
}

// ---------- reading ----------

function record(d: DevState, r: Readout) {
  d.readout = r;
  d.readAt = Date.now();
  d.error = "";
  if (r.stale) return;
  for (const v of r.values) {
    if (typeof v.v !== "number") continue;
    const h = (d.hist[v.k] ||= []);
    h.push(v.v);
    if (h.length > HIST_N) h.shift();
  }
}

async function readDev(b: BusState, d: DevState, kind: "read" | "measure"): Promise<Readout> {
  if (!d.chip || !d.cs) throw new HttpError(409, `${hex(d.addr)} isn't identified; use the raw tools`);
  const fn = kind === "read" ? d.chip.read : d.chip.measure;
  if (!fn) throw new HttpError(409, `${chipName(d)} has no ${kind === "read" ? "read-only decoder" : "measure operation"}`);
  d.cs.opts = devCfg(b, d.addr).opts || {};
  d.busy = true;
  try {
    const r = await withBus(b, () => fn.call(d.chip, new Dev(b.io!, d.addr), d.cs!));
    record(d, r);
    return r;
  } catch (e) {
    d.error = errMsg(e);
    throw e instanceof HttpError ? e : new HttpError(502, `${chipName(d)} ${hex(d.addr)}: ${errMsg(e)}`);
  } finally {
    d.busy = false;
    ctx.changed();
  }
}

/** 250 ms housekeeping: due scans and due polls. Everything is skipped while a device is busy. */
function onTick() {
  const now = Date.now();
  for (const b of buses.values()) {
    if (!b.io || b.kind === "ddc") continue;
    if (now >= b.nextScan && !b.scanning) void scan(b);
    for (const d of b.devices.values()) {
      if (d.busy || !d.chip || d.claimed || now < d.next) continue;
      const cfg = devCfg(b, d.addr);
      const auto = !!cfg.auto && !!d.chip.measure;
      const poll = (cfg.poll ?? true) && pollable(d);
      if (!auto && !poll) continue;
      const iv = Math.max(250, cfg.interval ?? 2000);
      d.next = now + iv;
      readDev(b, d, auto ? "measure" : "read").catch(() => {});
    }
  }
}

// ---------- views ----------

function devView(b: BusState, d: DevState) {
  const cfg = devCfg(b, d.addr);
  const c = d.chip;
  return {
    addr: d.addr,
    hex: hex(d.addr),
    claimed: d.claimed,
    chip: c?.id || null,
    name: chipName(d) || null,
    what: c?.what || "",
    confidence: d.cs?.probe.confidence || null,
    note: d.cs?.probe.note || "",
    hint: d.hint,
    hints: c ? [] : addressHints(d.addr),
    label: cfg.label || "",
    readout: d.readout,
    readAt: d.readAt,
    error: d.error,
    canRead: !!c?.read,
    canMeasure: !!c?.measure,
    measureNote: c?.measureNote || "",
    pollable: !!(c && pollable({ ...d })),
    poll: (cfg.poll ?? true) && !!(c && pollable(d)),
    auto: !!cfg.auto && !!c?.measure,
    interval: cfg.interval ?? 2000,
    opts: (c?.opts || []).map((o) => ({ ...o, value: cfg.opts?.[o.key] ?? o.default })),
    pointerUnsafe: !c || POINTER_UNSAFE.has(c.id),
    hist: d.hist,
    firstSeen: d.firstSeen,
  };
}

function busView(b: BusState) {
  return {
    bus: b.bus,
    dev: `/dev/i2c-${b.bus}`,
    name: b.name,
    label: b.label,
    kind: b.kind,
    hz: b.hz,
    funcs: funcNames(b.funcs),
    plainI2c: !!(b.funcs & 1),
    error: b.error,
    scanning: b.scanning,
    lastScan: b.lastScan,
    scanMs: b.scanMs,
    autoScan: autoScanSec(b),
    devices: [...b.devices.values()].sort((x, y) => x.addr - y.addr).map((d) => devView(b, d)),
  };
}

function userBuses() {
  return [...buses.values()].filter((b) => b.kind !== "ddc").sort((x, y) => x.bus - y.bus);
}

/** For the 1-Wire driver: DS2482 bridges seen on any bus, bound or not. */
export function ds2482Candidates(): { bus: number; addr: number; bound: boolean; driver: string | null }[] {
  const out = [];
  for (const b of buses.values())
    for (const d of b.devices.values()) {
      if (d.chip?.id === "ds2482" || d.claimed === "ds2482") out.push({ bus: b.bus, addr: d.addr, bound: d.claimed === "ds2482", driver: d.claimed });
    }
  return out;
}

/** For the 1-Wire driver: force a rescan of a bus (after binding/unbinding a bridge). */
export async function rescan(bus: number) {
  const b = buses.get(bus);
  if (b) await scan(b);
}

// ---------- raw operations ----------

type Size = "byte" | "word" | "block" | "raw";

async function rawRead(b: BusState, addr: number, a: any) {
  const size: Size = a.size || (a.register == null ? "raw" : "byte");
  const force = !!a.force;
  const reg = a.register == null ? null : intArg(a.register, "register", 0, 255);
  const len = a.length == null ? 1 : intArg(a.length, "length", 1, size === "raw" ? 256 : 32);
  return withBus(b, () => {
    try {
      const io = b.io!;
      if (size === "raw") {
        if (len === 1) return { bytes: [io.receive(addr, force)] };
        return { bytes: io.transfer(addr, [{ r: len }], force)[0] };
      }
      if (reg == null) throw new HttpError(400, `size "${size}" needs a register`);
      if (size === "byte") return { register: reg, bytes: [io.readByte(addr, reg, force)] };
      if (size === "word") {
        const w = io.readWord(addr, reg, force);
        return { register: reg, word: w, wordBigEndian: ((w & 0xff) << 8) | (w >> 8), bytes: [w & 0xff, w >> 8] };
      }
      if (size === "block") return { register: reg, bytes: io.readBlock(addr, reg, len, force) };
      throw new HttpError(400, `size must be byte, word, block or raw`);
    } catch (e) {
      if (e instanceof HttpError) throw e;
      throw new HttpError(502, errMsg(e));
    }
  });
}

async function rawWrite(b: BusState, addr: number, a: any) {
  const force = !!a.force;
  const reg = a.register == null ? null : intArg(a.register, "register", 0, 255);
  const size: Size = a.size || (a.bytes ? (reg == null ? "raw" : "block") : "byte");
  return withBus(b, () => {
    try {
      const io = b.io!;
      if (size === "raw") {
        const bytes = byteArr(a.bytes ?? [a.value], "bytes");
        if (bytes.length === 1 && reg == null && !(io.funcs & 1)) io.send(addr, bytes[0], force);
        else io.transfer(addr, [{ w: reg == null ? bytes : [reg, ...bytes] }], force);
        return { wrote: bytes };
      }
      if (reg == null) throw new HttpError(400, `size "${size}" needs a register`);
      if (size === "byte") {
        const v = intArg(a.value, "value", 0, 255);
        io.writeByte(addr, reg, v, force);
        return { register: reg, wrote: [v] };
      }
      if (size === "word") {
        const v = intArg(a.value, "value", 0, 0xffff);
        // `value` is the register's logical value; bigEndian (default true) puts the MSB on the wire first, as most sensors expect.
        const w = a.bigEndian === false ? v : ((v & 0xff) << 8) | (v >> 8);
        io.writeWord(addr, reg, w, force);
        return { register: reg, wrote: [w & 0xff, w >> 8] };
      }
      if (size === "block") {
        const bytes = byteArr(a.bytes, "bytes");
        if (bytes.length > 32) throw new HttpError(400, "block writes are at most 32 bytes");
        io.writeBlock(addr, reg, bytes, force);
        return { register: reg, wrote: bytes };
      }
      throw new HttpError(400, `size must be byte, word, block or raw`);
    } catch (e) {
      if (e instanceof HttpError) throw e;
      throw new HttpError(502, errMsg(e));
    }
  });
}

async function rawTransfer(b: BusState, addr: number, a: any) {
  let msgs: Msg[];
  if (Array.isArray(a.msgs)) {
    msgs = a.msgs.map((m: any) => ("r" in m ? { r: intArg(m.r, "r", 1, 256) } : { w: byteArr(m.w, "w") }));
  } else {
    msgs = [];
    const w = a.write == null ? [] : byteArr(a.write, "write");
    if (w.length) msgs.push({ w });
    const r = a.read == null ? 0 : intArg(a.read, "read", 0, 256);
    if (r) msgs.push({ r });
  }
  if (!msgs.length) throw new HttpError(400, "Nothing to do: give write bytes and/or a read length");
  if (!(b.funcs & 1)) throw new HttpError(409, `i2c-${b.bus} only supports SMBus transfers (${funcNames(b.funcs).join(", ")}); use read/write instead`);
  return withBus(b, () => {
    try {
      return { reads: b.io!.transfer(addr, msgs, !!a.force) };
    } catch (e) {
      throw new HttpError(502, errMsg(e));
    }
  });
}

/** i2cdump-style 256-register grid. null = that register didn't answer. */
async function dump(b: BusState, addr: number, force: boolean) {
  return withBus(b, () => {
    const io = b.io!;
    const out: (number | null)[] = new Array(256).fill(null);
    for (let base = 0; base < 256; base += 32) {
      try {
        io.readBlock(addr, base, 32, force).forEach((v, i) => (out[base + i] = v));
        continue;
      } catch (e) {
        if (e instanceof I2cError && e.errno === 16) throw new HttpError(409, errMsg(e));
      }
      for (let r = base; r < base + 32; r++) {
        try {
          out[r] = io.readByte(addr, r, force);
        } catch {}
      }
    }
    return out;
  });
}

/** EEPROM contents using the per-device size setting (1- or 2-byte word addresses). */
async function eeprom(b: BusState, addr: number, offset: number, len: number) {
  const size = Number(devCfg(b, addr).opts?.size ?? 256);
  if (offset >= size) throw new HttpError(400, `offset beyond the ${size}-byte EEPROM`);
  len = Math.min(len, size - offset, 4096);
  return withBus(b, () => {
    const io = b.io!;
    const out: number[] = [];
    try {
      if (size > 2048) {
        if (!(io.funcs & 1)) throw new HttpError(409, "2-byte-address EEPROMs need a plain-I²C adapter");
        for (let o = offset; o < offset + len; o += 256) {
          const n = Math.min(256, offset + len - o);
          out.push(...io.transfer(addr, [{ w: [o >> 8, o & 0xff] }, { r: n }])[0]);
        }
      } else {
        // 24C04/08/16 put address bits 8–10 in the I²C address (blocks of 256 at addr+1, +2…).
        for (let o = offset; o < offset + len; ) {
          const blk = o >> 8;
          const n = Math.min(32, offset + len - o, 256 - (o & 0xff));
          out.push(...io.readBlock(addr + blk, o & 0xff, n));
          o += n;
        }
      }
    } catch (e) {
      if (e instanceof HttpError) throw e;
      throw new HttpError(502, errMsg(e));
    }
    return { offset, size, bytes: out };
  });
}

function findDev(b: BusState, addr: number): DevState {
  const d = b.devices.get(addr);
  if (!d) throw new HttpError(404, `Nothing found at ${hex(addr)} on i2c-${b.bus} in the last scan`);
  return d;
}

function needConfirm(a: any, what: string) {
  if (a?.confirm !== true) throw new HttpError(400, `${what} writes to the device; repeat with "confirm": true`);
}

// ---------- routes ----------

const routes: Route[] = [
  {
    method: "GET",
    path: "/api/i2c",
    doc: "I²C buses (HDMI DDC buses excluded unless ?all=1), each with the devices from the last scan: identification, confidence, live readings and history",
    handler: (_r, _p, url) => ({ buses: (url.searchParams.get("all") ? [...buses.values()] : userBuses()).map(busView) }),
  },
  {
    method: "POST",
    path: "/api/i2c/:bus/scan",
    doc: { description: "Rescan a bus (i2cdetect auto mode, skips UU) and identify new devices. deep: re-identify everything and allow pointer probes at ambiguous addresses (e.g. 0x39 PCF8574A vs TSL2561)", body: { deep: false, reidentify: false } },
    handler: async (req, p) => {
      const b = parseBus(p.bus);
      const a = await body(req);
      await scan(b, { deep: !!a.deep, reidentify: !!a.reidentify || !!a.deep });
      return busView(b);
    },
  },
  {
    method: "PUT",
    path: "/api/i2c/:bus/settings",
    doc: { description: "Bus settings. autoScan: seconds between background rescans (0 = only on demand)", body: { autoScan: 30 } },
    handler: async (req, p) => {
      const b = parseBus(p.bus);
      const a = await body(req);
      if (a.autoScan != null) {
        const s = intArg(a.autoScan, "autoScan", 0, 86400);
        ctx.settings.set(`bus:${b.key}`, { ...busCfg(b), autoScan: s });
        b.nextScan = s > 0 ? Date.now() + s * 1000 : Infinity;
      }
      ctx.changed();
      return busView(b);
    },
  },
  {
    method: "POST",
    path: "/api/i2c/:bus/:addr/read",
    doc: "Run the device's read-only decoder now (no writes) and return the readings",
    handler: async (_r, p) => {
      const b = parseBus(p.bus);
      return readDev(b, findDev(b, parseAddr(p.addr)), "read");
    },
  },
  {
    method: "POST",
    path: "/api/i2c/:bus/:addr/measure",
    doc: "Take a measurement. May write to the chip (start a conversion, wake it briefly); see measureNote in GET /api/i2c",
    handler: async (_r, p) => {
      const b = parseBus(p.bus);
      return readDev(b, findDev(b, parseAddr(p.addr)), "measure");
    },
  },
  {
    method: "PUT",
    path: "/api/i2c/:bus/:addr/settings",
    doc: { description: "Per-device settings, saved by bus + address. poll: auto-read with the read-only decoder; auto: run Measure every interval (writes); interval in ms; opts: chip options such as shunt (Ω) or EEPROM size; label: your name for it", body: { poll: true, auto: false, interval: 2000, opts: { shunt: 0.1 }, label: "" } },
    handler: async (req, p) => {
      const b = parseBus(p.bus);
      const addr = parseAddr(p.addr);
      const d = findDev(b, addr);
      const a = await body(req);
      const patch: Record<string, any> = {};
      if (a.poll != null) patch.poll = !!a.poll;
      if (a.interval != null) patch.interval = intArg(a.interval, "interval", 250, 3600_000);
      if (a.label != null) patch.label = String(a.label).slice(0, 60);
      if (a.opts && typeof a.opts === "object") {
        const known = new Map((d.chip?.opts || []).map((o) => [o.key, o]));
        const opts = { ...(devCfg(b, addr).opts || {}) };
        for (const [k, v] of Object.entries(a.opts)) {
          const o = known.get(k);
          if (!o) throw new HttpError(400, `${chipName(d) || "This device"} has no option "${k}"`);
          const n = Number(v);
          if (!Number.isFinite(n) || n <= 0) throw new HttpError(400, `${o.label} must be a positive number`);
          opts[k] = n;
        }
        patch.opts = opts;
        if (d.cs) d.cs.opts = opts;
      }
      if (a.auto != null) {
        const on = !!a.auto;
        if (on && !d.chip?.measure) throw new HttpError(409, `${chipName(d) || "This device"} has no measure operation`);
        const was = !!devCfg(b, addr).auto;
        patch.auto = on;
        if (on !== was && d.chip && d.cs) {
          const hook = on ? d.chip.autoStart : d.chip.autoStop;
          if (hook) await withBus(b, () => hook.call(d.chip, new Dev(b.io!, addr), d.cs!)).catch((e) => { throw new HttpError(502, errMsg(e)); });
        }
      }
      setDevCfg(b, addr, patch);
      d.next = 0;
      ctx.changed();
      return devView(b, d);
    },
  },
  {
    method: "POST",
    path: "/api/i2c/:bus/:addr/raw/read",
    doc: { description: "Raw read. size: byte|word|block (need register) or raw (no register: receive-byte, or a pure read of length bytes). Note a register read writes the register pointer first. force: access an address a kernel driver owns", body: { register: "0xD0", size: "byte", length: 1, force: false } },
    handler: async (req, p) => {
      const b = parseBus(p.bus);
      return rawRead(b, parseAddr(p.addr), await body(req));
    },
  },
  {
    method: "POST",
    path: "/api/i2c/:bus/:addr/raw/write",
    doc: { description: "Raw write; requires confirm: true. size: byte (value), word (value, big-endian on the wire unless bigEndian: false), block (bytes, ≤32) with a register; raw: bytes as-is", body: { register: "0xF4", size: "byte", value: "0x27", confirm: true } },
    handler: async (req, p) => {
      const b = parseBus(p.bus);
      const a = await body(req);
      needConfirm(a, "A raw write");
      return rawWrite(b, parseAddr(p.addr), a);
    },
  },
  {
    method: "POST",
    path: "/api/i2c/:bus/:addr/transfer",
    doc: { description: "Combined transaction with repeated starts (i2ctransfer). Either write+read, or msgs: [{w:[…]},{r:n}]. Any write needs confirm: true", body: { write: ["0xFA", "0x0F"], read: 8, confirm: true } },
    handler: async (req, p) => {
      const b = parseBus(p.bus);
      const a = await body(req);
      const writes = Array.isArray(a.msgs) ? a.msgs.some((m: any) => "w" in m) : a.write != null && byteArr(a.write, "write").length > 0;
      if (writes) needConfirm(a, "This transfer");
      return rawTransfer(b, parseAddr(p.addr), a);
    },
  },
  {
    method: "GET",
    path: "/api/i2c/:bus/:addr/dump",
    doc: "i2cdump-style read of registers 0x00–0xFF (null = no answer). Each read sets the register pointer, which on command-style chips (PCF8574, TCA9548A, HTU21D…) executes a command: those need ?confirm=1",
    handler: async (_r, p, url) => {
      const b = parseBus(p.bus);
      const addr = parseAddr(p.addr);
      const d = b.devices.get(addr);
      const unsafe = !d?.chip || POINTER_UNSAFE.has(d.chip.id);
      if (unsafe && !url.searchParams.get("confirm")) throw new HttpError(400, `${d?.chip ? chipName(d) + " treats" : "An unidentified chip may treat"} register pointers as commands; add ?confirm=1 to dump anyway`);
      return { addr, regs: await dump(b, addr, !!url.searchParams.get("force")) };
    },
  },
  {
    method: "GET",
    path: "/api/i2c/:bus/:addr/eeprom",
    doc: "Read EEPROM contents (?offset=0&length=256, ≤4096 per call) using the device's size setting to pick 1- or 2-byte addressing",
    handler: async (_r, p, url) => {
      const b = parseBus(p.bus);
      const addr = parseAddr(p.addr);
      const off = intArg(url.searchParams.get("offset") ?? 0, "offset", 0, 65535);
      const len = intArg(url.searchParams.get("length") ?? 256, "length", 1, 4096);
      return eeprom(b, addr, off, len);
    },
  },
];

// ---------- MCP tools ----------

const busProp = { bus: { type: ["integer", "string"], description: 'Bus number, e.g. 1 or "i2c-1" (see i2c_buses)' } };
const addrProp = { address: { type: ["integer", "string"], description: 'Device address, 7-bit, e.g. 118 or "0x76"' } };

function devicesSummary(b: BusState) {
  return busView(b).devices.map((d) => ({
    address: d.hex,
    claimedBy: d.claimed,
    chip: d.name,
    what: d.what,
    confidence: d.confidence,
    note: d.note,
    possible: d.hints.length ? d.hints : undefined,
    readings: d.readout ? Object.fromEntries(d.readout.values.map((v) => [v.label, v.unit ? `${v.v} ${v.unit}` : v.v])) : null,
    readingNote: d.readout?.note,
    stale: d.readout?.stale || undefined,
    readAt: d.readAt ? new Date(d.readAt).toISOString() : null,
    canMeasure: d.canMeasure,
    measureNote: d.measureNote || undefined,
    error: d.error || undefined,
  }));
}

const tools: Tool[] = [
  {
    name: "i2c_buses",
    description: "List the I²C buses on DeskyPi (header pins 3/5 is i2c-1; USB adapters and i2c-stub appear as extra buses). HDMI DDC buses are omitted.",
    inputSchema: { type: "object", properties: {} },
    handler: () => userBuses().map((b) => ({ bus: b.bus, label: b.label, adapter: b.name, functionality: funcNames(b.funcs), devices: b.devices.size, lastScan: b.lastScan ? new Date(b.lastScan).toISOString() : null, error: b.error || undefined })),
  },
  {
    name: "i2c_scan",
    description: "Scan an I²C bus like `i2cdetect -y` (quick-write, read-byte at 0x30–0x37/0x50–0x5F, skips addresses owned by kernel drivers) and identify what answers. Returns the device list.",
    inputSchema: { type: "object", properties: { ...busProp, deep: { type: "boolean", description: "Re-identify every device and allow register-pointer probes at ambiguous addresses" } }, required: ["bus"] },
    handler: async (a) => {
      const b = parseBus(a.bus);
      await scan(b, { deep: !!a.deep, reidentify: !!a.deep });
      if (b.error) throw new Error(b.error);
      return { bus: b.bus, devices: devicesSummary(b) };
    },
  },
  {
    name: "i2c_devices",
    description: "Identified I²C devices with their latest decoded readings (°C, %RH, hPa, lux, V/A/W, g, RTC time…). Pass measure: true to take a fresh measurement on chips that need a command to convert (this writes to them).",
    inputSchema: { type: "object", properties: { ...busProp, ...addrProp, measure: { type: "boolean" } } },
    handler: async (a) => {
      const list = a.bus != null ? [parseBus(a.bus)] : userBuses().filter((b) => b.io);
      if (a.address != null || a.measure) {
        for (const b of list)
          for (const d of b.devices.values()) {
            if (a.address != null && d.addr !== parseAddr(a.address)) continue;
            if (!d.chip || d.claimed) continue;
            await readDev(b, d, a.measure && d.chip.measure ? "measure" : "read").catch(() => {});
          }
      }
      return list.map((b) => ({ bus: b.bus, label: b.label, devices: devicesSummary(b).filter((d) => a.address == null || parseInt(d.address, 16) === parseAddr(a.address)) }));
    },
  },
  {
    name: "i2c_read",
    description: "Raw I²C read. With a register: size byte/word/block (word returns both byte orders). Without: receive-byte, or a pure read of `length` bytes. Register reads write the register pointer first, which command-style chips (PCF8574, TCA9548A, HTU21D) treat as a command.",
    inputSchema: {
      type: "object",
      properties: { ...busProp, ...addrProp, register: { type: ["integer", "string"] }, size: { enum: ["byte", "word", "block", "raw"] }, length: { type: "integer", minimum: 1, maximum: 256 }, force: { type: "boolean", description: "Access an address owned by a kernel driver" } },
      required: ["bus", "address"],
    },
    handler: async (a) => rawRead(parseBus(a.bus), parseAddr(a.address), a),
  },
  {
    name: "i2c_write",
    description: "Raw I²C write (changes device state). size byte: value to register; word: 16-bit value (MSB first on the wire unless bigEndian false); block: bytes to register; raw: bytes as-is with no register.",
    inputSchema: {
      type: "object",
      properties: { ...busProp, ...addrProp, register: { type: ["integer", "string"] }, size: { enum: ["byte", "word", "block", "raw"] }, value: { type: ["integer", "string"] }, bytes: { type: "array", items: { type: ["integer", "string"] } }, bigEndian: { type: "boolean" }, force: { type: "boolean" } },
      required: ["bus", "address"],
    },
    handler: async (a) => rawWrite(parseBus(a.bus), parseAddr(a.address), a),
  },
  {
    name: "i2c_transfer",
    description: "Combined I²C transaction with repeated start: write bytes then read N bytes (like i2ctransfer w2@0x50 0x00 0x00 r16). Needs a plain-I²C adapter (i2c-1 is). Use msgs for more than one write/read.",
    inputSchema: {
      type: "object",
      properties: {
        ...busProp,
        ...addrProp,
        write: { type: "array", items: { type: ["integer", "string"] } },
        read: { type: "integer", minimum: 0, maximum: 256 },
        msgs: { type: "array", items: { type: "object", properties: { w: { type: "array", items: { type: ["integer", "string"] } }, r: { type: "integer" } } } },
        force: { type: "boolean" },
      },
      required: ["bus", "address"],
    },
    handler: async (a) => rawTransfer(parseBus(a.bus), parseAddr(a.address), a),
  },
];

// ---------- driver ----------

const driver: Driver = {
  id: "i2c",
  title: "I²C",
  order: 40,
  description: "I²C buses (header pins 3/5, USB adapters): scan, identify sensors, live readings, raw register tools",
  async init(c) {
    ctx = c;
    running = false;
    await refreshBuses();
    running = true;
    tick = setInterval(onTick, 250);
  },
  stop() {
    running = false;
    if (tick) clearInterval(tick);
    if (refreshTimer) clearTimeout(refreshTimer);
    tick = refreshTimer = null;
    for (const b of buses.values()) b.io?.close();
    buses.clear();
  },
  udev(ev: UdevEvent) {
    if (ev.props.SUBSYSTEM !== "i2c-dev" && ev.props.SUBSYSTEM !== "i2c") return;
    // A new adapter (USB, stub) or a client bound/unbound (changes UU): refresh soon.
    if (refreshTimer) clearTimeout(refreshTimer);
    refreshTimer = setTimeout(() => {
      refreshTimer = null;
      void refreshBuses();
      if (ev.props.SUBSYSTEM === "i2c") for (const b of buses.values()) if (b.kind !== "ddc") b.nextScan = Date.now();
    }, 300);
  },
  status() {
    const list = userBuses();
    const devs = list.flatMap((b) => [...b.devices.values()]);
    const names = devs.map((d) => (d.claimed ? `${hex(d.addr)} (${d.claimed})` : chipName(d) || hex(d.addr)));
    const busNames = list.map((b) => `i2c-${b.bus}`).join(", ") || "no buses";
    return {
      active: devs.length > 0,
      summary: devs.length ? `${busNames} · ${names.slice(0, 4).join(", ")}${names.length > 4 ? ` +${names.length - 4}` : ""}` : `${busNames} · no devices`,
      buses: list.map(busView),
      hiddenBuses: [...buses.values()].filter((b) => b.kind === "ddc").map((b) => `i2c-${b.bus}`),
    };
  },
  routes,
  tools,
};

export default driver;
