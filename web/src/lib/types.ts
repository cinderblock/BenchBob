// The status a Linux bob pushes over SSE every ~2 s (GET /api/status), and what a
// microcontroller node answers on GET /api/status (firmware/proto/src/api.rs). Written by
// hand and kept in sync with the backends; there is no schema between them on purpose.

export interface NetIface {
  name: string;
  state: "UP" | "DOWN" | string;
  ip?: string;
  prefix?: number;
  gateway?: string;
  gateway6?: string;
  ipv6?: string[];
  mac?: string;
  mtu?: number;
  metric?: number;
  rxRate: number;
  txRate: number;
  wifi?: { ssid: string; signal: number; bitrate: string };
}

export interface Partition {
  name: string;
  size: number;
  fstype?: string;
  label?: string;
  mountpoint?: string;
}

export interface Operation {
  id: string;
  type: string;
  device: string;
  status: string;
  progress: number;
  message: string;
  imageName?: string;
  scanResult?: { bytesScanned: number; signatures: { name: string; count: number; first: number[] }[] };
}

export interface NetbootServer {
  name: string;
  url: string;
  ip: string | null;
  version: string;
  caps: string[];
}

export interface Drive {
  name: string;
  model?: string;
  size: number;
  serial?: string;
  rev?: string;
  /** `sd`: a card in the SD slot of a machine that doesn't run from it. Unset: USB. */
  bus?: "sd";
  usbVersion?: string;
  readRate: number;
  writeRate: number;
  hasPartitions: boolean;
  children?: Partition[];
  operations?: Operation[];
}

export interface BootDevice {
  path: string;
  label: string;
  serial?: string;
  product?: string;
  chip: string;
}

export interface RpibootResult {
  path: string;
  kind: string;
  at: number;
  ok: boolean;
  message: string;
  metadata?: Record<string, string>;
  installed?: { label: string; bootConf?: string | null; buildDate?: string; lastWrite?: { imageLabel: string; at: number; bootConf: string } };
  board?: { model: string; memory: string; revision: string };
}

export interface Power {
  now: string[];
  sinceBoot: string[];
  undervoltageEvents?: number;
  lastUndervoltage?: number;
  inputVolts?: number;
  raw: string;
}

export interface UpdateInfo {
  available: boolean;
  latest?: string;
  current?: string;
  url?: string;
}

export interface BoardInfo {
  id: string;
  name?: string;
  model?: string;
  tested: boolean;
  soc?: string;
  notes?: string[];
  sources?: string[];
  capabilities?: Record<string, any>;
}

export interface PeriphEntry {
  id: string;
  title: string;
  description: string;
  disabled: boolean;
  active: boolean;
  summary: string;
}

export interface Peripherals {
  list: PeriphEntry[];
  state: Record<string, any>;
}

export interface Status {
  hostname: string;
  uptime: number;
  timestamp: number;
  version: string;
  buildVersion?: string;
  update?: UpdateInfo;
  board?: BoardInfo;
  system: {
    model: string;
    ramMB?: number;
    cpuUsage: number;
    cpuTemp: number;
    cores: number;
    memPercent: number;
    memTotal: number;
    memAvailable: number;
  };
  power?: Power | null;
  network: NetIface[];
  rootStorage?: { kind: string; source?: string; size?: number; usePercent?: number } | null;
  /** The board's own LEDs, which the header's hold button lights (POST /api/onboard-leds). */
  onboardLeds?: OnboardLed[];
  rootDevice?: { name: string; size: number; children?: Partition[] } | null;
  drives: Drive[];
  nbdExports: string[];
  bootDevices: BootDevice[];
  rpibootInstalled: boolean;
  rpibootResults: RpibootResult[];
  filesRoot?: string;
  /** BootBob servers on this network (mDNS): boot media can be made for them. */
  netbootServers?: NetbootServer[];
  operations: Operation[];
  peripherals?: Peripherals;
  history?: Record<string, number[]>;
}

/** GET /api/status on a node. */
export interface NodeStatus {
  hostname: string;
  uptime: number;
  version: string;
  /** Firmware version + embedded UI hash, as stamped into the page the node serves. */
  build?: string;
  board: string;
  boardName: string;
  chip: string;
  network: { mac: string; wifi: "unprovisioned" | "connecting" | "connected" | string; ssid: string | null; rssi: number | null; ip: string | null };
  heap: { free: number; used: number } | null;
  onboardLeds?: OnboardLed[];
}

/** An LED on the board itself: a Pi's ACT/PWR (id is its sysfs name) or a node's (an index). */
export interface OnboardLed {
  id: string | number;
  name: string;
  /** green, red, rgb, ...; "" when unknown. */
  color: string;
  /** What it shows the rest of the time. */
  normally: string;
  held: boolean;
}
