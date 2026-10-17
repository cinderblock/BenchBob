// ==================== Wi-Fi driver ====================
//
// Radios (every phy: driver, bus, bands/channels under the current regulatory domain,
// iftypes, what it's doing right now), a scanner with per-BSS history, and client tests
// that borrow a radio into an isolated network namespace (lib/wifi-client.ts).
//
// The onboard brcmfmac wlan0 on desky is NetworkManager's backup uplink on the same /20 as
// eth0, so the rules here are conservative: scans never change its connection, a test only
// borrows it when the main default route is via something else that's up, and the runner
// always gives it back and checks NM reconnected it. USB dongles show up as new phys; when
// DeskyPi uses one it tells NM (runtime only) to leave it alone, so NM's autoconnect
// doesn't grab it mid-test.
//
// Status stays small: the bulky lists (radio details, BSS table, results) are fetched over
// REST when their sequence numbers change. The running test's timeline rides in status so
// progress streams to the dashboard.

import { readlinkSync, realpathSync, existsSync } from "node:fs";
import { HttpError, body, run, usbName, type Ctx, type Driver, type UdevEvent } from "../core.ts";
import * as P from "../lib/wifi-parse.ts";
import { ClientTest, borrowRefusal, recoverNamespaces, validHost, validKey, validUrl, type Bins, type RadioCtx, type Security, type TestResult, type TestSpec } from "../lib/wifi-client.ts";

let ctx: Ctx;

// ---------- Binaries ----------

// The service's PATH (and a non-login SSH one) lacks /usr/sbin, where iw, wpa_supplicant
// and udhcpc live, so resolve everything once against an explicit PATH.
const SEARCH = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const which = (n: string) => Bun.which(n, { PATH: SEARCH }) || n;
const B: Bins = {
  sudo: process.getuid?.() === 0 ? [] : ["sudo", "-n"],
  ip: which("ip"),
  iw: which("iw"),
  wpa: which("wpa_supplicant"),
  udhcpc: which("udhcpc"),
  nmcli: which("nmcli"),
  ping: which("ping"),
  curl: which("curl"),
  busybox: which("busybox"),
  iperf3: which("iperf3"),
  stdbuf: which("stdbuf"),
  kill: which("kill"),
  rm: which("rm"),
  mkdir: which("mkdir"),
  tee: which("tee"),
};
const sudo = (cmd: string[], timeoutMs = 20000) => run([...B.sudo, ...cmd], { timeoutMs });

// ---------- State ----------

interface RadioPrefs {
  label?: string;
  keepUnmanaged?: boolean; // always tell NM to leave this radio alone
  scanMethod?: "auto" | "iw" | "nm";
}

interface Radio {
  phy: string;
  index: number;
  stableId: string;
  driver: string;
  bus: "usb" | "sdio" | "pci" | "platform" | "virtual" | "unknown";
  onboard: boolean;
  usb?: { vendor: string; model: string; id: string; port: string };
  ifaces: P.IwIface[];
  info?: P.PhyInfo;
  nm: { iface: string; state: string; connection: string }[];
  use: { kind: "primary" | "backup" | "connected" | "ap" | "idle" | "down" | "test" | "scan"; text: string };
  unmanagedByUs: string[];
  busy?: "test" | "scan";
  prefs: RadioPrefs;
}

interface BssRec extends P.Bss {
  vendor?: string;
  radio: string;
  firstSeen: number;
  lastSeen: number;
  history: [number, number][]; // [t, dBm]
}

interface ScanMeta {
  seq: number;
  at: number;
  radio: string;
  iface: string;
  method: string;
  durationMs: number;
  count: number;
  error?: string;
  survey: P.Survey[] | null;
  surveySupported: boolean;
}

interface Profile {
  id: string;
  name: string;
  ssid: string;
  security: Security;
  psk?: string;
  useSavedKey?: boolean;
  eap?: TestSpec["eap"];
  bssid?: string;
  ip?: TestSpec["ip"];
  static?: TestSpec["static"];
  pingGateway?: boolean;
  dns?: string[];
  http?: string[];
  ping?: string[];
  iperf?: TestSpec["iperf"];
  count?: number;
  holdS?: number;
}

const radios = new Map<string, Radio>();
let reg: P.RegDomain[] = [];
let regChangedFrom: string | null = null;
let radiosSeq = 0;
let nmWifi: { name: string; ssid: string; keyMgmt: string }[] = [];
let nmWifiAt = 0;
let routes: { dev: string; metric: number; gateway?: string }[] = [];
const bss = new Map<string, BssRec>();
let scan: ScanMeta = { seq: 0, at: 0, radio: "", iface: "", method: "", durationMs: 0, count: 0, survey: null, surveySupported: false };
const scanning = new Set<string>();
const tests = new Map<string, ClientTest>(); // running, by id
let results: TestResult[] = [];
let resultsSeq = 0;
let oui: Map<string, string> | null = null;
let ouiSource = "";
let timers: ReturnType<typeof setInterval>[] = [];
let refreshTimer: ReturnType<typeof setTimeout> | null = null;
let lastAutoScan = 0;
let stopping = false;

const RESULTS_MAX = 20;
const HISTORY_MAX = 120;
const BSS_MAX_AGE_MS = 60 * 60 * 1000;

interface AutoScan {
  enabled: boolean;
  intervalS: number;
  radio: string; // "" = first available
  method: "auto" | "iw" | "nm";
}
const autoScan = (): AutoScan => ({ enabled: false, intervalS: 60, radio: "", method: "auto", ...ctx.settings.get<Partial<AutoScan>>("autoScan", {}) });
const profiles = (): Profile[] => ctx.settings.get<Profile[]>("profiles", []);
const prefsAll = (): Record<string, RadioPrefs> => ctx.settings.get<Record<string, RadioPrefs>>("radioPrefs", {});

// ---------- Helpers ----------

/** nmcli -t output escapes ':' inside values as '\:'. */
function nmSplit(line: string): string[] {
  const out: string[] = [];
  let cur = "";
  for (let i = 0; i < line.length; i++) {
    if (line[i] === "\\" && i + 1 < line.length) cur += line[++i];
    else if (line[i] === ":") {
      out.push(cur);
      cur = "";
    } else cur += line[i];
  }
  out.push(cur);
  return out;
}

async function loadRoutes() {
  const r = await run([B.ip, "-j", "route", "show", "default"]);
  try {
    routes = (JSON.parse(r.out || "[]") as any[]).map((x) => ({ dev: x.dev, metric: x.metric ?? 0, gateway: x.gateway }));
  } catch {
    routes = [];
  }
}

async function uplink(): Promise<string | undefined> {
  await loadRoutes();
  return [...routes].sort((a, b) => a.metric - b.metric)[0]?.dev;
}

