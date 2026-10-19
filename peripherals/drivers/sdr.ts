// ==================== RTL-SDR ====================
//
// RTL2832U DVB-T sticks used as software-defined radios. A stick can only be opened by one
// program at a time, so the driver is a small state machine over one child process:
//
//   off ──▶ rtl_tcp  (raw I/Q over TCP for SDR++ / GQRX / SDR# on another machine)
//    ▲  ──▶ rtl_433  (decodes 433/868/915/345 MHz sensors: weather stations, TPMS, …)
//    └──────────┘
//
// Switching modes stops the running child and waits for it to exit before the next opens
// the stick. A probe (`rtl_test -t`) also needs the stick, so it only runs while off.
//
// The kernel's DVB driver (dvb_usb_rtl28xxu) grabs these sticks on plug-in unless it is
// blacklisted. Debian's rtl-sdr package doesn't ship that blacklist, so the panel checks
// modprobe.d and shows the fix rather than silently failing to open the device.

import type { Driver, Ctx, Props, Tool, Route } from "../core.ts";
import { HttpError, run, body, udevDecode, readText } from "../core.ts";
import type { Subprocess } from "bun";
import { readdirSync, existsSync } from "node:fs";
import { hostname } from "node:os";
import { DVB_MODULE, RTL_STICKS, Rtl433Aggregator, findBlacklist, parseRtlEeprom, parseRtlTest, tcpPeers, type RtlTestInfo } from "../lib/sdr-parse.ts";

const RTL_TCP = "/usr/bin/rtl_tcp";
const RTL_433 = "/usr/bin/rtl_433";
const RTL_TEST = "/usr/bin/rtl_test";
const RTL_EEPROM = "/usr/bin/rtl_eeprom";

// Side instances (tests, other agents) shift every default port so they never collide.
const PORT_OFFSET = parseInt(process.env.BENCHBOB_PORT_OFFSET || "") || 0;
const DEFAULT_TCP_PORT = 1234 + PORT_OFFSET;

export const PRESETS: Record<string, { label: string; freqs: string[] }> = {
  "433": { label: "433.92 MHz (EU/US ISM)", freqs: ["433.92M"] },
  "868": { label: "868.3 MHz (EU SRD)", freqs: ["868.3M"] },
  "915": { label: "915 MHz (US ISM)", freqs: ["915M"] },
  "345": { label: "345 MHz (US security sensors)", freqs: ["345M"] },
  "433+868": { label: "Hop 433.92 / 868.3 MHz", freqs: ["433.92M", "868.3M"] },
  "433+915": { label: "Hop 433.92 / 915 MHz", freqs: ["433.92M", "915M"] },
};

let ctx: Ctx;

// ---------- Sticks ----------

interface Stick {
  key: string; // stable: serial if set, else the USB port path
  devpath: string;
  usbId: string; // 0bda:2838
  knownAs: string; // librtlsdr's name for that id
  manufacturer: string; // EEPROM strings, as the USB descriptors report them
  product: string;
  serial: string;
  port: string; // ID_PATH
  bus: string; // "001/004"
  drivers: string[]; // kernel drivers bound to its interfaces
  dvbClaimed: boolean;
  probe: (RtlTestInfo & { t: number; eeprom?: Record<string, string> }) | null;
}

let sticks: Stick[] = [];
const probes = new Map<string, Stick["probe"]>();
let dvb = { loaded: false, blacklisted: false, where: [] as string[] };

function scanSticks() {
  const usbs = ctx.devices((p) => p.SUBSYSTEM === "usb" && p.DEVTYPE === "usb_device" && !!RTL_STICKS[`${p.ID_VENDOR_ID}:${p.ID_MODEL_ID}`]);
  sticks = usbs.map((p) => {
    const ifaces = ctx.devices((i) => i.SUBSYSTEM === "usb" && i.DEVTYPE === "usb_interface" && i.DEVPATH.startsWith(p.DEVPATH + "/"));
    const drivers = [...new Set(ifaces.map((i) => i.DRIVER).filter(Boolean))];
    const serial = p.ID_SERIAL_SHORT || "";
    const key = serial && serial !== "00000001" ? `sn:${serial}` : `path:${p.ID_PATH || p.DEVPATH}`;
    return {
      key,
      devpath: p.DEVPATH,
      usbId: `${p.ID_VENDOR_ID}:${p.ID_MODEL_ID}`,
      knownAs: RTL_STICKS[`${p.ID_VENDOR_ID}:${p.ID_MODEL_ID}`],
      manufacturer: udevDecode(p.ID_VENDOR_ENC) || p.ID_VENDOR || "",
      product: udevDecode(p.ID_MODEL_ENC) || p.ID_MODEL || "",
      serial,
      port: p.ID_PATH || "",
      bus: p.BUSNUM && p.DEVNUM ? `${p.BUSNUM}/${p.DEVNUM}` : "",
      drivers,
      dvbClaimed: drivers.some((d) => d.startsWith("dvb_usb") || d === DVB_MODULE),
      probe: probes.get(key) || null,
    };
  });
}

