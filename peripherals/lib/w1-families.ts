// ==================== 1-Wire family table + decoders ====================
//
// Pure functions over what the kernel's w1 slave drivers expose in sysfs, so they can be
// tested against fixture trees. Which attribute each family has (verified against the
// 6.18 module set on desky, see plans/peripherals-buses.md):
//   w1_therm   0x10 0x22 0x28 0x3B 0x42  w1_slave (scratchpad + CRC + t=), temperature,
//                                        resolution, ext_power, alarms; master therm_bulk_read
//   w1_ds2438  0x26  temperature (/256 °C), vad, vdd (/100 V), page0, page1
//   w1_ds2408  0x29  state, output, activity (1 byte each, binary)
//   w1_ds2413  0x3A  state (binary: A pin, A latch, B pin, B latch), output
//   w1_ds2406  0x12  state ('0'..'3' ASCII = flip-flops), output       (no autoload alias!)
//   w1_ds2423  0x1D  w1_slave: one line per counter page, "... crc=YES c=<n>"
//   w1_ds2431  0x2D, w1_ds2433 0x23/0x43, w1_ds28e04 0x1C, w1_ds2430 0x14: eeprom
//   w1_smem    0x01 0x81 (ROM only)
//   no driver  0x24 0x27 (RTCs): the generic `rw` attribute (select + raw bytes)

export interface Family {
  name: string;
  kind: "temp" | "thermocouple" | "battery" | "io" | "counter" | "eeprom" | "id" | "rtc" | "adapter" | "other";
  desc: string;
  module?: string;
  eepromSize?: number;
}

export const FAMILIES: Record<number, Family> = {
  0x01: { name: "DS2401 / DS1990A", kind: "id", desc: "Silicon serial number / iButton", module: "w1_smem" },
  0x02: { name: "DS1991", kind: "other", desc: "MultiKey iButton" },
  0x04: { name: "DS2404 / DS1994", kind: "rtc", desc: "EconoRAM time chip" },
  0x05: { name: "DS2405", kind: "io", desc: "Addressable switch" },
  0x06: { name: "DS1993", kind: "eeprom", desc: "4 Kbit memory iButton" },
  0x08: { name: "DS1992", kind: "eeprom", desc: "1 Kbit memory iButton" },
  0x09: { name: "DS2502 / DS1982", kind: "eeprom", desc: "1 Kbit add-only memory" },
  0x0a: { name: "DS1995", kind: "eeprom", desc: "16 Kbit memory iButton" },
  0x0b: { name: "DS2505 / DS1985", kind: "eeprom", desc: "16 Kbit add-only memory" },
  0x0c: { name: "DS1996", kind: "eeprom", desc: "64 Kbit memory iButton" },
  0x0f: { name: "DS2506 / DS1986", kind: "eeprom", desc: "64 Kbit add-only memory" },
  0x10: { name: "DS18S20", kind: "temp", desc: "Temperature (9-bit + extended)", module: "w1_therm" },
  0x12: { name: "DS2406 / DS2407", kind: "io", desc: "Dual addressable switch + 1 Kbit memory", module: "w1_ds2406" },
  0x14: { name: "DS2430A", kind: "eeprom", desc: "256-bit EEPROM", module: "w1_ds2430", eepromSize: 32 },
  0x18: { name: "DS1963S", kind: "other", desc: "SHA iButton" },
  0x19: { name: "DS28E17", kind: "adapter", desc: "1-Wire → I²C bridge", module: "w1_ds28e17" },
  0x1a: { name: "DS1963L", kind: "counter", desc: "Monetary iButton" },
  0x1c: { name: "DS28E04", kind: "eeprom", desc: "4 Kbit EEPROM + PIO", module: "w1_ds28e04", eepromSize: 512 },
  0x1d: { name: "DS2423", kind: "counter", desc: "4 Kbit RAM + counters", module: "w1_ds2423" },
  0x1f: { name: "DS2409", kind: "adapter", desc: "MicroLAN coupler" },
  0x20: { name: "DS2450", kind: "other", desc: "Quad A/D converter (no kernel driver)" },
  0x21: { name: "DS1921", kind: "temp", desc: "Thermochron logger iButton (no kernel driver)" },
  0x22: { name: "DS1822", kind: "temp", desc: "Econo temperature", module: "w1_therm" },
  0x23: { name: "DS2433", kind: "eeprom", desc: "4 Kbit EEPROM", module: "w1_ds2433", eepromSize: 512 },
  0x24: { name: "DS2415 / DS1904", kind: "rtc", desc: "Real-time clock", },
  0x26: { name: "DS2438", kind: "battery", desc: "Battery monitor: temperature, VAD, VDD, current", module: "w1_ds2438" },
  0x27: { name: "DS2417", kind: "rtc", desc: "Real-time clock with interrupt" },
  0x28: { name: "DS18B20", kind: "temp", desc: "Temperature (9–12 bit)", module: "w1_therm" },
  0x29: { name: "DS2408", kind: "io", desc: "8-channel addressable switch", module: "w1_ds2408" },
  0x2c: { name: "DS2890", kind: "other", desc: "Digital potentiometer" },
  0x2d: { name: "DS2431", kind: "eeprom", desc: "1 Kbit EEPROM", module: "w1_ds2431", eepromSize: 128 },
  0x30: { name: "DS2760", kind: "battery", desc: "Li-ion battery monitor" },
  0x32: { name: "DS2780", kind: "battery", desc: "Fuel gauge", module: "w1_ds2780" },
  0x33: { name: "DS1961S / DS2432", kind: "eeprom", desc: "SHA-1 EEPROM" },
  0x37: { name: "DS1977", kind: "eeprom", desc: "Password-protected 32 KB iButton" },
  0x3a: { name: "DS2413", kind: "io", desc: "Dual-channel addressable switch", module: "w1_ds2413" },
  0x3b: { name: "MAX31850 / DS1825", kind: "thermocouple", desc: "Thermocouple converter / temperature", module: "w1_therm" },
  0x3d: { name: "DS2781", kind: "battery", desc: "Fuel gauge", module: "w1_ds2781" },
  0x41: { name: "DS1922 / DS1923", kind: "temp", desc: "Hygrochron / Thermochron logger (no kernel driver)" },
  0x42: { name: "DS28EA00", kind: "temp", desc: "Temperature with sequence detect + PIO", module: "w1_therm" },
  0x43: { name: "DS28EC20", kind: "eeprom", desc: "20 Kbit EEPROM", module: "w1_ds2433", eepromSize: 2560 },
  0x51: { name: "DS2751", kind: "battery", desc: "Battery fuel gauge" },
  0x7e: { name: "EDS00xx", kind: "other", desc: "Embedded Data Systems environmental sensor" },
  0x81: { name: "DS1420", kind: "adapter", desc: "ID chip inside a DS9490R USB adapter", module: "w1_smem" },
};

