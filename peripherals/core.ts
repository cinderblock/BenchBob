// ==================== Peripherals Core ====================
//
// Everything that isn't a drive: serial ports, CAN, I²C, 1-Wire, GPIO, Wi-Fi, cameras,
// radios… Each device class is a *driver* in peripherals/drivers/<id>.ts that default-exports
// a `Driver`. This module discovers them, feeds them udev hotplug events, and mounts their
// HTTP routes, WebSockets and MCP tools, so adding a class never touches server.ts.
//
// Contract every driver keeps:
//  - status() is called every ~2 s for the SSE stream. It must be cheap: return cached
//    state, never spawn a process. Set `active` when there is something to show (a device
//    present, a capture running); the dashboard hides idle sections.
//  - Anything long-lived (child processes, sockets, listeners) is released in stop(),
//    which runs when the driver is disabled and on shutdown.
//  - Call ctx.changed() after a state change worth showing right away; the 2 s poll
//    catches everything else.

import { Glob, type Server, type ServerWebSocket, type Subprocess } from "bun";
import { statePath } from "../paths.ts";

// ---------- Types ----------

export type Props = Record<string, string>;

export interface UdevEvent {
  action: "add" | "remove" | "change" | "bind" | "unbind" | "move" | string;
  props: Props; // DEVPATH, SUBSYSTEM, DEVNAME, DEVTYPE, ID_* …
}

export interface Ctx {
  id: string;
  /** Current udev database, optionally filtered. Kept live by the hotplug monitor. */
  devices(filter?: (p: Props) => boolean): Props[];
  /** Parent chain lookup: the nearest ancestor of `devpath` with the given subsystem (+devtype). */
  ancestor(devpath: string, subsystem: string, devtype?: string): Props | null;
  /** Push a status refresh to dashboards now (debounced). */
  changed(): void;
  /** Add a line to the hardware timeline shown by the USB panel. */
  event(text: string, detail?: Record<string, any>): void;
  /** Persisted per-driver settings (peripherals.json, outside the build hash). */
  settings: {
    get<T>(key: string, fallback: T): T;
    set(key: string, value: any): void;
  };
  /** Spawn a child that is killed on stop()/shutdown. */
  spawn(cmd: string[], opts?: Parameters<typeof Bun.spawn>[1]): Subprocess;
  log(...args: any[]): void;
}

export interface Route {
  method: "GET" | "POST" | "PUT" | "DELETE";
  path: string; // "/api/serial/:id/config" — :name segments become params
  doc?: string | Record<string, any>;
  handler(req: Request, params: Record<string, string>, url: URL): any | Promise<any>;
}

export interface WsData {
  driver: string;
  params: Record<string, string>;
  query: URLSearchParams;
  socket: Socket;
  state: Record<string, any>; // per-connection scratch space for the driver
}

export interface Socket {
  path: string; // "/ws/serial/:id"
  doc?: string;
  open?(ws: ServerWebSocket<WsData>): void;
  message?(ws: ServerWebSocket<WsData>, msg: string | Buffer): void;
  close?(ws: ServerWebSocket<WsData>): void;
}

const MCP_CONTENT = Symbol("mcpContent");

/**
 * Return this from a tool handler to send MCP content blocks as-is (images, several text
 * blocks) instead of the default single JSON text block. `structured` becomes
 * structuredContent. Example: mcpContent([{ type: "image", data: b64, mimeType: "image/jpeg" }], meta)
 */
export function mcpContent(content: Record<string, any>[], structured?: Record<string, any>) {
  return { [MCP_CONTENT]: content, structured };
}

export interface Tool {
  name: string; // unique across drivers, e.g. "serial_write"
  description: string;
  inputSchema: Record<string, any>; // JSON Schema (type: "object")
  handler(args: any): any | Promise<any>;
}

