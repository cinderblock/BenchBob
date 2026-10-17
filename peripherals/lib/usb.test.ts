// Fixture tests for peripherals/lib/usb.ts. Run: bun test peripherals/lib
// desky-pi4.json / uhubctl-desky-pi4.txt are real captures from desky (Pi 4, nothing plugged in).
// The "busy" Pi 4 below extends that capture with synthetic devices; uhubctl-examples.txt
// follows the output format shown in the uhubctl README.
import { describe, expect, test } from "bun:test";
import { buildInventory, className, filterKernelLine, parseSsPeers, parseUhubctl, planPower, powerTopology, speedLabel, type Props, type RawUsbDevice } from "./usb.ts";

const FX = import.meta.dir + "/usb-fixtures/";
const desky: { raw: RawUsbDevice[]; udev: Props[] } = await Bun.file(FX + "desky-pi4.json").json();
const deskyUhub = await Bun.file(FX + "uhubctl-desky-pi4.txt").text();
const examples = await Bun.file(FX + "uhubctl-examples.txt").text();

const PI4 = "Raspberry Pi 4 Model B Rev 1.1";

describe("desky (real capture)", () => {
  const inv = buildInventory(desky.raw, desky.udev);
  const hubs = parseUhubctl(deskyUhub);
  const topo = powerTopology(inv, hubs, PI4);

  test("tree", () => {
    expect(inv.devices.map((d) => `${d.busid}@${d.depth}`)).toEqual(["usb1@0", "1-1@1", "usb2@0"]);
    expect(inv.controllers).toHaveLength(1);
    expect(inv.controllers[0]).toMatchObject({ id: "0000:01:00.0", pciId: "1106:3483", driver: "xhci_hcd", buses: [1, 2] });
    expect(inv.controllers[0].name).toContain("VL805");
    const via = inv.devices.find((d) => d.busid === "1-1")!;
    expect(via).toMatchObject({ hub: true, nports: 4, vidpid: "2109:3431", vendor: "VIA Labs, Inc.", speed: 480, version: "2.10", maxPowerMa: 100, parent: "usb1", port: 1 });
    expect(via.interfaces[0]).toMatchObject({ clsName: "Hub", driver: "hub" });
    expect(inv.devices[0].ports).toEqual([{ port: 1, child: "1-1", connectType: "unknown", overCurrent: 0 }]);
    expect(inv.devices.find((d) => d.busid === "usb2")!.speedLabel).toBe("5 Gb/s SuperSpeed");
  });

  test("uhubctl listing", () => {
    expect(hubs.map((h) => `${h.loc}:${h.mode}:${h.nports}`)).toEqual(["2:ppps:4", "1-1:ppps:4", "1:ppps:1"]);
    expect(hubs[0].ports[0]).toMatchObject({ power: true, speed: "5gbps", link: "Rx.Detect", connect: false, attached: null });
    expect(hubs[2].ports[0]).toMatchObject({ power: true, speed: "highspeed", suspend: true, enable: true, connect: true, attached: { vidpid: "2109:3431", desc: "USB2.0 Hub, USB 2.10, 4 ports, ppps" } });
    expect(hubs[0].desc).toBe("Linux 6.18.50+rpt-rpi-v8 xhci-hcd xHCI Host Controller 0000:01:00.0");
  });

  test("Pi 4 VL805 quirk: ganged group + internal link", () => {
    expect(topo.groups.map((g) => [g.id, g.hubs])).toEqual([["vl805-0000:01:00.0", ["1-1", "2"]]]);
    expect(Object.keys(topo.internal)).toEqual(["1:1"]);
  });

  test("switching one Pi 4 port widens to all eight and needs ganged confirmation", () => {
    const p = planPower(inv, hubs, topo, {}, { hub: "1-1", port: 2, action: "off" });
    expect(p.needs).toBe("ganged");
    expect(p.ports).toHaveLength(8);
    expect(p.commands).toEqual([["-l", "1-1", "-a", "off"], ["-l", "2", "-a", "off"]]);
    const on = planPower(inv, hubs, topo, {}, { hub: "2", port: 3, action: "on" });
    expect(on.needs).toBe("none");
    const cyc = planPower(inv, hubs, topo, {}, { hub: "2", action: "cycle" });
    expect(cyc.commands.map((c) => c.join(" "))).toEqual(["-l 1-1 -a off", "-l 2 -a off", "-l 1-1 -a on", "-l 2 -a on"]);
  });

  test("the internal link is unsafe", () => {
    const p = planPower(inv, hubs, topo, {}, { hub: "1", port: 1, action: "off" });
    expect(p.needs).toBe("unsafe");
    expect(p.devices).toEqual(["1-1"]);
  });

  test("bad requests", () => {
    expect(() => planPower(inv, hubs, topo, {}, { hub: "9", action: "off" })).toThrow(/not listed/);
    expect(() => planPower(inv, hubs, topo, {}, { hub: "1-1", port: 7, action: "off" })).toThrow(/ports 1/);
  });
});

