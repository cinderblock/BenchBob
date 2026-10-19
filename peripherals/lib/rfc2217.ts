// ==================== Telnet + RFC 2217 (Com Port Control) server session ====================
//
// One instance per TCP client. Transport-agnostic: feed() takes bytes from the socket and
// returns the serial payload with the Telnet layer stripped; everything the protocol wants
// to say back goes through `send`. The serial side is reached through `PortHooks`, so this
// file has no I/O of its own and is unit-tested with a fake port.
//
// Written against RFC 854/855 (Telnet option negotiation, using the "Q method" spirit of
// RFC 1143: only answer when an option's state actually changes, so two peers can never
// loop) and RFC 2217. The reference client is pyserial's rfc2217:// (3.5), which:
//  - requires its WILL COM-PORT-OPTION to be answered with DO;
//  - waits up to 3 s for every SET-* reply and checks that the reply value *starts with*
//    the value it asked for, so replies must echo the effective setting promptly;
//  - polls the modem state by sending NOTIFY-MODEMSTATE (7) and expects a 107 back.

import type { SerialConfig, Parity } from "./termios.ts";

export const IAC = 255, DONT = 254, DO = 253, WONT = 252, WILL = 251, SB = 250, SE = 240;
export const OPT_BINARY = 0, OPT_ECHO = 1, OPT_SGA = 3, OPT_COM_PORT = 44;

// Client -> server sub-option codes; the server answers with code + 100.
export const CPO = {
  SIGNATURE: 0, SET_BAUDRATE: 1, SET_DATASIZE: 2, SET_PARITY: 3, SET_STOPSIZE: 4, SET_CONTROL: 5,
  NOTIFY_LINESTATE: 6, NOTIFY_MODEMSTATE: 7, FLOWCONTROL_SUSPEND: 8, FLOWCONTROL_RESUME: 9,
  SET_LINESTATE_MASK: 10, SET_MODEMSTATE_MASK: 11, PURGE_DATA: 12,
} as const;

const PARITY_CODE: Record<Parity, number> = { none: 1, odd: 2, even: 3, mark: 4, space: 5 };
const CODE_PARITY: Record<number, Parity> = { 1: "none", 2: "odd", 3: "even", 4: "mark", 5: "space" };

export const MODEM = { CTS: 0x10, DSR: 0x20, RI: 0x40, CD: 0x80 } as const;

export interface PortHooks {
  config(): SerialConfig;
  /** Apply a partial change; return the effective config (or throw to keep the old one). */
  setConfig(c: Partial<SerialConfig>): SerialConfig;
  lines(): { dtr: boolean; rts: boolean; cts: boolean; dsr: boolean; dcd: boolean; ri: boolean } | null;
  setLines(l: { dtr?: boolean; rts?: boolean }): void;
  breakState(): boolean;
  setBreak(on: boolean): void;
  purge(which: "rx" | "tx" | "both"): void;
  /** Client asked us to pause (true) or resume (false) sending it serial data. */
  suspend?(on: boolean): void;
  log?(msg: string): void;
}

/** Double every IAC so serial bytes can't be mistaken for Telnet commands. */
export function escapeIac(data: Uint8Array): Uint8Array {
  let n = 0;
  for (const b of data) if (b === IAC) n++;
  if (!n) return data;
  const out = new Uint8Array(data.length + n);
  let j = 0;
  for (const b of data) {
    out[j++] = b;
    if (b === IAC) out[j++] = IAC;
  }
  return out;
}

export function modemByte(l: ReturnType<PortHooks["lines"]>): number {
  if (!l) return 0;
  return (l.cts ? MODEM.CTS : 0) | (l.dsr ? MODEM.DSR : 0) | (l.ri ? MODEM.RI : 0) | (l.dcd ? MODEM.CD : 0);
}

const enum M {
  Data,
  Iac,
  Neg,
  Sub,
  SubIac,
}

export class Rfc2217Session {
  // Per-option state for each side: "we" = this server performs it, "they" = the client does.
  private we = new Map<number, boolean>();
  private they = new Map<number, boolean>();
  private mode = M.Data;
  private negCmd = 0;
  private sub: number[] = [];
  private lastCr = false;
  private modemMask = 0xff;
  private lineMask = 0;
  private lastModem: number | null = null;
  /** True once the client has agreed to COM-PORT-OPTION in either direction. */
  rfc2217 = false;
  signature = "BenchBob";

  // Options we'll agree to. ECHO lets an interactive `telnet host port` switch to
  // character mode and leave echoing to the device, which is what ser2net does too.
  private static WE_OK = new Set([OPT_BINARY, OPT_SGA, OPT_ECHO, OPT_COM_PORT]);
  private static THEY_OK = new Set([OPT_BINARY, OPT_SGA, OPT_COM_PORT]);

  constructor(private port: PortHooks, private send: (b: Uint8Array) => void) {}

