import { access, rm } from "node:fs/promises";
import { constants as fsConstants } from "node:fs";
import { constants as zlibConstants, createGzip } from "node:zlib";
import { analyseDisk, type DiskInfo } from "./osinfo.ts";
import * as rpi from "./rpiboot.ts";
import * as periph from "./peripherals/core.ts";
import { board, boardInfo } from "./peripherals/boards/index.ts";
import { rpiPower } from "./peripherals/boards/rpi.ts";
import { cpuTemp, readSensors, type Sensors } from "./peripherals/lib/health.ts";
import { statePath, VERSION } from "./paths.ts";
import * as update from "./update.ts";

const PORT = parseInt(process.env.PORT || "") || 80;
const DEFAULTS_PATH = statePath("defaults.json");
const FLASH_DIR = "/var/tmp/benchbob-flash";
const BOOT_MOUNT = "/tmp/benchbob-boot";

// ==================== Helpers ====================

async function readSysFile(path: string): Promise<string> {
  try {
    return (await Bun.file(path).text()).trim();
  } catch {
    return "";
  }
}

async function run(cmd: string[]): Promise<{ out: string; err: string; code: number }> {
  let proc;
  try {
    proc = Bun.spawn(cmd, { stdout: "pipe", stderr: "pipe" });
  } catch (e: any) {
    // Not installed: a failed command, as in a shell (exit 127).
    return { out: "", err: `${cmd[0]}: ${e.code === "ENOENT" ? "not installed" : e.message}`, code: 127 };
  }
  const [out, err] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
  ]);
  return { out: out.trim(), err: err.trim(), code: await proc.exited };
}

