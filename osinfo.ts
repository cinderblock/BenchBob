// What is installed on a drive, and how it boots.
//
// Works only from what is mounted (read-only) under the files root, and reads as the
// service user with no sudo. Paths inside a drive are resolved as if the drive were `/`:
// a Linux rootfs is full of absolute symlinks (etc/os-release -> /usr/lib/os-release),
// and following them literally would read the *host's* files instead of the drive's.
//
// Cross-references are the useful part, so a whole disk is analysed at once:
//   - a Windows BCD entry stores the partition GUID it boots       -> which partition
//   - a GRUB stub on the ESP does `search.fs_uuid <uuid>`         -> which partition
//   - a Raspberry Pi bootfs has `root=PARTUUID=...` in cmdline.txt -> which partition

import { lstat, readdir, readlink, open, stat } from "node:fs/promises";
import { withHive, type Hive, type RegKey } from "./regf.ts";

// ==================== Types ====================

export interface LsblkPart {
  name: string;
  fstype?: string | null;
  label?: string | null;
  uuid?: string | null;
  partuuid?: string | null;
  parttype?: string | null;
  partlabel?: string | null;
  mountpoint?: string | null;
}

export interface LsblkDisk extends LsblkPart {
  pttype?: string | null;
  ptuuid?: string | null;
  children?: LsblkPart[];
}

interface Loader {
  dir: string; // folder under EFI/, as on disk
  name: string; // friendly name
  kind: "os" | "bootmanager" | "firmware" | "fallback" | "unknown";
  files: string[]; // .efi files in the folder
  arch: string | null;
  shim: boolean; // Secure Boot shim present
  // For a GRUB stub: the filesystem it hands off to.
  target?: { uuid: string; partition: string | null; system: string | null };
}

interface BcdEntry {
  description: string;
  path: string | null;
  partition: string | null;
  kind: "os" | "recovery" | "tool";
  isDefault: boolean;
  inMenu: boolean;
}

interface EspInfo {
  loaders: Loader[];
  fallback: { file: string; arch: string | null; sameAs: string | null } | null;
  windowsBoot: { timeout: number | null; entries: BcdEntry[] } | null;
  systemdBoot: { default: string | null; entries: { title: string; version: string | null; file: string }[] } | null;
}

interface WindowsInfo {
  product: string;
  edition: string | null;
  version: string | null;
  build: string | null;
  installed: number | null; // ms since epoch
  computerName: string | null;
}

interface LinuxInfo {
  name: string;
  id: string | null;
  version: string | null;
  hostname: string | null;
}

interface PiBootInfo {
  image: string | null;
  rootPartuuid: string | null;
  rootPartition: string | null;
}

export interface PartInfo {
  name: string;
  role: string | null;
  fstype: string | null;
  label: string | null;
  path: string | null; // relative to the files root, when mounted there
  note: string | null;
  esp?: EspInfo;
  windows?: WindowsInfo;
  linux?: LinuxInfo;
  pi?: PiBootInfo;
  macos?: { name: string; version: string | null };
  error?: string;
}

export interface SystemSummary {
  kind: "windows" | "linux" | "macos" | "pi";
  name: string;
  detail: string | null;
  partition: string | null;
  bootsVia: string[];
}

export interface DiskInfo {
  device: string;
  partitionTable: string | null;
  partitions: PartInfo[];
  systems: SystemSummary[];
  analysedAt: number;
}

// ==================== Reference tables ====================

const ESP_TYPES = new Set(["c12a7328-f81f-11d2-ba4b-00a0c93ec93b", "0xef"]);

