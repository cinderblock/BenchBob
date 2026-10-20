// ==================== GPIO character device (v2 uAPI) over bun:ffi ====================
//
// The board-independent way to see GPIO: every /dev/gpiochipN, its label and line count,
// and per line the name, consumer, direction and flags, straight from the kernel with
// ioctl() (no libgpiod, no gpioinfo process). This also covers USB GPIO adapters (FT232H,
// CH341, MCP2221, CP2112): their drivers register ordinary gpiochips.
//
// What v2 can't do is read a level passively. A value is only readable through a line
// *request*, and a request can disturb the pin: a direction flag reconfigures it, and the
// request itself fails with EBUSY if a driver owns the line. So levels are only offered for
// lines the user picks to watch, and those are requested "as is" (no INPUT/OUTPUT flag):
// the kernel then leaves direction, bias and drive alone and just lets us read the value.
// The request is released (its fd closed) when the watch ends.
//
// Struct layouts and ioctl numbers are from include/uapi/linux/gpio.h (v2 since Linux 5.10)
// and identical on arm64 and x86-64. Pure encode/decode is exported for the unit tests;
// libc is only opened on the first real ioctl, so the module loads anywhere.

import { closeSync, openSync, readdirSync, readFileSync, realpathSync } from "node:fs";
import { dlopen, FFIType, read } from "bun:ffi";

// ---------- Constants (include/uapi/linux/gpio.h) ----------

const NAME = 32; // GPIO_MAX_NAME_SIZE
const LINES_MAX = 64; // GPIO_V2_LINES_MAX
const ATTRS_MAX = 10; // GPIO_V2_LINE_NUM_ATTRS_MAX

export const CHIPINFO_SIZE = NAME * 2 + 4; // name, label, lines = 68
export const LINEINFO_SIZE = 256; // name, consumer, offset, num_attrs, flags, attrs[10], padding[4]
export const LINEREQ_SIZE = 592; // offsets[64], consumer, config (272), num_lines, event_buffer_size, padding[5], fd
export const VALUES_SIZE = 16; // bits, mask

/** _IOC(dir, type, nr, size) with the asm-generic layout arm64 and x86 share. */
export const ioc = (dir: number, type: number, nr: number, size: number) => ((dir << 30) | (size << 16) | (type << 8) | nr) >>> 0;
const R = 2, RW = 3;
export const GPIO_GET_CHIPINFO_IOCTL = ioc(R, 0xb4, 0x01, CHIPINFO_SIZE);
export const GPIO_V2_GET_LINEINFO_IOCTL = ioc(RW, 0xb4, 0x05, LINEINFO_SIZE);
export const GPIO_V2_GET_LINE_IOCTL = ioc(RW, 0xb4, 0x07, LINEREQ_SIZE);
export const GPIO_V2_LINE_GET_VALUES_IOCTL = ioc(RW, 0xb4, 0x0e, VALUES_SIZE);

export const FLAG = {
  USED: 1 << 0,
  ACTIVE_LOW: 1 << 1,
  INPUT: 1 << 2,
  OUTPUT: 1 << 3,
  EDGE_RISING: 1 << 4,
  EDGE_FALLING: 1 << 5,
  OPEN_DRAIN: 1 << 6,
  OPEN_SOURCE: 1 << 7,
  BIAS_PULL_UP: 1 << 8,
  BIAS_PULL_DOWN: 1 << 9,
  BIAS_DISABLED: 1 << 10,
  EVENT_CLOCK_REALTIME: 1 << 11,
  EVENT_CLOCK_HTE: 1 << 12,
} as const;
const ATTR_ID_DEBOUNCE = 3;

// ---------- Pure encode/decode ----------

const cstr = (b: Uint8Array, off: number, len: number) => {
  const s = b.subarray(off, off + len);
  const z = s.indexOf(0);
  return new TextDecoder().decode(z >= 0 ? s.subarray(0, z) : s);
};

export interface ChipInfo {
  name: string; // "gpiochip0"
  label: string; // "pinctrl-bcm2711", "gpio4", "ftdi-cbus"…
  lines: number;
}

export function decodeChipInfo(b: Uint8Array): ChipInfo {
  return { name: cstr(b, 0, NAME), label: cstr(b, NAME, NAME), lines: new DataView(b.buffer, b.byteOffset).getUint32(NAME * 2, true) };
}

export interface LineInfo {
  offset: number;
  name: string;
  /** Who holds the line: a driver ("onewire@4", "ACT", "spi0 CS0"), a process, or null. */
  consumer: string | null;
  /** Requested by someone, or (on pinctrl-strict SoCs) muxed to a peripheral. */
  used: boolean;
  direction: "input" | "output";
  activeLow: boolean;
  drive: "push-pull" | "open-drain" | "open-source";
  bias: "pull-up" | "pull-down" | "disabled" | null;
  edge: "rising" | "falling" | "both" | null;
  debounceUs: number | null;
  /** gpioinfo-style words, for display: ["used", "output", "open-drain"]. */
  flags: string[];
}

