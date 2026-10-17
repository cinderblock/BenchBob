// ==================== I²C via /dev/i2c-N ioctls ====================
//
// The i2c driver talks to buses through the kernel's i2c-dev interface directly (bun:ffi →
// libc ioctl) rather than shelling out to i2c-tools: no output parsing, a precise errno for
// every NACK/timeout, and fast enough (~0.2 ms per probe) to poll sensors. Operations are
// synchronous ioctls, so a single transfer is atomic with respect to the event loop;
// multi-step sequences (write a command, sleep, read) take the bus lock in the driver.
//
// Everything above this file speaks the `I2cIo` interface, so identification and decoding
// can be unit-tested against `FakeBus` (a byte-level chip simulator) with no hardware.

import { closeSync, openSync } from "node:fs";

// <linux/i2c-dev.h>, <linux/i2c.h>
const I2C_SLAVE = 0x0703;
const I2C_SLAVE_FORCE = 0x0706;
const I2C_FUNCS = 0x0705;
const I2C_RDWR = 0x0707;
const I2C_SMBUS = 0x0720;
const I2C_M_RD = 0x0001;

export const F = {
  I2C: 0x1,
  TEN_BIT: 0x2,
  PEC: 0x8,
  QUICK: 0x10000,
  READ_BYTE: 0x20000,
  WRITE_BYTE: 0x40000,
  READ_BYTE_DATA: 0x80000,
  WRITE_BYTE_DATA: 0x100000,
  READ_WORD_DATA: 0x200000,
  WRITE_WORD_DATA: 0x400000,
  READ_BLOCK_DATA: 0x1000000,
  READ_I2C_BLOCK: 0x4000000,
  WRITE_I2C_BLOCK: 0x8000000,
} as const;

/** Human summary of an I2C_FUNCS mask. */
export function funcNames(f: number): string[] {
  const out: string[] = [];
  if (f & F.I2C) out.push("I2C");
  if (f & F.QUICK) out.push("quick");
  if (f & F.READ_BYTE_DATA) out.push("byte");
  if (f & F.READ_WORD_DATA) out.push("word");
  if (f & F.READ_I2C_BLOCK) out.push("i2c-block");
  if (f & F.READ_BLOCK_DATA) out.push("smbus-block");
  if (f & F.PEC) out.push("PEC");
  if (f & F.TEN_BIT) out.push("10-bit");
  return out;
}

const SMBUS_READ = 1;
const SMBUS_WRITE = 0;
const SZ_QUICK = 0;
const SZ_BYTE = 1;
const SZ_BYTE_DATA = 2;
const SZ_WORD_DATA = 3;
const SZ_I2C_BLOCK_DATA = 8;

const ERRNO: Record<number, string> = {
  1: "EPERM",
  5: "EIO",
  6: "ENXIO",
  11: "EAGAIN",
  13: "EACCES",
  16: "EBUSY",
  19: "ENODEV",
  22: "EINVAL",
  71: "EPROTO",
  95: "EOPNOTSUPP",
  110: "ETIMEDOUT",
  121: "EREMOTEIO",
};

const ERRNO_TEXT: Record<number, string> = {
  6: "no device answered (NACK)",
  121: "no ACK from the device",
  110: "bus timeout (clock held low or device stretching too long)",
  11: "arbitration lost / bus busy",
  16: "address is claimed by a kernel driver",
  95: "the adapter doesn't support this transfer type",
  5: "I/O error",
  71: "protocol error",
  22: "invalid argument",
  13: "permission denied (is the user in the i2c group?)",
};

export class I2cError extends Error {
  constructor(public errno: number, public op: string) {
    super(`${op}: ${ERRNO_TEXT[errno] || "error"} (${ERRNO[errno] || "errno " + errno})`);
  }
  /** True for "nobody is there", as opposed to a bus or adapter fault. */
  get nack() {
    return this.errno === 121 || this.errno === 6;
  }
}

export type Msg = { w: number[] } | { r: number };

/** One bus. All methods are synchronous; errors throw I2cError. */
export interface I2cIo {
  readonly funcs: number;
  /** Throws EBUSY (unless force) when a kernel driver has the address. */
  quick(addr: number, read?: boolean, force?: boolean): void;
  receive(addr: number, force?: boolean): number;
  send(addr: number, v: number, force?: boolean): void;
  readByte(addr: number, reg: number, force?: boolean): number;
  writeByte(addr: number, reg: number, v: number, force?: boolean): void;
  /** SMBus word: low byte first on the wire. */
  readWord(addr: number, reg: number, force?: boolean): number;
  writeWord(addr: number, reg: number, v: number, force?: boolean): void;
  /** SMBus "I2C block": write reg, repeated start, read len (≤32) bytes. */
  readBlock(addr: number, reg: number, len: number, force?: boolean): number[];
  writeBlock(addr: number, reg: number, data: number[], force?: boolean): void;
  /** Combined plain-I²C transaction (repeated starts). Needs F.I2C. Returns the read buffers. */
  transfer(addr: number, msgs: Msg[], force?: boolean): number[][];
  close(): void;
}

