// Builds a fake /sys/bus/w1/devices tree with one slave of every family the onewire driver
// decodes, laid out the way the 6.18 kernel modules expose them. Used by the unit tests and
// by side instances on desky: `bun peripherals/lib/w1-fixtures.ts /tmp/w1fx`, then run the
// server with DESKYPI_W1_ROOT=/tmp/w1fx. Binary attributes are generated (not committed) so
// CRCs are computed, not hand-typed.

import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { crc8w1 } from "./w1-families.ts";

/** ROM id bytes for "ff-xxxxxxxxxxxx" (serial printed big-endian, stored little-endian). */
export function romBytes(id: string): number[] {
  const fam = parseInt(id.slice(0, 2), 16);
  const ser = id.slice(3).match(/../g)!.map((h) => parseInt(h, 16)).reverse();
  const b = [fam, ...ser];
  return [...b, crc8w1(b)];
}

const hex = (b: number[]) => b.map((x) => x.toString(16).padStart(2, "0")).join(" ");

/** w1_therm w1_slave text for a scratchpad (CRC computed unless `badCrc`). */
export function thermText(s8: number[], tMilli: number, badCrc = false): string {
  const s = [...s8, crc8w1(s8) ^ (badCrc ? 0x55 : 0)];
  const line = `${hex(s)} : crc=${s[8].toString(16).padStart(2, "0")} ${badCrc ? "NO" : "YES"}`;
  return `${line}\n${hex(s)} t=${tMilli}\n`;
}

export const FIX = {
  ds18b20: "28-0000075b1a2c",
  ds18b20por: "28-0000075b9911",
  ds18s20: "10-000802b4c1d2",
  ds1822: "22-000000a1b2c3",
  max31850: "3b-0000001a2b3c",
  ds28ea00: "42-00000011aa22",
  ds2438: "26-000001f2e3d4",
  ds2408: "29-00000012ab34",
  ds2413: "3a-00000045cd67",
  ds2406: "12-000000123456",
  ds2423: "1d-000000fedcba",
  ds2431: "2d-0000004a5b6c",
  ds2433: "23-000000112233",
  ds28ec20: "43-000000445566",
  ibutton: "01-000017c2a3b4",
  ds1904: "24-0000000a0b0c",
  ds1420: "81-00000034a1b2",
  ds2450: "20-000000778899",
  unknown: "7e-000000010203",
  ghost: "00-280000000000",
};

