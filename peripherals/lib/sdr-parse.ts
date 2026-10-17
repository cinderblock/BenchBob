// ==================== RTL-SDR parsing ====================
//
// Pure helpers for the sdr driver: which USB ids are RTL2832U sticks, what rtl_test and
// rtl_eeprom say about them, whether the DVB driver is blacklisted, and the rtl_433 JSON
// stream folded into a per-sensor table.

/** RTL2832U-based sticks, from librtlsdr's known_devices table. */
export const RTL_STICKS: Record<string, string> = {
  "0bda:2832": "Generic RTL2832U",
  "0bda:2838": "Generic RTL2832U OEM",
  "0413:6680": "DigitalNow Quad DVB-T PCI-E card",
  "0413:6f0f": "Leadtek WinFast DTV Dongle mini D",
  "0458:707f": "Genius TVGo DVB-T03 USB dongle (Ver. B)",
  "0ccd:00a9": "Terratec Cinergy T Stick Black (rev 1)",
  "0ccd:00b3": "Terratec NOXON DAB/DAB+ USB dongle (rev 1)",
  "0ccd:00b4": "Terratec Deutschlandradio DAB Stick",
  "0ccd:00b5": "Terratec NOXON DAB Stick - Radio Energy",
  "0ccd:00b7": "Terratec Media Broadcast DAB Stick",
  "0ccd:00b8": "Terratec BR DAB Stick",
  "0ccd:00b9": "Terratec WDR DAB Stick",
  "0ccd:00c0": "Terratec MuellerVerlag DAB Stick",
  "0ccd:00c6": "Terratec Fraunhofer DAB Stick",
  "0ccd:00d3": "Terratec Cinergy T Stick RC (Rev.3)",
  "0ccd:00d7": "Terratec T Stick PLUS",
  "0ccd:00e0": "Terratec NOXON DAB/DAB+ USB dongle (rev 2)",
  "1554:5020": "PixelView PV-DT235U(RN)",
  "15f4:0131": "Astrometa DVB-T/DVB-T2",
  "15f4:0133": "HanfTek DAB+FM+DVB-T",
  "185b:0620": "Compro Videomate U620F",
  "185b:0650": "Compro Videomate U650F",
  "185b:0680": "Compro Videomate U680F",
  "1b80:d393": "GIGABYTE GT-U7300",
  "1b80:d394": "DIKOM USB-DVBT HD",
  "1b80:d395": "Peak 102569AGPK",
  "1b80:d397": "KWorld KW-UB450-T USB DVB-T Pico TV",
  "1b80:d398": "Zaapa ZT-MINDVBZP",
  "1b80:d39d": "SVEON STV20 DVB-T USB & FM",
  "1b80:d3a4": "Twintech UT-40",
  "1b80:d3a8": "ASUS U3100MINI_PLUS_V2",
  "1b80:d3af": "SVEON STV27 DVB-T USB & FM",
  "1b80:d3b0": "SVEON STV21 DVB-T USB & FM",
  "1d19:1101": "Dexatek DK DVB-T Dongle (Logilink VG0002A)",
  "1d19:1102": "Dexatek DK DVB-T Dongle (MSI DigiVox mini II V3.0)",
  "1d19:1103": "Dexatek Technology Ltd. DK 5217 DVB-T Dongle",
  "1d19:1104": "MSI DigiVox Micro HD",
  "1f4d:a803": "Sweex DVB-T USB",
  "1f4d:b803": "GTek T803",
  "1f4d:c803": "Lifeview LV5TDeluxe",
  "1f4d:d286": "MyGica TD312",
  "1f4d:d803": "PROlectrix DV107669",
};

export const DVB_MODULE = "dvb_usb_rtl28xxu";

// ---------- rtl_test -t ----------

export interface RtlTestInfo {
  devices: { index: number; vendor: string; product: string; serial: string }[];
  using: string;
  tuner: string; // "Rafael Micro R820T"
  gains: number[];
  blogV4: boolean;
  claimed: boolean; // kernel DVB driver or another program has it
  detachedKernel: boolean; // librtlsdr built with DETACH_KERNEL_DRIVER kicked the DVB driver off
  error: string;
}