  /** Our opening offer. Harmless for clients that don't speak Telnet; they'll see a few bytes once. */
  start() {
    this.offer(WILL, OPT_SGA);
    this.offer(WILL, OPT_ECHO);
    this.offer(WILL, OPT_BINARY);
    this.offer(DO, OPT_BINARY);
    this.offer(WILL, OPT_COM_PORT);
  }

  private offer(cmd: number, opt: number) {
    // Record the request as granted-pending; if refused, the DONT/WONT resets it.
    (cmd === WILL ? this.we : this.they).set(opt, true);
    this.send(new Uint8Array([IAC, cmd, opt]));
  }

  /** Client bytes in; serial payload out. */
  feed(chunk: Uint8Array): Uint8Array {
    const out: number[] = [];
    const binIn = () => this.they.get(OPT_BINARY) === true;
    for (const b of chunk) {
      switch (this.mode) {
        case M.Data:
          if (b === IAC) this.mode = M.Iac;
          else {
            // NVT: a non-binary client sends CR as "CR NUL". Drop the NUL.
            if (b === 0 && this.lastCr && !binIn()) {
              this.lastCr = false;
              break;
            }
            this.lastCr = b === 13;
            out.push(b);
          }
          break;
        case M.Iac:
          if (b === IAC) {
            out.push(IAC);
            this.lastCr = false;
            this.mode = M.Data;
          } else if (b === SB) {
            this.sub = [];
            this.mode = M.Sub;
          } else if (b === DO || b === DONT || b === WILL || b === WONT) {
            this.negCmd = b;
            this.mode = M.Neg;
          } else this.mode = M.Data; // NOP, AYT, BRK…: nothing useful to do for a serial port
          break;
        case M.Neg:
          this.negotiate(this.negCmd, b);
          this.mode = M.Data;
          break;
        case M.Sub:
          if (b === IAC) this.mode = M.SubIac;
          else this.sub.push(b);
          break;
        case M.SubIac:
          if (b === IAC) {
            this.sub.push(IAC);
            this.mode = M.Sub;
          } else if (b === SE) {
            this.mode = M.Data;
            this.subnegotiation(this.sub);
          } else {
            // Malformed (IAC x inside SB). Abandon the sub-negotiation rather than desync.
            this.mode = M.Data;
          }
          break;
      }
    }
    return Uint8Array.from(out);
  }

  private negotiate(cmd: number, opt: number) {
    const reply = (c: number) => this.send(new Uint8Array([IAC, c, opt]));
    if (cmd === DO || cmd === DONT) {
      const want = cmd === DO;
      const ok = want && Rfc2217Session.WE_OK.has(opt);
      const cur = this.we.get(opt) === true;
      if (ok !== cur || (want && !ok && this.we.get(opt) === undefined)) {
        // Only answer on a state change (or the first refusal), never re-acknowledge.
        this.we.set(opt, ok);
        reply(ok ? WILL : WONT);
      } else this.we.set(opt, ok);
    } else {
      const want = cmd === WILL;
      const ok = want && Rfc2217Session.THEY_OK.has(opt);
      const cur = this.they.get(opt) === true;
      if (ok !== cur || (want && !ok && this.they.get(opt) === undefined)) {
        this.they.set(opt, ok);
        reply(ok ? DO : DONT);
      } else this.they.set(opt, ok);
    }
    if (opt === OPT_COM_PORT && (this.we.get(opt) || this.they.get(opt)) && !this.rfc2217) {
      this.rfc2217 = true;
      this.port.log?.("client speaks RFC 2217");
      this.notifyModem(true);
    }
  }

  private reply(code: number, value: number[]) {
    this.send(this.subFrame(code + 100, value));
  }

