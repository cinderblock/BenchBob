import { test, expect } from "bun:test";
import { Rfc2217Session, escapeIac, IAC, SB, SE, DO, DONT, WILL, WONT, OPT_COM_PORT, OPT_BINARY, OPT_ECHO, OPT_SGA, type PortHooks } from "./rfc2217.ts";
import { DEFAULT_CONFIG, normalizeConfig, type SerialConfig } from "./termios.ts";

function fake() {
  const state = { cfg: { ...DEFAULT_CONFIG } as SerialConfig, dtr: true, rts: true, cts: false, brk: false, purged: [] as string[], suspended: false };
  const hooks: PortHooks = {
    config: () => state.cfg,
    setConfig: (c) => (state.cfg = normalizeConfig(c, state.cfg)),
    lines: () => ({ dtr: state.dtr, rts: state.rts, cts: state.cts, dsr: false, dcd: true, ri: false }),
    setLines: (l) => {
      if (l.dtr !== undefined) state.dtr = l.dtr;
      if (l.rts !== undefined) state.rts = l.rts;
    },
    breakState: () => state.brk,
    setBreak: (on) => (state.brk = on),
    purge: (w) => state.purged.push(w),
    suspend: (on) => (state.suspended = on),
  };
  const sent: number[][] = [];
  const s = new Rfc2217Session(hooks, (b) => sent.push([...b]));
  return { s, state, sent, flat: () => sent.flat() };
}

const sub = (...v: number[]) => [IAC, SB, OPT_COM_PORT, ...v, IAC, SE];

test("opening offer", () => {
  const { s, sent } = fake();
  s.start();
  expect(sent).toEqual([
    [IAC, WILL, OPT_SGA],
    [IAC, WILL, OPT_ECHO],
    [IAC, WILL, OPT_BINARY],
    [IAC, DO, OPT_BINARY],
    [IAC, WILL, OPT_COM_PORT],
  ]);
});

test("pyserial's opening requests get exactly the answers it needs, no loops", () => {
  const { s, sent } = fake();
  s.start();
  sent.length = 0;
  // pyserial: DO ECHO, WILL SGA, DO SGA, WILL COM-PORT, DO COM-PORT, then WILL BINARY / DO BINARY in reply to our offers
  const data = s.feed(Uint8Array.from([IAC, DO, OPT_ECHO, IAC, WILL, OPT_SGA, IAC, DO, OPT_SGA, IAC, WILL, OPT_COM_PORT, IAC, DO, OPT_COM_PORT, IAC, WILL, OPT_BINARY, IAC, DO, OPT_BINARY]));
  expect(data.length).toBe(0);
  // Only the client's WILLs that we hadn't offered as DO need a reply (SGA, COM-PORT); then the forced modem state notification.
  expect(sent[0]).toEqual([IAC, DO, OPT_SGA]);
  expect(sent[1]).toEqual([IAC, DO, OPT_COM_PORT]);
  expect(sent[2]).toEqual(sub(107, 0x88)); // CD set, plus its delta bit on the first report
  expect(sent.length).toBe(3);
  expect(s.rfc2217).toBe(true);
});

test("unknown options are refused once", () => {
  const { s, sent } = fake();
  s.feed(Uint8Array.from([IAC, DO, 24, IAC, WILL, 31, IAC, DO, 24]));
  expect(sent).toEqual([
    [IAC, WONT, 24],
    [IAC, DONT, 31],
  ]);
});

test("SET-BAUDRATE applies and echoes; 0 queries", () => {
  const { s, state, sent } = fake();
  s.feed(Uint8Array.from(sub(1, 0x00, 0x01, 0x24, 0xf8))); // 0x0124f8 = 75000
  expect(state.cfg.baud).toBe(75000);
  expect(sent.pop()).toEqual(sub(101, 0x00, 0x01, 0x24, 0xf8));
  s.feed(Uint8Array.from(sub(1, 0, 0, 0, 0)));
  expect(sent.pop()).toEqual(sub(101, 0x00, 0x01, 0x24, 0xf8));
});

