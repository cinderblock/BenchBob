// Read-only view of the auto-mounted partitions. Paths are relative to the server's files
// root (/srv/nfs/drives); "" is the list of drives itself. Partitions with something
// recognisable on them (EFI loaders, an OS install, a Pi bootfs) get an info box on top.

import { Fragment, useEffect, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { useBob } from "../../bob/context";
import { fmtSz } from "../../lib/format";
import { ActionLine, errText, useAction } from "../../ui/status";
import type { Listing, OsInfo, OsInfoCache, OsPartition } from "./api";

const FB_ICONS: Record<string, string> = { dir: "📁", file: "📄", broken: "⚠️", outside: "🚫", other: "⚙️" };
const FB_NOTES: Record<string, string> = { broken: "broken link", outside: "points outside this drive" };
const fbJoin = (dir: string, name: string) => (dir ? dir + "/" + name : name);
const fbDate = (ms?: number) => (ms ? new Date(ms).toLocaleString(undefined, { year: "numeric", month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }) : "");

/** Open the browser at `path`; the first listing is fetched before the dialog shows. */
export function FileBrowser({ start, os }: { start: string; os: OsInfoCache }) {
  const b = useBob();
  const [fb, setFb] = useState<Listing | null>(null);
  const [path, setPath] = useState(start);
  const [q, setQ] = useState("");
  const [infoOpen, setInfoOpen] = useState(true);
  const nav = useAction();
  const seq = useRef(0);
  const listRef = useRef<HTMLDivElement>(null);
  const filterRef = useRef<HTMLInputElement>(null);
  useSyncExternalStore(os.subscribe, os.version);

  const open = async (p: string) => {
    // Responses can arrive out of order when clicking quickly; only the latest renders.
    const my = ++seq.current;
    setPath(p);
    try {
      const r = await b.api<Listing>("GET", "/api/files?path=" + encodeURIComponent(p));
      if (my !== seq.current) return;
      nav.clear();
      setFb(r);
      setQ("");
      if (listRef.current) listRef.current.scrollTop = 0;
    } catch (e) {
      if (my !== seq.current) return;
      // The listing on screen stays (the crumbs go back to it); the failure shows above it.
      if (fb) setPath(fb.path);
      nav.fail(`Couldn't open ${p ? "/" + p : "the drives list"}: ${errText(e)}`);
    }
  };
  useEffect(() => {
    open(start);
    filterRef.current?.focus();
  }, []);

  const cur = fb?.path ?? path;
  const parts = cur ? cur.split("/") : [];
  const info = os.filesTarget(cur);
  const ql = q.trim().toLowerCase();
  const list = fb ? (ql ? fb.entries.filter((e) => e.name.toLowerCase().includes(ql)) : fb.entries) : [];
  const files = fb?.entries.filter((e) => e.type === "file") || [];
  let foot = fb ? `${fb.total} item${fb.total === 1 ? "" : "s"} · ${fmtSz(files.reduce((a, e) => a + (e.size || 0), 0))} in files` : "";
  if (fb && ql) foot = `${list.length} of ${fb.total} shown`;
  if (fb?.truncated) foot += ` · only the first ${fb.entries.length} listed`;
  if (fb?.warning) foot += ` · ⚠️ ${fb.warning}`;

  return (
    <>
      <h3>Files</h3>
      <div className="fb-crumbs">
        <a onClick={() => open("")}>drives</a>
        {parts.map((seg, i) => (
          <Fragment key={i}>
            <span className="sep">/</span>
            {i === parts.length - 1 ? <span className="cur">{seg}</span> : <a onClick={() => open(parts.slice(0, i + 1).join("/"))}>{seg}</a>}
          </Fragment>
        ))}
      </div>
      <ActionLine a={nav} />
      {info && <OsDetails t={info} open={infoOpen} setOpen={setInfoOpen} />}
      <input ref={filterRef} className="fb-filter" placeholder="Filter this folder…" autoComplete="off" spellCheck={false} value={q} onInput={(e) => setQ(e.currentTarget.value)} />
      <div className={`fb-list${info ? " short" : ""}`} ref={listRef}>
        {!fb ? (
          <div className="fb-msg">{nav.status?.state === "error" ? "Nothing to show." : "Loading…"}</div>
        ) : (
          <>
            <table className="fb-tbl">
              <colgroup>
                <col />
                <col style={{ width: 90 }} />
                <col style={{ width: 170 }} className="fb-mod" />
                <col style={{ width: 44 }} />
              </colgroup>
              <thead>
                <tr>
                  <th>Name</th>
                  <th style={{ textAlign: "right" }}>Size</th>
                  <th style={{ textAlign: "right" }}>Modified</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                {fb.path && !ql && (
                  <tr className="nav" onClick={() => open(fb.path.split("/").slice(0, -1).join("/"))}>
                    <td colSpan={4}>
                      <span className="fb-ico">⬆️</span>..
                    </td>
                  </tr>
                )}
                {list.map((e) => {
                  const p = fbJoin(fb.path, e.name);
                  const note = FB_NOTES[e.type] ? ` (${FB_NOTES[e.type]})` : "";
                  const name = (
                    <>
                      <span className="fb-ico">{FB_ICONS[e.type] || FB_ICONS.other}</span>
                      {e.name}
                      {e.link && (
                        <span className="fb-link">
                          {" "}
                          → {e.link}
                          {note}
                        </span>
                      )}
                    </>
                  );
                  const size = e.size == null ? "" : fmtSz(e.size);
                  if (e.type === "dir")
                    return (
                      <tr key={e.name} className="nav" onClick={() => open(p)}>
                        <td className="fb-name">{name}</td>
                        <td className="num"></td>
                        <td className="num">{fbDate(e.mtime)}</td>
                        <td></td>
                      </tr>
                    );
                  if (e.type === "file") {
                    const href = b.conn.url("/api/download?path=" + encodeURIComponent(p));
                    return (
                      <tr key={e.name}>
                        <td className="fb-name">
                          <a href={href} download style={{ color: "inherit" }}>
                            {name}
                          </a>
                        </td>
                        <td className="num">{size}</td>
                        <td className="num">{fbDate(e.mtime)}</td>
                        <td>
                          <a className="fb-dl" href={href} download aria-label="Download">
                            ⬇️
                          </a>
                        </td>
                      </tr>
                    );
                  }
                  return (
                    <tr key={e.name} style={{ color: "var(--dim)" }}>
                      <td className="fb-name">{name}</td>
                      <td className="num">{size}</td>
                      <td className="num">{fbDate(e.mtime)}</td>
                      <td></td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
            {!list.length && <div className="fb-msg">{ql ? "Nothing matches the filter" : "Empty folder"}</div>}
          </>
        )}
      </div>
      <div className="fb-foot">
        <span>{foot}</span>
        <button className="btn btn-primary" onClick={b.closeModal}>
          Close
        </button>
      </div>
    </>
  );
}

function KV({ rows }: { rows: [string, ReactNode][] }) {
  return (
    <div className="fb-kv">
      {rows
        .filter((r) => r[1] != null && r[1] !== "")
        .map(([k, v]) => (
          <Fragment key={k}>
            <span>{k}</span>
            <span>{v}</span>
          </Fragment>
        ))}
    </div>
  );
}

const Tag = ({ children }: { children: ReactNode }) => <span className="fb-tag">{children}</span>;
const Warn = ({ children }: { children: ReactNode }) => <span className="fb-warn">{children}</span>;

function OsDetails({ t, open, setOpen }: { t: { part: OsPartition; disk: OsInfo }; open: boolean; setOpen(v: boolean): void }) {
  const { part: p, disk } = t;
  const [filesOpen, setFilesOpen] = useState<string | null>(null);
  const sections: ReactNode[] = [];
  let title: ReactNode = "";
  const espEntriesFor = (name: string) => disk.partitions.flatMap((q) => q.esp?.windowsBoot?.entries || []).filter((e) => e.partition === name);

  if (p.esp) {
    const e = p.esp;
    title = `💾 EFI System Partition — ${e.loaders.length} loader folder${e.loaders.length === 1 ? "" : "s"}`;
    sections.push(
      <div key="esp">
        <h4>Boot loaders</h4>
        <table className="fb-ld">
          <colgroup>
            <col style={{ width: "30%" }} />
            <col style={{ width: "22%" }} />
            <col style={{ width: "9%" }} />
            <col />
          </colgroup>
          <tbody>
            {e.loaders.map((l) => {
              const notes: ReactNode[] = [];
              if (l.shim) notes.push("Secure Boot shim");
              if (l.target)
                notes.push(
                  l.target.partition ? (
                    <>
                      GRUB loads <b>/boot</b> from {l.target.partition}
                      {l.target.system ? " — " + l.target.system : ""}
                    </>
                  ) : (
                    <Warn>
                      ⚠ GRUB looks for filesystem <span className="mono">{l.target.uuid}</span>, which isn't on this drive — left over from a removed install
                    </Warn>
                  ),
                );
              if (l.kind === "fallback" && e.fallback) {
                const f = e.fallback,
                  file = f.file.split("/").pop();
                if (f.sameAs) {
                  notes.push(
                    <>
                      {file} is a copy of <span className="mono">EFI/{f.sameAs}</span>
                    </>,
                  );
                  const src = e.loaders.find((x) => f.sameAs!.toLowerCase().startsWith(x.dir.toLowerCase() + "/"));
                  if (src?.target && !src.target.partition) notes.push(<Warn>⚠ so the firmware's fallback boot leads to that broken GRUB</Warn>);
                } else notes.push(`${file} doesn't match any other loader here`);
              }
              if (l.dir.toLowerCase() === "microsoft" && e.windowsBoot) {
                const menu = e.windowsBoot.entries.filter((x) => x.inMenu);
                if (menu.length) notes.push("Menu: " + menu.map((x) => `"${x.description}"${x.partition ? " → " + x.partition : ""}${x.isDefault ? " (default)" : ""}`).join(", "));
              }
              const showFiles = filesOpen === l.dir;
              return (
                <tr key={l.dir} onClick={() => setFilesOpen(showFiles ? null : l.dir)} style={{ cursor: "pointer" }}>
                  <td>{l.name}</td>
                  <td className="mono">EFI/{l.dir}</td>
                  <td className="mono">{l.arch || ""}</td>
                  <td>
                    {notes.length ? (
                      notes.map((n, i) => (
                        <Fragment key={i}>
                          {i > 0 && <br />}
                          {n}
                        </Fragment>
                      ))
                    ) : (
                      <span className="mono">{l.files.join(", ") || "no .efi files"}</span>
                    )}
                    {showFiles && notes.length > 0 && (
                      <div className="mono" style={{ marginTop: 2 }}>
                        {l.files.join(", ") || "no .efi files"}
                      </div>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
        <div className="pr-note">Tap a loader for its .efi files.</div>
      </div>,
    );
    if (e.windowsBoot?.entries.length) {
      const wb = e.windowsBoot;
      sections.push(
        <div key="bcd">
          <h4>Windows boot configuration (BCD){wb.timeout != null ? ` · menu timeout ${wb.timeout}s` : ""}</h4>
          <table className="fb-ld">
            <colgroup>
              <col style={{ width: "38%" }} />
              <col style={{ width: "12%" }} />
              <col />
            </colgroup>
            <tbody>
              {wb.entries.map((x, i) => (
                <tr key={i}>
                  <td>
                    {x.description}
                    {x.isDefault && <Tag>default</Tag>}
                    {x.inMenu && <Tag>in menu</Tag>}
                    {x.kind !== "os" && <Tag>{x.kind}</Tag>}
                  </td>
                  <td className="mono">{x.partition || "?"}</td>
                  <td className="mono">{x.path || ""}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>,
      );
    }
    if (e.systemdBoot?.entries.length) {
      const sb = e.systemdBoot;
      sections.push(
        <div key="sdb">
          <h4>systemd-boot entries</h4>
          <table className="fb-ld">
            <tbody>
              {sb.entries.map((x) => (
                <tr key={x.file}>
                  <td>
                    {x.title}
                    {sb.default && x.file.startsWith(sb.default.replace(/\*.*$/, "")) && <Tag>default</Tag>}
                  </td>
                  <td className="mono">{x.version || ""}</td>
                  <td className="mono">{x.file}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>,
      );
    }
  }

  if (p.windows) {
    const w = p.windows;
    title = `🪟 ${[w.product, w.version].filter(Boolean).join(" ")}`;
    const via = espEntriesFor(p.name)
      .filter((x) => x.kind === "os")
      .map((x) => `"${x.description}"${x.isDefault ? " (default)" : ""}`);
    sections.push(
      <div key="win">
        <h4>Windows installation</h4>
        <KV
          rows={[
            [
              "Edition",
              <>
                {w.product}
                {w.edition && <Tag>{w.edition}</Tag>}
              </>,
            ],
            ["Version", w.version],
            ["Build", w.build],
            ["Installed", w.installed ? new Date(w.installed).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" }) : null],
            ["PC name", w.computerName],
            ["Boot menu", via.length ? "Windows Boot Manager: " + via.join(", ") : <Warn>no boot entry on this drive points here</Warn>],
          ]}
        />
      </div>,
    );
  }

  if (p.linux) {
    const l = p.linux;
    title = `${disk.partitions.some((q) => q.pi?.rootPartition === p.name) ? "🍓" : "🐧"} ${l.name}`;
    const via = [
      ...disk.partitions.flatMap((q) => (q.esp?.loaders || []).filter((x) => x.target?.partition === p.name).map((x) => `EFI/${x.dir} on ${q.name}`)),
      ...disk.partitions.filter((q) => q.pi?.rootPartition === p.name).map((q) => `Raspberry Pi bootfs on ${q.name}`),
    ];
    sections.push(
      <div key="linux">
        <h4>Linux installation</h4>
        <KV
          rows={[
            ["OS", l.name],
            ["Hostname", l.hostname],
            ["Booted by", via.join(", ") || null],
          ]}
        />
      </div>,
    );
  }

  if (p.pi) {
    title = title || "🍓 Raspberry Pi boot partition";
    sections.push(
      <div key="pi">
        <h4>Raspberry Pi boot partition</h4>
        <KV
          rows={[
            ["Image", p.pi.image],
            ["Root filesystem", p.pi.rootPartition ? p.pi.rootPartition : p.pi.rootPartuuid ? <Warn>PARTUUID {p.pi.rootPartuuid} isn't on this drive</Warn> : "not set in cmdline.txt"],
          ]}
        />
      </div>,
    );
  }

  if (p.macos) {
    title = title || `🍎 ${p.macos.name} ${p.macos.version || ""}`;
    sections.push(
      <div key="mac">
        <h4>macOS installation</h4>
        <KV
          rows={[
            ["OS", p.macos.name],
            ["Version", p.macos.version],
          ]}
        />
      </div>,
    );
  }

  return (
    <details className="fb-info" open={open} onToggle={(e) => setOpen((e.currentTarget as HTMLDetailsElement).open)}>
      <summary>{title}</summary>
      <div className="fb-info-body">{sections}</div>
    </details>
  );
}