function fmtBytes(bytes: number): string {
  if (bytes === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  return `${(bytes / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0)} ${units[i]}`;
}

function shellEsc(s: string): string {
  return s.replace(/'/g, "'\\''");
}

// ==================== History Ring Buffer ====================

const HIST_N = 30;
const histBuf: Record<string, number[]> = {};

function pushHist(key: string, val: number): void {
  if (!histBuf[key]) histBuf[key] = [];
  histBuf[key].push(val);
  if (histBuf[key].length > HIST_N) histBuf[key].shift();
}

// ==================== Monitoring State ====================

let prevCpu: { total: number; idle: number } | null = null;
let cpuUsage = 0;
let prevDisk: Record<string, [number, number, number]> = {};
let diskRates: Record<string, [number, number]> = {};
let prevNet: Record<string, [number, number, number]> = {};
let netRates: Record<string, [number, number]> = {};

async function sampleIO(): Promise<void> {
  const now = Date.now();

  // CPU usage
  const statLine = (await readSysFile("/proc/stat")).split("\n")[0];
  if (statLine) {
    const f = statLine.split(/\s+/).slice(1).map(Number);
    const total = f.reduce((a, b) => a + b, 0);
    const idle = f[3] + (f[4] || 0);
    if (prevCpu) {
      const dT = total - prevCpu.total;
      const dI = idle - prevCpu.idle;
      cpuUsage = dT > 0 ? ((dT - dI) / dT) * 100 : 0;
    }
    prevCpu = { total, idle };
  }

  // Disk I/O
  const { out: blockList } = await run(["ls", "/sys/block"]);
  for (const name of blockList.split("\n").filter(Boolean)) {
    if (name.startsWith("loop") || name.startsWith("zram") || name.startsWith("ram")) continue;
    const stat = await readSysFile(`/sys/block/${name}/stat`);
    if (!stat) continue;
    const f = stat.split(/\s+/).filter(Boolean);
    const rs = parseInt(f[2]) || 0;
    const ws = parseInt(f[6]) || 0;
    const p = prevDisk[name];
    if (p) {
      const dt = (now - p[2]) / 1000;
      if (dt > 0) diskRates[name] = [Math.max(0, ((rs - p[0]) * 512) / dt), Math.max(0, ((ws - p[1]) * 512) / dt)];
    }
    prevDisk[name] = [rs, ws, now];
  }

  // Network I/O
  const { out: netList } = await run(["ls", "/sys/class/net"]);
  for (const name of netList.split("\n").filter(Boolean)) {
    if (name === "lo") continue;
    const rx = parseInt(await readSysFile(`/sys/class/net/${name}/statistics/rx_bytes`)) || 0;
    const tx = parseInt(await readSysFile(`/sys/class/net/${name}/statistics/tx_bytes`)) || 0;
    const p = prevNet[name];
    if (p) {
      const dt = (now - p[2]) / 1000;
      if (dt > 0) netRates[name] = [Math.max(0, (rx - p[0]) / dt), Math.max(0, (tx - p[1]) / dt)];
    }
    prevNet[name] = [rx, tx, now];
  }

  // Push to history
  pushHist("cpu", Math.round(cpuUsage * 10) / 10);
  for (const [name, [r, w]] of Object.entries(diskRates)) pushHist(`drv:${name}:r`, r), pushHist(`drv:${name}:w`, w);
  for (const [name, [rx, tx]] of Object.entries(netRates)) pushHist(`net:${name}:rx`, rx), pushHist(`net:${name}:tx`, tx);
}

// ==================== System Info ====================

// The board layer (peripherals/boards/) names the machine: the device-tree model on ARM
// (unchanged on a Pi), DMI vendor + product on a PC.
const hostModel = board().name;
let ramMB = 0;
(async () => {
  // Installed RAM from the board revision code (bits 20-22 of a new-style code), since
  // MemTotal is short by whatever the GPU reserved. Old-style codes: round MemTotal up.
  const rev = parseInt((await readSysFile("/proc/cpuinfo")).match(/^Revision\s*:\s*([0-9a-f]+)/im)?.[1] || "", 16);
  if (rev & 0x800000) ramMB = 256 << ((rev >> 20) & 7);
  else {
    const kb = parseInt((await readSysFile("/proc/meminfo")).match(/^MemTotal:\s+(\d+)/m)?.[1] || "");
    if (kb) ramMB = 2 ** Math.ceil(Math.log2(kb / 1024));
  }
})();

// Generic sensors (thermal, hwmon, power_supply), re-read at most every 5 s.
let sensorCache: { at: number; s: Sensors } | null = null;
function sensors(): Sensors {
  if (!sensorCache || Date.now() - sensorCache.at > 5000) sensorCache = { at: Date.now(), s: readSensors() };
  return sensorCache.s;
}

async function getSystemInfo() {
  // A Pi's thermal_zone0 is the SoC, as it always was here; elsewhere zone0 can be an ACPI
  // zone that never moves, so the sensor list picks the CPU package.
  const temp = board().def.health === "vcgencmd" ? parseInt(await readSysFile("/sys/class/thermal/thermal_zone0/temp")) / 1000 || 0 : cpuTemp(sensors()) ?? 0;
  const memRaw = await readSysFile("/proc/meminfo");
  const mem: Record<string, number> = {};
  for (const line of memRaw.split("\n")) {
    const m = line.match(/^(\w+):\s+(\d+)/);
    if (m) mem[m[1]] = parseInt(m[2]);
  }
  const cores = (await readSysFile("/proc/cpuinfo")).split("processor").length - 1;

  return {
    model: hostModel,
    ramMB,
    cores: cores || 4,
    cpuUsage: Math.round(cpuUsage * 10) / 10,
    cpuTemp: Math.round(temp * 10) / 10,
    memTotal: mem.MemTotal || 0,
    memAvailable: mem.MemAvailable || 0,
    memPercent: mem.MemTotal ? Math.round(((mem.MemTotal - mem.MemAvailable) / mem.MemTotal) * 1000) / 10 : 0,
  };
}

// ==================== Network Info ====================

async function getWifiInfo(): Promise<{ ssid: string; signal: number; bitrate: string } | null> {
  try {
    const { out, code } = await run(["iw", "dev", "wlan0", "link"]);
    if (code !== 0 || out.includes("Not connected")) return null;
    const ssid = out.match(/SSID:\s*(.+)/)?.[1] || "";
    const signal = parseInt(out.match(/signal:\s*(-?\d+)/)?.[1] || "0");
    const bitrate = out.match(/tx bitrate:\s*(.+)/)?.[1] || "";
    return { ssid, signal, bitrate };
  } catch {
    return null;
  }
}

async function getNetworkInfo() {
  const { out } = await run(["ip", "-j", "addr", "show"]);
  const wifi = await getWifiInfo();
  const { out: routeOut } = await run(["ip", "-j", "route", "show", "default"]);
  const { out: route6Out } = await run(["ip", "-j", "-6", "route", "show", "default"]);
  const gateways: Record<string, { gw: string; metric: number | null }> = {};
  const gateways6: Record<string, string> = {};
  try {
    for (const r of JSON.parse(routeOut)) {
      if (r.dev) gateways[r.dev] = { gw: r.gateway || "", metric: r.metric ?? null };
    }
  } catch {}
  try {
    for (const r of JSON.parse(route6Out)) {
      if (r.dev && r.gateway) gateways6[r.dev] = r.gateway;
    }
  } catch {}
  try {
    const ifaces = JSON.parse(out)
      .filter((i: any) => i.ifname !== "lo")
      .map((i: any) => {
        const inet = i.addr_info?.find((a: any) => a.family === "inet");
        const inet6Addrs = i.addr_info?.filter((a: any) => a.family === "inet6") || [];
        const r = netRates[i.ifname];
        const gw = gateways[i.ifname];
        return {
          name: i.ifname,
          ip: inet?.local || null,
          prefix: inet?.prefixlen || null,
          mac: i.address,
          mtu: i.mtu || null,
          state: i.operstate,
          metric: gw?.metric ?? null,
          gateway: gw?.gw || null,
          gateway6: gateways6[i.ifname] || null,
          ipv6: inet6Addrs.map((a: any) => `${a.local}/${a.prefixlen}`),
          rxRate: r ? r[0] : 0,
          txRate: r ? r[1] : 0,
          wifi: i.ifname === "wlan0" ? wifi : undefined,
        };
      });
    ifaces.sort((a: any, b: any) => (a.metric ?? 9999) - (b.metric ?? 9999));
    return ifaces;
  } catch {
    return [];
  }
}

// ==================== USB Speed ====================

async function getUsbSpeed(devName: string): Promise<string | null> {
  try {
    // Walk sysfs ancestors to find USB speed attribute
    let p = (await run(["readlink", "-f", `/sys/block/${devName}`])).out;
    for (let i = 0; i < 8; i++) {
      p = p.substring(0, p.lastIndexOf("/"));
      if (p.length < 5) break;
      const speed = await readSysFile(`${p}/speed`);
      if (speed) {
        const mbps = parseInt(speed);
        if (mbps <= 12) return "USB 1.1";
        if (mbps <= 480) return "USB 2.0";
        if (mbps <= 5000) return "USB 3.0";
        if (mbps <= 10000) return "USB 3.1";
        return `USB 3.2`;
      }
    }
    return null;
  } catch {
    return null;
  }
}

// ==================== Root Storage ====================
//
// What this Pi is running from. Found from the / mount rather than by guessing the SD
// card, so NVMe, USB and netboot hosts are described (and protected) correctly.

let rootDisk: { name: string | null; kind: string; size: number; source: string; fstype: string } | null = null;

async function getRootStorage() {
  const { out } = await run(["findmnt", "-n", "-J", "-b", "-o", "SOURCE,FSTYPE,SIZE,USE%", "/"]);
  let fs: any;
  try {
    fs = JSON.parse(out).filesystems[0];
  } catch {
    return null;
  }
  if (!rootDisk || rootDisk.source !== fs.source) {
    const source: string = fs.source, fstype: string = fs.fstype;
    rootDisk = { name: null, kind: fstype, size: 0, source, fstype };
    if (/^nfs/.test(fstype)) rootDisk.kind = "Netboot";
    else if (source.startsWith("/dev/")) {
      const parent = (await run(["lsblk", "-no", "PKNAME", source])).out.trim();
      const name = parent || source.slice(5);
      let d: any = {};
      try {
        d = JSON.parse((await run(["lsblk", "-dn", "-J", "-b", "-o", "NAME,TRAN,SIZE", `/dev/${name}`])).out).blockdevices[0];
      } catch {}
      rootDisk.name = name;
      rootDisk.size = Number(d.size) || 0;
      if (name.startsWith("nvme") || d.tran === "nvme") rootDisk.kind = "NVMe";
      else if (name.startsWith("mmcblk")) rootDisk.kind = (await readSysFile(`/sys/block/${name}/device/type`)) === "MMC" ? "eMMC" : "microSD";
      else if (d.tran === "usb") rootDisk.kind = "USB";
      else if (d.tran) rootDisk.kind = d.tran.toUpperCase();
    }
  }
  return { ...rootDisk, usePercent: parseInt(fs["use%"]) || 0 };
}

// ==================== Block Devices ====================

async function getDevices() {
  const { out } = await run([
    "lsblk", "-J", "-b", "-o",
    "NAME,PATH,SIZE,TYPE,MOUNTPOINT,MODEL,SERIAL,TRAN,FSTYPE,LABEL,UUID,RM,RO,REV",
  ]);
  let all: any[] = [];
  try {
    all = JSON.parse(out).blockdevices || [];
  } catch {}

  const enrich = (d: any) => {
    const r = diskRates[d.name];
    d.readRate = r ? r[0] : 0;
    d.writeRate = r ? r[1] : 0;
    d.rm = d.rm === true || d.rm === "1" || d.rm === 1;
    d.ro = d.ro === true || d.ro === "1" || d.ro === 1;
    if (d.children) d.children = d.children.map(enrich);
    return d;
  };
  all = all.map(enrich);

  // A USB-booted host must never offer its own root disk for erasing or flashing.
  const rootDev = all.find((d) => d.name === rootDisk?.name);
  const usbDrives: any[] = [];

  for (const d of all.filter((d) => d.tran === "usb" && d.type === "disk" && d.size > 0 && d !== rootDev)) {
    d.usbVersion = await getUsbSpeed(d.name);
    d.hasPartitions = !!(d.children && d.children.length > 0);
    d.operations = getOpsForDevice(d.name).map(opPublic);
    usbDrives.push(d);
  }

  return { rootDevice: rootDev ? enrich(rootDev) : null, drives: usbDrives };
}

// ==================== NBD Exports ====================

async function getNbdExports(): Promise<string[]> {
  try {
    const { out } = await run(["ls", "/etc/nbd-server/conf.d/"]);
    return out.split("\n").filter((f) => f.endsWith(".conf")).map((f) => f.replace(".conf", ""));
  } catch {
    return [];
  }
}

// ==================== Host Power ====================
//
// The Pi's firmware flags under-voltage and throttling (boards/rpi.ts, through vcgencmd);
// other boards have no such source, so `power` is null there and the generic readings
// (thermal, hwmon, power supplies) go out as `sensors` instead.

async function getPower() {
  return board().def.health === "vcgencmd" ? rpiPower() : null;
}

// ==================== Status Assembly ====================

async function getStatus() {
  await sampleIO();
  const system = await getSystemInfo();
  const network = await getNetworkInfo();
  const rootStorage = await getRootStorage();
  const { rootDevice, drives } = await getDevices();
  const nbdExports = await getNbdExports();
  const bootDevices = await rpi.getBootDevices();
  const hostname = await readSysFile("/etc/hostname");
  const uptimeStr = await readSysFile("/proc/uptime");
  const power = await getPower();

  // Push mem/wifi to history
  pushHist("mem", system.memPercent);
  const wifiIf = network.find((n: any) => n.wifi);
  if (wifiIf) pushHist("wifi", wifiIf.wifi.signal);

  return {
    hostname,
    uptime: parseFloat(uptimeStr.split(" ")[0]) || 0,
    timestamp: Date.now(),
    version: VERSION,
    update: update.summary(),
    buildVersion: (await getBuild()).version,
    // Alias kept so dashboards still open from the previous deploy — which compare
    // `uiVersion` — see the change and reload themselves once.
    uiVersion: (await getBuild()).version,
    board: boardInfo(),
    system,
    power,
    sensors: sensors(),
    network,
    rootStorage,
    rootDevice,
    drives,
    nbdExports,
    bootDevices,
    rpibootInstalled: await rpi.rpibootInstalled(),
    rpibootResults: [...rpibootResults.values()],
    filesRoot: FILES_ROOT,
    operations: getAllOps().map(opPublic),
    peripherals: periph.status(),
    history: histBuf,
  };
}

// ==================== Operation Queue ====================

interface Operation {
  id: string;
  type: "erase" | "flash" | "deepscan" | "blcard" | "msd" | "eeprom-check" | "eeprom-write";
  device: string;
  status: string;
  progress: number;
  message: string;
  mode?: string;
  imageUrl?: string;
  imageName?: string;
  extractSize?: number;
  initFormat?: InitFormat;
  settings?: any;
  scanResult?: any;
  usbPath?: string;
  eepromImageId?: string;
  bootConf?: string;
  chip?: "2711" | "2712"; // bootloader cards: which board family
  process?: ReturnType<typeof Bun.spawn>;
  aborted?: boolean;
}

const deviceOps = new Map<string, Operation[]>();

function opPublic(o: Operation) {
  return { id: o.id, type: o.type, device: o.device, status: o.status, progress: o.progress, message: o.message, imageName: o.imageName, scanResult: o.scanResult };
}

function getOpsForDevice(device: string): Operation[] {
  return deviceOps.get(device) || [];
}

function getAllOps(): Operation[] {
  return [...deviceOps.values()].flat();
}

function isDeviceBusy(device: string): boolean {
  return !!deviceOps.get(device)?.some((o) => !["done", "error", "queued"].includes(o.status));
}

function hasQueuedFlash(device: string): boolean {
  return !!deviceOps.get(device)?.some((o) => o.type === "flash" && o.status === "queued");
}

function enqueue(op: Operation): void {
  let ops = deviceOps.get(op.device);
  if (!ops) {
    ops = [];
    deviceOps.set(op.device, ops);
  }
  const running = ops.find((o) => !["done", "error", "queued"].includes(o.status));
  ops.push(op);
  if (!running) {
    execOp(op);
  } else {
    op.status = "queued";
    op.message = `Queued after ${running.type}`;
  }
}

async function execOp(op: Operation): Promise<void> {
  try {
    if (op.type === "erase") await doErase(op);
    else if (op.type === "flash") await doFlash(op);
    else if (op.type === "deepscan") await doDeepScan(op);
    else if (op.type === "blcard") await doBootloaderCard(op);
    else await doRpiboot(op);
  } catch (e: any) {
    if (op.status !== "error") {
      op.status = "error";
      op.message = e.message;
    }
  }

  setTimeout(() => {
    const ops = deviceOps.get(op.device);
    if (ops) {
      const idx = ops.indexOf(op);
      if (idx !== -1) ops.splice(idx, 1);
      if (ops.length === 0) deviceOps.delete(op.device);
    }
  }, 30000);

  const ops = deviceOps.get(op.device);
  const next = ops?.find((o) => o.status === "queued");
  if (next) execOp(next);
}

// ==================== Device Actions ====================

function isSysDev(name: string): boolean {
  return name.startsWith("mmcblk") || name.startsWith("zram") || name.startsWith("loop");
}

async function prepareDevice(devName: string): Promise<void> {
  const { out } = await run(["lsblk", "-J", "-o", "NAME,MOUNTPOINT", `/dev/${devName}`]);
  const mps: string[] = [];
  const collect = (d: any) => {
    if (d.mountpoint) mps.push(d.mountpoint);
    d.children?.forEach(collect);
  };
  try {
    JSON.parse(out).blockdevices?.[0] && collect(JSON.parse(out).blockdevices[0]);
  } catch {}
  for (const mp of mps) await run(["sudo", "umount", "-l", mp]);

  // Stale mount dirs
  try {
    const { out: dirs } = await run(["ls", "/srv/nfs/drives/"]);
    for (const d of dirs.split("\n").filter(Boolean)) {
      const { code } = await run(["mountpoint", "-q", `/srv/nfs/drives/${d}`]);
      if (code !== 0) await run(["sudo", "rmdir", `/srv/nfs/drives/${d}`]);
    }
  } catch {}

  // NBD cleanup
  const { out: nbd } = await run(["ls", "/etc/nbd-server/conf.d/"]);
  for (const f of nbd.split("\n").filter(Boolean)) {
    if (f.startsWith(devName)) await run(["sudo", "rm", "-f", `/etc/nbd-server/conf.d/${f}`]);
  }
  await run(["sudo", "systemctl", "restart", "nbd-server"]);
  await run(["sudo", "exportfs", "-ra"]);
}

async function ejectDevice(devName: string): Promise<{ ok: boolean; message: string }> {
  if (isSysDev(devName)) return { ok: false, message: "Cannot eject system device" };
  if (isDeviceBusy(devName)) return { ok: false, message: "Device has active operation" };
  try {
    await prepareDevice(devName);
    const { code } = await run(["sudo", "udisksctl", "power-off", "-b", `/dev/${devName}`, "--no-user-interaction"]);
    if (code !== 0) {
      const sp = (await run(["readlink", "-f", `/sys/block/${devName}/device`])).out;
      if (sp) await run(["sudo", "bash", "-c", `echo 1 > ${sp}/delete`]);
    }
    return { ok: true, message: `${devName} ejected` };
  } catch (e: any) {
    return { ok: false, message: e.message };
  }
}

async function scanDevice(devName: string): Promise<{ ok: boolean; message: string; partitions?: any[] }> {
  if (isSysDev(devName)) return { ok: false, message: "Cannot scan system device" };
  await run(["sudo", "partprobe", `/dev/${devName}`]);
  await run(["sudo", "udevadm", "settle", "--timeout=5"]);
  const { out } = await run(["lsblk", "-J", "-b", "-o", "NAME,SIZE,TYPE,FSTYPE,LABEL", `/dev/${devName}`]);
  try {
    const dev = JSON.parse(out).blockdevices?.[0];
    const parts = dev?.children || [];
    if (parts.length > 0) {
      return { ok: true, message: `Found ${parts.length} partition(s)`, partitions: parts };
    }
    // Check for filesystem directly on device
    const { out: blk } = await run(["sudo", "blkid", `/dev/${devName}`]);
    if (blk) return { ok: true, message: `Direct filesystem: ${blk}` };
    // Try file identification
    const { out: fout } = await run(["sudo", "file", "-sL", `/dev/${devName}`]);
    return { ok: true, message: fout || "No partitions or filesystems found" };
  } catch {
    return { ok: false, message: "Scan failed" };
  }
}

// ==================== Raw Sector Probe ====================
//
// A filesystem that reports "empty" says nothing about what is still on the platter.
// These helpers read the block device directly, so a quick-erased or freshly-formatted
// drive still reveals whatever the previous image left behind.

const PROBE_BLOCK = 4096;

// Deep-scan findings, kept per device so they outlive the operation record.
const lastScanResults = new Map<string, any>();

interface Signature {
  name: string;
  bytes: number[];
  at?: number; // required offset within the sampled block; otherwise matched anywhere
}

// Ordered roughly by how much they tell you. `at` entries are filesystem/container
// headers that only mean anything at a fixed offset, which keeps false positives down.
const SIGNATURES: Signature[] = [
  { name: "MBR partition table", bytes: [0x55, 0xaa], at: 510 },
  { name: "GPT header", bytes: [0x45, 0x46, 0x49, 0x20, 0x50, 0x41, 0x52, 0x54] },
  { name: "ext2/3/4 superblock", bytes: [0x53, 0xef], at: 0x438 },
  { name: "FAT32", bytes: [0x46, 0x41, 0x54, 0x33, 0x32], at: 0x52 },
  { name: "FAT12/16", bytes: [0x46, 0x41, 0x54, 0x31], at: 0x36 },
  { name: "NTFS", bytes: [0x4e, 0x54, 0x46, 0x53, 0x20, 0x20, 0x20, 0x20], at: 3 },
  { name: "exFAT", bytes: [0x45, 0x58, 0x46, 0x41, 0x54, 0x20, 0x20, 0x20], at: 3 },
  { name: "LUKS volume", bytes: [0x4c, 0x55, 0x4b, 0x53, 0xba, 0xbe] },
  { name: "squashfs", bytes: [0x68, 0x73, 0x71, 0x73] },
  { name: "Linux swap", bytes: [0x53, 0x57, 0x41, 0x50, 0x53, 0x50, 0x41, 0x43, 0x45, 0x32] },
  { name: "arm64 kernel Image", bytes: [0x41, 0x52, 0x4d, 0x64], at: 0x38 },
  { name: "devicetree blob", bytes: [0xd0, 0x0d, 0xfe, 0xed] },
  { name: "ELF binary", bytes: [0x7f, 0x45, 0x4c, 0x46] },
  { name: "SQLite database", bytes: [0x53, 0x51, 0x4c, 0x69, 0x74, 0x65, 0x20, 0x66, 0x6f, 0x72, 0x6d, 0x61, 0x74, 0x20, 0x33, 0x00] },
  { name: "JPEG", bytes: [0xff, 0xd8, 0xff] },
  { name: "PNG", bytes: [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a] },
  { name: "GIF", bytes: [0x47, 0x49, 0x46, 0x38] },
  { name: "PDF", bytes: [0x25, 0x50, 0x44, 0x46, 0x2d] },
  { name: "ZIP / Office / jar", bytes: [0x50, 0x4b, 0x03, 0x04] },
  { name: "gzip", bytes: [0x1f, 0x8b, 0x08] },
  { name: "xz", bytes: [0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00] },
  { name: "bzip2", bytes: [0x42, 0x5a, 0x68] },
  { name: "zstd", bytes: [0x28, 0xb5, 0x2f, 0xfd] },
  { name: "7-zip", bytes: [0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c] },
  { name: "tar archive", bytes: [0x75, 0x73, 0x74, 0x61, 0x72], at: 257 },
  { name: "MP4 / MOV", bytes: [0x66, 0x74, 0x79, 0x70], at: 4 },
  { name: "Matroska / WebM", bytes: [0x1a, 0x45, 0xdf, 0xa3] },
  { name: "RIFF (wav/avi)", bytes: [0x52, 0x49, 0x46, 0x46] },
  { name: "Ogg", bytes: [0x4f, 0x67, 0x67, 0x53] },
  { name: "FLAC", bytes: [0x66, 0x4c, 0x61, 0x43] },
  { name: "MP3 (ID3)", bytes: [0x49, 0x44, 0x33] },
];

const MAX_SIG_LEN = Math.max(...SIGNATURES.map((s) => s.bytes.length));

function matchesAt(buf: Uint8Array, pos: number, bytes: number[]): boolean {
  if (pos + bytes.length > buf.length) return false;
  for (let i = 0; i < bytes.length; i++) if (buf[pos + i] !== bytes[i]) return false;
  return true;
}

// Returns absolute byte offsets of every signature hit in `buf`, which starts at `base`.
function findSignatures(buf: Uint8Array, base: number): { name: string; offset: number }[] {
  const hits: { name: string; offset: number }[] = [];
  for (const sig of SIGNATURES) {
    if (sig.at !== undefined) {
      // Anchored: only meaningful at a fixed offset within each block-aligned unit.
      // Alignment is relative to the device, not the buffer — in the deep scan `base`
      // is offset by the carry-over overlap and is not itself block-aligned.
      const firstBlk = Math.ceil(base / PROBE_BLOCK) * PROBE_BLOCK;
      for (let abs = firstBlk; abs - base + sig.at + sig.bytes.length <= buf.length; abs += PROBE_BLOCK) {
        if (matchesAt(buf, abs - base + sig.at, sig.bytes)) hits.push({ name: sig.name, offset: abs });
      }
    } else {
      const first = sig.bytes[0];
      for (let i = 0; i < buf.length; i++) {
        if (buf[i] === first && matchesAt(buf, i, sig.bytes)) hits.push({ name: sig.name, offset: base + i });
      }
    }
  }
  return hits;
}

type BlockKind = "zero" | "ones" | "text" | "compressed" | "structured";

interface BlockReading {
  offset: number;
  kind: BlockKind;
  entropy: number;
  signatures: string[];
}

// Shannon entropy in bits/byte. ~0 = uniform fill, ~8 = compressed or encrypted.
function classifyBlock(buf: Uint8Array, offset: number): BlockReading {
  const freq = new Uint32Array(256);
  let zeros = 0, ones = 0, printable = 0;
  for (const b of buf) {
    freq[b]++;
    if (b === 0x00) zeros++;
    else if (b === 0xff) ones++;
    if ((b >= 0x20 && b < 0x7f) || b === 0x09 || b === 0x0a || b === 0x0d) printable++;
  }
  const n = buf.length || 1;
  let entropy = 0;
  for (const f of freq) {
    if (!f) continue;
    const p = f / n;
    entropy -= p * Math.log2(p);
  }

  let kind: BlockKind;
  if (zeros === n) kind = "zero";
  else if (ones === n) kind = "ones";
  else if (printable / n > 0.85) kind = "text";
  else if (entropy >= 7.5) kind = "compressed";
  else kind = "structured";

  const signatures = [...new Set(findSignatures(buf, 0).map((h) => h.name))];
  return { offset, kind, entropy: Math.round(entropy * 100) / 100, signatures };
}

async function getDeviceSizeBytes(devName: string): Promise<number> {
  const sectors = parseInt(await readSysFile(`/sys/block/${devName}/size`)) || 0;
  return sectors * 512;
}

// Byte ranges NOT covered by any declared partition — where leftovers hide.
async function getUnallocatedRanges(devName: string, total: number): Promise<{ start: number; end: number }[]> {
  const { out } = await run(["sudo", "sfdisk", "-J", `/dev/${devName}`]);
  let used: { start: number; end: number }[] = [];
  try {
    const pt = JSON.parse(out).partitiontable;
    const ss = pt?.sectorsize || 512;
    used = (pt?.partitions || [])
      .map((p: any) => ({ start: p.start * ss, end: (p.start + p.size) * ss }))
      .sort((a: any, b: any) => a.start - b.start);
  } catch {
    // No partition table at all — the whole device is "unallocated".
    return total > 0 ? [{ start: 0, end: total }] : [];
  }
  const gaps: { start: number; end: number }[] = [];
  let cursor = 0;
  for (const u of used) {
    if (u.start > cursor) gaps.push({ start: cursor, end: u.start });
    cursor = Math.max(cursor, u.end);
  }
  if (cursor < total) gaps.push({ start: cursor, end: total });
  return gaps.filter((g) => g.end - g.start >= PROBE_BLOCK);
}

// One sudo, many seeks — 250 separate `sudo dd` calls would spend most of the time in sudo.
async function readBlocks(devName: string, blockIndexes: number[]): Promise<Uint8Array[]> {
  if (blockIndexes.length === 0) return [];
  const script = blockIndexes
    .map((i) => `dd if=/dev/${devName} bs=${PROBE_BLOCK} count=1 skip=${i} status=none 2>/dev/null`)
    .join("; ");
  const proc = Bun.spawn(["sudo", "sh", "-c", script], { stdout: "pipe", stderr: "pipe" });
  const all = new Uint8Array(await new Response(proc.stdout).arrayBuffer());
  await proc.exited;
  const out: Uint8Array[] = [];
  for (let i = 0; i < blockIndexes.length; i++) {
    const s = i * PROBE_BLOCK;
    if (s >= all.length) break;
    out.push(all.subarray(s, Math.min(s + PROBE_BLOCK, all.length)));
  }
  return out;
}

function spreadBlocks(start: number, end: number, count: number): number[] {
  const first = Math.floor(start / PROBE_BLOCK);
  const last = Math.floor((end - PROBE_BLOCK) / PROBE_BLOCK);
  if (last < first) return [];
  const span = last - first;
  const n = Math.min(count, span + 1);
  if (n <= 1) return [first];
  const out: number[] = [];
  for (let i = 0; i < n; i++) out.push(first + Math.round((span * i) / (n - 1)));
  return [...new Set(out)];
}

// Fast sampled read — seconds, not minutes. Gives a feel for what is actually on the
// device without committing to a full pass.
async function probeDevice(devName: string): Promise<any> {
  if (isSysDev(devName)) return { ok: false, message: "Cannot probe system device" };
  const total = await getDeviceSizeBytes(devName);
  if (!total) return { ok: false, message: "Could not determine device size" };

  const gaps = await getUnallocatedRanges(devName, total);

  // Uniform sweep across the whole device, plus a denser look at the unallocated gaps
  // and at the head/tail where partition tables and stale superblocks live.
  const plan = new Set<number>(spreadBlocks(0, total, 192));
  for (const g of gaps) for (const b of spreadBlocks(g.start, g.end, 48)) plan.add(b);
  for (const b of spreadBlocks(0, Math.min(1024 * 1024, total), 16)) plan.add(b);
  for (const b of spreadBlocks(Math.max(0, total - 1024 * 1024), total, 16)) plan.add(b);

  const indexes = [...plan].sort((a, b) => a - b);
  const blocks = await readBlocks(devName, indexes);
  const readings: BlockReading[] = blocks.map((b, i) => classifyBlock(b, indexes[i] * PROBE_BLOCK));

  const counts: Record<BlockKind, number> = { zero: 0, ones: 0, text: 0, compressed: 0, structured: 0 };
  for (const r of readings) counts[r.kind]++;

  const sigMap = new Map<string, number[]>();
  for (const r of readings) {
    for (const name of r.signatures) {
      const list = sigMap.get(name) || [];
      if (list.length < 8) list.push(r.offset);
      sigMap.set(name, list);
    }
  }

  const sampled = readings.length || 1;
  const emptyBlocks = counts.zero + counts.ones;
  // Never round up to 100% while any sampled block still holds data — "100% empty"
  // next to "1 block with data" is worse than useless.
  const emptyPct = emptyBlocks === sampled ? 100 : Math.min(99, Math.round((emptyBlocks / sampled) * 100));
  const inGaps = readings.filter((r) => gaps.some((g) => r.offset >= g.start && r.offset < g.end));
  const gapsWithData = inGaps.filter((r) => r.kind !== "zero" && r.kind !== "ones").length;

  return {
    ok: true,
    device: devName,
    sizeBytes: total,
    sampledBlocks: sampled,
    blockSize: PROBE_BLOCK,
    bytesRead: sampled * PROBE_BLOCK,
    counts,
    emptyPct,
    unallocated: {
      ranges: gaps.map((g) => ({ start: g.start, end: g.end, bytes: g.end - g.start })),
      sampled: inGaps.length,
      withData: gapsWithData,
    },
    signatures: [...sigMap.entries()].map(([name, offsets]) => ({ name, offsets })),
    // Coarse strip for the UI: kind per sample, in device order.
    map: readings.map((r) => ({ o: r.offset, k: r.kind, e: r.entropy })),
    message:
      `Sampled ${sampled} blocks (${fmtBytes(sampled * PROBE_BLOCK)}) — ` +
      `${emptyPct}% empty` +
      (gapsWithData ? `, ${gapsWithData} block(s) with data outside any partition` : "") +
      (sigMap.size ? `, ${sigMap.size} signature type(s) found` : ""),
  };
}

// Exhaustive pass. Runs as a tracked Operation so it reports progress over SSE and can
// be cancelled like an erase or a flash.
async function doDeepScan(op: Operation): Promise<void> {
  const dev = op.device;
  const total = await getDeviceSizeBytes(dev);
  op.status = "scanning";
  op.progress = 0;
  op.message = "Scanning raw device...";

  const proc = Bun.spawn(["sudo", "dd", `if=/dev/${dev}`, "bs=4M", "status=none"], { stdout: "pipe", stderr: "pipe" });
  op.process = proc;

  const hits = new Map<string, { count: number; first: number[] }>();
  let pos = 0;
  let carry = new Uint8Array(0);

  for await (const chunk of proc.stdout) {
    if (op.aborted) { proc.kill(); break; }
    const buf = new Uint8Array(carry.length + chunk.length);
    buf.set(carry, 0);
    buf.set(chunk, carry.length);
    const base = pos - carry.length;

    // Anything starting inside the trailing overlap is deferred to the next chunk, so a
    // signature straddling a chunk boundary is found exactly once.
    const carryLen = Math.min(buf.length, MAX_SIG_LEN - 1);
    const boundary = base + buf.length - carryLen;
    for (const h of findSignatures(buf, base)) {
      if (h.offset >= boundary) continue;
      const e = hits.get(h.name) || { count: 0, first: [] };
      e.count++;
      if (e.first.length < 10) e.first.push(h.offset);
      hits.set(h.name, e);
    }

    pos += chunk.length;
    carry = new Uint8Array(buf.subarray(buf.length - carryLen));
    op.progress = total ? Math.min(99, Math.round((pos / total) * 100)) : 0;
    op.message = `Scanned ${fmtBytes(pos)}${total ? ` / ${fmtBytes(total)}` : ""} — ${hits.size} signature type(s)`;
  }

  await proc.exited;
  if (op.aborted) throw new Error("Cancelled");

  op.scanResult = {
    bytesScanned: pos,
    signatures: [...hits.entries()]
      .map(([name, v]) => ({ name, count: v.count, first: v.first }))
      .sort((a, b) => b.count - a.count),
  };
  // Operations are reaped 30s after completion; keep the findings retrievable after that.
  lastScanResults.set(dev, { ...op.scanResult, device: dev, at: Date.now() });
  op.progress = 100;
  op.status = "done";
  op.message = hits.size
    ? `Found ${hits.size} signature type(s) in ${fmtBytes(pos)}`
    : `No known signatures in ${fmtBytes(pos)} — device looks genuinely blank`;
}

// ==================== Erase ====================

async function doErase(op: Operation): Promise<void> {
  op.status = "preparing";
  op.message = "Unmounting...";
  await prepareDevice(op.device);
  op.status = "erasing";
  const mode = op.mode || "quick";

  if (mode === "quick") {
    op.message = "Wiping signatures...";
    await run(["sudo", "wipefs", "-a", `/dev/${op.device}`]);
    op.progress = 30;
    op.message = "Zeroing partition table...";
    await run(["sudo", "dd", "if=/dev/zero", `of=/dev/${op.device}`, "bs=1M", "count=10", "conv=fsync"]);
    op.progress = 60;
    const { out: sz } = await run(["sudo", "blockdev", "--getsz", `/dev/${op.device}`]);
    const sectors = parseInt(sz);
    if (sectors > 20480) {
      op.message = "Zeroing GPT backup...";
      await run(["sudo", "dd", "if=/dev/zero", `of=/dev/${op.device}`, "bs=512", `seek=${sectors - 20480}`, "count=20480", "conv=fsync"]);
    }
    op.progress = 100;
    op.status = "done";
    op.message = "Quick erase complete";
  } else if (mode === "full") {
    const { out: sizeStr } = await run(["sudo", "blockdev", "--getsize64", `/dev/${op.device}`]);
    const totalBytes = parseInt(sizeStr) || 0;
    op.message = `Writing zeros to ${fmtBytes(totalBytes)}...`;
    const proc = Bun.spawn(["sudo", "dd", "if=/dev/zero", `of=/dev/${op.device}`, "bs=4M", "conv=fsync", "status=progress"], { stdout: "pipe", stderr: "pipe" });
    op.process = proc;
    const reader = proc.stderr.getReader();
    const dec = new TextDecoder();
    let buf = "";
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      if (op.aborted) { proc.kill(); break; }
      buf += dec.decode(value, { stream: true });
      const ms = [...buf.matchAll(/(\d+) bytes/g)];
      if (ms.length && totalBytes) {
        const c = parseInt(ms[ms.length - 1][1]);
        op.progress = Math.round((c / totalBytes) * 100);
        op.message = `Erasing: ${fmtBytes(c)} / ${fmtBytes(totalBytes)} (${op.progress}%)`;
      }
      if (buf.length > 4096) buf = buf.slice(-2048);
    }
    await proc.exited;
    if (op.aborted) { op.status = "error"; op.message = "Cancelled"; return; }
    op.progress = 100;
    op.status = "done";
    op.message = "Full erase complete";
  } else if (mode === "discard") {
    op.message = "Discarding (TRIM)...";
    const { code, err } = await run(["sudo", "blkdiscard", "-f", `/dev/${op.device}`]);
    if (code !== 0) {
      const msg = err.includes("not supported") || err.includes("not permitted")
        ? "Device does not support TRIM/discard (only SSDs do). Use Quick or Full erase instead."
        : `Discard failed: ${err}`;
      op.status = "error"; op.message = msg; return;
    }
    op.progress = 100;
    op.status = "done";
    op.message = "Discard complete";
  }

  if (op.status === "done") await run(["sudo", "partprobe", `/dev/${op.device}`]);
}

// ==================== Flash ====================

async function doFlash(op: Operation): Promise<void> {
  const imageUrl = op.imageUrl!;
  const filename = imageUrl.split("/").pop()!;
  const dlPath = `${FLASH_DIR}/${filename}`;

  await run(["mkdir", "-p", FLASH_DIR]);

  // Download
  op.status = "downloading";
  op.message = "Starting download...";
  const resp = await fetch(imageUrl);
  if (!resp.ok) throw new Error(`Download failed: ${resp.status}`);
  const totalDl = parseInt(resp.headers.get("content-length") || "0");
  const writer = Bun.file(dlPath).writer();
  let downloaded = 0;
  for await (const chunk of resp.body!) {
    if (op.aborted) throw new Error("Cancelled");
    writer.write(chunk);
    downloaded += chunk.length;
    op.progress = totalDl > 0 ? Math.round((downloaded / totalDl) * 40) : 0;
    op.message = `Downloading: ${fmtBytes(downloaded)}${totalDl ? ` / ${fmtBytes(totalDl)}` : ""}`;
  }
  await writer.end();

  // Prepare
  op.status = "preparing";
  op.progress = 40;
  op.message = "Unmounting...";
  await prepareDevice(op.device);

  // Flash
  op.status = "flashing";
  op.message = "Writing image...";
  let cmd: string;
  if (filename.endsWith(".xz")) cmd = `xzcat "${dlPath}" | sudo dd of=/dev/${op.device} bs=4M conv=fsync status=progress`;
  else if (filename.endsWith(".gz")) cmd = `gzip -dc "${dlPath}" | sudo dd of=/dev/${op.device} bs=4M conv=fsync status=progress`;
  else if (filename.endsWith(".zip")) cmd = `unzip -p "${dlPath}" "*.img" | sudo dd of=/dev/${op.device} bs=4M conv=fsync status=progress`;
  else cmd = `sudo dd if="${dlPath}" of=/dev/${op.device} bs=4M conv=fsync status=progress`;

  // pipefail: without it a decompressor dying mid-stream (truncated download) leaves dd
  // exiting 0 on a partial image, and the flash reports success.
  const flashProc = Bun.spawn(["bash", "-o", "pipefail", "-c", cmd], { stdout: "pipe", stderr: "pipe" });
  op.process = flashProc;
  const reader = flashProc.stderr.getReader();
  const dec = new TextDecoder();
  let buf = "";
  const exSize = op.extractSize || 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    if (op.aborted) { flashProc.kill(); break; }
    buf += dec.decode(value, { stream: true });
    const ms = [...buf.matchAll(/(\d+) bytes/g)];
    if (ms.length && exSize) {
      const c = parseInt(ms[ms.length - 1][1]);
      op.progress = 40 + Math.round((c / exSize) * 45);
      op.message = `Flashing: ${fmtBytes(c)} / ${fmtBytes(exSize)}`;
    }
    if (buf.length > 4096) buf = buf.slice(-2048);
  }
  const exit = await flashProc.exited;
  if (op.aborted) throw new Error("Cancelled");
  if (exit !== 0) throw new Error("Flash failed");

  // Configure
  op.status = "configuring";
  op.progress = 85;
  await run(["sudo", "partprobe", `/dev/${op.device}`]);
  await Bun.sleep(2000);
  let applied = "";
  if (op.settings && Object.keys(op.settings).length) {
    op.message = "Applying settings...";
    try {
      applied = await applySettings(op.device, op.settings, op.initFormat);
    } catch (e: any) {
      // The image itself is good; say so, so nobody re-flashes to fix a settings problem.
      op.status = "error";
      op.message = `Image written, but settings were NOT applied: ${e.message}`;
      await run(["rm", "-f", dlPath]);
      return;
    }
  }

  op.progress = 100;
  op.status = "done";
  op.message = applied ? `Flash complete! ${applied}` : "Flash complete!";
  await run(["rm", "-f", dlPath]);
}

