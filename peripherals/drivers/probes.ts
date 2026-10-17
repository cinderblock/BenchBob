// ==================== Debug probes + MCU bootloaders ====================
//
// Spots debug probes (ST-Link, J-Link, CMSIS-DAP, ESP-Prog, ESP USB-JTAG, Black Magic Probe,
// WCH-Link, Atmel-ICE) and chips sitting in their bootloaders (RP2 BOOTSEL, STM32 DFU), then
// turns each probe into a LAN GDB server by running OpenOCD as a managed child.
//
// Every action (detect, flash, erase, reset, memory reads) goes through OpenOCD's TCL RPC
// port. If no GDB server is running for the probe, a throwaway OpenOCD is started on a
// loopback-only port for the duration of the action, so there is one code path and one log
// parser either way, and a running server is never fought over by a second OpenOCD.
//
// Pure logic (device table, command lines, log parsing, UF2) lives in ../lib/probes.ts.

import os from "node:os";
import { mkdir, rm, readdir } from "node:fs/promises";
import type { ServerWebSocket, Subprocess } from "bun";
import { HttpError, body, readText, run, usbName, type Ctx, type Driver, type Props, type UdevEvent, type WsData } from "../core.ts";
import * as L from "../lib/probes.ts";

const SCRIPTS = "/usr/share/openocd/scripts";
// Side instances shift every OpenOCD port so they never collide with the live service.
const PORT_OFFSET = parseInt(process.env.DESKYPI_PORT_OFFSET || "") || 0;
// Simulated devices for testing without hardware; the value is a scratch dir (fake BOOTSEL drive).
const SIM = process.env.DESKYPI_PROBES_SIM || "";
const WEB_PORT = parseInt(process.env.PORT || "") || 80;
// Per instance: init clears it, which mustn't eat another instance's in-flight upload.
const TMP = `/var/tmp/deskypi-probes-${WEB_PORT}`;
// sudo-installed tools live in /usr/sbin, which a service's PATH may lack.
const PATH = `${process.env.PATH || "/usr/bin:/bin"}:/usr/local/sbin:/usr/sbin:/sbin`;
const LOG_MAX = 400;
const UPLOAD_MAX = 64 << 20;
const BASE_PORTS: L.Ports = { gdb: 3333, telnet: 4444, tcl: 6666 };

let ctx: Ctx;

// ---------- State ----------

interface Session {
  proc: Subprocess<"ignore", "pipe", "pipe">;
  persistent: boolean; // the user's GDB server, as opposed to a one-shot action
  ports: { gdb: number | null; telnet: number | null; tcl: number };
  target: string;
  state: "starting" | "running" | "stopping" | "exited";
  stopped: boolean; // ended on request rather than by itself
  since: number;
  exitCode: number | null;
  tcl: L.TclClient;
  ready: Promise<void>;
}

interface Job {
  kind: string;
  stage: string;
  started: number;
  done?: number;
  ok?: boolean;
  message?: string;
  pct?: number;
}

interface Dev {
  id: string;
  devpath: string;
  props: Props;
  vid: string;
  pid: string;
  serial: string;
  vendor: string;
  model: string;
  log: string[];
  sockets: Set<ServerWebSocket<WsData>>;
  job: Job | null;
  lastJob: Job | null;
  sim?: boolean;
}

interface Probe extends Dev {
  type: L.ProbeType;
  ttys: { dev: string; iface: string }[];
  session: Session | null;
  ocd: L.OcdState;
}

interface Boot extends Dev {
  type: L.BootType;
  block: string; // /dev/sdX1 holding the BOOTSEL FAT
  label: string;
  mount: string;
  ro: boolean;
  privateMount: string; // our own rw mount when the drive handler hadn't mounted it
  info: { bootloader: string; fields: Record<string, string> } | null;
  ttys: { dev: string; iface: string }[];
}

const probes = new Map<string, Probe>();
const boots = new Map<string, Boot>();
let openocd = "";
let openocdVersion = "";
let tools = { dfuUtil: "", picotool: "", gdb: "" };
let targets: string[] = [];
let scanTimer: ReturnType<typeof setTimeout> | null = null;
let pollTimer: ReturnType<typeof setInterval> | null = null;
let ifaceStrings = new Map<string, string[]>(); // devpath -> USB interface strings

// ---------- Logging + streaming ----------

function send(d: Dev, msg: any) {
  const s = JSON.stringify(msg);
  for (const ws of d.sockets) ws.send(s);
}

function logLine(d: Dev, text: string) {
  d.log.push(text);
  if (d.log.length > LOG_MAX) d.log.splice(0, d.log.length - LOG_MAX);
  send(d, { type: "line", text });
}

function setJob(d: Dev, patch: Partial<Job> | null) {
  if (patch === null) d.job = null;
  else d.job = { ...(d.job || { kind: "", stage: "", started: Date.now() }), ...patch };
  send(d, { type: "job", job: d.job, lastJob: d.lastJob });
  ctx.changed();
}

/** Run a long action as the device's one job; records the outcome for the card. */
async function job<T>(d: Dev, kind: string, fn: () => Promise<T>): Promise<T> {
  if (d.job) throw new HttpError(409, `Busy: ${d.job.kind} in progress`);
  setJob(d, { kind, stage: "starting", started: Date.now() });
  try {
    const r = await fn();
    d.lastJob = { ...d.job!, stage: "done", done: Date.now(), ok: true, message: typeof (r as any)?.message === "string" ? (r as any).message : "" };
    return r;
  } catch (e: any) {
    d.lastJob = { ...d.job!, stage: "failed", done: Date.now(), ok: false, message: e?.message || String(e) };
    logLine(d, `!! ${kind} failed: ${d.lastJob.message}`);
    throw e;
  } finally {
    setJob(d, null);
  }
}

async function eachLine(stream: ReadableStream<Uint8Array>, cb: (l: string) => void) {
  const dec = new TextDecoder();
  let buf = "";
  for await (const chunk of stream as any) {
    buf += dec.decode(chunk, { stream: true });
    let i;
    while ((i = buf.search(/[\r\n]/)) >= 0) {
      const l = buf.slice(0, i);
      buf = buf.slice(i + 1);
      if (l.trim()) cb(l);
    }
  }
  if (buf.trim()) cb(buf);
}

// ---------- Settings ----------

interface ProbeConfig {
  target: string;
  transport: L.Transport;
  speed: number;
  ports: L.Ports; // as configured; PORT_OFFSET is added when spawning
}

/** Each probe gets a slot so its default ports stay put: 3333/4444/6666, then +10 per slot. */
function slotFor(id: string): number {
  const slots = ctx.settings.get<Record<string, number>>("slots", {});
  if (slots[id] !== undefined) return slots[id];
  const used = new Set(Object.values(slots));
  let n = 0;
  while (used.has(n)) n++;
  ctx.settings.set("slots", { ...slots, [id]: n });
  return n;
}

function defaultTarget(t: L.ProbeType): string {
  if (t.kind === "sim-dummy") return L.NO_TARGET;
  if (t.kind === "esp-usb-jtag") return "target/esp32s3.cfg";
  if (t.kind === "ftdi") return "target/esp32.cfg";
  if (t.name.startsWith("Raspberry Pi Debug Probe")) return "target/rp2040.cfg";
  return L.GENERIC_TARGET;
}

