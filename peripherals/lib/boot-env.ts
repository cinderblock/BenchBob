// ==================== Boot-time overlays outside the Raspberry Pi ====================
//
// Pure reading and editing (unit-tested) of the two places other SBCs list the device-tree
// overlays U-Boot applies at boot:
//
//  - "env": Armbian's /boot/armbianEnv.txt and Orange Pi's /boot/orangepiEnv.txt, a U-Boot
//    environment file of key=value lines. `overlays=` is a space-separated list of short
//    names; the boot script loads <overlay dir>/<overlay_prefix>-<name>.dtbo for each.
//    `user_overlays=` names .dtbo files the user compiled (left alone here). U-Boot's
//    `env import -t` lets a later line override an earlier one, so the last `overlays=` is
//    the one that counts and the one we edit.
//  - "extlinux": extlinux.conf (BeagleBoard.org's K3 images, /boot/firmware/extlinux/),
//    where the default label's `fdtoverlays` line lists .dtbo paths. Only that label is
//    edited; other labels (a recovery entry, say) are left as they are.
//
// Same contract as the Pi's config.txt editor: the caller shows a unified diff
// (gpio-config.ts unifiedDiff), backs the file up and writes only after a confirm; line
// endings and every line we don't manage are kept byte for byte.

export type EnvKind = "env" | "extlinux";

export interface Overlays {
  /** Enabled overlays as written: short names (env) or .dtbo paths (extlinux). */
  enabled: string[];
  /** env: overlay_prefix (e.g. "rockchip-rk3588", "sun50i-h616"); extlinux: null. */
  prefix: string | null;
  /** env: user_overlays=, shown but never edited. */
  user: string[];
  /** extlinux: the label whose fdtoverlays line we read and edit. */
  label: string | null;
}

const split = (text: string) => text.replace(/\r\n/g, "\n").split("\n");
const join = (lines: string[], text: string) => lines.join("\n").replace(/\n/g, text.includes("\r\n") ? "\r\n" : "\n");
const words = (s: string) => s.trim().split(/\s+/).filter(Boolean);

// ---------- env (armbianEnv.txt / orangepiEnv.txt) ----------

function envLines(lines: string[], key: string): number[] {
  const re = new RegExp(`^\\s*${key}\\s*=`);
  return lines.map((l, i) => (re.test(l) ? i : -1)).filter((i) => i >= 0);
}
const envValue = (l: string) => l.slice(l.indexOf("=") + 1);

function readEnv(text: string): Overlays {
  const lines = split(text);
  const last = (k: string) => {
    const hits = envLines(lines, k);
    return hits.length ? envValue(lines[hits[hits.length - 1]]) : null;
  };
  return { enabled: words(last("overlays") ?? ""), prefix: last("overlay_prefix")?.trim() || null, user: words(last("user_overlays") ?? ""), label: null };
}

function editEnv(text: string, enabled: string[]): string {
  const lines = split(text);
  const hits = envLines(lines, "overlays");
  const line = `overlays=${enabled.join(" ")}`;
  if (hits.length) {
    if (!enabled.length) lines.splice(hits[hits.length - 1], 1);
    else lines[hits[hits.length - 1]] = line;
  } else if (enabled.length) {
    // Before the file's trailing blank line(s), so the file still ends the way it did.
    let at = lines.length;
    while (at > 0 && lines[at - 1].trim() === "") at--;
    lines.splice(at, 0, line);
  }
  return join(lines, text);
}

// ---------- extlinux.conf ----------

interface Block {
  label: string;
  start: number; // the `label` line
  end: number; // exclusive
}

function blocks(lines: string[]): { blocks: Block[]; def: string | null } {
  const out: Block[] = [];
  let def: string | null = null;
  lines.forEach((l, i) => {
    const m = /^\s*(\w+)\s*(.*?)\s*$/.exec(l);
    if (!m) return;
    const k = m[1].toLowerCase();
    if (k === "default" && !out.length) def = m[2];
    if (k === "label") {
      if (out.length) out[out.length - 1].end = i;
      out.push({ label: m[2], start: i, end: lines.length });
    }
  });
  return { blocks: out, def };
}

/** The label U-Boot boots without input: `default`'s, else the first. */
function bootBlock(lines: string[]): Block | null {
  const { blocks: b, def } = blocks(lines);
  return b.find((x) => x.label === def) || b[0] || null;
}

const keyLine = (lines: string[], b: Block, key: string) => {
  for (let i = b.start + 1; i < b.end; i++) if (new RegExp(`^\\s*${key}\\b`, "i").test(lines[i])) return i;
  return -1;
};

function readExtlinux(text: string): Overlays {
  const lines = split(text);
  const b = bootBlock(lines);
  if (!b) return { enabled: [], prefix: null, user: [], label: null };
  const i = keyLine(lines, b, "fdtoverlays");
  return { enabled: i >= 0 ? words(lines[i].replace(/^\s*fdtoverlays\b/i, "")) : [], prefix: null, user: [], label: b.label };
}

function editExtlinux(text: string, enabled: string[]): string {
  const lines = split(text);
  const b = bootBlock(lines);
  if (!b) throw new Error("extlinux.conf has no label to add overlays to");
  const i = keyLine(lines, b, "fdtoverlays");
  if (i >= 0) {
    if (!enabled.length) lines.splice(i, 1);
    else lines[i] = `${/^\s*/.exec(lines[i])![0]}fdtoverlays ${enabled.join(" ")}`;
    return join(lines, text);
  }
  if (!enabled.length) return text;
  // After the fdt/fdtdir line, indented like it (the docs' example layout).
  const after = Math.max(keyLine(lines, b, "fdt"), keyLine(lines, b, "fdtdir"), keyLine(lines, b, "linux"), keyLine(lines, b, "kernel"));
  const ref = after >= 0 ? after : b.start;
  const indent = after >= 0 ? /^\s*/.exec(lines[after])![0] : "    ";
  lines.splice(ref + 1, 0, `${indent}fdtoverlays ${enabled.join(" ")}`);
  return join(lines, text);
}

// ---------- shared ----------

export function readOverlays(kind: EnvKind, text: string): Overlays {
  return kind === "env" ? readEnv(text) : readExtlinux(text);
}

/** Turn overlays on/off; anything not mentioned keeps its place in the list. */
export function editOverlays(kind: EnvKind, text: string, change: { enable?: string[]; disable?: string[] }): string {
  const bad = [...(change.enable || []), ...(change.disable || [])].find((n) => !/^[\w.,/+-]+$/.test(n));
  if (bad != null) throw new Error(`Not an overlay name: "${bad}"`);
  const cur = readOverlays(kind, text).enabled;
  const off = new Set(change.disable || []);
  const next = cur.filter((n) => !off.has(n));
  for (const n of change.enable || []) if (!next.includes(n)) next.push(n);
  if (next.join(" ") === cur.join(" ")) return text;
  return kind === "env" ? editEnv(text, next) : editExtlinux(text, next);
}