/** An empty gpio_v2_line_info with the offset filled in, as GPIO_V2_GET_LINEINFO_IOCTL wants it. */
export function encodeLineInfoReq(offset: number): Uint8Array {
  const b = new Uint8Array(LINEINFO_SIZE);
  new DataView(b.buffer).setUint32(64, offset, true);
  return b;
}

export function decodeLineInfo(b: Uint8Array): LineInfo {
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const offset = v.getUint32(64, true);
  const numAttrs = v.getUint32(68, true);
  // Flags are a u64, but every defined bit fits in the low word.
  const f = v.getUint32(72, true);
  let debounceUs: number | null = null;
  for (let i = 0; i < Math.min(numAttrs, ATTRS_MAX); i++) {
    const a = 80 + i * 16;
    if (v.getUint32(a, true) === ATTR_ID_DEBOUNCE) debounceUs = v.getUint32(a + 8, true);
  }
  const consumer = cstr(b, NAME, NAME);
  const used = !!(f & FLAG.USED);
  const direction = f & FLAG.OUTPUT ? "output" : "input";
  const drive = f & FLAG.OPEN_DRAIN ? "open-drain" : f & FLAG.OPEN_SOURCE ? "open-source" : "push-pull";
  const bias = f & FLAG.BIAS_PULL_UP ? "pull-up" : f & FLAG.BIAS_PULL_DOWN ? "pull-down" : f & FLAG.BIAS_DISABLED ? "disabled" : null;
  const rising = !!(f & FLAG.EDGE_RISING), falling = !!(f & FLAG.EDGE_FALLING);
  const edge = rising && falling ? "both" : rising ? "rising" : falling ? "falling" : null;
  const flags = [used ? "used" : "", direction, f & FLAG.ACTIVE_LOW ? "active-low" : "", drive !== "push-pull" ? drive : "", bias ? (bias === "disabled" ? "bias-disabled" : bias) : "", edge ? `edges=${edge}` : "", debounceUs ? `debounce=${debounceUs}us` : ""].filter(Boolean);
  // A line muxed to a peripheral on a pinctrl-strict SoC is "used" with no consumer; libgpiod
  // shows those as "kernel", and so do we, so "free" always means free.
  return { offset, name: cstr(b, 0, NAME), consumer: consumer || (used ? "kernel" : null), used, direction, activeLow: !!(f & FLAG.ACTIVE_LOW), drive, bias, edge, debounceUs, flags };
}

/**
 * gpio_v2_line_request for `offsets` with config flags `flags` (0 = as-is: direction,
 * bias and drive are left exactly as they are).
 */
export function encodeLineRequest(offsets: number[], consumer: string, flags = 0): Uint8Array {
  if (!offsets.length || offsets.length > LINES_MAX) throw new Error(`1–${LINES_MAX} lines per request`);
  const b = new Uint8Array(LINEREQ_SIZE);
  const v = new DataView(b.buffer);
  offsets.forEach((o, i) => v.setUint32(i * 4, o, true));
  b.set(new TextEncoder().encode(consumer).subarray(0, NAME - 1), 256);
  v.setUint32(288, flags, true); // config.flags (u64, low word)
  // config.num_attrs @296 = 0; attrs @320..560 unused.
  v.setUint32(560, offsets.length, true); // num_lines
  v.setInt32(588, -1, true); // fd, filled in by the kernel
  return b;
}

export const requestFd = (b: Uint8Array) => new DataView(b.buffer, b.byteOffset).getInt32(588, true);

/** gpio_v2_line_values asking for the lines in `mask` (bit i = i-th requested line). */
export function encodeValues(mask: bigint): Uint8Array {
  const b = new Uint8Array(VALUES_SIZE);
  new DataView(b.buffer).setBigUint64(8, mask, true);
  return b;
}

export const decodeValueBits = (b: Uint8Array) => new DataView(b.buffer, b.byteOffset).getBigUint64(0, true);

// ---------- libc ----------

type Libc = { ioctl(fd: number, req: number, arg: Uint8Array): number; errno(): number };
let libc: Libc | null = null;

function lib(): Libc {
  if (libc) return libc;
  // ioctl is variadic; a pointer third argument travels in the same register as a fixed
  // one on aarch64 and x86-64 Linux (see termios.ts).
  const l = dlopen("libc.so.6", {
    ioctl: { args: [FFIType.i32, FFIType.u64, FFIType.ptr], returns: FFIType.i32 },
    __errno_location: { args: [], returns: FFIType.ptr },
  });
  libc = { ioctl: (fd, req, arg) => l.symbols.ioctl(fd, req, arg) as number, errno: () => read.i32(l.symbols.__errno_location()!, 0) };
  return libc;
}

const ERRNO: Record<number, string> = { 1: "EPERM", 2: "ENOENT", 13: "EACCES", 16: "EBUSY", 19: "ENODEV", 22: "EINVAL", 25: "ENOTTY" };