export function buildW1Fixture(root: string) {
  rmSync(root, { recursive: true, force: true });
  const master = `${root}/w1_bus_master1`;
  mkdirSync(master, { recursive: true });
  const f = (dir: string, name: string, data: string | number[]) => writeFileSync(`${dir}/${name}`, typeof data === "string" ? data : Uint8Array.from(data));
  const slaves = Object.values(FIX);
  f(master, "w1_master_name", "w1_bus_master1\n");
  f(master, "w1_master_slaves", slaves.join("\n") + "\n");
  f(master, "w1_master_slave_count", `${slaves.length}\n`);
  f(master, "w1_master_search", "-1\n");
  f(master, "w1_master_pullup", "1\n");
  f(master, "w1_master_timeout", "10\n");
  f(master, "w1_master_timeout_us", "0\n");
  f(master, "w1_master_attempts", "42\n");
  f(master, "w1_master_max_slave_count", "64\n");
  f(master, "therm_bulk_read", "0\n");
  f(master, "w1_master_add", "");
  f(master, "w1_master_remove", "");

  const slave = (id: string, attrs: Record<string, string | number[]>) => {
    const d = `${root}/${id}`;
    mkdirSync(d, { recursive: true });
    f(d, "id", romBytes(id));
    f(d, "name", id + "\n");
    for (const [k, v] of Object.entries(attrs)) f(d, k, v);
  };
  const therm = (id: string, s: number[], t: number, extra: Record<string, string> = {}) => slave(id, { w1_slave: thermText(s, t), temperature: `${t}\n`, resolution: "12\n", ext_power: "1\n", ...extra });

  // DS18B20 23.125 °C, 12-bit (config 0x7F)
  therm(FIX.ds18b20, [0x72, 0x01, 0x4b, 0x46, 0x7f, 0xff, 0x0e, 0x10], 23125);
  // DS18B20 that never converted: power-on 85 °C (0x0550)
  therm(FIX.ds18b20por, [0x50, 0x05, 0x4b, 0x46, 0x7f, 0xff, 0x0c, 0x10], 85000);
  // DS18S20 25.0 °C via COUNT_REMAIN 12, COUNT_PER_C 16: 25 − 0.25 + 4/16
  therm(FIX.ds18s20, [0x32, 0x00, 0x4b, 0x46, 0xff, 0xff, 0x0c, 0x10], 25000, { resolution: "9\n" });
  // DS1822 −10.5 °C, 10-bit (config 0x3F)
  therm(FIX.ds1822, [0x58, 0xff, 0x4b, 0x46, 0x3f, 0xff, 0x08, 0x10], -10500, { resolution: "10\n" });
  // MAX31850, address pins 3, open thermocouple: TC 100.25 °C + fault, CJ 24.0625 °C + OC
  therm(FIX.max31850, [0x45, 0x06, 0x11, 0x18, 0xf3, 0xff, 0xff, 0xff], 100250);
  // DS28EA00 21.5 °C
  therm(FIX.ds28ea00, [0x58, 0x01, 0x4b, 0x46, 0x7f, 0xff, 0x08, 0x10], 21500);
  // DS2438: 23.5 °C, VAD 3.12 V, VDD 4.98 V, current register 205 (≈50 mV)
  slave(FIX.ds2438, { temperature: "6016\n", vad: "312\n", vdd: "498\n", page0: [0x0f, 0x00, 0x2f, 0xf2, 0x01, 0xcd, 0x00, 0x00], page1: new Array(8).fill(0), iad: "", offset: "" });
  // DS2408: inputs 0xF5, all latches off, no activity
  slave(FIX.ds2408, { state: [0xf5], output: [0xff], activity: [0x00], cond_search_mask: [0], cond_search_polarity: [0], status_control: [0x88] });
  // DS2413: PIO-A pin 1 latch 1 (off), PIO-B pin 0 latch 0 (on) → 0b0011 + complement 0xC
  slave(FIX.ds2413, { state: [0xc3], output: [0xff] });
  // DS2406: flip-flops A=1 (off), B=0 (on)
  slave(FIX.ds2406, { state: [0x31], output: [0] });
  // DS2423 counters on pages 12..15
  const page = (c: number) => `${hex(new Array(42).fill(0))} crc=YES c=${c}\n`;
  slave(FIX.ds2423, { w1_slave: page(1) + page(2) + page(12345) + page(678) });
  const eeprom = (n: number, tag: string) => Array.from({ length: n }, (_, i) => (i < tag.length ? tag.charCodeAt(i) : i & 0xff));
  slave(FIX.ds2431, { eeprom: eeprom(128, "DS2431 fixture") });
  slave(FIX.ds2433, { eeprom: eeprom(512, "DS2433 fixture") });
  slave(FIX.ds28ec20, { eeprom: eeprom(2560, "DS28EC20 fixture") });
  slave(FIX.ibutton, { rw: [] });
  // DS1904: control 0x0C (oscillator on), seconds = 2026-10-17 12:00:00 UTC
  const secs = Date.UTC(2026, 9, 17, 12, 0, 0) / 1000;
  slave(FIX.ds1904, { rw: [0x0c, secs & 0xff, (secs >> 8) & 0xff, (secs >> 16) & 0xff, (secs >>> 24) & 0xff] });
  slave(FIX.ds1420, {});
  slave(FIX.ds2450, { rw: [] });
  slave(FIX.unknown, { rw: [] });
  slave(FIX.ghost, { rw: [] });
}

if (import.meta.main) {
  const dir = process.argv[2];
  if (!dir) throw new Error("usage: bun w1-fixtures.ts <dir>");
  buildW1Fixture(dir);
  console.log(`1-Wire fixture tree written to ${dir}`);
}
