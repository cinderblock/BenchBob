// ==================== USB: pure logic for the usb driver ====================
//
// Everything here is either a sysfs reader or a pure function over plain data, so the tree
// builder, uhubctl parser, kernel-log filter and power planner can be unit-tested with
// fixtures on any machine (peripherals/lib/usb.test.ts). The driver owns processes and state.

import { readdir, readlink, realpath } from "node:fs/promises";

export type Props = Record<string, string>;

// ---------- sysfs reader ----------

export interface RawPort {
  port: number;
  connectType: string; // hotplug | hardwired | not used | unknown
  overCurrent: number;
}

export interface RawInterface {
  name: string; // "1-1.2:1.0"
  cls: number;
  sub: number;
  proto: number;
  num: number;
  alt: number;
  driver: string;
}

export interface RawUsbDevice {
  name: string; // sysfs name: "usb1", "1-1", "1-1.2"
  devpath: string; // udev DEVPATH, "/devices/…/usb1/1-1"
  attrs: Record<string, string>;
  driver: string; // "usb" normally, "usbip-host" while shared
  interfaces: RawInterface[];
  ports: RawPort[]; // hub ports (from <hub>:1.0/<name>-portN)
  usbipStatus?: number; // 1 available, 2 used, 3 error (only while bound to usbip-host)
  controller?: { devpath: string; name: string; driver: string; pciId: string };
}

const DEV_ATTRS = ["idVendor", "idProduct", "speed", "version", "bMaxPower", "bConfigurationValue", "maxchild", "busnum", "devnum", "devpath", "manufacturer", "product", "serial", "bDeviceClass", "bDeviceSubClass", "bDeviceProtocol", "removable", "bNumInterfaces"];
const IF_ATTRS = ["bInterfaceClass", "bInterfaceSubClass", "bInterfaceProtocol", "bInterfaceNumber", "bAlternateSetting"];

async function rd(path: string): Promise<string> {
  try {
    return (await Bun.file(path).text()).trim();
  } catch {
    return "";
  }
}

async function linkName(path: string): Promise<string> {
  try {
    const l = await readlink(path);
    return l.slice(l.lastIndexOf("/") + 1);
  } catch {
    return "";
  }
}

/** Snapshot /sys/bus/usb/devices: every device, its interfaces with bound drivers, hub ports and (for root hubs) the host controller. */
export async function readUsbSysfs(sys = "/sys"): Promise<RawUsbDevice[]> {
  const base = `${sys}/bus/usb/devices`;
  let names: string[] = [];
  try {
    names = await readdir(base);
  } catch {
    return [];
  }
  const out: RawUsbDevice[] = [];
  for (const name of names.filter((n) => !n.includes(":"))) {
    let real: string;
    try {
      real = await realpath(`${base}/${name}`);
    } catch {
      continue; // unplugged between readdir and here
    }
    const attrs: Record<string, string> = {};
    await Promise.all(DEV_ATTRS.map(async (a) => (attrs[a] = await rd(`${real}/${a}`))));
    const driver = await linkName(`${real}/driver`);
    const interfaces: RawInterface[] = [];
    const ports: RawPort[] = [];
    // Root hub usbN's interfaces are named "N-0:1.0"; everyone else's "<name>:1.0".
    const ifPrefix = name.startsWith("usb") ? `${name.slice(3)}-0:` : `${name}:`;
    for (const ifn of names.filter((n) => n.startsWith(ifPrefix))) {
      const ip = `${base}/${ifn}`;
      const a: Record<string, string> = {};
      await Promise.all(IF_ATTRS.map(async (k) => (a[k] = await rd(`${ip}/${k}`))));
      interfaces.push({
        name: ifn,
        cls: parseInt(a.bInterfaceClass, 16) || 0,
        sub: parseInt(a.bInterfaceSubClass, 16) || 0,
        proto: parseInt(a.bInterfaceProtocol, 16) || 0,
        num: parseInt(a.bInterfaceNumber, 16) || 0,
        alt: parseInt(a.bAlternateSetting) || 0,
        driver: await linkName(`${ip}/driver`),
      });
      // Hub ports appear as <name>-portN under the hub interface (usb1-port1, 1-1-port3).
      let entries: string[] = [];
      try {
        entries = await readdir(ip);
      } catch {}
      for (const e of entries) {
        const m = /-port(\d+)$/.exec(e);
        if (!m || !e.startsWith(name + "-port")) continue;
        ports.push({ port: +m[1], connectType: await rd(`${ip}/${e}/connect_type`), overCurrent: parseInt(await rd(`${ip}/${e}/over_current_count`)) || 0 });
      }
    }
    interfaces.sort((a, b) => a.num - b.num || a.alt - b.alt);
    ports.sort((a, b) => a.port - b.port);
    const dev: RawUsbDevice = { name, devpath: real.slice(sys.length), attrs, driver, interfaces, ports };
    if (driver === "usbip-host") dev.usbipStatus = parseInt(await rd(`${real}/usbip_status`)) || 0;
    if (name.startsWith("usb")) {
      const parent = real.slice(0, real.lastIndexOf("/"));
      const v = await rd(`${parent}/vendor`);
      const d = await rd(`${parent}/device`);
      dev.controller = {
        devpath: parent.slice(sys.length),
        name: parent.slice(parent.lastIndexOf("/") + 1),
        driver: await linkName(`${parent}/driver`),
        pciId: v && d ? `${v.replace(/^0x/, "")}:${d.replace(/^0x/, "")}` : "",
      };
    }
    out.push(dev);
  }
  return out;
}