export class GpioError extends Error {
  constructor(public what: string, public errno: number) {
    super(`${what}: ${ERRNO[errno] || `errno ${errno}`}${errno === 16 ? " (the line is in use)" : errno === 13 || errno === 1 ? " (no permission: is BenchBob in the gpio group?)" : ""}`);
  }
}

function ioctl(fd: number, req: number, buf: Uint8Array, what: string) {
  const L = lib();
  if (L.ioctl(fd, req, buf) < 0) throw new GpioError(what, L.errno());
}

// ---------- Chips and lines ----------

export interface Chip extends ChipInfo {
  path: string; // /dev/gpiochip0
  /** The parent device in sysfs: "fe200000.gpio", "fec50000.gpio", "1-1.2:1.0" for USB. */
  device: string;
  usb: boolean;
  error?: string;
}

const DEV = "/dev";

/** The kernel's own names for a chip's parent device, so board files can match a chip by its register base. */
function sysDevice(name: string): { device: string; usb: boolean } {
  try {
    const p = realpathSync(`/sys/bus/gpio/devices/${name}`);
    const parts = p.split("/");
    return { device: parts[parts.length - 2] || "", usb: p.includes("/usb") };
  } catch {
    return { device: "", usb: false };
  }
}

/** Every gpiochip with its uAPI info. Symlinks (Pi OS adds gpiochip4 → gpiochip0) are skipped. */
export function listChips(): Chip[] {
  let names: string[] = [];
  try {
    names = readdirSync(DEV).filter((n) => /^gpiochip\d+$/.test(n));
  } catch {
    return [];
  }
  const seen = new Set<string>();
  const out: Chip[] = [];
  for (const n of names.sort((a, b) => +a.slice(8) - +b.slice(8))) {
    const path = `${DEV}/${n}`;
    let real = path;
    try {
      real = realpathSync(path);
    } catch {}
    if (seen.has(real) || real !== path) {
      seen.add(real);
      continue;
    }
    seen.add(real);
    const sys = sysDevice(n);
    let fd: number | null = null;
    try {
      fd = openSync(path, "r");
      const b = new Uint8Array(CHIPINFO_SIZE);
      ioctl(fd, GPIO_GET_CHIPINFO_IOCTL, b, `${n} chip info`);
      out.push({ ...decodeChipInfo(b), path, ...sys });
    } catch (e: any) {
      // Typically EACCES: root-only /dev/gpiochip* on a stock Debian without a gpio group rule.
      const label = readSysLabel(n);
      out.push({ name: n, label, lines: 0, path, ...sys, error: e.code === "EACCES" ? `${path}: permission denied (BenchBob needs the gpio group)` : e.message });
    } finally {
      if (fd != null) closeSync(fd);
    }
  }
  return out;
}

/**
 * The legacy sysfs class (…/fe200000.gpio/gpio/gpiochip512/label, if CONFIG_GPIO_SYSFS is
 * on) still names a chip whose device node we can't open, so a board's chips still match.
 */
function readSysLabel(name: string): string {
  try {
    const dir = realpathSync(`/sys/bus/gpio/devices/${name}/..`) + "/gpio";
    for (const d of readdirSync(dir)) if (/^gpiochip\d+$/.test(d)) return readFileSync(`${dir}/${d}/label`, "utf8").trim();
  } catch {}
  return "";
}

/** Info for every line of one chip. */
export function readLines(chip: Pick<Chip, "path" | "lines">): LineInfo[] {
  const fd = openSync(chip.path, "r");
  try {
    const out: LineInfo[] = [];
    for (let i = 0; i < chip.lines; i++) {
      const b = encodeLineInfoReq(i);
      ioctl(fd, GPIO_V2_GET_LINEINFO_IOCTL, b, `${chip.path} line ${i}`);
      out.push(decodeLineInfo(b));
    }
    return out;
  } finally {
    closeSync(fd);
  }
}

// ---------- Watching (as-is line requests) ----------

export interface LineWatch {
  chip: string;
  offset: number;
  read(): 0 | 1;
  close(): void;
}

/**
 * Request one line "as is" so its level can be read. Fails with EBUSY if anything holds
 * it. While watched the line shows consumer "benchbob" to everyone else.
 */
export function watchLine(chipPath: string, offset: number, consumer = "benchbob"): LineWatch {
  const fd = openSync(chipPath, "r+");
  let lineFd = -1;
  try {
    const req = encodeLineRequest([offset], consumer, 0);
    ioctl(fd, GPIO_V2_GET_LINE_IOCTL, req, `${chipPath} line ${offset}`);
    lineFd = requestFd(req);
  } finally {
    closeSync(fd); // the line request outlives the chip descriptor
  }
  let open = true;
  return {
    chip: chipPath,
    offset,
    read() {
      if (!open) throw new Error("watch closed");
      const b = encodeValues(1n);
      ioctl(lineFd, GPIO_V2_LINE_GET_VALUES_IOCTL, b, `${chipPath} line ${offset} value`);
      return Number(decodeValueBits(b) & 1n) as 0 | 1;
    },
    close() {
      if (!open) return;
      open = false;
      try {
        closeSync(lineFd);
      } catch {}
    },
  };
}
