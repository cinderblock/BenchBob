// ==================== I²C chip identification + decoding ====================
//
// A scan only says "something ACKed at 0x76". This file turns that into "BME280, 23.4 °C".
//
// Safety model (the reason the code is ordered the way it is):
//  - Identification never writes data. The only bytes sent are register pointers and
//    read-type commands (e.g. SHT3x "read status").
//  - A register pointer is itself a command on some chips: on a PCF8574 it sets the output
//    pins, on a TCA9548A it selects mux channels, on an MCP4725 it starts a DAC write, on an
//    HTU21D 0xFE is soft reset. So at addresses where such chips live, probes start with
//    *pure reads* (no write phase at all) and only move on to pointer probes once those rule
//    the command chip out. CHIPS is tried in array order per address; the order is the
//    safety mechanism, so read the comments before reordering.
//  - read() decodes using reads only and is what auto-polling calls. measure() is allowed to
//    write (start a conversion, wake a sleeping chip) and only runs on an explicit Measure or
//    the per-device auto-measure opt-in. Where measure() wakes a chip it restores the
//    previous power state afterwards.

import { F, I2cError, type I2cIo } from "./i2c-dev.ts";

// ---------- Types ----------

export interface Val {
  k: string; // stable key, used for history
  label: string;
  v: number | string | boolean | null;
  unit?: string;
  d?: number; // decimals for display
}

export interface Pin {
  name: string;
  level: 0 | 1;
  dir?: "in" | "out";
  pull?: boolean;
}

export interface Readout {
  values: Val[];
  pins?: Pin[];
  channels?: { ch: number; duty: number; on: number; off: number; full?: "on" | "off" }[];
  hex?: { start: number; bytes: number[] };
  note?: string;
  /** Data is not fresh: the chip is asleep / idle, Measure would refresh it. */
  stale?: boolean;
}

export interface Probe {
  confidence: "id" | "likely" | "guess"; // id = an ID register matched
  variant?: string; // display name override, e.g. "BMP280"
  note?: string;
  data?: any; // cached probe results (calibration, register map) for read/measure
}

export interface ProbeCtx {
  /** Chips already identified on this bus (ascending address order), by address. */
  found: Map<number, string>;
  /** User asked for a deep identify: allows pointer probes at ambiguous addresses. */
  deep?: boolean;
}

export interface ChipState {
  probe: Probe;
  opts: Record<string, any>;
  cache?: any;
}

export interface Opt {
  key: string;
  label: string;
  type: "number" | "select";
  default: any;
  choices?: { v: any; label: string }[];
  unit?: string;
}

export interface ChipDef {
  id: string;
  name: string;
  what: string; // one-line kind, "Temperature / humidity / pressure"
  addrs: number[];
  probe(d: Dev, c: ProbeCtx): Promise<Probe | null>;
  /** Read-only decode (safe to auto-poll). */
  read?(d: Dev, s: ChipState): Promise<Readout>;
  /** Default false for chips whose read() is a one-off (EEPROM, display status). */
  poll?: boolean;
  /** Takes a measurement; may write. */
  measure?(d: Dev, s: ChipState): Promise<Readout>;
  /** What Measure writes, shown next to the button. */
  measureNote?: string;
  /** Called when auto-measure is switched on/off (e.g. SCD4x periodic mode). */
  autoStart?(d: Dev, s: ChipState): Promise<void>;
  autoStop?(d: Dev, s: ChipState): Promise<void>;
  opts?: Opt[];
}

// ---------- Device handle ----------

export const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
export const s8 = (v: number) => (v & 0x80 ? v - 0x100 : v);
export const s16 = (v: number) => (v & 0x8000 ? v - 0x10000 : v);
const bcd = (v: number) => (v >> 4) * 10 + (v & 0x0f);
const isBcd = (v: number) => (v & 0x0f) <= 9 && v >> 4 <= 9;
const r2 = (v: number, n = 2) => Math.round(v * 10 ** n) / 10 ** n;

/** One address on one bus. Thin, synchronous wrappers plus SMBus fallbacks. */
export class Dev {
  constructor(public io: I2cIo, public addr: number) {}
  get plain() {
    return !!(this.io.funcs & F.I2C);
  }
  u8(reg: number) {
    return this.io.readByte(this.addr, reg);
  }
  i8(reg: number) {
    return s8(this.u8(reg));
  }
  /** 16-bit big-endian register (TI/most sensors) in one transaction. */
  u16be(reg: number) {
    const w = this.io.readWord(this.addr, reg);
    return ((w & 0xff) << 8) | (w >> 8);
  }
  i16be(reg: number) {
    return s16(this.u16be(reg));
  }
  /** 16-bit little-endian register (SMBus native). */
  u16le(reg: number) {
    return this.io.readWord(this.addr, reg);
  }
  /** Consecutive registers, chunked to the 32-byte SMBus I2C-block limit. */
  block(reg: number, n: number): number[] {
    const out: number[] = [];
    while (out.length < n) {
      const want = Math.min(32, n - out.length);
      const got = this.io.readBlock(this.addr, reg + out.length, want);
      if (!got.length) throw new I2cError(5, "short block read");
      out.push(...got);
    }
    return out.slice(0, n);
  }
  recv() {
    return this.io.receive(this.addr);
  }
  /** Pure read (no pointer write). SMBus-only adapters fall back to repeated receive-byte. */
  read(n: number): number[] {
    if (this.plain) return this.io.transfer(this.addr, [{ r: n }])[0];
    return Array.from({ length: n }, () => this.io.receive(this.addr));
  }
  /** Write raw bytes (one transaction). */
  write(bytes: number[]) {
    if (this.plain) return void this.io.transfer(this.addr, [{ w: bytes }]);
    if (bytes.length === 1) return this.io.send(this.addr, bytes[0]);
    if (bytes.length === 2) return this.io.writeByte(this.addr, bytes[0], bytes[1]);
    if (bytes.length === 3) return this.io.writeWord(this.addr, bytes[0], bytes[1] | (bytes[2] << 8));
    return this.io.writeBlock(this.addr, bytes[0], bytes.slice(1));
  }
  /** Write then read with a repeated start. */
  writeRead(bytes: number[], n: number): number[] {
    if (this.plain) return this.io.transfer(this.addr, [{ w: bytes }, { r: n }])[0];
    if (bytes.length === 1) return this.block(bytes[0], n);
    throw new I2cError(95, "write-then-read needs a plain-I²C adapter");
  }
  wb(reg: number, v: number) {
    this.io.writeByte(this.addr, reg, v);
  }
}

/** Run a probe step, treating any bus error as "not this chip". */
function tryq<T>(f: () => T): T | null {
  try {
    return f();
  } catch (e) {
    if (e instanceof I2cError) return null;
    throw e;
  }
}

const range = (a: number, b: number) => Array.from({ length: b - a + 1 }, (_, i) => a + i);

// ---------- CRCs ----------

/** Sensirion / Aosong CRC-8: poly 0x31, init 0xFF. */
export function crc8sens(bytes: number[], init = 0xff): number {
  let c = init;
  for (const b of bytes) {
    c ^= b;
    for (let i = 0; i < 8; i++) c = c & 0x80 ? ((c << 1) ^ 0x31) & 0xff : (c << 1) & 0xff;
  }
  return c;
}

/** Sensirion-style words: [hi, lo, crc]×n → words, or null if any CRC fails. */
export function sensWords(b: number[], init = 0xff): number[] | null {
  const out: number[] = [];
  for (let i = 0; i + 2 < b.length; i += 3) {
    if (crc8sens([b[i], b[i + 1]], init) !== b[i + 2]) return null;
    out.push((b[i] << 8) | b[i + 1]);
  }
  return out;
}

// ---------- Decoders (pure functions, unit-tested) ----------

export interface Bme280Cal {
  T1: number; T2: number; T3: number;
  P1: number; P2: number; P3: number; P4: number; P5: number; P6: number; P7: number; P8: number; P9: number;
  H1?: number; H2?: number; H3?: number; H4?: number; H5?: number; H6?: number;
}

/** Calibration from 0x88..0xA1 (26 bytes) and, for BME280, 0xE1..0xE7 (7 bytes). */
export function bme280Cal(a: number[], h?: number[]): Bme280Cal {
  const u = (i: number) => a[i] | (a[i + 1] << 8);
  const s = (i: number) => s16(u(i));
  const c: Bme280Cal = { T1: u(0), T2: s(2), T3: s(4), P1: u(6), P2: s(8), P3: s(10), P4: s(12), P5: s(14), P6: s(16), P7: s(18), P8: s(20), P9: s(22) };
  if (h) {
    c.H1 = a[25];
    c.H2 = s16(h[0] | (h[1] << 8));
    c.H3 = h[2];
    c.H4 = (s8(h[3]) << 4) | (h[4] & 0x0f);
    c.H5 = (s8(h[5]) << 4) | (h[4] >> 4);
    c.H6 = s8(h[6]);
  }
  return c;
}

/** Bosch BME280/BMP280 floating-point compensation (datasheet §8.1). Pa, °C, %RH. */
export function bme280Comp(c: Bme280Cal, adcT: number, adcP: number, adcH?: number) {
  let v1 = (adcT / 16384 - c.T1 / 1024) * c.T2;
  let v2 = (adcT / 131072 - c.T1 / 8192) ** 2 * c.T3;
  const tf = v1 + v2;
  const T = tf / 5120;
  v1 = tf / 2 - 64000;
  v2 = (v1 * v1 * c.P6) / 32768;
  v2 = v2 + v1 * c.P5 * 2;
  v2 = v2 / 4 + c.P4 * 65536;
  v1 = ((c.P3 * v1 * v1) / 524288 + c.P2 * v1) / 524288;
  v1 = (1 + v1 / 32768) * c.P1;
  let P: number | null = null;
  if (v1 !== 0 && adcP !== 0x80000) {
    let p = 1048576 - adcP;
    p = ((p - v2 / 4096) * 6250) / v1;
    const a = (c.P9 * p * p) / 2147483648;
    const b = (p * c.P8) / 32768;
    P = p + (a + b + c.P7) / 16;
  }
  let H: number | null = null;
  if (adcH !== undefined && adcH !== 0x8000 && c.H1 !== undefined) {
    let h = tf - 76800;
    h = (adcH - (c.H4! * 64 + (c.H5! / 16384) * h)) * ((c.H2! / 65536) * (1 + (c.H6! / 67108864) * h * (1 + (c.H3! / 67108864) * h)));
    h = h * (1 - (c.H1 * h) / 524288);
    H = Math.max(0, Math.min(100, h));
  }
  return { T, P, H };
}