function findRadio(key: string | undefined): Radio {
  if (!key) {
    const r = [...radios.values()].find((x) => x.ifaces.some((i) => i.name && i.type === "managed")) || [...radios.values()][0];
    if (!r) throw new HttpError(404, "No Wi-Fi radios in the main network namespace (a radio borrowed into another DeskyPi process's test namespace is invisible until it comes back)");
    return r;
  }
  const r = radios.get(key) || [...radios.values()].find((x) => x.ifaces.some((i) => i.name === key) || x.stableId === key);
  if (!r) throw new HttpError(404, `No Wi-Fi radio "${key}"`);
  return r;
}

const managedIface = (r: Radio) => r.ifaces.find((i) => i.name && i.type === "managed")?.name || r.ifaces.find((i) => i.name)?.name;

function loadOui() {
  for (const p of ["/usr/share/ieee-data/oui.txt", "/var/lib/ieee-data/oui.txt", "/usr/share/misc/oui.txt", "/usr/share/nmap/nmap-mac-prefixes", "/usr/share/wireshark/manuf"]) {
    if (!existsSync(p)) continue;
    Bun.file(p)
      .text()
      .then((t) => {
        oui = P.parseOui(t);
        ouiSource = p;
        ctx.log(`OUI vendors: ${oui.size} from ${p}`);
      })
      .catch(() => {});
    return;
  }
}

// ---------- Radios ----------

function describeBus(phy: string): Pick<Radio, "driver" | "bus" | "onboard" | "usb" | "stableId"> {
  let driver = "";
  let devpath = "";
  try {
    driver = readlinkSync(`/sys/class/ieee80211/${phy}/device/driver`).split("/").pop() || "";
  } catch {}
  try {
    devpath = realpathSync(`/sys/class/ieee80211/${phy}`).replace(/^\/sys/, "");
  } catch {}
  const usbDev = devpath ? ctx.ancestor(devpath, "usb", "usb_device") : null;
  if (usbDev) {
    const n = usbName(usbDev);
    return {
      driver,
      bus: "usb",
      onboard: false,
      usb: { vendor: n.vendor, model: n.model, id: `${usbDev.ID_VENDOR_ID || "?"}:${usbDev.ID_MODEL_ID || "?"}`, port: usbDev.DEVPATH?.split("/").pop() || "" },
      stableId: "usb:" + (usbDev.ID_SERIAL || usbDev.ID_PATH || usbDev.DEVPATH),
    };
  }
  // mac80211_hwsim radios live under /devices/virtual: treat them like dongles, not onboard.
  const bus = /\/virtual\//.test(devpath) ? "virtual" : /\/mmc/.test(devpath) ? "sdio" : /\/pci/.test(devpath) ? "pci" : devpath ? "platform" : "unknown";
  return { driver, bus, onboard: bus !== "virtual", stableId: `${bus}:${driver}:${devpath.replace(/\/ieee80211\/phy\d+$/, "")}` };
}

function describeUse(r: Radio): Radio["use"] {
  if (r.busy === "test") return { kind: "test", text: "Borrowed for a client test (in its own network namespace)" };
  const primary = [...routes].sort((a, b) => a.metric - b.metric)[0];
  for (const i of r.ifaces) {
    if (!i.name) continue;
    if (i.type === "AP") return { kind: "ap", text: `Access point on ${i.name}${i.ssid ? ` (${i.ssid})` : ""}` };
    const nm = r.nm.find((n) => n.iface === i.name);
    const via = routes.find((x) => x.dev === i.name);
    if (i.ssid || nm?.state === "connected") {
      const net = i.ssid || nm?.connection || "?";
      if (via && primary?.dev === i.name) return { kind: "primary", text: `Primary uplink, connected to ${net}` };
      if (via) return { kind: "backup", text: `Backup uplink, connected to ${net}` };
      return { kind: "connected", text: `Connected to ${net} (no default route)` };
    }
  }
  if (r.busy === "scan") return { kind: "scan", text: "Scanning" };
  const nm = r.nm[0];
  if (r.unmanagedByUs.length) return { kind: "idle", text: `Idle; DeskyPi told NetworkManager to leave ${r.unmanagedByUs.join(", ")} alone (until reboot)` };
  if (nm?.state === "unavailable") return { kind: "down", text: "Idle (interface down or unavailable)" };
  return { kind: "idle", text: nm?.state === "unmanaged" ? "Idle (not managed by NetworkManager)" : "Idle" };
}

async function refreshNmWifi(force = false) {
  if (!force && Date.now() - nmWifiAt < 60000) return;
  nmWifiAt = Date.now();
  const r = await run([B.nmcli, "-t", "-f", "NAME,TYPE", "con", "show"]);
  const out: typeof nmWifi = [];
  for (const l of r.out.split("\n")) {
    const [name, type] = nmSplit(l);
    if (type !== "802-11-wireless") continue;
    const d = await run([B.nmcli, "-g", "802-11-wireless.ssid,802-11-wireless-security.key-mgmt", "con", "show", name]);
    const [ssid, keyMgmt] = d.out.split("\n");
    out.push({ name, ssid: ssid || "", keyMgmt: keyMgmt || "" });
  }
  nmWifi = out;
}

