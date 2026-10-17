// ==================== GPS (gpsd JSON) ====================
//
// Pure helpers for the gps driver: which USB ids are GPS receivers, and a fold of gpsd's
// JSON watch stream (TPV/SKY/PPS/TOFF/DEVICE…) into the state the panel shows.

/** USB GPS receivers we treat as GPS without asking. */
export const GPS_USB: Record<string, string> = {
  "1546:01a5": "u-blox 5",
  "1546:01a6": "u-blox 6",
  "1546:01a7": "u-blox 7",
  "1546:01a8": "u-blox 8",
  "1546:01a9": "u-blox 9",
  "0e8d:3329": "MediaTek MT3329 GPS",
  "091e:0003": "Garmin GPS (USB)",
  "1199:9071": "Sierra Wireless GNSS", // shows several ttys; one is NMEA
};

/** Generic USB-serial bridges that GPS pucks are often built on (GlobalSat BU-353 = Prolific,
 * SiRF modules, …). Never claimed automatically: the same chips sit in every other adapter. */
export const MAYBE_GPS_USB: Record<string, string> = {
  "067b:2303": "Prolific PL2303 (GlobalSat BU-353, SiRF pucks…)",
  "067b:23a3": "Prolific PL2303GC",
  "10c4:ea60": "Silicon Labs CP210x",
  "0403:6001": "FTDI FT232",
  "0403:6015": "FTDI FT-X",
  "1a86:7523": "QinHeng CH340",
};

export function classifyUsb(vidpid: string): { kind: "gps" | "maybe" | "unknown"; label: string } {
  if (GPS_USB[vidpid]) return { kind: "gps", label: GPS_USB[vidpid] };
  if (vidpid.startsWith("1546:")) return { kind: "gps", label: "u-blox" };
  if (MAYBE_GPS_USB[vidpid]) return { kind: "maybe", label: MAYBE_GPS_USB[vidpid] };
  return { kind: "unknown", label: "" };
}

// ---------- Satellites ----------

export const GNSS = ["GPS", "SBAS", "Galileo", "BeiDou", "IMES", "QZSS", "GLONASS", "NavIC"];

/** gpsd's legacy PRN numbering, for receivers/versions that don't send gnssid. */
export function constellationOf(sat: { gnssid?: number; PRN: number }): string {
  if (typeof sat.gnssid === "number") return GNSS[sat.gnssid] || `gnss ${sat.gnssid}`;
  const p = sat.PRN;
  if (p >= 1 && p <= 63) return "GPS";
  if (p >= 64 && p <= 96) return "GLONASS";
  if (p >= 120 && p <= 158) return "SBAS";
  if (p >= 193 && p <= 200) return "QZSS";
  if (p >= 201 && p <= 263) return "BeiDou";
  if (p >= 301 && p <= 336) return "Galileo";
  return "other";
}

export interface Sat {
  prn: number;
  svid: number | null;
  gnss: string;
  el: number | null;
  az: number | null;
  snr: number | null;
  used: boolean;
  health: number | null;
  seen: number; // our clock, last SKY that listed it
}

const SAT_TTL_MS = 10_000;

// ---------- gpsd stream state ----------

export const FIX_MODE = ["unknown", "no fix", "2D", "3D"];

export interface GpsdState {
  version: string;
  devices: { path: string; driver?: string; subtype?: string; subtype1?: string; bps?: number; cycle?: number; activated?: string; flags?: number }[];
  tpv: Record<string, any> | null;
  tpvAt: number; // our clock when the TPV arrived
  sky: Record<string, any> | null;
  skyAt: number;
  sats: Sat[]; // merged across partial SKY reports, newest data per satellite
  pps: { offset_ns: number; at: number; device: string; precision?: number } | null;
  toff: { offset_ns: number; at: number; device: string } | null;
  errors: string[];
  messages: number;
}

export function newState(): GpsdState {
  return { version: "", devices: [], tpv: null, tpvAt: 0, sky: null, skyAt: 0, sats: [], pps: null, toff: null, errors: [], messages: 0 };
}

/** Offset in ns from a gpsd PPS/TOFF report: the receiver's idea of the second minus our clock. */
function realMinusClock(o: any): number {
  return (o.real_sec - o.clock_sec) * 1e9 + (o.real_nsec - o.clock_nsec);
}

