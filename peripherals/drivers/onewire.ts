// ==================== 1-Wire ====================
//
// Masters (w1-gpio on GPIO4, DS2482 I²C bridges, DS9490R USB adapters) and their slaves,
// decoded by family through the kernel's w1 slave drivers' sysfs attributes (decoders in
// peripherals/lib/w1-families.ts). Readings are polled in the background, never from
// status(): a DS18B20 conversion takes up to 750 ms per sensor, so several sensors on one
// bus use therm_bulk_read (one simultaneous conversion) when the master offers it.
//
// iButton readers are a classic use, so family-0x01 arrivals/departures are kept in a touch
// log with timestamps. Family 0x00 "slaves" are not devices: a floating data line (no
// 4.7 kΩ pull-up, nothing attached) makes the search algorithm read noise that happens to
// pass the CRC; they're counted as bus noise and hidden.
//
// Root-only sysfs writes (search, bulk read, outputs, resolution, bridge binding) go through
// `sudo -n tee`. DESKYPI_W1_ROOT points the driver at a fixture tree for tests.

import { readdir, readFile, writeFile } from "node:fs/promises";
import { body, HttpError, run, type Ctx, type Driver, type Route, type Tool, type UdevEvent } from "../core.ts";
import * as W from "../lib/w1-families.ts";
import { ds2482Candidates, rescan as i2cRescan } from "./i2c.ts";

const ROOT = process.env.DESKYPI_W1_ROOT || "/sys/bus/w1/devices";
const FIXTURE = !!process.env.DESKYPI_W1_ROOT;
const PATH = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const HIST_N = 120;
const TOUCH_MAX = 200;

interface Master {
  id: string; // w1_bus_master1
  kind: "gpio" | "ds2482" | "ds2490" | "other";
  label: string;
  path: string;
  slaves: string[];
  search: number;
  pullup: string;
  timeout: string;
  attempts: number;
  bulk: boolean;
  ghosts: Map<string, number>; // family-00 ids → last seen
  error: string;
}

interface Slave {
  id: string;
  master: string;
  fam: number;
  family: W.Family;
  romOk: boolean | null;
  present: boolean;
  firstSeen: number;
  lastSeen: number;
  decoded: W.Decoded | null;
  readAt: number;
  error: string;
  hist: Record<string, number[]>;
}

let ctx: Ctx;
const masters = new Map<string, Master>();
const slaves = new Map<string, Slave>();
const touchLog: { t: number; id: string; event: "touch" | "release" | "appear" | "gone"; family: string }[] = [];
let presenceTimer: ReturnType<typeof setInterval> | null = null;
let pollTimer: ReturnType<typeof setTimeout> | null = null;
let polling: Promise<void> | null = null;
let stopped = true;
let gpioPins = new Map<string, number>(); // device-tree node → pin, for labelling w1-gpio masters

// ---------- sysfs I/O ----------

async function rd(path: string): Promise<string> {
  try {
    return (await readFile(path, "utf8")).trim();
  } catch {
    return "";
  }
}
async function rdBin(path: string): Promise<Uint8Array | null> {
  try {
    return new Uint8Array(await readFile(path));
  } catch {
    return null;
  }
}

/** Write a sysfs attribute, falling back to `sudo -n tee` for root-only files. */
async function wr(path: string, data: string | Uint8Array) {
  try {
    await writeFile(path, data);
    return;
  } catch (e: any) {
    if (e?.code !== "EACCES" && e?.code !== "EPERM") throw new HttpError(502, `${path}: ${e.message}`);
  }
  const proc = Bun.spawn(["sudo", "-n", "tee", path], { stdin: typeof data === "string" ? new Blob([data]) : new Blob([data]), stdout: "ignore", stderr: "pipe", env: { ...process.env, PATH } });
  const err = await new Response(proc.stderr).text();
  if ((await proc.exited) !== 0) throw new HttpError(502, `${path}: ${err.trim() || "write failed"}`);
}

const ok = (id: string) => /^[0-9a-f]{2}-[0-9a-f]{12}$/i.test(id);

// ---------- discovery ----------

