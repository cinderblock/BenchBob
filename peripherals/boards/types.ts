// ==================== Board layer: types ====================
//
// A board file (peripherals/boards/<family>.ts) describes one family of machines: how to
// recognise it, its pin headers, how GPIO levels are read, where its boot-time bus
// configuration lives, and which health readings it has. index.ts picks one at startup.
//
// Header maps are data, not code, and every board file cites where its pinout came from
// (vendor documentation or the kernel device tree). A board nobody here has run BenchBob
// on is `tested: false`, and the API and UI say so.

export type PinKind = "3v3" | "5v" | "gnd" | "gpio" | "other" | "nc";

export interface HeaderPin {
  pin: number;
  kind: PinKind;
  /** Raspberry Pi: the BCM GPIO number (also its line on the SoC gpiochip). */
  gpio?: number;
  /** Other boards: a key of BoardDef.chips, and the line offset on that chip. */
  chip?: string;
  line?: number;
  /** SoC pad name as the vendor prints it: "GPIO4_B3", "PH5", "GPIOX_12". */
  soc?: string;
  /** Power and special pins: the vendor's label ("VDD_ADC", "AIN0", "SARADC_IN4"). */
  label?: string;
  /** Alternate functions the vendor lists for the pin ("I2C5_SDA_M3", "UART1_TX_M1"). */
  funcs?: string[];
  note?: string;
}

export interface Header {
  id: string; // "j8", "p8"…
  name: string; // "40-pin header"
  pins: HeaderPin[];
}

/** What a board is recognised from. Pure data so detection is unit-testable. */
export interface DetectInput {
  arch: string; // process.arch: "arm64", "x64"
  /** /proc/device-tree/compatible, most specific first. Empty on x86. */
  compatible: string[];
  /** /proc/device-tree/model. */
  model: string;
  /** /sys/class/dmi/id/*. Empty on ARM boards and in WSL. */
  dmi: { sysVendor: string; productName: string; productVersion: string; boardVendor: string; boardName: string };
  /** Running under WSL (no DMI, no device tree, no hardware of its own). */
  wsl: boolean;
}

/**
 * Where a board's boot-time bus switches live:
 *  - config.txt: Raspberry Pi firmware (dtparam/dtoverlay), edited by lib/gpio-config.ts;
 *  - armbianEnv / orangepiEnv: `overlays=` in /boot/armbianEnv.txt or /boot/orangepiEnv.txt
 *    (the same U-Boot env format; Orange Pi's own images renamed the file);
 *  - extlinux: `fdtoverlays` in extlinux.conf (BeagleBoard.org's K3 images).
 * The last three are edited by lib/boot-env.ts. Boards without one (x86, generic, ones
 * whose vendor tool writes a whole merged DTB or regenerates extlinux.conf) are read-only.
 */
export type BootConfigKind = "config.txt" | "armbianEnv" | "orangepiEnv" | "extlinux";

export interface OverlayChoice {
  /** The vendor's short name ("i2c5-m3"); matched against the installed .dtbo files. */
  name: string;
  /** The same overlay under another image's name (Armbian's "i2c3-ph" for Orange Pi's "ph-i2c3"). */
  aliases?: string[];
  what: string; // "I²C5 (SDA 3, SCL 5)"
  pins: number[]; // physical pins on the board's first header
}

export interface BootConfig {
  /**
   * The file depends on the OS as much as the board (an Orange Pi runs Armbian or Orange
   * Pi's own image), so these are tried in order and the first that exists is used.
   * config.txt is taken as-is: the Pi path reports a missing file itself.
   */
  files: { kind: BootConfigKind; path: string }[];
  /** Where the installed .dtbo files are, to offer only overlays this image has. */
  overlayDirs?: string[];
  /** extlinux: how a .dtbo in overlayDirs is written on the fdtoverlays line ("/overlays/"). */
  overlayRef?: string;
  /** Header overlays the vendor documents, with the pins each takes (for conflict checks). */
  overlays?: OverlayChoice[];
}

export interface BoardDef {
  id: string;
  /** Name shown when the device tree / DMI gives none. */
  name: string;
  vendor: string;
  soc?: string;
  /** Has BenchBob been run on this board by us? Untested boards are labelled in the UI. */
  tested: boolean;
  /** Match score for this input: 0 = not this board; the highest score wins. */
  match(i: DetectInput): number;
  /** Per-model refinement of `tested` (the Pi family: only the Pi 4 has been run here). */
  isTested?(i: DetectInput): boolean;
  /** Display name from the input, when the device-tree model isn't the right one (x86: DMI). */
  label?(i: DetectInput): string;
  /** SoC from the input, when one family spans several (BCM2711 vs BCM2712). */
  socOf?(i: DetectInput): string | undefined;
  headers: Header[];
  /** Chip keys used by header pins → identifiers that match a gpiochip's uAPI label or its sysfs device ("gpio4", "fec50000.gpio"). */
  chips?: Record<string, string[]>;
  /**
   * How GPIO state is read: "rpi" = pinctrl + /dev/gpiomem (the Raspberry Pi path);
   * "gpiochip" = the generic v2 uAPI (line info, levels for watched lines only).
   */
  gpio: "rpi" | "gpiochip";
  bootConfig: BootConfig | null;
  /** "vcgencmd": the Pi firmware's throttling flags and PMIC; "sysfs": thermal/hwmon/power_supply only. */
  health: "vcgencmd" | "sysfs";
  /** Pinout and config sources (URLs), shown in the API so users can check our data. */
  sources: string[];
  /** Shown with the board: caveats, the vendor's own overlay tool… */
  notes?: string[];
}

/** The board BenchBob is running on: a definition plus what the hardware called itself. */
export interface Board {
  def: BoardDef;
  id: string;
  /** Best human name: the device tree model, DMI vendor + product, or the definition's name. */
  name: string;
  /** The raw model string (device tree or DMI), "" if none. */
  model: string;
  tested: boolean;
  /** Forced with BENCHBOB_BOARD rather than detected. */
  forced: boolean;
  /** What the board was recognised from. */
  input: DetectInput;
}

// ---------- helpers for board files ----------

/** True if any compatible string matches. */
export const hasCompat = (i: DetectInput, ...re: RegExp[]) => i.compatible.some((c) => re.some((r) => r.test(c)));

type PinSpec = [number, "3v3" | "5v" | "gnd" | "nc"] | [number, "other", string] | [number, string, string, number, string[]?, string?];

/**
 * Compact header tables: [pin, "gnd"] / [pin, "other", "AIN0"] / [pin, socName, chipKey, line, funcs?, note?].
 * Board files keep the vendor's table shape this way, so a reviewer can hold it against the PDF.
 */
export function pins(rows: PinSpec[]): HeaderPin[] {
  return rows.map((r) => {
    if (r.length === 2) return { pin: r[0], kind: r[1] };
    if (r[1] === "other") return { pin: r[0], kind: "other", label: r[2] as string };
    const [pin, soc, chip, line, funcs, note] = r as [number, string, string, number, string[]?, string?];
    return { pin, kind: "gpio", soc, chip, line, ...(funcs?.length ? { funcs } : {}), ...(note ? { note } : {}) };
  });
}
