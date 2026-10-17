// ==================== UPS (NUT) ====================
//
// A USB UPS plugged into DeskyPi is driven by NUT, run entirely as our own children: the
// right driver (usbhid-ups, nutdrv_qx… whatever NUT's udev table says) plus upsd, with
// config and state in a private temp dir. The system NUT units stay masked and /etc/nut is
// never read, so nothing here survives DeskyPi stopping. We poll `upsc` every 5 s, decode
// the status flags into words, keep charge/load history for sparklines, and log power events
// to the hardware timeline. "Share on network" opens upsd on 0.0.0.0 so NUT clients
// elsewhere (Home Assistant, upsmon on other servers) can follow the same UPS.
//
// Automatic shutdown of desky on low battery is deliberately not configured (no upsmon).
//
// Testing without hardware: DESKYPI_UPS_SIMULATE=/path/to.dev runs NUT's dummy-ups on that
// file; edit `ups.status:` in it to drive OL → OB → LB. Only that env var creates a fake UPS.

import { existsSync, readdirSync } from "node:fs";
import type { Ctx, Driver, Props, UdevEvent } from "../core.ts";
import { HttpError, body, readText, run, usbName } from "../core.ts";
import { BUILTIN_IDS, decodeStatus, fmtRuntime, isPowerDeviceDescriptor, matchUps, parseNutUdevRules, parseUpsc, statusEvents, upsConf, upsdConf, upsdUsers, type UpsId } from "../lib/ups.ts";

const NUT = "/lib/nut";
const RULES = "/lib/udev/rules.d/62-nut-usbups.rules";
const PORT_OFFSET = parseInt(process.env.DESKYPI_PORT_OFFSET || "") || 0;
const DIR = `/tmp/deskypi-nut-${process.env.PORT || 80}`;
const SIM = process.env.DESKYPI_UPS_SIMULATE || "";
const POLL_MS = 5000;
const HIST_N = 120; // 10 minutes at 5 s
const MON_USER = "monuser";

interface Ups {
  key: string; // stable id: udev ID_SERIAL, else USB port path; "sim" for the simulator
  name: string; // NUT section name: deskypi, deskypi-2…
  driver: string;
  models: string; // what NUT's table says this ID covers
  vid: string;
  pid: string;
  vendor: string;
  product: string;
  serial: string;
  bus: string;
  usbPort: string;
  simulated: boolean;
  proc: ReturnType<Ctx["spawn"]> | null;
  driverLog: string[];
  vars: Record<string, string>;
  status: string; // raw ups.status
  reachable: boolean; // upsc got fresh data
  error: string;
  lastPoll: number;
  charge: number[];
  load: number[];
  events: { t: number; text: string }[];
}

let ctx: Ctx;
let table: UpsId[] = BUILTIN_IDS;
const upses = new Map<string, Ups>();
const powerHid = new Map<string, boolean>(); // hid DEVPATH -> descriptor says Power Device
let upsd: ReturnType<Ctx["spawn"]> | null = null;
let upsdLog: string[] = [];
let pollTimer: ReturnType<typeof setInterval> | null = null;
let rescanTimer: ReturnType<typeof setTimeout> | null = null;
let restartTimer: ReturnType<typeof setTimeout> | null = null;
let stopping = false;

const nutPort = () => ctx.settings.get("port", 3493 + PORT_OFFSET);
const shared = () => ctx.settings.get("share", false);
function monPassword(): string {
  let pw = ctx.settings.get("monPassword", "");
  if (!pw) {
    pw = crypto.randomUUID().replace(/-/g, "").slice(0, 16);
    ctx.settings.set("monPassword", pw);
  }
  return pw;
}

function tail(buf: string[], chunk: string) {
  for (const l of chunk.split("\n")) if (l.trim() && !/upsnotify|Init SSL/.test(l)) buf.push(l.trim());
  if (buf.length > 20) buf.splice(0, buf.length - 20);
}

async function pipeLog(stream: ReadableStream<Uint8Array> | null | undefined, buf: string[]) {
  if (!stream) return;
  const dec = new TextDecoder();
  for await (const c of stream) tail(buf, dec.decode(c));
}

// ---------- Detection ----------