export interface Bme680Cal {
  t1: number; t2: number; t3: number;
  p1: number; p2: number; p3: number; p4: number; p5: number; p6: number; p7: number; p8: number; p9: number; p10: number;
  h1: number; h2: number; h3: number; h4: number; h5: number; h6: number; h7: number;
  g1: number; g2: number; g3: number;
  heatRange: number; heatVal: number; swErr: number;
}

/** reg(addr) → byte for 0x00..0x04, 0x8A..0xA1, 0xE1..0xEE. */
export function bme680Cal(reg: (a: number) => number): Bme680Cal {
  const u16 = (lo: number, hi: number) => reg(lo) | (reg(hi) << 8);
  return {
    t1: u16(0xe9, 0xea), t2: s16(u16(0x8a, 0x8b)), t3: s8(reg(0x8c)),
    p1: u16(0x8e, 0x8f), p2: s16(u16(0x90, 0x91)), p3: s8(reg(0x92)), p4: s16(u16(0x94, 0x95)), p5: s16(u16(0x96, 0x97)),
    p6: s8(reg(0x99)), p7: s8(reg(0x98)), p8: s16(u16(0x9c, 0x9d)), p9: s16(u16(0x9e, 0x9f)), p10: reg(0xa0),
    h1: (reg(0xe3) << 4) | (reg(0xe2) & 0x0f), h2: (reg(0xe1) << 4) | (reg(0xe2) >> 4),
    h3: s8(reg(0xe4)), h4: s8(reg(0xe5)), h5: s8(reg(0xe6)), h6: reg(0xe7), h7: s8(reg(0xe8)),
    g1: s8(reg(0xed)), g2: s16(u16(0xeb, 0xec)), g3: s8(reg(0xee)),
    heatRange: (reg(0x02) & 0x30) >> 4, heatVal: s8(reg(0x00)), swErr: s8(reg(0x04) & 0xf0) >> 4,
  };
}

const K1 = [0, 0, 0, 0, 0, -1, 0, -0.8, 0, 0, -0.2, -0.5, 0, -1, 0, 0];
const K2 = [0, 0, 0, 0, 0.1, 0.7, 0, -0.8, -0.1, 0, 0, 0, 0, 0, 0, 0];

/** Bosch BME68x API floating-point compensation (BME680 gas formula). */
export function bme680Comp(c: Bme680Cal, adcT: number, adcP: number, adcH: number, gas?: { adc: number; range: number }) {
  let v1 = (adcT / 16384 - c.t1 / 1024) * c.t2;
  let v2 = (adcT / 131072 - c.t1 / 8192) ** 2 * (c.t3 * 16);
  const tf = v1 + v2;
  const T = tf / 5120;
  v1 = tf / 2 - 64000;
  v2 = v1 * v1 * (c.p6 / 131072);
  v2 = v2 + v1 * c.p5 * 2;
  v2 = v2 / 4 + c.p4 * 65536;
  v1 = ((c.p3 * v1 * v1) / 16384 + c.p2 * v1) / 524288;
  v1 = (1 + v1 / 32768) * c.p1;
  let P = 1048576 - adcP;
  if (v1 !== 0) {
    P = ((P - v2 / 4096) * 6250) / v1;
    const a = (c.p9 * P * P) / 2147483648;
    const b = P * (c.p8 / 32768);
    const d = (P / 256) ** 3 * (c.p10 / 131072);
    P = P + (a + b + d + c.p7 * 128) / 16;
  }
  const tc = T;
  const h1 = adcH - (c.h1 * 16 + (c.h3 / 2) * tc);
  const h2 = h1 * ((c.h2 / 262144) * (1 + (c.h4 / 16384) * tc + (c.h5 / 1048576) * tc * tc));
  const h3 = c.h6 / 16384;
  const h4 = c.h7 / 2097152;
  const H = Math.max(0, Math.min(100, h2 + (h3 + h4 * tc) * h2 * h2));
  let G: number | null = null;
  if (gas) {
    const a = 1340 + 5 * c.swErr;
    const b = a * (1 + K1[gas.range] / 100);
    const d = 1 + K2[gas.range] / 100;
    G = 1 / (d * 0.000000125 * (1 << gas.range) * ((gas.adc - 512) / b + 1));
  }
  return { T, P, H, G };
}

/** BME680 heater resistance code for a target °C. */
export function bme680HeatRes(c: Bme680Cal, target: number, ambient: number) {
  const a = c.g1 / 16 + 49;
  const b = (c.g2 / 32768) * 0.0005 + 0.00235;
  const d = c.g3 / 1024;
  const e = a * (1 + b * target) + d * ambient;
  return Math.max(0, Math.min(255, Math.round(3.4 * (e * (4 / (4 + c.heatRange)) * (1 / (1 + c.heatVal * 0.002)) - 25))));
}

export interface Bmp180Cal { AC1: number; AC2: number; AC3: number; AC4: number; AC5: number; AC6: number; B1: number; B2: number; MB: number; MC: number; MD: number }

export function bmp180Cal(b: number[]): Bmp180Cal {
  const u = (i: number) => (b[i] << 8) | b[i + 1];
  const s = (i: number) => s16(u(i));
  return { AC1: s(0), AC2: s(2), AC3: s(4), AC4: u(6), AC5: u(8), AC6: u(10), B1: s(12), B2: s(14), MB: s(16), MC: s(18), MD: s(20) };
}

/** BMP180 datasheet integer algorithm. Power-of-two divisions are arithmetic shifts in the
 * reference code (floor, not truncation), which matters for the negative terms. °C and Pa. */
export function bmp180Comp(c: Bmp180Cal, UT: number, UP: number, oss = 0) {
  const sh = (x: number, n: number) => Math.floor(x / 2 ** n);
  const T = Math.trunc;
  let X1 = sh((UT - c.AC6) * c.AC5, 15);
  let X2 = T((c.MC * 2048) / (X1 + c.MD));
  const B5 = X1 + X2;
  const temp = sh(B5 + 8, 4) / 10;
  const B6 = B5 - 4000;
  X1 = sh(c.B2 * sh(B6 * B6, 12), 11);
  X2 = sh(c.AC2 * B6, 11);
  let X3 = X1 + X2;
  const B3 = T(((c.AC1 * 4 + X3) * 2 ** oss + 2) / 4);
  X1 = sh(c.AC3 * B6, 13);
  X2 = sh(c.B1 * sh(B6 * B6, 12), 16);
  X3 = sh(X1 + X2 + 2, 2);
  const B4 = sh(c.AC4 * (X3 + 32768), 15);
  const B7 = (UP - B3) * (50000 >> oss);
  let p = B7 < 0x80000000 ? T((B7 * 2) / B4) : T(B7 / B4) * 2;
  X1 = sh(p, 8) * sh(p, 8);
  X1 = sh(X1 * 3038, 16);
  X2 = sh(-7357 * p, 16);
  p = p + sh(X1 + X2 + 3791, 4);
  return { T: temp, P: p };
}

/** TSL2561 T/FN/CL lux from raw channels, integration code (0..2) and gain bit. */
export function tsl2561Lux(ch0: number, ch1: number, integ: number, gain16: boolean): number | null {
  if (ch0 === 0) return 0;
  const sat = integ === 0 ? 5047 : integ === 1 ? 37177 : 65535;
  if (ch0 >= sat || ch1 >= sat) return null; // saturated
  const scale = (integ === 0 ? 322 / 11 : integ === 1 ? 322 / 81 : 1) * (gain16 ? 1 : 16);
  const c0 = ch0 * scale;
  const c1 = ch1 * scale;
  const r = c1 / c0;
  let lux = 0;
  if (r <= 0.5) lux = 0.0304 * c0 - 0.062 * c0 * r ** 1.4;
  else if (r <= 0.61) lux = 0.0224 * c0 - 0.031 * c1;
  else if (r <= 0.8) lux = 0.0128 * c0 - 0.0153 * c1;
  else if (r <= 1.3) lux = 0.00146 * c0 - 0.00112 * c1;
  return Math.max(0, lux);
}

/** AHT10/AHT20 7-byte frame (status, 5 data bytes, CRC) → °C, %RH. */
export function ahtDecode(b: number[]): { T: number; H: number; crcOk: boolean | null } {
  const hr = (b[1] << 12) | (b[2] << 4) | (b[3] >> 4);
  const tr = ((b[3] & 0x0f) << 16) | (b[4] << 8) | b[5];
  return { H: (hr / 1048576) * 100, T: (tr / 1048576) * 200 - 50, crcOk: b.length >= 7 ? crc8sens(b.slice(0, 6)) === b[6] : null };
}

export const sht3xT = (raw: number) => -45 + (175 * raw) / 65535;
export const sht3xH = (raw: number) => (100 * raw) / 65535;
export const sht4xH = (raw: number) => Math.max(0, Math.min(100, -6 + (125 * raw) / 65535));

/** VEML7700 lux per count for a config word (gain bits 12:11, IT bits 9:6). */
export function veml7700It(cfg: number): number {
  return ({ 0xc: 25, 0x8: 50, 0x0: 100, 0x1: 200, 0x2: 400, 0x3: 800 } as Record<number, number>)[(cfg >> 6) & 0xf] ?? 100;
}
export function veml7700Res(cfg: number): number {
  const gain = [1, 2, 1 / 8, 1 / 4][(cfg >> 11) & 3];
  return 0.0036 * (800 / veml7700It(cfg)) * (2 / gain);
}
export function veml7700Lux(raw: number, cfg: number): number {
  let l = raw * veml7700Res(cfg);
  // Vishay's high-lux non-linearity correction (app note), valid above ~1000 lx.
  if (l > 1000) l = 6.0135e-13 * l ** 4 - 9.3924e-9 * l ** 3 + 8.1488e-5 * l ** 2 + 1.0023 * l;
  return l;
}

const ADS_FSR = [6.144, 4.096, 2.048, 1.024, 0.512, 0.256, 0.256, 0.256];
const ADS_MUX = ["AIN0−AIN1", "AIN0−AIN3", "AIN1−AIN3", "AIN2−AIN3", "AIN0", "AIN1", "AIN2", "AIN3"];

/** ADS1x15 conversion register → volts, given the config word that produced it. */
export function adsVolts(raw: number, cfg: number, ads1015 = false): number {
  const fsr = ADS_FSR[(cfg >> 9) & 7];
  return ads1015 ? (s16(raw) >> 4) * (fsr / 2048) : s16(raw) * (fsr / 32768);
}

