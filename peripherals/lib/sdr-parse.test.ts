import { describe, expect, test } from "bun:test";
import { RTL_STICKS, Rtl433Aggregator, deviceKey, findBlacklist, parseRtlEeprom, parseRtlTest, rtl433Time, tcpPeers } from "./sdr-parse.ts";

const FIX = import.meta.dir + "/radios-fixtures";

describe("rtl_test -t", () => {
  test("R820T stick", () => {
    const i = parseRtlTest(`Found 1 device(s):
  0:  Realtek, RTL2838UHIDIR, SN: 00000001

Using device 0: Generic RTL2832U OEM
Detached kernel driver
Found Rafael Micro R820T tuner
Supported gain values (29): 0.0 0.9 1.4 2.7 3.7 7.7 8.7 12.5 14.4 15.7 16.6 19.7 20.7 22.9 25.4 28.0 29.7 32.8 33.8 36.4 37.2 38.6 40.2 42.1 43.4 43.9 44.5 48.0 49.6
[R82XX] PLL not locked!
Sampling at 2048000 S/s.
No E4000 tuner found, aborting.
Reattached kernel driver`);
    expect(i.devices).toEqual([{ index: 0, vendor: "Realtek", product: "RTL2838UHIDIR", serial: "00000001" }]);
    expect(i.tuner).toBe("Rafael Micro R820T");
    expect(i.gains.length).toBe(29);
    expect(i.gains.at(-1)).toBe(49.6);
    expect(i.detachedKernel).toBe(true);
    expect(i.error).toBe("");
  });
  test("RTL-SDR Blog V4", () => {
    const i = parseRtlTest(`Found 1 device(s):
  0:  RTLSDRBlog, Blog V4, SN: 00000001

Using device 0: Generic RTL2832U OEM
Found Rafael Micro R828D tuner
RTL-SDR Blog V4 Detected
Supported gain values (29): 0.0 0.9 1.4`);
    expect(i.tuner).toBe("Rafael Micro R828D (RTL-SDR Blog V4)");
    expect(i.blogV4).toBe(true);
  });
  test("E4000 and FC0013 tuners", () => {
    expect(parseRtlTest("Found Elonics E4000 tuner").tuner).toBe("Elonics E4000");
    expect(parseRtlTest("Found Fitipower FC0013 tuner").tuner).toBe("Fitipower FC0013");
  });
  test("claimed by the DVB driver", () => {
    const i = parseRtlTest(`Found 1 device(s):
  0:  Realtek, RTL2838UHIDIR, SN: 00000001

Using device 0: Generic RTL2832U OEM
usb_claim_interface error -6
Failed to open rtlsdr device #0.`);
    expect(i.claimed).toBe(true);
    expect(i.error).toContain("busy");
  });
  test("no device (real desky output)", () => {
    const i = parseRtlTest("No supported devices found.");
    expect(i.devices).toEqual([]);
    expect(i.error).toBe("No supported devices found.");
  });
});

test("rtl_eeprom dump", () => {
  const e = parseRtlEeprom(`Found 1 device(s):
  0:  Generic RTL2832U OEM

Using device 0: Generic RTL2832U OEM
Found Rafael Micro R820T tuner

Current configuration:
__________________________________________
Vendor ID:		0x0bda
Product ID:		0x2838
Manufacturer:		Realtek
Product:		RTL2838UHIDIR
Serial number:		00000001
Serial number enabled:	yes
IR endpoint enabled:	yes
Remote wakeup enabled:	no
__________________________________________`);
  expect(e).toMatchObject({ "Vendor ID": "0x0bda", Manufacturer: "Realtek", Product: "RTL2838UHIDIR", "Serial number": "00000001" });
});

test("stick table covers the usual ids", () => {
  expect(RTL_STICKS["0bda:2838"]).toBeTruthy();
  expect(RTL_STICKS["0bda:2832"]).toBeTruthy();
});