// ==================== Flash Customization ====================
//
// How settings reach a freshly written image depends on the image. The catalog's
// `init_format` says which mechanism the OS reads on first boot. Writing the wrong one is
// silently ignored, and the Pi boots unconfigured with no SSH. Raspberry Pi OS trixie
// is `cloudinit-rpi` and ignores firstrun.sh. Bookworm/Legacy is `systemd`.
// The generators follow Raspberry Pi Imager 2.x (src/customization_generator.cpp,
// src/downloadthread.cpp) so a BenchBob flash boots the same as an Imager one.

type InitFormat = "systemd" | "cloudinit" | "cloudinit-rpi" | "rpi-preseed" | "none";

function partDev(devName: string, n: number): string {
  // sda → sda1, but nvme0n1 / mmcblk0 / loop0 → nvme0n1p1
  return `/dev/${devName}${/\d$/.test(devName) ? "p" : ""}${n}`;
}

// Write a root-owned file without the content ever touching a world-readable temp file
// (these carry password hashes and Wi-Fi keys).
async function sudoWrite(path: string, content: string): Promise<void> {
  const proc = Bun.spawn(["sudo", "tee", path], { stdin: "pipe", stdout: "ignore", stderr: "pipe" });
  proc.stdin.write(content);
  proc.stdin.end();
  const [err, code] = await Promise.all([new Response(proc.stderr).text(), proc.exited]);
  if (code !== 0) throw new Error(`writing ${path.split("/").pop()}: ${err.trim() || `exit ${code}`}`);
}

