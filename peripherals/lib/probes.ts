// ==================== Debug probes: pure logic ====================
//
// Everything here is side-effect free (or, for TclClient, only touches the socket it is
// given), so the detection table, OpenOCD command lines, log parsing and UF2 handling can be
// unit-tested with fixtures on any machine. The driver (peripherals/drivers/probes.ts) owns
// processes, udev and HTTP.

import type { Props } from "../core.ts";

// ---------- Device table ----------

export type Transport = "swd" | "jtag";

export interface ProbeType {
  kind: string; // "stlink", "jlink", "cmsis-dap", "ftdi", "esp-usb-jtag", "bmp", "wch-link", "atmel"
  name: string;
  /** OpenOCD interface script, relative to the scripts dir; null when OpenOCD can't drive it. */
  iface: string | null;
  transports: Transport[];
  /** Notes shown on the card: caveats, what the device also is. */
  note?: string;
  /** Black Magic Probe: the GDB server runs on the probe itself. */
  gdbBuiltIn?: boolean;
  /** An ESP32 with the built-in USB-JTAG/serial: its ROM bootloader is serial (esptool). */
  espSerial?: boolean;
}

export interface BootType {
  kind: "rp2-bootsel" | "stm32-dfu";
  name: string;
  chip: string;
  tool: string; // the CLI that talks to it natively
  pkg: string; // Debian package providing it
}

const STLINK: Record<string, string> = {
  "3744": "ST-LINK/V1",
  "3748": "ST-LINK/V2",
  "374b": "ST-LINK/V2-1",
  "3752": "ST-LINK/V2-1",
  "374d": "STLINK-V3MINIE",
  "374e": "STLINK-V3",
  "374f": "STLINK-V3",
  "3753": "STLINK-V3",
  "3754": "STLINK-V3",
  "3757": "STLINK-V3PWR",
};

const DAP_NOTE = "CMSIS-DAP";

/** Look a USB device up by VID:PID (lower-case hex) and its strings. */
export function probeType(vid: string, pid: string, strings: string[] = []): ProbeType | null {
  vid = vid.toLowerCase();
  pid = pid.toLowerCase();
  const arm: Transport[] = ["swd", "jtag"];
  if (vid === "0483" && STLINK[pid]) return { kind: "stlink", name: STLINK[pid], iface: "interface/stlink.cfg", transports: arm };
  // SEGGER's VID is only used for J-Links (PIDs vary by model and USB mode).
  if (vid === "1366") return { kind: "jlink", name: "SEGGER J-Link", iface: "interface/jlink.cfg", transports: arm };
  if (vid === "2e8a" && pid === "000c") return { kind: "cmsis-dap", name: "Raspberry Pi Debug Probe", iface: "interface/cmsis-dap.cfg", transports: ["swd"] };
  if (vid === "0d28" && pid === "0204") return { kind: "cmsis-dap", name: "DAPLink", iface: "interface/cmsis-dap.cfg", transports: arm };
  if (vid === "1d50" && pid === "6018")
    return { kind: "bmp", name: "Black Magic Probe", iface: null, transports: arm, gdbBuiltIn: true, note: "Runs its own GDB server on its first serial port; OpenOCD isn't needed." };
  if (vid === "1a86" && pid === "8010")
    return { kind: "wch-link", name: "WCH-Link (RISC-V mode)", iface: null, transports: [], note: "RISC-V mode needs WCH's OpenOCD fork, which isn't installed. Switch the probe to DAP mode (ARM targets) to use it here." };
  if (vid === "1a86" && pid === "8012") return { kind: "wch-link", name: "WCH-LinkE (DAP mode)", iface: "interface/cmsis-dap.cfg", transports: arm };
  if (vid === "03eb" && pid === "2141") return { kind: "atmel", name: "Atmel-ICE", iface: "interface/cmsis-dap.cfg", transports: arm, note: "SAM (ARM) targets only; AVR debugWIRE/UPDI isn't OpenOCD's." };
  if (vid === "03eb" && pid === "2111") return { kind: "atmel", name: "Atmel EDBG", iface: "interface/cmsis-dap.cfg", transports: arm, note: "SAM (ARM) targets only." };
  if (vid === "03eb" && pid === "2145") return { kind: "atmel", name: "Atmel mEDBG", iface: "interface/cmsis-dap.cfg", transports: arm, note: "SAM (ARM) targets only." };
  if (vid === "0403" && pid === "6010")
    return {
      kind: "ftdi",
      name: "FTDI FT2232H",
      iface: "interface/ftdi/esp32_devkitj_v1.cfg",
      transports: ["jtag"],
      note: "Assumed to be an ESP-Prog (channel A = JTAG, B = UART). Other FT2232H boards need their own interface/ftdi/*.cfg layout.",
    };
  if (vid === "303a" && pid === "1001")
    return {
      kind: "esp-usb-jtag",
      name: "ESP32 built-in USB-JTAG",
      iface: "interface/esp_usb_jtag.cfg",
      transports: ["jtag"],
      espSerial: true,
      note: "An ESP32-S3/C3/C6 with its own USB. JTAG debugging is here; serial flashing (esptool, ROM bootloader) is in the Serial panel.",
    };
  // Any other CMSIS-DAP: the spec requires "CMSIS-DAP" in the product or interface string.
  if (strings.some((s) => /CMSIS[-_ ]?DAP/i.test(s))) return { kind: "cmsis-dap", name: "CMSIS-DAP probe", iface: "interface/cmsis-dap.cfg", transports: arm, note: DAP_NOTE };
  return null;
}