/** DS1307/DS3231 time registers 0x00..0x06 → Date (UTC). */
export function rtcDecode(r: number[]): { date: Date | null; halted: boolean; h12: boolean } {
  const halted = !!(r[0] & 0x80); // DS1307 CH bit
  const sec = bcd(r[0] & 0x7f);
  const min = bcd(r[1] & 0x7f);
  const h12 = !!(r[2] & 0x40);
  let hr = h12 ? bcd(r[2] & 0x1f) % 12 + (r[2] & 0x20 ? 12 : 0) : bcd(r[2] & 0x3f);
  const day = bcd(r[4] & 0x3f);
  const mon = bcd(r[5] & 0x1f);
  const yr = 2000 + bcd(r[6]) + (r[5] & 0x80 ? 100 : 0);
  if (sec > 59 || min > 59 || hr > 23 || day < 1 || day > 31 || mon < 1 || mon > 12) return { date: null, halted, h12 };
  return { date: new Date(Date.UTC(yr, mon - 1, day, hr, min, sec)), halted, h12 };
}

/** Raspberry Pi HAT EEPROM header ("R-Pi" signature, atoms). Returns null if absent. */
export function parseHatEeprom(b: number[]): { version: number; atoms: number; length: number; vendor?: string; product?: string; uuid?: string; pid?: number; pver?: number } | null {
  if (b.length < 12 || String.fromCharCode(b[0], b[1], b[2], b[3]) !== "R-Pi") return null;
  const u16 = (i: number) => b[i] | (b[i + 1] << 8);
  const u32 = (i: number) => (b[i] | (b[i + 1] << 8) | (b[i + 2] << 16) | (b[i + 3] << 24)) >>> 0;
  const out: any = { version: b[4], atoms: u16(6), length: u32(8) };
  // First atom at 12: type u16, count u16, dlen u32, data. Type 1 = vendor info.
  if (b.length >= 20 && u16(12) === 1) {
    const d = 20;
    if (b.length >= d + 22) {
      const uuid = b.slice(d, d + 16).reverse().map((x) => x.toString(16).padStart(2, "0")).join("");
      out.uuid = `${uuid.slice(0, 8)}-${uuid.slice(8, 12)}-${uuid.slice(12, 16)}-${uuid.slice(16, 20)}-${uuid.slice(20)}`;
      out.pid = u16(d + 16);
      out.pver = u16(d + 18);
      const vl = b[d + 20];
      const pl = b[d + 21];
      const str = (s: number, n: number) => String.fromCharCode(...b.slice(s, s + n)).replace(/\0+$/, "");
      if (b.length >= d + 22 + vl) out.vendor = str(d + 22, vl);
      if (b.length >= d + 22 + vl + pl) out.product = str(d + 22 + vl, pl);
    }
  }
  return out;
}

// ---------- Chip definitions (ORDER MATTERS, see header) ----------

const deg = (k: string, label: string, v: number | null, d = 2): Val => ({ k, label, v: v == null ? null : r2(v, d), unit: "°C", d });

const ssd1306: ChipDef = {
  id: "ssd1306",
  name: "SSD1306 / SH1106 OLED",
  what: "Monochrome OLED display controller",
  addrs: [0x3c, 0x3d],
  // Pure read only: every write to a display controller is a control byte.
  async probe(d) {
    const st = tryq(() => d.recv());
    return { confidence: "likely", note: "No ID register; recognised by address (0x3C/0x3D is also a PCF8574A option).", data: { status: st } };
  },
  async read(d) {
    const st = tryq(() => d.recv());
    if (st == null) return { values: [], note: "This module doesn't support reads (write-only I²C)." };
    return { values: [{ k: "on", label: "Display", v: st & 0x40 ? "off" : "on" }, { k: "status", label: "Status byte", v: "0x" + st.toString(16).padStart(2, "0") }] };
  },
  poll: false,
};

const aht2x: ChipDef = {
  id: "aht2x",
  name: "AHT20",
  what: "Temperature / humidity",
  addrs: [0x38],
  // Pure status read: AHT parts answer a bare read with their status byte (bit3 = calibrated).
  async probe(d) {
    const st = tryq(() => d.recv());
    if (st == null || (st & 0x68) !== 0x08) return null;
    return { confidence: "likely", note: `Status 0x${st.toString(16)} (calibrated, idle). No ID register.` };
  },
  measureNote: "sends 0xAC 0x33 0x00 (trigger measurement)",
  async measure(d) {
    let st = d.recv();
    if (!(st & 0x08)) {
      d.write([0xbe, 0x08, 0x00]); // initialise / load calibration
      await sleep(10);
    }
    d.write([0xac, 0x33, 0x00]);
    await sleep(80);
    let b = d.read(7);
    for (let i = 0; i < 5 && b[0] & 0x80; i++) {
      await sleep(20);
      b = d.read(7);
    }
    if (b[0] & 0x80) throw new Error("AHT20 stayed busy");
    const r = ahtDecode(b);
    if (r.crcOk === false) throw new Error("AHT20 CRC mismatch");
    return { values: [deg("t", "Temperature", r.T), { k: "rh", label: "Humidity", v: r2(r.H, 1), unit: "%RH", d: 1 }] };
  },
};

/** PCF8574/PCF8574A/PCF8575 (quasi-bidirectional, no registers) vs MCP23008/MCP23017 (register files). */
const expander: ChipDef = {
  id: "expander",
  name: "GPIO expander",
  what: "8/16-bit I/O expander",
  addrs: [...range(0x20, 0x27), ...range(0x38, 0x3f)],
  // A pointer write to a PCF8574 *is* an output write, so classify with pure reads only:
  // a PCF857x returns the port state over and over; an MCP230xx auto-increments through
  // its (varied) registers.
  async probe(d, c) {
    const b = tryq(() => d.read(8));
    if (!b) return null;
    const hi = d.addr >= 0x38;
    const same = b.every((x) => x === b[0]);
    const alt = !same && b.every((x, i) => x === b[i % 2]);
    if (same) {
      if (d.addr === 0x39 && c.deep) return null; // let the TSL2561/APDS-9960 pointer probes run
      const v = hi ? "PCF8574A" : "PCF8574";
      return {
        confidence: d.addr === 0x39 ? "guess" : "likely",
        variant: v,
        note: d.addr === 0x39 ? "Could also be a TSL2561 or APDS-9960. Deep identify writes a register pointer to tell them apart (a PCF8574A would see it as an output write)." : "Reads return the same port byte every time.",
        data: { kind: "pcf" },
      };
    }
    if (alt && !hi) return { confidence: "likely", variant: "PCF8575", note: "Reads alternate between two port bytes.", data: { kind: "pcf16" } };
    if (!hi) return { confidence: "likely", variant: "MCP23017 / MCP23008", note: "Register file (reads auto-increment).", data: { kind: "mcp" } };
    return null;
  },
  async read(d, s) {
    const kind = s.probe.data?.kind;
    if (kind === "pcf" || kind === "pcf16") {
      const b = d.read(kind === "pcf" ? 1 : 2);
      const pins: Pin[] = [];
      b.forEach((byte, p) => {
        for (let i = 0; i < 8; i++) pins.push({ name: kind === "pcf" ? `P${i}` : `P${p}${i}`, level: ((byte >> i) & 1) as 0 | 1 });
      });
      return { values: [{ k: "port", label: "Port", v: b.map((x) => "0x" + x.toString(16).padStart(2, "0")).join(" ") }], pins, note: "Quasi-bidirectional: a 1 is a weak pull-up (input or output high); a 0 is driven or pulled low." };
    }
    // MCP230xx, BANK=0 map (IOCON.BANK=1 moves the registers; detect it via IOCON).
    const iocon = d.u8(0x0a);
    const bank1 = !!(iocon & 0x80);
    const R = bank1 ? { IODIRA: 0x00, IODIRB: 0x10, GPPUA: 0x06, GPPUB: 0x16, GPIOA: 0x09, GPIOB: 0x19 } : { IODIRA: 0x00, IODIRB: 0x01, GPPUA: 0x0c, GPPUB: 0x0d, GPIOA: 0x12, GPIOB: 0x13 };
    const pins: Pin[] = [];
    for (const [port, dir, pu, gp] of [["A", R.IODIRA, R.GPPUA, R.GPIOA], ["B", R.IODIRB, R.GPPUB, R.GPIOB]] as const) {
      const io = d.u8(dir), pull = d.u8(pu), lv = d.u8(gp);
      for (let i = 0; i < 8; i++) pins.push({ name: `GP${port}${i}`, level: ((lv >> i) & 1) as 0 | 1, dir: (io >> i) & 1 ? "in" : "out", pull: !!((pull >> i) & 1) });
    }
    return { values: [{ k: "iocon", label: "IOCON", v: "0x" + iocon.toString(16).padStart(2, "0") }], pins, note: "MCP23008 has only port A; port B values are meaningless on one." };
  },
};

const mcp4725: ChipDef = {
  id: "mcp4725",
  name: "MCP4725",
  what: "12-bit DAC",
  addrs: range(0x60, 0x67),
  // Any written byte with the top bits 00 is a "fast write" of the DAC output, so this
  // address range only ever gets pure reads unless MCP4725 is ruled out by a NACK.
  async probe(d) {
    const b = tryq(() => d.read(d.plain ? 5 : 1));
    if (!b) return null;
    const ok = b.length < 5 || ((b[2] & 0x0f) === 0 && (b[3] & 0x90) === 0);
    return { confidence: ok ? "likely" : "guess", note: ok ? "Read-back frame matches the MCP4725 layout." : "Answers reads but the frame doesn't look like an MCP4725." };
  },
  async read(d, s) {
    if (!d.plain) return { values: [], note: "Needs a plain-I²C adapter to read the 5-byte frame." };
    const b = d.read(5);
    const dac = (b[1] << 4) | (b[2] >> 4);
    const ee = ((b[3] & 0x0f) << 8) | b[4];
    const vdd = Number(s.opts.vdd ?? 3.3);
    const pd = (b[0] >> 1) & 3;
    return {
      values: [
        { k: "code", label: "DAC code", v: dac },
        { k: "vout", label: "Output", v: pd ? 0 : r2((dac / 4096) * vdd, 3), unit: "V", d: 3 },
        { k: "pd", label: "Power-down", v: ["normal", "1 kΩ", "100 kΩ", "500 kΩ"][pd] },
        { k: "eeprom", label: "EEPROM (power-on) code", v: ee },
        { k: "busy", label: "EEPROM write", v: b[0] & 0x80 ? "idle" : "busy" },
      ],
    };
  },
  opts: [{ key: "vdd", label: "Vdd", type: "number", default: 3.3, unit: "V" }],
};

