// Wi-Fi (backend/crates/wifi): radios + regulatory domain, a scanner with per-band
// channel-occupancy charts and a sortable BSS table, and netns client tests with a live
// timeline. Status carries only sequence numbers for the bulky data; this fetches
// /api/wifi/{radios,scan,results,profiles} when one changes. Forms hold their own state, so
// the status push never disturbs them.

import { Fragment, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useBob } from "../../bob/context";
import type { PanelProps } from "../../registry";
import { fmtAgo, wifiColor } from "../../lib/format";
import type { Bss, Profile, Radio, RadiosResp, ScanResp, Step, TestResult, WifiState } from "./api";
import { HistoryChart, OccupancyChart, WSpark, bandName, chOf, ssidColor } from "./charts";
import "./style.css";

const secs = (ms: number) => (ms / 1000).toFixed(1) + "s";
const Sig = ({ dbm }: { dbm: number | null | undefined }) => (dbm == null ? <span className="wf-dim">?</span> : <span style={{ color: wifiColor(dbm), fontWeight: 600 }}>{dbm}</span>);
const SecPill = ({ b }: { b: Bss }) => <span className={`pr-pill ${b.securityKind === "open" || b.securityKind === "wep" ? "bad" : b.securityKind === "psk" ? "" : "on"}`}>{b.security}</span>;
const ifName = (r: Radio) => r.ifaces.find((i) => i.name)?.name;

// Incremental history: after the first full fetch, ask only for samples newer than the last
// answer's server time and append them, trimming like the server does (1 h, N max).
function mergeScan(prev: ScanResp | null, next: ScanResp): ScanResp {
  if (!next.historySince || !prev) return next;
  const old = new Map(prev.bss.map((b) => [b.bssid, b.history]));
  for (const b of next.bss) {
    const h = [...(old.get(b.bssid) || []), ...b.history];
    let i = 0;
    while (i < h.length && next.now - h[i][0] > next.historyMaxAgeMs) i++;
    b.history = h.slice(Math.max(i, h.length - next.historyMax));
  }
  return next;
}

function bandSummary(b: Radio["bandInfo"][number]): ReactNode {
  const on = b.channels.filter((c) => !c.disabled);
  const off = b.channels.length - on.length;
  const dfs = on.filter((c) => c.radar).length;
  const pw = Math.max(0, ...on.map((c) => c.maxPower || 0));
  return (
    <>
      <b>{bandName[b.band]}</b> {on.length} channels{off ? ` (+${off} disabled)` : ""}
      {dfs ? `, ${dfs} DFS` : ""}, up to {b.maxWidth} MHz{pw ? `, ≤${pw} dBm` : ""} · {[b.ht && "n", b.vht && "ac", b.he && "ax", b.eht && "be"].filter(Boolean).join("/") || "legacy"}
    </>
  );
}

export default function Wifi({ st }: PanelProps<WifiState>) {
  const b = useBob();
  const [radios, setRadios] = useState<RadiosResp | null>(null);
  const [scan, setScanState] = useState<ScanResp | null>(null);
  const scanRef = useRef<ScanResp | null>(null);
  const setScan = (s: ScanResp | null) => {
    scanRef.current = s;
    setScanState(s);
  };
  const [results, setResults] = useState<TestResult[]>([]);
  const [profiles, setProfiles] = useState<Profile[]>([]);
  const seq = useRef({ radios: -1, scan: -1, results: -1, profiles: -1 });
  const [band, setBand] = useState("");
  const [hover, setHover] = useState("");
  const [pinned, setPinned] = useState("");
  const hl = hover || pinned;
  const togglePin = (bssid: string) => setPinned((p) => (p === bssid ? "" : bssid));
  const [win, setWin] = useState(30);
  const [group, setGroup] = useState(false);
  const formRef = useRef<HTMLDivElement>(null);
  const [form, setForm] = useState<Form>(EMPTY_FORM);

  const load = async (what: "radios" | "scan" | "results" | "profiles") => {
    try {
      if (what === "radios") setRadios(await b.api<RadiosResp>("GET", "/api/wifi/radios"));
      if (what === "scan") {
        const prev = scanRef.current;
        setScan(mergeScan(prev, await b.api<ScanResp>("GET", `/api/wifi/scan${prev?.now ? `?historySince=${prev.now}` : ""}`)));
      }
      if (what === "results") setResults((await b.api<{ results: TestResult[] }>("GET", "/api/wifi/results")).results);
      if (what === "profiles") setProfiles((await b.api<{ profiles: Profile[] }>("GET", "/api/wifi/profiles")).profiles);
    } catch {}
  };

  useEffect(() => {
    const s = seq.current;
    if (st.radiosSeq !== s.radios) {
      s.radios = st.radiosSeq;
      load("radios");
    }
    if (st.scan.seq !== s.scan) {
      s.scan = st.scan.seq;
      load("scan");
    }
    if (st.resultsSeq !== s.results) {
      s.results = st.resultsSeq;
      load("results");
    }
    if (st.profilesCount !== s.profiles) {
      s.profiles = st.profilesCount;
      load("profiles");
    }
  }, [st.radiosSeq, st.scan.seq, st.resultsSeq, st.profilesCount]);

  // "Someone is looking": the driver scans on its own schedule while any viewer's lease is
  // fresh. Plain fetch, not api(): a missed renewal isn't worth a toast.
  useEffect(() => {
    const id = Math.random().toString(36).slice(2);
    const send = (method: string) => b.conn.fetch("/api/wifi/watch", { method, headers: { "Content-Type": "application/json" }, body: JSON.stringify({ id }) }).catch(() => {});
    const watch = () => document.visibilityState === "visible" && send("POST");
    watch();
    const t = setInterval(watch, 20000);
    document.addEventListener("visibilitychange", watch);
    return () => {
      clearInterval(t);
      document.removeEventListener("visibilitychange", watch);
      send("DELETE");
    };
  }, [b.conn]);

  const useBss = (x: Bss) => {
    setForm((f) => ({ ...f, ssid: x.ssid, ...(x.securityKind !== "wep" ? { security: x.securityKind } : {}) }));
    formRef.current?.scrollIntoView({ behavior: "smooth", block: "center" });
  };

  return (
    <>
      <div className="wf-blk">
        <h4>Radios</h4>
        <div className="wf-radios">{radios ? <Radios R={radios} reload={() => load("radios")} /> : <div className="pr-note">Loading…</div>}</div>
      </div>
      <div className="wf-blk">
        <h4>Scanner</h4>
        <ScanControls st={st} radios={radios?.radios || []} band={band} setBand={setBand} group={group} setGroup={setGroup} onClear={() => setScan(null)} />
        <ScanMeta st={st} scan={scan} />
        {scan && <Charts scan={scan} radios={radios?.radios || []} band={band} />}
        {scan && <HistoryChart bss={scan.bss} band={band} win={win} setWin={setWin} ivS={st.scan.auto?.watchIntervalS || 30} hl={hl} pinned={pinned} setHover={setHover} togglePin={togglePin} />}
        <Survey scan={scan} />
        <div style={{ marginTop: 8 }} onPointerLeave={(e) => e.pointerType === "mouse" && setHover("")}>
          {scan && <BssTable scan={scan} band={band} group={group} hl={hl} setHover={setHover} togglePin={togglePin} onUse={useBss} />}
        </div>
      </div>
      <div className="wf-blk" ref={formRef}>
        <h4>Client test</h4>
        <div className="pr-note">Associates a radio inside its own network namespace (its address, routes and DNS never touch eth0 or the main routing table), then runs the checks below. The radio is always given back afterwards.</div>
        <ClientTest form={form} setForm={setForm} radios={radios} scan={scan} profiles={profiles} reloadProfiles={() => load("profiles")} />
        <RunningTests st={st} />
        <h4 style={{ marginTop: 12 }}>Recent results</h4>
        <Results results={results} />
      </div>
    </>
  );
}

