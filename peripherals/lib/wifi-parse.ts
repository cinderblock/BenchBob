// ==================== Wi-Fi parsers ====================
//
// Pure functions over the text that iw, wpa_supplicant, udhcpc, ping, nslookup and curl
// print, so the driver stays about orchestration and all of this is unit-testable against
// fixtures captured on desky (peripherals/lib/wifi-fixtures/). iw's output is meant for
// humans and changes between versions, so every parser here is section-based and
// tolerant: unknown lines are skipped, missing ones leave fields undefined.

// ---------- Frequencies, channels, bands ----------

export type Band = "2.4" | "5" | "6" | "60";

export function bandOf(mhz: number): Band | null {
  if (mhz >= 2400 && mhz < 2500) return "2.4";
  if (mhz >= 4900 && mhz < 5925) return "5";
  if (mhz >= 5925 && mhz < 7125 + 1) return "6";
  if (mhz >= 57000 && mhz < 72000) return "60";
  return null;
}

/** IEEE channel number for a centre frequency (the band decides the formula). */
export function freqToChannel(mhz: number): number {
  const f = Math.round(mhz);
  if (f === 2484) return 14;
  if (f >= 2407 && f < 2484) return (f - 2407) / 5;
  if (f === 5935) return 2; // 6 GHz channel 2 is the odd one out
  if (f >= 5950 && f <= 7125) return (f - 5950) / 5;
  if (f >= 4910 && f < 5925) return (f - 5000) / 5;
  if (f >= 58320 && f <= 70200) return (f - 56160) / 2160;
  return 0;
}

export function channelToFreq(ch: number, band: Band): number {
  if (band === "2.4") return ch === 14 ? 2484 : 2407 + ch * 5;
  if (band === "5") return 5000 + ch * 5;
  if (band === "6") return ch === 2 ? 5935 : 5950 + ch * 5;
  return 56160 + ch * 2160;
}

/** U-NII sub-band for 5/6 GHz frequencies; DFS bands are where radar detection applies (US rules). */
export function uniiOf(mhz: number): { name: string; dfs: boolean } | null {
  if (mhz >= 5150 && mhz < 5250) return { name: "U-NII-1", dfs: false };
  if (mhz >= 5250 && mhz < 5350) return { name: "U-NII-2A", dfs: true };
  if (mhz >= 5470 && mhz < 5730) return { name: "U-NII-2C", dfs: true };
  if (mhz >= 5730 && mhz < 5850) return { name: "U-NII-3", dfs: false };
  if (mhz >= 5850 && mhz < 5925) return { name: "U-NII-4", dfs: false };
  if (mhz >= 5925 && mhz < 6425) return { name: "U-NII-5", dfs: false };
  if (mhz >= 6425 && mhz < 6525) return { name: "U-NII-6", dfs: false };
  if (mhz >= 6525 && mhz < 6875) return { name: "U-NII-7", dfs: false };
  if (mhz >= 6875 && mhz <= 7125) return { name: "U-NII-8", dfs: false };
  return null;
}

// ---------- Generic helpers ----------

const tabs = (l: string) => /^\t*/.exec(l)![0].length;

/** iw prints SSIDs with unprintable bytes, leading/trailing spaces and backslashes as \xNN. */
export function decodeIwString(s: string): { text: string; bytes: number[] } {
  const bytes: number[] = [];
  for (let i = 0; i < s.length; i++) {
    const m = s[i] === "\\" && /^\\x([0-9a-fA-F]{2})/.exec(s.slice(i));
    if (m) {
      bytes.push(parseInt(m[1], 16));
      i += 3;
    } else for (const b of new TextEncoder().encode(s[i])) bytes.push(b);
  }
  return { text: new TextDecoder("utf-8", { fatal: false }).decode(new Uint8Array(bytes)), bytes };
}

function num(s: string | undefined): number | undefined {
  if (s == null) return undefined;
  const n = parseFloat(s);
  return Number.isFinite(n) ? n : undefined;
}

// ---------- iw dev ----------

export interface IwIface {
  phy: string; // "phy0"
  name: string | null; // null for wdev-only (P2P-device)
  ifindex?: number;
  wdev?: string;
  addr?: string;
  ssid?: string;
  type?: string;
  freq?: number;
  channel?: number;
  width?: string;
  center1?: number;
  txpower?: number;
}

