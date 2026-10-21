// Drives API types (backend/crates/drives): OS detection, the file browser, raw probes.

import type { BobConnection } from "../../lib/connection";
import type { Drive } from "../../lib/types";
import { errText } from "../../ui/status";

export interface EspLoader {
  name: string;
  dir: string;
  kind: string;
  arch?: string;
  files: string[];
  shim?: boolean;
  target?: { partition?: string; system?: string; uuid?: string };
}

export interface WinBootEntry {
  description: string;
  partition?: string;
  path?: string;
  kind: string;
  inMenu?: boolean;
  isDefault?: boolean;
}

export interface OsPartition {
  name: string;
  path?: string;
  role?: string;
  note?: string;
  esp?: {
    loaders: EspLoader[];
    fallback?: { file: string; sameAs?: string };
    windowsBoot?: { timeout?: number; entries: WinBootEntry[] };
    systemdBoot?: { default?: string; entries: { title: string; version?: string; file: string }[] };
  };
  windows?: { product?: string; version?: string; edition?: string; build?: string; installed?: number; computerName?: string };
  linux?: { name: string; hostname?: string };
  pi?: { image?: string; rootPartition?: string; rootPartuuid?: string };
  macos?: { name: string; version?: string };
}

export interface OsInfo {
  ok: true;
  systems: { kind: string; name: string; partition?: string; detail?: string }[];
  partitions: OsPartition[];
}

export interface FileEntry {
  name: string;
  type: "dir" | "file" | "broken" | "outside" | "other";
  size?: number | null;
  mtime?: number;
  link?: string;
}

export interface Listing {
  ok: true;
  path: string;
  entries: FileEntry[];
  total: number;
  truncated?: boolean;
  warning?: string;
}

export interface ProbeResult {
  ok: true;
  device: string;
  sampledBlocks: number;
  bytesRead: number;
  sizeBytes: number;
  emptyPct: number;
  counts: Record<string, number>;
  map: { o: number; k: string; e: number }[];
  unallocated?: { ranges: { bytes: number }[]; sampled: number; withData: number };
  signatures: { name: string; offsets: number[] }[];
}

/** Per-bob cache of /api/osinfo, fetched once per drive per set of mounts. */
export class OsInfoCache {
  private m = new Map<string, { key: string; data: OsInfo | null; error?: string }>();
  private subs = new Set<() => void>();
  private ver = 0;
  constructor(private conn: BobConnection) {}
  subscribe = (f: () => void) => {
    this.subs.add(f);
    return () => this.subs.delete(f);
  };
  version = () => this.ver;
  get(dev: string) {
    return this.m.get(dev)?.data || null;
  }
  /** Why the lookup for `dev` failed, if it did (shown on the drive card). */
  error(dev: string) {
    return this.m.get(dev)?.error || null;
  }
  ensure(d: Drive) {
    if (!d.children?.length) return;
    const key = d.children.map((p) => `${p.name}=${p.fstype || ""}@${p.mountpoint || ""}`).join(",");
    if (this.m.get(d.name)?.key === key) return;
    // A new key can mean a different drive under the same name: never show the old data.
    const entry: { key: string; data: OsInfo | null; error?: string } = { key, data: null };
    this.m.set(d.name, entry);
    const done = () => {
      this.ver++;
      for (const f of this.subs) f();
    };
    this.conn
      .json<OsInfo>("GET", "/api/osinfo?device=" + encodeURIComponent(d.name))
      .then((r) => {
        if (this.m.get(d.name) !== entry) return;
        entry.data = r;
        done();
      })
      .catch((e) => {
        if (this.m.get(d.name) !== entry) return;
        entry.error = errText(e);
        done();
      });
  }
  part(dev: string, part: string) {
    return this.get(dev)?.partitions.find((p) => p.name === part) || null;
  }
  /** The drive/partition whose mount root is `path`, if it has anything to show. */
  filesTarget(path: string): { dev: string; part: OsPartition; disk: OsInfo } | null {
    if (!path || path.includes("/")) return null;
    for (const [dev, c] of this.m) {
      const p = c.data?.partitions.find((x) => x.path === path);
      if (p && (p.esp || p.windows || p.linux || p.pi || p.macos)) return { dev, part: p, disk: c.data! };
    }
    return null;
  }
}

export function partOsLabel(p: OsPartition): string | null {
  if (p.windows) return [p.windows.product, p.windows.version].filter(Boolean).join(" ");
  if (p.linux) return p.linux.name;
  if (p.macos) return [p.macos.name, p.macos.version].filter(Boolean).join(" ");
  if (p.pi) return "Raspberry Pi boot";
  return null;
}

/** Root-relative path of a mountpoint, or null if it isn't browsable. */
export function filesRel(root: string | undefined, mountpoint: string | undefined): string | null {
  if (!root || !mountpoint || !mountpoint.startsWith(root + "/")) return null;
  return mountpoint.slice(root.length + 1);
}