async function loadGpioPins() {
  // w1-gpio overlay nodes: /proc/device-tree/onewire@N/gpios = <phandle pin flags>
  gpioPins = new Map();
  try {
    for (const n of await readdir("/proc/device-tree")) {
      if (!n.startsWith("onewire")) continue;
      const g = await rdBin(`/proc/device-tree/${n}/gpios`);
      if (g && g.length >= 8) gpioPins.set(n, new DataView(g.buffer).getUint32(4));
    }
  } catch {}
}

// Master sysfs nodes have no parent device (/sys/devices/w1_bus_masterN for every kind), so
// which bus driver created a master is inferred from the drivers' own bindings:
// w1-gpio platform devices, ds2482 I²C clients, ds2490 USB interfaces. A master that appears
// right after a new binding belongs to it; at start-up masters are matched in registration
// order (device-tree w1-gpio first, then bridges as they were bound).
interface Source { kind: Master["kind"]; key: string; label: string }
const sourceOf = new Map<string, Source>(); // master id → source
let knownSources = new Set<string>();

async function listSources(): Promise<Source[]> {
  const ls = async (d: string) => (await readdir(d).catch(() => [] as string[])).filter((n) => !["bind", "unbind", "uevent", "module", "new_id", "remove_id"].includes(n));
  const out: Source[] = [];
  for (const n of await ls("/sys/bus/platform/drivers/w1-gpio")) {
    const pin = gpioPins.get(n);
    out.push({ kind: "gpio", key: `gpio:${n}`, label: pin != null ? `GPIO${pin} (header pin ${headerPin(pin) ?? "?"})` : `w1-gpio ${n}` });
  }
  for (const n of await ls("/sys/bus/i2c/drivers/ds2482")) {
    const m = /^(\d+)-00([0-9a-f]{2})$/.exec(n);
    if (m) out.push({ kind: "ds2482", key: `ds2482:${n}`, label: `DS2482 on i2c-${m[1]} @0x${m[2]}` });
  }
  for (const n of await ls("/sys/bus/usb/drivers/ds2490")) if (/:/.test(n)) out.push({ kind: "ds2490", key: `ds2490:${n}`, label: `DS9490R USB adapter (${n.split(":")[0]})` });
  return out;
}

/** Attribute masters to sources; returns ids whose label changed. */
async function attributeMasters(ids: string[]) {
  if (FIXTURE) return;
  const sources = await listSources();
  const fresh = sources.filter((s) => !knownSources.has(s.key));
  knownSources = new Set(sources.map((s) => s.key));
  const live = new Set(sources.map((s) => s.key));
  for (const [id, src] of sourceOf) if (!live.has(src.key) || !ids.includes(id)) sourceOf.delete(id);
  const taken = new Set([...sourceOf.values()].map((s) => s.key));
  const num = (id: string) => parseInt(id.replace(/\D+/g, ""), 10);
  const unassigned = ids.filter((id) => !sourceOf.has(id)).sort((a, b) => num(a) - num(b));
  // Prefer just-bound sources for the newest masters, then registration order.
  const pool = [...sources.filter((s) => !taken.has(s.key) && !fresh.includes(s)), ...fresh.filter((s) => !taken.has(s.key))];
  for (const id of unassigned) {
    const src = pool.shift();
    if (src) sourceOf.set(id, src);
  }
}

function classifyMaster(id: string): { kind: Master["kind"]; label: string; path: string } {
  const src = sourceOf.get(id);
  if (src) return { kind: src.kind, label: src.label, path: `${ROOT}/${id}` };
  return { kind: FIXTURE ? "gpio" : "other", label: FIXTURE ? "Fixture bus (DESKYPI_W1_ROOT)" : id, path: `${ROOT}/${id}` };
}

function headerPin(gpio: number): number | null {
  const map: Record<number, number> = { 2: 3, 3: 5, 4: 7, 17: 11, 27: 13, 22: 15, 10: 19, 9: 21, 11: 23, 0: 27, 5: 29, 6: 31, 13: 33, 19: 35, 26: 37, 14: 8, 15: 10, 18: 12, 23: 16, 24: 18, 25: 22, 8: 24, 7: 26, 1: 28, 12: 32, 16: 36, 20: 38, 21: 40 };
  return map[gpio] ?? null;
}