const PART_ROLES: Record<string, string> = {
  "c12a7328-f81f-11d2-ba4b-00a0c93ec93b": "EFI System",
  "0xef": "EFI System",
  "e3c9e316-0b5c-4db8-817d-f92df00215ae": "Microsoft reserved",
  "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7": "Basic data",
  "de94bba4-06d1-4d40-a16a-bfd50179d6ac": "Windows recovery",
  "5808c8aa-7e8f-42e0-85d2-e1e90434cfb3": "Windows LDM metadata",
  "af9b60a0-1431-4f62-bc68-3311714a69ad": "Windows LDM data",
  "0fc63daf-8483-4772-8e79-3d69d8477de4": "Linux filesystem",
  "4f68bce3-e8cd-4db1-96e7-fbcaf984b709": "Linux root (x86-64)",
  "b921b045-1df0-41c3-af44-4c6f280d3fae": "Linux root (ARM64)",
  "44479540-f297-41b2-9af7-d131d5f0458a": "Linux root (x86)",
  "933ac7e1-2eb4-4f13-b844-0e14e2aef915": "Linux /home",
  "bc13c2ff-59e6-4262-a352-b275fd6f7172": "Linux extended boot",
  "0657fd6d-a4ab-43c4-84e5-0933c84b4f4f": "Linux swap",
  "e6d6d379-f507-44c2-a23c-238f2a3df928": "Linux LVM",
  "a19d880f-05fc-4d3b-a006-743f0f84911e": "Linux RAID",
  "ca7d7ccb-63ed-4c53-861c-1742536059cc": "LUKS",
  "21686148-6449-6e6f-744e-656564454649": "BIOS boot",
  "7c3457ef-0000-11aa-aa11-00306543ecac": "Apple APFS",
  "48465300-0000-11aa-aa11-00306543ecac": "Apple HFS+",
  "426f6f74-0000-11aa-aa11-00306543ecac": "Apple boot",
  "52637672-7900-11aa-aa11-00306543ecac": "Apple recovery",
  "0x7": "NTFS / exFAT",
  "0xb": "FAT32",
  "0xc": "FAT32 (LBA)",
  "0xe": "FAT16 (LBA)",
  "0x27": "Windows recovery (hidden)",
  "0x82": "Linux swap",
  "0x83": "Linux",
  "0x8e": "Linux LVM",
  "0xfd": "Linux RAID",
};

const FS_NOTES: Record<string, string> = {
  BitLocker: "BitLocker-encrypted — contents can't be read",
  crypto_LUKS: "LUKS-encrypted — contents can't be read",
  LVM2_member: "LVM physical volume",
  linux_raid_member: "Linux RAID member",
  zfs_member: "ZFS pool member",
  swap: "Swap space",
};

// Folder name under EFI/ (lower-cased) -> what put it there.
const EFI_VENDORS: Record<string, { name: string; kind: Loader["kind"] }> = {
  microsoft: { name: "Windows Boot Manager", kind: "os" },
  ubuntu: { name: "Ubuntu (GRUB)", kind: "os" },
  debian: { name: "Debian (GRUB)", kind: "os" },
  fedora: { name: "Fedora (GRUB)", kind: "os" },
  centos: { name: "CentOS (GRUB)", kind: "os" },
  redhat: { name: "Red Hat Enterprise Linux (GRUB)", kind: "os" },
  rocky: { name: "Rocky Linux (GRUB)", kind: "os" },
  almalinux: { name: "AlmaLinux (GRUB)", kind: "os" },
  opensuse: { name: "openSUSE (GRUB)", kind: "os" },
  sles: { name: "SUSE Linux Enterprise (GRUB)", kind: "os" },
  arch: { name: "Arch Linux (GRUB)", kind: "os" },
  manjaro: { name: "Manjaro (GRUB)", kind: "os" },
  endeavouros: { name: "EndeavourOS", kind: "os" },
  pop: { name: "Pop!_OS", kind: "os" },
  neon: { name: "KDE neon (GRUB)", kind: "os" },
  kali: { name: "Kali Linux (GRUB)", kind: "os" },
  gentoo: { name: "Gentoo (GRUB)", kind: "os" },
  proxmox: { name: "Proxmox VE (GRUB)", kind: "os" },
  nixos: { name: "NixOS", kind: "os" },
  void: { name: "Void Linux (GRUB)", kind: "os" },
  linux: { name: "Unified kernel images", kind: "os" },
  apple: { name: "Apple", kind: "os" },
  systemd: { name: "systemd-boot", kind: "bootmanager" },
  refind: { name: "rEFInd", kind: "bootmanager" },
  grub: { name: "GRUB", kind: "bootmanager" },
  clover: { name: "Clover", kind: "bootmanager" },
  oc: { name: "OpenCore", kind: "bootmanager" },
  boot: { name: "Fallback loader", kind: "fallback" },
  dell: { name: "Dell firmware tools", kind: "firmware" },
  hp: { name: "HP firmware tools", kind: "firmware" },
  lenovo: { name: "Lenovo firmware tools", kind: "firmware" },
  asus: { name: "ASUS firmware tools", kind: "firmware" },
  msi: { name: "MSI firmware tools", kind: "firmware" },
  insydeh2o: { name: "Insyde firmware tools", kind: "firmware" },
  tools: { name: "EFI tools", kind: "firmware" },
};