function Radios({ R, reload }: { R: RadiosResp; reload(): void }) {
  const b = useBob();
  const [showChans, setShowChans] = useState<Set<string>>(new Set());
  const [country, setCountry] = useState("");
  const [note, setNote] = useState("");
  const g = R.reg;
  return (
    <>
      {!R.radios.length && <div className="pr-note">No Wi-Fi radios found.</div>}
      {R.radios.map((r) => {
        const ifn =
          r.ifaces
            .filter((i) => i.name)
            .map((i) => i.name)
            .join(", ") || "no interface";
        const where = r.onboard ? (
          `Onboard (${r.bus.toUpperCase()})`
        ) : r.bus === "virtual" ? (
          "Virtual (mac80211_hwsim)"
        ) : (
          <>
            USB: {[r.usb?.vendor, r.usb?.model].filter(Boolean).join(" ") || r.usb?.id || "?"}
            {r.usb && <span className="wf-dim wf-mono"> {r.usb.id}</span>}
          </>
        );
        const kind = r.use.kind;
        const pill = kind === "primary" ? "warn" : kind === "backup" || kind === "connected" ? "on" : kind === "test" || kind === "scan" ? "warn" : "";
        const managed = async (v: boolean) => {
          try {
            await b.api("POST", `/api/wifi/radios/${r.phy}/managed`, { managed: v });
            reload();
          } catch {}
        };
        const nmCtl =
          !r.onboard && r.nm.length && r.use.kind !== "test" ? (
            r.unmanagedByUs.length ? (
              <button className="btn btn-sm" onClick={() => managed(true)}>
                Give back to NetworkManager
              </button>
            ) : r.nm.some((n) => n.state === "connected") ? null : (
              <button className="btn btn-sm" onClick={() => managed(false)}>
                Keep NetworkManager off it
              </button>
            )
          ) : null;
        const shown = showChans.has(r.phy);
        return (
          <div className="pr-card" key={r.phy}>
            <h4>
              {r.label || r.phy}{" "}
              <small>
                {ifn} · {r.driver || "?"} · {where}
              </small>
              <span className={`pr-pill ${pill}`}>{r.use.text}</span>
            </h4>
            <div className="pr-note">
              {r.bandInfo.length
                ? r.bandInfo.map((x, i) => (
                    <Fragment key={x.band}>
                      {i > 0 && <br />}
                      {bandSummary(x)}
                    </Fragment>
                  ))
                : "No band information"}
            </div>
            <div className="pr-note">
              Modes: {r.iftypes.join(", ")}
              {r.monitor ? "" : <> · <b>no monitor mode</b></>}
              {r.sae ? " · WPA3-SAE capable" : ""}
              {r.netns ? "" : <> · <b>can’t move to a netns (no client tests)</b></>}
              {r.regCountry ? ` · own regulatory domain ${r.regCountry}` : ""}
            </div>
            {r.unmanagedByUs.length > 0 && (
              <div className="pr-note" style={{ color: "var(--warning)" }}>
                BenchBob told NetworkManager to leave {r.unmanagedByUs.join(", ")} alone (until reboot) so it can’t auto-connect while BenchBob uses it.
              </div>
            )}
            <div className="pr-row">
              <button
                className="btn btn-sm"
                onClick={() =>
                  setShowChans((s) => {
                    const n = new Set(s);
                    n.has(r.phy) ? n.delete(r.phy) : n.add(r.phy);
                    return n;
                  })
                }
              >
                {shown ? "Hide channels" : "Show channels"}
              </button>
              {nmCtl}
            </div>
            {shown &&
              r.bandInfo.map((x) => (
                <Fragment key={x.band}>
                  <div className="pr-note" style={{ marginTop: 6 }}>
                    {bandName[x.band]}: <span className="wf-ch">enabled</span> <span className="wf-ch dfs">DFS/radar</span> <span className="wf-ch off">disabled</span>
                  </div>
                  <div className="wf-chans">
                    {x.channels.map((c) => (
                      <span key={c.ch} className={`wf-ch ${c.disabled ? "off" : c.radar ? "dfs" : ""}`}>
                        {c.ch}
                        {c.maxPower && !c.disabled ? <span className="wf-dim"> {c.maxPower}</span> : null}
                        {c.noIR && !c.disabled ? <span className="wf-dim"> passive</span> : null}
                      </span>
                    ))}
                  </div>
                </Fragment>
              ))}
          </div>
        );
      })}
      <div className="pr-row" style={{ marginTop: 10 }}>
        <span className="pr-note">
          Regulatory domain: <b style={{ color: "var(--text)" }}>{g.country || "?"}</b>
          {g.dfsRegion ? ` (${g.dfsRegion})` : ""}
          {g.changedFrom != null && <span className="pr-pill warn"> temporary, was {g.changedFrom}</span>}
          {g.phys.map((p) => ` · ${p.scope}: ${p.country}${p.selfManaged ? " (self-managed)" : ""}`).join("")}
        </span>
        <label>
          Country <input className="wf-reg" size={3} maxLength={2} placeholder="US" value={country} onInput={(e) => setCountry(e.currentTarget.value)} />
        </label>
        <button
          className="btn btn-sm"
          onClick={async () => {
            try {
              const r = await b.api<{ note?: string; country: string }>("POST", "/api/wifi/reg", { country: country.trim() });
              setNote(r.note || `Regulatory domain is now ${r.country}.`);
              reload();
            } catch {}
          }}
        >
          Set until reboot
        </button>
      </div>
      <div className="pr-note wf-regnote">{note}</div>
      {!R.ouiSource && (
        <div className="pr-note">
          No OUI vendor database installed, so the vendor column is empty (apt package <span className="wf-mono">ieee-data</span> provides one).
        </div>
      )}
    </>
  );
}