export function parseIwDev(out: string): IwIface[] {
  const res: IwIface[] = [];
  let phy = "";
  let cur: IwIface | null = null;
  for (const line of out.split("\n")) {
    let m;
    if ((m = /^phy#(\d+)/.exec(line))) {
      phy = "phy" + m[1];
      cur = null;
    } else if ((m = /^\tInterface (\S+)/.exec(line))) {
      cur = { phy, name: m[1] };
      res.push(cur);
    } else if (/^\tUnnamed\/non-netdev interface/.test(line)) {
      cur = { phy, name: null };
      res.push(cur);
    } else if (cur && (m = /^\t\t(\S+) (.*)$/.exec(line))) {
      const [, k, v] = m;
      if (k === "ifindex") cur.ifindex = parseInt(v);
      else if (k === "wdev") cur.wdev = v;
      else if (k === "addr") cur.addr = v;
      else if (k === "ssid") cur.ssid = decodeIwString(v).text;
      else if (k === "type") cur.type = v;
      else if (k === "txpower") cur.txpower = num(v);
      else if (k === "channel") {
        // "44 (5220 MHz), width: 40 MHz, center1: 5230 MHz"
        const c = /^(\d+) \((\d+(?:\.\d+)?) MHz\)(?:, width: ([^,]+))?(?:, center1: (\d+) MHz)?/.exec(v);
        if (c) {
          cur.channel = +c[1];
          cur.freq = +c[2];
          cur.width = c[3]?.trim();
          cur.center1 = c[4] ? +c[4] : undefined;
        }
      }
    }
  }
  return res;
}

// ---------- iw phy ----------

export interface PhyChannel {
  mhz: number;
  channel: number;
  band: Band;
  disabled: boolean;
  radar: boolean;
  noIR: boolean;
  maxPower?: number; // dBm
  flags: string[]; // every parenthesised note iw printed, e.g. "no 160MHz"
}

export interface PhyBand {
  band: Band;
  ht: boolean;
  vht: boolean;
  he: boolean;
  eht: boolean;
  /** Widest channel this band's capabilities allow, in MHz. */
  maxWidth: number;
  channels: PhyChannel[];
}

export interface PhyInfo {
  phy: string;
  iftypes: string[];
  softwareIftypes: string[];
  ciphers: string[];
  commands: string[];
  bands: PhyBand[];
  monitor: boolean;
  ap: boolean;
  netns: boolean; // set_wiphy_netns supported
  sae: boolean; // driver can do SAE (SME authenticate or offload)
  extFeatures: string[];
}

export function parseIwPhy(out: string): PhyInfo {
  const lines = out.split("\n");
  const info: PhyInfo = { phy: /Wiphy (\S+)/.exec(out)?.[1] || "", iftypes: [], softwareIftypes: [], ciphers: [], commands: [], bands: [], monitor: false, ap: false, netns: false, sae: false, extFeatures: [] };
  let section = "";
  let band: (PhyBand & { _caps: string }) | null = null;
  let bandSub = "";
  for (const line of lines) {
    const t = tabs(line);
    const s = line.trim();
    if (t === 1) {
      band = null;
      section = s;
      const bm = /^Band (\d+):/.exec(s);
      if (bm) {
        band = { band: "2.4", ht: false, vht: false, he: false, eht: false, maxWidth: 20, channels: [], _caps: "" };
        info.bands.push(band);
      }
      if (/supports SAE/i.test(s)) info.sae = true;
      continue;
    }
    if (band) {
      if (t === 2) bandSub = s;
      band._caps += s + "\n";
      const f = /^\* (\d+(?:\.\d+)?) MHz \[(\d+)\](.*)$/.exec(s);
      if (f && /^Frequencies/.test(bandSub)) {
        const mhz = parseFloat(f[1]);
        const flags = [...f[3].matchAll(/\(([^)]*)\)/g)].map((x) => x[1]);
        const pw = flags.map((x) => /^([\d.]+) dBm$/.exec(x)).find(Boolean);
        band.channels.push({
          mhz,
          channel: +f[2],
          band: bandOf(mhz) || "2.4",
          disabled: flags.includes("disabled"),
          radar: flags.some((x) => /radar/i.test(x)),
          noIR: flags.some((x) => /no IR|no-IR|passive scan/i.test(x)),
          maxPower: pw ? parseFloat(pw[1]) : undefined,
          flags: flags.filter((x) => !/dBm$/.test(x)),
        });
      }
      continue;
    }
    const item = /^\* (.*)$/.exec(s)?.[1];
    if (!item) continue;
    if (/^Supported interface modes/.test(section)) info.iftypes.push(item);
    else if (/^software interface modes/.test(section)) info.softwareIftypes.push(item);
    else if (/^Supported Ciphers/.test(section)) info.ciphers.push(item.replace(/\s*\(.*\)$/, ""));
    else if (/^Supported commands/.test(section)) info.commands.push(item);
    else if (/^Supported extended features/.test(section)) {
      const e = /\[ (\S+) \]/.exec(item)?.[1];
      if (e) info.extFeatures.push(e);
    }
  }
  for (const b of info.bands) {
    const caps = b._caps;
    delete (b as any)._caps;
    if (b.channels.length) b.band = b.channels[0].band;
    b.ht = /HT20\/HT40|HT20|HT Capabilities|HT TX\/RX MCS/.test(caps);
    b.vht = /VHT Capabilities/.test(caps);
    b.he = /HE Iftypes|HE MAC Capabilities/.test(caps);
    b.eht = /EHT Iftypes|EHT MAC Capabilities/.test(caps);
    let w = b.ht && /HT20\/HT40/.test(caps) ? 40 : 20;
    if (b.vht || (b.he && b.band !== "2.4")) w = 80;
    if (/160 MHz|160MHz|HE160/.test(caps) && !/neither 160 nor 80\+80/.test(caps)) w = 160;
    if (b.eht && /320MHz|320 MHz/.test(caps)) w = 320;
    if (b.band === "2.4") w = Math.min(w, 40);
    b.maxWidth = w;
  }
  // S1G (802.11ah, 902–928 MHz; mac80211_hwsim has one) lists frequencies without channel
  // numbers, so it parses to nothing; drop it and anything else we can't place.
  info.bands = info.bands.filter((b) => b.channels.length && bandOf(b.channels[0].mhz));
  info.monitor = info.iftypes.includes("monitor") || info.softwareIftypes.includes("monitor");
  info.ap = info.iftypes.includes("AP");
  info.netns = info.commands.includes("set_wiphy_netns");
  if (info.extFeatures.some((e) => /SAE_OFFLOAD/.test(e))) info.sae = true;
  return info;
}

