// Wi-Fi panel types (backend/crates/wifi). Status carries only sequence numbers
// for the bulky data; the panel fetches /api/wifi/{radios,scan,results,profiles} when one
// changes.

export interface Channel {
  ch: number;
  mhz: number;
  disabled?: boolean;
  radar?: boolean;
  noIR?: boolean;
  maxPower?: number;
}

export interface BandInfo {
  band: "2.4" | "5" | "6" | "60";
  channels: Channel[];
  maxWidth: number;
  ht?: boolean;
  vht?: boolean;
  he?: boolean;
  eht?: boolean;
}

export interface Radio {
  phy: string;
  label?: string;
  ifaces: { name?: string }[];
  onboard: boolean;
  bus: string;
  usb?: { vendor?: string; model?: string; id: string } | null;
  driver?: string;
  use: { kind: string; text: string };
  nm: { state: string }[];
  unmanagedByUs: string[];
  bandInfo: BandInfo[];
  bands: string[];
  iftypes: string[];
  monitor: boolean;
  sae: boolean;
  netns: boolean;
  regCountry?: string;
}

export interface RadiosResp {
  radios: Radio[];
  reg: { country?: string; dfsRegion?: string; changedFrom?: string | null; phys: { scope: string; country: string; selfManaged?: boolean }[] };
  ouiSource?: string | null;
  savedNetworks?: { ssid: string }[];
}

export interface Bss {
  bssid: string;
  ssid: string;
  hidden: boolean;
  ssidHex?: string;
  vendor?: string;
  localMac?: boolean;
  freq: number;
  band: string;
  channel: number;
  width: number | "80+80";
  centerFreq: number;
  centerChannel: number;
  signal: number | null;
  security: string;
  securityKind: string;
  akm: string[];
  pairwise: string[];
  group?: string;
  pmf: string;
  standards: string[];
  generation?: string;
  beaconInterval?: number;
  capability: string[];
  stations?: number | null;
  utilization?: number | null;
  country?: string;
  wps?: { state?: string; locked?: boolean; manufacturer?: string; model?: string; device?: string } | null;
  associated?: boolean;
  firstSeen: number;
  lastSeen: number;
  radio: string;
  /** [time, dBm] */
  history: [number, number][];
}

export interface ScanResp {
  scan: { at?: number; iface?: string; radio?: string; method?: string; durationMs: number; count: number; error?: string; survey?: { freq: number; inUse?: boolean; busyPct?: number | null; noise?: number | null; activeMs?: number | null }[] | null };
  bss: Bss[];
  now: number;
  historySince?: number;
  historyMaxAgeMs: number;
  historyMax: number;
}

export interface Step {
  dt: number;
  ok?: boolean | null;
  text: string;
}

export interface Running {
  id: string;
  ssid: string;
  phy: string;
  stage: string;
  startedAt: number;
  link?: { signal?: number | null; txRate?: string; rxRate?: string } | null;
  samples: { signal?: number | null }[];
  timeline: Step[];
}

export interface TestResult {
  id: string;
  ok: boolean;
  cancelled?: boolean;
  ssid: string;
  phy: string;
  security: string;
  startedAt: number;
  endedAt?: number;
  link?: { signal?: number | null; bssid?: string; freq?: number; txRate?: string; rxRate?: string } | null;
  address?: string;
  tests: { kind: string; ok: boolean; data?: { avg?: number } }[];
  error?: string;
  restore?: { ok: boolean; text: string } | null;
  samples: { signal?: number | null }[];
  lease?: Record<string, string> | null;
  uplinkBefore?: string;
  uplinkAfter?: string;
  timeline: Step[];
  wpaLog?: string[];
}

export interface Profile {
  id: string;
  name: string;
  ssid: string;
  security?: string;
  bssid?: string;
  ip?: string;
  pingGateway?: boolean;
  dns?: string[];
  http?: string[];
  ping?: string[];
  count?: number;
  holdS?: number;
  useSavedKey?: boolean;
  hasPsk?: boolean;
  static?: { address: string; gateway?: string; dns?: string[] };
  iperf?: { server: string; port?: number; seconds?: number; reverse?: boolean };
}

export interface WifiState {
  active: boolean;
  summary: string;
  radiosSeq: number;
  resultsSeq: number;
  profilesCount: number;
  scan: { seq: number; scanning?: string[]; watchers?: number; auto?: { enabled?: boolean; intervalS?: number; watchIntervalS?: number; radio?: string } };
  running: Running[];
}