/** Cheap presence pass (every 2 s): master attributes + slave lists. No bus traffic. */
async function refresh() {
  let names: string[] = [];
  try {
    names = await readdir(ROOT);
  } catch {}
  const seenMasters = new Set<string>();
  const now = Date.now();
  const ids = names.filter((n) => /^w1_bus_master\d+$/.test(n));
  await attributeMasters(ids);
  for (const id of ids) {
    seenMasters.add(id);
    let m = masters.get(id);
    if (m) Object.assign(m, classifyMaster(id));
    if (!m) {
      const c = classifyMaster(id);
      m = { id, ...c, slaves: [], search: -1, pullup: "", timeout: "", attempts: 0, bulk: false, ghosts: new Map(), error: "" };
      masters.set(id, m);
      if (!stopped) ctx.event(`1-Wire master ${id} appeared (${m.label})`);
    }
    const dir = `${ROOT}/${id}`;
    const list = (await rd(`${dir}/w1_master_slaves`)).split("\n").map((s) => s.trim()).filter((s) => ok(s));
    m.search = parseInt(await rd(`${dir}/w1_master_search`)) || 0;
    m.pullup = await rd(`${dir}/w1_master_pullup`);
    m.timeout = await rd(`${dir}/w1_master_timeout`);
    m.attempts = parseInt(await rd(`${dir}/w1_master_attempts`)) || 0;
    m.bulk = (await rd(`${dir}/therm_bulk_read`)) !== "";
    for (const s of list) if (W.familyOf(s) === 0) m.ghosts.set(s, now);
    for (const [g, t] of m.ghosts) if (now - t > 600_000) m.ghosts.delete(g);
    m.slaves = list.filter((s) => W.familyOf(s) !== 0);
    for (const sid of m.slaves) {
      let s = slaves.get(sid);
      if (!s || !s.present) {
        const fam = W.familyOf(sid);
        const fresh = !s;
        if (!s) {
          const id = await rdBin(`${ROOT}/${sid}/id`);
          s = { id: sid, master: id ? m.id : m.id, fam, family: W.family(fam), romOk: id ? W.romOk(id) : null, present: true, firstSeen: now, lastSeen: now, decoded: null, readAt: 0, error: "", hist: {} };
          slaves.set(sid, s);
        }
        s.present = true;
        s.master = m.id;
        logPresence(s, true, fresh);
        schedulePoll(50); // read the newcomer soon
      }
      s.lastSeen = now;
    }
  }
  for (const [id, m] of masters) {
    if (seenMasters.has(id)) continue;
    masters.delete(id);
    if (!stopped) ctx.event(`1-Wire master ${id} went away (${m.label})`);
  }
  for (const s of slaves.values()) {
    if (!s.present) continue;
    const m = masters.get(s.master);
    if (m && m.slaves.includes(s.id)) continue;
    s.present = false;
    logPresence(s, false, false);
  }
  ctx.changed();
}

function logPresence(s: Slave, on: boolean, fresh: boolean) {
  const label = slaveLabel(s);
  const ibutton = s.fam === 0x01;
  if (stopped && on) return; // initial enumeration at start-up isn't an event
  touchLog.push({ t: Date.now(), id: s.id, event: ibutton ? (on ? "touch" : "release") : on ? "appear" : "gone", family: s.family.name });
  if (touchLog.length > TOUCH_MAX) touchLog.splice(0, touchLog.length - TOUCH_MAX);
  ctx.event(`1-Wire ${label} ${ibutton ? (on ? "touched" : "released") : on ? (fresh ? "appeared" : "came back") : "went away"}`, { id: s.id });
}

function slaveLabel(s: Slave) {
  const l = labels()[s.id];
  return l ? `${l} (${s.id})` : `${s.family.name} ${s.id}`;
}
const labels = (): Record<string, string> => ctx.settings.get("labels", {});
const interval = () => Math.max(2000, ctx.settings.get("interval", 10_000));
const useBulk = () => ctx.settings.get("bulk", true);

// ---------- reading ----------

function record(s: Slave, d: W.Decoded) {
  s.decoded = d;
  s.readAt = Date.now();
  s.error = "";
  for (const v of d.values) {
    if (typeof v.v !== "number") continue;
    const h = (s.hist[v.k] ||= []);
    h.push(v.v);
    if (h.length > HIST_N) h.shift();
  }
}