export function bootType(vid: string, pid: string): BootType | null {
  vid = vid.toLowerCase();
  pid = pid.toLowerCase();
  if (vid === "2e8a" && pid === "0003") return { kind: "rp2-bootsel", name: "RP2040 BOOTSEL", chip: "rp2040", tool: "picotool", pkg: "picotool" };
  if (vid === "2e8a" && pid === "000f") return { kind: "rp2-bootsel", name: "RP2350 BOOTSEL", chip: "rp2350", tool: "picotool", pkg: "picotool" };
  if (vid === "0483" && pid === "df11") return { kind: "stm32-dfu", name: "STM32 DFU bootloader", chip: "stm32", tool: "dfu-util", pkg: "dfu-util" };
  return null;
}

/** Strings udev knows for a usb_device: the product string in its raw and encoded forms. */
export function usbStrings(p: Props): string[] {
  const dec = (s?: string) => (s || "").replace(/\\x([0-9a-fA-F]{2})/g, (_, h) => String.fromCharCode(parseInt(h, 16)));
  return [p.ID_MODEL, dec(p.ID_MODEL_ENC), p.PRODUCT_NAME].filter(Boolean) as string[];
}

/** Settings key and URL id: stable across replugs (serial), else the physical port. */
export function stableId(p: Props): string {
  const raw = p.ID_SERIAL || p.ID_PATH || p.DEVPATH || "unknown";
  return raw.replace(/[^A-Za-z0-9_.-]+/g, "_").slice(0, 80);
}

/** The USB port path OpenOCD's `adapter usb location` wants: DEVPATH's last part, "1-1.3". */
export function usbLocation(devpath: string): string | null {
  const last = devpath.split("/").pop() || "";
  return /^\d+-[\d.]+$/.test(last) ? last : null;
}

/**
 * How to make OpenOCD open *this* probe when several of a kind are plugged in.
 * The serial is preferred (survives moving ports). ST-Link/V2 has a binary serial that udev
 * mangles, so it's pinned by port instead.
 */
