// boot.conf editing. The text is the source of truth. The form only reads and rewrites
// keys in the unconditional part of the file ([all], or lines before any section), so
// comments, unknown keys and conditional sections like [gpio4=0] survive untouched. A
// [config.txt] section runs to the end of the file, so nothing after it is touched either.
// Property reference: https://www.raspberrypi.com/documentation/computers/raspberry-pi.html#raspberry-pi-bootloader-configuration

export const BL_MODES: Record<string, string> = {
  "1": "eMMC / SD", "2": "Network", "3": "RPIBOOT", "4": "USB storage", "5": "USB 2.0 (BCM)",
  "6": "NVMe", "7": "HTTP", e: "Stop", f: "Restart", "0": "SD detect",
};

// Keyed by board(): chip, plus "cm" for Compute Modules. The flagship ones match the boot
// orders in Imager's three Bootloader images (rpi-eeprom release zips, 2026-09).
export const BL_ORDER_PRESETS: Record<string, [string, string][]> = {
  "2711cm": [["0xf25641", "eMMC, USB, NVMe, USB 2.0, network (rpiboot's CM4 default)"], ["0xf41", "eMMC, USB (firmware default)"], ["0xf461", "eMMC, NVMe, USB"], ["0xf416", "NVMe, eMMC, USB"], ["0xf14", "USB, eMMC"], ["0xf21", "eMMC, network"]],
  "2712cm": [["0xf2461", "eMMC, NVMe, USB, network (rpiboot's CM5 default)"], ["0xf461", "eMMC, NVMe, USB (image default)"], ["0xf416", "NVMe, eMMC, USB"], ["0xf146", "NVMe, USB, eMMC"], ["0xf14", "USB, eMMC"], ["0xf21", "eMMC, network"]],
  "2711": [["0xf41", 'SD card, USB (Imager "SD Card Boot")'], ["0xf14", 'USB, SD card (Imager "USB Boot")'], ["0xf21", 'SD card, network (Imager "Network Boot")']],
  "2712": [["0xf461", 'SD card, NVMe, USB (Imager "SD Card Boot")'], ["0xf146", 'NVMe, USB, SD card (Imager "NVMe/USB Boot")'], ["0xf21", 'SD card, network (Imager "Network Boot")'], ["0xf416", "NVMe, SD card, USB"]],
};

export interface BlSetting {
  g: string;
  key: string;
  t: "int" | "bool" | "select" | "text";
  /** What the bootloader does when the key is left out, by board() (falling back to chip). */
  def: string | Record<string, string>;
  /** Only shown for these chips / this kind of board, per the property docs. */
  chips?: string[];
  board?: "cm" | "flagship";
  options?: [string, string][];
  label: string;
  hint?: string;
  ph?: string;
  min?: number;
  max?: number;
}

