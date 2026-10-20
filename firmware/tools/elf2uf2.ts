#!/usr/bin/env bun
// Convert an RP2350 (Arm) ELF to a UF2 for the BOOTSEL drive:
//   bun firmware/tools/elf2uf2.ts <input.elf> <output.uf2>
//
// elf2uf2-rs only knows the RP2040 family, which the RP2350 bootrom rejects, and picotool
// is a C++ build. This does what `picotool uf2 convert` does for a flash image:
// - PT_LOAD segments at their physical (load) addresses, in 256-byte pages, family
//   rp2350-arm-s;
// - every page of each 4 KiB sector the image touches (up to its last page) is present,
//   zero-filled, because the bootrom erases by sector;
// - a leading RP2350-E10 workaround block: an `absolute`-family block for 0x10FFFF00 that
//   carries the RP2 "ignore block" extension, so the bootrom drops it.

import { encodeUf2, UF2, type Uf2Block } from "./nodeconfig.ts";

const FLASH_START = 0x10000000;
const FLASH_END = 0x11000000; // 16 MiB window
const E10_ADDR = 0x10ffff00;

export function elfToUf2(elf: Uint8Array): Uint8Array {
  const dv = new DataView(elf.buffer, elf.byteOffset, elf.byteLength);
  if (dv.getUint32(0, false) !== 0x7f454c46) throw new Error("not an ELF file");
  if (elf[4] !== 1 || elf[5] !== 1) throw new Error("expected a 32-bit little-endian ELF");
  if (dv.getUint16(18, true) !== 40) throw new Error("not an Arm ELF");
  const phoff = dv.getUint32(28, true);
  const phentsize = dv.getUint16(42, true);
  const phnum = dv.getUint16(44, true);

  const pages = new Map<number, Uint8Array>();
  for (let i = 0; i < phnum; i++) {
    const o = phoff + i * phentsize;
    const type = dv.getUint32(o, true);
    const offset = dv.getUint32(o + 4, true);
    const paddr = dv.getUint32(o + 12, true);
    const filesz = dv.getUint32(o + 16, true);
    if (type !== 1 || filesz === 0) continue; // PT_LOAD with file contents only
    if (paddr < FLASH_START || paddr + filesz > FLASH_END) throw new Error(`segment at 0x${paddr.toString(16)} isn't in flash`);
    for (let done = 0; done < filesz; ) {
      const addr = paddr + done;
      const page = addr & ~(UF2.PAYLOAD - 1);
      const inPage = addr - page;
      const n = Math.min(filesz - done, UF2.PAYLOAD - inPage);
      let buf = pages.get(page);
      if (!buf) pages.set(page, (buf = new Uint8Array(UF2.PAYLOAD)));
      buf.set(elf.subarray(offset + done, offset + done + n), inPage);
      done += n;
    }
  }
  if (!pages.size) throw new Error("no loadable segments");

  // Fill out touched sectors, as picotool does.
  const last = Math.max(...pages.keys());
  for (const page of [...pages.keys()]) {
    const sector = page & ~0xfff;
    for (let p = sector; p < sector + 0x1000 && p <= last; p += UF2.PAYLOAD) if (!pages.has(p)) pages.set(p, new Uint8Array(UF2.PAYLOAD));
  }

  const addrs = [...pages.keys()].sort((a, b) => a - b);
  const ext = new Uint8Array(4);
  new DataView(ext.buffer).setUint32(0, UF2.EXTENSION_RP2_IGNORE_BLOCK, true);
  const e10: Uf2Block = { addr: E10_ADDR, data: new Uint8Array(UF2.PAYLOAD).fill(0xef), family: UF2.FAMILY_ABSOLUTE, flags: UF2.FLAG_FAMILY_ID | UF2.FLAG_EXTENSION_FLAGS, extension: ext };
  const blocks: Uf2Block[] = [e10, ...addrs.map((a) => ({ addr: a, data: pages.get(a)!, family: UF2.FAMILY_RP2350_ARM_S }))];
  const numbering = [{ first: 0, total: 2 }, ...addrs.map((_, i) => ({ first: i, total: addrs.length }))];
  return encodeUf2(blocks, numbering);
}

if (import.meta.main) {
  const [input, output] = process.argv.slice(2);
  if (!input || !output) {
    console.error("usage: bun elf2uf2.ts <input.elf> <output.uf2>");
    process.exit(2);
  }
  const uf2 = elfToUf2(new Uint8Array(await Bun.file(input).arrayBuffer()));
  await Bun.write(output, uf2);
  console.log(`${output}: ${uf2.length / UF2.BLOCK - 1} blocks (${uf2.length} bytes)`);
}