// ---------- Names ----------

const CLASSES: Record<number, string> = {
  0x00: "Per-interface",
  0x01: "Audio",
  0x02: "Communications (CDC)",
  0x03: "HID",
  0x05: "Physical",
  0x06: "Imaging",
  0x07: "Printer",
  0x08: "Mass storage",
  0x09: "Hub",
  0x0a: "CDC data",
  0x0b: "Smart card",
  0x0d: "Content security",
  0x0e: "Video",
  0x0f: "Personal healthcare",
  0x10: "Audio/video",
  0x11: "Billboard",
  0x12: "USB-C bridge",
  0x13: "Bulk display",
  0x14: "MCTP",
  0x3c: "I3C",
  0xdc: "Diagnostic",
  0xe0: "Wireless",
  0xef: "Miscellaneous",
  0xfe: "Application-specific",
  0xff: "Vendor-specific",
};

/** Human name for an interface (or device) class, with the subclasses people actually meet. */
export function className(cls: number, sub = 0, proto = 0): string {
  if (cls === 0x02 && sub === 0x02) return "CDC ACM (serial)";
  if (cls === 0x02 && sub === 0x06) return "CDC Ethernet (ECM)";
  if (cls === 0x02 && sub === 0x0d) return "CDC Ethernet (NCM)";
  if (cls === 0x02 && sub === 0x0e) return "CDC MBIM";
  if (cls === 0x01 && sub === 0x01) return "Audio control";
  if (cls === 0x01 && sub === 0x02) return "Audio streaming";
  if (cls === 0x01 && sub === 0x03) return "MIDI";
  if (cls === 0x03 && sub === 0x01 && proto === 0x01) return "HID keyboard";
  if (cls === 0x03 && sub === 0x01 && proto === 0x02) return "HID mouse";
  if (cls === 0x08 && proto === 0x62) return "Mass storage (UAS)";
  if (cls === 0x0e && sub === 0x01) return "Video control";
  if (cls === 0x0e && sub === 0x02) return "Video streaming";
  if (cls === 0xe0 && sub === 0x01 && proto === 0x01) return "Bluetooth";
  if (cls === 0xe0 && sub === 0x01 && proto === 0x03) return "RNDIS";
  if (cls === 0xef && sub === 0x04 && proto === 0x01) return "RNDIS";
  if (cls === 0xfe && sub === 0x01) return "DFU";
  if (cls === 0xfe && sub === 0x03) return "USBTMC (instrument)";
  return CLASSES[cls] ?? `Class 0x${cls.toString(16).padStart(2, "0")}`;
}

/** sysfs `speed` (Mb/s) → label. */
export function speedLabel(speed: string | number): string {
  const s = Number(speed);
  if (!s) return "";
  if (s === 1.5) return "1.5 Mb/s Low";
  if (s === 12) return "12 Mb/s Full";
  if (s === 480) return "480 Mb/s High";
  if (s === 5000) return "5 Gb/s SuperSpeed";
  if (s === 10000) return "10 Gb/s SuperSpeed+";
  if (s === 20000) return "20 Gb/s SuperSpeed+ 2x2";
  return s >= 1000 ? `${s / 1000} Gb/s` : `${s} Mb/s`;
}

// ---------- Device nodes + panel hints ----------

export interface UsbNode {
  dev: string; // "/dev/ttyACM0", "can0", "/dev/sda", "card1"
  kind: string; // serial | can | net | wifi | disk | camera | video | hidraw | i2c | sound | input | gpio | bluetooth | misc
  panel?: string; // driver id (or "drives") that handles it
  label?: string; // extra detail ("2 partitions", "wlan")
}