async function readSlave(s: Slave, opts: { eeprom?: boolean } = {}): Promise<W.Decoded> {
  const dir = `${ROOT}/${s.id}`;
  const k = s.family.kind;
  let d: W.Decoded;
  if (k === "temp" || k === "thermocouple") {
    const text = await rd(`${dir}/w1_slave`);
    const ts = W.parseThermSlave(text);
    if (!ts) {
      if (s.family.module && !(await rd(`${dir}/driver`)) && !text) throw new Error(`no w1_slave attribute (is ${s.family.module} loaded?)`);
      throw new Error(text ? `unexpected w1_slave: ${text.slice(0, 60)}` : "no reading (sensor didn't answer)");
    }
    d = W.decodeTherm(s.fam, ts);
    const ext = await rd(`${dir}/ext_power`);
    if (ext === "0") d.values.push({ k: "power", label: "Power", v: "parasite" });
    else if (ext === "1") d.values.push({ k: "power", label: "Power", v: "external" });
  } else if (s.fam === 0x26) {
    const page0 = await rdBin(`${dir}/page0`);
    d = W.decodeDs2438(await rd(`${dir}/temperature`), await rd(`${dir}/vad`), await rd(`${dir}/vdd`), page0 ? [...page0] : undefined, ctx.settings.get(`rsens:${s.id}`, 0) || undefined);
  } else if (s.fam === 0x29) {
    const st = await rdBin(`${dir}/state`), out = await rdBin(`${dir}/output`), act = await rdBin(`${dir}/activity`);
    if (!st?.length || !out?.length) throw new Error("couldn't read state/output (is w1_ds2408 loaded?)");
    d = W.decodeDs2408(st[0], out[0], act?.length ? act[0] : null);
  } else if (s.fam === 0x3a) {
    const st = await rdBin(`${dir}/state`);
    if (!st?.length) throw new Error("couldn't read state (is w1_ds2413 loaded?)");
    d = W.decodeDs2413(st[0]);
  } else if (s.fam === 0x12) {
    const st = await rdBin(`${dir}/state`);
    if (!st?.length) throw new Error("couldn't read state (w1_ds2406 has no autoload alias: modprobe it)");
    d = W.decodeDs2406(st[0]);
  } else if (s.fam === 0x1d) {
    d = W.parseDs2423(await rd(`${dir}/w1_slave`));
  } else if (k === "eeprom" && s.family.eepromSize) {
    const size = s.family.eepromSize;
    d = { values: [{ k: "size", label: "Size", v: `${size} bytes` }] };
    if (opts.eeprom) {
      const b = await rdBin(`${dir}/eeprom`);
      if (!b) throw new Error("couldn't read eeprom");
      d.values.push({ k: "blank", label: "Contents", v: b.every((x) => x === 0xff) ? "blank (all 0xFF)" : `${b.length} bytes read` });
      (d as any).eeprom = [...b];
    }
  } else if (k === "rtc" && (s.fam === 0x24 || s.fam === 0x27)) {
    d = W.decodeRtc(await readClock(s));
  } else if (s.fam === 0x01) {
    d = { values: [{ k: "present", label: "On the reader", v: s.present ? "yes" : "no" }] };
  } else if (s.fam === 0x81) {
    d = { values: [{ k: "adapter", label: "Identifies", v: "DS9490R USB 1-Wire adapter" }] };
  } else {
    d = { values: [{ k: "family", label: "Family", v: `0x${s.fam.toString(16).padStart(2, "0")}` }] };
  }
  if (s.romOk === false) (d.warn ||= []).push("ROM ID CRC mismatch: this ID may be bus noise.");
  return d;
}

/** DS1904/DS2415/DS2417 have no kernel driver: select + "read clock" (0x66) through `rw`. */
async function readClock(s: Slave): Promise<number[]> {
  const path = `${ROOT}/${s.id}/rw`;
  if (FIXTURE) return [...((await rdBin(path)) || [])]; // fixture rw holds the 5-byte reply
  // Write and read in one root shell so the select and the read are back to back.
  const r = await run(["sudo", "-n", "sh", "-c", `printf '\\146' > '${path}' && head -c 5 '${path}' | od -An -tx1`], { timeoutMs: 5000 });
  if (r.code !== 0) throw new Error(r.err || "clock read failed");
  return r.out.split(/\s+/).filter(Boolean).map((h) => parseInt(h, 16));
}

