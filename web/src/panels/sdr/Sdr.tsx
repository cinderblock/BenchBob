// RTL-SDR panel: sticks + DVB-driver check, mode switch (rtl_tcp / rtl_433 / off), and the
// rtl_433 sensor table, which is polled from /api/sdr/rtl433 while it has anything to show.

import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useBob } from "../../bob/context";
import type { PanelProps } from "../../registry";
import { fmtAgo } from "../../lib/format";
import { AsyncButton } from "../../ui/common";
import { errText } from "../../ui/status";
import type { ModeRequest, Rtl433Result, SdrState } from "./api";

const SR: [number, string][] = [
  [250000, "250 kS/s"],
  [1024000, "1.024 MS/s"],
  [1800000, "1.8 MS/s"],
  [2048000, "2.048 MS/s"],
  [2400000, "2.4 MS/s"],
  [3200000, "3.2 MS/s (may drop samples)"],
];
const META = new Set(["type", "subtype"]);

function Line({ pts, w = 70, h = 18, color = "#ee8a6c" }: { pts: [number, number][] | undefined; w?: number; h?: number; color?: string }) {
  if (!pts || pts.length < 2) return null;
  const vs = pts.map((p) => p[1]),
    mn = Math.min(...vs),
    mx = Math.max(...vs),
    span = mx - mn || 1;
  const t0 = pts[0][0],
    tspan = pts[pts.length - 1][0] - t0 || 1;
  const p = pts.map(([t, v]) => `${(((t - t0) / tspan) * w).toFixed(1)},${(h - 1 - ((v - mn) / span) * (h - 2)).toFixed(1)}`).join(" ");
  return (
    <svg width={w} height={h} viewBox={`0 0 ${w} ${h}`} style={{ display: "inline-block", verticalAlign: "middle" }}>
      <polyline points={p} fill="none" stroke={color} strokeWidth={1.2} />
    </svg>
  );
}