const CAN_DRIVERS = /^(gs_usb|peak_usb|kvaser_usb|usb_8dev|ems_usb|esd_usb|mcba_usb|ucan|etas_es58x|f81604|nct6694_canfd)$/;

/** Classify one udev entry that lives under a USB device into a user-visible node, or null. */
export function nodeFor(p: Props): UsbNode | null {
  const s = p.SUBSYSTEM;
  const dn = p.DEVNAME || "";
  if (s === "tty" && dn) return { dev: dn, kind: "serial", panel: "serial" };
  if (s === "net" && p.INTERFACE) {
    if (/^v?can\d/.test(p.INTERFACE) || CAN_DRIVERS.test(p.ID_NET_DRIVER || "")) return { dev: p.INTERFACE, kind: "can", panel: "can" };
    if (p.DEVTYPE === "wlan") return { dev: p.INTERFACE, kind: "wifi", panel: "wifi" };
    return { dev: p.INTERFACE, kind: "net", panel: "netdiag" };
  }
  if (s === "block" && p.DEVTYPE === "disk" && dn) return { dev: dn, kind: "disk", panel: "drives" };
  if (s === "video4linux" && dn) {
    const cap = /:capture:/.test(p.ID_V4L_CAPABILITIES || "");
    return cap ? { dev: dn, kind: "camera", panel: "camera" } : { dev: dn, kind: "video" };
  }
  if (s === "hidraw" && dn) return { dev: dn, kind: "hidraw" };
  if (s === "i2c-dev" && dn) return { dev: dn, kind: "i2c", panel: "i2c" };
  if (s === "sound" && /\/card\d+$/.test(p.DEVPATH || "")) return { dev: "card" + /card(\d+)$/.exec(p.DEVPATH)![1], kind: "sound", label: p.ID_MODEL_FROM_DATABASE || undefined };
  if (s === "input" && /\/event\d+$/.test(dn)) return { dev: dn, kind: "input" };
  if (s === "gpio" && /gpiochip/.test(dn)) return { dev: dn, kind: "gpio", panel: "gpio" };
  if (s === "bluetooth" && p.DEVTYPE === "host") return { dev: (p.DEVPATH || "").slice((p.DEVPATH || "").lastIndexOf("/") + 1), kind: "bluetooth", panel: "ble" };
  if (s === "usbmisc" && dn) return { dev: dn, kind: "misc" };
  return null;
}

/** Device-level panel hints for things identified by VID:PID rather than by a node. */
const ID_HINTS: [RegExp, string, string][] = [
  [/^0bda:(2832|2838)$/, "sdr", "RTL-SDR"],
  [/^1546:01a[5-9]$/, "gps", "u-blox GPS"],
  [/^067b:23a3$/, "gps", "GPS (PL2303)"],
  [/^0483:37(48|49|4b|4e|4f|52|53|54|55|57)$/, "probes", "ST-LINK"],
  [/^1366:/, "probes", "SEGGER J-Link"],
  [/^0d28:0204$/, "probes", "DAPLink"],
  [/^2e8a:000c$/, "probes", "Raspberry Pi Debug Probe"],
  [/^1a86:8010$/, "probes", "WCH-Link"],
  [/^0403:6010$/, "probes", "FTDI FT2232 (JTAG capable)"],
  [/^303a:1001$/, "probes", "ESP32 USB-JTAG"],
  [/^(051d|0764|0463|09ae|06da):/, "ups", "UPS"],
];

export function idHint(vidpid: string, product: string): { panel: string; label: string } | null {
  for (const [re, panel, label] of ID_HINTS) if (re.test(vidpid)) return { panel, label };
  if (/CMSIS-DAP/i.test(product)) return { panel: "probes", label: "CMSIS-DAP" };
  return null;
}

// ---------- Tree ----------

export interface UsbInterface {
  id: string; // "1.0" (config.interface)
  num: number;
  cls: number;
  sub: number;
  proto: number;
  clsName: string;
  driver: string | null;
}

export interface UsbPort {
  port: number;
  child: string | null; // busid of what's plugged in
  connectType: string;
  overCurrent: number;
}

