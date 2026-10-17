// ==================== Network diagnostics (wired) ====================
//
// The bench questions about a cable: what did it negotiate, is it taking errors, which
// switch port is it plugged into, and how fast is it really. For eth0 and any USB Ethernet
// adapter that appears:
//  - link: ethtool speed/duplex/autoneg/partner modes/EEE, error counters that are non-zero
//    or moving, driver/firmware, carrier changes, addresses, gateway, DNS
//  - neighbour: lldpd as a managed child in receive-only mode (never advertises DeskyPi),
//    also listening for CDP/FDP/EDP/SONMP, queried over a private control socket
//  - iperf3: an on-demand server (auto-off after an hour) and a client runner with live graph
//  - quick checks: gateway ping, DNS timing per nameserver, HTTP(S) reachability, path MTU
// lldpd/iperf3 system units stay masked; everything here dies with DeskyPi.

import { Resolver } from "node:dns/promises";
import type { Ctx, Driver, Props, UdevEvent } from "../core.ts";
import { HttpError, body, readText, run, usbName } from "../core.ts";
import {
  fmtBps, iperfSummary, isErrorCounter, parseEee, parseEthtool, parseEthtoolStats, parseKeyColon, parseLldpNeighbors,
  mtuSearch, parsePing, parseResolvConf, slowJson, validHost, type EthLink, type Eee, type IperfSummary, type Neighbor, type PingResult,
} from "../lib/netdiag.ts";

const PORT_OFFSET = parseInt(process.env.DESKYPI_PORT_OFFSET || "") || 0;
const INSTANCE = process.env.PORT || 80;
const LLDP_DIR = `/tmp/deskypi-lldpd-${INSTANCE}`;
const SBIN_PATH = `${process.env.PATH || "/usr/bin:/bin"}:/usr/sbin:/sbin`;
const bin = (name: string) => Bun.which(name, { PATH: SBIN_PATH }) || name;

// ---------- State ----------

interface Iface {
  name: string;
  usb: boolean;
  usbDesc: string; // "Realtek RTL8153 (0bda:8153)"
  netDriver: string;
  mac: string;
  operstate: string;
  carrier: boolean;
  carrierChanges: number;
  mtu: number;
  link: EthLink | null;
  info: Record<string, string>;
  eee: Eee | null;
  stats: Record<string, number>;
  /** Error counters worth showing: non-zero now, or moved since we started watching. */
  errors: { name: string; value: number; delta: number }[];
  baseline: Record<string, number>;
  addrs: { family: string; addr: string; prefix: number }[];
  dns: string[];
}

interface IperfTest {
  t: number;
  peer: string;
  summary: IperfSummary | null;
  error: string;
}

interface ClientRun {
  id: string;
  host: string;
  port: number;
  duration: number;
  reverse: boolean;
  udp: boolean;
  bitrate: string;
  parallel: number;
  started: number;
  status: "running" | "done" | "error";
  points: { t: number; bps: number; retransmits?: number }[];
  result: IperfSummary | null;
  error: string;
  proc?: ReturnType<Ctx["spawn"]>;
  done: Promise<void>;
}

let ctx: Ctx;
const ifaces = new Map<string, Iface>();
let gateways: { dev: string; gateway: string; metric: number }[] = [];
let resolv = { nameservers: [] as string[], search: [] as string[] };

const lldp = {
  proc: null as ReturnType<Ctx["spawn"]> | null,
  since: 0,
  neighbors: [] as Neighbor[],
  log: [] as string[],
  error: "",
  seen: new Set<string>(),
};

const server = {
  proc: null as ReturnType<Ctx["spawn"]> | null,
  port: 0,
  startedAt: 0,
  offAt: 0,
  current: null as null | { peer: string; started: number; bps: number; protocol: string; reverse: boolean },
  tests: [] as IperfTest[],
  log: [] as string[],
  offTimer: null as ReturnType<typeof setTimeout> | null,
};

let runs: ClientRun[] = [];
const checks: { ping?: PingResult & { t: number }; dns?: any; http?: any; mtu?: any } = {};
const timers: ReturnType<typeof setInterval>[] = [];

const lldpEnabled = () => ctx.settings.get("lldp", true);
const iperfPort = () => ctx.settings.get("iperfPort", 5201 + PORT_OFFSET);
const iperfAutoOffMin = () => ctx.settings.get("iperfAutoOffMin", 60);

/** validHost as a 400 rather than a 500. */
function hostOk(h: unknown): string {
  try {
    return validHost(h);
  } catch (e: any) {
    throw new HttpError(400, e.message);
  }
}

function tail(buf: string[], text: string, max = 20) {
  for (const l of text.split("\n")) if (l.trim()) buf.push(l.trim());
  if (buf.length > max) buf.splice(0, buf.length - max);
}

// ---------- Interfaces ----------