export function ingest(s: GpsdState, o: any, now = Date.now()): void {
  if (!o || typeof o !== "object") return;
  s.messages++;
  switch (o.class) {
    case "VERSION":
      s.version = o.release || "";
      break;
    case "DEVICES":
      s.devices = (o.devices || []).map(dev);
      break;
    case "DEVICE": {
      const d = dev(o);
      const i = s.devices.findIndex((x) => x.path === d.path);
      if (i >= 0) s.devices[i] = { ...s.devices[i], ...d };
      else s.devices.push(d);
      break;
    }
    case "TPV":
      // gpsd sends a mode-only TPV while it has nothing; keep the last position for display
      // but let the mode drop so the panel says "no fix".
      s.tpv = o.mode >= 2 || !s.tpv ? o : { ...s.tpv, mode: o.mode, time: o.time ?? s.tpv.time };
      s.tpvAt = now;
      break;
    case "SKY":
      // gpsd 3.2x splits SKY: DOPs come from GSA, satellites from GSV. Merge, don't replace.
      s.sky = { ...s.sky, ...o };
      s.skyAt = now;
      // Satellites arrive the same way, one constellation's GSV group at a time (9, then 13,
      // then 16… satellites within one second), so merge by identity and age out the ones
      // no longer reported instead of replacing the list.
      if (Array.isArray(o.satellites)) {
        const byId = new Map(s.sats.map((x) => [`${x.gnss}:${x.svid ?? x.prn}`, x]));
        for (const x of o.satellites) {
          const sat: Sat = { prn: x.PRN, svid: x.svid ?? null, gnss: constellationOf(x), el: num(x.el), az: num(x.az), snr: num(x.ss), used: !!x.used, health: x.health ?? null, seen: now };
          byId.set(`${sat.gnss}:${sat.svid ?? sat.prn}`, sat);
        }
        s.sats = [...byId.values()].filter((x) => now - x.seen <= SAT_TTL_MS).sort((a, b) => a.gnss.localeCompare(b.gnss) || (a.svid ?? a.prn) - (b.svid ?? b.prn));
      }
      break;
    case "PPS":
      s.pps = { offset_ns: realMinusClock(o), at: now, device: o.device || "", precision: o.precision };
      break;
    case "TOFF":
      s.toff = { offset_ns: realMinusClock(o), at: now, device: o.device || "" };
      break;
    case "ERROR":
      s.errors.push(o.message || JSON.stringify(o));
      if (s.errors.length > 20) s.errors.shift();
      break;
  }
}

function num(v: any): number | null {
  return typeof v === "number" && isFinite(v) ? v : null;
}

function dev(o: any) {
  return { path: o.path, driver: o.driver, subtype: o.subtype, subtype1: o.subtype1, bps: o.bps, cycle: o.cycle, activated: o.activated, flags: o.flags };
}

/** The compact view used by status, REST and MCP. */
export function summarize(s: GpsdState, now = Date.now()) {
  const t = s.tpv || {};
  const mode = typeof t.mode === "number" ? t.mode : 0;
  const used = s.sats.filter((x) => x.used).length;
  const byGnss: Record<string, { visible: number; used: number }> = {};
  for (const x of s.sats) {
    const g = (byGnss[x.gnss] ||= { visible: 0, used: 0 });
    g.visible++;
    if (x.used) g.used++;
  }
  // TPV time vs our clock at receipt. Includes the receiver's output latency and serial
  // transfer time (typically 50–500 ms), so it is a sanity check, not a time source.
  const tpvTime = t.time ? Date.parse(t.time) : NaN;
  const nmeaOffset_ms = isNaN(tpvTime) || !s.tpvAt ? null : Math.round(tpvTime - s.tpvAt);
  return {
    fix: FIX_MODE[mode] || "unknown",
    mode,
    status: t.status ?? null, // 2 = DGPS, 3 = RTK fixed…
    time: t.time || null,
    lat: num(t.lat),
    lon: num(t.lon),
    altHAE_m: num(t.altHAE ?? t.alt),
    altMSL_m: num(t.altMSL),
    speed_ms: num(t.speed),
    track_deg: num(t.track),
    climb_ms: num(t.climb),
    eph_m: num(t.eph),
    epv_m: num(t.epv),
    leapseconds: t.leapseconds ?? null,
    hdop: num(s.sky?.hdop),
    vdop: num(s.sky?.vdop),
    pdop: num(s.sky?.pdop),
    gdop: num(s.sky?.gdop),
    // From the merged list: SKY's own uSat/nSat only count the constellations in that report.
    satsUsed: used,
    satsVisible: s.sats.length,
    constellations: byGnss,
    nmeaOffset_ms,
    toffOffset_ms: s.toff ? Math.round(s.toff.offset_ns / 1e4) / 100 : null,
    ppsOffset_us: s.pps ? Math.round(s.pps.offset_ns / 10) / 100 : null,
    ppsAge_s: s.pps ? Math.round((now - s.pps.at) / 1000) : null,
    lastUpdate_s: s.tpvAt ? Math.round((now - s.tpvAt) / 1000) : null,
  };
}

// ---------- NMEA helpers (fixtures and tests) ----------

export function nmeaChecksum(body: string): string {
  let c = 0;
  for (let i = 0; i < body.length; i++) c ^= body.charCodeAt(i);
  return c.toString(16).toUpperCase().padStart(2, "0");
}

export function nmea(body: string): string {
  return `$${body}*${nmeaChecksum(body)}`;
}
