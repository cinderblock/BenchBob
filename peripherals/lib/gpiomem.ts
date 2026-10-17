// Live GPIO levels straight from the BCM283x/BCM2711 GPIO block via /dev/gpiomem.
//
// Spawning `pinctrl get` costs a few ms of CPU each time; at the GPIO panel's 10 Hz that
// was the most expensive thing an open dashboard did. Levels are just two registers
// (GPLEV0/GPLEV1), so map the block once and read them directly. /dev/gpiomem exposes only
// the GPIO registers and is group `gpio`, so this needs no root.
//
// Pi 5 is different: its header GPIOs sit on RP1 behind PCIe with another register layout,
// and /dev/gpiomem there is RP1's. openGpioLevels() returns null on anything but a BCM283x
// or BCM2711, and the caller falls back to pinctrl.

import { dlopen, FFIType, read, type Pointer } from "bun:ffi";
import { closeSync, openSync, readFileSync } from "node:fs";

const PROT_READ = 1;
const MAP_SHARED = 1;
const BLOCK = 4096;
const GPLEV0 = 0x34;
const GPLEV1 = 0x38;

export interface GpioLevels {
  /** Levels of GPIO 0–57 (bit n of the BigInt = GPIOn). */
  read(): bigint;
  close(): void;
}

function isBcmGpio(): boolean {
  try {
    const compat = readFileSync("/proc/device-tree/compatible", "latin1");
    return /brcm,bcm(2835|2836|2837|2711)/.test(compat);
  } catch {
    return false;
  }
}

export function openGpioLevels(path = "/dev/gpiomem"): GpioLevels | null {
  if (!isBcmGpio()) return null;
  let fd: number;
  try {
    fd = openSync(path, "r");
  } catch {
    return null;
  }
  try {
    const libc = dlopen("libc.so.6", {
      mmap: { args: [FFIType.ptr, FFIType.u64, FFIType.i32, FFIType.i32, FFIType.i32, FFIType.i64], returns: FFIType.ptr },
      munmap: { args: [FFIType.ptr, FFIType.u64], returns: FFIType.i32 },
    });
    const base = libc.symbols.mmap(null, BLOCK, PROT_READ, MAP_SHARED, fd, 0) as Pointer | null;
    // mmap's failure value is MAP_FAILED, (void *)-1.
    if (!base || Number(base) === -1 || Number(base) === 0xffffffffffffffff) {
      closeSync(fd);
      libc.close();
      return null;
    }
    closeSync(fd); // the mapping outlives the descriptor
    let open = true;
    return {
      read() {
        if (!open) throw new Error("GPIO levels closed");
        const lo = read.u32(base, GPLEV0);
        const hi = read.u32(base, GPLEV1);
        return (BigInt(hi) << 32n) | BigInt(lo);
      },
      close() {
        if (!open) return;
        open = false;
        libc.symbols.munmap(base, BLOCK);
        libc.close();
      },
    };
  } catch {
    try {
      closeSync(fd);
    } catch {}
    return null;
  }
}