export interface Driver {
  id: string;
  title: string;
  /** Section order on the dashboard (lower first). */
  order?: number;
  /** One-line description for the driver settings list. */
  description?: string;
  init?(ctx: Ctx): void | Promise<void>;
  /** "disable": the user turned the driver off; "shutdown": the service is stopping or restarting. */
  stop?(reason: "disable" | "shutdown"): void | Promise<void>;
  /** Hotplug: called for every udev event after the device DB is updated. */
  udev?(ev: UdevEvent): void;
  status?(): any;
  routes?: Route[];
  sockets?: Socket[];
  tools?: Tool[];
}

export class HttpError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

// ---------- Helpers shared by drivers ----------

export async function run(cmd: string[], opts: { stdin?: string; timeoutMs?: number } = {}): Promise<{ out: string; err: string; code: number }> {
  const proc = Bun.spawn(cmd, { stdin: opts.stdin != null ? new Blob([opts.stdin]) : "ignore", stdout: "pipe", stderr: "pipe" });
  const timer = opts.timeoutMs ? setTimeout(() => proc.kill(), opts.timeoutMs) : null;
  const [out, err] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text()]);
  const code = await proc.exited;
  if (timer) clearTimeout(timer);
  return { out: out.trim(), err: err.trim(), code };
}

/** run() that throws with stderr on a non-zero exit. */
export async function must(cmd: string[], opts?: { stdin?: string; timeoutMs?: number }): Promise<string> {
  const r = await run(cmd, opts);
  if (r.code !== 0) throw new Error(`${cmd.join(" ")}: ${r.err || r.out || `exit ${r.code}`}`);
  return r.out;
}

export async function readText(path: string): Promise<string> {
  try {
    return (await Bun.file(path).text()).trim();
  } catch {
    return "";
  }
}

/** Parse request JSON, or {} for an empty body. */
export async function body(req: Request): Promise<any> {
  const t = await req.text();
  if (!t) return {};
  try {
    return JSON.parse(t);
  } catch {
    throw new HttpError(400, "Body is not valid JSON");
  }
}

/** udev's \x20-style escapes in ID_MODEL_ENC etc. */
export function udevDecode(s: string | undefined): string {
  return (s || "").replace(/\\x([0-9a-fA-F]{2})/g, (_, h) => String.fromCharCode(parseInt(h, 16))).trim();
}

/** Vendor and product for display: prefers the database names, falls back to the descriptor strings. */
export function usbName(p: Props): { vendor: string; model: string } {
  return {
    vendor: p.ID_VENDOR_FROM_DATABASE || udevDecode(p.ID_VENDOR_ENC) || p.ID_VENDOR || "",
    model: p.ID_MODEL_FROM_DATABASE || udevDecode(p.ID_MODEL_ENC) || p.ID_MODEL || "",
  };
}

// ---------- Settings store ----------

const SETTINGS_PATH = statePath("peripherals.json");
let settingsData: { disabled: Record<string, boolean>; drivers: Record<string, Record<string, any>> } = { disabled: {}, drivers: {} };
let saveTimer: ReturnType<typeof setTimeout> | null = null;

async function loadSettings() {
  try {
    const d = await Bun.file(SETTINGS_PATH).json();
    settingsData = { disabled: d.disabled || {}, drivers: d.drivers || {} };
  } catch {}
}

function saveSettings() {
  if (saveTimer) return;
  saveTimer = setTimeout(async () => {
    saveTimer = null;
    await Bun.write(SETTINGS_PATH, JSON.stringify(settingsData, null, 2));
  }, 200);
}

// ---------- udev device database + hotplug monitor ----------

const udevDb = new Map<string, Props>(); // DEVPATH -> props

function parseBlock(lines: string[]): Props {
  const p: Props = {};
  for (const l of lines) {
    // export-db lines look like "E: KEY=VAL"; monitor lines are bare "KEY=VAL".
    const m = /^(?:E: )?([A-Z0-9_.]+)=(.*)$/.exec(l);
    if (m) p[m[1]] = m[2];
    else if (l.startsWith("P: ") && !p.DEVPATH) p.DEVPATH = l.slice(3);
    else if (l.startsWith("N: ") && !p.DEVNAME) p.DEVNAME = "/dev/" + l.slice(3);
  }
  return p;
}

