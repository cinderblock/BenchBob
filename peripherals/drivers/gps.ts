// ==================== GPS ====================
//
// USB GPS receivers, run through gpsd for as long as someone is using them. This is a
// bench experiment, not a time service: gpsd and gpsd.socket stay masked in systemd, and
// the driver starts `gpsd -N` as its own child only after the user (or an agent) asks,
// and stops it on request, on unplug, and on shutdown.
//
// The driver is also a gpsd client: it connects to its child over the JSON protocol
// (?WATCH) and folds TPV/SKY/PPS/TOFF into the state the panel and tools show. Other
// programs (cgps, gpsmon, QGIS, a laptop with "Share on network") can attach to the same
// gpsd at the same time.
//
// Serial ports are shared with the serial panel, which lists the same ttys. Whoever opens
// a port first keeps it. Before starting, the driver looks for another process holding the
// tty (/proc/*/fd) and refuses rather than fight over it; an EBUSY from gpsd is reported
// the same way.

import type { Driver, Ctx, Props, Tool, Route } from "../core.ts";
import { HttpError, body, usbName } from "../core.ts";
import type { Socket as TcpSocket, Subprocess } from "bun";
import { existsSync, readdirSync, readlinkSync, readFileSync, realpathSync } from "node:fs";
import { hostname } from "node:os";
import { classifyUsb, ingest, newState, summarize, type GpsdState } from "../lib/gps-parse.ts";

const GPSD = "/usr/sbin/gpsd";
const PORT_OFFSET = parseInt(process.env.DESKYPI_PORT_OFFSET || "") || 0;
const DEFAULT_PORT = 2947 + PORT_OFFSET;

let ctx: Ctx;

// ---------- Candidate ttys ----------

interface Candidate {
  key: string; // stable id for remembering "use as GPS"
  path: string; // /dev/ttyACM0
  byId: string; // /dev/serial/by-id/… when udev made one
  usbId: string;
  vendor: string;
  model: string;
  serial: string;
  kind: "gps" | "maybe" | "unknown";
  label: string;
  chosen: boolean; // user said "use as GPS"
  holders: { pid: number; comm: string }[];
}

let candidates: Candidate[] = [];

function scanTtys() {
  const chosen = ctx.settings.get<Record<string, boolean>>("useAsGps", {});
  const ttys = ctx.devices((p) => p.SUBSYSTEM === "tty" && /^\/dev\/tty(ACM|USB)\d+$/.test(p.DEVNAME || ""));
  const prev = new Map(candidates.map((c) => [c.path, c.holders]));
  candidates = ttys
    .map((p): Candidate => {
      const usb = ctx.ancestor(p.DEVPATH, "usb", "usb_device");
      const usbId = usb ? `${usb.ID_VENDOR_ID}:${usb.ID_MODEL_ID}` : "";
      const cls = classifyUsb(usbId);
      const key = p.ID_SERIAL && p.ID_SERIAL_SHORT ? `sn:${p.ID_SERIAL}${p.ID_USB_INTERFACE_NUM ? `:${p.ID_USB_INTERFACE_NUM}` : ""}` : `path:${p.ID_PATH || p.DEVNAME}`;
      const n = usb ? usbName(usb) : { vendor: "", model: "" };
      const byId = (p.DEVLINKS || "").split(" ").find((l) => l.startsWith("/dev/serial/by-id/")) || "";
      return { key, path: p.DEVNAME, byId, usbId, vendor: n.vendor, model: n.model, serial: p.ID_SERIAL_SHORT || "", kind: cls.kind, label: cls.label, chosen: !!chosen[key], holders: prev.get(p.DEVNAME) || [] };
    })
    .sort((a, b) => a.path.localeCompare(b.path, undefined, { numeric: true }));
}

const isGps = (c: Candidate) => c.kind === "gps" || c.chosen;

/** Processes with `dev` open. Reads /proc/<pid>/fd; root-owned processes we can't inspect are missed. */
function holdersOf(dev: string): { pid: number; comm: string }[] {
  let real = dev;
  try {
    real = realpathSync(dev);
  } catch {}
  const out: { pid: number; comm: string }[] = [];
  for (const pid of readdirSync("/proc")) {
    if (!/^\d+$/.test(pid)) continue;
    let fds: string[];
    try {
      fds = readdirSync(`/proc/${pid}/fd`);
    } catch {
      continue;
    }
    for (const fd of fds) {
      let target = "";
      try {
        target = readlinkSync(`/proc/${pid}/fd/${fd}`);
      } catch {
        continue;
      }
      if (target === real) {
        let comm = "";
        try {
          comm = readFileSync(`/proc/${pid}/comm`, "utf8").trim();
        } catch {}
        out.push({ pid: +pid, comm: +pid === process.pid ? "DeskyPi (serial panel)" : comm });
        break;
      }
    }
  }
  return out;
}

