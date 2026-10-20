import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { cpuTemp, readSensors } from "./health.ts";

// Fake /sys/class trees: desky's (Pi 4) and a typical Intel mini PC with a UPS.
const roots: string[] = [];
function tree(files: Record<string, string>) {
  const root = mkdtempSync(join(tmpdir(), "bb-sys-"));
  roots.push(root);
  for (const [p, v] of Object.entries(files)) {
    mkdirSync(join(root, p, ".."), { recursive: true });
    writeFileSync(join(root, p), v + "\n");
  }
  return root;
}
afterAll(() => roots.forEach((r) => rmSync(r, { recursive: true, force: true })));

describe("sysfs health", () => {
  test("desky: thermal zone and its hwmon twin are one reading", () => {
    const s = readSensors(tree({
      "thermal/thermal_zone0/type": "cpu-thermal",
      "thermal/thermal_zone0/temp": "44790",
      "hwmon/hwmon0/name": "cpu_thermal",
      "hwmon/hwmon0/temp1_input": "44790",
      "hwmon/hwmon1/name": "rpi_volt",
      "hwmon/hwmon1/in0_lcrit_alarm": "0",
    }));
    expect(s.temps).toEqual([{ source: "thermal_zone0", label: "cpu-thermal", c: 44.8 }]);
    expect(s.volts).toEqual([]);
    expect(cpuTemp(s)).toBe(44.8);
  });

  test("Intel PC: coretemp package wins over acpitz; voltages, fans and a UPS", () => {
    const s = readSensors(tree({
      "thermal/thermal_zone0/type": "acpitz",
      "thermal/thermal_zone0/temp": "27800",
      "thermal/thermal_zone1/type": "x86_pkg_temp",
      "thermal/thermal_zone1/temp": "51000",
      "hwmon/hwmon2/name": "coretemp",
      "hwmon/hwmon2/temp2_input": "48000",
      "hwmon/hwmon2/temp2_label": "Core 0",
      "hwmon/hwmon2/temp1_input": "52000",
      "hwmon/hwmon2/temp1_label": "Package id 0",
      "hwmon/hwmon3/name": "nct6798",
      "hwmon/hwmon3/in0_input": "1032",
      "hwmon/hwmon3/fan2_input": "812",
      "power_supply/ups/uevent": "POWER_SUPPLY_NAME=ups\nPOWER_SUPPLY_TYPE=UPS\nPOWER_SUPPLY_STATUS=Discharging\nPOWER_SUPPLY_CAPACITY=87\nPOWER_SUPPLY_VOLTAGE_NOW=13410000",
      "power_supply/AC/uevent": "POWER_SUPPLY_TYPE=Mains\nPOWER_SUPPLY_ONLINE=0",
    }));
    expect(cpuTemp(s)).toBe(52);
    expect(s.volts).toEqual([{ source: "nct6798", label: "in0", v: 1.032 }]);
    expect(s.fans).toEqual([{ source: "nct6798", label: "fan2", rpm: 812 }]);
    expect(s.supplies).toEqual([
      { name: "AC", type: "Mains", online: false, status: null, capacity: null, volts: null },
      { name: "ups", type: "UPS", online: null, status: "Discharging", capacity: 87, volts: 13.41 },
    ]);
  });

  test("RK3588: SoC zone picked by name; nothing at all → null", () => {
    const s = readSensors(tree({
      "thermal/thermal_zone0/type": "soc-thermal",
      "thermal/thermal_zone0/temp": "40000",
      "thermal/thermal_zone1/type": "bigcore0-thermal",
      "thermal/thermal_zone1/temp": "41000",
    }));
    expect(cpuTemp(s)).toBe(40);
    expect(cpuTemp(readSensors(tree({})))).toBeNull();
  });
});
