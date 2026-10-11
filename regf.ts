// Minimal read-only reader for Windows registry hive files (the "regf" format) — enough to
// pull values out of BCD stores and the SOFTWARE / SYSTEM hives on a mounted Windows drive.
//
// Hives are read by random access through a small page cache rather than loaded whole:
// SOFTWARE is often 100 MB+, and a lookup touches only a few dozen cells of it.
//
// Every offset comes from an untrusted file, so reads are bounds-checked and list walks
// are capped; a damaged hive throws rather than looping.

import { open, type FileHandle } from "node:fs/promises";

const PAGE = 4096;
const MAX_CACHED_PAGES = 4096; // 16 MB
const HBIN_BASE = 0x1000;
const MAX_LIST = 100_000;

export const REG_SZ = 1, REG_EXPAND_SZ = 2, REG_BINARY = 3, REG_DWORD = 4, REG_MULTI_SZ = 7, REG_QWORD = 11;

export interface RegKey {
  name: string;
  subkeyCount: number;
  subkeyList: number;
  valueCount: number;
  valueList: number;
}

export interface RegValue {
  name: string;
  type: number;
  size: number;
  inline: boolean;
  offset: number; // or the inline data itself
}

const utf16 = new TextDecoder("utf-16le");
const latin1 = new TextDecoder("latin1");

function dv(b: Uint8Array): DataView {
  return new DataView(b.buffer, b.byteOffset, b.byteLength);
}

export class Hive {
  private pages = new Map<number, Uint8Array>();

  private constructor(private fh: FileHandle, private size: number) {}

  static async open(path: string): Promise<Hive | null> {
    let fh: FileHandle;
    try {
      fh = await open(path, "r");
    } catch {
      return null;
    }
    const hive = new Hive(fh, (await fh.stat()).size);
    try {
      if (latin1.decode(await hive.bytes(0, 4)) === "regf") return hive;
    } catch {}
    await fh.close();
    return null;
  }

  async close(): Promise<void> {
    await this.fh.close();
  }

  private async bytes(off: number, len: number): Promise<Uint8Array> {
    if (off < 0 || len < 0 || off + len > this.size) throw new Error("regf: read out of range");
    const out = new Uint8Array(len);
    let done = 0;
    while (done < len) {
      const p = Math.floor((off + done) / PAGE);
      let page = this.pages.get(p);
      if (!page) {
        const buf = new Uint8Array(PAGE);
        const { bytesRead } = await this.fh.read(buf, 0, PAGE, p * PAGE);
        page = buf.subarray(0, bytesRead);
        if (this.pages.size >= MAX_CACHED_PAGES) this.pages.clear();
        this.pages.set(p, page);
      }
      const at = off + done - p * PAGE;
      const n = Math.min(len - done, page.length - at);
      if (n <= 0) throw new Error("regf: short read");
      out.set(page.subarray(at, at + n), done);
      done += n;
    }
    return out;
  }

  // Cell offsets are relative to the first hive bin; the cell starts with its size
  // (negative while allocated), followed by the data.
  private async cell(off: number): Promise<Uint8Array> {
    const size = Math.abs(dv(await this.bytes(HBIN_BASE + off, 4)).getInt32(0, true));
    if (size < 8 || size > 16 * 1024 * 1024) throw new Error("regf: bad cell size");
    return this.bytes(HBIN_BASE + off + 4, size - 4);
  }

  async root(): Promise<RegKey> {
    return this.key(dv(await this.bytes(0x24, 4)).getUint32(0, true));
  }

  private async key(off: number): Promise<RegKey> {
    const c = await this.cell(off);
    const v = dv(c);
    if (c[0] !== 0x6e || c[1] !== 0x6b) throw new Error("regf: expected nk"); // "nk"
    const flags = v.getUint16(2, true);
    const nameLen = v.getUint16(72, true);
    const raw = c.subarray(76, 76 + nameLen);
    return {
      name: flags & 0x20 ? latin1.decode(raw) : utf16.decode(raw),
      subkeyCount: v.getUint32(20, true),
      subkeyList: v.getUint32(28, true),
      valueCount: v.getUint32(36, true),
      valueList: v.getUint32(40, true),
    };
  }

  // Offsets of every key in a subkey list; "ri" is a list of lists, one level deep.
  private async listOffsets(off: number, depth = 0): Promise<number[]> {
    const c = await this.cell(off);
    const v = dv(c);
    const sig = latin1.decode(c.subarray(0, 2));
    const n = Math.min(v.getUint16(2, true), MAX_LIST);
    const out: number[] = [];
    if (sig === "lf" || sig === "lh") {
      for (let i = 0; i < n; i++) out.push(v.getUint32(4 + i * 8, true));
    } else if (sig === "li") {
      for (let i = 0; i < n; i++) out.push(v.getUint32(4 + i * 4, true));
    } else if (sig === "ri" && depth === 0) {
      for (let i = 0; i < n; i++) out.push(...(await this.listOffsets(v.getUint32(4 + i * 4, true), 1)));
    } else {
      throw new Error(`regf: unexpected subkey list ${sig}`);
    }
    return out;
  }

