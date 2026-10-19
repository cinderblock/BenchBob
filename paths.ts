// Where BenchBob keeps what it writes: settings, saved lists, EEPROM history.
//
// A packaged install keeps state apart from the code, in /var/lib/benchbob: systemd's
// StateDirectory= creates it and passes it as $STATE_DIRECTORY. BENCHBOB_STATE_DIR
// overrides that (a side instance, tests). Running straight from a git checkout, the
// default is the checkout itself, which is where these files have always lived.

import { mkdirSync } from "node:fs";

export const STATE_DIR = process.env.BENCHBOB_STATE_DIR || process.env.STATE_DIRECTORY?.split(":")[0] || import.meta.dir;
mkdirSync(STATE_DIR, { recursive: true });

export const statePath = (name: string) => `${STATE_DIR}/${name}`;