async function hashPassword(pw: string): Promise<string> {
  // Pasted passwords can carry a trailing newline that PAM never sends — strip CR/LF as
  // Imager does, or the hash can never match. Fed on stdin so it never shows in `ps`.
  const proc = Bun.spawn(["openssl", "passwd", "-6", "-stdin"], { stdin: "pipe", stdout: "pipe", stderr: "pipe" });
  proc.stdin.write(pw.replace(/[\r\n]/g, "") + "\n");
  proc.stdin.end();
  const [out, code] = await Promise.all([new Response(proc.stdout).text(), proc.exited]);
  const hash = out.trim();
  if (code !== 0 || !hash.startsWith("$")) throw new Error("could not hash the password");
  return hash;
}

function settingsKeys(s: any): string[] {
  const keys: string[] = Array.isArray(s.sshKeys) ? s.sshKeys : s.sshKeys ? String(s.sshKeys).split(/\r?\n/) : [];
  return keys.map((k) => String(k).trim()).filter((k) => k.length > 0);
}

function settingsWifi(s: any): { ssid: string; password: string; country: string; priority: number }[] {
  const nets: any[] = Array.isArray(s.wifiNetworks) ? [...s.wifiNetworks] : [];
  // Legacy single-network compat
  if (nets.length === 0 && s.configureWifi && s.wifiSsid) {
    nets.push({ ssid: s.wifiSsid, password: s.wifiPassword || "", country: s.wifiCountry || "US", priority: 0 });
  }
  return nets
    .filter((w) => w && w.ssid)
    .map((w) => ({ ssid: String(w.ssid), password: String(w.password || "").replace(/[\r\n]/g, ""), country: String(w.country || "US"), priority: Number(w.priority) || 0 }));
}

// cmdline.txt is one line of space-separated kernel parameters, so the Wi-Fi country
// lands there only if it is exactly two letters — anything else could inject parameters.
function countryCode(cc: string): string {
  const t = (cc || "").trim().toUpperCase();
  return /^[A-Z]{2}$/.test(t) ? t : "";
}

// A JSON string literal is a valid YAML double-quoted scalar, escapes included.
const yq = (v: string) => JSON.stringify(String(v));

async function applySettings(devName: string, s: any, initFormat: InitFormat | undefined): Promise<string> {
  await run(["sudo", "mkdir", "-p", BOOT_MOUNT]);
  const bootDev = partDev(devName, 1);
  const m = await run(["sudo", "mount", bootDev, BOOT_MOUNT]);
  if (m.code !== 0) throw new Error(`could not mount boot partition ${bootDev}: ${m.err || `exit ${m.code}`}`);
  try {
    const fmt = initFormat ?? (await detectInitFormat());
    if (fmt === "cloudinit" || fmt === "cloudinit-rpi") return await applyCloudInit(s);
    if (fmt === "systemd") return await applyFirstrun(s);
    if (fmt === "rpi-preseed") throw new Error("this image expects rpi-preseed customization, which BenchBob does not support yet");
    return "This image does not support customization; settings were not applied.";
  } finally {
    await run(["sudo", "umount", BOOT_MOUNT]);
  }
}

// Only used when the image isn't in the catalog (so has no init_format): cloud-init
// images ship NoCloud seed files on bootfs; anything else with a cmdline.txt is assumed
// to be a firstrun.sh-era Raspberry Pi OS.
async function detectInitFormat(): Promise<InitFormat> {
  if (await Bun.file(`${BOOT_MOUNT}/meta-data`).exists() || await Bun.file(`${BOOT_MOUNT}/user-data`).exists()) return "cloudinit";
  if (await Bun.file(`${BOOT_MOUNT}/cmdline.txt`).exists()) return "systemd";
  return "none";
}

async function appendCmdline(params: string): Promise<void> {
  const { out: cmdline, code } = await run(["sudo", "cat", `${BOOT_MOUNT}/cmdline.txt`]);
  if (code !== 0 || !cmdline) throw new Error("could not read cmdline.txt");
  await sudoWrite(`${BOOT_MOUNT}/cmdline.txt`, cmdline.trim() + params + "\n");
}

async function applyCloudInit(s: any): Promise<string> {
  const user = await mkCloudInitUserData(s);
  const net = mkCloudInitNetworkConfig(s);
  // A fresh instance-id per write makes cloud-init treat this as a new machine and run
  // first-boot setup; the stub meta-data in the image has a fixed one.
  const instanceId = `benchbob-${Date.now()}`;
  await sudoWrite(`${BOOT_MOUNT}/meta-data`, `instance-id: ${instanceId}\n`);
  await sudoWrite(`${BOOT_MOUNT}/user-data`, "#cloud-config\n" + user);
  const written = ["user-data", "meta-data"];
  if (net) {
    await sudoWrite(`${BOOT_MOUNT}/network-config`, net);
    written.push("network-config");
  }
  // ds= lets cloud-init validate its cache from the cmdline instead of rediscovering
  // the datasource on every boot.
  let params = ` ds=nocloud;i=${instanceId}`;
  const cc = countryCode(settingsWifi(s)[0]?.country || "");
  if (cc) params += ` cfg80211.ieee80211_regdom=${cc}`;
  await appendCmdline(params);
  written.push("cmdline.txt");
  return `Settings applied via cloud-init (${written.join(", ")}).`;
}

async function mkCloudInitUserData(s: any): Promise<string> {
  const L: string[] = ["manage_resolv_conf: false", ""];
  const hostname = String(s.hostname || "").trim();
  if (hostname) {
    L.push(`hostname: ${yq(hostname)}`, "manage_etc_hosts: true");
    // Parity with Imager: avahi for <hostname>.local, and don't let a first-boot clock
    // that hasn't synced yet make apt reject the repo signatures.
    L.push("packages:", "- avahi-daemon", "", "apt:", "  preserve_sources_list: true", "  conf: |",
      "    Acquire {", '      Check-Date "false";', "    };", "");
  }
  if (s.timezone) L.push(`timezone: ${yq(String(s.timezone).trim())}`);
  if (s.keyboardLayout) L.push("keyboard:", "  model: pc105", `  layout: ${yq(String(s.keyboardLayout).trim())}`);

  const username = String(s.username || "").trim() || "pi";
  const keys = s.enableSsh ? settingsKeys(s) : [];
  const passwordless = !!s.passwordlessSudo;
  // With neither a password nor a key the account would be unusable, so leave the
  // image's default user alone.
  if (s.password || keys.length) {
    // Singular `user:` renames/extends the distro's default user, keeping its groups
    // (sudo, gpio, video, ...). The plural `users:` list would make a group-less account.
    L.push("", "user:", `  name: ${yq(username)}`, "  shell: /bin/bash");
    if (s.password) L.push("  lock_passwd: false", `  passwd: ${yq(await hashPassword(String(s.password)))}`);
    else L.push("  lock_passwd: true");
    if (keys.length) {
      L.push("  ssh_authorized_keys:");
      for (const k of keys) L.push(`    - ${yq(k)}`);
    }
    // Must be explicit: the default user in /etc/cloud/cloud.cfg carries NOPASSWD sudo,
    // and `user:` is merged over it. `null` drops that rule but keeps the sudo group.
    L.push(passwordless ? "  sudo: ALL=(ALL) NOPASSWD:ALL" : "  sudo: null", "");
  }

  const runcmd: string[] = [];
  if (s.enableSsh) {
    L.push(`ssh_pwauth: ${s.password ? "true" : "false"}`);
    // cc_ssh writes host keys and authorized_keys but never enables sshd, and Raspberry
    // Pi OS ships it disabled.
    runcmd.push("  - [ systemctl, enable, --now, ssh ]");
  }
  if (passwordless) {
    // `sudo:` above isn't honoured by every cloud-init build; Imager writes the file too.
    const file = `/etc/sudoers.d/010_${username}-nopasswd`;
    runcmd.push(`  - [ sh, -c, ${yq(`echo '${shellEsc(`${username} ALL=(ALL) NOPASSWD:ALL`)}' >'${shellEsc(file)}' && chmod 0440 '${shellEsc(file)}'`)} ]`);
  }
  if (runcmd.length) L.push("", "runcmd:", ...runcmd);
  return L.join("\n") + "\n";
}

function mkCloudInitNetworkConfig(s: any): string {
  const nets = settingsWifi(s);
  if (!nets.length) return "";
  // Highest priority first; netplan has no per-AP priority, but order is the best hint.
  nets.sort((a, b) => b.priority - a.priority);
  const cc = countryCode(nets[0].country);
  // eth0 is listed too: once a network-config is supplied it replaces the distro default,
  // and wired Ethernet would otherwise stop coming up.
  const L = ["network:", "  version: 2", "  ethernets:", "    eth0:", "      dhcp4: true", "      dhcp6: true", "      optional: true",
    "  wifis:", "    wlan0:", "      dhcp4: true"];
  if (cc) L.push(`      regulatory-domain: ${yq(cc)}`);
  L.push("      access-points:");
  for (const w of nets) {
    L.push(`        ${yq(w.ssid)}:`);
    if (w.password) L.push(`          password: ${yq(w.password)}`);
    else L.push("          auth:", "            key-management: none");
  }
  L.push("      optional: true");
  return L.join("\n") + "\n";
}

async function applyFirstrun(s: any): Promise<string> {
  if (s.enableSsh) await sudoWrite(`${BOOT_MOUNT}/ssh`, "");
  if (s.username) {
    // No password: create the user with a locked password (SSH key-only)
    const hash = s.password ? await hashPassword(String(s.password)) : "*";
    await sudoWrite(`${BOOT_MOUNT}/userconf.txt`, `${s.username}:${hash}\n`);
  }
  await sudoWrite(`${BOOT_MOUNT}/firstrun.sh`, mkFirstrun(s));
  const { out: cmdline } = await run(["sudo", "cat", `${BOOT_MOUNT}/cmdline.txt`]);
  if (!cmdline.includes("firstrun.sh")) {
    // Must stay last on the line: firstrun.sh deletes from " systemd.run" to the end.
    await appendCmdline(" systemd.run=/boot/firmware/firstrun.sh systemd.run_success_action=reboot systemd.unit=kernel-command-line.target");
  }
  return "Settings applied via firstrun.sh.";
}

