// ==================== termios2 over bun:ffi ====================
//
// Serial port control without a native addon. The `serialport` npm package crashes Bun
// (uv_default_loop is unsupported), so ports are plain fds opened O_NONBLOCK and configured
// with libc ioctl() on a Linux `struct termios2`, which carries an arbitrary baud rate
// (BOTHER) instead of the fixed Bnnn table. Reading is left to the caller
// (Bun.file(fd).stream() is event-driven and works on ttys; async fs.read returns 0 bytes).
//
// Pure encode/decode lives here too, so it can be unit-tested anywhere; libc is only
// opened on the first real ioctl.
//
// Constants were checked against the 6.18 kernel headers on desky (gcc, asm/termbits.h);
// they are the asm-generic values shared by arm64 and x86.

import { openSync, closeSync, writeSync, constants as fsc } from "node:fs";
import { dlopen, FFIType, read } from "bun:ffi";

// ---------- Constants ----------

export const TCGETS2 = 0x802c542a;
export const TCSETS2 = 0x402c542b;
const TERMIOS2_SIZE = 44; // iflag@0 oflag@4 cflag@8 lflag@12 line@16 cc[19]@17 ispeed@36 ospeed@40
const VTIME = 5, VMIN = 6, VSTART = 8, VSTOP = 9;

const CBAUD = 0x100f, BOTHER = 0x1000, CIBAUD = 0x100f0000;
const CSIZE = 0x30, CSTOPB = 0x40, CREAD = 0x80, PARENB = 0x100, PARODD = 0x200, CLOCAL = 0x800;
const CMSPAR = 0x40000000, CRTSCTS = 0x80000000;
const IXON = 0x400, IXOFF = 0x1000, INPCK = 0x10;

const TIOCMGET = 0x5415, TIOCMBIS = 0x5416, TIOCMBIC = 0x5417;
const TIOCSBRK = 0x5427, TIOCCBRK = 0x5428, TCFLSH = 0x540b, TIOCEXCL = 0x540c, TIOCNXCL = 0x540d;
export const TIOCM = { DTR: 0x002, RTS: 0x004, CTS: 0x020, CAR: 0x040, RNG: 0x080, DSR: 0x100 } as const;

/** The fixed-rate codes. Standard rates are written with these rather than BOTHER so tools
 * that read termios the old way (stty, pyserial, getty) see the right speed. */
const BAUD_CODES: Record<number, number> = {
  0: 0, 50: 1, 75: 2, 110: 3, 134: 4, 150: 5, 200: 6, 300: 7, 600: 8, 1200: 9, 1800: 10, 2400: 11, 4800: 12,
  9600: 13, 19200: 14, 38400: 15, 57600: 0x1001, 115200: 0x1002, 230400: 0x1003, 460800: 0x1004,
  500000: 0x1005, 576000: 0x1006, 921600: 0x1007, 1000000: 0x1008, 1152000: 0x1009, 1500000: 0x100a,
  2000000: 0x100b, 2500000: 0x100c, 3000000: 0x100d, 3500000: 0x100e, 4000000: 0x100f,
};
const CODE_BAUDS: Record<number, number> = Object.fromEntries(Object.entries(BAUD_CODES).map(([b, c]) => [c, +b]));

// ---------- Config <-> termios2 ----------

export type Parity = "none" | "even" | "odd" | "mark" | "space";
export type Flow = "none" | "rtscts" | "xonxoff";

export interface SerialConfig {
  baud: number;
  dataBits: 5 | 6 | 7 | 8;
  parity: Parity;
  stopBits: 1 | 2;
  flow: Flow;
}

export const DEFAULT_CONFIG: SerialConfig = { baud: 115200, dataBits: 8, parity: "none", stopBits: 1, flow: "none" };

/** Validate and fill a partial config from the API. Throws a plain Error with a readable message. */
export function normalizeConfig(c: Partial<SerialConfig> & Record<string, any>, base: SerialConfig = DEFAULT_CONFIG): SerialConfig {
  const out: SerialConfig = { ...base };
  if (c.baud != null) {
    const b = Number(c.baud);
    if (!Number.isInteger(b) || b < 50 || b > 20_000_000) throw new Error(`Invalid baud rate ${c.baud}`);
    out.baud = b;
  }
  if (c.dataBits != null) {
    const d = Number(c.dataBits);
    if (![5, 6, 7, 8].includes(d)) throw new Error(`Data bits must be 5-8, not ${c.dataBits}`);
    out.dataBits = d as SerialConfig["dataBits"];
  }
  if (c.parity != null) {
    const p = String(c.parity).toLowerCase();
    const m: Record<string, Parity> = { n: "none", none: "none", e: "even", even: "even", o: "odd", odd: "odd", m: "mark", mark: "mark", s: "space", space: "space" };
    if (!m[p]) throw new Error(`Parity must be none/even/odd/mark/space, not ${c.parity}`);
    out.parity = m[p];
  }
  if (c.stopBits != null) {
    const s = Number(c.stopBits);
    if (s !== 1 && s !== 2) throw new Error(`Stop bits must be 1 or 2, not ${c.stopBits}`);
    out.stopBits = s;
  }
  if (c.flow != null) {
    const f = String(c.flow).toLowerCase().replace(/[^a-z]/g, "");
    const m: Record<string, Flow> = { none: "none", off: "none", rtscts: "rtscts", hw: "rtscts", hardware: "rtscts", xonxoff: "xonxoff", sw: "xonxoff", software: "xonxoff" };
    if (!m[f]) throw new Error(`Flow control must be none/rtscts/xonxoff, not ${c.flow}`);
    out.flow = m[f];
  }
  return out;
}

