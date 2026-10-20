// ==================== x86 PC ====================
//
// Mini PCs, NUCs, laptops, servers: named from DMI (/sys/class/dmi/id), no pin header.
// GPIO is whatever gpiochips the kernel has, which on a PC is usually just a USB adapter
// (FT232H, CH341, MCP2221) or the chipset's own lines. Health is generic: thermal zones and
// hwmon (coretemp, k10temp, acpitz…) and power supplies (a laptop battery, a UPS).

import type { BoardDef, DetectInput } from "./types.ts";

// Placeholder strings firmware vendors leave in DMI fields.
const JUNK = /^(to be filled by o\.?e\.?m\.?|default string|system (product name|manufacturer|version)|not applicable|none|n\/a|o\.?e\.?m\.?|x\.x|0123456789|\s*)$/i;
const clean = (s: string) => (JUNK.test(s.trim()) ? "" : s.trim());

/** "ASUSTeK COMPUTER INC. PN51" from DMI, falling back to the board fields, "" if all are placeholders. */
export function dmiName(i: DetectInput): string {
  const { sysVendor, productName, boardVendor, boardName } = i.dmi;
  const vendor = clean(sysVendor) || clean(boardVendor);
  const product = clean(productName) || clean(boardName);
  // Some vendors repeat themselves ("Intel Corporation" / "Intel NUC…"); don't say it twice.
  if (vendor && product && product.toLowerCase().startsWith(vendor.split(/\s/)[0].toLowerCase())) return product;
  return [vendor, product].filter(Boolean).join(" ");
}

const x86: BoardDef = {
  id: "x86",
  name: "x86-64 PC",
  vendor: "",
  tested: true,
  match: (i) => (i.arch === "x64" || i.arch === "ia32" ? 10 : 0),
  label: (i) => (i.wsl ? "Windows Subsystem for Linux" : dmiName(i) || "x86-64 PC"),
  headers: [],
  gpio: "gpiochip",
  bootConfig: null,
  health: "sysfs",
  sources: [],
};

export default x86;
