// ==================== GPIO ====================
//
// Live state of the SoC's GPIO bank (function, level, pull from `pinctrl`; kernel consumers
// from libgpiod's `gpioinfo`), a 40-pin header view, opt-in output control, and the
// I²C / SPI / UART / 1-Wire switches in config.txt, plus (on BCM2711) the extra I²C3–6 / UART2–5
// controllers and a software i2c-gpio bus, with a pin-conflict check across all of them.
//
// Cost model: nothing polls unless someone watches. /ws/gpio streams at ~10 Hz while at
// least one client is connected (pinctrl takes ~3 ms) and stops with the last one; REST
// reads are on demand. status() only reports cached settings, so the section stays folded
// in the Tools strip unless output control is switched on.
//
// Safety: output control defaults off and is refused for lines a kernel driver holds
// (gpioinfo consumer), pins muxed to a peripheral (ALT functions: I²C, UART…), GPIO0/1 (HAT
// ID EEPROM) and anything past GPIO27 (SoC-internal: Ethernet, SD, Wi-Fi). config.txt edits
// are staged as a diff, applied only with an explicit confirm, backed up first, and need a
// reboot, which is a second, separate confirm. BENCHBOB_CONFIG_TXT points at a copy for
// testing, and while it's set reboot is refused outright.
//
// Other boards (peripherals/boards/): everything above is the Raspberry Pi path, picked when
// the board says gpio: "rpi". Elsewhere the panel reads the kernel's gpiochips through the
// v2 uAPI (lib/gpiochip.ts): every line's name, owner and flags, mapped onto the board's
// header where it has a map. Levels are only read for lines the user chooses to watch
// (requested "as is", so nothing about the pin changes; released on unwatch), because the
// uAPI can't read a level without a request. Pins can't be driven there yet. Boot-time
// buses are device-tree overlays in armbianEnv.txt / orangepiEnv.txt / extlinux.conf
// (lib/boot-env.ts), staged and applied with the same diff, backup and reboot steps.
// BENCHBOB_BOOT_ENV (and BENCHBOB_OVERLAY_DIR) point that editor at test copies.
// Watching also works on the Pi, for chips other than the SoC's header bank (USB GPIO
// adapters such as an FT232H or MCP2221, the firmware's expander).

import { access, constants, readdir, readlink } from "node:fs/promises";
import { readdirSync } from "node:fs";
import type { ServerWebSocket } from "bun";
import { board, boardInfo, bootConfigOf, resolveChips } from "../boards/index.ts";
import { body, HttpError, readText, run, type Ctx, type Driver, type Route, type Tool, type WsData } from "../core.ts";
import * as BE from "../lib/boot-env.ts";
import * as GC from "../lib/gpiochip.ts";
import * as G from "../lib/gpio-config.ts";
import { openGpioLevels, type GpioLevels } from "../lib/gpiomem.ts";

const PATH = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const CONFIG = process.env.BENCHBOB_CONFIG_TXT || "/boot/firmware/config.txt";
const CONFIG_OVERRIDDEN = !!process.env.BENCHBOB_CONFIG_TXT;
const CMDLINE = process.env.BENCHBOB_CMDLINE_TXT || "/boot/firmware/cmdline.txt";
const STREAM_MS = 100;
const CONSUMERS_MS = 5000;
// With direct level reads, pinctrl (function, pull) only needs to run this often.
const FULL_MS = 1000;

let ctx: Ctx;
let pinctrlBin: string | null = null;
let gpioinfoBin: string | null = null;
const watchers = new Set<ServerWebSocket<WsData>>();
let loop: ReturnType<typeof setTimeout> | null = null;
let snap: { at: number; pins: G.PinState[]; lines: G.LineInfo[]; linesAt: number; error: string } = { at: 0, pins: [], lines: [], linesAt: 0, error: "" };
let lastSent = "";
let lastSentAt = 0;
let lastLevels = "";
let staged: { token: string; text: string; base: string; at: number; summary: string[] } | null = null;
let rebootRequired = false;

// ---------- reading ----------

async function readPins(): Promise<G.PinState[]> {
  if (!pinctrlBin) throw new HttpError(503, "pinctrl not found (package raspi-utils)");
  const r = await run([pinctrlBin, "get", "0-57"], { timeoutMs: 3000 });
  if (r.code !== 0) throw new HttpError(502, `pinctrl: ${r.err || r.out}`);
  return G.parsePinctrl(r.out);
}

async function readLines(): Promise<G.LineInfo[]> {
  if (!gpioinfoBin) return [];
  const r = await run([gpioinfoBin], { timeoutMs: 3000 });
  return r.code === 0 ? G.parseGpioinfo(r.out) : [];
}

/** The libgpiod chip for the SoC bank (gpiochip0 on a Pi 4: "pinctrl-bcm2711", 58 lines). */
function socLines(lines: G.LineInfo[]) {
  const counts = new Map<string, number>();
  for (const l of lines) counts.set(l.chip, (counts.get(l.chip) || 0) + 1);
  const chip = [...counts].sort((a, b) => b[1] - a[1]).find(([, n]) => n >= 54)?.[0] || "gpiochip0";
  return new Map(lines.filter((l) => l.chip === chip).map((l) => [l.offset, l]));
}

async function refresh(withLines: boolean) {
  try {
    const [pins, lines] = await Promise.all([readPins(), withLines ? readLines() : Promise.resolve(null)]);
    snap.pins = pins;
    snap.at = Date.now();
    if (lines) {
      snap.lines = lines;
      snap.linesAt = Date.now();
    }
    snap.error = "";
  } catch (e: any) {
    snap.error = e.message;
  }
  return snap;
}

/** Why a pin can't be driven, or null if it can. */
function refusal(p: G.PinState | undefined, line: G.LineInfo | undefined): string | null {
  if (!p) return "unknown GPIO";
  if (p.gpio <= 1) return "GPIO0/1 are the HAT ID EEPROM bus (ID_SD/ID_SC)";
  if (p.gpio > 27) return "SoC-internal pin (not on the header)";
  if (line?.consumer) return `claimed by the kernel (${line.consumer})`;
  if (/^a\d$/.test(p.fsel)) return `muxed to ${p.func} (${p.fsel.toUpperCase()})`;
  return null;
}

function pinView(p: G.PinState, lines: Map<number, G.LineInfo>) {
  const line = lines.get(p.gpio);
  return {
    gpio: p.gpio,
    header: G.GPIO_TO_PIN.get(p.gpio) ?? null,
    name: p.name,
    fsel: p.fsel,
    func: p.func,
    level: p.level,
    pull: p.pull,
    drive: p.drive,
    consumer: line?.consumer || null,
    lineName: line?.name || null,
    refusal: refusal(p, line),
  };
}

function stateView() {
  if (!isPi()) return chipStateView();
  const lines = socLines(snap.lines);
  return { mode: "rpi", at: snap.at, error: snap.error, outputEnabled: outputEnabled(), pins: snap.pins.map((p) => pinView(p, lines)), chips: chipsView(true), watched: [...watches.keys()] };
}

const outputEnabled = () => !!ctx.settings.get("outputEnabled", false);