const ivLabel = (v: number) => (v < 60 ? v + " s" : v / 60 + " min");

function ScanControls({ st, radios, band, setBand, group, setGroup, onClear }: { st: WifiState; radios: Radio[]; band: string; setBand(b: string): void; group: boolean; setGroup(g: boolean): void; onClear(): void }) {
  const b = useBob();
  const auto = st.scan.auto || {};
  const busy = !!st.scan.scanning?.length;
  const [radio, setRadio] = useState<string | null>(null);
  const [clicked, setClicked] = useState(false);
  const cur = radio ?? (auto.radio && radios.some((r) => r.phy === auto.radio) ? auto.radio : radios[0]?.phy) ?? "";
  useEffect(() => {
    if (!busy) setClicked(false);
  }, [busy]);
  const putAuto = (p: Record<string, unknown>) => b.api("PUT", "/api/wifi/autoscan", p).catch(() => {});
  return (
    <div className="pr-row wf-scanctl">
      <select className="wf-scanradio" value={cur} onChange={(e) => setRadio(e.currentTarget.value)}>
        {radios.map((r) => (
          <option key={r.phy} value={r.phy}>
            {r.label || r.phy} ({ifName(r) || "-"})
          </option>
        ))}
      </select>
      <button
        className="btn btn-sm btn-primary"
        disabled={busy || clicked}
        onClick={async () => {
          setClicked(true);
          try {
            await b.api("POST", "/api/wifi/scan", { radio: cur || undefined });
          } catch {
            setClicked(false);
          }
        }}
      >
        {busy || clicked ? "Scanning…" : "Scan now"}
      </button>
      <label className="pr-note">
        While open, every{" "}
        <select value={auto.watchIntervalS || 30} onChange={(e) => putAuto({ watchIntervalS: +e.currentTarget.value })}>
          {[15, 30, 60, 120].map((v) => (
            <option key={v} value={v}>
              {ivLabel(v)}
            </option>
          ))}
        </select>
      </label>
      <AutoScan auto={auto} radio={cur} put={putAuto} />
      <label className="pr-note">
        <input type="checkbox" checked={group} onChange={(e) => setGroup(e.currentTarget.checked)} /> group by SSID
      </label>
      <select value={band} onChange={(e) => setBand(e.currentTarget.value)}>
        <option value="">all bands</option>
        {["2.4", "5", "6"].map((x) => (
          <option key={x} value={x}>
            {bandName[x]}
          </option>
        ))}
      </select>
      <button
        className="btn btn-sm"
        onClick={async () => {
          try {
            await b.api("DELETE", "/api/wifi/scan");
            onClear();
          } catch {}
        }}
      >
        Forget history
      </button>
    </div>
  );
}

