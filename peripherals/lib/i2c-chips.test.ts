import { describe, expect, test } from "bun:test";
import { F, FakeBus, I2cError, RegChip, WordChip, type FakeChip } from "./i2c-dev.ts";
import {
  CHIP_BY_ID, Dev, adsVolts, ahtDecode, bme280Cal, bme280Comp, bmp180Comp, crc8sens, identify, parseHatEeprom, rtcDecode, sensWords, tsl2561Lux, veml7700Lux,
  type ChipState, type ProbeCtx,
} from "./i2c-chips.ts";
import { bme280Regs, eepromRegs, lm75Words, mpu6050Regs } from "./i2c-fixtures.ts";

const ctx = (): ProbeCtx => ({ found: new Map() });
const state = (probe: any, opts: any = {}): ChipState => ({ probe, opts });
const val = (r: any, k: string) => r.values.find((v: any) => v.k === k)?.v;
const SMBUS_ONLY = F.QUICK | F.READ_BYTE | F.WRITE_BYTE | F.READ_BYTE_DATA | F.WRITE_BYTE_DATA | F.READ_WORD_DATA | F.WRITE_WORD_DATA | F.READ_I2C_BLOCK | F.WRITE_I2C_BLOCK;

/** A command-style chip that NACKs unknown commands and records every write. */
class CmdChip implements FakeChip {
  writes: number[][] = [];
  next: number[] = [];
  constructor(public handlers: Record<string, () => number[]>, public bareRead?: () => number[]) {}
  write(b: number[]) {
    this.writes.push(b);
    const key = b.map((x) => x.toString(16).padStart(2, "0")).join("");
    const h = this.handlers[key];
    if (!h) throw new I2cError(121, "nack");
    this.next = h();
  }
  read(n: number) {
    if (!this.next.length) {
      if (this.bareRead) return this.bareRead().slice(0, n);
      throw new I2cError(121, "nack");
    }
    const out = this.next.slice(0, n);
    this.next = [];
    return out;
  }
}
const sens = (...words: number[]) => words.flatMap((w) => [w >> 8, w & 0xff, crc8sens([w >> 8, w & 0xff])]);