function isWired(p: Props) {
  return p.SUBSYSTEM === "net" && !!p.INTERFACE && !p.DEVPATH.includes("/virtual/") && p.DEVTYPE !== "wlan" && !/^(wl|lo$)/.test(p.INTERFACE) && !/^(bridge|bond|vlan|wwan)$/.test(p.DEVTYPE || "");
}

function rescanIfaces() {
  const found = ctx.devices(isWired);
  const names = new Set(found.map((p) => p.INTERFACE));
  for (const p of found) {
    if (ifaces.has(p.INTERFACE)) continue;
    const usbDev = ctx.ancestor(p.DEVPATH, "usb", "usb_device");
    const { vendor, model } = usbDev ? usbName(usbDev) : { vendor: "", model: "" };
    const i: Iface = {
      name: p.INTERFACE, usb: !!usbDev, usbDesc: usbDev ? `${vendor} ${model} (${usbDev.ID_VENDOR_ID}:${usbDev.ID_MODEL_ID})`.trim() : "",
      netDriver: p.ID_NET_DRIVER || "", mac: "", operstate: "", carrier: false, carrierChanges: 0, mtu: 0,
      link: null, info: {}, eee: null, stats: {}, errors: [], baseline: {}, addrs: [], dns: [],
    };
    ifaces.set(i.name, i);
    if (i.usb) ctx.event(`USB Ethernet adapter: ${i.name} — ${i.usbDesc}${i.netDriver ? `, driver ${i.netDriver}` : ""}`);
    pollFull(i).catch(() => {});
  }
  for (const [name, i] of ifaces) {
    if (names.has(name)) continue;
    ifaces.delete(name);
    if (i.usb) ctx.event(`USB Ethernet adapter removed: ${name}`);
  }
  ctx.changed();
}

/** Every 5 s, sysfs only (no processes): carrier and its change counter, operstate, MTU. */
async function pollSysfs() {
  for (const i of ifaces.values()) {
    const d = `/sys/class/net/${i.name}`;
    const [carrier, changes, oper, mtu, mac] = await Promise.all(["carrier", "carrier_changes", "operstate", "mtu", "address"].map((f) => readText(`${d}/${f}`)));
    const was = i.carrier;
    const prevOper = i.operstate;
    i.carrier = carrier === "1";
    i.carrierChanges = +changes || 0;
    i.operstate = oper;
    i.mtu = +mtu || 0;
    i.mac = mac;
    if (was !== i.carrier && i.link) {
      ctx.event(`${i.name}: link ${i.carrier ? "up" : "down"}`);
      pollFull(i).catch(() => {});
    }
    if (was !== i.carrier || oper !== prevOper) ctx.changed();
  }
}

/** Every 15 s (and on link change): ethtool's view plus addresses. */
async function pollFull(i: Iface) {
  const [l, s, e, inf] = await Promise.all([
    run([bin("ethtool"), i.name], { timeoutMs: 5000 }),
    run([bin("ethtool"), "-S", i.name], { timeoutMs: 5000 }),
    run([bin("ethtool"), "--show-eee", i.name], { timeoutMs: 5000 }),
    Object.keys(i.info).length ? null : run([bin("ethtool"), "-i", i.name], { timeoutMs: 5000 }),
  ]);
  i.link = parseEthtool(l.out);
  i.eee = parseEee(e.out);
  if (inf) i.info = parseKeyColon(inf.out);
  i.stats = parseEthtoolStats(s.out);
  i.errors = [];
  for (const [k, v] of Object.entries(i.stats)) {
    if (!isErrorCounter(k)) continue;
    if (!(k in i.baseline)) i.baseline[k] = v;
    const delta = v - i.baseline[k];
    if (v !== 0 || delta !== 0) i.errors.push({ name: k, value: v, delta });
  }
}

async function pollNet() {
  const [a, r, rc] = await Promise.all([run(["ip", "-j", "addr"]), run(["ip", "-j", "route", "show", "default"]), readText("/etc/resolv.conf")]);
  try {
    for (const x of JSON.parse(a.out || "[]")) {
      const i = ifaces.get(x.ifname);
      if (i) i.addrs = (x.addr_info || []).map((ai: any) => ({ family: ai.family, addr: ai.local, prefix: ai.prefixlen }));
    }
  } catch {}
  try {
    gateways = JSON.parse(r.out || "[]").filter((g: any) => g.gateway).map((g: any) => ({ dev: g.dev, gateway: g.gateway, metric: g.metric ?? 0 })).sort((x: any, y: any) => x.metric - y.metric);
  } catch {}
  resolv = parseResolvConf(rc);
  // Per-link DNS as NetworkManager has it (resolv.conf is the merged result).
  for (const i of ifaces.values()) {
    const n = await run([bin("nmcli"), "-t", "-f", "IP4.DNS,IP6.DNS", "dev", "show", i.name], { timeoutMs: 3000 });
    i.dns = n.code ? [] : n.out.split("\n").map((l) => l.split(":").slice(1).join(":")).filter(Boolean);
  }
}

async function pollAll() {
  await Promise.all([...ifaces.values()].map((i) => pollFull(i).catch(() => {})));
  await pollNet().catch(() => {});
  ctx.changed();
}