/** One background cycle over every present slave. */
async function pollAll() {
  const byMaster = new Map<string, Slave[]>();
  for (const s of slaves.values()) if (s.present) (byMaster.get(s.master) || byMaster.set(s.master, []).get(s.master)!).push(s);
  for (const [mid, list] of byMaster) {
    const m = masters.get(mid);
    const therm = list.filter((s) => s.family.kind === "temp" || s.family.kind === "thermocouple");
    if (m && m.bulk && useBulk() && therm.length > 1) {
      // One simultaneous conversion; w1_slave reads afterwards return the converted values.
      try {
        await wr(`${ROOT}/${mid}/therm_bulk_read`, "trigger\n");
        for (let i = 0; i < 20 && !FIXTURE; i++) {
          await Bun.sleep(100);
          if ((await rd(`${ROOT}/${mid}/therm_bulk_read`)) !== "-1") break;
        }
      } catch (e: any) {
        m.error = `bulk read: ${e.message}`;
      }
    }
    for (const s of list) {
      if (stopped) return;
      if (s.family.kind === "rtc" && s.readAt && Date.now() - s.readAt < 60_000) continue; // clocks: once a minute
      try {
        record(s, await readSlave(s));
      } catch (e: any) {
        s.error = e.message;
      }
      ctx.changed();
    }
  }
}

function schedulePoll(ms = interval()) {
  if (stopped) return;
  if (pollTimer) clearTimeout(pollTimer);
  pollTimer = setTimeout(async () => {
    pollTimer = null;
    if (polling) return schedulePoll(500);
    polling = pollAll().finally(() => (polling = null));
    await polling;
    schedulePoll();
  }, ms);
}

// ---------- actions ----------

function mustSlave(id: string): Slave {
  const s = slaves.get(String(id || "").toLowerCase());
  if (!s) throw new HttpError(404, `No 1-Wire slave "${id}". IDs look like 28-0000075b1a2c; see GET /api/onewire.`);
  return s;
}
function mustMaster(id?: string): Master {
  const m = id ? masters.get(id) : [...masters.values()][0];
  if (!m) throw new HttpError(404, id ? `No 1-Wire master "${id}"` : "No 1-Wire master present");
  return m;
}

async function searchNow(m: Master) {
  // Writing w1_master_search wakes the master thread for an immediate search. Writing the
  // current value keeps continuous mode (-1) as it is; when searching is off, do one search.
  await wr(`${ROOT}/${m.id}/w1_master_search`, `${m.search === 0 ? 1 : m.search}\n`);
  await Bun.sleep(FIXTURE ? 0 : 1500);
  await refresh();
}

async function setOutput(s: Slave, a: any) {
  const dir = `${ROOT}/${s.id}`;
  let byte: number;
  if (s.fam === 0x29) {
    if (a.value != null) byte = Number(a.value) & 0xff;
    else {
      const cur = (await rdBin(`${dir}/output`))?.[0] ?? 0xff;
      const pin = Number(a.pin);
      if (!(pin >= 0 && pin < 8)) throw new HttpError(400, "pin must be 0–7");
      byte = a.on ? cur & ~(1 << pin) : cur | (1 << pin);
    }
  } else if (s.fam === 0x3a || s.fam === 0x12) {
    const cur = s.fam === 0x3a ? W.decodeDs2413((await rdBin(`${dir}/state`))?.[0] ?? 0xff).pins! : W.decodeDs2406((await rdBin(`${dir}/state`))?.[0] ?? 0x33).pins!;
    let bits = (cur[0].latch ? 1 : 0) | (cur[1].latch ? 2 : 0);
    if (a.value != null) bits = Number(a.value) & 3;
    else {
      const pin = String(a.pin).toUpperCase().replace("PIO-", "").replace("PIO", "");
      const bit = pin === "A" || pin === "0" ? 1 : pin === "B" || pin === "1" ? 2 : 0;
      if (!bit) throw new HttpError(400, "pin must be A or B");
      bits = a.on ? bits & ~bit : bits | bit;
    }
    byte = s.fam === 0x3a ? 0xfc | bits : bits;
  } else throw new HttpError(409, `${s.family.name} has no switchable outputs`);
  // "on" = output transistor conducting (pin pulled low), the chips' own convention.
  await wr(`${dir}/output`, Uint8Array.of(byte));
  record(s, await readSlave(s));
  ctx.event(`1-Wire ${slaveLabel(s)} outputs set to 0x${byte.toString(16)}`);
  ctx.changed();
  return s.decoded;
}