// ---------- gpiochips (every board) ----------
//
// The uAPI view: on a Pi only the chips besides the SoC bank (that one is pinctrl's), on
// every other board all of them, mapped onto the header where the board file has one.

const isPi = () => board().def.gpio === "rpi";
interface ChipState extends GC.Chip {
  info: GC.LineInfo[];
}
let chipSnap: { at: number; list: ChipState[]; error: string } = { at: 0, list: [], error: "" };

function refreshChips() {
  try {
    chipSnap.list = GC.listChips().map((c) => {
      if (c.error) return { ...c, info: [] };
      try {
        return { ...c, info: GC.readLines(c) };
      } catch (e: any) {
        return { ...c, error: e.message, info: [] };
      }
    });
    chipSnap.error = "";
  } catch (e: any) {
    chipSnap.error = e.message;
  }
  chipSnap.at = Date.now();
  // A chip that went away (a USB adapter unplugged) takes its watches with it.
  for (const [k, w] of watches) if (!chipSnap.list.some((c) => c.name === w.chip)) unwatch(k);
  return chipSnap;
}

/** The Pi's header bank: shown through pinctrl, so not listed (or watchable) as a chip. */
const isSocBank = (c: { label: string }) => /^pinctrl-(bcm|rp1)/.test(c.label);

function chipsView(otherOnly: boolean) {
  return chipSnap.list
    .filter((c) => !otherOnly || !isSocBank(c))
    .map((c) => ({
      name: c.name,
      label: c.label,
      device: c.device,
      usb: c.usb,
      error: c.error || null,
      count: c.lines,
      lines: c.info.map((l) => ({ offset: l.offset, name: l.name, consumer: l.consumer, used: l.used, direction: l.direction, flags: l.flags, watched: watches.has(lineKey(c.name, l.offset)) })),
    }));
}

/** The board's headers with each GPIO pin's live line info (null where the chip isn't found). */
function headersView() {
  const def = board().def;
  const chips = resolveChips(def, chipSnap.list);
  return def.headers.map((h) => ({
    id: h.id,
    name: h.name,
    pins: h.pins.map((p) => {
      if (p.kind !== "gpio") return p;
      const c = chips.get(p.chip!);
      const li = c?.info[p.line!];
      return { ...p, chipName: c?.name ?? null, lineName: li?.name ?? null, consumer: li?.consumer ?? null, used: li?.used ?? null, direction: li?.direction ?? null, flags: li?.flags ?? [], watched: c ? watches.has(lineKey(c.name, p.line!)) : false };
    }),
  }));
}

function chipStateView() {
  return { mode: "gpiochip", at: chipSnap.at, error: chipSnap.error, board: boardInfo(), headers: headersView(), chips: chipsView(false), watched: [...watches.keys()] };
}

// ---------- watching lines ----------
//
// A watch holds an as-is line request (see lib/gpiochip.ts) so the level can be read at the
// stream's 10 Hz. It lasts until unwatched or BenchBob stops; it isn't saved, because a
// held request blocks anything else (a driver, a script) from claiming that line.

const MAX_WATCHES = 64;
const lineKey = (chip: string, line: number) => `${chip}:${line}`;
const watches = new Map<string, { chip: string; line: number; w: GC.LineWatch; level: 0 | 1 | null; error: string | null }>();

function watch(chip: string, line: number) {
  if (!chipSnap.list.some((c) => c.name === chip)) refreshChips();
  const c = chipSnap.list.find((x) => x.name === chip);
  if (!c) throw new HttpError(404, `No GPIO chip ${chip}`);
  if (c.error) throw new HttpError(503, c.error);
  if (isPi() && isSocBank(c)) throw new HttpError(409, `${chip} is the Pi's header bank: its levels are already live in the GPIO view`);
  if (!(Number.isInteger(line) && line >= 0 && line < c.lines)) throw new HttpError(400, `${chip} has lines 0–${c.lines - 1}`);
  const k = lineKey(chip, line);
  if (watches.has(k)) return watchView(k);
  if (watches.size >= MAX_WATCHES) throw new HttpError(409, `At most ${MAX_WATCHES} lines can be watched at once`);
  const li = c.info[line];
  if (li?.used) throw new HttpError(409, `${chip} line ${line}${li.name ? ` (${li.name})` : ""} is in use by ${li.consumer}; watching it would mean taking it from its owner`);
  let w: GC.LineWatch;
  try {
    w = GC.watchLine(c.path, line, "benchbob");
  } catch (e: any) {
    throw new HttpError(e instanceof GC.GpioError && e.errno === 16 ? 409 : 502, e.message);
  }
  const x = { chip, line, w, level: null as 0 | 1 | null, error: null as string | null };
  watches.set(k, x);
  readWatch(x);
  ctx.event(`Watching ${chip} line ${line}${li?.name ? ` (${li.name})` : ""}`);
  refreshChips();
  ctx.changed();
  if (watchers.size) startLoop();
  return watchView(k);
}

function unwatch(k: string) {
  const x = watches.get(k);
  if (!x) return false;
  x.w.close();
  watches.delete(k);
  return true;
}

function readWatch(x: { w: GC.LineWatch; level: 0 | 1 | null; error: string | null }) {
  try {
    x.level = x.w.read();
    x.error = null;
  } catch (e: any) {
    x.level = null;
    x.error = e.message;
  }
}

function levelsView() {
  return Object.fromEntries([...watches].map(([k, x]) => [k, x.error ? { level: null, error: x.error } : { level: x.level }]));
}

function watchView(k: string) {
  const x = watches.get(k)!;
  return { key: k, chip: x.chip, line: x.line, level: x.level, error: x.error };
}

/**
 * One-shot reads for the API and MCP: a watched line's last level, otherwise an as-is
 * request held just long enough to read it. Same refusal as watching for lines in use.
 */
function readOnce(chip: string, line: number): { chip: string; line: number; name: string | null; level: 0 | 1 | null; error?: string } {
  const k = lineKey(chip, line);
  const x = watches.get(k);
  if (x) {
    readWatch(x);
    return { chip, line, name: chipSnap.list.find((c) => c.name === chip)?.info[line]?.name || null, level: x.level, ...(x.error ? { error: x.error } : {}) };
  }
  if (!chipSnap.list.some((c) => c.name === chip)) refreshChips();
  const c = chipSnap.list.find((y) => y.name === chip);
  const name = c?.info[line]?.name || null;
  try {
    if (!c) throw new Error(`No GPIO chip ${chip}`);
    if (isPi() && isSocBank(c)) throw new Error("the Pi's header bank: use gpios instead");
    const li = c.info[line];
    if (!li) throw new Error(`${chip} has lines 0–${c.lines - 1}`);
    if (li.used) throw new Error(`in use by ${li.consumer}`);
    const w = GC.watchLine(c.path, line, "benchbob");
    try {
      return { chip, line, name, level: w.read() };
    } finally {
      w.close();
    }
  } catch (e: any) {
    return { chip, line, name, level: null, error: e.message };
  }
}

// ---------- streaming ----------

// BCM283x/2711 only: live levels read from the GPIO registers (see lib/gpiomem.ts), so the
// 10 Hz stream runs pinctrl once a second instead of every tick. null on a Pi 5.
let levels: GpioLevels | null = null;

