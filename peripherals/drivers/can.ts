// ==================== CAN (SocketCAN) ====================
//
// Every SocketCAN netdev (USB adapters such as gs_usb candleLight/CANable 2, peak_usb,
// slcan, vcan) gets: bit-timing config through `ip link`, a safe listen-only auto-bitrate,
// live stats (frames/s, bus load, controller state, error counters), a per-ID table, a frame
// log, send/periodic TX, candump-format recording, a WebSocket stream, a socketcand-protocol
// TCP server (python-can, SavvyCAN, Kayak) with optional UDP discovery beacon, and MCP
// tools.
//
// Traffic comes from one raw socket per interface (the "hub", see lib/cansock.ts) opened
// with RECV_OWN_MSGS and every error class, so a single reader feeds stats, the UI, captures
// and recordings. Frames we sent come back through it once the controller confirms them and
// are tagged dir:"tx". Aggregation happens here: browsers get a 4 Hz ID-table snapshot plus
// batched frames over the WebSocket, never one SSE push per frame.

import type { ServerWebSocket, Socket as TcpSocket, Subprocess, TCPSocketListener } from "bun";
import { existsSync, mkdirSync, readdirSync, readlinkSync, statSync, unlinkSync } from "node:fs";
import { hostname } from "node:os";
import { HttpError, body, readText, run, usbName, type Ctx, type Driver, type Props, type UdevEvent, type WsData } from "../core.ts";
import * as C from "../lib/cansock.ts";

const IP = existsSync("/usr/sbin/ip") ? "/usr/sbin/ip" : "ip";
/** Side instances (tests, other agents) shift their listener ports by this much. */
const PORT_OFFSET = parseInt(process.env.DESKYPI_PORT_OFFSET || "") || 0;
const SOCKETCAND_PORT = 29536;
const BEACON_PORT = 42000;
const REC_DIR = process.env.DESKYPI_CAN_REC_DIR || "/var/tmp/deskypi-can";
const REC_MAX_BYTES = 256 * 1024 * 1024; // a runaway recording must not fill the root fs again
const RING_MAX = 2000;
const ID_ROWS_MAX = 512;
const ERRS_MAX = 50;
const MIN_PERIOD_MS = 5;

/** Serial adapters running SLCAN (Lawicel ASCII) firmware, by USB VID:PID. */
const SLCAN_USB: Record<string, string> = {
  "16d0:117e": "CANable (slcan firmware)",
  "0403:ffa8": "Lawicel CANUSB",
};
/** slcand -s<N> codes. */
const SLCAN_SPEED: Record<number, number> = { 10000: 0, 20000: 1, 50000: 2, 100000: 3, 125000: 4, 250000: 5, 500000: 6, 800000: 7, 1000000: 8 };

let ctx: Ctx;

// ---------- Interfaces (from `ip -details -statistics -json link show`) ----------

export interface Iface {
  name: string;
  kind: string; // "can" for real controllers (incl. slcan), "vcan", "vxcan"
  driver: string;
  up: boolean;
  operstate: string;
  mtu: number;
  fdCapable: boolean;
  fdOn: boolean;
  state?: string; // ERROR-ACTIVE, ERROR-WARNING, ERROR-PASSIVE, BUS-OFF, STOPPED
  berr?: { tx: number; rx: number };
  restartMs?: number;
  bitrate?: number;
  samplePoint?: number;
  dbitrate?: number;
  dsamplePoint?: number;
  ctrl: string[];
  ctrlSupported: string[] | null; // null: kernel/iproute2 didn't say
  clock?: number;
  btConst?: any;
  dbtConst?: any;
  bitrateConst?: number[];
  xstats?: Record<string, number>;
  stats: { rxPackets: number; txPackets: number; rxErrors: number; txErrors: number; rxDropped: number; txDropped: number; rxBytes: number; txBytes: number };
  // Filled from udev / our own bookkeeping:
  key: string;
  label: string;
  usb?: { vendor: string; model: string; serial: string; vidpid: string; path: string };
  slcanTty?: string;
}

/** Pure: one `ip -d -s -j link show` entry → Iface (minus udev enrichment). null if not CAN. */
export function parseIpLink(l: any): Iface | null {
  if (l?.link_type !== "can") return null;
  const li = l.linkinfo || {};
  const d = li.info_data || {};
  const s = l.stats64 || {};
  const kind = li.info_kind || "can";
  const ctrl: string[] = Array.isArray(d.ctrlmode) ? d.ctrlmode : [];
  const ctrlSupported: string[] | null = Array.isArray(d.ctrlmode_supported) ? [...d.ctrlmode_supported, ...ctrl] : null;
  const num = (v: any) => (v == null || v === "" ? undefined : Number(v));
  const fdCapable = kind === "vcan" || kind === "vxcan" || !!d.data_bittiming_const || !!d.data_bitrate_const || !!ctrlSupported?.includes("FD") || ctrl.includes("FD");
  return {
    name: l.ifname,
    kind,
    driver: kind === "can" ? "" : kind,
    up: Array.isArray(l.flags) && l.flags.includes("UP"),
    operstate: l.operstate || "",
    mtu: l.mtu || 16,
    fdCapable,
    fdOn: kind === "can" ? ctrl.includes("FD") : l.mtu === 72,
    state: d.state,
    berr: d.berr_counter ? { tx: d.berr_counter.tx ?? 0, rx: d.berr_counter.rx ?? 0 } : undefined,
    restartMs: num(d.restart_ms),
    bitrate: num(d.bittiming?.bitrate ?? d.bittiming_bitrate) || undefined,
    samplePoint: num(d.bittiming?.sample_point),
    dbitrate: num(d.data_bittiming?.bitrate ?? d.data_bittiming_bitrate) || undefined,
    dsamplePoint: num(d.data_bittiming?.sample_point),
    ctrl,
    ctrlSupported,
    clock: num(d.clock),
    btConst: d.bittiming_const,
    dbtConst: d.data_bittiming_const,
    bitrateConst: d.bitrate_const,
    xstats: li.info_xstats,
    stats: {
      rxPackets: s.rx?.packets ?? 0,
      txPackets: s.tx?.packets ?? 0,
      rxErrors: s.rx?.errors ?? 0,
      txErrors: s.tx?.errors ?? 0,
      rxDropped: s.rx?.dropped ?? 0,
      txDropped: s.tx?.dropped ?? 0,
      rxBytes: s.rx?.bytes ?? 0,
      txBytes: s.tx?.bytes ?? 0,
    },
    key: "",
    label: "",
  };
}

const ifaces = new Map<string, Iface>();
const busy = new Map<string, string>(); // iface → what's running (configure / autobitrate)

function netProps(name: string): Props | undefined {
  return ctx.devices((p) => p.SUBSYSTEM === "net" && p.INTERFACE === name)[0];
}

function readLinkBase(p: string): string {
  try {
    return readlinkSync(p).split("/").pop() || "";
  } catch {
    return "";
  }
}

/** Stable identity for persisted config: USB serial (+channel) beats USB path beats the name. */
function enrich(i: Iface, devPort: string) {
  const slc = [...slcans.values()].find((s) => s.name === i.name);
  const np = netProps(i.name);
  const usb = np ? ctx.ancestor(np.DEVPATH, "usb", "usb_device") : null;
  if (i.kind === "can") i.driver = readLinkBase(`/sys/class/net/${i.name}/device/driver`) || (slc ? "slcan" : "");
  if (slc) {
    i.slcanTty = slc.tty;
    // slcand sets the speed with an "S<n>" command the kernel never sees: it reports bitrate 0.
    if (!i.bitrate) i.bitrate = slc.bitrate;
    i.key = "slcan:" + slc.ttyKey;
    i.label = slc.label;
  } else if (usb) {
    const { vendor, model } = usbName(usb);
    const vidpid = `${usb.ID_VENDOR_ID}:${usb.ID_MODEL_ID}`;
    i.usb = { vendor, model, serial: usb.ID_SERIAL_SHORT || "", vidpid, path: np?.ID_PATH || "" };
    const chan = devPort && devPort !== "0" ? `#${devPort}` : "";
    i.key = usb.ID_SERIAL_SHORT ? `usb:${vidpid}:${usb.ID_SERIAL_SHORT}${chan}` : np?.ID_PATH ? `path:${np.ID_PATH}${chan}` : `if:${i.name}`;
    i.label = [vendor, model].filter(Boolean).join(" ");
  } else {
    i.key = i.kind === "can" ? `if:${i.name}` : `${i.kind}:${i.name}`;
    i.label = i.kind === "vcan" ? "Virtual CAN" : i.kind === "vxcan" ? "Virtual CAN tunnel" : i.driver;
  }
}

let refreshing: Promise<void> | null = null;
let lastSig = "";

function refresh(): Promise<void> {
  if (refreshing) return refreshing;
  refreshing = (async () => {
    const r = await run([IP, "-details", "-statistics", "-json", "link", "show"], { timeoutMs: 5000 });
    let arr: any[] = [];
    try {
      arr = JSON.parse(r.out || "[]");
    } catch {}
    const seen = new Set<string>();
    for (const l of arr) {
      const i = parseIpLink(l);
      if (!i) continue;
      seen.add(i.name);
      enrich(i, await readText(`/sys/class/net/${i.name}/dev_port`));
      const prev = ifaces.get(i.name);
      ifaces.set(i.name, i);
      if (!prev) onAppear(i);
    }
    for (const name of [...ifaces.keys()]) if (!seen.has(name)) onGone(name);
    for (const i of ifaces.values()) syncHub(i);
    const sig = [...ifaces.values()].map((i) => `${i.name}:${i.up}:${i.state}:${i.bitrate}:${i.dbitrate}:${i.ctrl.join()}:${i.mtu}:${i.restartMs}`).join("|");
    if (sig !== lastSig) {
      lastSig = sig;
      ctx.changed();
    }
  })().finally(() => (refreshing = null));
  return refreshing;
}

let initialScan = true;

