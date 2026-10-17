import { describe, expect, test } from "bun:test";
import * as C from "./cansock.ts";

const raw16 = (id: number, data: number[]) => {
  const b = new Uint8Array(16);
  new DataView(b.buffer).setUint32(0, id >>> 0, true);
  b[4] = data.length;
  b.set(data, 8);
  return b;
};

describe("frame encode/decode", () => {
  test("classic std frame round trip", () => {
    const f = C.makeFrame({ id: 0x123, data: "DEADBEEF" });
    const b = C.encodeFrame(f);
    expect(b.length).toBe(16);
    expect([...b.slice(0, 8)]).toEqual([0x23, 0x01, 0, 0, 4, 0, 0, 0]);
    const [g] = C.splitFrames(b, 1000);
    expect(g).toMatchObject({ id: 0x123, ext: false, rtr: false, fd: false, len: 4 });
    expect(C.toHex(g.data)).toBe("DEADBEEF");
  });

  test("extended + RTR flags", () => {
    const f = C.makeFrame({ id: 0x18daf110, rtr: true, len: 3 });
    const b = C.encodeFrame(f);
    expect(new DataView(b.buffer).getUint32(0, true)).toBe((0x18daf110 | C.CAN_EFF_FLAG | C.CAN_RTR_FLAG) >>> 0);
    const [g] = C.splitFrames(b, 0);
    expect(g).toMatchObject({ id: 0x18daf110, ext: true, rtr: true, len: 3 });
    expect(g.data.length).toBe(0);
  });

  test("FD frame pads length and sets FDF|BRS", () => {
    const f = C.makeFrame({ id: 0x1234, fd: true, brs: true, data: "00112233445566778899" }); // 10 → 12
    expect(f.len).toBe(12);
    const b = C.encodeFrame(f);
    expect(b.length).toBe(72);
    expect(b[5]).toBe(C.CANFD_BRS | C.CANFD_FDF);
  });

  test("coalesced chunks as seen on desky: 16+16 and 16+72", () => {
    const a = raw16(0x100, [1]);
    const b = raw16(0x101, [2, 3]);
    expect(C.splitFrames(new Uint8Array([...a, ...b]), 0).map((f) => f.id)).toEqual([0x100, 0x101]);
    // The 88-byte chunk captured on desky: can_frame 0x123 [DEADBEEF], then canfd_frame 0x80001234 len 12 flags 5.
    const fd = new Uint8Array(72);
    new DataView(fd.buffer).setUint32(0, 0x80001234, true);
    fd[4] = 12;
    fd[5] = 5;
    const fs = C.splitFrames(new Uint8Array([...raw16(0x123, [0xde, 0xad, 0xbe, 0xef]), ...fd]), 0);
    expect(fs.length).toBe(2);
    expect(fs[1]).toMatchObject({ id: 0x1234, ext: true, fd: true, brs: true, len: 12 });
  });

  test("error frame", () => {
    const b = raw16(C.CAN_ERR_FLAG | 0x004 | 0x200, [0, 0x10, 0, 0, 0, 0, 0x80, 0x7f]);
    const [f] = C.splitFrames(b, 0);
    expect(f.err).toBe(true);
    expect(f.id).toBe(0x204);
    const d = C.decodeError(f);
    expect(d.text).toContain("RX passive");
    expect(d.tec).toBe(0x80);
    expect(d.rec).toBe(0x7f);
  });

  test("decodeError protocol violation with location", () => {
    const f = { ...C.makeFrame({ id: 0 }), err: true, id: 0x088, data: Uint8Array.from([0, 0, 0x04, 0x0a, 0, 0, 0, 0]) };
    expect(C.decodeError(f).text).toBe("bus error; protocol: stuff error at data");
  });

  test("validation", () => {
    expect(() => C.makeFrame({ id: 0x800 + 0x1fffffff })).toThrow();
    expect(() => C.makeFrame({ id: 1, data: "001122334455667788" })).toThrow(/at most 8/);
    expect(() => C.makeFrame({ id: 1, fd: true, rtr: true })).toThrow();
    expect(C.makeFrame({ id: 0x800 }).ext).toBe(true);
    expect(() => C.parseHex("abc")).toThrow(/Odd/);
    expect(C.toHex(C.parseHex("de ad:BE,ef"))).toBe("DEADBEEF");
  });
});

