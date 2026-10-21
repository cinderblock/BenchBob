// GPS panel: pick a receiver, run a temporary gpsd, show fix + sky plot. Everything comes
// from the status stream (~2 s), which is plenty for a 1 Hz receiver.

import { useRef, useState, type ReactNode } from "react";
import { useBob } from "../../bob/context";
import type { PanelProps } from "../../registry";
import { fmtAgo } from "../../lib/format";
import { AsyncButton } from "../../ui/common";
import { ActionLine, useAction } from "../../ui/status";
import type { GpsState, Sat } from "./api";

const PFX: Record<string, string> = { GPS: "G", GLONASS: "R", Galileo: "E", BeiDou: "C", SBAS: "S", QZSS: "J", NavIC: "I" };
const snrColor = (s?: number | null) => (!s ? "#7a7f94" : s < 20 ? "#ff5c5c" : s < 30 ? "#f5a623" : s < 40 ? "#c7d64f" : "#4caf7d");
const fmt = (v: number | null | undefined, d = 1, u = "") => (v == null ? "–" : (+v).toFixed(d) + u);

function SkyPlot({ sats, size = 240 }: { sats: Sat[]; size?: number }) {
  const c = size / 2,
    R = c - 16;
  return (
    <svg viewBox={`0 0 ${size} ${size}`} width="100%" style={{ maxWidth: size, display: "block" }}>
      {[0, 30, 60].map((el) => (
        <circle key={el} cx={c} cy={c} r={(R * (90 - el)) / 90} fill="none" stroke="#2a2f40" />
      ))}
      <line x1={c} y1={c - R} x2={c} y2={c + R} stroke="#2a2f40" />
      <line x1={c - R} y1={c} x2={c + R} y2={c} stroke="#2a2f40" />
      {(
        [
          ["N", c, 10],
          ["E", size - 6, c + 4],
          ["S", c, size - 2],
          ["W", 6, c + 4],
        ] as [string, number, number][]
      ).map(([l, x, y]) => (
        <text key={l} x={x} y={y} fill="#7a7f94" fontSize={10} textAnchor="middle">
          {l}
        </text>
      ))}
      <text x={c + 3} y={c - (R * 2) / 3 - 2} fill="#4a4f60" fontSize={8}>
        30°
      </text>
      <text x={c + 3} y={c - R / 3 - 2} fill="#4a4f60" fontSize={8}>
        60°
      </text>
      {sats.map((s, i) => {
        if (s.el == null || s.az == null || s.el < 0) return null;
        const r = (R * (90 - s.el)) / 90,
          a = (s.az * Math.PI) / 180;
        const x = c + r * Math.sin(a),
          y = c - r * Math.cos(a),
          col = snrColor(s.snr);
        return (
          <g key={i}>
            <circle cx={x.toFixed(1)} cy={y.toFixed(1)} r={7} fill={s.used ? col : "none"} stroke={col} strokeWidth={1.5} />
            <text x={x.toFixed(1)} y={(y + 3).toFixed(1)} fill={s.used ? "#0c0e13" : col} fontSize={7} fontWeight={700} textAnchor="middle">
              {(PFX[s.gnss] || "?") + (s.svid ?? s.prn)}
            </text>
          </g>
        );
      })}
    </svg>
  );
}