// ---------- A busy Pi 4, built on the real capture ----------

const ROOT1 = desky.raw.find((r) => r.name === "usb1")!.devpath;
const ROOT2 = desky.raw.find((r) => r.name === "usb2")!.devpath;
const P11 = ROOT1 + "/1-1";

function dev(name: string, devpath: string, a: Record<string, string>, ifs: [number, number, number, string][], extra: Partial<RawUsbDevice> = {}): RawUsbDevice {
  return {
    name,
    devpath,
    attrs: { busnum: name.split("-")[0], devnum: "5", bConfigurationValue: "1", bDeviceClass: "00", version: " 2.00", maxchild: "0", ...a },
    driver: "usb",
    interfaces: ifs.map(([cls, sub, proto, driver], i) => ({ name: `${name}:1.${i}`, cls, sub, proto, num: i, alt: 0, driver })),
    ports: [],
    ...extra,
  };
}

const busyRaw: RawUsbDevice[] = [
  ...desky.raw,
  dev("1-1.2", P11 + "/1-1.2", { idVendor: "0403", idProduct: "6001", speed: "12", bMaxPower: "90mA", serial: "A50285BI", product: "FT232R USB UART" }, [[0xff, 0xff, 0xff, "ftdi_sio"]]),
  dev("1-1.3", P11 + "/1-1.3", { idVendor: "0bda", idProduct: "5411", speed: "480", maxchild: "4", bDeviceClass: "09" }, [[9, 0, 0, "hub"]], {
    ports: [1, 2, 3, 4].map((port) => ({ port, connectType: "hotplug", overCurrent: port === 4 ? 2 : 0 })),
  }),
  dev("1-1.3.1", P11 + "/1-1.3/1-1.3.1", { idVendor: "1d50", idProduct: "606f", speed: "12" }, [[0xff, 0xff, 0xff, "gs_usb"]]),
  dev("1-1.3.4", P11 + "/1-1.3/1-1.3.4", { idVendor: "ffff", idProduct: "0001", speed: "12", product: "Mystery" }, [[0xff, 0, 0, ""]]),
  dev("1-1.4", P11 + "/1-1.4", { idVendor: "0483", idProduct: "374b", speed: "480" }, [[0xff, 0xff, 0xff, ""], [8, 6, 0x50, ""], [2, 2, 1, ""], [0x0a, 0, 0, ""]], { driver: "usbip-host", usbipStatus: 2 }),
  dev("2-1", ROOT2 + "/2-1", { idVendor: "0781", idProduct: "5581", speed: "5000", version: " 3.20", bMaxPower: "896mA" }, [[8, 6, 0x62, "uas"]]),
];
const SDA = ROOT2 + "/2-1/2-1:1.0/host0/target0:0:0/0:0:0:0/block/sda";
const busyUdev: Props[] = [
  ...desky.udev,
  { DEVPATH: P11 + "/1-1.2", SUBSYSTEM: "usb", DEVTYPE: "usb_device", ID_VENDOR_FROM_DATABASE: "Future Technology Devices International, Ltd", ID_MODEL_FROM_DATABASE: "FT232 Serial (UART) IC", ID_PATH: "platform-fd500000.pcie-pci-0000:01:00.0-usb-0:1.2" },
  { DEVPATH: P11 + "/1-1.2/1-1.2:1.0/ttyUSB0/tty/ttyUSB0", SUBSYSTEM: "tty", DEVNAME: "/dev/ttyUSB0" },
  { DEVPATH: P11 + "/1-1.3/1-1.3.1/1-1.3.1:1.0/net/can0", SUBSYSTEM: "net", INTERFACE: "can0", ID_NET_DRIVER: "gs_usb" },
  { DEVPATH: SDA, SUBSYSTEM: "block", DEVTYPE: "disk", DEVNAME: "/dev/sda" },
  { DEVPATH: SDA + "/sda1", SUBSYSTEM: "block", DEVTYPE: "partition", DEVNAME: "/dev/sda1" },
  { DEVPATH: SDA + "/sda2", SUBSYSTEM: "block", DEVTYPE: "partition", DEVNAME: "/dev/sda2" },
];

