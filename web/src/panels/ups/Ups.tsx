// UPS section: one card per UPS with big charge/runtime/load figures, decoded status pills,
// charge & load sparklines (10 minutes), details, the power-event log, and the NUT
// network-sharing switch. Rendered from the status push alone; nothing to fetch.

import { useState, type ReactNode } from "react";
import { useBob } from "../../bob/context";
import type { PanelProps } from "../../registry";
import type { Ups as UpsT, UpsState } from "./api";

// Linear line sparkline, 0–100 scale (both series are percentages).
function Spark({ vals, color }: { vals: number[]; color: string }) {
  const w = 240,
    h = 36;
  if (!vals || vals.length < 2) return <svg width="100%" height={h} viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none" />;
  const n = 120,
    dx = w / (n - 1),
    off = n - vals.length;
  const pts = vals.map((v, i) => `${((off + i) * dx).toFixed(1)},${(h - 2 - (Math.max(0, Math.min(100, v)) / 100) * (h - 4)).toFixed(1)}`).join(" ");
  return (
    <svg width="100%" height={h} viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none" style={{ display: "block" }}>
      <polyline points={pts} fill="none" stroke={color} strokeWidth={1.5} vectorEffect="non-scaling-stroke" />
    </svg>
  );
}

const Big = ({ label, value, sub }: { label: string; value: ReactNode; sub?: ReactNode }) => (
  <div style={{ flex: 1, minWidth: 90 }}>
    <div className="pr-note">{label}</div>
    <div style={{ fontSize: "1.4rem", fontWeight: 600, fontVariantNumeric: "tabular-nums" }}>{value}</div>
    {sub && <div className="pr-note">{sub}</div>}
  </div>
);
const v = (x: number | null | undefined, unit: string) => (x == null ? "—" : `${x}${unit}`);

function Card({ u }: { u: UpsT }) {
  const rows: [string, string | undefined][] = [
    ["Input", [u.inputVoltage != null ? `${u.inputVoltage} V` : "", u.inputFrequency != null ? `${u.inputFrequency} Hz` : ""].filter(Boolean).join(" · ")],
    ["Output", u.outputVoltage != null ? `${u.outputVoltage} V` : ""],
    ["Power", u.realpower != null ? `${u.realpower} W` : ""],
    ["Battery", [u.batteryVoltage != null ? `${u.batteryVoltage} V` : "", u.batteryType, u.batteryDate ? `dated ${u.batteryDate}` : ""].filter(Boolean).join(" · ")],
    ["Low battery at", [u.lowCharge != null ? `${u.lowCharge}%` : "", u.lowRuntime != null ? `${u.lowRuntime} s runtime` : ""].filter(Boolean).join(" or ")],
    ["Serial", u.serial],
    ["Firmware", u.firmware],
    ["Beeper", u.beeper],
    ["Connection", u.simulated ? "Simulated (dummy-ups)" : `USB ${u.usb} · NUT ${u.nutDriver}`],
  ];
  return (
    <div className="pr-card">
      <h4>
        {u.vendor} {u.model} <small>NUT name {u.name}</small>
        {u.simulated && <span className="pr-pill warn">simulated</span>}
      </h4>
      <div className="pr-row">
        {!u.driverRunning ? (
          <span className="pr-pill bad">NUT driver not running</span>
        ) : !u.reachable ? (
          <span className="pr-pill warn">waiting for data</span>
        ) : (
          u.flags.map((f) => (
            <span key={f.word} className={`pr-pill ${f.severity === "ok" ? "on" : f.severity === "warn" ? "warn" : f.severity === "bad" ? "bad" : ""}`}>
              {f.word}
            </span>
          ))
        )}
      </div>
      {u.error && (
        <div className="pr-note" style={{ color: "var(--danger)" }}>
          {u.error}
        </div>
      )}
      <div className="pr-row" style={{ gap: 16 }}>
        <Big label="Charge" value={v(u.charge, "%")} sub={<Spark vals={u.history.charge} color="var(--success)" />} />
        <Big label="Runtime" value={u.runtimeText || "—"} />
        <Big label="Load" value={v(u.load, "%")} sub={<Spark vals={u.history.load} color="var(--accent)" />} />
      </div>
      <div className="pr-note">Sparklines: last 10 minutes, 0–100%.</div>
      <table className="pr-tbl" style={{ marginTop: 8 }}>
        <tbody>
          {rows
            .filter((r) => r[1])
            .map((r) => (
              <tr key={r[0]}>
                <td className="pr-note" style={{ width: "9em" }}>
                  {r[0]}
                </td>
                <td>{r[1]}</td>
              </tr>
            ))}
        </tbody>
      </table>
      {u.events.length ? (
        <>
          <div className="pr-note" style={{ marginTop: 8 }}>
            Power events
          </div>
          <div className="pr-log" style={{ maxHeight: 140 }}>
            {u.events
              .slice()
              .reverse()
              .map((e) => `${new Date(e.t).toLocaleString()}  ${e.text}`)
              .join("\n")}
          </div>
        </>
      ) : (
        <div className="pr-note" style={{ marginTop: 8 }}>
          No power events since BenchBob started.
        </div>
      )}
    </div>
  );
}