function config(p: Probe): ProbeConfig {
  const saved = ctx.settings.get<Partial<ProbeConfig>>(`probe:${p.id}`, {});
  const slot = slotFor(p.id);
  const target = saved.target && validTarget(saved.target) ? saved.target : defaultTarget(p.type);
  const transport = saved.transport && p.type.transports.includes(saved.transport) ? saved.transport : L.defaultTransport(p.type, target);
  return {
    target,
    transport,
    speed: saved.speed || L.defaultSpeed(p.type, target),
    ports: { gdb: BASE_PORTS.gdb + 10 * slot, telnet: BASE_PORTS.telnet + 10 * slot, tcl: BASE_PORTS.tcl + 10 * slot, ...saved.ports },
  };
}

function validTarget(t: string) {
  return t === L.GENERIC_TARGET || t === L.NO_TARGET || targets.includes(t);
}

function effectivePorts(c: ProbeConfig): L.Ports {
  return { gdb: c.ports.gdb + PORT_OFFSET, telnet: c.ports.telnet + PORT_OFFSET, tcl: c.ports.tcl + PORT_OFFSET };
}

function setConfig(p: Probe, b: any): ProbeConfig {
  const cur = config(p);
  const next: ProbeConfig = { ...cur, ports: { ...cur.ports } };
  if (b.target !== undefined) {
    if (!validTarget(b.target)) throw new HttpError(400, `Unknown target script "${b.target}" (see GET /api/probes/targets)`);
    next.target = b.target;
    // A target change resets transport/speed to what suits it unless also given.
    if (b.transport === undefined) next.transport = L.defaultTransport(p.type, next.target);
    if (b.speed === undefined) next.speed = L.defaultSpeed(p.type, next.target);
  }
  if (b.transport !== undefined) {
    if (!p.type.transports.includes(b.transport)) throw new HttpError(400, `${p.type.name} supports ${p.type.transports.join("/")} only`);
    next.transport = b.transport;
  }
  if (b.speed !== undefined) {
    const s = L.parseNum(b.speed);
    if (s === null || s < 1 || s > 100000) throw new HttpError(400, "speed is in kHz, 1–100000");
    next.speed = s;
  }
  for (const k of ["gdb", "telnet", "tcl"] as const) {
    if (b.ports?.[k] === undefined) continue;
    const n = L.parseNum(b.ports[k]);
    if (n === null || n < 1024 || n > 65000) throw new HttpError(400, `${k} port must be 1024–65000`);
    next.ports[k] = n;
  }
  const ps = [next.ports.gdb, next.ports.telnet, next.ports.tcl];
  if (new Set(ps).size !== 3) throw new HttpError(400, "The three ports must differ");
  for (const o of probes.values()) {
    if (o.id === p.id || !o.type.iface) continue;
    const oc = config(o);
    if ([oc.ports.gdb, oc.ports.telnet, oc.ports.tcl].some((x) => ps.includes(x))) throw new HttpError(400, `Port clash with ${o.type.name} (${o.id})`);
  }
  ctx.settings.set(`probe:${p.id}`, next);
  ctx.changed();
  return next;
}

// ---------- Detection ----------

function simDevices(): Props[] {
  const usb = (n: string, vid: string, pid: string, model: string, serial: string, extra: Props = {}): Props => ({
    DEVPATH: `/devices/sim/usb9/9-1.${n}`,
    SUBSYSTEM: "usb",
    DEVTYPE: "usb_device",
    ID_VENDOR_ID: vid,
    ID_MODEL_ID: pid,
    ID_MODEL: model.replace(/ /g, "_"),
    ID_MODEL_ENC: model.replace(/ /g, "\\x20"),
    ID_SERIAL_SHORT: serial,
    ID_SERIAL: `${model.replace(/ /g, "_")}_${serial}`,
    DESKYPI_SIM: "1",
    ...extra,
  });
  const tty = (n: string, iface: string, dev: string): Props => ({
    DEVPATH: `/devices/sim/usb9/9-1.${n}/9-1.${n}:1.${+iface}/tty/${dev}`,
    SUBSYSTEM: "tty",
    DEVNAME: `/dev/${dev}`,
    ID_USB_INTERFACE_NUM: iface,
  });
  return [
    usb("1", "0483", "374b", "STM32 STLink", "066DFF383133524157185615"),
    usb("2", "2e8a", "000c", "Debugprobe on Pico (CMSIS-DAP)", "E6614C311B597D32"),
    usb("3", "1d50", "6018", "Black Magic Probe v1.10.0", "7BB180B4"),
    tty("3", "00", "ttyACM90"),
    tty("3", "02", "ttyACM91"),
    usb("4", "303a", "1001", "USB JTAG/serial debug unit", "F4:12:FA:5C:3B:A8"),
    tty("4", "00", "ttyACM92"),
    usb("5", "2e8a", "0003", "RP2 Boot", "E0C9125B0D9B"),
    { DEVPATH: "/devices/sim/usb9/9-1.5/host0/block/sdz/sdz1", SUBSYSTEM: "block", DEVTYPE: "partition", DEVNAME: "/dev/sdz1", ID_FS_TYPE: "vfat", ID_FS_LABEL: "RPI-RP2" },
    usb("6", "0483", "df11", "STM32 BOOTLOADER", "205D35A3424B"),
    // Not a real probe: OpenOCD's dummy adapter, so the server lifecycle and TCL path run end to end.
    usb("7", "dead", "beef", "DeskyPi dummy probe", "SIM0001"),
  ];
}

function allDevices(): Props[] {
  return SIM ? [...ctx.devices(), ...simDevices()] : ctx.devices();
}

function classify(p: Props): { probe?: L.ProbeType; boot?: L.BootType } | null {
  let vid = (p.ID_VENDOR_ID || "").toLowerCase();
  let pid = (p.ID_MODEL_ID || "").toLowerCase();
  if (!vid && p.PRODUCT) {
    // PRODUCT=483/3748/100 (hex, unpadded)
    const [v, d] = p.PRODUCT.split("/");
    vid = (v || "").padStart(4, "0");
    pid = (d || "").padStart(4, "0");
  }
  if (!vid) return null;
  if (p.DESKYPI_SIM && vid === "dead")
    return { probe: { kind: "sim-dummy", name: "Dummy probe (simulated)", iface: "interface/dummy.cfg", transports: ["jtag"], note: "OpenOCD's dummy adapter, for testing DeskyPi itself." } };
  const boot = L.bootType(vid, pid);
  if (boot) return { boot };
  const probe = L.probeType(vid, pid, [...L.usbStrings(p), ...(ifaceStrings.get(p.DEVPATH) || [])]);
  return probe ? { probe } : null;
}

/** USB interface strings live only in sysfs; CMSIS-DAP probes put "CMSIS-DAP" there. */
async function readIfaceStrings(devpath: string): Promise<string[]> {
  try {
    const dir = `/sys${devpath}`;
    const subs = (await readdir(dir)).filter((n) => /^\d+-[\d.]+:\d+\.\d+$/.test(n));
    return (await Promise.all(subs.map((s) => readText(`${dir}/${s}/interface`)))).filter(Boolean);
  } catch {
    return [];
  }
}

function childTtys(all: Props[], devpath: string) {
  return all
    .filter((t) => t.SUBSYSTEM === "tty" && t.DEVNAME && t.DEVPATH.startsWith(devpath + "/"))
    .map((t) => ({ dev: t.DEVNAME, iface: t.ID_USB_INTERFACE_NUM || "" }))
    .sort((a, b) => a.iface.localeCompare(b.iface));
}

