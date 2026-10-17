// ==================== SocketCAN raw sockets + CAN frame helpers ====================
//
// Two halves:
//  - CanSocket: an AF_CAN raw socket opened through bun:ffi (libc socket/setsockopt/bind/
//    fcntl/recvmsg), drained with non-blocking recvmsg() on a short timer (see poll() for
//    why not Bun.file(fd).stream()) and written with writeSync. No native addon, no
//    candump child to parse.
//  - Pure helpers (no FFI, testable on any OS): frame encode/decode, the candump log and
//    cansend formats, error-frame decoding, bus-load bit counting, the socketcand ASCII
//    protocol, filters and the auto-bitrate decision.
//
// Constants and struct layouts were checked against the kernel headers on desky with gcc
// (see plans/peripherals-can.md): can_frame is 16 bytes (id@0 len@4 data@8), canfd_frame is
// 72 (flags@5), sockaddr_can is 24 (family@0 ifindex@4).

import { closeSync, writeSync } from "node:fs";

export const AF_CAN = 29;
export const SOCK_RAW = 3;
export const CAN_RAW = 1;
export const SOL_CAN_RAW = 101;
export const CAN_RAW_FILTER = 1;
export const CAN_RAW_ERR_FILTER = 2;
export const CAN_RAW_LOOPBACK = 3;
export const CAN_RAW_RECV_OWN_MSGS = 4;
export const CAN_RAW_FD_FRAMES = 5;

export const CAN_EFF_FLAG = 0x80000000;
export const CAN_RTR_FLAG = 0x40000000;
export const CAN_ERR_FLAG = 0x20000000;
export const CAN_SFF_MASK = 0x7ff;
export const CAN_EFF_MASK = 0x1fffffff;
export const CAN_ERR_MASK = 0x1fffffff;
export const CANFD_BRS = 0x01;
export const CANFD_ESI = 0x02;
export const CANFD_FDF = 0x04;

export const CAN_MTU = 16;
export const CANFD_MTU = 72;

const SOL_SOCKET = 1;
const SO_RCVBUF = 8;
const SO_TIMESTAMP = 29; // = SCM_TIMESTAMP, struct timeval (2 x int64 on 64-bit)
const SO_RXQ_OVFL = 40;
const MSG_DONTROUTE = 0x4; // set on frames looped back from another local socket
const MSG_CONFIRM = 0x800; // set on our own frames echoed back (CAN_RAW_RECV_OWN_MSGS)
const MSG_DONTWAIT = 0x40;
const EAGAIN = 11;
const EINTR = 4;
const CTRL_LEN = 64; // room for SCM_TIMESTAMP (32) + SO_RXQ_OVFL (24)
const F_GETFL = 3;
const F_SETFL = 4;
const O_NONBLOCK = 0o4000;

/** A decoded frame. `ts` is Unix epoch milliseconds (fractional), taken when the read landed. */
export interface CanFrame {
  id: number;
  ext: boolean;
  rtr: boolean;
  err: boolean; // error frame: id holds the CAN_ERR_* class bits
  fd: boolean;
  brs: boolean;
  esi: boolean;
  len: number;
  data: Uint8Array;
  ts: number;
  own?: boolean; // sent by this socket, echoed once the controller confirmed it (MSG_CONFIRM)
  local?: boolean; // sent by another socket on this host (MSG_DONTROUTE)
}

/** What callers hand to send(): any missing flag is false. `data` may be hex or bytes. */
export interface FrameSpec {
  id: number;
  ext?: boolean;
  rtr?: boolean;
  fd?: boolean;
  brs?: boolean;
  esi?: boolean;
  len?: number; // RTR request length; defaults to data length
  data?: Uint8Array | string | number[];
}

// ---------- Bytes and hex ----------

const HEX = Array.from({ length: 256 }, (_, i) => i.toString(16).toUpperCase().padStart(2, "0"));

export function toHex(b: Uint8Array, sep = ""): string {
  let s = "";
  for (let i = 0; i < b.length; i++) s += (i && sep ? sep : "") + HEX[b[i]];
  return s;
}