// ---------- iw reg get ----------

export interface RegRule {
  startMhz: number;
  endMhz: number;
  maxBwMhz: number;
  maxEirp?: number; // dBm
  dfsCacMs?: number;
  flags: string[];
}

export interface RegDomain {
  scope: string; // "global" or "phy0"
  selfManaged: boolean;
  country: string; // "US", "00" (world), "99" (driver-defined)
  dfsRegion?: string; // "DFS-FCC"
  rules: RegRule[];
}

export function parseIwReg(out: string): RegDomain[] {
  const res: RegDomain[] = [];
  let cur: RegDomain | null = null;
  let scope = "global";
  let selfManaged = false;
  for (const line of out.split("\n")) {
    let m;
    if (/^global/.test(line)) {
      scope = "global";
      selfManaged = false;
    } else if ((m = /^phy#(\d+)(.*)$/.exec(line))) {
      scope = "phy" + m[1];
      selfManaged = /self-managed/.test(m[2]);
    } else if ((m = /^country (\S+?):\s*(\S+)?/.exec(line))) {
      cur = { scope, selfManaged, country: m[1], dfsRegion: m[2], rules: [] };
      res.push(cur);
    } else if (cur && (m = /^\s+\((\d+) - (\d+) @ (\d+)\), \(([^)]*)\)(?:, \(([^)]*)\))?(.*)$/.exec(line))) {
      const eirp = m[4].split(",")[1]?.trim();
      const cac = m[5] && /(\d+) ms/.exec(m[5]);
      cur.rules.push({
        startMhz: +m[1],
        endMhz: +m[2],
        maxBwMhz: +m[3],
        maxEirp: eirp && eirp !== "N/A" ? parseFloat(eirp) : undefined,
        dfsCacMs: cac ? +cac[1] : undefined,
        flags: m[6].split(",").map((x) => x.trim()).filter(Boolean),
      });
    }
  }
  return res;
}

// ---------- iw scan ----------

export type SecurityKind = "open" | "owe" | "wep" | "psk" | "sae" | "psk-sae" | "eap";

export interface Bss {
  bssid: string;
  iface: string;
  associated: boolean;
  ssid: string;
  ssidHex?: string; // set when the SSID isn't clean UTF-8 text
  hidden: boolean;
  freq: number;
  channel: number;
  band: Band | null;
  width: number | "80+80"; // MHz
  centerFreq: number; // MHz, centre of the whole (possibly bonded) channel
  centerChannel: number;
  signal?: number; // dBm
  lastSeenMs?: number;
  beaconInterval?: number;
  capability: string[];
  security: string; // human label, e.g. "WPA2/WPA3 (transition)"
  securityKind: SecurityKind;
  akm: string[];
  pairwise: string[];
  group?: string;
  pmf: "required" | "capable" | "none";
  standards: string[]; // ["n","ac","ax"]
  generation?: string; // "Wi-Fi 6"
  stations?: number;
  utilization?: number; // 0..100 %
  country?: string;
  wps?: { state?: string; locked?: boolean; device?: string; manufacturer?: string; model?: string };
  localMac: boolean; // locally-administered BSSID (multi-SSID virtual APs, randomised)
}

interface Section {
  key: string;
  value: string;
  body: string; // value + sub-lines, for regexes that don't care about layout
}

function sectionsOf(lines: string[]): Section[] {
  const out: Section[] = [];
  let cur: Section | null = null;
  for (const line of lines) {
    if (!line.trim()) continue;
    const t = tabs(line);
    if (t === 1) {
      const m = /^\t([^:]+?):\s?(.*)$/.exec(line);
      cur = m ? { key: m[1].trim(), value: m[2], body: m[2] } : { key: line.trim(), value: "", body: "" };
      out.push(cur);
    } else if (cur && t >= 2) cur.body += "\n" + line.trim();
  }
  return out;
}

const AKM_NUM: Record<string, string> = {
  "1": "802.1X", "2": "PSK", "3": "FT/802.1X", "4": "FT/PSK", "5": "802.1X/SHA-256", "6": "PSK/SHA-256",
  "8": "SAE", "9": "FT/SAE", "11": "802.1X/SUITE-B", "12": "802.1X/SUITE-B-192", "13": "FT/802.1X/SHA-384",
  "14": "FILS/SHA-256", "15": "FILS/SHA-384", "18": "OWE", "24": "SAE-EXT-KEY", "25": "FT/SAE-EXT-KEY",
};

/** "FT/IEEE 802.1X IEEE 802.1X/SHA-256" → ["FT/802.1X", "802.1X/SHA-256"]. */
export function parseAkms(s: string): string[] {
  return s
    .replace(/IEEE 802\.1X/g, "802.1X")
    .split(/\s+/)
    .filter(Boolean)
    .map((a) => {
      const n = /^00-0f-ac:(\d+)$/i.exec(a);
      return n ? AKM_NUM[n[1]] || a : a;
    });
}