test("DVB blacklist detection", () => {
  expect(findBlacklist([{ path: "/etc/modprobe.d/blacklist-8192cu.conf", text: "blacklist 8192cu\n" }]).blacklisted).toBe(false);
  const r = findBlacklist([{ path: "/etc/modprobe.d/rtl.conf", text: "# rtl-sdr\nblacklist dvb_usb_rtl28xxu\nblacklist rtl2832\n" }]);
  expect(r).toEqual({ blacklisted: true, where: ["/etc/modprobe.d/rtl.conf: blacklist dvb_usb_rtl28xxu"] });
  expect(findBlacklist([{ path: "x", text: "install dvb-usb-rtl28xxu /bin/false" }]).blacklisted).toBe(true);
  expect(findBlacklist([{ path: "x", text: "#blacklist dvb_usb_rtl28xxu" }]).blacklisted).toBe(false);
});

describe("rtl_433 JSON aggregation", () => {
  test("fixture stream", async () => {
    const a = new Rtl433Aggregator();
    for (const l of (await Bun.file(`${FIX}/rtl433.jsonl`).text()).split("\n")) a.line(l, 1);
    expect(a.log.map((l) => l.line)).toContain("Tuned to 433.920MHz.");
    const list = a.list();
    expect(list.map((d) => d.key).sort()).toEqual(["Acurite-Tower/2483/A", "Acurite-Tower/9120/C", "Fineoffset-WH51/0d0b1c", "LaCrosse-TX141THBv2/192/0", "Toyota/TPMS/f1e2d3c4"]);
    const tower = a.devices.get("Acurite-Tower/2483/A")!;
    expect(tower.count).toBe(3);
    expect(tower.fields).toEqual({ battery_ok: 0, temperature_C: 22.5, humidity: 46 });
    expect(tower.hist.temperature_C.map(([, v]) => v)).toEqual([22.3, 22.4, 22.5]);
    expect(tower.hist.temperature_C[0][0]).toBe(1792226000123);
    expect(tower.protocol).toBe(40);
    expect(tower.rssi).toBe(-0.5);
    expect(tower.snr).toBe(22);
    const wh51 = a.devices.get("Fineoffset-WH51/0d0b1c")!;
    expect(wh51.fields.moisture).toBe(35);
    expect(wh51.hist.moisture.length).toBe(1);
    expect(a.devices.get("Toyota/TPMS/f1e2d3c4")!.fields.pressure_PSI).toBe(32.5);
    expect(a.messages.length).toBe(7);
    expect(a.messages.at(-1)!.seq).toBe(7);
    // Newest first.
    expect(list[0].key).toBe("Acurite-Tower/9120/C");
  });
  test("ring buffers are bounded", () => {
    const a = new Rtl433Aggregator(5, 3);
    for (let i = 0; i < 20; i++) a.line(JSON.stringify({ time: String(1e9 + i), model: "X", id: 1, temperature_C: i }));
    for (let i = 0; i < 10; i++) a.line(`log ${i}`);
    expect(a.messages.length).toBe(5);
    expect(a.log.length).toBe(3);
    expect(a.devices.get("X/1")!.count).toBe(20);
  });
  test("time formats and keys", () => {
    expect(rtl433Time("1792226000.5", 0)).toBe(1792226000500);
    expect(rtl433Time(undefined, 42)).toBe(42);
    expect(deviceKey({ model: "Nexus-TH", id: 129, channel: 1 })).toBe("Nexus-TH/129/1");
  });
});

test("/proc/net/tcp peers for rtl_tcp clients", () => {
  const v4 = `  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:04D2 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1 0000000000000000 100 0 0 10 0
   1: 2D07FF0A:04D2 0507FF0A:D431 01 00000000:00000000 00:00000000 00000000  1000        0 2 1 0000000000000000 20 4 30 10 -1`;
  expect(tcpPeers(v4, 1234)).toEqual(["10.255.7.5:54321"]);
  const v6 = `  sl  local_address                         remote_address                        st
   0: 0000000000000000FFFF00002D07FF0A:04D2 0000000000000000FFFF00000507FF0A:D431 01 0`;
  expect(tcpPeers(v6, 1234)).toEqual(["10.255.7.5:54321"]);
});