async function checkDvb() {
  const files: { path: string; text: string }[] = [];
  for (const dir of ["/etc/modprobe.d", "/run/modprobe.d", "/usr/lib/modprobe.d", "/lib/modprobe.d", "/usr/local/lib/modprobe.d"]) {
    try {
      for (const f of readdirSync(dir)) if (f.endsWith(".conf")) files.push({ path: `${dir}/${f}`, text: await readText(`${dir}/${f}`) });
    } catch {}
  }
  const bl = findBlacklist(files);
  dvb = { loaded: existsSync(`/sys/module/${DVB_MODULE}`), ...bl, where: [...new Set(bl.where)] };
}

const FIX_HINT = `echo 'blacklist ${DVB_MODULE}' | sudo tee /etc/modprobe.d/blacklist-rtl-sdr.conf && sudo modprobe -r ${DVB_MODULE}`;

/** The -d argument: a unique serial when there is one (stable across replugs), else the index. */
function deviceArg(s: Stick | null, forRtl433 = false): string[] {
  if (!s) return [];
  const dupSerial = sticks.filter((x) => x.serial === s.serial).length > 1;
  if (s.serial && !dupSerial) return ["-d", forRtl433 ? `:${s.serial}` : s.serial];
  return ["-d", String(Math.max(0, sticks.indexOf(s)))];
}

function pickStick(key?: string, force = false): Stick | null {
  if (key) {
    const s = sticks.find((s) => s.key === key || s.serial === key);
    if (!s) throw new HttpError(404, `No RTL-SDR stick "${key}"`);
    return s;
  }
  if (sticks.length) return sticks[0];
  if (force) return null; // let the tool search on its own (e.g. a clone with an unlisted USB id)
  throw new HttpError(409, "No RTL-SDR stick detected. Plug one in, or pass force:true to try anyway.");
}

// ---------- Mode state machine ----------

interface Rtl433Opts { preset: string; freqs: string[]; hop: number; gain: string; ppm: number; extra?: string[] }
interface TcpOpts { port: number; bind: string; gain: number; ppm: number; sampleRate: number; freq: number; biasT: boolean }

type Mode = "off" | "rtl_tcp" | "rtl_433";
interface Running {
  mode: Exclude<Mode, "off">;
  stick: Stick | null;
  proc: Subprocess<"ignore", "pipe", "pipe">;
  since: number;
  opts: Rtl433Opts | TcpOpts;
  cmd: string[];
  stopping?: boolean;
}

let cur: Running | null = null;
let lastExit: { mode: string; code: number | null; at: number; message: string } | null = null;
let busy: Promise<any> = Promise.resolve();
let probing = false;
let tcpClients: string[] = [];
const rtl433 = new Rtl433Aggregator();
const tcpLog: { t: number; line: string }[] = [];

function serial<T>(fn: () => Promise<T>): Promise<T> {
  const p = busy.then(fn, fn);
  busy = p.catch(() => {});
  return p;
}

const stickSettings = (s: Stick | null) => ctx.settings.get<Record<string, any>>(`stick:${s?.key || "default"}`, {});
function saveStickSettings(s: Stick | null, patch: Record<string, any>) {
  ctx.settings.set(`stick:${s?.key || "default"}`, { ...stickSettings(s), ...patch });
}