// ---------- Linux implementation ----------

type Libc = { ioctl(fd: number, req: number, arg: number | bigint): number; errno(): number; ptr(b: ArrayBufferView): number };
let libc: Libc | null = null;

function getLibc(): Libc {
  if (libc) return libc;
  // Imported lazily so the pure-logic tests (and Windows dev boxes) never touch FFI.
  const ffi = require("bun:ffi");
  const lib = ffi.dlopen("libc.so.6", {
    ioctl: { args: [ffi.FFIType.i32, ffi.FFIType.u64, ffi.FFIType.u64], returns: ffi.FFIType.i32 },
    __errno_location: { args: [], returns: ffi.FFIType.ptr },
  });
  libc = {
    ioctl: (fd, req, arg) => lib.symbols.ioctl(fd, req, arg),
    errno: () => ffi.read.i32(lib.symbols.__errno_location(), 0),
    ptr: (b) => ffi.ptr(b),
  };
  return libc;
}

export class LinuxI2cBus implements I2cIo {
  private fd: number;
  private cur = -1;
  private curForce = false;
  readonly funcs: number;

  constructor(public readonly bus: number) {
    this.fd = openSync(`/dev/i2c-${bus}`, "r+");
    const L = getLibc();
    const f = new BigUint64Array(1);
    if (L.ioctl(this.fd, I2C_FUNCS, L.ptr(f)) < 0) throw new I2cError(L.errno(), "I2C_FUNCS");
    this.funcs = Number(f[0] & 0xffffffffn);
  }

  close() {
    try {
      closeSync(this.fd);
    } catch {}
    this.fd = -1;
  }

  private target(addr: number, force = false) {
    if (this.cur === addr && this.curForce === force) return;
    const L = getLibc();
    if (L.ioctl(this.fd, force ? I2C_SLAVE_FORCE : I2C_SLAVE, addr) < 0) {
      this.cur = -1;
      throw new I2cError(L.errno(), `set address ${hex(addr)}`);
    }
    this.cur = addr;
    this.curForce = force;
  }

  /** struct i2c_smbus_ioctl_data { u8 read_write; u8 command; u32 size; union i2c_smbus_data *data; } */
  private smbus(rw: number, cmd: number, size: number, data: Uint8Array | null, op: string) {
    const L = getLibc();
    const a = new Uint8Array(16);
    const dv = new DataView(a.buffer);
    a[0] = rw;
    a[1] = cmd & 0xff;
    dv.setUint32(4, size, true);
    dv.setBigUint64(8, data ? BigInt(L.ptr(data)) : 0n, true);
    if (L.ioctl(this.fd, I2C_SMBUS, L.ptr(a)) < 0) throw new I2cError(L.errno(), op);
  }

  quick(addr: number, read = false, force = false) {
    this.target(addr, force);
    this.smbus(read ? SMBUS_READ : SMBUS_WRITE, 0, SZ_QUICK, null, `quick ${hex(addr)}`);
  }
  receive(addr: number, force = false) {
    this.target(addr, force);
    const d = new Uint8Array(34);
    this.smbus(SMBUS_READ, 0, SZ_BYTE, d, `receive ${hex(addr)}`);
    return d[0];
  }
  send(addr: number, v: number, force = false) {
    this.target(addr, force);
    this.smbus(SMBUS_WRITE, v, SZ_BYTE, null, `send ${hex(addr)}`);
  }
  readByte(addr: number, reg: number, force = false) {
    this.target(addr, force);
    const d = new Uint8Array(34);
    this.smbus(SMBUS_READ, reg, SZ_BYTE_DATA, d, `read ${hex(addr)}[${hex(reg)}]`);
    return d[0];
  }
  writeByte(addr: number, reg: number, v: number, force = false) {
    this.target(addr, force);
    const d = new Uint8Array(34);
    d[0] = v & 0xff;
    this.smbus(SMBUS_WRITE, reg, SZ_BYTE_DATA, d, `write ${hex(addr)}[${hex(reg)}]`);
  }
  readWord(addr: number, reg: number, force = false) {
    this.target(addr, force);
    const d = new Uint8Array(34);
    this.smbus(SMBUS_READ, reg, SZ_WORD_DATA, d, `read word ${hex(addr)}[${hex(reg)}]`);
    return d[0] | (d[1] << 8);
  }
  writeWord(addr: number, reg: number, v: number, force = false) {
    this.target(addr, force);
    const d = new Uint8Array(34);
    d[0] = v & 0xff;
    d[1] = (v >> 8) & 0xff;
    this.smbus(SMBUS_WRITE, reg, SZ_WORD_DATA, d, `write word ${hex(addr)}[${hex(reg)}]`);
  }
  readBlock(addr: number, reg: number, len: number, force = false) {
    if (!(this.funcs & F.READ_I2C_BLOCK)) {
      const out: number[] = [];
      for (let i = 0; i < len; i++) out.push(this.readByte(addr, reg + i, force));
      return out;
    }
    this.target(addr, force);
    const d = new Uint8Array(34);
    d[0] = Math.min(32, len);
    this.smbus(SMBUS_READ, reg, SZ_I2C_BLOCK_DATA, d, `block read ${hex(addr)}[${hex(reg)}]`);
    return [...d.subarray(1, 1 + d[0])];
  }
  writeBlock(addr: number, reg: number, data: number[], force = false) {
    this.target(addr, force);
    const d = new Uint8Array(34);
    d[0] = Math.min(32, data.length);
    d.set(data.slice(0, 32), 1);
    this.smbus(SMBUS_WRITE, reg, SZ_I2C_BLOCK_DATA, d, `block write ${hex(addr)}[${hex(reg)}]`);
  }