export function classifySecurity(o: { privacy: boolean; rsnAkm: string[]; wpaAkm: string[]; mfpRequired: boolean }): { label: string; kind: SecurityKind } {
  const all = [...o.rsnAkm, ...o.wpaAkm];
  if (!all.length) return o.privacy ? { label: "WEP", kind: "wep" } : { label: "Open", kind: "open" };
  if (all.includes("OWE")) return { label: "OWE (Enhanced Open)", kind: "owe" };
  if (all.some((a) => /802\.1X|FILS/.test(a))) {
    const wpa3 = o.rsnAkm.some((a) => /SUITE-B|SHA-384/.test(a)) || (o.mfpRequired && o.rsnAkm.every((a) => /SHA-256|SHA-384|SUITE-B|FILS/.test(a)));
    return { label: wpa3 ? "WPA3-Enterprise" : !o.rsnAkm.length ? "WPA-Enterprise" : "WPA2-Enterprise", kind: "eap" };
  }
  const sae = all.some((a) => /SAE/.test(a));
  const psk = all.some((a) => /PSK/.test(a));
  if (sae && psk) return { label: "WPA2/WPA3 (transition)", kind: "psk-sae" };
  if (sae) return { label: "WPA3-SAE", kind: "sae" };
  if (o.rsnAkm.length && o.wpaAkm.length) return { label: "WPA/WPA2-PSK", kind: "psk" };
  if (o.wpaAkm.length) return { label: "WPA-PSK", kind: "psk" };
  return { label: "WPA2-PSK", kind: "psk" };
}

function macIsLocal(mac: string): boolean {
  return (parseInt(mac.slice(0, 2), 16) & 0x02) !== 0;
}