/** "115200 8N1", with flow control appended when on. */
export function configLabel(c: SerialConfig): string {
  return `${c.baud} ${c.dataBits}${c.parity[0].toUpperCase()}${c.stopBits}${c.flow === "none" ? "" : " " + (c.flow === "rtscts" ? "RTS/CTS" : "XON/XOFF")}`;
}

/** Build a raw-mode termios2 for `cfg`. `base` (from TCGETS2) keeps c_line and HUPCL as the driver left them. */
export function encodeTermios(cfg: SerialConfig, base?: Uint8Array): Uint8Array {
  const buf = new Uint8Array(TERMIOS2_SIZE);
  if (base) buf.set(base.subarray(0, TERMIOS2_SIZE));
  const v = new DataView(buf.buffer);
  const old = v.getUint32(8, true);

  // Raw: no line editing, no CR/LF translation, no signals. Bytes in, bytes out.
  let iflag = 0;
  if (cfg.flow === "xonxoff") iflag |= IXON | IXOFF;
  // With INPCK (and neither IGNPAR nor PARMRK) a byte with a parity error reads as NUL,
  // which is what auto-baud and a human both expect to see on a mismatched link.
  if (cfg.parity !== "none") iflag |= INPCK;

  let cflag = (old & 0x400 /* HUPCL */) | CREAD | CLOCAL;
  cflag |= ({ 5: 0x00, 6: 0x10, 7: 0x20, 8: 0x30 } as const)[cfg.dataBits];
  if (cfg.stopBits === 2) cflag |= CSTOPB;
  if (cfg.parity !== "none") cflag |= PARENB;
  if (cfg.parity === "odd" || cfg.parity === "mark") cflag |= PARODD;
  if (cfg.parity === "mark" || cfg.parity === "space") cflag |= CMSPAR;
  if (cfg.flow === "rtscts") cflag |= CRTSCTS;
  const code = BAUD_CODES[cfg.baud];
  cflag |= code !== undefined ? code : BOTHER; // CIBAUD left 0: input speed follows output

  v.setUint32(0, iflag >>> 0, true);
  v.setUint32(4, 0, true);
  v.setUint32(8, cflag >>> 0, true);
  v.setUint32(12, 0, true);
  buf[17 + VMIN] = 1;
  buf[17 + VTIME] = 0;
  buf[17 + VSTART] = 0x11;
  buf[17 + VSTOP] = 0x13;
  v.setUint32(36, cfg.baud, true);
  v.setUint32(40, cfg.baud, true);
  return buf;
}

export function decodeTermios(buf: Uint8Array): SerialConfig {
  const v = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const iflag = v.getUint32(0, true);
  const cflag = v.getUint32(8, true);
  const code = cflag & CBAUD;
  const baud = code === BOTHER ? v.getUint32(40, true) : CODE_BAUDS[code] ?? v.getUint32(40, true);
  const dataBits = (({ 0x00: 5, 0x10: 6, 0x20: 7, 0x30: 8 }) as Record<number, 5 | 6 | 7 | 8>)[cflag & CSIZE];
  let parity: Parity = "none";
  if (cflag & PARENB) {
    const odd = !!(cflag & PARODD);
    parity = cflag & CMSPAR ? (odd ? "mark" : "space") : odd ? "odd" : "even";
  }
  const flow: Flow = (cflag & CRTSCTS) >>> 0 ? "rtscts" : iflag & IXON ? "xonxoff" : "none";
  return { baud, dataBits, parity, stopBits: cflag & CSTOPB ? 2 : 1, flow };
}

// ---------- libc ----------

type Libc = {
  ioctlPtr(fd: number, req: number, arg: Uint8Array | Int32Array): number;
  ioctlInt(fd: number, req: number, arg: number): number;
  errno(): number;
};
let libc: Libc | null = null;

function lib(): Libc {
  if (libc) return libc;
  // Opened lazily so the pure half of this module works anywhere (tests on Windows).
  // ioctl is variadic. On aarch64/x86-64 Linux, variadic integer and pointer arguments
  // travel in the same registers as fixed ones, so two fixed-signature views of the same
  // symbol (pointer arg, integer arg) are both correct.
  const p = dlopen("libc.so.6", {
    ioctl: { args: [FFIType.i32, FFIType.u64, FFIType.ptr], returns: FFIType.i32 },
    __errno_location: { args: [], returns: FFIType.ptr },
  });
  const i = dlopen("libc.so.6", { ioctl: { args: [FFIType.i32, FFIType.u64, FFIType.i64], returns: FFIType.i32 } });
  libc = {
    ioctlPtr: (fd, req, arg) => p.symbols.ioctl(fd, req, arg) as number,
    ioctlInt: (fd, req, arg) => i.symbols.ioctl(fd, req, arg) as number,
    errno: () => read.i32(p.symbols.__errno_location()!, 0),
  };
  return libc;
}