function overlayLevels(bits: bigint) {
  for (const p of snap.pins) if (p.gpio < 58) p.level = Number((bits >> BigInt(p.gpio)) & 1n) as 0 | 1;
}

function startLoop() {
  if (loop) return;
  const tick = async () => {
    if (!watchers.size) {
      loop = null;
      return;
    }
    if (isPi()) {
      levels ??= openGpioLevels();
      if (!levels || Date.now() - snap.at > FULL_MS) await refresh(Date.now() - snap.linesAt > CONSUMERS_MS);
      else overlayLevels(levels.read());
      // Other chips (USB adapters) change rarely: their line info at the consumers' pace.
      if (Date.now() - chipSnap.at > CONSUMERS_MS) refreshChips();
    } else if (Date.now() - chipSnap.at > FULL_MS) refreshChips();
    const msg = JSON.stringify({ type: "state", ...stateView() });
    // Only push changes, plus a heartbeat so clients can show "live".
    const body = msg.replace(/"at":\d+,/, "");
    if (body !== lastSent || Date.now() - lastSentAt > 2000) {
      lastSent = body;
      lastSentAt = Date.now();
      for (const ws of watchers) ws.send(msg);
    }
    // Watched lines' levels go separately, so a toggling input doesn't resend every chip.
    if (watches.size) {
      for (const x of watches.values()) readWatch(x);
      const lv = JSON.stringify({ type: "levels", levels: levelsView() });
      if (lv !== lastLevels) {
        lastLevels = lv;
        for (const ws of watchers) ws.send(lv);
      }
    }
    loop = watchers.size ? setTimeout(tick, STREAM_MS) : null;
  };
  loop = setTimeout(tick, 0);
}

// ---------- output control ----------

async function writePin(gpio: number, a: { mode?: string; pull?: string }) {
  if (!isPi()) throw new HttpError(409, `Driving pins is only implemented on the Raspberry Pi so far; on ${board().name} lines can be watched (POST /api/gpio/watch) but not set`);
  if (!outputEnabled()) throw new HttpError(403, "GPIO output control is off. Enable it first (POST /api/gpio/output-control with enabled: true, confirm: true).");
  if (!pinctrlBin) throw new HttpError(503, "pinctrl not found");
  await refresh(true);
  const p = snap.pins.find((x) => x.gpio === gpio);
  const why = refusal(p, socLines(snap.lines).get(gpio));
  if (why) throw new HttpError(409, `GPIO${gpio}: ${why}`);
  const args: string[] = [];
  const mode = a.mode?.toLowerCase();
  if (mode === "high") args.push("op", "dh");
  else if (mode === "low") args.push("op", "dl");
  else if (mode === "input") args.push("ip");
  else if (mode != null) throw new HttpError(400, "mode must be high, low or input");
  const pull = a.pull?.toLowerCase();
  if (pull === "up") args.push("pu");
  else if (pull === "down") args.push("pd");
  else if (pull === "none") args.push("pn");
  else if (pull != null) throw new HttpError(400, "pull must be up, down or none");
  if (!args.length) throw new HttpError(400, "Nothing to set: give mode and/or pull");
  // pinctrl writes the GPIO registers directly, so the state persists after it exits
  // (unlike gpioset, which only holds a line while running).
  const r = await run([pinctrlBin, "set", String(gpio), ...args], { timeoutMs: 3000 });
  if (r.code !== 0) throw new HttpError(502, `pinctrl set: ${r.err || r.out}`);
  await refresh(false);
  const now = snap.pins.find((x) => x.gpio === gpio)!;
  ctx.event(`GPIO${gpio} (pin ${G.GPIO_TO_PIN.get(gpio)}) set: ${[mode, pull && `pull ${pull}`].filter(Boolean).join(", ")}`);
  ctx.changed();
  return pinView(now, socLines(snap.lines));
}

// ---------- interfaces ----------

async function dtStatus(alias: string): Promise<{ path: string; okay: boolean } | null> {
  const path = (await readText(`/proc/device-tree/aliases/${alias}`)).replace(/\0/g, "");
  if (!path) return null;
  const st = (await readText(`/proc/device-tree${path}/status`)).replace(/\0/g, "");
  return { path, okay: st === "" || st === "okay" };
}

async function dtClock(path: string): Promise<number | null> {
  try {
    const b = new Uint8Array(await Bun.file(`/proc/device-tree${path}/clock-frequency`).arrayBuffer());
    if (b.length >= 4) return new DataView(b.buffer).getUint32(0);
  } catch {}
  return null;
}

/** Pi 4 / 400 / CM4: the only boards the extra-controller overlays (i2c3–6, uart2–5) are offered on. */
let bcm2711: boolean | null = null;
async function isBcm2711() {
  if (bcm2711 == null) bcm2711 = process.env.BENCHBOB_BOARD_BCM2711 ? process.env.BENCHBOB_BOARD_BCM2711 === "1" : (await readText("/proc/device-tree/compatible")).includes("brcm,bcm2711");
  return bcm2711;
}

async function liveInterfaces() {
  const i2c = await dtStatus("i2c1");
  const hz = i2c ? await dtClock(i2c.path) : null;
  const spi = await dtStatus("spi0");
  const spidev = (await readdir("/dev").catch(() => [] as string[])).filter((n) => n.startsWith("spidev"));
  const serial0 = await readlink("/dev/serial0").catch(() => null);
  const cmdline = await readText("/proc/cmdline");
  const consoles = [...cmdline.matchAll(/console=([^,\s]+)/g)].map((m) => m[1]);
  const serialConsole = consoles.find((c) => /^(ttyS|ttyAMA|serial)/.test(c)) || null;
  const bootCmdline = await readText(CMDLINE);
  const w1 = [];
  for (const n of await readdir("/proc/device-tree").catch(() => [] as string[])) {
    if (!n.startsWith("onewire")) continue;
    const g = await Bun.file(`/proc/device-tree/${n}/gpios`).arrayBuffer().catch(() => null);
    if (g && g.byteLength >= 8) w1.push({ node: n, gpio: new DataView(g).getUint32(4) });
  }
  const masters = (await readdir("/sys/bus/w1/devices").catch(() => [] as string[])).filter((n) => n.startsWith("w1_bus_master"));
  const exists = (p: string) => access(p).then(() => true, () => false);
  // Extra controllers: device-tree status (the overlay sets it "okay") and the node it made.
  const extraI2c: Record<string, { enabled: boolean; dev: string | null; hz: number | null }> = {};
  if (await isBcm2711())
    for (const n of Object.keys(G.EXTRA_I2C)) {
      const s = await dtStatus(n);
      const dev = `/dev/i2c-${n.slice(3)}`;
      extraI2c[n] = { enabled: !!s?.okay, dev: (await exists(dev)) ? dev : null, hz: s?.okay ? await dtClock(s.path) : null };
    }
  const ttys = G.onboardUarts();
  const extraUart: Record<string, { enabled: boolean; tty: string | null }> = {};
  if (await isBcm2711())
    for (const n of Object.keys(G.EXTRA_UART)) {
      const s = await dtStatus(n);
      extraUart[n] = { enabled: !!s?.okay, tty: ttys.find((t) => t.uart === n)?.tty ?? null };
    }
  const gpioBuses = [];
  for (const n of await readdir("/proc/device-tree").catch(() => [] as string[])) {
    if (!/^i2c@?\d*$/.test(n) && !n.startsWith("i2c-gpio")) continue;
    if ((await readText(`/proc/device-tree/${n}/compatible`)).replace(/\0/g, " ").includes("i2c-gpio")) gpioBuses.push(n);
  }
  return {
    board: { model: (await readText("/proc/device-tree/model")).replace(/\0/g, ""), bcm2711: await isBcm2711() },
    extraI2c,
    extraUart,
    i2cGpio: { nodes: gpioBuses },
    i2c: { enabled: !!i2c?.okay, dev: (await access("/dev/i2c-1").then(() => true, () => false)) ? "/dev/i2c-1" : null, hz },
    spi: { enabled: !!spi?.okay, devices: spidev.map((n) => "/dev/" + n) },
    uart: { serial0: serial0 ? "/dev/" + serial0 : null, consoleOnSerial: serialConsole, bootConsole: /console=serial0/.test(bootCmdline) ? "serial0" : null, note: serial0 === "ttyS0" ? "serial0 is the mini-UART (ttyS0); the PL011 drives Bluetooth" : serial0 === "ttyAMA0" ? "serial0 is the PL011 (ttyAMA0)" : "" },
    onewire: { nodes: w1, masters: masters.length },
  };
}