const bh1750: ChipDef = {
  id: "bh1750",
  name: "BH1750",
  what: "Ambient light (lux)",
  addrs: [0x23, 0x5c],
  // BH1750 has no registers: every written byte is an opcode. A 2-byte pure read is safe.
  async probe(d) {
    if (!d.plain) return null;
    const b = tryq(() => d.read(2));
    return b ? { confidence: "likely", note: "No ID register; recognised by address and a 2-byte read." } : null;
  },
  measureNote: "sends opcode 0x20 (one-time high-resolution measurement; the chip powers down after)",
  async measure(d) {
    d.write([0x20]);
    await sleep(180);
    const b = d.read(2);
    return { values: [{ k: "lux", label: "Illuminance", v: r2(((b[0] << 8) | b[1]) / 1.2, 1), unit: "lx", d: 1 }] };
  },
};

const pca9685AllCall: ChipDef = {
  id: "pca9685-allcall",
  name: "PCA9685 All Call",
  what: "Broadcast address of the PCA9685(s) on this bus",
  addrs: [0x70],
  async probe(_d, c) {
    return [...c.found.values()].includes("pca9685") ? { confidence: "likely", note: "PCA9685 answers 0x70 when MODE1.ALLCALL is set (the power-on default)." } : null;
  },
};

const tca9548: ChipDef = {
  id: "tca9548",
  name: "TCA9548A",
  what: "8-channel I²C multiplexer",
  addrs: range(0x70, 0x77),
  // The mux has a single control register and any written byte selects channels, so only
  // a bare receive-byte. At 0x76/0x77 this runs after the Bosch probe (see order below).
  async probe(d) {
    const m = tryq(() => d.recv());
    return m == null ? null : { confidence: "guess", note: "No ID register; recognised by address (HT16K33 LED drivers also live at 0x70–0x77)." };
  },
  async read(d) {
    const m = d.recv();
    const on = range(0, 7).filter((i) => (m >> i) & 1);
    return { values: [{ k: "mask", label: "Enabled channels", v: on.length ? on.join(", ") : "none" }] };
  },
  poll: false,
};

const mcp9808: ChipDef = {
  id: "mcp9808",
  name: "MCP9808",
  what: "Precision temperature",
  addrs: range(0x18, 0x1f),
  async probe(d) {
    if (tryq(() => d.u16be(0x06)) !== 0x0054) return null;
    const dev = tryq(() => d.u16be(0x07));
    if (dev == null || dev >> 8 !== 0x04) return null;
    return { confidence: "id", note: `Manufacturer 0x0054, device 0x${dev.toString(16)}` };
  },
  async read(d) {
    const v = d.u16be(0x05);
    let t = (v & 0x0fff) / 16;
    if (v & 0x1000) t -= 256;
    return { values: [deg("t", "Temperature", t, 3)] };
  },
};

const lis3dh: ChipDef = {
  id: "lis3dh",
  name: "LIS3DH",
  what: "3-axis accelerometer",
  addrs: [0x18, 0x19],
  async probe(d) {
    return tryq(() => d.u8(0x0f)) === 0x33 ? { confidence: "id", note: "WHO_AM_I 0x33" } : null;
  },
};

const ds2482: ChipDef = {
  id: "ds2482",
  name: "DS2482",
  what: "I²C → 1-Wire bridge",
  addrs: range(0x18, 0x1b),
  // Runs after MCP9808/LIS3DH: those pointer bytes (0x06/0x07/0x0F) aren't DS2482 command
  // codes, so they can't change its state. The bridge itself is only read here.
  async probe(d) {
    const st = tryq(() => d.recv());
    if (st == null || st & 0xe1) return null; // DIR/TSB/SBR/1WB clear when idle
    return { confidence: "likely", note: `Status 0x${st.toString(16)}. Bind the kernel ds2482 driver from the 1-Wire panel to use it.` };
  },
  async read(d) {
    const st = d.recv();
    const bits = ["1WB busy", "PPD presence", "SD short", "LL line high", "RST reset", "SBR", "TSB", "DIR"].filter((_, i) => (st >> i) & 1);
    return { values: [{ k: "status", label: "Status", v: `0x${st.toString(16).padStart(2, "0")} ${bits.join(", ")}` }] };
  },
  poll: false,
};

const hmc5883: ChipDef = {
  id: "hmc5883",
  name: "HMC5883L",
  what: "3-axis magnetometer",
  addrs: [0x1e],
  async probe(d) {
    const b = tryq(() => [d.u8(0x0a), d.u8(0x0b), d.u8(0x0c)]);
    return b && String.fromCharCode(...b) === "H43" ? { confidence: "id", note: "ID registers 'H43'" } : null;
  },
};

const adxl345: ChipDef = {
  id: "adxl345",
  name: "ADXL345",
  what: "3-axis accelerometer",
  addrs: [0x1d, 0x53],
  async probe(d) {
    return tryq(() => d.u8(0x00)) === 0xe5 ? { confidence: "id", note: "DEVID 0xE5" } : null;
  },
  async read(d) {
    if (!(d.u8(0x2d) & 0x08)) return { values: [], stale: true, note: "In standby (POWER_CTL.Measure = 0). Measure wakes it briefly." };
    return adxlRead(d);
  },
  measureNote: "sets POWER_CTL.Measure, reads, restores standby",
  async measure(d) {
    const pc = d.u8(0x2d);
    if (!(pc & 0x08)) {
      d.wb(0x2d, pc | 0x08);
      await sleep(30);
    }
    try {
      return adxlRead(d);
    } finally {
      if (!(pc & 0x08)) d.wb(0x2d, pc);
    }
  },
};

function adxlRead(d: Dev): Readout {
  const fmt = d.u8(0x31);
  const b = d.block(0x32, 6);
  const scale = fmt & 0x08 ? 0.0039 : (2 << (fmt & 3)) / 512; // g per LSB
  const ax = ["x", "y", "z"].map((n, i) => ({ k: "a" + n, label: `Accel ${n.toUpperCase()}`, v: r2(s16(b[i * 2] | (b[i * 2 + 1] << 8)) * scale, 3), unit: "g", d: 3 }));
  return { values: ax };
}

const veml7700: ChipDef = {
  id: "veml7700",
  name: "VEML7700",
  what: "Ambient light (lux)",
  addrs: [0x10],
  async probe(d) {
    const id = tryq(() => d.u16le(0x07));
    return id != null && (id & 0xff) === 0x81 ? { confidence: "id", note: `ID 0x${id.toString(16)}` } : null;
  },
  async read(d) {
    const cfg = d.u16le(0x00);
    if (cfg & 1) return { values: [], stale: true, note: "Shut down (ALS_SD = 1). Measure powers it up for one integration." };
    return vemlRead(d, cfg);
  },
  measureNote: "clears ALS_SD, waits one integration, restores the config",
  async measure(d) {
    const cfg = d.u16le(0x00);
    if (cfg & 1) {
      d.io.writeWord(d.addr, 0x00, cfg & ~1);
      await sleep(5 + veml7700It(cfg) * 1.2); // power-up + one integration with margin
    }
    try {
      return vemlRead(d, cfg & ~1);
    } finally {
      if (cfg & 1) d.io.writeWord(d.addr, 0x00, cfg);
    }
  },
};

function vemlRead(d: Dev, cfg: number): Readout {
  const als = d.u16le(0x04);
  const white = d.u16le(0x05);
  return { values: [{ k: "lux", label: "Illuminance", v: r2(veml7700Lux(als, cfg), 2), unit: "lx", d: 2 }, { k: "white", label: "White (raw)", v: white }] };
}

const ak8963: ChipDef = {
  id: "ak8963",
  name: "AK8963",
  what: "Magnetometer (inside MPU-9250, visible in bypass mode)",
  addrs: [0x0c, 0x0d],
  async probe(d) {
    return tryq(() => d.u8(0x00)) === 0x48 ? { confidence: "id", note: "WIA 0x48" } : null;
  },
};

const htu21d: ChipDef = {
  id: "htu21d",
  name: "HTU21D / Si7021",
  what: "Temperature / humidity",
  addrs: [0x40],
  // Runs first at 0x40: HTU21D/Si70xx are command-based (0xFE = soft reset), so the TI ID
  // probe (pointer 0xFE) must not reach them. They NACK pointer 0x00, which every
  // register-based chip at 0x40 (INA2xx, PCA9685) accepts; then "read user register" (0xE7)
  // confirms.
  async probe(d) {
    if (tryq(() => d.u16be(0x00)) != null) return null;
    const ur = tryq(() => d.u8(0xe7));
    if (ur == null) return null;
    let variant = "HTU21D / Si7021";
    if (d.plain) {
      const id = tryq(() => d.writeRead([0xfc, 0xc9], 6));
      const snb3 = id?.[0];
      const names: Record<number, string> = { 0x0d: "Si7013", 0x14: "Si7020", 0x15: "Si7021", 0x32: "HTU21D" };
      if (snb3 != null && names[snb3]) variant = names[snb3];
    }
    return { confidence: "likely", variant, note: `User register 0x${ur.toString(16)}` };
  },
  measureNote: "sends 0xF3 / 0xF5 (no-hold temperature and humidity measurements)",
  async measure(d) {
    const get = async (cmd: number, ms: number) => {
      d.write([cmd]);
      await sleep(ms);
      const b = d.read(3);
      if (crc8sens([b[0], b[1]], 0x00) !== b[2]) throw new Error("CRC mismatch");
      return ((b[0] << 8) | b[1]) & 0xfffc;
    };
    const t = -46.85 + (175.72 * (await get(0xf3, 55))) / 65536;
    const h = -6 + (125 * (await get(0xf5, 20))) / 65536;
    return { values: [deg("t", "Temperature", t), { k: "rh", label: "Humidity", v: r2(Math.max(0, Math.min(100, h)), 1), unit: "%RH", d: 1 }] };
  },
};

