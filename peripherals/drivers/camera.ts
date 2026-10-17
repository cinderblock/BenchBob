// ==================== Camera ====================
//
// USB webcams and HDMI capture sticks (anything UVC). Each camera gets a `ustreamer` child,
// started only when someone looks (the dashboard's live view, a snapshot, an agent) and
// stopped once nobody has for a minute, so an idle camera costs nothing and its LED stays
// off. The MJPEG stream and snapshots are proxied through DeskyPi's own port; a per-camera
// "share directly" switch instead binds ustreamer to the LAN for tools that want it raw.
//
// CSI cameras (libcamera) are listed when present, but not streamed: ustreamer can't drive
// them and a Pi used as a bench server rarely has one fitted.
//
// Testing without hardware: DESKYPI_CAMERA_SIMULATE=/dev/video99 (any path) adds a fake
// MJPEG camera "sim"; ustreamer can't open it and serves its NO SIGNAL placeholder frame,
// which exercises the start-on-demand, proxy, snapshot and idle-stop paths.

import { hostname } from "node:os";
import type { Ctx, Driver, Props, UdevEvent } from "../core.ts";
import { HttpError, body, mcpContent, run, usbName } from "../core.ts";
import {
  cameraKey, cameraKind, capsCanCapture, chooseStream, isUsbCaptureNode, isUserControl,
  parseControls, parseDeviceCaps, parseFormats, parseLibcameraList, ustreamerFormat,
  type Control, type Format, type StreamChoice,
} from "../lib/camera.ts";

const IDLE_STOP_MS = 60_000;
const PORT_OFFSET = parseInt(process.env.DESKYPI_PORT_OFFSET || "") || 0;
const SHARE_PORT_BASE = 8080; // ustreamer's own default, so tools' examples work unchanged
const RUN_DIR = `/tmp/deskypi-camera-${process.env.PORT || 80}`;
const SIM = process.env.DESKYPI_CAMERA_SIMULATE || "";

interface Streamer {
  proc: ReturnType<Ctx["spawn"]>;
  started: number;
  /** Where ustreamer listens: a private unix socket, or TCP when shared. */
  unix?: string;
  port?: number;
  shared: boolean;
  ready: Promise<void>;
  stderr: string[];
  clients: number; // from ustreamer's /state: proxied viewers + direct ones
  fps: number;
  online: boolean;
  stopping?: boolean;
}

interface Cam {
  id: string; // "video0" — the /dev name; what URLs and tools use
  key: string; // stable settings key (serial, else USB port path)
  dev: string;
  devpath: string;
  name: string;
  vendor: string;
  vid: string;
  pid: string;
  serial: string;
  kind: "hdmi" | "webcam";
  usbPort: string; // "1-1.3"
  driver: string;
  formats: Format[];
  controls: Control[];
  stream: StreamChoice | null;
  streamer: Streamer | null;
  proxied: number; // streams currently piped through DeskyPi
  lastUse: number;
  error: string;
  last?: { jpeg: Uint8Array; t: number };
}

interface CamSettings {
  stream?: StreamChoice;
  controls?: Record<string, number>;
  share?: boolean;
  sharePort?: number;
}

let ctx: Ctx;
const cams = new Map<string, Cam>();
let csi: { index: number; sensor: string; detail: string }[] = [];
let rescanTimer: ReturnType<typeof setTimeout> | null = null;
let idleTimer: ReturnType<typeof setInterval> | null = null;

const getCfg = (c: Cam): CamSettings => ctx.settings.get<Record<string, CamSettings>>("cameras", {})[c.key] || {};
function setCfg(c: Cam, patch: Partial<CamSettings>) {
  const all = { ...ctx.settings.get<Record<string, CamSettings>>("cameras", {}) };
  all[c.key] = { ...all[c.key], ...patch };
  ctx.settings.set("cameras", all);
}

// ---------- Discovery ----------

function usbParent(p: Props) {
  return ctx.ancestor(p.DEVPATH, "usb", "usb_device");
}