async function interfaces() {
  const text = await readText(CONFIG);
  if (!text) throw new HttpError(500, `Can't read ${CONFIG}`);
  const configured = G.readInterfaces(text);
  return {
    configPath: CONFIG,
    testCopy: CONFIG_OVERRIDDEN,
    configured,
    live: await liveInterfaces(),
    // What the dialog may offer here, and the pin tables behind it (so the UI doesn't keep its own copy).
    catalog: (await isBcm2711()) ? { extraI2c: G.EXTRA_I2C, extraUart: G.EXTRA_UART, i2cGpio: G.I2C_GPIO_DEFAULTS } : null,
    pins: G.pinUsage(configured),
    conflicts: G.pinConflicts(configured),
    rebootRequired,
    staged: staged ? { token: staged.token, summary: staged.summary, at: staged.at } : null,
  };
}

function parseChanges(a: any): G.InterfaceChanges {
  const ch: G.InterfaceChanges = {};
  const bool = (v: any, what: string) => {
    if (typeof v !== "boolean") throw new HttpError(400, `${what} must be true or false`);
    return v;
  };
  if (a.i2c) {
    ch.i2c = {};
    if (a.i2c.enabled != null) ch.i2c.enabled = bool(a.i2c.enabled, "i2c.enabled");
    if (a.i2c.baudrate != null) {
      const b = Number(a.i2c.baudrate);
      if (!(b >= 10000 && b <= 3400000)) throw new HttpError(400, "i2c.baudrate must be 10000–3400000 Hz");
      ch.i2c.baudrate = b;
    }
  }
  if (a.spi?.enabled != null) ch.spi = { enabled: bool(a.spi.enabled, "spi.enabled") };
  if (a.uart?.enabled != null) ch.uart = { enabled: bool(a.uart.enabled, "uart.enabled") };
  if (a.onewire) {
    ch.onewire = {};
    if (a.onewire.enabled != null) ch.onewire.enabled = bool(a.onewire.enabled, "onewire.enabled");
    if (a.onewire.pin != null) {
      const p = Number(a.onewire.pin);
      if (!(Number.isInteger(p) && p >= 2 && p <= 27)) throw new HttpError(400, "onewire.pin must be GPIO2–27");
      ch.onewire.pin = p;
    }
  }
  const obj = (v: any, what: string) => {
    if (typeof v !== "object" || v == null || Array.isArray(v)) throw new HttpError(400, `${what} must be an object`);
    return v;
  };
  const optNum = (v: any, what: string) => {
    if (v === null) return null;
    const n = Number(v);
    if (!Number.isFinite(n)) throw new HttpError(400, `${what} must be a number`);
    return n;
  };
  if (a.extraI2c) {
    ch.extraI2c = {};
    for (const [n, c] of Object.entries<any>(obj(a.extraI2c, "extraI2c"))) {
      obj(c, `extraI2c.${n}`);
      ch.extraI2c[n] = {};
      if (c.enabled != null) ch.extraI2c[n].enabled = bool(c.enabled, `extraI2c.${n}.enabled`);
      if (c.pins != null) ch.extraI2c[n].pins = String(c.pins);
      if (c.baudrate != null) ch.extraI2c[n].baudrate = optNum(c.baudrate, `extraI2c.${n}.baudrate`)!;
    }
  }
  if (a.extraUart) {
    ch.extraUart = {};
    for (const [n, c] of Object.entries<any>(obj(a.extraUart, "extraUart"))) {
      obj(c, `extraUart.${n}`);
      ch.extraUart[n] = {};
      if (c.enabled != null) ch.extraUart[n].enabled = bool(c.enabled, `extraUart.${n}.enabled`);
      if (c.ctsrts != null) ch.extraUart[n].ctsrts = bool(c.ctsrts, `extraUart.${n}.ctsrts`);
    }
  }
  if (a.i2cGpio) {
    const c = obj(a.i2cGpio, "i2cGpio");
    ch.i2cGpio = {};
    if (c.enabled != null) ch.i2cGpio.enabled = bool(c.enabled, "i2cGpio.enabled");
    for (const k of ["sda", "scl"] as const) if (c[k] != null) ch.i2cGpio[k] = optNum(c[k], `i2cGpio.${k}`)!;
    for (const k of ["bus", "delayUs"] as const) if (c[k] !== undefined) ch.i2cGpio[k] = optNum(c[k], `i2cGpio.${k}`);
  }
  return ch;
}

function describe(before: G.Interfaces, after: G.Interfaces): string[] {
  const s: string[] = [];
  const onoff = (b: boolean) => (b ? "on" : "off");
  if (before.i2c.enabled !== after.i2c.enabled) s.push(`I²C (GPIO2/3) ${onoff(after.i2c.enabled)}`);
  if (before.i2c.baudrate !== after.i2c.baudrate) s.push(`I²C speed ${(after.i2c.baudrate ?? 100000) / 1000} kHz`);
  if (before.spi.enabled !== after.spi.enabled) s.push(`SPI0 ${onoff(after.spi.enabled)}`);
  if (before.uart.enabled !== after.uart.enabled) s.push(`UART (GPIO14/15) ${onoff(after.uart.enabled)}`);
  if (before.onewire.enabled !== after.onewire.enabled) s.push(`1-Wire ${onoff(after.onewire.enabled)}`);
  if (after.onewire.enabled && before.onewire.pin !== after.onewire.pin) s.push(`1-Wire on GPIO${after.onewire.pin}`);
  for (const [n, a] of Object.entries(after.extraI2c)) {
    const b = before.extraI2c[n];
    const pins = `GPIO${G.EXTRA_I2C[n].choices[a.pins].join("/")}`;
    if (a.enabled !== b.enabled) s.push(`${G.i2cLabel(n)} (${pins}) ${onoff(a.enabled)}`);
    else if (a.enabled && a.pins !== b.pins) s.push(`${G.i2cLabel(n)} moved to ${pins}`);
    if (a.enabled && (a.baudrate ?? 100000) !== (b.baudrate ?? 100000)) s.push(`${G.i2cLabel(n)} speed ${(a.baudrate ?? 100000) / 1000} kHz`);
  }
  for (const [n, a] of Object.entries(after.extraUart)) {
    const b = before.extraUart[n], p = G.EXTRA_UART[n];
    if (a.enabled !== b.enabled) s.push(`${n.toUpperCase()} (GPIO${p.tx}/${p.rx}) ${onoff(a.enabled)}`);
    if (a.enabled && a.ctsrts !== b.ctsrts) s.push(`${n.toUpperCase()} CTS/RTS (GPIO${p.cts}/${p.rts}) ${onoff(a.ctsrts)}`);
  }
  const ga = after.i2cGpio, gb = before.i2cGpio;
  if (ga.enabled !== gb.enabled) s.push(`software I²C (i2c-gpio) ${onoff(ga.enabled)}`);
  if (ga.enabled && (ga.sda !== gb.sda || ga.scl !== gb.scl || ga.bus !== gb.bus || ga.delayUs !== gb.delayUs)) s.push(`i2c-gpio on SDA GPIO${ga.sda} / SCL GPIO${ga.scl}${ga.bus != null ? `, bus ${ga.bus}` : ""}`);
  return s;
}