const tiIna: ChipDef = {
  id: "ti",
  name: "TI INA226",
  what: "Current / power monitor",
  addrs: range(0x40, 0x4f),
  async probe(d) {
    if (tryq(() => d.u16be(0xfe)) !== 0x5449) return null;
    const die = tryq(() => d.u16be(0xff)) ?? 0;
    const names: Record<number, string> = { 0x2260: "INA226", 0x2270: "INA260", 0x3220: "INA3221", 0x1050: "HDC1080" };
    const v = names[die] || `TI part (die 0x${die.toString(16)})`;
    return { confidence: "id", variant: v, note: `Manufacturer 0x5449 (TI), die 0x${die.toString(16)}`, data: { die } };
  },
  async read(d, s) {
    const die = s.probe.data?.die;
    if (die === 0x2260) {
      const shunt = d.i16be(0x01) * 2.5e-6;
      const bus = d.u16be(0x02) * 1.25e-3;
      const R = Number(s.opts.shunt ?? 0.1);
      const I = shunt / R;
      return { values: [{ k: "v", label: "Bus", v: r2(bus, 3), unit: "V", d: 3 }, { k: "i", label: "Current", v: r2(I * 1000, 2), unit: "mA", d: 2 }, { k: "p", label: "Power", v: r2(bus * I * 1000, 1), unit: "mW", d: 1 }, { k: "vs", label: "Shunt", v: r2(shunt * 1e3, 4), unit: "mV", d: 4 }] };
    }
    if (die === 0x2270) {
      const I = d.i16be(0x01) * 1.25e-3;
      const bus = d.u16be(0x02) * 1.25e-3;
      const P = d.u16be(0x03) * 10e-3;
      return { values: [{ k: "v", label: "Bus", v: r2(bus, 3), unit: "V", d: 3 }, { k: "i", label: "Current", v: r2(I * 1000, 1), unit: "mA", d: 1 }, { k: "p", label: "Power", v: r2(P * 1000, 0), unit: "mW", d: 0 }] };
    }
    if (die === 0x3220) {
      const vals: Val[] = [];
      const R = Number(s.opts.shunt ?? 0.1);
      for (let ch = 0; ch < 3; ch++) {
        const sh = (d.i16be(1 + ch * 2) >> 3) * 40e-6;
        const bus = (d.i16be(2 + ch * 2) >> 3) * 8e-3;
        vals.push({ k: `v${ch + 1}`, label: `CH${ch + 1} bus`, v: r2(bus, 3), unit: "V", d: 3 }, { k: `i${ch + 1}`, label: `CH${ch + 1} current`, v: r2((sh / R) * 1000, 1), unit: "mA", d: 1 });
      }
      return { values: vals };
    }
    return { values: [], note: "Identified by ID registers; no decoder for this part yet." };
  },
  measureNote: "HDC1080 only: writes pointer 0x00 (trigger), reads 4 bytes",
  async measure(d, s) {
    if (s.probe.data?.die !== 0x1050) return tiIna.read!(d, s);
    d.write([0x00]);
    await sleep(20);
    const b = d.read(4);
    const t = (((b[0] << 8) | b[1]) / 65536) * 165 - 40;
    const h = (((b[2] << 8) | b[3]) / 65536) * 100;
    return { values: [deg("t", "Temperature", t), { k: "rh", label: "Humidity", v: r2(h, 1), unit: "%RH", d: 1 }] };
  },
  opts: [{ key: "shunt", label: "Shunt resistor", type: "number", default: 0.1, unit: "Ω" }],
};

const ads1x15: ChipDef = {
  id: "ads1x15",
  name: "ADS1115",
  what: "4-channel ADC",
  addrs: range(0x48, 0x4b),
  async probe(d) {
    const cfg = tryq(() => d.u16be(0x01));
    const lo = tryq(() => d.u16be(0x02));
    const hi = tryq(() => d.u16be(0x03));
    if (cfg == null || lo == null || hi == null) return null;
    if (lo === 0x8000 && (hi === 0x7fff || hi === 0x7ff0)) return { confidence: "likely", variant: hi === 0x7ff0 ? "ADS1015" : "ADS1115", note: "Comparator thresholds at power-on defaults." };
    if ((cfg & 0x7fff) === 0x0583) return { confidence: "likely", variant: "ADS1115", note: "Config register at its power-on default." };
    return null;
  },
  async read(d, s) {
    const cfg = d.u16be(0x01);
    const v15 = s.probe.variant === "ADS1015";
    if (cfg & 0x0100) return { values: [{ k: "cfg", label: "Config", v: "0x" + cfg.toString(16) }], stale: true, note: "Single-shot mode: the conversion register holds the last result only. Measure converts all four inputs." };
    const raw = d.u16be(0x00);
    return { values: [{ k: "v", label: ADS_MUX[(cfg >> 12) & 7], v: r2(adsVolts(raw, cfg, v15), 4), unit: "V", d: 4 }, { k: "fsr", label: "Range", v: `±${ADS_FSR[(cfg >> 9) & 7]} V` }] };
  },
  measureNote: "writes the config register for 4 single-shot conversions (AIN0–3 vs GND, ±4.096 V), then restores it",
  async measure(d, s) {
    const orig = d.u16be(0x01);
    const v15 = s.probe.variant === "ADS1015";
    const vals: Val[] = [];
    try {
      for (let ch = 0; ch < 4; ch++) {
        // OS=1, MUX=1xx (single-ended), PGA=001 (±4.096), MODE=1, DR=100, comparator off.
        const cfg = 0x8000 | ((4 + ch) << 12) | (1 << 9) | 0x0100 | (4 << 5) | 0x0003;
        d.io.writeWord(d.addr, 0x01, ((cfg & 0xff) << 8) | (cfg >> 8));
        let ready = false;
        for (let i = 0; i < 20 && !ready; i++) {
          await sleep(3);
          ready = !!(d.u16be(0x01) & 0x8000);
        }
        vals.push({ k: `ain${ch}`, label: `AIN${ch}`, v: r2(adsVolts(d.u16be(0x00), cfg, v15), 4), unit: "V", d: 4 });
      }
    } finally {
      d.io.writeWord(d.addr, 0x01, ((orig & 0xff) << 8) | (orig >> 8));
    }
    return { values: vals };
  },
};

const tmp102: ChipDef = {
  id: "tmp102",
  name: "TMP102",
  what: "Temperature",
  addrs: range(0x48, 0x4b),
  async probe(d) {
    const cfg = tryq(() => d.u16be(0x01));
    const lo = tryq(() => d.u16be(0x02));
    if (cfg == null || lo == null) return null;
    return (cfg & 0x6000) === 0x6000 && (cfg & 0x000f) === 0 && (lo & 0x0f) === 0 ? { confidence: "likely", note: `Config 0x${cfg.toString(16)} (R1/R0 read as 1)` } : null;
  },
  async read(d) {
    const cfg = d.u16be(0x01);
    const raw = d.i16be(0x00);
    const t = cfg & 0x0010 ? (raw >> 3) * 0.0625 : (raw >> 4) * 0.0625;
    return { values: [deg("t", "Temperature", t, 4)], stale: !!(cfg & 0x0100), note: cfg & 0x0100 ? "Shutdown mode: value is from the last conversion." : undefined };
  },
};

const lm75: ChipDef = {
  id: "lm75",
  name: "LM75",
  what: "Temperature",
  addrs: range(0x48, 0x4f),
  async probe(d) {
    const cfg = tryq(() => d.u8(0x01));
    const hyst = tryq(() => d.u16be(0x02));
    const os = tryq(() => d.u16be(0x03));
    if (cfg == null || hyst == null || os == null) return null;
    if ((cfg & 0xe0) !== 0 || hyst & 0x7f || os & 0x7f) return null;
    const def = hyst === 0x4b00 && os === 0x5000;
    return { confidence: def ? "likely" : "guess", note: def ? "Thresholds at power-on defaults (75/80 °C)." : `Thresholds ${s16(hyst) / 256}/${s16(os) / 256} °C` };
  },
  async read(d) {
    return { values: [deg("t", "Temperature", d.i16be(0x00) / 256, 3)] };
  },
};

const ina219: ChipDef = {
  id: "ina219",
  name: "INA219",
  what: "Current / power monitor",
  addrs: range(0x40, 0x4f),
  // No ID register. Tried last in this range, after everything that has one.
  async probe(d) {
    const cfg = tryq(() => d.u16be(0x00));
    const bus = tryq(() => d.u16be(0x02));
    if (cfg == null || bus == null || cfg & 0x8000 || bus & 0x0004) return null;
    return { confidence: cfg === 0x399f ? "likely" : "guess", note: cfg === 0x399f ? "Config at power-on default 0x399F." : `Config 0x${cfg.toString(16)} (INA219 has no ID register)` };
  },
  async read(d, s) {
    const shunt = d.i16be(0x01) * 10e-6;
    const b = d.u16be(0x02);
    const bus = (b >> 3) * 4e-3;
    const R = Number(s.opts.shunt ?? 0.1);
    const I = shunt / R;
    return {
      values: [{ k: "v", label: "Bus", v: r2(bus, 3), unit: "V", d: 3 }, { k: "i", label: "Current", v: r2(I * 1000, 2), unit: "mA", d: 2 }, { k: "p", label: "Power", v: r2(bus * I * 1000, 1), unit: "mW", d: 1 }, { k: "vs", label: "Shunt", v: r2(shunt * 1e3, 3), unit: "mV", d: 3 }],
      note: b & 1 ? "Overflow: the shunt voltage exceeds the configured range." : undefined,
    };
  },
  opts: [{ key: "shunt", label: "Shunt resistor", type: "number", default: 0.1, unit: "Ω" }],
};

const tsl2561: ChipDef = {
  id: "tsl2561",
  name: "TSL2561",
  what: "Ambient light (lux)",
  addrs: [0x29, 0x39, 0x49],
  async probe(d) {
    const id = tryq(() => d.u8(0x8a));
    if (id == null) return null;
    const part = id >> 4;
    if (![0, 1, 4, 5].includes(part)) return null;
    return { confidence: "id", variant: part & 1 ? "TSL2561" : "TSL2560", note: `ID 0x${id.toString(16)}` };
  },
  async read(d) {
    if ((d.u8(0x80) & 3) !== 3) return { values: [], stale: true, note: "Powered down. Measure powers it up for one integration." };
    return tslRead(d);
  },
  measureNote: "powers the chip up (CONTROL=0x03), waits one integration, powers it back down",
  async measure(d) {
    const was = d.u8(0x80) & 3;
    if (was !== 3) {
      d.wb(0x80, 0x03);
      const t = d.u8(0x81) & 3;
      await sleep([14, 101, 402, 402][t] + 20);
    }
    try {
      return tslRead(d);
    } finally {
      if (was !== 3) d.wb(0x80, 0x00);
    }
  },
};

function tslRead(d: Dev): Readout {
  const timing = d.u8(0x81);
  const ch0 = d.u16le(0xac);
  const ch1 = d.u16le(0xae);
  const lux = tsl2561Lux(ch0, ch1, timing & 3, !!(timing & 0x10));
  return { values: [{ k: "lux", label: "Illuminance", v: lux == null ? "saturated" : r2(lux, 1), unit: lux == null ? undefined : "lx", d: 1 }, { k: "ch0", label: "Broadband", v: ch0 }, { k: "ch1", label: "IR", v: ch1 }] };
}

