import { describe, expect, test } from "bun:test";
import { ipArgs, normCfg, parseIpLink } from "./can.ts";

// Shaped like `ip -details -statistics -json link show` for a gs_usb FD adapter (key names
// from iproute2 6.15 ip/iplink_can.c) and the real vcan0 output captured on desky.
const GS_USB = {
  ifname: "can0",
  flags: ["NOARP", "UP", "LOWER_UP", "ECHO"],
  mtu: 72,
  operstate: "UP",
  link_type: "can",
  linkinfo: {
    info_kind: "can",
    info_data: {
      ctrlmode: ["FD", "BERR-REPORTING"],
      ctrlmode_supported: ["LOOPBACK", "LISTEN-ONLY", "TRIPLE-SAMPLING", "ONE-SHOT", "BERR-REPORTING", "FD", "CC-LEN8-DLC"],
      state: "ERROR-PASSIVE",
      berr_counter: { tx: 128, rx: 3 },
      restart_ms: 100,
      bittiming: { bitrate: 500000, sample_point: "0.875", tq: 25, prop_seg: 34, phase_seg1: 35, phase_seg2: 10, sjw: 1, brp: 2 },
      bittiming_const: { name: "gs_usb", tseg1: { min: 1, max: 256 }, tseg2: { min: 1, max: 128 }, sjw: { min: 1, max: 128 }, brp: { min: 1, max: 512 }, brp_inc: 1 },
      data_bittiming: { bitrate: 2000000, sample_point: "0.750" },
      data_bittiming_const: { name: "gs_usb" },
      clock: 80000000,
    },
    info_xstats: { restarts: 1, bus_error: 7, arbitration_lost: 0, error_warning: 2, error_passive: 1, bus_off: 0 },
  },
  stats64: { rx: { bytes: 800, packets: 100, errors: 7, dropped: 0 }, tx: { bytes: 80, packets: 10, errors: 0, dropped: 0 } },
};
const VCAN = { ifindex: 4, ifname: "vcan0", flags: ["NOARP", "UP", "LOWER_UP"], mtu: 72, operstate: "UNKNOWN", link_type: "can", linkinfo: { info_kind: "vcan" }, stats64: { rx: { bytes: 5330, packets: 923, errors: 0, dropped: 0 }, tx: { bytes: 5330, packets: 923, errors: 0, dropped: 0 } } };

describe("parseIpLink", () => {
  test("gs_usb FD adapter", () => {
    const i = parseIpLink(GS_USB)!;
    expect(i).toMatchObject({ name: "can0", kind: "can", up: true, fdCapable: true, fdOn: true, state: "ERROR-PASSIVE", berr: { tx: 128, rx: 3 }, bitrate: 500000, samplePoint: 0.875, dbitrate: 2000000, dsamplePoint: 0.75, clock: 80000000, restartMs: 100 });
    expect(i.xstats!.bus_error).toBe(7);
    expect(i.stats.rxPackets).toBe(100);
  });
  test("vcan", () => {
    expect(parseIpLink(VCAN)).toMatchObject({ name: "vcan0", kind: "vcan", up: true, fdOn: true, fdCapable: true, bitrate: undefined });
  });
  test("non-CAN links are skipped", () => {
    expect(parseIpLink({ ifname: "eth0", link_type: "ether" })).toBeNull();
  });
});

describe("ipArgs", () => {
  const can0 = parseIpLink(GS_USB)!;
  test("bitrate, FD and flags", () => {
    expect(ipArgs(can0, normCfg({ bitrate: 250000, samplePoint: 87.5, fd: true, dbitrate: 4000000, listenOnly: true, restartMs: 0 }))).toEqual([
      ["link", "set", "dev", "can0", "type", "can", "bitrate", "250000", "sample-point", "0.875", "dbitrate", "4000000", "fd", "on", "listen-only", "on", "restart-ms", "0"],
    ]);
  });
  test("FD reuses the current data bitrate", () => {
    expect(ipArgs(can0, { fd: true })[0]).toContain("2000000");
  });
  test("unsupported flag is refused when on, skipped when off", () => {
    const c = { ...can0, ctrlSupported: ["LISTEN-ONLY"] };
    expect(() => ipArgs(c, { loopback: true })).toThrow();
    expect(ipArgs(c, { loopback: false })).toEqual([]);
  });
  test("vcan only takes the FD MTU", () => {
    const v = parseIpLink(VCAN)!;
    expect(ipArgs(v, { bitrate: 500000 })).toEqual([]);
    expect(ipArgs(v, { fd: false })).toEqual([["link", "set", "dev", "vcan0", "mtu", "16"]]);
  });
  test("normCfg validation", () => {
    expect(() => normCfg({ bitrate: 5 })).toThrow();
    expect(() => normCfg({ samplePoint: 120 })).toThrow();
    expect(normCfg({ samplePoint: "0.8", up: 1, junk: 1 })).toEqual({ samplePoint: 0.8, up: true });
  });
});