function onAppear(i: Iface) {
  hubs.set(i.name, hubs.get(i.name) || new Hub(i.name));
  if (!initialScan) ctx.event(`CAN interface ${i.name} appeared${i.label ? ` (${i.label})` : ""}`, { iface: i.name, key: i.key });
  const cfg = savedCfg(i.key);
  // Apply saved config on hotplug, and at startup for adapters still down after a reboot.
  if (cfg?.bringUpOnPlug && !i.up) {
    setTimeout(() => configure(i.name, { ...cfg, up: true }, false).then(
      () => ctx.event(`${i.name}: applied saved config and brought up`),
      (e) => ctx.event(`${i.name}: saved config failed: ${e.message}`),
    ), 300);
  }
}

function onGone(name: string) {
  ifaces.delete(name);
  const hub = hubs.get(name);
  if (hub) {
    hub.closeSock();
    hub.stopRecording("interface removed");
    for (const j of hub.jobs.values()) stopJob(hub, j.id);
    for (const ws of hub.clients) {
      ws.send(JSON.stringify({ type: "error", message: `${name} was removed` }));
      ws.close();
    }
    hubs.delete(name);
  }
  ctx.event(`CAN interface ${name} removed`, { iface: name });
}

// ---------- Hub: one monitoring socket + aggregates per interface ----------

/** The wire shape used by the WebSocket, captures and the log. ts = Unix seconds. */
interface OutFrame {
  id: number;
  ext: boolean;
  rtr: boolean;
  fd: boolean;
  brs: boolean;
  esi?: boolean;
  err?: boolean;
  len: number;
  data: string;
  ts: number;
  dir: "rx" | "tx";
  text?: string; // decoded error
}

interface IdRow {
  id: number;
  ext: boolean;
  fd: boolean;
  rtr: boolean;
  len: number;
  count: number;
  last: number; // ms
  period: number; // ms, smoothed
  data: Uint8Array;
  changedAt: number[];
  dir: "rx" | "tx";
}

interface Job {
  id: number;
  frame: C.CanFrame;
  periodMs: number;
  sent: number;
  errors: number;
  lastErr?: string;
  timer: ReturnType<typeof setInterval>;
  since: number;
}

interface Capture {
  filters: C.IdFilter[] | null;
  frames: OutFrame[];
  max: number;
  done: () => void;
}

interface WsState {
  iface: string;
  filters: C.IdFilter[] | null;
  frames: boolean;
  batch: boolean;
  stats: boolean;
  q: OutFrame[];
  dropped: number;
}

let jobSeq = 1;

class Hub {
  sock: C.CanSocket | null = null;
  openErr = "";
  ids = new Map<string, IdRow>();
  ring: OutFrame[] = [];
  errors: (C.DecodedError & { iface: string })[] = [];
  tot = { rx: 0, tx: 0, err: 0 };
  win = { rx: 0, tx: 0, err: 0, busTime: 0, t: Date.now() };
  rates = { fps: 0, txps: 0, errps: 0, load: null as number | null };
  clients = new Set<ServerWebSocket<WsData>>();
  captures = new Set<Capture>();
  jobs = new Map<number, Job>();
  rec: { file: string; path: string; writer: ReturnType<ReturnType<typeof Bun.file>["writer"]>; frames: number; bytes: number; since: number } | null = null;

  constructor(public name: string) {}

  open(i: Iface) {
    if (this.sock) return;
    try {
      this.sock = new C.CanSocket(this.name, { fdFrames: true, errMask: C.CAN_ERR_MASK, recvOwn: true, rcvbuf: 1 << 20 }, (f) => this.onFrames(f), (err) => {
        this.sock = null;
        if (err && ifaces.has(this.name)) ctx.log(`${this.name}: reader stopped: ${err.message}`);
        // The interface went down (ENETDOWN) or vanished; the poll reopens when it's back.
        setTimeout(() => refresh(), 500);
      });
      this.openErr = "";
    } catch (e: any) {
      this.openErr = e.message;
      if (i.up) ctx.log(`${this.name}: ${e.message}`);
    }
  }

  closeSock() {
    const s = this.sock;
    this.sock = null;
    s?.close();
  }

  send(f: C.CanFrame) {
    const i = ifaces.get(this.name);
    if (!i) throw new HttpError(404, `No CAN interface ${this.name}`);
    if (!i.up || !this.sock) throw new HttpError(409, `${this.name} is down`);
    if (f.fd && !i.fdOn) throw new HttpError(400, `${this.name} is not in CAN FD mode${i.fdCapable ? " (enable FD in its config)" : " (the controller doesn't support FD)"}`);
    if (i.ctrl.includes("LISTEN-ONLY")) throw new HttpError(409, `${this.name} is listen-only`);
    try {
      this.sock.send(f);
    } catch (e: any) {
      throw new HttpError(503, e.message);
    }
  }

  onFrames(frames: C.CanFrame[]) {
    const i = ifaces.get(this.name);
    const now = Date.now();
    for (const f of frames) {
      const o = this.toOut(f);
      if (f.err) {
        this.tot.err++;
        this.win.err++;
        const d = C.decodeError(f);
        o.text = d.text;
        this.errors.push({ ...d, iface: this.name });
        if (this.errors.length > ERRS_MAX) this.errors.splice(0, this.errors.length - ERRS_MAX);
      } else {
        if (o.dir === "tx") {
          this.tot.tx++;
          this.win.tx++;
        } else {
          this.tot.rx++;
          this.win.rx++;
        }
        if (i?.bitrate) this.win.busTime += C.frameTime(f, i.bitrate, i.dbitrate);
        this.updateId(f, o.dir, now);
      }
      this.ring.push(o);
      if (this.rec) this.record(f);
      for (const c of this.captures) {
        if (c.frames.length < c.max && C.matchFilters(f, c.filters)) {
          c.frames.push(o);
          if (c.frames.length >= c.max) c.done();
        }
      }
      for (const ws of this.clients) {
        const st = ws.data.state as WsState;
        if (!st.frames || !C.matchFilters(f, st.filters)) continue; // errors pass only when unfiltered
        if (st.batch) {
          if (st.q.length < 1000) st.q.push(o);
          else st.dropped++;
        } else if (ws.getBufferedAmount() < 1 << 20) ws.send(JSON.stringify({ type: "frame", ...o }));
        else st.dropped++;
      }
    }
    if (this.ring.length > RING_MAX * 1.5) this.ring.splice(0, this.ring.length - RING_MAX);
  }

  toOut(f: C.CanFrame): OutFrame {
    // The kernel flags our own echoed frames (MSG_CONFIRM): they made it onto the bus.
    const dir: "rx" | "tx" = f.own ? "tx" : "rx";
    const o: OutFrame = { id: f.id, ext: f.ext, rtr: f.rtr, fd: f.fd, brs: f.brs, len: f.len, data: C.toHex(f.data), ts: Math.round(f.ts * 1000) / 1e6, dir };
    if (f.esi) o.esi = true;
    if (f.err) o.err = true;
    return o;
  }

  updateId(f: C.CanFrame, dir: "rx" | "tx", now: number) {
    const k = C.frameKey(f);
    let r = this.ids.get(k);
    if (!r) {
      if (this.ids.size >= 8192) return; // fuzzing/random ids: the table stops growing, counters don't
      r = { id: f.id, ext: f.ext, fd: f.fd, rtr: f.rtr, len: f.len, count: 0, last: 0, period: 0, data: f.data, changedAt: Array(f.data.length).fill(0), dir };
      this.ids.set(k, r);
    } else {
      const dt = f.ts - r.last;
      r.period = r.period ? r.period * 0.8 + dt * 0.2 : dt;
      const n = Math.max(r.data.length, f.data.length);
      for (let b = 0; b < n; b++) if (r.data[b] !== f.data[b]) r.changedAt[b] = now;
      r.changedAt.length = f.data.length;
      r.data = f.data;
    }
    r.count++;
    r.last = f.ts;
    r.len = f.len;
    r.fd = f.fd;
    r.rtr = f.rtr;
    r.dir = dir;
  }

  tick() {
    const now = Date.now();
    const dt = Math.max(0.2, (now - this.win.t) / 1000);
    const i = ifaces.get(this.name);
    this.rates = {
      fps: Math.round((this.win.rx + this.win.tx) / dt),
      txps: Math.round(this.win.tx / dt),
      errps: Math.round(this.win.err / dt),
      load: i?.bitrate ? Math.min(100, Math.round((this.win.busTime / dt) * 1000) / 10) : null,
    };
    this.win = { rx: 0, tx: 0, err: 0, busTime: 0, t: now };
  }

  clear() {
    this.ids.clear();
    this.ring = [];
    this.errors = [];
    this.tot = { rx: 0, tx: 0, err: 0 };
  }

  idRows(limit = ID_ROWS_MAX) {
    const now = Date.now();
    const rows = [...this.ids.values()].sort((a, b) => Number(a.ext) - Number(b.ext) || a.id - b.id).slice(0, limit);
    return rows.map((r) => {
      let hot = 0;
      for (let b = 0; b < r.changedAt.length && b < 31; b++) if (now - r.changedAt[b] < 1000) hot |= 1 << b;
      return { id: r.id, ext: r.ext, fd: r.fd, rtr: r.rtr, len: r.len, count: r.count, period: Math.round(r.period * 10) / 10, data: C.toHex(r.data), hot, last: Math.round(r.last), dir: r.dir };
    });
  }

  snapshot(full: boolean) {
    const i = ifaces.get(this.name);
    return {
      iface: this.name,
      up: !!i?.up,
      state: i?.state,
      berr: i?.berr,
      rates: this.rates,
      totals: this.tot,
      drops: this.sock?.drops || 0, // frames the kernel dropped because we fell behind
      idCount: this.ids.size,
      ...(full ? { ids: this.idRows(), errors: this.errors.slice(-20) } : {}),
      jobs: [...this.jobs.values()].map(jobOut),
      recording: this.rec && { file: this.rec.file, frames: this.rec.frames, bytes: this.rec.bytes, since: this.rec.since },
    };
  }

  // ----- recording (candump -L format) -----