test("datasize, parity, stopsize", () => {
  const { s, state, sent } = fake();
  s.feed(Uint8Array.from([...sub(2, 7), ...sub(3, 3), ...sub(4, 2)]));
  expect(state.cfg).toMatchObject({ dataBits: 7, parity: "even", stopBits: 2 });
  expect(sent).toEqual([sub(102, 7), sub(103, 3), sub(104, 2)]);
  sent.length = 0;
  s.feed(Uint8Array.from([...sub(3, 5), ...sub(4, 3)])); // space parity; 1.5 stop -> 2
  expect(sent).toEqual([sub(103, 5), sub(104, 2)]);
});

test("rejected setting answers with the current value", () => {
  const { s, sent } = fake();
  s.feed(Uint8Array.from(sub(2, 9)));
  expect(sent.pop()).toEqual(sub(102, 8));
});

test("SET-CONTROL: DTR/RTS/break/flow and queries", () => {
  const { s, state, sent } = fake();
  s.feed(Uint8Array.from([...sub(5, 9), ...sub(5, 11), ...sub(5, 5), ...sub(5, 3), ...sub(5, 7), ...sub(5, 4), ...sub(5, 0)]));
  expect(state.dtr).toBe(false);
  expect(state.rts).toBe(true);
  expect(state.brk).toBe(true);
  expect(state.cfg.flow).toBe("rtscts");
  expect(sent).toEqual([sub(105, 9), sub(105, 11), sub(105, 5), sub(105, 3), sub(105, 9), sub(105, 5), sub(105, 3)]);
});

test("PURGE-DATA and masks", () => {
  const { s, state, sent } = fake();
  s.feed(Uint8Array.from([...sub(12, 3), ...sub(11, 0x30), ...sub(10, 0)]));
  expect(state.purged).toEqual(["both"]);
  expect(sent).toEqual([sub(112, 3), sub(111, 0x30), sub(110, 0)]);
});

test("NOTIFY-MODEMSTATE poll and change notifications with deltas", () => {
  const { s, state, sent } = fake();
  s.feed(Uint8Array.from([IAC, WILL, OPT_COM_PORT]));
  sent.length = 0;
  s.feed(Uint8Array.from(sub(7)));
  expect(sent.pop()).toEqual(sub(107, 0x80));
  state.cts = true;
  s.notifyModem();
  expect(sent.pop()).toEqual(sub(107, 0x90 | 0x01));
  s.notifyModem(); // no change, nothing sent
  expect(sent.length).toBe(0);
});

test("IAC escaping both ways, including IAC inside a sub-negotiation value", () => {
  expect([...escapeIac(Uint8Array.from([1, 255, 2, 255]))]).toEqual([1, 255, 255, 2, 255, 255]);
  const { s, state, sent } = fake();
  expect([...s.feed(Uint8Array.from([0x41, IAC, IAC, 0x42]))]).toEqual([0x41, 255, 0x42]);
  // baud 0x0000ffff: each 0xff byte arrives doubled
  s.feed(Uint8Array.from([IAC, SB, OPT_COM_PORT, 1, 0, 0, 0xff, 0xff, 0xff, 0xff, IAC, SE]));
  expect(state.cfg.baud).toBe(65535);
  expect(sent.pop()).toEqual([IAC, SB, OPT_COM_PORT, 101, 0, 0, 0xff, 0xff, 0xff, 0xff, IAC, SE]);
});

test("parser survives frames split across reads", () => {
  const { s, state } = fake();
  const bytes = [0x61, ...sub(1, 0, 0, 0x25, 0x80), 0x62];
  const out: number[] = [];
  for (const b of bytes) out.push(...s.feed(Uint8Array.of(b)));
  expect(out).toEqual([0x61, 0x62]);
  expect(state.cfg.baud).toBe(9600);
});

test("NVT CR NUL is collapsed only when the client isn't in binary mode", () => {
  const a = fake();
  expect([...a.s.feed(Uint8Array.from([13, 0, 0x41]))]).toEqual([13, 0x41]);
  const b = fake();
  b.s.start();
  b.s.feed(Uint8Array.from([IAC, WILL, OPT_BINARY]));
  expect([...b.s.feed(Uint8Array.from([13, 0, 0x41]))]).toEqual([13, 0, 0x41]);
});

test("flow control suspend/resume", () => {
  const { s, state } = fake();
  s.feed(Uint8Array.from(sub(8)));
  expect(state.suspended).toBe(true);
  s.feed(Uint8Array.from(sub(9)));
  expect(state.suspended).toBe(false);
});
