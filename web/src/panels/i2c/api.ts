// I²C driver (peripherals/drivers/i2c.ts): status shapes the panel reads.

export interface Reading {
  k: string;
  label: string;
  v: number | string | boolean | null;
  d?: number;
  unit?: string;
}

export interface DevOpt {
  key: string;
  label: string;
  type: "select" | "number";
  value: number | string;
  unit?: string;
  choices?: { v: number | string; label: string }[];
}

export interface I2cDevice {
  addr: number;
  hex: string;
  name?: string;
  what?: string;
  label: string;
  chip?: string;
  claimed?: string;
  confidence?: "id" | "likely" | "guess" | string;
  note?: string;
  hint?: string;
  hints: string[];
  error?: string;
  readAt?: number;
  readout?: {
    stale?: boolean;
    values?: Reading[];
    pins?: { name: string; level: number; dir?: string; pull?: boolean }[];
    channels?: { ch: number; duty: number; full?: string }[];
    note?: string;
  };
  hist: Record<string, number[]>;
  canRead?: boolean;
  canMeasure?: boolean;
  measureNote?: string;
  pollable?: boolean;
  poll?: boolean;
  auto?: boolean;
  interval: number;
  opts: DevOpt[];
  pointerUnsafe?: boolean;
}

export interface I2cBus {
  bus: number;
  label: string;
  hz?: number;
  funcs: string[];
  scanning?: boolean;
  lastScan?: number;
  scanMs?: number;
  autoScan: number;
  plainI2c?: boolean;
  error?: string;
  devices: I2cDevice[];
}

export interface I2cState {
  buses: I2cBus[];
  hiddenBuses?: string[];
}

/** Raw read/write/transfer replies. */
export interface RawReply {
  bytes?: number[];
  reads?: number[][];
  wrote?: number[];
  word?: number;
  wordBigEndian?: number;
  regs?: (number | null)[];
}