async function bindBridge(a: any) {
  const bus = parseInt(String(a.bus).replace(/^i2c-/, ""), 10);
  const addr = typeof a.address === "number" ? a.address : parseInt(String(a.address), 16);
  if (!Number.isInteger(bus) || !(addr >= 0x18 && addr <= 0x1b)) throw new HttpError(400, "bus and address (0x18–0x1B) required");
  const base = `/sys/bus/i2c/devices/i2c-${bus}`;
  if (a.bind === false) {
    await wr(`${base}/delete_device`, `0x${addr.toString(16)}\n`);
    ctx.event(`Unbound ds2482 from i2c-${bus} 0x${addr.toString(16)}`);
  } else {
    await run(["sudo", "-n", "/usr/sbin/modprobe", "ds2482"], { timeoutMs: 10_000 });
    await wr(`${base}/new_device`, `ds2482 0x${addr.toString(16)}\n`);
    ctx.event(`Bound ds2482 to i2c-${bus} 0x${addr.toString(16)}`);
  }
  await Bun.sleep(500);
  await i2cRescan(bus).catch(() => {});
  await refresh();
  return view();
}

// ---------- views ----------

function bridges() {
  const out: any[] = ds2482Candidates().map((c) => ({ type: "DS2482", bus: c.bus, address: `0x${c.addr.toString(16)}`, bound: c.bound, note: c.bound ? "bound to the ds2482 driver" : c.driver ? `claimed by ${c.driver}` : "found on I²C, not bound" }));
  for (const p of ctx.devices((p) => p.SUBSYSTEM === "usb" && p.DEVTYPE === "usb_device" && p.ID_VENDOR_ID === "04fa" && p.ID_MODEL_ID === "2490")) {
    const has = [...masters.values()].some((m) => m.kind === "ds2490");
    out.push({ type: "DS9490R", usb: p.DEVPATH, bound: has, note: has ? "ds2490 master present" : "USB adapter present but no master (is the ds2490 module loaded?)" });
  }
  return out;
}

function slaveView(s: Slave) {
  return {
    id: s.id,
    family: `0x${s.fam.toString(16).padStart(2, "0")}`,
    name: s.family.name,
    kind: s.family.kind,
    desc: s.family.desc,
    module: s.family.module || null,
    label: labels()[s.id] || "",
    master: s.master,
    present: s.present,
    romOk: s.romOk,
    firstSeen: s.firstSeen,
    lastSeen: s.lastSeen,
    readAt: s.readAt,
    values: s.decoded?.values || [],
    pins: s.decoded?.pins || null,
    warn: s.decoded?.warn || [],
    error: s.error,
    hist: s.hist,
    outputs: [0x29, 0x3a, 0x12].includes(s.fam),
    eepromSize: s.family.eepromSize || 0,
    resolution: s.family.kind === "temp" && s.fam !== 0x10,
  };
}

function view() {
  return {
    root: ROOT,
    fixture: FIXTURE,
    interval: interval(),
    bulk: useBulk(),
    masters: [...masters.values()].map((m) => ({ id: m.id, kind: m.kind, label: m.label, slaves: m.slaves.length, search: m.search, searchText: m.search === -1 ? "continuous" : m.search === 0 ? "off" : `${m.search} left`, pullup: m.pullup === "1", timeout: m.timeout, ghosts: m.ghosts.size, bulk: m.bulk, error: m.error })),
    slaves: [...slaves.values()].sort((a, b) => Number(b.present) - Number(a.present) || a.id.localeCompare(b.id)).map(slaveView),
    touchLog: touchLog.slice(-50).reverse(),
    bridges: bridges(),
  };
}

// ---------- routes ----------