describe("busy Pi 4 (synthetic)", () => {
  const inv = buildInventory(busyRaw, busyUdev);
  const by = (b: string) => inv.devices.find((d) => d.busid === b)!;

  test("depth-first order and depths", () => {
    expect(inv.devices.map((d) => `${d.busid}@${d.depth}`)).toEqual(["usb1@0", "1-1@1", "1-1.2@2", "1-1.3@2", "1-1.3.1@3", "1-1.3.4@3", "1-1.4@2", "usb2@0", "2-1@1"]);
    expect(by("1-1").ports.map((p) => p.child)).toEqual([null, "1-1.2", "1-1.3", "1-1.4"]);
    expect(by("1-1.3").ports[3].overCurrent).toBe(2);
  });

  test("names, nodes and panel hints", () => {
    expect(by("1-1.2")).toMatchObject({ vendor: "Future Technology Devices International, Ltd", product: "FT232 Serial (UART) IC", serial: "A50285BI", speedLabel: "12 Mb/s Full", maxPowerMa: 90 });
    expect(by("1-1.2").nodes).toEqual([{ dev: "/dev/ttyUSB0", kind: "serial", panel: "serial" }]);
    // can0 belongs to the CAN adapter, not to the hub above it.
    expect(by("1-1.3").nodes).toEqual([]);
    expect(by("1-1.3.1").nodes).toEqual([{ dev: "can0", kind: "can", panel: "can" }]);
    expect(by("2-1").nodes).toEqual([{ dev: "/dev/sda", kind: "disk", panel: "drives", label: "2 partitions" }]);
    expect(by("2-1").interfaces[0].clsName).toBe("Mass storage (UAS)");
    expect(by("1-1.3.4").interfaces[0]).toMatchObject({ clsName: "Vendor-specific", driver: null });
    expect(by("1-1.4").hint).toEqual({ panel: "probes", label: "ST-LINK" });
  });

  test("usbip state", () => {
    expect(by("1-1.4").usbip).toBe("in use");
    expect(by("1-1.2").usbip).toBe(null);
  });

  test("per-port hub: plain switch, protection", () => {
    const hubs = [...parseUhubctl(deskyUhub), ...parseUhubctl(examples)];
    const topo = powerTopology(inv, hubs, PI4);
    const protect = { "1-1.3.1": ["is the management network adapter"], "2-1": ["holds a served drive (/dev/sda1)"] };
    const ok = planPower(inv, hubs, topo, protect, { hub: "1-1.3", port: 4, action: "off" });
    expect(ok).toMatchObject({ needs: "none", ports: ["1-1.3:4"], devices: ["1-1.3.4"], commands: [["-l", "1-1.3", "-p", "4", "-a", "off"]] });
    const bad = planPower(inv, hubs, topo, protect, { hub: "1-1.3", port: 1, action: "cycle" });
    expect(bad.needs).toBe("unsafe");
    expect(bad.warnings.join(" ")).toContain("management network adapter");
    expect(bad.commands).toEqual([["-l", "1-1.3", "-p", "1", "-a", "off"], ["-l", "1-1.3", "-p", "1", "-a", "on"]]);
    // Everything on the Pi 4 sockets is ganged, and the served drive on 2-1 makes it unsafe.
    const all = planPower(inv, hubs, topo, protect, { hub: "1-1", port: 2, action: "off" });
    expect(all.needs).toBe("unsafe");
    expect(all.devices.sort()).toEqual(["1-1.2", "1-1.3", "1-1.3.1", "1-1.3.4", "1-1.4", "2-1"]);
  });

  test("hub reporting ganged mode gets a group and -f; nops refuses", () => {
    const hubs = parseUhubctl(examples);
    const topo = powerTopology(inv, hubs, PI4);
    expect(topo.groups.find((g) => g.id === "ganged-3-2")?.hubs).toEqual(["3-2"]);
    const p = planPower(inv, hubs, topo, {}, { hub: "3-2", port: 2, action: "off" });
    expect(p.needs).toBe("ganged");
    expect(p.commands).toEqual([["-l", "3-2", "-a", "off", "-f"]]);
    expect(() => planPower(inv, hubs, topo, {}, { hub: "3-3", action: "off" })).toThrow(/no power switching/);
  });
});