export function pinCommand(kind: string, pid: string, serial: string | undefined, devpath: string): string | null {
  const clean = serial && /^[\x21-\x7e]{4,64}$/.test(serial) && !/[{}\[\]$"\\;]/.test(serial);
  const binarySerial = kind === "stlink" && (pid === "3748" || pid === "3744");
  if (clean && !binarySerial) return `adapter serial ${serial}`;
  const loc = usbLocation(devpath);
  return loc ? `adapter usb location ${loc}` : null;
}

// ---------- Targets ----------

/** Target presets offered first; only those whose cfg exists are shown. */
export const TARGET_PRESETS: { cfg: string; label: string }[] = [
  { cfg: "target/stm32f0x.cfg", label: "STM32F0" },
  { cfg: "target/stm32f1x.cfg", label: "STM32F1" },
  { cfg: "target/stm32f4x.cfg", label: "STM32F4" },
  { cfg: "target/stm32g0x.cfg", label: "STM32G0" },
  { cfg: "target/stm32g4x.cfg", label: "STM32G4" },
  { cfg: "target/stm32l4x.cfg", label: "STM32L4" },
  { cfg: "target/stm32h7x.cfg", label: "STM32H7" },
  { cfg: "target/rp2040.cfg", label: "RP2040" },
  { cfg: "target/rp2350.cfg", label: "RP2350" },
  { cfg: "target/nordic/nrf52.cfg", label: "nRF52" },
  { cfg: "target/at91samdXX.cfg", label: "SAMD21" },
  { cfg: "target/atsame5x.cfg", label: "SAMD51/E5x" },
  { cfg: "target/esp32.cfg", label: "ESP32" },
  { cfg: "target/esp32s3.cfg", label: "ESP32-S3" },
  { cfg: "target/esp32c3.cfg", label: "ESP32-C3" },
  { cfg: "target/esp32c6.cfg", label: "ESP32-C6" },
];

/** Built-in pseudo-target: any Cortex-M over SWD, for "what's on the other end?". */
export const GENERIC_TARGET = "generic-cortex-m";
/** No target script: just the adapter, for configuring targets by hand over telnet. */
export const NO_TARGET = "none";

/** Xtensa/RISC-V ESP targets are JTAG-only; everything else here defaults to SWD. */
export function defaultTransport(type: ProbeType, target: string): Transport {
  if (/esp32/.test(target) || !type.transports.includes("swd")) return "jtag";
  return "swd";
}

/** Sensible adapter speed (kHz) for a probe/target pair. */
export function defaultSpeed(type: ProbeType, target: string): number {
  if (type.kind === "esp-usb-jtag") return 40000;
  if (/esp32/.test(target)) return 5000;
  return 4000;
}

/** GDB binary a client would use for a target. */
export function gdbFor(target: string): string {
  const m = /esp32(s2|s3|c\d+|h\d+)?/.exec(target);
  if (m) return /^[ch]/.test(m[1] || "") ? "riscv32-esp-elf-gdb" : `xtensa-esp32${m[1] || ""}-elf-gdb`;
  if (/riscv/.test(target)) return "riscv64-unknown-elf-gdb";
  return "arm-none-eabi-gdb";
}

// ---------- OpenOCD command line ----------

export const READY_MARK = "DESKYPI-READY";

export interface Ports {
  gdb: number;
  telnet: number;
  tcl: number;
}

export interface OcdOptions {
  iface: string;
  target: string; // "target/x.cfg", GENERIC_TARGET or NO_TARGET
  transport: Transport;
  speed: number; // kHz
  pin?: string | null; // "adapter serial …" / "adapter usb location …"
  bindto: string; // "0.0.0.0" for a LAN-visible server, "127.0.0.1" for one-shot work
  /** null = disabled (one-shot sessions only open TCL). */
  ports: { gdb: number | null; telnet: number | null; tcl: number };
}

/**
 * Arguments for `openocd`. Order matters: OpenOCD runs -f/-c strictly in sequence, so the
 * transport is chosen after the adapter driver exists but before the target script (which
 * builds its DAP for whatever transport is active), and the speed comes after the target
 * script because most targets set their own.
 */
export function openocdArgs(o: OcdOptions): string[] {
  const a = ["-f", o.iface, "-c", `transport select ${o.transport}`];
  if (o.target === GENERIC_TARGET) {
    // ARM's JTAG-DP has a 4-bit IR; SWD has no IR at all.
    a.push("-c", o.transport === "swd" ? "swd newdap chip cpu -enable" : "jtag newtap chip cpu -irlen 4 -enable");
    a.push("-c", "dap create chip.dap -chain-position chip.cpu", "-c", "target create chip.cpu cortex_m -dap chip.dap");
  } else if (o.target !== NO_TARGET) a.push("-f", o.target);
  a.push("-c", `adapter speed ${Math.round(o.speed)}`);
  if (o.pin) a.push("-c", o.pin);
  a.push("-c", `bindto ${o.bindto}`);
  // This build (0.12+dev) deprecates gdb_port/telnet_port/tcl_port for the spaced forms.
  a.push("-c", `gdb port ${o.ports.gdb ?? "disabled"}`, "-c", `telnet port ${o.ports.telnet ?? "disabled"}`, "-c", `tcl port ${o.ports.tcl}`);
  // Explicit init so a dead probe fails here (exit 1, before any port opens); the marker
  // then tells the driver init succeeded. The TCL listener opens right after it.
  a.push("-c", "init", "-c", `echo ${READY_MARK}`);
  return a;
}

/** Quote one argument for display in a copy-pasteable shell command. */
export function shellQuote(s: string): string {
  return /^[A-Za-z0-9_./:=@+-]+$/.test(s) ? s : `'${s.replace(/'/g, "'\\''")}'`;
}

// ---------- OpenOCD log parsing ----------

export interface OcdState {
  adapter: string; // "STLINK V2J37S7 (API v2)", "CMSIS-DAPv2 FW 2.0.0"
  voltage: number | null;
  dpidr: string;
  cpus: string[]; // "stm32f4x.cpu: Cortex-M4 r0p1"
  examined: boolean;
  flash: string;
  listening: Partial<Record<"gdb" | "telnet" | "tcl", number>>;
  gdbClients: number;
  errors: string[]; // newest last, a handful
  hint: string;
}

export function newOcdState(): OcdState {
  return { adapter: "", voltage: null, dpidr: "", cpus: [], examined: false, flash: "", listening: {}, gdbClients: 0, errors: [], hint: "" };
}

const HINTS: [RegExp, string][] = [
  [/LIBUSB_ERROR_ACCESS|ermission denied/, "No permission to open the probe's USB device: the openocd package's udev rules give it to group plugdev; replug the probe after installing them."],
  [/LIBUSB_ERROR_BUSY|resource busy/i, "The probe is busy: another OpenOCD, GDB server or IDE on DeskyPi has it open."],
  [/Invalid serial number/, "OpenOCD rejected the serial number used to pick this probe (J-Link serials must be numeric). Unplug other probes of this kind, or report it."],
  [
    /unable to find a matching CMSIS-DAP device|^Error: open failed|No J-Link device found|could not find or open device|unable to open ftdi device|LIBUSB_ERROR_NO_DEVICE|No device selected/,
    "OpenOCD couldn't open the probe: it was unplugged, something else holds it, or it re-enumerated with a different serial.",
  ],
  [/missing IR length/, "The target script needs SWD but JTAG was selected (or the other way round). Change the transport."],
  [/all ones|all zeroes|IR capture error|scan chain interrogation failed|Unexpected idcode/, "The JTAG chain doesn't answer: check TCK/TMS/TDI/TDO and GND wiring, that the target is powered, and try a lower adapter speed."],
  [/target voltage may be too low/, "The probe sees little or no voltage on VTref: power the target and connect its supply to the probe's VTref/VCC pin."],
  [/[Ll]ocked|read.?out protection|RDP|flash is protected|protected sector/, "The chip's flash is locked/read-protected. A mass erase (or the vendor's unlock command) clears it, wiping the firmware."],
  [/lockup|double fault|HardFault/, "The firmware crashed into a fault/lockup. Reset-and-halt, then reflash."],
  [/Address already in use|Couldn't bind|couldn't bind/i, "One of the ports is already taken (another OpenOCD?). Pick different ports in the settings."],
  [
    /init mode failed|unable to connect to the target|Error connecting DP|cannot read IDR|SWD ack not OK|DAP init failed|[Ee]xamination failed|Target not examined|Failed to read memory|timed out while waiting for target|DP initialisation failed/,
    "The probe works but the target doesn't answer. Check SWDIO/SWCLK and GND wiring, that the target is powered, try a lower adapter speed, or wire NRST and connect under reset if the firmware sleeps or reuses the SWD pins.",
  ],
];

export function hintFor(line: string): string {
  for (const [re, h] of HINTS) if (re.test(line)) return h;
  return "";
}

/**
 * Fold one OpenOCD stderr line into the running state. Handles both the 0.12 "[target]"
 * prefix and the older "target:" one. Returns true when something user-visible changed.
 */
export function parseOcdLine(st: OcdState, line: string): boolean {
  let m: RegExpExecArray | null;
  line = line.replace(/\s+$/, "");
  if ((m = /^Info : (?:\[([^\]]+)\]\s*|(\S+): )(.+?) processor detected/.exec(line))) {
    const s = `${m[1] || m[2]}: ${m[3]}`;
    if (!st.cpus.includes(s)) st.cpus.push(s);
    st.examined = true;
    return true;
  }
  if ((m = /^Info : \[([^\]]+)\] Examination succeed/.exec(line))) {
    if (!st.cpus.some((c) => c.startsWith(m![1] + ":"))) st.cpus.push(`${m[1]}: examined`);
    st.examined = true;
    return true;
  }
  if ((m = /Target voltage: ([\d.]+)/.exec(line)) || (m = /VTarget = ([\d.]+) ?V/.exec(line))) {
    st.voltage = Math.round(parseFloat(m[1]) * 100) / 100;
    if (st.voltage < 1.0) st.hint = hintFor("target voltage may be too low");
    return true;
  }
  if ((m = /^Info : (STLINK \S+ \(API v\d\))/.exec(line))) return (st.adapter = m[1]), true;
  if ((m = /Using CMSIS-DAPv(\d)/.exec(line))) return (st.adapter = `CMSIS-DAPv${m[1]}`), true;
  if ((m = /CMSIS-DAP: FW Version = (\S+)/.exec(line))) return (st.adapter = `${st.adapter || "CMSIS-DAP"} FW ${m[1]}`), true;
  if ((m = /^Info : (J-Link .+?) compiled/.exec(line))) return (st.adapter = m[1]), true;
  if ((m = /^Info : esp_usb_jtag: Device found\. Base speed (\d+)KHz/.exec(line))) return (st.adapter = "ESP USB-JTAG"), true;
  if ((m = /(?:SWD|JTAG) DPIDR (0x[0-9a-f]+)/i.exec(line))) return (st.dpidr = m[1]), true;
  if ((m = /^Info : JTAG tap: (\S+) tap\/device found: (0x[0-9a-f]+)/.exec(line))) return (st.dpidr ||= `${m[1]} ${m[2]}`), true;
  if ((m = /[Ff]lash size = (.+)$/.exec(line)) || (m = /Flash Probe: (\d+ bytes)/.exec(line))) return (st.flash = m[1].trim()), true;
  if ((m = /Listening on port (\d+) for (gdb|telnet|tcl) connections/.exec(line))) return (st.listening[m[2] as "gdb"] = +m[1]), true;
  if ((m = /starting gdb server for \S+ on (\d+)/.exec(line))) return (st.listening.gdb ??= +m[1]), true;
  if (/accepting 'gdb' connection/.test(line)) return st.gdbClients++, true;
  if (/dropped 'gdb' connection/.test(line)) return (st.gdbClients = Math.max(0, st.gdbClients - 1)), true;
  if (line.startsWith("Error: ") || /^Warn : .*examination failed/i.test(line)) {
    st.errors.push(line);
    if (st.errors.length > 6) st.errors.shift();
    const h = hintFor(line);
    if (h) st.hint = h;
    return true;
  }
  return false;
}

