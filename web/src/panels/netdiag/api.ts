// Wired-network panel types (backend/crates/netdiag).

export interface Neighbor {
  name?: string;
  description?: string;
  portId: string;
  portDescription?: string;
  chassisId: string;
  chassisIdType: string;
  vlans: { id: number; name?: string; pvid?: boolean }[];
  poe?: Record<string, string> | null;
  mgmtIps: string[];
  capabilities: { type: string; enabled: boolean }[];
  protocol: string;
  age: string;
  ttl?: number | null;
  medDeviceType?: string;
}

export interface NdIface {
  name: string;
  carrier: boolean;
  link?: { speed?: number; duplex?: string; autoneg?: boolean; partner?: string[]; advertised?: string[]; partnerAutoneg?: boolean; masterSlave?: string } | null;
  usb?: boolean;
  usbDesc?: string;
  errors: { name: string; value: number; delta: number }[];
  eee?: { supported: boolean; enabled: boolean; active: boolean; partner: string[] } | null;
  addrs: { family: string; addr: string; prefix: number }[];
  gateway?: string;
  dns: string[];
  driver?: string;
  firmware?: string;
  bus?: string;
  mac?: string;
  mtu: number;
  carrierChanges: number;
  neighbor?: Neighbor | null;
}

export interface IperfResult {
  bps: number;
  retransmits?: number | null;
  jitterMs?: number | null;
  lostPct?: number;
}

export interface IperfRun {
  id: string;
  host: string;
  port: number;
  status: "running" | "done" | "error";
  error?: string;
  started: number;
  duration: number;
  reverse: boolean;
  udp: boolean;
  parallel: number;
  bitrate?: string;
  points: { t: number; bps: number }[];
  result?: IperfResult | null;
}

export interface IperfServer {
  running: boolean;
  port: number;
  offAt?: number | null;
  log: string[];
  current?: { peer: string; protocol: string; reverse: boolean; bps: number } | null;
  tests: { t: number; peer: string; summary?: (IperfResult & { protocol: string; reverse: boolean }) | null; error?: string }[];
}

export interface Ping {
  host: string;
  target?: string;
  sent: number;
  received: number;
  lossPct: number;
  min?: number;
  avg?: number | null;
  max?: number;
  mdev?: number | null;
  t: number;
}

export interface Checks {
  pings?: Ping[];
  ping?: Ping;
  dns?: { t: number; results: { server: string; name: string; ms?: number | null; error?: string; addresses: string[] }[] };
  http?: { t: number; results: { url: string; status?: number | null; error?: string; ms?: number | null }[] };
  mtu?: { host: string; pathMtu?: number | null; interfaceMtu: number; note: string; steps: unknown[]; t: number };
}

export interface Preset {
  host: string;
  label: string;
}

export interface AuthEntry {
  host: string;
  username?: string;
  ready: boolean;
  hasPassword: boolean;
  hasKey: boolean;
  pkcs1?: boolean;
  usersLine?: string;
}

export interface NetdiagState {
  active: boolean;
  summary: string;
  ifaces: NdIface[];
  lldp: { enabled: boolean; running: boolean; since: number; error?: string };
  dns: { nameservers: string[] };
  iperf: { runs: IperfRun[]; server: IperfServer };
  checks: Checks;
  presets?: { iperf: Preset[]; ping: Preset[]; auth?: AuthEntry[] };
}

export interface IperfForm {
  host: string;
  port: number;
  duration: number;
  reverse: boolean;
  udp: boolean;
  bitrate: string;
  parallel: number;
}