export default function Gps({ st }: PanelProps<GpsState>) {
  const b = useBob();
  const opts = useRef<HTMLDivElement>(null);
  const cs = st.candidates || [],
    set = st.settings || {};
  const o = (k: string) => opts.current?.querySelector<HTMLInputElement>(`[data-o="${k}"]`) || null;
  const startOpts = () => ({ port: +(o("port")?.value || 0) || undefined, share: !!o("share")?.checked, readOnly: !!o("ro")?.checked, pps: o("pps") ? o("pps")!.checked : undefined });
  const host = b.conn.hostname;
  // "Use as GPS": the last one ticked reports in its own card.
  const use = useAction();
  const [useKey, setUseKey] = useState("");

  return (
    <>
      {cs.map((c) => {
        const gps = c.kind === "gps" || c.chosen,
          mine = st.running && st.device === c.path;
        return (
          <div className="pr-card" key={c.key}>
            <h4>
              {c.path}{" "}
              <small>
                {[c.vendor, c.model].filter(Boolean).join(" ")} · {c.usbId}
                {c.serial ? " · SN " + c.serial : ""}
              </small>
              {c.kind === "gps" ? <span className="pr-pill on">{c.label}</span> : c.label ? <span className="pr-pill">{c.label}</span> : null}
              {mine && <span className="pr-pill on">gpsd running</span>}
              {c.holders.length > 0 && <span className="pr-pill warn">open by {c.holders.map((x) => x.comm + " " + x.pid).join(", ")}</span>}
            </h4>
            <div className="pr-row">
              {c.kind !== "gps" && (
                <label>
                  <input
                    type="checkbox"
                    checked={!!c.chosen}
                    disabled={use.busy}
                    onChange={(e) => {
                      setUseKey(c.key);
                      use.run(() => b.api("POST", "/api/gps/use", { device: c.key, use: e.currentTarget.checked }), { busy: "Saving…" });
                    }}
                  />{" "}
                  Use as GPS
                </label>
              )}
              {useKey === c.key && <ActionLine a={use} />}
              {gps && !mine && (
                <AsyncButton className="btn btn-sm btn-primary" disabled={c.holders.length > 0} onClick={() => b.api("POST", "/api/gps/start", { device: c.key, ...startOpts() })}>
                  Start gpsd
                </AsyncButton>
              )}
              {c.holders.length > 0 && <span className="pr-note">Another program has this port. Close it there (e.g. the Serial panel) to use it here.</span>}
            </div>
          </div>
        );
      })}
      {!cs.length && <div className="pr-note">No USB serial devices. GPS receivers show up here when plugged in (u-blox and MediaTek are recognised; for other adapters tick "Use as GPS").</div>}
      {!st.running && (
        <div ref={opts}>
          <div className="pr-row">
            <label>
              Port <input data-o="port" type="number" min={1} max={65535} defaultValue={set.port} style={{ width: 80 }} />
            </label>
            <label>
              <input type="checkbox" data-o="share" defaultChecked={!!set.share} /> Share on network
            </label>
            <label>
              <input type="checkbox" data-o="ro" defaultChecked={!!set.readOnly} /> Read-only (never configure the receiver)
            </label>
            {st.ppsDevices?.length ? (
              <label>
                <input type="checkbox" data-o="pps" defaultChecked={set.pps !== false} /> Use {st.ppsDevices.join(", ")}
              </label>
            ) : null}
          </div>
          <div className="pr-row">
            <input data-o="path" placeholder="Other tty, e.g. /dev/serial/by-id/…" style={{ flex: 1, minWidth: 160 }} />
            <AsyncButton onClick={() => b.api("POST", "/api/gps/start", { path: (o("path")?.value || "").trim(), ...startOpts() })}>Start on this path</AsyncButton>
          </div>
          <div className="pr-note">gpsd runs only while started here: a temporary experiment, not a system service.</div>
          {st.lastExit && (
            <div className="pr-row">
              <span className="pr-pill bad">gpsd exited ({st.lastExit.code})</span> <span className="pr-note">{st.lastExit.message}</span>
            </div>
          )}
        </div>
      )}
      {st.running && <Running st={st} host={host} />}
      {st.log?.length ? (
        <>
          <div className="pr-note" style={{ marginTop: 8 }}>
            gpsd output
          </div>
          <div className="pr-log" style={{ maxHeight: 140 }}>
            {st.log.map((l) => l.line).join("\n")}
          </div>
        </>
      ) : null}
    </>
  );
}