  private subnegotiation(s: number[]) {
    if (s[0] !== OPT_COM_PORT || s.length < 2) return;
    const code = s[1];
    const v = s.slice(2);
    const tryCfg = (c: Partial<SerialConfig>) => {
      try {
        return this.port.setConfig(c);
      } catch (e: any) {
        this.port.log?.(`rejected ${JSON.stringify(c)}: ${e.message}`);
        return this.port.config();
      }
    };
    switch (code) {
      case CPO.SIGNATURE:
        if (!v.length) this.reply(code, [...new TextEncoder().encode(this.signature)]);
        break;
      case CPO.SET_BAUDRATE: {
        const req = v.length >= 4 ? ((v[0] << 24) | (v[1] << 16) | (v[2] << 8) | v[3]) >>> 0 : 0;
        const c = req ? tryCfg({ baud: req }) : this.port.config();
        this.reply(code, [(c.baud >>> 24) & 255, (c.baud >>> 16) & 255, (c.baud >>> 8) & 255, c.baud & 255]);
        break;
      }
      case CPO.SET_DATASIZE: {
        const req = v[0] || 0;
        const c = req ? tryCfg({ dataBits: req as any }) : this.port.config();
        this.reply(code, [c.dataBits]);
        break;
      }
      case CPO.SET_PARITY: {
        const req = CODE_PARITY[v[0]];
        const c = req ? tryCfg({ parity: req }) : this.port.config();
        this.reply(code, [PARITY_CODE[c.parity]]);
        break;
      }
      case CPO.SET_STOPSIZE: {
        // 1 = 1, 2 = 2, 3 = 1.5. Linux has no 1.5 (except CSTOPB at 5 bits), so 1.5 maps to 2
        // and the reply says so; pyserial then reports the mismatch to its user.
        const req = v[0] === 1 ? 1 : v[0] === 2 || v[0] === 3 ? 2 : 0;
        const c = req ? tryCfg({ stopBits: req as 1 | 2 }) : this.port.config();
        this.reply(code, [c.stopBits]);
        break;
      }
      case CPO.SET_CONTROL:
        this.reply(code, [this.control(v[0] ?? 0)]);
        break;
      case CPO.NOTIFY_MODEMSTATE:
        // pyserial uses this as a poll.
        this.notifyModem(true);
        break;
      case CPO.NOTIFY_LINESTATE:
        this.send(this.subFrame(106, [0]));
        break;
      case CPO.FLOWCONTROL_SUSPEND:
        this.port.suspend?.(true);
        break;
      case CPO.FLOWCONTROL_RESUME:
        this.port.suspend?.(false);
        break;
      case CPO.SET_LINESTATE_MASK:
        this.lineMask = v[0] ?? 0;
        this.reply(code, [this.lineMask]);
        break;
      case CPO.SET_MODEMSTATE_MASK:
        this.modemMask = v[0] ?? 0;
        this.reply(code, [this.modemMask]);
        break;
      case CPO.PURGE_DATA: {
        const which = v[0] === 1 ? "rx" : v[0] === 2 ? "tx" : v[0] === 3 ? "both" : null;
        if (which) {
          try {
            this.port.purge(which);
          } catch {}
        }
        this.reply(code, [v[0] ?? 0]);
        break;
      }
    }
  }

  /** SET-CONTROL: apply and return the value to echo (requests answer with the current state). */
  private control(c: number): number {
    const l = this.port.lines();
    const cfg = this.port.config();
    const flowCode = cfg.flow === "rtscts" ? 3 : cfg.flow === "xonxoff" ? 2 : 1;
    const setFlow = (flow: SerialConfig["flow"]) => {
      try {
        this.port.setConfig({ flow });
      } catch {}
    };
    switch (c) {
      case 0:
        return flowCode;
      case 1:
        setFlow("none");
        return c;
      case 2:
        setFlow("xonxoff");
        return c;
      case 3:
        setFlow("rtscts");
        return c;
      case 4:
        return this.port.breakState() ? 5 : 6;
      case 5:
      case 6:
        this.port.setBreak(c === 5);
        return c;
      case 7:
        return l?.dtr ? 8 : 9;
      case 8:
      case 9:
        this.port.setLines({ dtr: c === 8 });
        return c;
      case 10:
        return l?.rts ? 11 : 12;
      case 11:
      case 12:
        this.port.setLines({ rts: c === 11 });
        return c;
      case 13:
        return cfg.flow === "rtscts" ? 16 : cfg.flow === "xonxoff" ? 15 : 14;
      case 14:
      case 15:
      case 16:
        // Inbound flow control can't differ from outbound on Linux; apply the same setting.
        setFlow(c === 16 ? "rtscts" : c === 15 ? "xonxoff" : "none");
        return c;
      default:
        // DCD/DTR/DSR flow control (17-19) aren't supported by the tty layer: report "none".
        return 1;
    }
  }

  private subFrame(code: number, value: number[]) {
    const body = escapeIac(Uint8Array.from([OPT_COM_PORT, code, ...value]));
    const out = new Uint8Array(body.length + 4);
    out.set([IAC, SB]);
    out.set(body, 2);
    out.set([IAC, SE], body.length + 2);
    return out;
  }

  /** Call when modem lines change (and periodically is fine): sends NOTIFY-MODEMSTATE on a change. */
  notifyModem(force = false) {
    if (!this.rfc2217) return;
    const state = modemByte(this.port.lines());
    const prev = this.lastModem ?? 0;
    if (!force && state === this.lastModem) return;
    const delta = state ^ prev;
    let v = state | (delta & MODEM.CTS ? 1 : 0) | (delta & MODEM.DSR ? 2 : 0) | (delta & MODEM.RI ? 4 : 0) | (delta & MODEM.CD ? 8 : 0);
    this.lastModem = state;
    v &= force ? 0xff : this.modemMask;
    if (!force && !v) return;
    this.send(this.subFrame(107, [v]));
  }

  /** Serial data out to the client, IAC-escaped. */
  data(b: Uint8Array): Uint8Array {
    return escapeIac(b);
  }
}
