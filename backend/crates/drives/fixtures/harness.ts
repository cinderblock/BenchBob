// Regenerates this directory's *.expected.json from the TypeScript server's own code, so
// the Rust port is checked against what server.ts, rpiboot.ts and osinfo.ts actually do.
//
//   bun backend/crates/drives/fixtures/harness.ts        (from the repository root)
//
// server.ts can't be imported (it starts a server), so the sections under test are cut
// out of its source by their "// ====" headings and loaded behind a preamble that swaps
// every outside effect for a scripted mock: commands (run, Bun.spawn), files, fetch,
// sleeps and the clock. Each scenario in scenarios.json says how commands answer; the
// mock records every command (with its stdin), every stream read and every file check,
// together with the operation's state at that moment. The Rust tests replay the same
// scenarios against their own mock (src/sys.rs) and compare the logs entry by entry.

import { mkdirSync, readFileSync, writeFileSync, rmSync, mkdtempSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { pathToFileURL } from "node:url";

const HERE = import.meta.dir.replace(/\\/g, "/");
const ROOT = `${HERE}/../../../..`;
const lf = (s: string) => s.replace(/\r\n/g, "\n");
const read = (p: string) => lf(readFileSync(p, "utf8"));
const out = (name: string, v: unknown) => writeFileSync(`${HERE}/${name}`, JSON.stringify(v, null, 2) + "\n");

const RealBun = globalThis.Bun;

// ---------- the mock ----------

let LOG: any[] = [];
let CUR: any = null;
let SCN: any = {};

const snap = () => (CUR ? { status: CUR.status, progress: CUR.progress, message: CUR.message } : undefined);
function rec(e: any) {
  const s = snap();
  if (s) e.op = s;
  LOG.push(e);
  return e;
}
function answer(cmd: string[]): any {
  const line = cmd.join(" ");
  const as: any[] = SCN.answers || [];
  return as.find((a) => a.match === line) || as.find((a) => a.match && line.startsWith(a.match)) || {};
}
const bytes = (s: string, b64: boolean) => (b64 ? new Uint8Array(Buffer.from(s, "base64")) : new TextEncoder().encode(s));

// A stream that hands out one chunk per read (highWaterMark 0: nothing is read ahead), and
// when scripted chunk by chunk, records every read with the operation's state then.
function stream(chunks: string[], recording: boolean, b64 = false, fd = 1) {
  const list = chunks.map((c) => bytes(c, b64));
  let n = 0;
  return new ReadableStream(
    {
      pull(c) {
        if (recording) rec({ read: n, fd });
        n++;
        const x = list.shift();
        if (x) c.enqueue(x);
        else c.close();
      },
    },
    { highWaterMark: 0 },
  );
}

function spawn(cmd: string[]) {
  const e = rec({ cmd });
  const a = answer(cmd);
  let killed = false;
  const so = a.stdout ? stream(a.stdout, true, false, 1) : a.stdout_b64 ? stream(a.stdout_b64, true, true, 1) : stream(a.out_b64 ? [a.out_b64] : a.out ? [a.out] : [], false, !!a.out_b64);
  const se = a.stderr ? stream(a.stderr, true, false, 2) : stream(a.err ? [a.err] : [], false);
  return {
    stdin: { write(d: string) { e.stdin = (e.stdin || "") + d; }, end() {} },
    stdout: so,
    stderr: se,
    get exited() { return Promise.resolve(killed ? -1 : (a.code ?? 0)); },
    kill() { killed = true; rec({ kill: true }); },
  };
}

const mockBun: any = {
  spawn: (cmd: string[]) => spawn(cmd),
  sleep: async (ms: number) => { rec({ sleep: ms }); },
  CryptoHasher: RealBun.CryptoHasher,
  hash: RealBun.hash,
  file: (p: string) => ({
    exists: async () => { rec({ exists: p }); return p in (SCN.files || {}); },
    text: async () => { if (p in (SCN.files || {})) return SCN.files[p]; throw new Error("ENOENT"); },
    json: async () => JSON.parse(SCN.files?.[p] ?? "!"),
    arrayBuffer: async () => new TextEncoder().encode(SCN.files?.[p] ?? "").buffer,
    writer: () => {
      const last = [...LOG].reverse().find((x) => x.fetch);
      if (last) last.to = p;
      return { write() {}, end: async () => {} };
    },
  }),
  write: async (p: string, data: string) => { rec({ write: p, data }); },
};

const harness: any = {
  Bun: mockBun,
  run: async (cmd: string[]) => {
    rec({ cmd });
    const a = answer(cmd);
    return { out: (a.out || "").trim(), err: (a.err || "").trim(), code: a.code ?? 0 };
  },
  readSysFile: async (p: string) => String(SCN.files?.[p] ?? "").trim(),
  fetch: async (url: string) => {
    rec({ fetch: url });
    const chunks: string[] = SCN.fetch || [];
    const total = chunks.reduce((n, c) => n + c.length, 0);
    return new Response(stream(chunks, false), { status: SCN.fetchStatus ?? 200, headers: { "content-length": String(total) } });
  },
  rm: async (p: string) => { rec({ rm: p }); },
  access: async (p: string) => { rec({ access: p }); if (!(SCN.readable || []).includes(p)) throw new Error("EACCES"); },
  // Date.now() is the scenario's "now" (instance ids, timestamps).
  Date: class extends globalThis.Date { static now() { return SCN.now ?? 1792490400000; } },
  rpi: null as any,
  update: null as any,
  diskRates: {} as Record<string, [number, number]>,
  rootDisk: null as any,
};
(globalThis as any).__harness = harness;

// ---------- load the code under test ----------

const work = mkdtempSync(`${tmpdir()}/bb-harness-`).replace(/\\/g, "/");

// update.ts as is, but with its key read with LF line ends whatever the checkout.
writeFileSync(`${work}/update.ts`, read(`${ROOT}/update.ts`).replace('readFileSync(p, "utf8")', 'readFileSync(p, "utf8").replace(/\\r\\n/g, "\\n")').replace("${import.meta.dir}/packaging", `${ROOT}/packaging`).replace('from "./paths.ts"', `from "${ROOT}/paths.ts"`));
harness.update = await import(pathToFileURL(`${work}/update.ts`).href);

const IMAGES = JSON.parse(read(`${HERE}/eeprom-images.json`));
const RPI: any = await import(pathToFileURL(`${ROOT}/rpiboot.ts`).href);
harness.rpi = {
  CARD_BOARDS: { "2711": "Raspberry Pi 4 / 400", "2712": "Raspberry Pi 5 / 500" },
  eepromImages: async (chip: string) => IMAGES[chip] || [],
  buildCardFiles: async (chip: string, image: any, bootConf: string) => {
    rec({ buildCardFiles: [chip, image.id, bootConf] });
    const files = ["recovery.bin", "pieeprom.bin", "pieeprom.sig", ...(chip === "2711" ? ["vl805.bin", "vl805.sig"] : []), "README.txt"];
    return { dir: "/tmp/benchbob-rpiboot/card", files, imageSha256: "sha" };
  },
  getBootDevices: async () => SCN.bootDevices || [],
  rpibootInstalled: async () => true,
  decodeBoardRev: RPI.decodeBoardRev,
  loadMassStorage: async (dev: any) => { rec({ rpi: "loadMassStorage", path: dev.path }); return SCN.rpiboot; },
  readBootloaderInfo: async (dev: any) => { rec({ rpi: "readBootloaderInfo", path: dev.path }); return SCN.rpiboot; },
  writeBootloader: async (dev: any, image: any, bootConf: string) => { rec({ rpi: "writeBootloader", path: dev.path, image: image.id, bootConf }); return SCN.rpiboot; },
  recordWrite: async (r: any) => { rec({ rpi: "recordWrite", record: r }); },
  identifyBootloader: async (chip: string, md: any) => { rec({ rpi: "identifyBootloader", chip }); return { source: "unknown", buildDate: null, label: "x", bootConf: null }; },
};

const src = read(`${ROOT}/server.ts`);
function section(from: string, to: string): string {
  const a = src.indexOf(from), b = src.indexOf(to, a);
  if (a < 0 || b < 0) throw new Error(`server.ts section not found: ${from}`);
  return src.slice(a, b);
}
const preamble = `
const __h = (globalThis as any).__harness;
const Bun = __h.Bun, Date = __h.Date, fetch = __h.fetch, rm = __h.rm, access = __h.access, rpi = __h.rpi, update = __h.update;
const fsConstants = { R_OK: 4 };
const statePath = (n: string) => "/state/" + n;
const FLASH_DIR = "/var/tmp/benchbob-flash";
const BOOT_MOUNT = "/tmp/benchbob-boot";
async function readSysFile(p: string) { return __h.readSysFile(p); }
async function run(cmd: string[]) { return __h.run(cmd); }
const diskRates = __h.diskRates;
let rootDisk: any = __h.rootDisk;
`;
const body = [
  section("function fmtBytes", "// ==================== History Ring Buffer"),
  section("// ==================== USB Speed", "// ==================== Root Storage"),
  section("// ==================== Block Devices", "// ==================== Host Power"),
  section("// ==================== Operation Queue", "// ==================== Device Actions"),
  section("// ==================== Device Actions", "// ==================== USB Boot Mode (rpiboot)"),
  section("// ==================== USB Boot Mode (rpiboot)", "// ==================== RPi Image Catalog"),
  section("// ==================== RPi Image Catalog", "// ==================== SSE"),
  section("// ==================== File Browser", "// ==================== OS Detection"),
].join("\n");
const exports = `export { getDevices, deviceOps, opPublic, doErase, doFlash, doBootloaderCard, doDeepScan, doRpiboot, rpibootResults,
  ejectDevice, scanDevice, probeDevice, classifyBlock, findSignatures, spreadBlocks, mkCloudInitUserData, mkCloudInitNetworkConfig,
  mkFirstrun, flattenImages, listDir, downloadFile, parseRange, contentDisposition, fmtBytes };
export const setRootDisk = (r: any) => { rootDisk = r; };`;
writeFileSync(`${work}/server-parts.ts`, preamble + body + "\n" + exports);
const S: any = await import(pathToFileURL(`${work}/server-parts.ts`).href);

// ---------- devices ----------

{
  SCN = JSON.parse(read(`${HERE}/devices-scenario.json`));
  LOG = [];
  CUR = null;
  Object.assign(harness.diskRates, SCN.rates);
  S.setRootDisk({ name: "mmcblk0" });
  S.deviceOps.set("sda", [{ id: "op-1", type: "erase", device: "sda", status: "erasing", progress: 30, message: "Zeroing partition table..." }]);
  // getDevices runs lsblk through run(): answer it with the fixture.
  SCN.answers = [{ match: "lsblk -J -b -o", out: read(`${HERE}/lsblk-desky-usb.json`) }, ...SCN.answers];
  out("devices-desky-usb.expected.json", await S.getDevices());
  S.deviceOps.clear();
  for (const k of Object.keys(harness.diskRates)) delete harness.diskRates[k];
}

// ---------- operations and device actions ----------

const results: Record<string, any> = {};
for (const scn of JSON.parse(read(`${HERE}/scenarios.json`))) {
  SCN = scn;
  LOG = [];
  CUR = null;
  const a = scn.args;
  let result: any;
  const op = (type: string, extra: any = {}) => ({ id: "op", type, device: a.device, status: "starting", progress: 0, message: "Starting...", ...extra });
  const runOp = async (o: any, fn: (o: any) => Promise<void>) => {
    CUR = o;
    try {
      await fn(o);
    } catch (e: any) {
      if (o.status !== "error") { o.status = "error"; o.message = e.message; }
    }
    CUR = null;
    return S.opPublic(o);
  };
  switch (scn.fn) {
    case "erase": result = await runOp(op("erase", { mode: a.mode }), S.doErase); break;
    case "flash": result = await runOp(op("flash", { imageUrl: a.imageUrl, imageName: a.imageName, extractSize: a.extractSize, initFormat: a.initFormat, settings: a.settings }), S.doFlash); break;
    case "blcard": result = await runOp(op("blcard", { chip: a.chip, eepromImageId: a.imageId, bootConf: a.bootConf }), S.doBootloaderCard); break;
    case "deepscan": result = await runOp(op("deepscan"), S.doDeepScan); break;
    case "rpiboot": {
      result = await runOp({ id: "op", type: a.type, device: `usb:${a.usbPath}`, status: "starting", progress: 0, message: "Starting...", usbPath: a.usbPath, eepromImageId: a.imageId, bootConf: a.bootConf }, S.doRpiboot);
      result = { op: result, results: [...S.rpibootResults.values()].map((r: any) => ({ ...r, at: "<at>" })) };
      S.rpibootResults.clear();
      break;
    }
    case "eject": result = await S.ejectDevice(a.device); break;
    case "scan": result = await S.scanDevice(a.device); break;
    case "probe": result = await S.probeDevice(a.device); break;
    case "listDir": try { result = await S.listDir(a.path); } catch (e: any) { result = { error: e.message, status: e.status }; } break;
    case "download": {
      try {
        const r: Response = await S.downloadFile(a.path, new Request("http://x/api/download", { method: a.method || "GET", headers: a.range ? { range: a.range } : {} }));
        result = { status: r.status, headers: Object.fromEntries(r.headers.entries()) };
      } catch (e: any) {
        result = { error: e.message, status: e.status };
      }
      break;
    }
    default: throw new Error(`unknown scenario fn ${scn.fn}`);
  }
  results[scn.name] = { log: LOG, result };
}
out("scenarios.expected.json", results);

// ---------- flash customisation generators ----------

{
  SCN = { answers: [{ match: "openssl passwd -6 -stdin", out: "$6$fixture$0123456789abcdef\n" }] };
  const gen: Record<string, any> = {};
  for (const [name, s] of Object.entries(JSON.parse(read(`${HERE}/settings.json`)))) {
    LOG = [];
    gen[name] = { userData: await S.mkCloudInitUserData(s), networkConfig: S.mkCloudInitNetworkConfig(structuredClone(s)), firstrun: S.mkFirstrun(s), log: LOG };
  }
  out("settings.expected.json", gen);
}

// ---------- raw sector probe: block classification and signatures ----------

{
  // Deterministic blocks (xorshift), so both sides classify the same bytes.
  let x = 2463534242;
  const rnd = () => ((x ^= x << 13), (x ^= x >>> 17), (x ^= x << 5), (x >>> 0) & 0xff);
  const block = (kind: string) => {
    const b = new Uint8Array(4096);
    if (kind === "ones") b.fill(0xff);
    if (kind === "random") for (let i = 0; i < b.length; i++) b[i] = rnd();
    if (kind === "text") for (let i = 0; i < b.length; i++) b[i] = "The quick brown fox\n".charCodeAt(i % 20);
    if (kind === "mbr") { b[510] = 0x55; b[511] = 0xaa; b.set(new TextEncoder().encode("FAT32   "), 0x52); }
    if (kind === "mixed") { for (let i = 0; i < 2048; i++) b[i] = rnd() & 0x3f; b.set([0x7f, 0x45, 0x4c, 0x46], 100); b.set([0x1f, 0x8b, 0x08], 4000); b.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], 4088); }
    if (kind === "ext4") { b.set([0x53, 0xef], 0x438); b.set(new TextEncoder().encode("hsqs"), 9); }
    return b;
  };
  const kinds = ["zero", "ones", "random", "text", "mbr", "mixed", "ext4"];
  const blocks = kinds.map((k, i) => {
    const b = block(k);
    return { kind: k, b64: Buffer.from(b).toString("base64"), classify: S.classifyBlock(b, i * 4096), sigs: S.findSignatures(b, i * 4096) };
  });
  // Anchored signatures in a buffer that doesn't start on a block boundary (the deep scan's carry-over).
  const off = new Uint8Array(9000);
  off.set([0x55, 0xaa], 4096 - 100 + 510);
  off.set([0x55, 0xaa], 200);
  const spread = [[0, 49152, 192], [0, 1 << 20, 16], [4096, 8191, 4], [0, 4096 * 1000, 7], [100, 5000, 3], [8192, 8192, 2]].map(([s, e, n]) => ({ args: [s, e, n], blocks: S.spreadBlocks(s, e, n) }));
  out("blocks.expected.json", { blocks, offset: { b64: Buffer.from(off).toString("base64"), base: 100, sigs: S.findSignatures(off, 100) }, spread });
}

// ---------- catalog ----------

{
  const os = JSON.parse(read(`${HERE}/os_list_v4.json`));
  out("catalog.expected.json", { images: S.flattenImages(os.os_list || []), devices: (os.imager?.devices || []).map((d: any) => ({ name: d.name, tags: d.tags || [], description: d.description || "", exclusive: d.matching_type === "exclusive" })) });
}

// ---------- fmtBytes ----------

out("fmtbytes.expected.json", [0, 1, 512, 1023, 1024, 1280, 1536, 10239, 1048576, 1572864, 4194304 * 250, 15978201088, 62742792192, 1099511627776 * 3.25, 5e15].map((n) => [n, S.fmtBytes(n)]));

rmSync(work, { recursive: true, force: true });
console.log("fixtures written");