export interface UsbDevice {
  busid: string; // "usb1", "1-1", "1-1.2" (also the physical port path)
  devpath: string;
  bus: number;
  devnum: number;
  parent: string | null;
  port: number | null; // port number on the parent hub
  depth: number;
  root: boolean;
  hub: boolean;
  nports: number;
  ports: UsbPort[];
  vid: string;
  pid: string;
  vidpid: string;
  vendor: string;
  product: string;
  serial: string;
  speed: number;
  speedLabel: string;
  version: string;
  maxPowerMa: number;
  config: number;
  cls: string;
  driver: string;
  interfaces: UsbInterface[];
  nodes: UsbNode[];
  hint: { panel: string; label: string } | null;
  idPath: string;
  usbip: null | "available" | "in use" | "error";
  controller: string | null; // controller id (root hubs only)
}

export interface UsbController {
  id: string; // "0000:01:00.0"
  devpath: string;
  name: string;
  driver: string;
  pciId: string;
  buses: number[];
}

export interface Inventory {
  controllers: UsbController[];
  devices: UsbDevice[]; // depth-first tree order
}

function portKey(busid: string): number[] {
  // "usb2" → [2]; "2-1.4.3" → [2, 1, 4, 3]
  if (busid.startsWith("usb")) return [+busid.slice(3)];
  const [b, rest] = busid.split("-");
  return [+b, ...rest.split(".").map(Number)];
}

export function parentOf(busid: string): { parent: string; port: number } | null {
  if (busid.startsWith("usb")) return null;
  const [b, rest] = busid.split("-");
  const i = rest.lastIndexOf(".");
  return i < 0 ? { parent: `usb${b}`, port: +rest } : { parent: `${b}-${rest.slice(0, i)}`, port: +rest.slice(i + 1) };
}

/** Udev nodes that belong to this device itself (its interfaces), not to a child device. */
function nodesOf(devpath: string, udev: Props[]): UsbNode[] {
  const pre = devpath + "/";
  const out: UsbNode[] = [];
  const seen = new Set<string>();
  for (const p of udev) {
    const dp = p.DEVPATH;
    if (!dp || !dp.startsWith(pre)) continue;
    const first = dp.slice(pre.length).split("/")[0];
    if (!first.includes(":")) continue; // a child device's subtree
    const n = nodeFor(p);
    if (!n || seen.has(n.kind + n.dev)) continue;
    seen.add(n.kind + n.dev);
    if (n.kind === "disk") {
      const parts = udev.filter((q) => q.SUBSYSTEM === "block" && q.DEVTYPE === "partition" && q.DEVPATH?.startsWith(dp + "/")).length;
      if (parts) n.label = `${parts} partition${parts > 1 ? "s" : ""}`;
    }
    out.push(n);
  }
  const order = ["serial", "can", "net", "wifi", "disk", "camera", "video", "sound", "i2c", "gpio", "bluetooth", "hidraw", "input", "misc"];
  return out.sort((a, b) => order.indexOf(a.kind) - order.indexOf(b.kind) || a.dev.localeCompare(b.dev, undefined, { numeric: true }));
}

const CTRL_NAMES: Record<string, string> = { "1106:3483": "VIA VL805/806 xHCI (USB 3.0)" };

