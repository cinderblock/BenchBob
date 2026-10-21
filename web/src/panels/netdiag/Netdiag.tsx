// Wired network section: per-interface link card (with the switch port it's plugged into),
// iperf3 server/client with a live throughput graph, and quick checks (preset hosts and
// one-click test profiles for both). The forms hold their own state, so the 2 s status
// push never disturbs what is being typed.

import { useState, type ReactNode } from "react";
import { useBob, useSnapshot } from "../../bob/context";
import type { PanelProps } from "../../registry";
import { fmtAgo } from "../../lib/format";
import { readLocal, writeLocal } from "../../lib/storage";
import { AsyncButton, Hint } from "../../ui/common";
import { ActionLine, useAction } from "../../ui/status";
import type { AuthEntry, Checks, IperfForm, IperfRun, IperfServer, NdIface, NetdiagState, Neighbor, Preset } from "./api";

const fmtBps = (b: number) => {
  if (!b) return "0 bit/s";
  const u = ["bit/s", "kbit/s", "Mbit/s", "Gbit/s"];
  const i = Math.min(Math.floor(Math.log10(b) / 3), 3);
  const v = b / 10 ** (i * 3);
  return `${v.toFixed(i ? (v < 10 ? 2 : 1) : 0)} ${u[i]}`;
};
const Pill = ({ t, cls = "" }: { t: ReactNode; cls?: string }) => <span className={`pr-pill ${cls}`}>{t}</span>;
const sp = (s?: number | null) => (s == null ? "" : s >= 1000 ? `${s / 1000} Gb/s` : `${s} Mb/s`);
// "1000baseT/Full 100baseT/Full" → "1000/Full 100/Full": shorter, same meaning on a phone.
const modes = (m?: string[]) => (m?.length ? m.map((x) => x.replace("baseT", "").replace(/base(\w+)/, " $1")).join(" · ") : "—");

// One-click iperf3 tests for a bench Pi 4 on gigabit: each direction alone, several streams
// (one TCP flow can be window- or CPU-bound), and UDP at a rate a link should carry.
const PROFILES: { label: string; o: Partial<IperfForm> }[] = [
  { label: "TCP ↑ 10 s", o: { duration: 10, reverse: false, udp: false, parallel: 1, bitrate: "" } },
  { label: "TCP ↓ 10 s", o: { duration: 10, reverse: true, udp: false, parallel: 1, bitrate: "" } },
  { label: "TCP ↓ ×4 streams", o: { duration: 10, reverse: true, udp: false, parallel: 4, bitrate: "" } },
  { label: "UDP 100M ↑", o: { duration: 10, reverse: false, udp: true, parallel: 1, bitrate: "100M" } },
  { label: "UDP 100M ↓", o: { duration: 10, reverse: true, udp: true, parallel: 1, bitrate: "100M" } },
];

