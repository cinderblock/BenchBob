import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import * as W from "./w1-families.ts";
import { buildW1Fixture, FIX, romBytes, thermText } from "./w1-fixtures.ts";

describe("CRC + ROM", () => {
  test("Maxim CRC-8 of the classic DS18B20 scratchpad", () => {
    // Kernel docs example: 72 01 4b 46 7f ff 0e 10 : crc=57
    expect(W.crc8w1([0x72, 0x01, 0x4b, 0x46, 0x7f, 0xff, 0x0e, 0x10])).toBe(0x57);
  });
  test("ROM id bytes and CRC, including the ghost id seen on desky", () => {
    expect(W.romOk(romBytes(FIX.ds18b20))).toBe(true);
    // desky: /sys/bus/w1/devices/00-280000000000/id = 00 00 00 00 00 00 28 e1
    expect(romBytes("00-280000000000")).toEqual([0, 0, 0, 0, 0, 0, 0x28, 0xe1]);
    expect(W.romOk([0, 0, 0, 0, 0, 0, 0x28, 0xe0])).toBe(false);
  });
});

describe("w1_therm", () => {
  test("DS18B20 23.125 °C at 12 bit", () => {
    const ts = W.parseThermSlave(thermText([0x72, 0x01, 0x4b, 0x46, 0x7f, 0xff, 0x0e, 0x10], 23125))!;
    expect(ts.crcOk).toBe(true);
    const d = W.decodeTherm(0x28, ts);
    expect(d.values.find((v) => v.k === "t")!.v).toBe(23.125);
    expect(String(d.values.find((v) => v.k === "res")!.v)).toMatch(/^12-bit/);
    expect(d.warn).toEqual([]);
  });
  test("CRC NO → value withheld + warning", () => {
    const d = W.decodeTherm(0x28, W.parseThermSlave(thermText([0x72, 0x01, 0x4b, 0x46, 0x7f, 0xff, 0x0e, 0x10], 23125, true))!);
    expect(d.values.find((v) => v.k === "t")!.v).toBeNull();
    expect(d.warn![0]).toMatch(/CRC/);
  });
  test("85 °C power-on value is flagged", () => {
    const d = W.decodeTherm(0x28, W.parseThermSlave(thermText([0x50, 0x05, 0x4b, 0x46, 0x7f, 0xff, 0x0c, 0x10], 85000))!);
    expect(d.warn!.join()).toMatch(/power-on/);
  });
  test("DS18S20 extended resolution", () => {
    const d = W.decodeTherm(0x10, W.parseThermSlave(thermText([0x32, 0x00, 0x4b, 0x46, 0xff, 0xff, 0x0c, 0x10], 25000))!);
    expect(d.values.find((v) => v.k === "t")!.v).toBe(25);
    const d2 = W.decodeTherm(0x10, W.parseThermSlave(thermText([0x31, 0x00, 0x4b, 0x46, 0xff, 0xff, 0x05, 0x10], 0))!);
    expect(d2.values.find((v) => v.k === "t")!.v).toBeCloseTo(24 - 0.25 + 11 / 16, 3);
  });
  test("DS1822 negative, 10-bit", () => {
    const d = W.decodeTherm(0x22, W.parseThermSlave(thermText([0x58, 0xff, 0x4b, 0x46, 0x3f, 0xff, 0x08, 0x10], -10500))!);
    expect(d.values.find((v) => v.k === "t")!.v).toBe(-10.5);
    expect(String(d.values.find((v) => v.k === "res")!.v)).toMatch(/^10-bit/);
  });
  test("MAX31850 fault bits and cold junction", () => {
    const d = W.decodeTherm(0x3b, W.parseThermSlave(thermText([0x45, 0x06, 0x11, 0x18, 0xf3, 0xff, 0xff, 0xff], 100250))!);
    expect(d.values.find((v) => v.k === "t")!.v).toBeNull(); // faulted
    expect(d.values.find((v) => v.k === "cj")!.v).toBe(24.0625);
    expect(d.values.find((v) => v.k === "addr")!.v).toBe(3);
    expect(d.warn!.join()).toMatch(/open circuit/);
    const ok = W.decodeTherm(0x3b, W.parseThermSlave(thermText([0x44, 0x06, 0x10, 0x18, 0xf3, 0xff, 0xff, 0xff], 100250))!);
    expect(ok.values.find((v) => v.k === "t")!.v).toBe(100.25);
  });
});

