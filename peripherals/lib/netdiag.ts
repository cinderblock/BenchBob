// Pure helpers for the netdiag driver: parsers for ethtool, lldpcli JSON, ping and iperf3
// output, plus a keepalive JSON response for slow checks. No I/O except slowJson's stream.

// ---------- ethtool ----------

export interface EthLink {
  speed: number | null; // Mb/s
  duplex: string;
  autoneg: boolean | null;
  link: boolean | null;
  port: string;
  supported: string[];
  advertised: string[];
  partner: string[]; // link partner advertised modes
  partnerAutoneg: boolean | null;
  partnerPause: string;
  masterSlave: string;
  mdix: string;
}

/**
 * `ethtool IFACE` text. Multi-line mode lists continue on lines with no "Key:" prefix, so
 * we keep appending to the last key until the next "Key: value" line.
 */
export function parseEthtool(text: string): EthLink {
  const kv: Record<string, string> = {};
  let last = "";
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("Settings for") || line.startsWith("netlink error")) continue;
    const m = /^([A-Za-z][\w /-]*?):\s*(.*)$/.exec(line);
    if (m && !/^\d/.test(line)) {
      last = m[1];
      kv[last] = m[2];
    } else if (last) kv[last] += " " + line;
  }
  const modes = (s?: string) => (s || "").split(/\s+/).filter((x) => /^\d+base/.test(x));
  const yn = (s?: string) => (s == null ? null : /^(yes|on)$/i.test(s.trim()));
  const sp = /^(\d+)Mb\/s/.exec(kv["Speed"] || "");
  return {
    speed: sp ? +sp[1] : null,
    duplex: /unknown/i.test(kv["Duplex"] || "") ? "" : kv["Duplex"] || "",
    autoneg: yn(kv["Auto-negotiation"]),
    link: yn(kv["Link detected"]),
    port: kv["Port"] || "",
    supported: modes(kv["Supported link modes"]),
    advertised: modes(kv["Advertised link modes"]),
    partner: modes(kv["Link partner advertised link modes"]),
    partnerAutoneg: yn(kv["Link partner advertised auto-negotiation"]),
    partnerPause: kv["Link partner advertised pause frame use"] || "",
    masterSlave: kv["master-slave status"] || "",
    mdix: kv["MDI-X"] || "",
  };
}

/** `ethtool -i`: driver, version, firmware-version, bus-info… */
export function parseKeyColon(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const l of text.split("\n")) {
    const m = /^([\w-]+):\s*(.*)$/.exec(l.trim());
    if (m) out[m[1]] = m[2].trim();
  }
  return out;
}

/** `ethtool -S`: counters. Some drivers repeat a name (bcmgenet has rx_bytes twice); the first wins. */
export function parseEthtoolStats(text: string): Record<string, number> {
  const out: Record<string, number> = {};
  for (const l of text.split("\n")) {
    const m = /^\s+([\w.\-[\]]+):\s*(-?\d+)\s*$/.exec(l);
    if (m && !(m[1] in out)) out[m[1]] = +m[2];
  }
  return out;
}

/** Counters that mean something went wrong (as opposed to traffic, size histograms, per-queue totals). */
export function isErrorCounter(name: string): boolean {
  if (/^(rx|tx)q\d+_(bytes|packets|multicast|broadcast)$/.test(name)) return false;
  // bcmgenet's MAC-destination-filter count: frames addressed to other hosts that the
  // switch flooded to us. It climbs all day on a busy LAN and means nothing is wrong.
  if (name === "mdf_err_cnt") return false;
  return /err|drop|crc|fcs|miss|overrun|over_|oversize|undersize|runt|jabber|align|frag|collision|_col$|late_col|excess|defer|discard|fail|ovflow|carrier|abort|timeout|symbol|code$|outrange|lost|no_buf|nobuf|fifo/i.test(name);
}

export interface Eee {
  supported: boolean;
  enabled: boolean;
  active: boolean;
  txLpiUs: number | null;
  advertised: string[];
  partner: string[];
}

export function parseEee(text: string): Eee | null {
  if (!/EEE status:/i.test(text)) return null;
  const st = /EEE status:\s*(.*)/i.exec(text)![1];
  const block = (k: string) => {
    const m = new RegExp(`${k}:\\s*([^\\n]*(?:\\n\\s+\\d+base[^\\n]*)*)`, "i").exec(text);
    return (m?.[1] || "").split(/\s+/).filter((x) => /^\d+base/.test(x));
  };
  const lpi = /Tx LPI:\s*(\d+)/i.exec(text);
  return {
    supported: !/not supported/i.test(st),
    enabled: /^enabled/i.test(st),
    active: /- active/i.test(st),
    txLpiUs: lpi ? +lpi[1] : null,
    advertised: block("Advertised EEE link modes"),
    partner: block("Link partner advertised EEE link modes"),
  };
}