const PE_MACHINES: Record<number, string> = {
  0x8664: "x64",
  0xaa64: "ARM64",
  0x14c: "x86",
  0x1c2: "ARM32",
  0x1c4: "ARM32",
  0x5064: "RISC-V 64",
  0xebc: "EFI byte code",
};

// BCD object types (Description\Type).
const BCD_BOOTMGR = 0x10100002;
const BCD_OSLOADER = 0x10200003;
const BCD_TOOL = 0x10200005;

// ==================== Reading files inside a drive ====================

// Resolves `rel` inside `root` as though `root` were `/`: absolute symlinks restart at the
// drive root and `..` stops there, so nothing outside the drive is reachable. Components
// are matched case-insensitively when there is no exact match — Windows and EFI paths
// differ in case between installs, and ntfs-3g / ext4 compare case-sensitively.
async function resolveUnder(root: string, rel: string): Promise<string | null> {
  let parts = rel.split("/").filter(Boolean);
  let cur: string[] = [];
  let hops = 0;
  while (parts.length) {
    const p = parts.shift()!;
    if (p === ".") continue;
    if (p === "..") {
      cur.pop();
      continue;
    }
    const dir = [root, ...cur].join("/");
    let name = p;
    let st = await lstat(`${dir}/${name}`).catch(() => null);
    if (!st) {
      const lower = p.toLowerCase();
      const match = (await readdir(dir).catch(() => [] as string[])).find((e) => e.toLowerCase() === lower);
      if (!match) return null;
      name = match;
      st = await lstat(`${dir}/${name}`).catch(() => null);
      if (!st) return null;
    }
    if (st.isSymbolicLink()) {
      if (++hops > 40) return null;
      const target = await readlink(`${dir}/${name}`).catch(() => null);
      if (target === null) return null;
      if (target.startsWith("/")) cur = [];
      parts = [...target.split("/").filter(Boolean), ...parts];
      continue;
    }
    cur.push(name);
  }
  return [root, ...cur].join("/");
}

async function readText(root: string, rel: string, max = 256 * 1024): Promise<string | null> {
  const p = await resolveUnder(root, rel);
  if (!p) return null;
  const st = await stat(p).catch(() => null);
  if (!st?.isFile() || st.size > max) return null;
  return Bun.file(p).text().catch(() => null);
}

async function listNames(root: string, rel: string): Promise<{ name: string; dir: boolean }[]> {
  const p = await resolveUnder(root, rel);
  if (!p) return [];
  const ents = await readdir(p, { withFileTypes: true }).catch(() => []);
  return ents.map((e) => ({ name: e.name, dir: e.isDirectory() }));
}