export function family(code: number): Family {
  return FAMILIES[code] || { name: `Family 0x${code.toString(16).padStart(2, "0").toUpperCase()}`, kind: "other", desc: "Unknown family" };
}

/** Dallas/Maxim CRC-8 (x^8 + x^5 + x^4 + 1, reflected 0x8C), as used for ROM IDs and scratchpads. */
export function crc8w1(bytes: number[]): number {
  let c = 0;
  for (let b of bytes) {
    for (let i = 0; i < 8; i++) {
      const mix = (c ^ b) & 1;
      c >>= 1;
      if (mix) c ^= 0x8c;
      b >>= 1;
    }
  }
  return c;
}

/** "28-0000075b1a2c" → family 0x28. */
export function familyOf(slaveId: string): number {
  return parseInt(slaveId.slice(0, 2), 16);
}

/** Check the 8-byte `id` attribute (family, 48-bit serial LE, CRC). */
export function romOk(id: Uint8Array | number[]): boolean {
  return id.length === 8 && crc8w1([...id].slice(0, 7)) === id[7];
}

// ---------- w1_therm ----------

export interface ThermSlave {
  crcOk: boolean;
  scratch: number[];
  tMilli: number | null;
}

/**
 * w1_therm's w1_slave:
 *   72 01 4b 46 7f ff 0e 10 57 : crc=57 YES
 *   72 01 4b 46 7f ff 0e 10 57 t=23125
 */
export function parseThermSlave(text: string): ThermSlave | null {
  const lines = text.trim().split("\n");
  if (!lines[0]) return null;
  const m = /^((?:[0-9a-f]{2} ){9}): crc=([0-9a-f]{2}) (YES|NO)/i.exec(lines[0]);
  if (!m) return null;
  const scratch = m[1].trim().split(" ").map((x) => parseInt(x, 16));
  const t = /t=(-?\d+)/.exec(lines[1] || "");
  return { crcOk: m[3] === "YES", scratch, tMilli: t ? +t[1] : null };
}

export interface Reading {
  k: string;
  label: string;
  v: number | string | boolean | null;
  unit?: string;
  d?: number;
}

export interface Decoded {
  values: Reading[];
  warn?: string[];
  pins?: { name: string; level: 0 | 1; latch?: 0 | 1; out?: boolean }[];
}

const s16le = (lo: number, hi: number) => {
  const v = lo | (hi << 8);
  return v & 0x8000 ? v - 0x10000 : v;
};

