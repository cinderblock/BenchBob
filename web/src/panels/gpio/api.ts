// GPIO driver (peripherals/drivers/gpio.ts): types as the dashboard uses them.

import type { BoardInfo } from "../../lib/types";

/** Pi mode (BCM gpiomem): one pin from /ws/gpio. */
export interface PiPin {
  gpio: number;
  header?: number | null;
  fsel: string; // "ip" | "op" | "a0".."a5" | ...
  func: string;
  level: number | null;
  pull?: string;
  consumer?: string;
  refusal?: string | null;
}

export interface HeaderPos {
  pin: number;
  kind: "gpio" | "gnd" | "3v3" | "5v" | string;
  gpio: number;
}

/** gpiochip boards: a header pin mapped onto a chip line. */
export interface ChipPin {
  pin: number;
  kind: string; // gpio | gnd | 3v3 | 5v | nc | other
  label?: string;
  soc?: string;
  chipName?: string;
  line?: number;
  lineName?: string;
  direction?: string;
  consumer?: string;
  flags?: string[];
  used?: boolean;
  funcs?: string[];
  note?: string;
}

export interface ChipHeader {
  name: string;
  pins: ChipPin[];
}

export interface ChipLine {
  offset: number;
  name?: string;
  consumer?: string;
  flags: string[];
  used?: boolean;
  watched?: boolean;
}

export interface Chip {
  name: string;
  label: string;
  device?: string;
  usb?: boolean;
  count: number;
  error?: string;
  lines: ChipLine[];
}

export interface GpioState {
  mode?: "gpiochip" | string;
  outputEnabled?: boolean;
  rebootRequired?: boolean;
  header?: HeaderPos[];
  board?: BoardInfo & { capabilities?: { headers?: unknown[] } };
  bootConfig?: { kind: string } | null;
}

/** /ws/gpio "state" message. */
export interface WsState {
  type: "state";
  mode?: string;
  error?: string;
  chips?: Chip[];
  pins?: PiPin[];
  outputEnabled?: boolean;
  watched?: string[];
  headers?: ChipHeader[];
  board?: GpioState["board"];
}

export interface Level {
  level?: number | null;
  error?: string;
}

export interface Staged {
  diff?: string;
  message?: string;
  summary: string[];
  warnings: string[];
  token: string;
  configPath: string;
}

export interface Applied {
  backup: string;
  summary: string[];
}

export interface OverlayInfo {
  path: string;
  testCopy?: boolean;
  label?: string;
  prefix?: string;
  overlayDirs: string[];
  installed: { name: string; what?: string }[];
  enabled: { name: string; installed: boolean }[];
  user: string[];
  conflicts: { pin: number; users: string[] }[];
}

export interface IfaceInfo {
  configPath: string;
  testCopy?: boolean;
  configured: any;
  live: any;
  catalog: { extraI2c: Record<string, { choices: Record<string, number[]>; def: string }>; extraUart: Record<string, { tx: number; rx: number; cts: number; rts: number }> } | null;
  pins: { gpio: number; owner: string; role: string }[];
  conflicts: { gpio: number; users: string[] }[];
}
