// RTL-SDR (peripherals/drivers/sdr.ts): status, mode switch, probe, rtl_433 results.

export interface SdrStick {
  key: string;
  usbId: string;
  knownAs: string;
  manufacturer?: string;
  product?: string;
  serial?: string;
  bus: string;
  port?: string;
  drivers: string[];
  dvbClaimed: boolean;
  probe?: { t: number; tuner?: string; gains?: number[]; eeprom?: Record<string, string>; error?: string } | null;
}

export interface SdrState {
  active: boolean;
  summary: string;
  sticks: SdrStick[];
  dvb: { blacklisted: boolean; loaded?: boolean; where?: string[]; fix: string };
  mode: "off" | "rtl_tcp" | "rtl_433";
  since?: number;
  opts?: { port: number; bind: string; sampleRate: number; gain: number | string; ppm: number };
  clients?: string[];
  lastExit?: { mode: string; code: number; message: string } | null;
  probing?: boolean;
  presets: Record<string, { label: string }>;
  defaults?: { tcpPort: number; settings?: { tcp?: Record<string, any>; rtl433?: Record<string, any> } };
  rtl433?: { sensors: number; recent: number; messages: number };
}

export interface Rtl433Device {
  model: string;
  id?: string | number;
  channel?: string | number;
  protocol?: number;
  fields: Record<string, any>;
  hist: Record<string, [number, number][]>;
  rssi?: number | null;
  snr?: number | null;
  freq?: number | null;
  lastSeen: number;
  count: number;
}

export interface Rtl433Result {
  seq: number;
  devices: Rtl433Device[];
  messages: { seq: number; t: number; msg: unknown }[];
  log?: { line: string }[];
}

export type ModeRequest =
  | { mode: "off" }
  | { mode: "rtl_tcp"; force?: boolean; port: number; bind: string; gain: number; ppm: number; sampleRate: number }
  | { mode: "rtl_433"; force?: boolean; preset: string; hop?: number; gain: string; ppm: number };