// ---------- LLDP ----------

export interface Neighbor {
  iface: string;
  protocol: string; // LLDP, CDPv2, FDP…
  age: string;
  name: string;
  description: string;
  chassisId: string;
  chassisIdType: string;
  mgmtIps: string[];
  capabilities: { type: string; enabled: boolean }[];
  portId: string;
  portIdType: string;
  portDescription: string;
  ttl: number | null;
  vlans: { id: number; pvid: boolean; name: string }[];
  mtu: number | null;
  poe: Record<string, string> | null;
  medDeviceType: string;
}

const arr = <T>(x: T | T[] | undefined | null): T[] => (x == null ? [] : Array.isArray(x) ? x : [x]);

/** Entries of a lldpcli "keyed object": `{ "eth0": {...} }` or `[{ "eth0": {...} }, …]`. */
function entries(x: any): [string, any][] {
  const out: [string, any][] = [];
  for (const o of arr(x)) if (o && typeof o === "object") for (const [k, v] of Object.entries(o)) out.push([k, v]);
  return out;
}

/** `lldpcli -f json show neighbors details` (lldpd 1.0.x JSON). */
export function parseLldpNeighbors(j: any): Neighbor[] {
  const out: Neighbor[] = [];
  for (const [iface, n] of entries(j?.lldp?.interface)) {
    if (!n || typeof n !== "object") continue;
    // The chassis is keyed by system name when there is one, else it's the bare object.
    let name = "";
    let ch: any = n.chassis || {};
    if (!ch.id) {
      const e = entries(ch)[0];
      if (e) [name, ch] = e;
    }
    const port = n.port || {};
    const ttl = port.ttl ?? n.ttl?.ttl;
    const power = port.power || null;
    const medPoe = n["lldp-med"]?.poe || null;
    let poe: Record<string, string> | null = null;
    if (power || medPoe) {
      poe = {};
      for (const [k, v] of Object.entries({ ...(medPoe || {}), ...(power || {}) })) if (v != null && typeof v !== "object") poe[k] = String(v);
    }
    out.push({
      iface,
      protocol: n.via || "LLDP",
      age: n.age || "",
      name,
      description: arr(ch.descr).join(" "),
      chassisId: ch.id?.value || "",
      chassisIdType: ch.id?.type || "",
      mgmtIps: arr(ch["mgmt-ip"]).map(String),
      capabilities: arr(ch.capability).map((c: any) => ({ type: c.type, enabled: !!c.enabled })),
      portId: port.id?.value || "",
      portIdType: port.id?.type || "",
      portDescription: port.descr || "",
      ttl: ttl != null ? +ttl : null,
      vlans: arr(n.vlan).map((v: any) => ({ id: +v["vlan-id"], pvid: !!v.pvid, name: v.value || "" })).filter((v) => Number.isFinite(v.id)),
      mtu: port.mfs != null ? +port.mfs : null,
      poe,
      medDeviceType: n["lldp-med"]?.["device-type"] || "",
    });
  }
  return out;
}

// ---------- ping ----------

export interface PingResult {
  host: string;
  sent: number;
  received: number;
  lossPct: number;
  min: number | null;
  avg: number | null;
  max: number | null;
  mdev: number | null;
  errors: number;
  times: number[];
  tooBig: boolean; // "Message too long": the local interface MTU already refuses it
}

export function parsePing(text: string, host = ""): PingResult {
  const st = /(\d+) packets transmitted, (\d+) received(?:, \+(\d+) errors)?(?:, [^,]*)?, ([\d.]+)% packet loss/.exec(text);
  const rtt = /= ([\d.]+)\/([\d.]+)\/([\d.]+)\/([\d.]+) ms/.exec(text);
  const times = [...text.matchAll(/time=([\d.]+) ms/g)].map((m) => +m[1]);
  return {
    host,
    sent: st ? +st[1] : 0,
    received: st ? +st[2] : 0,
    lossPct: st ? +st[4] : 100,
    min: rtt ? +rtt[1] : null,
    avg: rtt ? +rtt[2] : null,
    max: rtt ? +rtt[3] : null,
    mdev: rtt ? +rtt[4] : null,
    errors: st?.[3] ? +st[3] : 0,
    times,
    tooBig: /message too long|frag needed/i.test(text),
  };
}

/**
 * Path MTU by binary search on the ICMP payload size (MTU = payload + 28 bytes of IPv4 and
 * ICMP headers). Tries a full-size packet first, since that's the common answer, and an
 * empty one second so a host that ignores ping isn't reported as MTU 28.
 */