async function refreshRadios(opts: { phyInfo?: boolean } = {}) {
  const [dev, regOut, nmDev] = await Promise.all([run([B.iw, "dev"]), run([B.iw, "reg", "get"]), run([B.nmcli, "-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "dev"]), loadRoutes(), refreshNmWifi()]);
  reg = P.parseIwReg(regOut.out);
  const ifs = P.parseIwDev(dev.out);
  const nm = nmDev.out.split("\n").map(nmSplit).filter((x) => x.length >= 4);
  // `iw dev` omits phys with no interfaces; `iw phy` lists them all.
  const phys = new Set(ifs.map((i) => i.phy));
  for (const m of (await run([B.iw, "list"])).out.matchAll(/^Wiphy (\S+)/gm)) phys.add(m[1]);
  const prefs = prefsAll();
  const seen = new Set<string>();
  for (const phy of phys) {
    seen.add(phy);
    let r = radios.get(phy);
    const isNew = !r;
    if (!r) {
      r = { phy, index: +phy.replace(/\D/g, ""), ...describeBus(phy), ifaces: [], nm: [], use: { kind: "idle", text: "" }, unmanagedByUs: [], prefs: {} };
      radios.set(phy, r);
    }
    if (r.busy === "test") continue; // it's in the netns; keep what we knew
    r.prefs = prefs[r.stableId] || {};
    r.ifaces = ifs.filter((i) => i.phy === phy);
    r.nm = r.ifaces.filter((i) => i.name).map((i) => {
      const row = nm.find((x) => x[0] === i.name);
      return { iface: i.name!, state: row?.[2] || "", connection: row?.[3] && row[3] !== "--" ? row[3] : "" };
    });
    if (isNew || opts.phyInfo || !r.info) {
      const info = await run([B.iw, "phy", phy, "info"]);
      if (!info.code) r.info = P.parseIwPhy(info.out);
    }
    if (isNew && r.prefs.keepUnmanaged) await unmanage(r).catch(() => {});
    if (isNew && !r.onboard) ctx.event(`Wi-Fi radio ${phy} appeared: ${r.usb?.vendor || ""} ${r.usb?.model || r.driver}`.trim(), { phy, driver: r.driver });
    r.use = describeUse(r);
  }
  for (const [phy, r] of radios) {
    if (seen.has(phy) || r.busy === "test") continue;
    radios.delete(phy);
    ctx.event(`Wi-Fi radio ${phy} removed (${r.usb?.model || r.driver})`, { phy });
  }
  radiosSeq++;
  ctx.changed();
}

function scheduleRefresh(ms = 800) {
  if (refreshTimer) clearTimeout(refreshTimer);
  refreshTimer = setTimeout(() => {
    refreshTimer = null;
    refreshRadios().catch((e) => ctx.log("refresh:", e.message));
  }, ms);
}

/** Tell NM (runtime only) to leave a radio DeskyPi is using alone. Never for NM's own connected radio. */
async function unmanage(r: Radio) {
  for (const n of r.nm) {
    if (n.state === "connected" || n.state === "unmanaged" || r.unmanagedByUs.includes(n.iface)) continue;
    const res = await sudo([B.nmcli, "dev", "set", n.iface, "managed", "no"]);
    if (res.code) throw new Error(`nmcli dev set ${n.iface} managed no: ${res.err}`);
    r.unmanagedByUs.push(n.iface);
    n.state = "unmanaged";
    ctx.event(`Told NetworkManager to leave ${n.iface} alone (runtime only, DeskyPi is using it)`);
  }
}

function radioJson(r: Radio, full = false) {
  const base = {
    phy: r.phy,
    stableId: r.stableId,
    label: r.prefs.label || "",
    driver: r.driver,
    bus: r.bus,
    onboard: r.onboard,
    usb: r.usb,
    ifaces: r.ifaces.map((i) => ({ name: i.name, type: i.type, addr: i.addr, ssid: i.ssid, channel: i.channel, freq: i.freq, width: i.width, txpower: i.txpower })),
    nm: r.nm,
    use: r.use,
    busy: r.busy || null,
    unmanagedByUs: r.unmanagedByUs,
    bands: r.info?.bands.map((b) => b.band) || [],
    monitor: !!r.info?.monitor,
    ap: !!r.info?.ap,
    netns: !!r.info?.netns,
    prefs: r.prefs,
    regCountry: reg.find((x) => x.scope === r.phy)?.country,
  };
  if (!full) return base;
  return {
    ...base,
    iftypes: r.info?.iftypes || [],
    ciphers: r.info?.ciphers || [],
    sae: !!r.info?.sae,
    bandInfo: (r.info?.bands || []).map((b) => ({ band: b.band, ht: b.ht, vht: b.vht, he: b.he, eht: b.eht, maxWidth: b.maxWidth, channels: b.channels.map((c) => ({ ch: c.channel, mhz: c.mhz, disabled: c.disabled, radar: c.radar, noIR: c.noIR, maxPower: c.maxPower, flags: c.flags })) })),
    reg: reg.find((x) => x.scope === r.phy) || null,
  };
}

function regJson() {
  const g = reg.find((x) => x.scope === "global");
  return { country: g?.country || "", dfsRegion: g?.dfsRegion || "", rules: g?.rules || [], phys: reg.filter((x) => x.scope !== "global"), changedFrom: regChangedFrom };
}

function radiosJson() {
  return {
    radios: [...radios.values()].map((r) => radioJson(r, true)),
    reg: regJson(),
    savedNetworks: nmWifi.map((n) => ({ connection: n.name, ssid: n.ssid, keyMgmt: n.keyMgmt })),
    ouiSource: ouiSource || null,
    uplink: [...routes].sort((a, b) => a.metric - b.metric)[0]?.dev || null,
  };
}

async function setReg(country: string) {
  const c = String(country || "").toUpperCase();
  if (!/^([A-Z]{2}|00)$/.test(c)) throw new HttpError(400, "Country must be a two-letter ISO code, or 00 for world");
  const before = reg.find((x) => x.scope === "global")?.country || "";
  const r = await sudo([B.iw, "reg", "set", c]);
  if (r.code) throw new HttpError(500, `iw reg set ${c}: ${r.err || r.out}`);
  // The kernel applies it asynchronously (it asks the regulatory db / CRDA).
  await Bun.sleep(1500);
  await refreshRadios({ phyInfo: true });
  const now = reg.find((x) => x.scope === "global")?.country || "";
  if (regChangedFrom === null && before !== c) regChangedFrom = before;
  if (regChangedFrom === now) regChangedFrom = null;
  ctx.event(`Wi-Fi regulatory domain set to ${c} (now ${now}; temporary, until reboot)`);
  // Seen on desky: asking for GB while wlan0 was associated to an AP advertising "US" gave
  // "98" (cfg80211's intersection of the user hint and the AP's country IE), and asking
  // for US again kept 98 until wlan0's connection dropped and the core reset itself.
  const notes: string[] = [];
  if (now !== c) notes.push(`The kernel applied ${now}, not ${c}: ${now === "98" ? "98 is the intersection of your request with the country an associated AP advertises" : "another regulatory hint takes precedence"}. It recalculates when that connection drops (a client test on the radio does this).`);
  if (reg.some((x) => x.selfManaged)) notes.push("Some radios are self-managed and keep their own regulatory domain.");
  return { ok: true, requested: c, ...regJson(), note: notes.join(" ") || undefined };
}

// ---------- Scanner ----------

async function doScan(radioKey?: string, methodIn?: string) {
  const r = findRadio(radioKey);
  if (r.busy === "test") throw new HttpError(409, `${r.phy} is busy with a client test`);
  if (scanning.has(r.phy)) throw new HttpError(409, `${r.phy} is already scanning`);
  const iface = managedIface(r);
  if (!iface) throw new HttpError(409, `${r.phy} has no network interface to scan with`);
  const method = methodIn && methodIn !== "auto" ? methodIn : r.prefs.scanMethod && r.prefs.scanMethod !== "auto" ? r.prefs.scanMethod : "auto";
  if (!["auto", "iw", "nm"].includes(method)) throw new HttpError(400, "method must be auto, iw or nm");
  scanning.add(r.phy);
  r.busy = "scan";
  ctx.changed();
  const t0 = Date.now();
  let used = method;
  try {
    const nmOwn = r.nm.find((n) => n.iface === iface);
    // A radio NM isn't using (a dongle) gets taken away from NM before we touch it.
    if (nmOwn && nmOwn.state !== "connected") await unmanage(r);
    const link = await run([B.ip, "-j", "link", "show", iface]);
    if (!/"UP"/.test(link.out)) await sudo([B.ip, "link", "set", iface, "up"]);

    let out = "";
    let err = "";
    if (method !== "nm") {
      // `iw scan` works while NM is connected (verified on desky); it can collide with
      // NM's own background scans (EBUSY), so retry before falling back.
      for (let i = 0; i < 3; i++) {
        const s = await sudo([B.iw, "dev", iface, "scan"], 30000);
        if (!s.code) {
          out = s.out;
          err = "";
          used = "iw";
          break;
        }
        err = s.err || s.out;
        if (!/busy|-16/i.test(err)) break;
        await Bun.sleep(1500);
      }
    }
    if (!out && (method === "nm" || (method === "auto" && nmOwn && nmOwn.state !== "unmanaged"))) {
      // NM's rescan only triggers; results land in the kernel BSS cache, which
      // `iw scan dump` reads with full IE detail.
      const t = await sudo([B.nmcli, "dev", "wifi", "rescan", "ifname", iface]);
      await Bun.sleep(t.code ? 500 : 5000);
      const d = await sudo([B.iw, "dev", iface, "scan", "dump"]);
      if (!d.code) {
        out = d.out;
        err = "";
        used = "nm";
      } else err = err || d.err;
    }
    if (!out && err) throw new HttpError(500, `Scan on ${iface} failed: ${err}`);

    const list = P.parseIwScan(out);
    const now = Date.now();
    for (const b of list) {
      const rec: BssRec = bss.get(b.bssid) || ({ firstSeen: now, history: [], lastSeen: 0 } as any);
      const fresh = b.lastSeenMs == null || b.lastSeenMs < 15000;
      Object.assign(rec, b, { radio: r.phy });
      if (fresh) {
        rec.lastSeen = now - (b.lastSeenMs || 0);
        if (b.signal != null) rec.history.push([now, b.signal]);
        if (rec.history.length > HISTORY_MAX) rec.history.splice(0, rec.history.length - HISTORY_MAX);
      } else if (!rec.lastSeen) rec.lastSeen = now - (b.lastSeenMs || 0);
      if (oui && !b.localMac) rec.vendor = oui.get(b.bssid.replace(/:/g, "").slice(0, 6));
      bss.set(b.bssid, rec);
    }
    for (const [k, v] of bss) if (now - v.lastSeen > BSS_MAX_AGE_MS) bss.delete(k);

    const sv = await sudo([B.iw, "dev", iface, "survey", "dump"]);
    const survey = P.parseSurvey(sv.out).filter((s) => P.bandOf(s.freq)); // drop S1G etc.
    scan = { seq: scan.seq + 1, at: now, radio: r.phy, iface, method: used, durationMs: now - t0, count: list.length, survey: survey.length ? survey : null, surveySupported: survey.length > 0 };
    return scanJson();
  } catch (e: any) {
    scan = { ...scan, seq: scan.seq + 1, at: Date.now(), radio: r.phy, iface, error: e.message, durationMs: Date.now() - t0 };
    throw e;
  } finally {
    scanning.delete(r.phy);
    if (r.busy === "scan") r.busy = undefined;
    r.use = describeUse(r);
    ctx.changed();
  }
}

function scanJson(filter: { ssid?: string; band?: string; limit?: number } = {}) {
  let list = [...bss.values()].sort((a, b) => (b.signal ?? -200) - (a.signal ?? -200));
  if (filter.ssid) list = list.filter((b) => b.ssid.toLowerCase().includes(filter.ssid!.toLowerCase()));
  if (filter.band) list = list.filter((b) => b.band === filter.band);
  if (filter.limit) list = list.slice(0, filter.limit);
  return { scan, bss: list, ouiSource: ouiSource || null };
}

// ---------- Client tests ----------

let testCounter = 0;

async function savedKey(ssid: string): Promise<{ key: string; connection: string } | null> {
  await refreshNmWifi(true);
  const c = nmWifi.find((n) => n.ssid === ssid);
  if (!c) return null;
  const r = await sudo([B.nmcli, "-s", "-g", "802-11-wireless-security.psk", "con", "show", c.name]);
  return r.out ? { key: r.out, connection: c.name } : null;
}

const strList = (v: any, name: string): string[] => {
  if (v == null || v === "") return [];
  const a = Array.isArray(v) ? v : String(v).split(/[\s,]+/);
  const out = a.map((x) => String(x).trim()).filter(Boolean);
  if (out.length > 10) throw new HttpError(400, `At most 10 ${name}`);
  return out;
};

/** Normalise a test request (REST or MCP) into a spec + key. Throws HttpError on bad input. */
async function resolveSpec(input: any): Promise<{ spec: TestSpec; key?: string; save?: string }> {
  let base: Partial<Profile> = {};
  if (input.profile) {
    base = profiles().find((p) => p.id === input.profile || p.name === input.profile) || (() => {
      throw new HttpError(404, `No profile "${input.profile}"`);
    })();
  }
  const o = { ...base, ...Object.fromEntries(Object.entries(input).filter(([, v]) => v !== undefined && v !== "")) } as any;
  const r = findRadio(o.radio);
  const ssid = String(o.ssid || "");
  if (!ssid || Buffer.byteLength(ssid) > 32) throw new HttpError(400, "ssid is required (1–32 bytes)");
  const scanned = [...bss.values()].filter((b) => b.ssid === ssid).sort((a, b) => (b.signal ?? -200) - (a.signal ?? -200))[0];
  let security: Security = o.security || (scanned?.securityKind !== "wep" ? (scanned?.securityKind as Security) : undefined) || (o.psk || o.useSavedKey ? "psk" : "open");
  if ((security as string) === "wep") throw new HttpError(400, "WEP isn't supported");
  if (!["open", "owe", "psk", "sae", "psk-sae", "eap"].includes(security)) throw new HttpError(400, "security must be open, owe, psk, sae, psk-sae or eap");

  let key: string | undefined = typeof input.psk === "string" && input.psk ? input.psk : base.psk;
  let keySource: TestSpec["keySource"] = input.psk ? "given" : base.psk ? "profile" : "none";
  if (!key && o.useSavedKey && security !== "open" && security !== "owe") {
    const s = await savedKey(ssid);
    if (!s) throw new HttpError(400, `NetworkManager has no saved key for "${ssid}"`);
    key = s.key;
    keySource = "networkmanager";
    // NM keeps WPA2 keys as a 64-hex PMK; SAE needs the passphrase, so fall back to WPA2.
    if (/^[0-9a-f]{64}$/i.test(key) && security === "sae") throw new HttpError(400, "NetworkManager stores this network's key as a hashed PSK, which can't be used for WPA3-SAE");
  }
  const keyErr = validKey(security, key);
  if (keyErr) throw new HttpError(400, keyErr);

  const ip = (o.ip || (o.dhcp === false ? "none" : "dhcp")) as TestSpec["ip"];
  if (!["dhcp", "static", "none"].includes(ip)) throw new HttpError(400, "ip must be dhcp, static or none");
  let stat: TestSpec["static"];
  if (ip === "static") {
    const s = o.static || {};
    if (!/^\d+\.\d+\.\d+\.\d+\/\d+$/.test(s.address || "")) throw new HttpError(400, "static.address must look like 192.168.1.50/24");
    if (s.gateway && !/^\d+\.\d+\.\d+\.\d+$/.test(s.gateway)) throw new HttpError(400, "static.gateway must be an IPv4 address");
    const dns = strList(s.dns, "DNS servers");
    if (dns.some((d) => !/^[\d.:a-fA-F]+$/.test(d))) throw new HttpError(400, "static.dns must be IP addresses");
    stat = { address: s.address, gateway: s.gateway || undefined, dns };
  }
  const dns = strList(o.dns ?? ["example.com"], "DNS names");
  const http = strList(o.http ?? ["https://example.com/"], "URLs");
  const ping = strList(o.ping, "ping targets");
  for (const h of [...dns, ...ping]) if (!validHost(h)) throw new HttpError(400, `Bad host name "${h}"`);
  for (const u of http) if (!validUrl(u)) throw new HttpError(400, `Bad URL "${u}" (http or https only)`);
  let iperf: TestSpec["iperf"] = null;
  if (o.iperf?.server) {
    if (!validHost(o.iperf.server)) throw new HttpError(400, "Bad iperf3 server");
    iperf = { server: o.iperf.server, port: Math.min(65535, Math.max(1, +o.iperf.port || 5201)), seconds: Math.min(60, Math.max(1, +o.iperf.seconds || 5)), reverse: !!o.iperf.reverse };
  }
  if (o.bssid && !/^[0-9a-f]{2}(:[0-9a-f]{2}){5}$/i.test(o.bssid)) throw new HttpError(400, "bssid must look like aa:bb:cc:dd:ee:ff");
  if (security === "eap" && !o.eap?.identity) throw new HttpError(400, "eap.identity is required for Enterprise");
  const spec: TestSpec = {
    phy: r.phy,
    ssid,
    security,
    bssid: o.bssid?.toLowerCase() || undefined,
    eap: security === "eap" ? { method: o.eap.method === "TTLS" ? "TTLS" : "PEAP", identity: String(o.eap.identity), anonymousIdentity: o.eap.anonymousIdentity || undefined } : undefined,
    ip,
    static: stat,
    pingGateway: o.pingGateway !== false,
    dns,
    http,
    ping,
    iperf,
    count: Math.min(50, Math.max(1, +o.count || 5)),
    holdS: Math.min(600, Math.max(0, +o.holdS || 0)),
    assocTimeoutS: Math.min(120, Math.max(5, +o.assocTimeoutS || 30)),
    keySource,
    profile: base.id,
  };
  return { spec, key, save: typeof input.saveAs === "string" && input.saveAs.trim() ? input.saveAs.trim() : undefined };
}

/** Refuse to borrow a radio whose interface is the working uplink. */
async function safetyCheck(r: Radio) {
  await loadRoutes();
  const mine = new Set(r.ifaces.map((i) => i.name).filter(Boolean) as string[]);
  // Link state of every interface, so the pure rule can ask about the best other route.
  const up = new Set<string>();
  try {
    for (const l of JSON.parse((await run([B.ip, "-j", "link", "show"])).out)) if (l.operstate === "UP" && !(l.flags || []).includes("NO-CARRIER")) up.add(l.ifname);
  } catch {}
  const why = borrowRefusal(routes, mine, (dev) => up.has(dev));
  if (why) throw new HttpError(409, why);
}

async function startTest(input: any): Promise<ClientTest> {
  const { spec, key, save } = await resolveSpec(input);
  const r = radios.get(spec.phy)!;
  if (r.busy === "test") throw new HttpError(409, `${r.phy} is already running a client test`);
  // An (auto-)scan takes ~4 s; wait it out rather than fail the caller.
  for (let i = 0; i < 80 && scanning.has(r.phy); i++) await Bun.sleep(250);
  if (scanning.has(r.phy)) throw new HttpError(409, `${r.phy} is scanning; try again in a few seconds`);
  if (!r.info?.netns) throw new HttpError(409, `${r.phy}'s driver can't move into a network namespace (no set_wiphy_netns)`);
  await safetyCheck(r);
  if (save) saveProfile({ name: save, ssid: spec.ssid, security: spec.security, psk: spec.keySource === "given" ? key : undefined, useSavedKey: spec.keySource === "networkmanager" || undefined, eap: spec.eap, bssid: spec.bssid, ip: spec.ip, static: spec.static, pingGateway: spec.pingGateway, dns: spec.dns, http: spec.http, ping: spec.ping, iperf: spec.iperf, count: spec.count, holdS: spec.holdS });

  const rc: RadioCtx = {
    phy: r.phy,
    ifaces: r.ifaces.map((i) => i.name).filter(Boolean) as string[],
    nm: r.nm.filter((n) => n.state === "connected" && n.connection).map((n) => ({ iface: n.iface, connection: n.connection })),
    unmanage: [...new Set([...r.unmanagedByUs, ...(r.prefs.keepUnmanaged ? (r.ifaces.map((i) => i.name).filter(Boolean) as string[]) : [])])],
  };
  // A dongle NM isn't using: stop NM grabbing it when it comes back.
  if (!rc.nm.length) {
    await unmanage(r).catch(() => {});
    rc.unmanage = [...new Set([...rc.unmanage, ...r.unmanagedByUs])];
  }
  const id = `${Date.now().toString(36)}-${++testCounter}`;
  const t = new ClientTest(id, spec, key, rc, { bins: B, spawn: (c, o) => ctx.spawn(c, o), changed: () => ctx.changed(), log: ctx.log, uplink });
  r.busy = "test";
  r.use = describeUse(r);
  tests.set(id, t);
  // Remember what to put back if DeskyPi dies mid-test (recovered on next init).
  ctx.settings.set("borrowed", { ...ctx.settings.get<Record<string, RadioCtx>>("borrowed", {}), [r.phy]: rc });
  ctx.event(`Wi-Fi client test started: ${spec.ssid} on ${r.phy}`);
  t.run().then((res) => {
    tests.delete(id);
    r.busy = undefined;
    const b = { ...ctx.settings.get<Record<string, RadioCtx>>("borrowed", {}) };
    delete b[r.phy];
    ctx.settings.set("borrowed", b);
    results.unshift(res);
    results = results.slice(0, RESULTS_MAX);
    ctx.settings.set("results", results);
    resultsSeq++;
    ctx.event(`Wi-Fi client test ${res.ok ? "passed" : "failed"}: ${spec.ssid} on ${r.phy}${res.error ? ` (${res.error})` : ""}`);
    scheduleRefresh(500);
    // NM may take a moment longer to settle the route table; refresh again.
    scheduleRefresh(5000);
  });
  ctx.changed();
  return t;
}

function liveJson(t: ClientTest) {
  return { ...t.r, running: true };
}

function resultSummary(r: TestResult) {
  return { id: r.id, phy: r.phy, ssid: r.ssid, security: r.security, ok: r.ok, error: r.error, startedAt: r.startedAt, endedAt: r.endedAt, address: r.address, signal: r.link?.signal, tests: r.tests.map((t) => ({ kind: t.kind, target: t.target, ok: t.ok, summary: t.summary })), restore: r.restore };
}

// ---------- Profiles ----------

function maskProfile(p: Profile) {
  const { psk, ...rest } = p;
  return { ...rest, hasPsk: !!psk };
}

function saveProfile(p: Partial<Profile> & { name: string }): Profile {
  const list = profiles();
  const existing = list.find((x) => (p.id && x.id === p.id) || x.name === p.name);
  const merged: Profile = { ...(existing || {}), ...p, id: existing?.id || p.id || crypto.randomUUID().slice(0, 8) } as Profile;
  // Keep the stored key when an edit doesn't send one (the UI never has it).
  if (!p.psk && existing?.psk && p.psk !== null) merged.psk = existing.psk;
  if ((p as any).psk === null) delete merged.psk;
  ctx.settings.set("profiles", [...list.filter((x) => x.id !== merged.id), merged]);
  ctx.changed();
  return merged;
}

// ---------- Recovery ----------

async function recover() {
  const lines = await recoverNamespaces(B);
  for (const l of lines) ctx.event(l);
  const borrowed = ctx.settings.get<Record<string, RadioCtx>>("borrowed", {});
  for (const rc of Object.values(borrowed)) {
    for (const iface of rc.unmanage) await sudo([B.nmcli, "dev", "set", iface, "managed", "no"]);
    for (const { iface, connection } of rc.nm) {
      const s = await run([B.nmcli, "-t", "-f", "GENERAL.STATE,GENERAL.CONNECTION", "dev", "show", iface]);
      if (/GENERAL.STATE:100/.test(s.out)) continue;
      // Give NM its usual few seconds to notice the returned interface first.
      await Bun.sleep(8000);
      const s2 = await run([B.nmcli, "-t", "-f", "GENERAL.STATE", "dev", "show", iface]);
      if (/GENERAL.STATE:100/.test(s2.out)) continue;
      const up = await sudo([B.nmcli, "con", "up", "id", connection, "ifname", iface], 60000);
      ctx.event(`Recovery: nmcli con up ${connection} on ${iface}: ${up.code ? up.err : "ok"}`);
    }
  }
  if (Object.keys(borrowed).length) ctx.settings.set("borrowed", {});
}

// ---------- Status ----------

function status() {
  const list = [...radios.values()];
  const usb = list.filter((r) => !r.onboard);
  const running = [...tests.values()];
  const auto = autoScan();
  const active = running.length > 0 || scanning.size > 0 || usb.length > 0 || auto.enabled;
  const parts: string[] = [];
  if (running.length) parts.push(`testing ${running.map((t) => `${t.r.ssid} on ${t.r.phy} (${t.r.stage})`).join(", ")}`);
  else if (scanning.size) parts.push(`scanning on ${[...scanning].join(", ")}`);
  for (const r of list) {
    const n = managedIface(r) || r.phy;
    if (r.use.kind === "primary" || r.use.kind === "backup") parts.push(`${n} ${r.use.kind === "primary" ? "primary" : "backup"} uplink`);
  }
  if (usb.length) parts.push(`${usb.length} USB radio${usb.length > 1 ? "s" : ""}`);
  if (bss.size) parts.push(`${bss.size} BSS seen`);
  const last = results[0];
  return {
    active,
    summary: parts.join(" · ") || `${list.length} radio${list.length === 1 ? "" : "s"}`,
    radios: list.map((r) => radioJson(r)),
    radiosSeq,
    reg: { country: regJson().country, changedFrom: regChangedFrom },
    scan: { seq: scan.seq, at: scan.at, radio: scan.radio, count: scan.count, error: scan.error, method: scan.method, durationMs: scan.durationMs, scanning: [...scanning], bssTotal: bss.size, auto },
    running: running.map((t) => ({ id: t.r.id, phy: t.r.phy, ssid: t.r.ssid, stage: t.r.stage, startedAt: t.r.startedAt, timeline: t.r.timeline, link: t.r.link, samples: t.r.samples.slice(-60) })),
    resultsSeq,
    lastResult: last ? { id: last.id, ok: last.ok, ssid: last.ssid, endedAt: last.endedAt, error: last.error } : null,
    profilesCount: profiles().length,
  };
}

// ---------- Driver ----------

const testSchema = {
  type: "object",
  properties: {
    radio: { type: "string", description: "phy (e.g. phy0) or interface (wlan0); default: the first radio" },
    ssid: { type: "string", description: "Network name" },
    security: { type: "string", enum: ["open", "owe", "psk", "sae", "psk-sae", "eap"], description: "Default: what the last scan saw for this SSID" },
    psk: { type: "string", description: "Passphrase (8–63 chars) or 64-hex PSK. Never echoed back." },
    useSavedKey: { type: "boolean", description: "Use the key NetworkManager has saved for this SSID (stays on DeskyPi)" },
    profile: { type: "string", description: "Saved test profile id or name to start from" },
    bssid: { type: "string", description: "Pin to one AP" },
    eap: { type: "object", properties: { method: { type: "string", enum: ["PEAP", "TTLS"] }, identity: { type: "string" }, anonymousIdentity: { type: "string" } }, description: "Enterprise (MSCHAPv2 inner; server certificate NOT validated). Password goes in psk." },
    ip: { type: "string", enum: ["dhcp", "static", "none"], description: "Default dhcp; none = association only" },
    static: { type: "object", properties: { address: { type: "string" }, gateway: { type: "string" }, dns: { type: "array", items: { type: "string" } } } },
    pingGateway: { type: "boolean", description: "Default true" },
    dns: { type: "array", items: { type: "string" }, description: "Names to resolve (default [example.com])" },
    http: { type: "array", items: { type: "string" }, description: "URLs to fetch (default [https://example.com/])" },
    ping: { type: "array", items: { type: "string" }, description: "Extra hosts to ping" },
    iperf: { type: "object", properties: { server: { type: "string" }, port: { type: "number" }, seconds: { type: "number" }, reverse: { type: "boolean" } } },
    count: { type: "number", description: "Pings per target (default 5)" },
    holdS: { type: "number", description: "Stay connected this many seconds afterwards, sampling RSSI/bitrate" },
    assocTimeoutS: { type: "number", description: "Give up associating after this long (default 30)" },
    saveAs: { type: "string", description: "Also save these settings as a named profile" },
  },
  required: ["ssid"],
};

const driver: Driver = {
  id: "wifi",
  title: "Wi-Fi",
  order: 40,
  description: "Radios and regulatory channels, a band scanner with channel charts, and client tests (associate, DHCP, ping/DNS/HTTP/iperf3) in an isolated network namespace. Works with USB dongles.",

  async init(c) {
    ctx = c;
    stopping = false;
    results = ctx.settings.get<TestResult[]>("results", []);
    loadOui();
    await recover().catch((e) => ctx.log("recovery:", e.message));
    await refreshRadios().catch((e) => ctx.log("radios:", e.message));
    // iw/nmcli/ip, no sudo, a few ms each: cheap enough to keep "current use" fresh.
    timers.push(setInterval(() => refreshRadios().catch(() => {}), 15000));
    timers.push(
      setInterval(() => {
        const a = autoScan();
        if (!a.enabled || stopping || Date.now() - lastAutoScan < Math.max(15, a.intervalS) * 1000) return;
        lastAutoScan = Date.now();
        doScan(a.radio || undefined, a.method).catch((e) => ctx.log("auto-scan:", e.message));
      }, 5000),
    );
  },

  async stop() {
    stopping = true;
    for (const t of timers) clearInterval(t);
    timers = [];
    if (refreshTimer) clearTimeout(refreshTimer);
    const running = [...tests.values()];
    for (const t of running) {
      t.fast = true;
      t.cancel();
    }
    // Let cleanup give the phys back before core kills our children.
    await Promise.race([Promise.all(running.map((t) => new Promise((res) => { const iv = setInterval(() => { if (t.r.stage === "done") { clearInterval(iv); res(null); } }, 200); }))), Bun.sleep(20000)]);
  },

  udev(ev: UdevEvent) {
    const p = ev.props;
    if (p.SUBSYSTEM === "ieee80211" || (p.SUBSYSTEM === "net" && (p.DEVTYPE === "wlan" || /^wl/.test(p.INTERFACE || "")))) {
      if (ev.action === "add" || ev.action === "remove" || ev.action === "move") scheduleRefresh();
    }
  },

  status,

  routes: [
    { method: "GET", path: "/api/wifi", doc: "Everything at once: status, radios (full), regulatory domain, profiles (keys masked)", handler: () => ({ ...status(), ...radiosJson(), profiles: profiles().map(maskProfile) }) },
    {
      method: "GET",
      path: "/api/wifi/radios",
      doc: "Radios: driver, onboard/USB, bands and channels (disabled / DFS / max power), iftypes, current use, NM state. ?refresh=1 re-reads iw phy.",
      async handler(_req, _p, url) {
        if (url.searchParams.get("refresh")) await refreshRadios({ phyInfo: true });
        return radiosJson();
      },
    },
    {
      method: "PUT",
      path: "/api/wifi/radios/:phy",
      doc: { body: { label: "Alfa AWUS036ACM", keepUnmanaged: true, scanMethod: "auto|iw|nm" }, description: "Per-radio preferences, stored by stable id (USB serial/port or onboard path)" },
      async handler(req, p) {
        const r = findRadio(p.phy);
        const b = await body(req);
        const cur = { ...r.prefs };
        if (b.label !== undefined) cur.label = String(b.label).slice(0, 60);
        if (b.keepUnmanaged !== undefined) cur.keepUnmanaged = !!b.keepUnmanaged;
        if (b.scanMethod !== undefined) {
          if (!["auto", "iw", "nm"].includes(b.scanMethod)) throw new HttpError(400, "scanMethod must be auto, iw or nm");
          cur.scanMethod = b.scanMethod;
        }
        ctx.settings.set("radioPrefs", { ...prefsAll(), [r.stableId]: cur });
        r.prefs = cur;
        if (cur.keepUnmanaged) await unmanage(r);
        radiosSeq++;
        ctx.changed();
        return { ok: true, prefs: cur };
      },
    },
    {
      method: "POST",
      path: "/api/wifi/radios/:phy/managed",
      doc: { body: { managed: false }, description: "Runtime NetworkManager control for a radio's interfaces (nmcli dev set … managed yes|no). Refuses to unmanage the connection NM is using." },
      async handler(req, p) {
        const r = findRadio(p.phy);
        const { managed } = await body(req);
        if (r.busy === "test") throw new HttpError(409, `${r.phy} is in a client test`);
        if (managed) {
          for (const n of r.nm) {
            const x = await sudo([B.nmcli, "dev", "set", n.iface, "managed", "yes"]);
            if (x.code) throw new HttpError(500, x.err);
          }
          r.unmanagedByUs = [];
        } else {
          if (r.nm.some((n) => n.state === "connected")) throw new HttpError(409, "NetworkManager is using this radio; disconnect it first");
          await unmanage(r);
        }
        await refreshRadios();
        return { ok: true, radio: radioJson(r) };
      },
    },
    { method: "GET", path: "/api/wifi/reg", doc: "Regulatory domain: global country + rules, and any per-phy (self-managed) domains", handler: () => regJson() },
    {
      method: "POST",
      path: "/api/wifi/reg",
      doc: { body: { country: "US" }, description: "Temporarily set the regulatory country (iw reg set; reverts on reboot). Self-managed radios may ignore it." },
      async handler(req) {
        return setReg((await body(req)).country);
      },
    },
    {
      method: "POST",
      path: "/api/wifi/scan",
      doc: { body: { radio: "phy0", method: "auto|iw|nm" }, description: "Scan now and return every BSS seen (with history). auto = `iw scan`, retried on EBUSY, falling back to an NM rescan + `iw scan dump`. Never disturbs an existing connection." },
      async handler(req) {
        const b = await body(req);
        return doScan(b.radio, b.method);
      },
    },
    {
      method: "GET",
      path: "/api/wifi/scan",
      doc: "Accumulated scan results (?ssid=substring&band=2.4|5|6&limit=N): per BSS SSID, BSSID, channel/width/centre, signal + history, security, standards, BSS load, country, WPS; plus survey data",
      handler: (_r, _p, url) => scanJson({ ssid: url.searchParams.get("ssid") || undefined, band: url.searchParams.get("band") || undefined, limit: +(url.searchParams.get("limit") || 0) || undefined }),
    },
    {
      method: "DELETE",
      path: "/api/wifi/scan",
      doc: "Forget all scan history",
      handler() {
        bss.clear();
        scan = { ...scan, seq: scan.seq + 1, count: 0 };
        ctx.changed();
        return { ok: true };
      },
    },
    {
      method: "PUT",
      path: "/api/wifi/autoscan",
      doc: { body: { enabled: true, intervalS: 60, radio: "phy0", method: "auto" }, description: "Periodic scanning (off by default; minimum interval 15 s)" },
      async handler(req) {
        const b = await body(req);
        const a = autoScan();
        if (b.enabled !== undefined) a.enabled = !!b.enabled;
        if (b.intervalS !== undefined) a.intervalS = Math.min(3600, Math.max(15, +b.intervalS || 60));
        if (b.radio !== undefined) a.radio = b.radio ? findRadio(b.radio).phy : "";
        if (b.method !== undefined) a.method = ["auto", "iw", "nm"].includes(b.method) ? b.method : "auto";
        ctx.settings.set("autoScan", a);
        lastAutoScan = 0;
        ctx.changed();
        return { ok: true, autoScan: a };
      },
    },
    {
      method: "POST",
      path: "/api/wifi/test",
      doc: { body: testSchema.properties, description: "Start a client test in an isolated netns. Returns {id} at once; ?wait=1 returns the finished result. Refuses (409) to borrow a radio that carries the primary default route." },
      async handler(req, _p, url) {
        const t = await startTest(await body(req));
        if (url.searchParams.get("wait")) {
          while (t.r.stage !== "done") await Bun.sleep(250);
          return t.r;
        }
        return { ok: true, id: t.r.id, phy: t.r.phy, ns: t.r.ns };
      },
    },
    {
      method: "GET",
      path: "/api/wifi/test/:id",
      doc: "A running or finished test: stage, timeline, link, lease, tests, RSSI/bitrate samples",
      handler(_r, p) {
        const t = tests.get(p.id);
        if (t) return liveJson(t);
        const r = results.find((x) => x.id === p.id);
        if (!r) throw new HttpError(404, "No such test");
        return r;
      },
    },
    {
      method: "POST",
      path: "/api/wifi/test/:id/cancel",
      doc: "Cancel a running test (cleanup and NM restore still run)",
      handler(_r, p) {
        const t = tests.get(p.id);
        if (!t) throw new HttpError(404, "No running test with that id");
        t.cancel();
        return { ok: true };
      },
    },
    { method: "GET", path: "/api/wifi/results", doc: "The last 20 client-test results (full timelines)", handler: () => ({ results }) },
    {
      method: "GET",
      path: "/api/wifi/results/:id",
      doc: "One client-test result",
      handler(_r, p) {
        const r = results.find((x) => x.id === p.id);
        if (!r) throw new HttpError(404, "No such result");
        return r;
      },
    },
    {
      method: "DELETE",
      path: "/api/wifi/results",
      doc: "Clear stored results",
      handler() {
        results = [];
        ctx.settings.set("results", results);
        resultsSeq++;
        ctx.changed();
        return { ok: true };
      },
    },
    { method: "GET", path: "/api/wifi/profiles", doc: "Saved test profiles (keys masked: hasPsk)", handler: () => ({ profiles: profiles().map(maskProfile) }) },
    {
      method: "POST",
      path: "/api/wifi/profiles",
      doc: { body: { id: "(optional, to edit)", name: "Lab AP", ssid: "LabNet", security: "psk", psk: "(omit to keep the stored key; null to clear)", useSavedKey: false, dns: ["example.com"], http: ["https://example.com/"] }, description: "Create or update a test profile" },
      async handler(req) {
        const b = await body(req);
        if (!b.name || !b.ssid) throw new HttpError(400, "name and ssid are required");
        if (b.psk) {
          const e = validKey(b.security || "psk", b.psk);
          if (e) throw new HttpError(400, e);
        }
        const allowed = ["id", "name", "ssid", "security", "psk", "useSavedKey", "eap", "bssid", "ip", "static", "pingGateway", "dns", "http", "ping", "iperf", "count", "holdS"];
        const p = saveProfile(Object.fromEntries(Object.entries(b).filter(([k]) => allowed.includes(k))) as any);
        return { ok: true, profile: maskProfile(p) };
      },
    },
    {
      method: "DELETE",
      path: "/api/wifi/profiles/:id",
      doc: "Delete a test profile",
      handler(_r, p) {
        ctx.settings.set("profiles", profiles().filter((x) => x.id !== p.id));
        ctx.changed();
        return { ok: true };
      },
    },
  ],

  tools: [
    {
      name: "wifi_radios",
      description: "List DeskyPi's Wi-Fi radios: driver, onboard or USB, supported bands, enabled/disabled/DFS channels with max power, interface modes (monitor/AP), what each is doing now (e.g. backup uplink), NetworkManager state, and the regulatory domain.",
      inputSchema: { type: "object", properties: { channels: { type: "boolean", description: "Include the per-channel list (long). Default false." } } },
      async handler(a) {
        await refreshRadios();
        const j = radiosJson();
        if (!a.channels) j.radios = j.radios.map((r: any) => ({ ...r, bandInfo: r.bandInfo.map((b: any) => ({ ...b, enabled: b.channels.filter((c: any) => !c.disabled).map((c: any) => c.ch), dfs: b.channels.filter((c: any) => c.radar && !c.disabled).map((c: any) => c.ch), channels: undefined })) }));
        return j;
      },
    },
    {
      name: "wifi_scan",
      description: "Scan for Wi-Fi networks (BSSes) and return them strongest first: SSID (hidden flagged), BSSID, band, channel, width and centre channel, signal dBm (+ recent history), security (Open/OWE/WEP/WPA2-PSK/WPA3-SAE/transition/Enterprise), 802.11 standards, station count and channel utilisation, country, WPS. Doesn't disturb existing connections.",
      inputSchema: {
        type: "object",
        properties: {
          radio: { type: "string", description: "phy or interface; default the first radio" },
          fresh: { type: "boolean", description: "Run a new scan first (default true); false returns accumulated results" },
          ssid: { type: "string", description: "Only BSSes whose SSID contains this" },
          band: { type: "string", enum: ["2.4", "5", "6"] },
          limit: { type: "number", description: "Max BSSes (default 50)" },
        },
      },
      async handler(a) {
        if (a.fresh !== false) await doScan(a.radio);
        const j = scanJson({ ssid: a.ssid, band: a.band, limit: a.limit || 50 });
        return { scan: j.scan, bss: j.bss.map(({ history, ...b }) => ({ ...b, history: history.slice(-10).map((h) => h[1]) })) };
      },
    },
    {
      name: "wifi_test",
      description: "Run a Wi-Fi client test and wait for it: borrow a radio into an isolated network namespace (never affects eth0 or the main routing table), associate to the SSID, optionally DHCP, then ping the gateway, resolve DNS names, fetch URLs, ping hosts and optionally run iperf3. Returns the timeline (association, 4-way handshake, DHCP discover/offer/ack, each test), the lease, RSSI/bitrates, and whether NetworkManager reconnected the radio afterwards. Refuses to borrow a radio that is the primary uplink. Takes ~10–60 s.",
      inputSchema: testSchema,
      async handler(a) {
        const t = await startTest(a);
        while (t.r.stage !== "done") await Bun.sleep(250);
        const { samples, wpaLog, ...rest } = t.r;
        return { ...rest, samples: samples.length, wpaLog: wpaLog.slice(-15) };
      },
    },
    {
      name: "wifi_results",
      description: "Recent Wi-Fi client-test results (newest first). Without id: a summary of each of the last 20. With id: that result in full (timeline, lease, link, tests).",
      inputSchema: { type: "object", properties: { id: { type: "string" }, limit: { type: "number" } } },
      handler(a) {
        if (a.id) {
          const r = results.find((x) => x.id === a.id) || tests.get(a.id)?.r;
          if (!r) throw new Error("No such result");
          return r;
        }
        return { running: [...tests.values()].map((t) => ({ id: t.r.id, ssid: t.r.ssid, stage: t.r.stage })), results: results.slice(0, a.limit || 20).map(resultSummary) };
      },
    },
  ],
};

export default driver;