  /** struct i2c_msg { u16 addr; u16 flags; u16 len; u8 *buf; } (16 bytes on 64-bit), struct i2c_rdwr_ioctl_data { i2c_msg *msgs; u32 nmsgs; } */
  transfer(addr: number, msgs: Msg[], force = false) {
    if (!(this.funcs & F.I2C)) throw new I2cError(95, `transfer ${hex(addr)}`);
    if (!msgs.length || msgs.length > 42) throw new I2cError(22, `transfer ${hex(addr)}`);
    // I2C_RDWR addresses each message itself, but a bound kernel driver must still win:
    // check it the same way i2ctransfer does (it refuses without -f).
    this.target(addr, force);
    const L = getLibc();
    const bufs = msgs.map((m) => ("w" in m ? Uint8Array.from(m.w) : new Uint8Array(m.r)));
    const arr = new Uint8Array(16 * msgs.length);
    const dv = new DataView(arr.buffer);
    msgs.forEach((m, i) => {
      dv.setUint16(i * 16, addr, true);
      dv.setUint16(i * 16 + 2, "r" in m ? I2C_M_RD : 0, true);
      dv.setUint16(i * 16 + 4, bufs[i].length, true);
      dv.setBigUint64(i * 16 + 8, bufs[i].length ? BigInt(L.ptr(bufs[i])) : 0n, true);
    });
    const io = new Uint8Array(16);
    const iv = new DataView(io.buffer);
    iv.setBigUint64(0, BigInt(L.ptr(arr)), true);
    iv.setUint32(8, msgs.length, true);
    if (L.ioctl(this.fd, I2C_RDWR, L.ptr(io)) < 0) throw new I2cError(L.errno(), `transfer ${hex(addr)}`);
    return msgs.flatMap((m, i) => ("r" in m ? [[...bufs[i]]] : []));
  }
}

// ---------- Fake bus for tests ----------

/** A chip on the fake bus, modelled at the byte level the way real silicon sees it. */
export interface FakeChip {
  /** Bytes written in one transaction (after the address byte). Throw I2cError(121) to NACK. */
  write?(bytes: number[]): void;
  /** Bytes read in one transaction. */
  read?(n: number): number[];
}

/** Register-file chip: first written byte sets the pointer, further bytes are stored; reads auto-increment. */
export class RegChip implements FakeChip {
  ptr = 0;
  regs: number[];
  writes: number[][] = [];
  constructor(init: Record<number, number> | number[] = {}, public opts: { size?: number; autoInc?: boolean; readOnly?: boolean } = {}) {
    this.regs = new Array(opts.size ?? 256).fill(0);
    if (Array.isArray(init)) init.forEach((v, i) => (this.regs[i] = v));
    else for (const [k, v] of Object.entries(init)) this.regs[Number(k)] = v;
  }
  /** Set big-endian 16-bit registers (for word-register chips modelled byte-wise, use setBytes). */
  setBytes(reg: number, bytes: number[]) {
    bytes.forEach((b, i) => (this.regs[(reg + i) % this.regs.length] = b));
    return this;
  }
  write(bytes: number[]) {
    this.writes.push(bytes);
    if (!bytes.length) return;
    this.ptr = bytes[0] % this.regs.length;
    if (this.opts.readOnly) return;
    for (const b of bytes.slice(1)) {
      this.regs[this.ptr] = b;
      if (this.opts.autoInc !== false) this.ptr = (this.ptr + 1) % this.regs.length;
    }
  }
  read(n: number) {
    const out: number[] = [];
    for (let i = 0; i < n; i++) {
      out.push(this.regs[this.ptr]);
      if (this.opts.autoInc !== false) this.ptr = (this.ptr + 1) % this.regs.length;
    }
    return out;
  }
}

