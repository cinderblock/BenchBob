// Compute Modules (and other Pis) sitting in USB boot mode.
//
// With nRPIBOOT pulled low (the "disable eMMC boot" jumper on an IO board), a CM's boot
// ROM enumerates as a bare Broadcom USB device and waits for a host to send it code.
// `rpiboot` (apt package `rpiboot`) is that host. Its payload directories in
// /usr/share/rpiboot do the two useful things:
//
//   mass-storage-gadget64  boots a tiny Linux that exports the eMMC/NVMe as a USB disk,
//                          which then shows up here as an ordinary drive to flash/browse
//   recovery / recovery5   recovery.bin, which reads OTP metadata and (given pieeprom.bin
//                          + .sig) rewrites the SPI bootloader EEPROM
//
// Every rpiboot run is pinned to one USB port path (`-p`), so two CMs on one host
// can't be confused with each other.

import { readdir, readFile, mkdir, rm, copyFile, writeFile, stat, rename } from "node:fs/promises";

const USB_ROOT = "/sys/bus/usb/devices";
const RPIBOOT_DIR = "/usr/share/rpiboot";
const EEPROM_FW_DIR = "/lib/firmware/raspberrypi";
const WORK_ROOT = "/tmp/deskypi-rpiboot";
const HISTORY_PATH = import.meta.dir + "/eeprom-history.json";

// Broadcom vendor 0a5c, product = the SoC's boot ROM.
const BOOT_ROMS: Record<string, { chip: Chip; label: string }> = {
  "2763": { chip: "2835", label: "BCM2835/2837 (CM1/CM3)" },
  "2764": { chip: "2835", label: "BCM2837 (CM3/CM3+)" },
  "2711": { chip: "2711", label: "BCM2711 (CM4 / Pi 4 family)" },
  "2712": { chip: "2712", label: "BCM2712 (CM5 / Pi 5 family)" },
};

export type Chip = "2835" | "2711" | "2712";

export interface BootDevice {
  path: string; // USB port path, e.g. "1-1.3" — what `rpiboot -p` wants
  chip: Chip;
  label: string;
  serial: string | null;
  // BCM2711 re-enumerates once bootcode4.bin is running; same IDs, different serial
  // string. Shown so a half-booted device is recognisable.
  product: string | null;
}

export interface EepromImage {
  id: string;
  label: string;
  path: string;
  recoveryBin: string;
  buildDate: string;
  config: string; // the boot.conf embedded in the image as shipped
}

async function readAttr(dir: string, name: string): Promise<string | null> {
  try {
    return (await readFile(`${dir}/${name}`, "utf8")).trim() || null;
  } catch {
    return null;
  }
}

export async function rpibootInstalled(): Promise<boolean> {
  return Bun.file("/usr/bin/rpiboot").exists();
}

export async function getBootDevices(): Promise<BootDevice[]> {
  let names: string[];
  try {
    names = await readdir(USB_ROOT);
  } catch {
    return [];
  }
  const out: BootDevice[] = [];
  for (const name of names) {
    if (name.includes(":")) continue; // interfaces, not devices
    const dir = `${USB_ROOT}/${name}`;
    if ((await readAttr(dir, "idVendor")) !== "0a5c") continue;
    const rom = BOOT_ROMS[(await readAttr(dir, "idProduct")) || ""];
    if (!rom) continue;
    out.push({ path: name, chip: rom.chip, label: rom.label, serial: await readAttr(dir, "serial"), product: await readAttr(dir, "product") });
  }
  return out.sort((a, b) => a.path.localeCompare(b.path));
}

// ==================== Board revision ====================
// https://www.raspberrypi.com/documentation/computers/raspberry-pi.html#new-style-revision-codes

const BOARD_TYPES: Record<number, string> = {
  0x00: "Pi A", 0x01: "Pi B", 0x02: "Pi A+", 0x03: "Pi B+", 0x04: "Pi 2B", 0x08: "Pi 3B", 0x09: "Pi Zero",
  0x0c: "Pi Zero W", 0x0d: "Pi 3B+", 0x0e: "Pi 3A+", 0x12: "Pi Zero 2 W",
  0x06: "CM1", 0x0a: "CM3", 0x10: "CM3+", 0x11: "Pi 4B", 0x13: "Pi 400", 0x14: "CM4", 0x15: "CM4S",
  0x17: "Pi 5", 0x18: "CM5", 0x19: "Pi 500", 0x1a: "CM5 Lite", 0x1b: "CM0",
};
const MEMORY = ["256 MB", "512 MB", "1 GB", "2 GB", "4 GB", "8 GB", "16 GB"];

