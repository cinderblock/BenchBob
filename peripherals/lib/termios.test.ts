import { test, expect } from "bun:test";
import { encodeTermios, decodeTermios, normalizeConfig, configLabel, DEFAULT_CONFIG, type SerialConfig } from "./termios.ts";

const u32 = (b: Uint8Array, off: number) => new DataView(b.buffer).getUint32(off, true);

test("standard rate uses the Bnnn code, not BOTHER", () => {
  const b = encodeTermios(DEFAULT_CONFIG);
  expect(b.length).toBe(44);
  const cflag = u32(b, 8);
  expect(cflag & 0x100f).toBe(0x1002); // B115200
  expect(cflag & 0x30).toBe(0x30); // CS8
  expect(cflag & 0x80).toBe(0x80); // CREAD
  expect(cflag & 0x800).toBe(0x800); // CLOCAL
  expect(u32(b, 0)).toBe(0); // raw iflag
  expect(u32(b, 12)).toBe(0); // raw lflag
  expect(b[17 + 6]).toBe(1); // VMIN
  expect(b[17 + 5]).toBe(0); // VTIME
  expect(u32(b, 36)).toBe(115200);
});

test("odd rate uses BOTHER with the speed fields", () => {
  const b = encodeTermios({ ...DEFAULT_CONFIG, baud: 74880 });
  expect(u32(b, 8) & 0x100f).toBe(0x1000);
  expect(u32(b, 40)).toBe(74880);
  expect(decodeTermios(b).baud).toBe(74880);
});

test("round trip of framing and flow", () => {
  const cases: SerialConfig[] = [
    { baud: 9600, dataBits: 7, parity: "even", stopBits: 2, flow: "none" },
    { baud: 250000, dataBits: 8, parity: "odd", stopBits: 1, flow: "rtscts" },
    { baud: 1200, dataBits: 5, parity: "mark", stopBits: 1, flow: "xonxoff" },
    { baud: 3000000, dataBits: 6, parity: "space", stopBits: 2, flow: "none" },
  ];
  for (const c of cases) expect(decodeTermios(encodeTermios(c))).toEqual(c);
});

test("mark/space set CMSPAR, rtscts sets the top bit", () => {
  const m = u32(encodeTermios({ ...DEFAULT_CONFIG, parity: "mark" }), 8);
  expect(m & 0x40000000).toBe(0x40000000);
  expect(m & 0x300).toBe(0x300); // PARENB|PARODD
  const r = u32(encodeTermios({ ...DEFAULT_CONFIG, flow: "rtscts" }), 8);
  expect(r >>> 31).toBe(1);
});

test("base termios keeps HUPCL and c_line", () => {
  const base = new Uint8Array(44);
  new DataView(base.buffer).setUint32(8, 0x400 | 0x0f, true); // HUPCL + B38400
  base[16] = 0; // N_TTY
  const b = encodeTermios(DEFAULT_CONFIG, base);
  expect(u32(b, 8) & 0x400).toBe(0x400);
});

test("normalizeConfig validates and accepts shorthands", () => {
  expect(normalizeConfig({ baud: "57600", parity: "E", flow: "RTS/CTS" } as any)).toEqual({ baud: 57600, dataBits: 8, parity: "even", stopBits: 1, flow: "rtscts" });
  expect(() => normalizeConfig({ baud: 12.5 })).toThrow(/baud/);
  expect(() => normalizeConfig({ dataBits: 9 as any })).toThrow(/Data bits/);
  expect(() => normalizeConfig({ stopBits: 1.5 as any })).toThrow(/Stop bits/);
  expect(() => normalizeConfig({ parity: "x" as any })).toThrow(/Parity/);
  expect(configLabel({ baud: 9600, dataBits: 7, parity: "even", stopBits: 1, flow: "xonxoff" })).toBe("9600 7E1 XON/XOFF");
});