async function hidIsPower(p: Props): Promise<boolean> {
  if (powerHid.has(p.DEVPATH)) return powerHid.get(p.DEVPATH)!;
  let yes = false;
  try {
    yes = isPowerDeviceDescriptor(new Uint8Array(await Bun.file(`/sys${p.DEVPATH}/report_descriptor`).arrayBuffer()));
  } catch {}
  powerHid.set(p.DEVPATH, yes);
  return yes;
}

async function detect(): Promise<Map<string, Omit<Ups, "name">>> {
  const found = new Map<string, Omit<Ups, "name">>();
  const add = (usb: Props, id: UpsId | null) => {
    const key = usb.ID_SERIAL || `port:${usb.DEVPATH.split("/").pop()}`;
    if (found.has(key)) return;
    const { vendor, model } = usbName(usb);
    let driver = id?.driver || "usbhid-ups";
    if (!existsSync(`${NUT}/${driver}`)) driver = "usbhid-ups"; // e.g. apc_modbus isn't built into Debian's NUT
    found.set(key, blank({
      key, driver, models: id?.models || "HID Power Device", vid: usb.ID_VENDOR_ID, pid: usb.ID_MODEL_ID, vendor, product: model,
      serial: usb.ID_SERIAL_SHORT || "", bus: (usb.BUSNUM || "").padStart(3, "0"), usbPort: usb.DEVPATH.split("/").pop() || "", simulated: false,
    }));
  };
  for (const usb of ctx.devices((p) => p.SUBSYSTEM === "usb" && p.DEVTYPE === "usb_device")) {
    const id = matchUps(usb.ID_VENDOR_ID, usb.ID_MODEL_ID, table);
    if (id) add(usb, id);
  }
  // Vendors nobody listed: any HID interface whose report descriptor is the Power Device class.
  for (const hid of ctx.devices((p) => p.SUBSYSTEM === "hid")) {
    if (!(await hidIsPower(hid))) continue;
    const usb = ctx.ancestor(hid.DEVPATH, "usb", "usb_device");
    if (usb) add(usb, null);
  }
  if (SIM) {
    found.set("sim", blank({ key: "sim", driver: "dummy-ups", models: "Simulated (DESKYPI_UPS_SIMULATE)", vid: "", pid: "", vendor: "DeskyPi", product: "Simulated UPS", serial: "", bus: "", usbPort: "", simulated: true }));
  }
  return found;
}

function blank(u: Pick<Ups, "key" | "driver" | "models" | "vid" | "pid" | "vendor" | "product" | "serial" | "bus" | "usbPort" | "simulated">): Omit<Ups, "name"> {
  return { ...u, proc: null, driverLog: [], vars: {}, status: "", reachable: false, error: "", lastPoll: 0, charge: [], load: [], events: [] };
}

async function rescan() {
  const found = await detect();
  let changed = false;
  for (const [key, u] of found) {
    if (upses.has(key)) continue;
    // Names are stable per run: the first UPS is "deskypi", as clients elsewhere expect.
    const used = new Set([...upses.values()].map((x) => x.name));
    let name = "deskypi";
    for (let i = 2; used.has(name); i++) name = `deskypi-${i}`;
    upses.set(key, { ...u, name });
    ctx.event(`UPS connected: ${u.vendor} ${u.product} (${u.simulated ? "simulated" : `${u.vid}:${u.pid}`}, NUT driver ${u.driver})`);
    changed = true;
  }
  for (const [key, u] of upses) {
    if (found.has(key)) continue;
    u.proc?.kill();
    upses.delete(key);
    ctx.event(`UPS disconnected: ${u.vendor} ${u.product}`);
    changed = true;
  }
  if (changed) scheduleRestart();
  ctx.changed();
}

function scheduleRescan() {
  if (rescanTimer) clearTimeout(rescanTimer);
  rescanTimer = setTimeout(() => rescan().catch((e) => ctx.log("rescan:", e)), 1000);
}

// ---------- NUT children ----------

const nutEnv = () => ["env", `NUT_CONFPATH=${DIR}/conf`, `NUT_STATEPATH=${DIR}/state`];