describe("other families", () => {
  test("DS2413 state byte", () => {
    const d = W.decodeDs2413(0xc3);
    expect(d.pins).toEqual([
      { name: "PIO-A", level: 1, latch: 1, out: false },
      { name: "PIO-B", level: 0, latch: 0, out: true },
    ]);
    expect(d.warn).toEqual([]);
    expect(W.decodeDs2413(0x03).warn![0]).toMatch(/complement/);
  });
  test("DS2408 pins", () => {
    const d = W.decodeDs2408(0xf5, 0xfe, 0);
    expect(d.pins![0]).toEqual({ name: "P0", level: 1, latch: 0, out: true });
    expect(d.pins![1].level).toBe(0);
  });
  test("DS2406 ASCII state", () => {
    expect(W.decodeDs2406(0x31).pins!.map((p) => p.out)).toEqual([false, true]);
  });
  test("DS2423 counters A/B on pages 14/15", () => {
    const line = (c: number, ok = "YES") => `${"00 ".repeat(42)}crc=${ok} c=${c}`;
    const d = W.parseDs2423([line(1), line(2), line(12345), line(678, "NO")].join("\n"));
    expect(d.values.find((v) => v.label === "Counter A")!.v).toBe(12345);
    expect(d.values.find((v) => v.label === "Counter B")!.v).toBeNull();
    expect(d.warn![0]).toMatch(/Counter B/);
  });
  test("DS2438 scaling", () => {
    const d = W.decodeDs2438("6016", "312", "498", [0x0f, 0, 0, 0, 0, 0xcd, 0x00, 0], 0.05);
    expect(d.values.map((v) => v.v)).toEqual([23.5, 3.12, 4.98, 50.04, 1000.8]);
  });
  test("DS1904 clock", () => {
    const secs = Date.UTC(2026, 9, 17, 12) / 1000;
    const d = W.decodeRtc([0x0c, secs & 0xff, (secs >> 8) & 0xff, (secs >> 16) & 0xff, secs >>> 24]);
    expect(d.values[0].v).toBe("2026-10-17 12:00:00");
    expect(d.values[2].v).toBe("running");
  });
  test("family table covers the task list", () => {
    for (const f of [0x28, 0x10, 0x22, 0x42, 0x3b, 0x26, 0x29, 0x3a, 0x12, 0x1d, 0x2d, 0x23, 0x43, 0x01, 0x24, 0x27, 0x81]) expect(W.FAMILIES[f]).toBeDefined();
    expect(W.family(0x7f).name).toBe("Family 0x7F");
  });
});

// ---------- the real driver against the fixture tree ----------