function newDev(p: Props, id: string) {
  const n = usbName(p);
  return {
    id,
    devpath: p.DEVPATH,
    props: p,
    vid: (p.ID_VENDOR_ID || "").toLowerCase(),
    pid: (p.ID_MODEL_ID || "").toLowerCase(),
    serial: p.ID_SERIAL_SHORT || "",
    vendor: n.vendor,
    model: n.model,
    log: [] as string[],
    sockets: new Set<ServerWebSocket<WsData>>(),
    job: null,
    lastJob: null,
    sim: !!p.DESKYPI_SIM,
  };
}

async function scan() {
  const all = allDevices();
  const usb = all.filter((p) => p.SUBSYSTEM === "usb" && p.DEVTYPE === "usb_device");
  // Interface strings only matter for devices the VID:PID table doesn't already know.
  for (const p of usb) if (!ifaceStrings.has(p.DEVPATH) && !p.DESKYPI_SIM) ifaceStrings.set(p.DEVPATH, await readIfaceStrings(p.DEVPATH));
  const seenP = new Set<string>();
  const seenB = new Set<string>();
  for (const p of usb) {
    const c = classify(p);
    if (!c) continue;
    const id = L.stableId(p);
    if (c.probe) {
      seenP.add(id);
      let pr = probes.get(id);
      if (!pr) {
        pr = { ...newDev(p, id), type: c.probe, ttys: [], session: null, ocd: L.newOcdState() };
        probes.set(id, pr);
        if (!pr.sim) ctx.event(`Debug probe connected: ${c.probe.name}${pr.serial ? ` (${pr.serial})` : ""}`, { id, vid: pr.vid, pid: pr.pid });
      }
      pr.devpath = p.DEVPATH;
      pr.props = p;
      pr.ttys = childTtys(all, p.DEVPATH);
    } else if (c.boot) {
      seenB.add(id);
      let b = boots.get(id);
      if (!b) {
        b = { ...newDev(p, id), type: c.boot, block: "", label: "", mount: "", ro: false, privateMount: "", info: null, ttys: [] };
        boots.set(id, b);
        if (!b.sim) ctx.event(`${c.boot.name} connected`, { id });
      }
      b.devpath = p.DEVPATH;
      const parts = all.filter((x) => x.SUBSYSTEM === "block" && x.DEVPATH.startsWith(p.DEVPATH + "/") && x.ID_FS_TYPE);
      const part = parts.find((x) => x.DEVTYPE === "partition") || parts[0];
      b.block = part?.DEVNAME || "";
      b.label = part?.ID_FS_LABEL || "";
    }
  }
  for (const [id, pr] of probes)
    if (!seenP.has(id)) {
      probes.delete(id);
      if (pr.session) stopSession(pr.session);
      for (const ws of pr.sockets) ws.close();
      ctx.event(`Debug probe removed: ${pr.type.name}`, { id });
    }
  for (const [id, b] of boots)
    if (!seenB.has(id)) {
      boots.delete(id);
      if (b.privateMount) run(["sudo", "umount", "-l", b.privateMount]).then(() => run(["sudo", "rmdir", b.privateMount]));
      for (const ws of b.sockets) ws.close();
      ctx.event(`${b.type.name} left bootloader mode`, { id });
    }
  for (const id of [...ifaceStrings.keys()]) if (!usb.some((p) => p.DEVPATH === id)) ifaceStrings.delete(id);
  await refreshMounts();
  // BOOTSEL drives get mounted a second or two after they appear; poll until they are.
  if (boots.size && !pollTimer) pollTimer = setInterval(() => refreshMounts().catch(() => {}), 2500);
  if (!boots.size && pollTimer) clearInterval(pollTimer), (pollTimer = null);
  ctx.changed();
}

function scheduleScan() {
  if (scanTimer) clearTimeout(scanTimer);
  scanTimer = setTimeout(() => {
    scanTimer = null;
    scan().catch((e) => ctx.log("scan failed:", e));
  }, 300);
}

/** /proc/mounts escapes spaces etc. as \040. */
function unescapeMount(s: string) {
  return s.replace(/\\([0-7]{3})/g, (_, o) => String.fromCharCode(parseInt(o, 8)));
}

async function refreshMounts() {
  const mounts = (await readText("/proc/mounts")).split("\n").map((l) => l.split(" "));
  let changed = false;
  for (const b of boots.values()) {
    if (b.type.kind !== "rp2-bootsel") continue;
    let mount = "",
      ro = false;
    if (b.sim) {
      mount = `${SIM}/${b.label}`;
      await mkdir(mount, { recursive: true });
      if (!(await Bun.file(`${mount}/INFO_UF2.TXT`).exists()))
        await Bun.write(`${mount}/INFO_UF2.TXT`, "UF2 Bootloader v3.0\r\nModel: Raspberry Pi RP2\r\nBoard-ID: RPI-RP2\r\n");
    } else if (b.block) {
      const m = mounts.find((f) => f[0] === b.block);
      if (m) (mount = unescapeMount(m[1])), (ro = (m[3] || "").split(",").includes("ro"));
    }
    if (mount !== b.mount || ro !== b.ro) {
      b.mount = mount;
      b.ro = ro;
      b.info = mount ? L.parseInfoUf2(await readText(`${mount}/INFO_UF2.TXT`)) : null;
      changed = true;
    }
  }
  if (changed) ctx.changed();
}

// ---------- OpenOCD sessions ----------

async function freePort(): Promise<number> {
  const l = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data() {} } });
  const port = l.port;
  l.stop(true);
  return port;
}

function ocdOptions(p: Probe, c: ProbeConfig, bindto: string, ports: Session["ports"]): L.OcdOptions {
  if (!p.type.iface) throw new HttpError(400, `${p.type.name}: ${p.type.note || "OpenOCD can't drive this probe"}`);
  return {
    iface: p.type.iface,
    target: c.target,
    transport: c.transport,
    speed: c.speed,
    pin: p.sim ? null : L.pinCommand(p.type.kind, p.pid, p.serial, p.devpath),
    bindto,
    ports,
  };
}

function spawnSession(p: Probe, o: L.OcdOptions, persistent: boolean): Session {
  if (!openocd) throw new HttpError(503, "openocd isn't installed (apt package: openocd)");
  const args = L.openocdArgs(o);
  p.ocd = L.newOcdState();
  logLine(p, `$ openocd ${args.map(L.shellQuote).join(" ")}`);
  const proc = ctx.spawn([openocd, ...args], { stdin: "ignore", stdout: "pipe", stderr: "pipe", env: { ...process.env, PATH } }) as Session["proc"];
  let initDone = false;
  let resolve!: () => void, reject!: (e: Error) => void;
  const s: Session = {
    proc,
    persistent,
    ports: o.ports,
    target: o.target,
    state: "starting",
    stopped: false,
    since: Date.now(),
    exitCode: null,
    tcl: new L.TclClient(o.ports.tcl),
    ready: new Promise<void>((res, rej) => ((resolve = res), (reject = rej))),
  };
  s.ready.catch(() => {}); // observed by whoever awaits it; never an unhandled rejection
  const onLine = (line: string) => {
    logLine(p, line);
    let vis = L.parseOcdLine(p.ocd, line);
    if (line.trim() === L.READY_MARK) initDone = true;
    if (s.state === "starting" && initDone && p.ocd.listening.tcl === o.ports.tcl) {
      s.state = "running";
      resolve();
      vis = true;
    }
    if (vis) ctx.changed();
  };
  eachLine(proc.stdout, onLine);
  eachLine(proc.stderr, onLine);
  proc.exited.then((code) => {
    s.exitCode = code;
    const was = s.state;
    s.state = "exited";
    s.tcl.close();
    logLine(p, `-- openocd exited (${code})`);
    const why = p.ocd.errors.at(-1) || `openocd exited with code ${code}`;
    reject(new Error(p.ocd.hint ? `${why} — ${p.ocd.hint}` : why));
    if (persistent && was !== "stopping" && p.session === s) ctx.event(`GDB server for ${p.type.name} stopped: ${why}`, { id: p.id });
    ctx.changed();
  });
  return s;
}