// ---------- LLDP ----------

async function lldpKillStale() {
  // A previous DeskyPi that died hard leaves lldpd holding our socket path: find it by that path.
  const r = await run(["pgrep", "-f", "--", `-u ${LLDP_DIR}/ctl.sock`]);
  const pids = r.out.split("\n").filter((p) => /^\d+$/.test(p));
  if (pids.length) {
    ctx.log("killing stale lldpd", pids.join(" "));
    await run(["sudo", "-n", "kill", ...pids]);
    await Bun.sleep(500);
  }
}

async function lldpStart() {
  if (lldp.proc) return;
  await lldpKillStale();
  // lldpd drops to _lldpd after creating the socket; lldpcli then needs to traverse the dir.
  await run(["mkdir", "-p", LLDP_DIR]);
  await run(["chmod", "755", LLDP_DIR]);
  lldp.log = [];
  lldp.error = "";
  lldp.neighbors = [];
  lldp.since = Date.now();
  // -r receive-only: we listen but never transmit, so the switch learns nothing about us.
  // -c -e -f -s: also decode CDP, EDP, FDP, SONMP. -O /dev/null: ignore /etc/lldpd.d.
  const p = ctx.spawn(["sudo", "-n", bin("lldpd"), "-d", "-r", "-c", "-e", "-f", "-s", "-u", `${LLDP_DIR}/ctl.sock`, "-O", "/dev/null", "-I", "eth*,en*,usb*"], { stdout: "pipe", stderr: "pipe" });
  lldp.proc = p;
  (async () => {
    const dec = new TextDecoder();
    for await (const c of p.stderr as any as ReadableStream<Uint8Array>) tail(lldp.log, dec.decode(c));
  })();
  p.exited.then((code) => {
    if (lldp.proc !== p) return;
    lldp.proc = null;
    lldp.error = `lldpd exited (${code}): ${lldp.log.slice(-2).join(" / ")}`;
    ctx.changed();
  });
  ctx.changed();
  setTimeout(() => {
    lldpQuery().catch(() => {});
    lldpWatch(p);
  }, 3000);
}

// `lldpcli watch` prints a block whenever a neighbour is added, updated or expires, so one
// long-lived child replaces polling. (Polling went through sudo every 10 s, and each sudo
// writes three journal lines.) Every block triggers one full query for the detailed view.
function lldpWatch(daemon: ReturnType<Ctx["spawn"]>) {
  if (lldp.proc !== daemon) return;
  const w = ctx.spawn(["sudo", "-n", bin("lldpcli"), "-u", `${LLDP_DIR}/ctl.sock`, "watch"], { stdout: "pipe", stderr: "ignore" });
  let debounce: ReturnType<typeof setTimeout> | null = null;
  (async () => {
    for await (const _ of w.stdout as any as ReadableStream<Uint8Array>) {
      if (debounce) clearTimeout(debounce);
      debounce = setTimeout(() => lldpQuery().catch(() => {}), 500);
    }
  })();
  daemon.exited.then(() => w.kill());
  // The watcher only dies on its own if lldpd's socket went away; retry while lldpd lives.
  w.exited.then(() => setTimeout(() => lldpWatch(daemon), 5000));
}

async function lldpStop() {
  const p = lldp.proc;
  lldp.proc = null;
  lldp.neighbors = [];
  lldp.since = 0;
  if (p) {
    p.kill();
    await Promise.race([p.exited, Bun.sleep(3000)]);
  }
  ctx.changed();
}

async function lldpQuery() {
  if (!lldp.proc) return;
  const r = await run(["sudo", "-n", bin("lldpcli"), "-u", `${LLDP_DIR}/ctl.sock`, "-f", "json", "show", "neighbors", "details"], { timeoutMs: 5000 });
  if (r.code) return;
  try {
    lldp.neighbors = parseLldpNeighbors(JSON.parse(r.out));
  } catch {
    return;
  }
  for (const n of lldp.neighbors) {
    const k = `${n.iface}|${n.chassisId}|${n.portId}`;
    if (lldp.seen.has(k)) continue;
    lldp.seen.add(k);
    ctx.event(`${n.iface} is plugged into ${n.name || n.chassisId} ${n.portDescription || n.portId} (${n.protocol})`, { chassis: n.chassisId, port: n.portId });
  }
  ctx.changed();
}

// ---------- iperf3 server ----------

/** An iperf3 server left by a DeskyPi that died hard would hold the port; ours are recognisable by their exact command line. */
async function serverKillStale() {
  const r = await run(["pgrep", "-f", "--", `^\\S*iperf3 -s -p ${iperfPort()} --json-stream --forceflush$`]);
  const pids = r.out.split("\n").filter((p) => /^\d+$/.test(p));
  if (pids.length) {
    ctx.log("killing stale iperf3 server", pids.join(" "));
    await run(["kill", ...pids]);
  }
}