describe("onewire driver on a fixture tree", () => {
  const dir = mkdtempSync(`${tmpdir()}/w1fx-`);
  let drv: any;
  const events: string[] = [];
  const store: Record<string, any> = {};
  const ctx: any = {
    id: "onewire",
    devices: () => [],
    ancestor: () => null,
    changed: () => {},
    event: (t: string) => events.push(t),
    settings: { get: (k: string, f: any) => (k in store ? store[k] : f), set: (k: string, v: any) => (store[k] = v) },
    spawn: () => {
      throw new Error("no spawn in tests");
    },
    log: () => {},
  };

  beforeAll(async () => {
    buildW1Fixture(dir);
    process.env.DESKYPI_W1_ROOT = dir;
    drv = (await import("../drivers/onewire.ts")).default;
    await drv.init(ctx);
    // Wait for the first background cycle to read every slave.
    for (let i = 0; i < 50 && drv.status().slaves.some((s: any) => !s.readAt && !s.error); i++) await Bun.sleep(100);
  });
  afterAll(async () => {
    await drv?.stop();
    rmSync(dir, { recursive: true, force: true });
  });

  const slave = (id: string) => drv.status().slaves.find((s: any) => s.id === id);
  const v = (id: string, k: string) => slave(id).values.find((x: any) => x.k === k)?.v;

  test("ghost family-00 ids are hidden and counted as noise", () => {
    const st = drv.status();
    expect(st.slaves.find((s: any) => s.id === FIX.ghost)).toBeUndefined();
    expect(st.masters[0].ghosts).toBe(1);
    expect(st.active).toBe(true);
    expect(st.summary).toMatch(/devices: 23\.1 °C/);
  });

  test("every family decodes", () => {
    expect(v(FIX.ds18b20, "t")).toBe(23.125);
    expect(slave(FIX.ds18b20por).warn.join()).toMatch(/power-on/);
    expect(v(FIX.ds18s20, "t")).toBe(25);
    expect(v(FIX.ds1822, "t")).toBe(-10.5);
    expect(v(FIX.max31850, "cj")).toBe(24.0625);
    expect(v(FIX.ds28ea00, "t")).toBe(21.5);
    expect(v(FIX.ds2438, "vdd")).toBe(4.98);
    expect(slave(FIX.ds2408).pins.length).toBe(8);
    expect(slave(FIX.ds2413).pins[1].out).toBe(true);
    expect(slave(FIX.ds2406).pins[0].out).toBe(false);
    expect(v(FIX.ds2423, "c14")).toBe(12345);
    expect(v(FIX.ds1904, "time")).toBe("2026-10-17 12:00:00");
    expect(slave(FIX.ibutton).name).toBe("DS2401 / DS1990A");
    expect(slave(FIX.ds1420).values[0].v).toMatch(/DS9490R/);
    expect(slave(FIX.unknown).name).toBe("EDS00xx");
    for (const s of drv.status().slaves) {
      expect(s.romOk).toBe(true);
      expect(s.error).toBe("");
    }
  });

  test("bulk conversion was triggered (several therm sensors on one master)", () => {
    expect(readFileSync(`${dir}/w1_bus_master1/therm_bulk_read`, "utf8")).toBe("trigger\n");
  });

  test("EEPROM read via REST route", async () => {
    const r = drv.routes.find((x: any) => x.path === "/api/onewire/:id/read");
    const out = await r.handler(new Request("http://x/"), { id: FIX.ds28ec20 }, new URL("http://x/?eeprom=1"));
    expect(out.eeprom.length).toBe(2560);
    expect(String.fromCharCode(...out.eeprom.slice(0, 16))).toBe("DS28EC20 fixture");
  });

  test("outputs need confirm, then write the latch byte", async () => {
    const r = drv.routes.find((x: any) => x.path === "/api/onewire/:id/output");
    await expect(r.handler(new Request("http://x/", { method: "POST", body: JSON.stringify({ pin: "A", on: true }) }), { id: FIX.ds2413 })).rejects.toThrow(/confirm/);
    await r.handler(new Request("http://x/", { method: "POST", body: JSON.stringify({ pin: "A", on: true, confirm: true }) }), { id: FIX.ds2413 });
    // A was off (latch 1), B on (latch 0): turning A on → bits 00 → 0xFC
    expect([...readFileSync(`${dir}/${FIX.ds2413}/output`)]).toEqual([0xfc]);
    await r.handler(new Request("http://x/", { method: "POST", body: JSON.stringify({ pin: 3, on: true, confirm: true }) }), { id: FIX.ds2408 });
    expect([...readFileSync(`${dir}/${FIX.ds2408}/output`)]).toEqual([0xf7]);
  });

  test("iButton touch + release land in the touch log", async () => {
    const list = `${dir}/w1_bus_master1/w1_master_slaves`;
    const all = readFileSync(list, "utf8");
    await Bun.write(list, all.replace(FIX.ibutton + "\n", ""));
    drv.udev({ action: "remove", props: { SUBSYSTEM: "w1" } });
    await Bun.sleep(200);
    await Bun.write(list, all);
    drv.udev({ action: "add", props: { SUBSYSTEM: "w1" } });
    await Bun.sleep(200);
    const log = drv.status().touchLog.filter((t: any) => t.id === FIX.ibutton).map((t: any) => t.event);
    expect(log).toEqual(["touch", "release"]); // newest first
    expect(events.some((e) => /touched/.test(e))).toBe(true);
  });

  test("search writes the current mode back (keeps continuous)", async () => {
    const t = drv.tools.find((x: any) => x.name === "onewire_search");
    await t.handler({});
    expect(readFileSync(`${dir}/w1_bus_master1/w1_master_search`, "utf8")).toBe("-1\n");
  });

  test("resolution change + label", async () => {
    const r = drv.routes.find((x: any) => x.path === "/api/onewire/:id/resolution");
    await r.handler(new Request("http://x/", { method: "PUT", body: JSON.stringify({ bits: 10 }) }), { id: FIX.ds18b20 });
    expect(readFileSync(`${dir}/${FIX.ds18b20}/resolution`, "utf8")).toBe("10\n");
    const l = drv.routes.find((x: any) => x.path === "/api/onewire/:id/label");
    await l.handler(new Request("http://x/", { method: "PUT", body: JSON.stringify({ label: "Freezer" }) }), { id: FIX.ds18b20 });
    expect(slave(FIX.ds18b20).label).toBe("Freezer");
  });
});