async function stopSession(s: Session) {
  if (s.state === "exited") return;
  s.state = "stopping";
  s.stopped = true;
  s.tcl.close();
  s.proc.kill("SIGTERM"); // OpenOCD shuts down cleanly on SIGTERM
  const t = await Promise.race([s.proc.exited.then(() => true), Bun.sleep(3000).then(() => false)]);
  if (!t) s.proc.kill("SIGKILL");
  await s.proc.exited;
}

async function startServer(p: Probe, over: any = {}) {
  if (p.session && p.session.state !== "exited") throw new HttpError(409, "A GDB server is already running for this probe");
  if (p.job) throw new HttpError(409, `Busy: ${p.job.kind} in progress`);
  const c = Object.keys(over).length ? setConfig(p, over) : config(p);
  const s = spawnSession(p, ocdOptions(p, c, "0.0.0.0", effectivePorts(c)), true);
  p.session = s;
  ctx.changed();
  try {
    await Promise.race([s.ready, Bun.sleep(20000).then(() => Promise.reject(new Error("OpenOCD didn't become ready within 20 s")))]);
  } catch (e: any) {
    if (s.state !== "exited") await stopSession(s);
    throw new HttpError(502, e.message);
  }
  ctx.event(`GDB server for ${p.type.name} on :${s.ports.gdb}`, { id: p.id, target: c.target });
  return probeStatus(p);
}

async function stopServer(p: Probe) {
  if (!p.session || p.session.state === "exited") return probeStatus(p);
  await stopSession(p.session);
  return probeStatus(p);
}

/** Run fn against the probe's TCL port: the GDB server's, or a throwaway loopback OpenOCD's. */
async function withTcl<T>(p: Probe, fn: (tcl: L.TclClient) => Promise<T>): Promise<T> {
  const s = p.session;
  if (s?.state === "running") return fn(s.tcl);
  if (s && (s.state === "starting" || s.state === "stopping")) throw new HttpError(409, `The GDB server is ${s.state}; try again in a moment`);
  setJob(p, { stage: "connecting" });
  const tmp = spawnSession(p, ocdOptions(p, config(p), "127.0.0.1", { gdb: null, telnet: null, tcl: await freePort() }), false);
  try {
    await Promise.race([tmp.ready, Bun.sleep(20000).then(() => Promise.reject(new Error("OpenOCD didn't become ready within 20 s")))]);
    return await fn(tmp.tcl);
  } catch (e: any) {
    throw e instanceof HttpError ? e : new HttpError(502, e.message);
  } finally {
    // Let the last log lines land before tearing down.
    try {
      await tmp.tcl.cmd("shutdown", { capture: false, timeoutMs: 3000 });
    } catch {}
    await stopSession(tmp);
  }
}

function tclErr(e: any): never {
  throw e instanceof L.TclError ? new HttpError(502, `OpenOCD: ${e.message}`) : e;
}

// ---------- Actions ----------

function probeById(id: string | undefined): Probe {
  if (!id) {
    if (probes.size === 1) return [...probes.values()][0];
    throw new HttpError(400, probes.size ? `Several probes are connected; give one of: ${[...probes.keys()].join(", ")}` : "No debug probe connected");
  }
  const p = probes.get(id) || [...probes.values()].find((x) => x.serial === id);
  if (!p) throw new HttpError(404, `No probe "${id}"`);
  return p;
}

function bootById(id: string | undefined): Boot {
  if (!id) {
    if (boots.size === 1) return [...boots.values()][0];
    throw new HttpError(400, boots.size ? `Several bootloader devices; give one of: ${[...boots.keys()].join(", ")}` : "No device in bootloader mode");
  }
  const b = boots.get(id) || [...boots.values()].find((x) => x.serial === id);
  if (!b) throw new HttpError(404, `No bootloader device "${id}"`);
  return b;
}

function needOcd(p: Probe) {
  if (p.type.gdbBuiltIn) throw new HttpError(400, `${p.type.name} is its own GDB server: connect GDB to ${p.ttys[0]?.dev || "its first serial port"}`);
}

async function examine(p: Probe) {
  needOcd(p);
  return job(p, "detect", () =>
    withTcl(p, async (tcl) => {
      setJob(p, { stage: "examining" });
      const out = await tcl.cmd("targets").catch(tclErr);
      return { targets: out.trim(), adapter: p.ocd.adapter, voltage: p.ocd.voltage, dpidr: p.ocd.dpidr, cpus: p.ocd.cpus, flash: p.ocd.flash, errors: p.ocd.errors, hint: p.ocd.hint, message: p.ocd.cpus.join(", ") || "no core detected" };
    }),
  );
}

async function saveUpload(buf: Uint8Array, name: string): Promise<string> {
  if (!buf.length) throw new HttpError(400, "Empty file");
  if (buf.length > UPLOAD_MAX) throw new HttpError(413, `File too large (max ${UPLOAD_MAX >> 20} MB)`);
  await mkdir(TMP, { recursive: true });
  const ext = (/\.[A-Za-z0-9]{1,5}$/.exec(name)?.[0] || "").toLowerCase();
  const path = `${TMP}/${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}${ext}`;
  await Bun.write(path, buf);
  return path;
}

async function flash(p: Probe, buf: Uint8Array, o: { name?: string; address?: string | number | null; verify?: boolean; reset?: boolean }) {
  needOcd(p);
  const name = o.name || "firmware.bin";
  const kind = L.firmwareKind(buf, name);
  if (kind === "uf2") throw new HttpError(400, "That's a UF2 file: put the chip in BOOTSEL/bootloader mode and use the UF2 flash instead");
  const addr = L.parseNum(o.address as any);
  if (o.address != null && o.address !== "" && addr === null) throw new HttpError(400, `Bad address "${o.address}"`);
  if (kind === "bin" && addr === null) throw new HttpError(400, "A .bin file needs a base address (e.g. 0x08000000 for STM32, 0x10000000 for RP2040)");
  const path = await saveUpload(buf, name);
  try {
    return await job(p, "flash", () =>
      withTcl(p, async (tcl) => {
        setJob(p, { stage: `programming ${kind} (${buf.length} bytes)` });
        const verify = o.verify !== false;
        const reset = o.reset !== false;
        // ELF/hex carry their own addresses; an address given anyway would offset them.
        const cmd = `program {${path}}${kind === "bin" ? ` 0x${addr!.toString(16)}` : ""}${verify ? " verify" : ""}${reset ? " reset" : ""}`;
        logLine(p, `> ${cmd}`);
        const out = await tcl.cmd(cmd, { timeoutMs: 10 * 60_000 }).catch(tclErr);
        return { ok: true, kind, bytes: buf.length, address: addr, verified: verify, reset, output: out.trim(), message: `Programmed ${buf.length} bytes${verify ? ", verified" : ""}` };
      }),
    );
  } finally {
    await rm(path, { force: true });
  }
}