export function buildInventory(raw: RawUsbDevice[], udev: Props[]): Inventory {
  const byPath = new Map(udev.map((p) => [p.DEVPATH, p]));
  const devices: UsbDevice[] = raw.map((r) => {
    const a = r.attrs;
    const u = byPath.get(r.devpath) || {};
    const vid = (a.idVendor || "").toLowerCase();
    const pid = (a.idProduct || "").toLowerCase();
    const root = r.name.startsWith("usb");
    const hub = a.bDeviceClass === "09" || r.interfaces.some((i) => i.cls === 9);
    const par = parentOf(r.name);
    const dbVendor = u.ID_VENDOR_FROM_DATABASE || "";
    const dbModel = u.ID_MODEL_FROM_DATABASE || "";
    const product = dbModel || a.product || "";
    const nports = parseInt(a.maxchild) || 0;
    const usbip = r.driver === "usbip-host" ? (r.usbipStatus === 2 ? "in use" : r.usbipStatus === 3 ? "error" : "available") : null;
    const devCls = parseInt(a.bDeviceClass, 16) || 0;
    return {
      busid: r.name,
      devpath: r.devpath,
      bus: parseInt(a.busnum) || +(r.name.match(/^usb(\d+)/)?.[1] || r.name.split("-")[0]),
      devnum: parseInt(a.devnum) || 0,
      parent: par?.parent ?? null,
      port: par?.port ?? null,
      depth: 0,
      root,
      hub,
      nports,
      ports: [],
      vid,
      pid,
      vidpid: `${vid}:${pid}`,
      vendor: dbVendor || a.manufacturer || "",
      product,
      serial: root ? "" : a.serial || "",
      speed: Number(a.speed) || 0,
      speedLabel: speedLabel(a.speed),
      version: (a.version || "").trim(),
      maxPowerMa: parseInt(a.bMaxPower) || 0,
      config: parseInt(a.bConfigurationValue) || 0,
      cls: className(devCls, parseInt(a.bDeviceSubClass, 16) || 0, parseInt(a.bDeviceProtocol, 16) || 0),
      driver: r.driver,
      interfaces: r.interfaces.map((i) => ({ id: i.name.slice(i.name.indexOf(":") + 1), num: i.num, cls: i.cls, sub: i.sub, proto: i.proto, clsName: className(i.cls, i.sub, i.proto), driver: i.driver || null })),
      nodes: nodesOf(r.devpath, udev),
      hint: root || hub ? null : idHint(`${vid}:${pid}`, product),
      idPath: u.ID_PATH || "",
      usbip,
      controller: r.controller ? r.controller.name : null,
    } satisfies UsbDevice;
  });

  const byId = new Map(devices.map((d) => [d.busid, d]));
  for (const d of devices) {
    const r = raw.find((x) => x.name === d.busid)!;
    const ports = new Map<number, UsbPort>();
    for (const p of r.ports) ports.set(p.port, { port: p.port, child: null, connectType: p.connectType, overCurrent: p.overCurrent });
    for (let i = 1; i <= d.nports; i++) if (!ports.has(i)) ports.set(i, { port: i, child: null, connectType: "", overCurrent: 0 });
    d.ports = [...ports.values()].sort((a, b) => a.port - b.port);
  }
  for (const d of devices) {
    if (!d.parent) continue;
    const p = byId.get(d.parent);
    const slot = p?.ports.find((x) => x.port === d.port);
    if (slot) slot.child = d.busid;
  }
  for (const d of devices) {
    let depth = 0;
    for (let p = d.parent; p && byId.has(p); p = byId.get(p)!.parent) depth++;
    d.depth = depth;
  }

  // Depth-first order: sorting by the numeric port path is exactly that.
  devices.sort((a, b) => {
    const ka = portKey(a.busid);
    const kb = portKey(b.busid);
    for (let i = 0; i < Math.max(ka.length, kb.length); i++) {
      if (ka[i] === undefined) return -1;
      if (kb[i] === undefined) return 1;
      if (ka[i] !== kb[i]) return ka[i] - kb[i];
    }
    return 0;
  });

  const ctrls = new Map<string, UsbController>();
  for (const r of raw) {
    if (!r.controller) continue;
    const c = r.controller;
    const u = byPath.get(c.devpath) || {};
    let e = ctrls.get(c.devpath);
    if (!e) {
      const dbName = [u.ID_VENDOR_FROM_DATABASE, u.ID_MODEL_FROM_DATABASE].filter(Boolean).join(" ");
      const name = CTRL_NAMES[c.pciId] || dbName || (c.driver === "dwc2" ? "DWC2 OTG controller" : c.driver === "vhci_hcd" ? "USB/IP virtual host controller" : c.name);
      e = { id: c.name, devpath: c.devpath, name, driver: c.driver, pciId: c.pciId, buses: [] };
      ctrls.set(c.devpath, e);
    }
    e.buses.push(parseInt(r.attrs.busnum) || +r.name.slice(3));
  }
  const controllers = [...ctrls.values()];
  for (const c of controllers) c.buses.sort((a, b) => a - b);
  controllers.sort((a, b) => a.buses[0] - b.buses[0]);
  return { controllers, devices };
}

/** All busids in the subtree rooted at `busid` (inclusive). */
export function subtree(devices: UsbDevice[], busid: string): string[] {
  const out = [busid];
  for (let i = 0; i < out.length; i++) for (const d of devices) if (d.parent === out[i]) out.push(d.busid);
  return out;
}

// ---------- uhubctl ----------

export interface UhubPort {
  port: number;
  status: string; // raw hex word
  power: boolean;
  connect: boolean;
  enable: boolean;
  suspend: boolean;
  overCurrent: boolean;
  speed: string; // lowspeed | highspeed | 5gbps | … ("" = full speed / unknown)
  link: string; // USB3 link state: U0, Rx.Detect, SS.Disabled…
  flags: string[];
  attached: { vidpid: string; desc: string } | null;
}

export interface UhubHub {
  loc: string; // "1-1", "2" (root hub = bus number)
  vidpid: string;
  desc: string;
  usb: string;
  nports: number;
  mode: string; // ppps | ganged | nops
  ports: UhubPort[];
}