function refreshHolders() {
  let changed = false;
  for (const c of candidates) {
    const h = holdersOf(c.path).filter((x) => !(run && x.pid === run.proc.pid));
    if (JSON.stringify(h) !== JSON.stringify(c.holders)) (c.holders = h), (changed = true);
  }
  if (changed) ctx.changed();
}

const ppsDevices = () => {
  try {
    return readdirSync("/dev").filter((f) => /^pps\d+$/.test(f)).map((f) => `/dev/${f}`);
  } catch {
    return [];
  }
};

// ---------- gpsd child + client ----------

interface Run {
  path: string;
  key: string | null;
  port: number;
  share: boolean;
  readOnly: boolean;
  pps: string[];
  proc: Subprocess<"ignore", "pipe", "pipe">;
  since: number;
  sock: TcpSocket<any> | null;
  connected: boolean;
  state: GpsdState;
  stopping?: boolean;
}

let run: Run | null = null;
let lastExit: { at: number; code: number | null; message: string } | null = null;
const log: { t: number; line: string }[] = [];
let busy: Promise<any> = Promise.resolve();

function serial<T>(fn: () => Promise<T>): Promise<T> {
  const p = busy.then(fn, fn);
  busy = p.catch(() => {});
  return p;
}

function addLog(line: string) {
  if (!line.trim()) return;
  log.push({ t: Date.now(), line: line.trim() });
  if (log.length > 100) log.shift();
}

function resolveDevice(a: { device?: string; path?: string }): { path: string; key: string | null } {
  if (a.device) {
    const c = candidates.find((c) => c.key === a.device || c.path === a.device || c.byId === a.device);
    if (!c) throw new HttpError(404, `No serial device "${a.device}"`);
    return { path: c.path, key: c.key };
  }
  if (a.path) {
    // Any tty by path, for receivers on odd adapters, ptys (testing) and /dev/serial/by-id links.
    if (!/^\/dev\/(tty\w+|pts\/\d+|serial\/by-(id|path)\/[\w:.,+-]+|gnss\d+)$/.test(a.path)) throw new HttpError(400, "path must be a /dev/tty*, /dev/pts/N, /dev/gnssN or /dev/serial/by-* device");
    if (!existsSync(a.path)) throw new HttpError(404, `${a.path} doesn't exist`);
    const c = candidates.find((c) => c.path === a.path || c.byId === a.path);
    return { path: c?.path || a.path, key: c?.key || null };
  }
  const gps = candidates.filter(isGps);
  if (gps.length === 1) return { path: gps[0].path, key: gps[0].key };
  if (!gps.length) throw new HttpError(409, candidates.length ? `No known GPS receiver; pick one of ${candidates.map((c) => c.path).join(", ")} with "device", or mark it "use as GPS"` : "No GPS receiver or USB serial device found");
  throw new HttpError(409, `Several GPS receivers (${gps.map((c) => c.path).join(", ")}); pick one with "device"`);
}

function start(a: { device?: string; path?: string; port?: number; share?: boolean; readOnly?: boolean; pps?: boolean }): Promise<any> {
  return serial(async () => {
    scanTtys();
    const dev = resolveDevice(a);
    if (run && !run.stopping) {
      if (run.path === dev.path) return view();
      await stopRun();
    }
    const holders = holdersOf(dev.path);
    if (holders.length) throw new HttpError(409, `${dev.path} is already open by ${holders.map((h) => `${h.comm} (pid ${h.pid})`).join(", ")}. Close it there first.`);

    const port = Math.trunc(Number(a.port ?? ctx.settings.get("port", DEFAULT_PORT)));
    if (!(port > 0 && port < 65536)) throw new HttpError(400, "port must be 1–65535");
    const share = !!(a.share ?? ctx.settings.get("share", false));
    const readOnly = !!(a.readOnly ?? ctx.settings.get("readOnly", false));
    const pps = (a.pps ?? ctx.settings.get("pps", true)) ? ppsDevices() : [];
    ctx.settings.set("port", port);
    ctx.settings.set("share", share);
    ctx.settings.set("readOnly", readOnly);
    if (a.pps !== undefined) ctx.settings.set("pps", !!a.pps);

    // -N foreground, -n poll the receiver without waiting for a client, -S port,
    // -G listen on all addresses (else loopback only), -b never reconfigure the receiver.
    // No -F control socket: nothing else should be adding devices to this instance.
    const cmd = [GPSD, "-N", "-n", "-D", "1", "-S", String(port), ...(share ? ["-G"] : []), ...(readOnly ? ["-b"] : []), dev.path, ...pps];
    const proc = ctx.spawn(cmd, { stdin: "ignore", stdout: "pipe", stderr: "pipe" }) as Subprocess<"ignore", "pipe", "pipe">;
    const r: Run = { path: dev.path, key: dev.key, port, share, readOnly, pps, proc, since: Date.now(), sock: null, connected: false, state: newState() };
    run = r;
    lastExit = null;
    log.length = 0;
    addLog(`$ ${cmd.join(" ")}`);
    pumpLines(proc.stdout, addLog);
    pumpLines(proc.stderr, (l) => {
      addLog(l);
      if (/EBUSY|Device or resource busy/i.test(l)) r.state.errors.push(`${dev.path} is busy: another program has it open`);
    });
    proc.exited.then((code) => {
      if (run !== r) return;
      lastExit = { at: Date.now(), code, message: log.slice(-3).map((l) => l.line).join(" · ") };
      r.sock?.end();
      run = null;
      ctx.event(`gpsd exited (${code})`);
      ctx.changed();
    });
    ctx.event(`gpsd started on ${dev.path} (port ${port}${share ? ", shared on the network" : ", local only"})`);
    connect(r);
    // Give gpsd a moment to bind or fail, so the caller hears about a taken port.
    for (let i = 0; i < 20 && run === r && !r.connected; i++) await Bun.sleep(150);
    if (run !== r) throw new HttpError(503, `gpsd exited: ${lastExit?.message || "unknown error"}`);
    ctx.changed();
    return view();
  });
}