export default function Sdr({ st }: PanelProps<SdrState>) {
  const b = useBob();
  const [data, setData] = useState<Rtl433Result | null>(null);
  const [msgs, setMsgs] = useState<Rtl433Result["messages"]>([]);
  const seq = useRef(0);
  const want = st.mode === "rtl_433" || (st.rtl433?.sensors ?? 0) > 0;
  // The 2 s poll's last failure, as a quiet note; the next success clears it.
  const [pollErr, setPollErr] = useState("");

  const refresh = async () => {
    try {
      const r = await b.api<Rtl433Result>("GET", "/api/sdr/rtl433?since=" + seq.current);
      setMsgs((m) => (r.seq < seq.current ? [] : m).concat(r.messages).slice(-100));
      seq.current = r.seq;
      setData(r);
      setPollErr("");
    } catch (e) {
      setPollErr(errText(e));
    }
  };
  useEffect(() => {
    if (!want) {
      setData(null);
      return;
    }
    refresh();
    const t = setInterval(refresh, 2000);
    return () => clearInterval(t);
  }, [want]);

  // Only ever called from AsyncButtons, which show a failure beside themselves.
  const mode = (req: ModeRequest) => b.api("POST", "/api/sdr/mode", req);
  const noStick = !(st.sticks || []).length;
  const [force, setForce] = useState(false);

  return (
    <div>
      {(st.sticks || []).map((s) => {
        const p = s.probe;
        return (
          <div className="pr-card" key={s.key}>
            <h4>
              {s.manufacturer || ""} {s.product || s.knownAs}{" "}
              <small>
                {s.usbId} · {s.knownAs}
              </small>
              {s.dvbClaimed && <span className="pr-pill bad">held by DVB driver</span>}
            </h4>
            <div className="pr-note">
              Serial {s.serial || "–"} · USB {s.bus} · port {s.port || "–"}
              {s.drivers.length ? " · driver " + s.drivers.join(", ") : ""}
            </div>
            <div className="pr-note">
              {p ? (
                <>
                  Tuner <b>{p.tuner || "unknown"}</b>
                  {p.gains?.length ? ` · ${p.gains.length} gain steps 0–${p.gains[p.gains.length - 1]} dB` : ""}
                  {p.eeprom ? ` · EEPROM: ${p.eeprom.Manufacturer || ""} / ${p.eeprom.Product || ""} / SN ${p.eeprom["Serial number"] || ""}` : ""}
                  {p.error && <span className="pr-pill bad"> {p.error}</span>} · probed {fmtAgo(p.t)}
                </>
              ) : (
                "Not probed yet"
              )}{" "}
              <AsyncButton disabled={st.mode !== "off" || st.probing} onClick={() => b.api("POST", "/api/sdr/probe", { stick: s.key })}>
                {st.probing ? "Probing…" : "Probe tuner"}
              </AsyncButton>
            </div>
          </div>
        );
      })}
      {noStick && <div className="pr-note">No RTL2832U stick detected (known USB ids: 0bda:2838, 0bda:2832 and clones).</div>}
      <div className="pr-row pr-note">
        DVB driver dvb_usb_rtl28xxu:{" "}
        {st.dvb?.blacklisted ? (
          <>
            <span className="pr-pill on">blacklisted</span> {st.dvb.where?.[0] || ""}
          </>
        ) : (
          <>
            <span className="pr-pill warn">not blacklisted</span>
            {st.dvb?.loaded && <span className="pr-pill bad"> module loaded</span>} A stick plugged in now would be grabbed by the TV driver. Fix: <span className="pr-endpoint">{st.dvb?.fix}</span>
          </>
        )}
      </div>

      <div className="pr-row">
        {st.mode === "off" ? (
          <>
            <span className="pr-pill">Off</span>
            {st.lastExit && (
              <>
                {" "}
                <span className="pr-pill bad">
                  {st.lastExit.mode} exited ({st.lastExit.code})
                </span>{" "}
                <span className="pr-note">{st.lastExit.message}</span>
              </>
            )}
          </>
        ) : (
          <>
            <span className="pr-pill on">{st.mode}</span> <span className="pr-note">since {st.since ? fmtAgo(st.since) : ""}</span>{" "}
            <AsyncButton className="btn btn-sm btn-danger" onClick={() => mode({ mode: "off" })}>
              Stop
            </AsyncButton>
          </>
        )}
      </div>
      {st.mode === "rtl_tcp" && (
        <div className="pr-card">
          <h4>
            rtl_tcp server{" "}
            <small>
              {st.opts?.bind}:{st.opts?.port} · {((st.opts?.sampleRate || 0) / 1e6).toFixed(3)} MS/s · gain {st.opts?.gain ? st.opts.gain + " dB" : "auto"} · {st.opts?.ppm} ppm
            </small>
          </h4>
          <div className="pr-note">
            SDR++: source <b>RTL-TCP</b>, host <span className="pr-endpoint">{b.conn.hostname}</span> port <span className="pr-endpoint">{st.opts?.port}</span>
          </div>
          <div className="pr-note">
            GQRX device string:{" "}
            <span className="pr-endpoint">
              rtl_tcp={b.conn.hostname}:{st.opts?.port}
            </span>
          </div>
          <div className="pr-note">
            SDR#: source "RTL-SDR (TCP)", host {b.conn.hostname}, port {st.opts?.port}
          </div>
          <div className="pr-note">
            Client:{" "}
            {st.clients?.length
              ? st.clients.map((c) => (
                  <span key={c} className="pr-pill on">
                    {c}
                  </span>
                ))
              : "none connected (rtl_tcp serves one client at a time)"}
          </div>
        </div>
      )}

      {st.mode === "off" && <Forms key={JSON.stringify([(st.sticks || []).length, st.defaults])} st={st} force={force} mode={mode} />}
      {st.mode === "off" && noStick && (
        <div className="pr-row">
          <label>
            <input type="checkbox" checked={force} onChange={(e) => setForce(e.currentTarget.checked)} /> Try anyway (a clone with an unlisted USB id)
          </label>
        </div>
      )}

      {want && pollErr && <div className="pr-note">Couldn’t refresh the rtl_433 readings: {pollErr}</div>}
      {data && <R433 data={data} msgs={msgs} onClear={async () => {
        await b.api("DELETE", "/api/sdr/rtl433");
        seq.current = 0;
        setMsgs([]);
        setData(null);
        refresh();
      }} />}
    </div>
  );
}