function serverStart() {
  if (server.proc) return;
  server.port = iperfPort();
  server.log = [];
  server.startedAt = Date.now();
  const p = ctx.spawn([bin("iperf3"), "-s", "-p", String(server.port), "--json-stream", "--forceflush"], { stdout: "pipe", stderr: "pipe" });
  server.proc = p;
  let start: any = null;
  let peer = "";
  (async () => {
    let buf = "";
    const dec = new TextDecoder();
    for await (const c of p.stdout as any as ReadableStream<Uint8Array>) {
      buf += dec.decode(c, { stream: true });
      let nl;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (!line) continue;
        let ev: any;
        try {
          ev = JSON.parse(line);
        } catch {
          tail(server.log, line);
          continue;
        }
        const d = ev.data;
        if (ev.event === "start") {
          start = d;
          const cn = d?.connected?.[0];
          peer = cn ? `${cn.remote_host}:${cn.remote_port}` : "?";
          server.current = { peer, started: Date.now(), bps: 0, protocol: d?.test_start?.protocol || "TCP", reverse: !!d?.test_start?.reverse };
        } else if (ev.event === "interval" && server.current) {
          server.current.bps = d?.sum?.bits_per_second || 0;
        } else if (ev.event === "end") {
          pushTest({ t: Date.now(), peer, summary: iperfSummary(d, start, "server"), error: "" });
        } else if (ev.event === "error") {
          // A client that vanishes mid-test; the server carries on listening.
          if (server.current) pushTest({ t: Date.now(), peer, summary: null, error: String(d) });
          else tail(server.log, String(d));
        }
        ctx.changed();
      }
    }
  })();
  (async () => {
    const dec = new TextDecoder();
    for await (const c of p.stderr as any as ReadableStream<Uint8Array>) tail(server.log, dec.decode(c));
  })();
  p.exited.then((code) => {
    if (server.proc !== p) return;
    server.proc = null;
    server.current = null;
    tail(server.log, `iperf3 server exited (${code})`);
    ctx.changed();
  });
  // "Temporary": an iperf server left open forever is a surprise for whoever finds it.
  const mins = iperfAutoOffMin();
  if (server.offTimer) clearTimeout(server.offTimer);
  server.offAt = mins > 0 ? Date.now() + mins * 60_000 : 0;
  if (mins > 0)
    server.offTimer = setTimeout(() => {
      ctx.event(`iperf3 server stopped automatically after ${mins} min`);
      serverStop();
    }, mins * 60_000);
  ctx.event(`iperf3 server listening on port ${server.port}`);
}

function pushTest(t: IperfTest) {
  server.current = null;
  server.tests.unshift(t);
  if (server.tests.length > 20) server.tests.pop();
}

function serverStop() {
  if (server.offTimer) clearTimeout(server.offTimer);
  server.offTimer = null;
  server.offAt = 0;
  const p = server.proc;
  server.proc = null;
  server.current = null;
  p?.kill();
  ctx.changed();
}

// ---------- iperf3 client ----------

function startRun(a: any): ClientRun {
  if (runs[0]?.status === "running") throw new HttpError(409, `An iperf3 run to ${runs[0].host} is already going`);
  const host = hostOk(a.host);
  const port = a.port != null ? +a.port : 5201;
  const duration = a.duration != null ? +a.duration : 5;
  const parallel = a.parallel != null ? +a.parallel : 1;
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new HttpError(400, "port must be 1–65535");
  if (!(duration >= 1 && duration <= 60)) throw new HttpError(400, "duration must be 1–60 s");
  if (!Number.isInteger(parallel) || parallel < 1 || parallel > 16) throw new HttpError(400, "parallel must be 1–16");
  const bitrate = a.bitrate ? String(a.bitrate) : "";
  if (bitrate && !/^\d+(\.\d+)?[KMG]?$/i.test(bitrate)) throw new HttpError(400, 'bitrate looks like "100M", "1G" or "500K"');
  const udp = !!a.udp;
  const r: ClientRun = {
    id: Date.now().toString(36), host, port, duration, reverse: !!a.reverse, udp, bitrate: bitrate || (udp ? "1M" : ""), parallel,
    started: Date.now(), status: "running", points: [], result: null, error: "", done: null as any,
  };
  const cmd = [bin("iperf3"), "-c", host, "-p", String(port), "-t", String(duration), "-P", String(parallel), "--json-stream", "--forceflush", "--connect-timeout", "3000"];
  if (r.reverse) cmd.push("-R");
  if (udp) cmd.push("-u");
  if (r.bitrate) cmd.push("-b", r.bitrate);
  const p = ctx.spawn(cmd, { stdout: "pipe", stderr: "pipe" });
  r.proc = p;
  const kill = setTimeout(() => p.kill(), (duration + 20) * 1000);
  runs.unshift(r);
  runs = runs.slice(0, 20);
  r.done = (async () => {
    let start: any = null;
    let buf = "";
    const dec = new TextDecoder();
    for await (const c of p.stdout as any as ReadableStream<Uint8Array>) {
      buf += dec.decode(c, { stream: true });
      let nl;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        let ev: any;
        try {
          ev = JSON.parse(line);
        } catch {
          continue;
        }
        if (ev.event === "start") start = ev.data;
        else if (ev.event === "interval") r.points.push({ t: ev.data?.sum?.end ?? r.points.length + 1, bps: ev.data?.sum?.bits_per_second || 0, retransmits: ev.data?.sum?.retransmits });
        else if (ev.event === "end") r.result = iperfSummary(ev.data, start, "client");
        else if (ev.event === "error") r.error = String(ev.data);
        ctx.changed();
      }
    }
    const err = await new Response(p.stderr as any).text();
    await p.exited;
    clearTimeout(kill);
    r.status = r.result && !r.error ? "done" : "error";
    // iperf3 still emits an (all-zero) "end" after a connect failure; don't present it as a result.
    if (r.status === "error") r.result = null;
    if (r.status === "error" && !r.error) r.error = err.trim() || "iperf3 gave no result";
    delete r.proc;
    ctx.event(`iperf3 to ${host}: ${r.result ? `${fmtBps(r.result.bps)}${r.reverse ? " (reverse)" : ""}` : r.error}`);
    ctx.changed();
  })();
  ctx.changed();
  return r;
}