function tcpOpts(s: Stick | null, o: any): TcpOpts {
  const d = { port: DEFAULT_TCP_PORT, bind: "0.0.0.0", gain: 0, ppm: 0, sampleRate: 2048000, freq: 100_000_000, biasT: false, ...stickSettings(s).tcp, ...o };
  const port = Math.trunc(Number(d.port));
  if (!(port > 0 && port < 65536)) throw new HttpError(400, "port must be 1–65535");
  const sr = Math.trunc(Number(d.sampleRate));
  // librtlsdr accepts 225001–300000 and 900001–3200000 S/s.
  if (!((sr > 225000 && sr <= 300000) || (sr > 900000 && sr <= 3200000))) throw new HttpError(400, "sampleRate must be 225001–300000 or 900001–3200000");
  if (!/^[0-9a-fA-F:.]+$/.test(String(d.bind))) throw new HttpError(400, "bind must be an IP address");
  return { port, bind: String(d.bind), gain: Number(d.gain) || 0, ppm: Math.trunc(Number(d.ppm)) || 0, sampleRate: sr, freq: Math.trunc(Number(d.freq)) || 100_000_000, biasT: !!d.biasT };
}

function r433Opts(s: Stick | null, o: any): Rtl433Opts {
  const d = { preset: "433", hop: 0, gain: "", ppm: 0, ...stickSettings(s).rtl433, ...o };
  let freqs: string[] = Array.isArray(d.freqs) && d.freqs.length ? d.freqs.map(String) : PRESETS[d.preset]?.freqs;
  if (!freqs) throw new HttpError(400, `Unknown preset "${d.preset}"; use one of ${Object.keys(PRESETS).join(", ")} or pass freqs`);
  for (const f of freqs) if (!/^\d+(\.\d+)?[kMG]?$/.test(f)) throw new HttpError(400, `Bad frequency "${f}" (e.g. 433.92M)`);
  const hop = Math.max(0, Math.trunc(Number(d.hop)) || (freqs.length > 1 ? 60 : 0));
  const gain = String(d.gain ?? "");
  if (gain && !/^\d+(\.\d+)?$/.test(gain)) throw new HttpError(400, "gain is dB (e.g. 40) or empty for auto");
  return { preset: Array.isArray(d.freqs) && d.freqs.length ? "custom" : d.preset, freqs, hop, gain, ppm: Math.trunc(Number(d.ppm)) || 0 };
}

async function stopMode(): Promise<void> {
  const r = cur;
  if (!r) return;
  r.stopping = true;
  r.proc.kill("SIGINT"); // both tools close the device cleanly on SIGINT
  const done = await Promise.race([r.proc.exited.then(() => true), Bun.sleep(3000).then(() => false)]);
  if (!done) {
    r.proc.kill("SIGKILL");
    await r.proc.exited;
  }
  cur = null;
  tcpClients = [];
  ctx.event(`SDR ${r.mode} stopped`);
  ctx.changed();
}

function setMode(mode: Mode, o: any = {}): Promise<any> {
  return serial(async () => {
    if (!["off", "rtl_tcp", "rtl_433"].includes(mode)) throw new HttpError(400, "mode must be off, rtl_tcp or rtl_433");
    if (probing) throw new HttpError(409, "A probe is using the stick; try again in a few seconds");
    if (mode === "off") {
      await stopMode();
      return statusMode();
    }
    const s = pickStick(o.stick, !!o.force);
    if (s?.dvbClaimed) throw new HttpError(409, `The kernel DVB driver has this stick. Fix: ${FIX_HINT}`);
    const opts = mode === "rtl_tcp" ? tcpOpts(s, o) : r433Opts(s, o);
    await stopMode();

    let cmd: string[];
    if (mode === "rtl_tcp") {
      const t = opts as TcpOpts;
      cmd = [RTL_TCP, "-a", t.bind, "-p", String(t.port), "-s", String(t.sampleRate), "-f", String(t.freq), "-g", String(t.gain), "-P", String(t.ppm), ...(t.biasT ? ["-T"] : []), ...deviceArg(s)];
      saveStickSettings(s, { tcp: t });
    } else {
      const r = opts as Rtl433Opts;
      // -F log puts rtl_433's own messages on stdout as plain text; with only -F json they vanish.
      cmd = [RTL_433, ...deviceArg(s, true), "-F", "json", "-F", "log", "-M", "time:unix:usec", "-M", "level", "-M", "protocol", ...r.freqs.flatMap((f) => ["-f", f])];
      if (r.freqs.length > 1) cmd.push("-H", String(r.hop));
      if (r.gain) cmd.push("-g", r.gain);
      if (r.ppm) cmd.push("-p", String(r.ppm));
      saveStickSettings(s, { rtl433: { preset: r.preset, freqs: r.preset === "custom" ? r.freqs : undefined, hop: r.hop, gain: r.gain, ppm: r.ppm } });
    }
    const proc = ctx.spawn(cmd, { stdin: "ignore", stdout: "pipe", stderr: "pipe" }) as Subprocess<"ignore", "pipe", "pipe">;
    const r: Running = { mode, stick: s, proc, since: Date.now(), opts, cmd };
    cur = r;
    lastExit = null;
    pump(r, r.proc.stdout, mode === "rtl_433" ? (l) => rtl433.line(l) : (l) => tcpLine(l));
    pump(r, r.proc.stderr, mode === "rtl_433" ? (l) => rtl433.line(l) : (l) => tcpLine(l));
    proc.exited.then((code) => {
      if (cur !== r) return; // replaced or stopped on purpose
      const log = mode === "rtl_433" ? rtl433.log : tcpLog;
      const message = log.slice(-3).map((l) => l.line).join(" · ");
      lastExit = { mode, code, at: Date.now(), message };
      cur = null;
      tcpClients = [];
      ctx.event(`SDR ${mode} exited (${code})${message ? `: ${message}` : ""}`);
      ctx.changed();
    });
    ctx.event(`SDR ${mode} started${s ? ` on ${s.product || s.knownAs}` : ""}`);
    ctx.changed();
    // Most failures (no device, busy) show up within a second; report them to the caller.
    await Bun.sleep(1200);
    if (cur !== r && lastExit) throw new HttpError(503, `${mode} exited: ${lastExit.message || `code ${lastExit.code}`}`);
    return statusMode();
  });
}