export function decodeBoardRev(hex: string | undefined): { model: string; memory: string; revision: string } | null {
  if (!hex) return null;
  const v = parseInt(hex, 16);
  if (!Number.isFinite(v) || !(v & (1 << 23))) return null; // old-style codes predate CM4
  return {
    model: BOARD_TYPES[(v >> 4) & 0xff] || `type 0x${((v >> 4) & 0xff).toString(16)}`,
    memory: MEMORY[(v >> 20) & 7] || "?",
    revision: `1.${v & 0xf}`,
  };
}

// ==================== Bootloader images ====================

function buildDate(path: string): string {
  return path.match(/pieeprom-(\d{4}-\d{2}-\d{2})\.bin$/)?.[1] || "";
}

// Candidate images for a chip: the newest image in each rpi-eeprom release channel
// (kept current by apt), plus the one rpiboot ships as its known-good recovery image.
export async function eepromImages(chip: Chip): Promise<EepromImage[]> {
  if (chip === "2835") return [];
  const imgs: EepromImage[] = [];
  const base = `${EEPROM_FW_DIR}/bootloader-${chip}`;
  for (const channel of ["default", "latest"]) {
    const newest = (await channelImages(`${base}/${channel}`)).at(-1);
    if (!newest) continue;
    const path = `${base}/${channel}/${newest}`;
    const date = buildDate(path);
    if (imgs.some((i) => i.buildDate === date)) continue; // same build in both channels
    imgs.push({ id: channel, label: `${date} (rpi-eeprom "${channel}" channel)`, path, recoveryBin: `${base}/${channel}/recovery.bin`, buildDate: date, config: (await imageInfo(path))?.config || "" });
  }
  const stock = await stockImage(chip);
  if (stock) {
    imgs.push({ id: "rpiboot", label: `${stock.buildDate || "?"} (rpiboot's stock recovery image)`, path: stock.path, recoveryBin: `${RPIBOOT_DIR}/firmware/${chip}/recovery.bin`, buildDate: stock.buildDate, config: (await imageInfo(stock.path))?.config || "" });
  }
  return imgs;
}

async function channelImages(dir: string): Promise<string[]> {
  try {
    return (await readdir(dir)).filter((f) => /^pieeprom-\d{4}-\d{2}-\d{2}\.bin$/.test(f)).sort();
  } catch {
    return [];
  }
}

async function stockImage(chip: Chip): Promise<{ path: string; buildDate: string } | null> {
  const path = `${RPIBOOT_DIR}/firmware/${chip}/pieeprom.bin`;
  if (!(await Bun.file(path).exists())) return null;
  const ts = parseInt((await readFile(path)).toString("latin1").match(/BUILD_TIMESTAMP=(\d+)/)?.[1] || "0");
  return { path, buildDate: ts ? new Date(ts * 1000).toISOString().slice(0, 10) : "" };
}

function sha256(data: ArrayBuffer): string {
  return new Bun.CryptoHasher("sha256").update(data).digest("hex");
}

// sha256 and embedded config of an image file, cached until the file changes (apt
// upgrades replace them).
const imageInfoCache = new Map<string, { mtime: number; sha256: string; config: string }>();

async function imageInfo(path: string): Promise<{ sha256: string; config: string } | null> {
  let mtime: number;
  try {
    mtime = (await stat(path)).mtimeMs;
  } catch {
    return null;
  }
  const hit = imageInfoCache.get(path);
  if (hit && hit.mtime === mtime) return hit;
  const cfg = Bun.spawnSync(["rpi-eeprom-config", path]);
  const info = { mtime, sha256: sha256(await Bun.file(path).arrayBuffer()), config: cfg.exitCode === 0 ? cfg.stdout.toString() : "" };
  imageInfoCache.set(path, info);
  return info;
}

// The boot.conf rpiboot's recovery directory ships for this chip: the documented
// starting point for a CM (e.g. BOOT_ORDER=0xf25641 on CM4).
export async function defaultBootConf(chip: Chip): Promise<string> {
  const dir = chip === "2712" ? "recovery5" : "recovery";
  try {
    return await readFile(`${RPIBOOT_DIR}/${dir}/boot.conf`, "utf8");
  } catch {
    return "[all]\nBOOT_UART=1\n";
  }
}

// ==================== Identifying the installed bootloader ====================
//
// Nothing reads the EEPROM (or its config) back over USB. recovery.bin does report an
// EEPROM_HASH in its metadata, so the installed image is recognised by hash: against every
// stock release image, and against every image DeskyPi wrote itself. For our own writes
// both the sha256 of the file and the hash the module reported straight afterwards are
// kept, since what EEPROM_HASH covers isn't documented. No match means "unknown", never a
// guess.

