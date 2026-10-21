import { describe, expect, test } from "bun:test";
import { parseUf2, uf2Accepts } from "./uf2-parse.ts";
import { elfToUf2 } from "./elf2uf2.ts";
import { checkConfig, configUf2, crc32, decodeConfigUf2, PICO2W_CONFIG_ADDR, UF2 } from "./nodeconfig.ts";

const dir = import.meta.dir;
// Also checked byte-for-byte by firmware/proto (uf2.rs), so all three encoders agree.
const EXAMPLE = { ssid: "Bench Lab", password: "hunter2hunter2", hostname: "bench-node-1" };
const fixture = async () => new Uint8Array(await Bun.file(`${dir}/testdata/example-config.uf2`).arrayBuffer());

/** The encoder inlined in config.html, loaded the way the page runs it. */
async function htmlEncoder(): Promise<{ configUf2: (c: object) => Uint8Array }> {
  const html = await Bun.file(`${dir}/config.html`).text();
  const src = html.match(/<script id="nodeconfig">([\s\S]*?)<\/script>/)?.[1];
  if (!src) throw new Error("no nodeconfig script in config.html");
  return new Function(`${src}\nreturn NodeConfig;`)();
}

describe("config.uf2", () => {
  test("matches the golden fixture", async () => {
    expect(configUf2(EXAMPLE)).toEqual(await fixture());
  });

  test("config.html's inline encoder produces the same bytes", async () => {
    const enc = await htmlEncoder();
    expect(enc.configUf2(EXAMPLE)).toEqual(await fixture());
    expect(enc.configUf2({ ssid: "open" })).toEqual(configUf2({ ssid: "open" }));
    expect(() => enc.configUf2({ ssid: "x", password: "short" })).toThrow(/8-63/);
  });

  test("covers exactly the config region with the absolute family", async () => {
    const info = parseUf2(await fixture());
    expect(info.familyIds).toEqual([UF2.FAMILY_ABSOLUTE]);
    expect(info.start).toBe(PICO2W_CONFIG_ADDR);
    expect(info.end).toBe(0x10400000);
    expect(info.blocks).toBe(32);
    expect(uf2Accepts("rp2350", info.familyIds)).toBe(true);
  });

  test("round-trips", () => {
    expect(decodeConfigUf2(configUf2(EXAMPLE))).toEqual({ ...EXAMPLE, seq: 1 });
    expect(decodeConfigUf2(configUf2({ ssid: "café ☕" }))).toEqual({ ssid: "café ☕", seq: 1 });
  });

  test("validation", () => {
    expect(() => checkConfig({ ssid: "" })).toThrow(/SSID/);
    expect(() => checkConfig({ ssid: "x".repeat(33) })).toThrow(/SSID/);
    expect(() => checkConfig({ ssid: "x", password: "p".repeat(65) })).toThrow(/64/);
    expect(() => checkConfig({ ssid: "x", hostname: "-bad" })).toThrow(/Hostname/);
    expect(() => checkConfig({ ssid: "x", hostname: "under_score" })).toThrow(/Hostname/);
    expect(() => checkConfig({ ssid: "x", password: "12345678", hostname: "ok-1" })).not.toThrow();
  });

  test("crc32 check value", () => {
    expect(crc32(new TextEncoder().encode("123456789"))).toBe(0xcbf43926);
  });
});

/** A minimal 32-bit Arm ELF with the given PT_LOAD segments (paddr, bytes). */
function fakeElf(segs: [number, Uint8Array][]): Uint8Array {
  const phoff = 52;
  const dataStart = phoff + 32 * segs.length;
  const total = dataStart + segs.reduce((n, [, d]) => n + d.length, 0);
  const out = new Uint8Array(total);
  const dv = new DataView(out.buffer);
  dv.setUint32(0, 0x7f454c46, false);
  out[4] = 1; // 32-bit
  out[5] = 1; // little-endian
  dv.setUint16(18, 40, true); // EM_ARM
  dv.setUint32(28, phoff, true);
  dv.setUint16(42, 32, true);
  dv.setUint16(44, segs.length, true);
  let off = dataStart;
  segs.forEach(([paddr, data], i) => {
    const o = phoff + i * 32;
    dv.setUint32(o, 1, true); // PT_LOAD
    dv.setUint32(o + 4, off, true);
    dv.setUint32(o + 8, paddr, true);
    dv.setUint32(o + 12, paddr, true);
    dv.setUint32(o + 16, data.length, true);
    dv.setUint32(o + 20, data.length, true);
    out.set(data, off);
    off += data.length;
  });
  return out;
}

describe("elf2uf2", () => {
  test("pages, sector fill and the E10 block", () => {
    const a = new Uint8Array(300).map((_, i) => i & 0xff);
    const b = new Uint8Array(10).fill(0xaa);
    const uf2 = elfToUf2(fakeElf([[0x10000000, a], [0x10001104, b]]));
    const blocks = [];
    for (let o = 0; o < uf2.length; o += 512) {
      const dv = new DataView(uf2.buffer, o, 512);
      blocks.push({ flags: dv.getUint32(8, true), addr: dv.getUint32(12, true), no: dv.getUint32(20, true), of: dv.getUint32(24, true), family: dv.getUint32(28, true), first: uf2[o + 32], ext: dv.getUint32(32 + 256, true) });
    }
    // E10: absolute family, end of the 16 MiB window, ignore-block extension.
    expect(blocks[0]).toMatchObject({ addr: 0x10ffff00, family: UF2.FAMILY_ABSOLUTE, flags: 0xa000, no: 0, of: 2, first: 0xef, ext: UF2.EXTENSION_RP2_IGNORE_BLOCK });
    const data = blocks.slice(1);
    // Sector 0: pages 0x000-0xF00 all present (16); sector 1: 0x1000 and 0x1100 (up to the last page).
    expect(data.map((x) => x.addr - 0x10000000)).toEqual([...Array(16)].map((_, i) => i * 256).concat([0x1000, 0x1100]));
    expect(data.every((x, i) => x.family === UF2.FAMILY_RP2350_ARM_S && x.no === i && x.of === 18)).toBe(true);
    // Content lands at the right offsets.
    const page1 = 512 * 2 + 32; // second data block
    expect(uf2[page1]).toBe(0); // byte 256 of a: 256 & 0xff
    expect(uf2[page1 + 43]).toBe(43);
    const last = 512 * 18 + 32;
    expect(uf2[last + 4]).toBe(0xaa);
    expect(uf2[last + 3]).toBe(0);
    expect(parseUf2(uf2).familyIds.sort()).toEqual([UF2.FAMILY_ABSOLUTE, UF2.FAMILY_RP2350_ARM_S]);
  });

  test("rejects non-flash segments and non-ELF input", () => {
    expect(() => elfToUf2(fakeElf([[0x20000000, new Uint8Array(4)]]))).toThrow(/flash/);
    expect(() => elfToUf2(new Uint8Array(64))).toThrow(/ELF/);
  });
});
