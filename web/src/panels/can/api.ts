// CAN (peripherals/drivers/can.ts): SocketCAN interfaces, SLCAN/vcan, recordings,
// socketcand, and the /ws/can/:iface monitor stream.

export interface CanIface {
  name: string;
  key: string;
  kind: "can" | "vcan" | "slcan" | string;
  up: boolean;
  state?: string;
  busy?: string | null;
  bitrate?: number;
  samplePoint?: number;
  dbitrate?: number;
  dsamplePoint?: number;
  fdOn?: boolean;
  fdCapable?: boolean;
  ctrl: string[];
  ctrlSupported?: string[] | null;
  restartMs?: number;
  rates?: { fps?: number; txps?: number; load?: number | null; errps?: number };
  berr?: { tx: number; rx: number } | null;
  xstats?: Record<string, number> | null;
  label?: string;
  driver?: string;
  usb?: { serial?: string } | null;
  slcanTty?: string;
  clock?: number;
  autobitrate?: { bitrate: number; step: number; of: number } | null;
  lastAutobitrate?: { at: number; text: string; applied?: boolean; note?: string } | null;
  recording?: { file: string; frames: number; bytes: number } | null;
  reader?: string;
  bringUpOnPlug?: boolean;
  saved?: { bringUpOnPlug?: boolean } | null;
}

export interface CanState {
  ifaces: CanIface[];
  slcan: { candidates: { kind: string; tty: string; serial?: string; attached?: string | null }[] };
  recordings: { file: string; size: number; mtime: number }[];
  socketcand: { enabled: boolean; port: number; beacon: boolean; listening: boolean; error?: string; beaconTo?: string; clients: { peer: string; iface?: string; mode: string; rx: number; tx: number; dropped?: number }[] };
  standardBitrates: number[];
  slcanBitrates: number[];
}

export interface CanFrame {
  ts: number;
  id: number;
  ext?: boolean;
  rtr?: boolean;
  fd?: boolean;
  brs?: boolean;
  len: number;
  data: string;
  dir: string;
  err?: boolean;
  text?: string;
}

export interface CanStats {
  type: "stats";
  up: boolean;
  rates?: { fps?: number; load?: number | null };
  idCount: number;
  totals: { rx: number; tx: number; err: number };
  drops?: number;
  ids?: { id: number; ext?: boolean; fd?: boolean; dir?: string; len: number; count: number; period: number; data: string; hot: number; rtr?: boolean; last: number }[];
  errors?: { ts: number; text: string; tec?: number; rec?: number }[];
  jobs?: { id: number; frame: string; periodMs: number; sent: number; errors?: number; lastErr?: string }[];
}

export interface AutobitrateResult {
  text: string;
  note?: string;
  bitrate?: number;
  trials: { bitrate: number; skipped?: string; frames: number; errors: number; busErrors: number }[];
}

/** A frame for /send and /periodic: cansend syntax, or fields. */
export type FrameReq = { cansend: string } | { id: string; data: string; ext?: boolean; rtr?: boolean; fd?: boolean; brs?: boolean };