// ---------- TCL RPC ----------
//
// OpenOCD's TCL server takes a command terminated by 0x1a and answers with the Tcl result
// terminated by 0x1a. An error comes back as bare message text, indistinguishable from a
// result, so every command is wrapped in `catch` and tagged. `capture` collects what the
// command prints (most OpenOCD commands print rather than return).

export const TCL_EOM = "\x1a";
const OK = "@@OK@@";
const ERR = "@@ERR@@";

export function tclWrap(cmd: string, capture = true): string {
  const body = capture ? `capture {${cmd}}` : cmd;
  return `if {[catch {${body}} __r]} {set __r "${ERR}$__r"} else {set __r "${OK}$__r"}`;
}

export class TclError extends Error {}

export function tclUnwrap(reply: string): string {
  if (reply.startsWith(OK)) return reply.slice(OK.length);
  if (reply.startsWith(ERR)) throw new TclError(reply.slice(ERR.length).trim() || "OpenOCD command failed");
  // Unwrapped (unbalanced braces in the command make Tcl reject the wrapper itself).
  throw new TclError(reply.trim() || "OpenOCD command failed");
}

/** Braces must balance for the command to sit inside the catch wrapper. */
export function tclBalanced(cmd: string): boolean {
  let d = 0;
  for (let i = 0; i < cmd.length; i++) {
    if (cmd[i] === "\\") i++;
    else if (cmd[i] === "{") d++;
    else if (cmd[i] === "}" && --d < 0) return false;
  }
  return d === 0 && !cmd.includes(TCL_EOM);
}

