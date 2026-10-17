// Pure helpers for the camera driver: which V4L2 nodes are cameras, and parsers for
// v4l2-ctl's text output. Kept free of I/O so they can be unit-tested with fixtures.

import type { Props } from "../core.ts";

// ---------- Detection ----------

/**
 * A node is a camera worth listing when udev says it can capture and it hangs off USB.
 * The USB requirement is what keeps out the Pi's bcm2835-isp outputs (video14-16, 21-23),
 * which udev also marks `:capture:` but which are memory-to-memory pipeline stages.
 * CSI cameras are found separately via libcamera, not through this filter.
 */
export function isUsbCaptureNode(p: Props, usbParent: Props | null): boolean {
  if (p.SUBSYSTEM !== "video4linux" || !/^\/dev\/video\d+$/.test(p.DEVNAME || "")) return false;
  if (!(p.ID_V4L_CAPABILITIES || "").includes(":capture:")) return false;
  if (!usbParent) return false;
  // Belt and braces: the codec/ISP drivers never sit under USB, but if a future kernel
  // reparents them we still don't want them.
  if (/^(bcm2835|rpi-)/.test(p.ID_V4L_PRODUCT || "")) return false;
  return true;
}

/**
 * Device Caps from `v4l2-ctl -D`. UVC cameras expose a second node per camera for metadata;
 * older udev marked both `:capture:` because it looked at the *device-wide* capabilities.
 * The per-node Device Caps are authoritative: a real capture node lists "Video Capture".
 */
export function parseDeviceCaps(text: string): { driver: string; card: string; bus: string; caps: string[] } {
  const field = (k: string) => new RegExp(`^\\s*${k}\\s*:\\s*(.*)$`, "m").exec(text)?.[1].trim() || "";
  const caps: string[] = [];
  const m = /Device Caps\s*:\s*0x[0-9a-f]+\n((?:\t\t.*\n?)*)/i.exec(text);
  if (m) for (const l of m[1].split("\n")) if (l.trim()) caps.push(l.trim());
  return { driver: field("Driver name"), card: field("Card type"), bus: field("Bus info"), caps };
}

export function capsCanCapture(caps: string[]): boolean {
  return caps.some((c) => /^Video Capture( Multiplanar)?$/.test(c));
}

/** USB IDs of the cheap MS2109/MS2130-class HDMI-to-USB capture sticks (UVC, MJPEG + YUYV). */
const HDMI_CAPTURE_IDS = new Set(["534d:2109", "534d:2130", "345f:2130", "345f:2131", "1d6c:0103", "eba4:7588"]);

export function cameraKind(vid: string, pid: string, model: string): "hdmi" | "webcam" {
  if (HDMI_CAPTURE_IDS.has(`${vid}:${pid}`.toLowerCase())) return "hdmi";
  // Rebadged sticks keep the chip but change the strings; the strings usually still say so.
  if (/hdmi|capture|usb3\.0 video|cam link|ms210|ms213/i.test(model)) return "hdmi";
  return "webcam";
}

/** Settings key that survives replugging into the same port, and moving between ports when there's a serial. */
export function cameraKey(p: Props): string {
  if (p.ID_SERIAL_SHORT && p.ID_VENDOR_ID) return `usb:${p.ID_VENDOR_ID}:${p.ID_MODEL_ID}:${p.ID_SERIAL_SHORT}`;
  return `path:${p.ID_PATH || p.DEVPATH}`;
}

// ---------- --list-formats-ext ----------

export interface FrameSize {
  w: number;
  h: number;
  fps: number[]; // discrete rates, highest first; empty when the driver reports none
}

export interface Format {
  fourcc: string; // "MJPG", "YUYV"
  description: string;
  compressed: boolean;
  sizes: FrameSize[];
  stepwise?: { minW: number; minH: number; maxW: number; maxH: number; stepW: number; stepH: number };
}

export function parseFormats(text: string): Format[] {
  const out: Format[] = [];
  let fmt: Format | null = null;
  let size: FrameSize | null = null;
  for (const line of text.split("\n")) {
    let m;
    if ((m = /^\s*\[\d+\]: '(.{1,4})' \((.*)\)\s*$/.exec(line))) {
      fmt = { fourcc: m[1].trim(), description: m[2], compressed: /compressed/i.test(m[2]), sizes: [] };
      out.push(fmt);
      size = null;
    } else if (fmt && (m = /^\s*Size: Discrete (\d+)x(\d+)/.exec(line))) {
      size = { w: +m[1], h: +m[2], fps: [] };
      fmt.sizes.push(size);
    } else if (fmt && (m = /^\s*Size: (?:Stepwise|Continuous) (\d+)x(\d+) - (\d+)x(\d+)(?: with step (\d+)\/(\d+))?/.exec(line))) {
      fmt.stepwise = { minW: +m[1], minH: +m[2], maxW: +m[3], maxH: +m[4], stepW: +(m[5] || 1), stepH: +(m[6] || 1) };
      size = null;
    } else if (size && (m = /^\s*Interval: Discrete [\d.]+s \(([\d.]+) fps\)/.exec(line))) {
      const fps = Math.round(+m[1] * 1000) / 1000;
      if (!size.fps.includes(fps)) size.fps.push(fps);
    }
  }
  for (const f of out) {
    for (const s of f.sizes) s.fps.sort((a, b) => b - a);
    f.sizes.sort((a, b) => b.w * b.h - a.w * a.h);
  }
  return out;
}