/** "DE AD be:ef", "deadbeef", "0xDE,0xAD" → bytes. Throws on odd digits or junk. */
export function parseHex(s: string): Uint8Array {
  const t = s.replace(/0x/gi, "").replace(/[\s,:.\-_]/g, "");
  if (!/^[0-9a-fA-F]*$/.test(t)) throw new Error(`Not hex: "${s}"`);
  if (t.length % 2) throw new Error(`Odd number of hex digits: "${s}"`);
  const out = new Uint8Array(t.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(t.substr(i * 2, 2), 16);
  return out;
}

function toBytes(d: FrameSpec["data"]): Uint8Array {
  if (d == null) return new Uint8Array(0);
  if (typeof d === "string") return parseHex(d);
  return Uint8Array.from(d);
}

/** CAN FD payload sizes. A length in between is padded up to the next one. */
export const FD_LENS = [0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32, 48, 64];
export function fdLen(n: number): number {
  for (const l of FD_LENS) if (l >= n) return l;
  return -1;
}

// ---------- Frame encode / decode ----------

/** Validate a spec and turn it into a frame (ts = now). Throws a readable message on bad input. */
export function makeFrame(spec: FrameSpec): CanFrame {
  const id = Number(spec.id);
  if (!Number.isInteger(id) || id < 0) throw new Error("id must be a non-negative integer");
  const ext = !!spec.ext || id > CAN_SFF_MASK;
  if (id > (ext ? CAN_EFF_MASK : CAN_SFF_MASK)) throw new Error(`id 0x${id.toString(16)} is out of range`);
  const fd = !!spec.fd;
  const rtr = !!spec.rtr;
  if (fd && rtr) throw new Error("CAN FD has no remote frames");
  let data = toBytes(spec.data);
  let len = data.length;
  if (rtr) {
    len = spec.len ?? data.length;
    if (len < 0 || len > 8) throw new Error("RTR length must be 0–8");
    data = new Uint8Array(0);
  } else if (fd) {
    const l = fdLen(len);
    if (l < 0) throw new Error("CAN FD payload is at most 64 bytes");
    if (l !== len) {
      const p = new Uint8Array(l);
      p.set(data);
      data = p;
      len = l;
    }
  } else if (len > 8) throw new Error("Classic CAN payload is at most 8 bytes (use fd for more)");
  return { id, ext, rtr, err: false, fd, brs: fd && !!spec.brs, esi: fd && !!spec.esi, len, data, ts: Date.now() };
}

export function encodeFrame(f: CanFrame): Uint8Array {
  const buf = new Uint8Array(f.fd ? CANFD_MTU : CAN_MTU);
  const dv = new DataView(buf.buffer);
  let raw = f.id & (f.ext ? CAN_EFF_MASK : CAN_SFF_MASK);
  if (f.ext) raw |= CAN_EFF_FLAG;
  if (f.rtr) raw |= CAN_RTR_FLAG;
  dv.setUint32(0, raw >>> 0, true);
  buf[4] = f.len;
  if (f.fd) buf[5] = (f.brs ? CANFD_BRS : 0) | (f.esi ? CANFD_ESI : 0) | CANFD_FDF;
  if (!f.rtr) buf.set(f.data.subarray(0, f.len), 8);
  return buf;
}

function decodeAt(b: Uint8Array, off: number, size: number, ts: number): CanFrame {
  const dv = new DataView(b.buffer, b.byteOffset + off, size);
  const raw = dv.getUint32(0, true);
  const fd = size === CANFD_MTU;
  const err = !!(raw & CAN_ERR_FLAG);
  const ext = !err && !!(raw & CAN_EFF_FLAG);
  const rtr = !err && !fd && !!(raw & CAN_RTR_FLAG);
  const len = Math.min(b[off + 4], fd ? 64 : 8);
  const flags = fd ? b[off + 5] : 0;
  return {
    id: err ? raw & CAN_ERR_MASK : raw & (ext ? CAN_EFF_MASK : CAN_SFF_MASK),
    ext,
    rtr,
    err,
    fd,
    brs: !!(flags & CANFD_BRS),
    esi: !!(flags & CANFD_ESI),
    len,
    data: rtr ? new Uint8Array(0) : b.slice(off + 8, off + 8 + len),
    ts,
  };
}

/**
 * Split a read chunk into frames. Each raw-socket read() returns one datagram, but a
 * byte-stream reader such as Bun.file(fd).stream() keeps reading until EAGAIN and hands over
 * the concatenation (seen on desky: 32 = 2×16 and 88 = 16+72 byte chunks), so this copes. A canfd_frame is recognised by the
 * CANFD_FDF flag the kernel sets on every received FD frame (or a length over 8); a
 * can_frame's byte 5 is padding and always 0.
 */
export function splitFrames(b: Uint8Array, ts: number, out: CanFrame[] = []): CanFrame[] {
  let off = 0;
  while (b.length - off >= CAN_MTU) {
    const isFd = b.length - off >= CANFD_MTU && (b[off + 5] & CANFD_FDF || b[off + 4] > 8);
    const size = isFd ? CANFD_MTU : CAN_MTU;
    out.push(decodeAt(b, off, size, ts));
    off += size;
  }
  return out;
}

export function frameKey(f: { id: number; ext: boolean }): string {
  return (f.ext ? "x" : "s") + f.id.toString(16);
}

export function fmtId(id: number, ext: boolean): string {
  return ext ? id.toString(16).toUpperCase().padStart(8, "0") : id.toString(16).toUpperCase().padStart(3, "0");
}

// ---------- candump -L log lines and cansend syntax ----------

/** The part after the interface in `candump -L`: 123#DEADBEEF, 1234ABCD#R, 123##5DEADBEEF, 20000080#… */
export function candumpBody(f: CanFrame): string {
  if (f.err) return fmtId((f.id | CAN_ERR_FLAG) >>> 0, true) + "#" + toHex(f.data);
  const id = fmtId(f.id, f.ext);
  if (f.rtr) return id + "#R" + (f.len ? f.len.toString(16).toUpperCase() : "");
  // candump prints the whole flags nibble, FDF included, so a BRS frame is "##5".
  if (f.fd) return id + "##" + ((f.brs ? CANFD_BRS : 0) | (f.esi ? CANFD_ESI : 0) | CANFD_FDF).toString(16).toUpperCase() + toHex(f.data);
  return id + "#" + toHex(f.data);
}

/** Epoch ms → "SECS.USECS", rounded to the microsecond (ms floats like 1700000000000.001 sit just below). */
export function fmtSecs(ts: number): string {
  let s = Math.floor(ts / 1000);
  let us = Math.round((ts - s * 1000) * 1000);
  if (us >= 1000000) {
    s++;
    us -= 1000000;
  }
  return `${s}.${String(us).padStart(6, "0")}`;
}

export function candumpLine(f: CanFrame, iface: string): string {
  return `(${fmtSecs(f.ts)}) ${iface} ${candumpBody(f)}`;
}

/** cansend syntax → spec. "123#DEADBEEF", "123#DE.AD", "1F334455#R", "123#R4", "123##1DEADBEEF". */
export function parseCansend(s: string): FrameSpec {
  const m = /^\s*([0-9a-fA-F]{1,8})#(#?)(.*?)\s*$/.exec(s);
  if (!m) throw new Error(`Expected cansend syntax like 123#DEADBEEF, got "${s}"`);
  const [, idS, fdMark, rest] = m;
  const id = parseInt(idS, 16);
  const ext = idS.length > 3;
  if (fdMark) {
    if (!rest.length) throw new Error("CAN FD frame needs a flags nibble after ##");
    const flags = parseInt(rest[0], 16);
    return { id, ext, fd: true, brs: !!(flags & CANFD_BRS), esi: !!(flags & CANFD_ESI), data: parseHex(rest.slice(1)) };
  }
  if (/^R/i.test(rest)) return { id, ext, rtr: true, len: rest.length > 1 ? parseInt(rest.slice(1), 16) : 0 };
  return { id, ext, data: parseHex(rest) };
}

/** Parse one `candump -L` line (for replay and tests). */
export function parseCandumpLine(line: string): { ts: number; iface: string; frame: CanFrame } | null {
  const m = /^\((\d+)\.(\d{6})\)\s+(\S+)\s+(\S+)/.exec(line);
  if (!m) return null;
  const ts = Number(m[1]) * 1000 + Number(m[2]) / 1000;
  const body = m[4];
  const idS = body.slice(0, body.indexOf("#"));
  if (idS.length === 8 && parseInt(idS, 16) & CAN_ERR_FLAG) {
    const data = parseHex(body.slice(body.indexOf("#") + 1));
    return { ts, iface: m[3], frame: { id: parseInt(idS, 16) & CAN_ERR_MASK, ext: false, rtr: false, err: true, fd: false, brs: false, esi: false, len: data.length, data, ts } };
  }
  const f = makeFrame(parseCansend(body));
  f.ts = ts;
  return { ts, iface: m[3], frame: f };
}

// ---------- Error frames (linux/can/error.h) ----------

const ERR_CLASS: [number, string][] = [
  [0x001, "TX timeout"],
  [0x002, "lost arbitration"],
  [0x004, "controller problem"],
  [0x008, "protocol violation"],
  [0x010, "transceiver"],
  [0x020, "no ACK"],
  [0x040, "bus off"],
  [0x080, "bus error"],
  [0x100, "restarted"],
  [0x200, "error counters"],
];
const ERR_CTRL: [number, string][] = [
  [0x01, "RX overflow"],
  [0x02, "TX overflow"],
  [0x04, "RX warning"],
  [0x08, "TX warning"],
  [0x10, "RX passive"],
  [0x20, "TX passive"],
  [0x40, "back to error-active"],
];
const ERR_PROT: [number, string][] = [
  [0x01, "bit"],
  [0x02, "form"],
  [0x04, "stuff"],
  [0x08, "dominant bit"],
  [0x10, "recessive bit"],
  [0x20, "overload"],
  [0x40, "active error flag"],
  [0x80, "during TX"],
];
const ERR_LOC: Record<number, string> = {
  0x03: "SOF", 0x02: "ID28–21", 0x06: "ID20–18", 0x04: "SRTR", 0x05: "IDE", 0x07: "ID17–13", 0x0f: "ID12–5", 0x0e: "ID4–0",
  0x0c: "RTR", 0x0d: "res1", 0x09: "res0", 0x0b: "DLC", 0x0a: "data", 0x08: "CRC", 0x18: "CRC delim", 0x19: "ACK slot",
  0x1b: "ACK delim", 0x1a: "EOF", 0x12: "intermission",
};
const ERR_TRX: Record<number, string> = {
  0x04: "CANH no wire", 0x05: "CANH short to BAT", 0x06: "CANH short to VCC", 0x07: "CANH short to GND",
  0x40: "CANL no wire", 0x50: "CANL short to BAT", 0x60: "CANL short to VCC", 0x70: "CANL short to GND", 0x80: "CANL short to CANH",
};

export interface DecodedError {
  ts: number;
  classes: string[];
  text: string;
  busOff: boolean;
  tec?: number;
  rec?: number;
}

export function decodeError(f: CanFrame): DecodedError {
  const d = f.data;
  const classes = ERR_CLASS.filter(([b]) => f.id & b).map(([, n]) => n);
  const parts: string[] = [];
  if (f.id & 0x002 && d[0]) parts.push(`arbitration lost at bit ${d[0]}`);
  if (f.id & 0x004 && d[1]) parts.push("controller: " + ERR_CTRL.filter(([b]) => d[1] & b).map(([, n]) => n).join(", "));
  if (f.id & 0x008) {
    const types = ERR_PROT.filter(([b]) => d[2] & b).map(([, n]) => n);
    const loc = ERR_LOC[d[3]];
    parts.push(`protocol: ${types.length ? types.join("/") + " error" : "unspecified"}${loc ? " at " + loc : ""}`);
  }
  if (f.id & 0x010 && d[4]) parts.push("transceiver: " + (ERR_TRX[d[4]] || "0x" + HEX[d[4]]));
  const out: DecodedError = { ts: f.ts, classes, text: [...classes.filter((c) => !["controller problem", "protocol violation", "transceiver", "error counters"].includes(c)), ...parts].join("; ") || "error frame", busOff: !!(f.id & 0x040) };
  if (f.id & 0x200 || d[6] || d[7]) {
    out.tec = d[6];
    out.rec = d[7];
  }
  return out;
}

// ---------- Bus load ----------

/**
 * Bits on the wire for one frame, worst-case bit stuffing, including the 3-bit
 * intermission. Classic: the usual 47+8n (std) / 67+8n (ext) plus ⌊(34+8n−1)/4⌋ or
 * ⌊(54+8n−1)/4⌋ stuff bits. CAN FD is split into the part sent at the nominal rate and, with
 * BRS, the part sent at the data rate (ESI…CRC delimiter), with the FD CRC's fixed stuff bits.
 */
export function frameBits(f: { ext: boolean; rtr: boolean; fd: boolean; brs: boolean; len: number }): { nom: number; data: number } {
  const n = f.rtr ? 0 : f.len;
  if (!f.fd) {
    const base = f.ext ? 67 : 47;
    const stuffable = (f.ext ? 54 : 34) + 8 * n;
    return { nom: base + 8 * n + Math.floor((stuffable - 1) / 4), data: 0 };
  }
  const arb = f.ext ? 36 : 17; // SOF … BRS
  const tail = 1 + 1 + 7 + 3; // ACK slot, ACK delim, EOF, intermission
  const crc = n > 16 ? 21 : 17;
  const dyn = 1 + 4 + 8 * n; // ESI, DLC, data: dynamically stuffed
  const dataBits = dyn + Math.floor((dyn - 1) / 4) + 4 + crc + Math.ceil((4 + crc) / 4) + 1; // + stuff count, CRC (+fixed stuff), CRC delim
  const nom = arb + Math.floor((arb - 1) / 4) + tail;
  return f.brs ? { nom, data: dataBits } : { nom: nom + dataBits, data: 0 };
}

/** Seconds of bus time one frame takes. */
export function frameTime(f: Parameters<typeof frameBits>[0], bitrate: number, dbitrate?: number): number {
  const b = frameBits(f);
  return b.nom / bitrate + (b.data ? b.data / (dbitrate || bitrate) : 0);
}

// ---------- Filters ----------

export interface IdFilter {
  id: number;
  mask?: number; // default: exact match
  ext?: boolean; // omitted: either
}

export function parseFilters(x: any): IdFilter[] | null {
  if (x == null || x === "" || (Array.isArray(x) && !x.length)) return null;
  const arr = Array.isArray(x) ? x : [x];
  return arr.map((e: any) => {
    const num = (v: any) => (typeof v === "string" ? parseInt(v.replace(/^0x/i, ""), 16) : Number(v));
    const id = num(e.id);
    if (!Number.isFinite(id)) throw new Error("filter id must be a number or hex string");
    const mask = e.mask == null ? undefined : num(e.mask);
    return { id, mask, ext: e.ext == null ? undefined : !!e.ext };
  });
}

/**
 * Compact filter syntax for query strings and the UI: comma-separated hex ids, each with an
 * optional ":mask"; an "x" prefix forces extended. "123,7E0:7F8,x18DAF100:1FFFFF00".
 */
export function parseFilterString(s: string | null | undefined): IdFilter[] | null {
  if (!s || !s.trim()) return null;
  return s.split(/[,\s]+/).filter(Boolean).map((tok) => {
    const m = /^(x)?(?:0x)?([0-9a-f]{1,8})(?::(?:0x)?([0-9a-f]{1,8}))?$/i.exec(tok);
    if (!m) throw new Error(`Bad filter "${tok}": use hex ID or ID:MASK, x prefix for extended`);
    const id = parseInt(m[2], 16);
    return { id, mask: m[3] ? parseInt(m[3], 16) : undefined, ext: m[1] ? true : m[2].length > 3 || id > CAN_SFF_MASK ? true : undefined };
  });
}

/**
 * A frame from JSON (REST, MCP, WebSocket). `id` as a number, or a hex string ("0x123" or
 * "123"); `data` as hex or a byte array; or `cansend: "123#DEADBEEF"` instead of both.
 */
export function specFromJson(o: any): FrameSpec {
  if (!o || typeof o !== "object") throw new Error("frame must be an object");
  if (typeof o.cansend === "string") return parseCansend(o.cansend);
  if (o.id == null || o.id === "") throw new Error("frame needs an id");
  const id = typeof o.id === "string" ? parseInt(o.id.replace(/^0x/i, ""), 16) : Number(o.id);
  if (!Number.isFinite(id)) throw new Error(`bad id ${JSON.stringify(o.id)}`);
  return { id, ext: o.ext, rtr: o.rtr, fd: o.fd, brs: o.brs, esi: o.esi, len: o.len, data: o.data ?? "" };
}

export function matchFilters(f: { id: number; ext: boolean; err: boolean }, filters: IdFilter[] | null): boolean {
  if (!filters) return true;
  if (f.err) return false;
  for (const flt of filters) {
    if (flt.ext !== undefined && flt.ext !== f.ext) continue;
    const mask = flt.mask ?? (f.ext ? CAN_EFF_MASK : CAN_SFF_MASK);
    if (((f.id ^ flt.id) & mask) === 0) return true;
  }
  return false;
}

// ---------- Auto-bitrate decision ----------

export const STD_BITRATES = [1000000, 800000, 500000, 250000, 125000, 100000, 83333, 50000, 33333, 20000, 10000];

export interface BitrateTrial {
  bitrate: number;
  frames: number; // valid frames received
  errors: number; // error frames received
  busErrors: number; // controller bus_error counter delta
}

export interface BitrateVerdict {
  result: "found" | "silent" | "none";
  bitrate?: number;
  confidence?: "high" | "low";
  text: string;
}

/**
 * Pick the bitrate. At the right rate a listen-only controller decodes frames cleanly; at a
 * wrong one it sees mostly stuff/form/CRC errors, with the odd fluke frame. So the winner is
 * the rate with the most valid frames whose error share stays under half; confidence is high
 * when it saw a few frames and errors were rare.
 */
export function pickBitrate(trials: BitrateTrial[]): BitrateVerdict {
  const bad = (t: BitrateTrial) => t.errors + t.busErrors;
  const ratio = (t: BitrateTrial) => bad(t) / Math.max(1, t.frames + bad(t));
  const ok = trials.filter((t) => t.frames > 0 && ratio(t) < 0.5);
  if (ok.length) {
    ok.sort((a, b) => b.frames - a.frames || ratio(a) - ratio(b) || b.bitrate - a.bitrate);
    const w = ok[0];
    const high = w.frames >= 3 && ratio(w) < 0.05;
    return { result: "found", bitrate: w.bitrate, confidence: high ? "high" : "low", text: `${fmtBitrate(w.bitrate)}: ${w.frames} frames, ${bad(w)} errors${high ? "" : " (low confidence: few frames or many errors)"}` };
  }
  if (trials.every((t) => t.frames === 0 && bad(t) === 0)) return { result: "silent", text: "Silent bus: no frames and no errors at any rate. Is anything transmitting, and is the bus wired and terminated?" };
  return { result: "none", text: "Errors at every rate and no clean frames: check CAN-H/CAN-L wiring and termination, or the bus uses a non-standard bitrate." };
}

export function fmtBitrate(b: number): string {
  if (!b) return "?";
  if (b >= 1000000) return +(b / 1000000).toFixed(3) + "M";
  if (b >= 1000) return +(b / 1000).toFixed(3) + "k";
  return String(b);
}

// ---------- socketcand ASCII protocol ----------

/** Pull complete `< … >` elements out of a receive buffer, socketcand style: junk before '<' is dropped. */
export function scTake(buf: string): { cmds: string[]; rest: string } {
  const cmds: string[] = [];
  let i = 0;
  for (;;) {
    const s = buf.indexOf("<", i);
    if (s < 0) return { cmds, rest: "" };
    const e = buf.indexOf(">", s + 1);
    if (e < 0) {
      const rest = buf.slice(s);
      // A command never gets this long; don't let a peer grow the buffer forever.
      return { cmds, rest: rest.length > 4096 ? "" : rest };
    }
    cmds.push(buf.slice(s, e + 1).replace(/\s+/g, " "));
    i = e + 1;
  }
}

/** The words inside "< a b c >". */
export function scWords(cmd: string): string[] {
  return cmd.replace(/^<\s*/, "").replace(/\s*>$/, "").split(" ").filter(Boolean);
}

const scSecs = fmtSecs;

/** Raw-mode RX line: `< frame 123 1700000000.123456 DEADBEEF >` (ext ids are 8 digits; python-can keys on that). */
export function scFrameRaw(f: CanFrame): string {
  return `< frame ${fmtId(f.id, f.ext)} ${scSecs(f.ts)} ${toHex(f.data)} >`;
}

/** BCM-mode RX line: data bytes space separated, as socketcand's state_bcm prints them. */
export function scFrameBcm(f: CanFrame): string {
  let s = `< frame ${fmtId(f.id, f.ext)} ${scSecs(f.ts)} `;
  for (const b of f.data) s += HEX[b] + " ";
  return s + " >";
}

export function scError(f: CanFrame): string {
  return `< error ${f.id.toString(16).toUpperCase().padStart(3, "0")} ${scSecs(f.ts)} >`;
}

/**
 * Parse the id/len/data part of `< send ID LEN B0 … >` (also `add`/`update` after their
 * leading words). socketcand marks the id extended when it is 8 characters; SavvyCAN also
 * sets bit 31 on extended ids, so mask that off. Bytes are hex, `a` or `0A`.
 */
export function scParseIdLenData(words: string[]): CanFrame {
  if (words.length < 2) throw new Error("expected ID LEN [bytes…]");
  const [idS, lenS, ...bytes] = words;
  if (!/^[0-9a-fA-F]{1,8}$/.test(idS)) throw new Error("bad id");
  const raw = parseInt(idS, 16);
  const len = parseInt(lenS, 10);
  if (!(len >= 0 && len <= 8)) throw new Error("bad length");
  if (bytes.length !== len || bytes.some((b) => !/^[0-9a-fA-F]{1,2}$/.test(b))) throw new Error("byte count doesn't match length");
  const id = raw & CAN_EFF_MASK;
  return makeFrame({ id, ext: idS.length === 8 || id > CAN_SFF_MASK, data: bytes.map((b) => parseInt(b, 16)) });
}

export function beaconXml(host: string, ip: string, port: number, buses: string[]): string {
  const x = (s: string) => s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!);
  return `<CANBeacon name="${x(host)}" type="SocketCAN" description="DeskyPi socketcand">\n<URL>can://${ip}:${port}</URL>${buses.map((b) => `<Bus name="${x(b)}"/>`).join("")}</CANBeacon>`;
}