async function pump(r: Running, stream: ReadableStream<Uint8Array>, onLine: (l: string) => void) {
  const dec = new TextDecoder();
  let buf = "";
  for await (const chunk of stream) {
    buf += dec.decode(chunk, { stream: true });
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      onLine(buf.slice(0, i));
      buf = buf.slice(i + 1);
    }
  }
  if (buf.trim()) onLine(buf);
}

function tcpLine(l: string) {
  if (!l.trim()) return;
  tcpLog.push({ t: Date.now(), line: l.trim() });
  if (tcpLog.length > 100) tcpLog.shift();
  if (/client accepted|all threads dead/i.test(l)) ctx.changed();
}

async function pollTcpClients() {
  if (cur?.mode !== "rtl_tcp") return;
  const port = (cur.opts as TcpOpts).port;
  const next = [...tcpPeers(await readText("/proc/net/tcp"), port), ...tcpPeers(await readText("/proc/net/tcp6"), port)];
  if (next.join() !== tcpClients.join()) {
    if (next.length && !tcpClients.length) ctx.event(`rtl_tcp client connected: ${next.join(", ")}`);
    tcpClients = next;
    ctx.changed();
  }
}

// ---------- Probe ----------

async function probe(key?: string) {
  return serial(async () => {
    if (cur) throw new HttpError(409, `The stick is in use by ${cur.mode}; turn it off to probe`);
    const s = pickStick(key);
    if (!s) return null;
    probing = true;
    try {
      // rtl_test -t only tunes and lists gains; it exits by itself (with E4000-specific noise).
      const t = await run([RTL_TEST, "-t", ...deviceArg(s)], { timeoutMs: 10000 });
      const info = parseRtlTest(`${t.out}\n${t.err}`);
      const e = await run([RTL_EEPROM, ...deviceArg(s)], { timeoutMs: 10000 });
      const eeprom = parseRtlEeprom(`${e.out}\n${e.err}`);
      const p = { ...info, t: Date.now(), eeprom: Object.keys(eeprom).length ? eeprom : undefined };
      probes.set(s.key, p);
      s.probe = p;
      ctx.changed();
      return p;
    } finally {
      probing = false;
    }
  });
}

// ---------- Views ----------

function statusMode() {
  if (!cur) return { mode: "off" as Mode, lastExit };
  const host = hostname();
  const base = { mode: cur.mode, since: cur.since, stick: cur.stick?.key || null, opts: cur.opts, cmd: cur.cmd.join(" ") };
  if (cur.mode === "rtl_tcp") {
    const t = cur.opts as TcpOpts;
    return { ...base, clients: tcpClients, connect: { sdrpp: `Source \"RTL-TCP\", host ${host}, port ${t.port}`, gqrx: `rtl_tcp=${host}:${t.port}`, address: `${host}:${t.port}` } };
  }
  return { ...base, decoded: rtl433.devices.size, messages: rtl433.seq };
}