describe("decoders", () => {
  test("Sensirion CRC-8 matches the datasheet example (0xBEEF → 0x92)", () => {
    expect(crc8sens([0xbe, 0xef])).toBe(0x92);
    expect(sensWords([0xbe, 0xef, 0x92])).toEqual([0xbeef]);
    expect(sensWords([0xbe, 0xef, 0x93])).toBeNull();
  });

  test("BME280 compensation reproduces the Bosch worked example", () => {
    const r = bme280Regs();
    const cal = bme280Cal(Array.from({ length: 26 }, (_, i) => r[0x88 + i]), Array.from({ length: 7 }, (_, i) => r[0xe1 + i]));
    expect(cal).toMatchObject({ T1: 27504, T2: 26435, T3: -1000, P1: 36477, P9: 6000, H1: 75, H2: 362, H4: 313, H5: 50, H6: 30 });
    const c = bme280Comp(cal, 519888, 415148, 27000);
    expect(c.T).toBeCloseTo(25.08, 2);
    expect(c.P! / 100).toBeCloseTo(1006.53, 1);
    expect(c.H!).toBeGreaterThan(0);
    expect(c.H!).toBeLessThan(100);
  });

  test("BMP180 integer algorithm reproduces the datasheet example", () => {
    const c = { AC1: 408, AC2: -72, AC3: -14383, AC4: 32741, AC5: 32757, AC6: 23153, B1: 6190, B2: 4, MB: -32768, MC: -8711, MD: 2868 };
    const r = bmp180Comp(c, 27898, 23843, 0);
    expect(r.T).toBe(15.0);
    expect(r.P).toBe(69964);
  });

  test("AHT20 frame decode + CRC", () => {
    const b = [0x1c, 0x80, 0x00, 0x06, 0x00, 0x00];
    const r = ahtDecode([...b, crc8sens(b)]);
    expect(r.H).toBeCloseTo(50, 3);
    expect(r.T).toBeCloseTo(25, 3);
    expect(r.crcOk).toBe(true);
    expect(ahtDecode([...b, 0]).crcOk).toBe(false);
  });

  test("TSL2561 lux: dark, typical, saturated", () => {
    expect(tsl2561Lux(0, 0, 2, false)).toBe(0);
    const l = tsl2561Lux(1000, 200, 2, true)!; // 402 ms, 16×: ratio 0.2
    expect(l).toBeCloseTo(0.0304 * 1000 - 0.062 * 1000 * 0.2 ** 1.4, 3);
    expect(tsl2561Lux(5047, 10, 0, true)).toBeNull();
  });

  test("VEML7700 resolution (gain ×1, 100 ms = 0.0576 lx/count)", () => {
    expect(veml7700Lux(1000, 0x0000)).toBeCloseTo(57.6, 3);
  });

  test("ADS1115 / ADS1015 volts", () => {
    expect(adsVolts(0x4000, 0x8583)).toBeCloseTo(1.024, 4); // ±2.048 FSR, half scale
    expect(adsVolts(0x4000, 0x8583, true)).toBeCloseTo(1.024, 4);
    expect(adsVolts(0xc000, 0x8383)).toBeCloseTo(-2.048, 4); // ±4.096
  });

  test("RTC BCD decode (24 h and 12 h PM)", () => {
    expect(rtcDecode([0x30, 0x45, 0x13, 0x05, 0x17, 0x10, 0x26]).date!.toISOString()).toBe("2026-10-17T13:45:30.000Z");
    expect(rtcDecode([0x00, 0x00, 0x61, 0x05, 0x17, 0x10, 0x26]).date!.getUTCHours()).toBe(13); // 12 h mode, PM, 1
    expect(rtcDecode([0x00, 0, 0, 0, 1, 0x13, 0]).date).toBeNull(); // month 13
  });

  test("Pi HAT EEPROM header", () => {
    const r = eepromRegs();
    const h = parseHatEeprom(Array.from({ length: 64 }, (_, i) => r[i] ?? 0xff))!;
    expect(h).toMatchObject({ version: 1, atoms: 1, vendor: "DeskyPi", product: "Bench HAT", pid: 0x1234, pver: 2 });
    expect(parseHatEeprom(new Array(64).fill(0xff))).toBeNull();
  });
});

