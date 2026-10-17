// ==================== BLE scanner ====================
//
// Passive view of every Bluetooth LE advertiser in range, on the onboard hci0 or a USB
// dongle, with decoders for the common beacon and sensor formats.
//
// How a scan runs (chosen after trying D-Bus/busctl, an interactive bluetoothctl, and
// btmgmt; see plans/peripherals-radios.md):
//  - `bluetoothctl` stays running as a child and holds a BlueZ discovery session with
//    `duplicate-data on`, so the controller reports every advertisement (RSSI history needs
//    them) and other BlueZ clients keep working. Discovery is tied to the D-Bus connection
//    that started it, which is why a one-shot `busctl call StartDiscovery` doesn't work.
//  - `sudo btmon -w <fifo>` streams raw HCI traffic as btsnoop. The LE advertising
//    reports in it carry what BlueZ's D-Bus view throws away: the PDU type (connectable or
//    not), scan responses as separate packets, and the exact AD bytes for the decoders.
//
// btmon writes to a FIFO rather than its stdout because Bun hands children a socketpair,
// not a pipe, and btmon reopens `/dev/stdout` by path, which fails on a socket (exit 1, no
// message). We open the FIFO O_RDWR so the open never blocks the event loop waiting for a
// writer.
//
// The radio is left as it was found: if a scan had to lift an rfkill soft block or power
// the adapter on, it is put back when scanning stops.