const routes: Route[] = [
  { method: "GET", path: "/api/onewire", doc: "1-Wire masters, slaves (decoded readings + history), iButton touch log and DS2482/DS9490R bridges", handler: () => view() },
  {
    method: "POST",
    path: "/api/onewire/search",
    doc: { description: "Trigger an immediate search on a master (default: the first). mode: continuous|off sets w1_master_search persistently", body: { master: "w1_bus_master1", mode: "continuous" } },
    handler: async (req) => {
      const a = await body(req);
      const m = mustMaster(a.master);
      if (a.mode === "continuous") await wr(`${ROOT}/${m.id}/w1_master_search`, "-1\n");
      else if (a.mode === "off") await wr(`${ROOT}/${m.id}/w1_master_search`, "0\n");
      await searchNow(m);
      return view();
    },
  },
  {
    method: "POST",
    path: "/api/onewire/:id/read",
    doc: "Read one slave now (a temperature conversion takes up to 750 ms). ?eeprom=1 also returns EEPROM contents for memory chips",
    handler: async (_r, p, url) => {
      const s = mustSlave(p.id);
      const d = await readSlave(s, { eeprom: !!url.searchParams.get("eeprom") });
      record(s, d);
      ctx.changed();
      return { ...slaveView(s), eeprom: (d as any).eeprom };
    },
  },
  {
    method: "POST",
    path: "/api/onewire/:id/output",
    doc: { description: "Set outputs on DS2408 (pin 0–7), DS2413/DS2406 (pin A/B). on: true = output transistor conducting (pin pulled low). Or value: raw latch byte. Requires confirm: true", body: { pin: "A", on: true, confirm: true } },
    handler: async (req, p) => {
      const a = await body(req);
      if (a.confirm !== true) throw new HttpError(400, 'Switching outputs drives hardware; repeat with "confirm": true');
      return setOutput(mustSlave(p.id), a);
    },
  },
  {
    method: "PUT",
    path: "/api/onewire/:id/resolution",
    doc: { description: "Set DS18B20/DS1822/DS28EA00 resolution (9–12 bit). save: true also stores it in the sensor's EEPROM (limited write cycles)", body: { bits: 12, save: false } },
    handler: async (req, p) => {
      const s = mustSlave(p.id);
      const a = await body(req);
      const bits = Number(a.bits);
      if (s.family.kind !== "temp" || s.fam === 0x10) throw new HttpError(409, `${s.family.name} has no adjustable resolution`);
      if (![9, 10, 11, 12].includes(bits)) throw new HttpError(400, "bits must be 9–12");
      await wr(`${ROOT}/${s.id}/resolution`, `${bits}\n`);
      if (a.save) await wr(`${ROOT}/${s.id}/eeprom_cmd`, "save\n");
      record(s, await readSlave(s));
      ctx.changed();
      return slaveView(s);
    },
  },
  {
    method: "PUT",
    path: "/api/onewire/:id/label",
    doc: { description: "Name a slave (saved by ROM id)", body: { label: "Freezer" } },
    handler: async (req, p) => {
      const s = mustSlave(p.id);
      const a = await body(req);
      const l = { ...labels() };
      if (a.label) l[s.id] = String(a.label).slice(0, 60);
      else delete l[s.id];
      ctx.settings.set("labels", l);
      if (a.rsens != null) ctx.settings.set(`rsens:${s.id}`, Number(a.rsens) || 0);
      ctx.changed();
      return slaveView(s);
    },
  },
  {
    method: "PUT",
    path: "/api/onewire/settings",
    doc: { description: "interval: ms between background read cycles (≥2000); bulk: simultaneous conversion of all temperature sensors on a master", body: { interval: 10000, bulk: true } },
    handler: async (req) => {
      const a = await body(req);
      if (a.interval != null) {
        const n = Number(a.interval);
        if (!(n >= 2000 && n <= 3600_000)) throw new HttpError(400, "interval must be 2000–3600000 ms");
        ctx.settings.set("interval", n);
        schedulePoll(100);
      }
      if (a.bulk != null) ctx.settings.set("bulk", !!a.bulk);
      ctx.changed();
      return view();
    },
  },
  {
    method: "POST",
    path: "/api/onewire/bridge",
    doc: { description: "Bind (or with bind:false, unbind) the kernel ds2482 driver to a DS2482 I²C→1-Wire bridge, which then appears as another master", body: { bus: 1, address: "0x18", bind: true } },
    handler: async (req) => bindBridge(await body(req)),
  },
];