/** Decode a w1_therm scratchpad (9 bytes) for the given family. */
export function decodeTherm(fam: number, ts: ThermSlave): Decoded {
  const s = ts.scratch;
  const warn: string[] = [];
  if (!ts.crcOk) warn.push("Scratchpad CRC mismatch: wiring/pull-up problem or a collision; value discarded.");
  const values: Reading[] = [];
  if (fam === 0x3b && s[4] >> 4 === 0x0f) {
    // MAX31850: bytes 0-1 thermocouple (14-bit, 0.25 °C) + fault bit, 2-3 cold junction (12-bit, 0.0625 °C) + OC/SCG/SCV.
    const tc = (s16le(s[0], s[1]) >> 2) * 0.25;
    const cj = (s16le(s[2], s[3]) >> 4) * 0.0625;
    const fault = !!(s[0] & 1);
    const faults = [s[2] & 1 ? "open circuit" : "", s[2] & 2 ? "short to GND" : "", s[2] & 4 ? "short to VDD" : ""].filter(Boolean);
    values.push({ k: "t", label: "Thermocouple", v: ts.crcOk && !fault ? tc : null, unit: "°C", d: 2 });
    values.push({ k: "cj", label: "Cold junction", v: ts.crcOk ? cj : null, unit: "°C", d: 2 });
    values.push({ k: "addr", label: "Address pins", v: s[4] & 0x0f });
    if (fault) warn.push(`Thermocouple fault: ${faults.join(", ") || "unspecified"}`);
    return { values, warn };
  }
  let t: number | null = null;
  if (fam === 0x10) {
    // DS18S20: 0.5 °C + COUNT_REMAIN/COUNT_PER_C extension.
    const raw = s16le(s[0], s[1]);
    t = s[7] ? (raw >> 1) - 0.25 + (s[7] - s[6]) / s[7] : raw / 2;
    values.push({ k: "t", label: "Temperature", v: ts.crcOk ? Math.round(t * 1000) / 1000 : null, unit: "°C", d: 3 });
    values.push({ k: "res", label: "Resolution", v: "9-bit + extended" });
  } else {
    const raw = s16le(s[0], s[1]);
    const bits = 9 + ((s[4] >> 5) & 3);
    t = raw / 16;
    values.push({ k: "t", label: "Temperature", v: ts.crcOk ? t : null, unit: "°C", d: bits === 9 ? 1 : bits === 10 ? 2 : 3 });
    values.push({ k: "res", label: "Resolution", v: `${bits}-bit (${[0.5, 0.25, 0.125, 0.0625][bits - 9]} °C, ${[94, 188, 375, 750][bits - 9]} ms)` });
    if (fam === 0x3b) values.push({ k: "addr", label: "Address pins", v: s[4] & 0x0f });
  }
  // 85.0 °C is the power-on reset value: a sensor that lost power mid-conversion, or a
  // parasite-powered sensor without enough strong pull-up, reads exactly this.
  if (ts.crcOk && s[0] === 0x50 && s[1] === 0x05 && fam !== 0x10) warn.push("85 °C is the power-on value: the conversion didn't run (power glitch or parasite power without strong pull-up).");
  if (ts.crcOk && fam === 0x10 && s[0] === 0xaa && s[1] === 0x00) warn.push("85 °C is the power-on value: the conversion didn't run.");
  if (ts.crcOk && s.every((b) => b === 0xff)) warn.push("All-ones scratchpad: the sensor isn't responding.");
  if (ts.crcOk && s.slice(0, 8).every((b) => b === 0)) warn.push("All-zero scratchpad: the data line is stuck low.");
  values.push({ k: "alarm", label: "Alarm TH/TL", v: `${(s[2] << 24) >> 24} / ${(s[3] << 24) >> 24} °C` });
  return { values, warn };
}

// ---------- other families ----------

/** DS2413 state byte: bit0 PIOA pin, bit1 PIOA latch, bit2 PIOB pin, bit3 PIOB latch; high nibble = complement. */
export function decodeDs2413(state: number): Decoded {
  const ok = ((~state >> 4) & 0x0f) === (state & 0x0f);
  const pins = [
    { name: "PIO-A", level: (state & 1) as 0 | 1, latch: ((state >> 1) & 1) as 0 | 1 },
    { name: "PIO-B", level: ((state >> 2) & 1) as 0 | 1, latch: ((state >> 3) & 1) as 0 | 1 },
  ].map((p) => ({ ...p, out: p.latch === 0 }));
  return { values: [{ k: "state", label: "State", v: "0x" + state.toString(16).padStart(2, "0") }], pins, warn: ok ? [] : ["State byte failed its complement check."] };
}