async function loadUdevDb() {
  const { out } = await run(["udevadm", "info", "--export-db"]);
  udevDb.clear();
  for (const block of out.split(/\n\s*\n/)) {
    const p = parseBlock(block.split("\n"));
    if (p.DEVPATH) udevDb.set(p.DEVPATH, p);
  }
}

let udevProc: Subprocess<"ignore", "pipe", "ignore"> | null = null;

function startUdevMonitor() {
  udevProc = Bun.spawn(["udevadm", "monitor", "--udev", "--property"], { stdout: "pipe", stderr: "ignore" });
  (async () => {
    let buf = "";
    const dec = new TextDecoder();
    for await (const chunk of udevProc!.stdout) {
      buf += dec.decode(chunk, { stream: true });
      let i;
      while ((i = buf.indexOf("\n\n")) >= 0) {
        const block = buf.slice(0, i);
        buf = buf.slice(i + 2);
        const p = parseBlock(block.split("\n"));
        if (!p.ACTION || !p.DEVPATH) continue;
        if (p.ACTION === "remove") udevDb.delete(p.DEVPATH);
        else udevDb.set(p.DEVPATH, { ...udevDb.get(p.DEVPATH), ...p });
        dispatchUdev({ action: p.ACTION, props: p });
      }
    }
    // udevadm died (shouldn't happen) — restart rather than silently losing hotplug.
    if (!shuttingDown) setTimeout(startUdevMonitor, 2000);
  })();
}

function ancestor(devpath: string, subsystem: string, devtype?: string): Props | null {
  let p = devpath;
  while (p.includes("/")) {
    p = p.slice(0, p.lastIndexOf("/"));
    const d = udevDb.get(p);
    if (d && d.SUBSYSTEM === subsystem && (!devtype || d.DEVTYPE === devtype)) return d;
  }
  return null;
}

// ---------- Timeline ----------

export interface TimelineEntry {
  t: number;
  source: string;
  text: string;
  detail?: Record<string, any>;
}

export const timeline: TimelineEntry[] = [];
const TIMELINE_MAX = 300;

function addEvent(source: string, text: string, detail?: Record<string, any>) {
  timeline.push({ t: Date.now(), source, text, detail });
  if (timeline.length > TIMELINE_MAX) timeline.splice(0, timeline.length - TIMELINE_MAX);
}

// ---------- Driver registry ----------

interface Loaded {
  driver: Driver;
  ctx: Ctx;
  started: boolean;
  children: Set<Subprocess>;
}

const loaded = new Map<string, Loaded>();
let changedCb: () => void = () => {};
let changedTimer: ReturnType<typeof setTimeout> | null = null;
let shuttingDown = false;

function changed() {
  if (changedTimer) return;
  changedTimer = setTimeout(() => {
    changedTimer = null;
    changedCb();
  }, 150);
}

function makeCtx(id: string, children: Set<Subprocess>): Ctx {
  return {
    id,
    devices: (filter) => [...udevDb.values()].filter((p) => !filter || filter(p)),
    ancestor,
    changed,
    event: (text, detail) => {
      addEvent(id, text, detail);
      changed();
    },
    settings: {
      get(key, fallback) {
        const v = settingsData.drivers[id]?.[key];
        return v === undefined ? fallback : v;
      },
      set(key, value) {
        (settingsData.drivers[id] ||= {})[key] = value;
        saveSettings();
      },
    },
    spawn(cmd, opts) {
      const proc = Bun.spawn(cmd, opts as any);
      children.add(proc);
      proc.exited.then(() => children.delete(proc));
      return proc;
    },
    log: (...args) => console.log(`[${id}]`, ...args),
  };
}

