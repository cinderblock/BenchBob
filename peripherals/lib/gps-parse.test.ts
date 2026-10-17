import { describe, expect, test } from "bun:test";
import { classifyUsb, constellationOf, ingest, newState, nmea, nmeaChecksum, summarize } from "./gps-parse.ts";

const FIX = import.meta.dir + "/radios-fixtures";

describe("gpsd JSON (captured from gpsd 3.25 via gpsfake on desky)", () => {
  test("TPV/SKY/DEVICE fold into a 3D multi-constellation fix", async () => {
    const lines = (await Bun.file(`${FIX}/gpsd-watch.jsonl`).text()).trim().split("\n");
    const s = newState();
    let t = 1_000_000;
    for (const l of lines) ingest(s, JSON.parse(l), (t += 100));
    expect(s.version).toMatch(/^3\.25/);
    expect(s.devices[0]).toMatchObject({ path: "/dev/pts/2", driver: "NMEA0183", bps: 4800 });
    const v = summarize(s, t);
    expect(v.fix).toBe("3D");
    expect(v.lat).toBeCloseTo(37.3861, 3);
    expect(v.lon).toBeCloseTo(-122.0839, 3);
    expect(v.altMSL_m).toBe(32.5);
    expect(v.track_deg).toBe(87.5);
    expect(v.hdop).toBe(0.9);
    expect(v.pdop).toBe(1.6);
    // The fixture has 9 GPS + 4 GLONASS + 3 Galileo + 3 BeiDou, 14 used.
    expect(v.satsVisible).toBe(19);
    expect(v.satsUsed).toBe(14);
    expect(v.constellations).toEqual({ BeiDou: { visible: 3, used: 2 }, GLONASS: { visible: 4, used: 3 }, GPS: { visible: 9, used: 7 }, Galileo: { visible: 3, used: 2 } });
    const s12 = s.sats.find((x) => x.gnss === "GPS" && x.svid === 12)!;
    expect(s12).toMatchObject({ el: 62, az: 45, snr: 42, used: true });
    expect(v.time).toMatch(/^2026-10-17T08:30/);
  });

  test("partial SKY reports merge; stale satellites age out", () => {
    const s = newState();
    ingest(s, { class: "SKY", satellites: [{ PRN: 2, gnssid: 0, svid: 2, el: 10, az: 20, ss: 30, used: true }] }, 0);
    ingest(s, { class: "SKY", satellites: [{ PRN: 65, gnssid: 6, svid: 1, el: 10, az: 20, ss: 30, used: false }] }, 500);
    expect(s.sats.length).toBe(2);
    ingest(s, { class: "SKY", hdop: 1.1 }, 600); // DOP-only report keeps satellites
    expect(s.sats.length).toBe(2);
    expect(summarize(s, 600).hdop).toBe(1.1);
    ingest(s, { class: "SKY", satellites: [{ PRN: 65, gnssid: 6, svid: 1, el: 11, az: 20, ss: 31, used: true }] }, 10_400);
    expect(s.sats.map((x) => x.gnss)).toEqual(["GLONASS"]);
  });

  test("mode-only TPV drops the fix but keeps the last position", () => {
    const s = newState();
    ingest(s, { class: "TPV", mode: 3, lat: 1, lon: 2, time: "2026-10-17T00:00:00.000Z" }, 1000);
    ingest(s, { class: "TPV", mode: 1 }, 2000);
    const v = summarize(s, 2000);
    expect(v.fix).toBe("no fix");
    expect(v.lat).toBe(1);
  });

  test("PPS / TOFF offsets and NMEA offset", () => {
    const s = newState();
    ingest(s, { class: "PPS", device: "/dev/pps0", real_sec: 100, real_nsec: 0, clock_sec: 99, clock_nsec: 999_990_000, precision: -20 }, 5000);
    ingest(s, { class: "TOFF", device: "/dev/ttyACM0", real_sec: 100, real_nsec: 0, clock_sec: 100, clock_nsec: 120_000_000 }, 5000);
    ingest(s, { class: "TPV", mode: 3, time: new Date(4800).toISOString() }, 5000);
    const v = summarize(s, 6000);
    expect(v.ppsOffset_us).toBe(10);
    expect(v.toffOffset_ms).toBe(-120);
    expect(v.nmeaOffset_ms).toBe(-200);
    expect(v.ppsAge_s).toBe(1);
  });
});

test("constellation from gnssid or legacy PRN ranges", () => {
  expect(constellationOf({ gnssid: 2, PRN: 3 })).toBe("Galileo");
  expect(constellationOf({ PRN: 70 })).toBe("GLONASS");
  expect(constellationOf({ PRN: 305 })).toBe("Galileo");
  expect(constellationOf({ PRN: 133 })).toBe("SBAS");
});

test("USB classification", () => {
  expect(classifyUsb("1546:01a8").kind).toBe("gps");
  expect(classifyUsb("1546:0502").kind).toBe("gps");
  expect(classifyUsb("067b:2303").kind).toBe("maybe");
  expect(classifyUsb("2109:3431").kind).toBe("unknown");
});

test("NMEA checksums match the fixture", async () => {
  expect(nmeaChecksum("GPGSV,3,3,09,29,35,240,33")).toBe("4B");
  const lines = (await Bun.file(`${FIX}/gps-multi.nmea`).text()).trim().split(/\r?\n/);
  for (const l of lines) {
    const m = /^\$(.*)\*([0-9A-F]{2})$/.exec(l)!;
    expect(nmea(m[1])).toBe(l);
  }
});