function devicesView(limit = 100) {
  return rtl433.list().slice(0, limit).map((d) => ({ ...d, lastSeenAgo_s: Math.round((Date.now() - d.lastSeen) / 1000) }));
}

function status() {
  const recent = rtl433.list().filter((d) => Date.now() - d.lastSeen < 5 * 60_000).length;
  const m = statusMode();
  const anyClaimed = sticks.some((s) => s.dvbClaimed);
  let summary = !sticks.length ? "No RTL-SDR stick" : `${sticks.length} stick${sticks.length > 1 ? "s" : ""}`;
  if (m.mode === "rtl_tcp") summary = `rtl_tcp on :${(cur!.opts as TcpOpts).port} · ${tcpClients.length ? `${tcpClients.length} client` : "no client"}`;
  else if (m.mode === "rtl_433") summary = `rtl_433 · ${rtl433.devices.size} sensor${rtl433.devices.size === 1 ? "" : "s"}`;
  else if (anyClaimed) summary += " · held by DVB driver";
  return {
    active: sticks.length > 0 || m.mode !== "off" || recent > 0,
    summary,
    sticks,
    dvb: { ...dvb, fix: dvb.blacklisted ? "" : FIX_HINT },
    ...m,
    probing,
    presets: PRESETS,
    defaults: { tcpPort: DEFAULT_TCP_PORT, settings: stickSettings(sticks[0] || null) },
    rtl433: { sensors: rtl433.devices.size, recent, messages: rtl433.seq },
  };
}

// ---------- Routes / tools ----------

const routes: Route[] = [
  { method: "GET", path: "/api/sdr", doc: "RTL-SDR sticks (tuner, EEPROM strings, DVB-driver state), current mode and connection info", handler: () => status() },
  {
    method: "POST",
    path: "/api/sdr/mode",
    doc: {
      body: { mode: "rtl_tcp | rtl_433 | off", stick: "key from GET /api/sdr (optional)", force: false, "rtl_tcp options": { port: DEFAULT_TCP_PORT, bind: "0.0.0.0", gain: "0 = auto, else dB", ppm: 0, sampleRate: 2048000, freq: 100000000, biasT: false }, "rtl_433 options": { preset: Object.keys(PRESETS).join(" | "), freqs: ["433.92M"], hop: 60, gain: "", ppm: 0 } },
      description: "Switch the stick between rtl_tcp server, rtl_433 decoder and off. Options are remembered per stick.",
    },
    handler: async (req) => {
      const b = await body(req);
      return setMode(b.mode, b);
    },
  },
  { method: "POST", path: "/api/sdr/probe", doc: { body: { stick: "optional key" }, description: "Run rtl_test -t and rtl_eeprom (only while the mode is off): tuner type, gain table, EEPROM strings" }, handler: async (req) => probe((await body(req)).stick) },
  {
    method: "GET",
    path: "/api/sdr/rtl433",
    doc: { description: "rtl_433 decoded sensors (latest fields + histories) and messages", query: { since: "message seq; only newer messages", limit: "messages (default 100)" } },
    handler: (_r, _p, url) => {
      const since = Number(url.searchParams.get("since")) || 0;
      const limit = Math.min(Number(url.searchParams.get("limit")) || 100, 300);
      return { seq: rtl433.seq, devices: devicesView(200), messages: rtl433.messages.filter((m) => m.seq > since).slice(-limit), log: rtl433.log.slice(-50) };
    },
  },
  { method: "DELETE", path: "/api/sdr/rtl433", doc: "Clear rtl_433 sensors, messages and log", handler: () => (rtl433.clear(), ctx.changed(), { ok: true }) },
];