const ERRNO: Record<number, string> = { 1: "EPERM", 5: "EIO", 9: "EBADF", 11: "EAGAIN", 13: "EACCES", 16: "EBUSY", 19: "ENODEV", 22: "EINVAL", 25: "ENOTTY" };

export class TtyError extends Error {
  constructor(public op: string, public errno: number) {
    super(`${op} failed: ${ERRNO[errno] || "errno " + errno}`);
  }
  get code() {
    return ERRNO[this.errno] || `E${this.errno}`;
  }
}

function check(op: string, rc: number) {
  if (rc < 0) throw new TtyError(op, lib().errno());
}

// ---------- Port operations ----------

/** Open a tty for the event-driven stream reader. `readOnly` for ports we must not drive (a console). */
export function openPort(path: string, readOnly = false): number {
  return openSync(path, (readOnly ? fsc.O_RDONLY : fsc.O_RDWR) | fsc.O_NOCTTY | fsc.O_NONBLOCK);
}

export function closePort(fd: number) {
  try {
    closeSync(fd);
  } catch {}
}

export function getTermios(fd: number): Uint8Array {
  const buf = new Uint8Array(TERMIOS2_SIZE);
  check("TCGETS2", lib().ioctlPtr(fd, TCGETS2, buf));
  return buf;
}

/** Apply `cfg` in raw mode; returns what the driver actually accepted (read back). */
export function configure(fd: number, cfg: SerialConfig): SerialConfig {
  const base = getTermios(fd);
  check("TCSETS2", lib().ioctlPtr(fd, TCSETS2, encodeTermios(cfg, base)));
  return decodeTermios(getTermios(fd));
}

/** Refuse further opens of this tty by other (non-root) processes while we own it. */
export function setExclusive(fd: number) {
  check("TIOCEXCL", lib().ioctlInt(fd, TIOCEXCL, 0));
}

/** Undo setExclusive. The flag lives on the tty, not the fd, and only resets when the last
 * fd closes; if anything else holds the tty (a pty's other end, a stray `cat`), it would
 * outlive our close and lock out the next opener, esptool or ourselves included. */
export function clearExclusive(fd: number) {
  check("TIOCNXCL", lib().ioctlInt(fd, TIOCNXCL, 0));
}

export interface ModemLines {
  dtr: boolean;
  rts: boolean;
  cts: boolean;
  dsr: boolean;
  dcd: boolean;
  ri: boolean;
}

/** TIOCMGET. Throws TtyError (ENOTTY/EINVAL) on ttys without modem lines, e.g. ptys. */
export function getModem(fd: number): ModemLines {
  const v = new Int32Array(1);
  check("TIOCMGET", lib().ioctlPtr(fd, TIOCMGET, v));
  const m = v[0];
  return { dtr: !!(m & TIOCM.DTR), rts: !!(m & TIOCM.RTS), cts: !!(m & TIOCM.CTS), dsr: !!(m & TIOCM.DSR), dcd: !!(m & TIOCM.CAR), ri: !!(m & TIOCM.RNG) };
}

/** Assert (true) or drop (false) DTR and/or RTS; undefined leaves a line alone. */
export function setModem(fd: number, lines: { dtr?: boolean; rts?: boolean }) {
  let on = 0, off = 0;
  if (lines.dtr !== undefined) lines.dtr ? (on |= TIOCM.DTR) : (off |= TIOCM.DTR);
  if (lines.rts !== undefined) lines.rts ? (on |= TIOCM.RTS) : (off |= TIOCM.RTS);
  if (on) check("TIOCMBIS", lib().ioctlPtr(fd, TIOCMBIS, new Int32Array([on])));
  if (off) check("TIOCMBIC", lib().ioctlPtr(fd, TIOCMBIC, new Int32Array([off])));
}

export function setBreak(fd: number, on: boolean) {
  check(on ? "TIOCSBRK" : "TIOCCBRK", lib().ioctlInt(fd, on ? TIOCSBRK : TIOCCBRK, 0));
}

/** Discard queued data: "rx" (received, unread), "tx" (written, unsent) or "both". */
export function flush(fd: number, which: "rx" | "tx" | "both") {
  check("TCFLSH", lib().ioctlInt(fd, TCFLSH, which === "rx" ? 0 : which === "tx" ? 1 : 2));
}

/** Non-blocking write. Returns bytes written (0 when the kernel buffer is full). Throws on real errors. */
export function writeSome(fd: number, data: Uint8Array): number {
  try {
    return writeSync(fd, data);
  } catch (e: any) {
    if (e?.code === "EAGAIN" || e?.code === "EWOULDBLOCK") return 0;
    throw e;
  }
}