/** DS2408 state/output/activity bytes. A PIO with output latch 0 has its transistor on (pulled low). */
export function decodeDs2408(state: number, output: number, activity: number | null): Decoded {
  const pins = Array.from({ length: 8 }, (_, i) => ({ name: `P${i}`, level: ((state >> i) & 1) as 0 | 1, latch: ((output >> i) & 1) as 0 | 1, out: !((output >> i) & 1) }));
  const values: Reading[] = [{ k: "state", label: "Pin state", v: "0x" + state.toString(16).padStart(2, "0") }, { k: "output", label: "Output latch", v: "0x" + output.toString(16).padStart(2, "0") }];
  if (activity != null) values.push({ k: "activity", label: "Activity latch", v: "0x" + activity.toString(16).padStart(2, "0") });
  return { values, pins };
}

/** DS2406 `state`: '0'..'3' (ASCII) = PIO-A / PIO-B flip-flops; 1 = transistor off. */
export function decodeDs2406(raw: number): Decoded {
  const v = raw >= 0x30 ? raw - 0x30 : raw;
  return {
    values: [{ k: "ff", label: "Flip-flops", v }],
    pins: [
      { name: "PIO-A", level: (v & 1) as 0 | 1, latch: (v & 1) as 0 | 1, out: !(v & 1) },
      { name: "PIO-B", level: ((v >> 1) & 1) as 0 | 1, latch: ((v >> 1) & 1) as 0 | 1, out: !((v >> 1) & 1) },
    ],
  };
}

/** DS2423 w1_slave: one line per counter page, "<hex bytes> crc=YES c=<n>". Counters A/B are on pages 14/15. */
export function parseDs2423(text: string): Decoded {
  const lines = text.trim().split("\n").filter(Boolean);
  const counters = lines.map((l) => {
    const m = /crc=(YES|NO)(?: c=(\d+))?/.exec(l);
    return m ? { ok: m[1] === "YES", c: m[2] != null ? +m[2] : null } : null;
  });
  const firstPage = 16 - counters.length;
  const values: Reading[] = [];
  const warn: string[] = [];
  counters.forEach((c, i) => {
    const page = firstPage + i;
    if (!c || c.c == null) return;
    const label = page === 14 ? "Counter A" : page === 15 ? "Counter B" : `Page ${page} counter`;
    values.push({ k: `c${page}`, label, v: c.ok ? c.c : null });
    if (!c.ok) warn.push(`${label}: CRC mismatch`);
  });
  return { values, warn };
}

/** DS2438 text attributes (°C×256, V×100) + page0 current register. */
export function decodeDs2438(temp: string, vad: string, vdd: string, page0?: number[], rsens?: number): Decoded {
  const values: Reading[] = [];
  const n = (s: string) => (/^-?\d+$/.test(s.trim()) ? +s.trim() : null);
  const t = n(temp);
  values.push({ k: "t", label: "Temperature", v: t == null ? null : Math.round((t / 256) * 100) / 100, unit: "°C", d: 2 });
  const a = n(vad);
  values.push({ k: "vad", label: "VAD", v: a == null ? null : a / 100, unit: "V", d: 2 });
  const d = n(vdd);
  values.push({ k: "vdd", label: "VDD", v: d == null ? null : d / 100, unit: "V", d: 2 });
  if (page0 && page0.length >= 7) {
    const raw = s16le(page0[5], page0[6]) >> 0;
    const vsens = raw * 0.2441; // mV
    values.push({ k: "vsens", label: "Current sense", v: Math.round(vsens * 100) / 100, unit: "mV", d: 2 });
    if (rsens) values.push({ k: "i", label: "Current", v: Math.round((vsens / rsens) * 10) / 10, unit: "mA", d: 1 });
  }
  return { values };
}

/** DS1904/DS2415/DS2417 "read clock" (0x66) reply: control byte + 32-bit seconds (LE). */
export function decodeRtc(b: number[]): Decoded {
  if (b.length < 5) return { values: [], warn: ["Short clock read"] };
  const secs = (b[1] | (b[2] << 8) | (b[3] << 16) | (b[4] << 24)) >>> 0;
  const running = (b[0] & 0x0c) === 0x0c; // OSC bits
  const date = new Date(secs * 1000);
  return {
    values: [
      { k: "time", label: "Clock (UTC)", v: date.toISOString().replace("T", " ").slice(0, 19) },
      { k: "drift", label: "vs system clock", v: Math.round(secs - Date.now() / 1000), unit: "s", d: 0 },
      { k: "osc", label: "Oscillator", v: running ? "running" : "stopped" },
    ],
  };
}
