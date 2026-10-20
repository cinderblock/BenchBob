#!/usr/bin/env bun
// Write a config.uf2 for a BenchBob node on a Pico 2 W: Wi-Fi credentials and (optionally)
// a hostname, written only to the reserved config region. Flash the firmware UF2 first,
// then hold BOOTSEL, plug the board in, and copy this onto the RP2350 drive.
//
//   bun firmware/tools/config-uf2.ts --ssid "Bench Lab" --password "…" [--hostname bench-1] [-o config.uf2]
//   bun firmware/tools/config-uf2.ts --check config.uf2
//
// The file holds the Wi-Fi password in plain text: treat it like the password.

import { parseArgs } from "node:util";
import { configUf2, decodeConfigUf2, PICO2W_CONFIG_ADDR } from "./nodeconfig.ts";

const { values } = parseArgs({
  options: {
    ssid: { type: "string" },
    password: { type: "string", default: "" },
    hostname: { type: "string", default: "" },
    output: { type: "string", short: "o", default: "config.uf2" },
    check: { type: "string" },
    help: { type: "boolean", short: "h" },
  },
});

if (values.help || (!values.ssid && !values.check)) {
  console.log(`usage: bun config-uf2.ts --ssid <name> [--password <pw>] [--hostname <name>] [-o config.uf2]
       bun config-uf2.ts --check <config.uf2>`);
  process.exit(values.help ? 0 : 2);
}

try {
  if (values.check) {
    const c = decodeConfigUf2(new Uint8Array(await Bun.file(values.check).arrayBuffer()));
    console.log(`${values.check}: SSID "${c.ssid}", password ${c.password ? `${c.password.length} chars` : "none (open network)"}, hostname ${c.hostname || "default"}, sequence ${c.seq}`);
  } else {
    const uf2 = configUf2({ ssid: values.ssid!, password: values.password, hostname: values.hostname });
    await Bun.write(values.output!, uf2);
    console.log(`${values.output}: config for "${values.ssid}" at 0x${PICO2W_CONFIG_ADDR.toString(16)} (${uf2.length / 512} blocks)`);
  }
} catch (e: any) {
  console.error(e.message);
  process.exit(1);
}
