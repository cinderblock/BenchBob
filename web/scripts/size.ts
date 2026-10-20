// Size report and budget for both builds: `bun scripts/size.ts` after `bun run build`.
//
// The MCU build is served from a node's flash (4 MB, shared with the firmware), so the
// whole of it, every lazy chunk included, has a budget. The full build is served by a Linux
// bob, so only what loads before any panel does (the entry script and CSS) is budgeted;
// the rest is reported.
import { gzipSync } from "node:zlib";
import { readdirSync, readFileSync, statSync, existsSync } from "node:fs";
import { join, relative } from "node:path";

export const BUDGET = {
  /** Everything in dist-mcu/, gzipped. */
  mcuTotalGz: 60 * 1024,
  /** dist/index.html + entry JS + entry CSS, gzipped. */
  fullInitialGz: 50 * 1024,
};

export interface Size {
  file: string;
  raw: number;
  gz: number;
}

function walk(dir: string): string[] {
  return readdirSync(dir).flatMap((f) => {
    const p = join(dir, f);
    return statSync(p).isDirectory() ? walk(p) : [p];
  });
}

export function sizes(dir: string): Size[] {
  return walk(dir)
    .filter((f) => !f.endsWith(".gz"))
    .map((f) => {
      const b = readFileSync(f);
      return { file: relative(dir, f).replaceAll("\\", "/"), raw: b.length, gz: gzipSync(b, { level: 9 }).length };
    })
    .sort((a, b) => b.gz - a.gz);
}

/** The entry: index.html plus the files it references directly. */
export function initial(dir: string, all: Size[]): Size[] {
  const html = readFileSync(join(dir, "index.html"), "utf8");
  return all.filter((s) => s.file === "index.html" || html.includes(s.file));
}

const sum = (l: Size[], k: "raw" | "gz") => l.reduce((a, s) => a + s[k], 0);
const kb = (n: number) => (n / 1024).toFixed(1) + " KB";

if (import.meta.main) {
  const web = join(import.meta.dir, "..");
  let fail = false;
  for (const [name, dir] of [
    ["full", join(web, "dist")],
    ["mcu", join(web, "dist-mcu")],
  ] as const) {
    if (!existsSync(dir)) {
      console.log(`${name}: ${dir} missing (run the build first)`);
      fail = true;
      continue;
    }
    const all = sizes(dir);
    const init = initial(dir, all);
    console.log(`\n${name} build (${relative(web, dir)}): ${all.length} files, ${kb(sum(all, "raw"))} raw, ${kb(sum(all, "gz"))} gzipped; initial load ${kb(sum(init, "raw"))} raw, ${kb(sum(init, "gz"))} gzipped`);
    for (const s of all.slice(0, 8)) console.log(`  ${s.file.padEnd(40)} ${kb(s.raw).padStart(10)} ${kb(s.gz).padStart(10)} gz`);
    if (name === "mcu" && sum(all, "gz") > BUDGET.mcuTotalGz) {
      console.log(`  OVER BUDGET: ${kb(sum(all, "gz"))} gzipped > ${kb(BUDGET.mcuTotalGz)}`);
      fail = true;
    }
    if (name === "full" && sum(init, "gz") > BUDGET.fullInitialGz) {
      console.log(`  OVER BUDGET: initial ${kb(sum(init, "gz"))} gzipped > ${kb(BUDGET.fullInitialGz)}`);
      fail = true;
    }
  }
  if (fail) process.exit(1);
  console.log(`\nWithin budget (MCU total ≤ ${kb(BUDGET.mcuTotalGz)} gz, full initial ≤ ${kb(BUDGET.fullInitialGz)} gz).`);
}