export async function mtuSearch(probe: (payload: number) => Promise<boolean>, ifMtu: number): Promise<{ mtu: number | null; steps: { size: number; ok: boolean }[] }> {
  const steps: { size: number; ok: boolean }[] = [];
  const t = async (payload: number) => {
    const ok = await probe(payload);
    steps.push({ size: payload + 28, ok });
    return ok;
  };
  let lo = 0;
  let hi = ifMtu - 28;
  if (await t(hi)) return { mtu: ifMtu, steps };
  if (!(await t(0))) return { mtu: null, steps };
  while (hi - lo > 1) {
    const mid = (lo + hi) >> 1;
    if (await t(mid)) lo = mid;
    else hi = mid;
  }
  return { mtu: lo + 28, steps };
}

// ---------- iperf3 ----------

export interface IperfSummary {
  protocol: string;
  reverse: boolean;
  seconds: number;
  bytes: number;
  bps: number;
  retransmits: number | null;
  jitterMs: number | null;
  lostPct: number | null;
}

/**
 * Throughput from an iperf3 "end" event, as measured by the receiver.
 * Client: sum_received is always the receiving side's count (iperf3 exchanges results).
 * Server: it only knows its own side, so it's sum_received normally and sum_sent under -R.
 * UDP has a single "sum" with jitter and loss.
 */
export function iperfSummary(end: any, start: any, role: "client" | "server"): IperfSummary {
  const ts = start?.test_start || {};
  const protocol = ts.protocol || (end?.sum?.jitter_ms != null ? "UDP" : "TCP");
  const reverse = !!ts.reverse;
  if (protocol === "UDP") {
    const s = end?.sum || {};
    return { protocol, reverse, seconds: s.seconds || 0, bytes: s.bytes || 0, bps: s.bits_per_second || 0, retransmits: null, jitterMs: s.jitter_ms ?? null, lostPct: s.lost_percent ?? null };
  }
  const rcv = role === "server" && reverse ? end?.sum_sent : end?.sum_received;
  const snd = end?.sum_sent || {};
  return { protocol, reverse, seconds: rcv?.seconds || 0, bytes: rcv?.bytes || 0, bps: rcv?.bits_per_second || 0, retransmits: snd.retransmits ?? null, jitterMs: null, lostPct: null };
}

/**
 * iperf3's client error, said plainly. Its connect failures all read "unable to connect to
 * server - server may have stopped running or use a different port, firewall issue, etc.:
 * <errno text>" (3.18); the errno text is the useful part.
 */
export function iperfError(raw: string, host: string, port: number): string {
  const s = String(raw || "").trim();
  const why = s.replace(/^unable to connect to server[^:]*:\s*/i, "");
  const at = `${host}:${port}`;
  if (/connection refused/i.test(why)) return `connection refused: no iperf3 server listening on ${at}`;
  if (/timed out/i.test(why)) return `no answer from ${at} (timed out): host down, or a firewall drops the port`;
  if (/no route|unreachable/i.test(why)) return `can't reach ${host} (${why})`;
  if (/name or service|not known|getaddrinfo|resolve|invalid argument/i.test(why) && why !== s) return `couldn't connect to ${at} (${why}): check the host name`;
  if (/busy/i.test(s)) return `the iperf3 server on ${at} is busy with another test; try again shortly`;
  if (/authoriz|authenticat/i.test(s)) return `${at} refused the login: check the username, password and the server's public key (Logins…); a server on iperf3 3.16 or older also needs "older server" ticked`;
  return why || s || "iperf3 gave no result";
}

// ---------- iperf3 logins ----------
// iperf3 ≥3.10 servers started with --rsa-private-key-path + --authorized-users-path only
// run tests for listed users. The client encrypts "username + password" with the server's
// RSA public key; the server's users file holds `username,sha256("{username}password")`.

export const validIperfUser = (u: string) => /^[A-Za-z0-9_.-]{1,32}$/.test(u);

/** The line for the server's --authorized-users-path file. Safe to publish: only the hash. */
export function iperfAuthLine(username: string, password: string): string {
  return `${username},${new Bun.CryptoHasher("sha256").update(`{${username}}${password}`).digest("hex")}`;
}

/** An RSA public key as `openssl rsa -pubout` writes it, normalised to LF + final newline. */
export function iperfPublicKey(pem: string): string {
  const t = String(pem || "").replace(/\r\n/g, "\n").trim();
  if (!/^-----BEGIN PUBLIC KEY-----\n[A-Za-z0-9+/=\n]+\n-----END PUBLIC KEY-----$/.test(t)) throw new Error("publicKey must be a PEM public key (-----BEGIN PUBLIC KEY----- … -----END PUBLIC KEY-----)");
  return t + "\n";
}