/** The two start forms; rebuilt only when the stick count or saved defaults change. */
function Forms({ st, force, mode }: { st: SdrState; force: boolean; mode: (r: ModeRequest) => Promise<unknown> }) {
  const t = st.defaults?.settings?.tcp || {},
    r = st.defaults?.settings?.rtl433 || {};
  const [port, setPort] = useState(String(t.port || st.defaults?.tcpPort || 1234));
  const [bind, setBind] = useState(t.bind === "127.0.0.1" ? "127.0.0.1" : "0.0.0.0");
  const [sr, setSr] = useState(String(t.sampleRate || 2048000));
  const [tgain, setTgain] = useState(String(t.gain || ""));
  const [tppm, setTppm] = useState(String(t.ppm || 0));
  const [preset, setPreset] = useState(String(r.preset || "433"));
  const [hop, setHop] = useState(String(r.hop || 60));
  const [rgain, setRgain] = useState(String(r.gain || ""));
  const [rppm, setRppm] = useState(String(r.ppm || 0));
  const f = force || undefined;
  return (
    <>
      <div className="pr-card">
        <h4>
          rtl_tcp server <small>raw I/Q for SDR++, GQRX, SDR#</small>
        </h4>
        <div className="pr-row">
          <label>
            Port <input type="number" min={1} max={65535} value={port} onInput={(e) => setPort(e.currentTarget.value)} style={{ width: 80 }} />
          </label>
          <label>
            Listen{" "}
            <select value={bind} onChange={(e) => setBind(e.currentTarget.value)}>
              <option value="0.0.0.0">all interfaces</option>
              <option value="127.0.0.1">this Pi only</option>
            </select>
          </label>
          <label>
            Rate{" "}
            <select value={sr} onChange={(e) => setSr(e.currentTarget.value)}>
              {SR.map(([v, l]) => (
                <option key={v} value={v}>
                  {l}
                </option>
              ))}
            </select>
          </label>
          <label>
            Gain dB <input type="number" step={0.1} min={0} max={50} value={tgain} placeholder="auto" onInput={(e) => setTgain(e.currentTarget.value)} style={{ width: 64 }} />
          </label>
          <label>
            PPM <input type="number" value={tppm} onInput={(e) => setTppm(e.currentTarget.value)} style={{ width: 56 }} />
          </label>
          <AsyncButton className="btn btn-sm btn-primary" onClick={() => mode({ mode: "rtl_tcp", force: f, port: +port, bind, gain: +tgain || 0, ppm: +tppm || 0, sampleRate: +sr })}>
            Start rtl_tcp
          </AsyncButton>
        </div>
      </div>
      <div className="pr-card">
        <h4>
          rtl_433 <small>decode ISM-band sensors</small>
        </h4>
        <div className="pr-row">
          <label>
            Band{" "}
            <select value={preset} onChange={(e) => setPreset(e.currentTarget.value)}>
              {Object.entries(st.presets || {}).map(([k, p]) => (
                <option key={k} value={k}>
                  {p.label}
                </option>
              ))}
            </select>
          </label>
          <label>
            Hop every <input type="number" min={5} value={hop} onInput={(e) => setHop(e.currentTarget.value)} style={{ width: 56 }} /> s
          </label>
          <label>
            Gain dB <input type="number" step={0.1} min={0} max={50} value={rgain} placeholder="auto" onInput={(e) => setRgain(e.currentTarget.value)} style={{ width: 64 }} />
          </label>
          <label>
            PPM <input type="number" value={rppm} onInput={(e) => setRppm(e.currentTarget.value)} style={{ width: 56 }} />
          </label>
          <AsyncButton className="btn btn-sm btn-primary" onClick={() => mode({ mode: "rtl_433", force: f, preset, hop: +hop || undefined, gain: rgain || "", ppm: +rppm || 0 })}>
            Start rtl_433
          </AsyncButton>
        </div>
      </div>
    </>
  );
}

function fieldStr(k: string, v: any) {
  if (k === "battery_ok") return v >= 1 ? "battery ok" : <span className="pr-pill bad">battery low</span>;
  if (k === "temperature_C") return <b>{String(v)} °C</b>;
  if (k === "humidity") return <b>{String(v)} %RH</b>;
  return `${k} ${typeof v === "object" ? JSON.stringify(v) : v}`;
}