function isDisabled(id: string) {
  return !!settingsData.disabled[id];
}

async function startDriver(l: Loaded) {
  if (l.started || isDisabled(l.driver.id)) return;
  l.started = true;
  try {
    await l.driver.init?.(l.ctx);
  } catch (e: any) {
    console.error(`[${l.driver.id}] init failed:`, e);
    addEvent(l.driver.id, `Driver failed to start: ${e.message}`);
  }
}

async function stopDriver(l: Loaded, reason: "disable" | "shutdown") {
  if (!l.started) return;
  l.started = false;
  try {
    await l.driver.stop?.(reason);
  } catch (e) {
    console.error(`[${l.driver.id}] stop failed:`, e);
  }
  for (const c of l.children) c.kill();
}

function dispatchUdev(ev: UdevEvent) {
  for (const l of loaded.values()) {
    if (!l.started || !l.driver.udev) continue;
    try {
      l.driver.udev(ev);
    } catch (e) {
      console.error(`[${l.driver.id}] udev handler:`, e);
    }
  }
}

export async function start(onChanged: () => void) {
  changedCb = onChanged;
  await loadSettings();
  await loadUdevDb();
  startUdevMonitor();

  // Tests and _helpers live alongside drivers but aren't drivers; shared code goes in peripherals/lib/.
  const dir = import.meta.dir + "/drivers";
  const files = [...new Glob("*.ts").scanSync(dir)].filter((f) => !f.endsWith(".test.ts") && !f.startsWith("_")).sort();
  for (const f of files) {
    try {
      const mod = await import(`${dir}/${f}`);
      const driver: Driver = mod.default;
      if (!driver?.id) continue;
      if (loaded.has(driver.id)) throw new Error(`duplicate driver id ${driver.id}`);
      const children = new Set<Subprocess>();
      loaded.set(driver.id, { driver, ctx: makeCtx(driver.id, children), started: false, children });
    } catch (e) {
      console.error(`[peripherals] failed to load ${f}:`, e);
    }
  }
  // Start concurrently: one slow init (a radio scan, say) mustn't hold up the rest.
  await Promise.all([...loaded.values()].map(startDriver));

  const shutdown = async () => {
    if (shuttingDown) return;
    shuttingDown = true;
    udevProc?.kill();
    await Promise.all([...loaded.values()].map((l) => stopDriver(l, "shutdown")));
    process.exit(0);
  };
  process.on("SIGTERM", shutdown);
  process.on("SIGINT", shutdown);
}

// ---------- Status ----------

export function status() {
  const list = [];
  const state: Record<string, any> = {};
  for (const { driver, started } of [...loaded.values()].sort((a, b) => (a.driver.order ?? 100) - (b.driver.order ?? 100))) {
    const disabled = isDisabled(driver.id);
    let st: any = null;
    if (started) {
      try {
        st = driver.status?.() ?? null;
      } catch (e: any) {
        st = { error: e.message };
      }
    }
    list.push({ id: driver.id, title: driver.title, description: driver.description || "", disabled, active: !!st?.active, summary: st?.summary || "" });
    if (st) state[driver.id] = st;
  }
  return { list, state };
}

// ---------- HTTP + WebSocket routing ----------

function matchPath(pattern: string, path: string): Record<string, string> | null {
  const a = pattern.split("/");
  const b = path.split("/");
  if (a.length !== b.length) return null;
  const params: Record<string, string> = {};
  for (let i = 0; i < a.length; i++) {
    if (a[i].startsWith(":")) params[a[i].slice(1)] = decodeURIComponent(b[i]);
    else if (a[i] !== b[i]) return null;
  }
  return params;
}

function errorResponse(e: any) {
  const status = e instanceof HttpError ? e.status : 500;
  if (status === 500) console.error(e);
  return Response.json({ ok: false, message: e?.message || String(e) }, { status });
}