// ---------- MCP ----------

function slaveSummary(s: Slave) {
  return {
    id: s.id,
    chip: s.family.name,
    label: labels()[s.id] || undefined,
    present: s.present,
    master: s.master,
    readings: Object.fromEntries((s.decoded?.values || []).map((v) => [v.label, v.unit && v.v != null ? `${v.v} ${v.unit}` : v.v])),
    pins: s.decoded?.pins || undefined,
    warnings: s.decoded?.warn?.length ? s.decoded.warn : undefined,
    error: s.error || undefined,
    readAt: s.readAt ? new Date(s.readAt).toISOString() : null,
  };
}

const tools: Tool[] = [
  {
    name: "onewire_list",
    description: "List 1-Wire masters and slaves (DS18B20 temperatures, DS2438, DS2408/DS2413 I/O, iButtons, EEPROMs…) with their latest readings, plus the recent iButton touch log.",
    inputSchema: { type: "object", properties: {} },
    handler: () => ({
      masters: view().masters,
      slaves: [...slaves.values()].map(slaveSummary),
      touchLog: touchLog.slice(-20).map((t) => ({ ...t, t: new Date(t.t).toISOString() })),
    }),
  },
  {
    name: "onewire_read",
    description: "Read one 1-Wire slave now by ROM id (e.g. 28-0000075b1a2c). Temperature sensors take up to 750 ms. eeprom: true returns memory contents for EEPROM chips.",
    inputSchema: { type: "object", properties: { id: { type: "string" }, eeprom: { type: "boolean" } }, required: ["id"] },
    handler: async (a) => {
      const s = mustSlave(a.id);
      const d = await readSlave(s, { eeprom: !!a.eeprom });
      record(s, d);
      return { ...slaveSummary(s), eeprom: (d as any).eeprom };
    },
  },
  {
    name: "onewire_search",
    description: "Trigger an immediate 1-Wire bus search (new devices otherwise appear within the master's search interval, ~10 s) and return the slave list.",
    inputSchema: { type: "object", properties: { master: { type: "string", description: "e.g. w1_bus_master1 (default: first)" } } },
    handler: async (a) => {
      await searchNow(mustMaster(a.master));
      return [...slaves.values()].filter((s) => s.present).map(slaveSummary);
    },
  },
];

// ---------- driver ----------

const driver: Driver = {
  id: "onewire",
  title: "1-Wire",
  order: 41,
  description: "1-Wire buses (GPIO4, DS2482, DS9490R): DS18B20 and other sensors, I/O switches, iButton touch log",
  async init(c) {
    ctx = c;
    stopped = true; // suppress events for the initial enumeration
    await loadGpioPins();
    await refresh();
    stopped = false;
    presenceTimer = setInterval(() => void refresh(), 2000);
    schedulePoll(200);
  },
  stop() {
    stopped = true;
    if (presenceTimer) clearInterval(presenceTimer);
    if (pollTimer) clearTimeout(pollTimer);
    presenceTimer = pollTimer = null;
    masters.clear();
    slaves.clear();
  },
  udev(ev: UdevEvent) {
    if (ev.props.SUBSYSTEM === "w1") void refresh(); // slave added/removed: react immediately (iButton touches)
  },
  status() {
    const present = [...slaves.values()].filter((s) => s.present);
    const ms = [...masters.values()];
    const where = ms.map((m) => m.label.replace(/ \(header pin \d+\)/, "")).join(", ") || "no master";
    let summary: string;
    if (!present.length) summary = `${where} · no devices`;
    else {
      const temps = present.flatMap((s) => (s.decoded?.values || []).filter((v) => v.k === "t" && typeof v.v === "number").map((v) => `${(v.v as number).toFixed(1)} °C`));
      summary = `${where} · ${present.length} device${present.length > 1 ? "s" : ""}${temps.length ? ": " + temps.slice(0, 4).join(", ") : ""}`;
    }
    return { active: present.length > 0, summary, ...view() };
  },
  routes,
  tools,
};

export default driver;