describe("uhubctl parser", () => {
  test("port flags", () => {
    const h = parseUhubctl(examples);
    expect(h.map((x) => x.loc)).toEqual(["1-1.3", "2-1", "3-2", "3-3"]);
    expect(h[0].ports[1]).toMatchObject({ power: false, flags: ["off"], connect: false });
    expect(h[0].ports[3]).toMatchObject({ power: true, overCurrent: true });
    expect(h[0].ports[0].attached).toEqual({ vidpid: "1d50:606f", desc: "bytewerk candleLight USB to CAN adapter 003A00385744" });
    expect(h[1].ports[0]).toMatchObject({ speed: "5gbps", link: "U0", enable: true, connect: true });
    expect(h[1].ports[3]).toMatchObject({ power: false, link: "Rx.Detect" });
    expect(h[2].ports[1].speed).toBe("lowspeed");
  });

  test("action output: New status wins", async () => {
    const h = parseUhubctl(await Bun.file(FX + "uhubctl-action.txt").text());
    expect(h).toHaveLength(1);
    expect(h[0].ports).toEqual([expect.objectContaining({ port: 2, power: false })]);
  });
});

test("kernel log filter", async () => {
  const lines = (await Bun.file(FX + "dmesg.txt").text()).split(/\r?\n/).filter(Boolean);
  const got = lines.map(filterKernelLine).filter(Boolean).map((k) => `${k!.level} ${k!.text}`);
  expect(got).toEqual([
    "info usb 1-1.2: new full-speed USB device number 3 using xhci_hcd",
    "info ftdi_sio 1-1.2:1.0: FTDI USB Serial Device converter detected",
    "info usb 1-1.2: FTDI USB Serial Device converter now attached to ttyUSB0",
    "info cdc_acm 1-1.3:1.0: ttyACM0: USB ACM device",
    "error usb 1-1.4: device descriptor read/64, error -71",
    "error usb usb1-port4: over-current condition",
    "warn usb 1-1.4: reset high-speed USB device number 5 using xhci_hcd",
    "error usb 1-1-port4: unable to enumerate USB device",
    "info usb 2-1: new SuperSpeed USB device number 2 using xhci_hcd",
    "info usb-storage 2-1:1.0: USB Mass Storage device detected",
    "info scsi host0: usb-storage 2-1:1.0",
    "info gs_usb 1-1.3.1:1.0: Configuring for 1 interfaces",
    "info usb 1-1.2: USB disconnect, device number 3",
    "info ftdi_sio ttyUSB0: FTDI USB Serial Device converter now disconnected from ttyUSB0",
    "info vhci_hcd vhci_hcd.0: new USB bus registered, assigned bus number 3",
    "info vhci_hcd vhci_hcd.0: USB bus 3 deregistered",
  ]);
  expect(filterKernelLine(lines[0])!.ts).toBe("2026-10-17T09:00:01,000001-07:00");
});

test("ss peers", () => {
  const out = "0      0      10.255.7.45:3240     10.255.3.20:51234\n0 0 [::ffff:10.255.7.45]:4000 [::ffff:10.255.3.21]:40000\n0 0 10.255.7.45:22 10.255.3.20:5555";
  expect(parseSsPeers(out, [3240, 4000])).toEqual([
    { local: 3240, peer: "10.255.3.20:51234" },
    { local: 4000, peer: "10.255.3.21:40000" },
  ]);
});

test("labels", () => {
  expect(speedLabel("1.5")).toBe("1.5 Mb/s Low");
  expect(speedLabel("10000")).toBe("10 Gb/s SuperSpeed+");
  expect(className(2, 2, 1)).toBe("CDC ACM (serial)");
  expect(className(0xe0, 1, 1)).toBe("Bluetooth");
});

test("uhubctl -N (no descriptions) output from desky", async () => {
  const h = parseUhubctl(await Bun.file(FX + "uhubctl-desky-pi4-nodesc.txt").text());
  expect(h.map((x) => `${x.loc} ${x.vidpid} "${x.desc}" ${x.usb} ${x.nports} ${x.mode}`)).toEqual(['2 1d6b:0003 "" 3.00 4 ppps', '1-1 2109:3431 "" 2.10 4 ppps', '1 1d6b:0002 "" 2.00 1 ppps']);
  expect(h[2].ports[0].attached).toEqual({ vidpid: "2109:3431", desc: "USB 2.10, 4 ports, ppps" });
});