  async subkeys(k: RegKey): Promise<RegKey[]> {
    if (!k.subkeyCount || k.subkeyList === 0xffffffff) return [];
    const out: RegKey[] = [];
    for (const o of await this.listOffsets(k.subkeyList)) out.push(await this.key(o));
    return out;
  }

  async child(k: RegKey, name: string): Promise<RegKey | null> {
    const want = name.toLowerCase();
    for (const s of await this.subkeys(k)) if (s.name.toLowerCase() === want) return s;
    return null;
  }

  async path(k: RegKey, parts: string[]): Promise<RegKey | null> {
    let cur: RegKey | null = k;
    for (const p of parts) {
      if (!cur) return null;
      cur = await this.child(cur, p);
    }
    return cur;
  }

  async values(k: RegKey): Promise<RegValue[]> {
    if (!k.valueCount || k.valueList === 0xffffffff) return [];
    const list = dv(await this.cell(k.valueList));
    const out: RegValue[] = [];
    for (let i = 0; i < Math.min(k.valueCount, MAX_LIST); i++) {
      const c = await this.cell(list.getUint32(i * 4, true));
      const v = dv(c);
      if (c[0] !== 0x76 || c[1] !== 0x6b) continue; // "vk"
      const nameLen = v.getUint16(2, true);
      const rawSize = v.getUint32(4, true);
      const raw = c.subarray(20, 20 + nameLen);
      out.push({
        name: v.getUint16(16, true) & 1 ? latin1.decode(raw) : utf16.decode(raw),
        type: v.getUint32(12, true),
        size: rawSize & 0x7fffffff,
        inline: !!(rawSize & 0x80000000),
        offset: v.getUint32(8, true),
      });
    }
    return out;
  }

  async data(val: RegValue): Promise<Uint8Array> {
    if (val.inline) {
      const b = new Uint8Array(4);
      dv(b).setUint32(0, val.offset, true);
      return b.subarray(0, Math.min(val.size, 4));
    }
    const c = await this.cell(val.offset);
    // Values over ~16 KB are split into "db" big-data segments.
    if (val.size > c.length && c[0] === 0x64 && c[1] === 0x62) {
      const v = dv(c);
      const segs = dv(await this.cell(v.getUint32(4, true)));
      const out = new Uint8Array(val.size);
      let at = 0;
      for (let i = 0; i < v.getUint16(2, true) && at < val.size; i++) {
        const s = await this.cell(segs.getUint32(i * 4, true));
        const n = Math.min(s.length, val.size - at, 16344);
        out.set(s.subarray(0, n), at);
        at += n;
      }
      return out;
    }
    return c.subarray(0, Math.min(val.size, c.length));
  }

  // Value as a JS value: string for SZ, string[] for MULTI_SZ, number for DWORD/QWORD,
  // bytes otherwise. Name match is case-insensitive, like Windows.
  async get(k: RegKey | null, name: string): Promise<string | string[] | number | Uint8Array | null> {
    if (!k) return null;
    const want = name.toLowerCase();
    const val = (await this.values(k)).find((x) => x.name.toLowerCase() === want);
    if (!val) return null;
    const d = await this.data(val);
    switch (val.type) {
      case REG_SZ:
      case REG_EXPAND_SZ:
        return utf16.decode(d).replace(/\0+$/, "");
      case REG_MULTI_SZ:
        return utf16.decode(d).split("\0").filter(Boolean);
      case REG_DWORD:
        return d.length >= 4 ? dv(d).getUint32(0, true) : null;
      case REG_QWORD:
        return d.length >= 8 ? Number(dv(d).getBigUint64(0, true)) : null;
      default:
        return d;
    }
  }

  async str(k: RegKey | null, name: string): Promise<string | null> {
    const v = await this.get(k, name);
    return typeof v === "string" ? v : null;
  }

  async num(k: RegKey | null, name: string): Promise<number | null> {
    const v = await this.get(k, name);
    return typeof v === "number" ? v : null;
  }
}

// Opens, runs, and always closes.
export async function withHive<T>(path: string, fn: (h: Hive) => Promise<T>): Promise<T | null> {
  const h = await Hive.open(path);
  if (!h) return null;
  try {
    return await fn(h);
  } finally {
    await h.close();
  }
}