// ---------- The socket (bun:ffi) ----------

let libc: any = null;
function lib() {
  if (libc) return libc;
  const { dlopen, FFIType } = require("bun:ffi");
  libc = dlopen("libc.so.6", {
    socket: { args: [FFIType.i32, FFIType.i32, FFIType.i32], returns: FFIType.i32 },
    setsockopt: { args: [FFIType.i32, FFIType.i32, FFIType.i32, FFIType.ptr, FFIType.u32], returns: FFIType.i32 },
    bind: { args: [FFIType.i32, FFIType.ptr, FFIType.u32], returns: FFIType.i32 },
    // fcntl is variadic; on aarch64/x86-64 Linux the third int still goes in a register.
    fcntl: { args: [FFIType.i32, FFIType.i32, FFIType.i32], returns: FFIType.i32 },
    if_nametoindex: { args: [FFIType.cstring], returns: FFIType.u32 },
    close: { args: [FFIType.i32], returns: FFIType.i32 },
    recvmsg: { args: [FFIType.i32, FFIType.ptr, FFIType.i32], returns: FFIType.i64 },
    __errno_location: { args: [], returns: FFIType.ptr },
    strerror: { args: [FFIType.i32], returns: FFIType.cstring },
  }).symbols;
  return libc;
}

let readI32: ((p: number, off: number) => number) | null = null;
function errno(): number {
  readI32 ??= require("bun:ffi").read.i32;
  return readI32!(lib().__errno_location(), 0);
}