const LINK = /^(U0|U1|U2|U3|SS\.Disabled|Rx\.Detect|SS\.Inactive|Polling|Recovery|HotReset|Compliance|Loopback|Resume|SS\.Disable)$/i;

/** Parse `uhubctl` output (listing, or the Current/New status blocks around an action). Later blocks for the same hub win. */
export function parseUhubctl(out: string): UhubHub[] {
  const hubs = new Map<string, UhubHub>();
  let cur: UhubHub | null = null;
  for (const line of out.split("\n")) {
    const h = /^(?:Current|New) status for hub (\S+) \[([0-9a-f]{4}:[0-9a-f]{4})\s*(.*?),\s*USB ([\d.]+),\s*(\d+) ports?,\s*(\w+)\]\s*$/i.exec(line);
    if (h) {
      cur = { loc: h[1], vidpid: h[2].toLowerCase(), desc: h[3].trim(), usb: h[4], nports: +h[5], mode: h[6].toLowerCase(), ports: [] };
      hubs.set(cur.loc, cur);
      continue;
    }
    const p = /^\s+Port (\d+):\s+([0-9a-f]{4})\s*(.*?)\s*$/i.exec(line);
    if (p && cur) {
      let rest = p[3];
      let attached: UhubPort["attached"] = null;
      const b = rest.indexOf("[");
      if (b >= 0) {
        const inner = rest.slice(b + 1).replace(/\]\s*$/, "");
        rest = rest.slice(0, b).trim();
        const m = /^([0-9a-f]{4}:[0-9a-f]{4})\s*(.*)$/i.exec(inner);
        attached = m ? { vidpid: m[1].toLowerCase(), desc: m[2].replace(/^,\s*/, "").trim() } : { vidpid: "", desc: inner.trim() };
      }
      const flags = rest.split(/\s+/).filter(Boolean);
      const has = (f: string) => flags.includes(f);
      cur.ports.push({
        port: +p[1],
        status: p[2].toLowerCase(),
        power: has("power"),
        connect: has("connect"),
        enable: has("enable"),
        suspend: has("suspend"),
        overCurrent: has("oc"),
        speed: flags.find((f) => /^(lowspeed|highspeed|fullspeed|\d+gbps)$/i.test(f)) || "",
        link: flags.find((f) => LINK.test(f)) || "",
        flags,
        attached,
      });
    } else if (!/^\s/.test(line)) cur = null;
  }
  return [...hubs.values()];
}

/** uhubctl location ↔ busid: root hubs are just the bus number. */
export function hubLoc(busid: string): string {
  return busid.startsWith("usb") ? busid.slice(3) : busid;
}
export function locBusid(loc: string): string {
  return loc.includes("-") ? loc : `usb${loc}`;
}

// ---------- Ganged groups and internal links ----------

export interface Group {
  id: string;
  hubs: string[]; // uhubctl locations, all switched together
  label: string;
}

export interface PowerTopology {
  groups: Group[];
  /** "hubloc:port" → why it must not be switched casually (internal hub-to-hub link). */
  internal: Record<string, string>;
}

/**
 * Work out which ports really switch together. uhubctl's own mode is the starting point, but
 * Raspberry Pi onboard hubs claim per-port switching (ppps) while the hardware is ganged:
 * see the uhubctl README, "Raspberry Pi turns power off on all ports".
 */
