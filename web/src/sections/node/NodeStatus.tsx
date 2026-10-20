// A node's status: board, firmware, Wi-Fi and memory. Polled by the store (/api/status).

import { useRef } from "react";
import { useSnapshot } from "../../bob/context";
import { fmtSz, fmtUp, wifiColor } from "../../lib/format";
import { SignalBars } from "../../ui/spark";
import { Pill } from "../../ui/common";
import { HIST_N } from "../../lib/store";

export default function NodeStatus() {
  const snap = useSnapshot();
  const n = snap.node;
  // RSSI history for the sparkline, one sample per status poll.
  const rssi = useRef<{ tick: number; v: number[] }>({ tick: -1, v: [] });
  if (n?.network.rssi != null && rssi.current.tick !== snap.tick) {
    rssi.current.tick = snap.tick;
    rssi.current.v = [...rssi.current.v, n.network.rssi].slice(-HIST_N);
  }
  if (!n) return null;
  const w = n.network;
  const info = snap.nodeInfo;
  return (
    <div className="section">
      <div className="section-title">Node Status</div>
      <div className="status-grid">
        <div>
          <div className="stat-label">Board</div>
          <div className="stat-val">{n.boardName || n.board}</div>
          <div className="stat-sub">
            {n.chip} · board file {n.board}
          </div>
          <div className="stat-sub">
            Firmware {n.version} · {fmtUp(n.uptime)}
          </div>
          {n.heap && (
            <div className="stat-sub">
              Heap {fmtSz(n.heap.used)} used, {fmtSz(n.heap.free)} free
            </div>
          )}
        </div>
        <div>
          <div className="stat-label">Wi-Fi</div>
          <div className="pr-row" style={{ margin: 0 }}>
            <Pill cls={w.wifi === "connected" ? "on" : w.wifi === "connecting" ? "warn" : "bad"}>{w.wifi}</Pill>
            {w.ssid && <span>{w.ssid}</span>}
            {w.rssi != null && (
              <>
                <SignalBars data={rssi.current.v} w={70} h={18} inline />
                <span style={{ color: wifiColor(w.rssi), fontWeight: 600, fontSize: ".82rem" }}>{w.rssi} dBm</span>
              </>
            )}
          </div>
          <div className="stat-sub mono">
            {w.ip || "no address"} · MAC {w.mac}
          </div>
          {w.wifi === "unprovisioned" && <div className="pr-note">No Wi-Fi configured: provision it over USB with Improv Wi-Fi (or, on a Pico 2 W, a config.uf2). See the node firmware README.</div>}
        </div>
      </div>
      {info && (
        <div className="pr-body" style={{ paddingTop: 10 }}>
          <div className="pr-note">
            {info.name} · MCP at <span className="pr-endpoint">/mcp</span> · endpoints listed at <span className="pr-endpoint">/api</span>
          </div>
        </div>
      )}
    </div>
  );
}