// Fast whole-chip erase commands by OpenOCD flash driver name; anything else erases sector by sector.
const MASS_ERASE: Record<string, string> = {
  stm32f1x: "stm32f1x mass_erase 0",
  stm32f2x: "stm32f2x mass_erase 0",
  stm32l4x: "stm32l4x mass_erase 0",
  stm32h7x: "stm32h7x mass_erase 0",
  nrf5: "nrf5 mass_erase",
  at91samd: "at91samd chip-erase",
  atsame5: "atsame5 chip-erase",
};

async function erase(p: Probe) {
  needOcd(p);
  return job(p, "erase", () =>
    withTcl(p, async (tcl) => {
      setJob(p, { stage: "halting" });
      await tcl.cmd("reset halt").catch(tclErr);
      const banks = await tcl.cmd("flash list", { capture: false }).catch(tclErr);
      const drv = /name (\S+)/.exec(banks)?.[1] || "";
      if (!drv) throw new HttpError(400, "No flash bank: pick a real target (not the generic Cortex-M one)");
      setJob(p, { stage: `erasing (${drv})` });
      let how = MASS_ERASE[drv];
      try {
        if (!how) throw new Error("no mass erase");
        await tcl.cmd(how, { timeoutMs: 120000 });
      } catch {
        how = "flash erase_sector 0 0 last";
        await tcl.cmd(how, { timeoutMs: 300000 }).catch(tclErr);
      }
      return { ok: true, driver: drv, command: how, message: `Erased with "${how}"` };
    }),
  );
}

async function control(p: Probe, cmd: "reset run" | "reset halt" | "reset init" | "halt" | "resume") {
  needOcd(p);
  return job(p, cmd, () =>
    withTcl(p, async (tcl) => {
      const out = await tcl.cmd(cmd).catch(tclErr);
      return { ok: true, output: out.trim(), message: cmd };
    }),
  );
}

async function readMemory(p: Probe, address: any, length: any) {
  needOcd(p);
  const a = L.parseNum(address);
  const n = L.parseNum(length) ?? 256;
  if (a === null || a < 0 || a > 0xffffffff) throw new HttpError(400, "address must be a number (hex 0x… or decimal)");
  if (n < 1 || n > 65536) throw new HttpError(400, "length must be 1–65536 bytes");
  return job(p, "read memory", () =>
    withTcl(p, async (tcl) => {
      const out = await tcl.cmd(`read_memory 0x${a.toString(16)} 8 ${n}`, { capture: false, timeoutMs: 60000 }).catch(tclErr);
      const bytes = L.parseReadMemory(out);
      return { address: a, length: bytes.length, hex: bytes.map((b) => b.toString(16).padStart(2, "0")).join(""), dump: L.hexdump(bytes, a) };
    }),
  );
}

async function command(p: Probe, cmd: string) {
  needOcd(p);
  if (!cmd?.trim()) throw new HttpError(400, "command is required");
  if (!L.tclBalanced(cmd)) throw new HttpError(400, "Unbalanced braces in command");
  return job(p, "command", () =>
    withTcl(p, async (tcl) => {
      logLine(p, `> ${cmd}`);
      const out = await tcl.cmd(cmd, { timeoutMs: 120000 }).catch(tclErr);
      return { output: out };
    }),
  );
}

// ---------- Bootloaders ----------

async function flashUf2(b: Boot, buf: Uint8Array, name = "firmware.uf2", force = false) {
  if (b.type.kind !== "rp2-bootsel") throw new HttpError(400, `${b.type.name} doesn't take UF2 files`);
  let info: L.Uf2Info;
  try {
    info = L.parseUf2(buf);
  } catch (e: any) {
    throw new HttpError(400, e.message);
  }
  // The bootrom silently ignores blocks of a foreign family, so "success" would mean nothing.
  if (!force && !L.uf2Accepts(b.type.chip, info.familyIds))
    throw new HttpError(400, `This UF2 is for ${info.families.join(", ")}; a ${b.type.chip.toUpperCase()} would ignore it (pass force to copy anyway)`);
  const file = (name.split(/[\\/]/).pop() || "firmware").replace(/[^A-Za-z0-9_.-]/g, "_").replace(/(\.uf2)?$/i, ".uf2");
  return job(b, "uf2", async () => {
    logLine(b, `UF2: ${info.blocks} blocks, ${info.payload} bytes for 0x${info.start.toString(16)}–0x${info.end.toString(16)}, family ${info.families.join(", ") || "none"}`);
    if (b.sim) {
      setJob(b, { stage: "copying" });
      await Bun.write(`${b.mount}/${file}`, buf);
      logLine(b, `Copied to ${b.mount}/${file} (simulated device: no reboot)`);
      return { ok: true, ...info, path: `${b.mount}/${file}`, rebooted: false, message: "Copied (simulated)" };
    }
    if (!b.block) throw new HttpError(409, "The BOOTSEL drive has no block device yet; wait a second and retry");
    let mount = b.mount;
    // DeskyPi's drive handler mounts every USB drive read-only on purpose. A BOOTSEL drive
    // is a fake FAT in the bootrom, so flipping it to rw is harmless; it's gone after reboot.
    if (!mount) {
      mount = `/run/deskypi-uf2/${b.id}`;
      setJob(b, { stage: "mounting" });
      await mustOk(["sudo", "mkdir", "-p", mount]);
      await mustOk(["sudo", "mount", "-o", "rw,sync", b.block, mount]);
      b.privateMount = mount;
    } else if (b.ro) {
      setJob(b, { stage: "remounting rw" });
      await mustOk(["sudo", "mount", "-o", "remount,rw", mount]);
    }
    const src = await saveUpload(buf, file);
    try {
      setJob(b, { stage: "copying" });
      const dst = `${mount}/${file}`;
      logLine(b, `$ sudo dd if=${src} of=${dst} bs=64k conv=fsync`);
      const r = await run(["sudo", "dd", `if=${src}`, `of=${dst}`, "bs=64k", "conv=fsync", "status=none"], { timeoutMs: 120000 });
      // The chip reboots as the last block lands, often before dd's fsync returns, so an
      // I/O error isn't a failure if the device then disappears.
      setJob(b, { stage: "waiting for reboot" });
      const gone = await waitGone(b.id, 8000);
      if (r.code !== 0 && !gone) throw new Error(`Copy failed: ${r.err || r.out || `exit ${r.code}`}`);
      logLine(b, gone ? "Device rebooted into the new firmware" : "Copied, but the device is still in BOOTSEL (wrong family or address?)");
      return { ok: true, ...info, path: dst, rebooted: gone, message: gone ? "Flashed; the board rebooted" : "Copied; the board didn't reboot" };
    } finally {
      await rm(src, { force: true });
    }
  });
}

async function mustOk(cmd: string[]) {
  const r = await run(cmd, { timeoutMs: 30000 });
  if (r.code !== 0) throw new Error(`${cmd.join(" ")}: ${r.err || r.out || `exit ${r.code}`}`);
}

async function waitGone(id: string, ms: number) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (!boots.has(id)) return true;
    await Bun.sleep(250);
  }
  return !boots.has(id);
}