/** Word-register chip (TI INA/TMP/ADS style): 1-byte pointer, 16-bit big-endian registers. */
export class WordChip implements FakeChip {
  ptr = 0;
  writes: number[][] = [];
  constructor(public regs: Record<number, number>, public opts: { nackUnknown?: boolean } = {}) {}
  write(bytes: number[]) {
    this.writes.push(bytes);
    if (!bytes.length) return;
    if (this.opts.nackUnknown && !(bytes[0] in this.regs)) throw new I2cError(121, "fake");
    this.ptr = bytes[0];
    if (bytes.length >= 3) this.regs[this.ptr] = (bytes[1] << 8) | bytes[2];
  }
  read(n: number) {
    const v = this.regs[this.ptr] ?? 0xffff;
    const out: number[] = [];
    for (let i = 0; i < n; i++) out.push(i % 2 === 0 ? v >> 8 : v & 0xff);
    return out;
  }
}

export class FakeBus implements I2cIo {
  chips = new Map<number, FakeChip>();
  claimed = new Set<number>();
  log: string[] = [];
  constructor(public funcs: number = F.I2C | F.QUICK | F.READ_BYTE | F.WRITE_BYTE | F.READ_BYTE_DATA | F.WRITE_BYTE_DATA | F.READ_WORD_DATA | F.WRITE_WORD_DATA | F.READ_I2C_BLOCK | F.WRITE_I2C_BLOCK) {}
  add(addr: number, chip: FakeChip) {
    this.chips.set(addr, chip);
    return this;
  }
  private chip(addr: number, op: string, force = false): FakeChip {
    if (this.claimed.has(addr) && !force) throw new I2cError(16, op);
    const c = this.chips.get(addr);
    if (!c) throw new I2cError(121, op);
    return c;
  }
  private w(addr: number, bytes: number[], op: string, force = false) {
    this.log.push(`${hex(addr)} W ${bytes.map((b) => hex(b)).join(" ")}`);
    const c = this.chip(addr, op, force);
    if (!c.write) throw new I2cError(121, op);
    c.write(bytes);
  }
  private r(addr: number, n: number, op: string, force = false) {
    this.log.push(`${hex(addr)} R ${n}`);
    const c = this.chip(addr, op, force);
    if (!c.read) throw new I2cError(121, op);
    return c.read(n).map((b) => b & 0xff);
  }
  private need(f: number, op: string) {
    if (!(this.funcs & f)) throw new I2cError(95, op);
  }
  quick(addr: number, read = false, force = false) {
    this.need(F.QUICK, "quick");
    this.log.push(`${hex(addr)} Q${read ? "r" : "w"}`);
    this.chip(addr, "quick", force);
  }
  receive(addr: number, force = false) {
    return this.r(addr, 1, "receive", force)[0];
  }
  send(addr: number, v: number, force = false) {
    this.w(addr, [v], "send", force);
  }
  readByte(addr: number, reg: number, force = false) {
    this.w(addr, [reg], "read", force);
    return this.r(addr, 1, "read", force)[0];
  }
  writeByte(addr: number, reg: number, v: number, force = false) {
    this.w(addr, [reg, v], "write", force);
  }
  readWord(addr: number, reg: number, force = false) {
    this.w(addr, [reg], "read word", force);
    const [lo, hi] = this.r(addr, 2, "read word", force);
    return lo | (hi << 8);
  }
  writeWord(addr: number, reg: number, v: number, force = false) {
    this.w(addr, [reg, v & 0xff, (v >> 8) & 0xff], "write word", force);
  }
  readBlock(addr: number, reg: number, len: number, force = false) {
    this.need(F.READ_I2C_BLOCK, "block read");
    this.w(addr, [reg], "block read", force);
    return this.r(addr, Math.min(32, len), "block read", force);
  }
  writeBlock(addr: number, reg: number, data: number[], force = false) {
    this.w(addr, [reg, ...data.slice(0, 32)], "block write", force);
  }
  transfer(addr: number, msgs: Msg[], force = false) {
    this.need(F.I2C, "transfer");
    const out: number[][] = [];
    for (const m of msgs) {
      if ("w" in m) this.w(addr, m.w, "transfer", force);
      else out.push(this.r(addr, m.r, "transfer", force));
    }
    return out;
  }
  close() {}
}

export function hex(n: number, w = 2) {
  return "0x" + n.toString(16).toUpperCase().padStart(w, "0");
}
