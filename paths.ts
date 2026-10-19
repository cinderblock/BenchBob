// Where BenchBob keeps what it writes (settings, saved lists, EEPROM history), and the
// environment it reads that from.
//
// A packaged install keeps state apart from the code, in /var/lib/benchbob: systemd's
// StateDirectory= creates it and passes it as $STATE_DIRECTORY. BENCHBOB_STATE_DIR
// overrides that (a side instance, tests). Running straight from a git checkout, the
// default is the checkout itself, which is where these files have always lived.

import { mkdirSync, readFileSync } from "node:fs";

// BenchBob was DeskyPi: honour the old DESKYPI_* variables (port offset, simulators, test
// paths) when the BENCHBOB_* one isn't set. Drivers load after this module, so they see
// only the new names.
for (const [k, v] of Object.entries(process.env)) {
  if (k.startsWith("DESKYPI_")) process.env["BENCHBOB_" + k.slice(8)] ??= v;
}

export const STATE_DIR = process.env.BENCHBOB_STATE_DIR || process.env.STATE_DIRECTORY?.split(":")[0] || import.meta.dir;
mkdirSync(STATE_DIR, { recursive: true });

export const statePath = (name: string) => `${STATE_DIR}/${name}`;

/** The release version: packaging writes a VERSION file next to the code. A checkout says "dev". */
export const VERSION = (() => {
  try {
    return readFileSync(`${import.meta.dir}/VERSION`, "utf8").trim() || "dev";
  } catch {
    return "dev";
  }
})();