describe("identification", () => {
  test("BME280: id, normal mode auto-read decodes the worked example", async () => {
    const bus = new FakeBus().add(0x76, new RegChip(bme280Regs()));
    const id = await identify(bus, 0x76, ctx());
    expect(id.chip?.id).toBe("bosch");
    expect(id.probe?.variant).toBe("BME280");
    const r = await id.chip!.read!(new Dev(bus, 0x76), state(id.probe));
    expect(val(r, "t")).toBeCloseTo(25.08, 2);
    expect(val(r, "p")).toBeCloseTo(1006.53, 1);
    expect(r.stale).toBeFalsy();
  });

  test("BME280 on an SMBus-only adapter (i2c-stub-like) still identifies and decodes", async () => {
    const bus = new FakeBus(SMBUS_ONLY).add(0x76, new RegChip(bme280Regs()));
    const id = await identify(bus, 0x76, ctx());
    expect(id.probe?.variant).toBe("BME280");
    const r = await id.chip!.read!(new Dev(bus, 0x76), state(id.probe));
    expect(val(r, "t")).toBeCloseTo(25.08, 2);
  });

  test("BME280 in sleep mode: read is marked stale; measure triggers forced mode", async () => {
    const chip = new RegChip({ ...bme280Regs(), 0xf4: 0x24 });
    const bus = new FakeBus().add(0x77, chip);
    const id = await identify(bus, 0x77, ctx());
    const st = state(id.probe);
    expect((await id.chip!.read!(new Dev(bus, 0x77), st)).stale).toBe(true);
    const before = chip.writes.filter((w) => w.length > 1).length;
    expect(before).toBe(0); // identification + read wrote no data
    const m = await id.chip!.measure!(new Dev(bus, 0x77), st);
    expect(val(m, "t")).toBeCloseTo(25.08, 2);
    expect(chip.regs[0xf4] & 3).toBe(1); // forced
  });

  test("MPU-6050 asleep: read stale, measure wakes then restores PWR_MGMT_1", async () => {
    const chip = new RegChip({ ...mpu6050Regs(), 0x6b: 0x40 });
    const bus = new FakeBus().add(0x68, chip);
    const id = await identify(bus, 0x68, ctx());
    expect(id.probe?.variant).toBe("MPU-6050");
    const st = state(id.probe);
    expect((await id.chip!.read!(new Dev(bus, 0x68), st)).stale).toBe(true);
    const m = await id.chip!.measure!(new Dev(bus, 0x68), st);
    expect(val(m, "az")).toBeCloseTo(1, 3);
    expect(val(m, "t")).toBeCloseTo(25, 0);
    expect(chip.regs[0x6b]).toBe(0x40);
  });

  test("PCF8574: classified by pure reads, never written", async () => {
    const writes: number[][] = [];
    const bus = new FakeBus().add(0x27, { read: (n) => new Array(n).fill(0xf7), write: (b) => void writes.push(b) });
    const id = await identify(bus, 0x27, ctx());
    expect(id.probe?.variant).toBe("PCF8574");
    const r = await id.chip!.read!(new Dev(bus, 0x27), state(id.probe));
    expect(r.pins!.map((p) => p.level).join("")).toBe("11101111");
    expect(writes).toEqual([]);
  });

  test("PCF8574 on an SMBus-only adapter is still classified without writes", async () => {
    const writes: number[][] = [];
    const bus = new FakeBus(SMBUS_ONLY).add(0x20, { read: (n) => new Array(n).fill(0xff), write: (b) => void writes.push(b) });
    expect((await identify(bus, 0x20, ctx())).probe?.variant).toBe("PCF8574");
    expect(writes).toEqual([]);
  });

  test("MCP23017: register file → pins with direction", async () => {
    const chip = new RegChip({ 0x00: 0xff, 0x01: 0x0f, 0x0d: 0x01, 0x12: 0x05, 0x13: 0x80 });
    const bus = new FakeBus().add(0x20, chip);
    const id = await identify(bus, 0x20, ctx());
    expect(id.probe?.variant).toBe("MCP23017 / MCP23008");
    const r = await id.chip!.read!(new Dev(bus, 0x20), state(id.probe));
    expect(r.pins![0]).toMatchObject({ name: "GPA0", level: 1, dir: "in" });
    expect(r.pins![15]).toMatchObject({ name: "GPB7", level: 1, dir: "out" });
    expect(r.pins![8].pull).toBe(true);
  });

  test("0x40: HTU21D is recognised before any TI-ID probe (0xFE = soft reset)", async () => {
    const chip = new CmdChip({ e7: () => [0x02], fcc9: () => [0x32, 0, 0, 0, 0, 0] });
    const bus = new FakeBus().add(0x40, chip);
    const id = await identify(bus, 0x40, ctx());
    expect(id.chip?.id).toBe("htu21d");
    expect(id.probe?.variant).toBe("HTU21D");
    expect(chip.writes.some((w) => w[0] === 0xfe)).toBe(false);
  });

  test("0x40: INA219 (no ID register) identified last and decoded with the shunt setting", async () => {
    const bus = new FakeBus().add(0x40, new WordChip({ 0: 0x399f, 1: 1000, 2: (1250 << 3) | 2, 3: 0, 4: 0, 5: 0 }));
    const id = await identify(bus, 0x40, ctx());
    expect(id.chip?.id).toBe("ina219");
    const r = await id.chip!.read!(new Dev(bus, 0x40), state(id.probe, { shunt: 0.1 }));
    expect(val(r, "v")).toBeCloseTo(5.0, 3);
    expect(val(r, "i")).toBeCloseTo(100, 2);
    expect(val(r, "p")).toBeCloseTo(500, 1);
  });

  test("INA226 / INA260 by TI manufacturer + die id", async () => {
    const bus = new FakeBus()
      .add(0x41, new WordChip({ 0: 0x4127, 1: 4000, 2: 9600, 0xfe: 0x5449, 0xff: 0x2260 }))
      .add(0x42, new WordChip({ 0: 0x6127, 1: 800, 2: 9600, 3: 120, 0xfe: 0x5449, 0xff: 0x2270 }));
    const a = await identify(bus, 0x41, ctx());
    expect(a.probe?.variant).toBe("INA226");
    const ra = await a.chip!.read!(new Dev(bus, 0x41), state(a.probe, { shunt: 0.1 }));
    expect(val(ra, "v")).toBeCloseTo(12, 3);
    expect(val(ra, "i")).toBeCloseTo(100, 2);
    const b = await identify(bus, 0x42, ctx());
    expect(b.probe?.variant).toBe("INA260");
    expect(val(await b.chip!.read!(new Dev(bus, 0x42), state(b.probe)), "i")).toBeCloseTo(1000, 1);
  });

  test("0x48: LM75 vs ADS1115 vs TMP102", async () => {
    const bus = new FakeBus()
      .add(0x48, new WordChip({ ...lm75Words }))
      .add(0x49, new WordChip({ 0: 0x4000, 1: 0x8583, 2: 0x8000, 3: 0x7fff }))
      .add(0x4a, new WordChip({ 0: 0x1900, 1: 0x60a0, 2: 0x4b00, 3: 0x5000 }));
    const lm = await identify(bus, 0x48, ctx());
    expect(lm.chip?.id).toBe("lm75");
    expect(val(await lm.chip!.read!(new Dev(bus, 0x48), state(lm.probe)), "t")).toBe(25.5);
    const ads = await identify(bus, 0x49, ctx());
    expect(ads.probe?.variant).toBe("ADS1115");
    expect((await ads.chip!.read!(new Dev(bus, 0x49), state(ads.probe))).stale).toBe(true); // single-shot POR
    const tmp = await identify(bus, 0x4a, ctx());
    expect(tmp.chip?.id).toBe("tmp102");
    expect(val(await tmp.chip!.read!(new Dev(bus, 0x4a), state(tmp.probe)), "t")).toBe(25);
  });

  test("SHT3x and SHT4x by CRC-checked read commands; measure decodes", async () => {
    const sht3 = new CmdChip({ f32d: () => sens(0x8010), "2400": () => sens(0x6666, 0x8000) });
    const sht4 = new CmdChip({ "89": () => sens(0x1234, 0x5678), fd: () => sens(0x6666, 0x8000) });
    const bus = new FakeBus().add(0x44, sht4).add(0x45, sht3);
    const a = await identify(bus, 0x44, ctx());
    expect(a.chip?.id).toBe("sht4x");
    const b = await identify(bus, 0x45, ctx());
    expect(b.chip?.id).toBe("sht3x");
    const m = await b.chip!.measure!(new Dev(bus, 0x45), state(b.probe));
    expect(val(m, "t")).toBeCloseTo(25, 0);
    expect(val(m, "rh")).toBeCloseTo(50, 0);
  });

  test("0x62: an MCP4725 answers the bare read and is never sent SCD4x commands", async () => {
    const writes: number[][] = [];
    const bus = new FakeBus().add(0x62, { read: (n) => [0xc0, 0x80, 0x00, 0x08, 0x00].slice(0, n), write: (b) => void writes.push(b) });
    const id = await identify(bus, 0x62, ctx());
    expect(id.chip?.id).toBe("mcp4725");
    expect(writes).toEqual([]);
    const r = await id.chip!.read!(new Dev(bus, 0x62), state(id.probe, { vdd: 3.3 }));
    expect(val(r, "code")).toBe(2048);
    expect(val(r, "vout")).toBeCloseTo(1.65, 3);
  });

  test("0x62: SCD4x (NACKs bare reads) by serial-number command", async () => {
    const bus = new FakeBus().add(0x62, new CmdChip({ "3682": () => sens(0xabcd, 0x1234, 0x5678), e4b8: () => sens(0x0000) }));
    const id = await identify(bus, 0x62, ctx());
    expect(id.chip?.id).toBe("scd4x");
    expect(id.probe?.data.periodic).toBe(false);
  });

  test("TCA9548A at 0x70: receive-byte only", async () => {
    const writes: number[][] = [];
    const bus = new FakeBus().add(0x70, { read: () => [0x05], write: (b) => void writes.push(b) });
    const id = await identify(bus, 0x70, ctx());
    expect(id.chip?.id).toBe("tca9548");
    expect(val(await id.chip!.read!(new Dev(bus, 0x70), state(id.probe)), "mask")).toBe("0, 2");
    expect(writes).toEqual([]);
  });

  test("PCA9685 at 0x40 and its All Call address", async () => {
    const regs: Record<number, number> = { 0x00: 0x21, 0x01: 0x04, 0x02: 0xe2, 0x03: 0xe4, 0x04: 0xe8, 0x05: 0xe0, 0xfe: 121 };
    // channel 0: on 0, off 2048 (50 %); channel 1 full on
    Object.assign(regs, { 0x06: 0, 0x07: 0, 0x08: 0x00, 0x09: 0x08, 0x0b: 0x10 });
    const bus = new FakeBus().add(0x40, new RegChip(regs)).add(0x70, { read: () => [0] });
    const c = ctx();
    const id = await identify(bus, 0x40, c);
    expect(id.chip?.id).toBe("pca9685");
    c.found.set(0x40, "pca9685");
    expect((await identify(bus, 0x70, c)).chip?.id).toBe("pca9685-allcall");
    const r = await id.chip!.read!(new Dev(bus, 0x40), state(id.probe));
    expect(val(r, "freq")).toBeCloseTo(50, 0);
    expect(r.channels![0].duty).toBe(50);
    expect(r.channels![1].full).toBe("on");
  });

  test("DS3231 RTC time + temperature; EEPROM with HAT header", async () => {
    const rtcRegs = [0x30, 0x45, 0x13, 0x05, 0x17, 0x10, 0x26, 0, 0, 0, 0, 0, 0, 0, 0x1c, 0x00, 0, 0x19, 0x40];
    const bus = new FakeBus().add(0x68, new RegChip(rtcRegs)).add(0x50, new RegChip(eepromRegs()));
    const id = await identify(bus, 0x68, ctx());
    expect(id.probe?.variant).toBe("DS3231");
    const r = await id.chip!.read!(new Dev(bus, 0x68), state(id.probe));
    expect(val(r, "time")).toBe("2026-10-17 13:45:30");
    expect(val(r, "t")).toBe(25.25);
    const e = await identify(bus, 0x50, ctx());
    expect(e.chip?.id).toBe("at24");
    expect(val(await e.chip!.read!(new Dev(bus, 0x50), state(e.probe, { size: 256 })), "hat")).toBe("DeskyPi · Bench HAT");
  });

  test("claimed (UU) addresses throw EBUSY rather than being probed", async () => {
    const bus = new FakeBus().add(0x76, new RegChip(bme280Regs()));
    bus.claimed.add(0x76);
    expect(() => bus.quick(0x76)).toThrow(/claimed/);
  });

  test("every chip id resolves", () => {
    for (const id of ["bosch", "mpu", "rtc", "at24", "lm75", "ina219", "sht3x", "scd4x", "expander", "ds2482"]) expect(CHIP_BY_ID.has(id)).toBe(true);
  });
});