function errnoMsg(what: string): Error {
  const e = errno();
  return new Error(`${what}: ${String(lib().strerror(e))} (errno ${e})`);
}

export interface CanSocketOptions {
  fdFrames?: boolean; // receive (and allow sending) CAN FD frames
  errMask?: number; // CAN_ERR_* classes to receive as error frames; 0 = none
  recvOwn?: boolean; // also receive frames this socket sent (after the controller confirms them)
  loopback?: boolean; // let other local sockets see our frames (kernel default: on)
  rcvbuf?: number; // SO_RCVBUF bytes (kernel caps it at net.core.rmem_max; each frame costs ~1 KB of skb)
}

/**
 * A bound raw CAN socket. Frames arrive in batches through `onFrames`; `onClose` fires once,
 * with the error when the read side failed (ENETDOWN when the interface goes down).
 */
export class CanSocket {
  readonly fd: number;
  readonly iface: string;
  readonly fdFrames: boolean;
  closed = false;
  /** Frames the kernel dropped because our receive queue was full (SO_RXQ_OVFL, cumulative). */
  drops = 0;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private idleTicks = 0;
  // Buffers the kernel writes into must be built over an explicit ArrayBuffer. A small
  // `new Uint8Array(n)` is an inline JSC array: ptr() returns its current address, and the
  // first `.buffer` access later moves the bytes to new storage, so the kernel kept writing
  // to the old address while we re-read a frozen copy of the first frame (seen on desky).
  private buf = new Uint8Array(new ArrayBuffer(CANFD_MTU));
  private ctrl = new Uint8Array(new ArrayBuffer(CTRL_LEN));
  private iov = new Uint8Array(new ArrayBuffer(16));
  private msg = new Uint8Array(new ArrayBuffer(56));
  private msgDv = new DataView(this.msg.buffer);
  private ctrlDv = new DataView(this.ctrl.buffer);
  private msgPtr = 0;