describe("candump / cansend formats", () => {
  test("candump -L lines match can-utils output", () => {
    const f = C.makeFrame({ id: 0x123, fd: true, brs: true, data: "DEADBEEF" });
    f.ts = 1792225996360.433;
    // Exactly what `candump -L` printed on desky for `cansend vcan0 123##1DEADBEEF`.
    expect(C.candumpLine(f, "vcan0")).toBe("(1792225996.360433) vcan0 123##5DEADBEEF");
    expect(C.candumpBody(C.makeFrame({ id: 0x12345678, rtr: true }))).toBe("12345678#R");
    expect(C.candumpBody(C.makeFrame({ id: 0x123, rtr: true, len: 3 }))).toBe("123#R3");
    expect(C.candumpBody(C.makeFrame({ id: 0x7ff, data: "" }))).toBe("7FF#");
  });

  test("cansend parsing", () => {
    expect(C.parseCansend("123#DEADBEEF")).toMatchObject({ id: 0x123, ext: false });
    expect(C.parseCansend("00000123#11")).toMatchObject({ id: 0x123, ext: true });
    expect(C.parseCansend("123#R4")).toMatchObject({ rtr: true, len: 4 });
    expect(C.parseCansend("123##1DEAD")).toMatchObject({ fd: true, brs: true });
    expect(() => C.parseCansend("garbage")).toThrow();
  });

  test("candump line parse round trip", () => {
    for (const l of ["(1700000000.000001) can0 123#DEADBEEF", "(1700000000.500000) can0 18DAF110#R", "(1700000000.250000) vcan0 123##5DEADBEEF", "(1700000001.000000) can0 20000080#0000000000000000"]) {
      const p = C.parseCandumpLine(l)!;
      expect(C.candumpLine(p.frame, p.iface)).toBe(l);
    }
  });
});

describe("bus load bits", () => {
  test("classic worst case matches the textbook figures", () => {
    // 8-byte std frame: 111 bits + 24 stuff bits = 135; 8-byte ext: 131 + 29 = 160 (incl. 3-bit IFS).
    expect(C.frameBits({ ext: false, rtr: false, fd: false, brs: false, len: 8 }).nom).toBe(135);
    expect(C.frameBits({ ext: true, rtr: false, fd: false, brs: false, len: 8 }).nom).toBe(160);
    expect(C.frameBits({ ext: false, rtr: false, fd: false, brs: false, len: 0 }).nom).toBe(47 + 8);
  });
  test("FD with BRS splits phases", () => {
    const b = C.frameBits({ ext: false, rtr: false, fd: true, brs: true, len: 64 });
    expect(b.nom).toBeGreaterThan(25);
    expect(b.data).toBeGreaterThan(64 * 8);
    expect(C.frameTime({ ext: false, rtr: false, fd: true, brs: true, len: 64 }, 500000, 2000000)).toBeCloseTo(b.nom / 500000 + b.data / 2000000);
  });
});

describe("filters", () => {
  test("string syntax", () => {
    const f = C.parseFilterString("123, 7E0:7F8 x18DAF100:1FFFFF00")!;
    expect(f).toEqual([{ id: 0x123, mask: undefined, ext: undefined }, { id: 0x7e0, mask: 0x7f8, ext: undefined }, { id: 0x18daf100, mask: 0x1fffff00, ext: true }]);
    const m = (id: number, ext = false) => C.matchFilters({ id, ext, err: false }, f);
    expect(m(0x123)).toBe(true);
    expect(m(0x7e8)).toBe(false); // 0x7E8 & 0x7F8 = 0x7E8 ≠ 0x7E0
    expect(m(0x7e7)).toBe(true);
    expect(m(0x18daf1aa, true)).toBe(true);
    expect(m(0x18daf1aa, false)).toBe(false);
    expect(C.matchFilters({ id: 4, ext: false, err: true }, f)).toBe(false);
    expect(C.matchFilters({ id: 4, ext: false, err: true }, null)).toBe(true);
    expect(() => C.parseFilterString("zz")).toThrow();
  });
  test("JSON frame specs", () => {
    expect(C.specFromJson({ id: "0x7DF", data: "0201" })).toMatchObject({ id: 0x7df });
    expect(C.specFromJson({ id: 291 })).toMatchObject({ id: 291 });
    expect(C.specFromJson({ cansend: "1#00" })).toMatchObject({ id: 1 });
    expect(() => C.specFromJson({})).toThrow();
  });
});