function Running({ st, host }: { st: GpsState; host: string }) {
  const b = useBob();
  const f = st.fix || {},
    sats = st.satellites || [];
  const pill = f.mode === 3 ? "on" : f.mode === 2 ? "warn" : "bad";
  const d = st.gpsd?.devices?.[0];
  const rows: [string, ReactNode][] = [
    [
      "Position",
      f.lat != null ? (
        <>
          {f.lat.toFixed(6)}, {f.lon!.toFixed(6)}{" "}
          <a href={`https://www.openstreetmap.org/?mlat=${f.lat}&mlon=${f.lon}#map=17/${f.lat}/${f.lon}`} target="_blank" rel="noopener">
            map
          </a>
        </>
      ) : (
        "–"
      ),
    ],
    ["Altitude", `${fmt(f.altMSL_m, 1, " m")} MSL${f.altHAE_m != null ? ` · ${fmt(f.altHAE_m, 1, " m")} HAE` : ""}`],
    ["Speed / track", `${fmt(f.speed_ms, 2, " m/s")}${f.speed_ms != null ? ` (${(f.speed_ms * 3.6).toFixed(1)} km/h)` : ""} · ${fmt(f.track_deg, 1, "°")}`],
    ["Accuracy", `±${fmt(f.eph_m, 1, " m")} horiz${f.epv_m != null ? ` · ±${fmt(f.epv_m, 1, " m")} vert` : ""}`],
    ["DOP", `HDOP ${fmt(f.hdop, 2)} · PDOP ${fmt(f.pdop, 2)} · VDOP ${fmt(f.vdop, 2)}`],
    ["UTC time", f.time || "–"],
    [
      "vs system clock",
      <>
        {f.toffOffset_ms != null ? `${f.toffOffset_ms} ms (gpsd TOFF)` : "–"}
        {f.nmeaOffset_ms != null ? ` · NMEA arrival ${f.nmeaOffset_ms} ms` : ""} <span className="pr-note">negative = receiver's time is behind; includes serial latency</span>
      </>,
    ],
    ["PPS", st.ppsDevices?.length ? (f.ppsOffset_us != null ? `${f.ppsOffset_us} µs offset (${f.ppsAge_s} s ago)` : `${st.ppsDevices.join(", ")}: no pulses yet`) : "no /dev/pps* device"],
  ];
  const cons = Object.entries(f.constellations || {});
  return (
    <>
      <div className="pr-row">
        <span className={`pr-pill ${pill}`}>{f.fix === "unknown" ? "waiting for data" : f.fix}</span>
        <span className="pr-note">
          gpsd on {st.device} · clients connect to <span className="pr-endpoint">{st.share ? host + ":" + st.port : st.connect}</span> · since {fmtAgo(st.since || Date.now())}
          {st.readOnly ? " · read-only" : ""}
        </span>
        <AsyncButton className="btn btn-sm btn-danger" onClick={() => b.api("POST", "/api/gps/stop")}>
          Stop gpsd
        </AsyncButton>
      </div>
      {st.gpsd?.errors?.length ? (
        <div className="pr-row">
          <span className="pr-pill bad">{st.gpsd.errors[st.gpsd.errors.length - 1]}</span>
        </div>
      ) : null}
      {d && (
        <div className="pr-note">
          {d.driver || "detecting protocol…"}
          {d.subtype ? " · " + d.subtype : ""}
          {d.subtype1 ? " · " + d.subtype1 : ""}
          {d.bps ? " · " + d.bps + " bps" : ""}
          {st.gpsd?.version ? " · gpsd " + st.gpsd.version : ""}
        </div>
      )}
      <div style={{ display: "flex", flexWrap: "wrap", gap: 16, alignItems: "flex-start", marginTop: 8 }}>
        <div style={{ flex: "0 1 240px", minWidth: 180 }}>
          <SkyPlot sats={sats} />
          <div className="pr-note">
            Filled = used in fix. Colour = signal: <span style={{ color: "#4caf7d" }}>≥40</span> <span style={{ color: "#c7d64f" }}>30s</span> <span style={{ color: "#f5a623" }}>20s</span> <span style={{ color: "#ff5c5c" }}>&lt;20</span> dB-Hz. G GPS · R GLONASS · E Galileo · C BeiDou · S SBAS · J QZSS
          </div>
        </div>
        <div style={{ flex: "1 1 260px", minWidth: 0 }}>
          <table className="pr-tbl">
            <tbody>
              {rows.map(([k, v]) => (
                <tr key={k}>
                  <td className="pr-note" style={{ whiteSpace: "nowrap" }}>
                    {k}
                  </td>
                  <td style={{ wordBreak: "break-word" }}>{v}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <table className="pr-tbl" style={{ marginTop: 8 }}>
            <thead>
              <tr>
                <th>Constellation</th>
                <th>Used</th>
                <th>Visible</th>
              </tr>
            </thead>
            <tbody>
              {cons.length ? (
                cons.map(([g, c]) => (
                  <tr key={g}>
                    <td>{g}</td>
                    <td>{c.used}</td>
                    <td>{c.visible}</td>
                  </tr>
                ))
              ) : (
                <tr>
                  <td colSpan={3} className="pr-note">
                    No satellites reported yet
                  </td>
                </tr>
              )}
              <tr>
                <td>
                  <b>Total</b>
                </td>
                <td>
                  <b>{f.satsUsed ?? 0}</b>
                </td>
                <td>
                  <b>{f.satsVisible ?? 0}</b>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>
    </>
  );
}
