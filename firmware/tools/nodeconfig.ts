// BenchBob node config record and UF2 encoding: the TypeScript twin of
// firmware/proto/src/config.rs and uf2.rs. Same bytes, pinned by
// testdata/example-config.uf2, which tools.test.ts and the Rust tests both check.
// config.html carries an inline copy of the browser-facing part (checked by the tests too).

export const PICO2W_CONFIG_ADDR = 0x103fe000; // last 8 KiB of the Pico 2 W's 4 MiB flash
export const SLOT_SIZE = 4096;
export const REGION_SIZE = 2 * SLOT_SIZE;

export const UF2 = {
  MAGIC_START0: 0x0a324655,
  MAGIC_START1: 0x9e5d5157,
  MAGIC_END: 0x0ab16f30,
  FLAG_FAMILY_ID: 0x2000,
  FLAG_EXTENSION_FLAGS: 0x8000,
  FAMILY_ABSOLUTE: 0xe48bff57,
  FAMILY_RP2350_ARM_S: 0xe48bff59,
  EXTENSION_RP2_IGNORE_BLOCK: 0x9957e304,
  PAYLOAD: 256,
  BLOCK: 512,
} as const;

export interface NodeConfig {
  ssid: string;
  password?: string;
  /** Empty or missing: the firmware uses benchbob-<last 3 MAC bytes>. */
  hostname?: string;
}

const enc = new TextEncoder();

export function crc32(...parts: Uint8Array[]): number {
  let crc = 0xffffffff;
  for (const p of parts)
    for (const b of p) {
      crc ^= b;
      for (let k = 0; k < 8; k++) crc = crc & 1 ? (crc >>> 1) ^ 0xedb88320 : crc >>> 1;
    }
  return (crc ^ 0xffffffff) >>> 0;
}