/** Serialized client for one OpenOCD TCL port. */
export class TclClient {
  private sock: any = null;
  private buf = "";
  private waiting: { resolve: (s: string) => void; reject: (e: Error) => void; timer: any } | null = null;
  private chain: Promise<any> = Promise.resolve();

  constructor(private port: number, private host = "127.0.0.1") {}

  private async connect() {
    if (this.sock) return;
    const self = this;
    this.sock = await Bun.connect({
      hostname: this.host,
      port: this.port,
      socket: {
        data(_s, chunk) {
          self.buf += new TextDecoder().decode(chunk);
          const i = self.buf.indexOf(TCL_EOM);
          if (i < 0 || !self.waiting) return;
          const reply = self.buf.slice(0, i);
          self.buf = self.buf.slice(i + 1);
          const w = self.waiting;
          self.waiting = null;
          clearTimeout(w.timer);
          w.resolve(reply);
        },
        close() {
          self.sock = null;
          self.fail(new Error("OpenOCD closed the TCL connection"));
        },
        error(_s, e) {
          self.sock = null;
          self.fail(e);
        },
      },
    });
  }

  private fail(e: Error) {
    const w = this.waiting;
    this.waiting = null;
    if (w) clearTimeout(w.timer), w.reject(e);
  }

  /** Send a raw Tcl string; resolves with the raw reply. */
  raw(cmd: string, timeoutMs = 30000): Promise<string> {
    const run = async () => {
      await this.connect();
      return new Promise<string>((resolve, reject) => {
        const timer = setTimeout(() => {
          this.waiting = null;
          // The reply may still arrive later and would be misread as the next answer.
          this.close();
          reject(new Error(`OpenOCD didn't answer within ${Math.round(timeoutMs / 1000)} s`));
        }, timeoutMs);
        this.waiting = { resolve, reject, timer };
        this.sock.write(cmd + TCL_EOM);
      });
    };
    const p = this.chain.then(run, run);
    this.chain = p.catch(() => {});
    return p;
  }