export function parseIwScan(out: string): Bss[] {
  const res: Bss[] = [];
  const blocks = out.split(/^(?=BSS [0-9a-f]{2}(?::[0-9a-f]{2}){5})/im);
  for (const block of blocks) {
    const lines = block.split("\n");
    const head = /^BSS ([0-9a-f:]{17})\s*\(on ([^)]+)\)(?:\s*--\s*(\S+))?/i.exec(lines[0]);
    if (!head) continue;
    const secs = sectionsOf(lines.slice(1));
    const get = (k: string) => secs.find((s) => s.key === k);
    const all = (k: string) => secs.filter((s) => s.key === k);
    const has = (re: RegExp) => secs.some((s) => re.test(s.key));

    const freq = num(get("freq")?.value) ?? 0;
    const band = bandOf(freq);
    const channel = freqToChannel(freq) || num(/channel (\d+)/.exec(get("DS Parameter set")?.value || "")?.[1]) || 0;

    // SSID: missing, empty, or all NULs means hidden.
    const ssidSec = get("SSID");
    const dec = decodeIwString(ssidSec?.value ?? "");
    const hidden = !ssidSec || dec.bytes.length === 0 || dec.bytes.every((b) => b === 0);
    const clean = !hidden && !dec.text.includes("�") && !/[\x00-\x1f]/.test(dec.text);

    // "last seen" appears twice: once as boottime, once as "N ms ago".
    const ago = all("last seen").map((s) => /(\d+) ms ago/.exec(s.value)).find(Boolean);

    const capability = (get("capability")?.value || "").replace(/\(0x[0-9a-f]+\)/i, "").trim().split(/\s+/).filter(Boolean);
    const privacy = capability.includes("Privacy");

    const rsn = get("RSN")?.body || "";
    const wpa = get("WPA")?.body || "";
    const rsnAkm = parseAkms(/Authentication suites: (.*)/.exec(rsn)?.[1] || "");
    const wpaAkm = parseAkms(/Authentication suites: (.*)/.exec(wpa)?.[1] || "");
    const pairwise = [...new Set([...(/Pairwise ciphers: (.*)/.exec(rsn)?.[1] || "").split(/\s+/), ...(/Pairwise ciphers: (.*)/.exec(wpa)?.[1] || "").split(/\s+/)].filter(Boolean))];
    const group = /Group cipher: (\S+)/.exec(rsn)?.[1] || /Group cipher: (\S+)/.exec(wpa)?.[1];
    const mfpRequired = /MFP-required/.test(rsn);
    const pmf = mfpRequired ? "required" : /MFP-capable/.test(rsn) ? "capable" : "none";
    const sec = classifySecurity({ privacy, rsnAkm, wpaAkm, mfpRequired });

    // Standards from which capability elements are present.
    const standards: string[] = [];
    const ht = has(/^HT (capabilities|operation)$/);
    const vht = has(/^VHT (capabilities|operation)$/);
    const he = has(/^HE (capabilities|operation)$/i);
    const eht = has(/^EHT (capabilities|operation)$/i);
    if (!ht && !vht && !he && !eht) {
      const rates = [get("Supported rates"), get("Extended supported rates")].map((s) => s?.value || "").join(" ");
      const maxRate = Math.max(0, ...[...rates.matchAll(/([\d.]+)\*?/g)].map((x) => parseFloat(x[1])));
      standards.push(band === "2.4" ? (maxRate > 11 ? "g" : "b") : "a");
    }
    if (ht) standards.push("n");
    if (vht && band !== "2.4") standards.push("ac");
    if (he) standards.push("ax");
    if (eht) standards.push("be");
    const generation = eht ? "Wi-Fi 7" : he ? (band === "6" ? "Wi-Fi 6E" : "Wi-Fi 6") : vht && band !== "2.4" ? "Wi-Fi 5" : ht ? "Wi-Fi 4" : undefined;

    // Channel width + centre. HT gives 40 MHz with a secondary above/below; VHT/HE/EHT
    // operation elements widen it from their centre segments.
    let width: number | "80+80" = 20;
    let centerFreq = freq;
    const htOp = get("HT operation")?.body || "";
    const off = /secondary channel offset: (\S+)/.exec(htOp)?.[1];
    if (off && off !== "no" && !/STA channel width: 20 MHz/.test(htOp)) {
      width = 40;
      centerFreq = freq + (off === "above" ? 10 : -10);
    }
    const vhtOp = get("VHT operation")?.body || "";
    const vw = /channel width: (\d)/.exec(vhtOp)?.[1];
    const s1 = num(/center freq segment 1: (\d+)/.exec(vhtOp)?.[1]) || 0;
    const s2 = num(/center freq segment 2: (\d+)/.exec(vhtOp)?.[1]) || 0;
    if (band === "5" && vw && vw !== "0" && s1) {
      if (vw === "1" && s2 && Math.abs(s2 - s1) === 8) {
        width = 160;
        centerFreq = channelToFreq(s2, "5");
      } else if (vw === "1" && s2) {
        width = "80+80";
        centerFreq = channelToFreq(s1, "5");
      } else if (vw === "1") {
        width = 80;
        centerFreq = channelToFreq(s1, "5");
      } else if (vw === "2") {
        width = 160;
        centerFreq = channelToFreq(s1, "5");
      } else if (vw === "3") {
        width = "80+80";
        centerFreq = channelToFreq(s1, "5");
      }
    }
    // 6 GHz BSSes carry width in the HE operation's "6 GHz Operation Information".
    const heOp = secs.find((s) => /^HE operation$/i.test(s.key))?.body || "";
    if (band === "6" && heOp) {
      const w6 = /Channel Width:\s*(\d+)/i.exec(heOp)?.[1];
      const c0 = num(/Center Frequency Segment 0:\s*(\d+)/i.exec(heOp)?.[1]);
      const c1 = num(/Center Frequency Segment 1:\s*(\d+)/i.exec(heOp)?.[1]);
      const map: Record<string, number> = { "0": 20, "1": 40, "2": 80, "3": 160, "20": 20, "40": 40, "80": 80, "160": 160 };
      if (w6 && map[w6]) width = map[w6];
      const cc = width === 160 && c1 ? c1 : c0;
      if (cc) centerFreq = channelToFreq(cc, "6");
    }
    const ehtOp = secs.find((s) => /^EHT operation$/i.test(s.key))?.body || "";
    const ew = /Channel Width:\s*(\d+)/i.exec(ehtOp)?.[1];
    const ec = num(/CCFS1?:\s*(\d+)|Center Frequency Segment 1:\s*(\d+)/i.exec(ehtOp)?.slice(1).find(Boolean));
    if (ew && /^(4|320)$/.test(ew)) {
      width = 320;
      if (ec && band) centerFreq = channelToFreq(ec, band);
    }

    const load = get("BSS Load")?.body || "";
    const util = /channel utili[sz]ation: (\d+)\/255/.exec(load);
    const wpsB = get("WPS")?.body;
    const bssid = head[1].toLowerCase();

    res.push({
      bssid,
      iface: head[2],
      associated: head[3] === "associated",
      ssid: hidden ? "" : dec.text,
      ssidHex: hidden || clean ? undefined : dec.bytes.map((b) => b.toString(16).padStart(2, "0")).join(""),
      hidden,
      freq,
      channel,
      band,
      width,
      centerFreq,
      centerChannel: freqToChannel(centerFreq),
      signal: num(/(-?[\d.]+) dBm/.exec(get("signal")?.value || "")?.[1]),
      lastSeenMs: ago ? +ago[1] : undefined,
      beaconInterval: num(get("beacon interval")?.value),
      capability,
      security: sec.label,
      securityKind: sec.kind,
      akm: [...new Set([...rsnAkm, ...wpaAkm])],
      pairwise,
      group,
      pmf,
      standards,
      generation,
      stations: num(/station count: (\d+)/.exec(load)?.[1]),
      utilization: util ? Math.round((+util[1] / 255) * 1000) / 10 : undefined,
      country: /^(\S\S)/.exec(get("Country")?.value || "")?.[1],
      wps: wpsB
        ? {
            state: /Protected Setup State: \d+ \(([^)]+)\)/.exec(wpsB)?.[1],
            locked: /AP setup locked: 0x0*1/.test(wpsB) || undefined,
            device: /Device name: (.*)/.exec(wpsB)?.[1]?.trim(),
            manufacturer: /Manufacturer: (.*)/.exec(wpsB)?.[1]?.trim(),
            model: /\* Model: (.*)/.exec(wpsB)?.[1]?.trim(),
          }
        : undefined,
      localMac: macIsLocal(bssid),
    });
  }
  return res;
}

// ---------- iw survey dump ----------

export interface Survey {
  freq: number;
  inUse: boolean;
  noise?: number;
  activeMs?: number;
  busyMs?: number;
  rxMs?: number;
  txMs?: number;
  busyPct?: number;
}

