// A pretend microcontroller node for developing the node sections and the MCU build without
// hardware: the firmware's API shapes (firmware/proto/src/api.rs), in memory, with CORS.
//
//   bun scripts/mock-node.ts [port=8099]
//   then open the dev server with ?bob=http://localhost:8099
//
// It also serves dist-mcu/ at / when built, as a node will serve the UI from flash.
import { join } from "node:path";

const port = Number(process.argv[2] || 8099);
const t0 = Date.now();
let outputEnabled = false;
const pins = [
  ...Array.from({ length: 23 }, (_, g) => g),
  26,
  27,
  28,
].map((gpio) => ({ gpio, header: gpio < 23 ? gpio + 1 : gpio - 5, mode: "input", level: gpio % 3 === 0 ? 1 : 0, pull: "none", func: gpio === 4 || gpio === 5 ? "I2C0" : null as string | null }));
let scan: { at: number | null; ms: number; found: number[] } = { at: null, ms: 0, found: [] };
const uptimeMs = () => Date.now() - t0;
const cors = { "Access-Control-Allow-Origin": "*", "Access-Control-Allow-Methods": "GET, POST, OPTIONS", "Access-Control-Allow-Headers": "Content-Type" };
const json = (v: unknown, status = 200) => Response.json(v, { status, headers: cors });
const err = (status: number, message: string) => json({ ok: false, message }, status);
const pinView = (p: (typeof pins)[number]) =>
  p.func
    ? { gpio: p.gpio, header: p.header, name: `GPIO${p.gpio}`, func: p.func, level: null, pull: null, refusal: `used by ${p.func}`, note: null }
    : { gpio: p.gpio, header: p.header, name: `GPIO${p.gpio}`, func: p.mode, level: p.level, pull: p.pull, refusal: null, note: p.gpio === 25 ? "LED" : null };
const bus = () => ({
  bus: 0, dev: "i2c-0", label: "I2C0", sda: 4, scl: 5, hz: 100000, quickWrite: false,
  lastScanUptimeMs: scan.at, scanMs: scan.ms,
  devices: scan.found.map((a) => ({ addr: a, hex: "0x" + a.toString(16).padStart(2, "0"), address: "0x" + a.toString(16).padStart(2, "0"), chip: null })),
});
const ENDPOINTS = ["GET /api", "GET /api/status", "GET /api/gpio", "POST /api/gpio/output-control", "POST /api/gpio/:gpio", "GET /api/i2c", "POST /api/i2c/:bus/scan", "POST /api/i2c/:bus/:addr/raw/read", "POST /mcp"];

Bun.serve({
  port,
  async fetch(req) {
    const u = new URL(req.url);
    const p = u.pathname.replace(/\/+$/, "") || "/";
    if (req.method === "OPTIONS") return new Response(null, { status: 204, headers: cors });
    const body = req.method === "POST" ? await req.json().catch(() => ({})) : {};
    if (p === "/" || p.startsWith("/assets/")) {
      const f = Bun.file(join(import.meta.dir, "..", "dist-mcu", p === "/" ? "index.html" : p));
      if (await f.exists()) return new Response(f, { headers: cors });
    }
    if (p === "/" || p === "/api") return json({ name: "BenchBob node API", version: "1.0", appVersion: "0.1.0-mock", board: "pico2w", boardName: "Raspberry Pi Pico 2 W", chip: "RP2350", base: u.origin, endpoints: Object.fromEntries(ENDPOINTS.map((e) => [e, ""])), mcpTools: ["gpio_read", "gpio_write", "i2c_buses", "i2c_scan", "i2c_read"] });
    if (p === "/api/status")
      return json({ hostname: "benchbob-0a1b2c", uptime: uptimeMs() / 1000, version: "0.1.0-mock", board: "pico2w", boardName: "Raspberry Pi Pico 2 W", chip: "RP2350", network: { mac: "28:cd:c1:0a:1b:2c", wifi: "connected", ssid: "Bench Lab", rssi: -55 - Math.round(Math.random() * 10), ip: "10.0.0.42" }, heap: null });
    if (p === "/api/gpio" && req.method === "GET") return json({ outputEnabled, board: "pico2w", pins: pins.map(pinView) });
    if (p === "/api/gpio/output-control") {
      if (body.enabled && body.confirm !== true) return err(400, 'Enabling output control lets this API drive pins; repeat with "confirm": true');
      outputEnabled = !!body.enabled;
      return json({ outputEnabled });
    }
    const g = p.match(/^\/api\/gpio\/(\d+)$/);
    if (g && req.method === "POST") {
      const pin = pins.find((x) => x.gpio === Number(g[1]));
      if (!pin) return err(404, `No GPIO ${g[1]} on this board (see GET /api/gpio)`);
      if (!outputEnabled) return err(403, "GPIO output control is off. Enable it first (POST /api/gpio/output-control with enabled: true, confirm: true).");
      if (pin.func) return err(409, `GPIO${pin.gpio}: used by ${pin.func}`);
      if (body.mode) Object.assign(pin, body.mode === "input" ? { mode: "input" } : { mode: "output", level: body.mode === "high" ? 1 : 0 });
      if (body.pull) pin.pull = body.pull;
      return json(pinView(pin));
    }
    if (p === "/api/i2c") return json({ buses: [bus()] });
    if (p === "/api/i2c/0/scan" || p === "/api/i2c/i2c-0/scan") {
      scan = { at: uptimeMs(), ms: 41, found: [0x3c, 0x76] };
      return json(bus());
    }
    const r = p.match(/^\/api\/i2c\/(?:i2c-)?0\/(0x[0-9a-f]+|\d+)\/raw\/read$/i);
    if (r) {
      const addr = Number(r[1]);
      if (!scan.found.includes(addr)) return err(502, `i2c-0 0x${addr.toString(16)}: no ACK`);
      const n = body.size === "word" ? 2 : body.size === "byte" || !body.size ? 1 : Number(body.length) || 1;
      const bytes = Array.from({ length: n }, (_, i) => (addr + i * 7) & 0xff);
      return json({ ...(body.size !== "raw" ? { register: Number(body.register) || 0 } : {}), ...(n === 2 && body.size === "word" ? { word: bytes[0] | (bytes[1] << 8), wordBigEndian: (bytes[0] << 8) | bytes[1] } : {}), bytes });
    }
    return err(404, `Not found: ${req.method} ${u.pathname}`);
  },
});
console.log(`mock node on http://localhost:${port}`);
