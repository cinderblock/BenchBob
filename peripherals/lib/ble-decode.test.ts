import { describe, expect, test } from "bun:test";
import {
  BleTracker, BtsnoopReader, MON, addrKind, decodeApple, decodeAtc, decodeBTHome, decodeBeacons, decodeEddystone, decodeGovee,
  decodeIBeacon, decodeMiBeacon, decodeMicrosoft, decodeRuuvi, deviceKind, fromHex, parseAd, parseHciEvent, parseNewIndex,
} from "./ble-decode.ts";

const FIX = import.meta.dir + "/radios-fixtures";

// The capture is real over-the-air traffic, so it carries neighbours' device addresses
// and names. It stays out of the (public) repo: gitignored, and this test skips without it.
const CAPTURE = `${FIX}/ble-scan.btsnoop`;
const haveCapture = await Bun.file(CAPTURE).exists();

describe("btsnoop + HCI (real capture from desky, 10 s scan)", () => {
  test.skipIf(!haveCapture)("parses the whole capture, chunked arbitrarily", async () => {
    const bytes = new Uint8Array(await Bun.file(CAPTURE).arrayBuffer());
    const r = new BtsnoopReader();
    const recs = [];
    for (let i = 0; i < bytes.length; i += 777) recs.push(...r.push(bytes.slice(i, i + 777)));
    expect(r.datalink).toBe(2001);
    const ni = recs.find((x) => x.opcode === MON.NEW_INDEX)!;
    expect(parseNewIndex(ni.data)).toEqual({ addr: "DC:A6:32:03:FC:38", name: "hci0" });
    const reports = recs.filter((x) => x.opcode === MON.EVENT).flatMap((x) => parseHciEvent(x.data, x.ts, x.index));
    // btmon -r on the same file counts 289 ADV_NONCONN_IND + 255 ADV_IND + 244 SCAN_RSP.
    expect(reports.length).toBe(788);
    // Timestamps are real wall-clock times (2026).
    expect(new Date(reports[0].ts).getUTCFullYear()).toBeGreaterThanOrEqual(2025);
    expect(reports.every((x) => x.rssi === null || (x.rssi < 0 && x.rssi > -128))).toBe(true);

    const t = new BleTracker();
    for (const x of reports) t.ingest(x);
    const list = t.list();
    expect(list.length).toBeGreaterThan(10);
    const companies = new Set(list.flatMap((d) => d.mfr.map((m) => m.companyName)));
    for (const c of ["Microsoft", "Apple", "Tuya"]) expect(companies.has(c)).toBe(true);
    expect(list.some((d) => d.connectable)).toBe(true);
    expect(list.some((d) => d.scanRsp)).toBe(true);
    expect(list.some((d) => d.beacons.some((b) => b.kind === "ms-cdp"))).toBe(true);
    expect(list.some((d) => d.beacons.some((b) => b.kind === "apple"))).toBe(true);
    // RSSI history is bucketed per second, so ≤ ~11 slots for a 10 s capture.
    expect(Math.max(...list.map((d) => d.rssiHist.length))).toBeLessThanOrEqual(12);
    for (const d of list) expect(typeof deviceKind(d)).toBe("string");
  });
});

describe("HCI event parsing", () => {
  test("legacy report: ADV_IND, random static address, RSSI", () => {
    // 3E len 02 num=1 type=0 addrType=1 addr(LE) dataLen=3 data rssi
    const ev = fromHex("3e 0f 02 01 00 01 66 55 44 33 22 c1 03 02 01 06 b5");
    const [r] = parseHciEvent(ev);
    expect(r.addr).toBe("C1:22:33:44:55:66");
    expect(r.pdu).toBe("ADV_IND");
    expect(r.connectable).toBe(true);
    expect(r.rssi).toBe(-75);
    expect(addrKind(r.addr, r.addrType)).toBe("random static");
    expect(parseAd(r.data).flags).toBe(6);
  });
  test("extended report with TX power", () => {
    // et=0x0013 (connectable, scannable, legacy)… addr, phys, sid, tx=-8, rssi=-60, interval, direct, len 0
    const ev = fromHex("3e 1a 0d 01 13 00 00 01 02 03 04 05 06 01 00 ff f8 c4 00 00 00 00 00 00 00 00 00 00 00");
    const [r] = parseHciEvent(ev);
    expect(r.addr).toBe("06:05:04:03:02:01");
    expect(r.pdu).toBe("ADV_IND");
    expect(r.txPower).toBe(-8);
    expect(r.rssi).toBe(-60);
  });
  test("address kinds", () => {
    expect(addrKind("4A:00:00:00:00:00", 1)).toBe("random resolvable");
    expect(addrKind("0A:00:00:00:00:00", 1)).toBe("random non-resolvable");
    expect(addrKind("DC:A6:32:03:FC:38", 0)).toBe("public");
  });
});