describe("auto-bitrate decision", () => {
  const t = (bitrate: number, frames: number, errors: number, busErrors = 0) => ({ bitrate, frames, errors, busErrors });
  test("clear winner among error-ridden wrong rates", () => {
    const v = C.pickBitrate([t(1000000, 0, 40, 200), t(800000, 0, 35, 150), t(500000, 120, 0), t(250000, 1, 30, 90), t(125000, 0, 20, 40)]);
    expect(v).toMatchObject({ result: "found", bitrate: 500000, confidence: "high" });
  });
  test("a fluke frame at a wrong rate doesn't win", () => {
    const v = C.pickBitrate([t(500000, 2, 50), t(250000, 0, 0)]);
    expect(v.result).toBe("none");
  });
  test("few frames → low confidence", () => {
    expect(C.pickBitrate([t(125000, 1, 0), t(250000, 0, 5)])).toMatchObject({ bitrate: 125000, confidence: "low" });
  });
  test("silent bus", () => {
    expect(C.pickBitrate(C.STD_BITRATES.map((b) => t(b, 0, 0))).result).toBe("silent");
  });
});

describe("socketcand protocol", () => {
  test("element framing like socketcand's receive_command", () => {
    expect(C.scTake("junk< open can0 >< rawmode >< send 12")).toEqual({ cmds: ["< open can0 >", "< rawmode >"], rest: "< send 12" });
    expect(C.scWords("< send 123 2 a 0B >")).toEqual(["send", "123", "2", "a", "0B"]);
  });
  test("raw-mode frame lines are what python-can parses", () => {
    const f = C.makeFrame({ id: 0x123, data: "DEADBEEF" });
    f.ts = 1700000000123.456;
    expect(C.scFrameRaw(f)).toBe("< frame 123 1700000000.123456 DEADBEEF >");
    const e = C.makeFrame({ id: 0x1abcdef, ext: true, data: "" });
    e.ts = 1700000000000;
    expect(C.scFrameRaw(e)).toBe("< frame 01ABCDEF 1700000000.000000  >");
    expect(C.scFrameBcm(f)).toBe("< frame 123 1700000000.123456 DE AD BE EF  >");
  });
  test("send parsing: python-can and SavvyCAN styles", () => {
    expect(C.scParseIdLenData(["123", "2", "de", "a"])).toMatchObject({ id: 0x123, ext: false, len: 2 });
    expect(C.toHex(C.scParseIdLenData(["123", "2", "de", "a"]).data)).toBe("DE0A");
    expect(C.scParseIdLenData(["00000123", "0"])).toMatchObject({ id: 0x123, ext: true });
    expect(C.scParseIdLenData(["80000123", "1", "ff"])).toMatchObject({ id: 0x123, ext: true }); // SavvyCAN sets bit 31
    expect(() => C.scParseIdLenData(["123", "3", "00"])).toThrow();
  });
  test("beacon XML", () => {
    expect(C.beaconXml("deskypi", "10.0.0.2", 29536, ["can0", "vcan0"])).toBe('<CANBeacon name="deskypi" type="SocketCAN" description="DeskyPi socketcand">\n<URL>can://10.0.0.2:29536</URL><Bus name="can0"/><Bus name="vcan0"/></CANBeacon>');
  });
});
