// ==================== Wi-Fi client test runner ====================
//
// Associates a radio to a network inside its own network namespace, so the test's
// address, routes and DNS can never compete with eth0 (or wlan0's own NetworkManager
// connection) in the main namespace. Sequence:
//
//   netns deskypi-wt-<phy index> → move the whole phy in (iw phy … set netns) →
//   wpa_supplicant (nl80211, generated conf) → udhcpc / static → ping, DNS, HTTP, iperf3
//   → cleanup in `finally`: kill children, phy back to netns 1, delete the netns →
//   make sure NetworkManager reconnects whatever it had before.
//
// Everything is timestamped into a timeline the dashboard streams while the test runs.
// Secrets: the key is written only into a 0600 conf in a 0700 temp dir that's deleted in
// cleanup, and every wpa_supplicant line is redacted before it's stored (a malformed key
// line makes wpa_supplicant echo the key in its parse error; seen on desky).

import { mkdtempSync, rmSync, writeFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import type { Subprocess } from "bun";
import { run } from "../core.ts";
import * as P from "./wifi-parse.ts";

export const NS_PREFIX = "deskypi-wt-";
export const TMP_PREFIX = "deskypi-wifi-";
/** In /etc/netns/<ns>/: the pid of the DeskyPi process running that test. */
export const OWNER_FILE = "deskypi-owner";

/** Is another live process using this pid? (EPERM: alive but not ours to signal.) */
export function pidAlive(pid: number): boolean {
  if (!pid || pid === process.pid) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (e: any) {
    return e?.code === "EPERM";
  }
}

export interface Bins {
  sudo: string[]; // ["sudo", "-n"], or [] when already root
  ip: string;
  iw: string;
  wpa: string;
  udhcpc: string;
  nmcli: string;
  ping: string;
  curl: string;
  busybox: string;
  iperf3: string;
  stdbuf: string;
  kill: string;
  rm: string;
  mkdir: string;
  tee: string;
}

export type Security = "open" | "owe" | "psk" | "sae" | "psk-sae" | "eap";

export interface TestSpec {
  phy: string;
  ssid: string;
  security: Security;
  bssid?: string;
  eap?: { method: "PEAP" | "TTLS"; identity: string; anonymousIdentity?: string };
  ip: "dhcp" | "static" | "none";
  static?: { address: string; gateway?: string; dns?: string[] };
  pingGateway: boolean;
  dns: string[];
  http: string[];
  ping: string[];
  iperf?: { server: string; port?: number; seconds?: number; reverse?: boolean } | null;
  count: number;
  holdS: number;
  assocTimeoutS: number;
  keySource?: "given" | "profile" | "networkmanager" | "none";
  profile?: string;
}

/** What the runner must put back afterwards, captured by the driver before borrowing. */
export interface RadioCtx {
  phy: string;
  ifaces: string[]; // netdevs on this phy in the main namespace
  nm: { iface: string; connection: string }[]; // NM connections to see restored
  unmanage: string[]; // ifaces DeskyPi keeps NM away from (USB dongles)
}

export interface Step {
  t: number;
  dt: number; // ms since test start
  kind: string;
  ok?: boolean;
  text: string;
  data?: any;
}

export interface TestOutcome {
  kind: "ping-gateway" | "ping" | "dns" | "http" | "iperf";
  target: string;
  ok: boolean;
  ms: number;
  summary: string;
  data?: any;
}

export interface Sample {
  t: number;
  signal?: number;
  tx?: number;
  rx?: number;
}

export interface TestResult {
  id: string;
  phy: string;
  ns: string;
  iface?: string;
  ssid: string;
  security: Security;
  spec: TestSpec; // never contains a key
  startedAt: number;
  endedAt?: number;
  stage: string;
  ok: boolean | null;
  error?: string;
  cancelled?: boolean;
  timeline: Step[];
  link?: P.LinkInfo;
  lease?: Record<string, string>;
  address?: string;
  tests: TestOutcome[];
  samples: Sample[];
  restore?: { ok: boolean; text: string };
  uplinkBefore?: string;
  uplinkAfter?: string;
  wpaLog: string[];
}

export interface Deps {
  bins: Bins;
  spawn(cmd: string[], opts?: any): Subprocess;
  changed(): void;
  log(...a: any[]): void;
  /** Lowest-metric default route's device in the main namespace. */
  uplink(): Promise<string | undefined>;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const hex = (s: string) => Buffer.from(s, "utf8").toString("hex");

// ---------- Validation ----------

export function validKey(security: Security, key: string | undefined): string | null {
  if (security === "open" || security === "owe") return null;
  if (!key) return "A key (passphrase) is required for " + security;
  if (security === "eap") return null; // EAP password: any text, written as hex
  const pmk = /^[0-9a-fA-F]{64}$/.test(key);
  if (security === "sae" && pmk) return "WPA3-SAE needs the passphrase itself, not a 64-hex PSK";
  if (pmk) return null;
  if (key.length < 8 || key.length > 63) return "WPA passphrases are 8–63 characters";
  if (!/^[\x20-\x7e]+$/.test(key)) return "WPA passphrases must be printable ASCII";
  return null;
}

/** Hosts and URLs end up as argv entries; refuse anything that could be read as an option. */
export function validHost(h: string): boolean {
  return /^[A-Za-z0-9][A-Za-z0-9.:_-]*$/.test(h) && h.length < 254;
}

export function validUrl(u: string): boolean {
  try {
    const x = new URL(u);
    return (x.protocol === "http:" || x.protocol === "https:") && !u.startsWith("-");
  } catch {
    return false;
  }
}

/**
 * May a radio whose interfaces are `mine` be borrowed? Only if it carries no default route,
 * or a lower-metric default route exists via another interface that is up. Returns the
 * refusal reason, or null. `otherUp` reports the best other route's link state.
 */
export function borrowRefusal(routes: { dev: string; metric: number }[], mine: Set<string>, otherUp: (dev: string) => boolean): string | null {
  const own = routes.filter((x) => mine.has(x.dev));
  if (!own.length) return null;
  const name = own[0].dev;
  const others = routes.filter((x) => !mine.has(x.dev)).sort((a, b) => a.metric - b.metric);
  if (!others.length) return `${name} is the uplink (the only default route); refusing to borrow it`;
  if (Math.min(...own.map((x) => x.metric)) <= others[0].metric) return `${name} is the uplink (lowest-metric default route); refusing to borrow it`;
  if (!otherUp(others[0].dev)) return `${others[0].dev} is down, so ${name} is effectively the uplink; refusing to borrow it`;
  return null;
}

/** wpa_supplicant network block. String fields go in as hex so no quoting can break out. */
export function wpaConf(spec: TestSpec, key?: string): string {
  const n: string[] = [`ssid=${hex(spec.ssid)}`, "scan_ssid=1"];
  if (spec.bssid) n.push(`bssid=${spec.bssid}`);
  const pmk = !!key && /^[0-9a-fA-F]{64}$/.test(key);
  const psk = () => (pmk ? `psk=${key!.toLowerCase()}` : `psk="${key}"`);
  switch (spec.security) {
    case "open":
      n.push("key_mgmt=NONE");
      break;
    case "owe":
      n.push("key_mgmt=OWE", "ieee80211w=2");
      break;
    case "psk":
      n.push("key_mgmt=WPA-PSK", "ieee80211w=1", psk());
      break;
    case "sae":
      n.push("key_mgmt=SAE", "ieee80211w=2", `sae_password=${hex(key!)}`);
      break;
    case "psk-sae":
      // A hex PMK can't do SAE, so with one this is plain WPA2-PSK.
      if (pmk) n.push("key_mgmt=WPA-PSK", "ieee80211w=1", psk());
      else n.push("key_mgmt=WPA-PSK SAE", "ieee80211w=1", psk(), `sae_password=${hex(key!)}`);
      break;
    case "eap":
      n.push("key_mgmt=WPA-EAP WPA-EAP-SHA256", "ieee80211w=1", `eap=${spec.eap?.method || "PEAP"}`, `identity=${hex(spec.eap?.identity || "")}`, `password=${hex(key || "")}`, 'phase2="auth=MSCHAPV2"');
      if (spec.eap?.anonymousIdentity) n.push(`anonymous_identity=${hex(spec.eap.anonymousIdentity)}`);
      break;
  }
  return ["p2p_disabled=1", "sae_pwe=2", "network={", ...n.map((l) => "\t" + l), "}", ""].join("\n");
}

/** udhcpc script: applies the lease inside the netns and echoes it for the timeline. */
export function dhcpScript(ns: string, bins: Bins): string {
  return `#!/bin/sh
# Generated by DeskyPi for a Wi-Fi client test in netns ${ns}; deleted afterwards.
echo "DESKYPI-DHCP $1|ip=$ip|mask=$mask|router=$router|dns=$dns|lease=$lease|serverid=$serverid|domain=$domain|broadcast=$broadcast|ntpsrv=$ntpsrv"
case "$1" in
  deconfig) ${bins.ip} addr flush dev "$interface" ;;
  bound|renew)
    ${bins.ip} addr flush dev "$interface"
    ${bins.ip} addr add "$ip/$mask" \${broadcast:+broadcast $broadcast} dev "$interface"
    for r in $router; do ${bins.ip} route replace default via "$r" dev "$interface"; break; done
    : > /etc/netns/${ns}/resolv.conf
    [ -n "$domain" ] && echo "search $domain" >> /etc/netns/${ns}/resolv.conf
    for d in $dns; do echo "nameserver $d" >> /etc/netns/${ns}/resolv.conf; done ;;
esac
exit 0
`;
}

// ---------- Line reader ----------

async function lines(stream: ReadableStream<Uint8Array> | null | undefined, cb: (l: string) => void) {
  if (!stream) return;
  const dec = new TextDecoder();
  let buf = "";
  for await (const chunk of stream as any) {
    buf += dec.decode(chunk, { stream: true });
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      cb(buf.slice(0, i));
      buf = buf.slice(i + 1);
    }
  }
  if (buf) cb(buf);
}

// ---------- Runner ----------

export class ClientTest {
  r: TestResult;
  private key?: string;
  private procs = new Set<Subprocess>();
  private cancelled = false;
  private tmp = "";
  private createdNs = false;
  private sampler: ReturnType<typeof setInterval> | null = null;
  private wpaEvents: P.WpaEvent[] = [];
  private wpaWaiters: ((e: P.WpaEvent | null) => void)[] = [];
  // Events not yet consumed by associate(). Several lines often arrive in one stdout chunk
  // (SAE commit, confirm and the reject within 2 ms), faster than the loop re-waits.
  private wpaQueue: P.WpaEvent[] = [];
  private wpaExited = false;
  private connected = false;
  /** Skip the NetworkManager wait (driver shutting down): NM reconnects by itself anyway. */
  fast = false;

  constructor(id: string, public spec: TestSpec, key: string | undefined, private radio: RadioCtx, private d: Deps) {
    this.key = key;
    const ns = NS_PREFIX + spec.phy.replace(/^phy/, "");
    this.r = { id, phy: spec.phy, ns, ssid: spec.ssid, security: spec.security, spec, startedAt: Date.now(), stage: "prepare", ok: null, timeline: [], tests: [], samples: [], wpaLog: [] };
  }

  get ns() {
    return this.r.ns;
  }

  private step(kind: string, text: string, ok?: boolean, data?: any) {
    const t = Date.now();
    this.r.timeline.push({ t, dt: t - this.r.startedAt, kind, ok, text, data });
    this.d.changed();
  }

  private stage(s: string) {
    this.r.stage = s;
    this.d.changed();
  }

  private check() {
    if (this.cancelled) throw new Error("Cancelled");
  }

  cancel() {
    if (this.cancelled) return;
    this.cancelled = true;
    this.step("cancel", "Cancel requested");
    for (const p of this.procs) p.kill();
    for (const w of this.wpaWaiters.splice(0)) w(null);
  }

  private priv(cmd: string[], timeoutMs = 15000) {
    return run([...this.d.bins.sudo, ...cmd], { timeoutMs });
  }

  private inNs(cmd: string[], timeoutMs = 15000) {
    return this.priv([this.d.bins.ip, "netns", "exec", this.ns, ...cmd], timeoutMs);
  }

  private spawnNs(cmd: string[]): Subprocess {
    const p = this.d.spawn([...this.d.bins.sudo, this.d.bins.ip, "netns", "exec", this.ns, ...cmd], { stdin: "ignore", stdout: "pipe", stderr: "pipe" });
    this.procs.add(p);
    p.exited.then(() => this.procs.delete(p));
    return p;
  }

  async run(): Promise<TestResult> {
    const b = this.d.bins;
    try {
      this.r.uplinkBefore = await this.d.uplink();
      this.step("start", `Testing ${this.spec.ssid} (${this.spec.security}) on ${this.spec.phy}; main uplink ${this.r.uplinkBefore || "none"}`);
      await this.prepare();
      this.check();
      await this.borrow();
      this.check();
      await this.associate();
      this.check();
      this.startSampler();
      await this.address();
      this.check();
      await this.runTests();
      this.check();
      if (this.spec.holdS > 0) {
        this.stage("hold");
        this.step("hold", `Holding the connection for ${this.spec.holdS} s`);
        const until = Date.now() + this.spec.holdS * 1000;
        while (Date.now() < until && this.connected && !this.cancelled) await sleep(500);
        this.check();
        if (!this.connected) throw new Error("Connection dropped during the hold period");
        this.check();
        this.step("hold", "Hold complete", true);
      }
      this.r.ok = this.r.tests.every((t) => t.ok);
      if (!this.r.ok) this.r.error = `${this.r.tests.filter((t) => !t.ok).length} of ${this.r.tests.length} tests failed`;
    } catch (e: any) {
      this.r.ok = false;
      this.r.cancelled = this.cancelled || undefined;
      this.r.error = e?.message || String(e);
      this.step("error", this.r.error!, false);
    } finally {
      this.stopSampler();
      this.stage("cleanup");
      await this.cleanup().catch((e) => this.step("cleanup", `Cleanup error: ${e.message}`, false));
      this.key = undefined;
      this.stage("restore");
      await this.restore().catch((e) => this.step("restore", `Restore error: ${e.message}`, false));
      this.r.uplinkAfter = await this.d.uplink();
      this.step("uplink", `Main namespace default route via ${this.r.uplinkAfter || "nothing"}`, this.r.uplinkAfter === this.r.uplinkBefore);
      this.r.endedAt = Date.now();
      this.stage("done");
      this.step("done", this.r.ok ? "Passed" : `Failed: ${this.r.error}`, !!this.r.ok);
    }
    void b;
    return this.r;
  }

  // ---- stages ----

  private async prepare() {
    const b = this.d.bins;
    const keyErr = validKey(this.spec.security, this.key);
    if (keyErr) throw new Error(keyErr);
    // Creating the netns is the claim on this phy: it fails if another DeskyPi process (a
    // second instance, say) is mid-test on it, and then cleanup must leave it alone.
    let r = await this.priv([b.ip, "netns", "add", this.ns]);
    if (r.code) throw new Error(/exists/i.test(r.err) ? `${this.ns} already exists: another DeskyPi process is testing ${this.spec.phy}` : `ip netns add ${this.ns}: ${r.err}`);
    this.createdNs = true;
    // The pid lets crash recovery in another process tell a live test from a leftover.
    this.tmp = mkdtempSync(`${tmpdir()}/${TMP_PREFIX}${process.pid}-`); // mkdtemp makes it 0700
    writeFileSync(this.tmp + "/wpa.conf", wpaConf(this.spec, this.key), { mode: 0o600 });
    writeFileSync(this.tmp + "/dhcp.sh", dhcpScript(this.ns, b), { mode: 0o700 });
    chmodSync(this.tmp + "/dhcp.sh", 0o700);
    // `ip netns exec` bind-mounts /etc/netns/<ns>/resolv.conf over /etc/resolv.conf, but
    // only if it exists when the command starts, so create it (empty) up front.
    r = await this.priv([b.mkdir, "-p", `/etc/netns/${this.ns}`]);
    if (r.code) throw new Error(`mkdir /etc/netns/${this.ns}: ${r.err}`);
    await run([...b.sudo, b.tee, `/etc/netns/${this.ns}/resolv.conf`], { stdin: "" });
    await run([...b.sudo, b.tee, `/etc/netns/${this.ns}/${OWNER_FILE}`], { stdin: String(process.pid) });
    this.step("netns", `Created network namespace ${this.ns}`, true);
  }

  private async borrow() {
    const b = this.d.bins;
    this.stage("borrow");
    const r = await this.priv([b.iw, "phy", this.spec.phy, "set", "netns", "name", this.ns]);
    if (r.code) throw new Error(`Couldn't move ${this.spec.phy} into ${this.ns}: ${r.err || r.out}`);
    const dev = P.parseIwDev((await this.inNs([b.iw, "dev"])).out);
    const iface = dev.find((x) => x.name && x.type === "managed")?.name || dev.find((x) => x.name)?.name;
    if (!iface) throw new Error(`${this.spec.phy} has no network interface inside ${this.ns}`);
    this.r.iface = iface;
    await this.inNs([b.ip, "link", "set", "lo", "up"]);
    const up = await this.inNs([b.ip, "link", "set", iface, "up"]);
    if (up.code) throw new Error(`ip link set ${iface} up: ${up.err}`);
    this.step("borrow", `Moved ${this.spec.phy} (${this.radio.ifaces.join(", ") || iface}) into ${this.ns}; ${iface} is up`, true);
  }

  private onWpaLine(line: string) {
    const ev = P.parseWpaLine(line, this.key ? [this.key, hex(this.key)] : []);
    if (!ev) return;
    this.r.wpaLog.push(ev.text);
    if (this.r.wpaLog.length > 80) this.r.wpaLog.shift();
    this.wpaEvents.push(ev);
    const bss = ev.bssid ? ` ${ev.bssid}` : "";
    switch (ev.kind) {
      case "auth-start":
        this.step("auth", `Authenticating with${bss}${ev.freq ? ` on ${ev.freq} MHz` : ""}`);
        break;
      case "assoc-start":
        this.step("assoc-start", `Authenticating + associating with${bss}${ev.freq ? ` on ${ev.freq} MHz` : ""}`, undefined, { bssid: ev.bssid, freq: ev.freq });
        break;
      case "assoc":
        this.step("assoc", `Associated with${bss}`, true);
        break;
      case "handshake":
        this.step("handshake", `4-way handshake complete [${ev.detail}]`, true);
        break;
      case "connected":
        this.connected = true;
        if (this.spec.security === "open" || this.spec.security === "owe") this.step("connected", "Connected", true);
        else this.step("connected", "Connected (keys installed)", true);
        break;
      case "disconnected":
        if (this.cancelled || ["cleanup", "restore", "done"].includes(this.r.stage)) this.step("disconnected", "Disconnected (test teardown)");
        else if (this.connected) this.step("disconnected", `Disconnected: reason ${ev.reason}${ev.detail ? ` (${ev.detail})` : ""}`, false);
        else this.step("disconnected", `Attempt failed: reason ${ev.reason}${ev.detail ? ` (${ev.detail})` : ""}`, false);
        this.connected = false;
        break;
      case "wrong-key":
        this.step("wrong-key", "4-way handshake failed: wrong key", false);
        break;
      case "temp-disabled":
        this.step("temp-disabled", `wpa_supplicant gave up on this network for now: ${ev.detail}`, false);
        break;
      case "assoc-reject":
      case "auth-reject":
        this.step(ev.kind, `${ev.kind === "assoc-reject" ? "Association" : "Authentication"} rejected by${bss}: status ${ev.status}${ev.detail ? ` (${ev.detail})` : ""}`, false);
        break;
      case "not-found":
        this.step("not-found", "Scan found no matching network", false);
        break;
      case "eap":
        this.step("eap", ev.text);
        break;
      case "config-error":
      case "error":
        this.step("wpa-error", ev.text, false);
        break;
    }
    const w = this.wpaWaiters.shift();
    if (w) w(ev);
    else this.wpaQueue.push(ev);
  }

  private nextWpa(timeoutMs: number): Promise<P.WpaEvent | null> {
    if (this.wpaQueue.length) return Promise.resolve(this.wpaQueue.shift()!);
    return new Promise((resolve) => {
      const t = setTimeout(() => {
        this.wpaWaiters = this.wpaWaiters.filter((w) => w !== done);
        resolve(null);
      }, timeoutMs);
      const done = (e: P.WpaEvent | null) => {
        clearTimeout(t);
        resolve(e);
      };
      this.wpaWaiters.push(done);
    });
  }

  private async associate() {
    const b = this.d.bins;
    this.stage("associate");
    // stdbuf: wpa_supplicant's stdout is block-buffered on a pipe, which would hold every
    // event until exit.
    const p = this.spawnNs([b.stdbuf, "-oL", "-eL", b.wpa, "-i", this.r.iface!, "-D", "nl80211", "-c", this.tmp + "/wpa.conf"]);
    lines(p.stdout as any, (l) => this.onWpaLine(l));
    lines(p.stderr as any, (l) => this.onWpaLine(l));
    p.exited.then((code) => {
      this.wpaExited = true;
      if (!this.cancelled && this.r.stage !== "cleanup" && this.r.stage !== "restore" && this.r.stage !== "done") this.step("wpa-exit", `wpa_supplicant exited (code ${code})`, false);
      this.connected = false;
      for (const w of this.wpaWaiters.splice(0)) w(null);
    });
    this.step("wpa-start", `wpa_supplicant started on ${this.r.iface} (${this.spec.security}${this.spec.bssid ? `, pinned to ${this.spec.bssid}` : ""})`);

    const deadline = Date.now() + this.spec.assocTimeoutS * 1000;
    let notFound = 0;
    let rejects = 0;
    let failedAttempts = 0;
    while (!this.connected) {
      this.check();
      if (this.wpaExited) {
        const cfg = this.wpaEvents.find((e) => e.kind === "config-error");
        throw new Error(cfg ? `wpa_supplicant rejected the configuration: ${cfg.text}` : "wpa_supplicant exited before connecting");
      }
      const left = deadline - Date.now();
      if (left <= 0) {
        const seen = this.wpaEvents.some((e) => e.kind === "assoc-start" || e.kind === "auth-start");
        throw new Error(seen ? `Timed out after ${this.spec.assocTimeoutS} s without completing the connection` : `Network "${this.spec.ssid}" not found within ${this.spec.assocTimeoutS} s`);
      }
      const ev = await this.nextWpa(Math.min(left, 3000));
      if (!ev) {
        // CTRL-EVENT-NETWORK-NOT-FOUND only goes to the control socket, not stdout, so a
        // missing network would otherwise just time out. The kernel's scan cache inside
        // the netns holds what wpa_supplicant's scans (with probe requests for this SSID,
        // so hidden networks answer too) have found.
        const elapsed = Date.now() - (deadline - this.spec.assocTimeoutS * 1000);
        const started = this.wpaEvents.some((e) => e.kind === "assoc-start" || e.kind === "auth-start");
        if (!started && elapsed > 12000 && !this.connected) {
          const dump = await this.inNs([this.d.bins.iw, "dev", this.r.iface!, "scan", "dump"], 5000);
          const list = P.parseIwScan(dump.out);
          if (list.length && !list.some((x) => x.ssid === this.spec.ssid)) {
            if (++notFound >= 2) throw new Error(`Network "${this.spec.ssid}" not found: not among the ${list.length} BSSes this radio can see`);
          } else notFound = 0;
        }
        continue;
      }
      if (ev.kind === "wrong-key" || (ev.kind === "temp-disabled" && /WRONG_KEY/.test(ev.detail || ""))) throw new Error("Wrong key: the 4-way handshake failed");
      if (ev.kind === "temp-disabled" && /AUTH_FAILED/.test(ev.detail || "")) throw new Error("Authentication failed (wrong password, or SAE/EAP rejected)");
      // SAE (auth_type=3) rejected at the confirm (transaction 2): the AP's and our
      // password-derived confirms didn't match. Seen on desky with a wrong password.
      if (ev.kind === "auth-reject" && /auth_type=3 auth_transaction=2/.test(ev.text)) throw new Error(`Wrong password: WPA3-SAE confirm rejected by the AP (status ${ev.status})`);
      if (ev.kind === "not-found" && ++notFound >= 4) throw new Error(`Network "${this.spec.ssid}" not found (${notFound} scans)`);
      if ((ev.kind === "assoc-reject" || ev.kind === "auth-reject") && ++rejects >= 3) throw new Error(`Rejected by the AP ${rejects} times: status ${ev.status}${ev.detail ? ` (${ev.detail})` : ""}`);
      if (ev.kind === "disconnected" && ++failedAttempts >= 3) {
        throw new Error(ev.reason === 15 ? "Wrong key: 4-way handshake timed out (reason 15)" : `Connection attempts keep failing: reason ${ev.reason}${ev.detail ? ` (${ev.detail})` : ""}`);
      }
      if (ev.kind === "disconnected" && ev.reason === 15 && this.spec.security !== "open") throw new Error("Wrong key: 4-way handshake timed out (reason 15)");
    }
    await this.sampleLink(true);
  }

  private async sampleLink(record = false) {
    const b = this.d.bins;
    if (!this.r.iface) return;
    const [link, sta] = await Promise.all([this.inNs([b.iw, "dev", this.r.iface, "link"], 5000), this.inNs([b.iw, "dev", this.r.iface, "station", "dump"], 5000)]);
    const l = P.parseLink(link.out, sta.out);
    if (!l.connected) return;
    this.r.samples.push({ t: Date.now(), signal: l.signal, tx: l.txBitrate, rx: l.rxBitrate });
    if (this.r.samples.length > 300) this.r.samples.shift();
    if (record) {
      this.r.link = l;
      this.step("link", `${l.bssid} on ${l.freq} MHz (ch ${P.freqToChannel(l.freq || 0)}), ${l.signal} dBm, tx ${l.txRate || "?"}, rx ${l.rxRate || "?"}`, true, l);
    } else if (this.r.link) Object.assign(this.r.link, { signal: l.signal, txRate: l.txRate, rxRate: l.rxRate, txBitrate: l.txBitrate, rxBitrate: l.rxBitrate });
    this.d.changed();
  }

  private startSampler() {
    this.sampler = setInterval(() => {
      if (this.connected) this.sampleLink().catch(() => {});
    }, 2000);
  }

  private stopSampler() {
    if (this.sampler) clearInterval(this.sampler);
    this.sampler = null;
  }

  private async address() {
    const b = this.d.bins;
    const iface = this.r.iface!;
    if (this.spec.ip === "none") {
      this.step("ip", "No IP configuration requested (link-layer test only)");
      return;
    }
    if (this.spec.ip === "static") {
      this.stage("static");
      const s = this.spec.static!;
      let r = await this.inNs([b.ip, "addr", "add", s.address, "dev", iface]);
      if (r.code) throw new Error(`ip addr add ${s.address}: ${r.err}`);
      if (s.gateway) {
        r = await this.inNs([b.ip, "route", "replace", "default", "via", s.gateway, "dev", iface]);
        if (r.code) throw new Error(`default route via ${s.gateway}: ${r.err}`);
      }
      if (s.dns?.length) await run([...b.sudo, b.tee, `/etc/netns/${this.ns}/resolv.conf`], { stdin: s.dns.map((d) => `nameserver ${d}\n`).join("") });
      this.r.address = s.address;
      this.r.lease = { router: s.gateway || "", dns: (s.dns || []).join(" ") };
      this.step("static", `Static ${s.address}${s.gateway ? ` via ${s.gateway}` : ""}${s.dns?.length ? `, DNS ${s.dns.join(" ")}` : ""}`, true);
      return;
    }
    this.stage("dhcp");
    const p = this.spawnNs([b.udhcpc, "-i", iface, "-f", "-q", "-n", "-t", "4", "-T", "3", "-A", "1", "-s", this.tmp + "/dhcp.sh"]);
    let lease: Record<string, string> | undefined;
    let discovers = 0;
    const onLine = (l: string) => {
      const e = P.parseUdhcpcLine(l);
      if (!e) return;
      if (e.kind === "discover") this.step("dhcp-discover", `DHCP discover${++discovers > 1 ? ` (#${discovers})` : ""}`);
      else if (e.kind === "offer") this.step("dhcp-offer", `Offer of ${e.ip} from ${e.server}; requesting it`, true, { ip: e.ip, server: e.server });
      else if (e.kind === "ack") this.step("dhcp-ack", `ACK: ${e.ip} from ${e.server}, lease ${e.leaseS} s`, true, { ip: e.ip, server: e.server, leaseS: e.leaseS });
      else if (e.kind === "nak") this.step("dhcp-nak", "DHCP NAK", false);
      else if (e.kind === "fail") this.step("dhcp-fail", "No DHCP lease", false);
      else if (e.kind === "lease" && (e.lease?.event === "bound" || e.lease?.event === "renew")) lease = e.lease;
    };
    const timer = setTimeout(() => p.kill(), 30000);
    await Promise.all([lines(p.stdout as any, onLine), lines(p.stderr as any, onLine), p.exited]);
    clearTimeout(timer);
    if (!lease) throw new Error(this.cancelled ? "Cancelled" : "DHCP failed: no lease");
    const { event, ...rest } = lease;
    void event;
    this.r.lease = rest;
    this.r.address = `${lease.ip}/${P.maskToPrefix(lease.mask || "32")}`;
    this.step("lease", `Address ${this.r.address}, router ${lease.router || "none"}, DNS ${lease.dns || "none"}${lease.domain ? `, domain ${lease.domain}` : ""}`, true, rest);
  }

  private async timed<T>(fn: () => Promise<T>): Promise<[T, number]> {
    const t0 = performance.now();
    const v = await fn();
    return [v, Math.round(performance.now() - t0)];
  }

  private record(o: TestOutcome) {
    this.r.tests.push(o);
    this.step("test", `${o.kind} ${o.target}: ${o.summary}`, o.ok, o);
  }

  private async ping(kind: "ping-gateway" | "ping", host: string) {
    const b = this.d.bins;
    const n = this.spec.count;
    const [r, ms] = await this.timed(() => this.inNs([b.ping, "-c", String(n), "-i", "0.3", "-W", "2", "-w", String(Math.ceil(n * 0.3 + 5)), host], (n * 0.3 + 10) * 1000));
    const p = P.parsePing(r.out + "\n" + r.err);
    const ok = p.received > 0;
    this.record({ kind, target: host, ok, ms, summary: p.error && !p.received ? p.error : `${p.received}/${p.sent} replies, ${p.lossPct}% loss${p.avg != null ? `, avg ${p.avg} ms (min ${p.min}, max ${p.max})` : ""}`, data: p });
  }

  private async runTests() {
    const b = this.d.bins;
    const s = this.spec;
    if (s.ip === "none") return;
    this.stage("tests");
    const gw = this.r.lease?.router?.split(" ")[0];
    if (s.pingGateway) {
      if (gw) await this.ping("ping-gateway", gw);
      else this.record({ kind: "ping-gateway", target: "-", ok: false, ms: 0, summary: "No gateway to ping" });
    }
    for (const name of s.dns) {
      this.check();
      const [r, ms] = await this.timed(() => this.inNs([b.busybox, "nslookup", name], 12000));
      const n = P.parseNslookup(r.out + "\n" + r.err);
      this.record({ kind: "dns", target: name, ok: n.addresses.length > 0, ms, summary: n.addresses.length ? `${n.addresses.slice(0, 4).join(", ")}${n.addresses.length > 4 ? ` +${n.addresses.length - 4}` : ""} via ${n.server || "?"} in ${ms} ms` : n.error || "no answer", data: n });
    }
    for (const url of s.http) {
      this.check();
      const [r, ms] = await this.timed(() => this.inNs([b.curl, "-sS", "-L", "--max-redirs", "5", "-o", "/dev/null", "--max-time", "20", "-w", "%{json}", url], 25000));
      const h = P.parseCurlJson(r.out);
      this.record({ kind: "http", target: url, ok: h.ok, ms, summary: h.ok ? `HTTP ${h.code} from ${h.remoteIp} in ${h.totalMs} ms (DNS ${h.dnsMs ?? "-"}, connect ${h.connectMs ?? "-"}, TLS ${h.tlsMs ?? "-"}, first byte ${h.firstByteMs ?? "-"})` : h.error || `HTTP ${h.code}`, data: h });
    }
    for (const host of s.ping) {
      this.check();
      await this.ping("ping", host);
    }
    if (s.iperf?.server) {
      this.check();
      const secs = s.iperf.seconds || 5;
      const args = [b.iperf3, "-c", s.iperf.server, "-p", String(s.iperf.port || 5201), "-t", String(secs), "-J", "--connect-timeout", "5000"];
      if (s.iperf.reverse) args.push("-R");
      const [r, ms] = await this.timed(() => this.inNs(args, (secs + 20) * 1000));
      const ip = P.parseIperf(r.out);
      const mbps = (x?: number) => (x != null ? (x / 1e6).toFixed(1) + " Mbit/s" : "?");
      this.record({ kind: "iperf", target: `${s.iperf.server}:${s.iperf.port || 5201}${s.iperf.reverse ? " (download)" : " (upload)"}`, ok: !ip.error, ms, summary: ip.error || `sent ${mbps(ip.sentBps)}, received ${mbps(ip.receivedBps)}${ip.retransmits != null ? `, ${ip.retransmits} retransmits` : ""}`, data: ip });
    }
  }

  // ---- cleanup + restore ----

  private async cleanup() {
    const b = this.d.bins;
    for (const p of this.procs) p.kill();
    if (!this.createdNs) {
      // We never claimed the namespace (it was someone else's, or creating it failed):
      // nothing of ours is in it and the phy never moved.
      if (this.tmp) rmSync(this.tmp, { recursive: true, force: true });
      return;
    }
    await sleep(300);
    // Belt and braces: anything still running in the namespace (sudo relays SIGTERM, but a
    // child may have outlived its sudo).
    await killNsPids(this.ns, b);
    let back = false;
    for (let i = 0; i < 3 && !back; i++) {
      const r = await this.priv([b.ip, "netns", "exec", this.ns, b.iw, "phy", this.spec.phy, "set", "netns", "1"]);
      // A phy that never left (borrow failed) reports "No such file"; check it's home.
      const home = await run([b.iw, "phy", this.spec.phy, "info"]);
      back = home.code === 0;
      if (!back) {
        this.step("cleanup", `Couldn't return ${this.spec.phy} yet: ${r.err || r.out}`, false);
        await sleep(1000);
      }
    }
    if (back) this.step("cleanup", `${this.spec.phy} is back in the main namespace`, true);
    else this.step("cleanup", `${this.spec.phy} is STILL in ${this.ns}; the driver retries on its next start`, false);
    if (back) {
      const d = await this.priv([b.ip, "netns", "del", this.ns]);
      this.step("cleanup", d.code ? `ip netns del ${this.ns}: ${d.err}` : `Deleted ${this.ns}`, !d.code);
      await this.priv([b.rm, "-rf", `/etc/netns/${this.ns}`]);
    }
    if (this.tmp) rmSync(this.tmp, { recursive: true, force: true });
  }

  private async restore() {
    const b = this.d.bins;
    // NetworkManager sees the returning interface as a new device; dongles DeskyPi keeps
    // to itself need telling again before NM's autoconnect grabs them.
    for (const iface of this.radio.unmanage) {
      for (let i = 0; i < 10 && (await run([b.ip, "link", "show", iface])).code; i++) await sleep(500);
      const r = await this.priv([b.nmcli, "dev", "set", iface, "managed", "no"]);
      this.step("restore", r.code ? `nmcli dev set ${iface} managed no: ${r.err}` : `Told NetworkManager to leave ${iface} alone again`, !r.code);
    }
    if (!this.radio.nm.length) {
      this.r.restore = { ok: true, text: "Nothing to restore (NetworkManager had no connection on this radio)" };
      return;
    }
    const t0 = Date.now();
    for (const { iface, connection } of this.radio.nm) {
      const state = async () => {
        const r = await run([b.nmcli, "-t", "-f", "GENERAL.STATE,GENERAL.CONNECTION", "dev", "show", iface]);
        return { connected: /GENERAL.STATE:100/.test(r.out), conn: /GENERAL.CONNECTION:(.*)/.exec(r.out)?.[1] || "" };
      };
      let s = await state();
      const limit = this.fast ? 0 : 20000;
      while (!(s.connected && s.conn === connection) && Date.now() - t0 < limit) {
        await sleep(1000);
        s = await state();
      }
      if (s.connected && s.conn === connection) {
        this.r.restore = { ok: true, text: `NetworkManager reconnected ${iface} to ${connection} after ${((Date.now() - t0) / 1000).toFixed(1)} s` };
        this.step("restore", this.r.restore.text, true);
        continue;
      }
      if (this.fast) {
        this.r.restore = { ok: false, text: `Shutting down; left ${iface} for NetworkManager to reconnect` };
        this.step("restore", this.r.restore.text);
        continue;
      }
      this.step("restore", `${iface} not back on ${connection} after 20 s (state: ${s.connected ? "connected to " + s.conn : "not connected"}); running nmcli con up`, false);
      const up = await this.priv([b.nmcli, "con", "up", "id", connection, "ifname", iface], 60000);
      s = await state();
      this.r.restore = { ok: s.connected && s.conn === connection, text: up.code ? `nmcli con up ${connection} failed: ${up.err || up.out}` : `nmcli con up ${connection}: ${s.connected ? "connected" : "still not connected"}` };
      this.step("restore", this.r.restore.text, this.r.restore.ok);
    }
  }
}

/** Kill every process inside a namespace: TERM, then KILL for stragglers. */
export async function killNsPids(ns: string, b: Bins) {
  for (const sig of ["-TERM", "-KILL"]) {
    const pids = (await run([...b.sudo, b.ip, "netns", "pids", ns])).out.split(/\s+/).filter((x) => /^\d+$/.test(x));
    if (!pids.length) return;
    await run([...b.sudo, b.kill, sig, ...pids]);
    await sleep(500);
  }
}

/**
 * After a crash: give back every phy found in leftover deskypi-wt-* namespaces and delete
 * them. Returns a line per action for the timeline.
 */
export async function recoverNamespaces(b: Bins): Promise<string[]> {
  const out: string[] = [];
  const list = (await run([...b.sudo, b.ip, "netns", "list"])).out;
  for (const ns of list.split("\n").map((l) => l.split(" ")[0]).filter((n) => n.startsWith(NS_PREFIX))) {
    // A second DeskyPi instance (a side instance next to the service) may be mid-test.
    const owner = parseInt((await run([...b.sudo, "cat", `/etc/netns/${ns}/${OWNER_FILE}`])).out) || 0;
    if (pidAlive(owner)) {
      out.push(`Left ${ns} alone: DeskyPi process ${owner} is still using it`);
      continue;
    }
    await killNsPids(ns, b);
    const phys = [...new Set(P.parseIwDev((await run([...b.sudo, b.ip, "netns", "exec", ns, b.iw, "dev"])).out).map((x) => x.phy))];
    // A phy with no interfaces doesn't show in `iw dev`; `iw phy` lists those too.
    for (const m of (await run([...b.sudo, b.ip, "netns", "exec", ns, b.iw, "phy"])).out.matchAll(/^Wiphy (\S+)/gm)) if (!phys.includes(m[1])) phys.push(m[1]);
    for (const phy of phys) {
      const r = await run([...b.sudo, b.ip, "netns", "exec", ns, b.iw, "phy", phy, "set", "netns", "1"]);
      out.push(r.code ? `Couldn't return ${phy} from ${ns}: ${r.err}` : `Returned ${phy} from leftover ${ns}`);
    }
    const d = await run([...b.sudo, b.ip, "netns", "del", ns]);
    await run([...b.sudo, b.rm, "-rf", `/etc/netns/${ns}`]);
    out.push(d.code ? `Couldn't delete ${ns}: ${d.err}` : `Deleted leftover ${ns}`);
  }
  // Temp dirs from a crashed run hold a key; remove them.
  for (const f of new Bun.Glob(TMP_PREFIX + "*").scanSync({ cwd: tmpdir(), onlyFiles: false })) {
    if (pidAlive(parseInt(f.slice(TMP_PREFIX.length)))) continue;
    rmSync(tmpdir() + "/" + f, { recursive: true, force: true });
    out.push(`Removed stale ${tmpdir()}/${f}`);
  }
  return out;
}