function AutoScan({ auto, radio, put }: { auto: NonNullable<WifiState["scan"]["auto"]>; radio: string; put(p: Record<string, unknown>): void }) {
  const [interval, setInterval_] = useState<number | null>(null);
  const iv = interval ?? auto.intervalS ?? 60;
  return (
    <>
      <label className="pr-note">
        <input type="checkbox" checked={!!auto.enabled} onChange={(e) => put({ enabled: e.currentTarget.checked, intervalS: iv, radio })} /> also in the background, every
      </label>
      <select
        value={iv}
        onChange={(e) => {
          setInterval_(+e.currentTarget.value);
          put({ enabled: !!auto.enabled, intervalS: +e.currentTarget.value, radio });
        }}
      >
        {[15, 30, 60, 120, 300, 600].map((v) => (
          <option key={v} value={v}>
            {ivLabel(v)}
          </option>
        ))}
      </select>
    </>
  );
}

function ScanMeta({ st, scan }: { st: WifiState; scan: ScanResp | null }) {
  const s = scan?.scan;
  const auto = st.scan.auto || {};
  if (!s?.at && !st.scan.watchers) return <div className="pr-note wf-scanmeta">No scan yet. Scans don’t disturb an existing connection.</div>;
  return (
    <div className="pr-note wf-scanmeta">
      {s?.at && (
        <>
          Last scan {fmtAgo(s.at)} on {s.iface || s.radio} via {s.method === "nm" ? "NetworkManager rescan + iw scan dump" : "iw scan"} in {secs(s.durationMs)}: {s.count} BSS · {scan!.bss.length} remembered (1 h)
        </>
      )}
      {s?.error && (
        <>
          {" "}
          · <span style={{ color: "var(--danger)" }}>{s.error}</span>
        </>
      )}
      {s?.at && !s.survey ? " · channel survey: not supported by this driver" : ""}
      {st.scan.watchers ? ` · scanning every ${auto.watchIntervalS || 30} s while ${st.scan.watchers > 1 ? `${st.scan.watchers} viewers have the panel` : "this panel is"} open` : ""}
    </div>
  );
}

