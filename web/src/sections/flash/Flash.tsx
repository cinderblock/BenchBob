// Flashing: the image picker (Raspberry Pi OS grid, other OSes by distro, favourites, the
// Quick Pi bar) with first-boot settings, and the Quick Pi dialog, which flashes an image
// from the Quick Pi list with the saved defaults and asks only for a hostname.

import { useEffect, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import type { BobApi } from "../../bob/context";
import { useBob, useSnapshot } from "../../bob/context";
import { cap, fmtSz } from "../../lib/format";
import { readLocalStr, writeLocal } from "../../lib/storage";
import type { BobStore } from "../../lib/store";
import {
  HOSTNAME_RE,
  POPULAR_DISTROS,
  RPIOS_EDITIONS,
  boardShort,
  distroDepth,
  distroGroup,
  distroName,
  distroOf,
  fitsBoard,
  flashRequest,
  imgBoards,
  imgLabel,
  initFormatLabel,
  quickBlocker,
  quickSettings,
  takesSettings,
  type CatalogBoard,
  type CatalogImage,
  type Defaults,
  type FlashSettings,
  type ImageList,
} from "./catalog";

type ListName = "favorites" | "quick";
const LIST_API: Record<ListName, string> = { favorites: "/api/favorites", quick: "/api/quicklist" };

/** Per-bob flash data: the catalog, the saved defaults and the two image lists. */
class FlashData {
  images: CatalogImage[] = [];
  boards: CatalogBoard[] = [];
  defaults: Defaults = {};
  lists: Record<ListName, ImageList> = { favorites: [], quick: [] };
  private loaded = false;
  private subs = new Set<() => void>();
  private ver = 0;
  constructor(private b: BobApi) {}
  subscribe = (f: () => void) => {
    this.subs.add(f);
    return () => this.subs.delete(f);
  };
  version = () => this.ver;
  changed() {
    this.ver++;
    for (const f of this.subs) f();
  }
  async ensure() {
    const c = this.b.conn;
    await Promise.all([
      !this.images.length &&
        c
          .json<{ images?: CatalogImage[]; devices?: CatalogBoard[] }>("GET", "/api/catalog")
          .then((r) => {
            this.images = r.images || [];
            this.boards = (r.devices || []).filter((d) => d.tags.length);
          })
          .catch(() => {}),
      !Object.keys(this.defaults).length && this.loadDefaults(),
      !this.loaded &&
        Promise.all(
          (Object.keys(LIST_API) as ListName[]).map((n) =>
            c
              .json<ImageList>("GET", LIST_API[n])
              .then((l) => (this.lists[n] = Array.isArray(l) ? l : []))
              .catch(() => {}),
          ),
        ).then(() => (this.loaded = true)),
    ]);
    this.changed();
  }
  async loadDefaults() {
    try {
      this.defaults = await this.b.conn.json<Defaults>("GET", "/api/defaults");
    } catch {
      this.defaults = {};
    }
  }
  byKey = (key: string | null | undefined) => this.images.find((i) => i.key === key);
  inList = (n: ListName, key: string) => this.lists[n].some((f) => f.key === key);
  async toggle(n: ListName, key: string) {
    const im = this.byKey(key);
    this.lists[n] = this.inList(n, key) ? this.lists[n].filter((f) => f.key !== key) : [...this.lists[n], { key, label: im ? imgLabel(im) : key }];
    this.changed();
    try {
      await this.b.conn.json("POST", LIST_API[n], this.lists[n]);
    } catch {
      this.b.toast("Saving the list failed", "error");
    }
  }
  /** The Quick Pi list's images that are in the catalog, and the one to flash: the last picked, else the first. */
  quickImages = () => this.lists.quick.map((f) => this.byKey(f.key)).filter(Boolean) as CatalogImage[];
  quickImage(): CatalogImage | null {
    const k = readLocalStr(this.b.key("quickLast"));
    const ims = this.quickImages();
    return ims.find((i) => i.key === k) || ims[0] || null;
  }
  pickQuick(key: string) {
    writeLocal(this.b.key("quickLast"), key);
    this.changed();
  }
}

const data = new WeakMap<BobStore, FlashData>();
function flashData(b: BobApi) {
  let d = data.get(b.store);
  if (!d) data.set(b.store, (d = new FlashData(b)));
  return d;
}
function useFlashData() {
  const b = useBob();
  const d = flashData(b);
  useSyncExternalStore(d.subscribe, d.version);
  return d;
}

async function startFlash(b: BobApi, req: ReturnType<typeof flashRequest>) {
  writeLocal(b.key("flashLast"), req.imageKey);
  try {
    await b.conn.json("POST", "/api/flash", req);
  } catch (e: any) {
    b.toast(e?.message || "Flash failed", "error");
  }
}

export async function flashDrive(b: BobApi, dev: string, hostname?: string) {
  await flashData(b).ensure();
  b.openModal(<FlashPicker dev={dev} hostname={hostname} />, { cls: "wide flash" });
}

export async function quickPiDrive(b: BobApi, dev: string) {
  await flashData(b).ensure();
  b.openModal(<QuickPi dev={dev} />);
}

function DevLine({ dev }: { dev: string }) {
  const d = useSnapshot().status?.drives.find((x) => x.name === dev);
  return (
    <div className="modal-dev">
      /dev/{dev}
      {d ? ` · ${d.model || ""} · ${fmtSz(d.size)}` : ""}
    </div>
  );
}

/** Hostname box: flags invalid names, Enter presses the button. */
function HostInput({ id, value, set, onEnter, autoFocus }: { id: string; value: string; set(v: string): void; onEnter(): void; autoFocus?: boolean }) {
  return (
    <input
      type="text"
      id={id}
      value={value}
      placeholder="e.g. garage-pi"
      autoComplete="off"
      spellCheck={false}
      autoFocus={autoFocus}
      className={value && !HOSTNAME_RE.test(value.trim()) ? "bad" : ""}
      onInput={(e) => set(e.currentTarget.value)}
      onKeyDown={(e) => e.key === "Enter" && onEnter()}
    />
  );
}

/** Chips for choosing the Quick Pi image; entries gone from the catalog can be removed. */
function QuickChips() {
  const fd = useFlashData();
  const cur = fd.quickImage();
  if (!fd.lists.quick.length) return <span className="dim">The list is empty. Add images with ⚡ in the flash dialog.</span>;
  return (
    <>
      {fd.lists.quick.map((f) => {
        const im = fd.byKey(f.key);
        if (!im)
          return (
            <span key={f.key} className="fp-chip gone">
              {f.label} (not in the catalog now)
              <span className="x" role="button" aria-label="Remove" onClick={() => fd.toggle("quick", f.key)}>
                ×
              </span>
            </span>
          );
        return (
          <span key={f.key} className={`fp-chip qk${cur === im ? " sel" : ""}`} role="button" onClick={() => fd.pickQuick(im.key)}>
            {imgLabel(im)}
          </span>
        );
      })}
    </>
  );
}

function QuickPi({ dev }: { dev: string }) {
  const b = useBob();
  const fd = useFlashData();
  const [host, setHost] = useState("");
  const s = quickSettings(fd.defaults, "");
  const im = fd.quickImage();
  const blocker = quickBlocker(s, im, fd.lists.quick.length);
  const ok = !blocker && HOSTNAME_RE.test(host.trim());
  const skipped = (fd.defaults.wifiNetworks || []).filter((w) => w.ssid && !w.password).map((w) => w.ssid);
  const go = () => {
    if (!ok || !im) return;
    b.closeModal();
    startFlash(b, flashRequest(im, dev, quickSettings(fd.defaults, host.trim())));
  };
  return (
    <>
      <h3>⚡ Quick Pi</h3>
      <DevLine dev={dev} />
      <div className="fg">
        <label htmlFor="qp-host">Hostname</label>
        <HostInput id="qp-host" value={host} set={setHost} onEnter={go} autoFocus />
      </div>
      <div className="qp-imgs" id="qp-imgs">
        <QuickChips />
      </div>
      <div className="qp-sum">
        <span>Image</span>
        <span>{im ? `${im.name} · ${fmtSz(im.downloadSize)} download · released ${im.releaseDate || "?"}` : "—"}</span>
        <span>Login</span>
        <span>
          {s.username} · {s.sshKeys.length} SSH key{s.sshKeys.length === 1 ? "" : "s"} · no password{s.passwordlessSudo ? " · passwordless sudo" : ""}
          {s.installBenchBob ? " · installs BenchBob" : ""}
        </span>
        <span>Wi-Fi</span>
        <span>
          {s.wifiNetworks.length ? s.wifiNetworks.map((w) => w.ssid).join(", ") : "none (wired)"}
          {skipped.length > 0 && <span className="dim"> · skipped {skipped.join(", ")}: no saved password, re-save defaults with it</span>}
        </span>
        <span>Locale</span>
        <span>
          {s.timezone} · {s.keyboardLayout} keyboard
        </span>
      </div>
      {blocker && <div className="fp-warn">{blocker}</div>}
      <div className="modal-btns">
        <button
          className="btn"
          style={{ marginRight: "auto" }}
          onClick={() => {
            b.closeModal();
            flashDrive(b, dev, host.trim());
          }}
        >
          More options…
        </button>
        <button className="btn" onClick={b.closeModal}>
          Cancel
        </button>
        <button className="btn btn-primary" disabled={!ok} onClick={go}>
          Flash
        </button>
      </div>
    </>
  );
}

interface WifiRow {
  id: number;
  ssid: string;
  password: string;
  country: string;
}

function FlashPicker({ dev, hostname }: { dev: string; hostname?: string }) {
  const b = useBob();
  const fd = useFlashData();
  const drive = useSnapshot().status?.drives.find((x) => x.name === dev);
  const d = fd.defaults;
  const [init] = useState(() => {
    const lastIm = fd.byKey(readLocalStr(b.key("flashLast")));
    return { sel: lastIm?.key || null, tab: lastIm && !lastIm.rpios ? "other" : "rpios", distro: lastIm && !lastIm.rpios ? distroOf(lastIm, fd.images) : null };
  });
  const [sel, setSel] = useState<string | null>(init.sel);
  const [tab, setTab] = useState(init.tab);
  const [distro, setDistro] = useState<string | null>(init.distro);
  const [q, setQ] = useState("");
  const [boardName, setBoardName] = useState(() => readLocalStr(b.key("flashBoard")) || "");
  const board = fd.boards.find((x) => x.name === boardName) || null;
  const setBoard = (n: string) => {
    writeLocal(b.key("flashBoard"), n);
    setBoardName(n);
  };

  // Settings form.
  const [host, setHost] = useState(hostname || d.hostname || "raspberrypi");
  const [user, setUser] = useState(d.username || "pi");
  const [pass, setPass] = useState("");
  const [ssh, setSsh] = useState(d.enableSsh !== false);
  const [sudo, setSudo] = useState(!!d.passwordlessSudo);
  const [bb, setBb] = useState(!!d.installBenchBob);
  const [keys, setKeys] = useState(Array.isArray(d.sshKeys) ? d.sshKeys.join("\n") : "");
  const wid = useRef(0);
  const [wifi, setWifi] = useState<WifiRow[]>(() => {
    const nets = Array.isArray(d.wifiNetworks) ? d.wifiNetworks : [];
    return (nets.length ? nets : [{ ssid: "", password: "", country: "US" }]).map((w) => ({ id: ++wid.current, ssid: w.ssid || "", password: w.password || "", country: w.country || "US" }));
  });
  const [tz, setTz] = useState(d.timezone || "UTC");
  const [kb, setKb] = useState(d.keyboardLayout || "us");
  const [qpHost, setQpHost] = useState("");

  const readSettings = (): FlashSettings => ({
    hostname: host || "raspberrypi",
    username: user || "pi",
    password: pass,
    enableSsh: ssh,
    passwordlessSudo: sudo,
    installBenchBob: bb,
    sshKeys: keys
      .split("\n")
      .map((k) => k.trim())
      .filter(Boolean),
    wifiNetworks: wifi.filter((w) => w.ssid).map((w, i, all) => ({ ssid: w.ssid, password: w.password, country: w.country || "US", priority: all.length - i })),
    timezone: tz || "UTC",
    keyboardLayout: kb || "us",
  });

  const saveDefaults = async () => {
    const s: Partial<FlashSettings> = readSettings();
    // The login password isn't kept, but Wi-Fi passwords are: without them a saved network
    // is useless (Quick Pi leaves it out), and defaults.json only lives on this Pi.
    delete s.password;
    try {
      await b.conn.json("POST", "/api/defaults", s);
      fd.defaults = s;
      fd.changed();
      b.toast("Defaults saved", "success");
    } catch {
      b.toast("Save failed", "error");
    }
  };

  const hostOk = !host.trim() || HOSTNAME_RE.test(host.trim());
  const im = fd.byKey(sel);
  const qim = fd.quickImage();
  const quickBlock = quickBlocker(quickSettings(d, ""), qim, fd.lists.quick.length);
  const quickOk = !quickBlock && HOSTNAME_RE.test(qpHost.trim());
  const goQuick = () => {
    if (!quickOk || !qim) return;
    b.closeModal();
    startFlash(b, flashRequest(qim, dev, quickSettings(d, qpHost.trim())));
  };
  const go = () => {
    if (!im || !hostOk) return;
    b.closeModal();
    startFlash(b, flashRequest(im, dev, readSettings()));
  };
  const pickFav = (key: string) => {
    const x = fd.byKey(key);
    if (!x) return;
    setSel(key);
    if (x.rpios) setTab("rpios");
    else {
      setTab("other");
      setDistro(distroOf(x, fd.images));
      setQ("");
    }
  };

  const warns: string[] = [];
  if (im) {
    if (!fitsBoard(im, board)) warns.push(`Not listed for the ${boardShort(board!.name)}.`);
    if (!takesSettings(im)) warns.push(`This image ignores the settings below (first-boot setup: ${initFormatLabel(im.initFormat)}).`);
    if (drive && im.extractSize > drive.size) warns.push(`Needs ${fmtSz(im.extractSize)} but the drive is ${fmtSz(drive.size)}.`);
  }

  return (
    <>
      <h3>Flash Image</h3>
      <DevLine dev={dev} />
      <div className="qp-bar">
        <span className="qp-title">⚡ Quick Pi</span>
        <span className="qp-imgs">
          <QuickChips />
        </span>
        <HostInput id="qpb-host" value={qpHost} set={setQpHost} onEnter={goQuick} />
        <button className="btn btn-primary btn-sm" disabled={!quickOk} onClick={goQuick}>
          Flash
        </button>
        <span className="pr-note" style={{ flexBasis: "100%" }}>
          {quickBlock || "Flashes the chosen image with the saved defaults; only the hostname is asked."}
        </span>
      </div>
      {fd.lists.favorites.length ? (
        <div className="fp-favs">
          <span className="fp-lbl">★ Favorites</span>
          {fd.lists.favorites.map((f) => {
            const x = fd.byKey(f.key);
            if (!x)
              return (
                <span key={f.key} className="fp-chip gone">
                  {f.label} (not in the catalog now)
                  <span className="x" role="button" aria-label="Remove" onClick={() => fd.toggle("favorites", f.key)}>
                    ×
                  </span>
                </span>
              );
            return (
              <span key={f.key} role="button" className={`fp-chip${sel === f.key ? " sel" : ""}${fitsBoard(x, board) ? "" : " off"}`} onClick={() => pickFav(f.key)}>
                {f.label}
              </span>
            );
          })}
        </div>
      ) : (
        <div className="fp-favs empty">☆ Star an image to pin it here.</div>
      )}
      <div className="fp-bar">
        <div className="fp-tabs">
          {[
            ["rpios", "Raspberry Pi OS"],
            ["other", "Other OSes"],
          ].map(([t, l]) => (
            <button key={t} className={`fp-tab${tab === t ? " on" : ""}`} onClick={() => setTab(t)}>
              {l}
            </button>
          ))}
        </div>
        <div className="fp-boards">
          <label className="fp-lbl" htmlFor="fp-board">
            Board
          </label>
          <select id="fp-board" value={board?.name || ""} onChange={(e) => setBoard(e.currentTarget.value)}>
            <option value="">Any</option>
            {fd.boards.map((x) => (
              <option key={x.name} value={x.name}>
                {boardShort(x.name)}
              </option>
            ))}
          </select>
        </div>
      </div>
      {tab === "rpios" ? (
        <RpiOs board={board} sel={sel} setSel={setSel} />
      ) : (
        <Distros
          board={board}
          sel={sel}
          setSel={setSel}
          distro={distro}
          setDistro={(id) => {
            setDistro(id);
            setQ("");
          }}
          q={q}
          setQ={setQ}
          setBoard={setBoard}
        />
      )}
      {!im ? (
        <div className="fp-selbox none">Pick an image above.</div>
      ) : (
        <div className="fp-selbox">
          <div className="t">
            <Star k={im.key} /> <QuickToggle im={im} /> <b>{im.name}</b> <span className="dim">{im.rpios ? "" : im.path.join(" › ")}</span>
          </div>
          <div className="dim">{im.description.replace(/<[^>]*>/g, "")}</div>
          <div className="dim">
            {fmtSz(im.downloadSize)} download · {fmtSz(im.extractSize)} on disk · released {im.releaseDate || "?"} · {imgBoards(im, fd.boards).join(", ") || "any board"} · settings via {initFormatLabel(im.initFormat)}
          </div>
          {warns.map((w) => (
            <div key={w} className="fp-warn">
              {w}
            </div>
          ))}
        </div>
      )}
      <div className="fg fp-host">
        <label htmlFor="sf-host">Hostname</label>
        <HostInput id="sf-host" value={host} set={setHost} onEnter={go} />
      </div>
      <details>
        <summary>More settings</summary>
        <div className="settings-grid">
          <div className="fg">
            <label htmlFor="sf-user">Username</label>
            <input type="text" id="sf-user" value={user} onInput={(e) => setUser(e.currentTarget.value)} />
          </div>
          <div className="fg">
            <label htmlFor="sf-pass">Password</label>
            <input type="password" id="sf-pass" placeholder="Blank = key-only SSH" value={pass} onInput={(e) => setPass(e.currentTarget.value)} />
          </div>
          <div className="fg full">
            <div className="check">
              <input type="checkbox" id="sf-ssh" checked={ssh} onChange={(e) => setSsh(e.currentTarget.checked)} />
              <label htmlFor="sf-ssh">Enable SSH</label>
            </div>
            <div className="check">
              <input type="checkbox" id="sf-sudo" checked={sudo} onChange={(e) => setSudo(e.currentTarget.checked)} />
              <label htmlFor="sf-sudo">Passwordless sudo</label>
            </div>
            <div className="check">
              <input type="checkbox" id="sf-bb" checked={bb} onChange={(e) => setBb(e.currentTarget.checked)} />
              <label htmlFor="sf-bb">
                Install BenchBob on first boot <span className="dim">(from its apt repo, once online)</span>
              </label>
            </div>
          </div>
          <div className="fg full">
            <label htmlFor="sf-keys">SSH Authorized Keys</label>
            <textarea
              id="sf-keys"
              rows={3}
              placeholder="ssh-ed25519 AAAA... user@host (one per line)"
              style={{ background: "var(--surface2)", border: "1px solid var(--border)", borderRadius: 5, padding: "7px 10px", color: "var(--text)", fontSize: ".8rem", fontFamily: "'SF Mono',ui-monospace,monospace", resize: "vertical", outline: "none", width: "100%" }}
              value={keys}
              onInput={(e) => setKeys(e.currentTarget.value)}
            />
          </div>
          <div className="fg full" style={{ marginTop: 8 }}>
            <label>WiFi Networks</label>
            <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
              {wifi.map((w) => {
                const upd = (p: Partial<WifiRow>) => setWifi((l) => l.map((x) => (x.id === w.id ? { ...x, ...p } : x)));
                return (
                  <div className="wifi-entry" key={w.id}>
                    <input type="text" className="wf-ssid" placeholder="SSID" value={w.ssid} onInput={(e) => upd({ ssid: e.currentTarget.value })} />
                    <input type="password" className="wf-pass" placeholder="Password" value={w.password} onInput={(e) => upd({ password: e.currentTarget.value })} />
                    <input type="text" className="wf-cc" placeholder="CC" value={w.country} onInput={(e) => upd({ country: e.currentTarget.value })} style={{ width: 50 }} />
                    <button className="btn btn-sm btn-danger" type="button" aria-label="Remove network" onClick={() => setWifi((l) => l.filter((x) => x.id !== w.id))}>
                      &times;
                    </button>
                  </div>
                );
              })}
            </div>
            <button className="btn btn-sm" type="button" style={{ marginTop: 6 }} onClick={() => setWifi((l) => [...l, { id: ++wid.current, ssid: "", password: "", country: "US" }])}>
              + Add WiFi Network
            </button>
          </div>
          <div className="fg">
            <label htmlFor="sf-tz">Timezone</label>
            <input type="text" id="sf-tz" value={tz} onInput={(e) => setTz(e.currentTarget.value)} />
          </div>
          <div className="fg">
            <label htmlFor="sf-kb">Keyboard</label>
            <input type="text" id="sf-kb" value={kb} onInput={(e) => setKb(e.currentTarget.value)} />
          </div>
          <div className="fg full" style={{ justifyContent: "flex-end" }}>
            <button className="btn btn-sm" type="button" onClick={saveDefaults}>
              Save as Default
            </button>
          </div>
        </div>
      </details>
      <div className="modal-btns">
        <button className="btn" onClick={b.closeModal}>
          Cancel
        </button>
        <button className="btn btn-primary" disabled={!im || !hostOk} onClick={go}>
          Flash
        </button>
      </div>
    </>
  );
}

function Star({ k }: { k: string }) {
  const fd = useFlashData();
  const on = fd.inList("favorites", k);
  return (
    <span
      className={`fp-star${on ? " on" : ""}`}
      role="button"
      aria-label={on ? "Remove from favorites" : "Add to favorites"}
      onClick={(e) => {
        e.stopPropagation();
        fd.toggle("favorites", k);
      }}
    >
      {on ? "★" : "☆"}
    </span>
  );
}

// Only images that take first-boot settings can go on the Quick Pi list: it sets the hostname.
function QuickToggle({ im }: { im: CatalogImage }) {
  const fd = useFlashData();
  if (!takesSettings(im)) return null;
  const on = fd.inList("quick", im.key);
  return (
    <span
      className={`fp-qk${on ? " on" : ""}`}
      role="button"
      aria-label={on ? "Remove from the Quick Pi list" : "Add to the Quick Pi list"}
      onClick={(e) => {
        e.stopPropagation();
        fd.toggle("quick", im.key);
      }}
    >
      ⚡
    </span>
  );
}

const ICON_NOTE = "★ pins an image to Favorites; ⚡ adds it to the Quick Pi list (only images that take first-boot settings).";

// Raspberry Pi OS as a grid: release × arch down the side, edition across the top.
function RpiOs({ board, sel, setSel }: { board: CatalogBoard | null; sel: string | null; setSel(k: string): void }) {
  const fd = useFlashData();
  const pios = fd.images.filter((i) => i.rpios);
  if (!pios.length) return <div className="fp-empty">Raspberry Pi OS isn't in the catalog right now (offline?).</div>;
  const rows: CatalogImage[][] = [];
  for (const legacy of [false, true])
    for (const arch of ["64", "32"]) {
      const row = pios.filter((i) => i.rpios!.legacy === legacy && i.rpios!.arch === arch);
      if (row.length) rows.push(row);
    }
  return (
    <>
      <table className="fp-rpios">
        <thead>
          <tr>
            <th></th>
            {RPIOS_EDITIONS.map(([, l, sub]) => (
              <th key={l}>
                {l}
                <small>{sub}</small>
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={`${row[0].rpios!.legacy}${row[0].rpios!.arch}`}>
              <th>
                <b>{cap(row[0].rpios!.release)}</b> {row[0].rpios!.arch}-bit{row[0].rpios!.legacy && <span className="fp-tag"> legacy</span>}
                <small>{imgBoards(row[0], fd.boards).join(" · ")}</small>
              </th>
              {RPIOS_EDITIONS.map(([ed]) => {
                const im = row.find((i) => i.rpios!.edition === ed);
                if (!im)
                  return (
                    <td key={ed} className="na">
                      —
                    </td>
                  );
                const cls = [sel === im.key && "sel", !fitsBoard(im, board) && "off"].filter(Boolean).join(" ");
                return (
                  <td key={ed}>
                    <div className={`fp-cell ${cls}`} role="button" onClick={() => setSel(im.key)}>
                      <Star k={im.key} />
                      <QuickToggle im={im} />
                      <span className="sz">{fmtSz(im.downloadSize)}</span>
                      <span className="sub">{fmtSz(im.extractSize)} on disk</span>
                    </div>
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
      <div className="fp-note">
        Released {pios[0].releaseDate}. Legacy is the previous Debian release. Sizes are download / written. {ICON_NOTE}
      </div>
    </>
  );
}

// Everything else: a distro menu on the left, that distro's images on the right.
function Distros(p: { board: CatalogBoard | null; sel: string | null; setSel(k: string): void; distro: string | null; setDistro(id: string): void; q: string; setQ(q: string): void; setBoard(n: string): void }) {
  const fd = useFlashData();
  const searchRef = useRef<HTMLInputElement>(null);
  const images = fd.images;
  const others = images.filter((i) => !i.rpios && !(i.path[0] || "").startsWith("Raspberry Pi OS"));
  const q = p.q.trim().toLowerCase();
  const match = (im: CatalogImage) => !q || [im.name, im.description, ...im.path].some((s) => s.toLowerCase().includes(q));
  const shown = others.filter((im) => match(im) && fitsBoard(im, p.board));
  const hidden = others.filter((im) => match(im) && !fitsBoard(im, p.board)).length;
  const ids = [...new Set(others.map((im) => distroOf(im, images)))];
  const count = (id: string) => shown.filter((im) => distroOf(im, images) === id).length;
  const hasFav = (id: string) => others.some((im) => distroOf(im, images) === id && fd.inList("favorites", im.key));
  const popular = POPULAR_DISTROS.map((n) => ids.find((id) => distroName(id) === n)).filter(Boolean) as string[];
  const distro = p.distro && ids.includes(p.distro) ? p.distro : popular.find(count) || ids[0];
  useEffect(() => {
    if (distro !== p.distro && distro) p.setDistro(distro);
  }, [distro]);

  const item = (id: string) => {
    const n = count(id);
    return (
      <div key={id} className={`fp-di${!q && distro === id ? " on" : ""}${n ? "" : " off"}`} role="button" onClick={() => p.setDistro(id)}>
        <span>
          {hasFav(id) && <span className="fp-star on">★ </span>}
          {distroName(id)}
        </span>
        <span className="n">{n}</span>
      </div>
    );
  };
  const commonShown = popular.filter((id) => !q || count(id));
  const groups = [...new Set(ids.map(distroGroup))];
  const list = q ? shown : shown.filter((im) => distroOf(im, images) === distro);
  let group: string | null = null;
  const rows: ReactNode[] = [];
  for (const im of list) {
    // Search results span distros, so group by full path; a distro's own list only by its
    // sub-folders (Apertis has three).
    const g = (q ? im.path : im.path.slice(distroDepth(im, images))).join(" › ");
    if (g !== group) {
      group = g;
      if (g)
        rows.push(
          <tr className="grp" key={"g" + g}>
            <td colSpan={5}>{g}</td>
          </tr>,
        );
    }
    rows.push(
      <tr key={im.key} className={p.sel === im.key ? "sel" : ""} onClick={() => p.setSel(im.key)}>
        <td className="st">
          <Star k={im.key} />
          <QuickToggle im={im} />
        </td>
        <td className="nm">
          <div>{im.name}</div>
          <div className="desc">{im.description.replace(/<[^>]*>/g, "")}</div>
        </td>
        <td className="bd">{imgBoards(im, fd.boards).join(" · ") || "any"}</td>
        <td className="num">{fmtSz(im.downloadSize)}</td>
        <td className="fs">{takesSettings(im) ? <span className="fp-ok">settings ✓</span> : <span className="fp-no">no settings</span>}</td>
      </tr>,
    );
  }
  return (
    <>
      <div className="fp-distros">
        <div className="fp-side">
          <input ref={searchRef} type="search" className="fb-filter" placeholder="Search all images…" value={p.q} onInput={(e) => p.setQ(e.currentTarget.value)} />
          {commonShown.length > 0 && (
            <>
              <div className="fp-dgrp">Common</div>
              {commonShown.map(item)}
            </>
          )}
          {groups.map((grp) => {
            const inGrp = ids.filter((id) => distroGroup(id) === grp && !popular.includes(id) && (!q || count(id)));
            return inGrp.length ? (
              <div key={grp}>
                <div className="fp-dgrp">{grp}</div>
                {inGrp.map(item)}
              </div>
            ) : null;
          })}
        </div>
        <div className="fp-main">
          <table className="fp-imgs">
            <tbody>
              {rows}
              {!list.length && (
                <tr>
                  <td className="fp-empty" colSpan={5}>
                    {q ? "No images match." : "Nothing here for this board."}
                  </td>
                </tr>
              )}
            </tbody>
          </table>
          {hidden > 0 && p.board && (
            <div className="fp-note">
              {hidden} more image{hidden === 1 ? "" : "s"} hidden: not for the {boardShort(p.board.name)}. <a onClick={() => p.setBoard("")}>Show all boards</a>
            </div>
          )}
        </div>
      </div>
      <div className="fp-note">“settings ✓”: hostname, user, SSH and Wi-Fi are applied on first boot. “no settings”: the image ignores them. {ICON_NOTE}</div>
    </>
  );
}