  startRecording(): string {
    if (this.rec) throw new HttpError(409, `${this.name} is already recording to ${this.rec.file}`);
    mkdirSync(REC_DIR, { recursive: true });
    const stamp = new Date().toISOString().replace(/[:]/g, "").replace(/\..*/, "");
    const file = `${this.name}-${stamp}.log`;
    const path = `${REC_DIR}/${file}`;
    this.rec = { file, path, writer: Bun.file(path).writer({ highWaterMark: 64 * 1024 }), frames: 0, bytes: 0, since: Date.now() };
    ctx.event(`${this.name}: recording to ${file}`);
    refreshRecordings();
    return file;
  }

  record(f: C.CanFrame) {
    const r = this.rec!;
    const line = C.candumpLine(f, this.name) + "\n";
    r.writer.write(line);
    r.frames++;
    r.bytes += line.length;
    if (r.bytes > REC_MAX_BYTES) this.stopRecording(`size limit ${REC_MAX_BYTES >> 20} MB reached`);
  }

  stopRecording(why = "") {
    const r = this.rec;
    if (!r) return null;
    this.rec = null;
    Promise.resolve(r.writer.end()).then(refreshRecordings, () => {});
    ctx.event(`${this.name}: recording ${r.file} stopped (${r.frames} frames)${why ? ": " + why : ""}`);
    return { file: r.file, frames: r.frames, bytes: r.bytes };
  }
}

const hubs = new Map<string, Hub>();

function syncHub(i: Iface) {
  const hub = hubs.get(i.name) || (hubs.set(i.name, new Hub(i.name)), hubs.get(i.name)!);
  if (busy.has(i.name)) return;
  if (i.up && !hub.sock) hub.open(i);
  else if (!i.up && hub.sock) hub.closeSock();
}

function hubFor(name: string): Hub {
  const h = hubs.get(name);
  if (!h || !ifaces.has(name)) throw new HttpError(404, `No CAN interface "${name}"`);
  return h;
}

function frameFrom(x: any): C.CanFrame {
  try {
    return C.makeFrame(C.specFromJson(x));
  } catch (e: any) {
    throw new HttpError(400, e.message);
  }
}

// ---------- Periodic transmit ----------

function jobOut(j: Job) {
  return { id: j.id, frame: C.candumpBody(j.frame), periodMs: j.periodMs, sent: j.sent, errors: j.errors, lastErr: j.lastErr, since: j.since };
}

function startJob(hub: Hub, frame: C.CanFrame, periodMs: number): Job {
  if (!(periodMs >= MIN_PERIOD_MS && periodMs <= 3600_000)) throw new HttpError(400, `periodMs must be ${MIN_PERIOD_MS}–3600000`);
  if (hub.jobs.size >= 64) throw new HttpError(409, "Too many periodic jobs on this interface");
  hub.send(frame); // validates (up, FD mode, listen-only) before we commit to a timer
  const job: Job = {
    id: jobSeq++,
    frame,
    periodMs,
    sent: 1,
    errors: 0,
    since: Date.now(),
    timer: setInterval(() => {
      try {
        hub.send(frame);
        job.sent++;
        job.lastErr = undefined;
      } catch (e: any) {
        job.errors++;
        job.lastErr = e.message;
        // A bus with nothing ACKing fills the TX queue; give up rather than spin forever.
        if (job.errors >= 50 && job.errors > job.sent) {
          stopJob(hub, job.id);
          ctx.event(`${hub.name}: periodic ${C.candumpBody(frame)} stopped: ${e.message}`);
        }
      }
    }, periodMs),
  };
  hub.jobs.set(job.id, job);
  ctx.changed();
  return job;
}

function stopJob(hub: Hub, id: number) {
  const j = hub.jobs.get(id);
  if (!j) return false;
  clearInterval(j.timer);
  hub.jobs.delete(id);
  ctx.changed();
  return true;
}

// ---------- Configuration via ip link ----------

export interface CanCfg {
  bitrate?: number;
  samplePoint?: number;
  dbitrate?: number;
  dsamplePoint?: number;
  fd?: boolean;
  listenOnly?: boolean;
  loopback?: boolean;
  tripleSampling?: boolean;
  oneShot?: boolean;
  berrReporting?: boolean;
  restartMs?: number;
  bringUpOnPlug?: boolean;
  up?: boolean;
}

const FLAG_ARGS: [keyof CanCfg, string, string][] = [
  ["listenOnly", "listen-only", "LISTEN-ONLY"],
  ["loopback", "loopback", "LOOPBACK"],
  ["tripleSampling", "triple-sampling", "TRIPLE-SAMPLING"],
  ["oneShot", "one-shot", "ONE-SHOT"],
  ["berrReporting", "berr-reporting", "BERR-REPORTING"],
];

/** Accept 0.875, 87.5 or "87.5%". */
function normSp(v: any): number | undefined {
  if (v == null || v === "") return undefined;
  let n = parseFloat(String(v));
  if (n > 1) n /= 100;
  if (!(n > 0 && n < 1)) throw new HttpError(400, `sample point ${v} must be between 0 and 1 (or 0–100 %)`);
  return Math.round(n * 1000) / 1000;
}

export function normCfg(x: any): CanCfg {
  const c: CanCfg = {};
  const int = (k: string, min: number, max: number) => {
    if (x[k] == null || x[k] === "") return undefined;
    const n = Math.round(Number(x[k]));
    if (!(n >= min && n <= max)) throw new HttpError(400, `${k} must be ${min}–${max}`);
    return n;
  };
  c.bitrate = int("bitrate", 1000, 1000000);
  c.dbitrate = int("dbitrate", 1000, 15000000);
  c.restartMs = int("restartMs", 0, 3600000);
  c.samplePoint = normSp(x.samplePoint);
  c.dsamplePoint = normSp(x.dsamplePoint);
  for (const k of ["fd", "listenOnly", "loopback", "tripleSampling", "oneShot", "berrReporting", "bringUpOnPlug", "up"] as const) if (x[k] != null) c[k] = !!x[k];
  for (const k of Object.keys(c) as (keyof CanCfg)[]) if (c[k] === undefined) delete c[k];
  return c;
}

/** Pure: the `ip` argument lists (after `sudo ip`) that apply cfg to an interface. Up/down not included. */
export function ipArgs(i: Pick<Iface, "name" | "kind" | "fdCapable" | "ctrlSupported" | "dbitrate">, c: CanCfg): string[][] {
  if (i.kind !== "can") {
    // vcan/vxcan have no bit timing; CAN FD is just the 72-byte MTU.
    return c.fd !== undefined ? [["link", "set", "dev", i.name, "mtu", c.fd ? "72" : "16"]] : [];
  }
  const a: string[] = [];
  if (c.bitrate) a.push("bitrate", String(c.bitrate));
  if (c.bitrate && c.samplePoint) a.push("sample-point", c.samplePoint.toFixed(3));
  if (c.fd && i.fdCapable) {
    const db = c.dbitrate || i.dbitrate;
    if (!db) throw new HttpError(400, "CAN FD needs a data bitrate (dbitrate)");
    a.push("dbitrate", String(db));
    if (c.dsamplePoint) a.push("dsample-point", c.dsamplePoint.toFixed(3));
    a.push("fd", "on");
  } else if (c.fd === false && i.fdCapable) a.push("fd", "off");
  else if (c.fd && !i.fdCapable) throw new HttpError(400, `${i.name} doesn't support CAN FD`);
  for (const [k, arg, flag] of FLAG_ARGS) {
    const v = c[k];
    if (v === undefined) continue;
    // Only send flags the controller supports; "off" for an unsupported mode is a no-op anyway.
    if (i.ctrlSupported && !i.ctrlSupported.includes(flag)) {
      if (v) throw new HttpError(400, `${i.name} doesn't support ${arg}`);
      continue;
    }
    a.push(arg, v ? "on" : "off");
  }
  if (c.restartMs !== undefined) a.push("restart-ms", String(c.restartMs));
  return a.length ? [["link", "set", "dev", i.name, "type", "can", ...a]] : [];
}

async function sudoIp(args: string[]) {
  const r = await run(["sudo", "-n", IP, ...args], { timeoutMs: 10000 });
  if (r.code !== 0) throw new HttpError(400, `ip ${args.join(" ")}: ${r.err || r.out || "exit " + r.code}`);
}

function savedCfg(key: string): CanCfg | undefined {
  return ctx.settings.get<Record<string, CanCfg>>("ifaces", {})[key];
}

function saveCfg(key: string, c: CanCfg) {
  const all = { ...ctx.settings.get<Record<string, CanCfg>>("ifaces", {}) };
  const { up, ...keep } = { ...all[key], ...c };
  all[key] = keep;
  ctx.settings.set("ifaces", all);
}

async function configure(name: string, raw: any, persist = true) {
  const i = ifaces.get(name);
  if (!i) throw new HttpError(404, `No CAN interface "${name}"`);
  if (busy.has(name)) throw new HttpError(409, `${name} is busy: ${busy.get(name)}`);
  const c = normCfg(raw);
  if (persist) saveCfg(i.key, c);
  const cmds = ipArgs(i, c);
  const wantUp = c.up ?? i.up;
  if (!cmds.length && wantUp === i.up) return ifaceOut(i); // only bringUpOnPlug changed
  busy.set(name, "configuring");
  const hub = hubs.get(name);
  hub?.closeSock();
  try {
    if (i.up && (cmds.length || !wantUp)) await sudoIp(["link", "set", "dev", name, "down"]);
    try {
      for (const a of cmds) await sudoIp(a);
    } catch (e) {
      if (i.up) await sudoIp(["link", "set", "dev", name, "up"]).catch(() => {});
      throw e;
    }
    if (wantUp) {
      try {
        await sudoIp(["link", "set", "dev", name, "up"]);
      } catch (e: any) {
        throw new HttpError(400, /bit-timing|Invalid argument/i.test(e.message) ? `${name} can't come up until a bitrate is set` : e.message);
      }
    }
  } finally {
    busy.delete(name);
    await refresh();
  }
  const n = ifaces.get(name)!;
  ctx.event(`${name}: ${n.up ? "up" : "down"}${n.bitrate ? ` at ${C.fmtBitrate(n.bitrate)}` : ""}${n.fdOn && n.dbitrate ? `/${C.fmtBitrate(n.dbitrate)} FD` : ""}${n.ctrl.length ? " " + n.ctrl.join(" ").toLowerCase() : ""}`);
  return ifaceOut(n);
}