function Survey({ scan }: { scan: ScanResp | null }) {
  const sv = scan?.scan.survey;
  if (!sv) return null;
  return (
    <div className="wf-survey">
      <details>
        <summary>
          Channel survey: {sv.length} channels, busiest {Math.max(...sv.map((x) => x.busyPct ?? 0))}%
        </summary>
        <div className="wf-scroll">
          <table className="pr-tbl">
            <thead>
              <tr>
                <th>Freq</th>
                <th>Ch</th>
                <th>Busy</th>
                <th>Noise</th>
                <th>Active</th>
              </tr>
            </thead>
            <tbody>
              {sv.map((x) => (
                <tr key={x.freq}>
                  <td>
                    {x.freq}
                    {x.inUse && <span className="pr-pill on"> in use</span>}
                  </td>
                  <td>{chOf(x.freq)}</td>
                  <td>{x.busyPct != null ? x.busyPct + "%" : "-"}</td>
                  <td>{x.noise ?? "-"} dBm</td>
                  <td>{x.activeMs ?? "-"} ms</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </details>
    </div>
  );
}

function Charts({ scan, radios, band }: { scan: ScanResp; radios: Radio[]; band: string }) {
  const all = scan.bss.filter((x) => Date.now() - x.lastSeen < 10 * 60 * 1000); // recent only
  const radio = radios.find((r) => r.phy === (scan.scan.radio || "")) || radios[0];
  const chans = (bd: string) => radio?.bandInfo.find((x) => x.band === bd)?.channels || [];
  const has6 = radios.some((r) => r.bands.includes("6")) || all.some((x) => x.band === "6");
  return (
    <div className="wf-charts">
      {["2.4", "5", ...(has6 ? ["6"] : [])]
        .filter((bd) => !band || band === bd)
        .map((bd) => {
          const list = all.filter((x) => x.band === bd);
          return (
            <Fragment key={bd}>
              <div className="pr-note" style={{ marginTop: 8 }}>
                <b style={{ color: "var(--text)" }}>{bandName[bd]}</b> · {list.length} BSS{radio && !chans(bd).length ? ` · ${radio.phy} doesn’t support this band` : ""}
              </div>
              <div className="wf-scroll">
                <OccupancyChart band={bd} list={list} radioChans={chans(bd)} />
              </div>
            </Fragment>
          );
        })}
      {!has6 && !band && <div className="pr-note">No 6 GHz chart: no radio here supports 6 GHz.</div>}
    </div>
  );
}

const COLS: [string, string, string?][] = [
  ["ssid", "SSID"],
  ["bssid", "BSSID", "wf-hide-sm"],
  ["channel", "Ch"],
  ["width", "Width"],
  ["signal", "Signal"],
  ["security", "Security"],
  ["std", "Std", "wf-hide-sm"],
  ["load", "Load", "wf-hide-sm"],
  ["seen", "Seen"],
];
const sortVal = (b: Bss, k: string): string | number =>
  k === "ssid" ? (b.hidden ? "￿" : b.ssid.toLowerCase()) : k === "channel" ? b.freq : k === "width" ? (b.width === "80+80" ? 160 : b.width) : k === "signal" ? b.signal ?? -200 : k === "security" ? b.security : k === "std" ? b.standards.length : k === "load" ? b.utilization ?? -1 : k === "seen" ? b.lastSeen : b.bssid;

function BssTable({ scan, band, group, hl, setHover, togglePin, onUse }: { scan: ScanResp; band: string; group: boolean; hl: string; setHover(b: string): void; togglePin(b: string): void; onUse(b: Bss): void }) {
  const [sort, setSort] = useState({ key: "signal", dir: -1 });
  const [open, setOpen] = useState<Set<string>>(new Set());
  const list = scan.bss.filter((x) => !band || x.band === band);
  const cmp = (a: Bss, c: Bss) => {
    const x = sortVal(a, sort.key),
      y = sortVal(c, sort.key);
    return (x < y ? -1 : x > y ? 1 : 0) * sort.dir;
  };
  list.sort(cmp);
  if (!list.length) return null;
  const row = (x: Bss, indent?: boolean) => {
    const age = Date.now() - x.lastSeen;
    const isOpen = open.has(x.bssid);
    return (
      <Fragment key={x.bssid}>
        <tr
          className={`wf-r${hl === x.bssid ? " hl" : ""}`}
          style={age > 120000 ? { opacity: 0.55 } : undefined}
          onPointerEnter={(e) => e.pointerType === "mouse" && setHover(x.bssid)}
          onClick={(e) => {
            if ((e.target as HTMLElement).closest("button")) return;
            setOpen((o) => {
              const n = new Set(o);
              n.has(x.bssid) ? n.delete(x.bssid) : n.add(x.bssid);
              return n;
            });
            togglePin(x.bssid);
          }}
        >
          <td>
            {indent ? "  " : ""}
            <span style={{ color: ssidColor(x) }}>●</span> {x.hidden ? <i className="wf-dim">hidden</i> : x.ssid}
            {x.associated && <span className="pr-pill on"> connected</span>}
          </td>
          <td className="wf-mono wf-hide-sm">
            {x.bssid}
            {x.vendor && (
              <>
                <br />
                <span className="wf-dim">{x.vendor}</span>
              </>
            )}
          </td>
          <td>
            {x.channel} <span className="wf-dim">{x.band}</span>
          </td>
          <td>
            {x.width}
            {x.width !== 20 && <span className="wf-dim"> @{x.centerChannel}</span>}
          </td>
          <td>
            <WSpark hist={x.history} /> <Sig dbm={x.signal} />
          </td>
          <td>
            <SecPill b={x} />
          </td>
          <td className="wf-hide-sm">{x.standards.join("/")}</td>
          <td className="wf-hide-sm">
            {x.stations != null ? `${x.stations} sta` : ""}
            {x.utilization != null ? ` · ${x.utilization}%` : ""}
          </td>
          <td className="wf-dim">{fmtAgo(x.lastSeen)}</td>
        </tr>
        {isOpen && (
          <tr className="wf-d">
            <td colSpan={9}>
              <BssDetail x={x} onUse={() => onUse(x)} />
            </td>
          </tr>
        )}
      </Fragment>
    );
  };
  let body: ReactNode[];
  if (group) {
    const groups = new Map<string, Bss[]>();
    for (const x of list) {
      const k = x.hidden ? "\u0000hidden" : x.ssid;
      if (!groups.has(k)) groups.set(k, []);
      groups.get(k)!.push(x);
    }
    body = [...groups.values()]
      .sort((a, c) => cmp(a[0], c[0]))
      .flatMap((g) => {
        const best = Math.max(...g.map((x) => x.signal ?? -200));
        return [
          <tr className="wf-g" key={"g" + g[0].bssid}>
            <td colSpan={9}>
              <span style={{ color: ssidColor(g[0]) }}>●</span> {g[0].hidden ? <i>hidden</i> : g[0].ssid}{" "}
              <span className="wf-dim">
                · {g.length} BSS · best {best} dBm · ch{" "}
                {[...new Set(g.map((x) => x.channel))].sort((a, c) => a - c).join(", ")} · {[...new Set(g.map((x) => x.security))].join(" / ")}
              </span>
            </td>
          </tr>,
          ...g.map((x) => row(x, true)),
        ];
      });
  } else body = list.map((x) => row(x));
  return (
    <div className="wf-table">
      <div className="wf-scroll">
        <table className="pr-tbl wf-tbl">
          <thead>
            <tr>
              {COLS.map(([k, t, cls]) => (
                <th key={k} className={`${cls || ""} ${sort.key === k ? "on" : ""}`} onClick={() => setSort((s) => ({ key: k, dir: s.key === k ? -s.dir : k === "ssid" || k === "security" ? 1 : -1 }))}>
                  {t}
                  {sort.key === k ? (sort.dir > 0 ? " ▲" : " ▼") : ""}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>{body}</tbody>
        </table>
      </div>
      <div className="pr-note">Tap a row for details (it also highlights the network in the chart above). Dashed humps are hidden SSIDs; thick ones are networks a radio is connected to; faded rows weren’t seen in the last 2 minutes.</div>
    </div>
  );
}

function BssDetail({ x, onUse }: { x: Bss; onUse(): void }) {
  const rows: ReactNode[] = [
    <>
      <b>BSSID</b> {x.bssid}
      {x.localMac ? " (locally administered: a virtual AP / extra SSID, so no vendor)" : ""}
      {x.vendor ? ` · ${x.vendor}` : ""}
    </>,
    <>
      <b>Frequency</b> {x.freq} MHz, channel {x.channel}, {x.width} MHz wide centred on {x.centerFreq} MHz (ch {x.centerChannel})
    </>,
    <>
      <b>Security</b> {x.security} · AKM {x.akm.join(", ") || "none"} · pairwise {x.pairwise.join(", ") || "-"} · group {x.group || "-"} · PMF {x.pmf}
    </>,
    <>
      <b>Standards</b> {x.standards.join(", ")}
      {x.generation ? ` (${x.generation})` : ""} · beacon {x.beaconInterval ?? "?"} TU · {x.capability.join(" ")}
    </>,
  ];
  if (x.stations != null || x.utilization != null)
    rows.push(
      <>
        <b>BSS load</b> {x.stations ?? "?"} stations, channel {x.utilization ?? "?"}% busy
      </>,
    );
  if (x.country)
    rows.push(
      <>
        <b>Country IE</b> {x.country}
      </>,
    );
  rows.push(
    x.wps ? (
      <>
        <b>WPS</b> {x.wps.state || "present"}
        {x.wps.locked ? ", AP setup locked" : ""}
        {x.wps.manufacturer ? ` · ${x.wps.manufacturer}` : ""}
        {x.wps.model ? ` ${x.wps.model}` : ""}
        {x.wps.device ? ` · “${x.wps.device}”` : ""}
      </>
    ) : (
      <>
        <b>WPS</b> not advertised
      </>
    ),
  );
  rows.push(
    <>
      <b>Seen</b> first {fmtAgo(x.firstSeen)}, last {fmtAgo(x.lastSeen)} by {x.radio} · {x.history.length} samples
      {x.history.length ? `, min ${Math.min(...x.history.map((h) => h[1]))} / max ${Math.max(...x.history.map((h) => h[1]))} dBm` : ""}
    </>,
  );
  if (x.ssidHex)
    rows.push(
      <>
        <b>Raw SSID</b> <span className="wf-mono">{x.ssidHex}</span>
      </>,
    );
  rows.push(
    <button className="btn btn-sm" onClick={onUse}>
      Test this network
    </button>,
  );
  return (
    <>
      {rows.map((r, i) => (
        <Fragment key={i}>
          {i > 0 && <br />}
          {r}
        </Fragment>
      ))}
    </>
  );
}

// ---------- client test ----------

interface Form {
  radio: string;
  profile: string;
  ssid: string;
  security: string;
  psk: string;
  pskPh: string;
  useSavedKey: boolean;
  eapMethod: string;
  eapIdentity: string;
  bssid: string;
  ip: string;
  sAddr: string;
  sGw: string;
  sDns: string;
  pingGateway: boolean;
  dns: string;
  http: string;
  ping: string;
  count: string;
  holdS: string;
  iperf: string;
  iperfS: string;
  iperfR: boolean;
  saveAs: string;
}
const EMPTY_FORM: Form = { radio: "", profile: "", ssid: "", security: "psk", psk: "", pskPh: "", useSavedKey: false, eapMethod: "PEAP", eapIdentity: "", bssid: "", ip: "dhcp", sAddr: "", sGw: "", sDns: "", pingGateway: true, dns: "example.com", http: "https://example.com/", ping: "", count: "5", holdS: "0", iperf: "", iperfS: "5", iperfR: false, saveAs: "" };

function ClientTest({ form: f, setForm, radios, scan, profiles, reloadProfiles }: { form: Form; setForm(fn: (f: Form) => Form): void; radios: RadiosResp | null; scan: ScanResp | null; profiles: Profile[]; reloadProfiles(): void }) {
  const b = useBob();
  const [busy, setBusy] = useState(false);
  const set = (p: Partial<Form>) => setForm((x) => ({ ...x, ...p }));
  const rs = radios?.radios || [];
  const radio = rs.some((r) => r.phy === f.radio) ? f.radio : (rs.find((r) => r.netns) || rs[0])?.phy || "";
  const ssids = useMemo(() => [...new Set((scan?.bss || []).filter((x) => !x.hidden).map((x) => x.ssid))].sort(), [scan]);
  const saved = (radios?.savedNetworks || []).some((n) => n.ssid === f.ssid);
  const sec = f.security;
  const showKey = sec !== "open" && sec !== "owe";
  const showNmKey = saved && showKey && sec !== "eap";
  const r = rs.find((x) => x.phy === radio);
  let warn: ReactNode = "";
  if (r && (r.use.kind === "backup" || r.use.kind === "connected")) warn = `${ifName(r)} is in use (${r.use.text}). The test moves it into its own network namespace, so that connection drops for the test (~15 s plus the test time). NetworkManager reconnects it afterwards, and BenchBob checks that it does.`;
  if (r && r.use.kind === "primary") warn = <span style={{ color: "var(--danger)" }}>{ifName(r)} is the primary uplink; BenchBob will refuse to borrow it.</span>;
  if (r && !r.onboard) warn = `BenchBob tells NetworkManager to leave ${ifName(r) || r.phy} alone while it uses it (runtime only).`;

  const onSsid = (v: string) => {
    // Pick the security the scan saw for this SSID (strongest BSS).
    const x = (scan?.bss || []).filter((y) => y.ssid === v).sort((a, c) => (c.signal ?? -200) - (a.signal ?? -200))[0];
    set({ ssid: v, ...(x && x.securityKind !== "wep" ? { security: x.securityKind } : {}) });
  };
  const loadProfile = (id: string) => {
    const p = profiles.find((x) => x.id === id);
    if (!p) return set({ profile: id });
    set({
      profile: id,
      ssid: p.ssid,
      security: p.security || "psk",
      bssid: p.bssid || "",
      ip: p.ip || "dhcp",
      pingGateway: p.pingGateway !== false,
      dns: (p.dns || []).join(", "),
      http: (p.http || []).join(", "),
      ping: (p.ping || []).join(", "),
      count: String(p.count || 5),
      holdS: String(p.holdS || 0),
      useSavedKey: !!p.useSavedKey,
      psk: "",
      pskPh: p.hasPsk ? "saved in profile" : "",
      ...(p.static ? { sAddr: p.static.address, sGw: p.static.gateway || "", sDns: (p.static.dns || []).join(", ") } : {}),
      ...(p.iperf ? { iperf: p.iperf.server + (p.iperf.port && p.iperf.port !== 5201 ? ":" + p.iperf.port : ""), iperfS: String(p.iperf.seconds ?? 5), iperfR: !!p.iperf.reverse } : {}),
    });
  };
  const readForm = () => {
    const list = (s: string) => s.split(/[\s,]+/).filter(Boolean);
    const o: Record<string, any> = { radio, ssid: f.ssid, security: sec, bssid: f.bssid || undefined, ip: f.ip, pingGateway: f.pingGateway, dns: list(f.dns), http: list(f.http), ping: list(f.ping), count: +f.count || 5, holdS: +f.holdS || 0 };
    if (f.profile) o.profile = f.profile;
    if (f.psk) o.psk = f.psk;
    if (f.useSavedKey && showNmKey) o.useSavedKey = true;
    if (sec === "eap") o.eap = { method: f.eapMethod, identity: f.eapIdentity };
    if (f.ip === "static") o.static = { address: f.sAddr, gateway: f.sGw || undefined, dns: list(f.sDns) };
    const ip = f.iperf.trim();
    if (ip) {
      const [server, port] = ip.split(":");
      o.iperf = { server, port: +port || 5201, seconds: +f.iperfS || 5, reverse: f.iperfR };
    }
    if (f.saveAs.trim()) o.saveAs = f.saveAs.trim();
    return o;
  };
  const run = async () => {
    setBusy(true);
    const o = readForm();
    try {
      await b.api("POST", "/api/wifi/test", o);
      set({ psk: "", saveAs: "" });
      if (o.saveAs) reloadProfiles();
    } catch {}
    setBusy(false);
  };
  const txt = (k: keyof Form, props: Record<string, unknown> = {}) => <input value={f[k] as string} onInput={(e) => set({ [k]: e.currentTarget.value } as Partial<Form>)} {...props} />;
  const chk = (k: keyof Form) => <input type="checkbox" checked={f[k] as boolean} onChange={(e) => set({ [k]: e.currentTarget.checked } as Partial<Form>)} />;
  return (
    <>
      <div className="wf-form">
        <label>
          Radio
          <select value={radio} onChange={(e) => set({ radio: e.currentTarget.value })}>
            {rs.map((x) => (
              <option key={x.phy} value={x.phy} disabled={!x.netns}>
                {x.label || x.phy} · {ifName(x) || "-"} · {x.use.kind}
              </option>
            ))}
          </select>
        </label>
        <label>
          Profile
          <select value={f.profile} onChange={(e) => loadProfile(e.currentTarget.value)}>
            <option value="">(none)</option>
            {profiles.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name} · {p.ssid}
                {p.hasPsk ? " · key saved" : p.useSavedKey ? " · NM key" : ""}
              </option>
            ))}
          </select>
        </label>
        <label>
          SSID
          <input list="wf-ssids" autoComplete="off" value={f.ssid} onInput={(e) => (ssids.includes(e.currentTarget.value) ? onSsid(e.currentTarget.value) : set({ ssid: e.currentTarget.value }))} onBlur={(e) => onSsid(e.currentTarget.value)} />
          <datalist id="wf-ssids">
            {ssids.map((s) => (
              <option key={s} value={s} />
            ))}
          </datalist>
        </label>
        <label>
          Security
          <select value={sec} onChange={(e) => set({ security: e.currentTarget.value })}>
            <option value="psk">WPA2-PSK</option>
            <option value="psk-sae">WPA2/WPA3 transition</option>
            <option value="sae">WPA3-SAE</option>
            <option value="open">Open</option>
            <option value="owe">OWE (Enhanced Open)</option>
            <option value="eap">Enterprise (PEAP/TTLS)</option>
          </select>
        </label>
        {showKey && <label className="wf-keyrow">Key / password{txt("psk", { type: "password", autoComplete: "off", placeholder: f.pskPh })}</label>}
        {showNmKey && (
          <label className="chk wf-nmkey">
            {chk("useSavedKey")} Use NetworkManager’s saved key
          </label>
        )}
        {sec === "eap" && (
          <>
            <label className="wf-eap">
              EAP method
              <select value={f.eapMethod} onChange={(e) => set({ eapMethod: e.currentTarget.value })}>
                <option>PEAP</option>
                <option>TTLS</option>
              </select>
            </label>
            <label className="wf-eap">Identity{txt("eapIdentity", { autoComplete: "off" })}</label>
          </>
        )}
        <label>Pin to BSSID (optional){txt("bssid", { placeholder: "aa:bb:cc:dd:ee:ff" })}</label>
        <label>
          Addressing
          <select value={f.ip} onChange={(e) => set({ ip: e.currentTarget.value })}>
            <option value="dhcp">DHCP</option>
            <option value="static">Static</option>
            <option value="none">None (associate only)</option>
          </select>
        </label>
        {f.ip === "static" && (
          <>
            <label className="wf-static">Address/prefix{txt("sAddr", { placeholder: "192.168.1.50/24" })}</label>
            <label className="wf-static">Gateway{txt("sGw")}</label>
            <label className="wf-static">DNS servers{txt("sDns", { placeholder: "1.1.1.1" })}</label>
          </>
        )}
        <label className="chk">{chk("pingGateway")} Ping the gateway</label>
        <label>Resolve (DNS){txt("dns")}</label>
        <label>Fetch URLs{txt("http")}</label>
        <label>Ping hosts{txt("ping", { placeholder: "1.1.1.1, 8.8.8.8" })}</label>
        <label>Pings per target{txt("count", { type: "number", min: 1, max: 50 })}</label>
        <label>Stay connected afterwards (s){txt("holdS", { type: "number", min: 0, max: 600 })}</label>
        <label>iperf3 server[:port] (optional){txt("iperf", { placeholder: "10.0.0.2" })}</label>
        <label>iperf3 seconds{txt("iperfS", { type: "number", min: 1, max: 60 })}</label>
        <label className="chk">{chk("iperfR")} iperf3 download (-R)</label>
        <label>Save as profile (name){txt("saveAs", { placeholder: "optional" })}</label>
      </div>
      <div className="pr-note wf-warn" style={{ marginTop: 6 }}>
        {warn}
      </div>
      <div className="pr-row">
        <button className="btn btn-primary" disabled={busy} onClick={run}>
          Run test
        </button>
        <button
          className="btn btn-sm"
          onClick={async () => {
            if (!f.profile) return;
            try {
              await b.api("DELETE", `/api/wifi/profiles/${f.profile}`);
              set({ profile: "" });
              reloadProfiles();
            } catch {}
          }}
        >
          Delete selected profile
        </button>
      </div>
    </>
  );
}

function Timeline({ steps }: { steps: Step[] }) {
  return (
    <div className="wf-tl">
      {steps.map((s, i) => (
        <Fragment key={i}>
          <span className="t">+{secs(s.dt)}</span>
          <span className={s.ok === true ? "ok" : s.ok === false ? "bad" : "wf-dim"}>{s.ok === true ? "✓" : s.ok === false ? "✗" : "·"}</span>
          <span>{s.text}</span>
        </Fragment>
      ))}
    </div>
  );
}

function RunningTests({ st }: { st: WifiState }) {
  const b = useBob();
  return (
    <div className="wf-running">
      {(st.running || []).map((t) => (
        <div className="pr-card" key={t.id}>
          <h4>
            Testing {t.ssid} on {t.phy}{" "}
            <small>
              {t.stage} · started {fmtAgo(t.startedAt)}
            </small>
            {t.link?.signal != null && (
              <small>
                <WSpark hist={t.samples.map((x) => x.signal).filter((x): x is number => x != null)} /> <Sig dbm={t.link.signal} /> dBm · tx {t.link.txRate || "?"} · rx {t.link.rxRate || "?"}
              </small>
            )}
            <button className="btn btn-sm btn-danger" onClick={() => b.api("POST", `/api/wifi/test/${t.id}/cancel`).catch(() => {})}>
              Cancel
            </button>
          </h4>
          <Timeline steps={t.timeline} />
        </div>
      ))}
    </div>
  );
}

function Results({ results: R }: { results: TestResult[] }) {
  const b = useBob();
  const [open, setOpen] = useState<Set<string>>(new Set());
  if (!R.length) return <div className="wf-results pr-note">No tests yet.</div>;
  return (
    <div className="wf-results">
      {R.map((r) => {
        const gw = r.tests.find((t) => t.kind === "ping-gateway");
        const isOpen = open.has(r.id);
        const sig = r.samples.map((x) => x.signal).filter((x): x is number => x != null);
        return (
          <div className="wf-res" key={r.id}>
            <div
              className="hd"
              role="button"
              onClick={() =>
                setOpen((o) => {
                  const n = new Set(o);
                  n.has(r.id) ? n.delete(r.id) : n.add(r.id);
                  return n;
                })
              }
            >
              <span className={`pr-pill ${r.ok ? "on" : "bad"}`}>{r.ok ? "pass" : r.cancelled ? "cancelled" : "fail"}</span>
              <b>{r.ssid}</b>
              <span className="wf-dim">
                {r.phy} · {r.security} · {fmtAgo(r.startedAt)} · {secs((r.endedAt || r.startedAt) - r.startedAt)}
              </span>
              {r.link?.signal != null && (
                <span>
                  <Sig dbm={r.link.signal} /> dBm
                </span>
              )}
              {r.address && <span className="wf-mono">{r.address}</span>}
              {gw?.data?.avg != null && <span className="wf-dim">gw {gw.data.avg} ms</span>}
              <span className="wf-dim">
                {r.tests.filter((t) => t.ok).length}/{r.tests.length} tests
              </span>
              {r.error && <span style={{ color: "var(--danger)" }}>{r.error}</span>}
              {r.restore && !r.restore.ok && <span style={{ color: "var(--warning)" }}>{r.restore.text}</span>}
            </div>
            {isOpen && (
              <>
                {r.link && (
                  <div className="pr-note">
                    Link: {r.link.bssid || ""} {r.link.freq || ""} MHz · tx {r.link.txRate || "?"} · rx {r.link.rxRate || "?"}{" "}
                    {r.samples.length > 1 && (
                      <>
                        · RSSI over the test <WSpark hist={sig} w={90} />
                      </>
                    )}
                  </div>
                )}
                {r.lease && (
                  <div className="pr-note">
                    Lease:{" "}
                    {Object.entries(r.lease)
                      .filter(([, v]) => v)
                      .map(([k, v]) => `${k} ${v}`)
                      .join(" · ")}
                  </div>
                )}
                {r.restore && (
                  <div className="pr-note">
                    Afterwards: {r.restore.text}; main default route {r.uplinkBefore || "?"} → {r.uplinkAfter || "?"}
                  </div>
                )}
                <Timeline steps={r.timeline} />
                {r.wpaLog?.length ? (
                  <details>
                    <summary>wpa_supplicant log ({r.wpaLog.length} lines, keys redacted)</summary>
                    <div className="pr-log">{r.wpaLog.join("\n")}</div>
                  </details>
                ) : null}
              </>
            )}
          </div>
        );
      })}
      <div className="pr-row">
        <button className="btn btn-sm" onClick={() => b.api("DELETE", "/api/wifi/results").catch(() => {})}>
          Clear results
        </button>
      </div>
    </div>
  );
}