describe("AD structures", () => {
  test("name, uuids, tx power, service + manufacturer data", () => {
    const ad = parseAd(fromHex("020106 0509 54657374 0303 0F18 020A F4 0716 AAFE 10E70001 05FF 4C00 1002"));
    expect(ad.name).toBe("Test");
    expect(ad.uuids).toEqual(["180F"]);
    expect(ad.txPower).toBe(-12);
    expect(ad.svc[0].uuid).toBe("FEAA");
    expect(ad.mfr[0].company).toBe(0x004c);
  });
});

describe("beacon decoders (published example payloads)", () => {
  test("iBeacon", () => {
    const b = decodeIBeacon(0x004c, fromHex("0215 E2C56DB5DFFB48D2B060D0F5A71096E0 0001 0002 C5"))!;
    expect(b.fields).toEqual({ uuid: "E2C56DB5-DFFB-48D2-B060-D0F5A71096E0", major: 1, minor: 2, measuredPower_dBm: -59 });
  });
  test("Eddystone UID / URL / TLM", () => {
    const uid = decodeEddystone("FEAA", fromHex("00E7 00112233445566778899 AABBCCDDEEFF 0000"))!;
    expect(uid.fields).toEqual({ txPower0m_dBm: -25, namespace: "00112233445566778899", instance: "AABBCCDDEEFF" });
    expect(decodeEddystone("FEAA", fromHex("10E7 00 676f6f676c65 07"))!.fields!.url).toBe("http://www.google.com");
    expect(decodeEddystone("FEAA", fromHex("10E7 03 6578616d706c65 00 6162"))!.fields!.url).toBe("https://example.com/ab");
    const tlm = decodeEddystone("FEAA", fromHex("2000 0BB8 1980 00000064 00000E10"))!;
    expect(tlm.fields).toEqual({ battery_mV: 3000, temperature_C: 25.5, advCount: 100, uptime_s: 360 });
  });
  test("BTHome v2 (spec examples)", () => {
    expect(decodeBTHome("FCD2", fromHex("40 02CA09 03BF13"))!.fields).toEqual({ temperature_C: 25.06, humidity: 50.55 });
    const btn = decodeBTHome("FCD2", fromHex("44 0161 3A01"))!;
    expect(btn.fields).toEqual({ battery: 97, button: "press", trigger_based: true });
    expect(decodeBTHome("FCD2", fromHex("40 02CA09 02F6FF"))!.fields).toEqual({ temperature_C: 25.06, temperature_C_2: -0.1 });
    expect(decodeBTHome("FCD2", fromHex("41 0102"))!.fields).toEqual({ encrypted: true });
    expect(decodeBTHome("FCD2", fromHex("40 5304 74657374"))!.fields).toEqual({ text: "test" });
  });
  test("RuuviTag RAWv2 (format 5 spec example)", () => {
    const b = decodeRuuvi(0x0499, fromHex("0512FC5394C37C0004FFFC040CAC364200CDCBB8334C884F"))!;
    expect(b.fields).toEqual({
      temperature_C: 24.3, humidity: 53.49, pressure_hPa: 1000.44, accelX_mg: 4, accelY_mg: -4, accelZ_mg: 1036,
      battery_mV: 2977, txPower_dBm: 4, movements: 66, sequence: 205, mac: "CB:B8:33:4C:88:4F",
    });
  });
  test("Govee H5075 positive and negative", () => {
    expect(decodeGovee(0xec88, fromHex("000351B46400"), "GVH5075_1A2B")!.fields).toEqual({ temperature_C: 21.7, humidity: 52.4, battery: 100 });
    const neg = decodeGovee(0xec88, fromHex("0080C6703700"))!;
    expect(neg.fields).toEqual({ temperature_C: -5, humidity: 80, battery: 55 });
    expect(decodeGovee(0xec88, fromHex("000351B46400"), "GVH5075_1A2B")!.label).toContain("GVH5075");
  });
  test("ATC1441 and pvvx custom formats", () => {
    expect(decodeAtc("181A", fromHex("A4C138AABBCC 00E1 2D 5A 0B86 07"))!.fields).toEqual({ mac: "A4:C1:38:AA:BB:CC", temperature_C: 22.5, humidity: 45, battery: 90, battery_mV: 2950, frameCounter: 7 });
    const p = decodeAtc("181A", fromHex("CCBBAA38C1A4 CA08 A011 860B 5A 07 05"))!;
    expect(p.kind).toBe("pvvx");
    expect(p.fields).toEqual({ mac: "A4:C1:38:AA:BB:CC", temperature_C: 22.5, humidity: 45.12, battery_mV: 2950, battery: 90, frameCounter: 7, flags: 5 });
  });
  test("Xiaomi MiBeacon", () => {
    const b = decodeMiBeacon("FE95", fromHex("5050 5B05 01 CCBBAA38C1A4 0D10 04 E100 C201"))!;
    expect(b.fields).toEqual({ product: "LYWSD03MMC", frameCounter: 1, mac: "A4:C1:38:AA:BB:CC", temperature_C: 22.5, humidity: 45 });
    expect(decodeMiBeacon("FE95", fromHex("5050 5B05 02 CCBBAA38C1A4 0A10 01 64"))!.fields!.battery).toBe(100);
    expect(decodeMiBeacon("FE95", fromHex("5858 5B05 01 0102030405060708"))!.fields!.encrypted).toBe(true);
  });
  test("Apple Continuity labels", () => {
    const sep = decodeApple(0x004c, fromHex("1219" + "10" + "00".repeat(24)))!;
    expect(sep.fields!.findMySeparated).toBe(true);
    expect(sep.label).toContain("separated");
    expect(decodeApple(0x004c, fromHex("12022400"))!.fields!.findMySeparated).toBe(false);
    expect(decodeApple(0x004c, fromHex("0719010E20" + "00".repeat(22)))!.fields!.audioModel).toBe("AirPods Pro");
    expect(decodeApple(0x004c, fromHex("10050B1C4E5F38 0C0E00" + "00".repeat(14)))!.fields!.messages).toEqual(["Nearby Info", "Handoff"]);
    // iBeacon payloads are left to the iBeacon decoder.
    expect(decodeApple(0x004c, fromHex("0215E2C56DB5DFFB48D2B060D0F5A71096E000010002C5"))).toBeNull();
  });
  test("Microsoft Swift Pair and CDP", () => {
    const sp = decodeMicrosoft(0x0006, new Uint8Array([0x03, 0x00, 0x80, ...new TextEncoder().encode("Surface Mouse")]))!;
    expect(sp.fields).toEqual({ scenario: "LE", displayName: "Surface Mouse" });
    expect(decodeMicrosoft(0x0006, fromHex("010920"))!.fields!.deviceType).toBe("Windows desktop");
  });
  test("decodeBeacons dispatches across a full advertisement", () => {
    const ad = parseAd(fromHex("020106 0C16 D2FC 40 02CA09 03BF13 0161"));
    const [b] = decodeBeacons(ad);
    expect(b.kind).toBe("bthome");
    expect(b.fields).toEqual({ temperature_C: 25.06, humidity: 50.55, battery: 97 });
  });
});