async function restartBus(name: string) {
  const i = ifaces.get(name);
  if (!i) throw new HttpError(404, `No CAN interface "${name}"`);
  if (i.kind !== "can") throw new HttpError(400, `${name} has no controller to restart`);
  await sudoIp(["link", "set", "dev", name, "type", "can", "restart"]);
  await refresh();
  return ifaceOut(ifaces.get(name)!);
}

// ---------- Auto-bitrate (listen-only, never ACKs) ----------

const abProgress = new Map<string, { step: number; of: number; bitrate: number; trials: (C.BitrateTrial & { skipped?: string })[] }>();
const abLast = new Map<string, any>();

async function xstat(name: string): Promise<number> {
  const r = await run([IP, "-details", "-statistics", "-json", "link", "show", "dev", name]);
  try {
    return Number(JSON.parse(r.out)[0]?.linkinfo?.info_xstats?.bus_error) || 0;
  } catch {
    return 0;
  }
}

async function autobitrate(name: string, opts: { rates?: number[]; listenMs?: number; apply?: boolean } = {}) {
  const i = ifaces.get(name);
  if (!i) throw new HttpError(404, `No CAN interface "${name}"`);
  if (busy.has(name)) throw new HttpError(409, `${name} is busy: ${busy.get(name)}`);
  if (i.kind !== "can" && i.kind !== "vcan") throw new HttpError(400, `Auto-bitrate needs a CAN controller (${name} is ${i.kind})`);
  const rates = (opts.rates?.length ? opts.rates : C.STD_BITRATES).map(Number).filter((r) => r >= 1000 && r <= 1000000);
  const listenMs = Math.min(5000, Math.max(100, Number(opts.listenMs) || 400));
  const real = i.kind === "can";
  const supportsBerr = !i.ctrlSupported || i.ctrlSupported.includes("BERR-REPORTING");
  const prev = { bitrate: i.bitrate, samplePoint: i.samplePoint, listenOnly: i.ctrl.includes("LISTEN-ONLY"), berr: i.ctrl.includes("BERR-REPORTING"), up: i.up };
  busy.set(name, "auto-bitrate");
  hubs.get(name)?.closeSock();
  const prog = { step: 0, of: rates.length, bitrate: 0, trials: [] as (C.BitrateTrial & { skipped?: string })[] };
  abProgress.set(name, prog);
  ctx.changed();
  let applied: number | undefined;
  try {
    for (const rate of rates) {
      prog.step++;
      prog.bitrate = rate;
      ctx.changed();
      try {
        await sudoIp(["link", "set", "dev", name, "down"]);
        if (real) {
          const flags = ["listen-only", "on", ...(supportsBerr ? ["berr-reporting", "on"] : [])];
          await sudoIp(["link", "set", "dev", name, "type", "can", "bitrate", String(rate), ...flags]);
        }
        await sudoIp(["link", "set", "dev", name, "up"]);
      } catch (e: any) {
        prog.trials.push({ bitrate: rate, frames: 0, errors: 0, busErrors: 0, skipped: e.message });
        continue;
      }
      const b0 = real ? await xstat(name) : 0;
      let frames = 0;
      let errors = 0;
      let sock: C.CanSocket | null = null;
      try {
        sock = new C.CanSocket(name, { fdFrames: true, errMask: C.CAN_ERR_MASK }, (fs) => {
          for (const f of fs) f.err ? errors++ : frames++;
        });
        await Bun.sleep(listenMs);
      } catch (e: any) {
        prog.trials.push({ bitrate: rate, frames: 0, errors: 0, busErrors: 0, skipped: e.message });
        continue;
      } finally {
        sock?.close();
      }
      const busErrors = real ? Math.max(0, (await xstat(name)) - b0) : 0;
      prog.trials.push({ bitrate: rate, frames, errors, busErrors });
    }
    const verdict = C.pickBitrate(prog.trials.filter((t) => !t.skipped));
    // Put the interface back how it was, or on the winner if asked to apply it.
    await sudoIp(["link", "set", "dev", name, "down"]).catch(() => {});
    if (real) {
      const br = opts.apply && verdict.bitrate ? verdict.bitrate : prev.bitrate;
      const restore = ["link", "set", "dev", name, "type", "can", ...(br ? ["bitrate", String(br)] : []), ...(br && prev.samplePoint && br === prev.bitrate ? ["sample-point", prev.samplePoint.toFixed(3)] : []), "listen-only", prev.listenOnly ? "on" : "off", ...(supportsBerr ? ["berr-reporting", prev.berr ? "on" : "off"] : [])];
      await sudoIp(restore).catch((e) => ctx.log(`${name}: restore failed: ${e.message}`));
      if (opts.apply && verdict.bitrate) {
        applied = verdict.bitrate;
        saveCfg(i.key, { bitrate: verdict.bitrate });
      }
    }
    if (prev.up || applied) await sudoIp(["link", "set", "dev", name, "up"]).catch((e) => ctx.log(`${name}: bring-up failed: ${e.message}`));
    const out = {
      iface: name,
      ...verdict,
      applied: applied ?? null,
      listenMs,
      trials: prog.trials,
      ...(real ? {} : { note: "vcan has no bit timing: every rate sees the same traffic, so this only exercises the procedure" }),
    };
    abLast.set(name, { ...out, at: Date.now() });
    ctx.event(`${name}: auto-bitrate: ${verdict.text}`);
    return out;
  } finally {
    abProgress.delete(name);
    busy.delete(name);
    await refresh();
  }
}

// ---------- vcan ----------

async function createVcan(name?: string, fd = true) {
  if (name && !/^[a-zA-Z][\w-]{0,14}$/.test(name)) throw new HttpError(400, "Interface names are 1–15 letters, digits, - or _");
  if (!name) for (let n = 0; ; n++) if (!existsSync(`/sys/class/net/vcan${n}`)) { name = `vcan${n}`; break; }
  if (existsSync(`/sys/class/net/${name}`)) throw new HttpError(409, `${name} already exists`);
  await run(["sudo", "-n", "modprobe", "vcan"]);
  await sudoIp(["link", "add", "dev", name!, "type", "vcan"]);
  if (fd) await sudoIp(["link", "set", "dev", name!, "mtu", "72"]);
  await sudoIp(["link", "set", "dev", name!, "up"]);
  await refresh();
  ctx.event(`Created virtual CAN ${name}${fd ? " (FD)" : ""}`);
  return ifaceOut(ifaces.get(name!)!);
}

async function removeVcan(name: string) {
  const i = ifaces.get(name);
  if (!i) throw new HttpError(404, `No CAN interface "${name}"`);
  if (i.kind !== "vcan") throw new HttpError(400, `${name} is not a virtual CAN interface`);
  await sudoIp(["link", "del", "dev", name]);
  await refresh();
  return { ok: true, removed: name };
}

// ---------- SLCAN (serial adapters, attached with slcand) ----------

interface Slcan {
  name: string;
  tty: string;
  ttyKey: string;
  label: string;
  bitrate: number;
  proc: Subprocess;
  since: number;
}
const slcans = new Map<string, Slcan>(); // by tty

function slcanCandidates() {
  return ctx
    .devices((p) => p.SUBSYSTEM === "tty" && !!p.DEVNAME && !!SLCAN_USB[`${p.ID_VENDOR_ID}:${p.ID_MODEL_ID}`])
    .map((p) => {
      const { vendor, model } = usbName(p);
      const att = slcans.get(p.DEVNAME);
      return { tty: p.DEVNAME, byId: (p.DEVLINKS || "").split(" ").find((l) => l.includes("/by-id/")) || "", kind: SLCAN_USB[`${p.ID_VENDOR_ID}:${p.ID_MODEL_ID}`], vendor, model, serial: p.ID_SERIAL_SHORT || "", attached: att ? att.name : null };
    });
}

function ttyKey(tty: string): string {
  const p = ctx.devices((d) => d.DEVNAME === tty)[0];
  return p?.ID_SERIAL_SHORT ? `${p.ID_VENDOR_ID}:${p.ID_MODEL_ID}:${p.ID_SERIAL_SHORT}` : p?.ID_PATH || tty;
}

async function attachSlcan(tty: string, opts: { bitrate?: number; name?: string; serialBaud?: number } = {}) {
  if (!/^\/dev\/[\w/.+-]+$/.test(tty || "") || !existsSync(tty)) throw new HttpError(400, `No such serial device ${tty}`);
  if (slcans.has(tty)) throw new HttpError(409, `${tty} is already attached as ${slcans.get(tty)!.name}`);
  const bitrate = Number(opts.bitrate) || 500000;
  const code = SLCAN_SPEED[bitrate];
  if (code === undefined) throw new HttpError(400, `SLCAN supports ${Object.keys(SLCAN_SPEED).map((b) => C.fmtBitrate(+b)).join(", ")}`);
  let name = opts.name;
  if (name && !/^[a-zA-Z][\w-]{0,14}$/.test(name)) throw new HttpError(400, "Interface names are 1–15 letters, digits, - or _");
  if (!name) for (let n = 0; ; n++) if (!existsSync(`/sys/class/net/slcan${n}`)) { name = `slcan${n}`; break; }
  if (existsSync(`/sys/class/net/${name}`)) throw new HttpError(409, `${name} already exists`);
  const baud = Number(opts.serialBaud) || 3000000;
  // -F keeps slcand in the foreground so it's our managed child: killing it (via sudo, which
  // relays SIGTERM) closes the channel (-c) and removes the netdev.
  const proc = ctx.spawn(["sudo", "-n", "slcand", "-o", "-c", "-F", `-s${code}`, "-t", "hw", "-S", String(baud), tty, name!], { stdin: "ignore", stdout: "ignore", stderr: "pipe" });
  for (let t = 0; t < 40 && !existsSync(`/sys/class/net/${name}`); t++) {
    await Bun.sleep(100);
    if (proc.exitCode !== null) break;
  }
  if (!existsSync(`/sys/class/net/${name}`)) {
    proc.kill();
    const err = await new Response(proc.stderr as ReadableStream).text().catch(() => "");
    throw new HttpError(500, `slcand didn't create ${name}${err ? ": " + err.trim() : ""}`);
  }
  const cand = slcanCandidates().find((c) => c.tty === tty);
  const s: Slcan = { name: name!, tty, ttyKey: ttyKey(tty), label: cand ? `${cand.kind} on ${tty}` : `SLCAN on ${tty}`, bitrate, proc, since: Date.now() };
  slcans.set(tty, s);
  proc.exited.then((code) => {
    if (slcans.get(tty) === s) slcans.delete(tty);
    ctx.event(`SLCAN ${s.name} on ${tty} detached${code ? ` (slcand exit ${code})` : ""}`);
    refresh();
  });
  await sudoIp(["link", "set", "dev", name!, "up"]).catch((e) => ctx.log(e.message));
  await refresh();
  ctx.event(`SLCAN ${name} attached on ${tty} at ${C.fmtBitrate(bitrate)}`);
  return ifaceOut(ifaces.get(name!) || ({ name } as any));
}