/** Handles /api/periph/*, driver routes, /ws/* upgrades and /mcp. Returns null when the request isn't ours. */
export async function route(req: Request, url: URL, server: Server): Promise<Response | "upgraded" | null> {
  const p = url.pathname;

  if (p === "/mcp") return mcp(req);

  if (p === "/api/periph/driver" && req.method === "POST") {
    try {
      const { id, disabled } = await body(req);
      const l = loaded.get(id);
      if (!l) throw new HttpError(404, `No driver "${id}"`);
      if (disabled) settingsData.disabled[id] = true;
      else delete settingsData.disabled[id];
      saveSettings();
      await (disabled ? stopDriver(l, "disable") : startDriver(l));
      changed();
      return Response.json({ ok: true });
    } catch (e) {
      return errorResponse(e);
    }
  }
  if (p === "/api/periph/timeline") return Response.json(timeline);

  if (p.startsWith("/ws/")) {
    for (const l of loaded.values()) {
      if (!l.started) continue;
      for (const s of l.driver.sockets || []) {
        const params = matchPath(s.path, p);
        if (!params) continue;
        const data: WsData = { driver: l.driver.id, params, query: url.searchParams, socket: s, state: {} };
        return server.upgrade(req, { data }) ? "upgraded" : new Response("WebSocket upgrade failed", { status: 400 });
      }
    }
    return new Response("Not Found", { status: 404 });
  }

  // The most literal match wins (fewest :params), so "/api/gpio/reboot" isn't swallowed by
  // "/api/gpio/:pin" whatever order a driver happened to list them in.
  let best: { l: Loaded; r: Route; params: Record<string, string>; n: number } | null = null;
  for (const l of loaded.values()) {
    for (const r of l.driver.routes || []) {
      if (r.method !== req.method) continue;
      const params = matchPath(r.path, p);
      if (!params) continue;
      const n = Object.keys(params).length;
      if (!best || n < best.n) best = { l, r, params, n };
    }
  }
  if (!best) return null;
  if (!best.l.started) return Response.json({ ok: false, message: `${best.l.driver.title} is disabled` }, { status: 409 });
  try {
    const res = await best.r.handler(req, best.params, url);
    return res instanceof Response ? res : Response.json(res ?? { ok: true });
  } catch (e) {
    return errorResponse(e);
  }
}

export const websocket = {
  open(ws: ServerWebSocket<WsData>) {
    ws.data.socket.open?.(ws);
  },
  message(ws: ServerWebSocket<WsData>, msg: string | Buffer) {
    try {
      ws.data.socket.message?.(ws, msg);
    } catch (e: any) {
      ws.send(JSON.stringify({ type: "error", message: e.message }));
    }
  },
  close(ws: ServerWebSocket<WsData>) {
    ws.data.socket.close?.(ws);
  },
};

/** Endpoint docs contributed by drivers, merged into GET /api. */
export function docs(): Record<string, any> {
  const out: Record<string, any> = {
    "POST /api/periph/driver": { body: { id: "gpio", disabled: true }, description: "Enable or disable a peripheral driver (disabled drivers release their devices and disappear from the dashboard)" },
    "GET /api/periph/timeline": "Hardware event timeline: USB plug/unplug, kernel USB messages, driver events",
    "POST /mcp": "Model Context Protocol endpoint (Streamable HTTP, stateless JSON). Every peripheral operation below is also an MCP tool.",
  };
  for (const { driver } of loaded.values()) {
    for (const r of driver.routes || []) if (r.doc) out[`${r.method} ${r.path}`] = r.doc;
    for (const s of driver.sockets || []) if (s.doc) out[`WS ${s.path}`] = s.doc;
  }
  return out;
}

// ---------- MCP (Streamable HTTP, stateless) ----------
//
// Enough of the protocol for agents to discover and call tools: initialize, ping,
// tools/list, tools/call. No sessions or server-initiated messages, so every request is
// answered with plain JSON and GET (the SSE side channel) is refused, as the spec allows.