export function parseSurvey(out: string): Survey[] {
  const res: Survey[] = [];
  for (const block of out.split(/^Survey data from \S+/m).slice(1)) {
    const f = /frequency:\s+(\d+(?:\.\d+)?) MHz( \[in use\])?/.exec(block);
    if (!f) continue;
    const g = (re: RegExp) => num(re.exec(block)?.[1]);
    const s: Survey = {
      freq: parseFloat(f[1]),
      inUse: !!f[2],
      noise: g(/noise:\s+(-?\d+) dBm/),
      activeMs: g(/channel active time:\s+(\d+) ms/),
      busyMs: g(/channel busy time:\s+(\d+) ms/),
      rxMs: g(/channel receive time:\s+(\d+) ms/),
      txMs: g(/channel transmit time:\s+(\d+) ms/),
    };
    if (s.activeMs && s.busyMs != null) s.busyPct = Math.round((s.busyMs / s.activeMs) * 1000) / 10;
    res.push(s);
  }
  return res;
}

// ---------- iw link / station dump ----------

export interface LinkInfo {
  connected: boolean;
  bssid?: string;
  ssid?: string;
  freq?: number;
  signal?: number;
  rxBitrate?: number; // Mbit/s
  txBitrate?: number;
  rxRate?: string; // full descriptor, e.g. "200.0 MBit/s VHT-MCS 9 40MHz short GI VHT-NSS 1"
  txRate?: string;
  txFailed?: number;
  txRetries?: number;
  connectedS?: number;
}

export function parseLink(link: string, station = ""): LinkInfo {
  const l: LinkInfo = { connected: /^Connected to/m.test(link) };
  const g = (src: string, re: RegExp) => re.exec(src)?.[1];
  l.bssid = g(link, /Connected to ([0-9a-f:]{17})/i) || g(station, /Station ([0-9a-f:]{17})/i);
  const ssid = g(link, /^\s*SSID: (.*)$/m);
  if (ssid != null) l.ssid = decodeIwString(ssid).text;
  l.freq = num(g(link, /freq: ([\d.]+)/));
  l.signal = num(g(station, /^\s*signal:\s+(-?\d+)/m) ?? g(link, /signal: (-?\d+) dBm/));
  l.rxRate = (g(station, /rx bitrate:\s+(.*)/) ?? g(link, /rx bitrate: (.*)/))?.trim();
  l.txRate = (g(station, /tx bitrate:\s+(.*)/) ?? g(link, /tx bitrate: (.*)/))?.trim();
  l.rxBitrate = num(l.rxRate);
  l.txBitrate = num(l.txRate);
  l.txFailed = num(g(station, /tx failed:\s+(\d+)/));
  l.txRetries = num(g(station, /tx retries:\s+(\d+)/));
  l.connectedS = num(g(station, /connected time:\s+(\d+)/));
  return l;
}

// ---------- wpa_supplicant stdout ----------

export interface WpaEvent {
  kind: "assoc-start" | "auth-start" | "assoc" | "handshake" | "connected" | "disconnected" | "wrong-key" | "temp-disabled" | "assoc-reject" | "auth-reject" | "not-found" | "scan" | "eap" | "config-error" | "error" | "info";
  text: string;
  bssid?: string;
  freq?: number;
  reason?: number;
  status?: number;
  detail?: string;
}

/** IEEE 802.11 reason codes worth a word (Table 9-49). */
export const REASONS: Record<number, string> = {
  1: "unspecified", 2: "previous authentication no longer valid", 3: "station leaving", 4: "inactivity",
  5: "AP overloaded", 6: "class 2 frame from unauthenticated station", 7: "class 3 frame from unassociated station",
  8: "station leaving BSS", 9: "not authenticated", 14: "MIC failure", 15: "4-way handshake timeout (wrong key?)",
  16: "group key handshake timeout", 17: "IE mismatch in 4-way handshake", 23: "802.1X authentication failed",
  24: "cipher rejected by policy", 34: "too many unacknowledged frames",
};

/** IEEE 802.11 status codes for association/authentication rejections. */
export const STATUSES: Record<number, string> = {
  1: "unspecified failure", 10: "capabilities not supported", 12: "denied for other reason", 13: "auth algorithm not supported",
  15: "challenge failure", 16: "auth timeout", 17: "AP full (too many stations)", 18: "basic rates not supported",
  30: "rejected temporarily (PMF)", 31: "robust management frame policy violation", 37: "request declined",
  40: "invalid information element", 41: "invalid group cipher", 42: "invalid pairwise cipher", 43: "invalid AKMP",
  53: "invalid PMKID", 72: "invalid RSNE capabilities", 77: "SAE: unsupported finite cyclic group", 126: "SAE: unknown password identifier",
};

/**
 * Classify one wpa_supplicant line. `secrets` are blanked out first: a malformed key line
 * makes wpa_supplicant echo the key itself in its parse error.
 */