const publicRun = (r: ClientRun) => {
  const { proc, done, ...rest } = r;
  return { ...rest, bpsText: r.result ? fmtBps(r.result.bps) : "" };
};

// ---------- Quick checks ----------

function defaultGateway(): string {
  return gateways.find((g) => ifaces.has(g.dev))?.gateway || gateways[0]?.gateway || "";
}

async function ping(hostArg?: string, count = 10): Promise<PingResult & { t: number }> {
  const host = hostArg ? hostOk(hostArg) : defaultGateway();
  if (!host) throw new HttpError(409, "No default gateway to ping; pass a host");
  if (!(count >= 1 && count <= 50)) throw new HttpError(400, "count must be 1–50");
  const r = await run(["ping", "-c", String(count), "-i", "0.2", "-W", "1", host], { timeoutMs: count * 1200 + 3000 });
  const res = { ...parsePing(r.out + "\n" + r.err, host), t: Date.now() };
  if (!res.sent) throw new HttpError(400, (r.err || r.out).split("\n")[0] || "ping failed");
  checks.ping = res;
  ctx.changed();
  return res;
}

async function dnsCheck(namesArg?: string[]) {
  const names = (namesArg?.length ? namesArg : ["google.com", "cloudflare.com", "debian.org"]).slice(0, 10).map(hostOk);
  const servers = resolv.nameservers.length ? resolv.nameservers : ["127.0.0.53"];
  const results: { server: string; name: string; ms: number | null; addresses: string[]; error: string }[] = [];
  // Query each nameserver directly (c-ares), so the timing is the server's, not a cache's.
  await Promise.all(
    servers.slice(0, 4).flatMap((server) =>
      names.map(async (name) => {
        const r = new Resolver({ timeout: 2000, tries: 1 });
        r.setServers([server]);
        const t0 = performance.now();
        try {
          const addresses = await r.resolve4(name);
          results.push({ server, name, ms: Math.round((performance.now() - t0) * 10) / 10, addresses, error: "" });
        } catch (e: any) {
          results.push({ server, name, ms: null, addresses: [], error: e.code || e.message });
        }
      }),
    ),
  );
  results.sort((a, b) => a.server.localeCompare(b.server) || names.indexOf(a.name) - names.indexOf(b.name));
  checks.dns = { t: Date.now(), results };
  ctx.changed();
  return checks.dns;
}

