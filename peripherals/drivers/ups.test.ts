import { expect, test } from "bun:test";
import { decodeStatus, fmtRuntime, isPowerDeviceDescriptor, matchUps, parseNutUdevRules, parseUpsc, statusEvents, upsConf, upsdConf } from "../lib/ups.ts";

// Excerpt of desky's /lib/udev/rules.d/62-nut-usbups.rules (NUT 2.8.1).
const rules = `# This file is generated and installed by the Network UPS Tools package.

ACTION=="remove", GOTO="nut-usbups_rules_end"
SUBSYSTEM=="usb_device", GOTO="nut-usbups_rules_real"

LABEL="nut-usbups_rules_real"
#  SNR-UPS-LID-XXXX UPSes  - nutdrv_qx
ATTR{idVendor}=="0001", ATTR{idProduct}=="0000", MODE="664", GROUP="nut"

#  various models  - usbhid-ups
ATTR{idVendor}=="051d", ATTR{idProduct}=="0002", MODE="664", GROUP="nut"
#  USB IDs device table  - apc_modbus
ATTR{idVendor}=="051d", ATTR{idProduct}=="0003", MODE="664", GROUP="nut"
#  Dynex DX-800U?, CP1200AVR/BC1200D, CP825AVR-G, CP1000AVRLCD, CP1000PFCLCD, CP1500C, CP550HG, etc.  - usbhid-ups
ATTR{idVendor}=="0764", ATTR{idProduct}=="0501", MODE="664", GROUP="nut"
#  e.g. OMNIVS1000, SMART550USB, ...  - tripplite_usb
ATTR{idVendor}=="09ae", ATTR{idProduct}=="0001", MODE="664", GROUP="nut"
`;

test("NUT udev table parsing", () => {
  const t = parseNutUdevRules(rules);
  expect(t.length).toBe(5);
  expect(t[0]).toEqual({ vid: "0001", pid: "0000", driver: "nutdrv_qx", models: "SNR-UPS-LID-XXXX UPSes" });
  expect(t[2].driver).toBe("apc_modbus");
  expect(t[3].models).toContain("CP1200AVR");
});

test("UPS matching: exact NUT entry, then vendor-wide fallbacks", () => {
  const t = parseNutUdevRules(rules);
  expect(matchUps("09AE", "0001", t)?.driver).toBe("tripplite_usb");
  expect(matchUps("09ae", "3016", t)?.models).toBe("Tripp Lite"); // not in table → vendor wildcard
  expect(matchUps("10af", "0002", [])?.driver).toBe("usbhid-ups");
  expect(matchUps("0764", "0601", [])?.models).toBe("CyberPower");
  expect(matchUps("046d", "c52b", t)).toBeNull();
});

test("HID Power Device detection from report descriptors", () => {
  // Start of a CyberPower descriptor: Usage Page (Power Device), Usage (UPS), Collection…
  expect(isPowerDeviceDescriptor(new Uint8Array([0x05, 0x84, 0x09, 0x04, 0xa1, 0x01, 0x09, 0x24, 0xa1, 0x00]))).toBe(true);
  // Battery System page via a 2-byte usage page item, after a generic-desktop collection.
  expect(isPowerDeviceDescriptor(new Uint8Array([0x05, 0x01, 0x09, 0x00, 0xa1, 0x01, 0x06, 0x85, 0x00, 0x09, 0x2c]))).toBe(true);
  // Keyboard: Generic Desktop + Keyboard page; 0x84 appears only as data, not as a usage page.
  expect(isPowerDeviceDescriptor(new Uint8Array([0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0x84, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01]))).toBe(false);
});

test("ups.conf / upsd.conf generation", () => {
  const c = upsConf([
    { name: "deskypi", driver: "usbhid-ups", port: "auto", vid: "0764", pid: "0501", serial: "CXXKZ200", desc: "CyberPower CP1500" },
    { name: "deskypi-2", driver: "usbhid-ups", port: "auto", vid: "0764", pid: "0501", bus: "001" },
  ]);
  expect(c).toContain('[deskypi]\n  driver = usbhid-ups\n  port = "auto"\n  vendorid = 0764\n  productid = 0501\n  serial = "CXXKZ200"\n  desc = "CyberPower CP1500"\n');
  expect(c).toContain('[deskypi-2]');
  expect(c).toContain('  bus = "001"');
  expect(upsdConf(3493, false)).toContain("LISTEN 127.0.0.1 3493");
  expect(upsdConf(3493, true)).toContain("LISTEN 0.0.0.0 3493");
});

// Real `upsc deskypi@127.0.0.1:13493` output from the dummy-ups run on desky.
const upsc = `battery.charge: 87
battery.runtime: 1800
device.mfr: SimCorp
device.model: Sim 1500
device.type: ups
driver.name: dummy-ups
driver.parameter.port: /tmp/nutx.zEhg/sim.dev
driver.version: 2.8.1
input.voltage: 121.0
ups.load: 23
ups.mfr: SimCorp
ups.model: Sim 1500
ups.status: OL CHRG`;

test("upsc parsing and status decoding", () => {
  const v = parseUpsc("Init SSL without certificate database\n" + upsc);
  expect(v["battery.charge"]).toBe("87");
  expect(v["ups.status"]).toBe("OL CHRG");
  expect(v["driver.parameter.port"]).toBe("/tmp/nutx.zEhg/sim.dev");
  expect(Object.keys(v)).not.toContain("Init SSL without certificate database");
  expect(decodeStatus("OB DISCHRG LB")).toEqual([
    { flag: "OB", word: "On battery", severity: "warn" },
    { flag: "DISCHRG", word: "Discharging", severity: "info" },
    { flag: "LB", word: "Low battery", severity: "bad" },
  ]);
});

test("power events from status transitions", () => {
  expect(statusEvents("OL CHRG", "OB DISCHRG")).toEqual(["Power lost: running on battery"]);
  expect(statusEvents("OB DISCHRG", "OB DISCHRG LB")).toEqual(["Battery low"]);
  expect(statusEvents("OB DISCHRG LB", "OL CHRG")).toEqual(["Power restored: back on mains", "Battery no longer low"]);
  expect(statusEvents("OL", "OL RB")).toEqual(["UPS says: replace battery"]);
  expect(statusEvents("", "OL")).toEqual([]); // first sample is not an event
});

test("runtime formatting", () => {
  expect(fmtRuntime(45)).toBe("45 s");
  expect(fmtRuntime(1800)).toBe("30 min");
  expect(fmtRuntime(5000)).toBe("1 h 23 min");
});
