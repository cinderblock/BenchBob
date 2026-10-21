// The UF2 check BenchBob's debug probes panel runs before it copies a file to a BOOTSEL
// drive, as the old Bun server had it (the server's own is Rust now: backend/crates/probes).
// The tests use it as an independent decoder of what the encoders here produce.

const UF2_MAGIC0 = 0x0a324655;
const UF2_MAGIC1 = 0x9e5d5157;
const UF2_MAGIC_END = 0x0ab16f30;
const UF2_FLAG_FAMILY = 0x2000;

export interface Uf2Info {
  blocks: number;
  familyIds: number[];
  start: number;
  end: number;
  payload: number;
}

/** Validate a UF2 file: every 512-byte block must carry both start magics and the end magic. */
export function parseUf2(buf: Uint8Array): Uf2Info {
  if (buf.length === 0 || buf.length % 512) throw new Error(`Not a UF2 file: ${buf.length} bytes isn't a whole number of 512-byte blocks`);
  const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const fams = new Set<number>();
  let start = Infinity,
    end = 0,
    payload = 0;
  for (let o = 0; o < buf.length; o += 512) {
    if (dv.getUint32(o, true) !== UF2_MAGIC0 || dv.getUint32(o + 4, true) !== UF2_MAGIC1 || dv.getUint32(o + 508, true) !== UF2_MAGIC_END)
      throw new Error(`Not a UF2 file: bad magic in block ${o / 512}`);
    const flags = dv.getUint32(o + 8, true);
    const addr = dv.getUint32(o + 12, true);
    const size = dv.getUint32(o + 16, true);
    if (flags & 1) continue; // "not main flash" blocks are comments/extra data
    if (flags & UF2_FLAG_FAMILY) fams.add(dv.getUint32(o + 28, true));
    start = Math.min(start, addr);
    end = Math.max(end, addr + size);
    payload += size;
  }
  return { blocks: buf.length / 512, familyIds: [...fams], start: start === Infinity ? 0 : start, end, payload };
}

/** Families a BOOTSEL device will accept (others are silently ignored by the bootrom). */
export function uf2Accepts(chip: string, familyIds: number[]): boolean {
  if (!familyIds.length) return true;
  const ok = chip === "rp2040" ? [0xe48bff56] : [0xe48bff57, 0xe48bff58, 0xe48bff59, 0xe48bff5a, 0xe48bff5b];
  return familyIds.some((f) => ok.includes(f));
}
