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

export interface LedNetIn {
  protocol: "sacn" | "artnet" | "ddp";
  universe: number;
  startChannel: number;
  ddpOffset: number;
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
  /** While receiving: the pattern that comes back when the network lets go. */
  pattern: LedPattern | null;
  network?: LedNetIn | null;
  /** The last packets this string's network input took (null: none yet). */
  receiving?: { active: boolean; from: string; packets: number; ageMs: number } | null;
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
  capabilities: {
    maxStrings: number;
    maxPixels: number;
    maxClockHz: number;
    chips: LedChip[];
    patterns: string[];
    pins: number[];
    /** Linux: every header GPIO, which kinds of string it can carry and why not. */
    pinInfo?: LedPin[];
    /** Linux: where one-wire strings can go (or how to get such a pin). */
    oneWireHint?: string;
    /** Linux: the network receivers in use (listening, or why not). */
    receivers?: Record<string, { listening: boolean; error: string | null } | number[]>;
  } | null;
  strings: LedString[];
}

export interface LedPin {
  gpio: number;
  header: number | null;
  oneWire: boolean;
  clocked: boolean;
  /** What drives a string there: "SPI0 MOSI", "PWM0"… */
  via: string | null;
  refusal: string | null;
}

export const PROTOCOLS: Record<LedNetIn["protocol"], string> = { sacn: "sACN (E1.31)", artnet: "Art-Net", ddp: "DDP" };

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