const MCP_PROTOCOL = "2025-06-18";

function allTools(): { tool: Tool; l: Loaded }[] {
  const out: { tool: Tool; l: Loaded }[] = [];
  for (const l of loaded.values()) if (l.started) for (const tool of l.driver.tools || []) out.push({ tool, l });
  return out;
}

async function mcpOne(msg: any): Promise<any | null> {
  const { id, method, params } = msg || {};
  const reply = (result: any) => ({ jsonrpc: "2.0", id, result });
  const fail = (code: number, message: string) => ({ jsonrpc: "2.0", id, error: { code, message } });
  if (id === undefined) return null; // notification (e.g. notifications/initialized)
  switch (method) {
    case "initialize":
      return reply({
        protocolVersion: params?.protocolVersion || MCP_PROTOCOL,
        capabilities: { tools: { listChanged: false } },
        serverInfo: { name: "benchbob", version: "1.0" },
        instructions:
          "BenchBob is a bench server, usually a Raspberry Pi. These tools reach the peripherals plugged into it (serial ports, CAN, I²C, 1-Wire, GPIO, Wi-Fi, USB, radios). Call a *_list tool first to find device ids.",
      });
    case "ping":
      return reply({});
    case "tools/list":
      return reply({ tools: allTools().map(({ tool }) => ({ name: tool.name, description: tool.description, inputSchema: tool.inputSchema })) });
    case "tools/call": {
      const hit = allTools().find(({ tool }) => tool.name === params?.name);
      if (!hit) return fail(-32602, `Unknown tool: ${params?.name}`);
      try {
        const result = await hit.tool.handler(params?.arguments || {});
        if (result && typeof result === "object" && MCP_CONTENT in result)
          return reply({ content: result[MCP_CONTENT], ...(result.structured ? { structuredContent: result.structured } : {}) });
        const text = typeof result === "string" ? result : JSON.stringify(result, null, 2);
        return reply({ content: [{ type: "text", text }], ...(typeof result === "object" && result !== null && !Array.isArray(result) ? { structuredContent: result } : {}) });
      } catch (e: any) {
        return reply({ content: [{ type: "text", text: e?.message || String(e) }], isError: true });
      }
    }
    default:
      return fail(-32601, `Method not found: ${method}`);
  }
}

async function mcp(req: Request): Promise<Response> {
  if (req.method === "GET") return new Response("This MCP server is stateless; use POST", { status: 405, headers: { Allow: "POST" } });
  if (req.method === "DELETE") return new Response(null, { status: 405 });
  if (req.method !== "POST") return new Response("Method Not Allowed", { status: 405 });
  let msg: any;
  try {
    msg = await req.json();
  } catch {
    return Response.json({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "Parse error" } }, { status: 400 });
  }
  const headers = { "MCP-Protocol-Version": MCP_PROTOCOL };
  if (Array.isArray(msg)) {
    const out = (await Promise.all(msg.map(mcpOne))).filter(Boolean);
    return out.length ? Response.json(out, { headers }) : new Response(null, { status: 202, headers });
  }
  const out = await mcpOne(msg);
  return out ? Response.json(out, { headers }) : new Response(null, { status: 202, headers });
}

// ---------- Build files ----------

/** Peripheral sources that define the running app, for the build hash. */
export function buildFiles(): string[] {
  const root = import.meta.dir + "/..";
  return [
    ...[...new Glob("peripherals/**/*.ts").scanSync(root)].filter((f) => !f.endsWith(".test.ts")).sort().map((f) => `${root}/${f}`),
    ...uiFiles(),
  ];
}

/** Per-driver dashboard scripts, inlined into the page at serve time. */
export function uiFiles(): string[] {
  const root = import.meta.dir + "/..";
  return [...new Glob("ui/*.js").scanSync(root)].sort().map((f) => `${root}/${f}`);
}