export const BL_SETTINGS: BlSetting[] = [
  { g: "Boot", key: "SD_BOOT_MAX_RETRIES", t: "int", def: "0", label: "SD / eMMC retries", hint: "Before moving on to the next boot mode. -1 = forever." },
  { g: "Boot", key: "NET_BOOT_MAX_RETRIES", t: "int", def: "0", label: "Network boot retries", hint: "-1 = forever." },
  { g: "Boot", key: "BOOT_WATCHDOG_TIMEOUT", t: "int", def: "0", label: "Boot watchdog (s)", hint: "Reset if no OS has started by then. 0 = off." },
  { g: "Boot", key: "PARTITION", t: "int", def: "0", label: "Boot partition", hint: "0 = the default partition." },
  { g: "Boot", key: "PARTITION_WALK", t: "bool", def: "1", label: "Partition walk", hint: "Try other partitions if the chosen one isn't bootable." },
  { g: "Boot", key: "REBOOT_ON_FATAL_ERROR", t: "bool", def: "1", label: "Reboot on fatal error", hint: "Off = blink the error pattern until power-cycled." },
  { g: "Power", key: "POWER_OFF_ON_HALT", t: "bool", def: { "2711": "0", "2712": "0", "2712cm": "1" }, label: "Power off on halt", hint: "Switch off the PMIC outputs on halt. Pi 4 / CM4: needs Wake on GPIO off. Pi 500 defaults to on." },
  { g: "Power", key: "WAKE_ON_GPIO", t: "bool", def: "1", chips: ["2711"], label: "Wake on GPIO3", hint: "Halt in low power until GPIO3 or GLOBAL_EN is grounded." },
  { g: "Power", key: "WAIT_FOR_POWER_BUTTON", t: "bool", def: "0", chips: ["2712"], board: "flagship", label: "Wait for power button", hint: "After power loss, stay off until the button is pressed. Needs Power off on halt." },
  { g: "Power", key: "PSU_MAX_CURRENT", t: "select", def: "", chips: ["2712"], board: "flagship", options: [["3000", "3 A"], ["5000", "5 A"]], label: "Power supply current", hint: "Skip USB-PD negotiation and assume this rating." },
  { g: "Diagnostics", key: "BOOT_UART", t: "bool", def: "0", label: "UART boot log", hint: "Bootloader debug output on the primary UART." },
  { g: "Diagnostics", key: "UART_BAUD", t: "select", def: "115200", chips: ["2712"], board: "flagship", options: ["9600", "19200", "38400", "57600", "115200", "230400", "460800", "921600"].map((b) => [b, b] as [string, string]), label: "UART baud rate" },
  { g: "Diagnostics", key: "DISABLE_HDMI", t: "bool", def: "0", label: "Disable HDMI diagnostics", hint: "Also disables network install." },
  { g: "Diagnostics", key: "HDMI_DELAY", t: "int", def: "5", label: "HDMI diagnostics delay (s)", hint: "Shown sooner on a fatal error." },
  { g: "Diagnostics", key: "NET_INSTALL_ENABLED", t: "bool", def: { "2711": "1", "2712": "1", "2711cm": "0", "2712cm": "0" }, label: "Network install", hint: "Offer it when a keyboard is attached. Adds ~1 s to boot." },
  { g: "Diagnostics", key: "NET_INSTALL_AT_POWER_ON", t: "bool", def: "0", label: "Network install at power-on", hint: "Briefly show the network install screen after a cold boot." },
  { g: "Updates", key: "ENABLE_SELF_UPDATE", t: "bool", def: "1", label: "Self-update", hint: "Apply a pieeprom.upd found on the boot filesystem (USB / network boot)." },
  { g: "Updates", key: "FREEZE_VERSION", t: "bool", def: "0", label: "Freeze version", hint: "Block automatic updates, including rpi-eeprom-update. Rewriting over USB still works." },
  { g: "USB & PCIe", key: "PCIE_PROBE", t: "bool", def: "0", chips: ["2712"], board: "flagship", label: "Probe PCIe for NVMe", hint: "For PCIe add-ons that aren't HAT+ (those are detected automatically)." },
  { g: "USB & PCIe", key: "USB_MSD_DISCOVER_TIMEOUT", t: "int", def: "20000", min: 5000, label: "USB discover timeout (ms)", hint: "Give up on USB boot after this. Minimum 5000." },
  { g: "USB & PCIe", key: "USB_MSD_STARTUP_DELAY", t: "int", def: "0", min: 0, max: 30000, label: "USB startup delay (ms)", hint: "For slow-to-spin-up disks. Maximum 30000." },
  { g: "USB & PCIe", key: "VL805", t: "bool", def: "0", chips: ["2711"], board: "cm", label: "VL805 USB 3 firmware", hint: "Only for a VL805 controller with no SPI EEPROM of its own." },
  { g: "Network boot", key: "TFTP_IP", t: "text", def: "", ph: "192.168.1.99", label: "TFTP server", hint: "Overrides the DHCP server's choice." },
  { g: "Network boot", key: "TFTP_PREFIX", t: "select", def: "0", options: [["0", "Serial number"], ["1", "Custom string"], ["2", "MAC address"]], label: "TFTP directory" },
  { g: "Network boot", key: "TFTP_PREFIX_STR", t: "text", def: "", ph: "tftp_test/", label: "Custom TFTP directory", hint: "Used when TFTP directory is Custom string." },
  { g: "Network boot", key: "CLIENT_IP", t: "text", def: "", ph: "192.168.0.32", label: "Static IP", hint: "With TFTP server set, skips DHCP." },
  { g: "Network boot", key: "SUBNET", t: "text", def: "", ph: "255.255.255.0", label: "Subnet mask" },
  { g: "Network boot", key: "GATEWAY", t: "text", def: "", ph: "192.168.0.1", label: "Gateway" },
  { g: "Network boot", key: "DHCP_TIMEOUT", t: "int", def: "45000", min: 5000, label: "DHCP timeout (ms)" },
  { g: "Network boot", key: "HTTP_HOST", t: "text", def: "", ph: "fw-download-alias1.raspberrypi.com", label: "HTTP boot host", hint: "A custom host means plain HTTP." },
  { g: "Network boot", key: "HTTP_PATH", t: "text", def: "", ph: "net_install", label: "HTTP boot path" },
];
export const BL_GROUPS = ["Boot", "Power", "Diagnostics", "Updates", "USB & PCIe", "Network boot"];

/** The board an editor is for: chip plus whether it's a Compute Module. */
export interface BlBoard {
  chip: string;
  cm: boolean;
}
export const boardKey = (b: BlBoard) => b.chip + (b.cm ? "cm" : "");
export const settingDefault = (s: BlSetting, b: BlBoard) => (typeof s.def === "object" ? s.def[boardKey(b)] ?? s.def[b.chip] : s.def);
export const modeLabel = (m: string, b: BlBoard) => (m === "1" && !b.cm ? "SD card" : BL_MODES[m] || "0x" + m);
export const settingsFor = (b: BlBoard) => BL_SETTINGS.filter((s) => (!s.chips || s.chips.includes(b.chip)) && (!s.board || s.board === (b.cm ? "cm" : "flagship")));