const tcs34725: ChipDef = {
  id: "tcs34725",
  name: "TCS34725",
  what: "RGB colour sensor",
  addrs: [0x29],
  async probe(d) {
    const id = tryq(() => d.u8(0x92));
    return id === 0x44 || id === 0x4d ? { confidence: "id", variant: id === 0x44 ? "TCS34725" : "TCS34727", note: `ID 0x${id.toString(16)}` } : null;
  },
  async read(d) {
    if ((d.u8(0x80) & 3) !== 3) return { values: [], stale: true, note: "RGBC not enabled. Measure enables it for one integration." };
    return tcsRead(d);
  },
  measureNote: "sets ENABLE=PON|AEN, waits one integration, restores ENABLE",
  async measure(d) {
    const en = d.u8(0x80);
    if ((en & 3) !== 3) {
      d.wb(0x80, en | 0x01);
      await sleep(3);
      d.wb(0x80, en | 0x03);
      await sleep((256 - d.u8(0x81)) * 2.4 + 10);
    }
    try {
      return tcsRead(d);
    } finally {
      if ((en & 3) !== 3) d.wb(0x80, en);
    }
  },
};

function tcsRead(d: Dev): Readout {
  const b = d.block(0xb4, 8);
  const w = (i: number) => b[i] | (b[i + 1] << 8);
  const [c, r, g, bl] = [w(0), w(2), w(4), w(6)];
  const hexc = c ? "#" + [r, g, bl].map((x) => Math.min(255, Math.round((x / c) * 255)).toString(16).padStart(2, "0")).join("") : "#000000";
  return { values: [{ k: "c", label: "Clear", v: c }, { k: "r", label: "Red", v: r }, { k: "g", label: "Green", v: g }, { k: "b", label: "Blue", v: bl }, { k: "rgb", label: "Colour", v: hexc }] };
}

const tsl2591: ChipDef = {
  id: "tsl2591",
  name: "TSL2591",
  what: "Ambient light",
  addrs: [0x29],
  async probe(d) {
    return tryq(() => d.u8(0xb2)) === 0x50 ? { confidence: "id", note: "ID 0x50" } : null;
  },
};

const vl53l0x: ChipDef = {
  id: "vl53l0x",
  name: "VL53L0X",
  what: "Time-of-flight distance",
  addrs: [0x29],
  async probe(d) {
    return tryq(() => d.u8(0xc0)) === 0xee ? { confidence: "id", note: "Model ID 0xEE. Ranging needs ST's init sequence (not implemented)." } : null;
  },
};

const apds9960: ChipDef = {
  id: "apds9960",
  name: "APDS-9960",
  what: "Gesture / proximity / colour",
  addrs: [0x39],
  async probe(d) {
    const id = tryq(() => d.u8(0x92));
    return id === 0xab || id === 0xa8 ? { confidence: "id", note: `ID 0x${id.toString(16)}` } : null;
  },
};

const sht4x: ChipDef = {
  id: "sht4x",
  name: "SHT4x",
  what: "Temperature / humidity",
  addrs: [0x44, 0x45, 0x46],
  // Single-byte commands; 0x89 = read serial number (no state change).
  async probe(d) {
    if (!d.plain) return null;
    const ok = tryq(() => d.write([0x89]));
    if (ok === null) return null;
    await sleep(2);
    const b = tryq(() => d.read(6));
    const w = b && sensWords(b);
    if (!w || w.length !== 2) return null;
    return { confidence: "id", note: `Serial ${((w[0] << 16) | w[1]) >>> 0}` };
  },
  measureNote: "sends 0xFD (high-precision measurement)",
  async measure(d) {
    d.write([0xfd]);
    await sleep(10);
    const w = sensWords(d.read(6));
    if (!w) throw new Error("SHT4x CRC mismatch");
    return { values: [deg("t", "Temperature", sht3xT(w[0])), { k: "rh", label: "Humidity", v: r2(sht4xH(w[1]), 1), unit: "%RH", d: 1 }] };
  },
};

const sht3x: ChipDef = {
  id: "sht3x",
  name: "SHT3x",
  what: "Temperature / humidity",
  addrs: [0x44, 0x45],
  // 16-bit commands; 0xF32D = read status register.
  async probe(d) {
    if (!d.plain) return null;
    if (tryq(() => d.write([0xf3, 0x2d])) === null) return null;
    await sleep(1);
    const b = tryq(() => d.read(3));
    const w = b && sensWords(b);
    if (!w) return null;
    return { confidence: "likely", note: `Status 0x${w[0].toString(16)} (CRC valid)` };
  },
  measureNote: "sends 0x2400 (single-shot, high repeatability)",
  async measure(d) {
    d.write([0x24, 0x00]);
    await sleep(20);
    const w = sensWords(d.read(6));
    if (!w) throw new Error("SHT3x CRC mismatch");
    return { values: [deg("t", "Temperature", sht3xT(w[0])), { k: "rh", label: "Humidity", v: r2(sht3xH(w[1]), 1), unit: "%RH", d: 1 }] };
  },
};

const pca9685: ChipDef = {
  id: "pca9685",
  name: "PCA9685",
  what: "16-channel PWM / servo driver",
  addrs: range(0x40, 0x7f),
  async probe(d) {
    const r = tryq(() => [d.u8(0x00), d.u8(0x01), d.u8(0x02), d.u8(0x03), d.u8(0x04), d.u8(0x05), d.u8(0xfe)]);
    if (!r) return null;
    const [m1, m2, s1, s2, s3, ac, pre] = r;
    if (m2 & 0xe0 || [s1, s2, s3, ac].some((x) => x & 1) || pre < 3) return null;
    const defaults = [s1 === 0xe2, s2 === 0xe4, s3 === 0xe8, ac === 0xe0].filter(Boolean).length;
    if (defaults < 2) return null;
    return { confidence: "likely", note: `MODE1 0x${m1.toString(16)}, sub-addresses at defaults (${defaults}/4)` };
  },
  async read(d) {
    const m1 = d.u8(0x00);
    const pre = d.u8(0xfe);
    const regs = m1 & 0x20 ? d.block(0x06, 64) : range(0x06, 0x45).map((r) => d.u8(r));
    const channels = range(0, 15).map((ch) => {
      const b = regs.slice(ch * 4, ch * 4 + 4);
      const on = b[0] | ((b[1] & 0x0f) << 8);
      const off = b[2] | ((b[3] & 0x0f) << 8);
      const fullOn = !!(b[1] & 0x10), fullOff = !!(b[3] & 0x10);
      const duty = fullOff ? 0 : fullOn ? 100 : r2((((off - on + 4096) % 4096) / 4096) * 100, 1);
      return { ch, duty, on, off, full: fullOff ? ("off" as const) : fullOn ? ("on" as const) : undefined };
    });
    return {
      values: [{ k: "freq", label: "PWM frequency", v: r2(25e6 / (4096 * (pre + 1)), 1), unit: "Hz", d: 1 }, { k: "sleep", label: "Oscillator", v: m1 & 0x10 ? "sleeping" : "running" }],
      channels,
      note: "Frequency assumes the internal 25 MHz oscillator.",
    };
  },
};

const at24: ChipDef = {
  id: "at24",
  name: "EEPROM (24Cxx)",
  what: "Serial EEPROM",
  addrs: range(0x50, 0x57),
  async probe(d) {
    return tryq(() => d.u8(0x00)) == null ? null : { confidence: "likely", note: "EEPROMs have no ID register; recognised by address." };
  },
  async read(d, s) {
    const two = Number(s.opts.size ?? 256) > 2048;
    const head = two ? (d.plain ? d.writeRead([0, 0], 64) : null) : d.block(0, 64);
    if (!head) return { values: [], note: "2-byte-address EEPROMs need a plain-I²C adapter." };
    const hat = parseHatEeprom(head);
    const values: Val[] = [];
    if (hat) values.push({ k: "hat", label: "Pi HAT EEPROM", v: [hat.vendor, hat.product].filter(Boolean).join(" · ") || `v${hat.version}, ${hat.atoms} atoms` });
    const blank = head.every((b) => b === 0xff);
    values.push({ k: "first", label: "First bytes", v: blank ? "blank (all 0xFF)" : head.slice(0, 16).map((b) => b.toString(16).padStart(2, "0")).join(" ") });
    return { values, hex: { start: 0, bytes: head } };
  },
  poll: false,
  opts: [
    {
      key: "size",
      label: "Size",
      type: "select",
      default: 256,
      choices: [
        { v: 128, label: "24C01 (128 B)" },
        { v: 256, label: "24C02 (256 B)" },
        { v: 4096, label: "24C32 (4 KiB, 2-byte address)" },
        { v: 8192, label: "24C64 (8 KiB)" },
        { v: 16384, label: "24C128 (16 KiB)" },
        { v: 32768, label: "24C256 (32 KiB)" },
        { v: 65536, label: "24C512 (64 KiB)" },
      ],
    },
  ],
};

const ccs811: ChipDef = {
  id: "ccs811",
  name: "CCS811",
  what: "eCO₂ / TVOC gas sensor",
  addrs: [0x5a, 0x5b],
  async probe(d) {
    return tryq(() => d.u8(0x20)) === 0x81 ? { confidence: "id", note: "HW_ID 0x81" } : null;
  },
};

const mlx90614: ChipDef = {
  id: "mlx90614",
  name: "MLX90614",
  what: "Infrared thermometer",
  addrs: [0x5a],
  async probe(d) {
    const ta = tryq(() => d.u16le(0x06));
    const ad = tryq(() => d.u16le(0x2e));
    if (ta == null || ad == null) return null;
    const t = ta * 0.02 - 273.15;
    return (ad & 0x7f) === d.addr && t > -40 && t < 125 ? { confidence: "likely", note: "SMBus address register matches; ambient in range." } : null;
  },
  async read(d) {
    return { values: [deg("tobj", "Object", d.u16le(0x07) * 0.02 - 273.15), deg("ta", "Ambient", d.u16le(0x06) * 0.02 - 273.15)] };
  },
};

