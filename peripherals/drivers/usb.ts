// ==================== USB ====================
//
// Inventory tree (controllers → buses → hubs → devices, with interfaces, bound drivers and the
// device nodes each one created), a hotplug timeline (udev plug/unplug plus the USB-relevant
// part of the kernel log), port power switching through uhubctl, and network sharing of single
// devices over USB/IP (usbipd) or usbredir. Pure logic lives in peripherals/lib/usb.ts.
//
// Safety model for anything that cuts power or detaches a device from this machine: work out
// what is downstream first (lib planPower), and refuse, or require an explicit confirmation
// level, when that includes the root filesystem, a drive DeskyPi is serving, the management
// network adapter, an internal hub link, or sibling ports on a ganged hub.

import { hostname } from "node:os";
import { readdir } from "node:fs/promises";
import type { Subprocess } from "bun";
import { body, HttpError, readText, run, timeline, usbName, type Ctx, type Driver, type Props, type Route, type Tool } from "../core.ts";
import * as U from "../lib/usb.ts";

// /usr/sbin isn't on a non-login PATH, and usbip, usbipd, uhubctl and modprobe all live there.
const PATH = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const which = (bin: string) => Bun.which(bin, { PATH });
// Side instances (tests, other agents) shift every listener so they never collide with the live service.
const PORT_OFFSET = parseInt(process.env.DESKYPI_PORT_OFFSET || "") || 0;
const TIMELINE_IN_STATUS = 100;

let ctx: Ctx;
let stopped = true;
let model = "";
let rootSource = "";

let raw: U.RawUsbDevice[] = [];
let inv: U.Inventory = { controllers: [], devices: [] };
let protect: Record<string, string[]> = {};
let hubs: U.UhubHub[] = [];
let topo: U.PowerTopology = { groups: [], internal: {} };
const uhub = { bin: null as string | null, error: "", at: 0 };
let view: any = null;

// ---------- small helpers ----------