async function detachSlcan(nameOrTty: string) {
  const s = slcans.get(nameOrTty) || [...slcans.values()].find((x) => x.name === nameOrTty);
  if (!s) throw new HttpError(404, `${nameOrTty} isn't an attached SLCAN adapter`);
  s.proc.kill("SIGTERM");
  await Promise.race([s.proc.exited, Bun.sleep(3000)]);
  if (s.proc.exitCode === null) s.proc.kill("SIGKILL");
  slcans.delete(s.tty);
  await refresh();
  return { ok: true, detached: s.name, tty: s.tty };
}

// ---------- Recordings ----------

let recordings: { file: string; size: number; mtime: number }[] = [];

function refreshRecordings() {
  try {
    recordings = readdirSync(REC_DIR)
      .filter((f) => f.endsWith(".log"))
      .map((f) => {
        const st = statSync(`${REC_DIR}/${f}`);
        return { file: f, size: st.size, mtime: st.mtimeMs };
      })
      .sort((a, b) => b.mtime - a.mtime);
  } catch {
    recordings = [];
  }
  ctx.changed();
}

function recPath(file: string): string {
  if (!/^[\w.-]+\.log$/.test(file) || file.startsWith(".")) throw new HttpError(400, "Bad recording name");
  const p = `${REC_DIR}/${file}`;
  if (!existsSync(p)) throw new HttpError(404, `No recording ${file}`);
  return p;
}

// ---------- Capture (for REST + MCP) ----------

async function capture(name: string, ms: number, filters: C.IdFilter[] | null, max: number) {
  const hub = hubFor(name);
  if (!hub.sock) throw new HttpError(409, `${name} is down`);
  ms = Math.min(60000, Math.max(10, Number(ms) || 1000));
  max = Math.min(20000, Math.max(1, Number(max) || 1000));
  let done!: () => void;
  const finished = new Promise<void>((r) => (done = r));
  const c: Capture = { filters, frames: [], max, done };
  hub.captures.add(c);
  await Promise.race([finished, Bun.sleep(ms)]);
  hub.captures.delete(c);
  return { iface: name, ms, count: c.frames.length, truncated: c.frames.length >= max, frames: c.frames };
}

// ---------- socketcand server ----------

interface ScClient {
  n: number;
  peer: string;
  mode: "nobus" | "bcm" | "raw" | "control";
  iface: string;
  sock: C.CanSocket | null;
  buf: string;
  pending: string;
  holdUntil: number;
  held: string;
  subs: Map<string, { throttleMs: number; last: number; mask?: Uint8Array; lastMasked?: string }>;
  jobs: Map<string, { timer: ReturnType<typeof setInterval>; frame: C.CanFrame }>;
  statTimer: ReturnType<typeof setInterval> | null;
  rx: number;
  tx: number;
  dropped: number;
  since: number;
}

let scServer: TCPSocketListener<ScClient> | null = null;
let scError = "";
let scSeq = 1;
const scClients = new Set<TcpSocket<ScClient>>();

function scPort(): number {
  return ctx.settings.get("socketcandPort", SOCKETCAND_PORT) + PORT_OFFSET;
}

function scWrite(s: TcpSocket<ScClient>, str: string) {
  const c = s.data;
  if (c.pending) {
    if (c.pending.length > 1 << 20) {
      c.dropped++;
      return;
    }
    c.pending += str;
    return;
  }
  const n = s.write(str);
  if (n < str.length) c.pending = str.slice(Math.max(0, n));
}

function scOnFrames(s: TcpSocket<ScClient>, frames: C.CanFrame[]) {
  const c = s.data;
  let out = "";
  const now = Date.now();
  for (const f of frames) {
    if (c.mode === "raw") {
      if (f.err) out += C.scError(f);
      else if (!f.rtr) out += C.scFrameRaw(f); // socketcand doesn't forward RTR in raw mode
      c.rx++;
    } else if (c.mode === "bcm" && !f.err) {
      const sub = c.subs.get(C.frameKey(f));
      if (!sub) continue;
      if (sub.mask) {
        const masked = C.toHex(f.data.map((b, i) => b & (sub.mask![i] ?? 0)));
        if (masked === sub.lastMasked) continue;
        sub.lastMasked = masked;
      }
      if (sub.throttleMs && now - sub.last < sub.throttleMs) continue;
      sub.last = now;
      out += C.scFrameBcm(f);
      c.rx++;
    }
  }
  if (!out) return;
  // python-can reads "< ok >" with an exact-match recv(), so frames that arrive right after
  // the rawmode reply are held back briefly (not dropped) to keep them out of its segment.
  if (now < c.holdUntil || c.held) {
    // (While a flush is pending, later frames queue behind the held ones to keep order.)
    if (!c.held) setTimeout(() => {
      const h = c.held;
      c.held = "";
      if (h && scClients.has(s)) scWrite(s, h);
    }, Math.max(0, c.holdUntil - now));
    c.held += out;
  } else scWrite(s, out);
}

function scIdKey(tok: string): string {
  const id = parseInt(tok, 16) & C.CAN_EFF_MASK;
  return C.frameKey({ id, ext: tok.length === 8 || id > C.CAN_SFF_MASK });
}

async function scCommand(s: TcpSocket<ScClient>, cmd: string) {
  const c = s.data;
  const w = C.scWords(cmd);
  const verb = w[0] || "";
  if (c.mode === "nobus") {
    if (verb === "open" && w[1]) {
      const name = w[1];
      const i = ifaces.get(name);
      if (!i) {
        scWrite(s, "< error could not open bus >");
        s.end();
        return;
      }
      // Like socketcand's canctl_start_iface: a down bus with a saved bitrate (or a vcan) is brought up.
      if (!i.up && (i.kind !== "can" || savedCfg(i.key)?.bitrate)) await configure(name, { ...savedCfg(i.key), up: true }, false).catch(() => {});
      try {
        c.sock = new C.CanSocket(name, { errMask: ctx.settings.get("socketcandErrMask", 0) }, (f) => scOnFrames(s, f), (err) => {
          if (err && scClients.has(s)) s.end();
        });
      } catch {
        scWrite(s, "< error could not open bus >");
        s.end();
        return;
      }
      c.iface = name;
      c.mode = "bcm";
      scWrite(s, "< ok >");
      ctx.changed();
    } else if (w.length >= 2 && (w[1] === "B" || w[1] === "C")) scWrite(s, "< error configuration operation not supported >");
    else scWrite(s, "< error unknown command >");
    return;
  }
  if (verb === "rawmode" || verb === "bcmmode" || verb === "controlmode") {
    c.mode = verb === "rawmode" ? "raw" : verb === "bcmmode" ? "bcm" : "control";
    if (c.mode !== "control" && c.statTimer) {
      clearInterval(c.statTimer);
      c.statTimer = null;
    }
    c.holdUntil = Date.now() + 50;
    scWrite(s, "< ok >");
    ctx.changed();
    return;
  }
  if (verb === "isotpmode") return scWrite(s, "< error isotp mode not supported >");
  if (verb === "echo") return scWrite(s, "< echo >");
  if (verb === "send" && c.mode !== "control") {
    try {
      c.sock?.send(C.scParseIdLenData(w.slice(1)));
      c.tx++;
    } catch (e: any) {
      // socketcand ignores bad sends silently; an "< error … >" line would be misread as an
      // error frame by python-can's parser.
      ctx.log(`socketcand ${c.peer}: ${cmd}: ${e.message}`);
    }
    return;
  }
  if (c.mode === "bcm") {
    try {
      if (verb === "add") {
        // < add SEC USEC ID LEN DATA… >
        const period = Number(w[1]) * 1000 + Number(w[2]) / 1000;
        if (!(period >= 1)) throw new Error("period too short");
        const frame = C.scParseIdLenData(w.slice(3));
        const k = C.frameKey(frame);
        if (c.jobs.has(k)) clearInterval(c.jobs.get(k)!.timer);
        const job = { frame, timer: setInterval(() => { try { c.sock?.send(job.frame); c.tx++; } catch {} }, Math.max(MIN_PERIOD_MS, period)) };
        c.jobs.set(k, job);
      } else if (verb === "update") {
        const frame = C.scParseIdLenData(w.slice(1));
        const j = c.jobs.get(C.frameKey(frame));
        if (j) j.frame = frame;
        c.sock?.send(frame);
      } else if (verb === "delete") {
        const k = scIdKey(w[1]);
        const j = c.jobs.get(k);
        if (j) clearInterval(j.timer);
        c.jobs.delete(k);
      } else if (verb === "subscribe") {
        // < subscribe SEC USEC ID >
        c.subs.set(scIdKey(w[3]), { throttleMs: Number(w[1]) * 1000 + Number(w[2]) / 1000, last: 0 });
      } else if (verb === "unsubscribe") {
        c.subs.delete(scIdKey(w[1]));
      } else if (verb === "filter") {
        // < filter SEC USEC ID LEN MASK… >: deliver when the masked content changes.
        const f = C.scParseIdLenData(w.slice(3));
        c.subs.set(scIdKey(w[3]), { throttleMs: Number(w[1]) * 1000 + Number(w[2]) / 1000, last: 0, mask: f.data });
      } else if (verb === "muxfilter") {
        scWrite(s, "< error muxfilter not supported >");
      } else scWrite(s, "< error unknown command >");
    } catch (e: any) {
      ctx.log(`socketcand ${c.peer}: ${cmd}: ${e.message}`);
    }
    return;
  }
  if (c.mode === "control" && verb === "statistics") {
    const ival = Number(w[1]) || 0;
    if (c.statTimer) clearInterval(c.statTimer);
    c.statTimer = ival > 0 ? setInterval(async () => {
      const st = (k: string) => readText(`/sys/class/net/${c.iface}/statistics/${k}`).then((v) => Number(v) || 0);
      const [rb, rp, tb, tp] = await Promise.all([st("rx_bytes"), st("rx_packets"), st("tx_bytes"), st("tx_packets")]);
      scWrite(s, `< stat ${rb} ${rp} ${tb} ${tp} >`);
    }, Math.max(100, ival)) : null;
    return;
  }
  scWrite(s, "< error unknown command >");
}