async function stage(a: any) {
  // Raw text (not core's readText, which trims): the diff and the write keep the file's own endings.
  const raw = await Bun.file(CONFIG).text().catch(() => {
    throw new HttpError(500, `Can't read ${CONFIG}`);
  });
  const ch = parseChanges(a);
  const enablesExtra = [...Object.values(ch.extraI2c || {}), ...Object.values(ch.extraUart || {})].some((c) => c.enabled);
  if (enablesExtra && !(await isBcm2711())) throw new HttpError(400, "Extra I²C buses and UARTs (i2c3–6, uart2–5) are BCM2711 overlays (Pi 4, Pi 400, CM4); this board isn't one");
  let next: string;
  try {
    next = G.editConfig(raw, ch);
  } catch (e: any) {
    throw new HttpError(400, e.message);
  }
  const diff = G.unifiedDiff(raw, next, CONFIG);
  const before = G.readInterfaces(raw), after = G.readInterfaces(next);
  const warnings = after.conditional.map((c) => `[${c.section}] also has "${c.line}"; on a board matching [${c.section}] that line wins.`);
  if (before.uart.enabled && !after.uart.enabled && /console=serial0/.test(await readText(CMDLINE))) warnings.push("cmdline.txt puts a login console on serial0; with the UART off that console disappears (cmdline.txt is not changed here).");
  // Two interfaces on one pin can't both work: refuse any overlap this change creates, and
  // keep reminding about ones config.txt already had.
  const key = (c: { gpio: number; users: string[] }) => `${c.gpio}:${c.users.join("|")}`;
  const had = new Set(G.pinConflicts(before).map(key));
  const fresh = G.pinConflicts(after).filter((c) => !had.has(key(c)));
  if (fresh.length) throw new HttpError(400, `Pin conflict: ${fresh.map((c) => `GPIO${c.gpio} would be ${c.users.join(" and ")}`).join("; ")}. Turn one off or move it first.`);
  for (const c of G.pinConflicts(after)) warnings.push(`GPIO${c.gpio} is already claimed twice in config.txt (${c.users.join(", ")}); only one of them can work.`);
  const usesHatPins = (r: G.Interfaces) => G.pinUsage(r).some((u) => u.gpio <= 1);
  if (usesHatPins(after) && !usesHatPins(before)) warnings.push("GPIO0/1 are the HAT ID EEPROM bus: the firmware probes them at boot. Fine with no HAT attached; with a HAT that has an ID EEPROM, don't use them.");
  if (ch.i2cGpio?.enabled && after.i2cGpio.bus == null) warnings.push("i2c-gpio without a bus number gets the next free /dev/i2c-N (probably 3, or higher if I²C3 is on); set a bus number for a stable name.");
  if (!diff) {
    staged = null;
    return { diff: "", summary: [], warnings, message: "No change: config.txt already says that." };
  }
  staged = { token: crypto.randomUUID(), text: next, base: raw, at: Date.now(), summary: describe(before, after) };
  return { token: staged.token, diff, summary: staged.summary, warnings, configPath: CONFIG, testCopy: CONFIG_OVERRIDDEN };
}

async function writeRoot(path: string, data: string) {
  try {
    await access(path, constants.W_OK);
    await Bun.write(path, data);
    return;
  } catch {}
  const r = await run(["sudo", "-n", "tee", path], { stdin: data, timeoutMs: 10_000 });
  if (r.code !== 0) throw new HttpError(502, `writing ${path}: ${r.err || "failed"}`);
}

async function apply(a: any) {
  if (a.confirm !== true) throw new HttpError(400, 'Applying changes config.txt; repeat with "confirm": true');
  if (!staged || staged.token !== a.token) throw new HttpError(409, "Nothing staged with that token (stage again)");
  const now = await Bun.file(CONFIG).text();
  if (now !== staged.base) {
    staged = null;
    throw new HttpError(409, `${CONFIG} changed since the diff was made; stage again`);
  }
  const backup = await backupAndWrite(CONFIG, staged.text);
  const summary = staged.summary;
  staged = null;
  rebootRequired = true;
  ctx.event(`config.txt changed (${summary.join(", ")}); backup ${backup}; reboot required`);
  ctx.changed();
  return { ok: true, backup, summary, rebootRequired: true, message: "Saved. The change takes effect after a reboot." };
}

/** Copy `path` to path.bak-<timestamp> (as root if need be), then write `text`. Returns the backup's path. */
async function backupAndWrite(path: string, text: string) {
  const ts = new Date().toISOString().replace(/[:T]/g, "-").slice(0, 19);
  const backup = `${path}.bak-${ts}`;
  const cp = await run(["cp", "-p", path, backup]);
  if (cp.code !== 0) {
    const s = await run(["sudo", "-n", "cp", "-p", path, backup]);
    if (s.code !== 0) throw new HttpError(502, `backup failed: ${s.err}`);
  }
  await writeRoot(path, text);
  return backup;
}

// ---------- boot overlays (boards other than the Pi) ----------
//
// The list U-Boot applies at boot, in whichever file this image keeps it (boards/*.ts list
// the candidates). Only overlays installed on this image can be enabled (a missing .dtbo
// makes U-Boot skip it or, on Armbian, run its fixup script), and a change that puts two
// of the board's documented overlays on one pin is refused, as on the Pi.

const ENV_OVERRIDE = process.env.BENCHBOB_BOOT_ENV || "";
const DIR_OVERRIDE = process.env.BENCHBOB_OVERLAY_DIR || "";
let stagedOv: { token: string; text: string; base: string; path: string; at: number; summary: string[] } | null = null;

function overlayTarget() {
  const def = board().def;
  const bc = def.bootConfig;
  if (!bc) return null;
  const found = bootConfigOf(def);
  const kind = ENV_OVERRIDE ? (found?.kind ?? bc.files.find((f) => f.kind !== "config.txt")?.kind) : found?.kind;
  if (!kind || kind === "config.txt") return null;
  return { kind, path: ENV_OVERRIDE || found!.path, env: (kind === "extlinux" ? "extlinux" : "env") as BE.EnvKind, bc, testCopy: !!ENV_OVERRIDE };
}