async function lines(stream: ReadableStream<Uint8Array> | null | undefined, cb: (line: string) => void) {
  if (!stream) return;
  const dec = new TextDecoder();
  let buf = "";
  try {
    for await (const chunk of stream) {
      buf += dec.decode(chunk, { stream: true });
      let i;
      while ((i = buf.indexOf("\n")) >= 0) {
        const l = buf.slice(0, i).replace(/\r$/, "");
        buf = buf.slice(i + 1);
        if (l) cb(l);
      }
    }
  } catch {}
  if (buf.trim()) cb(buf.trim());
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const dev = (busid: string) => inv.devices.find((d) => d.busid === busid);
function mustDev(busid: string) {
  const d = typeof busid === "string" ? dev(busid) : undefined;
  if (!d) throw new HttpError(404, `No USB device "${busid}". Busids look like 1-1.2 (bus 1, port path 1.2); see GET /api/usb.`);
  return d;
}
function label(d: U.UsbDevice) {
  const n = [d.vendor, d.product].filter(Boolean).join(" ");
  return n || d.vidpid;
}
function usbipPort() {
  return ctx.settings.get("usbipPort", 3240) + PORT_OFFSET;
}
// 4200+: the serial bridges own 4000+ (raw TCP) and 4100+ (RFC 2217).
function redirBase() {
  return ctx.settings.get("redirBasePort", 4200) + PORT_OFFSET;
}
let fqdn = "";
/** Name clients should use in usbip/QEMU commands (the UI prefers the name it was reached by). */
function host() {
  return fqdn || hostname();
}

// ---------- Protection: what must not lose power or be detached ----------

/** The deepest USB device whose sysfs path contains `devpath`. */
function usbOwner(devpath: string): string | null {
  let best: U.RawUsbDevice | null = null;
  for (const r of raw) if (devpath.startsWith(r.devpath + "/") && (!best || r.devpath.length > best.devpath.length)) best = r;
  return best?.name ?? null;
}

async function computeProtect(): Promise<Record<string, string[]>> {
  const out: Record<string, string[]> = {};
  const add = (busid: string | null, why: string) => {
    if (busid && !(out[busid] ||= []).includes(why)) out[busid].push(why);
  };
  const byName = new Map(ctx.devices((p) => !!p.DEVNAME).map((p) => [p.DEVNAME, p]));
  const usbPath = (devname: string) => byName.get(devname)?.DEVPATH;

  // Root filesystem (and its whole disk).
  if (rootSource.startsWith("/dev/")) {
    const p = usbPath(rootSource);
    if (p) add(usbOwner(p), `holds the root filesystem (${rootSource})`);
  }

  // Served drives: anything mounted, or exported over NBD by nfs-drive-handler.sh.
  const mounts = (await readText("/proc/self/mounts")).split("\n").map((l) => l.split(" "));
  for (const [src, mnt] of mounts) {
    if (!src?.startsWith("/dev/") || src === rootSource) continue;
    const p = usbPath(src);
    if (p) add(usbOwner(p), `has a mounted filesystem (${src} on ${mnt.replace(/\\040/g, " ")})`);
  }
  try {
    for (const f of await readdir("/etc/nbd-server/conf.d")) {
      if (!f.endsWith(".conf")) continue;
      const devname = `/dev/${f.slice(0, -5)}`;
      const p = usbPath(devname);
      if (p) add(usbOwner(p), `is served over NBD (${devname})`);
    }
  } catch {}

  // The adapter carrying the default route is how everyone reaches this box (USB Ethernet on
  // older Pis, or a USB NIC someone added).
  try {
    const r = await run(["ip", "-j", "route", "show", "default"], { timeoutMs: 3000 });
    const routes: any[] = JSON.parse(r.out || "[]");
    routes.sort((a, b) => (a.metric ?? 0) - (b.metric ?? 0));
    const ifname = routes[0]?.dev;
    if (ifname) {
      const n = ctx.devices((p) => p.SUBSYSTEM === "net" && p.INTERFACE === ifname)[0];
      if (n?.DEVPATH) add(usbOwner(n.DEVPATH), `is the management network adapter (${ifname})`);
    }
  } catch {}
  return out;
}

// ---------- Scanning ----------

let rescanning: Promise<void> | null = null;
let rescanAgain = false;
let rescanTimer: ReturnType<typeof setTimeout> | null = null;
let hubTimer: ReturnType<typeof setTimeout> | null = null;
const pendingAuto = new Set<string>();

function scheduleRescan(ms = 250) {
  if (rescanTimer) clearTimeout(rescanTimer);
  rescanTimer = setTimeout(() => {
    rescanTimer = null;
    rescan().catch((e) => ctx.log("rescan:", e));
  }, ms);
}

function scheduleHubRefresh(ms = 1000) {
  if (!uhub.bin) return;
  if (hubTimer) clearTimeout(hubTimer);
  hubTimer = setTimeout(() => {
    hubTimer = null;
    uhubLocked(refreshHubs).then(() => {
      rebuild();
      ctx.changed();
    });
  }, ms);
}

async function rescan(): Promise<void> {
  if (rescanning) {
    rescanAgain = true;
    return rescanning;
  }
  rescanning = (async () => {
    do {
      rescanAgain = false;
      raw = await U.readUsbSysfs();
      const ctrl = new Set(raw.map((r) => r.controller?.devpath).filter(Boolean));
      inv = U.buildInventory(raw, ctx.devices((p) => !!p.DEVPATH && (p.DEVPATH.includes("/usb") || ctrl.has(p.DEVPATH))));
      protect = await computeProtect();
    } while (rescanAgain);
  })();
  try {
    await rescanning;
  } finally {
    rescanning = null;
  }
  for (const [busid, s] of redir) if (!dev(busid)) stopRedir(busid, "device unplugged");
  rebuild();
  reconcileUsbipd();
  applyRules();
  ctx.changed();
}

// One uhubctl at a time: a listing racing a power change would read half-switched hubs.
let uhubQ: Promise<any> = Promise.resolve();
function uhubLocked<T>(fn: () => Promise<T>): Promise<T> {
  const p = uhubQ.then(fn, fn);
  uhubQ = p.catch(() => {});
  return p;
}

async function refreshHubs() {
  if (!uhub.bin) return;
  // -f lists ganged/no-switching hubs too (shown, not switched); -N skips reading device
  // string descriptors, so listing never wakes or pokes the devices themselves.
  const r = await run(["sudo", "-n", uhub.bin, "-f", "-N"], { timeoutMs: 20000 });
  const parsed = U.parseUhubctl(r.out);
  if (parsed.length || r.code === 0) {
    hubs = parsed;
    uhub.error = "";
  } else {
    hubs = [];
    uhub.error = (r.err || r.out || `exit ${r.code}`).split("\n")[0];
  }
  uhub.at = Date.now();
  topo = U.powerTopology(inv, hubs, model);
}

// ---------- The view status() returns ----------

function hubPower(d: U.UsbDevice) {
  if (!d.hub) return null;
  const loc = U.hubLoc(d.busid);
  const h = hubs.find((x) => x.loc === loc);
  if (!h) return { loc, switchable: false, why: uhub.bin ? (uhub.error ? `uhubctl: ${uhub.error}` : "uhubctl doesn't list this hub") : "uhubctl isn't installed (apt: uhubctl)" };
  const group = topo.groups.find((g) => g.hubs.includes(loc));
  return {
    loc,
    mode: h.mode,
    switchable: h.mode !== "nops",
    why: h.mode === "nops" ? "This hub has no power switching" : "",
    group: group ? { id: group.id, label: group.label, hubs: group.hubs } : null,
    ports: h.ports.map((p) => ({
      port: p.port,
      power: p.power,
      connect: p.connect,
      enable: p.enable,
      suspend: p.suspend,
      overCurrent: p.overCurrent,
      speed: p.speed,
      link: p.link,
      internal: topo.internal[`${loc}:${p.port}`] || null,
    })),
  };
}

function shareBlock(d: U.UsbDevice): string | null {
  if (d.root) return "Root hubs can't be shared";
  if (d.hub) return "Hubs can't be shared; share the devices on it";
  if (protect[d.busid]) return `Refused: this device ${protect[d.busid].join("; ")}`;
  if (redir.has(d.busid)) return "In use by usbredir; stop that first";
  return null;
}

function rebuild() {
  topo = U.powerTopology(inv, hubs, model);
  const rules = getRules();
  view = {
    controllers: inv.controllers,
    devices: inv.devices.map((d) => {
      const r = redir.get(d.busid);
      return {
        ...d,
        power: hubPower(d),
        upstream: d.parent ? { hub: U.hubLoc(d.parent), port: d.port } : null,
        protect: protect[d.busid] || [],
        shareBlock: shareBlock(d),
        redir: r ? { port: r.port, since: r.since, peers: peers.filter((p) => p.local === r.port).map((p) => p.peer) } : null,
        rules: rules.filter((x) => ruleMatches(x, d)).map((x) => x.match),
      };
    }),
    groups: topo.groups,
  };
}

// ---------- Hotplug ----------

async function onPlug(p: Props) {
  const busid = p.DEVPATH.slice(p.DEVPATH.lastIndexOf("/") + 1);
  const speed = await readText(`/sys${p.DEVPATH}/speed`);
  const { vendor, model: product } = usbName(p);
  const vidpid = `${p.ID_VENDOR_ID || ""}:${p.ID_MODEL_ID || ""}`;
  const name = [vendor, product].filter(Boolean).join(" ") || vidpid;
  if (busid.startsWith("usb")) ctx.event(`New USB bus ${busid.slice(3)}: ${name}`, { kind: "plug", busid, vidpid, speed: U.speedLabel(speed) });
  else ctx.event(`Plugged in ${name} (${vidpid}) · ${U.speedLabel(speed) || "speed ?"} · port ${busid}`, { kind: "plug", busid, vidpid, speed: U.speedLabel(speed) });
  pendingAuto.add(busid);
}

function onUnplug(p: Props) {
  const busid = p.DEVPATH.slice(p.DEVPATH.lastIndexOf("/") + 1);
  const d = inv.devices.find((x) => x.devpath === p.DEVPATH);
  const name = d ? label(d) : `${p.ID_VENDOR_ID || p.PRODUCT || "device"}`;
  if (busid.startsWith("usb")) ctx.event(`USB bus ${busid.slice(3)} removed`, { kind: "unplug", busid });
  else ctx.event(`Unplugged ${name}${d ? ` (${d.vidpid})` : ""} · port ${busid}${d?.usbip ? " · was shared over USB/IP" : ""}`, { kind: "unplug", busid, vidpid: d?.vidpid });
  if (redir.has(busid)) stopRedir(busid, "device unplugged");
}

// ---------- Kernel log ----------

let dmesgFails = 0;
let dmesgError = "";

function startDmesg() {
  if (stopped) return;
  const bin = which("dmesg");
  if (!bin) {
    dmesgError = "dmesg not found";
    return;
  }
  // dmesg_restrict is on, hence sudo. -W follows only *new* messages, so the boot backlog
  // never floods the timeline (GET /api/usb/dmesg shows it on demand, marked historical).
  const started = Date.now();
  const proc = ctx.spawn(["sudo", "-n", bin, "-W", "--time-format", "iso"], { stdout: "pipe", stderr: "pipe" });
  let err = "";
  lines(proc.stdout as ReadableStream<Uint8Array>, (line) => {
    const k = U.filterKernelLine(line);
    if (k) ctx.event(k.text, { kind: "kernel", level: k.level, ts: k.ts });
  });
  lines(proc.stderr as ReadableStream<Uint8Array>, (l) => (err = l));
  proc.exited.then((code) => {
    if (stopped) return;
    dmesgFails = Date.now() - started < 5000 ? dmesgFails + 1 : 0;
    dmesgError = `kernel log follower exited (${code})${err ? `: ${err}` : ""}`;
    ctx.log(dmesgError);
    if (dmesgFails < 5) setTimeout(startDmesg, 5000 * (dmesgFails + 1));
    ctx.changed();
  });
  dmesgError = "";
}

// ---------- USB/IP ----------

let usbipd: { proc: Subprocess; port: number; since: number; log: string[] } | null = null;
let usbipdError = "";
let usbipdFails = 0;
let loadedUsbipHost = false;
let peers: { local: number; peer: string }[] = [];

const boundCount = () => raw.filter((r) => r.driver === "usbip-host").length;

function reconcileUsbipd() {
  const want = !stopped && boundCount() > 0;
  if (want && !usbipd && usbipdFails < 4) startUsbipd();
  else if (!want && usbipd) stopUsbipd("nothing is shared");
}

function startUsbipd() {
  const bin = which("usbipd");
  if (!bin) {
    usbipdError = "usbipd isn't installed (apt: usbip)";
    return;
  }
  const port = usbipPort();
  // Foreground (no -D) so it's our child: killed on stop/shutdown, never left orphaned.
  const proc = ctx.spawn(["sudo", "-n", bin, "-t", String(port)], { stdout: "pipe", stderr: "pipe" });
  const me = { proc, port, since: Date.now(), log: [] as string[] };
  usbipd = me;
  usbipdError = "";
  const keep = (l: string) => {
    me.log.push(l);
    if (me.log.length > 30) me.log.shift();
  };
  lines(proc.stdout as ReadableStream<Uint8Array>, keep);
  lines(proc.stderr as ReadableStream<Uint8Array>, keep);
  ctx.event(`USB/IP server (usbipd) started on TCP ${port}`, { kind: "share" });
  proc.exited.then((code) => {
    if (usbipd !== me) return; // stopped on purpose
    usbipd = null;
    usbipdFails = Date.now() - me.since < 5000 ? usbipdFails + 1 : 0;
    usbipdError = `usbipd exited (${code})${me.log.length ? `: ${me.log.slice(-2).join(" / ")}` : ""}`;
    ctx.event(usbipdError, { kind: "share", level: "error" });
    if (!stopped) setTimeout(reconcileUsbipd, 3000);
    rebuild();
    ctx.changed();
  });
}

function stopUsbipd(why: string) {
  const u = usbipd;
  if (!u) return;
  usbipd = null;
  u.proc.kill();
  ctx.event(`USB/IP server (usbipd) stopped: ${why}`, { kind: "share" });
}

async function ensureUsbipHost() {
  if (await Bun.file("/sys/bus/usb/drivers/usbip-host/uevent").exists()) return;
  const mp = which("modprobe") || "/usr/sbin/modprobe";
  const r = await run(["sudo", "-n", mp, "usbip-host"], { timeoutMs: 15000 });
  if (r.code !== 0) throw new HttpError(500, `Couldn't load the usbip-host module: ${r.err || r.out}`);
  loadedUsbipHost = true;
}

async function share(busid: string, origin: "user" | "rule" = "user") {
  await rescan();
  const d = mustDev(busid);
  if (d.usbip) return { ok: true, already: true, busid, ...clientHelp(busid) };
  const block = shareBlock(d);
  if (block) throw new HttpError(409, block);
  const bin = which("usbip");
  if (!bin) throw new HttpError(501, "usbip isn't installed (apt: usbip)");
  await ensureUsbipHost();
  const r = await run(["sudo", "-n", bin, "bind", "-b", busid], { timeoutMs: 15000 });
  if (r.code !== 0) throw new HttpError(409, `usbip bind -b ${busid} failed: ${(r.err || r.out).replace(/^usbip: error: /, "")}`);
  usbipdFails = 0;
  ctx.event(`Shared ${label(d)} (${busid}) over USB/IP${origin === "rule" ? " by an auto-share rule" : ""}; it's gone from this machine's drivers until unshared`, { kind: "share", busid });
  await rescan();
  return { ok: true, busid, ...clientHelp(busid) };
}

async function unshare(busid: string) {
  await rescan();
  const d = mustDev(busid);
  if (!d.usbip) throw new HttpError(409, `${busid} isn't shared`);
  const bin = which("usbip");
  if (!bin) throw new HttpError(501, "usbip isn't installed (apt: usbip)");
  const wasUsed = d.usbip === "in use";
  const r = await run(["sudo", "-n", bin, "unbind", "-b", busid], { timeoutMs: 15000 });
  if (r.code !== 0) throw new HttpError(409, `usbip unbind -b ${busid} failed: ${(r.err || r.out).replace(/^usbip: error: /, "")}`);
  ctx.event(`Stopped sharing ${label(d)} (${busid})${wasUsed ? "; the remote client was disconnected" : ""}`, { kind: "share", busid });
  await rescan();
  return { ok: true, busid };
}

function clientHelp(busid: string) {
  const port = usbipPort();
  const pf = port === 3240 ? "" : ` --tcp-port ${port}`;
  return {
    server: `${host()}:${port}`,
    list: `usbip${pf} list -r ${host()}`,
    attach: `sudo usbip${pf} attach -r ${host()} -b ${busid}`,
    windows: `usbip.exe${pf} attach -r ${host()} -b ${busid}   (usbip-win2)`,
  };
}

// ---------- Auto-share rules ----------

interface Rule {
  match: "vidpid" | "port";
  value: string;
}
const getRules = (): Rule[] => ctx.settings.get<Rule[]>("autoShare", []);
const ruleMatches = (r: Rule, d: U.UsbDevice) => !d.hub && (r.match === "vidpid" ? r.value === d.vidpid : r.value === d.busid);

function validateRule(r: any): Rule {
  if (r?.match === "vidpid" && /^[0-9a-f]{4}:[0-9a-f]{4}$/.test(String(r.value).toLowerCase())) return { match: "vidpid", value: String(r.value).toLowerCase() };
  if (r?.match === "port" && /^\d+-\d+(\.\d+)*$/.test(String(r.value))) return { match: "port", value: String(r.value) };
  throw new HttpError(400, 'A rule is {match:"vidpid", value:"0403:6001"} or {match:"port", value:"1-1.2"}');
}

function applyRules(all = false) {
  const rules = getRules();
  const todo = all ? inv.devices.map((d) => d.busid) : [...pendingAuto];
  pendingAuto.clear();
  if (!rules.length) return;
  for (const busid of todo) {
    const d = dev(busid);
    if (!d || d.usbip || shareBlock(d) || !rules.some((r) => ruleMatches(r, d))) continue;
    share(busid, "rule").catch((e) => ctx.event(`Auto-share of ${busid} failed: ${e.message}`, { kind: "share", level: "error", busid }));
  }
}

// ---------- usbredir ----------

const redir = new Map<string, { proc: Subprocess; port: number; since: number; log: string[] }>();

function startRedir(busid: string) {
  const d = mustDev(busid);
  const bin = which("usbredirserver");
  if (!bin) throw new HttpError(501, "usbredirserver isn't installed (apt: usbredirserver)");
  if (redir.has(busid)) return { ok: true, already: true, port: redir.get(busid)!.port };
  if (d.usbip) throw new HttpError(409, "Shared over USB/IP; unshare it first (a device can only go to one remote at a time)");
  const block = shareBlock(d);
  if (block) throw new HttpError(409, block);
  const used = new Set([...redir.values()].map((r) => r.port));
  let port = redirBase();
  while (used.has(port)) port++;
  // busnum-devnum pins this exact device (two identical adapters share a VID:PID).
  const proc = ctx.spawn(["sudo", "-n", bin, "-p", String(port), `${d.bus}-${d.devnum}`], { stdout: "pipe", stderr: "pipe" });
  const me = { proc, port, since: Date.now(), log: [] as string[] };
  redir.set(busid, me);
  const keep = (l: string) => {
    me.log.push(l);
    if (me.log.length > 30) me.log.shift();
  };
  lines(proc.stdout as ReadableStream<Uint8Array>, keep);
  lines(proc.stderr as ReadableStream<Uint8Array>, keep);
  proc.exited.then((code) => {
    if (redir.get(busid) !== me) return;
    redir.delete(busid);
    ctx.event(`usbredir for ${busid} exited (${code})${me.log.length ? `: ${me.log.at(-1)}` : ""}`, { kind: "share", level: "warn", busid });
    rebuild();
    ctx.changed();
  });
  ctx.event(`usbredir serving ${label(d)} (${busid}) on TCP ${port}`, { kind: "share", busid });
  rebuild();
  ctx.changed();
  return { ok: true, busid, port, qemu: redirSnippet(port).qemu, libvirt: redirSnippet(port).libvirt };
}

function stopRedir(busid: string, why = "stopped") {
  const r = redir.get(busid);
  if (!r) return false;
  redir.delete(busid);
  r.proc.kill();
  ctx.event(`usbredir for ${busid} ${why}`, { kind: "share", busid });
  return true;
}

function redirSnippet(port: number) {
  return {
    qemu: `-chardev socket,id=redir0,host=${host()},port=${port} -device usb-redir,chardev=redir0,id=redir0`,
    libvirt: `<redirdev bus='usb' type='tcp'><source mode='connect' host='${host()}' service='${port}'/></redirdev>`,
  };
}

// ---------- Peers / live usbip state ----------

let pollTimer: ReturnType<typeof setInterval> | null = null;

async function poll() {
  if (!usbipd && !redir.size && !boundCount()) {
    if (peers.length) peers = [];
    return;
  }
  const ports = [...(usbipd ? [usbipd.port] : []), ...[...redir.values()].map((r) => r.port)];
  const r = await run(["ss", "-tnH", "state", "established"], { timeoutMs: 3000 });
  const next = U.parseSsPeers(r.out, ports);
  const key = (x: { local: number; peer: string }) => `${x.local}>${x.peer}`;
  const before = new Set(peers.map(key));
  const after = new Set(next.map(key));
  let dirty = false;
  for (const p of next) if (!before.has(key(p))) (ctx.event(`Remote client ${p.peer} connected to ${p.local === usbipd?.port ? "USB/IP" : "usbredir"} :${p.local}`, { kind: "share" }), (dirty = true));
  for (const p of peers) if (!after.has(key(p))) (ctx.event(`Remote client ${p.peer} disconnected from :${p.local}`, { kind: "share" }), (dirty = true));
  peers = next;
  // A remote attach/detach changes usbip_status without any udev event.
  for (const x of raw.filter((x) => x.driver === "usbip-host")) {
    const s = parseInt(await readText(`/sys${x.devpath}/usbip_status`)) || 0;
    if (s !== x.usbipStatus) {
      x.usbipStatus = s;
      const d = dev(x.name);
      if (d) d.usbip = s === 2 ? "in use" : s === 3 ? "error" : "available";
      dirty = true;
    }
  }
  if (dirty) {
    rebuild();
    ctx.changed();
  }
}

// ---------- Power ----------

const ACTIONS = ["on", "off", "cycle"] as const;

function resolvePowerTarget(a: any): { hub: string; port: number | null; action: U.PowerAction } {
  const action = a?.action;
  if (!ACTIONS.includes(action)) throw new HttpError(400, 'action must be "on", "off" or "cycle"');
  if (a.busid) {
    const d = mustDev(String(a.busid));
    if (!d.parent || d.port == null) throw new HttpError(400, `${d.busid} is a root hub; switch its ports instead`);
    return { hub: U.hubLoc(d.parent), port: d.port, action };
  }
  if (a.hub == null) throw new HttpError(400, "Give hub (uhubctl location, e.g. 1-1 or 2) and optionally port, or busid of the device to switch");
  const port = a.port == null || a.port === "" ? null : Number(a.port);
  if (port !== null && !Number.isInteger(port)) throw new HttpError(400, "port must be an integer");
  return { hub: String(a.hub), port, action };
}

async function plan(a: any) {
  const t = resolvePowerTarget(a);
  if (!uhub.bin) throw new HttpError(501, "uhubctl isn't installed (apt: uhubctl)");
  await rescan();
  await uhubLocked(refreshHubs);
  rebuild();
  try {
    return U.planPower(inv, hubs, topo, protect, t);
  } catch (e: any) {
    throw new HttpError(400, e.message);
  }
}

class NeedsConfirm extends HttpError {
  constructor(public plan: U.PowerPlan) {
    super(409, `${plan.warnings.join(" ")} Repeat with confirm: "${plan.needs}" to go ahead.`);
  }
}

async function power(a: any) {
  const p = await plan(a);
  const confirmed = p.needs === "none" || a.confirm === "unsafe" || (p.needs === "ganged" && a.confirm === "ganged");
  if (!confirmed) throw new NeedsConfirm(p);
  const delay = Math.min(30, Math.max(1, Number(a.delay) || 2));
  const outputs: string[] = [];
  await uhubLocked(async () => {
    const exec = async (cmd: string[]) => {
      const r = await run(["sudo", "-n", uhub.bin!, ...cmd], { timeoutMs: 20000 });
      outputs.push(`$ uhubctl ${cmd.join(" ")}\n${r.out}${r.err ? "\n" + r.err : ""}`);
      if (r.code !== 0) throw new HttpError(500, `uhubctl ${cmd.join(" ")} failed: ${r.err || r.out}`);
    };
    if (p.action === "cycle") {
      const half = p.commands.length / 2;
      for (const c of p.commands.slice(0, half)) await exec(c);
      await sleep(delay * 1000);
      for (const c of p.commands.slice(half)) await exec(c);
    } else for (const c of p.commands) await exec(c);
    await refreshHubs();
  });
  const what = p.group ? `${p.group.label.split(":")[0]} (${p.ports.length} ports, ganged)` : p.port !== null ? `hub ${p.hub} port ${p.port}` : `all ports of hub ${p.hub}`;
  ctx.event(`Power ${p.action}${p.action === "cycle" ? ` (${delay} s)` : ""}: ${what}${p.devices.length ? ` · affects ${p.devices.join(", ")}` : ""}`, { kind: "power", hub: p.hub, port: p.port, action: p.action });
  scheduleRescan(500);
  return { ok: true, plan: p, output: outputs.join("\n\n") };
}

// ---------- Flattened list (REST + MCP) ----------

function flatList() {
  return {
    controllers: inv.controllers.map((c) => ({ id: c.id, name: c.name, driver: c.driver, buses: c.buses })),
    devices: (view?.devices || []).map((d: any) => ({
      busid: d.busid,
      depth: d.depth,
      parent: d.parent,
      port: d.port,
      name: label(d),
      vidpid: d.vidpid,
      serial: d.serial || undefined,
      speed: d.speedLabel,
      usb: d.version,
      maxPowerMa: d.maxPowerMa,
      hub: d.hub ? { ports: d.nports, power: d.power?.switchable ? `${d.power.mode}${d.power.group ? " (ganged group)" : ""}` : "not switchable" } : undefined,
      interfaces: d.interfaces.map((i: U.UsbInterface) => `${i.id} ${i.clsName} → ${i.driver || "no driver"}`),
      nodes: d.nodes.map((n: U.UsbNode) => `${n.dev}${n.panel ? ` (${n.panel} panel)` : ""}`),
      hint: d.hint?.label,
      usbip: d.usbip || undefined,
      redir: d.redir ? `TCP ${d.redir.port}` : undefined,
      protected: d.protect.length ? d.protect : undefined,
    })),
  };
}

function timelineEntries(limit = TIMELINE_IN_STATUS, kinds?: string[]) {
  const t = kinds ? timeline.filter((e) => kinds.includes(e.detail?.kind)) : timeline;
  return t.slice(-limit).reverse();
}

// ---------- Routes ----------

const busidParam = { busid: "1-1.2" };

const routes: Route[] = [
  { method: "GET", path: "/api/usb", doc: "Full USB state: controllers, the device tree with interfaces/drivers/nodes, hub port power, sharing and protection", handler: () => ({ ...status(), timeline: undefined }) },
  { method: "GET", path: "/api/usb/devices", doc: "Flattened USB tree (one entry per device, depth-first)", handler: () => flatList() },
  {
    method: "GET",
    path: "/api/usb/timeline",
    doc: { query: { limit: "100", kinds: "plug,unplug,kernel,power,share (optional)" }, description: "Hardware timeline, newest first (all peripheral drivers' events; USB ones carry detail.kind)" },
    handler: (_r, _p, url) => timelineEntries(Math.min(300, parseInt(url.searchParams.get("limit") || "") || 100), url.searchParams.get("kinds")?.split(",")),
  },
  {
    method: "GET",
    path: "/api/usb/dmesg",
    doc: "USB-related kernel log lines since boot (historical; the live timeline only shows new ones)",
    handler: async () => {
      const r = await run(["sudo", "-n", which("dmesg") || "dmesg", "--time-format", "iso"], { timeoutMs: 10000 });
      if (r.code !== 0) throw new HttpError(500, r.err || "dmesg failed");
      return { lines: r.out.split("\n").map(U.filterKernelLine).filter(Boolean).slice(-300) };
    },
  },
  {
    method: "POST",
    path: "/api/usb/rescan",
    doc: "Re-read sysfs and uhubctl now",
    handler: async () => {
      await rescan();
      await uhubLocked(refreshHubs);
      rebuild();
      ctx.changed();
      return { ok: true };
    },
  },
  {
    method: "GET",
    path: "/api/usb/power/plan",
    doc: { query: { hub: "1-1", port: "2 (optional)", busid: "or the device to switch", action: "on|off|cycle" }, description: "Dry run: which ports and devices a power action affects, warnings, and the confirm level it needs (none|ganged|unsafe)" },
    handler: (_r, _p, url) => plan(Object.fromEntries(url.searchParams)),
  },
  {
    method: "POST",
    path: "/api/usb/power",
    doc: { body: { hub: "1-1", port: 2, action: "on|off|cycle", delay: 2, confirm: "ganged|unsafe (when the plan needs it)" }, description: "Switch port power with uhubctl. Answers 409 with the plan when confirmation is needed. busid instead of hub/port switches the port a device is on." },
    handler: async (req) => {
      try {
        return await power(await body(req));
      } catch (e) {
        if (e instanceof NeedsConfirm) return Response.json({ ok: false, message: e.message, needs: e.plan.needs, plan: e.plan }, { status: 409 });
        throw e;
      }
    },
  },
  { method: "POST", path: "/api/usb/share", doc: { body: busidParam, description: "Share a device over USB/IP (binds it to usbip-host and runs usbipd). It disappears from this machine's drivers while shared." }, handler: async (req) => share(String((await body(req)).busid)) },
  { method: "POST", path: "/api/usb/unshare", doc: { body: busidParam, description: "Stop sharing a device over USB/IP (a connected client is dropped); local drivers take it back" }, handler: async (req) => unshare(String((await body(req)).busid)) },
  { method: "GET", path: "/api/usb/rules", doc: "Auto-share rules", handler: () => getRules() },
  {
    method: "POST",
    path: "/api/usb/rules",
    doc: { body: { match: "vidpid|port", value: "0403:6001 or 1-1.2" }, description: "Add an auto-share rule: matching devices are shared over USB/IP when plugged in (and now, if present)" },
    handler: async (req) => {
      const r = validateRule(await body(req));
      const rules = getRules();
      if (!rules.some((x) => x.match === r.match && x.value === r.value)) ctx.settings.set("autoShare", [...rules, r]);
      rebuild();
      applyRules(true);
      ctx.changed();
      return { ok: true, rules: getRules() };
    },
  },
  {
    method: "DELETE",
    path: "/api/usb/rules",
    doc: { body: { match: "vidpid|port", value: "0403:6001" }, description: "Remove an auto-share rule (devices already shared stay shared)" },
    handler: async (req) => {
      const r = validateRule(await body(req));
      ctx.settings.set("autoShare", getRules().filter((x) => !(x.match === r.match && x.value === r.value)));
      rebuild();
      ctx.changed();
      return { ok: true, rules: getRules() };
    },
  },
  { method: "POST", path: "/api/usb/redir/start", doc: { body: busidParam, description: "Serve a device with usbredirserver (for QEMU/libvirt usb-redir). Exclusive with USB/IP." }, handler: async (req) => startRedir(String((await body(req)).busid)) },
  {
    method: "POST",
    path: "/api/usb/redir/stop",
    doc: { body: busidParam, description: "Stop a usbredir session" },
    handler: async (req) => {
      const b = String((await body(req)).busid);
      if (!stopRedir(b)) throw new HttpError(409, `No usbredir session for ${b}`);
      rebuild();
      ctx.changed();
      return { ok: true };
    },
  },
  {
    method: "POST",
    path: "/api/usb/config",
    doc: { body: { usbipPort: 3240, redirBasePort: 4200 }, description: "Listener ports for usbipd and the first usbredir session (DESKYPI_PORT_OFFSET is added at runtime)" },
    handler: async (req) => {
      const b = await body(req);
      for (const k of ["usbipPort", "redirBasePort"]) {
        if (b[k] === undefined) continue;
        const v = Number(b[k]);
        if (!Number.isInteger(v) || v < 1 || v > 65535) throw new HttpError(400, `${k} must be a TCP port`);
        ctx.settings.set(k, v);
      }
      if (usbipd && usbipd.port !== usbipPort()) {
        stopUsbipd("port changed");
        reconcileUsbipd();
      }
      ctx.changed();
      return { ok: true, usbipPort: usbipPort(), redirBasePort: redirBase() };
    },
  },
];

// ---------- MCP tools ----------

const tools: Tool[] = [
  {
    name: "usb_list",
    description: "List every USB controller, hub and device on DeskyPi as a flattened depth-first tree: busid (also the physical port path), VID:PID and names, speed, power draw, each interface's class and bound kernel driver (or 'no driver'), the device nodes it created (/dev/ttyACM0, can0, /dev/sda…) and which DeskyPi panel handles them, USB/IP or usbredir sharing state, and protection notes.",
    inputSchema: { type: "object", properties: {} },
    handler: () => flatList(),
  },
  {
    name: "usb_timeline",
    description: "Recent hardware events, newest first: USB plug/unplug (with name, speed, port), USB-related kernel messages (enumeration errors, over-current, resets, driver binds), port power changes and sharing changes.",
    inputSchema: { type: "object", properties: { limit: { type: "integer", minimum: 1, maximum: 300, default: 50 }, kinds: { type: "array", items: { enum: ["plug", "unplug", "kernel", "power", "share"] }, description: "Only these event kinds" } } },
    handler: (a) => timelineEntries(a.limit || 50, a.kinds),
  },
  {
    name: "usb_power",
    description:
      "Switch USB port power with uhubctl: on, off, or cycle (off, wait delay seconds, on; power-cycling is the usual fix for a wedged device). Target a device by busid (switches the port it's plugged into) or a hub location plus port. Some hubs (all Raspberry Pi onboard ports) are ganged: one switch cuts every port. Those calls fail with an explanation until repeated with confirm:'ganged'. Ones that would cut power to the root filesystem, a served drive, the management NIC or an internal hub link need confirm:'unsafe'. Check usb_list first.",
    inputSchema: {
      type: "object",
      properties: {
        busid: { type: "string", description: "Device whose port to switch, e.g. 1-1.2" },
        hub: { type: "string", description: "uhubctl hub location, e.g. 1-1 or 2 (root hub of bus 2)" },
        port: { type: "integer", minimum: 1, description: "Port on that hub; omit for every port" },
        action: { enum: ["on", "off", "cycle"] },
        delay: { type: "number", minimum: 1, maximum: 30, default: 2, description: "Seconds off during cycle" },
        confirm: { enum: ["ganged", "unsafe"], description: "Only after reading the warning from a previous attempt" },
      },
      required: ["action"],
    },
    handler: async (a) => {
      const r = await power(a);
      return { ok: true, action: r.plan.action, ports: r.plan.ports, devicesAffected: r.plan.devices, warnings: r.plan.warnings };
    },
  },
  {
    name: "usb_share",
    description: "Share one USB device over the network with USB/IP so another computer can attach it as if it were plugged in locally. While shared the device is detached from DeskyPi's own drivers. Returns the client commands (Linux: usbip attach; Windows: usbip-win2). Hubs, the root-filesystem device, served drives and the management NIC are refused.",
    inputSchema: { type: "object", properties: { busid: { type: "string", description: "e.g. 1-1.2 (from usb_list)" } }, required: ["busid"] },
    handler: (a) => share(String(a.busid)),
  },
  {
    name: "usb_unshare",
    description: "Stop sharing a USB device over USB/IP; any attached remote client is disconnected and DeskyPi's local drivers take the device back.",
    inputSchema: { type: "object", properties: { busid: { type: "string" } }, required: ["busid"] },
    handler: (a) => unshare(String(a.busid)),
  },
];

// ---------- Status ----------

function status() {
  if (!view) return { active: false, summary: "Scanning…" };
  const devices = inv.devices.filter((d) => !d.hub);
  const buses = inv.devices.filter((d) => d.root).length;
  const shared = boundCount();
  const parts = [devices.length ? `${devices.length} device${devices.length > 1 ? "s" : ""}` : "No devices"];
  if (shared) parts.push(`${shared} shared`);
  if (redir.size) parts.push(`${redir.size} usbredir`);
  const oc = inv.devices.some((d) => d.ports.some((p) => p.overCurrent));
  if (oc) parts.push("over-current seen");
  if (!devices.length) parts.push(`${buses} bus${buses === 1 ? "" : "es"}`);
  return {
    active: devices.length > 0 || !!usbipd || redir.size > 0,
    summary: parts.join(" · "),
    host: host(),
    controllers: view.controllers,
    devices: view.devices,
    groups: view.groups,
    uhubctl: { available: !!uhub.bin, error: uhub.error, at: uhub.at },
    usbip: {
      available: !!which("usbip") && !!which("usbipd"),
      running: !!usbipd,
      port: usbipPort(),
      since: usbipd?.since || null,
      error: usbipdError,
      peers: usbipd ? peers.filter((p) => p.local === usbipd!.port).map((p) => p.peer) : [],
      shared,
    },
    redir: { available: !!which("usbredirserver"), apt: "usbredirserver", basePort: redirBase(), sessions: redir.size },
    rules: getRules(),
    kernelLog: { error: dmesgError },
    timeline: timelineEntries(),
  };
}

// ---------- Driver ----------

const driver: Driver = {
  id: "usb",
  title: "USB",
  order: 10,
  description: "USB device tree, hotplug timeline, port power switching (uhubctl), and sharing devices over the network (USB/IP, usbredir)",
  async init(c) {
    ctx = c;
    stopped = false;
    uhub.bin = which("uhubctl");
    model = (await readText("/proc/device-tree/model")).replace(/\0/g, "");
    rootSource = (await run(["findmnt", "-no", "SOURCE", "/"])).out.trim();
    fqdn = (await run(["hostname", "-f"], { timeoutMs: 3000 })).out.trim();
    await rescan();
    await uhubLocked(refreshHubs);
    rebuild();
    startDmesg();
    applyRules(true);
    pollTimer = setInterval(() => poll().catch((e) => ctx.log("poll:", e)), 3000);
    ctx.changed();
  },
  async stop() {
    stopped = true;
    for (const t of [rescanTimer, hubTimer]) if (t) clearTimeout(t);
    if (pollTimer) clearInterval(pollTimer);
    pollTimer = null;
    // Children (usbipd, usbredirserver, dmesg) are killed by core after this returns.
    // Devices stay bound to usbip-host: established USB/IP attachments live in the kernel and
    // survive a restart of this service; the next init restarts usbipd for new clients.
    usbipd = null;
    redir.clear();
    if (loadedUsbipHost && boundCount() === 0) {
      await run(["sudo", "-n", which("modprobe") || "/usr/sbin/modprobe", "-r", "usbip-host"], { timeoutMs: 10000 });
      loadedUsbipHost = false;
    }
  },
  udev(ev) {
    const p = ev.props;
    if (!p.DEVPATH?.includes("/usb")) return;
    if (p.SUBSYSTEM === "usb" && p.DEVTYPE === "usb_device") {
      if (ev.action === "add") onPlug(p).catch(() => {});
      else if (ev.action === "remove") onUnplug(p);
      if (ev.action === "add" || ev.action === "remove") scheduleHubRefresh();
    }
    scheduleRescan();
  },
  status,
  routes,
  tools,
};

export default driver;

/** For tests on the target: drive the usbipd lifecycle without a bindable device. */
export const __test = {
  startUsbipd: () => startUsbipd(),
  stopUsbipd: (why: string) => stopUsbipd(why),
  usbipdState: () => ({ running: !!usbipd, port: usbipd?.port, error: usbipdError, log: usbipd?.log }),
};
