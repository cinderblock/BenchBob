// Bluetooth LE panel types (backend/crates/ble). Status comes over SSE; the device
// list over /ws/ble ({type:"devices"} roughly once a second while it changes).

export interface BleController {
  name: string;
  address?: string;
  bus: string;
  powered?: boolean;
  powerState?: string;
  rfkill?: { soft?: boolean; hard?: boolean };
}

export interface BleState {
  active: boolean;
  summary: string;
  controllers: BleController[];
  defaultController: string;
  restoreRadio: boolean;
  scan: { controller: string; live?: boolean; until?: number; packets: number; stopping?: boolean } | null;
  lastScan?: { ended: number; ctrl: string; packets: number } | null;
  error?: string;
  recent: number;
  known: number;
}

export interface BleBeacon {
  kind: string;
  label: string;
  fields?: Record<string, unknown>;
}

export interface BleDevice {
  addr: string;
  addrType: string;
  name?: string;
  kind?: string;
  rssi: number | null;
  rssiHist?: number[];
  connectable?: boolean;
  scannable?: boolean;
  firstSeen: number;
  lastSeen: number;
  count: number;
  controller: string;
  lastPdu: string;
  txPower?: number | null;
  flags?: number | null;
  appearance?: number | null;
  beacons: BleBeacon[];
  mfr: { company: number; companyName?: string; data: string }[];
  uuids: { uuid: string; name?: string }[];
  svc: { uuid: string; name?: string; data: string }[];
  adv?: string;
  scanRsp?: string;
}

export interface BlePrefs {
  live: boolean;
  q: string;
  minRssi: string;
  beacons: boolean;
  sort: "rssi" | "seen" | "first" | "name";
}

export const bleApi = {
  scan: (duration: number, controller?: string) => ["POST", "/api/ble/scan", { duration, controller }] as const,
  stop: () => ["POST", "/api/ble/stop"] as const,
  clear: () => ["DELETE", "/api/ble/devices"] as const,
  settings: (s: { controller?: string; restoreRadio?: boolean }) => ["POST", "/api/ble/settings", s] as const,
};