  constructor(iface: string, opts: CanSocketOptions, private onFrames: (f: CanFrame[]) => void, private onClose?: (err?: Error) => void) {
    const { ptr } = require("bun:ffi");
    const L = lib();
    this.iface = iface;
    this.fdFrames = !!opts.fdFrames;
    const ifindex = L.if_nametoindex(Buffer.from(iface + "\0"));
    if (!ifindex) throw new Error(`No such interface: ${iface}`);
    const fd = L.socket(AF_CAN, SOCK_RAW, CAN_RAW);
    if (fd < 0) throw errnoMsg("socket(AF_CAN)");
    const opt = (level: number, name: number, v: number, label: string) => {
      const a = new Uint32Array([v >>> 0]);
      if (L.setsockopt(fd, level, name, ptr(a), 4) < 0) {
        const e = errnoMsg(label);
        L.close(fd);
        throw e;
      }
    };
    if (opts.fdFrames) opt(SOL_CAN_RAW, CAN_RAW_FD_FRAMES, 1, "CAN_RAW_FD_FRAMES");
    if (opts.errMask) opt(SOL_CAN_RAW, CAN_RAW_ERR_FILTER, opts.errMask & CAN_ERR_MASK, "CAN_RAW_ERR_FILTER");
    if (opts.recvOwn) opt(SOL_CAN_RAW, CAN_RAW_RECV_OWN_MSGS, 1, "CAN_RAW_RECV_OWN_MSGS");
    if (opts.loopback === false) opt(SOL_CAN_RAW, CAN_RAW_LOOPBACK, 0, "CAN_RAW_LOOPBACK");
    opt(SOL_SOCKET, SO_TIMESTAMP, 1, "SO_TIMESTAMP");
    opt(SOL_SOCKET, SO_RXQ_OVFL, 1, "SO_RXQ_OVFL");
    // Frames arrive between polls; the default ~200 KB buffer holds only a few hundred.
    // Best effort: SO_RCVBUF is silently capped at net.core.rmem_max, never an error.
    if (opts.rcvbuf) L.setsockopt(fd, SOL_SOCKET, SO_RCVBUF, ptr(new Uint32Array([opts.rcvbuf])), 4);
    const sa = new Uint8Array(24);
    const dv = new DataView(sa.buffer);
    dv.setUint16(0, AF_CAN, true);
    dv.setInt32(4, ifindex, true);
    if (L.bind(fd, ptr(sa), 24) < 0) {
      const e = errnoMsg(`bind(${iface})`);
      L.close(fd);
      throw e;
    }
    const fl = L.fcntl(fd, F_GETFL, 0);
    if (fl < 0 || L.fcntl(fd, F_SETFL, fl | O_NONBLOCK) < 0) {
      const e = errnoMsg("fcntl(O_NONBLOCK)");
      L.close(fd);
      throw e;
    }
    this.fd = fd;
    // struct iovec { base, len } → our frame buffer; struct msghdr (56 bytes on 64-bit Linux):
    // name@0 namelen@8 iov@16 iovlen@24 control@32 controllen@40 flags@48. The typed arrays
    // live as long as this object, so their pointers stay valid.
    const iovDv = new DataView(this.iov.buffer);
    iovDv.setBigUint64(0, BigInt(ptr(this.buf)), true);
    iovDv.setBigUint64(8, BigInt(CANFD_MTU), true);
    this.msgDv.setBigUint64(16, BigInt(ptr(this.iov)), true);
    this.msgDv.setBigUint64(24, 1n, true);
    this.msgDv.setBigUint64(32, BigInt(ptr(this.ctrl)), true);
    this.msgPtr = ptr(this.msg);
    this.poll();
  }