export function parseRtlTest(out: string): RtlTestInfo {
  const info: RtlTestInfo = { devices: [], using: "", tuner: "", gains: [], blogV4: false, claimed: false, detachedKernel: false, error: "" };
  for (const raw of out.split("\n")) {
    const l = raw.trim();
    let m;
    if ((m = /^(\d+):\s+(.*?),\s*(.*?),\s*SN:\s*(.*)$/.exec(l))) info.devices.push({ index: +m[1], vendor: m[2], product: m[3], serial: m[4].trim() });
    else if ((m = /^Using device \d+:\s*(.*)$/.exec(l))) info.using = m[1];
    else if ((m = /^Found (.+?) tuner$/.exec(l))) info.tuner = m[1];
    else if ((m = /^Supported gain values \(\d+\):\s*(.*)$/.exec(l))) info.gains = m[1].trim().split(/\s+/).map(Number).filter((n) => !isNaN(n));
    else if (/RTL-SDR Blog V4 Detected/i.test(l)) info.blogV4 = true;
    else if (/^Detached kernel driver/i.test(l)) info.detachedKernel = true;
    else if (/usb_claim_interface error|Kernel driver is active|claimed by second instance/i.test(l)) {
      info.claimed = true;
      info.error ||= "Device is busy: the kernel DVB driver or another program has it";
    } else if (/No supported devices found|Failed to open rtlsdr device|No matching devices/i.test(l)) info.error ||= l;
  }
  if (info.blogV4 && info.tuner) info.tuner += " (RTL-SDR Blog V4)";
  return info;
}

/** rtl_eeprom's dump: "Manufacturer:\t\tRealtek" etc. */
export function parseRtlEeprom(out: string): Record<string, string> {
  const r: Record<string, string> = {};
  let inConfig = false;
  for (const l of out.split("\n")) {
    if (/^Current configuration/.test(l)) inConfig = true;
    if (!inConfig) continue;
    const m = /^([A-Za-z][A-Za-z ]+):\s+(.*)$/.exec(l.trim());
    if (m) r[m[1].trim()] = m[2].trim();
  }
  return r;
}

// ---------- DVB driver blacklist ----------

