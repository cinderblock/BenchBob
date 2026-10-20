// System status: CPU & memory sparklines, the network table and the root device.

import { Fragment, useState } from "react";
import { useBob, useSnapshot } from "../../bob/context";
import { fmtRate, fmtSz, pctColor, wifiColor } from "../../lib/format";
import type { SectionProps } from "../../registry";
import type { NetIface } from "../../lib/types";
import { Bars, MemBars, SignalBars } from "../../ui/spark";

export default function System({ status: s }: SectionProps) {
  const { store } = useBob();
  useSnapshot();
  const h = store.hist;
  const [rootOpen, setRootOpen] = useState(false);
  const [netOpen, setNetOpen] = useState<string | null>(null);
  let netMax = 0;
  for (const [k, v] of Object.entries(h)) if (k.startsWith("net:")) for (const x of v) if (x > netMax) netMax = x;
  netMax ||= 1;
  const memUsed = s.system.memTotal - s.system.memAvailable;

  return (
    <div className="section">
      <div className="section-title">System Status</div>
      <div className="status-grid">
        <div>
          <div className="stat-label">CPU &amp; Memory</div>
          <div className="spark-row">
            <div className="spark-box" style={{ flex: 1, minWidth: 120 }}>
              <div className="spark-hdr">
                <span className="spark-lbl">CPU</span>
                <span className="spark-val" style={{ color: pctColor(s.system.cpuUsage) }}>
                  {s.system.cpuUsage.toFixed(1)}%
                </span>
              </div>
              <div className="spark-fit">
                <Bars data={h.cpu} w={140} h={28} color={pctColor(s.system.cpuUsage)} max={100} />
              </div>
              <div className="stat-sub">
                {s.system.cpuTemp.toFixed(1)}°C · {s.system.cores} cores{s.power?.inputVolts ? ` · ${s.power.inputVolts.toFixed(2)} V in` : ""}
              </div>
            </div>
            <div className="spark-box" style={{ flex: 1, minWidth: 120 }}>
              <div className="spark-hdr">
                <span className="spark-lbl">RAM</span>
                <span className="spark-val" style={{ color: pctColor(s.system.memPercent) }}>
                  {s.system.memPercent}%
                </span>
              </div>
              <div className="spark-fit">
                <MemBars data={h.mem} w={140} h={28} color={pctColor(s.system.memPercent)} />
              </div>
              <div className="stat-sub">
                {fmtSz(memUsed * 1024)} / {fmtSz(s.system.memTotal * 1024)}
              </div>
            </div>
          </div>
        </div>
        <div>
          <div className="stat-label">Network</div>
          {/* Sorted by metric on the server. The interface column takes whatever RX/TX leave. */}
          <table className="net-tbl">
            <colgroup>
              <col style={{ width: 42 }} />
              <col />
              <col style={{ width: "33%" }} />
              <col style={{ width: "33%" }} />
            </colgroup>
            <thead>
              <tr>
                <th></th>
                <th>Interface</th>
                <th>RX</th>
                <th>TX</th>
              </tr>
            </thead>
            <tbody>
              {s.network.map((n) => (
                <Fragment key={n.name}>
                  <tr>
                    <td>
                      <span className={`net-badge ${n.state}`}>{n.state === "UP" ? "Up" : "Dn"}</span>
                    </td>
                    <td className="net-if">
                      <button type="button" className="net-name net-name-btn" aria-expanded={netOpen === n.name} onClick={() => setNetOpen(netOpen === n.name ? null : n.name)}>
                        {n.name}
                      </button>
                      {n.ip && <span className="net-ip">{n.ip}</span>}
                    </td>
                    <td>
                      <div className="net-bw">
                        <div className="spark-fit">
                          <Bars data={h[`net:${n.name}:rx`]} w={70} h={18} color="#6cb4ee" max={netMax} log />
                        </div>
                        <span className="bw-rx">{fmtRate(n.rxRate)}</span>
                      </div>
                    </td>
                    <td>
                      <div className="net-bw">
                        <div className="spark-fit">
                          <Bars data={h[`net:${n.name}:tx`]} w={70} h={18} color="#ee8a6c" max={netMax} log />
                        </div>
                        <span className="bw-tx">{fmtRate(n.txRate)}</span>
                      </div>
                    </td>
                  </tr>
                  {n.wifi && (
                    <tr>
                      <td colSpan={4} style={{ padding: "4px 8px 8px", whiteSpace: "normal" }}>
                        <div className="wifi-info">
                          <div style={{ display: "flex", alignItems: "center", flexWrap: "wrap", gap: "4px 8px" }}>
                            <SignalBars data={h.wifi} w={70} h={18} />
                            <span style={{ color: wifiColor(n.wifi.signal), fontWeight: 600, fontSize: ".82rem" }}>{n.wifi.signal} dBm</span>
                            <span>
                              {n.wifi.ssid} · {n.wifi.bitrate}
                            </span>
                          </div>
                        </div>
                      </td>
                    </tr>
                  )}
                  {netOpen === n.name && (
                    <tr className="net-detail">
                      <td colSpan={4}>
                        <NetDetail n={n} />
                      </td>
                    </tr>
                  )}
                </Fragment>
              ))}
            </tbody>
          </table>
        </div>
      </div>
      {s.rootDevice && (
        <>
          <button className={`root-toggle${rootOpen ? " open" : ""}`} onClick={() => setRootOpen(!rootOpen)}>
            <span className="arrow">&#9654;</span>
            <span>
              Root Device ({s.rootDevice.name} · {fmtSz(s.rootDevice.size)})
            </span>
          </button>
          <div className={`root-detail${rootOpen ? " open" : ""}`}>
            {s.rootDevice.children && (
              <div className="root-parts">
                {s.rootDevice.children.map((p) => (
                  <div className="part" key={p.name}>
                    <div className="part-left">
                      <span className="part-name">{p.name}</span>
                      <span className="part-info">
                        {fmtSz(p.size)} · {p.fstype || "?"}
                        {p.label ? " · " + p.label : ""} → {p.mountpoint || ""}
                      </span>
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
        </>
      )}
    </div>
  );
}

/** An interface's details, each value tap-to-copy (what the old hover card showed). */
function NetDetail({ n }: { n: NetIface }) {
  const { toast } = useBob();
  const rows: [string, unknown][] = [
    ["IPv4", n.ip ? `${n.ip}/${n.prefix || "?"}` : null],
    ["Gateway", n.gateway],
    ["MAC", n.mac],
    ["MTU", n.mtu],
    ["Metric", n.metric],
    ["State", n.state],
    ...(n.ipv6 || []).map((a) => ["IPv6", a] as [string, unknown]),
    ["GW6", n.ipv6?.length ? n.gateway6 : null],
    ["SSID", n.wifi?.ssid],
    ["Signal", n.wifi ? `${n.wifi.signal} dBm` : null],
    ["Bitrate", n.wifi?.bitrate],
  ];
  const copy = (v: string) =>
    navigator.clipboard?.writeText(v).then(
      () => toast(`Copied ${v}`, "success"),
      () => {},
    );
  return (
    <>
      <div className="net-kv">
        {rows
          .filter(([, v]) => v || v === 0)
          .map(([k, v], i) => (
            <Fragment key={i}>
              <span>{k}</span>
              <button type="button" onClick={() => copy(String(v))}>
                {String(v)}
              </button>
            </Fragment>
          ))}
      </div>
      <div className="pr-note">Tap a value to copy it.</div>
    </>
  );
}
