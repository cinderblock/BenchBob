// Serial ports (peripherals/drivers/serial.ts portStatus()): REST under /api/serial/:id/*,
// and /ws/serial/:id (binary = raw bytes both ways, JSON text = control/status).

export interface SerialConfig {
  baud: number;
  dataBits: number;
  parity: "none" | "even" | "odd" | "mark" | "space";
  stopBits: number;
  flow: "none" | "rtscts" | "xonxoff";
}

export interface ModemLines {
  dtr: boolean;
  rts: boolean;
  cts: boolean;
  dsr: boolean;
  dcd: boolean;
  ri: boolean;
}

export interface Bridge {
  enabled: boolean;
  port: number;
  listening: boolean;
  error?: string | null;
  clients: string[];
}

export interface FlashJob {
  tool: string;
  action: string;
  label: string;
  state: "running" | "done" | "error" | "cancelled" | string;
  phase?: string;
  progress: number;
  message?: string;
  logTail?: string[];
}

export interface SerialPort {
  id: string;
  path: string;
  byId?: string;
  kind: string;
  name: string;
  vendor?: string;
  model?: string;
  vidpid?: string;
  serial?: string;
  usbDriver?: string;
  family?: string;
  resets: ("esp" | "esp-boot" | "dtr" | "touch1200")[];
  flashTools: ("esptool" | "avrdude")[];
  console: boolean;
  present: boolean;
  state: "open" | "closed" | "flashing" | "autobaud" | string;
  open: boolean;
  error?: string | null;
  config: SerialConfig;
  configLabel: string;
  lines: ModemLines | null;
  rxBytes: number;
  txBytes: number;
  openOnPlug: boolean;
  clients: { ws: number; tcp: number; rfc2217: number };
  bridges: { tcp: Bridge; rfc2217: Bridge };
  lock: { owner: string; since: number } | null;
  flash: FlashJob | null;
  autobaud?: { best?: { baud: number; confidence: number } | null; applied?: boolean } | null;
}

export interface SerialState {
  ports: SerialPort[];
}

/** Messages on /ws/serial/:id (besides binary data). */
export type SerialWsMsg =
  | { type: "hello"; port: SerialPort }
  | { type: "history"; chunks: [number, string][] }
  | { type: "status"; port: SerialPort }
  | { type: "lines"; lines: ModemLines | null }
  | { type: "flash"; line?: string; job: FlashJob | null }
  | { type: "error"; message: string };