import type { Driver, Ctx, Props, Tool, Route, Socket, WsData } from "../core.ts";
import { HttpError, run, body, usbName, readText } from "../core.ts";
import type { ServerWebSocket, Subprocess } from "bun";
import { BleTracker, BtsnoopReader, MON, parseHciEvent, deviceKind, type BleDevice } from "../lib/ble-decode.ts";
import { readdirSync, mkdtempSync, openSync, closeSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";

const BTMON = "/usr/bin/btmon";
const BTCTL = "/usr/bin/bluetoothctl";
const BUSCTL = "/usr/bin/busctl";
const RFKILL = "/usr/sbin/rfkill";

const RECENT_MS = 5 * 60_000; // "active" while anything was heard this recently
const FORGET_MS = 30 * 60_000; // drop advertisers silent for this long
const MAX_SCAN_S = 600;
const LIVE_GRACE_MS = 4000; // a page reload shouldn't bounce the radio

let ctx: Ctx;
const tracker = new BleTracker();

// ---------- Controllers ----------

interface Controller {
  name: string; // hci0
  index: number;
  devpath: string;
  bus: string; // "onboard (UART)" or the USB product
  address: string;
  powered: boolean | null;
  powerState: string; // BlueZ: on / off / off-blocked / …
  rfkill: { index: number; soft: boolean; hard: boolean } | null;
}

let controllers: Controller[] = [];

async function readRfkill(name: string): Promise<Controller["rfkill"]> {
  try {
    const dir = `/sys/class/bluetooth/${name}`;
    const rf = readdirSync(dir).find((f) => /^rfkill\d+$/.test(f));
    if (!rf) return null;
    return { index: parseInt(await readText(`${dir}/${rf}/index`)), soft: (await readText(`${dir}/${rf}/soft`)) === "1", hard: (await readText(`${dir}/${rf}/hard`)) === "1" };
  } catch {
    return null;
  }
}

async function adapterProps(name: string): Promise<{ address: string; powered: boolean | null; powerState: string }> {
  const r = await run([BUSCTL, "--json=short", "get-property", "org.bluez", `/org/bluez/${name}`, "org.bluez.Adapter1", "Address", "Powered", "PowerState"], { timeoutMs: 3000 });
  if (r.code !== 0) return { address: "", powered: null, powerState: r.err.includes("ServiceUnknown") ? "bluetoothd not running" : "unknown" };
  const [a, p, s] = r.out.split("\n").map((l) => {
    try {
      return JSON.parse(l).data;
    } catch {
      return null;
    }
  });
  return { address: a || "", powered: p ?? null, powerState: s || "" };
}

async function refreshControllers() {
  const hosts = ctx.devices((p) => p.SUBSYSTEM === "bluetooth" && p.DEVTYPE === "host");
  const next: Controller[] = [];
  for (const p of hosts) {
    const name = p.DEVPATH.split("/").pop()!;
    const usb = ctx.ancestor(p.DEVPATH, "usb", "usb_device");
    const bus = usb ? Object.values(usbName(usb)).filter(Boolean).join(" ") || `USB ${usb.ID_VENDOR_ID}:${usb.ID_MODEL_ID}` : p.DEVPATH.includes("serial") ? "onboard (UART)" : "onboard";
    next.push({ name, index: parseInt(name.slice(3)), devpath: p.DEVPATH, bus, ...(await adapterProps(name)), rfkill: await readRfkill(name) });
  }
  controllers = next.sort((a, b) => a.index - b.index);
  ctx.changed();
}

function pickController(want?: string): Controller {
  const pref = want || ctx.settings.get<string>("controller", "");
  const c = controllers.find((c) => c.name === pref || (c.address && c.address === pref.toUpperCase())) || (!want ? controllers[0] : undefined);
  if (!c) throw new HttpError(want ? 404 : 409, want ? `No Bluetooth controller "${want}"` : "No Bluetooth controller found");
  return c;
}

// ---------- Scan session ----------

interface Scan {
  ctrl: Controller;
  started: number;
  until: number; // 0 = open-ended (live watchers)
  btmon: Subprocess;
  fifo: { dir: string; fd: number };
  btctl: Subprocess<"pipe", "pipe", "pipe">;
  restore: { rfkill?: number; power?: boolean };
  packets: number;
  stopping?: boolean;
}

let scan: Scan | null = null;
let lastError = "";
let lastScan: { ctrl: string; started: number; ended: number; packets: number } | null = null;
let busy: Promise<any> = Promise.resolve(); // serialises start/stop
let stopTimer: ReturnType<typeof setTimeout> | null = null;
const scanWaiters = new Set<() => void>();

function serial<T>(fn: () => Promise<T>): Promise<T> {
  const p = busy.then(fn, fn);
  busy = p.catch(() => {});
  return p;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function powerUp(c: Controller): Promise<Scan["restore"]> {
  const restore: Scan["restore"] = {};
  const rf = await readRfkill(c.name);
  if (rf?.hard) throw new HttpError(409, `${c.name} is hard-blocked (hardware switch); can't scan`);
  if (rf?.soft) {
    const r = await run(["sudo", "-n", RFKILL, "unblock", String(rf.index)], { timeoutMs: 5000 });
    if (r.code !== 0) throw new HttpError(500, `rfkill unblock ${c.name}: ${r.err || r.out}`);
    restore.rfkill = rf.index;
  }
  // bluetoothd needs a moment after an unblock before Powered=true sticks.
  for (let i = 0; i < 10; i++) {
    const st = await adapterProps(c.name);
    if (st.powered) break;
    const r = await run([BUSCTL, "set-property", "org.bluez", `/org/bluez/${c.name}`, "org.bluez.Adapter1", "Powered", "b", "true"], { timeoutMs: 5000 });
    if (r.code === 0) {
      restore.power = true;
      break;
    }
    if (i === 9) throw new HttpError(500, `Couldn't power on ${c.name}: ${r.err}`);
    await sleep(300);
  }
  return restore;
}

async function restoreRadio(c: Controller, restore: Scan["restore"]) {
  if (!ctx.settings.get("restoreRadio", true)) return;
  if (restore.power) await run([BUSCTL, "set-property", "org.bluez", `/org/bluez/${c.name}`, "org.bluez.Adapter1", "Powered", "b", "false"], { timeoutMs: 5000 });
  if (restore.rfkill != null) await run(["sudo", "-n", RFKILL, "block", String(restore.rfkill)], { timeoutMs: 5000 });
}

function startScan(opts: { controller?: string; durationS?: number }): Promise<void> {
  return serial(async () => {
    if (stopTimer) clearTimeout(stopTimer), (stopTimer = null);
    const durUntil = opts.durationS ? Date.now() + Math.min(opts.durationS, MAX_SCAN_S) * 1000 : 0;
    if (scan && !scan.stopping) {
      if (opts.controller && opts.controller !== scan.ctrl.name && opts.controller !== scan.ctrl.address) throw new HttpError(409, `Already scanning on ${scan.ctrl.name}; stop it first`);
      // Extend a timed scan; an open-ended (live) one already outlasts it.
      if (scan.until && durUntil) scan.until = Math.max(scan.until, durUntil);
      else if (!durUntil) scan.until = 0;
      return;
    }
    await refreshControllers();
    const c = pickController(opts.controller);
    lastError = "";
    const restore = await powerUp(c);

    // Raw HCI first, so the first advertisements after discovery starts aren't missed.
    const dir = mkdtempSync(`${tmpdir()}/deskypi-ble-`);
    const mk = await run(["mkfifo", "-m", "600", `${dir}/hci.btsnoop`]);
    if (mk.code !== 0) {
      rmSync(dir, { recursive: true, force: true });
      await restoreRadio(c, restore);
      throw new Error(`mkfifo: ${mk.err}`);
    }
    const fd = openSync(`${dir}/hci.btsnoop`, "r+");
    const btmon = ctx.spawn(["sudo", "-n", BTMON, "-w", `${dir}/hci.btsnoop`], { stdin: "ignore", stdout: "ignore", stderr: "pipe" });
    const btctl = ctx.spawn([BTCTL], { stdin: "pipe", stdout: "pipe", stderr: "pipe", env: { ...process.env, TERM: "dumb" } }) as Subprocess<"pipe", "pipe", "pipe">;
    const s: Scan = { ctrl: c, started: Date.now(), until: durUntil, btmon, fifo: { dir, fd }, btctl, restore, packets: 0 };
    scan = s;
    readBtmon(s);
    readBtctl(s);
    btctl.stdin.write(`select ${c.address}\nmenu scan\nduplicate-data on\nback\nscan le\n`);
    btctl.stdin.flush();
    ctx.event(`BLE scan started on ${c.name}${durUntil ? ` for ${Math.round((durUntil - Date.now()) / 1000)} s` : " (live)"}`);
    ctx.changed();
  });
}

function stopScan(reason = ""): Promise<void> {
  return serial(async () => {
    const s = scan;
    if (!s || s.stopping) return;
    s.stopping = true;
    try {
      s.btctl.stdin.write("scan off\nquit\n");
      s.btctl.stdin.flush();
    } catch {}
    await Promise.race([s.btctl.exited, sleep(1500)]);
    s.btctl.kill();
    s.btmon.kill();
    await Promise.race([s.btmon.exited, sleep(1500)]);
    await restoreRadio(s.ctrl, s.restore);
    lastScan = { ctrl: s.ctrl.name, started: s.started, ended: Date.now(), packets: s.packets };
    if (reason) lastError = reason;
    scan = null;
    for (const w of scanWaiters) w();
    ctx.event(`BLE scan stopped on ${s.ctrl.name}${reason ? `: ${reason}` : ""} (${s.packets} adverts)`);
    await refreshControllers();
  });
}

async function readBtmon(s: Scan) {
  const reader = new BtsnoopReader();
  let err = "";
  (async () => {
    err = await new Response(s.btmon.stderr as ReadableStream).text();
  })();
  // Holding the FIFO open read-write means no EOF when btmon exits; end the read on exit instead.
  const rd = Bun.file(s.fifo.fd).stream().getReader();
  s.btmon.exited.then(() => rd.cancel().catch(() => {}));
  try {
    while (true) {
      const { value: chunk, done } = await rd.read();
      if (done || !chunk) break;
      for (const rec of reader.push(chunk)) {
        if (rec.opcode !== MON.EVENT || rec.index !== s.ctrl.index) continue;
        for (const r of parseHciEvent(rec.data, rec.ts, rec.index)) {
          tracker.ingest(r, s.ctrl.name);
          s.packets++;
        }
      }
    }
  } catch (e: any) {
    err ||= e.message;
  }
  const code = await s.btmon.exited;
  try {
    closeSync(s.fifo.fd);
  } catch {}
  rmSync(s.fifo.dir, { recursive: true, force: true });
  if (scan === s && !s.stopping) stopScan(`btmon exited (${code})${err ? `: ${err.trim().split("\n").pop()}` : ""}`);
}

async function readBtctl(s: Scan) {
  // Drain stdout (it prints a line per device change; a full pipe would stall it) and pick
  // out discovery failures.
  const dec = new TextDecoder();
  let buf = "";
  for await (const chunk of s.btctl.stdout as ReadableStream<Uint8Array>) {
    buf += dec.decode(chunk, { stream: true }).replace(/\x1b\[[0-9;]*[A-Za-z]|\r/g, "");
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, i).trim();
      buf = buf.slice(i + 1);
      if (/Failed to start discovery|No default controller|Controller .* not available/i.test(line)) {
        lastError = line.replace(/^\[.*?\]>?\s*/, "");
        ctx.log(lastError);
      }
    }
  }
  if (scan === s && !s.stopping) stopScan("bluetoothctl exited");
}

// Timed scans end on their own; live scans end when the last watcher has been gone a while.
let tick: ReturnType<typeof setInterval> | null = null;
function onTick() {
  const now = Date.now();
  tracker.prune(now, FORGET_MS);
  if (scan && !scan.stopping) {
    const live = liveWatchers() > 0;
    if (scan.until && now >= scan.until && !live) stopScan();
    else if (!scan.until && !live && !stopTimer) stopTimer = setTimeout(() => {
      stopTimer = null;
      if (scan && !scan.until && liveWatchers() === 0) stopScan();
    }, LIVE_GRACE_MS);
  }
  pushDevices();
}

// ---------- WebSocket watchers ----------

const watchers = new Set<ServerWebSocket<WsData>>();
const liveWatchers = () => [...watchers].filter((w) => w.data.state.live).length;

function devicePayload(d: BleDevice) {
  return { ...d, kind: deviceKind(d) };
}

function snapshot() {
  return {
    type: "devices",
    t: Date.now(),
    scan: scanInfo(),
    packets: tracker.packets,
    devices: tracker.list({ limit: 300 }).map(devicePayload),
  };
}

let lastPushSig = "";
function pushDevices() {
  if (!watchers.size) return;
  const snap = snapshot();
  const sig = `${tracker.packets}|${snap.devices.length}|${JSON.stringify(snap.scan)}`;
  if (sig === lastPushSig) return;
  lastPushSig = sig;
  const msg = JSON.stringify(snap);
  for (const w of watchers) w.send(msg);
}

function scanInfo() {
  return scan
    ? { controller: scan.ctrl.name, started: scan.started, until: scan.until || null, live: !scan.until, packets: scan.packets, stopping: !!scan.stopping }
    : null;
}

// ---------- MCP/REST shapes ----------

function compact(d: BleDevice) {
  return {
    addr: d.addr,
    addrType: d.addrType,
    name: d.name || undefined,
    rssi: d.rssi,
    txPower: d.txPower ?? undefined,
    connectable: d.connectable,
    lastSeenAgo_s: Math.round((Date.now() - d.lastSeen) / 1000),
    seenFor_s: Math.round((d.lastSeen - d.firstSeen) / 1000),
    packets: d.count,
    kind: deviceKind(d) || undefined,
    manufacturers: d.mfr.length ? d.mfr.map((m) => ({ company: `0x${m.company.toString(16).padStart(4, "0")}`, name: m.companyName || undefined, data: m.data })) : undefined,
    services: d.uuids.length ? d.uuids.map((u) => (u.name ? `${u.uuid} (${u.name})` : u.uuid)) : undefined,
    serviceData: d.svc.length ? d.svc.map((s) => ({ uuid: s.uuid, name: s.name || undefined, data: s.data })) : undefined,
    beacons: d.beacons.length ? d.beacons : undefined,
  };
}

function listDevices(q: { seconds?: number; minRssi?: number; limit?: number; beaconsOnly?: boolean; since?: number }) {
  let l = tracker.list({ since: q.since ?? (q.seconds ? Date.now() - q.seconds * 1000 : undefined), minRssi: q.minRssi, limit: undefined });
  if (q.beaconsOnly) l = l.filter((d) => d.beacons.length);
  return l.slice(0, Math.min(q.limit || 100, 500)).map(compact);
}

async function scanAndWait(args: { duration?: number; controller?: string; minRssi?: number; limit?: number }) {
  const dur = Math.max(1, Math.min(Number(args.duration) || 10, 120));
  const since = Date.now();
  await startScan({ controller: args.controller, durationS: dur });
  await new Promise<void>((res) => {
    const done = () => (scanWaiters.delete(done), clearTimeout(t), res());
    const t = setTimeout(done, dur * 1000 + 500);
    scanWaiters.add(done);
  });
  const devices = listDevices({ since, minRssi: args.minRssi, limit: args.limit });
  return { controller: scan?.ctrl.name || lastScan?.ctrl, duration_s: dur, count: devices.length, error: lastError || undefined, devices };
}

function num(v: string | null): number | undefined {
  return v != null && v !== "" && !isNaN(+v) ? +v : undefined;
}

// ---------- Driver ----------

const routes: Route[] = [
  {
    method: "GET",
    path: "/api/ble",
    doc: "BLE scanner state: controllers (hci0 + USB dongles, power/rfkill), the running scan, device counts",
    handler: () => status(),
  },
  {
    method: "GET",
    path: "/api/ble/devices",
    doc: { description: "Advertisers heard recently, strongest first, with decoded beacons", query: { seconds: "only those heard in the last N s", minRssi: "e.g. -70", limit: "default 100", beacons: "1 = only decoded beacons/sensors" } },
    handler: (_r, _p, url) => listDevices({ seconds: num(url.searchParams.get("seconds")), minRssi: num(url.searchParams.get("minRssi")), limit: num(url.searchParams.get("limit")), beaconsOnly: url.searchParams.get("beacons") === "1" }),
  },
  {
    method: "GET",
    path: "/api/ble/devices/:addr",
    doc: "One advertiser in full: RSSI history (1 s slots), raw advertising + scan-response bytes, every AD field",
    handler: (_r, p) => {
      const d = tracker.devices.get(p.addr.toUpperCase());
      if (!d) throw new HttpError(404, `No device ${p.addr} heard recently`);
      return devicePayload(d);
    },
  },
  {
    method: "POST",
    path: "/api/ble/scan",
    doc: { body: { duration: 10, controller: "hci0 (optional)", wait: false }, description: "Scan for `duration` s (max 600; extends a running scan). With wait:true, responds when done with the devices heard (max 120 s)." },
    handler: async (req) => {
      const b = await body(req);
      if (b.wait) return scanAndWait(b);
      await startScan({ controller: b.controller, durationS: Math.max(1, Number(b.duration) || 10) });
      return { ok: true, scan: scanInfo() };
    },
  },
  {
    method: "POST",
    path: "/api/ble/stop",
    doc: "Stop scanning now (restores the radio's previous power/rfkill state)",
    handler: async () => {
      await stopScan();
      return { ok: true };
    },
  },
  {
    method: "DELETE",
    path: "/api/ble/devices",
    doc: "Forget all advertisers seen so far",
    handler: () => {
      tracker.devices.clear();
      lastPushSig = "";
      ctx.changed();
      return { ok: true };
    },
  },
  {
    method: "POST",
    path: "/api/ble/settings",
    doc: { body: { controller: "hci1 or its address", restoreRadio: true }, description: "Default controller (remembered by address) and whether to put the radio back to off/blocked after scanning" },
    handler: async (req) => {
      const b = await body(req);
      if (b.controller !== undefined) {
        const c = pickController(String(b.controller));
        ctx.settings.set("controller", c.address || c.name);
      }
      if (b.restoreRadio !== undefined) ctx.settings.set("restoreRadio", !!b.restoreRadio);
      ctx.changed();
      return { ok: true };
    },
  },
];

const sockets: Socket[] = [
  {
    path: "/ws/ble",
    doc: 'Live device list (pushed ~1/s while it changes). Send {"live":true} to keep scanning while this socket is open; {"live":false} to stop watching.',
    open(ws) {
      watchers.add(ws);
      lastPushSig = "";
      ws.send(JSON.stringify(snapshot()));
    },
    message(ws, msg) {
      const m = JSON.parse(String(msg));
      if (typeof m.live === "boolean") {
        ws.data.state.live = m.live;
        if (m.live) startScan({ controller: m.controller }).catch((e) => ws.send(JSON.stringify({ type: "error", message: e.message })));
        ctx.changed();
      }
    },
    close(ws) {
      watchers.delete(ws);
    },
  },
];

const tools: Tool[] = [
  {
    name: "ble_scan",
    description:
      "Scan for Bluetooth LE advertisers for `duration` seconds and return what was heard: address (+public/random type), name, RSSI, TX power, connectable, manufacturer and service data, and decoded beacons/sensors (iBeacon, Eddystone, BTHome, RuuviTag, Xiaomi/ATC/pvvx thermometers, Govee, Apple Continuity labels, Microsoft Swift Pair).",
    inputSchema: {
      type: "object",
      properties: {
        duration: { type: "number", description: "Seconds to scan (1–120, default 10)" },
        controller: { type: "string", description: "hci0 (onboard) or another controller name/address; default is the remembered one" },
        minRssi: { type: "number", description: "Only devices at least this strong, e.g. -75" },
        limit: { type: "number", description: "Max devices (default 100)" },
      },
    },
    handler: (a) => scanAndWait(a),
  },
  {
    name: "ble_devices",
    description: "List BLE advertisers already heard (from earlier or ongoing scans) without starting a scan. Strongest first.",
    inputSchema: {
      type: "object",
      properties: {
        seconds: { type: "number", description: "Only devices heard in the last N seconds" },
        minRssi: { type: "number" },
        beaconsOnly: { type: "boolean", description: "Only devices with a decoded beacon/sensor payload" },
        limit: { type: "number", description: "Max devices (default 100)" },
      },
    },
    handler: (a) => ({ scan: scanInfo(), lastScan, controllers: controllers.map(({ name, address, bus, powered, powerState }) => ({ name, address, bus, powered, powerState })), devices: listDevices(a) }),
  },
];

function status() {
  const now = Date.now();
  let recent = 0;
  for (const d of tracker.devices.values()) if (now - d.lastSeen < RECENT_MS) recent++;
  const s = scanInfo();
  return {
    active: !!s || recent > 0,
    summary: s ? `Scanning ${s.controller} · ${recent} device${recent === 1 ? "" : "s"}` : recent ? `${recent} device${recent === 1 ? "" : "s"} heard recently` : controllers.length ? `${controllers.map((c) => c.name).join(", ")} idle` : "No controller",
    controllers,
    defaultController: ctx.settings.get<string>("controller", ""),
    restoreRadio: ctx.settings.get("restoreRadio", true),
    scan: s,
    lastScan,
    error: lastError || undefined,
    recent,
    known: tracker.devices.size,
  };
}

export default {
  id: "ble",
  title: "Bluetooth LE",
  order: 60,
  description: "Scan BLE advertisers on hci0 or USB dongles; decodes iBeacon, Eddystone, BTHome, Ruuvi, Xiaomi, Govee and more",
  async init(c) {
    ctx = c;
    await refreshControllers();
    tick = setInterval(onTick, 1000);
  },
  async stop() {
    if (tick) clearInterval(tick);
    tick = null;
    await stopScan();
    for (const w of watchers) w.close();
  },
  udev(ev) {
    if (ev.props.SUBSYSTEM !== "bluetooth" || ev.props.DEVTYPE !== "host" || (ev.action !== "add" && ev.action !== "remove")) return;
    const name = ev.props.DEVPATH.split("/").pop();
    ctx.event(`Bluetooth controller ${name} ${ev.action === "add" ? "appeared" : "removed"}`);
    if (ev.action === "remove" && scan?.ctrl.name === name) stopScan("controller removed");
    // bluetoothd registers the adapter a moment after udev announces it.
    setTimeout(refreshControllers, 1500);
  },
  status,
  routes,
  sockets,
  tools,
} satisfies Driver;