const max1704x: ChipDef = {
  id: "max1704x",
  name: "MAX17048",
  what: "Li-ion fuel gauge",
  addrs: [0x36],
  async probe(d) {
    const ver = tryq(() => d.u16be(0x08));
    if (ver == null) return null;
    if ((ver & 0xfff0) === 0x0010) return { confidence: "likely", variant: "MAX17048/49", note: `Version 0x${ver.toString(16)}` };
    if (ver === 0x0002 || ver === 0x0003) return { confidence: "likely", variant: "MAX17043/44", note: `Version 0x${ver.toString(16)}` };
    return null;
  },
  async read(d, s) {
    const v = d.u16be(0x02) * 78.125e-6;
    const soc = d.u16be(0x04) / 256;
    const vals: Val[] = [{ k: "v", label: "Cell", v: r2(v, 3), unit: "V", d: 3 }, { k: "soc", label: "Charge", v: r2(soc, 1), unit: "%", d: 1 }];
    if (s.probe.variant?.startsWith("MAX17048")) vals.push({ k: "rate", label: "Rate", v: r2(s16(d.u16be(0x16)) * 0.208, 1), unit: "%/h", d: 1 });
    return { values: vals };
  },
};

const scd4x: ChipDef = {
  id: "scd4x",
  name: "SCD4x",
  what: "CO₂ / temperature / humidity",
  addrs: [0x62],
  // After mcp4725 (pure read) has ruled out a DAC at this address: SCD4x NACKs a bare read.
  async probe(d) {
    if (!d.plain) return null;
    if (tryq(() => d.write([0x36, 0x82])) !== null) {
      await sleep(2);
      const w = tryq(() => sensWords(d.read(9)));
      if (w && w.length === 3) return { confidence: "id", note: `Serial 0x${w.map((x) => x.toString(16).padStart(4, "0")).join("")}`, data: { periodic: false } };
    }
    // In periodic mode only a few commands are accepted; get_data_ready_status is one.
    if (tryq(() => d.write([0xe4, 0xb8])) === null) return null;
    await sleep(2);
    const w = tryq(() => sensWords(d.read(3)));
    return w && w.length === 1 ? { confidence: "likely", note: "Periodic measurement running.", data: { periodic: true } } : null;
  },
  async read(d, s) {
    // Reading a result is non-destructive state-wise (it only clears data-ready), so allow it
    // when another agent or a previous auto-measure left periodic mode on.
    if (!s.probe.data?.periodic && !s.cache?.periodic) return { values: [], stale: true, note: "Idle. Measure takes a single shot (5 s, SCD41 only); auto-measure runs periodic mode." };
    return scdRead(d, s);
  },
  measureNote: "SCD41 single shot (0x219D, ~5 s) when idle; reads the latest sample when periodic mode is on",
  async measure(d, s) {
    const periodic = s.probe.data?.periodic || s.cache?.periodic;
    if (!periodic) {
      d.write([0x21, 0x9d]);
      await sleep(5100);
    }
    return scdRead(d, s);
  },
  async autoStart(d, s) {
    d.write([0x21, 0xb1]);
    s.cache = { ...s.cache, periodic: true };
  },
  async autoStop(d, s) {
    d.write([0x3f, 0x86]);
    await sleep(500);
    s.cache = { ...s.cache, periodic: false };
    s.probe.data = { ...s.probe.data, periodic: false };
  },
};

async function scdRead(d: Dev, s: ChipState): Promise<Readout> {
  d.write([0xe4, 0xb8]);
  await sleep(2);
  const rdy = sensWords(d.read(3));
  if (!rdy) throw new Error("SCD4x CRC mismatch");
  if (!(rdy[0] & 0x07ff)) return { ...(s.cache?.last || { values: [] }), stale: true, note: "No new sample yet (periodic mode produces one every 5 s)." };
  d.write([0xec, 0x05]);
  await sleep(2);
  const w = sensWords(d.read(9));
  if (!w) throw new Error("SCD4x CRC mismatch");
  const out = { values: [{ k: "co2", label: "CO₂", v: w[0], unit: "ppm", d: 0 }, deg("t", "Temperature", sht3xT(w[1])), { k: "rh", label: "Humidity", v: r2(sht3xH(w[2]), 1), unit: "%RH", d: 1 }] };
  s.cache = { ...s.cache, last: out };
  return out;
}

const MPU_IDS: Record<number, string> = { 0x68: "MPU-6050", 0x70: "MPU-6500", 0x71: "MPU-9250", 0x73: "MPU-9255", 0x19: "MPU-6886", 0x12: "ICM-20602", 0x98: "ICM-20689", 0xaf: "ICM-20608" };

const mpu: ChipDef = {
  id: "mpu",
  name: "MPU-6050",
  what: "6-axis IMU",
  addrs: [0x68, 0x69],
  async probe(d) {
    const id = tryq(() => d.u8(0x75));
    if (id == null || !MPU_IDS[id]) return null;
    return { confidence: "id", variant: MPU_IDS[id], note: `WHO_AM_I 0x${id.toString(16)}`, data: { id } };
  },
  async read(d, s) {
    if (d.u8(0x6b) & 0x40) return { values: [], stale: true, note: "Asleep (PWR_MGMT_1.SLEEP, the power-on state). Measure wakes it briefly." };
    return mpuRead(d, s);
  },
  measureNote: "clears PWR_MGMT_1.SLEEP, reads, restores PWR_MGMT_1",
  async measure(d, s) {
    const pm = d.u8(0x6b);
    if (pm & 0x40) {
      d.wb(0x6b, 0x01);
      await sleep(100);
    }
    try {
      return mpuRead(d, s);
    } finally {
      if (pm & 0x40) d.wb(0x6b, pm);
    }
  },
};

function mpuRead(d: Dev, s: ChipState): Readout {
  const b = d.block(0x3b, 14);
  const w = (i: number) => s16((b[i] << 8) | b[i + 1]);
  const afs = (d.u8(0x1c) >> 3) & 3;
  const gfs = (d.u8(0x1b) >> 3) & 3;
  const as = 16384 >> afs;
  const gs = 131 / (1 << gfs);
  const id = s.probe.data?.id;
  const t = id === 0x68 ? w(6) / 340 + 36.53 : w(6) / 333.87 + 21;
  return {
    values: [
      ...["x", "y", "z"].map((n, i) => ({ k: "a" + n, label: `Accel ${n.toUpperCase()}`, v: r2(w(i * 2) / as, 3), unit: "g", d: 3 })),
      ...["x", "y", "z"].map((n, i) => ({ k: "g" + n, label: `Gyro ${n.toUpperCase()}`, v: r2(w(8 + i * 2) / gs, 2), unit: "°/s", d: 2 })),
      deg("t", "Die temperature", t, 1),
    ],
  };
}

const icm20948: ChipDef = {
  id: "icm20948",
  name: "ICM-20948",
  what: "9-axis IMU",
  addrs: [0x68, 0x69],
  async probe(d) {
    return tryq(() => d.u8(0x00)) === 0xea ? { confidence: "id", note: "WHO_AM_I 0xEA" } : null;
  },
  async read(d) {
    if ((d.u8(0x7f) >> 4) & 3) return { values: [], stale: true, note: "Register bank ≠ 0 (someone else is using it); not switching banks without a Measure." };
    if (d.u8(0x06) & 0x40) return { values: [], stale: true, note: "Asleep (power-on state). Measure wakes it briefly." };
    return icmRead(d);
  },
  measureNote: "selects bank 0, clears PWR_MGMT_1.SLEEP, reads, restores both",
  async measure(d) {
    const bank = d.u8(0x7f);
    if (bank & 0x30) d.wb(0x7f, 0);
    const pm = d.u8(0x06);
    if (pm & 0x40) {
      d.wb(0x06, 0x01);
      await sleep(50);
    }
    try {
      return icmRead(d);
    } finally {
      if (pm & 0x40) d.wb(0x06, pm);
      if (bank & 0x30) d.wb(0x7f, bank);
    }
  },
};

function icmRead(d: Dev): Readout {
  const b = d.block(0x2d, 14);
  const w = (i: number) => s16((b[i] << 8) | b[i + 1]);
  return {
    values: [
      ...["x", "y", "z"].map((n, i) => ({ k: "a" + n, label: `Accel ${n.toUpperCase()}`, v: r2(w(i * 2) / 16384, 3), unit: "g", d: 3 })),
      ...["x", "y", "z"].map((n, i) => ({ k: "g" + n, label: `Gyro ${n.toUpperCase()}`, v: r2(w(6 + i * 2) / 131, 2), unit: "°/s", d: 2 })),
      deg("t", "Die temperature", (w(12) - 21) / 333.87 + 21, 1),
    ],
    note: "Scales assume the power-on ranges (±2 g, ±250 °/s); range settings live in bank 2.",
  };
}

const rtc: ChipDef = {
  id: "rtc",
  name: "DS3231",
  what: "Real-time clock",
  addrs: [0x68],
  async probe(d) {
    const r = tryq(() => d.block(0x00, 0x13));
    if (!r) return null;
    if (!isBcd(r[0] & 0x7f) || !isBcd(r[1]) || !isBcd(r[4]) || !isBcd(r[5] & 0x1f) || !isBcd(r[6])) return null;
    if (!rtcDecode(r).date) return null;
    const ds3231 = (r[0x12] & 0x3f) === 0 && (r[0x0f] & 0x70) === 0 && s8(r[0x11]) > -40 && s8(r[0x11]) < 85;
    return { confidence: "likely", variant: ds3231 ? "DS3231" : "DS1307 (or compatible)", note: "Time registers hold valid BCD.", data: { ds3231 } };
  },
  async read(d, s) {
    const r = d.block(0x00, s.probe.data?.ds3231 ? 0x13 : 0x08);
    const t = rtcDecode(r);
    const vals: Val[] = [{ k: "time", label: "RTC time (UTC)", v: t.date ? t.date.toISOString().replace("T", " ").slice(0, 19) : "invalid" }];
    if (t.date) vals.push({ k: "drift", label: "vs system clock", v: r2((t.date.getTime() - Date.now()) / 1000, 0), unit: "s", d: 0 });
    let note: string | undefined;
    if (s.probe.data?.ds3231) {
      vals.push(deg("t", "Temperature", s8(r[0x11]) + (r[0x12] >> 6) * 0.25));
      if (r[0x0f] & 0x80) note = "Oscillator-stop flag set: the clock lost power at some point; time may be wrong.";
    } else if (t.halted) note = "Clock halted (CH bit set): the oscillator isn't running.";
    return { values: vals, note };
  },
};