export interface WriteRecord {
  at: number;
  chip: Chip;
  mac: string | null;
  imageLabel: string;
  buildDate: string;
  bootConf: string;
  hashes: string[];
}

export interface Installed {
  source: "deskypi" | "stock" | "unknown";
  buildDate: string | null;
  label: string;
  bootConf: string | null; // the installed config, when the image was recognised
  writtenAt?: number;
  // The newest DeskyPi write to this MAC when the installed image isn't it: something
  // else (self-update, rpi-eeprom-update on the module) has replaced it since.
  lastWrite?: WriteRecord;
}

async function readHistory(): Promise<WriteRecord[]> {
  try {
    const h = JSON.parse(await readFile(HISTORY_PATH, "utf8"));
    return Array.isArray(h) ? h : [];
  } catch {
    return [];
  }
}

export async function recordWrite(rec: WriteRecord): Promise<void> {
  const hashes = [...new Set(rec.hashes.filter(Boolean).map((x) => x.toLowerCase()))];
  const h = [...(await readHistory()), { ...rec, mac: rec.mac?.toLowerCase() || null, hashes }].slice(-200);
  await writeFile(`${HISTORY_PATH}.tmp`, JSON.stringify(h, null, 2) + "\n");
  await rename(`${HISTORY_PATH}.tmp`, HISTORY_PATH);
}

export async function identifyBootloader(chip: Chip, metadata: Record<string, string> | null): Promise<Installed> {
  const hash = metadata?.EEPROM_HASH?.toLowerCase() || null;
  const mac = metadata?.MAC_ADDR?.toLowerCase() || null;
  const history = (await readHistory()).filter((r) => r.chip === chip);
  const lastWrite = mac ? history.filter((r) => r.mac === mac).at(-1) : undefined;
  if (!hash) return { source: "unknown", buildDate: null, label: "Unknown: the module didn't report an EEPROM hash", bootConf: null, lastWrite };

  const ours = history.filter((r) => r.hashes.includes(hash)).at(-1);
  if (ours) return { source: "deskypi", buildDate: ours.buildDate, label: `${ours.buildDate}, written by DeskyPi`, bootConf: ours.bootConf, writtenAt: ours.at };

  const base = `${EEPROM_FW_DIR}/bootloader-${chip}`;
  const paths = new Set<string>();
  for (const channel of ["default", "latest", "stable", "beta", "critical"]) {
    for (const f of await channelImages(`${base}/${channel}`)) paths.add(`${base}/${channel}/${f}`);
  }
  const stock = await stockImage(chip);
  if (stock) paths.add(stock.path);
  for (const path of paths) {
    const info = await imageInfo(path);
    if (info?.sha256 !== hash) continue;
    const date = path === stock?.path ? stock.buildDate : buildDate(path);
    return { source: "stock", buildDate: date, label: `${date}, a stock release with its default settings`, bootConf: info.config, lastWrite };
  }
  return { source: "unknown", buildDate: null, label: "Unrecognised: not a stock release with default settings, and not written by DeskyPi", bootConf: null, lastWrite };
}

// ==================== Running rpiboot ====================

export interface RpibootRun {
  code: number;
  log: string[];
  metadata: Record<string, string> | null;
  timedOut: boolean;
  imageSha256?: string; // of the pieeprom.bin sent, for bootloader writes
}

export interface RunHooks {
  onLine: (line: string) => void;
  onSpawn: (proc: ReturnType<typeof Bun.spawn>) => void;
  isAborted: () => boolean;
}

// rpiboot polls forever when nothing answers, so every run has a deadline.
async function rpiboot(args: string[], hooks: RunHooks, timeoutMs: number): Promise<{ code: number; log: string[]; timedOut: boolean }> {
  const proc = Bun.spawn(["sudo", "rpiboot", ...args], { stdout: "pipe", stderr: "pipe" });
  hooks.onSpawn(proc);
  let timedOut = false;
  const timer = setTimeout(() => { timedOut = true; proc.kill(); }, timeoutMs);
  const log: string[] = [];
  const pump = async (stream: ReadableStream<Uint8Array>) => {
    const dec = new TextDecoder();
    let buf = "";
    for await (const chunk of stream) {
      buf += dec.decode(chunk, { stream: true });
      let i;
      while ((i = buf.search(/[\r\n]/)) !== -1) {
        const line = buf.slice(0, i).trim();
        buf = buf.slice(i + 1);
        if (!line) continue;
        log.push(line);
        if (log.length > 400) log.splice(0, log.length - 400);
        hooks.onLine(line);
      }
    }
    if (buf.trim()) { log.push(buf.trim()); hooks.onLine(buf.trim()); }
  };
  await Promise.all([pump(proc.stdout), pump(proc.stderr)]);
  const code = await proc.exited;
  clearTimeout(timer);
  return { code, log, timedOut };
}