async function writeConfig() {
  await run(["mkdir", "-p", `${DIR}/conf`, `${DIR}/state`]);
  const sections = [...upses.values()].map((u) => ({
    name: u.name, driver: u.driver, port: u.simulated ? SIM : "auto",
    vid: u.simulated ? undefined : u.vid, pid: u.simulated ? undefined : u.pid,
    serial: u.serial || undefined, bus: u.serial ? undefined : u.bus || undefined,
    desc: `${u.vendor} ${u.product}`.trim(),
  }));
  await Bun.write(`${DIR}/conf/ups.conf`, upsConf(sections));
  await Bun.write(`${DIR}/conf/upsd.conf`, upsdConf(nutPort(), shared()));
  await Bun.write(`${DIR}/conf/upsd.users`, upsdUsers(MON_USER, monPassword()));
  await Bun.write(`${DIR}/conf/nut.conf`, "MODE=standalone\n");
}

/** Kill NUT processes a previous DeskyPi run left behind (it was killed hard, say). -FF wrote their pid files. */
async function killStale() {
  const files: string[] = [];
  try {
    for (const f of readdirSync(`${DIR}/state`)) if (f.endsWith(".pid")) files.push(`${DIR}/state/${f}`);
  } catch {}
  if (!files.length) return;
  // Only kill a pid that is still a NUT program; after a reboot the number may belong to anything.
  const pids: string[] = [];
  for (const pid of await Promise.all(files.map(readText))) {
    const comm = /^\d+$/.test(pid) ? await readText(`/proc/${pid}/comm`) : "";
    if (comm && existsSync(`${NUT}/${comm}`)) pids.push(pid);
  }
  if (pids.length) {
    ctx.log("killing stale NUT processes", pids.join(" "));
    await run(["sudo", "-n", "kill", ...pids]);
    await Bun.sleep(500);
  }
  await run(["sudo", "-n", "rm", "-f", ...files]);
}

async function stopNut() {
  const procs = [upsd, ...[...upses.values()].map((u) => u.proc)].filter(Boolean) as ReturnType<Ctx["spawn"]>[];
  upsd = null;
  for (const u of upses.values()) u.proc = null;
  for (const p of procs) p.kill();
  await Promise.race([Promise.all(procs.map((p) => p.exited)), Bun.sleep(5000)]);
}

function startDriver(u: Ups) {
  // Drivers need root for the USB device; -u root stops them dropping to the `nut` user,
  // which couldn't write our temp state dir. -FF: foreground, but still write a pid file.
  const p = ctx.spawn(["sudo", "-n", ...nutEnv(), `${NUT}/${u.driver}`, "-a", u.name, "-FF", "-u", "root"], { stdout: "pipe", stderr: "pipe" });
  u.proc = p;
  u.driverLog = [];
  pipeLog(p.stdout as any, u.driverLog);
  pipeLog(p.stderr as any, u.driverLog);
  p.exited.then((code) => {
    if (u.proc !== p || stopping) return;
    u.proc = null;
    u.error = `NUT driver ${u.driver} exited (${code}): ${u.driverLog.slice(-2).join(" / ") || "no output"}`;
    ctx.log(u.error);
    ctx.changed();
    // Retry: a UPS that was just plugged in sometimes isn't ready for the first claim.
    setTimeout(() => upses.get(u.key) === u && !u.proc && !stopping && startDriver(u), 30_000);
  });
}

function startUpsd() {
  const p = ctx.spawn(["sudo", "-n", ...nutEnv(), `${NUT}/upsd`, "-FF", "-u", "root"], { stdout: "pipe", stderr: "pipe" });
  upsd = p;
  upsdLog = [];
  pipeLog(p.stdout as any, upsdLog);
  pipeLog(p.stderr as any, upsdLog);
  p.exited.then((code) => {
    if (upsd !== p || stopping) return;
    upsd = null;
    ctx.log(`upsd exited (${code}):`, upsdLog.slice(-3).join(" / "));
    ctx.changed();
  });
}

async function restartNut() {
  await stopNut();
  if (!upses.size || stopping) return ctx.changed();
  await writeConfig();
  for (const u of upses.values()) {
    u.error = "";
    startDriver(u);
  }
  // upsd connects to the drivers' sockets at start and retries later, so a short head start is enough.
  await Bun.sleep(1500);
  if (!stopping) startUpsd();
  ctx.changed();
}

function scheduleRestart() {
  if (restartTimer) clearTimeout(restartTimer);
  restartTimer = setTimeout(() => restartNut().catch((e) => ctx.log("restart:", e)), 300);
}

// ---------- Polling ----------