const bosch: ChipDef = {
  id: "bosch",
  name: "BME280",
  what: "Temperature / humidity / pressure",
  addrs: [0x76, 0x77],
  async probe(d) {
    const id = tryq(() => d.u8(0xd0));
    const names: Record<number, string> = { 0x60: "BME280", 0x58: "BMP280", 0x57: "BMP280", 0x56: "BMP280", 0x61: "BME680", 0x55: "BMP180" };
    if (id == null || !names[id]) return null;
    return { confidence: "id", variant: names[id], note: `Chip ID 0x${id.toString(16)}`, data: { id } };
  },
  async read(d, s) {
    const id = s.probe.data?.id;
    if (id === 0x55) return { values: [], stale: true, note: "BMP180 only converts on command. Use Measure." };
    if (id === 0x61) return bme680Read(d, s, false);
    const mode = d.u8(0xf4) & 3;
    if (mode !== 3) {
      const last = await bme280Read(d, s);
      return { ...last, stale: true, note: "Sleep/forced mode: values are from the last conversion. Measure runs a forced conversion." };
    }
    return bme280Read(d, s);
  },
  measureNote: "forced-mode conversion (writes ctrl_hum/ctrl_meas; BME680 also runs the gas heater at 320 °C for 150 ms)",
  async measure(d, s) {
    const id = s.probe.data?.id;
    if (id === 0x55) return bmp180Measure(d, s);
    if (id === 0x61) return bme680Read(d, s, true);
    const meas = d.u8(0xf4);
    if ((meas & 3) === 3) return bme280Read(d, s); // already in normal mode
    if (id === 0x60) d.wb(0xf2, 0x01); // osrs_h ×1 (latched by the ctrl_meas write)
    d.wb(0xf4, (1 << 5) | (1 << 2) | 1); // osrs_t ×1, osrs_p ×1, forced
    await sleep(10);
    for (let i = 0; i < 10 && d.u8(0xf3) & 0x08; i++) await sleep(5);
    return bme280Read(d, s);
  },
};

async function bme280Read(d: Dev, s: ChipState): Promise<Readout> {
  const hum = s.probe.data?.id === 0x60;
  if (!s.cache?.cal) s.cache = { ...s.cache, cal: bme280Cal(d.block(0x88, 26), hum ? d.block(0xe1, 7) : undefined) };
  const b = d.block(0xf7, hum ? 8 : 6);
  const adcP = (b[0] << 12) | (b[1] << 4) | (b[2] >> 4);
  const adcT = (b[3] << 12) | (b[4] << 4) | (b[5] >> 4);
  if (adcT === 0x80000) return { values: [], stale: true, note: "No conversion since power-on. Measure runs one." };
  const r = bme280Comp(s.cache.cal, adcT, adcP, hum ? (b[6] << 8) | b[7] : undefined);
  const vals: Val[] = [deg("t", "Temperature", r.T)];
  if (r.H != null) vals.push({ k: "rh", label: "Humidity", v: r2(r.H, 1), unit: "%RH", d: 1 });
  if (r.P != null) vals.push({ k: "p", label: "Pressure", v: r2(r.P / 100, 2), unit: "hPa", d: 2 });
  return { values: vals };
}

async function bme680Read(d: Dev, s: ChipState, measure: boolean): Promise<Readout> {
  if (!s.cache?.cal680) {
    const lo = d.block(0x00, 5);
    const a = d.block(0x8a, 23);
    const e = d.block(0xe1, 16);
    const reg = (r: number) => (r < 5 ? lo[r] : r >= 0xe1 ? e[r - 0xe1] : a[r - 0x8a]);
    s.cache = { ...s.cache, cal680: bme680Cal(reg) };
  }
  const c: Bme680Cal = s.cache.cal680;
  if (measure) {
    d.wb(0x72, 0x01); // osrs_h ×1
    d.wb(0x5a, bme680HeatRes(c, 320, s.cache.lastT ?? 25)); // res_heat_0
    d.wb(0x64, 0x65); // gas_wait_0 = 150 ms
    d.wb(0x71, 0x10); // run_gas, heater profile 0
    d.wb(0x74, (2 << 5) | (1 << 2) | 1); // osrs_t ×2, osrs_p ×1, forced
    await sleep(200);
    for (let i = 0; i < 20 && !(d.u8(0x1d) & 0x80); i++) await sleep(10);
  }
  const b = d.block(0x1d, 15);
  const adcP = (b[2] << 12) | (b[3] << 4) | (b[4] >> 4);
  const adcT = (b[5] << 12) | (b[6] << 4) | (b[7] >> 4);
  const adcH = (b[8] << 8) | b[9];
  if (adcT === 0x80000) return { values: [], stale: true, note: "No conversion yet. Measure runs one (BME680 has no free-running mode)." };
  const gasValid = !!(b[14] & 0x20) && !!(b[14] & 0x10);
  const r = bme680Comp(c, adcT, adcP, adcH, gasValid ? { adc: (b[13] << 2) | (b[14] >> 6), range: b[14] & 0x0f } : undefined);
  s.cache.lastT = r.T;
  const vals: Val[] = [deg("t", "Temperature", r.T), { k: "rh", label: "Humidity", v: r2(r.H, 1), unit: "%RH", d: 1 }, { k: "p", label: "Pressure", v: r2(r.P / 100, 2), unit: "hPa", d: 2 }];
  if (r.G != null) vals.push({ k: "gas", label: "Gas resistance", v: r2(r.G / 1000, 1), unit: "kΩ", d: 1 });
  return { values: vals, stale: !measure && !(b[0] & 0x80) };
}

async function bmp180Measure(d: Dev, s: ChipState): Promise<Readout> {
  if (!s.cache?.cal180) s.cache = { ...s.cache, cal180: bmp180Cal(d.block(0xaa, 22)) };
  d.wb(0xf4, 0x2e);
  await sleep(5);
  const ut = d.u16be(0xf6);
  d.wb(0xf4, 0x34);
  await sleep(8);
  const b = d.block(0xf6, 3);
  const up = ((b[0] << 16) | (b[1] << 8) | b[2]) >> 8;
  const r = bmp180Comp(s.cache.cal180, ut, up, 0);
  return { values: [deg("t", "Temperature", r.T, 1), { k: "p", label: "Pressure", v: r2(r.P / 100, 2), unit: "hPa", d: 2 }] };
}

const bmp3xx: ChipDef = {
  id: "bmp3xx",
  name: "BMP388",
  what: "Pressure / temperature",
  addrs: [0x76, 0x77],
  async probe(d) {
    const id = tryq(() => d.u8(0x00));
    return id === 0x50 || id === 0x60 ? { confidence: "id", variant: id === 0x50 ? "BMP388" : "BMP390", note: `Chip ID 0x${id.toString(16)} (decoder not implemented)` } : null;
  },
};

const dps310: ChipDef = {
  id: "dps310",
  name: "DPS310",
  what: "Pressure / temperature",
  addrs: [0x76, 0x77],
  async probe(d) {
    return tryq(() => d.u8(0x0d)) === 0x10 ? { confidence: "id", note: "Product ID 0x10 (decoder not implemented)" } : null;
  },
};

const tca9548hi: ChipDef = { ...tca9548, addrs: [0x76, 0x77] };

/** Probe order. Read the file header before changing it. */
export const CHIPS: ChipDef[] = [
  // Pure-read probes for command-style chips first.
  ssd1306,
  aht2x,
  expander,
  mcp4725,
  bh1750,
  pca9685AllCall,
  { ...tca9548, addrs: range(0x70, 0x75) },
  // Register-based chips with ID registers.
  mcp9808,
  lis3dh,
  ds2482,
  hmc5883,
  adxl345,
  veml7700,
  ak8963,
  htu21d, // must precede everything else at 0x40 (see its comment)
  tiIna,
  ads1x15,
  tmp102,
  lm75,
  tsl2561, // after LM75/ADS at 0x49 (an LM75 THYST byte can look like a TSL2560 ID)
  tcs34725,
  tsl2591,
  vl53l0x,
  apds9960,
  sht4x,
  sht3x,
  pca9685,
  ina219, // no ID register: last in 0x40–0x4F
  at24,
  ccs811,
  mlx90614,
  max1704x,
  scd4x,
  mpu,
  icm20948,
  rtc,
  bosch,
  bmp3xx,
  dps310,
  tca9548hi, // at 0x76/0x77 a BME/BMP is far likelier than a re-strapped mux
];

export const CHIP_BY_ID = new Map<string, ChipDef>();
for (const c of CHIPS) if (!CHIP_BY_ID.has(c.id)) CHIP_BY_ID.set(c.id, c);

/** Well-known residents per address, for "unknown device" hints. */
export function addressHints(addr: number): string[] {
  const names = new Set<string>();
  for (const c of CHIPS) if (c.addrs.includes(addr) && c.id !== "pca9685-allcall") names.add(c.name);
  const extra: Record<number, string[]> = {
    0x27: ["HD44780 LCD backpack (PCF8574)"],
    0x3f: ["HD44780 LCD backpack (PCF8574A)"],
    0x50: ["Pi HAT ID EEPROM", "DDR SPD"],
    0x51: ["PCF8563 RTC"],
    0x57: ["AT24C32 on DS3231 modules", "MAX30102"],
    0x5c: ["AM2320"],
    0x60: ["Si5351", "MPL3115A2", "ATECC608"],
    0x68: ["PCF8523 RTC", "BMI160"],
    0x77: ["MS5611"],
  };
  for (const n of extra[addr] || []) names.add(n);
  return [...names];
}

export interface Ident {
  chip: ChipDef | null;
  probe: Probe | null;
}

/** Identify one address. Probes run in CHIPS order; the first match wins. */
export async function identify(io: I2cIo, addr: number, ctx: ProbeCtx): Promise<Ident> {
  const d = new Dev(io, addr);
  for (const chip of CHIPS) {
    if (!chip.addrs.includes(addr)) continue;
    let p: Probe | null = null;
    try {
      p = await chip.probe(d, ctx);
    } catch (e) {
      if (!(e instanceof I2cError)) throw e;
    }
    if (p) return { chip: CHIP_BY_ID.get(chip.id)!, probe: p };
  }
  return { chip: null, probe: null };
}

/** Bus-level hints once every address is identified (pairs that explain each other). */
export function crossHints(found: Map<number, string>): Map<number, string> {
  const out = new Map<number, string>();
  if (found.get(0x68) === "rtc" && found.has(0x57)) out.set(0x57, "Next to a DS3231: this is usually the module's AT24C32 (4 KiB, 2-byte addresses). Set the size accordingly.");
  if (found.get(0x68) === "mpu" && (found.has(0x0c) || found.has(0x0d))) out.set(0x0c, "AK8963 magnetometer of an MPU-9250 (I²C bypass enabled).");
  return out;
}
