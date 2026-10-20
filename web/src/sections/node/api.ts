// The microcontroller node API (firmware/proto/src/api.rs, firmware/README.md "API"): the
// same REST shapes as the Linux server for GPIO and I²C, without WebSockets or SSE, so
// these sections poll.

export interface NodePin {
  gpio: number;
  header: number | null;
  name: string;
  /** "input" / "output", or what the firmware uses the pin for (then it can't be driven). */
  func: string;
  level: number | null;
  pull: "up" | "down" | "none" | null;
  refusal: string | null;
  note: string | null;
}

export interface NodeGpio {
  outputEnabled: boolean;
  board: string;
  pins: NodePin[];
}

export interface NodeI2cBus {
  bus: number;
  dev: string;
  label: string;
  sda: number;
  scl: number;
  hz: number;
  quickWrite: boolean;
  /** Node uptime (ms) at the last scan. */
  lastScanUptimeMs: number | null;
  scanMs: number;
  devices: { addr: number; hex: string; address: string; chip: null }[];
}

export interface NodeI2cRead {
  register?: number;
  word?: number;
  wordBigEndian?: number;
  bytes: number[];
}