async function readMetadata(dir: string): Promise<Record<string, string> | null> {
  try {
    const f = (await readdir(dir)).find((n) => n.endsWith(".json"));
    if (!f) return null;
    return JSON.parse(await readFile(`${dir}/${f}`, "utf8"));
  } catch {
    return null;
  }
}

// Load the mass-storage gadget. Once it's up the device leaves boot mode and its
// storage enumerates as a USB disk (udev auto-mount, NFS/NBD export, Flash all apply).
export async function loadMassStorage(dev: BootDevice, hooks: RunHooks): Promise<RpibootRun> {
  // CM1/CM3 use rpiboot's built-in msd (bootcode.bin + start.elf); 2711/2712 the
  // Linux mass-storage-gadget.
  const args = dev.chip === "2835" ? ["-p", dev.path] : ["-d", `${RPIBOOT_DIR}/mass-storage-gadget64`, "-p", dev.path];
  const r = await rpiboot(args, hooks, 120_000);
  return { ...r, metadata: null };
}

// recovery.bin in a work dir of our own: never modify /usr/share/rpiboot itself.
async function recoveryWorkDir(chip: Chip, recoveryBin: string): Promise<string> {
  const dir = `${WORK_ROOT}/${crypto.randomUUID()}`;
  await mkdir(`${dir}/metadata`, { recursive: true });
  await copyFile(recoveryBin, `${dir}/${chip === "2712" ? "bootcode5.bin" : "bootcode4.bin"}`);
  const stockDir = chip === "2712" ? "recovery5" : "recovery";
  await copyFile(`${RPIBOOT_DIR}/${stockDir}/config.txt`, `${dir}/config.txt`);
  return dir;
}

// Read-only: recovery.bin with no pieeprom.* files reports "No EEPROM files", writes
// nothing, and still sends the OTP metadata.
export async function readBootloaderInfo(dev: BootDevice, hooks: RunHooks): Promise<RpibootRun> {
  if (dev.chip === "2835") throw new Error("CM1/CM3 have no bootloader EEPROM");
  const stock = `${RPIBOOT_DIR}/firmware/${dev.chip}/recovery.bin`;
  const dir = await recoveryWorkDir(dev.chip, stock);
  try {
    const r = await rpiboot(["-d", dir, "-j", `${dir}/metadata`, "-p", dev.path], hooks, 60_000);
    return { ...r, metadata: await readMetadata(`${dir}/metadata`) };
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

export async function writeBootloader(dev: BootDevice, image: EepromImage, bootConf: string, hooks: RunHooks): Promise<RpibootRun> {
  if (dev.chip === "2835") throw new Error("CM1/CM3 have no bootloader EEPROM");
  const dir = await recoveryWorkDir(dev.chip, image.recoveryBin);
  try {
    await writeFile(`${dir}/boot.conf`, bootConf.replace(/\r\n/g, "\n").trimEnd() + "\n");
    // Embed the config into the image, then the .sig recovery.bin checks it against.
    const cfg = Bun.spawnSync(["rpi-eeprom-config", "--config", `${dir}/boot.conf`, "--out", `${dir}/pieeprom.bin`, image.path]);
    if (cfg.exitCode !== 0) throw new Error(`rpi-eeprom-config: ${cfg.stderr.toString().trim() || `exit ${cfg.exitCode}`}`);
    const dig = Bun.spawnSync(["rpi-eeprom-digest", "-i", `${dir}/pieeprom.bin`, "-o", `${dir}/pieeprom.sig`]);
    if (dig.exitCode !== 0) throw new Error(`rpi-eeprom-digest: ${dig.stderr.toString().trim() || `exit ${dig.exitCode}`}`);
    hooks.onLine(`Built pieeprom.bin from ${image.label}`);
    const imageSha256 = sha256(await Bun.file(`${dir}/pieeprom.bin`).arrayBuffer());
    const r = await rpiboot(["-d", dir, "-j", `${dir}/metadata`, "-p", dev.path], hooks, 180_000);
    return { ...r, metadata: await readMetadata(`${dir}/metadata`), imageSha256 };
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}