// ---------- presets ----------

export interface Preset {
  host: string; // "gateway" (ping only) = the default gateway
  label: string;
}

export const DEFAULT_IPERF_PRESETS: Preset[] = [
  { host: "steamboat.tsl", label: "steamboat · LAN" },
  { host: "firefly.isozilla.com", label: "firefly · DO SF" },
  { host: "beelzebub.isozilla.com", label: "beelzebub · Hetzner DE" },
];

export const DEFAULT_PING_TARGETS: Preset[] = [
  { host: "gateway", label: "Gateway" },
  { host: "1.1.1.1", label: "1.1.1.1" },
  { host: "8.8.8.8", label: "8.8.8.8" },
  ...DEFAULT_IPERF_PRESETS.map((p) => ({ host: p.host, label: p.label.split(" · ")[0] })),
];

/** Presets from text, one per line: "host label…" (label optional, # comments). Max 16. */
export function parsePresets(text: string, allowGateway = false): Preset[] {
  const out: Preset[] = [];
  for (const raw of String(text ?? "").split("\n")) {
    const l = raw.replace(/#.*$/, "").trim();
    if (!l) continue;
    const [host, ...rest] = l.split(/\s+/);
    if (!(allowGateway && host === "gateway")) validHost(host);
    out.push({ host, label: rest.join(" ") || host });
  }
  if (out.length > 16) throw new Error("At most 16 presets");
  return out;
}

/** Presets from the API: text, or a list of {host, label}. */
export function presetsFrom(v: unknown, allowGateway = false): Preset[] {
  if (typeof v === "string") return parsePresets(v, allowGateway);
  if (!Array.isArray(v)) throw new Error("presets must be text or a list of {host, label}");
  return parsePresets(v.map((p: any) => `${p?.host ?? ""} ${String(p?.label ?? "").replace(/[\n#]/g, " ")}`).join("\n"), allowGateway);
}

export const formatPresets = (list: Preset[]) => list.map((p) => (p.label && p.label !== p.host ? `${p.host} ${p.label}` : p.host)).join("\n");

export function fmtBps(bps: number): string {
  if (!bps) return "0 bit/s";
  const u = ["bit/s", "kbit/s", "Mbit/s", "Gbit/s"];
  const i = Math.min(Math.floor(Math.log10(bps) / 3), u.length - 1);
  return `${(bps / 10 ** (i * 3)).toFixed(i ? (bps / 10 ** (i * 3) < 10 ? 2 : 1) : 0)} ${u[i]}`;
}

// ---------- misc ----------

export function parseResolvConf(text: string): { nameservers: string[]; search: string[] } {
  const nameservers: string[] = [];
  const search: string[] = [];
  for (const l of text.split("\n")) {
    const m = /^\s*(nameserver|search|domain)\s+(.+)$/.exec(l);
    if (!m) continue;
    if (m[1] === "nameserver") nameservers.push(m[2].trim());
    else search.push(...m[2].trim().split(/\s+/));
  }
  return { nameservers, search };
}

/** Hostnames/IPs only: these end up as command arguments (never via a shell, but no option injection either). */
export function validHost(h: unknown): string {
  const s = String(h ?? "").trim();
  if (!/^[A-Za-z0-9][A-Za-z0-9.:\-_%]{0,252}$/.test(s)) throw new Error(`"${s}" isn't a hostname or IP address`);
  return s;
}

/**
 * A JSON response for work that may outlast Bun.serve's 10 s idle timeout (server.ts sets
 * none): headers go out at once and a space every 4 s keeps the connection alive, then the
 * JSON follows. Leading whitespace is valid JSON, so clients just parse the body.
 */
export function slowJson(work: Promise<any>): Response {
  const enc = new TextEncoder();
  let timer: ReturnType<typeof setInterval>;
  const body = new ReadableStream<Uint8Array>({
    start(ctl) {
      timer = setInterval(() => ctl.enqueue(enc.encode(" ")), 4000);
      work.then(
        (v) => ctl.enqueue(enc.encode(JSON.stringify(v ?? { ok: true }))),
        (e) => ctl.enqueue(enc.encode(JSON.stringify({ ok: false, message: e?.message || String(e) }))),
      ).finally(() => {
        clearInterval(timer);
        ctl.close();
      });
    },
    cancel() {
      clearInterval(timer);
    },
  });
  // X-Accel-Buffering: the keep-alive spaces must flow, so server.ts must not buffer this to gzip it.
  return new Response(body, { headers: { "Content-Type": "application/json", "Cache-Control": "no-store", "X-Accel-Buffering": "no" } });
}