function mkFirstrun(s: any): string {
  const hn = shellEsc(s.hostname || "raspberrypi");
  let sc = `#!/bin/bash\nset +e\nCURRENT_HOSTNAME=$(cat /etc/hostname | tr -d " \\t\\n\\r")\n`;

  // Hostname
  sc += `\nif [ -f /usr/lib/raspberrypi-sys-mods/imager_custom ]; then\n  /usr/lib/raspberrypi-sys-mods/imager_custom set_hostname '${hn}'\nelse\n  echo '${hn}' >/etc/hostname\n  sed -i "s/127.0.1.1.*$CURRENT_HOSTNAME/127.0.1.1\\t${hn}/g" /etc/hosts\nfi\n`;

  // SSH
  if (s.enableSsh) {
    sc += `\nif [ -f /usr/lib/raspberrypi-sys-mods/imager_custom ]; then\n  /usr/lib/raspberrypi-sys-mods/imager_custom enable_ssh\nelse\n  systemctl enable ssh\nfi\n`;
    if (!s.password) {
      sc += `\nsed -i 's/^#*PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config\n`;
    }
  }

  // SSH authorized keys
  const validKeys = settingsKeys(s);
  const user = shellEsc(s.username || "pi");
  if (validKeys.length > 0) {
    sc += `\nFIRSTUSERHOME=$(getent passwd '${user}' | cut -d: -f6)\nif [ -z "$FIRSTUSERHOME" ]; then FIRSTUSERHOME="/home/${user}"; fi\nmkdir -p "$FIRSTUSERHOME/.ssh"\nchmod 700 "$FIRSTUSERHOME/.ssh"\ncat >> "$FIRSTUSERHOME/.ssh/authorized_keys" <<'SSHEOF'\n`;
    for (const key of validKeys) sc += `${key}\n`;
    sc += `SSHEOF\nchmod 600 "$FIRSTUSERHOME/.ssh/authorized_keys"\nchown -R '${user}:${user}' "$FIRSTUSERHOME/.ssh"\n`;
  }

  // Passwordless sudo (opt-in; Raspberry Pi OS prompts by default)
  if (s.passwordlessSudo) {
    sc += `\necho '${user} ALL=(ALL) NOPASSWD:ALL' >'/etc/sudoers.d/010_${user}-nopasswd'\nchmod 0440 '/etc/sudoers.d/010_${user}-nopasswd'\n`;
  }

  // WiFi networks (supports array)
  const wifiNets = settingsWifi(s);
  for (let i = 0; i < wifiNets.length; i++) {
    const w = wifiNets[i];
    const ssid = shellEsc(w.ssid);
    const psk = shellEsc(w.password);
    const cc = shellEsc(w.country);
    const connName = `wifi-${i}`;
    sc += `\nif [ -f /usr/lib/raspberrypi-sys-mods/imager_custom ]; then\n  /usr/lib/raspberrypi-sys-mods/imager_custom set_wlan '${ssid}' '${psk}' '${cc}'\nelse\n  cat > '/etc/NetworkManager/system-connections/${connName}.nmconnection' <<'NMEOF'\n[connection]\nid=${w.ssid}\ntype=wifi\nautoconnect-priority=${w.priority}\n[wifi]\nmode=infrastructure\nssid=${w.ssid}\n[wifi-security]\nauth-alg=open\nkey-mgmt=wpa-psk\npsk=${w.password}\n[ipv4]\nmethod=auto\n[ipv6]\nmethod=auto\nNMEOF\n  chmod 600 '/etc/NetworkManager/system-connections/${connName}.nmconnection'\nfi\n`;
  }

  // Timezone
  if (s.timezone) sc += `\nif [ -f /usr/lib/raspberrypi-sys-mods/imager_custom ]; then\n  /usr/lib/raspberrypi-sys-mods/imager_custom set_timezone '${shellEsc(s.timezone)}'\nelse\n  rm -f /etc/localtime\n  echo '${shellEsc(s.timezone)}' >/etc/timezone\n  dpkg-reconfigure -f noninteractive tzdata\nfi\n`;

  // Keyboard
  if (s.keyboardLayout) sc += `\nif [ -f /usr/lib/raspberrypi-sys-mods/imager_custom ]; then\n  /usr/lib/raspberrypi-sys-mods/imager_custom set_keymap '${shellEsc(s.keyboardLayout)}' '${shellEsc(s.keyboardLayout)}'\nfi\n`;

  // Cleanup
  sc += `\nrm -f /boot/firmware/firstrun.sh\nsed -i 's| systemd.run.*||g' /boot/firmware/cmdline.txt\nexit 0\n`;
  return sc;
}

// ==================== USB Boot Mode (rpiboot) ====================
//
// Operations on a device in USB boot mode are keyed `usb:<port path>` in the normal op
// queue, so one device never has two rpiboot runs at once. See rpiboot.ts.

interface RpibootResult {
  path: string;
  chip: rpi.Chip;
  at: number;
  kind: "eeprom-check" | "eeprom-write";
  ok: boolean;
  message: string;
  metadata: Record<string, string> | null;
  board: ReturnType<typeof rpi.decodeBoardRev>;
  installed: rpi.Installed | null;
}

// Kept after the op itself is gone: the device leaves boot mode when recovery.bin is
// done, and the board details are the point of a check.
const rpibootResults = new Map<string, RpibootResult>();

async function doRpiboot(op: Operation): Promise<void> {
  const dev = (await rpi.getBootDevices()).find((d) => d.path === op.usbPath);
  if (!dev) throw new Error("Device is no longer in USB boot mode");
  const log: string[] = [];
  const hooks: rpi.RunHooks = {
    onLine: (l) => { log.push(l); op.message = l.length > 160 ? l.slice(0, 160) + "…" : l; },
    onSpawn: (proc) => { op.process = proc; },
    isAborted: () => !!op.aborted,
  };
  op.status = "running";
  op.progress = 10;
  const tail = () => log.slice(-3).join(" | ") || "no output";

  if (op.type === "msd") {
    op.message = "Loading mass-storage gadget...";
    const r = await rpi.loadMassStorage(dev, hooks);
    if (op.aborted) throw new Error("Cancelled");
    // The gadget is up once the boot ROM device has gone away; rpiboot's exit code
    // alone doesn't say that.
    await Bun.sleep(1500);
    const still = (await rpi.getBootDevices()).some((d) => d.path === dev.path);
    if (r.timedOut || (r.code !== 0 && still)) throw new Error(`rpiboot ${r.timedOut ? "timed out" : `failed (exit ${r.code})`}: ${tail()}`);
    op.progress = 100;
    op.status = "done";
    op.message = "Mass-storage gadget loaded. The module's storage should appear under Connected Drives within a few seconds.";
    return;
  }

  const kind = op.type as "eeprom-check" | "eeprom-write";
  let r: rpi.RpibootRun;
  let image: rpi.EepromImage | undefined;
  if (kind === "eeprom-check") {
    op.message = "Reading bootloader metadata...";
    r = await rpi.readBootloaderInfo(dev, hooks);
  } else {
    image = (await rpi.eepromImages(dev.chip)).find((i) => i.id === op.eepromImageId);
    if (!image) throw new Error("Unknown bootloader image");
    op.message = `Writing bootloader ${image.label}...`;
    r = await rpi.writeBootloader(dev, image, op.bootConf || "", hooks);
  }
  if (op.aborted) throw new Error("Cancelled");

  // Remember what was sent unless the module said it failed. A hash match later is
  // proof on its own, so recording a write whose outcome is unknown can't mislead.
  if (image && r.imageSha256 && (!r.metadata || r.metadata.EEPROM_UPDATE === "success")) {
    await rpi.recordWrite({
      at: Date.now(), chip: dev.chip, mac: r.metadata?.MAC_ADDR || null, imageLabel: image.label, buildDate: image.buildDate,
      bootConf: op.bootConf || "", hashes: [r.imageSha256, r.metadata?.EEPROM_HASH || ""],
    }).catch((e) => console.error("could not record bootloader write:", e));
  }

  const md = r.metadata;
  const board = rpi.decodeBoardRev(md?.USER_BOARDREV);
  const why = r.timedOut ? "timed out" : `rpiboot exit ${r.code}`;
  let ok: boolean;
  let message: string;
  if (kind === "eeprom-check") {
    ok = !!md;
    message = md
      ? `Read metadata${board ? ` from a ${board.model} (${board.memory})` : ""}. Nothing was written. Power-cycle the module to leave recovery.`
      : `No metadata came back (${why}): ${tail()}`;
  } else {
    const upd = md?.EEPROM_UPDATE;
    ok = upd === "success";
    message = ok
      ? "Bootloader EEPROM written and verified. Power-cycle the module (remove the nRPIBOOT jumper to boot normally)."
      : upd
        ? `Bootloader update reported "${upd}". The old bootloader is most likely still in place.`
        : `Outcome unknown, no metadata came back (${why}). A rapidly flashing green LED on the module means success. Last output: ${tail()}`;
  }
  const installed = md ? await rpi.identifyBootloader(dev.chip, md) : null;
  rpibootResults.set(dev.path, { path: dev.path, chip: dev.chip, at: Date.now(), kind, ok, message, metadata: md, board, installed });
  op.progress = 100;
  op.status = ok ? "done" : "error";
  op.message = message;
}

const RPIBOOT_OPS: Record<string, "msd" | "eeprom-check" | "eeprom-write"> = {
  "/api/rpiboot/msd": "msd",
  "/api/rpiboot/eeprom/check": "eeprom-check",
  "/api/rpiboot/eeprom/write": "eeprom-write",
};

async function rpibootRoute(p: string, req: Request, url: URL): Promise<Response> {
  if (p === "/api/rpiboot/eeprom/options") {
    const dev = (await rpi.getBootDevices()).find((d) => d.path === url.searchParams.get("path"));
    if (!dev) return Response.json({ ok: false, message: "No such device in USB boot mode" }, { status: 404 });
    // A check (or write) on this port since it was last power-cycled is the only source
    // of what's installed; see identifyBootloader.
    const last = rpibootResults.get(dev.path);
    return Response.json({
      ok: true, chip: dev.chip, images: await rpi.eepromImages(dev.chip), bootConf: await rpi.defaultBootConf(dev.chip),
      installed: last?.chip === dev.chip ? last.installed : null, checkedAt: last?.chip === dev.chip ? last.at : null,
    });
  }
  if (p === "/api/rpiboot/results/clear" && req.method === "POST") {
    const { path } = await req.json();
    rpibootResults.delete(path);
    return Response.json({ ok: true });
  }
  const type = RPIBOOT_OPS[p];
  if (!type || req.method !== "POST") return new Response("Not Found", { status: 404 });
  if (!(await rpi.rpibootInstalled())) return Response.json({ ok: false, message: "rpiboot is not installed (sudo apt install rpiboot)" }, { status: 501 });

  const { path, imageId, bootConf } = await req.json();
  const dev = (await rpi.getBootDevices()).find((d) => d.path === path);
  if (!dev) return Response.json({ ok: false, message: "No such device in USB boot mode" }, { status: 404 });
  if (type !== "msd" && dev.chip === "2835") return Response.json({ ok: false, message: "CM1/CM3 have no bootloader EEPROM" }, { status: 400 });
  if (type === "eeprom-write") {
    if (!(await rpi.eepromImages(dev.chip)).some((i) => i.id === imageId)) return Response.json({ ok: false, message: "Unknown bootloader image" }, { status: 400 });
    if (typeof bootConf !== "string" || !bootConf.trim() || bootConf.length > 4000) return Response.json({ ok: false, message: "boot.conf is missing or too long" }, { status: 400 });
  }
  const key = `usb:${dev.path}`;
  if (isDeviceBusy(key) || getOpsForDevice(key).some((o) => o.status === "queued")) return Response.json({ ok: false, message: "Device busy" }, { status: 409 });
  const op: Operation = { id: crypto.randomUUID(), type, device: key, status: "starting", progress: 0, message: "Starting...", usbPath: dev.path, eepromImageId: imageId, bootConf };
  enqueue(op);
  return Response.json({ ok: true, message: "Started", operationId: op.id });
}

// ==================== Bootloader update SD cards ====================
//
// A drive operation like erase/flash; rpiboot.ts buildCardFiles says what goes on the
// card. Laid out like the official image (MBR, one FAT32 partition at 1 MiB with 512-byte
// clusters) but sized to fit the card, and capped at the official 256 MiB because
// recovery.bin wants a FAT partition under 32 GB.

const CARD_MOUNT = "/tmp/benchbob-blcard";
const MiB = 1024 * 1024;
const CARD_PART_MAX = 256 * MiB;
const CARD_PART_MIN = 8 * MiB;

async function mustRun(cmd: string[], stdin?: string): Promise<string> {
  const proc = Bun.spawn(cmd, { stdin: stdin === undefined ? "ignore" : "pipe", stdout: "pipe", stderr: "pipe" });
  if (stdin !== undefined) {
    proc.stdin!.write(stdin);
    proc.stdin!.end();
  }
  const [out, err, code] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
  const name = cmd[0] === "sudo" ? cmd[1] : cmd[0];
  if (code !== 0) throw new Error(`${name} failed: ${err.trim().split("\n").at(-1) || `exit ${code}`}`);
  return out;
}

async function unmountAll(part: string): Promise<void> {
  const { out } = await run(["lsblk", "-nro", "MOUNTPOINT", part]);
  for (const mp of out.split("\n").filter(Boolean)) await mustRun(["sudo", "umount", mp]);
}