/** Does any modprobe.d file blacklist (or neuter with `install … /bin/false`) the DVB driver? */
export function findBlacklist(files: { path: string; text: string }[], mod = DVB_MODULE): { blacklisted: boolean; where: string[] } {
  const where: string[] = [];
  const name = mod.replace(/-/g, "_");
  for (const f of files) {
    for (const raw of f.text.split("\n")) {
      const l = raw.replace(/#.*/, "").trim().replace(/-/g, "_");
      if (new RegExp(`^blacklist\\s+${name}$`).test(l) || new RegExp(`^install\\s+${name}\\s+/bin/(false|true)`).test(l)) where.push(`${f.path}: ${raw.trim()}`);
    }
  }
  return { blacklisted: where.length > 0, where };
}

// ---------- rtl_433 ----------

// Keys that describe the reception rather than the sensor.
const META = new Set(["time", "model", "id", "channel", "mic", "mod", "freq", "freq1", "freq2", "rssi", "snr", "noise", "protocol"]);
// Numeric fields worth a sparkline.
export const HIST_FIELDS = ["temperature_C", "temperature_F", "humidity", "moisture", "pressure_hPa", "pressure_kPa", "pressure_PSI", "wind_avg_km_h", "wind_avg_m_s", "rain_mm", "light_lux", "uv", "power_W"];
const HIST_N = 120;

export interface Rtl433Device {
  key: string;
  model: string;
  id: string;
  channel: string;
  protocol: number | null;
  count: number;
  firstSeen: number;
  lastSeen: number;
  rssi: number | null;
  snr: number | null;
  freq: number | null;
  fields: Record<string, any>; // latest sensor values
  hist: Record<string, [number, number][]>; // field → [t ms, value]
}

export interface Rtl433Message {
  seq: number;
  t: number;
  key: string;
  msg: Record<string, any>;
}

/** rtl_433 -M time:unix:usec gives "1729123456.123456"; plain -M time gives a local "YYYY-MM-DD HH:MM:SS". */
export function rtl433Time(v: any, fallback: number): number {
  if (typeof v === "number") return v > 1e12 ? v : v * 1000;
  if (typeof v === "string") {
    if (/^\d+(\.\d+)?$/.test(v)) return Math.round(parseFloat(v) * 1000);
    const d = Date.parse(v.replace(" ", "T"));
    if (!isNaN(d)) return d;
  }
  return fallback;
}

export function deviceKey(m: Record<string, any>): string {
  return [m.model, m.subtype ?? m.type, m.id, m.channel].filter((x) => x !== undefined && x !== null && x !== "").join("/");
}

export class Rtl433Aggregator {
  devices = new Map<string, Rtl433Device>();
  messages: Rtl433Message[] = [];
  log: { t: number; line: string }[] = [];
  seq = 0;
  constructor(private maxMessages = 300, private maxLog = 200) {}

  /** Feed one stdout line: JSON becomes a message, anything else a log line. Returns the device touched, if any. */
  line(raw: string, now = Date.now()): Rtl433Device | null {
    const line = raw.trim();
    if (!line) return null;
    if (line[0] !== "{") {
      this.log.push({ t: now, line });
      if (this.log.length > this.maxLog) this.log.shift();
      return null;
    }
    let m: Record<string, any>;
    try {
      m = JSON.parse(line);
    } catch {
      this.log.push({ t: now, line });
      return null;
    }
    if (!m.model) return null;
    return this.ingest(m, now);
  }

  ingest(m: Record<string, any>, now = Date.now()): Rtl433Device {
    const t = rtl433Time(m.time, now);
    const key = deviceKey(m);
    let d = this.devices.get(key);
    if (!d) {
      d = { key, model: String(m.model), id: m.id != null ? String(m.id) : "", channel: m.channel != null ? String(m.channel) : "", protocol: null, count: 0, firstSeen: t, lastSeen: t, rssi: null, snr: null, freq: null, fields: {}, hist: {} };
      this.devices.set(key, d);
    }
    d.count++;
    d.lastSeen = t;
    if (typeof m.protocol === "number") d.protocol = m.protocol;
    if (typeof m.rssi === "number") d.rssi = m.rssi;
    if (typeof m.snr === "number") d.snr = m.snr;
    if (typeof m.freq === "number") d.freq = m.freq;
    for (const [k, v] of Object.entries(m)) if (!META.has(k)) d.fields[k] = v;
    for (const f of HIST_FIELDS) {
      if (typeof m[f] !== "number") continue;
      const h = (d.hist[f] ||= []);
      h.push([t, m[f]]);
      if (h.length > HIST_N) h.shift();
    }
    this.messages.push({ seq: ++this.seq, t, key, msg: m });
    if (this.messages.length > this.maxMessages) this.messages.shift();
    return d;
  }

  list(): Rtl433Device[] {
    return [...this.devices.values()].sort((a, b) => b.lastSeen - a.lastSeen);
  }

  clear() {
    this.devices.clear();
    this.messages = [];
    this.log = [];
  }
}

// ---------- /proc/net/tcp ----------

/** Established TCP peers connected to a local port, from /proc/net/tcp{,6} text. */
export function tcpPeers(procNetTcp: string, port: number): string[] {
  const out: string[] = [];
  for (const l of procNetTcp.split("\n").slice(1)) {
    const f = l.trim().split(/\s+/);
    if (f.length < 4 || f[3] !== "01") continue; // 01 = ESTABLISHED
    const [, lp] = f[1].split(":");
    if (parseInt(lp, 16) !== port) continue;
    const [ra, rp] = f[2].split(":");
    out.push(`${hexIp(ra)}:${parseInt(rp, 16)}`);
  }
  return out;
}

function hexIp(h: string): string {
  if (h.length === 8) return [3, 2, 1, 0].map((i) => parseInt(h.substr(i * 2, 2), 16)).join(".");
  // IPv6: four little-endian 32-bit words. Show v4-mapped addresses as plain IPv4.
  const words = [0, 1, 2, 3].map((w) => h.substr(w * 8, 8).match(/../g)!.reverse().join(""));
  const full = words.join("").toLowerCase();
  if (full.startsWith("00000000000000000000ffff")) return hexIp(h.substr(24, 8));
  return full.match(/.{4}/g)!.map((g) => g.replace(/^0+(?=.)/, "")).join(":").replace(/(^|:)0(:0)+(:|$)/, "::");
}
