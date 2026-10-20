// ==================== Raspberry Pi ====================
//
// Pi 3 / 4 / 5, 400 / 500, CM3 / CM4 / CM5 and Zero 2 W on a 64-bit OS. The Pi keeps the
// path BenchBob grew up on: pinctrl for function/pull, /dev/gpiomem for 10 Hz levels on
// BCM283x/2711 (lib/gpiomem.ts; Pi 5's RP1 falls back to pinctrl every tick), config.txt
// for the header buses (lib/gpio-config.ts), and the firmware's throttling flags and PMIC
// through vcgencmd for host power.
//
// Tested on a Pi 4 (desky). The other models run the same code, but nobody here has a Pi 5
// or Pi 3 to try it on, so they are reported as untested.
//
// Header: the standard 40-pin layout (lib/gpio-config.ts HEADER), from
// https://www.raspberrypi.com/documentation/computers/raspberry-pi.html#gpio

import { run } from "../core.ts";
import { HEADER } from "../lib/gpio-config.ts";
import { hasCompat, type BoardDef } from "./types.ts";

const PI = /^(raspberrypi,|brcm,bcm(2835|2836|2837|2710|2711|2712))/;

const rpi: BoardDef = {
  id: "rpi",
  name: "Raspberry Pi",
  vendor: "Raspberry Pi",
  tested: true,
  match: (i) => (hasCompat(i, PI) ? 100 : 0),
  isTested: (i) => hasCompat(i, /^brcm,bcm2711$/),
  socOf: (i) => i.compatible.find((c) => /^brcm,bcm\d+$/.test(c))?.slice(5).toUpperCase(),
  headers: [{ id: "j8", name: "40-pin header", pins: HEADER }],
  gpio: "rpi",
  bootConfig: { files: [{ kind: "config.txt", path: "/boot/firmware/config.txt" }] },
  health: "vcgencmd",
  sources: ["https://www.raspberrypi.com/documentation/computers/raspberry-pi.html#gpio", "https://www.raspberrypi.com/documentation/computers/config_txt.html"],
};

export default rpi;

// ---------- Host power (vcgencmd) ----------
//
// A sagging supply on this Pi is worth shouting about: USB drives (and any Compute
// Module powered from our ports) hang off the same 5 V rail, and under-voltage shows
// up as flaky writes, dropped disks and corrupted flashes long before anything else.

const THROTTLE_FLAGS: [number, string][] = [[0, "under-voltage"], [1, "ARM frequency capped"], [2, "throttled"], [3, "soft temperature limit"]];
let uvLog = { at: 0, times: [] as number[] };
let ext5vSupported = true;

export async function rpiPower() {
  const { out, code } = await run(["vcgencmd", "get_throttled"]);
  const m = out.match(/throttled=(0x[0-9a-f]+)/i);
  if (code !== 0 || !m) return null;
  const v = parseInt(m[1], 16);
  const now = THROTTLE_FLAGS.filter(([b]) => v & (1 << b)).map(([, n]) => n);
  const sinceBoot = THROTTLE_FLAGS.filter(([b]) => v & (1 << (b + 16))).map(([, n]) => n);

  // get_throttled only says "it happened since boot". The firmware hwmon driver logs
  // every episode, which gives a count and how recent the last one was. Re-read at most
  // every 30 s, and only once there is something to find.
  if (v & (1 << 16) && Date.now() - uvLog.at > 30000) {
    uvLog.at = Date.now();
    const j = await run(["sudo", "journalctl", "-k", "-b", "-o", "short-unix", "--no-pager", "-g", "Undervoltage detected"]);
    uvLog.times = j.out.split("\n").map((l) => parseFloat(l) * 1000).filter((t) => t > 0);
  }

  // Pi 5 can read its 5 V input directly; older boards can't ("Command not registered").
  let inputVolts: number | null = null;
  if (ext5vSupported) {
    const r = await run(["vcgencmd", "pmic_read_adc", "EXT5V_V"]);
    const vm = r.out.match(/=([\d.]+)V/);
    if (vm) inputVolts = Math.round(parseFloat(vm[1]) * 100) / 100;
    else ext5vSupported = false;
  }

  return {
    raw: m[1],
    now,
    sinceBoot,
    undervoltageEvents: uvLog.times.length,
    lastUndervoltage: uvLog.times.length ? uvLog.times[uvLog.times.length - 1] : null,
    inputVolts,
  };
}