export function powerTopology(inv: Inventory, hubs: UhubHub[], model: string): PowerTopology {
  const groups: Group[] = [];
  const internal: Record<string, string> = {};
  const inGroup = new Set<string>();
  const byId = new Map(inv.devices.map((d) => [d.busid, d]));

  // Pi 4: VL805. USB2 root hub port 1 is the internal link to the VIA hub (2109:3431); that hub
  // and the USB3 root hub are the same four sockets, ganged.
  for (const c of inv.controllers.filter((c) => c.pciId === "1106:3483")) {
    const roots = inv.devices.filter((d) => d.root && c.buses.includes(d.bus));
    const r2 = roots.find((d) => d.speed <= 480);
    const r3 = roots.find((d) => d.speed >= 5000);
    const via = r2 ? inv.devices.find((d) => d.parent === r2.busid && d.port === 1 && d.hub) : undefined;
    const locs = [via && hubLoc(via.busid), r3 && hubLoc(r3.busid)].filter(Boolean) as string[];
    if (r2) internal[`${hubLoc(r2.busid)}:1`] = "Internal link to the onboard VIA hub. Cutting it drops every USB socket and confuses the hub.";
    if (locs.length) {
      groups.push({ id: `vl805-${c.id}`, hubs: locs, label: `Raspberry Pi 4 sockets: all 4 switch together (hub ${locs.join(" and bus ")} are the USB 2 and USB 3 halves of the same sockets)` });
      locs.forEach((l) => inGroup.add(l));
    }
  }

  // Pi 5: every onboard root hub (RP1's two xHCIs) is one ganged VBUS group.
  if (/Raspberry Pi 5|Compute Module 5/i.test(model)) {
    const locs = inv.devices.filter((d) => d.root && hubs.some((h) => h.loc === hubLoc(d.busid))).map((d) => hubLoc(d.busid)).filter((l) => !inGroup.has(l));
    if (locs.length) {
      groups.push({ id: "pi5", hubs: locs, label: "Raspberry Pi 5 sockets: all 4 share one VBUS switch; power only drops when every port is off" });
      locs.forEach((l) => inGroup.add(l));
    }
  }

  for (const h of hubs) {
    if (h.mode === "ganged" && !inGroup.has(h.loc)) groups.push({ id: `ganged-${h.loc}`, hubs: [h.loc], label: `Hub ${h.loc} has ganged power: all ${h.nports} ports switch together` });
  }

  // Generic: a hard-wired port feeding another hub is an internal daisy-chain link.
  for (const d of inv.devices) {
    for (const p of d.ports) {
      const child = p.child ? byId.get(p.child) : null;
      if (p.connectType === "hardwired" && child?.hub && !internal[`${hubLoc(d.busid)}:${p.port}`]) internal[`${hubLoc(d.busid)}:${p.port}`] = "Hard-wired link to an internal hub. Cutting it confuses the hub circuitry.";
    }
  }
  return { groups, internal };
}

// ---------- Power planning ----------

export type PowerAction = "on" | "off" | "cycle";

export interface PowerPlan {
  action: PowerAction;
  hub: string; // uhubctl location
  port: number | null; // null = whole hub
  commands: string[][]; // uhubctl argument lists (without sudo/binary); cycle = off-set then on-set
  group: Group | null;
  ports: string[]; // "hub:port" switched
  devices: string[]; // busids that lose power
  warnings: string[];
  needs: "none" | "ganged" | "unsafe";
}

/** Busids of a device and every hub above it (cutting any of those ports kills it). */
export function withUpstream(devices: UsbDevice[], busid: string): string[] {
  const byId = new Map(devices.map((d) => [d.busid, d]));
  const out: string[] = [];
  for (let b: string | null = busid; b && byId.has(b); b = byId.get(b)!.parent) out.push(b);
  return out;
}

/**
 * What switching a hub/port would actually do, and how much confirmation it needs.
 * `protect` maps busid → reasons (rootfs, served drive, management NIC).
 */
export function planPower(
  inv: Inventory,
  hubs: UhubHub[],
  topo: PowerTopology,
  protect: Record<string, string[]>,
  req: { hub: string; port?: number | null; action: PowerAction },
): PowerPlan {
  const hub = hubs.find((h) => h.loc === req.hub);
  if (!hub) throw new Error(`Hub ${req.hub} isn't power-switchable (not listed by uhubctl)`);
  if (hub.mode === "nops") throw new Error(`Hub ${req.hub} has no power switching`);
  const port = req.port ?? null;
  if (port !== null && (port < 1 || port > hub.nports)) throw new Error(`Hub ${req.hub} has ports 1–${hub.nports}`);
  const group = topo.groups.find((g) => g.hubs.includes(hub.loc)) || null;
  const force = (loc: string) => (hubs.find((h) => h.loc === loc)?.mode === "ppps" ? [] : ["-f"]);

  // Ganged: the request widens to the whole group whatever port was asked for.
  const targets: { loc: string; port: number | null }[] = group ? group.hubs.map((loc) => ({ loc, port: null })) : [{ loc: hub.loc, port }];
  const ports: string[] = [];
  for (const t of targets) {
    const h = hubs.find((x) => x.loc === t.loc);
    const n = h?.nports || 0;
    for (let i = 1; i <= n; i++) if (t.port === null || t.port === i) ports.push(`${t.loc}:${i}`);
  }

  const byId = new Map(inv.devices.map((d) => [d.busid, d]));
  const devices = new Set<string>();
  for (const pk of ports) {
    const [loc, pn] = pk.split(":");
    const child = byId.get(locBusid(loc))?.ports.find((p) => p.port === +pn)?.child;
    if (child) subtree(inv.devices, child).forEach((b) => devices.add(b));
  }

  const warnings: string[] = [];
  let needs: PowerPlan["needs"] = "none";
  if (req.action !== "on") {
    if (group) {
      needs = "ganged";
      warnings.push(`${group.label}. Switching ${port !== null ? `port ${port}` : "this hub"} switches all ${ports.length} ports listed.`);
    }
    for (const pk of ports) {
      if (topo.internal[pk]) {
        needs = "unsafe";
        warnings.push(`Port ${pk}: ${topo.internal[pk]}`);
      }
    }
    for (const b of devices) {
      for (const why of protect[b] || []) {
        needs = "unsafe";
        warnings.push(`${b} would lose power: it ${why}.`);
      }
    }
  }

  const cmd = (a: string) => targets.map((t) => ["-l", t.loc, ...(t.port !== null ? ["-p", String(t.port)] : []), "-a", a, ...force(t.loc)]);
  const commands = req.action === "cycle" ? [...cmd("off"), ...cmd("on")] : cmd(req.action);
  return { action: req.action, hub: hub.loc, port, commands, group, ports, devices: [...devices], warnings, needs };
}

