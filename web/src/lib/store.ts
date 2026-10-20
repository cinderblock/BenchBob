// Live state for one bob: its status stream, sparkline history and build version. One
// store per BobConnection, owned by a <Bob>; nothing here is module-level, so several bobs
// can share a page.
//
// A Linux bob pushes its whole status over SSE (/api/events) every ~2 s, plus "periph"
// events carrying just the peripherals part in between. A microcontroller node has no SSE:
// its status is polled from /api/status, and its panels poll their own endpoints.

import type { BobConnection } from "./connection";
import type { NodeStatus, Peripherals, Status } from "./types";

export const HIST_N = 30;

export type BobKind = "connecting" | "linux" | "node";

export interface NodeInfo {
  name: string;
  appVersion?: string;
  board?: string;
  boardName?: string;
  chip?: string;
  endpoints: Record<string, unknown>;
}

export interface BobSnapshot {
  kind: BobKind;
  connected: boolean;
  status: Status | null;
  node: NodeStatus | null;
  nodeInfo: NodeInfo | null;
  /** Build/firmware version of the bob as first seen, and as now. */
  firstBuild: string | null;
  build: string | null;
  /** The page should reload (same-origin, stamped page, the bob's build changed). */
  reloadPending: boolean;
  /** The bob's version changed under a page it didn't serve: tell the user, don't reload. */
  versionNotice: { from: string; to: string } | null;
  /** Bumped on every full status message, for things that tick with the stream. */
  tick: number;
}

/** What to do when the bob reports `current` and the page was stamped with `pageBuild`. */
export function buildCheck(o: { sameOrigin: boolean; pageBuild?: string | null; first: string | null; current: string | null | undefined }): "ok" | "reload" | "notice" {
  if (!o.current) return "ok";
  const stamped = !!o.pageBuild && !o.pageBuild.startsWith("__BUILD_");
  if (o.sameOrigin && stamped) return o.current === o.pageBuild ? "ok" : "reload";
  if (o.first && o.current !== o.first) return "notice";
  return "ok";
}

export interface StoreOptions {
  /** The build version the serving bob stamped into the page, if any. */
  pageBuild?: string | null;
  /** The MCU build: only nodes, no SSE attempt. */
  nodeOnly?: boolean;
  /** Status poll period for nodes (ms). */
  nodePollMs?: number;
}

export class BobStore {
  private snap: BobSnapshot = { kind: "connecting", connected: false, status: null, node: null, nodeInfo: null, firstBuild: null, build: null, reloadPending: false, versionNotice: null, tick: 0 };
  private subs = new Set<() => void>();
  private es: EventSource | null = null;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private stopped = true;
  private seeded = false;
  /** Sparkline history: "cpu", "mem", "wifi", "net:<if>:rx|tx", "drv:<dev>:r|w". */
  readonly hist: Record<string, number[]> = {};

  constructor(readonly conn: BobConnection, private opts: StoreOptions = {}) {}

  subscribe = (fn: () => void) => {
    this.subs.add(fn);
    return () => this.subs.delete(fn);
  };
  get = () => this.snap;

  private set(p: Partial<BobSnapshot>) {
    this.snap = { ...this.snap, ...p };
    for (const f of this.subs) f();
  }

  start() {
    if (!this.stopped) return;
    this.stopped = false;
    if (this.opts.nodeOnly) this.startNode();
    else this.connectSse();
  }

  stop() {
    this.stopped = true;
    this.es?.close();
    this.es = null;
    clearTimeout(this.timer);
  }

  /** The user acknowledged a version notice: the new version becomes the reference. */
  dismissVersionNotice() {
    this.set({ versionNotice: null, firstBuild: this.snap.build });
  }

  // ---------- Linux: SSE ----------

  private connectSse() {
    this.es?.close();
    let gotOne = false;
    const es = (this.es = this.conn.events());
    es.onmessage = (e) => {
      gotOne = true;
      this.onStatus(JSON.parse(e.data));
    };
    // Peripheral changes between full pushes: only that part of the status.
    es.addEventListener("periph", (e) => {
      const s = this.snap.status;
      if (!s) return;
      this.set({ status: { ...s, peripherals: JSON.parse((e as MessageEvent).data) as Peripherals } });
    });
    es.onerror = () => {
      es.close();
      if (this.stopped) return;
      this.set({ connected: false });
      // Never heard from it: maybe it's a node, which has no SSE.
      if (!gotOne && this.snap.kind === "connecting") {
        this.probeNode().then((isNode) => {
          if (this.stopped) return;
          if (isNode) this.startNode();
          else this.timer = setTimeout(() => this.connectSse(), 3000);
        });
        return;
      }
      this.timer = setTimeout(() => this.connectSse(), 3000);
    };
  }

  private onStatus(s: Status) {
    if (!this.seeded && s.history) {
      // The server's history already holds this sample.
      this.seeded = true;
      for (const [k, v] of Object.entries(s.history)) if (Array.isArray(v)) this.hist[k] = v.slice(-HIST_N);
    } else {
      this.seeded = true;
      this.push("cpu", s.system.cpuUsage);
      this.push("mem", s.system.memPercent);
      for (const n of s.network) {
        this.push(`net:${n.name}:rx`, n.rxRate);
        this.push(`net:${n.name}:tx`, n.txRate);
        if (n.wifi) this.push("wifi", n.wifi.signal);
      }
      for (const d of s.drives) {
        this.push(`drv:${d.name}:r`, d.readRate);
        this.push(`drv:${d.name}:w`, d.writeRate);
      }
    }
    this.set({ kind: "linux", connected: true, status: s, tick: this.snap.tick + 1, ...this.versionUpdate(s.buildVersion) });
  }

  private push(key: string, val: number) {
    const h = (this.hist[key] ||= []);
    h.push(val);
    if (h.length > HIST_N) h.shift();
  }

  private versionUpdate(current: string | undefined): Partial<BobSnapshot> {
    if (!current) return {};
    const first = this.snap.firstBuild ?? current;
    const r = buildCheck({ sameOrigin: this.conn.sameOrigin, pageBuild: this.opts.pageBuild, first: this.snap.firstBuild, current });
    return {
      firstBuild: first,
      build: current,
      ...(r === "reload" ? { reloadPending: true } : {}),
      ...(r === "notice" && !this.snap.versionNotice ? { versionNotice: { from: first, to: current } } : {}),
    };
  }

  // ---------- Node: polling ----------

  private async probeNode(): Promise<boolean> {
    try {
      const d = await this.conn.json<NodeInfo>("GET", "/api");
      if (d?.name === "BenchBob node API") {
        this.set({ nodeInfo: d });
        return true;
      }
    } catch {}
    return false;
  }

  private startNode() {
    const poll = async () => {
      if (this.stopped) return;
      if (!this.snap.nodeInfo) await this.probeNode();
      try {
        const n = await this.conn.json<NodeStatus>("GET", "/api/status");
        this.set({ kind: "node", connected: true, node: n, tick: this.snap.tick + 1, ...this.versionUpdate(n.version) });
      } catch {
        this.set({ connected: false });
      }
      if (!this.stopped) this.timer = setTimeout(poll, this.opts.nodePollMs ?? 2000);
    };
    poll();
  }
}