/** Line indexes in the unconditional part of the file. */
export function blParse(text: string) {
  const lines = text.replace(/\r\n/g, "\n").split("\n");
  const uncond: number[] = [];
  let inAll = true;
  for (let i = 0; i < lines.length; i++) {
    const h = lines[i].match(/^\s*\[([^\]]*)\]/);
    if (h) {
      const f = h[1].trim().toLowerCase();
      if (f === "config.txt") break;
      inAll = f === "all";
    } else if (inAll) uncond.push(i);
  }
  return { lines, uncond };
}

function blKV(line: string): [string, string] | null {
  const m = line.match(/^\s*([A-Za-z0-9_]+)\s*=(.*)$/);
  return m ? [m[1].toUpperCase(), m[2].trim()] : null;
}

export function blValues(text: string): Record<string, string> {
  const { lines, uncond } = blParse(text);
  const v: Record<string, string> = {};
  for (const i of uncond) {
    const kv = blKV(lines[i]);
    if (kv) v[kv[0]] = kv[1];
  }
  return v;
}

/**
 * Set (or with val null, remove) a key: rewrites the last unconditional occurrence and
 * drops earlier ones; a new key goes after the first unconditional block.
 */
export function blSet(text: string, key: string, val: string | null): string {
  const { lines, uncond } = blParse(text);
  const hits = uncond.filter((i) => blKV(lines[i])?.[0] === key);
  const drop = val == null ? hits : hits.slice(0, -1);
  if (val != null) {
    if (hits.length) lines[hits[hits.length - 1]] = `${key}=${val}`;
    else {
      let at = 0;
      if (uncond.length) {
        at = uncond[0];
        for (let k = 0; k < uncond.length && uncond[k] === uncond[0] + k; k++) if (lines[uncond[k]].trim()) at = uncond[k] + 1;
      }
      lines.splice(at, 0, `${key}=${val}`);
    }
  }
  for (const i of [...drop].sort((a, b) => b - a)) lines.splice(i, 1);
  return lines.join("\n");
}

/** The conditional sections, for noticing edits the key diff can't show. */
export function blRest(text: string): string {
  const { lines, uncond } = blParse(text);
  const u = new Set(uncond);
  return lines.filter((l, i) => !u.has(i) && l.trim() && !/^\s*\[all\]\s*$/i.test(l)).join("\n");
}

/** BOOT_ORDER is read right to left: one hex nibble per mode, lowest first. */
export function orderItems(v: string | undefined): string[] | null {
  if (v == null || v === "") return [];
  const hex = v.trim().toLowerCase().replace(/^0x/, "").replace(/^0+(?=.)/, "");
  return /^[0-9a-f]{1,8}$/.test(hex) ? [...hex].reverse() : null;
}
export const orderHex = (items: string[]) => (items.length ? "0x" + [...items].reverse().join("") : null);

/** Warnings about a boot order for this board. */
export function orderWarnings(items: string[], b: BlBoard): string[] {
  const nvme = b.chip === "2712" || b.cm; // CM4 has PCIe; the Pi 4 doesn't
  const warn: string[] = [];
  const stopAt = items.findIndex((m) => m === "3" || m === "e" || m === "f");
  if (stopAt !== -1 && stopAt < items.length - 1) warn.push(`Nothing after ${modeLabel(items[stopAt], b)} is ever tried.`);
  if (items.length && !["f", "e", "3"].includes(items[items.length - 1])) warn.push(`No Restart at the end: the ${b.cm ? "module" : "Pi"} halts with an error if every mode fails.`);
  if (b.chip === "2712" && items.includes("5")) warn.push("USB 2.0 (BCM) boot is not available on BCM2712.");
  if (!nvme && items.includes("6")) warn.push("The Pi 4 has no NVMe boot.");
  if (items.includes("0")) warn.push("SD detect (0) is deprecated. Use Restart instead.");
  return warn;
}

/** Modes offered in "+ Add boot mode…" for this board. */
export function addableModes(b: BlBoard): string[] {
  const nvme = b.chip === "2712" || b.cm;
  return ["1", ...(nvme ? ["6"] : []), "4", ...(b.chip === "2711" ? ["5"] : []), "2", "7", "3", "f", "e"];
}

/** Insert a mode: new ones go before a trailing Restart/Stop, which only makes sense last. */
export function orderAdd(items: string[], m: string): string[] {
  const out = [...items];
  const end = out.length && ["f", "e"].includes(out[out.length - 1]) && !["f", "e"].includes(m) ? out.length - 1 : out.length;
  out.splice(end, 0, m);
  return out.slice(0, 8);
}

/** Is an int setting's text acceptable? */
export function intOk(s: BlSetting, v: string): boolean {
  if (v === "") return true;
  const n = Number(v);
  return /^-?\d+$/.test(v) && (s.min == null || n >= s.min) && (s.max == null || n <= s.max);
}