  /** Run an OpenOCD command; resolves with its output, throws TclError on failure. */
  async cmd(cmd: string, opts: { capture?: boolean; timeoutMs?: number } = {}): Promise<string> {
    if (!tclBalanced(cmd)) throw new TclError("Unbalanced braces in command");
    return tclUnwrap(await this.raw(tclWrap(cmd, opts.capture ?? true), opts.timeoutMs));
  }

  close() {
    try {
      this.sock?.end();
    } catch {}
    this.sock = null;
  }
}

// ---------- Memory ----------

export function parseNum(s: string | number | undefined | null): number | null {
  if (s == null || s === "") return null;
  if (typeof s === "number") return Number.isFinite(s) ? s : null;
  const t = s.trim().replace(/_/g, "");
  const n = /^0x[0-9a-f]+$/i.test(t) ? parseInt(t, 16) : /^\d+$/.test(t) ? parseInt(t, 10) : NaN;
  return Number.isFinite(n) ? n : null;
}

/** `read_memory` returns a Tcl list of hex/decimal words. */
export function parseReadMemory(out: string): number[] {
  return out
    .trim()
    .split(/\s+/)
    .filter(Boolean)
    .map((w) => parseNum(w) ?? 0);
}

export function hexdump(bytes: ArrayLike<number>, base: number): string {
  const lines: string[] = [];
  for (let i = 0; i < bytes.length; i += 16) {
    const row = Array.from({ length: Math.min(16, bytes.length - i) }, (_, j) => bytes[i + j]);
    const hex = row.map((b) => b.toString(16).padStart(2, "0")).join(" ");
    const asc = row.map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : ".")).join("");
    lines.push(`${(base + i).toString(16).padStart(8, "0")}  ${hex.padEnd(47)}  ${asc}`);
  }
  return lines.join("\n");
}