async function peArch(path: string): Promise<string | null> {
  try {
    const fh = await open(path, "r");
    try {
      const b = new Uint8Array(4096);
      const { bytesRead } = await fh.read(b, 0, b.length, 0);
      const v = new DataView(b.buffer, 0, bytesRead);
      if (bytesRead < 0x40 || b[0] !== 0x4d || b[1] !== 0x5a) return null; // "MZ"
      const pe = v.getUint32(0x3c, true);
      if (pe + 6 > bytesRead || v.getUint32(pe, true) !== 0x4550) return null; // "PE\0\0"
      const m = v.getUint16(pe + 4, true);
      return PE_MACHINES[m] || `machine 0x${m.toString(16)}`;
    } finally {
      await fh.close();
    }
  } catch {
    return null;
  }
}

function parseKeyValues(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of text.split("\n")) {
    const m = line.match(/^\s*([A-Za-z_][A-Za-z0-9_]*)=(.*)$/);
    if (m) out[m[1]] = m[2].trim().replace(/^(["'])(.*)\1$/, "$2");
  }
  return out;
}

// ==================== Partition lookups ====================

const norm = (s: string | null | undefined) => (s || "").toLowerCase();

// A GPT GUID as it is stored on disk: the first three groups little-endian.
function guidBytes(g: string): Uint8Array | null {
  const h = g.replace(/[{}-]/g, "");
  if (!/^[0-9a-f]{32}$/i.test(h)) return null;
  const b = Buffer.from(h, "hex");
  return Uint8Array.from([b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], ...b.subarray(8)]);
}

function indexOf(hay: Uint8Array, needle: Uint8Array): number {
  outer: for (let i = 0; i + needle.length <= hay.length; i++) {
    for (let j = 0; j < needle.length; j++) if (hay[i + j] !== needle[j]) continue outer;
    return i;
  }
  return -1;
}

async function partStart(name: string): Promise<number | null> {
  const s = parseInt(await Bun.file(`/sys/class/block/${name}/start`).text().catch(() => ""));
  return Number.isFinite(s) ? s * 512 : null;
}

// Which partition a BCD device element points at. Rather than decode the device-path
// structure (several undocumented layouts), look for the identifiers in it: the GPT
// partition GUID, or for MBR the partition's byte offset next to the disk signature.
async function bcdDevicePartition(blob: Uint8Array, disk: LsblkDisk): Promise<string | null> {
  for (const p of disk.children || []) {
    if (disk.pttype === "gpt") {
      const g = p.partuuid ? guidBytes(p.partuuid) : null;
      if (g && indexOf(blob, g) >= 0) return p.name;
    } else if (disk.pttype === "dos" && p.partuuid) {
      const start = await partStart(p.name);
      const sig = parseInt(p.partuuid.split("-")[0], 16);
      if (start === null || !Number.isFinite(sig)) continue;
      const off = new Uint8Array(8);
      new DataView(off.buffer).setBigUint64(0, BigInt(start), true);
      const s = new Uint8Array(4);
      new DataView(s.buffer).setUint32(0, sig, true);
      if (indexOf(blob, off) >= 0 && indexOf(blob, s) >= 0) return p.name;
    }
  }
  return null;
}

// ==================== ESP ====================

async function readBcd(path: string, disk: LsblkDisk): Promise<EspInfo["windowsBoot"]> {
  return withHive(path, async (h: Hive) => {
    const objects = await h.child(await h.root(), "Objects");
    if (!objects) return null;
    const objs = new Map<string, { type: number; els: RegKey }>();
    for (const o of await h.subkeys(objects)) {
      const type = await h.num(await h.child(o, "Description"), "Type");
      const els = await h.child(o, "Elements");
      if (type !== null && els) objs.set(o.name.toLowerCase(), { type, els });
    }
    const el = async (els: RegKey, id: string) => h.get(await h.child(els, id), "Element");
    const u64 = (v: unknown) => (v instanceof Uint8Array && v.length >= 8 ? Number(new DataView(v.buffer, v.byteOffset, 8).getBigUint64(0, true)) : null);

    let order: string[] = [];
    let def: string | null = null;
    let timeout: number | null = null;
    const mgr = [...objs.values()].find((o) => o.type === BCD_BOOTMGR);
    if (mgr) {
      const o = await el(mgr.els, "24000001"); // displayorder
      if (Array.isArray(o)) order = o.map((s) => s.toLowerCase());
      const d = await el(mgr.els, "23000003"); // default
      if (typeof d === "string") def = d.toLowerCase();
      timeout = u64(await el(mgr.els, "25000004"));
    }

    const entries: BcdEntry[] = [];
    for (const [id, o] of objs) {
      if (o.type !== BCD_OSLOADER && o.type !== BCD_TOOL) continue;
      const description = await el(o.els, "12000004");
      const path = await el(o.els, "12000002");
      const device = await el(o.els, "11000001");
      const deviceText = device instanceof Uint8Array ? Buffer.from(device).toString("utf16le") : "";
      const kind: BcdEntry["kind"] =
        o.type === BCD_TOOL ? "tool" : /recovery/i.test(String(description)) || /\.wim\b/i.test(deviceText) ? "recovery" : "os";
      entries.push({
        description: typeof description === "string" ? description : "(unnamed)",
        path: typeof path === "string" ? path : null,
        partition: device instanceof Uint8Array ? await bcdDevicePartition(device, disk) : null,
        kind,
        isDefault: id === def,
        inMenu: order.includes(id),
      });
    }
    // Menu order first, then the rest (recovery, tools).
    entries.sort((a, b) => Number(b.inMenu) - Number(a.inMenu) || ["os", "recovery", "tool"].indexOf(a.kind) - ["os", "recovery", "tool"].indexOf(b.kind));
    return { timeout, entries };
  });
}

async function readSystemdBoot(root: string): Promise<EspInfo["systemdBoot"]> {
  const entries: { title: string; version: string | null; file: string }[] = [];
  for (const e of await listNames(root, "loader/entries")) {
    if (e.dir || !e.name.toLowerCase().endsWith(".conf")) continue;
    const text = await readText(root, `loader/entries/${e.name}`, 64 * 1024);
    if (!text) continue;
    const kv: Record<string, string> = {};
    for (const line of text.split("\n")) {
      const m = line.match(/^\s*(\S+)\s+(.+?)\s*$/);
      if (m && !m[1].startsWith("#") && !(m[1] in kv)) kv[m[1]] = m[2];
    }
    entries.push({ title: kv.title || e.name.replace(/\.conf$/i, ""), version: kv.version || null, file: e.name });
  }
  const conf = await readText(root, "loader/loader.conf", 64 * 1024);
  const def = conf?.match(/^\s*default\s+(\S+)/m)?.[1] || null;
  if (!entries.length && !conf) return null;
  return { default: def, entries };
}

async function fileHash(path: string): Promise<string | null> {
  try {
    return Bun.hash(await Bun.file(path).arrayBuffer()).toString(16);
  } catch {
    return null;
  }
}

async function analyseEsp(root: string, disk: LsblkDisk): Promise<EspInfo> {
  const loaders: Loader[] = [];
  const allEfi: { rel: string; path: string }[] = [];

  for (const d of await listNames(root, "EFI")) {
    if (!d.dir) continue;
    const key = d.name.toLowerCase();
    const vendor = EFI_VENDORS[key];
    // Microsoft keeps its loaders one level down, in Boot/.
    const sub = key === "microsoft" ? `EFI/${d.name}/Boot` : `EFI/${d.name}`;
    const files = (await listNames(root, sub)).filter((f) => !f.dir && f.name.toLowerCase().endsWith(".efi")).map((f) => f.name);
    for (const f of files) {
      const p = await resolveUnder(root, `${sub}/${f}`);
      if (p) allEfi.push({ rel: `${sub.slice(4)}/${f}`, path: p });
    }
    const lower = files.map((f) => f.toLowerCase());
    const main =
      files[lower.findIndex((f) => f.startsWith("shim"))] ??
      files[lower.findIndex((f) => f.startsWith("grub"))] ??
      files[lower.findIndex((f) => f === "bootmgfw.efi")] ??
      files[lower.findIndex((f) => f.startsWith("systemd-boot"))] ??
      files[lower.findIndex((f) => f.startsWith("refind"))] ??
      files[lower.findIndex((f) => /^boot(x64|aa64|ia32|arm|riscv64)\.efi$/.test(f))] ??
      files[0];
    const mainPath = main ? await resolveUnder(root, `${sub}/${main}`) : null;

    const loader: Loader = {
      dir: d.name,
      name: vendor?.name || d.name,
      kind: vendor?.kind || "unknown",
      files,
      arch: mainPath ? await peArch(mainPath) : null,
      shim: lower.some((f) => f.startsWith("shim")),
    };

    // A distro's ESP grub.cfg is a stub that finds the real /boot by filesystem UUID.
    const cfg = await readText(root, `EFI/${d.name}/grub.cfg`, 64 * 1024);
    const uuid = cfg?.match(/(?:search\.fs_uuid|--fs-uuid)(?:\s+--?\S+)*\s+([0-9A-Fa-f][0-9A-Fa-f-]{7,})/)?.[1];
    if (uuid) {
      const part = (disk.children || []).find((p) => norm(p.uuid) === norm(uuid));
      loader.target = { uuid, partition: part?.name || null, system: null };
    }
    loaders.push(loader);
  }
  const KIND_ORDER: Loader["kind"][] = ["os", "bootmanager", "fallback", "firmware", "unknown"];
  loaders.sort((a, b) => KIND_ORDER.indexOf(a.kind) - KIND_ORDER.indexOf(b.kind) || a.dir.localeCompare(b.dir));

  // The firmware's last resort, EFI/BOOT/BOOT<arch>.EFI, is almost always a copy of some
  // other loader — say which.
  let fallback: EspInfo["fallback"] = null;
  const fb = allEfi.find((e) => /^boot\/boot(x64|aa64|ia32|arm|riscv64)\.efi$/i.test(e.rel));
  if (fb) {
    const size = (await stat(fb.path).catch(() => null))?.size;
    let sameAs: string | null = null;
    const h = size ? await fileHash(fb.path) : null;
    for (const o of allEfi) {
      if (o === fb || /^boot\//i.test(o.rel) || (await stat(o.path).catch(() => null))?.size !== size) continue;
      if (h && (await fileHash(o.path)) === h) {
        sameAs = o.rel;
        break;
      }
    }
    fallback = { file: fb.rel, arch: await peArch(fb.path), sameAs };
  }

  const bcdPath = await resolveUnder(root, "EFI/Microsoft/Boot/BCD");
  return {
    loaders,
    fallback,
    windowsBoot: bcdPath ? await readBcd(bcdPath, disk).catch(() => null) : null,
    systemdBoot: await readSystemdBoot(root),
  };
}

// ==================== OS partitions ====================

async function analyseWindows(root: string): Promise<WindowsInfo | null> {
  const software = await resolveUnder(root, "Windows/System32/config/SOFTWARE");
  if (!software) return null;
  const info = await withHive(software, async (h) => {
    const cv = await h.path(await h.root(), ["Microsoft", "Windows NT", "CurrentVersion"]);
    if (!cv) return null;
    const build = (await h.str(cv, "CurrentBuild")) || (await h.str(cv, "CurrentBuildNumber"));
    const ubr = await h.num(cv, "UBR");
    let product = (await h.str(cv, "ProductName")) || "Windows";
    // Windows 11 never updated ProductName; the build number is what tells them apart.
    if (build && parseInt(build) >= 22000) product = product.replace(/^Windows 10/, "Windows 11");
    const installDate = await h.num(cv, "InstallDate");
    return {
      product,
      edition: await h.str(cv, "EditionID"),
      version: (await h.str(cv, "DisplayVersion")) || (await h.str(cv, "ReleaseId")),
      build: build ? (ubr !== null ? `${build}.${ubr}` : build) : null,
      installed: installDate ? installDate * 1000 : null,
      computerName: null as string | null,
    };
  }).catch(() => null);
  if (!info) return null;

  const system = await resolveUnder(root, "Windows/System32/config/SYSTEM");
  if (system) {
    info.computerName = await withHive(system, async (h) => {
      const r = await h.root();
      const cur = (await h.num(await h.child(r, "Select"), "Current")) || 1;
      const cn = await h.path(r, [`ControlSet${String(cur).padStart(3, "0")}`, "Control", "ComputerName", "ComputerName"]);
      return h.str(cn, "ComputerName");
    }).catch(() => null);
  }
  return info;
}

async function analyseLinux(root: string): Promise<LinuxInfo | null> {
  const text = (await readText(root, "etc/os-release", 64 * 1024)) ?? (await readText(root, "usr/lib/os-release", 64 * 1024));
  if (!text) return null;
  const kv = parseKeyValues(text);
  return {
    name: kv.PRETTY_NAME || [kv.NAME, kv.VERSION].filter(Boolean).join(" ") || "Linux",
    id: kv.ID || null,
    version: kv.VERSION_ID || null,
    hostname: (await readText(root, "etc/hostname", 4096))?.trim() || null,
  };
}

async function analysePiBoot(root: string, disk: LsblkDisk): Promise<PiBootInfo | null> {
  const names = new Set((await listNames(root, "")).map((e) => e.name.toLowerCase()));
  const firmware = [...names].some((n) => /^start\d*\.elf$/.test(n) || /^kernel(\d|_2712)?.*\.img$/.test(n));
  if (!names.has("config.txt") || !firmware) return null;
  const cmdline = await readText(root, "cmdline.txt", 64 * 1024);
  const rootPartuuid = cmdline?.match(/\broot=PARTUUID=(\S+)/)?.[1] || null;
  const issue = await readText(root, "issue.txt", 64 * 1024);
  return {
    image: issue?.split("\n")[0].trim() || null,
    rootPartuuid,
    rootPartition: rootPartuuid ? (disk.children || []).find((p) => norm(p.partuuid) === norm(rootPartuuid))?.name || null : null,
  };
}

async function analyseMac(root: string): Promise<PartInfo["macos"] | null> {
  const plist = await readText(root, "System/Library/CoreServices/SystemVersion.plist", 64 * 1024);
  if (!plist) return null;
  const get = (k: string) => plist.match(new RegExp(`<key>${k}</key>\\s*<string>([^<]*)</string>`))?.[1] || null;
  return { name: get("ProductName") || "macOS", version: get("ProductUserVisibleVersion") || get("ProductVersion") };
}

// ==================== Disk ====================

export async function analyseDisk(disk: LsblkDisk, filesRoot: string): Promise<DiskInfo> {
  const parts: PartInfo[] = [];
  for (const p of disk.children || []) {
    const mp = p.mountpoint || "";
    const mounted = mp.startsWith(filesRoot + "/");
    const info: PartInfo = {
      name: p.name,
      role: PART_ROLES[norm(p.parttype)] || null,
      fstype: p.fstype || null,
      label: p.label || p.partlabel || null,
      path: mounted ? mp.slice(filesRoot.length + 1) : null,
      note: (p.fstype && FS_NOTES[p.fstype]) || null,
    };
    if (mounted) {
      try {
        const isEsp = ESP_TYPES.has(norm(p.parttype)) || (/^vfat$/i.test(p.fstype || "") && (await resolveUnder(mp, "EFI")) !== null);
        if (isEsp) info.esp = await analyseEsp(mp, disk);
        const win = await analyseWindows(mp);
        if (win) info.windows = win;
        const linux = await analyseLinux(mp);
        if (linux) info.linux = linux;
        const pi = await analysePiBoot(mp, disk);
        if (pi) info.pi = pi;
        const mac = await analyseMac(mp);
        if (mac) info.macos = mac;
      } catch (e: any) {
        info.error = e.message;
      }
    } else if (!info.note && p.fstype && !mp) {
      info.note = "Not mounted — contents not inspected";
    }
    parts.push(info);
  }

  const byName = new Map(parts.map((p) => [p.name, p]));
  // Fill in what each GRUB stub ends up booting, now that every partition is known.
  for (const p of parts) {
    for (const l of p.esp?.loaders || []) {
      if (l.target?.partition) l.target.system = byName.get(l.target.partition)?.linux?.name || null;
    }
  }

  return { device: disk.name, partitionTable: disk.pttype || null, partitions: parts, systems: summarise(parts), analysedAt: Date.now() };
}

function summarise(parts: PartInfo[]): SystemSummary[] {
  const systems: SystemSummary[] = [];
  const esps = parts.filter((p) => p.esp);
  const bcdEntries = esps.flatMap((p) => p.esp!.windowsBoot?.entries || []);
  const grubLoaders = esps.flatMap((p) => p.esp!.loaders.filter((l) => l.target));

  for (const p of parts) {
    if (p.windows) {
      const w = p.windows;
      const via = bcdEntries.filter((e) => e.partition === p.name && e.kind === "os").map((e) => `Windows Boot Manager: "${e.description}"`);
      systems.push({
        kind: "windows",
        name: [w.product, w.version].filter(Boolean).join(" "),
        detail: [w.build && `build ${w.build}`, w.computerName && `PC name ${w.computerName}`].filter(Boolean).join(" · ") || null,
        partition: p.name,
        bootsVia: via,
      });
    }
    if (p.linux) {
      const via = grubLoaders.filter((l) => l.target!.partition === p.name).map((l) => `EFI/${l.dir}`);
      for (const q of parts) if (q.pi?.rootPartition === p.name) via.push(`Raspberry Pi bootfs (${q.name})`);
      systems.push({
        kind: parts.some((q) => q.pi?.rootPartition === p.name) ? "pi" : "linux",
        name: p.linux.name,
        detail: p.linux.hostname ? `hostname ${p.linux.hostname}` : null,
        partition: p.name,
        bootsVia: via,
      });
    }
    if (p.macos) {
      systems.push({ kind: "macos", name: [p.macos.name, p.macos.version].filter(Boolean).join(" "), detail: null, partition: p.name, bootsVia: [] });
    }
  }

  // Boot entries whose OS partition couldn't be read (encrypted, unmounted) still say
  // something is installed.
  for (const e of bcdEntries) {
    if (e.kind !== "os" || !e.inMenu) continue;
    if (e.partition && parts.find((p) => p.name === e.partition)?.windows) continue;
    systems.push({ kind: "windows", name: e.description, detail: "from the Windows boot menu; partition not readable", partition: e.partition, bootsVia: ["Windows Boot Manager"] });
  }
  for (const p of parts) {
    if (p.pi && (!p.pi.rootPartition || !byNameHasLinux(parts, p.pi.rootPartition))) {
      systems.push({ kind: "pi", name: p.pi.image || "Raspberry Pi OS", detail: "rootfs not readable", partition: p.pi.rootPartition, bootsVia: [`Raspberry Pi bootfs (${p.name})`] });
    }
  }
  return systems;
}

function byNameHasLinux(parts: PartInfo[], name: string): boolean {
  return !!parts.find((p) => p.name === name)?.linux;
}