async function doBootloaderCard(op: Operation): Promise<void> {
  const chip = op.chip!;
  const image = (await rpi.eepromImages(chip)).find((i) => i.id === op.eepromImageId);
  if (!image) throw new Error("Unknown bootloader image");
  const dev = `/dev/${op.device}`;
  const part = partDev(op.device, 1);
  const cancelled = () => { if (op.aborted) throw new Error("Cancelled"); };

  op.status = "preparing";
  op.message = "Building bootloader files...";
  const card = await rpi.buildCardFiles(chip, image, op.bootConf || "");
  try {
    op.progress = 10;
    op.message = "Unmounting...";
    await prepareDevice(op.device);
    cancelled();

    op.status = "writing";
    op.progress = 20;
    op.message = "Partitioning...";
    const total = await getDeviceSizeBytes(op.device);
    const size = Math.min(CARD_PART_MAX, Math.floor((total - MiB) / MiB) * MiB);
    if (size < CARD_PART_MIN) throw new Error(`Card too small (${fmtBytes(total)})`);
    await mustRun(["sudo", "wipefs", "-a", dev]);
    await mustRun(["sudo", "sfdisk", "--wipe", "always", "--wipe-partitions", "always", dev], `label: dos\nstart=2048, size=${size / 512}, type=c\n`);
    await run(["sudo", "partprobe", dev]);
    await run(["sudo", "udevadm", "settle"]);
    for (let i = 0; i < 50 && (await run(["test", "-b", part])).code !== 0; i++) await Bun.sleep(100);
    cancelled();

    op.progress = 35;
    op.message = "Formatting...";
    // FAT32 needs 65525 clusters, about 33 MiB of 512-byte clusters; FAT16 below that.
    const fat = size >= 64 * MiB ? ["-F", "32", "-s", "1"] : ["-F", "16"];
    await mustRun(["sudo", "mkfs.vfat", ...fat, "-n", "BOOTLOADER", part]);
    cancelled();

    op.progress = 50;
    op.message = "Copying files...";
    await run(["sudo", "mkdir", "-p", CARD_MOUNT]);
    await unmountAll(part); // udev may have auto-mounted the new filesystem already
    await mustRun(["sudo", "mount", "-t", "vfat", part, CARD_MOUNT]);
    try {
      for (const f of card.files) await mustRun(["sudo", "cp", `${card.dir}/${f}`, `${CARD_MOUNT}/${f}`]);
    } finally {
      await run(["sudo", "umount", CARD_MOUNT]);
    }
    cancelled();

    // Read back from the card itself, not the page cache.
    op.progress = 75;
    op.message = "Verifying...";
    await unmountAll(part);
    await mustRun(["sudo", "blockdev", "--flushbufs", part]);
    await mustRun(["sudo", "mount", "-o", "ro", "-t", "vfat", part, CARD_MOUNT]);
    try {
      const out = await mustRun(["sudo", "sha256sum", ...card.files.map((f) => `${CARD_MOUNT}/${f}`)]);
      const onCard = new Map(out.trim().split("\n").map((l) => [l.slice(l.lastIndexOf("/") + 1), l.split(/\s+/)[0]] as [string, string]));
      for (const f of card.files) {
        const want = new Bun.CryptoHasher("sha256").update(await Bun.file(`${card.dir}/${f}`).arrayBuffer()).digest("hex");
        if (onCard.get(f) !== want) throw new Error(`Verify failed: ${f} on the card doesn't match what was written`);
      }
    } finally {
      await run(["sudo", "umount", CARD_MOUNT]);
    }

    await run(["sudo", "partprobe", dev]); // lets udev mount it, so the files show up here
    op.progress = 100;
    op.status = "done";
    op.message = `Bootloader card ready and verified: ${image.label} for the ${rpi.CARD_BOARDS[chip]}. Insert it, power on, and wait for the green LED to blink rapidly (or a green HDMI screen).`;
  } finally {
    await rm(card.dir, { recursive: true, force: true });
  }
}

// ==================== RPi Image Catalog ====================

let imageCache: any = null;
let deviceCache: any[] = [];
let imageCacheTime = 0;

async function getRpiImages() {
  if (imageCache && Date.now() - imageCacheTime < 3600000) return imageCache;
  try {
    const resp = await fetch("https://downloads.raspberrypi.com/os_list_imagingutility_v4.json");
    const data: any = await resp.json();
    imageCache = flattenImages(data.os_list || []);
    // "exclusive" boards (Pi 5) only match images that list them; others also take
    // images with no board list, as Imager does.
    deviceCache = (data.imager?.devices || []).map((d: any) => ({ name: d.name, tags: d.tags || [], description: d.description || "", exclusive: d.matching_type === "exclusive" }));
    imageCacheTime = Date.now();
    return imageCache;
  } catch {
    return imageCache || [];
  }
}

// Images plus the board list, for the flash picker.
async function getCatalog() {
  return { images: await getRpiImages(), devices: deviceCache };
}

function flattenImages(items: any[], path: string[] = []): any[] {
  const r: any[] = [];
  for (const i of items) {
    if (i.subitems) r.push(...flattenImages(i.subitems, [...path, i.name]));
    else if (i.url) {
      const rpios = parseRpiOs(i.url);
      r.push({
        name: i.name, description: i.description || "", category: path[path.length - 1] || "", path, url: i.url,
        extractSize: i.extract_size || 0, downloadSize: i.image_download_size || 0, releaseDate: i.release_date || "",
        icon: i.icon || "", initFormat: i.init_format || "none", devices: [...new Set<string>(i.devices || [])],
        key: rpios ? `rpios/${rpios.legacy ? "legacy" : "current"}/${rpios.arch}/${rpios.edition}` : [...path, i.name].join("/"),
        ...(rpios && { rpios }),
      });
    }
  }
  if (!path.length) {
    // A few names repeat with different boards (DietPi, Alpine); tell them apart so a
    // favorite points at one image. Some even share the boards (PINN, FullPageOS
    // nightlies), so those fall back to catalog order.
    const count = (key: (im: any) => string) => {
      const n = new Map<string, number>();
      for (const im of r) n.set(key(im), (n.get(key(im)) || 0) + 1);
      return n;
    };
    const byName = count((im) => im.key);
    for (const im of r) if (byName.get(im.key)! > 1) im.key += `@${im.devices.join(",")}`;
    const byBoards = count((im) => im.key), nth = new Map<string, number>();
    for (const im of r) if (byBoards.get(im.key)! > 1) { const i = (nth.get(im.key) || 0) + 1; nth.set(im.key, i); im.key += `#${i}`; }
  }
  return r;
}

// Raspberry Pi OS images are named <date>-raspios-<release>-<arm64|armhf>[-lite|-full].img.xz,
// under a raspios[_oldstable][_lite|_full]_<arch>/ directory. "Legacy" comes from that
// directory rather than the codename, so this keeps working when the next Debian release
// takes over. Third-party images built on Pi OS (SatNOGS, nymea) live on other hosts.
function parseRpiOs(url: string) {
  if (!url.startsWith("https://downloads.raspberrypi.com/")) return null;
  const m = url.match(/\/[^/]*-raspios-([a-z]+)-(arm64|armhf)(?:-(lite|full))?\.img\.xz$/);
  if (!m || !/\/raspios(_oldstable)?(_lite|_full)?_(arm64|armhf)\//.test(url)) return null;
  return { release: m[1], arch: m[2] === "arm64" ? "64" : "32", edition: m[3] || "desktop", legacy: /_oldstable_/.test(url) };
}

// ==================== Image Lists ====================
//
// Lists of catalog keys the flash picker keeps: starred images, and the images Quick Pi
// offers. Until the Quick Pi list is first edited it holds just the current Pi OS Lite.

type ImageList = { key: string; label: string }[];
const IMAGE_LISTS: Record<string, { path: string; initial: ImageList }> = {
  "/api/favorites": { path: statePath("favorites.json"), initial: [] },
  "/api/quicklist": { path: statePath("quicklist.json"), initial: [{ key: "rpios/current/64/lite", label: "Pi OS Lite 64-bit" }] },
};

async function imageListRoute(list: { path: string; initial: ImageList }, req: Request): Promise<Response> {
  if (req.method === "GET") { try { return Response.json(await Bun.file(list.path).json()); } catch { return Response.json(list.initial); } }
  if (req.method !== "POST") return new Response("Method Not Allowed", { status: 405 });
  const body = await req.json();
  if (!Array.isArray(body) || !body.every((f) => typeof f?.key === "string")) return Response.json({ ok: false, message: "Expected [{ key, label }]" }, { status: 400 });
  await Bun.write(list.path, JSON.stringify(body.map((f) => ({ key: f.key, label: String(f.label ?? f.key) })), null, 2));
  return Response.json({ ok: true });
}

// ==================== Default Settings ====================

async function getDefaultSettings() {
  const tz = (await readSysFile("/etc/timezone")) || "UTC";
  let wifiSsid = "";
  try {
    const { out } = await run(["nmcli", "-t", "-f", "active,ssid", "dev", "wifi"]);
    const line = out.split("\n").find((l) => l.startsWith("yes:"));
    if (line) wifiSsid = line.split(":").slice(1).join(":");
  } catch {}
  return {
    hostname: "raspberrypi", username: "pi", password: "", enableSsh: true,
    sshKeys: [] as string[],
    wifiNetworks: wifiSsid ? [{ ssid: wifiSsid, password: "", country: "US", priority: 0 }] : [],
    timezone: tz, keyboardLayout: "us",
  };
}

// ==================== SSE ====================

// Gzipped where the browser accepts it: one gzip stream per client, flushed after every
// message, so each arrives at once and repeats of a nearly identical ~20 KB status cost a
// few hundred bytes on the wire instead of the whole thing every 2 s.
interface SseClient { send(b: Uint8Array): void; close(): void }
const sseClients = new Set<SseClient>();

function sseClient(ctrl: ReadableStreamDefaultController, gzip: boolean): SseClient {
  if (!gzip) return { send: (b) => ctrl.enqueue(b), close() {} };
  const gz = createGzip();
  const c: SseClient = {
    send(b) { gz.write(b); gz.flush(zlibConstants.Z_SYNC_FLUSH); },
    close() { sseClients.delete(c); gz.destroy(); },
  };
  gz.on("data", (chunk: Buffer) => { try { ctrl.enqueue(new Uint8Array(chunk)); } catch { c.close(); } });
  gz.on("error", () => c.close());
  return c;
}

// `event` names a non-default SSE event; without one it's the full status message.
function broadcast(status: any, event?: string) {
  const enc = new TextEncoder().encode(`${event ? `event: ${event}\n` : ""}data: ${JSON.stringify(status)}\n\n`);
  for (const c of sseClients) {
    try { c.send(enc); } catch { c.close(); }
  }
}

// ==================== Compression ====================
// JSON answers over 1 KB and the page itself, when the client takes gzip. Streams are left
// alone: SSE has its own above, and a JSON body that streams on purpose (slowJson's
// keep-alive spaces) says so with X-Accel-Buffering: no. Downloads are octet-stream.

const acceptsGzip = (req: Request) => /\bgzip\b/.test(req.headers.get("accept-encoding") || "");

async function compress(req: Request, res: Response): Promise<Response> {
  const h = res.headers;
  if (res.status !== 200 || !res.body || h.has("content-encoding") || h.get("x-accel-buffering") === "no" || !/^application\/json/.test(h.get("content-type") || "") || !acceptsGzip(req)) return res;
  const raw = new Uint8Array(await res.arrayBuffer());
  const headers = new Headers(h);
  headers.append("Vary", "Accept-Encoding");
  if (raw.length < 1024) return new Response(raw, { status: res.status, headers });
  headers.set("Content-Encoding", "gzip");
  headers.delete("Content-Length");
  return new Response(Bun.gzipSync(raw), { status: res.status, headers });
}

let gzPage: { version: string; body: Uint8Array } | null = null;

setInterval(async () => {
  if (sseClients.size === 0) return;
  try { broadcast(await getStatus()); } catch {}
}, 2000);

// ==================== File Browser ====================
//
// Read-only browsing of the auto-mounted partitions under FILES_ROOT. Everything goes
// through sudo: ext4 drives keep their own ownership, so `pi` alone can't read them all.
// That grants nothing new — Samba already shares this tree to guests as root — but it
// does mean a path must never escape the root. Every request is realpath-resolved and
// checked, which also turns away absolute symlinks on a Linux rootfs drive: they resolve
// against the host, not the drive.

const FILES_ROOT = (process.env.FILES_ROOT || "/srv/nfs/drives").replace(/\/+$/, "");
const MAX_LISTING = 5000;

class FileError extends Error {
  constructor(message: string, public status: number) {
    super(message);
  }
}

// Resolves a root-relative path to a real absolute path inside FILES_ROOT, or throws.
async function resolveInRoot(rel: string): Promise<string> {
  if (rel.includes("\0")) throw new FileError("Invalid path", 400);
  const parts = rel.split("/").filter((s) => s && s !== ".");
  if (parts.includes("..")) throw new FileError("Invalid path", 400);
  const abs = [FILES_ROOT, ...parts].join("/");
  // -z: NUL-terminated, so run()'s trim() can't eat trailing spaces in a directory name.
  const { out: raw, code } = await run(["sudo", "realpath", "-ez", "--", abs]);
  const out = raw.replace(/\0$/, "");
  if (code !== 0 || !out) throw new FileError("Not found", 404);
  if (out !== FILES_ROOT && !out.startsWith(FILES_ROOT + "/")) throw new FileError("Path leads outside the drives", 403);
  return out;
}

function relOf(real: string): string {
  return real === FILES_ROOT ? "" : real.slice(FILES_ROOT.length + 1);
}

// Single `find` call: type, target type, size, mtime, link target, name — NUL-separated,
// since NUL is the one byte a filename can't contain.
const FIND_FMT = "%y\\0%Y\\0%s\\0%T@\\0%l\\0%f\\0";
const FIND_FIELDS = 6;

interface StatEntry {
  type: string; // find %y: f d l b c p s
  targetType: string; // find %Y: type after following symlinks; N = dangling, L = loop
  size: number;
  mtime: number; // ms since epoch
  link: string;
  name: string;
}

function parseFind(raw: string): StatEntry[] {
  const f = raw.split("\0");
  const out: StatEntry[] = [];
  for (let i = 0; i + FIND_FIELDS <= f.length; i += FIND_FIELDS) {
    out.push({
      type: f[i],
      targetType: f[i + 1],
      size: parseInt(f[i + 2]) || 0,
      mtime: Math.round(parseFloat(f[i + 3]) * 1000) || 0,
      link: f[i + 4],
      name: f[i + 5],
    });
  }
  return out;
}

async function statReal(real: string): Promise<StatEntry> {
  const { out, code } = await run(["sudo", "find", real, "-maxdepth", "0", "-printf", FIND_FMT]);
  const e = parseFind(out)[0];
  if (code !== 0 || !e) throw new FileError("Not found", 404);
  return e;
}

// Two batched calls however many links a directory holds: realpath them all, then stat
// only the targets that stay inside the root, so a link to a host file never reveals so
// much as that file's size.
async function resolveLinks(dir: string, names: string[]): Promise<Map<string, { outside: boolean; stat?: StatEntry }>> {
  const result = new Map<string, { outside: boolean; stat?: StatEntry }>();
  if (!names.length) return result;
  // Output has to line up with input. -m prints dangling links too, so one call normally
  // suffices; anything it can't resolve makes it skip a record and exit non-zero, and then
  // a per-link loop (one process each — seconds for a big directory) pads the gaps.
  const paths = names.map((n) => `${dir}/${n}`);
  let { out: realsRaw, code } = await run(["sudo", "realpath", "-mz", "--", ...paths]);
  if (code !== 0) {
    ({ out: realsRaw } = await run([
      "sudo", "sh", "-c", 'for p; do realpath -mz -- "$p" 2>/dev/null || printf "\\0"; done', "sh", ...paths,
    ]));
  }
  const reals = realsRaw.split("\0");
  const inside: string[] = [];
  names.forEach((name, i) => {
    const r = reals[i] || "";
    const outside = !!r && r !== FILES_ROOT && !r.startsWith(FILES_ROOT + "/");
    result.set(name, { outside });
    if (r && !outside) inside.push(r);
  });
  if (inside.length) {
    // Keyed by %p because find skips (rather than reports) targets that don't exist.
    const { out } = await run(["sudo", "find", ...inside, "-maxdepth", "0", "-printf", "%p\\0" + FIND_FMT]);
    const f = out.split("\0");
    const byPath = new Map<string, StatEntry>();
    for (let i = 0; i + FIND_FIELDS + 1 <= f.length; i += FIND_FIELDS + 1) {
      byPath.set(f[i], parseFind(f.slice(i + 1, i + 1 + FIND_FIELDS).join("\0") + "\0")[0]);
    }
    names.forEach((name, i) => {
      const s = byPath.get(reals[i]);
      // Still a link after resolving means a loop: as unusable as a dangling one.
      if (s && s.type !== "l") result.get(name)!.stat = s;
    });
  }
  return result;
}

async function listDir(rel: string) {
  const real = await resolveInRoot(rel);
  if ((await statReal(real)).type !== "d") throw new FileError("Not a directory", 400);

  // run() trims output, which is harmless: every record ends in a NUL, not whitespace.
  const { out, code, err } = await run(["sudo", "find", real, "-mindepth", "1", "-maxdepth", "1", "-printf", FIND_FMT]);
  if (code !== 0 && !out) throw new FileError(err || "Could not read directory", 500);

  const raw = parseFind(out);
  const targets = await resolveLinks(real, raw.filter((e) => e.type === "l").map((e) => e.name));

  const entries = raw.map((e) => {
    // A symlink is presented as whatever it points at; `link` keeps it recognisable.
    // Its own %s/%T@ describe the link, not the target, so those come from resolveLinks.
    const t = e.type === "l" ? targets.get(e.name) : undefined;
    const st = t?.stat || e;
    const type = e.type === "l" && !t?.stat ? (t?.outside ? "outside" : "broken") : st.type === "d" ? "dir" : st.type === "f" ? "file" : "other";
    return {
      name: e.name,
      type,
      size: type === "file" || type === "other" ? st.size : null,
      mtime: e.type === "l" && !t?.stat ? null : st.mtime,
      link: e.type === "l" ? e.link : undefined,
    };
  });
  entries.sort((a, b) => (a.type === "dir" ? 0 : 1) - (b.type === "dir" ? 0 : 1) || a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: "base" }));

  return {
    ok: true,
    path: relOf(real),
    // Readable only partially (e.g. permission errors on some entries) still lists what it can.
    warning: code !== 0 ? err.split("\n")[0] : undefined,
    truncated: entries.length > MAX_LISTING,
    total: entries.length,
    entries: entries.slice(0, MAX_LISTING),
  };
}

