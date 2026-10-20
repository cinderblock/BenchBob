// ==================== Board registry ====================
//
// Picks the board BenchBob runs on, once, from what the kernel says about the machine:
// /proc/device-tree/compatible and model on ARM, DMI on x86. detect() is a pure function
// over those strings (unit-tested with fixtures from every board file); board() reads them
// and caches the answer. BENCHBOB_BOARD=<id> forces a board, to try a board's UI on a
// machine that isn't one (its header then shows no live lines, since the chips don't match).
//
// The list is explicit rather than globbed: the order is part of the contract (more
// specific boards first, generic last), and a board file is data a reviewer should see
// land in one place.

import { existsSync, readFileSync } from "node:fs";
import generic from "./generic.ts";
import orangepi from "./orangepi.ts";
import rpi from "./rpi.ts";
import type { Board, BoardDef, BootConfigKind, DetectInput, Header } from "./types.ts";
import x86 from "./x86.ts";

export type { Board, BoardDef, DetectInput, Header, HeaderPin } from "./types.ts";

export const BOARDS: BoardDef[] = [rpi, ...orangepi, x86, generic];

/** The best-matching board for this input (generic if nothing else claims it). */
export function detect(i: DetectInput): BoardDef {
  let best = generic, score = 0;
  for (const b of BOARDS) {
    const s = b.match(i);
    if (s > score) [best, score] = [b, s];
  }
  return best;
}

export function describe(def: BoardDef, i: DetectInput, forced = false): Board {
  const model = i.model || [i.dmi.sysVendor, i.dmi.productName].map((s) => s.trim()).filter(Boolean).join(" ");
  return {
    def,
    id: def.id,
    // A forced board isn't the machine: name it after the board file, not the host's model.
    name: forced ? def.name : def.label?.(i) || i.model || def.name,
    model,
    tested: def.isTested ? def.isTested(i) : def.tested,
    forced,
    input: i,
  };
}

// ---------- reading the machine ----------

const rd = (p: string) => {
  try {
    return readFileSync(p, "latin1");
  } catch {
    return "";
  }
};

export function readDetectInput(): DetectInput {
  const dmi = (f: string) => rd(`/sys/class/dmi/id/${f}`).trim();
  return {
    arch: process.arch,
    compatible: rd("/proc/device-tree/compatible").split("\0").filter(Boolean),
    model: rd("/proc/device-tree/model").replace(/\0/g, "").trim(),
    dmi: { sysVendor: dmi("sys_vendor"), productName: dmi("product_name"), productVersion: dmi("product_version"), boardVendor: dmi("board_vendor"), boardName: dmi("board_name") },
    wsl: /microsoft/i.test(rd("/proc/sys/kernel/osrelease")),
  };
}

let current: Board | null = null;

export function board(): Board {
  if (current) return current;
  const i = readDetectInput();
  const want = process.env.BENCHBOB_BOARD;
  const forced = want ? BOARDS.find((b) => b.id === want) : undefined;
  if (want && !forced) console.warn(`[boards] BENCHBOB_BOARD=${want} isn't a known board (${BOARDS.map((b) => b.id).join(", ")}); detecting instead`);
  current = describe(forced || detect(i), i, !!forced);
  console.log(`[boards] ${current.name} → ${current.id}${current.forced ? " (forced)" : ""}${current.tested ? "" : " (untested board)"}`);
  return current;
}

let infoCache: { b: Board; info: ReturnType<typeof makeInfo> } | null = null;

/** What the API and dashboards get: identity, plus what the board can do. */
export function boardInfo(b: Board = board()) {
  if (infoCache?.b !== b) infoCache = { b, info: makeInfo(b) };
  return infoCache.info;
}

function makeInfo(b: Board) {
  const d = b.def;
  return {
    id: b.id,
    name: b.name,
    model: b.model,
    tested: b.tested,
    forced: b.forced,
    vendor: d.vendor,
    soc: d.socOf?.(b.input) ?? d.soc ?? null,
    capabilities: {
      headers: d.headers.map((h: Header) => ({ id: h.id, name: h.name, pins: h.pins.length })),
      gpio: d.gpio,
      bootConfig: bootConfigOf(d),
      health: d.health,
    },
    sources: d.sources,
    notes: d.notes || [],
  };
}

/**
 * The boot-config file this machine actually has: the first of the board's candidates that
 * exists (Armbian's or the vendor's), or null if none does. config.txt is taken as-is.
 */
export function bootConfigOf(d: BoardDef, exists: (p: string) => boolean = existsSync): { kind: BootConfigKind; path: string } | null {
  const hit = d.bootConfig?.files.find((f) => f.kind === "config.txt" || exists(f.path));
  return hit ? { kind: hit.kind, path: hit.path } : null;
}

// ---------- gpiochips ↔ header pins ----------

/** Board chip keys → the gpiochip (by uAPI label or sysfs device name) that is that controller here. */
export function resolveChips<C extends { label: string; device: string }>(def: BoardDef, chips: C[]): Map<string, C> {
  const out = new Map<string, C>();
  for (const [key, ids] of Object.entries(def.chips || {})) {
    const want = ids.map((s) => s.toLowerCase());
    const hit = chips.find((c) => want.includes(c.label.toLowerCase()) || want.includes(c.device.toLowerCase()));
    if (hit) out.set(key, hit);
  }
  return out;
}
