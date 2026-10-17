// Register images of real chips, shared by the unit tests and the i2c-stub preload used to
// test the driver end to end on desky (`bun peripherals/lib/i2c-fixtures.ts <bus>` prints the
// i2cset commands). Calibration values are the worked examples from the Bosch datasheets,
// so the decoded numbers are known: BME280/BMP280 adc_T 519888 → 25.08 °C, adc_P 415148 →
// 100653.27 Pa.

const le = (v: number) => [v & 0xff, (v >> 8) & 0xff];

/** BME280 bytes by register (0x88..0xA1 calibration, 0xD0 id, 0xE1..0xE7 humidity cal, data). */
export function bme280Regs(): Record<number, number> {
  const cal = [27504, 26435, -1000, 36477, -10685, 3024, 2855, 140, -7, 15500, -14600, 6000].flatMap((v) => le(v & 0xffff));
  const r: Record<number, number> = {};
  cal.forEach((b, i) => (r[0x88 + i] = b));
  r[0xa1] = 75; // H1
  // H2=362, H3=0, H4=313, H5=50, H6=30 (typical BME280 values)
  [0x6a, 0x01, 0x00, 0x13, 0x29, 0x03, 0x1e].forEach((b, i) => (r[0xe1 + i] = b));
  r[0xd0] = 0x60;
  r[0xf2] = 0x01; // ctrl_hum osrs_h ×1
  r[0xf4] = 0x27; // normal mode, osrs_t/p ×1
  // adc_P 415148, adc_T 519888, adc_H 27000
  [0x65, 0x5a, 0xc0, 0x7e, 0xed, 0x00, 0x69, 0x78].forEach((b, i) => (r[0xf7 + i] = b));
  return r;
}

/** MPU-6050, awake, ±2 g / ±250 °/s, lying flat (Z = +1 g), die 25 °C-ish. */
export function mpu6050Regs(): Record<number, number> {
  const r: Record<number, number> = { 0x75: 0x68, 0x6b: 0x00, 0x1b: 0x00, 0x1c: 0x00 };
  const w = (v: number) => [(v >> 8) & 0xff, v & 0xff];
  const temp = Math.round((25 - 36.53) * 340) & 0xffff;
  [...w(164), ...w(-328 & 0xffff), ...w(16384), ...w(temp), ...w(131), ...w(0), ...w(-262 & 0xffff)].forEach((b, i) => (r[0x3b + i] = b));
  return r;
}

/** 24C02 with a Raspberry Pi HAT header and a vendor atom. */
export function eepromRegs(): Record<number, number> {
  const vendor = "DeskyPi";
  const product = "Bench HAT";
  const atom = [...new Array(16).fill(0).map((_, i) => i + 1), ...le(0x1234), ...le(2), vendor.length, product.length, ...[...vendor, ...product].map((c) => c.charCodeAt(0))];
  const bytes = [..."R-Pi"].map((c) => c.charCodeAt(0)).concat([1, 0, ...le(1), ...le(12 + 8 + atom.length), 0, 0, ...le(1), ...le(0), ...le(atom.length), 0, 0, ...atom]);
  const r: Record<number, number> = {};
  bytes.forEach((b, i) => (r[i] = b));
  return r;
}

/** LM75 words (big-endian values as the chip holds them): 25.5 °C, default thresholds. */
export const lm75Words: Record<number, number> = { 0x00: 0x1980, 0x01: 0x0000, 0x02: 0x4b00, 0x03: 0x5000 };

/** i2cset commands that load these images into i2c-stub (chip_addr=0x76,0x48,0x68,0x50). */
export function stubCommands(bus: number): string[] {
  const cmds: string[] = [];
  const bytes = (addr: number, regs: Record<number, number>) => {
    for (const [k, v] of Object.entries(regs)) cmds.push(`i2cset -y ${bus} 0x${addr.toString(16)} 0x${Number(k).toString(16)} 0x${v.toString(16)} b`);
  };
  bytes(0x76, bme280Regs());
  bytes(0x68, mpu6050Regs());
  bytes(0x50, eepromRegs());
  // i2c-stub keeps one 16-bit word per register; SMBus words are low byte first, so store
  // the byte-swapped value for big-endian chips.
  for (const [k, v] of Object.entries(lm75Words)) cmds.push(`i2cset -y ${bus} 0x48 0x${Number(k).toString(16)} 0x${(((v & 0xff) << 8) | (v >> 8)).toString(16)} w`);
  return cmds;
}

if (import.meta.main) console.log(stubCommands(Number(process.argv[2] ?? 22)).join("\n"));