async function probe(p: Props): Promise<Cam | null> {
  const dev = p.DEVNAME;
  // Older udev marks a UVC camera's metadata node `:capture:` too; the node's own Device Caps settle it.
  const caps = parseDeviceCaps((await run(["v4l2-ctl", "-d", dev, "-D"], { timeoutMs: 5000 })).out);
  if (!capsCanCapture(caps.caps)) return null;
  const usb = usbParent(p)!;
  const { vendor, model } = usbName(usb);
  const vid = usb.ID_VENDOR_ID || p.ID_VENDOR_ID || "";
  const pid = usb.ID_MODEL_ID || p.ID_MODEL_ID || "";
  const name = caps.card || model || p.ID_V4L_PRODUCT || dev;
  const c: Cam = {
    id: dev.slice(5),
    key: cameraKey({ ...usb, ...p }),
    dev,
    devpath: p.DEVPATH,
    name,
    vendor,
    vid,
    pid,
    serial: p.ID_SERIAL_SHORT || usb.ID_SERIAL_SHORT || "",
    kind: cameraKind(vid, pid, `${name} ${model}`),
    usbPort: usb.DEVPATH.split("/").pop() || "",
    driver: caps.driver,
    formats: [],
    controls: [],
    stream: null,
    streamer: null,
    proxied: 0,
    lastUse: 0,
    error: "",
  };
  c.formats = parseFormats((await run(["v4l2-ctl", "-d", dev, "--list-formats-ext"], { timeoutMs: 5000 })).out);
  c.stream = chooseStream(c.formats, getCfg(c).stream);
  await applySavedControls(c);
  return c;
}

async function readControls(c: Cam) {
  c.controls = parseControls((await run(["v4l2-ctl", "-d", c.dev, "--list-ctrls-menus"], { timeoutMs: 5000 })).out);
}

/** Cameras forget their controls on every replug; put back what the user chose. */
async function applySavedControls(c: Cam) {
  const saved = getCfg(c).controls || {};
  await readControls(c);
  // Auto modes first: an absolute exposure or temperature is refused while its auto switch is on.
  const names = Object.keys(saved).filter((n) => c.controls.some((x) => x.name === n)).sort((a, b) => +/auto/.test(b) - +/auto/.test(a));
  for (const n of names) await run(["v4l2-ctl", "-d", c.dev, "-c", `${n}=${saved[n]}`], { timeoutMs: 3000 });
  if (names.length) await readControls(c);
}

async function rescan() {
  const nodes = ctx.devices((p) => p.SUBSYSTEM === "video4linux" && isUsbCaptureNode(p, usbParent(p)));
  const seen = new Set<string>();
  for (const p of nodes) {
    const id = p.DEVNAME.slice(5);
    seen.add(id);
    if (cams.has(id) && cams.get(id)!.devpath === p.DEVPATH) continue;
    try {
      const c = await probe(p);
      if (!c) continue;
      cams.set(id, c);
      ctx.event(`Camera connected: ${c.name} (${c.dev}${c.kind === "hdmi" ? ", HDMI capture" : ""})`, { id, vid: c.vid, pid: c.pid });
      if (getCfg(c).share) ensureStreamer(c).catch((e) => (c.error = e.message));
    } catch (e: any) {
      ctx.log(`probe ${p.DEVNAME} failed:`, e.message);
    }
  }
  if (SIM) {
    seen.add("sim");
    if (!cams.has("sim")) {
      const formats = [{ fourcc: "MJPG", description: "Motion-JPEG, compressed", compressed: true, sizes: [{ w: 640, h: 480, fps: [30, 15] }] }];
      cams.set("sim", {
        id: "sim", key: "sim", dev: SIM, devpath: "", name: "Simulated camera", vendor: "DeskyPi", vid: "0000", pid: "0000", serial: "", kind: "webcam",
        usbPort: "-", driver: "simulated", formats, controls: [], stream: chooseStream(formats), streamer: null, proxied: 0, lastUse: 0, error: "",
      });
    }
  }
  for (const [id, c] of cams) {
    if (seen.has(id)) continue;
    stopStreamer(c);
    cams.delete(id);
    ctx.event(`Camera disconnected: ${c.name} (${c.dev})`, { id });
  }
  ctx.changed();
}

function scheduleRescan() {
  if (rescanTimer) clearTimeout(rescanTimer);
  // A UVC camera arrives as several udev events (usb device, interfaces, two video nodes).
  rescanTimer = setTimeout(() => rescan().catch((e) => ctx.log("rescan:", e)), 700);
}

// ---------- ustreamer ----------

async function upstream(s: Streamer, path: string, init?: RequestInit): Promise<Response> {
  if (s.unix) return fetch(`http://localhost${path}`, { ...init, unix: s.unix } as any);
  return fetch(`http://127.0.0.1:${s.port}${path}`, init);
}