function NeighborInfo({ n, lldp }: { n?: Neighbor | null; lldp: NetdiagState["lldp"] }) {
  if (!lldp.enabled) return <div className="pr-note">Switch info: listening is off.</div>;
  if (!n) return <div className="pr-note">Switch info: {lldp.running ? `listening… (${Math.round((Date.now() - lldp.since) / 1000)} s; switches announce about every 30 s)` : lldp.error || "lldpd not running"}</div>;
  const vl = n.vlans.map((v) => `${v.id}${v.name ? ` ${v.name}` : ""}${v.pvid ? " (untagged)" : ""}`).join(", ");
  const caps = n.capabilities
    .filter((c) => c.enabled)
    .map((c) => c.type)
    .join(", ");
  const poe = n.poe
    ? Object.entries(n.poe)
        .map(([k, v]) => `${k} ${v}`)
        .join(" · ")
    : "";
  const rows = (
    [
      ["Switch", `${n.name || ""}${n.description ? ` — ${n.description}` : ""}`],
      ["Port", `${n.portDescription || n.portId}${n.portDescription && n.portId !== n.portDescription ? ` (id ${n.portId})` : ""}`],
      ["Chassis", `${n.chassisId} (${n.chassisIdType})`],
      ["VLAN", vl],
      ["PoE", poe],
      ["Management", n.mgmtIps.join(", ")],
      ["Capabilities", caps],
      ["Heard via", `${n.protocol} · ${n.age.replace(/^0 day, /, "")} ago-ish · TTL ${n.ttl ?? "?"} s${n.medDeviceType ? ` · ${n.medDeviceType}` : ""}`],
    ] as [string, string][]
  ).filter((r) => r[1]);
  return (
    <table className="pr-tbl">
      <tbody>
        {rows.map((r) => (
          <tr key={r[0]}>
            <td className="pr-note" style={{ width: "7.5em" }}>
              {r[0]}
            </td>
            <td>{r[1]}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function IfaceCard({ i, st }: { i: NdIface; st: NetdiagState }) {
  const l = i.link || {};
  const eee = i.eee ? (i.eee.supported ? `${i.eee.enabled ? "enabled" : "disabled"}, ${i.eee.active ? "active" : "inactive"}${i.eee.partner.length ? "" : " (partner advertises none)"}` : "not supported") : "—";
  const addrs =
    i.addrs
      .filter((a) => a.family === "inet" || !a.addr.startsWith("fe80"))
      .map((a) => `${a.addr}/${a.prefix}`)
      .join(", ") || "—";
  const row = (k: string, v: ReactNode, w?: boolean) => (
    <tr>
      <td className="pr-note" style={w ? { width: "7.5em" } : undefined}>
        {k}
      </td>
      <td>{v}</td>
    </tr>
  );
  return (
    <div className="pr-card">
      <h4>
        {i.name}{" "}
        {i.carrier ? (
          <>
            <Pill t={`${sp(l.speed)} ${l.duplex || ""}`.trim() || "link up"} cls="on" /> {l.autoneg === false && <Pill t="autoneg off" cls="warn" />} {l.duplex === "Half" && <Pill t="half duplex" cls="warn" />}
          </>
        ) : (
          <Pill t="no link" cls="bad" />
        )}{" "}
        {i.usb && <Pill t="USB" />}
        <small>{i.usbDesc}</small>
      </h4>
      <table className="pr-tbl">
        <tbody>
          {row("Addresses", addrs, true)}
          {row("Gateway / DNS", `${i.gateway || "—"} / ${(i.dns.length ? i.dns : st.dns.nameservers).join(", ") || "—"}`)}
          {row("Partner offers", `${modes(l.partner)}${l.partnerAutoneg === false ? " (no autoneg)" : ""}`)}
          {row("We advertise", modes(l.advertised))}
          {row("EEE", eee)}
          {row("Hardware", [i.driver, i.firmware && `fw ${i.firmware}`, i.bus, i.mac, `MTU ${i.mtu}`].filter(Boolean).join(" · "))}
          {row("Carrier changes", `${i.carrierChanges}${l.masterSlave ? ` · PHY ${l.masterSlave}` : ""}`)}
        </tbody>
      </table>
      {i.errors.length ? (
        <table className="pr-tbl">
          <tbody>
            <tr>
              <th>Error counter</th>
              <th>Total</th>
              <th>Since BenchBob started</th>
            </tr>
            {i.errors.map((e) => (
              <tr key={e.name}>
                <td>{e.name}</td>
                <td>{e.value}</td>
                <td>{e.delta ? <span style={{ color: "var(--warning)" }}>+{e.delta}</span> : "0"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : (
        <div className="pr-note">No error counters above zero.</div>
      )}
      <div style={{ marginTop: 8 }}>
        <NeighborInfo n={i.neighbor} lldp={st.lldp} />
      </div>
    </div>
  );
}

// Throughput over time, filled area + line, with the peak labelled.
function Graph({ points, duration }: { points: { t: number; bps: number }[]; duration: number }) {
  const w = 320,
    h = 90;
  if (!points.length) return <svg viewBox={`0 0 ${w} ${h}`} width="100%" height={h} />;
  const max = Math.max(...points.map((p) => p.bps), 1);
  const tmax = Math.max(duration, points[points.length - 1].t);
  const x = (t: number) => (t / tmax) * (w - 2) + 1,
    y = (b: number) => h - 14 - (b / max) * (h - 24);
  const pts: [number, number][] = [[0, points[0].bps], ...points.map((p) => [p.t, p.bps] as [number, number])];
  const line = pts.map(([t, b]) => `${x(t).toFixed(1)},${y(b).toFixed(1)}`).join(" ");
  return (
    <svg viewBox={`0 0 ${w} ${h}`} width="100%" height={h} preserveAspectRatio="none" style={{ display: "block", background: "var(--surface3)", borderRadius: 4 }}>
      <polygon points={`${x(0)},${h - 14} ${line} ${x(pts[pts.length - 1][0])},${h - 14}`} fill="var(--accent)" opacity={0.18} />
      <polyline points={line} fill="none" stroke="var(--accent)" strokeWidth={1.5} vectorEffect="non-scaling-stroke" />
      <text x={4} y={11} fontSize={9} fill="var(--dim)">
        peak {fmtBps(max)}
      </text>
      <text x={w - 4} y={h - 3} fontSize={9} fill="var(--dim)" textAnchor="end">
        {tmax.toFixed(0)} s
      </text>
    </svg>
  );
}

function Runs({ runs }: { runs: IperfRun[] }) {
  const b = useBob();
  // Stop disappears when the run ends, so its status sits below the run, not beside it.
  const stop = useAction();
  if (!runs.length) return <div className="pr-note">No client runs yet.</div>;
  const cur = runs[0];
  const last = cur.points[cur.points.length - 1];
  const res = (r: IperfRun) =>
    r.result
      ? `${fmtBps(r.result.bps)}${r.result.retransmits != null ? ` · ${r.result.retransmits} retr` : ""}${r.result.jitterMs != null ? ` · jitter ${r.result.jitterMs.toFixed(2)} ms · loss ${r.result.lostPct}%` : ""}`
      : r.status === "running"
        ? "…"
        : r.error;
  return (
    <>
      {cur.status === "running" ? (
        <div className="pr-row">
          <Pill t="running" cls="on" /> {cur.host} {last ? fmtBps(last.bps) : "connecting…"}{" "}
          <button className="btn btn-sm" disabled={stop.busy} onClick={() => stop.run(() => b.api("POST", `/api/netdiag/iperf/${cur.id}/stop`), { busy: "Stopping…" })}>
            Stop
          </button>
        </div>
      ) : cur.status === "error" ? (
        <div className="pr-note" style={{ color: "var(--danger)" }}>
          {cur.host}: {cur.error}
        </div>
      ) : null}
      <ActionLine a={stop} />
      <Graph points={cur.points} duration={cur.duration} />
      <table className="pr-tbl" style={{ marginTop: 6 }}>
        <tbody>
          <tr>
            <th>When</th>
            <th>Host</th>
            <th>Test</th>
            <th>Result</th>
          </tr>
          {runs.map((r) => (
            <tr key={r.id}>
              <td>{fmtAgo(r.started)}</td>
              <td>
                {r.host}:{r.port}
              </td>
              <td>
                {r.udp ? "UDP" : "TCP"}
                {r.reverse ? " ↓ (reverse)" : " ↑"} {r.duration}s{r.parallel > 1 ? ` ×${r.parallel}` : ""}
                {r.bitrate ? ` @${r.bitrate}` : ""}
              </td>
              <td>{res(r)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <div className="pr-note">↑ BenchBob sends · ↓ BenchBob receives</div>
    </>
  );
}

function Server({ s }: { s: IperfServer }) {
  const { conn } = useBob();
  const off = s.offAt ? ` · turns off in ${Math.max(0, Math.round((s.offAt - Date.now()) / 60000))} min` : "";
  return (
    <>
      <div className="pr-row">
        {s.running ? (
          <>
            <Pill t="listening" cls="on" /> on port {s.port}: run{" "}
            <span className="pr-endpoint">
              iperf3 -c {conn.hostname} -p {s.port}
            </span>{" "}
            from another machine{off}
          </>
        ) : (
          <span className="pr-note">Off{s.log.length ? ` (${s.log[s.log.length - 1]})` : ""}</span>
        )}
      </div>
      {s.current && (
        <div className="pr-row">
          <Pill t="test running" cls="on" /> from {s.current.peer} · {s.current.protocol}
          {s.current.reverse ? " reverse" : ""} · {fmtBps(s.current.bps)}
        </div>
      )}
      {s.tests.length > 0 && (
        <table className="pr-tbl">
          <tbody>
            <tr>
              <th>When</th>
              <th>Client</th>
              <th>Test</th>
              <th>Result</th>
            </tr>
            {s.tests.map((t, i) => (
              <tr key={i}>
                <td>{fmtAgo(t.t)}</td>
                <td>{t.peer}</td>
                <td>{t.summary ? `${t.summary.protocol}${t.summary.reverse ? " (we sent)" : " (we received)"}` : ""}</td>
                <td>
                  {t.summary
                    ? `${fmtBps(t.summary.bps)}${t.summary.retransmits != null ? ` · ${t.summary.retransmits} retr` : ""}${t.summary.jitterMs != null ? ` · jitter ${t.summary.jitterMs.toFixed(2)} ms · loss ${t.summary.lostPct}%` : ""}`
                    : t.error}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </>
  );
}

function ChecksView({ c, targets }: { c: Checks; targets: Preset[] }) {
  const pings = c.pings?.length ? c.pings : c.ping ? [c.ping] : [];
  const name = (p: { host: string; target?: string }) => {
    const t = targets.find((x) => x.host === (p.target || p.host));
    return t && t.label !== p.host ? (
      <>
        {t.label} <span className="pr-note">{p.host}</span>
      </>
    ) : (
      p.host
    );
  };
  if (!pings.length && !c.dns && !c.http && !c.mtu) return <div className="pr-note">Run a check above.</div>;
  return (
    <>
      {pings.length > 0 && (
        <div>
          <b>Ping</b>
          <table className="pr-tbl">
            <tbody>
              <tr>
                <th>Target</th>
                <th>Replies</th>
                <th>min / avg / max</th>
                <th>Jitter</th>
                <th>When</th>
              </tr>
              {pings.map((p, i) => (
                <tr key={i}>
                  <td>{name(p)}</td>
                  <td style={p.lossPct ? { color: "var(--warning)" } : undefined}>
                    {p.received}/{p.sent}
                    {p.lossPct ? ` (${p.lossPct}% loss)` : ""}
                  </td>
                  <td>
                    {p.avg != null ? (
                      <>
                        {p.min} / <b>{p.avg}</b> / {p.max} ms
                      </>
                    ) : (
                      "—"
                    )}
                  </td>
                  <td>{p.mdev != null ? `${p.mdev} ms` : ""}</td>
                  <td className="pr-note">{fmtAgo(p.t)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {c.dns && (
        <div style={{ marginTop: 6 }}>
          <b>DNS</b> <span className="pr-note">{fmtAgo(c.dns.t)}</span>
          <table className="pr-tbl">
            <tbody>
              {c.dns.results.map((r, i) => (
                <tr key={i}>
                  <td>{r.server}</td>
                  <td>{r.name}</td>
                  <td>{r.ms != null ? `${r.ms} ms` : <span style={{ color: "var(--danger)" }}>{r.error}</span>}</td>
                  <td className="pr-note">{r.addresses.slice(0, 2).join(", ")}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {c.http && (
        <div style={{ marginTop: 6 }}>
          <b>HTTP</b> <span className="pr-note">{fmtAgo(c.http.t)}</span>
          <table className="pr-tbl">
            <tbody>
              {c.http.results.map((r, i) => (
                <tr key={i}>
                  <td style={{ wordBreak: "break-all" }}>{r.url}</td>
                  <td>{r.status != null ? r.status : <span style={{ color: "var(--danger)" }}>{r.error}</span>}</td>
                  <td>{r.ms != null ? `${r.ms} ms` : ""}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {c.mtu && (
        <div className="pr-row">
          <b>Path MTU to {c.mtu.host}</b> {c.mtu.pathMtu ?? "?"} (interface {c.mtu.interfaceMtu}) · {c.mtu.note}{" "}
          <span className="pr-note">
            {c.mtu.steps.length} probes, {fmtAgo(c.mtu.t)}
          </span>
        </div>
      )}
    </>
  );
}

/** A quick-pick host chip that pings (or fills a field) without clobbering its two-line label. */
function PresetChip({ p, onClick }: { p: Preset; onClick(): Promise<unknown> | void }) {
  const a = useAction();
  return (
    <>
      <button className="pr-tool" disabled={a.busy} style={a.busy ? { opacity: 0.5 } : undefined} onClick={() => a.run(async () => onClick(), { busy: "" })}>
        {p.label}
        {p.label !== p.host && <small>{p.host}</small>}
      </button>
      {a.status?.state === "error" && <ActionLine a={a} />}
    </>
  );
}

export default function Netdiag({ st }: PanelProps<NetdiagState>) {
  const b = useBob();
  const formKey = b.key("ndIperf");
  const [form, setFormState] = useState<IperfForm>(() => {
    const saved = readLocal<Partial<IperfForm>>(formKey, {}) || {};
    return { port: 5201, duration: 10, reverse: false, udp: false, bitrate: "", parallel: 1, ...saved, host: saved.host || st?.presets?.iperf?.[0]?.host || "" };
  });
  const set = (p: Partial<IperfForm>) => setFormState((f) => ({ ...f, ...p }));
  const [sport, setSport] = useState(() => String(st?.iperf?.server?.port || 5201));
  const [chost, setChost] = useState("");
  const P = st.presets || { iperf: [], ping: [] };
  const locked = new Set((P.auth || []).filter((a) => a.ready).map((a) => a.host));

  const run = (f: IperfForm) => {
    writeLocal(formKey, JSON.stringify(f));
    return b.api("POST", "/api/netdiag/iperf", { ...f, bitrate: f.bitrate || undefined });
  };
  const check = (kind: string) => b.api("POST", `/api/netdiag/${kind}`, kind === "ping" || kind === "mtu" ? { host: chost.trim() || undefined } : {});
  // Start/Stop swap places, so the server's status belongs to the card.
  const srv = useAction();
  const server = (on: boolean) => srv.run(() => b.api("POST", "/api/netdiag/iperf/server", { enabled: on, port: Number(sport) }), { busy: on ? "Starting…" : "Stopping…" });
  const lldp = useAction();
  const num = (v: string) => Number(v);

  return (
    <>
      {st.ifaces.length ? st.ifaces.map((i) => <IfaceCard key={i.name} i={i} st={st} />) : <div className="pr-note">No wired interface found.</div>}
      <div className="pr-row">
        <label>
          <input type="checkbox" checked={st.lldp.enabled} disabled={lldp.busy} onChange={(e) => lldp.run(() => b.api("POST", "/api/netdiag/lldp", { enabled: e.currentTarget.checked }), { busy: "Saving…" })} /> Listen for switch info (LLDP/CDP, receive-only: BenchBob never announces itself)
        </label>
        <ActionLine a={lldp} />
      </div>
      <div className="pr-card">
        <h4>
          iperf3 client <small>from BenchBob</small>
        </h4>
        <div className="pr-row">
          {P.iperf.map((p) => (
            <PresetChip key={p.host} p={locked.has(p.host) ? { ...p, label: `🔒 ${p.label}` } : p} onClick={() => set({ host: p.host })} />
          ))}
          <button className="btn btn-sm" onClick={() => b.openModal(<PresetsDialog />)}>
            Edit hosts…
          </button>
          <button className="btn btn-sm" onClick={() => b.openModal(<LoginsDialog />, { cls: "wide" })}>
            Logins…
          </button>
        </div>
        <div className="pr-row">
          <label>
            Host <input placeholder="server IP" style={{ width: "10em" }} value={form.host} onInput={(e) => set({ host: e.currentTarget.value })} />
          </label>
          <label>
            Port <input type="number" style={{ width: "5em" }} value={form.port} onInput={(e) => set({ port: num(e.currentTarget.value) })} />
          </label>
          <label>
            Seconds <input type="number" min={1} max={60} style={{ width: "4em" }} value={form.duration} onInput={(e) => set({ duration: num(e.currentTarget.value) })} />
          </label>
          <label>
            Streams <input type="number" min={1} max={16} style={{ width: "3.5em" }} value={form.parallel} onInput={(e) => set({ parallel: num(e.currentTarget.value) })} />
          </label>
        </div>
        <div className="pr-row">
          <label>
            <input type="checkbox" checked={form.reverse} onChange={(e) => set({ reverse: e.currentTarget.checked })} /> Reverse (server sends)
          </label>
          <label>
            <input type="checkbox" checked={form.udp} onChange={(e) => set({ udp: e.currentTarget.checked })} /> UDP
          </label>
          <label>
            Bitrate <input placeholder="e.g. 100M" style={{ width: "6em" }} value={form.bitrate} onInput={(e) => set({ bitrate: e.currentTarget.value })} />
          </label>
          <AsyncButton className="btn btn-sm btn-primary" busyText="Run…" onClick={() => run({ ...form, host: form.host.trim(), bitrate: form.bitrate.trim() })}>
            Run
          </AsyncButton>
        </div>
        <div className="pr-row">
          <span className="pr-note">Quick test:</span>
          {PROFILES.map((p) => (
            <AsyncButton
              key={p.label}
              busyText={p.label + "…"}
              onClick={() => {
                const f = { ...form, ...p.o, host: form.host.trim() };
                setFormState(f);
                return run(f);
              }}
            >
              {p.label}
            </AsyncButton>
          ))}
        </div>
        <div className="pr-note">
          The server end needs <span className="pr-endpoint">iperf3 -s</span> running (port 5201). To test from another machine instead, start BenchBob's server below.
        </div>
        <Runs runs={st.iperf.runs} />
      </div>
      <div className="pr-card">
        <h4>
          iperf3 server <small>temporary; turns itself off</small>
        </h4>
        <div className="pr-row">
          <label>
            Port <input type="number" style={{ width: "5em" }} value={sport} onInput={(e) => setSport(e.currentTarget.value)} />
          </label>
          {st.iperf.server.running ? (
            <button className="btn btn-sm" disabled={srv.busy} onClick={() => server(false)}>
              Stop
            </button>
          ) : (
            <button className="btn btn-sm" disabled={srv.busy} onClick={() => server(true)}>
              Start
            </button>
          )}
          <ActionLine a={srv} />
        </div>
        <Server s={st.iperf.server} />
      </div>
      <div className="pr-card">
        <h4>Quick checks</h4>
        <div className="pr-row">
          <span className="pr-note">Ping:</span>
          {P.ping.map((p) => (
            <PresetChip key={p.host} p={p} onClick={() => b.api("POST", "/api/netdiag/ping", { host: p.host })} />
          ))}
        </div>
        <div className="pr-row">
          <label>
            Host <input placeholder="gateway / 1.1.1.1" style={{ width: "10em" }} value={chost} onInput={(e) => setChost(e.currentTarget.value)} />
          </label>
          <AsyncButton busyText="Ping…" onClick={() => check("ping")}>
            Ping
          </AsyncButton>
          <AsyncButton busyText="DNS timing…" onClick={() => check("dns")}>
            DNS timing
          </AsyncButton>
          <AsyncButton busyText="HTTP(S)…" onClick={() => check("http")}>
            HTTP(S)
          </AsyncButton>
          <AsyncButton busyText="Path MTU…" onClick={() => check("mtu")}>
            Path MTU
          </AsyncButton>
        </div>
        <div className="pr-note">Pings are 10 at 0.2 s; an empty host pings the gateway. Path MTU defaults to 1.1.1.1.</div>
        <ChecksView c={st.checks} targets={P.ping} />
      </div>
    </>
  );
}

const TA = { width: "100%", minHeight: 110, background: "var(--surface2)", border: "1px solid var(--border)", borderRadius: 5, padding: "7px 10px", color: "var(--text)", fontSize: ".8rem", fontFamily: "'SF Mono',ui-monospace,monospace", resize: "vertical" } as const;

function usePresets() {
  return (useSnapshot().status?.peripherals?.state?.netdiag as NetdiagState | undefined)?.presets || { iperf: [], ping: [], auth: [] };
}

function PresetsDialog() {
  const b = useBob();
  const P = usePresets();
  const fmt = (l: Preset[]) => l.map((p) => (p.label && p.label !== p.host ? `${p.host} ${p.label}` : p.host)).join("\n");
  const [iperf, setIperf] = useState(() => fmt(P.iperf));
  const [ping, setPing] = useState(() => fmt(P.ping));
  const a = useAction();
  const save = async (body: unknown) => {
    if (await a.run(() => b.api("PUT", "/api/netdiag/presets", body), { busy: "Saving…" })) b.closeModal();
  };
  return (
    <>
      <h3>Quick-pick hosts</h3>
      <p className="pr-note">
        One per line: host, then an optional label. In ping targets, <b>gateway</b> means the default gateway. The iperf3 hosts need an iperf3 server running (<span className="pr-endpoint">iperf3 -s</span>).
      </p>
      <label className="pr-note" htmlFor="nd-pi">
        iperf3 servers
      </label>
      <textarea id="nd-pi" spellCheck={false} style={TA} value={iperf} onInput={(e) => setIperf(e.currentTarget.value)} />
      <label className="pr-note" htmlFor="nd-pp" style={{ display: "block", marginTop: 10 }}>
        Ping targets
      </label>
      <textarea id="nd-pp" spellCheck={false} style={TA} value={ping} onInput={(e) => setPing(e.currentTarget.value)} />
      <ActionLine a={a} />
      <div className="modal-btns">
        <button className="btn" disabled={a.busy} onClick={() => save({ reset: true })}>
          Restore defaults
        </button>
        <button className="btn" onClick={b.closeModal}>
          Cancel
        </button>
        <button className="btn btn-primary" disabled={a.busy} onClick={() => save({ iperf, ping })}>
          Save
        </button>
      </div>
    </>
  );
}

// Logins for iperf3 servers that require one. Passwords go in, never come back out: the
// dialog shows whether one is set and the users-file line (a hash) to put on the server.
function LoginsDialog() {
  const b = useBob();
  const live = usePresets();
  // A save fetches the driver's state right away rather than waiting for the next push.
  const [fresh, setFresh] = useState<NetdiagState["presets"] | null>(null);
  const P = fresh || live;
  const auth = Object.fromEntries((P.auth || []).map((a) => [a.host, a]));
  const hosts = [...new Set([...P.iperf.map((p) => p.host), ...Object.keys(auth)])];
  const load = useAction();
  const refresh = async () => {
    const s = await load.run(() => b.api<NetdiagState>("GET", "/api/netdiag"), { busy: "Refreshing…" });
    if (s) setFresh(s.presets || null);
  };
  return (
    <>
      <h3>iperf3 logins</h3>
      <p className="pr-note">
        For servers started with <span className="pr-endpoint">--authorized-users-path</span>. BenchBob keeps the password and never shows it again; put the users-file line on the server, and paste the server's public key here.
      </p>
      {hosts.length ? hosts.map((h) => <LoginCard key={h + (auth[h]?.usersLine || "") + (auth[h]?.hasKey ? "k" : "")} host={h} a={auth[h]} onSaved={refresh} />) : <p className="pr-note">No iperf3 hosts.</p>}
      <ActionLine a={load} time={false} />
      <div className="modal-btns">
        <button className="btn" onClick={b.closeModal}>
          Close
        </button>
      </div>
    </>
  );
}

function LoginCard({ host, a, onSaved }: { host: string; a?: AuthEntry; onSaved(): void }) {
  const b = useBob();
  const [user, setUser] = useState(a?.username || "benchbob");
  const [pw, setPw] = useState("");
  const [pkcs1, setPkcs1] = useState(!!a?.pkcs1);
  const [key, setKey] = useState("");
  const inp = { background: "var(--surface2)", border: "1px solid var(--border)", borderRadius: 5, padding: "5px 8px", color: "var(--text)", fontSize: ".8rem" } as const;
  // A save that changes the users line or key re-keys (remounts) the card, dropping this line;
  // the card's new state shows the result then.
  const st = useAction();
  const save = async (act: "save" | "gen" | "del") => {
    const ok = await st.run(
      () =>
        act === "del"
          ? b.api("DELETE", "/api/netdiag/iperf/auth", { host })
          : b.api("PUT", "/api/netdiag/iperf/auth", {
              host,
              username: user.trim(),
              pkcs1,
              ...(act === "gen" ? { generate: true } : pw.trim() ? { password: pw } : {}),
              ...(key.trim() ? { publicKey: key.trim() } : {}),
            }),
      { busy: act === "del" ? "Forgetting…" : "Saving…", ok: act === "del" ? "Forgotten" : "Saved" },
    );
    if (ok) onSaved();
  };
  const status = !a ? <span className="pr-pill">no login</span> : a.ready ? <span className="pr-pill on">ready</span> : <span className="pr-pill warn">needs {[!a.hasPassword && "password", !a.hasKey && "public key"].filter(Boolean).join(" + ")}</span>;
  return (
    <div className="pr-card">
      <h4>
        {host} {status}
      </h4>
      <div className="pr-row">
        <label>
          User <input value={user} onInput={(e) => setUser(e.currentTarget.value)} style={inp} size={10} />
        </label>
        <label>
          Password <input type="password" autoComplete="new-password" placeholder={a?.hasPassword ? "(unchanged)" : ""} value={pw} onInput={(e) => setPw(e.currentTarget.value)} style={inp} size={14} />
        </label>
        <button className="btn btn-sm" disabled={st.busy} onClick={() => save("gen")}>
          Generate
        </button>
        <label>
          <input type="checkbox" checked={pkcs1} onChange={(e) => setPkcs1(e.currentTarget.checked)} /> Older server (≤3.16)
        </label>
        <Hint label="About older servers">iperf3 3.16 and older (e.g. Ubuntu 24.04) only accept PKCS#1 padding.</Hint>
      </div>
      <textarea
        spellCheck={false}
        placeholder={a?.hasKey ? "(server public key set; paste to replace)" : "Server public key: -----BEGIN PUBLIC KEY----- …"}
        value={key}
        onInput={(e) => setKey(e.currentTarget.value)}
        style={{ ...inp, width: "100%", minHeight: 54, fontFamily: "'SF Mono',ui-monospace,monospace" }}
      />
      {a?.usersLine && (
        <div className="pr-note">
          Server users file line:{" "}
          <span className="pr-endpoint" style={{ wordBreak: "break-all" }}>
            {a.usersLine}
          </span>
        </div>
      )}
      <div className="pr-row">
        <button className="btn btn-sm btn-primary" disabled={st.busy} onClick={() => save("save")}>
          Save
        </button>
        {a && (
          <button className="btn btn-sm" disabled={st.busy} onClick={() => save("del")}>
            Forget
          </button>
        )}
        <ActionLine a={st} />
      </div>
    </div>
  );
}