/** v4l2 fourcc → ustreamer --format name. Formats ustreamer can't ingest map to null. */
export function ustreamerFormat(fourcc: string): string | null {
  return ({ MJPG: "MJPEG", JPEG: "JPEG", YUYV: "YUYV", UYVY: "UYVY", RGBP: "RGB565", RGB3: "RGB24" } as Record<string, string>)[fourcc] || null;
}

export interface StreamChoice {
  fourcc: string;
  w: number;
  h: number;
  fps: number;
}

/**
 * Pick what to stream: the user's saved choice if the camera still offers it, otherwise
 * MJPEG (the camera has already compressed it, so the Pi only forwards frames) at the
 * largest size up to 1080p, otherwise an uncompressed format ustreamer can encode, kept to
 * 720p because the Pi then has to JPEG-encode every frame.
 */
export function chooseStream(formats: Format[], saved?: Partial<StreamChoice>): StreamChoice | null {
  const usable = formats.filter((f) => ustreamerFormat(f.fourcc) && (f.sizes.length || f.stepwise));
  if (!usable.length) return null;
  if (saved?.fourcc && saved.w && saved.h) {
    const f = usable.find((f) => f.fourcc === saved.fourcc);
    const s = f?.sizes.find((s) => s.w === saved.w && s.h === saved.h);
    if (f && s) return { fourcc: f.fourcc, w: s.w, h: s.h, fps: s.fps.includes(saved.fps!) ? saved.fps! : s.fps[0] || 30 };
    if (f?.stepwise && saved.w <= f.stepwise.maxW && saved.h <= f.stepwise.maxH) return { fourcc: f.fourcc, w: saved.w, h: saved.h, fps: saved.fps || 30 };
  }
  const f = usable.find((f) => f.fourcc === "MJPG") || usable.find((f) => f.fourcc === "JPEG") || usable[0];
  const cap = f.compressed ? 1920 * 1080 : 1280 * 720;
  if (!f.sizes.length && f.stepwise) {
    const w = Math.min(f.stepwise.maxW, f.compressed ? 1920 : 1280);
    const h = Math.min(f.stepwise.maxH, f.compressed ? 1080 : 720);
    return { fourcc: f.fourcc, w, h, fps: 30 };
  }
  const s = f.sizes.find((s) => s.w * s.h <= cap) || f.sizes[f.sizes.length - 1];
  return { fourcc: f.fourcc, w: s.w, h: s.h, fps: s.fps[0] || 30 };
}

// ---------- --list-ctrls-menus ----------

export interface Control {
  name: string; // v4l2-ctl name: "brightness", "auto_exposure"
  id: string;
  type: string; // int, bool, menu, intmenu, button, int64, bitmask…
  group: string; // "User Controls", "Camera Controls"
  min?: number;
  max?: number;
  step?: number;
  default?: number;
  value?: number;
  flags: string[]; // inactive, read-only, …
  menu?: { value: number; label: string }[];
}

export function parseControls(text: string): Control[] {
  const out: Control[] = [];
  let group = "";
  let cur: Control | null = null;
  for (const line of text.split("\n")) {
    let m;
    if ((m = /^\s*([a-z0-9_]+) (0x[0-9a-f]+) \(([a-z0-9]+)\)\s*:\s*(.*)$/.exec(line))) {
      const kv: Record<string, string> = {};
      const rest = m[4];
      for (const km of rest.matchAll(/(\w+)=(-?\d+)/g)) kv[km[1]] = km[2];
      const flags = /flags=([\w\-, ]+)$/.exec(rest)?.[1].split(",").map((s) => s.trim()).filter(Boolean) || [];
      cur = { name: m[1], id: m[2], type: m[3], group, flags };
      for (const k of ["min", "max", "step", "default", "value"] as const) if (kv[k] != null) cur[k] = +kv[k];
      if (m[3] === "menu" || m[3] === "intmenu") cur.menu = [];
      out.push(cur);
    } else if (cur?.menu && (m = /^\t\t\t\t(-?\d+): (.*)$/.exec(line))) {
      cur.menu.push({ value: +m[1], label: m[2].trim() });
    } else if ((m = /^([A-Z][\w ]+ Controls)\s*$/.exec(line.trim()))) {
      group = m[1];
      cur = null;
    }
  }
  return out;
}

/** Controls a person adjusts on a camera; the rest (codec levels, read-only counters) is noise. */
export function isUserControl(c: Control): boolean {
  if (c.flags.includes("read-only") || c.flags.includes("write-only")) return false;
  if (!["int", "bool", "menu", "intmenu"].includes(c.type)) return false;
  return !/^(Codec|Detection|Colorimetry)/.test(c.group);
}

// ---------- libcamera ----------

/** `rpicam-hello --list-cameras` → [{index, sensor, modes}]. */
export function parseLibcameraList(text: string): { index: number; sensor: string; detail: string }[] {
  const out: { index: number; sensor: string; detail: string }[] = [];
  for (const m of text.matchAll(/^(\d+) : (\S+) \[([^\]]*)\]\s*\(([^)]*)\)/gm)) out.push({ index: +m[1], sensor: m[2], detail: `${m[3]} ${m[4]}`.trim() });
  return out;
}
