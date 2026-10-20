// ==================== Generic Linux ====================
//
// The fallback for any machine no other board file claims: an unknown ARM board, a VM, a
// RISC-V box. Everything BenchBob does through sysfs, character devices and USB works
// here; what's missing is a header map, so the GPIO panel lists the kernel's gpiochips and
// lines (names, owners, flags) instead of physical pins, with levels for lines the user
// chooses to watch. No boot-config editing: we don't know where this board keeps it.

import type { BoardDef } from "./types.ts";

const generic: BoardDef = {
  id: "generic",
  name: "Linux computer",
  vendor: "",
  tested: true,
  match: () => 1,
  headers: [],
  gpio: "gpiochip",
  bootConfig: null,
  health: "sysfs",
  sources: [],
};

export default generic;