  /**
   * Drain the socket with non-blocking recvmsg() on a short timer. Measured on desky (Pi 4,
   * vcan, ~1k frames/s): Bun.file(fd).stream() costs ~37 % of a core, because nearly every
   * datagram becomes its own chunk, promise and stream turn; this loop costs ~3 % (7 % at
   * 3k fps, 9 % at 7k fps), and recvmsg also hands over the kernel RX timestamp, the
   * MSG_CONFIRM flag on our own echoed frames, and the drop counter. Polls every 5 ms while
   * traffic flows and every 20 ms after ~250 ms of silence.
   */
  private poll = () => {
    this.timer = null;
    if (this.closed) return;
    const L = lib();
    const frames: CanFrame[] = [];
    for (let n = 0; n < 8192; n++) {
      this.msgDv.setBigUint64(40, BigInt(CTRL_LEN), true);
      this.msgDv.setInt32(48, 0, true);
      const r = Number(L.recvmsg(this.fd, this.msgPtr, MSG_DONTWAIT));
      if (r < 0) {
        const e = errno();
        if (e === EAGAIN || e === EINTR) break;
        if (frames.length) this.onFrames(frames);
        this.close(new Error(`${this.iface}: ${String(L.strerror(e))} (errno ${e})`));
        return;
      }
      if (r < CAN_MTU) continue;
      const flags = this.msgDv.getInt32(48, true);
      let ts = 0;
      const clen = Number(this.msgDv.getBigUint64(40, true));
      for (let off = 0; off + 16 <= clen; ) {
        const len = Number(this.ctrlDv.getBigUint64(off, true));
        const level = this.ctrlDv.getInt32(off + 8, true);
        const type = this.ctrlDv.getInt32(off + 12, true);
        if (level === SOL_SOCKET && type === SO_TIMESTAMP) ts = Number(this.ctrlDv.getBigInt64(off + 16, true)) * 1000 + Number(this.ctrlDv.getBigInt64(off + 24, true)) / 1000;
        else if (level === SOL_SOCKET && type === SO_RXQ_OVFL) this.drops = this.ctrlDv.getUint32(off + 16, true);
        if (len < 16) break;
        off += (len + 7) & ~7;
      }
      const at = frames.length;
      splitFrames(this.buf.subarray(0, r), ts || performance.timeOrigin + performance.now(), frames);
      for (let i = at; i < frames.length; i++) {
        if (flags & MSG_CONFIRM) frames[i].own = true;
        else if (flags & MSG_DONTROUTE) frames[i].local = true;
      }
    }
    if (frames.length) {
      this.idleTicks = 0;
      this.onFrames(frames);
    } else this.idleTicks++;
    if (!this.closed) this.timer = setTimeout(this.poll, this.idleTicks > 50 ? 20 : 5);
  };

  /** Throws on failure (ENOBUFS when the TX queue is full, ENETDOWN, EINVAL for FD on a classic socket…). */
  send(f: CanFrame) {
    if (this.closed) throw new Error(`${this.iface}: socket closed`);
    if (f.fd && !this.fdFrames) throw new Error(`${this.iface} is not in CAN FD mode`);
    try {
      writeSync(this.fd, encodeFrame(f));
    } catch (e: any) {
      const hint = e.code === "ENOBUFS" || e.code === "EAGAIN" ? " (TX queue full: is anything ACKing on the bus?)" : e.code === "ENETDOWN" ? " (interface is down)" : "";
      throw new Error(`${this.iface}: send failed: ${e.code || e.message}${hint}`);
    }
  }

  close(err?: Error) {
    if (this.closed) return;
    this.closed = true;
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    try {
      closeSync(this.fd);
    } catch {}
    this.onClose?.(err);
  }
}