async function flashDfu(b: Boot, buf: Uint8Array, address: any, name = "firmware.bin") {
  if (b.type.kind !== "stm32-dfu") throw new HttpError(400, `${b.type.name} isn't a DFU device`);
  if (!tools.dfuUtil) throw new HttpError(503, "dfu-util isn't installed (apt package: dfu-util)");
  const addr = L.parseNum(address) ?? 0x08000000;
  if (L.firmwareKind(buf, name) !== "bin") throw new HttpError(400, "DFU takes a raw .bin (convert with arm-none-eabi-objcopy -O binary)");
  const path = await saveUpload(buf, name);
  try {
    return await job(b, "dfu", async () => {
      const cmd = [tools.dfuUtil, "-d", `${b.vid}:${b.pid}`, ...(b.serial ? ["-S", b.serial] : []), "-a", "0", "-s", `0x${addr.toString(16)}:leave`, "-D", path];
      logLine(b, `$ ${cmd.map(L.shellQuote).join(" ")}`);
      const proc = ctx.spawn(cmd, { stdin: "ignore", stdout: "pipe", stderr: "pipe", env: { ...process.env, PATH } }) as Subprocess<"ignore", "pipe", "pipe">;
      const onLine = (l: string) => {
        const pr = L.parseDfuProgress(l);
        if (pr) setJob(b, { stage: pr.phase.toLowerCase(), pct: pr.pct });
        else logLine(b, l);
      };
      await Promise.all([eachLine(proc.stdout, onLine), eachLine(proc.stderr, onLine)]);
      const code = await proc.exited;
      // dfu-util's ":leave" makes the chip jump to the app, which often trips a final error.
      if (code !== 0 && !b.log.slice(-5).some((l) => /File downloaded successfully|Submitting leave request/.test(l))) throw new Error(`dfu-util exited ${code}`);
      return { ok: true, address: addr, bytes: buf.length, message: `Downloaded ${buf.length} bytes to 0x${addr.toString(16)}` };
    });
  } finally {
    await rm(path, { force: true });
  }
}

// ---------- Status ----------

function sessionStatus(s: Session | null) {
  if (!s) return null;
  return { state: s.state, stopped: s.stopped, persistent: s.persistent, ports: s.ports, target: s.target, since: s.since, exitCode: s.exitCode, pid: s.proc.pid };
}

function probeStatus(p: Probe) {
  // Probes OpenOCD can't drive (BMP, WCH RISC-V mode) have no config and take no port slot.
  const c = p.type.iface ? config(p) : null;
  return {
    id: p.id,
    kind: p.type.kind,
    name: p.type.name,
    vendor: p.vendor,
    model: p.model,
    vidpid: `${p.vid}:${p.pid}`,
    serial: p.serial,
    usb: L.usbLocation(p.devpath) || "",
    note: p.type.note || "",
    iface: p.type.iface,
    transports: p.type.transports,
    gdbBuiltIn: !!p.type.gdbBuiltIn,
    espSerial: !!p.type.espSerial,
    ttys: p.ttys,
    sim: !!p.sim,
    config: c && { ...c, effectivePorts: effectivePorts(c) },
    // A one-shot session is internal; only the user's GDB server shows as "the server".
    server: p.session?.persistent ? sessionStatus(p.session) : null,
    ocd: { adapter: p.ocd.adapter, voltage: p.ocd.voltage, cpus: p.ocd.cpus, dpidr: p.ocd.dpidr, flash: p.ocd.flash, gdbClients: p.ocd.gdbClients, errors: p.ocd.errors.slice(-3), hint: p.ocd.hint },
    job: p.job,
    lastJob: p.lastJob,
  };
}

function bootStatus(b: Boot) {
  return {
    id: b.id,
    kind: b.type.kind,
    name: b.type.name,
    chip: b.type.chip,
    vendor: b.vendor,
    model: b.model,
    vidpid: `${b.vid}:${b.pid}`,
    serial: b.serial,
    usb: L.usbLocation(b.devpath) || "",
    sim: !!b.sim,
    block: b.block,
    label: b.label,
    mount: b.mount,
    readOnly: b.ro,
    info: b.info,
    tool: { name: b.type.tool, pkg: b.type.pkg, path: b.type.tool === "dfu-util" ? tools.dfuUtil : tools.picotool },
    job: b.job,
    lastJob: b.lastJob,
  };
}

function hostAddr(): string {
  for (const list of Object.values(os.networkInterfaces()))
    for (const a of list || []) if (a.family === "IPv4" && !a.internal) return a.address;
  return os.hostname();
}

function listAll() {
  return {
    openocd: { path: openocd, version: openocdVersion, scripts: SCRIPTS },
    tools,
    portOffset: PORT_OFFSET,
    probes: [...probes.values()].map(probeStatus),
    bootloaders: [...boots.values()].map(bootStatus),
  };
}

function summary(): string {
  const parts: string[] = [];
  const servers = [...probes.values()].filter((p) => p.session?.persistent && p.session.state === "running");
  if (probes.size) parts.push(`${probes.size} probe${probes.size > 1 ? "s" : ""}`);
  for (const p of servers) parts.push(`GDB :${p.session!.ports.gdb}${p.ocd.cpus[0] ? ` ${p.ocd.cpus[0].split(": ")[1]}` : ""}`);
  for (const b of boots.values()) parts.push(b.type.name);
  return parts.join(" · ");
}

// ---------- Request helpers ----------

async function rawBody(req: Request): Promise<Uint8Array> {
  const len = parseInt(req.headers.get("content-length") || "0");
  if (len > UPLOAD_MAX) throw new HttpError(413, `File too large (max ${UPLOAD_MAX >> 20} MB)`);
  return new Uint8Array(await req.arrayBuffer());
}

/** MCP tools take firmware as base64 or a URL DeskyPi fetches. */
async function fileArg(a: any): Promise<{ buf: Uint8Array; name: string }> {
  if (a.data_base64) return { buf: new Uint8Array(Buffer.from(a.data_base64, "base64")), name: a.name || "" };
  if (a.url) {
    const r = await fetch(a.url);
    if (!r.ok) throw new Error(`Fetching ${a.url}: HTTP ${r.status}`);
    return { buf: new Uint8Array(await r.arrayBuffer()), name: a.name || new URL(a.url).pathname.split("/").pop() || "" };
  }
  throw new Error("Give the firmware as data_base64 or url");
}

const flag = (v: string | null, dflt: boolean) => (v === null || v === "" ? dflt : !/^(0|false|no|off)$/i.test(v));

// ---------- Driver ----------