function Nut({ st }: { st: UpsState }) {
  const b = useBob();
  const n = st.nut;
  const any = st.upses.length > 0;
  // Local port while editing, so a status push doesn't overwrite what's typed.
  const [port, setPort] = useState<string | null>(null);
  const host = b.conn.hostname;
  const share = (shared: boolean, p: number) => b.api("POST", "/api/ups/share", { share: shared, port: p }).catch(() => {});
  const curPort = port ?? String(n.port);
  return (
    <div className="pr-card">
      <h4>
        NUT server <small>{n.running ? "running" : any ? "stopped" : "starts when a UPS is connected"}</small>
      </h4>
      <div className="pr-row">
        <label>
          <input type="checkbox" checked={n.shared} onChange={(e) => share(e.currentTarget.checked, Number(curPort))} /> Share on network, port
        </label>
        <input
          type="number"
          min={1}
          max={65535}
          value={curPort}
          style={{ width: "6em" }}
          onInput={(e) => setPort(e.currentTarget.value)}
          // preact/compat turns onChange on inputs into onInput, so commit on blur / Enter.
          onBlur={() => {
            if (port != null && port !== String(n.port)) share(n.shared, Number(port));
            setPort(null);
          }}
          onKeyDown={(e) => e.key === "Enter" && e.currentTarget.blur()}
        />
        <button className="btn btn-sm" onClick={() => b.api("POST", "/api/ups/restart").catch(() => {})}>
          Restart NUT
        </button>
      </div>
      <div className="pr-note">
        {n.shared ? (
          <>
            NUT clients elsewhere can monitor <span className="pr-endpoint">benchbob@{host}:{n.port}</span>. For upsmon:{" "}
            <span className="pr-endpoint">
              MONITOR benchbob@{host}:{n.port} 1 {n.user} {n.password || ""} secondary
            </span>
          </>
        ) : (
          `Listening on 127.0.0.1:${n.port} only (BenchBob itself).`
        )}
      </div>
      <div className="pr-note">
        Automatic shutdown of BenchBob on low battery is <b>not configured</b>.
      </div>
      {n.log?.length ? <div className="pr-log">{n.log.join("\n")}</div> : null}
    </div>
  );
}

export default function Ups({ st }: PanelProps<UpsState>) {
  return (
    <>
      {st.upses.length ? st.upses.map((u) => <Card key={u.name} u={u} />) : <div className="pr-note">No UPS connected. Plug a UPS in by USB (APC, CyberPower, Eaton, Tripp Lite, Liebert and anything NUT supports).</div>}
      <Nut st={st} />
    </>
  );
}