// ---------- Firmware files ----------

export type FwKind = "elf" | "hex" | "srec" | "bin" | "uf2";

/** What OpenOCD will see: it sniffs ELF by magic and ihex/srec by first character. */
export function firmwareKind(buf: Uint8Array, name = ""): FwKind {
  if (buf[0] === 0x7f && buf[1] === 0x45 && buf[2] === 0x4c && buf[3] === 0x46) return "elf";
  if (buf.length >= 8 && new DataView(buf.buffer, buf.byteOffset).getUint32(0, true) === UF2_MAGIC0) return "uf2";
  if (buf[0] === 0x3a /* : */ || /\.hex$/i.test(name)) return "hex";
  if ((buf[0] === 0x53 /* S */ && buf[1] >= 0x30 && buf[1] <= 0x39) || /\.(srec|s19|mot)$/i.test(name)) return "srec";
  return "bin";
}

// ---------- UF2 ----------

export const UF2_MAGIC0 = 0x0a324655;
const UF2_MAGIC1 = 0x9e5d5157;
const UF2_MAGIC_END = 0x0ab16f30;
const UF2_FLAG_FAMILY = 0x2000;

export const UF2_FAMILIES: Record<number, string> = {
  0xe48bff56: "RP2040",
  0xe48bff57: "RP2XXX absolute",
  0xe48bff58: "RP2XXX data",
  0xe48bff59: "RP2350 ARM-S",
  0xe48bff5a: "RP2350 RISC-V",
  0xe48bff5b: "RP2350 ARM-NS",
  0x68ed2b88: "SAMD21",
  0x55114460: "SAMD51",
  0xada52840: "nRF52840",
  0x1c5f21b0: "ESP32",
  0xc47e5767: "ESP32-S3",
  0xd42ba06c: "ESP32-C3",
  0x57755a57: "STM32F4",
};

export interface Uf2Info {
  blocks: number;
  families: string[];
  familyIds: number[];
  start: number;
  end: number;
  payload: number;
}