// ---------- Kernel log filter ----------

export interface KernelLine {
  ts: string; // ISO timestamp from dmesg, "" if none
  level: "error" | "warn" | "info";
  text: string;
}

const K_SKIP = /New USB device strings|: Product: |: Manufacturer: |: SerialNumber: |New USB device found|registered new (interface |device )?driver|USB hub found|\d+ ports? detected/;
const K_ERR = /over-?current|cannot enumerate|unable to enumerate|device descriptor read|device not accepting address|can't set config|cannot reset|Cannot enable|not enough bandwidth|HC died|host controller halted|hub_port_status failed|cannot get .*descriptor|config .* has an invalid|unable to read config|-EPROTO|error -(71|110|32|19)/i;
const K_WARN = /\breset (?:low|full|high|super)[\w +-]*USB device number|disabled by hub \(EMI\?\)|over-?current condition cleared|Cannot use|usbip.*(detached|error)/i;
const K_INFO = /new (?:low|full|high|super|wireless)[\w +-]*USB device number|USB disconnect|now attached to tty|now disconnected from tty|: tty\w+: USB ACM device|USB Mass Storage device detected|scsi host\d+: (usb-storage|uas)|Attached SCSI removable disk|^(cdc_acm|ftdi_sio|cp210x|ch341|pl2303|usbserial|option|gs_usb|peak_usb|usb-storage|uas|uvcvideo|cdc_ether|cdc_ncm|r8152|ax88179_178a|asix|qmi_wwan|rndis_host|btusb|snd-usb-audio|dvb_usb_rtl28xxu|rtl2832|usbtmc|xpad)\b|usbip-host \S+: usbip-host: register new device|hid-generic .*: input|: USB HID v|new USB bus registered|USB bus \d+ deregistered|vhci_hcd \S+: (Device attached|devid)/i;

/** Classify one `dmesg --time-format iso` line; null if it isn't USB-worthy. */
export function filterKernelLine(line: string): KernelLine | null {
  const m = /^(\d{4}-\d\d-\d\dT[\d:,.]+[+-]\d\d:?\d\d)\s+(.*)$/.exec(line.trim());
  const ts = m ? m[1] : "";
  const text = (m ? m[2] : line).trim();
  if (!text || K_SKIP.test(text)) return null;
  const usbish = /\busb\b|usb[-_]|xhci|ehci|ohci|dwc2|\bhub\b|tty(USB|ACM)|usbip|vhci/i.test(text) || K_INFO.test(text);
  if (!usbish) return null;
  if (K_ERR.test(text)) return { ts, level: "error", text };
  if (K_WARN.test(text)) return { ts, level: "warn", text };
  if (K_INFO.test(text)) return { ts, level: "info", text };
  return null;
}

// ---------- ss ----------

/** Peers of established TCP connections on the given local ports, from `ss -tnH state established`. */
export function parseSsPeers(out: string, localPorts: number[]): { local: number; peer: string }[] {
  const res: { local: number; peer: string }[] = [];
  for (const line of out.split("\n")) {
    const f = line.trim().split(/\s+/);
    if (f.length < 4) continue;
    // With a state filter, columns are: Recv-Q Send-Q Local:Port Peer:Port [process]
    const local = f[2];
    const peer = f[3];
    const lp = parseInt(local.slice(local.lastIndexOf(":") + 1));
    if (localPorts.includes(lp)) res.push({ local: lp, peer: peer.replace(/^\[::ffff:([\d.]+)\]/, "$1").replace(/^::ffff:/, "") });
  }
  return res;
}