export function parseWpaLine(raw: string, secrets: string[] = []): WpaEvent | null {
  let text = raw.trim();
  for (const s of secrets) if (s) text = text.split(s).join("•••");
  text = text.replace(/((?:psk|password|sae_password)\s*[=:]\s*)\S+/gi, "$1•••");
  if (!text) return null;
  const body = text.replace(/^[\w.-]+: /, ""); // drop the "wlan0: " prefix
  let m;
  if ((m = /SME: Trying to authenticate with ([0-9a-f:]{17}) \(SSID='(.*)' freq=(\d+) MHz\)/.exec(body))) return { kind: "auth-start", text: body, bssid: m[1], freq: +m[3] };
  if ((m = /Trying to associate with (?:SSID '.*'|([0-9a-f:]{17}) \(SSID='(.*)' freq=(\d+) MHz\))/.exec(body))) return { kind: "assoc-start", text: body, bssid: m[1], freq: m[3] ? +m[3] : undefined };
  if ((m = /^Associated with ([0-9a-f:]{17})/.exec(body))) return { kind: "assoc", text: body, bssid: m[1] };
  if ((m = /WPA: Key negotiation completed with ([0-9a-f:]{17}) \[(.*)\]/.exec(body))) return { kind: "handshake", text: body, bssid: m[1], detail: m[2] };
  if (/CTRL-EVENT-CONNECTED/.test(body)) return { kind: "connected", text: body, bssid: /Connection to ([0-9a-f:]{17})/.exec(body)?.[1] };
  if (/4-Way Handshake failed|pre-shared key may be incorrect/.test(body)) return { kind: "wrong-key", text: body };
  if ((m = /CTRL-EVENT-SSID-TEMP-DISABLED .*reason=(\S+)/.exec(body))) return { kind: "temp-disabled", text: body, detail: m[1] };
  if ((m = /CTRL-EVENT-DISCONNECTED bssid=([0-9a-f:]{17}) reason=(\d+)/.exec(body))) return { kind: "disconnected", text: body, bssid: m[1], reason: +m[2], detail: REASONS[+m[2]] };
  if ((m = /CTRL-EVENT-ASSOC-REJECT (?:bssid=)?([0-9a-f:]{17}).*status_code=(\d+)/.exec(body))) return { kind: "assoc-reject", text: body, bssid: m[1], status: +m[2], detail: STATUSES[+m[2]] };
  if ((m = /CTRL-EVENT-AUTH-REJECT ([0-9a-f:]{17}).*status_code=(\d+)/.exec(body))) return { kind: "auth-reject", text: body, bssid: m[1], status: +m[2], detail: STATUSES[+m[2]] };
  if (/CTRL-EVENT-NETWORK-NOT-FOUND/.test(body)) return { kind: "not-found", text: body };
  if (/CTRL-EVENT-SCAN-RESULTS/.test(body)) return { kind: "scan", text: body };
  if (/CTRL-EVENT-EAP-|EAP-/.test(body)) return { kind: "eap", text: body };
  if (/^Line \d+:|Failed to read or parse configuration|failed to parse/.test(body)) return { kind: "config-error", text: body };
  if (/Could not|Failed to initialize|nl80211: (?:Failed|Could)|ioctl.*failed/i.test(body) && !/P2P/.test(body)) return { kind: "error", text: body };
  return { kind: "info", text: body };
}

// ---------- udhcpc ----------

export interface DhcpEvent {
  kind: "discover" | "offer" | "ack" | "nak" | "fail" | "lease" | "info";
  text: string;
  ip?: string;
  server?: string;
  leaseS?: number;
  lease?: Record<string, string>;
}

/** Our udhcpc script echoes "DESKYPI-DHCP <event>|key=value|…" with the lease details. */
export function parseUdhcpcLine(raw: string): DhcpEvent | null {
  const text = raw.trim();
  if (!text) return null;
  let m;
  if ((m = /^DESKYPI-DHCP (\w+)\|(.*)$/.exec(text))) {
    const lease: Record<string, string> = { event: m[1] };
    for (const kv of m[2].split("|")) {
      const i = kv.indexOf("=");
      if (i > 0 && kv.slice(i + 1)) lease[kv.slice(0, i)] = kv.slice(i + 1).trim();
    }
    return { kind: "lease", text, lease, ip: lease.ip };
  }
  if (/(?:broadcasting|sending) discover/.test(text)) return { kind: "discover", text };
  if ((m = /(?:broadcasting|sending) select for (\S+), server (\S+)/.exec(text))) return { kind: "offer", text, ip: m[1], server: m[2] };
  if ((m = /lease of (\S+) obtained from (\S+), lease time (\d+)/.exec(text))) return { kind: "ack", text, ip: m[1], server: m[2], leaseS: +m[3] };
  if (/received DHCP NAK|NAK/.test(text)) return { kind: "nak", text };
  if (/no lease, failing/.test(text)) return { kind: "fail", text };
  return { kind: "info", text };
}

/** udhcpc gives the mask as a dotted quad or (newer BusyBox) a prefix length. */
export function maskToPrefix(mask: string): number {
  if (/^\d+$/.test(mask)) return +mask;
  return mask.split(".").reduce((n, o) => n + ((+o >>> 0).toString(2).match(/1/g)?.length || 0), 0);
}

// ---------- ping / nslookup / curl / iperf3 ----------

export interface PingResult {
  sent: number;
  received: number;
  lossPct: number;
  min?: number;
  avg?: number;
  max?: number;
  mdev?: number;
  error?: string;
}

export function parsePing(out: string): PingResult {
  const s = /(\d+) packets transmitted, (\d+) (?:packets )?received.*?(\d+(?:\.\d+)?)% packet loss/.exec(out);
  const r = /= ([\d.]+)\/([\d.]+)\/([\d.]+)\/([\d.]+) ms/.exec(out);
  const err = /^ping: (.*)$/m.exec(out)?.[1];
  return {
    sent: s ? +s[1] : 0,
    received: s ? +s[2] : 0,
    lossPct: s ? +s[3] : 100,
    min: num(r?.[1]),
    avg: num(r?.[2]),
    max: num(r?.[3]),
    mdev: num(r?.[4]),
    error: err,
  };
}