function scCleanup(s: TcpSocket<ScClient>) {
  const c = s.data;
  if (!scClients.delete(s)) return;
  c.sock?.close();
  for (const j of c.jobs.values()) clearInterval(j.timer);
  if (c.statTimer) clearInterval(c.statTimer);
  ctx.changed();
}

function startSocketcand() {
  stopSocketcand();
  if (!ctx.settings.get("socketcand", true)) return;
  const port = scPort();
  try {
    scServer = Bun.listen<ScClient>({
      hostname: "0.0.0.0",
      port,
      socket: {
        open(s) {
          s.data = { n: scSeq++, peer: `${s.remoteAddress}`, mode: "nobus", iface: "", sock: null, buf: "", pending: "", holdUntil: 0, held: "", subs: new Map(), jobs: new Map(), statTimer: null, rx: 0, tx: 0, dropped: 0, since: Date.now() };
          s.setNoDelay?.(true);
          scClients.add(s);
          scWrite(s, "< hi >");
          ctx.changed();
        },
        async data(s, chunk) {
          const c = s.data;
          const { cmds, rest } = C.scTake(c.buf + chunk.toString("latin1"));
          c.buf = rest;
          for (const cmd of cmds) {
            try {
              await scCommand(s, cmd);
            } catch (e: any) {
              ctx.log(`socketcand ${c.peer}: ${cmd}: ${e.message}`);
            }
          }
        },
        drain(s) {
          const c = s.data;
          if (!c.pending) return;
          const p = c.pending;
          c.pending = "";
          const n = s.write(p);
          if (n < p.length) c.pending = p.slice(Math.max(0, n));
        },
        close: scCleanup,
        error(s, e) {
          ctx.log(`socketcand ${s.data?.peer}: ${e?.message || e}`);
          scCleanup(s);
        },
      },
    });
    scError = "";
  } catch (e: any) {
    scServer = null;
    scError = `port ${port}: ${e.code === "EADDRINUSE" ? "already in use" : e.message}`;
    ctx.log("socketcand:", scError);
  }
  ctx.changed();
}

function stopSocketcand() {
  for (const s of [...scClients]) {
    s.end();
    scCleanup(s);
  }
  scServer?.stop(true);
  scServer = null;
}

// ---------- socketcand discovery beacon (UDP 42000) ----------

let beaconTimer: ReturnType<typeof setInterval> | null = null;
let beaconSock: Awaited<ReturnType<typeof Bun.udpSocket>> | null = null;
let beaconTo = "";

async function beaconTarget(): Promise<{ ip: string; bcast: string } | null> {
  try {
    const dev = JSON.parse((await run([IP, "-j", "route", "get", "1.1.1.1"])).out)[0];
    const addr = JSON.parse((await run([IP, "-j", "-4", "addr", "show", "dev", dev.dev])).out)[0]?.addr_info?.find((a: any) => a.family === "inet");
    if (!addr) return null;
    return { ip: addr.local, bcast: addr.broadcast || "255.255.255.255" };
  } catch {
    return null;
  }
}

async function startBeacon() {
  stopBeacon();
  if (!ctx.settings.get("beacon", false)) return;
  beaconSock = await Bun.udpSocket({});
  beaconSock.setBroadcast(true);
  let target = await beaconTarget();
  let n = 0;
  const send = async () => {
    if (++n % 20 === 0) target = (await beaconTarget()) || target; // follow DHCP changes
    if (!target || !scServer || !beaconSock) return;
    beaconTo = `${target.bcast}:${BEACON_PORT}`;
    beaconSock.send(C.beaconXml(hostname(), target.ip, scPort(), [...ifaces.keys()]), BEACON_PORT, target.bcast);
  };
  send();
  beaconTimer = setInterval(send, 3000); // socketcand's interval; python-can waits 3.1 s
}

function stopBeacon() {
  if (beaconTimer) clearInterval(beaconTimer);
  beaconTimer = null;
  beaconSock?.close();
  beaconSock = null;
  beaconTo = "";
}

// ---------- Output shapes ----------

function ifaceOut(i: Iface) {
  const hub = hubs.get(i.name);
  const cfg = savedCfg(i.key) || {};
  return {
    name: i.name,
    kind: i.kind,
    driver: i.driver,
    label: i.label,
    key: i.key,
    usb: i.usb,
    slcanTty: i.slcanTty,
    up: i.up,
    state: i.state,
    berr: i.berr,
    bitrate: i.bitrate,
    samplePoint: i.samplePoint,
    dbitrate: i.dbitrate,
    dsamplePoint: i.dsamplePoint,
    fdCapable: i.fdCapable,
    fdOn: i.fdOn,
    ctrl: i.ctrl,
    ctrlSupported: i.ctrlSupported,
    restartMs: i.restartMs,
    clock: i.clock,
    bitrateConst: i.bitrateConst,
    xstats: i.xstats,
    stats: i.stats,
    bringUpOnPlug: !!cfg.bringUpOnPlug,
    saved: cfg,
    busy: busy.get(i.name) || null,
    autobitrate: abProgress.get(i.name) || null,
    lastAutobitrate: abLast.get(i.name) || null,
    reader: hub?.sock ? "open" : hub?.openErr || (i.up ? "starting" : "down"),
    rates: hub?.rates,
    totals: hub?.tot,
    drops: hub?.sock?.drops || 0,
    idCount: hub?.ids.size || 0,
    jobs: hub ? [...hub.jobs.values()].map(jobOut) : [],
    recording: hub?.rec ? { file: hub.rec.file, frames: hub.rec.frames, bytes: hub.rec.bytes, since: hub.rec.since } : null,
    wsClients: hub?.clients.size || 0,
  };
}

function socketcandOut() {
  return {
    enabled: ctx.settings.get("socketcand", true),
    port: scPort(),
    listening: !!scServer,
    error: scError,
    errMask: ctx.settings.get("socketcandErrMask", 0),
    beacon: ctx.settings.get("beacon", false),
    beaconTo,
    clients: [...scClients].map((s) => ({ n: s.data.n, peer: s.data.peer, iface: s.data.iface, mode: s.data.mode, rx: s.data.rx, tx: s.data.tx, dropped: s.data.dropped, since: s.data.since })),
  };
}

function list() {
  return {
    ifaces: [...ifaces.values()].map(ifaceOut),
    slcan: { candidates: slcanCandidates(), attached: [...slcans.values()].map((s) => ({ name: s.name, tty: s.tty, bitrate: s.bitrate, since: s.since })) },
    socketcand: socketcandOut(),
    recordings,
  };
}

// ---------- WebSocket ----------

const WS_DOC =
  "Live traffic for one interface. Query: filter=123,7E0:7F8,x18DAF100:1FFFFF00 (hex id[:mask], x = extended); batch=1 to get {type:'frames',frames:[…],dropped} every 50 ms instead of one {type:'frame',…} message per frame; stats=1 for a {type:'stats',…} snapshot (rates, controller state, per-ID table, errors) at 4 Hz; frames=0 for stats only. Frame: {id, ext, rtr, fd, brs, esi?, err?, len, data (hex), ts (Unix s, µs), dir ('rx'|'tx'), text? (decoded error)}. Send {send:{id:'123'|291, ext?, rtr?, fd?, brs?, data:'DEADBEEF'}} or {send:{cansend:'123#DEADBEEF'}}, {filter:'…'|[{id,mask?,ext?}]|null}, {frames:false|true}, {clear:true}.";

function wsOpen(ws: ServerWebSocket<WsData>) {
  const name = ws.data.params.iface;
  const hub = hubs.get(name);
  if (!hub || !ifaces.has(name)) {
    ws.send(JSON.stringify({ type: "error", message: `No CAN interface "${name}"` }));
    ws.close();
    return;
  }
  const q = ws.data.query;
  let filters: C.IdFilter[] | null = null;
  try {
    filters = C.parseFilterString(q.get("filter"));
  } catch (e: any) {
    ws.send(JSON.stringify({ type: "error", message: e.message }));
  }
  const st: WsState = { iface: name, filters, frames: q.get("frames") !== "0", batch: q.get("batch") === "1", stats: q.get("stats") === "1", q: [], dropped: 0 };
  ws.data.state = st;
  hub.clients.add(ws);
  ws.send(JSON.stringify({ type: "hello", iface: ifaceOut(ifaces.get(name)!), recent: hub.ring.slice(-200).filter((f) => C.matchFilters({ id: f.id, ext: f.ext, err: !!f.err }, filters)) }));
  if (st.stats) ws.send(JSON.stringify({ type: "stats", ...hub.snapshot(true) }));
}

function wsMessage(ws: ServerWebSocket<WsData>, msg: string | Buffer) {
  const st = ws.data.state as WsState;
  const hub = hubs.get(st.iface);
  if (!hub) return;
  let m: any;
  try {
    m = JSON.parse(String(msg));
  } catch {
    throw new Error("Messages must be JSON");
  }
  if (m.send) {
    try {
      hub.send(frameFrom(m.send));
    } catch (e: any) {
      ws.send(JSON.stringify({ type: "error", message: e.message }));
    }
  }
  if ("filter" in m) {
    try {
      st.filters = typeof m.filter === "string" ? C.parseFilterString(m.filter) : C.parseFilters(m.filter);
    } catch (e: any) {
      ws.send(JSON.stringify({ type: "error", message: e.message }));
    }
  }
  if ("frames" in m) st.frames = !!m.frames;
  if ("stats" in m) st.stats = !!m.stats;
  if (m.clear) hub.clear();
}