async function pollOne(u: Ups) {
  const r = await run(["upsc", `${u.name}@127.0.0.1:${nutPort()}`], { timeoutMs: 4000 });
  u.lastPoll = Date.now();
  const wasReachable = u.reachable;
  if (r.code !== 0) {
    u.reachable = false;
    const msg = (r.err || r.out).replace(/Init SSL without certificate database\s*/g, "").trim();
    if (u.proc && upsd) u.error = /stale|not connected/i.test(msg) ? "NUT driver isn't getting data from the UPS (Data stale)" : msg;
    if (wasReachable) logEvent(u, "Lost contact with the UPS");
    return;
  }
  u.vars = parseUpsc(r.out);
  u.reachable = true;
  u.error = "";
  if (!wasReachable && u.status) logEvent(u, "Contact with the UPS restored");
  const st = u.vars["ups.status"] || "";
  for (const e of statusEvents(u.status, st)) logEvent(u, e);
  u.status = st;
  const push = (arr: number[], v: string | undefined) => {
    if (v == null || v === "" || !Number.isFinite(+v)) return;
    arr.push(+v);
    if (arr.length > HIST_N) arr.shift();
  };
  push(u.charge, u.vars["battery.charge"]);
  push(u.load, u.vars["ups.load"]);
}

function logEvent(u: Ups, text: string) {
  u.events.push({ t: Date.now(), text });
  if (u.events.length > 50) u.events.shift();
  ctx.event(`UPS ${u.vars["device.model"] || u.vars["ups.model"] || u.product || u.name}: ${text}`, { ups: u.name, status: u.vars["ups.status"] });
}

async function poll() {
  if (!upsd) return;
  await Promise.all([...upses.values()].map((u) => pollOne(u).catch(() => {})));
  ctx.changed();
}

// ---------- Presentation ----------

const num = (v: string | undefined) => (v == null || v === "" || !Number.isFinite(+v) ? null : +v);

function view(u: Ups) {
  const v = u.vars;
  const runtime = num(v["battery.runtime"]);
  return {
    name: u.name,
    key: u.key,
    simulated: u.simulated,
    vendor: v["device.mfr"] || v["ups.mfr"] || u.vendor,
    model: v["device.model"] || v["ups.model"] || u.product,
    serial: v["device.serial"] || v["ups.serial"] || u.serial,
    firmware: v["ups.firmware"] || v["ups.firmware.aux"] || "",
    usb: u.simulated ? "" : `${u.vid}:${u.pid} on port ${u.usbPort}`,
    nutDriver: u.driver,
    driverRunning: !!u.proc,
    reachable: u.reachable,
    error: u.error,
    status: u.status,
    flags: decodeStatus(u.status),
    charge: num(v["battery.charge"]),
    runtime,
    runtimeText: runtime != null ? fmtRuntime(runtime) : "",
    load: num(v["ups.load"]),
    realpower: num(v["ups.realpower"]) ?? (num(v["ups.realpower.nominal"]) != null && num(v["ups.load"]) != null ? Math.round((num(v["ups.realpower.nominal"])! * num(v["ups.load"])!) / 100) : null),
    inputVoltage: num(v["input.voltage"]),
    outputVoltage: num(v["output.voltage"]),
    inputFrequency: num(v["input.frequency"]),
    batteryVoltage: num(v["battery.voltage"]),
    batteryDate: v["battery.mfr.date"] || v["battery.date"] || "",
    batteryType: v["battery.type"] || "",
    lowRuntime: num(v["battery.runtime.low"]),
    lowCharge: num(v["battery.charge.low"]),
    beeper: v["ups.beeper.status"] || "",
    lastPoll: u.lastPoll,
    history: { charge: u.charge, load: u.load },
    events: u.events.slice(-20),
  };
}

function ups(name: string): Ups {
  const u = [...upses.values()].find((x) => x.name === name || x.key === name);
  if (!u) throw new HttpError(404, `No UPS "${name}"${upses.size ? "" : " (none connected)"}`);
  return u;
}

function nutInfo() {
  return {
    running: !!upsd,
    port: nutPort(),
    shared: shared(),
    user: MON_USER,
    // Only worth showing when someone else can connect.
    password: shared() ? monPassword() : undefined,
    log: upsd ? undefined : upsdLog.slice(-3),
    autoShutdown: false,
  };
}

// ---------- Driver ----------