function sharePort(c: Cam): number {
  const p = getCfg(c).sharePort;
  if (p) return p;
  const used = new Set([...cams.values()].map((x) => getCfg(x).sharePort).filter(Boolean));
  let port = SHARE_PORT_BASE + PORT_OFFSET;
  while (used.has(port)) port++;
  return port;
}

function ensureStreamer(c: Cam): Promise<void> {
  c.lastUse = Date.now();
  if (c.streamer) return c.streamer.ready;
  if (!c.stream) return Promise.reject(new HttpError(409, `${c.name} offers no format ustreamer can stream`));
  const cfg = getCfg(c);
  const shared = !!cfg.share;
  const fmt = ustreamerFormat(c.stream.fourcc)!;
  const compressed = fmt === "MJPEG" || fmt === "JPEG";
  // MJPEG from the camera is forwarded as-is. Raw frames go to the Pi's JPEG block
  // (/dev/video31) when it's there; the CPU encoder is the fallback.
  const encoder = compressed ? "HW" : ctx.devices((p) => p.DEVNAME === "/dev/video31").length ? "M2M-IMAGE" : "CPU";
  const cmd = [
    "ustreamer", "--device", c.dev, "--format", fmt, "--resolution", `${c.stream.w}x${c.stream.h}`,
    "--desired-fps", String(c.stream.fps), "--encoder", encoder, "--exit-on-parent-death",
    "--process-name-prefix", `deskypi-${c.id}`, "--no-log-colors",
  ];
  const s: Streamer = { proc: null as any, started: Date.now(), shared, ready: null as any, stderr: [], clients: 0, fps: 0, online: false };
  if (shared) {
    s.port = sharePort(c);
    if (!cfg.sharePort) setCfg(c, { sharePort: s.port });
    cmd.push("--host", "0.0.0.0", "--port", String(s.port));
  } else {
    s.unix = `${RUN_DIR}/${c.id}.sock`;
    cmd.push("--unix", s.unix, "--unix-rm", "--unix-mode", "660");
  }
  c.error = "";
  s.proc = ctx.spawn(cmd, { stdout: "ignore", stderr: "pipe" });
  c.streamer = s;
  (async () => {
    const dec = new TextDecoder();
    for await (const chunk of s.proc.stderr as ReadableStream<Uint8Array>) {
      for (const l of dec.decode(chunk).split("\n")) if (l.trim()) s.stderr.push(l.trim());
      if (s.stderr.length > 30) s.stderr.splice(0, s.stderr.length - 30);
    }
  })();
  s.proc.exited.then((code) => {
    if (c.streamer !== s) return;
    c.streamer = null;
    // ustreamer logs "-- ERROR [...] ..." lines; the last one says why it gave up.
    if (code && !s.stopping) c.error = s.stderr.filter((l) => /error/i.test(l)).pop() || s.stderr.pop() || `ustreamer exited (${code})`;
    ctx.changed();
  });
  s.ready = (async () => {
    for (let i = 0; i < 50; i++) {
      if (c.streamer !== s) throw new HttpError(503, c.error || "ustreamer stopped");
      try {
        const r = await upstream(s, "/state");
        if (r.ok) {
          readState(s, await r.json());
          ctx.changed();
          return;
        }
      } catch {}
      await Bun.sleep(100);
    }
    throw new HttpError(504, "ustreamer didn't start within 5 s");
  })();
  ctx.changed();
  return s.ready;
}

function readState(s: Streamer, j: any) {
  const r = j?.result || {};
  s.clients = r.stream?.clients ?? 0;
  s.fps = r.source?.captured_fps ?? 0;
  s.online = !!r.source?.online;
}

/** Resolves once ustreamer has exited, so a restart doesn't find the device or port still busy. */
function stopStreamer(c: Cam): Promise<unknown> {
  const s = c.streamer;
  if (!s) return Promise.resolve();
  s.stopping = true;
  c.streamer = null;
  s.proc.kill();
  ctx.changed();
  return s.proc.exited;
}

/** Every 10 s: refresh viewer counts and stop streamers nobody has used for a minute. */
async function idleSweep() {
  for (const c of cams.values()) {
    const s = c.streamer;
    if (!s) continue;
    try {
      const r = await upstream(s, "/state");
      if (r.ok) readState(s, await r.json());
    } catch {}
    if (s.clients > 0 || c.proxied > 0) c.lastUse = Date.now();
    // A shared stream is for other tools, so it stays up until sharing is turned off.
    if (!s.shared && Date.now() - c.lastUse > IDLE_STOP_MS) {
      ctx.log(`${c.id}: idle for a minute, stopping ustreamer`);
      stopStreamer(c);
    }
  }
  ctx.changed();
}