function wsClose(ws: ServerWebSocket<WsData>) {
  const st = ws.data.state as WsState;
  hubs.get(st?.iface)?.clients.delete(ws);
}

// ---------- Timers ----------

let tickTimer: ReturnType<typeof setInterval> | null = null;
let tickN = 0;

function tick() {
  tickN++;
  // 50 ms: flush batched frames.
  for (const hub of hubs.values()) {
    for (const ws of hub.clients) {
      const st = ws.data.state as WsState;
      if (!st.batch || (!st.q.length && !st.dropped)) continue;
      if (ws.getBufferedAmount() > 2 << 20) {
        st.dropped += st.q.length; // a slow browser: drop rather than buffer without bound
        st.q = [];
        continue;
      }
      ws.send(JSON.stringify({ type: "frames", frames: st.q, dropped: st.dropped }));
      st.q = [];
      st.dropped = 0;
    }
  }
  // 250 ms: stats snapshots.
  if (tickN % 5 === 0) {
    for (const hub of hubs.values()) {
      let snap: string | null = null;
      for (const ws of hub.clients) {
        const st = ws.data.state as WsState;
        if (!st.stats) continue;
        snap ??= JSON.stringify({ type: "stats", ...hub.snapshot(true) });
        ws.send(snap);
      }
    }
  }
  // 1 s: rates; poll `ip` for controller state and error counters while a real controller is
  // up. vcan has neither, so otherwise every 5 s is enough to notice links changed from a shell.
  if (tickN % 20 === 0) {
    for (const hub of hubs.values()) hub.tick();
    const ctrlUp = [...ifaces.values()].some((i) => i.up && i.kind === "can");
    if (ctrlUp || tickN % 100 === 0) refresh();
  }
}

// ---------- Driver ----------

function iface(params: Record<string, string>): Iface {
  const i = ifaces.get(params.iface);
  if (!i) throw new HttpError(404, `No CAN interface "${params.iface}"`);
  return i;
}

const FRAME_SCHEMA = {
  type: "object",
  properties: {
    id: { type: ["integer", "string"], description: "Arbitration id: a number, or a hex string ('0x123' or '123')" },
    ext: { type: "boolean", description: "29-bit extended id (implied when id > 0x7FF)" },
    rtr: { type: "boolean", description: "Remote transmission request (classic CAN only)" },
    len: { type: "integer", description: "RTR request length (0–8)" },
    fd: { type: "boolean", description: "CAN FD frame (interface must be in FD mode)" },
    brs: { type: "boolean", description: "CAN FD bit-rate switch" },
    data: { type: "string", description: "Payload as hex, e.g. 'DEADBEEF' or 'DE AD BE EF'" },
    cansend: { type: "string", description: "Alternative to the above: cansend syntax, e.g. '123#DEADBEEF', '18DAF110#R', '123##1DEADBEEF'" },
  },
};

