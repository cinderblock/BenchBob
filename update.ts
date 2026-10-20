// Release updates: notice a newer BenchBob on GitHub, and install it through apt.
//
// The check asks GitHub's API for the latest release every 6 hours (and on demand); a
// checkout ("dev") never checks. Installing hands off to benchbob-update, a root script
// started with systemd-run as its own unit: the upgrade's postinst restarts this service,
// which would kill an apt run that lived in our cgroup halfway through dpkg.

import { statePath, VERSION } from "./paths.ts";

const REPO = "cinderblock/BenchBob";
const CHECK_EVERY_MS = 6 * 3600_000;
const SETTINGS_PATH = statePath("update.json");
const LOG = "/var/log/benchbob-update.log";
const UNIT = "benchbob-update";

/**
 * Compare Debian-style versions as this project makes them: x.y.z, optionally
 * `~git<n>.<sha>` (before x.y.z) or `+git<n>.<sha>` (after it). Negative if a < b.
 */
export function compareVersions(a: string, b: string): number {
  const parse = (v: string) => {
    const m = /^v?(\d+)\.(\d+)\.(\d+)(?:([~+])git(\d+))?/.exec(v.trim());
    if (!m) return null;
    // ~ sorts before the release, + after it; within either, by commit count.
    const rank = m[4] === "~" ? -1 : m[4] === "+" ? 1 : 0;
    return [+m[1], +m[2], +m[3], rank, m[5] ? +m[5] : 0];
  };
  const pa = parse(a), pb = parse(b);
  if (!pa || !pb) return 0;
  for (let i = 0; i < pa.length; i++) if (pa[i] !== pb[i]) return pa[i] - pb[i];
  return 0;
}

interface State {
  check: boolean;
  latest: string | null;
  url: string | null;
  notes: string;
  checkedAt: number;
  error: string;
}

let state: State = { check: true, latest: null, url: null, notes: "", checkedAt: 0, error: "" };
let timer: ReturnType<typeof setInterval> | null = null;

async function loadSettings() {
  try {
    const d = await Bun.file(SETTINGS_PATH).json();
    if (typeof d.check === "boolean") state.check = d.check;
  } catch {}
}

export async function check(): Promise<void> {
  if (VERSION === "dev") return;
  try {
    const r = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, {
      headers: { Accept: "application/vnd.github+json", "User-Agent": `BenchBob/${VERSION}` },
      signal: AbortSignal.timeout(15_000),
    });
    if (!r.ok) throw new Error(`GitHub answered ${r.status}`);
    const j: any = await r.json();
    state = { ...state, latest: String(j.tag_name || "").replace(/^v/, "") || null, url: j.html_url || null, notes: String(j.body || "").slice(0, 4000), error: "" };
  } catch (e: any) {
    state.error = e.message;
  }
  state.checkedAt = Date.now();
}

export async function start(): Promise<void> {
  await loadSettings();
  if (VERSION === "dev" || timer) return;
  // Not at startup itself: a boot loop shouldn't hammer the API.
  setTimeout(() => state.check && check(), 60_000);
  timer = setInterval(() => state.check && check(), CHECK_EVERY_MS);
}

async function installing(): Promise<boolean> {
  const p = Bun.spawn(["systemctl", "is-active", "--quiet", `${UNIT}.service`], { stdout: "ignore", stderr: "ignore" });
  return (await p.exited) === 0;
}

async function logTail(): Promise<string> {
  try {
    return (await Bun.file(LOG).text()).split("\n").slice(-40).join("\n");
  } catch {
    return "";
  }
}

/** For /api/status: small enough to send every 2 s. */
export function summary() {
  const available = !!state.latest && VERSION !== "dev" && compareVersions(state.latest, VERSION) > 0;
  return { current: VERSION, latest: state.latest, available, url: state.url, checkedAt: state.checkedAt, error: state.error, check: state.check };
}

const json = (body: unknown, status = 200) => Response.json(body, { status });

/** Routes under /api/update; null when the path isn't ours. */
export async function route(req: Request, p: string): Promise<Response | null> {
  if (p === "/api/update" && req.method === "GET") {
    return json({ ...summary(), notes: state.notes, installing: await installing(), log: await logTail() });
  }
  if (p === "/api/update/check" && req.method === "POST") {
    if (VERSION === "dev") return json({ ok: false, message: "Running from a checkout: update with git, not the package" }, 409);
    await check();
    return json({ ok: true, ...summary() });
  }
  if (p === "/api/update/settings" && req.method === "PUT") {
    const b: any = await req.json().catch(() => ({}));
    if (typeof b.check !== "boolean") return json({ ok: false, message: "body: { check: true|false }" }, 400);
    state.check = b.check;
    await Bun.write(SETTINGS_PATH, JSON.stringify({ check: state.check }, null, 2));
    return json({ ok: true, check: state.check });
  }
  if (p === "/api/update/install" && req.method === "POST") {
    const b: any = await req.json().catch(() => ({}));
    if (b.confirm !== true) return json({ ok: false, message: "Installing restarts BenchBob: send { confirm: true }" }, 400);
    if (VERSION === "dev") return json({ ok: false, message: "Running from a checkout: update with git, not the package" }, 409);
    if (await installing()) return json({ ok: false, message: "An update is already running" }, 409);
    const r = Bun.spawn(["sudo", "-n", "systemd-run", `--unit=${UNIT}`, "--collect", "--quiet", "/usr/lib/benchbob/system/benchbob-update"], { stdout: "pipe", stderr: "pipe" });
    const [err, code] = await Promise.all([new Response(r.stderr).text(), r.exited]);
    if (code) return json({ ok: false, message: `Couldn't start the update: ${err.trim() || `exit ${code}`}` }, 500);
    return json({ ok: true, message: "Updating: BenchBob restarts when it's done" });
  }
  return null;
}

export const DOCS = {
  "GET /api/update": "Installed and latest release, whether an update is available, release notes, and the log of the last update run",
  "POST /api/update/check": "Ask GitHub for the latest release now (also happens every 6 hours unless turned off)",
  "PUT /api/update/settings": { body: { check: true }, description: "Turn the periodic release check on or off" },
  "POST /api/update/install": { body: { confirm: true }, description: "Install the latest release with apt (falls back to the GitHub download). BenchBob restarts when it's done" },
};