const driver: Driver = {
  id: "probes",
  title: "Debug probes",
  order: 60,
  description: "ST-Link, J-Link, CMSIS-DAP, ESP and Black Magic probes as LAN GDB servers (OpenOCD); flashing; RP2 UF2 and STM32 DFU bootloaders",

  async init(c) {
    ctx = c;
    openocd = Bun.which("openocd", { PATH }) || "";
    if (openocd) {
      const r = await run([openocd, "--version"], { timeoutMs: 5000 });
      openocdVersion = (r.err || r.out).split("\n")[0].replace(/^Open On-Chip Debugger\s*/, "");
    }
    tools = { dfuUtil: Bun.which("dfu-util", { PATH }) || "", picotool: Bun.which("picotool", { PATH }) || "", gdb: Bun.which("gdb-multiarch", { PATH }) || Bun.which("gdb", { PATH }) || "" };
    try {
      targets = [...new Bun.Glob("target/**/*.cfg").scanSync(SCRIPTS)].sort();
    } catch {
      targets = [];
    }
    // Leftovers from a crash: uploads are only needed for the duration of a flash.
    await rm(TMP, { recursive: true, force: true }).catch(() => {});
    await scan();
  },

  async stop() {
    if (scanTimer) clearTimeout(scanTimer), (scanTimer = null);
    if (pollTimer) clearInterval(pollTimer), (pollTimer = null);
    await Promise.all([...probes.values()].map((p) => (p.session ? stopSession(p.session) : null)));
    for (const b of boots.values()) if (b.privateMount) await run(["sudo", "umount", "-l", b.privateMount]);
    probes.clear();
    boots.clear();
  },

  udev(ev: UdevEvent) {
    const s = ev.props.SUBSYSTEM;
    if (s === "usb" || s === "tty" || s === "block") scheduleScan();
  },

  status() {
    const running = [...probes.values()].some((p) => p.session && p.session.state !== "exited");
    return { active: probes.size > 0 || boots.size > 0 || running, summary: summary(), ...listAll() };
  },

  routes: [
    { method: "GET", path: "/api/probes", doc: "Debug probes and bootloader-mode MCUs, with each probe's OpenOCD config, GDB server state and parsed target info", handler: () => listAll() },
    {
      method: "GET",
      path: "/api/probes/targets",
      doc: "OpenOCD target scripts: presets (common chips present in this OpenOCD) and every target/*.cfg",
      handler: () => ({
        presets: [{ cfg: L.GENERIC_TARGET, label: "Any Cortex-M (detect)" }, ...L.TARGET_PRESETS.filter((t) => targets.includes(t.cfg)), { cfg: L.NO_TARGET, label: "No target (telnet setup)" }],
        all: targets,
      }),
    },
    {
      method: "PUT",
      path: "/api/probes/:id/config",
      doc: { body: { target: "target/stm32f4x.cfg", transport: "swd", speed: 4000, ports: { gdb: 3333, telnet: 4444, tcl: 6666 } }, description: "Set a probe's OpenOCD target, transport, adapter speed (kHz) and ports (persisted per probe)" },
      handler: async (req, { id }) => {
        const p = probeById(id);
        setConfig(p, await body(req));
        return probeStatus(p);
      },
    },
    {
      method: "POST",
      path: "/api/probes/:id/start",
      doc: { body: { target: "(optional) target/rp2040.cfg", transport: "(optional)", speed: "(optional)" }, description: "Start OpenOCD as a LAN GDB server for this probe; waits until it is ready or fails (with OpenOCD's error and a hint)" },
      handler: async (req, { id }) => {
        const p = probeById(id);
        needOcd(p);
        return startServer(p, await body(req));
      },
    },
    { method: "POST", path: "/api/probes/:id/stop", doc: "Stop this probe's GDB server", handler: (_r, { id }) => stopServer(probeById(id)) },
    { method: "GET", path: "/api/probes/:id/log", doc: "OpenOCD / tool log tail for a probe or bootloader device", handler: (_r, { id }) => ({ lines: (probes.get(id) || bootById(id)).log }) },
    {
      method: "GET",
      path: "/api/probes/:id/clients",
      doc: { query: { host: "(optional) hostname clients should use", web: "(optional) DeskyPi base URL" }, description: "Copy-paste client configs for this probe's GDB server: gdb, telnet, VS Code cortex-debug, PlatformIO" },
      handler: (_r, { id }, url) => {
        const p = probeById(id);
        const host = url.searchParams.get("host") || hostAddr();
        const web = url.searchParams.get("web") || `http://${host}${WEB_PORT === 80 ? "" : ":" + WEB_PORT}`;
        if (p.type.gdbBuiltIn) {
          const tty = p.ttys.find((t) => t.iface === "00")?.dev || p.ttys[0]?.dev || "/dev/ttyACM0";
          return {
            local: `target extended-remote ${tty}`,
            remote: `Start a raw TCP bridge on ${tty} in the Serial panel, then: target extended-remote ${host}:<bridge port>`,
            session: "monitor swdp_scan\nattach 1\nload\nrun",
          };
        }
        needOcd(p);
        if (!p.type.iface) throw new HttpError(400, `${p.type.name}: ${p.type.note}`);
        const c = config(p);
        return L.clientSnippets({ host, ports: effectivePorts(c), target: c.target, id: p.id, web });
      },
    },
    { method: "POST", path: "/api/probes/:id/examine", doc: "Detect the target: connect with the configured target script and report core(s), DPIDR, voltage, flash size", handler: (_r, { id }) => examine(probeById(id)) },
    {
      method: "POST",
      path: "/api/probes/:id/flash",
      doc: {
        query: { name: "firmware.elf (extension picks the format: .elf/.hex/.bin)", address: "base address, required for .bin (0x08000000)", verify: "1 (default) / 0", reset: "1 (default) / 0" },
        body: "raw file bytes",
        description: "Program firmware through OpenOCD (uses the running GDB server's TCL port if there is one); progress streams on WS /ws/probes/:id",
      },
      handler: async (req, { id }, url) => {
        const q = url.searchParams;
        return flash(probeById(id), await rawBody(req), { name: q.get("name") || "", address: q.get("address"), verify: flag(q.get("verify"), true), reset: flag(q.get("reset"), true) });
      },
    },
    { method: "POST", path: "/api/probes/:id/erase", doc: "Mass-erase the target's flash (halts it first)", handler: (_r, { id }) => erase(probeById(id)) },
    {
      method: "POST",
      path: "/api/probes/:id/reset",
      doc: { body: { mode: "run | halt | init" }, description: "Reset the target" },
      handler: async (req, { id }) => {
        const mode = (await body(req)).mode || "run";
        if (!["run", "halt", "init"].includes(mode)) throw new HttpError(400, "mode must be run, halt or init");
        return control(probeById(id), `reset ${mode}` as any);
      },
    },
    { method: "POST", path: "/api/probes/:id/halt", doc: "Halt the target CPU", handler: (_r, { id }) => control(probeById(id), "halt") },
    { method: "POST", path: "/api/probes/:id/resume", doc: "Resume the target CPU", handler: (_r, { id }) => control(probeById(id), "resume") },
    {
      method: "GET",
      path: "/api/probes/:id/memory",
      doc: { query: { address: "0x20000000", length: "256 (max 65536)" }, description: "Read target memory: hex string plus a hexdump" },
      handler: (_r, { id }, url) => readMemory(probeById(id), url.searchParams.get("address"), url.searchParams.get("length")),
    },
    {
      method: "POST",
      path: "/api/probes/:id/command",
      doc: { body: { command: "flash banks" }, description: "Run any OpenOCD command over TCL and return its output" },
      handler: async (req, { id }) => command(probeById(id), (await body(req)).command),
    },
    {
      method: "POST",
      path: "/api/probes/boot/:id/uf2",
      doc: { query: { name: "firmware.uf2", force: "1 to copy even if the UF2 family doesn't match the chip" }, body: "raw .uf2 bytes", description: "Flash an RP2040/RP2350 in BOOTSEL mode by copying the UF2 onto its drive; the board then reboots" },
      handler: async (req, { id }, url) => flashUf2(bootById(id), await rawBody(req), url.searchParams.get("name") || undefined, flag(url.searchParams.get("force"), false)),
    },
    {
      method: "POST",
      path: "/api/probes/boot/:id/dfu",
      doc: { query: { address: "0x08000000 (default)", name: "firmware.bin" }, body: "raw .bin bytes", description: "Flash an STM32 in DFU mode with dfu-util (needs the dfu-util package)" },
      handler: async (req, { id }, url) => flashDfu(bootById(id), await rawBody(req), url.searchParams.get("address"), url.searchParams.get("name") || undefined),
    },
    {
      method: "GET",
      path: "/api/probes/boot/:id/info",
      doc: "Bootloader device details; for RP2 also `picotool info -a` when picotool is installed",
      handler: async (_r, { id }) => {
        const b = bootById(id);
        const out: any = bootStatus(b);
        if (b.type.kind === "rp2-bootsel" && tools.picotool && !b.sim) {
          const r = await run(["sudo", tools.picotool, "info", "-a"], { timeoutMs: 15000 });
          out.picotool = r.out || r.err;
        }
        return out;
      },
    },
  ],

  sockets: [
    {
      path: "/ws/probes/:id",
      doc: 'Live log for a probe or bootloader device: {type:"log",lines} on connect, then {type:"line",text} and {type:"job",job,lastJob}',
      open(ws) {
        const d = probes.get(ws.data.params.id) || boots.get(ws.data.params.id);
        if (!d) return ws.close(4004, "No such device");
        d.sockets.add(ws);
        ws.send(JSON.stringify({ type: "log", lines: d.log, job: d.job, lastJob: d.lastJob }));
      },
      close(ws) {
        const d = probes.get(ws.data.params.id) || boots.get(ws.data.params.id);
        d?.sockets.delete(ws);
      },
    },
  ],

  tools: [
    {
      name: "probes_list",
      description:
        "List debug probes (ST-Link, J-Link, CMSIS-DAP, ESP USB-JTAG, Black Magic Probe…) and MCUs in bootloader mode (RP2 BOOTSEL, STM32 DFU) attached to DeskyPi, with each probe's id, OpenOCD target config, GDB server state/ports and detected target core/voltage.",
      inputSchema: { type: "object", properties: {} },
      handler: () => listAll(),
    },
    {
      name: "probes_server",
      description:
        "Start or stop the OpenOCD GDB server for a probe. On start it waits until OpenOCD is ready and returns the ports (connect with `target extended-remote <deskypi>:<gdb port>`), or fails with OpenOCD's error and a wiring/power hint. target is an OpenOCD script such as target/stm32f4x.cfg, target/rp2040.cfg, target/nordic/nrf52.cfg, or generic-cortex-m.",
      inputSchema: {
        type: "object",
        properties: {
          probe: { type: "string", description: "Probe id or serial from probes_list (optional when only one probe is attached)" },
          action: { type: "string", enum: ["start", "stop"] },
          target: { type: "string", description: "OpenOCD target script, e.g. target/stm32f4x.cfg (persisted)" },
          transport: { type: "string", enum: ["swd", "jtag"] },
          speed: { type: "number", description: "Adapter speed in kHz" },
        },
        required: ["action"],
      },
      handler: async (a) => {
        const p = probeById(a.probe);
        needOcd(p);
        if (a.action === "stop") return stopServer(p);
        const over: any = {};
        for (const k of ["target", "transport", "speed"]) if (a[k] !== undefined) over[k] = a[k];
        return startServer(p, over);
      },
    },
    {
      name: "probes_examine",
      description: "Connect to the target through a probe and report what's there: core(s), DPIDR, target voltage, flash size, and OpenOCD errors with hints. Uses the probe's configured target script (generic-cortex-m by default).",
      inputSchema: { type: "object", properties: { probe: { type: "string", description: "Probe id or serial (optional when only one)" } } },
      handler: (a) => examine(probeById(a.probe)),
    },
    {
      name: "probes_flash",
      description:
        "Program firmware (.elf, .hex or .bin) into the target through a debug probe with OpenOCD, then verify and reset. Give the file as base64 or a URL DeskyPi can fetch. A .bin needs a base address (STM32 0x08000000, RP2040 0x10000000). Set the probe's target first (probes_server start with target, or it must already be configured).",
      inputSchema: {
        type: "object",
        properties: {
          probe: { type: "string", description: "Probe id or serial (optional when only one)" },
          data_base64: { type: "string", description: "Firmware file, base64" },
          url: { type: "string", description: "Or: URL to fetch the firmware from" },
          name: { type: "string", description: "File name; its extension picks the format if the content doesn't (firmware.hex)" },
          address: { type: "string", description: "Base address for .bin, e.g. 0x08000000" },
          verify: { type: "boolean", default: true },
          reset: { type: "boolean", default: true },
        },
      },
      handler: async (a) => {
        const f = await fileArg(a);
        return flash(probeById(a.probe), f.buf, { name: f.name, address: a.address, verify: a.verify, reset: a.reset });
      },
    },
    {
      name: "probes_reset",
      description: "Reset, halt or resume the target CPU behind a debug probe.",
      inputSchema: {
        type: "object",
        properties: {
          probe: { type: "string", description: "Probe id or serial (optional when only one)" },
          mode: { type: "string", enum: ["run", "halt", "init", "halt-only", "resume"], description: "run = reset and run (default); halt = reset and halt; init = reset-init; halt-only = halt without reset; resume = continue" },
        },
      },
      handler: (a) => {
        const p = probeById(a.probe);
        const m = a.mode || "run";
        if (m === "halt-only") return control(p, "halt");
        if (m === "resume") return control(p, "resume");
        if (!["run", "halt", "init"].includes(m)) throw new Error("mode must be run, halt, init, halt-only or resume");
        return control(p, `reset ${m}` as any);
      },
    },
    {
      name: "probes_read_memory",
      description: "Read target memory through a debug probe. Returns hex and a hexdump. Works while the target runs on most Cortex-M parts.",
      inputSchema: {
        type: "object",
        properties: {
          probe: { type: "string", description: "Probe id or serial (optional when only one)" },
          address: { type: "string", description: "Start address, hex (0x20000000) or decimal" },
          length: { type: "number", description: "Bytes to read (1–65536, default 256)" },
        },
        required: ["address"],
      },
      handler: (a) => readMemory(probeById(a.probe), a.address, a.length),
    },
    {
      name: "probes_command",
      description: "Run any OpenOCD command (e.g. `flash banks`, `mdw 0x08000000 4`, `reg pc`) through a probe and return its output.",
      inputSchema: { type: "object", properties: { probe: { type: "string" }, command: { type: "string" } }, required: ["command"] },
      handler: (a) => command(probeById(a.probe), a.command),
    },
    {
      name: "probes_uf2_flash",
      description:
        "Flash an RP2040/RP2350 that is in BOOTSEL mode (hold BOOTSEL while plugging in) by copying a .uf2 onto its USB drive. The board reboots into the new firmware. Give the UF2 as base64 or a URL.",
      inputSchema: {
        type: "object",
        properties: {
          device: { type: "string", description: "Bootloader device id from probes_list (optional when only one)" },
          data_base64: { type: "string" },
          url: { type: "string" },
          name: { type: "string", description: "File name, e.g. blink.uf2" },
          force: { type: "boolean", description: "Copy even if the UF2 family doesn't match the chip" },
        },
      },
      handler: async (a) => {
        const f = await fileArg(a);
        return flashUf2(bootById(a.device), f.buf, f.name || undefined, !!a.force);
      },
    },
  ],
};

export default driver;
