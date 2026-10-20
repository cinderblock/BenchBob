// The OS image catalog (GET /api/catalog: Raspberry Pi Imager's lists, merged), and the
// rules the flash picker and Quick Pi apply to it.

export interface CatalogImage {
  key: string;
  name: string;
  description: string;
  url: string;
  devices: string[];
  path: string[];
  downloadSize: number;
  extractSize: number;
  releaseDate?: string;
  initFormat?: string;
  rpios?: { edition: string; arch: string; legacy: boolean; release: string };
}

export interface CatalogBoard {
  name: string;
  tags: string[];
  description?: string;
  exclusive?: boolean;
}

export interface FlashSettings {
  hostname: string;
  username: string;
  password: string;
  enableSsh: boolean;
  passwordlessSudo: boolean;
  installBenchBob: boolean;
  sshKeys: string[];
  wifiNetworks: { ssid: string; password: string; country: string; priority?: number }[];
  timezone: string;
  keyboardLayout: string;
}

export type Defaults = Partial<FlashSettings>;
export type ImageList = { key: string; label: string }[];

export const POPULAR_DISTROS = ["Ubuntu", "DietPi", "Armbian OS", "Alpine Linux", "Home Assistant", "LibreELEC", "Kali Linux", "OctoPi"];
export const RPIOS_EDITIONS: [string, string, string][] = [
  ["lite", "Lite", "No desktop"],
  ["desktop", "Desktop", "Desktop + basics"],
  ["full", "Full", "Desktop + all apps"],
];
export const HOSTNAME_RE = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/i;

export function imgLabel(im: CatalogImage): string {
  if (!im.rpios) return im.name;
  const ed = RPIOS_EDITIONS.find((e) => e[0] === im.rpios!.edition)?.[1] || im.rpios.edition;
  return `Pi OS ${ed} ${im.rpios.arch}-bit${im.rpios.legacy ? " (legacy)" : ""}`;
}

export const boardShort = (name: string) => name.replace(/^Raspberry Pi /, "").replace(/^(\d)/, "Pi $1");
export const imgBoards = (im: CatalogImage, boards: CatalogBoard[]) => boards.filter((b) => b.tags.some((t) => im.devices.includes(t))).map((b) => boardShort(b.name));
export const fitsBoard = (im: CatalogImage, b: CatalogBoard | null) => !b || (im.devices.length ? b.tags.some((t) => im.devices.includes(t)) : !b.exclusive);
export const takesSettings = (im: CatalogImage) => ["systemd", "cloudinit", "cloudinit-rpi"].includes(im.initFormat || "");

// A distro is usually the second folder (Ubuntu, Apertis). Themed folders like "Home
// automation" hold one folder per project instead, so go a level deeper there. A sub-folder
// that starts with its parent's name ("Apertis (daily build)") is still the same distro, and
// a lone sub-folder (Bass OS › Bass ARM) doesn't make a theme.
const themeCache = new WeakMap<CatalogImage[], Set<string>>();
function themeFolders(images: CatalogImage[]): Set<string> {
  let t = themeCache.get(images);
  if (!t) {
    const kids = new Map<string, Set<string>>();
    for (const { path: p } of images) {
      if (p.length < 3 || p[2].startsWith(p[1])) continue;
      const k = p.slice(0, 2).join(" › ");
      if (!kids.has(k)) kids.set(k, new Set());
      kids.get(k)!.add(p[2]);
    }
    t = new Set([...kids].filter(([, s]) => s.size > 1).map(([k]) => k));
    themeCache.set(images, t);
  }
  return t;
}
export const distroDepth = (im: CatalogImage, images: CatalogImage[]) => (im.path.length > 2 && themeFolders(images).has(im.path.slice(0, 2).join(" › ")) ? 3 : 2);
export const distroOf = (im: CatalogImage, images: CatalogImage[]) => im.path.slice(0, distroDepth(im, images)).join(" › ");
export const distroName = (id: string) => id.split(" › ").pop() || "Other";
export function distroGroup(id: string): string {
  const p = id.split(" › ").slice(0, -1);
  return p.length > 1 ? `${p[p.length - 1]}${p[0].startsWith("Freemium") ? " (paid)" : ""}` : p[0];
}

/**
 * Saved defaults as flash settings, for Quick Pi. Networks saved without a password (by an
 * older dashboard, which stripped them) are left out rather than written as open networks.
 */
export function quickSettings(d: Defaults, hostname: string): FlashSettings {
  return {
    hostname,
    username: d.username || "pi",
    password: "",
    enableSsh: d.enableSsh !== false,
    passwordlessSudo: !!d.passwordlessSudo,
    installBenchBob: !!d.installBenchBob,
    sshKeys: Array.isArray(d.sshKeys) ? d.sshKeys.filter(Boolean) : [],
    wifiNetworks: (Array.isArray(d.wifiNetworks) ? d.wifiNetworks : []).filter((w) => w.ssid && w.password),
    timezone: d.timezone || "UTC",
    keyboardLayout: d.keyboardLayout || "us",
  };
}

/** Why Quick Pi can't run with this image and the saved defaults, or "" if it can. */
export function quickBlocker(s: FlashSettings, im: CatalogImage | null, listLen: number): string {
  if (!im) return listLen ? "None of the Quick Pi images are in the catalog right now." : "The Quick Pi list is empty.";
  if (!takesSettings(im)) return `${im.name} ignores first-boot settings, so Quick Pi can't set its hostname or SSH keys.`;
  if (!s.enableSsh) return "SSH is off in the saved defaults, so there would be no way in.";
  if (!s.sshKeys.length) return "No SSH keys are saved in the defaults, and Quick Pi sets no password. Save a key with “Save as Default” first.";
  return "";
}

/** How the chosen image takes first-boot settings (the catalog's init_format). */
export function initFormatLabel(f?: string): string {
  return ({ systemd: "firstrun.sh", cloudinit: "cloud-init", "cloudinit-rpi": "cloud-init", "rpi-preseed": "not supported (rpi-preseed)" } as Record<string, string>)[f || ""] || "not supported by this image";
}

export const flashRequest = (im: CatalogImage, dev: string, settings: FlashSettings) => ({ imageUrl: im.url, imageName: im.name, imageKey: im.key, extractSize: im.extractSize, device: dev, settings });