const driver: Driver = {
  id: "can",
  title: "CAN bus",
  order: 30,
  description: "SocketCAN adapters (candleLight/CANable, PEAK, SLCAN, vcan): bitrate config and auto-detect, live traffic, recording, socketcand server for python-can/SavvyCAN, MCP tools",

  async init(c) {
    ctx = c;
    initialScan = true;
    await refresh();
    initialScan = false;
    refreshRecordings();
    startSocketcand();
    startBeacon();
    tickTimer = setInterval(tick, 50);
  },

  async stop() {
    if (tickTimer) clearInterval(tickTimer);
    tickTimer = null;
    stopBeacon();
    stopSocketcand();
    for (const hub of hubs.values()) {
      for (const j of hub.jobs.values()) clearInterval(j.timer);
      hub.jobs.clear();
      hub.stopRecording("driver stopped");
      hub.closeSock();
      for (const ws of hub.clients) ws.close();
    }
    for (const s of slcans.values()) s.proc.kill("SIGTERM");
    slcans.clear();
    hubs.clear();
    ifaces.clear();
  },

  udev(ev: UdevEvent) {
    const p = ev.props;
    if (p.SUBSYSTEM === "net" && (ev.action === "add" || ev.action === "remove" || ev.action === "move")) setTimeout(() => refresh(), 200);
    else if (p.SUBSYSTEM === "tty" && SLCAN_USB[`${p.ID_VENDOR_ID}:${p.ID_MODEL_ID}`]) {
      if (ev.action === "add") ctx.event(`SLCAN adapter on ${p.DEVNAME} (${SLCAN_USB[`${p.ID_VENDOR_ID}:${p.ID_MODEL_ID}`]})`);
      ctx.changed();
    }
  },

  status() {
    const list = [...ifaces.values()];
    const cands = slcanCandidates();
    const sc = socketcandOut();
    const up = list.filter((i) => i.up);
    const busiest = up.map((i) => hubs.get(i.name)?.rates.fps || 0).reduce((a, b) => a + b, 0);
    const summary = !list.length
      ? cands.length
        ? `${cands.length} SLCAN adapter${cands.length > 1 ? "s" : ""} (not attached)`
        : "No CAN interfaces"
      : list.map((i) => `${i.name} ${i.up ? (i.bitrate ? C.fmtBitrate(i.bitrate) : "up") : "down"}${i.state && i.state !== "ERROR-ACTIVE" ? " " + i.state.toLowerCase() : ""}`).join(", ") + (busiest ? ` · ${busiest} fps` : "") + (sc.clients.length ? ` · ${sc.clients.length} socketcand` : "");
    return {
      active: list.length > 0 || cands.length > 0 || sc.clients.length > 0,
      summary,
      ifaces: list.map(ifaceOut),
      slcan: { candidates: cands, attached: [...slcans.values()].map((s) => ({ name: s.name, tty: s.tty, bitrate: s.bitrate })) },
      socketcand: sc,
      recordings: recordings.slice(0, 20),
      recDir: REC_DIR,
      standardBitrates: C.STD_BITRATES,
      slcanBitrates: Object.keys(SLCAN_SPEED).map(Number),
    };
  },

  routes: [
    { method: "GET", path: "/api/can", doc: "CAN interfaces (bitrate, controller state, error counters, rates), SLCAN adapters, socketcand server, recordings", handler: () => list() },
    {
      method: "POST",
      path: "/api/can/vcan",
      doc: { body: { name: "vcan0 (optional)", fd: true }, description: "Create a virtual CAN interface for testing (FD MTU by default) and bring it up" },
      handler: async (req) => {
        const b = await body(req);
        return createVcan(b.name, b.fd !== false);
      },
    },
    { method: "DELETE", path: "/api/can/vcan/:name", doc: "Remove a virtual CAN interface", handler: (_r, p) => removeVcan(p.name) },
    {
      method: "POST",
      path: "/api/can/slcan",
      doc: { body: { tty: "/dev/ttyACM0", bitrate: 500000, name: "slcan0 (optional)", serialBaud: 3000000 }, description: "Attach a serial SLCAN adapter as a SocketCAN interface (runs slcand). While attached, the serial port is busy and the serial panel can't open it." },
      handler: async (req) => {
        const b = await body(req);
        return attachSlcan(b.tty, b);
      },
    },
    { method: "DELETE", path: "/api/can/slcan/:name", doc: "Detach an SLCAN adapter (by interface name or tty), freeing the serial port", handler: (_r, p) => detachSlcan(p.name) },
    {
      method: "POST",
      path: "/api/can/socketcand",
      doc: { body: { enabled: true, port: 29536, beacon: false, errMask: 0 }, description: "socketcand-protocol TCP server settings (python-can interface='socketcand', SavvyCAN, Kayak) and its UDP discovery beacon on port 42000" },
      handler: async (req) => {
        const b = await body(req);
        if (b.port != null) {
          const p = Number(b.port);
          if (!(p >= 1024 && p <= 65535)) throw new HttpError(400, "port must be 1024–65535");
          ctx.settings.set("socketcandPort", p);
        }
        if (b.enabled != null) ctx.settings.set("socketcand", !!b.enabled);
        if (b.beacon != null) ctx.settings.set("beacon", !!b.beacon);
        if (b.errMask != null) ctx.settings.set("socketcandErrMask", Number(b.errMask) >>> 0);
        startSocketcand();
        await startBeacon();
        return socketcandOut();
      },
    },
    { method: "GET", path: "/api/can/recordings", doc: `candump -L format recordings in ${REC_DIR}`, handler: () => (refreshRecordings(), recordings) },
    {
      method: "GET",
      path: "/api/can/recordings/:file",
      doc: "Download a recording (candump -L format: (ts) can0 123#DEADBEEF; replay with canplayer -I file)",
      handler: (_r, p) => new Response(Bun.file(recPath(p.file)), { headers: { "Content-Type": "text/plain; charset=utf-8", "Content-Disposition": `attachment; filename="${p.file}"` } }),
    },
    {
      method: "DELETE",
      path: "/api/can/recordings/:file",
      doc: "Delete a recording",
      handler: (_r, p) => {
        if ([...hubs.values()].some((h) => h.rec?.file === p.file)) throw new HttpError(409, "Stop the recording first");
        unlinkSync(recPath(p.file));
        refreshRecordings();
        return { ok: true };
      },
    },
    {
      method: "GET",
      path: "/api/can/:iface",
      doc: "One interface: config, capabilities (FD, clock, ctrlmodes), state, counters, per-ID table, last error frames, periodic jobs",
      handler: (_r, p) => ({ ...ifaceOut(iface(p)), ...hubFor(p.iface).snapshot(true) }),
    },
    {
      method: "POST",
      path: "/api/can/:iface/config",
      doc: {
        body: { bitrate: 500000, samplePoint: 0.875, fd: false, dbitrate: 2000000, dsamplePoint: 0.75, listenOnly: false, loopback: false, tripleSampling: false, oneShot: false, berrReporting: false, restartMs: 100, bringUpOnPlug: true, up: true },
        description: "Configure with ip link (down, set, up). Every field is optional. Saved per adapter (USB serial/path) and re-applied on hotplug when bringUpOnPlug is set. vcan only takes fd (MTU) and up.",
      },
      handler: async (req, p) => configure(iface(p).name, await body(req)),
    },
    { method: "POST", path: "/api/can/:iface/restart", doc: "Restart the controller after bus-off (ip link set … type can restart)", handler: (_r, p) => restartBus(iface(p).name) },
    {
      method: "POST",
      path: "/api/can/:iface/autobitrate",
      doc: { body: { rates: C.STD_BITRATES, listenMs: 400, apply: false }, description: "Detect the bus bitrate: tries each rate in listen-only mode (never ACKs, safe on a live bus), counting valid vs error frames. Restores the old config unless apply is true. Takes ~0.6 s per rate." },
      handler: async (req, p) => autobitrate(iface(p).name, await body(req)),
    },
    {
      method: "POST",
      path: "/api/can/:iface/send",
      doc: { body: { id: "123", ext: false, rtr: false, fd: false, brs: false, data: "DEADBEEF" }, description: "Send one frame. id may be a number or hex string; or pass {cansend:'123#DEADBEEF'}" },
      handler: async (req, p) => {
        const f = frameFrom(await body(req));
        hubFor(iface(p).name).send(f);
        return { ok: true, sent: C.candumpBody(f) };
      },
    },
    {
      method: "GET",
      path: "/api/can/:iface/capture",
      doc: "Collect frames for ms (default 1000, max 60000), optionally filtered: ?ms=2000&filter=7E8,x18DAF100:1FFFFF00&max=1000",
      handler: (_r, p, url) => {
        let filters: C.IdFilter[] | null;
        try {
          filters = C.parseFilterString(url.searchParams.get("filter"));
        } catch (e: any) {
          throw new HttpError(400, e.message);
        }
        return capture(iface(p).name, Number(url.searchParams.get("ms")) || 1000, filters, Number(url.searchParams.get("max")) || 1000);
      },
    },
    {
      method: "POST",
      path: "/api/can/:iface/periodic",
      doc: { body: { periodMs: 100, id: "123", data: "0011223344556677" }, description: `Start a periodic transmit job (min ${MIN_PERIOD_MS} ms; timer based, ~1 ms jitter). Frame fields as for /send.` },
      handler: async (req, p) => {
        const b = await body(req);
        return jobOut(startJob(hubFor(iface(p).name), frameFrom(b), Number(b.periodMs)));
      },
    },
    {
      method: "DELETE",
      path: "/api/can/:iface/periodic/:job",
      doc: "Stop a periodic transmit job",
      handler: (_r, p) => {
        if (!stopJob(hubFor(p.iface), Number(p.job))) throw new HttpError(404, `No job ${p.job}`);
        return { ok: true };
      },
    },
    {
      method: "POST",
      path: "/api/can/:iface/record",
      doc: { body: { on: true }, description: "Start or stop recording everything on the interface (incl. error frames) to a candump -L log" },
      handler: async (req, p) => {
        const hub = hubFor(iface(p).name);
        const { on } = await body(req);
        if (on) return { ok: true, file: hub.startRecording() };
        const r = hub.stopRecording();
        if (!r) throw new HttpError(409, "Not recording");
        return { ok: true, ...r };
      },
    },
    { method: "POST", path: "/api/can/:iface/clear", doc: "Reset the per-ID table, counters and frame log", handler: (_r, p) => (hubFor(iface(p).name).clear(), { ok: true }) },
  ],

  sockets: [{ path: "/ws/can/:iface", doc: WS_DOC, open: wsOpen, message: wsMessage, close: wsClose }],

  tools: [
    {
      name: "can_list",
      description: "List CAN interfaces on DeskyPi with bitrate, FD mode, controller state (error-active/warning/passive/bus-off), error counters, frame rates and bus load; plus attachable SLCAN serial adapters and the socketcand server address.",
      inputSchema: { type: "object", properties: {} },
      handler: () => list(),
    },
    {
      name: "can_configure",
      description: "Configure a CAN interface (applies with ip link: down, set, up). Set bitrate/samplePoint, CAN FD data bitrate, listen-only, loopback, triple-sampling, restart-ms (bus-off auto-restart), or bring it up/down. Settings persist per adapter. Pass restart:true to restart a controller stuck in bus-off.",
      inputSchema: {
        type: "object",
        properties: {
          iface: { type: "string", description: "Interface name, e.g. can0" },
          bitrate: { type: "integer", description: "Nominal bitrate in bit/s, e.g. 500000" },
          samplePoint: { type: "number", description: "Sample point, 0–1 (e.g. 0.875)" },
          fd: { type: "boolean", description: "Enable CAN FD (needs dbitrate)" },
          dbitrate: { type: "integer", description: "CAN FD data-phase bitrate, e.g. 2000000" },
          dsamplePoint: { type: "number" },
          listenOnly: { type: "boolean", description: "Never transmit or ACK" },
          loopback: { type: "boolean", description: "Controller internal loopback (testing without a bus)" },
          tripleSampling: { type: "boolean" },
          restartMs: { type: "integer", description: "Auto-restart delay after bus-off in ms; 0 disables" },
          bringUpOnPlug: { type: "boolean", description: "Apply this config and bring the interface up whenever the adapter is plugged in" },
          up: { type: "boolean", description: "Bring the interface up (true) or down (false)" },
          restart: { type: "boolean", description: "Restart the controller (bus-off recovery) instead of configuring" },
        },
        required: ["iface"],
      },
      handler: (a) => (a.restart ? restartBus(a.iface) : configure(a.iface, a)),
    },
    {
      name: "can_autobitrate",
      description: "Detect a CAN bus's bitrate safely: tries standard rates (1M…10k) in listen-only mode (the controller never ACKs or transmits), counting valid vs error frames for listenMs each. Returns the winner, 'silent' (no traffic) or 'none' (errors at every rate). Restores the previous config unless apply is true.",
      inputSchema: {
        type: "object",
        properties: {
          iface: { type: "string" },
          rates: { type: "array", items: { type: "integer" }, description: "Bitrates to try (default: standard list)" },
          listenMs: { type: "integer", description: "Listen time per rate, default 400" },
          apply: { type: "boolean", description: "Configure the detected bitrate (normal mode) when found" },
        },
        required: ["iface"],
      },
      handler: (a) => autobitrate(a.iface, a),
    },
    {
      name: "can_send",
      description: "Send a CAN frame, or start/stop a periodic one. Give id + data (hex), or cansend syntax ('123#DEADBEEF', '18DAF110#R', '123##1DEADBEEF' for FD+BRS). With periodMs, starts a periodic job and returns its id; stopJob stops one.",
      inputSchema: {
        type: "object",
        properties: { iface: { type: "string" }, ...FRAME_SCHEMA.properties, periodMs: { type: "integer", description: `Repeat every N ms (min ${MIN_PERIOD_MS})` }, stopJob: { type: "integer", description: "Stop the periodic job with this id" } },
        required: ["iface"],
      },
      handler: (a) => {
        const hub = hubFor(a.iface);
        if (a.stopJob != null) return { ok: stopJob(hub, Number(a.stopJob)) };
        const f = frameFrom(a);
        if (a.periodMs) return jobOut(startJob(hub, f, Number(a.periodMs)));
        hub.send(f);
        return { ok: true, sent: C.candumpBody(f) };
      },
    },
    {
      name: "can_capture",
      description: "Capture CAN frames for a time window and return them (id, ext, rtr, fd, data hex, ts in Unix seconds, dir rx/tx; error frames include a decoded text). Optional id filter with mask, e.g. id '7E8' mask '7F8' for OBD responses.",
      inputSchema: {
        type: "object",
        properties: {
          iface: { type: "string" },
          ms: { type: "integer", description: "Window length, default 1000, max 60000" },
          id: { type: ["integer", "string"], description: "Only frames matching this id (number or hex)" },
          mask: { type: ["integer", "string"], description: "Bits of id that must match (default all)" },
          ext: { type: "boolean", description: "Only extended (true) or standard (false) ids" },
          filter: { type: "string", description: "Instead of id/mask: several, e.g. '123,7E0:7F8,x18DAF100:1FFFFF00'" },
          max: { type: "integer", description: "Stop after this many frames (default 1000)" },
        },
        required: ["iface"],
      },
      handler: (a) => {
        const filters = a.filter ? C.parseFilterString(a.filter) : a.id != null ? C.parseFilters({ id: a.id, mask: a.mask, ext: a.ext }) : null;
        return capture(a.iface, a.ms, filters, a.max);
      },
    },
    {
      name: "can_stats",
      description: "Traffic statistics for a CAN interface: frames/s, TX/s, errors/s, bus load %, totals, controller state and error counters, the per-ID table (count, period, last data) and recent decoded error frames.",
      inputSchema: { type: "object", properties: { iface: { type: "string" }, limit: { type: "integer", description: "Max ID rows (default 100)" } }, required: ["iface"] },
      handler: (a) => {
        const hub = hubFor(a.iface);
        const snap = hub.snapshot(true);
        const limit = Math.max(1, Number(a.limit) || 100);
        return {
          ...snap,
          ids: hub.idRows(limit).map((r) => ({ ...r, id: C.fmtId(r.id, r.ext), periodMs: r.period, hot: undefined, last: new Date(r.last).toISOString() })),
          idsTruncated: hub.ids.size > limit,
          interface: ifaceOut(ifaces.get(a.iface)!),
        };
      },
    },
    {
      name: "can_vcan",
      description: "Create or remove a virtual CAN interface (vcan) for testing without hardware. Frames sent on it are seen by every listener on the same vcan.",
      inputSchema: { type: "object", properties: { action: { type: "string", enum: ["create", "remove"] }, name: { type: "string", description: "e.g. vcan0 (default: next free)" }, fd: { type: "boolean", description: "CAN FD MTU (default true)" } }, required: ["action"] },
      handler: (a) => (a.action === "remove" ? removeVcan(a.name) : createVcan(a.name, a.fd !== false)),
    },
  ],
};

export default driver;
