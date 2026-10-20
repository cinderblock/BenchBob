// ==================== Host health from sysfs ====================
//
// Temperatures, voltages, fans and power supplies for any Linux box, from the kernel's
// generic classes: thermal zones, hwmon (coretemp, k10temp, nct67xx, the SoC's own sensors)
// and power_supply (laptop batteries, USB-C PD, some UPSes and SBC PMICs). The Raspberry
// Pi's firmware-only readings (throttling, the PMIC ADC) stay in boards/rpi.ts.
//
// Only small sysfs files are read, so this is cheap enough for the 2 s status tick. The
// root is a parameter so the tests can point it at a fake tree.

import { readdirSync, readFileSync } from "node:fs";

export interface Sensors {
  temps: { source: string; label: string; c: number }[];
  volts: { source: string; label: string; v: number }[];
  fans: { source: string; label: string; rpm: number }[];
  supplies: { name: string; type: string; online: boolean | null; status: string | null; capacity: number | null; volts: number | null }[];
}

const rd = (p: string) => {
  try {
    return readFileSync(p, "utf8").trim();
  } catch {
    return "";
  }
};
const ls = (p: string) => {
  try {
    return readdirSync(p).sort((a, b) => a.localeCompare(b, undefined, { numeric: true }));
  } catch {
    return [] as string[];
  }
};
const num = (s: string) => (s === "" || isNaN(Number(s)) ? null : Number(s));
const norm = (s: string) => s.toLowerCase().replace(/[-_\s]/g, "");

export function readSensors(root = "/sys/class"): Sensors {
  const s: Sensors = { temps: [], volts: [], fans: [], supplies: [] };
  const zoneTypes = new Set<string>();
  for (const z of ls(`${root}/thermal`)) {
    if (!z.startsWith("thermal_zone")) continue;
    const type = rd(`${root}/thermal/${z}/type`);
    const t = num(rd(`${root}/thermal/${z}/temp`));
    if (t == null) continue;
    zoneTypes.add(norm(type));
    s.temps.push({ source: z, label: type || z, c: Math.round(t / 100) / 10 });
  }
  for (const h of ls(`${root}/hwmon`)) {
    const dir = `${root}/hwmon/${h}`;
    const name = rd(`${dir}/name`) || h;
    const files = ls(dir);
    for (const f of files) {
      const m = /^(temp|in|fan)(\d+)_input$/.exec(f);
      if (!m) continue;
      const v = num(rd(`${dir}/${f}`));
      if (v == null) continue;
      const label = rd(`${dir}/${m[1]}${m[2]}_label`) || `${m[1]}${m[2]}`;
      // A thermal zone that is also registered as hwmon (cpu-thermal / cpu_thermal) is listed once.
      if (m[1] === "temp" && zoneTypes.has(norm(name))) continue;
      if (m[1] === "temp") s.temps.push({ source: name, label, c: Math.round(v / 100) / 10 });
      else if (m[1] === "in") s.volts.push({ source: name, label, v: Math.round(v) / 1000 });
      else s.fans.push({ source: name, label, rpm: v });
    }
  }
  for (const p of ls(`${root}/power_supply`)) {
    const u: Record<string, string> = {};
    for (const l of rd(`${root}/power_supply/${p}/uevent`).split("\n")) {
      const m = /^POWER_SUPPLY_([A-Z_]+)=(.*)$/.exec(l);
      if (m) u[m[1]] = m[2];
    }
    const uv = num(u.VOLTAGE_NOW ?? "");
    s.supplies.push({ name: p, type: u.TYPE || "", online: u.ONLINE != null ? u.ONLINE === "1" : null, status: u.STATUS || null, capacity: num(u.CAPACITY ?? ""), volts: uv == null ? null : Math.round(uv / 10000) / 100 });
  }
  return s;
}

// Package-level CPU sensors first, then the SoC's zone, then anything.
const CPU_HWMON = [/^coretemp$/, /^k10temp$/, /^zenpower$/, /^cpu.?thermal$/, /^soc.?thermal$/];
const CPU_LABEL = /^(package id 0|tctl|tdie|cpu)/i;
const CPU_ZONE = /x86_pkg_temp|cpu|soc|package|bigcore|littlecore/i;

/** The temperature to show as "CPU", or null if the box has no sensors at all (WSL, most VMs). */
export function cpuTemp(s: Sensors): number | null {
  for (const re of CPU_HWMON) {
    const ts = s.temps.filter((t) => re.test(t.source));
    if (ts.length) return (ts.find((t) => CPU_LABEL.test(t.label)) || ts[0]).c;
  }
  return (s.temps.find((t) => t.source.startsWith("thermal_zone") && CPU_ZONE.test(t.label)) || s.temps[0])?.c ?? null;
}