// Parses a single-range `Range` header. null = serve the whole file; "bad" = 416.
function parseRange(h: string | null, size: number): { start: number; end: number } | null | "bad" {
  if (!h) return null;
  const m = h.match(/^bytes=(\d*)-(\d*)$/);
  if (!m || (!m[1] && !m[2])) return null; // multi-range or malformed: ignore, send it all
  let start: number, end: number;
  if (!m[1]) {
    start = Math.max(0, size - parseInt(m[2]));
    end = size - 1;
  } else {
    start = parseInt(m[1]);
    end = m[2] ? Math.min(parseInt(m[2]), size - 1) : size - 1;
  }
  if (start >= size || start > end) return "bad";
  return { start, end };
}

function contentDisposition(name: string): string {
  const ascii = name.replace(/[^\x20-\x7e]/g, "_").replace(/["\\]/g, "_");
  return `attachment; filename="${ascii}"; filename*=UTF-8''${encodeURIComponent(name)}`;
}

async function downloadFile(rel: string, req: Request): Promise<Response> {
  const real = await resolveInRoot(rel);
  const st = await statReal(real);
  if (st.type !== "f") throw new FileError("Not a regular file", 400);

  const range = parseRange(req.headers.get("range"), st.size);
  if (range === "bad") {
    return new Response(null, { status: 416, headers: { "Content-Range": `bytes */${st.size}` } });
  }
  const start = range ? range.start : 0;
  const length = range ? range.end - range.start + 1 : st.size;
  const headers: Record<string, string> = {
    "Content-Type": "application/octet-stream",
    "Content-Disposition": contentDisposition(st.name),
    "Content-Length": String(length),
    "Accept-Ranges": "bytes",
    "Last-Modified": new Date(st.mtime).toUTCString(),
    "Cache-Control": "no-cache",
  };
  if (range) headers["Content-Range"] = `bytes ${range.start}-${range.end}/${st.size}`;
  const status = range ? 206 : 200;
  if (req.method === "HEAD" || length === 0) return new Response(null, { status, headers });

  // Readable without sudo — every FAT/exFAT/NTFS mount — gets Bun's file path: sendfile,
  // and a real Content-Length (Bun drops it on streamed bodies, which leaves the browser
  // with no size or progress). Only files `pi` can't open take the sudo dd stream.
  if (await access(real, fsConstants.R_OK).then(() => true, () => false)) {
    return new Response(Bun.file(real).slice(start, start + length), { status, headers });
  }

  const proc = Bun.spawn(
    ["sudo", "dd", `if=${real}`, "bs=1M", "iflag=skip_bytes,count_bytes", `skip=${start}`, `count=${length}`, "status=none"],
    { stdout: "pipe", stderr: "ignore" },
  );
  // A cancelled download must not leave dd reading a multi-GB file into a dead pipe.
  req.signal.addEventListener("abort", () => proc.kill());
  return new Response(proc.stdout, { status, headers });
}

// ==================== OS Detection ====================
//
// Which systems are installed on a drive and how they boot — see osinfo.ts. Reading the
// registry hives and hashing loaders takes a moment, so results are cached per disk until
// its set of mounts changes (a remount gets a new mount ID, so it counts as a change).

const osInfoCache = new Map<string, { key: string; result: Promise<DiskInfo> }>();

async function mountIds(): Promise<Map<string, string>> {
  const ids = new Map<string, string>();
  for (const line of (await readSysFile("/proc/self/mountinfo")).split("\n")) {
    const f = line.split(" ");
    // Mount points are octal-escaped (\040 for a space) in mountinfo.
    if (f.length > 4) ids.set(f[4].replace(/\\(\d{3})/g, (_, o) => String.fromCharCode(parseInt(o, 8))), f[0]);
  }
  return ids;
}

async function getOsInfo(device: string): Promise<DiskInfo> {
  const { out } = await run([
    "lsblk", "-J", "-o", "NAME,TYPE,TRAN,FSTYPE,LABEL,UUID,PARTUUID,PARTTYPE,PARTLABEL,MOUNTPOINT,PTTYPE,PTUUID", `/dev/${device}`,
  ]);
  let disk: any;
  try {
    disk = JSON.parse(out).blockdevices?.[0];
  } catch {}
  if (!disk || disk.type !== "disk" || disk.tran !== "usb") throw new FileError("Not a USB disk", 404);

  const ids = await mountIds();
  const key = JSON.stringify((disk.children || []).map((p: any) => [p.name, p.fstype, p.mountpoint, ids.get(p.mountpoint)]));
  const hit = osInfoCache.get(device);
  if (hit && hit.key === key) return hit.result;
  const result = analyseDisk(disk, FILES_ROOT);
  osInfoCache.set(device, { key, result });
  // A failure shouldn't stick: drop it so the next request tries again.
  result.catch(() => osInfoCache.get(device)?.result === result && osInfoCache.delete(device));
  return result;
}

// ==================== API Docs ====================

const API_DOCS = {
  name: "BenchBob API", version: "1.0", appVersion: VERSION, base: "http://benchbob.local", // rewritten to the host asked, per request
  endpoints: {
    "GET /api": "This documentation",
    "GET /api/status": "System status snapshot (same data as SSE). board: { id, name, model, tested, capabilities } says which board file BenchBob picked and what it supports; sensors: thermal/hwmon/power_supply readings",
    "GET /api/events": "SSE stream, sends status every ~2s",
    "POST /api/eject": { body: { device: "sdb" }, description: "Safely eject USB device (unmount, remove NFS/NBD, power off)" },
    "POST /api/erase": { body: { device: "sdb", mode: "quick|full|discard" }, description: "Erase device. Returns immediately; track via SSE. Queueable before flash." },
    "POST /api/flash": { body: { imageUrl: "https://...", extractSize: 0, device: "sdb", settings: {} }, description: "Flash RPi image. Can queue after erase." },
    "POST /api/scan": { body: { device: "sdb" }, description: "Re-read partition table and identify contents" },
    "POST /api/probe": { body: { device: "sdb" }, description: "Fast sampled raw-sector probe. Reads ~1MB spread across the device (denser in unallocated gaps), classifies each block as zero/ones/text/compressed/structured and reports file signatures. Seconds." },
    "POST /api/deepscan": { body: { device: "sdb" }, description: "Exhaustive raw scan for file signatures across the whole device. Tracked like erase/flash: progress via SSE, cancellable." },
    "GET /api/scanresult?device=sdb": "Findings from the last deep scan of a device",
    "POST /api/cancel": { body: { operationId: "uuid" }, description: "Cancel active operation" },
    "GET /api/files?path=LABEL/dir": "List a directory on a mounted drive (read-only). path is relative to /srv/nfs/drives; empty lists the mounted drives. Symlinks that lead outside the drives are refused.",
    "GET /api/download?path=LABEL/dir/file": "Download one file. Supports Range requests, so interrupted downloads can resume (curl -C -).",
    "GET /api/osinfo?device=sda": "What's installed on a USB disk and how it boots: per-partition role, EFI loaders (with Windows BCD entries, GRUB targets, systemd-boot entries, fallback-loader identity), Windows edition/build/PC name, Linux os-release, Raspberry Pi bootfs, macOS version. Only mounted partitions are inspected.",
    "GET /api/images": "Official RPi OS image catalog (cached 1hr)",
    "GET /api/catalog": "Image catalog with picker metadata (full category path, board tags, stable favorite key, parsed Raspberry Pi OS release/arch/edition) plus the board list",
    "GET /api/favorites": "Starred images: [{ key, label }], keys as in /api/catalog",
    "POST /api/favorites": { body: [{ key: "rpios/current/64/lite", label: "Raspberry Pi OS Lite (64-bit)" }], description: "Replace the starred images" },
    "GET /api/quicklist": "Images Quick Pi offers: [{ key, label }]. Defaults to the current Pi OS Lite 64-bit until first saved.",
    "POST /api/quicklist": { body: [{ key: "rpios/current/64/lite", label: "Pi OS Lite 64-bit" }], description: "Replace the Quick Pi list" },
    "GET /api/defaults": "Saved default flash settings",
    "POST /api/rpiboot/msd": { body: { path: "1-1.3" }, description: "Load the mass-storage gadget onto a device in USB boot mode (path from status.bootDevices). Its eMMC/NVMe then appears as a normal drive." },
    "POST /api/rpiboot/eeprom/check": { body: { path: "1-1.3" }, description: "Read OTP metadata (board, MAC, secure-boot state, EEPROM hash) through recovery.bin without writing anything. Result in status.rpibootResults." },
    "GET /api/rpiboot/eeprom/options?path=1-1.3": "Bootloader images available for that device's SoC, plus rpiboot's default boot.conf",
    "POST /api/rpiboot/eeprom/write": { body: { path: "1-1.3", imageId: "default|latest|rpiboot", bootConf: "[all]\nBOOT_ORDER=0xf25641" }, description: "Rewrite the SPI bootloader EEPROM with the chosen image and config." },
    "POST /api/rpiboot/results/clear": { body: { path: "1-1.3" }, description: "Dismiss a stored bootloader check/write result" },
    "GET /api/blcard/options?chip=2711|2712": "Bootloader images for a Pi 4 (2711) or Pi 5 (2712) bootloader update card, each with its built-in boot.conf",
    "POST /api/blcard": { body: { device: "sdb", chip: "2711|2712", imageId: "default|latest|rpiboot", bootConf: "[all]\nBOOT_ORDER=0xf41" }, description: "Erase a drive and write a bootloader update SD card (recovery.bin + pieeprom.bin with that config), sized to fit any card. Tracked like erase/flash." },
    "POST /api/defaults": "Save default flash settings",
  },
  examples: {
    eject: 'curl -X POST http://benchbob.local/api/eject -H "Content-Type: application/json" -d \'{"device":"sdb"}\'',
    status: "curl -s http://benchbob.local/api/status | jq '.drives'",
    list_exports: "curl -s http://benchbob.local/api/status | jq '.nbdExports'",
    list_files: "curl -s 'http://benchbob.local/api/files?path=bootfs' | jq '.entries[].name'",
    download: "curl -OJ 'http://benchbob.local/api/download?path=bootfs/config.txt'",
  },
};

// ==================== HTTP Server ====================

// ==================== Build Versioning ====================
//
// The dashboard is a long-lived page held open for hours, so a deploy would otherwise
// leave a stale UI talking to a newer backend. The version is substituted into the page
// at serve time, so a client always knows exactly which build it is running, rather than
// inferring it from the first status message it happens to see (which would be the *new*
// version for anyone connecting mid-deploy).
//
// It hashes every file whose content defines the running app, not just index.html: a
// server.ts-only deploy leaves index.html byte-identical, so hashing the page alone would
// let an open dashboard keep talking to a changed API forever.

const UI_PATH = import.meta.dir + "/index.html";

// UI_PATH must stay first — getBuild() serves texts[0] as the page.
// defaults.json, favorites.json and quicklist.json are deliberately excluded: they are user data, rewritten
// whenever flash settings or stars are saved, and would cause a spurious reload each time.
// Peripheral drivers and their dashboard scripts are globbed, so a new driver joins the hash
// (and its ui/*.js is inlined into the page) without editing this list.
const CORE_FILES = [UI_PATH, ...["server.ts", "osinfo.ts", "paths.ts", "regf.ts", "rpiboot.ts", "update.ts"].map((f) => `${import.meta.dir}/${f}`)];

let buildCache: { stamp: string; version: string; html: string } | null = null;

async function getBuild(): Promise<{ version: string; html: string }> {
  const uiFiles = periph.uiFiles();
  const files = [...CORE_FILES, ...periph.buildFiles()].map((p) => Bun.file(p));
  // Cheap staleness check so a git pull is picked up without a restart.
  const stamp = files.map((f) => `${f.lastModified}:${f.size}`).join("|");
  if (!buildCache || buildCache.stamp !== stamp) {
    const texts = await Promise.all(files.map((f) => f.text().catch(() => "")));
    // Hash the sources as stored, before substitution, so the value is stable rather
    // than self-referential.
    const version = Bun.hash(texts.join(" ")).toString(16);
    // One <script> per driver UI, so a mistake in one can't take down the rest of the page.
    const uiTexts = texts.slice(files.length - uiFiles.length);
    const uiScripts = uiTexts.map((t, i) => `<script>/* ${uiFiles[i].split("/").pop()} */\n${t}\n</script>`).join("\n");
    buildCache = { stamp, version, html: texts[0].replace("<!--PERIPH_UI-->", () => uiScripts).replaceAll("__BUILD_VERSION__", version) };
  }
  return buildCache;
}

// A peripheral change only pushes the peripherals part, as its own event. A full getStatus()
// spawns a dozen processes and re-samples CPU/IO rates over a sub-second window, and every
// full message adds a point to the dashboard's sparklines, so drivers that change often
// would both load the Pi and distort those graphs. The 2 s tick still carries everything.
// A bug in one panel (a promise nobody awaited) must not take the whole server down.
process.on("unhandledRejection", (e) => console.error("Unhandled rejection:", e));

await update.start();
await periph.start(() => {
  if (sseClients.size === 0) return;
  try { broadcast(periph.status(), "periph"); } catch {}
});

Bun.serve({
  port: PORT, hostname: "::",
  // Bun drops a request that is quiet for 10 s by default. Peripheral calls legitimately
  // wait longer (an agent waiting for a serial prompt, a CAN capture, a Wi-Fi client
  // test), so allow up to 2 minutes. SSE and WebSockets are unaffected: they send often.
  idleTimeout: 120,
  websocket: periph.websocket,
  async fetch(req, server) {
    const res = await handle(req, server);
    return res ? compress(req, res) : res;
  },
});

async function handle(req: Request, server: Parameters<typeof periph.route>[2]): Promise<Response | undefined> {
  {
    const url = new URL(req.url);
    const p = url.pathname;

    const pr = await periph.route(req, url, server);
    if (pr === "upgraded") return undefined;
    if (pr) return pr;

    // no-cache (revalidate every load), not no-store — otherwise a deploy is invisible
    // until the user knows to hard-refresh. The ETag keeps revalidation cheap.
    if (p === "/" || p === "/index.html") {
      const ui = await getBuild();
      const etag = `"${ui.version}"`;
      const headers: Record<string, string> = { "Content-Type": "text/html; charset=utf-8", "Cache-Control": "no-cache", ETag: etag, Vary: "Accept-Encoding" };
      if (req.headers.get("if-none-match") === etag) return new Response(null, { status: 304, headers });
      if (!acceptsGzip(req)) return new Response(ui.html, { headers });
      // ~430 KB of HTML with every driver UI inlined; gzip it once per build.
      if (gzPage?.version !== ui.version) gzPage = { version: ui.version, body: Bun.gzipSync(new TextEncoder().encode(ui.html)) };
      return new Response(gzPage.body, { headers: { ...headers, "Content-Encoding": "gzip" } });
    }
    if (p === "/api" || p === "/api/") {
      // Examples name the host the caller reached us by, so they paste and run as-is.
      // The board says what this machine can do (header, boot config), so agents reading
      // the docs don't assume a Pi.
      const docs = JSON.stringify({ ...API_DOCS, board: boardInfo(), endpoints: { ...API_DOCS.endpoints, ...update.DOCS, ...periph.docs() } });
      return new Response(docs.replaceAll("http://benchbob.local", url.origin), { headers: { "Content-Type": "application/json" } });
    }
    if (p.startsWith("/api/update")) {
      const r = await update.route(req, p);
      if (r) return r;
    }
    if (p === "/api/status") return Response.json(await getStatus());

    if (p === "/api/events") {
      let client: SseClient;
      const gzip = acceptsGzip(req);
      const stream = new ReadableStream({
        start(c) { client = sseClient(c, gzip); sseClients.add(client); getStatus().then((s) => { try { client.send(new TextEncoder().encode(`data: ${JSON.stringify(s)}\n\n`)); } catch {} }); },
        cancel() { client.close(); sseClients.delete(client); },
      });
      return new Response(stream, { headers: { "Content-Type": "text/event-stream", "Cache-Control": "no-cache", Connection: "keep-alive", Vary: "Accept-Encoding", ...(gzip ? { "Content-Encoding": "gzip" } : {}) } });
    }

    if (p === "/api/eject" && req.method === "POST") {
      const { device } = await req.json();
      if (!device || isSysDev(device)) return Response.json({ ok: false, message: "Invalid device" }, { status: 400 });
      if (isDeviceBusy(device)) return Response.json({ ok: false, message: "Device busy" }, { status: 409 });
      return Response.json(await ejectDevice(device));
    }

    if (p === "/api/erase" && req.method === "POST") {
      const { device, mode } = await req.json();
      if (!device || isSysDev(device)) return Response.json({ ok: false, message: "Invalid device" }, { status: 400 });
      if (isDeviceBusy(device)) return Response.json({ ok: false, message: "Device busy" }, { status: 409 });
      const op: Operation = { id: crypto.randomUUID(), type: "erase", device, status: "starting", progress: 0, message: "Starting...", mode: mode || "quick" };
      enqueue(op);
      return Response.json({ ok: true, message: "Erase started" });
    }

    if (p === "/api/flash" && req.method === "POST") {
      const { imageUrl, imageName, extractSize, device, settings } = await req.json();
      if (!device || isSysDev(device)) return Response.json({ ok: false, message: "Invalid device" }, { status: 400 });
      if (!imageUrl) return Response.json({ ok: false, message: "No image URL" }, { status: 400 });
      // Allow queueing after erase, but not if already queued or non-erase is running
      const ops = getOpsForDevice(device);
      const nonEraseRunning = ops.some((o) => !["done", "error", "queued"].includes(o.status) && o.type !== "erase");
      const alreadyQueued = ops.some((o) => o.status === "queued");
      if (nonEraseRunning || alreadyQueued) return Response.json({ ok: false, message: "Device busy" }, { status: 409 });
      // init_format comes from our own catalog, not the client: an old dashboard tab
      // doesn't send it, and guessing wrong silently drops every setting.
      const initFormat: InitFormat | undefined = (await getRpiImages()).find((i: any) => i.url === imageUrl)?.initFormat;
      const op: Operation = { id: crypto.randomUUID(), type: "flash", device, status: "starting", progress: 0, message: "Starting...", imageUrl, imageName, extractSize, initFormat, settings };
      enqueue(op);
      return Response.json({ ok: true, message: hasQueuedFlash(device) ? "Flash queued after erase" : "Flash started" });
    }

    if (p === "/api/scan" && req.method === "POST") {
      const { device } = await req.json();
      if (!device || isSysDev(device)) return Response.json({ ok: false, message: "Invalid device" }, { status: 400 });
      return Response.json(await scanDevice(device));
    }

    if (p === "/api/probe" && req.method === "POST") {
      const { device } = await req.json();
      if (!device || isSysDev(device)) return Response.json({ ok: false, message: "Invalid device" }, { status: 400 });
      return Response.json(await probeDevice(device));
    }

    if (p === "/api/deepscan" && req.method === "POST") {
      const { device } = await req.json();
      if (!device || isSysDev(device)) return Response.json({ ok: false, message: "Invalid device" }, { status: 400 });
      if (isDeviceBusy(device)) return Response.json({ ok: false, message: "Device busy" }, { status: 409 });
      const op: Operation = { id: crypto.randomUUID(), type: "deepscan", device, status: "starting", progress: 0, message: "Starting..." };
      enqueue(op);
      return Response.json({ ok: true, message: "Deep scan started", operationId: op.id });
    }

    if (p === "/api/blcard/options") {
      const chip = url.searchParams.get("chip");
      if (chip !== "2711" && chip !== "2712") return Response.json({ ok: false, message: "chip must be 2711 or 2712" }, { status: 400 });
      return Response.json({ ok: true, chip, board: rpi.CARD_BOARDS[chip], images: await rpi.eepromImages(chip) });
    }

    if (p === "/api/blcard" && req.method === "POST") {
      const { device, chip, imageId, bootConf } = await req.json();
      // Goes into sfdisk/mkfs arguments: a plain kernel disk name that exists, nothing else.
      if (typeof device !== "string" || !/^[a-z][a-z0-9]*$/.test(device) || isSysDev(device) || !(await getDeviceSizeBytes(device))) {
        return Response.json({ ok: false, message: "Invalid device" }, { status: 400 });
      }
      if (chip !== "2711" && chip !== "2712") return Response.json({ ok: false, message: "chip must be 2711 or 2712" }, { status: 400 });
      if (!(await rpi.eepromImages(chip)).some((i) => i.id === imageId)) return Response.json({ ok: false, message: "Unknown bootloader image" }, { status: 400 });
      if (typeof bootConf !== "string" || !bootConf.trim() || bootConf.length > 4000) return Response.json({ ok: false, message: "boot.conf is missing or too long" }, { status: 400 });
      if ((await getDeviceSizeBytes(device)) < CARD_PART_MIN + MiB) return Response.json({ ok: false, message: "Card too small" }, { status: 400 });
      if (isDeviceBusy(device) || getOpsForDevice(device).some((o) => o.status === "queued")) return Response.json({ ok: false, message: "Device busy" }, { status: 409 });
      const image = (await rpi.eepromImages(chip)).find((i) => i.id === imageId)!;
      const op: Operation = {
        id: crypto.randomUUID(), type: "blcard", device, status: "starting", progress: 0, message: "Starting...",
        chip, eepromImageId: imageId, bootConf, imageName: `${rpi.CARD_BOARDS[chip]} · ${image.buildDate}`,
      };
      enqueue(op);
      return Response.json({ ok: true, message: "Writing bootloader card", operationId: op.id });
    }

    if (p === "/api/scanresult") {
      const device = url.searchParams.get("device") || "";
      const r = lastScanResults.get(device);
      return Response.json(r ? { ok: true, ...r } : { ok: false, message: "No deep scan result for this device" });
    }

    if (p === "/api/cancel" && req.method === "POST") {
      const { operationId } = await req.json();
      const all = getAllOps();
      const op = all.find((o) => o.id === operationId);
      if (op) { op.aborted = true; op.process?.kill(); op.status = "error"; op.message = "Cancelled"; }
      return Response.json({ ok: true });
    }

    if (p === "/api/osinfo") {
      const device = url.searchParams.get("device") || "";
      if (!/^[a-z][a-z0-9]*$/.test(device) || isSysDev(device)) return Response.json({ ok: false, message: "Invalid device" }, { status: 400 });
      try {
        return Response.json({ ok: true, ...(await getOsInfo(device)) });
      } catch (e: any) {
        return Response.json({ ok: false, message: e.message }, { status: e instanceof FileError ? e.status : 500 });
      }
    }

    if (p === "/api/files" || p === "/api/download") {
      try {
        const rel = url.searchParams.get("path") || "";
        if (p === "/api/files") return Response.json(await listDir(rel));
        if (req.method === "GET" || req.method === "HEAD") return await downloadFile(rel, req);
        return new Response("Method Not Allowed", { status: 405 });
      } catch (e: any) {
        return Response.json({ ok: false, message: e.message }, { status: e instanceof FileError ? e.status : 500 });
      }
    }

    if (p.startsWith("/api/rpiboot/")) return await rpibootRoute(p, req, url);

    if (p === "/api/images") return Response.json(await getRpiImages());
    if (p === "/api/catalog") return Response.json(await getCatalog());
    if (IMAGE_LISTS[p]) return await imageListRoute(IMAGE_LISTS[p], req);
    if (p === "/api/defaults") {
      if (req.method === "GET") { try { return Response.json(await Bun.file(DEFAULTS_PATH).json()); } catch { return Response.json(await getDefaultSettings()); } }
      if (req.method === "POST") { await Bun.write(DEFAULTS_PATH, JSON.stringify(await req.json(), null, 2)); return Response.json({ ok: true }); }
    }

    return new Response("Not Found", { status: 404 });
  }
}

console.log(`BenchBob running on http://[::]:${PORT} (dual-stack: IPv6 + IPv4)`);