const driver: Driver = {
  id: "ups",
  title: "UPS",
  order: 70,
  description: "USB UPS via NUT (run privately by DeskyPi): battery, runtime, load, power events, optional network sharing for NUT clients",

  async init(c) {
    ctx = c;
    stopping = false;
    const rules = await readText(RULES);
    if (rules) table = parseNutUdevRules(rules);
    await killStale();
    await rescan();
    pollTimer = setInterval(() => poll().catch(() => {}), POLL_MS);
  },

  async stop() {
    stopping = true;
    if (pollTimer) clearInterval(pollTimer);
    if (restartTimer) clearTimeout(restartTimer);
    await stopNut();
    upses.clear();
  },

  udev(ev: UdevEvent) {
    const s = ev.props.SUBSYSTEM;
    if ((s === "usb" && ev.props.DEVTYPE === "usb_device") || s === "hid") {
      if (ev.action === "remove") powerHid.delete(ev.props.DEVPATH);
      if (ev.action === "add" || ev.action === "remove" || ev.action === "bind") scheduleRescan();
    }
  },

  status() {
    const list = [...upses.values()].map(view);
    const u = list[0];
    let summary = "No UPS";
    if (u) {
      const words = u.flags.filter((f) => ["OL", "OB", "LB", "RB", "BYPASS", "OVER"].includes(f.flag)).map((f) => f.word).join(", ");
      summary = [u.model, u.reachable ? words || u.status : "no data", u.charge != null ? `${u.charge}%` : "", u.runtimeText, u.load != null ? `${u.load}% load` : ""].filter(Boolean).join(" · ");
      if (list.length > 1) summary += ` (+${list.length - 1} more)`;
    }
    return { active: list.length > 0, summary, upses: list, nut: nutInfo() };
  },

  routes: [
    {
      method: "GET",
      path: "/api/ups",
      doc: "UPSes attached to DeskyPi with decoded status (On mains / On battery / Low battery…), charge %, runtime, load, voltages, recent power events; plus the private NUT server's port and sharing state",
      handler: () => ({ upses: [...upses.values()].map(view), nut: nutInfo() }),
    },
    {
      method: "GET",
      path: "/api/ups/:name",
      doc: "Every variable NUT reports for one UPS (raw `upsc` output as JSON); :name is the NUT name, e.g. deskypi",
      handler: (_req, p) => {
        const u = ups(p.name);
        return { ...view(u), vars: u.vars };
      },
    },
    {
      method: "POST",
      path: "/api/ups/share",
      doc: { body: { share: true, port: 3493 }, description: "Share on network: upsd listens on 0.0.0.0:port so NUT clients elsewhere can monitor (read-only user monuser, password shown in GET /api/ups while shared). Off = 127.0.0.1 only" },
      async handler(req) {
        const b = await body(req);
        if (b.port != null) {
          const port = +b.port;
          if (!Number.isInteger(port) || port < 1 || port > 65535) throw new HttpError(400, "port must be 1–65535");
          ctx.settings.set("port", port);
        }
        if (b.share != null) ctx.settings.set("share", !!b.share);
        ctx.event(`UPS network sharing ${shared() ? `on (0.0.0.0:${nutPort()})` : "off"}`);
        await restartNut();
        return { ok: true, nut: nutInfo() };
      },
    },
    {
      method: "POST",
      path: "/api/ups/restart",
      doc: "Restart the private NUT driver(s) and upsd (e.g. after the UPS stopped answering)",
      async handler() {
        await rescan();
        await restartNut();
        return { ok: true, nut: nutInfo() };
      },
    },
  ],

  tools: [
    {
      name: "ups_status",
      description: "Status of the UPS(es) attached to DeskyPi via NUT: whether it's on mains or battery (decoded flags), battery charge %, estimated runtime, load %, input/output voltage and frequency, model/serial/firmware, battery date, and recent power events. Automatic shutdown is not configured.",
      inputSchema: { type: "object", properties: { name: { type: "string", description: "NUT UPS name (default: all). The first UPS is 'deskypi'." }, raw: { type: "boolean", description: "Also include every raw NUT variable" } } },
      handler(args) {
        const list = args.name ? [ups(args.name)] : [...upses.values()];
        if (!list.length) return { upses: [], note: "No UPS connected" };
        return { upses: list.map((u) => ({ ...view(u), history: undefined, ...(args.raw ? { vars: u.vars } : {}) })), nut: nutInfo() };
      },
    },
  ],
};

export default driver;
