// LED strings (`/api/leds`): the same shapes on a node (firmware/proto/src/api/leds.rs)
// and, later, on a Linux bob's LED strips panel (plans/gpio-led-strips.md, "API").

export interface LedTiming {
  t0hNs: number;
  t1hNs: number;
  periodNs: number;
  resetUs: number;
}

export interface LedPower {
  maPerChannel: number;
  idleMa: number;
  maxMa: number;
}

export interface LedPattern {
  name: string;
  color?: string;
  background?: string;
  a?: string;
  b?: string;
  spread?: number;
  periodMs?: number;
  width?: number;
  speed?: number;
  density?: number;
}

export interface LedString {
  id: number;
  name: string;
  chip: string;
  kind: "one-wire" | "clocked";
  pin: number;
  clockPin: number | null;
  pixels: number;
  order: string;
  timing: LedTiming | null;
  clockHz: number | null;
  brightness: number;
  gamma: boolean;
  invert: boolean;
  power: LedPower | null;
  source: "pattern" | "pixels" | "network";
  pattern: LedPattern | null;
  /** off: output control is off, so the string is dark. */
  state: "running" | "off" | "pending" | "error";
  error: string | null;
  estimateMa: number | null;
}

export interface LedChip {
  name: string;
  kind: "one-wire" | "clocked";
  description: string;
}

export interface LedsState {
  outputEnabled: boolean;
  capabilities: { maxStrings: number; maxPixels: number; maxClockHz: number; chips: LedChip[]; patterns: string[]; pins: number[] } | null;
  strings: LedString[];
}

/** Each pattern's parameters, for the pattern editor. */
export const PATTERN_PARAMS: Record<string, (keyof LedPattern)[]> = {
  off: [],
  solid: ["color"],
  gradient: ["a", "b"],
  rainbow: ["spread", "periodMs"],
  chase: ["color", "background", "width", "speed"],
  breathe: ["color", "periodMs"],
  twinkle: ["color", "density", "periodMs"],
  "order-test": [],
  ruler: [],
};

export const PARAM_LABEL: Record<string, string> = {
  color: "Colour",
  background: "Background",
  a: "From",
  b: "To",
  spread: "Pixels per cycle",
  periodMs: "Period (ms)",
  width: "Width (px)",
  speed: "Speed (px/s)",
  density: "Density (0–255)",
};

/** Colour orders offered in the picker; any other R/G/B(/W) permutation can be typed. */
export const ORDERS = ["GRB", "RGB", "BGR", "BRG", "RBG", "GBR", "GRBW", "RGBW", "WRGB"];