export function parseNslookup(out: string): { server?: string; addresses: string[]; error?: string } {
  const server = /^Server:\s+(\S+)/m.exec(out)?.[1];
  // Addresses before the first "Name:" belong to the server block.
  const i = out.search(/^Name:/m);
  const addresses = i < 0 ? [] : [...out.slice(i).matchAll(/^Address(?: \d+)?:\s+(\S+)/gm)].map((m) => m[1]);
  const error = /\*\* (.*)/.exec(out)?.[1] || /(connection timed out.*|can't resolve.*|no servers could be reached)/.exec(out)?.[1];
  return { server, addresses: [...new Set(addresses)], error: addresses.length ? undefined : error || "no answer" };
}

export interface HttpResult {
  code: number;
  ok: boolean;
  remoteIp?: string;
  httpVersion?: string;
  dnsMs?: number;
  connectMs?: number;
  tlsMs?: number;
  firstByteMs?: number;
  totalMs?: number;
  bytes?: number;
  speedBps?: number;
  error?: string;
}

export function parseCurlJson(out: string): HttpResult {
  let j: any;
  try {
    j = JSON.parse(out.slice(out.indexOf("{")));
  } catch {
    return { code: 0, ok: false, error: out.trim().split("\n").pop() || "curl produced no output" };
  }
  const ms = (s: any) => (typeof s === "number" && s > 0 ? Math.round(s * 10000) / 10 : undefined);
  const code = j.http_code || j.response_code || 0;
  return {
    code,
    ok: !j.errormsg && code > 0 && code < 400,
    remoteIp: j.remote_ip || undefined,
    httpVersion: j.http_version || undefined,
    dnsMs: ms(j.time_namelookup),
    connectMs: ms(j.time_connect),
    tlsMs: ms(j.time_appconnect),
    firstByteMs: ms(j.time_starttransfer),
    totalMs: ms(j.time_total),
    bytes: j.size_download,
    speedBps: j.speed_download,
    error: j.errormsg || (code >= 400 ? `HTTP ${code}` : undefined),
  };
}

export function parseIperf(out: string): { sentBps?: number; receivedBps?: number; retransmits?: number; error?: string } {
  try {
    const j = JSON.parse(out);
    if (j.error) return { error: j.error };
    const e = j.end || {};
    return { sentBps: e.sum_sent?.bits_per_second, receivedBps: e.sum_received?.bits_per_second, retransmits: e.sum_sent?.retransmits };
  } catch {
    return { error: out.trim().split("\n").pop() || "iperf3 produced no output" };
  }
}

// ---------- OUI vendor lookup ----------

/**
 * Parse whichever OUI list is installed: IEEE's oui.txt ("AA-BB-CC   (hex)\t\tVendor"),
 * nmap's ("AABBCC Vendor") or Wireshark's manuf ("AA:BB:CC\tShort\tLong").
 */
export function parseOui(text: string): Map<string, string> {
  const map = new Map<string, string>();
  for (const line of text.split("\n")) {
    if (/\(base 16\)/.test(line)) continue; // oui.txt repeats each entry in this form
    let m = /^([0-9A-F]{2})-([0-9A-F]{2})-([0-9A-F]{2})\s+\(hex\)\s+(.+)$/i.exec(line);
    if (m) {
      map.set((m[1] + m[2] + m[3]).toLowerCase(), m[4].trim());
      continue;
    }
    m = /^([0-9A-F]{2}):([0-9A-F]{2}):([0-9A-F]{2})\s+(\S+)(?:\s+(.+))?$/i.exec(line);
    if (m) {
      map.set((m[1] + m[2] + m[3]).toLowerCase(), (m[5] || m[4]).trim());
      continue;
    }
    m = /^([0-9A-F]{6})\s+(.+)$/i.exec(line);
    if (m) map.set(m[1].toLowerCase(), m[2].trim());
  }
  return map;
}

// ---------- Scan scheduling + history ----------

/** Keep samples newer than maxAgeMs, and at most `max` of them (newest win). */
export function trimHistory(h: [number, number][], now: number, maxAgeMs: number, max: number): [number, number][] {
  let i = 0;
  while (i < h.length && now - h[i][0] > maxAgeMs) i++;
  return h.slice(Math.max(i, h.length - max));
}

/**
 * Is a scan due? Background auto-scan and "someone has the panel open" both ask for scans;
 * the shorter applicable interval wins, measured from the last scan of either kind, so two
 * viewers (or a viewer plus auto-scan) never double the rate.
 */
export function scanDue(o: { now: number; lastScan: number; busy: boolean; auto: { enabled: boolean; intervalS: number }; watching: boolean; watchIntervalS: number }): boolean {
  if (o.busy) return false;
  const ivs: number[] = [];
  if (o.auto.enabled) ivs.push(Math.max(15, o.auto.intervalS));
  if (o.watching) ivs.push(Math.max(15, o.watchIntervalS));
  if (!ivs.length) return false;
  return o.now - o.lastScan >= Math.min(...ivs) * 1000;
}