async function httpCheck(urlsArg?: string[]) {
  const urls = (urlsArg?.length ? urlsArg : ["http://connectivitycheck.gstatic.com/generate_204", "https://www.cloudflare.com/cdn-cgi/trace", "https://deb.debian.org/debian/"]).slice(0, 6);
  const results = await Promise.all(
    urls.map(async (url) => {
      if (!/^https?:\/\//.test(url)) return { url, status: null, ms: null, error: "not an http(s) URL" };
      const t0 = performance.now();
      try {
        const r = await fetch(url, { redirect: "manual", signal: AbortSignal.timeout(6000) });
        await r.body?.cancel();
        return { url, status: r.status, ms: Math.round(performance.now() - t0), error: "" };
      } catch (e: any) {
        return { url, status: null, ms: null, error: e.name === "TimeoutError" ? "timed out after 6 s" : e.message };
      }
    }),
  );
  checks.http = { t: Date.now(), results };
  ctx.changed();
  return checks.http;
}

/** Largest packet that gets through unfragmented: binary search on `ping -M do -s`. */
async function mtuProbe(hostArg?: string) {
  const host = hostArg ? hostOk(hostArg) : "1.1.1.1";
  const probe = async (payload: number) => {
    const r = await run(["ping", "-M", "do", "-s", String(payload), "-c", "1", "-W", "1", host], { timeoutMs: 3000 });
    return parsePing(r.out + "\n" + r.err).received > 0;
  };
  const dev = gateways.find((g) => ifaces.has(g.dev))?.dev;
  const ifMtu = (dev && ifaces.get(dev)?.mtu) || 1500;
  const { mtu, steps } = await mtuSearch(probe, ifMtu);
  checks.mtu = { t: Date.now(), host, interfaceMtu: ifMtu, pathMtu: mtu, steps, note: mtu == null ? `${host} doesn't answer ping` : mtu < ifMtu ? `Something on the path to ${host} has a smaller MTU than ${dev}` : "Full-size packets get through" };
  ctx.changed();
  return checks.mtu;
}

// ---------- Presentation ----------

function publicIface(i: Iface) {
  const neighbor = lldp.neighbors.find((n) => n.iface === i.name) || null;
  return {
    name: i.name, usb: i.usb, usbDesc: i.usbDesc, mac: i.mac, operstate: i.operstate, carrier: i.carrier, carrierChanges: i.carrierChanges, mtu: i.mtu,
    driver: i.info.driver || i.netDriver, driverVersion: i.info.version || "", firmware: i.info["firmware-version"] || "", bus: i.info["bus-info"] || "",
    link: i.link, eee: i.eee, errors: i.errors, addrs: i.addrs, dns: i.dns,
    gateway: gateways.find((g) => g.dev === i.name)?.gateway || "",
    neighbor,
  };
}

function speedText(i: Iface) {
  if (!i.carrier) return `${i.name} no link`;
  const s = i.link?.speed;
  const sp = s == null ? "" : s >= 1000 ? `${s / 1000} Gb/s` : `${s} Mb/s`;
  return `${i.name} ${sp} ${(i.link?.duplex || "").toLowerCase()}`.trim();
}

// ---------- Driver ----------

const driver: Driver = {
  id: "netdiag",
  title: "Wired network",
  order: 80,
  description: "Ethernet diagnostics: link/ethtool details and errors, LLDP/CDP switch neighbour (receive-only), iperf3 server and client, ping/DNS/HTTP/MTU checks",

  async init(c) {
    ctx = c;
    rescanIfaces();
    await pollSysfs();
    pollAll();
    // Carrier is checked often (a link change triggers a full poll at once); the ethtool
    // view is otherwise near-static, so it refreshes once a minute.
    timers.push(setInterval(() => pollSysfs().catch(() => {}), 5000));
    timers.push(setInterval(() => pollAll().catch(() => {}), 60_000));
    // Safety net only: lldpWatch() picks up changes as they happen.
    timers.push(setInterval(() => lldpQuery().catch(() => {}), 300_000));
    await serverKillStale();
    if (lldpEnabled()) lldpStart().catch((e) => (lldp.error = e.message));
  },

  async stop() {
    for (const t of timers.splice(0)) clearInterval(t);
    serverStop();
    for (const r of runs) r.proc?.kill();
    await lldpStop();
    ifaces.clear();
  },

  udev(ev: UdevEvent) {
    if (ev.props.SUBSYSTEM === "net" && (ev.action === "add" || ev.action === "remove" || ev.action === "move")) rescanIfaces();
  },

  status() {
    const list = [...ifaces.values()];
    const running = runs[0]?.status === "running";
    const main = list.find((i) => gateways.some((g) => g.dev === i.name)) || list[0];
    const n = main && lldp.neighbors.find((x) => x.iface === main.name);
    const parts = [main ? speedText(main) : "no wired interface"];
    if (n) parts.push(`${n.name || n.chassisId} ${n.portDescription || n.portId}`.trim());
    if (running) parts.push(`iperf3 → ${runs[0].host}`);
    if (server.current) parts.push(`iperf3 test from ${server.current.peer}`);
    return {
      active: lldp.neighbors.length > 0 || running || !!server.current || list.some((i) => i.usb),
      summary: parts.join(" · "),
      ifaces: list.map(publicIface),
      gateways,
      dns: resolv,
      lldp: { enabled: lldpEnabled(), running: !!lldp.proc, since: lldp.since, neighbors: lldp.neighbors, error: lldp.error },
      iperf: {
        server: { running: !!server.proc, port: server.proc ? server.port : iperfPort(), startedAt: server.startedAt, offAt: server.offAt, current: server.current, tests: server.tests, log: server.proc ? [] : server.log.slice(-3) },
        runs: runs.slice(0, 10).map(publicRun),
      },
      checks,
    };
  },

  routes: [
    {
      method: "GET",
      path: "/api/netdiag",
      doc: "Everything at once: wired interfaces with link details, LLDP neighbours, iperf3 server/client state, last quick-check results",
      handler: () => driver.status!(),
    },
    {
      method: "GET",
      path: "/api/netdiag/link",
      doc: "Wired interfaces (eth0 + USB adapters): speed/duplex/autoneg, link-partner modes, EEE, non-zero error counters (with change since DeskyPi started), driver/firmware, carrier changes, IPs, gateway, DNS. ?refresh=1 re-runs ethtool first",
      async handler(_req, _p, url) {
        if (url.searchParams.get("refresh")) await pollAll();
        return { ifaces: [...ifaces.values()].map(publicIface), gateways, dns: resolv };
      },
    },
    {
      method: "GET",
      path: "/api/netdiag/link/:iface/stats",
      doc: "All ethtool -S counters for one interface",
      handler(_req, p) {
        const i = ifaces.get(p.iface);
        if (!i) throw new HttpError(404, `No wired interface "${p.iface}"`);
        return { iface: i.name, stats: i.stats };
      },
    },
    {
      method: "GET",
      path: "/api/netdiag/neighbors",
      doc: "LLDP/CDP/FDP/EDP/SONMP neighbours heard on wired ports: switch name/description, chassis id, port id/description, VLANs, PoE, management IPs, capabilities, TTL/age. Receive-only: DeskyPi never advertises itself",
      async handler() {
        await lldpQuery();
        return { enabled: lldpEnabled(), listening: !!lldp.proc, since: lldp.since, neighbors: lldp.neighbors, error: lldp.error };
      },
    },
    {
      method: "POST",
      path: "/api/netdiag/lldp",
      doc: { body: { enabled: true }, description: "Listen for switch info (receive-only lldpd). Default on; remembered" },
      async handler(req) {
        const { enabled } = await body(req);
        ctx.settings.set("lldp", !!enabled);
        if (enabled) await lldpStart();
        else await lldpStop();
        return { ok: true, enabled: !!enabled };
      },
    },
    {
      method: "POST",
      path: "/api/netdiag/iperf/server",
      doc: { body: { enabled: true, port: 5201, autoOffMin: 60 }, description: "Start/stop the iperf3 server (iperf3 -s). It turns itself off after autoOffMin minutes (0 = never). Recent client tests appear in GET /api/netdiag" },
      async handler(req) {
        const b = await body(req);
        if (b.port != null) {
          const port = +b.port;
          if (!Number.isInteger(port) || port < 1024 || port > 65535) throw new HttpError(400, "port must be 1024–65535");
          ctx.settings.set("iperfPort", port);
        }
        if (b.autoOffMin != null) {
          const m = +b.autoOffMin;
          if (!(m >= 0 && m <= 24 * 60)) throw new HttpError(400, "autoOffMin must be 0–1440");
          ctx.settings.set("iperfAutoOffMin", m);
        }
        if (b.enabled === false || (b.enabled && server.proc && server.port !== iperfPort())) serverStop();
        if (b.enabled) serverStart();
        await Bun.sleep(300);
        if (b.enabled && !server.proc) throw new HttpError(500, `iperf3 server failed: ${server.log.slice(-2).join(" / ")}`);
        return { ok: true, running: !!server.proc, port: server.port || iperfPort(), offAt: server.offAt };
      },
    },
    {
      method: "POST",
      path: "/api/netdiag/iperf",
      doc: { body: { host: "10.0.0.5", port: 5201, duration: 5, reverse: false, udp: false, bitrate: "100M", parallel: 1, wait: false }, description: "Run an iperf3 client test from DeskyPi (reverse = server sends). Returns the run id at once; progress and result appear in GET /api/netdiag/iperf/:id. wait=true answers when the test is done" },
      async handler(req) {
        const b = await body(req);
        const r = startRun(b);
        if (!b.wait) return { ok: true, run: publicRun(r) };
        return slowJson(r.done.then(() => ({ ok: r.status === "done", message: r.error || undefined, run: publicRun(r) })));
      },
    },
    {
      method: "GET",
      path: "/api/netdiag/iperf/:id",
      doc: "One iperf3 client run: per-second throughput points and the final summary",
      handler(_req, p) {
        const r = runs.find((x) => x.id === p.id);
        if (!r) throw new HttpError(404, `No run "${p.id}"`);
        return publicRun(r);
      },
    },
    {
      method: "POST",
      path: "/api/netdiag/iperf/:id/stop",
      doc: "Abort a running iperf3 client test",
      handler(_req, p) {
        const r = runs.find((x) => x.id === p.id);
        if (!r) throw new HttpError(404, `No run "${p.id}"`);
        r.proc?.kill();
        return { ok: true };
      },
    },
    {
      method: "POST",
      path: "/api/netdiag/ping",
      doc: { body: { host: "(default: the gateway)", count: 10 }, description: "Ping (0.2 s apart): loss and min/avg/max/mdev latency" },
      async handler(req) {
        const b = await body(req);
        return slowJson(ping(b.host || undefined, b.count != null ? +b.count : 10));
      },
    },
    {
      method: "POST",
      path: "/api/netdiag/dns",
      doc: { body: { names: ["google.com", "debian.org"] }, description: "Resolve names against each configured nameserver directly and time each answer" },
      async handler(req) {
        const b = await body(req);
        return slowJson(dnsCheck(Array.isArray(b.names) ? b.names : undefined));
      },
    },
    {
      method: "POST",
      path: "/api/netdiag/http",
      doc: { body: { urls: ["https://example.com/"] }, description: "HTTP(S) reachability: status code and time to first response for each URL (redirects not followed)" },
      async handler(req) {
        const b = await body(req);
        return slowJson(httpCheck(Array.isArray(b.urls) ? b.urls : undefined));
      },
    },
    {
      method: "POST",
      path: "/api/netdiag/mtu",
      doc: { body: { host: "1.1.1.1" }, description: "Path MTU probe: binary search with don't-fragment pings (takes a few seconds)" },
      async handler(req) {
        const b = await body(req);
        return slowJson(mtuProbe(b.host || undefined));
      },
    },
  ],

  tools: [
    {
      name: "net_link",
      description: "Wired Ethernet link state on DeskyPi (eth0 and any USB Ethernet adapters): negotiated speed/duplex, autonegotiation and link-partner modes, EEE, error counters that are non-zero or increasing, driver/firmware, carrier change count, IP addresses, gateway and DNS servers.",
      inputSchema: { type: "object", properties: { iface: { type: "string", description: "Only this interface, e.g. eth0" }, refresh: { type: "boolean", description: "Re-run ethtool before answering (default: data up to 15 s old)" } } },
      async handler(a) {
        if (a.refresh) await pollAll();
        const list = [...ifaces.values()].filter((i) => !a.iface || i.name === a.iface).map(publicIface);
        if (a.iface && !list.length) throw new Error(`No wired interface "${a.iface}" (have: ${[...ifaces.keys()].join(", ")})`);
        return { ifaces: list, gateways, dns: resolv };
      },
    },
    {
      name: "net_neighbors",
      description: "Which switch and port DeskyPi's wired interfaces are plugged into, learned passively from LLDP/CDP (receive-only; switches send about every 30 s, so it can be empty for a short while after startup). Includes switch name/description, chassis id, port id and description, VLANs, PoE, management IPs, capabilities.",
      inputSchema: { type: "object", properties: {} },
      async handler() {
        await lldpQuery();
        return { listening: !!lldp.proc, listeningForSeconds: lldp.since ? Math.round((Date.now() - lldp.since) / 1000) : 0, neighbors: lldp.neighbors, ...(lldpEnabled() ? {} : { note: "LLDP listening is turned off in DeskyPi's settings" }) };
      },
    },
    {
      name: "net_iperf",
      description: "Measure throughput between DeskyPi and an iperf3 server with iperf3 (TCP by default; reverse=true makes the server send; udp with a bitrate measures loss/jitter). Tests of up to 6 s return the result directly; longer ones return a runId — call again with {runId} to get the result.",
      inputSchema: {
        type: "object",
        properties: {
          host: { type: "string", description: "iperf3 server host or IP" },
          port: { type: "integer", default: 5201 },
          duration: { type: "number", default: 5, minimum: 1, maximum: 60, description: "Seconds" },
          reverse: { type: "boolean", description: "Server sends, DeskyPi receives (-R)" },
          udp: { type: "boolean" },
          bitrate: { type: "string", description: "Target rate for UDP (or a TCP cap), e.g. 100M" },
          parallel: { type: "integer", default: 1, minimum: 1, maximum: 16 },
          runId: { type: "string", description: "Fetch the result of an earlier run instead of starting one" },
        },
      },
      async handler(a) {
        if (a.runId) {
          const r = runs.find((x) => x.id === a.runId);
          if (!r) throw new Error(`No run "${a.runId}"`);
          return publicRun(r);
        }
        if (!a.host) throw new Error("host is required");
        const r = startRun(a);
        // Core MCP calls must finish inside Bun.serve's 10 s idle timeout.
        if (r.duration <= 6) {
          await Promise.race([r.done, Bun.sleep(9000)]);
          if (r.status !== "running") return publicRun(r);
        }
        return { runId: r.id, status: "running", note: `Running for ${r.duration} s; call net_iperf with {"runId":"${r.id}"} afterwards` };
      },
    },
    {
      name: "net_ping",
      description: "Ping a host from DeskyPi (default: its default gateway) and report packet loss and min/avg/max/mdev latency.",
      inputSchema: { type: "object", properties: { host: { type: "string", description: "Host or IP; default is the gateway" }, count: { type: "integer", default: 10, minimum: 1, maximum: 30 } } },
      handler: (a) => ping(a.host || undefined, Math.min(+(a.count || 10), 30)),
    },
    {
      name: "net_dns",
      description: "Resolve hostnames against each of DeskyPi's configured DNS servers directly and report the answers and per-server response time.",
      inputSchema: { type: "object", properties: { names: { type: "array", items: { type: "string" }, description: "Names to resolve (default: google.com, cloudflare.com, debian.org)" } } },
      handler: (a) => dnsCheck(Array.isArray(a.names) ? a.names : undefined),
    },
  ],
};

export default driver;