function noOverlays(): string {
  const b = board();
  if (isPi()) return "This is a Raspberry Pi: its buses are in config.txt (GET /api/gpio/interfaces)";
  const files = b.def.bootConfig?.files.map((f) => f.path) || [];
  return files.length ? `None of ${files.join(", ")} exists on this image, so BenchBob can't change boot overlays here${b.def.notes?.length ? ` (${b.def.notes.join(" ")})` : ""}` : `BenchBob doesn't know where ${b.name} keeps its boot configuration`;
}

type Target = NonNullable<ReturnType<typeof overlayTarget>>;

/** The board's documented overlay that this name (an installed file's, or one in overlays=) is. */
function known(t: Target, name: string) {
  const short = name.replace(/^.*\//, "").replace(/\.dtbo$/, "");
  return (t.bc.overlays || []).find((o) => [o.name, ...(o.aliases || [])].some((n) => short === n || short.endsWith(`-${n}`))) || null;
}

/** .dtbo files this image has, as they'd be written in the boot file. */
function installedOverlays(t: Target, prefix: string | null) {
  const out: { name: string; file: string; what: string | null; pins: number[] }[] = [];
  for (const dir of DIR_OVERRIDE ? [DIR_OVERRIDE] : t.bc.overlayDirs || []) {
    let files: string[] = [];
    try {
      files = readdirSync(dir).filter((f) => f.endsWith(".dtbo")).sort();
    } catch {
      continue;
    }
    for (const f of files) {
      const base = f.slice(0, -5);
      // env: Armbian/Orange Pi load <prefix>-<name>.dtbo, so the short name is written;
      // a file without the prefix is written whole (the boot scripts try that too).
      const name = t.env === "extlinux" ? `${t.bc.overlayRef ?? dir + "/"}${f}` : prefix && base.startsWith(`${prefix}-`) ? base.slice(prefix.length + 1) : base;
      const k = known(t, name);
      out.push({ name, file: `${dir}/${f}`, what: k?.what ?? null, pins: k?.pins ?? [] });
    }
  }
  return out;
}

function overlayConflicts(t: Target, enabled: string[]) {
  const by = new Map<number, string[]>();
  for (const n of enabled) for (const p of known(t, n)?.pins || []) by.set(p, [...(by.get(p) || []), n]);
  return [...by].filter(([, u]) => u.length > 1).map(([pin, users]) => ({ pin, users })).sort((a, b) => a.pin - b.pin);
}

async function readTarget(t: Target) {
  // Raw text: the diff and the write keep the file's own line endings.
  return Bun.file(t.path).text().catch(() => {
    throw new HttpError(500, `Can't read ${t.path}`);
  });
}

async function overlaysInfo() {
  const t = overlayTarget();
  if (!t) throw new HttpError(409, noOverlays());
  const cur = BE.readOverlays(t.env, await readTarget(t));
  const installed = installedOverlays(t, cur.prefix);
  return {
    kind: t.kind,
    path: t.path,
    testCopy: t.testCopy,
    prefix: cur.prefix,
    label: cur.label,
    enabled: cur.enabled.map((n) => ({ name: n, what: known(t, n)?.what ?? null, pins: known(t, n)?.pins ?? [], installed: installed.some((i) => i.name === n) })),
    user: cur.user,
    installed,
    overlayDirs: DIR_OVERRIDE ? [DIR_OVERRIDE] : t.bc.overlayDirs || [],
    conflicts: overlayConflicts(t, cur.enabled),
    rebootRequired,
    staged: stagedOv ? { token: stagedOv.token, summary: stagedOv.summary, at: stagedOv.at } : null,
  };
}

async function stageOverlays(a: any) {
  const t = overlayTarget();
  if (!t) throw new HttpError(409, noOverlays());
  const list = (v: any, what: string): string[] => {
    if (v == null) return [];
    if (!Array.isArray(v) || v.some((x) => typeof x !== "string")) throw new HttpError(400, `${what} must be a list of overlay names`);
    return v;
  };
  const enable = list(a.enable, "enable"), disable = list(a.disable, "disable");
  const raw = await readTarget(t);
  const cur = BE.readOverlays(t.env, raw);
  const installed = installedOverlays(t, cur.prefix);
  const missing = enable.filter((n) => !cur.enabled.includes(n) && !installed.some((i) => i.name === n));
  if (missing.length) throw new HttpError(400, `Not installed on this image: ${missing.join(", ")} (looked in ${(DIR_OVERRIDE ? [DIR_OVERRIDE] : t.bc.overlayDirs || []).join(", ") || "no overlay directory"})`);
  let next: string;
  try {
    next = BE.editOverlays(t.env, raw, { enable, disable });
  } catch (e: any) {
    throw new HttpError(400, e.message);
  }
  const after = BE.readOverlays(t.env, next).enabled;
  const key = (c: { pin: number; users: string[] }) => `${c.pin}:${c.users.join("|")}`;
  const had = new Set(overlayConflicts(t, cur.enabled).map(key));
  const fresh = overlayConflicts(t, after).filter((c) => !had.has(key(c)));
  if (fresh.length) throw new HttpError(400, `Pin conflict: ${fresh.map((c) => `pin ${c.pin} would be ${c.users.join(" and ")}`).join("; ")}. Turn one off first.`);
  const warnings = overlayConflicts(t, after).map((c) => `Pin ${c.pin} is already claimed by ${c.users.join(" and ")}; only one of them can work.`);
  for (const n of after) if (!known(t, n) && !cur.enabled.includes(n)) warnings.push(`${n} isn't one of the overlays documented for ${board().name}, so its pins can't be checked for conflicts.`);
  const diff = G.unifiedDiff(raw, next, t.path);
  if (!diff) {
    stagedOv = null;
    return { diff: "", summary: [], warnings, message: `No change: ${t.path} already says that.` };
  }
  const summary = [...after.filter((n) => !cur.enabled.includes(n)).map((n) => `${n} on`), ...cur.enabled.filter((n) => !after.includes(n)).map((n) => `${n} off`)];
  stagedOv = { token: crypto.randomUUID(), text: next, base: raw, path: t.path, at: Date.now(), summary };
  return { token: stagedOv.token, diff, summary, warnings, configPath: t.path, testCopy: t.testCopy };
}

async function applyOverlays(a: any) {
  if (a.confirm !== true) throw new HttpError(400, 'Applying changes the boot configuration; repeat with "confirm": true');
  if (!stagedOv || stagedOv.token !== a.token) throw new HttpError(409, "Nothing staged with that token (stage again)");
  const path = stagedOv.path;
  if ((await Bun.file(path).text()) !== stagedOv.base) {
    stagedOv = null;
    throw new HttpError(409, `${path} changed since the diff was made; stage again`);
  }
  const backup = await backupAndWrite(path, stagedOv.text);
  const summary = stagedOv.summary;
  stagedOv = null;
  rebootRequired = true;
  ctx.event(`Boot overlays changed in ${path} (${summary.join(", ")}); backup ${backup}; reboot required`);
  ctx.changed();
  return { ok: true, backup, summary, rebootRequired: true, message: "Saved. The change takes effect after a reboot." };
}

// ---------- WebSocket, routes, tools ----------

const sockets = [
  {
    path: "/ws/gpio",
    doc: "Live GPIO state while connected: {type:'state', mode:'rpi', pins:[{gpio, header, func, level, pull, consumer, refusal}], outputEnabled, chips} on a Pi, or {type:'state', mode:'gpiochip', headers, chips} elsewhere (as GET /api/gpio), pushed on change (polled at 10 Hz) plus a 2 s heartbeat; {type:'levels', levels:{'gpiochip2:17':{level}}} when a watched line changes. Polling stops when the last client disconnects.",
    open(ws: ServerWebSocket<WsData>) {
      watchers.add(ws);
      if (isPi() ? snap.at : chipSnap.at) ws.send(JSON.stringify({ type: "state", ...stateView() }));
      if (watches.size) ws.send(JSON.stringify({ type: "levels", levels: levelsView() }));
      startLoop();
    },
    close(ws: ServerWebSocket<WsData>) {
      watchers.delete(ws);
    },
  },
];

const routes: Route[] = [
  {
    method: "GET",
    path: "/api/gpio",
    doc: "GPIO state now. Raspberry Pi (mode rpi): the SoC bank from pinctrl + gpioinfo (function/alt name, level, pull, kernel consumer, why a pin can't be driven) and the 40-pin header map, plus other gpiochips (USB adapters). Other boards (mode gpiochip): every gpiochip line (name, consumer, direction, flags) and the board's headers with each pin's line. levels: the watched lines. board: which board this is and whether it's been tested",
    handler: async () => {
      refreshChips();
      if (!isPi()) return { ...stateView(), levels: levelsView() };
      await refresh(true);
      return { ...stateView(), header: G.HEADER, board: boardInfo(), levels: levelsView() };
    },
  },
  {
    method: "POST",
    path: "/api/gpio/watch",
    doc: { description: "Start or stop watching a gpiochip line's level (streamed on /ws/gpio as {type:'levels'}). The line is requested as-is, so its direction, bias and drive don't change; lines in use by a driver are refused. Not for the Pi's header bank, whose levels are always live", body: { chip: "gpiochip2", line: 17, watch: true } },
    handler: async (req) => {
      const a = await body(req);
      const chip = String(a.chip || ""), line = Number(a.line);
      if (!/^gpiochip\d+$/.test(chip)) throw new HttpError(400, 'chip must be a gpiochip name, e.g. "gpiochip2"');
      if (a.watch === false) {
        const was = unwatch(lineKey(chip, line));
        if (was) {
          refreshChips();
          ctx.changed();
        }
        return { ok: true, watching: false };
      }
      return { ok: true, watching: true, ...watch(chip, line) };
    },
  },
  { method: "GET", path: "/api/gpio/overlays", doc: "Boards other than the Pi: the device-tree overlays the boot file enables (armbianEnv.txt, orangepiEnv.txt or extlinux.conf), the ones installed on this image, which header pins each documented one takes, and conflicts", handler: () => overlaysInfo() },
  {
    method: "POST",
    path: "/api/gpio/overlays/stage",
    doc: { description: "Compute a boot-overlay change and return a unified diff + token. Nothing is written. Only installed overlays can be enabled; a change that puts two documented overlays on one pin is refused (400)", body: { enable: ["i2c5-m3"], disable: ["uart1-m1"] } },
    handler: async (req) => stageOverlays(await body(req)),
  },
  {
    method: "POST",
    path: "/api/gpio/overlays/apply",
    doc: { description: "Write the staged overlay change (backup first: <file>.bak-<timestamp>). Needs the stage token and confirm: true. Takes effect after a reboot", body: { token: "…", confirm: true } },
    handler: async (req) => applyOverlays(await body(req)),
  },
  {
    method: "POST",
    path: "/api/gpio/output-control",
    doc: { description: "Turn GPIO output control on/off (default off). Enabling requires confirm: true", body: { enabled: true, confirm: true } },
    handler: async (req) => {
      const a = await body(req);
      if (a.enabled && a.confirm !== true) throw new HttpError(400, 'Enabling output control lets this API drive header pins; repeat with "confirm": true');
      ctx.settings.set("outputEnabled", !!a.enabled);
      ctx.event(`GPIO output control ${a.enabled ? "enabled" : "disabled"}`);
      ctx.changed();
      return { outputEnabled: !!a.enabled };
    },
  },
  { method: "GET", path: "/api/gpio/interfaces", doc: "Raspberry Pi: I²C / SPI / UART / 1-Wire and the extra I²C/UART controllers: what config.txt says ([all] context), what is live now, the GPIOs each uses and any pin conflicts. Other boards: see /api/gpio/overlays", handler: () => piOnly(interfaces) },
  {
    method: "POST",
    path: "/api/gpio/interfaces/stage",
    doc: { description: "Compute a config.txt change and return a unified diff + token. Nothing is written. extraI2c (i2c3–6, pins = pins_a_b), extraUart (uart2–5) and i2cGpio are offered on BCM2711 boards only (see catalog in GET /api/gpio/interfaces). A change that puts two interfaces on one GPIO is refused (400)", body: { i2c: { enabled: true, baudrate: 400000 }, spi: { enabled: true }, uart: { enabled: true }, onewire: { enabled: true, pin: 4 }, extraI2c: { i2c6: { enabled: true, pins: "pins_22_23", baudrate: 400000 } }, extraUart: { uart2: { enabled: true, ctsrts: false } }, i2cGpio: { enabled: false, sda: 23, scl: 24, bus: 11 } } },
    handler: async (req) => piOnly(async () => stage(await body(req))),
  },
  {
    method: "POST",
    path: "/api/gpio/interfaces/apply",
    doc: { description: "Write the staged config.txt (backup first: config.txt.bak-<timestamp>). Needs the stage token and confirm: true. Takes effect after a reboot", body: { token: "…", confirm: true } },
    handler: async (req) => piOnly(async () => apply(await body(req))),
  },
  {
    method: "POST",
    path: "/api/gpio/reboot",
    doc: { description: 'Reboot so config.txt or boot-overlay changes take effect. Requires confirm: "REBOOT"', body: { confirm: "REBOOT" } },
    handler: async (req) => {
      const a = await body(req);
      if (a.confirm !== "REBOOT") throw new HttpError(400, 'Rebooting needs confirm: "REBOOT"');
      if (CONFIG_OVERRIDDEN) throw new HttpError(409, `Refusing to reboot: BENCHBOB_CONFIG_TXT points at a test copy (${CONFIG})`);
      if (ENV_OVERRIDE) throw new HttpError(409, `Refusing to reboot: BENCHBOB_BOOT_ENV points at a test copy (${ENV_OVERRIDE})`);
      ctx.event("Reboot requested from the GPIO panel");
      setTimeout(() => void run(["sudo", "-n", "systemctl", "reboot"]), 500);
      return { ok: true, message: "Rebooting in a moment" };
    },
  },
  // Last: /api/gpio/:gpio would otherwise shadow /api/gpio/reboot etc. (core matches in order).
  {
    method: "POST",
    path: "/api/gpio/:gpio",
    doc: { description: "Drive a header GPIO (needs output control on): mode high|low|input, pull up|down|none. Refused for GPIO0/1, >27, kernel-claimed lines and pins in an ALT function", body: { mode: "high", pull: "none" } },
    handler: async (req, p) => {
      const g = parseInt(p.gpio.replace(/^gpio/i, ""), 10);
      if (!Number.isInteger(g)) throw new HttpError(400, "GPIO number required, e.g. /api/gpio/17");
      return writePin(g, await body(req));
    },
  },
];

/** config.txt routes on a board that has none: say where to look instead. */
async function piOnly<T>(f: () => Promise<T>): Promise<T> {
  if (!isPi()) throw new HttpError(409, `${board().name} isn't a Raspberry Pi: there is no config.txt. ${overlayTarget() ? "Boot overlays: GET /api/gpio/overlays" : noOverlays()}`);
  return f();
}

/** gpio_read's lines argument: [{chip, line}], read once each. */
function readLinesArg(v: any) {
  if (!Array.isArray(v) || !v.length) return undefined;
  refreshChips();
  return v.slice(0, 64).map((x) => readOnce(String(x?.chip), Number(x?.line)));
}

/** A compact header view for agents: GPIO pins with their line and owner. */
function headersForTool() {
  return headersView().map((h) => ({
    header: h.name,
    pins: h.pins.filter((p: any) => p.kind === "gpio").map((p: any) => ({ pin: p.pin, soc: p.soc, chip: p.chipName, line: p.line, lineName: p.lineName, consumer: p.consumer, direction: p.direction, funcs: p.funcs || [] })),
  }));
}

const tools: Tool[] = [
  {
    name: "gpio_read",
    description:
      "Read GPIO state. On a Raspberry Pi: for each pin the function (input/output/ALT name like SDA1, TXD1), level, pull, header pin number and kernel consumer; optionally limit to a list of BCM GPIO numbers. On other boards: the board's header pins with their gpiochip line, owner and direction, and every chip's lines in use. `lines` reads levels of specific gpiochip lines (any board, incl. USB GPIO adapters) with an as-is request, so the pin isn't reconfigured; lines a driver holds can't be read.",
    inputSchema: {
      type: "object",
      properties: {
        gpios: { type: "array", items: { type: "integer", minimum: 0, maximum: 57 }, description: "Raspberry Pi: BCM numbers; default all of 0–27" },
        lines: { type: "array", items: { type: "object", properties: { chip: { type: "string", description: "e.g. gpiochip2" }, line: { type: "integer", minimum: 0 } }, required: ["chip", "line"] }, description: "gpiochip lines to read once" },
      },
    },
    handler: async (a) => {
      const lines = readLinesArg(a.lines);
      if (!isPi()) {
        refreshChips();
        return { board: boardInfo(), headers: headersForTool(), chips: chipsView(false).map((c) => ({ name: c.name, label: c.label, device: c.device, usb: c.usb, lines: c.count, error: c.error, inUse: c.lines.filter((l) => l.used).map((l) => ({ line: l.offset, name: l.name, consumer: l.consumer, direction: l.direction })) })), ...(lines ? { read: lines } : {}) };
      }
      await refresh(true);
      const want = Array.isArray(a.gpios) && a.gpios.length ? new Set(a.gpios.map(Number)) : null;
      return { outputEnabled: outputEnabled(), pins: (stateView() as { pins: ReturnType<typeof pinView>[] }).pins.filter((p) => (want ? want.has(p.gpio) : p.gpio <= 27)), ...(lines ? { read: lines } : {}) };
    },
  },
  {
    name: "gpio_write",
    description: "Drive a header GPIO: mode high/low (output) or input, and/or pull up/down/none. Only works when the user has enabled GPIO output control on the dashboard; refused for pins used by the kernel or a peripheral.",
    inputSchema: { type: "object", properties: { gpio: { type: "integer", minimum: 2, maximum: 27 }, mode: { enum: ["high", "low", "input"] }, pull: { enum: ["up", "down", "none"] } }, required: ["gpio"] },
    handler: async (a) => writePin(Number(a.gpio), a),
  },
  {
    name: "gpio_interfaces",
    description: "Raspberry Pi: show whether I²C (and extra buses i2c3–6, i2c-gpio), SPI, UART (extra UARTs uart2–5, and the serial console) and 1-Wire are enabled, which GPIOs each uses, and any pin conflicts, both in /boot/firmware/config.txt and live. Other boards: the boot device-tree overlays (armbianEnv.txt, orangepiEnv.txt or extlinux.conf), the installed ones and the header pins each takes. Read-only; changing them is done from the dashboard (needs a reboot).",
    inputSchema: { type: "object", properties: {} },
    handler: async () => (isPi() ? interfaces() : overlayTarget() ? overlaysInfo() : { board: boardInfo(), message: noOverlays() }),
  },
];

const driver: Driver = {
  id: "gpio",
  title: "GPIO",
  order: 42,
  description: "Live GPIO header view (Raspberry Pi and mapped boards) or gpiochip lines, opt-in output control on the Pi, and the boot-time bus switches (config.txt, or device-tree overlays elsewhere)",
  async init(c) {
    ctx = c;
    pinctrlBin = Bun.which("pinctrl", { PATH });
    gpioinfoBin = Bun.which("gpioinfo", { PATH });
    refreshChips();
  },
  stop() {
    if (loop) clearTimeout(loop);
    loop = null;
    for (const ws of watchers) ws.close();
    watchers.clear();
    levels?.close();
    levels = null;
    for (const k of [...watches.keys()]) unwatch(k);
  },
  status() {
    const on = outputEnabled();
    const b = boardInfo();
    if (!isPi()) {
      const h = board().def.headers;
      const t = overlayTarget();
      return {
        active: watches.size > 0,
        summary: `${h.length ? h.map((x) => x.name).join(" + ") : `${chipSnap.list.length} GPIO chip${chipSnap.list.length === 1 ? "" : "s"}`}${watches.size ? ` · watching ${watches.size}` : ""}${b.tested ? "" : " · untested board"}${rebootRequired ? " · reboot required" : ""}`,
        mode: "gpiochip",
        board: b,
        watchers: watchers.size,
        watching: watches.size,
        rebootRequired,
        bootConfig: t ? { kind: t.kind, path: t.path, testCopy: t.testCopy } : null,
        header: h[0]?.pins || [],
      };
    }
    return {
      active: on,
      summary: `40-pin header${on ? " · output control ON" : ""}${rebootRequired ? " · reboot required" : ""}${b.tested ? "" : " · untested board"}`,
      mode: "rpi",
      board: b,
      outputEnabled: on,
      watchers: watchers.size,
      watching: watches.size,
      rebootRequired,
      configPath: CONFIG,
      testCopy: CONFIG_OVERRIDDEN,
      bootConfig: { kind: "config.txt", path: CONFIG, testCopy: CONFIG_OVERRIDDEN },
      tools: { pinctrl: !!pinctrlBin, gpioinfo: !!gpioinfoBin },
      header: G.HEADER,
    };
  },
  routes,
  sockets,
  tools,
};

export default driver;