/** Validate a UF2 file: every 512-byte block must carry both start magics and the end magic. */
export function parseUf2(buf: Uint8Array): Uf2Info {
  if (buf.length === 0 || buf.length % 512) throw new Error(`Not a UF2 file: ${buf.length} bytes isn't a whole number of 512-byte blocks`);
  const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const fams = new Set<number>();
  let start = Infinity,
    end = 0,
    payload = 0;
  for (let o = 0; o < buf.length; o += 512) {
    if (dv.getUint32(o, true) !== UF2_MAGIC0 || dv.getUint32(o + 4, true) !== UF2_MAGIC1 || dv.getUint32(o + 508, true) !== UF2_MAGIC_END)
      throw new Error(`Not a UF2 file: bad magic in block ${o / 512}`);
    const flags = dv.getUint32(o + 8, true);
    const addr = dv.getUint32(o + 12, true);
    const size = dv.getUint32(o + 16, true);
    if (flags & 1) continue; // "not main flash" blocks are comments/extra data
    if (flags & UF2_FLAG_FAMILY) fams.add(dv.getUint32(o + 28, true));
    start = Math.min(start, addr);
    end = Math.max(end, addr + size);
    payload += size;
  }
  const familyIds = [...fams];
  return {
    blocks: buf.length / 512,
    familyIds,
    families: familyIds.map((f) => UF2_FAMILIES[f] || "0x" + f.toString(16)),
    start: start === Infinity ? 0 : start,
    end,
    payload,
  };
}

/** Families a BOOTSEL device will accept (others are silently ignored by the bootrom). */
export function uf2Accepts(chip: string, familyIds: number[]): boolean {
  if (!familyIds.length) return true;
  const ok = chip === "rp2040" ? [0xe48bff56] : [0xe48bff57, 0xe48bff58, 0xe48bff59, 0xe48bff5a, 0xe48bff5b];
  return familyIds.some((f) => ok.includes(f));
}

/** INFO_UF2.TXT: a first line naming the bootloader, then "Key: value" lines. */
export function parseInfoUf2(text: string): { bootloader: string; fields: Record<string, string> } {
  const lines = text.split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
  const fields: Record<string, string> = {};
  let bootloader = "";
  for (const l of lines) {
    const m = /^([^:]+):\s*(.*)$/.exec(l);
    if (m) fields[m[1].trim()] = m[2].trim();
    else if (!bootloader) bootloader = l;
  }
  return { bootloader, fields };
}

// ---------- dfu-util ----------

/** dfu-util redraws a bar with \r: "Download\t[=====    ]  36%    45056 bytes". */
export function parseDfuProgress(chunk: string): { phase: string; pct: number } | null {
  let last: { phase: string; pct: number } | null = null;
  for (const m of chunk.matchAll(/(Erase|Download|Upload)\s*\[[= ]*\]\s*(\d+)%/g)) last = { phase: m[1], pct: +m[2] };
  return last;
}

// ---------- Client snippets ----------

export function clientSnippets(o: { host: string; ports: Ports; target: string; id: string; web: string }): Record<string, string> {
  const gdb = gdbFor(o.target);
  const ep = `${o.host}:${o.ports.gdb}`;
  return {
    gdb: `${gdb} -ex "target extended-remote ${ep}" build/app.elf`,
    telnet: `telnet ${o.host} ${o.ports.telnet}`,
    cortexDebug: JSON.stringify(
      { name: "DeskyPi probe", type: "cortex-debug", request: "launch", servertype: "external", gdbTarget: ep, executable: "${workspaceFolder}/build/app.elf", runToEntryPoint: "main" },
      null,
      2,
    ),
    platformio: [
      `debug_tool = custom`,
      `debug_port = ${ep}`,
      `debug_server =`,
      `upload_protocol = custom`,
      `upload_command = curl -fsS --data-binary @$SOURCE "${o.web}/api/probes/${o.id}/flash?name=$SOURCE"`,
    ].join("\n"),
  };
}