function R433({ data, msgs, onClear }: { data: Rtl433Result; msgs: Rtl433Result["messages"]; onClear(): Promise<unknown> }) {
  const logRef = useRef<HTMLDivElement>(null);
  const atEnd = useRef(true);
  // Keep the raw log pinned to the bottom while the user hasn't scrolled up.
  useLayoutEffect(() => {
    const l = logRef.current;
    if (l && atEnd.current) l.scrollTop = l.scrollHeight;
  });
  return (
    <div>
      <div className="pr-row">
        <b style={{ fontSize: ".85rem" }}>Decoded sensors</b>{" "}
        <span className="pr-note">
          {data.devices.length} sensors · {data.seq} messages
        </span>
        <AsyncButton onClick={onClear}>Clear</AsyncButton>
      </div>
      {!data.devices.length ? (
        <div className="pr-note">Nothing decoded yet. Most sensors transmit every 30–60 s.</div>
      ) : (
        <table className="pr-tbl">
          <thead>
            <tr>
              <th>Sensor</th>
              <th>Latest</th>
              <th>Signal</th>
              <th>Seen</th>
            </tr>
          </thead>
          <tbody>
            {data.devices.map((d, i) => {
              const f = Object.entries(d.fields).filter(([k]) => !META.has(k));
              const sp = d.hist.temperature_C ? <Line pts={d.hist.temperature_C} /> : d.hist.humidity ? <Line pts={d.hist.humidity} color="#6cb4ee" /> : d.hist.moisture ? <Line pts={d.hist.moisture} color="#4caf7d" /> : null;
              return (
                <tr key={`${d.model}:${d.id}:${d.channel}:${i}`}>
                  <td style={{ wordBreak: "break-word" }}>
                    <b>{d.model}</b>
                    {d.fields.type && <span className="pr-pill"> {String(d.fields.type)}</span>}
                    <br />
                    <span className="pr-note">
                      id {d.id ?? "–"}
                      {d.channel ? " · ch " + d.channel : ""}
                      {d.protocol ? " · proto " + d.protocol : ""}
                    </span>
                  </td>
                  <td style={{ wordBreak: "break-word" }}>
                    {f.map(([k, v], j) => (
                      <span key={k}>
                        {j > 0 && " · "}
                        {fieldStr(k, v)}
                      </span>
                    ))}
                    {sp && (
                      <>
                        <br />
                        {sp}
                      </>
                    )}
                  </td>
                  <td style={{ whiteSpace: "nowrap" }}>
                    {d.rssi != null ? d.rssi.toFixed(1) + " dB" : "–"}
                    {d.snr != null && (
                      <>
                        <br />
                        <span className="pr-note">SNR {d.snr.toFixed(1)}</span>
                      </>
                    )}
                    {d.freq ? (
                      <>
                        <br />
                        <span className="pr-note">{d.freq.toFixed(3)} MHz</span>
                      </>
                    ) : null}
                  </td>
                  <td style={{ whiteSpace: "nowrap" }}>
                    {fmtAgo(d.lastSeen)}
                    <br />
                    <span className="pr-note">{d.count}×</span>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
      <div className="pr-note" style={{ marginTop: 8 }}>
        Raw messages (newest last)
      </div>
      <div
        className="pr-log"
        style={{ maxHeight: 200 }}
        ref={logRef}
        onScroll={(e) => {
          const l = e.currentTarget;
          atEnd.current = l.scrollTop + l.clientHeight >= l.scrollHeight - 4;
        }}
      >
        {msgs.map((m) => new Date(m.t).toLocaleTimeString() + " " + JSON.stringify(m.msg)).join("\n") || "–"}
      </div>
      {data.log?.length ? (
        <>
          <div className="pr-note" style={{ marginTop: 8 }}>
            rtl_433 log
          </div>
          <div className="pr-log" style={{ maxHeight: 120 }}>
            {data.log.map((l) => l.line).join("\n")}
          </div>
        </>
      ) : null}
    </div>
  );
}