async function pumpLines(stream: ReadableStream<Uint8Array>, onLine: (l: string) => void) {
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
}

function connect(r: Run, attempt = 0) {
  if (run !== r || r.stopping) return;
  let buf = "";
  Bun.connect({
    hostname: "127.0.0.1",
    port: r.port,
    socket: {
      open(s) {
        r.sock = s;
        r.connected = true;
        // pps:true also enables TOFF (serial time vs system clock at the start of each cycle).
        s.write('?WATCH={"enable":true,"json":true,"pps":true};\n');
      },
      data(_s, d) {
        buf += d.toString();
        let i;
        while ((i = buf.indexOf("\n")) >= 0) {
          const line = buf.slice(0, i).trim();
          buf = buf.slice(i + 1);
          if (!line) continue;
          try {
            const o = JSON.parse(line);
            const before = r.state.tpv?.mode;
            ingest(r.state, o);
            if (o.class === "TPV" && o.mode !== before) ctx.changed();
          } catch {}
        }
      },
      close() {
        r.connected = false;
        r.sock = null;
        if (run === r && !r.stopping) setTimeout(() => connect(r, 0), 1000);
      },
      error() {},
    },
  }).catch(() => {
    // gpsd hasn't bound yet; retry quickly at first, then back off.
    if (run === r && !r.stopping) setTimeout(() => connect(r, attempt + 1), Math.min(150 * 2 ** attempt, 2000));
  });
}

async function stopRun() {
  const r = run;
  if (!r) return;
  r.stopping = true;
  r.sock?.end();
  r.proc.kill("SIGTERM");
  if (!(await Promise.race([r.proc.exited.then(() => true), Bun.sleep(3000).then(() => false)]))) r.proc.kill("SIGKILL");
  await r.proc.exited;
  run = null;
  ctx.event(`gpsd stopped (${r.path})`);
  ctx.changed();
}

// ---------- Views ----------

function view() {
  const host = hostname();
  return run
    ? {
        running: true,
        device: run.path,
        key: run.key,
        since: run.since,
        port: run.port,
        share: run.share,
        readOnly: run.readOnly,
        pps: run.pps,
        connected: run.connected,
        connect: run.share ? `${host}:${run.port}` : `127.0.0.1:${run.port} (this Pi only)`,
        gpsd: { version: run.state.version, devices: run.state.devices, errors: run.state.errors.slice(-5) },
        fix: summarize(run.state),
        satellites: run.state.sats.map(({ seen, ...s }) => s),
      }
    : { running: false, lastExit };
}

function status() {
  const gps = candidates.filter(isGps);
  const v = view();
  const fix = run ? summarize(run.state) : null;
  return {
    active: !!run || gps.length > 0,
    summary: run ? `${fix!.fix === "unknown" ? "Waiting for data" : fix!.fix} · ${fix!.satsUsed}/${fix!.satsVisible} sats · ${run.path}` : gps.length ? `${gps.length} receiver${gps.length > 1 ? "s" : ""}, gpsd off` : "No GPS receiver",
    candidates,
    ppsDevices: ppsDevices(),
    settings: { port: ctx.settings.get("port", DEFAULT_PORT), share: ctx.settings.get("share", false), readOnly: ctx.settings.get("readOnly", false), pps: ctx.settings.get("pps", true) },
    log: log.slice(-30),
    ...v,
  };
}

// ---------- Routes / tools ----------

