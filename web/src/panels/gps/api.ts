// GPS driver (backend/crates/gps): status shapes the panel reads.

export interface GpsCandidate {
  key: string;
  path: string;
  vendor?: string;
  model?: string;
  usbId: string;
  serial?: string;
  kind: string;
  label?: string;
  chosen?: boolean;
  holders: { comm: string; pid: number }[];
}

export interface Sat {
  gnss: string;
  svid?: number;
  prn?: number;
  el?: number | null;
  az?: number | null;
  snr?: number | null;
  used?: boolean;
}

export interface Fix {
  mode?: number;
  fix?: string;
  lat?: number;
  lon?: number;
  altMSL_m?: number;
  altHAE_m?: number;
  speed_ms?: number;
  track_deg?: number;
  eph_m?: number;
  epv_m?: number;
  hdop?: number;
  pdop?: number;
  vdop?: number;
  time?: string;
  toffOffset_ms?: number;
  nmeaOffset_ms?: number;
  ppsOffset_us?: number;
  ppsAge_s?: number;
  constellations?: Record<string, { used: number; visible: number }>;
  satsUsed?: number;
  satsVisible?: number;
}

export interface GpsState {
  candidates?: GpsCandidate[];
  settings?: { port?: number; share?: boolean; readOnly?: boolean; pps?: boolean };
  running?: boolean;
  device?: string;
  ppsDevices?: string[];
  lastExit?: { code: number; message: string };
  fix?: Fix;
  satellites?: Sat[];
  share?: boolean;
  port?: number;
  connect?: string;
  since?: number;
  readOnly?: boolean;
  gpsd?: { errors?: string[]; version?: string; devices?: { driver?: string; subtype?: string; subtype1?: string; bps?: number }[] };
  log?: { line: string }[];
}