export function validHostname(h: string): boolean {
  return /^[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?$/.test(h);
}

/** Throws with a user-facing message if the config can't be stored. */
export function checkConfig(c: NodeConfig): void {
  const ssid = enc.encode(c.ssid);
  const pw = enc.encode(c.password ?? "");
  if (ssid.length < 1 || ssid.length > 32) throw new Error("SSID must be 1-32 bytes");
  if (pw.length > 64) throw new Error("Password must be at most 64 bytes");
  if (pw.length > 0 && pw.length < 8) throw new Error("WPA passwords are 8-63 characters (or 64 hex digits); leave it empty for an open network");
  if (c.hostname && !validHostname(c.hostname)) throw new Error("Hostname: letters, digits and hyphens, 1-63 characters, not starting or ending with a hyphen");
}

/** One config record (header + TLVs), as the firmware writes it. */
export function encodeRecord(c: NodeConfig, seq: number): Uint8Array {
  checkConfig(c);
  const fields: [number, Uint8Array][] = [[1, enc.encode(c.ssid)]];
  if (c.password) fields.push([2, enc.encode(c.password)]);
  if (c.hostname) fields.push([3, enc.encode(c.hostname)]);
  const len = fields.reduce((n, [, v]) => n + 2 + v.length, 0);
  const out = new Uint8Array(16 + len);
  const dv = new DataView(out.buffer);
  out.set(enc.encode("BBNC"), 0);
  out[4] = 1; // format
  out[5] = 0; // flags
  dv.setUint16(6, len, true);
  dv.setUint32(8, seq >>> 0, true);
  let i = 16;
  for (const [tag, v] of fields) {
    out[i] = tag;
    out[i + 1] = v.length;
    out.set(v, i + 2);
    i += 2 + v.length;
  }
  dv.setUint32(12, crc32(out.subarray(0, 12), out.subarray(16)), true);
  return out;
}

/** The whole 8 KiB region: slot 0 holds the record (sequence 1), the rest is erased. */
export function regionImage(c: NodeConfig): Uint8Array {
  const img = new Uint8Array(REGION_SIZE).fill(0xff);
  img.set(encodeRecord(c, 1), 0);
  return img;
}

export interface Uf2Block {
  addr: number;
  data: Uint8Array; // ≤ 256 bytes
  family: number;
  flags?: number;
  /** Bytes placed after the payload (UF2 extension tags). */
  extension?: Uint8Array;
}

/** Encode blocks, numbering them 0..n-1 of n. */
export function encodeUf2(blocks: Uf2Block[], numbering?: { first: number; total: number }[]): Uint8Array {
  const out = new Uint8Array(blocks.length * UF2.BLOCK);
  blocks.forEach((b, i) => {
    const o = i * UF2.BLOCK;
    const dv = new DataView(out.buffer, o, UF2.BLOCK);
    const n = numbering?.[i] ?? { first: i, total: blocks.length };
    dv.setUint32(0, UF2.MAGIC_START0, true);
    dv.setUint32(4, UF2.MAGIC_START1, true);
    dv.setUint32(8, b.flags ?? UF2.FLAG_FAMILY_ID, true);
    dv.setUint32(12, b.addr, true);
    dv.setUint32(16, b.data.length, true);
    dv.setUint32(20, n.first, true);
    dv.setUint32(24, n.total, true);
    dv.setUint32(28, b.family, true);
    out.set(b.data, o + 32);
    if (b.extension) out.set(b.extension, o + 32 + b.data.length);
    dv.setUint32(508, UF2.MAGIC_END, true);
  });
  return out;
}

/** A UF2 that writes only the config region, for the BOOTSEL drive (absolute family). */
export function configUf2(c: NodeConfig, base = PICO2W_CONFIG_ADDR): Uint8Array {
  const img = regionImage(c);
  const blocks: Uf2Block[] = [];
  for (let off = 0; off < img.length; off += UF2.PAYLOAD)
    blocks.push({ addr: base + off, data: img.subarray(off, off + UF2.PAYLOAD), family: UF2.FAMILY_ABSOLUTE });
  return encodeUf2(blocks);
}

/** Read the record back out of a config UF2 (for tests and `config-uf2.ts --check`). */
export function decodeConfigUf2(uf2: Uint8Array, base = PICO2W_CONFIG_ADDR): NodeConfig & { seq: number } {
  const img = new Uint8Array(REGION_SIZE).fill(0xff);
  for (let o = 0; o < uf2.length; o += UF2.BLOCK) {
    const dv = new DataView(uf2.buffer, uf2.byteOffset + o, UF2.BLOCK);
    if (dv.getUint32(0, true) !== UF2.MAGIC_START0 || dv.getUint32(508, true) !== UF2.MAGIC_END) throw new Error(`bad UF2 block at ${o}`);
    const addr = dv.getUint32(12, true);
    const size = dv.getUint32(16, true);
    if (addr < base || addr + size > base + REGION_SIZE) throw new Error(`block outside the config region: 0x${addr.toString(16)}`);
    img.set(uf2.subarray(o + 32, o + 32 + size), addr - base);
  }
  const dv = new DataView(img.buffer);
  if (new TextDecoder().decode(img.subarray(0, 4)) !== "BBNC" || img[4] !== 1) throw new Error("no config record in slot 0");
  const len = dv.getUint16(6, true);
  const seq = dv.getUint32(8, true);
  if (crc32(img.subarray(0, 12), img.subarray(16, 16 + len)) !== dv.getUint32(12, true)) throw new Error("config record CRC mismatch");
  const dec = new TextDecoder();
  const c: NodeConfig & { seq: number } = { ssid: "", seq };
  for (let i = 16; i + 2 <= 16 + len; ) {
    const [tag, l] = [img[i], img[i + 1]];
    const v = dec.decode(img.subarray(i + 2, i + 2 + l));
    if (tag === 1) c.ssid = v;
    else if (tag === 2) c.password = v;
    else if (tag === 3) c.hostname = v;
    i += 2 + l;
  }
  return c;
}