// ---------- Snapshots and streams ----------

async function snapshot(c: Cam): Promise<Uint8Array> {
  await ensureStreamer(c);
  const s = c.streamer!;
  // Right after start ustreamer answers 503 until the camera delivers a first frame.
  for (let i = 0; i < 40; i++) {
    const r = await upstream(s, "/snapshot");
    if (r.ok) {
      const jpeg = new Uint8Array(await r.arrayBuffer());
      c.last = { jpeg, t: Date.now() };
      c.lastUse = Date.now();
      return jpeg;
    }
    await r.arrayBuffer().catch(() => {});
    await Bun.sleep(150);
  }
  throw new HttpError(504, `${c.name} hasn't delivered a frame (no signal?)`);
}

/** Pipe ustreamer's multipart MJPEG through, counting viewers so the idle sweep knows. */
async function stream(c: Cam): Promise<Response> {
  await ensureStreamer(c);
  // Cancelling the body reader alone leaves Bun's upstream socket open (ustreamer keeps
  // counting a client and never idles), so the abort signal is what really hangs up.
  const ac = new AbortController();
  const r = await upstream(c.streamer!, "/stream", { signal: ac.signal });
  if (!r.ok || !r.body) throw new HttpError(502, `ustreamer: ${r.status}`);
  const reader = r.body.getReader();
  c.proxied++;
  let done = false;
  const finish = () => {
    if (done) return;
    done = true;
    c.proxied--;
    c.lastUse = Date.now();
    ctx.changed();
  };
  const body = new ReadableStream<Uint8Array>({
    async pull(ctl) {
      try {
        const { value, done: end } = await reader.read();
        if (end) {
          finish();
          ctl.close();
        } else ctl.enqueue(value);
      } catch (e) {
        finish();
        ctl.error(e);
      }
    },
    cancel() {
      finish();
      reader.cancel().catch(() => {});
      ac.abort();
    },
  });
  ctx.changed();
  return new Response(body, { headers: { "Content-Type": r.headers.get("content-type") || "multipart/x-mixed-replace", "Cache-Control": "no-store" } });
}

// ---------- API helpers ----------

function cam(id: string): Cam {
  const c = cams.get(id);
  if (!c) throw new HttpError(404, `No camera "${id}"${cams.size ? ` (have: ${[...cams.keys()].join(", ")})` : " (none connected)"}`);
  return c;
}

function publicCam(c: Cam) {
  const cfg = getCfg(c);
  const s = c.streamer;
  return {
    id: c.id,
    dev: c.dev,
    name: c.name,
    vendor: c.vendor,
    vid: c.vid,
    pid: c.pid,
    serial: c.serial,
    kind: c.kind,
    usbPort: c.usbPort,
    driver: c.driver,
    stream: c.stream,
    streaming: !!s,
    viewers: s ? Math.max(s.clients, c.proxied) : 0,
    fps: s?.fps || 0,
    signal: s ? s.online : null,
    share: !!cfg.share,
    sharePort: cfg.share ? s?.port || sharePort(c) : cfg.sharePort || sharePort(c),
    lastSnapshot: c.last?.t || 0,
    error: c.error,
  };
}

function baseUrl() {
  const port = parseInt(process.env.PORT || "") || 80;
  return `http://${hostname()}${port === 80 ? "" : ":" + port}`;
}

async function setControls(c: Cam, values: Record<string, any>) {
  if (!values || typeof values !== "object") throw new HttpError(400, "Expected { name: value, … }");
  await readControls(c);
  const saved = { ...(getCfg(c).controls || {}) };
  const order = Object.keys(values).sort((a, b) => +/auto/.test(b) - +/auto/.test(a));
  for (const name of order) {
    const ctl = c.controls.find((x) => x.name === name);
    if (!ctl) throw new HttpError(400, `${c.name} has no control "${name}"`);
    const v = Math.round(Number(values[name]));
    if (!Number.isFinite(v)) throw new HttpError(400, `${name}: not a number`);
    const r = await run(["v4l2-ctl", "-d", c.dev, "-c", `${name}=${v}`], { timeoutMs: 3000 });
    if (r.code) throw new HttpError(400, `${name}=${v}: ${r.err || r.out}`);
    saved[name] = v;
  }
  setCfg(c, { controls: saved });
  // Flipping an auto switch changes which other controls are "inactive".
  await readControls(c);
  return c.controls.filter(isUserControl);
}