const tools: Tool[] = [
  {
    name: "sdr_status",
    description: "RTL-SDR state: detected sticks (USB id, EEPROM manufacturer/product/serial, tuner from rtl_test), whether the kernel DVB driver is blacklisted or holding a stick, current mode (off / rtl_tcp / rtl_433) and how to connect to rtl_tcp.",
    inputSchema: { type: "object", properties: {} },
    handler: () => status(),
  },
  {
    name: "sdr_mode",
    description:
      "Switch the RTL-SDR stick. 'rtl_tcp' serves raw I/Q on a TCP port (default 1234) for SDR++/GQRX/SDR#. 'rtl_433' decodes ISM-band sensors (weather stations, TPMS, door sensors); read results with sdr_messages. 'off' releases the stick. Only one mode at a time.",
    inputSchema: {
      type: "object",
      required: ["mode"],
      properties: {
        mode: { type: "string", enum: ["off", "rtl_tcp", "rtl_433"] },
        stick: { type: "string", description: "Stick key from sdr_status (default: first stick)" },
        preset: { type: "string", enum: Object.keys(PRESETS), description: "rtl_433 frequency preset (default 433)" },
        freqs: { type: "array", items: { type: "string" }, description: "rtl_433 custom frequencies, e.g. [\"433.92M\",\"868.3M\"] (overrides preset)" },
        hop: { type: "number", description: "rtl_433 seconds per frequency when hopping (default 60)" },
        gain: { description: "Gain in dB; rtl_tcp 0 / rtl_433 empty = auto" },
        ppm: { type: "number", description: "Frequency correction in ppm" },
        port: { type: "number", description: "rtl_tcp port (default 1234)" },
        bind: { type: "string", description: "rtl_tcp listen address (default 0.0.0.0)" },
        sampleRate: { type: "number", description: "rtl_tcp sample rate in S/s (default 2048000)" },
        freq: { type: "number", description: "rtl_tcp initial centre frequency in Hz (clients usually retune)" },
      },
    },
    handler: (a) => setMode(a.mode, a),
  },
  {
    name: "sdr_messages",
    description: "rtl_433 results: one entry per sensor (model, id, channel, latest fields such as temperature_C/humidity/battery_ok, rssi/snr, count, last seen) plus the most recent raw decoded messages.",
    inputSchema: { type: "object", properties: { limit: { type: "number", description: "Recent messages to include (default 20)" }, sensors: { type: "number", description: "Max sensors (default 50)" } } },
    handler: (a) => ({
      mode: statusMode().mode,
      sensors: devicesView(Math.min(a.sensors || 50, 200)).map(({ hist, ...d }) => d),
      messages: rtl433.messages.slice(-Math.min(a.limit || 20, 300)).map((m) => m.msg),
    }),
  },
];

let tick: ReturnType<typeof setInterval> | null = null;

export default {
  id: "sdr",
  title: "RTL-SDR",
  order: 62,
  description: "RTL2832U sticks: rtl_tcp server for SDR++/GQRX, or rtl_433 sensor decoding",
  async init(c) {
    ctx = c;
    await checkDvb();
    scanSticks();
    tick = setInterval(pollTcpClients, 2000);
  },
  async stop() {
    if (tick) clearInterval(tick);
    await stopMode();
  },
  udev(ev) {
    const p = ev.props;
    const isStick = p.SUBSYSTEM === "usb" && (p.DEVTYPE === "usb_device" || p.DEVTYPE === "usb_interface");
    if (!isStick) return;
    const before = new Set(sticks.map((s) => s.devpath));
    scanSticks();
    const added = sticks.filter((s) => !before.has(s.devpath));
    const removed = ev.action === "remove" && before.has(p.DEVPATH);
    if (!added.length && !removed && !(p.DEVTYPE === "usb_interface" && sticks.some((s) => p.DEVPATH.startsWith(s.devpath)))) return;
    checkDvb().then(() => ctx.changed());
    for (const s of added) {
      ctx.event(`RTL-SDR stick plugged in: ${s.manufacturer} ${s.product} (${s.usbId})`);
      // Let the DVB driver bind (or not) first, then probe if the stick is free.
      setTimeout(() => {
        scanSticks();
        const now = sticks.find((x) => x.devpath === s.devpath);
        if (now?.dvbClaimed) ctx.event(`RTL-SDR stick is held by the kernel DVB driver (${DVB_MODULE}); blacklist it to use the stick`);
        else if (now && !cur) probe(now.key).catch((e) => ctx.log("probe:", e.message));
      }, 1500);
    }
    if (removed) {
      ctx.event("RTL-SDR stick unplugged");
      if (cur && cur.stick?.devpath === p.DEVPATH) serial(stopMode);
    }
    ctx.changed();
  },
  status,
  routes,
  tools,
} satisfies Driver;