const startDoc = { device: "candidate key or /dev/ttyACM0 (optional when there's exactly one GPS)", path: "any tty path instead, e.g. /dev/serial/by-id/…", port: DEFAULT_PORT, share: false, readOnly: false, pps: true };

const routes: Route[] = [
  { method: "GET", path: "/api/gps", doc: "GPS: candidate ttys (known receivers, serial adapters, who has them open), gpsd state, fix, DOPs, satellites, PPS/time offsets", handler: () => status() },
  {
    method: "POST",
    path: "/api/gps/start",
    doc: { body: startDoc, description: "Start gpsd on a receiver as a temporary child (share:true listens on all addresses with -G; readOnly adds -b so gpsd never reconfigures the receiver). Settings are remembered." },
    handler: async (req) => start(await body(req)),
  },
  { method: "POST", path: "/api/gps/stop", doc: "Stop gpsd", handler: () => serial(stopRun).then(() => ({ ok: true })) },
  {
    method: "POST",
    path: "/api/gps/use",
    doc: { body: { device: "candidate key or path", use: true }, description: "Mark a generic USB serial device as a GPS receiver (remembered by its udev serial/port)" },
    handler: async (req) => {
      const b = await body(req);
      const c = candidates.find((c) => c.key === b.device || c.path === b.device);
      if (!c) throw new HttpError(404, `No serial device "${b.device}"`);
      const m = { ...ctx.settings.get<Record<string, boolean>>("useAsGps", {}) };
      if (b.use === false) delete m[c.key];
      else m[c.key] = true;
      ctx.settings.set("useAsGps", m);
      scanTtys();
      ctx.changed();
      return { ok: true };
    },
  },
];

const tools: Tool[] = [
  {
    name: "gps_status",
    description: "GPS fix and satellites from the GPS receiver on DeskyPi: fix mode (no fix/2D/3D), lat/lon/altitude, speed/track, UTC time and its offset from the system clock, HDOP/PDOP, satellites used/visible per constellation (GPS/GLONASS/Galileo/BeiDou) with elevation/azimuth/SNR, PPS offset. Also lists candidate serial devices when gpsd isn't running.",
    inputSchema: { type: "object", properties: { satellites: { type: "boolean", description: "Include the per-satellite list (default false)" } } },
    handler: (a) => {
      const s = status();
      const { log: _l, satellites, ...rest } = s as any;
      return a.satellites ? { ...rest, satellites } : rest;
    },
  },
  {
    name: "gps_start",
    description: "Start gpsd (temporary, stops on gps_stop/unplug) on a GPS receiver so its fix can be read with gps_status and other machines can use it as a gpsd server.",
    inputSchema: {
      type: "object",
      properties: {
        device: { type: "string", description: "Candidate key or tty path from gps_status; optional when exactly one known GPS is plugged in" },
        port: { type: "number", description: `gpsd TCP port (default ${DEFAULT_PORT})` },
        share: { type: "boolean", description: "Listen on all network interfaces instead of localhost only" },
        readOnly: { type: "boolean", description: "Never send configuration commands to the receiver (gpsd -b)" },
      },
    },
    handler: (a) => start(a),
  },
  { name: "gps_stop", description: "Stop gpsd and release the GPS receiver's serial port.", inputSchema: { type: "object", properties: {} }, handler: () => serial(stopRun).then(() => ({ ok: true })) },
];

let tick: ReturnType<typeof setInterval> | null = null;

export default {
  id: "gps",
  title: "GPS",
  order: 64,
  description: "USB GPS receivers via a temporary gpsd: fix, sky plot, PPS (experimental, nothing runs unless started)",
  init(c) {
    ctx = c;
    scanTtys();
    // Who holds each candidate tty: only worth the /proc walk while there are candidates.
    tick = setInterval(() => candidates.length && refreshHolders(), 5000);
  },
  async stop() {
    if (tick) clearInterval(tick);
    await stopRun();
  },
  udev(ev) {
    if (ev.props.SUBSYSTEM !== "tty" || !/^\/dev\/tty(ACM|USB)\d+$/.test(ev.props.DEVNAME || "")) return;
    const before = new Set(candidates.filter(isGps).map((c) => c.path));
    scanTtys();
    if (ev.action === "add") {
      const c = candidates.find((c) => c.path === ev.props.DEVNAME);
      if (c && isGps(c) && !before.has(c.path)) ctx.event(`GPS receiver plugged in: ${c.label || c.model} on ${c.path}`);
    }
    if (ev.action === "remove" && run?.path === ev.props.DEVNAME) {
      ctx.event(`GPS receiver on ${run.path} unplugged; stopping gpsd`);
      serial(stopRun);
    }
    ctx.changed();
  },
  status,
  routes,
  tools,
} satisfies Driver;