// ---------- Driver ----------

const driver: Driver = {
  id: "camera",
  title: "Camera",
  order: 60,
  description: "USB webcams and HDMI capture sticks: live MJPEG view, snapshots, image controls",

  async init(c) {
    ctx = c;
    await Bun.$`mkdir -p ${RUN_DIR}`.quiet().nothrow();
    await rescan();
    idleTimer = setInterval(() => idleSweep().catch(() => {}), 10_000);
    // libcamera enumeration takes a second or two; nothing waits on it.
    run(["rpicam-hello", "--list-cameras"], { timeoutMs: 15_000 }).then((r) => {
      csi = parseLibcameraList(r.out + "\n" + r.err);
      if (csi.length) ctx.changed();
    });
  },

  stop() {
    if (idleTimer) clearInterval(idleTimer);
    for (const c of cams.values()) stopStreamer(c);
    cams.clear();
  },

  udev(ev: UdevEvent) {
    if (ev.props.SUBSYSTEM === "video4linux") scheduleRescan();
  },

  status() {
    const list = [...cams.values()].map(publicCam);
    const live = list.filter((c) => c.streaming);
    const viewers = live.reduce((a, c) => a + c.viewers, 0);
    const names = list.map((c) => c.name).join(", ");
    return {
      active: list.length > 0 || csi.length > 0,
      summary: !list.length ? (csi.length ? `CSI: ${csi.map((x) => x.sensor).join(", ")}` : "No camera") : live.length ? `${names} · live, ${viewers} viewer${viewers === 1 ? "" : "s"}` : names,
      cameras: list,
      csi,
    };
  },

  routes: [
    {
      method: "GET",
      path: "/api/camera",
      doc: "List cameras: id (e.g. video0), name, kind (webcam | hdmi), chosen stream format, whether ustreamer is running, viewers; plus CSI cameras seen by libcamera (listed only)",
      handler: () => ({ cameras: [...cams.values()].map(publicCam), csi }),
    },
    {
      method: "GET",
      path: "/api/camera/:id",
      doc: "One camera in detail: every format/resolution/fps it offers and its adjustable controls (brightness, exposure, white balance, focus…) with ranges and current values",
      async handler(_req, p) {
        const c = cam(p.id);
        await readControls(c);
        return { ...publicCam(c), formats: c.formats, controls: c.controls.filter(isUserControl) };
      },
    },
    {
      method: "GET",
      path: "/api/camera/:id/stream",
      doc: "Live MJPEG (multipart/x-mixed-replace) — use as an <img> src. Starts the camera on demand; it stops a minute after the last viewer leaves",
      handler: (_req, p) => stream(cam(p.id)),
    },
    {
      method: "GET",
      path: "/api/camera/:id/snapshot",
      doc: "A fresh JPEG frame (starts the camera if needed). ?download=1 adds a Content-Disposition filename",
      async handler(_req, p, url) {
        const c = cam(p.id);
        const jpeg = await snapshot(c);
        const headers: Record<string, string> = { "Content-Type": "image/jpeg", "Cache-Control": "no-store" };
        if (url.searchParams.get("download")) headers["Content-Disposition"] = `attachment; filename="${c.id}-${new Date(c.last!.t).toISOString().replace(/[:.]/g, "-")}.jpg"`;
        return new Response(jpeg, { headers });
      },
    },
    {
      method: "GET",
      path: "/api/camera/:id/last.jpg",
      doc: "The most recent snapshot taken (by anyone), without waking the camera. 404 if none yet",
      handler(_req, p) {
        const c = cam(p.id);
        if (!c.last) throw new HttpError(404, "No snapshot taken yet");
        return new Response(c.last.jpeg, { headers: { "Content-Type": "image/jpeg", "Cache-Control": "no-store", "X-Taken-At": new Date(c.last.t).toISOString() } });
      },
    },
    {
      method: "POST",
      path: "/api/camera/:id/controls",
      doc: { body: { brightness: 140, auto_exposure: 1, exposure_time_absolute: 300 }, description: "Set image controls (v4l2 names from GET /api/camera/:id). Remembered per camera and re-applied when it's plugged in again" },
      async handler(req, p) {
        return { ok: true, controls: await setControls(cam(p.id), await body(req)) };
      },
    },
    {
      method: "POST",
      path: "/api/camera/:id/controls/reset",
      doc: "Put every control back to the camera's default and forget the saved values",
      async handler(_req, p) {
        const c = cam(p.id);
        await readControls(c);
        const defaults: Record<string, number> = {};
        for (const x of c.controls.filter(isUserControl)) if (x.default != null) defaults[x.name] = x.default;
        const controls = await setControls(c, defaults);
        setCfg(c, { controls: {} });
        return { ok: true, controls };
      },
    },
    {
      method: "POST",
      path: "/api/camera/:id/config",
      doc: { body: { fourcc: "MJPG", w: 1280, h: 720, fps: 30, share: false, sharePort: 8080 }, description: "Stream format/resolution/fps, and 'share directly': ustreamer on 0.0.0.0:sharePort (its own /stream, /snapshot, /state) for tools that want it without DeskyPi in the path. A shared stream stays running until sharing is turned off" },
      async handler(req, p) {
        const c = cam(p.id);
        const b = await body(req);
        const patch: Partial<CamSettings> = {};
        if (b.fourcc || b.w || b.h || b.fps) {
          const want = { fourcc: b.fourcc ?? c.stream?.fourcc, w: +(b.w ?? c.stream?.w), h: +(b.h ?? c.stream?.h), fps: +(b.fps ?? c.stream?.fps) };
          const pick = chooseStream(c.formats, want);
          if (!pick || pick.fourcc !== want.fourcc || pick.w !== want.w || pick.h !== want.h) throw new HttpError(400, `${c.name} doesn't offer ${want.fourcc} ${want.w}x${want.h}`);
          c.stream = pick;
          patch.stream = pick;
        }
        if (b.sharePort != null) {
          const port = +b.sharePort;
          if (!Number.isInteger(port) || port < 1024 || port > 65535) throw new HttpError(400, "sharePort must be 1024–65535");
          patch.sharePort = port;
        }
        if (b.share != null) patch.share = !!b.share;
        setCfg(c, patch);
        // Restart with the new settings; a shared camera comes straight back up.
        const wasRunning = !!c.streamer;
        await stopStreamer(c);
        if (getCfg(c).share || wasRunning) await ensureStreamer(c).catch((e) => (c.error = e.message));
        ctx.changed();
        return { ok: true, camera: publicCam(c) };
      },
    },
    {
      method: "POST",
      path: "/api/camera/:id/stop",
      doc: "Stop the camera's ustreamer now (viewers are disconnected; it restarts on the next view)",
      handler(_req, p) {
        stopStreamer(cam(p.id));
        return { ok: true };
      },
    },
  ],

  tools: [
    {
      name: "camera_list",
      description: "List cameras attached to DeskyPi (USB webcams, HDMI capture sticks; CSI cameras are listed but can't be streamed). Returns ids for camera_snapshot, the stream format, and the URLs of the live MJPEG stream and snapshot endpoints.",
      inputSchema: { type: "object", properties: {} },
      handler: () => {
        const base = baseUrl();
        return {
          cameras: [...cams.values()].map((c) => ({ ...publicCam(c), streamUrl: `${base}/api/camera/${c.id}/stream`, snapshotUrl: `${base}/api/camera/${c.id}/snapshot` })),
          csi,
        };
      },
    },
    {
      name: "camera_snapshot",
      description: "Capture a JPEG from a camera now and return the image itself, plus a URL to fetch it again (valid until the next snapshot), its size and time. Use camera_list first to get the id.",
      inputSchema: { type: "object", properties: { id: { type: "string", description: "Camera id from camera_list, e.g. video0. Optional when exactly one camera is connected." } } },
      async handler(args) {
        const id = args.id || (cams.size === 1 ? [...cams.keys()][0] : "");
        if (!id) throw new HttpError(400, cams.size ? `Several cameras; pass id (${[...cams.keys()].join(", ")})` : "No camera connected");
        const c = cam(id);
        const jpeg = await snapshot(c);
        const meta = {
          url: `${baseUrl()}/api/camera/${c.id}/last.jpg?t=${c.last!.t}`,
          path: `/api/camera/${c.id}/last.jpg`,
          camera: c.name,
          bytes: jpeg.length,
          width: c.stream?.w,
          height: c.stream?.h,
          takenAt: new Date(c.last!.t).toISOString(),
        };
        // Agents can look at the picture directly instead of fetching the URL.
        return mcpContent(
          [{ type: "image", data: Buffer.from(jpeg).toString("base64"), mimeType: "image/jpeg" }, { type: "text", text: JSON.stringify(meta, null, 2) }],
          meta,
        );
      },
    },
  ],
};

export default driver;
