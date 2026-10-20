// Bluetooth LE scanner panel. Device list arrives over /ws/ble (~1/s while it changes);
// the section's status (controllers, scan state) comes from the regular status stream.

import { Fragment, useEffect, useRef, useState, type ReactNode } from "react";
import { useBob } from "../../bob/context";
import type { PanelProps } from "../../registry";
import { fmtAgo, fmtHex } from "../../lib/format";
import { readLocal, writeLocal } from "../../lib/storage";
import type { Ws } from "../../lib/connection";
import { bleApi, type BleDevice, type BlePrefs, type BleState } from "./api";

const PREF_DEFAULTS: BlePrefs = { live: false, q: "", minRssi: "", beacons: false, sort: "rssi" };

// RSSI sparkline: fixed −100…−30 dBm scale so devices can be compared at a glance.
function RssiSpark({ h, w = 60, ht = 16 }: { h?: number[]; w?: number; ht?: number }) {
  if (!h?.length) return null;
  const n = 60,
    x = (i: number) => ((n - h.length + i) / (n - 1)) * w,
    y = (v: number) => ht - Math.max(0, Math.min(1, (v + 100) / 70)) * ht;
  const pts = h.map((v, i) => `${x(i).toFixed(1)},${y(v).toFixed(1)}`).join(" ");
  return (
    <svg width={w} height={ht} viewBox={`0 0 ${w} ${ht}`} style={{ display: "block" }}>
      <polyline points={pts} fill="none" stroke="#6cb4ee" strokeWidth={1.2} />
    </svg>
  );
}
const rssiCls = (r: number | null) => (r == null ? "" : r >= -60 ? "on" : r >= -80 ? "warn" : "bad");

export default function Ble({ st }: PanelProps<BleState>) {
  const b = useBob();
  const prefKey = b.key("ble-prefs");
  const [prefs, setPrefsState] = useState<BlePrefs>(() => ({ ...PREF_DEFAULTS, ...readLocal<Partial<BlePrefs>>(prefKey, {}) }));
  const prefsRef = useRef(prefs);
  prefsRef.current = prefs;
  const setPrefs = (p: Partial<BlePrefs>) => {
    const n = { ...prefsRef.current, ...p };
    prefsRef.current = n;
    writeLocal(prefKey, JSON.stringify(n));
    setPrefsState(n);
  };
  const [snap, setSnap] = useState<{ devices: BleDevice[] } | null>(null);
  const [open, setOpen] = useState<Set<string>>(new Set());
  const ws = useRef<Ws | null>(null);
  const cs = st.controllers || [];
  const def = cs.find((c) => c.address && c.address === st.defaultController) || cs.find((c) => c.name === st.defaultController) || cs[0];
  const [ctrl, setCtrl] = useState<string | undefined>(def?.name);
  const ctrlRef = useRef(ctrl);
  ctrlRef.current = ctrl ?? def?.name;

  useEffect(() => {
    const s = b.conn.ws("/ws/ble", {
      onopen: () => s.send({ live: prefsRef.current.live }),
      onmessage: (m) => {
        if (m?.type === "devices") setSnap(m);
        else if (m?.type === "error") b.toast(m.message, "error");
      },
    });
    ws.current = s;
    return () => {
      s.close();
      ws.current = null;
    };
  }, [b.conn]);

  const call = (r: readonly [string, string, unknown?]) => b.api(r[0], r[1], r[2]).catch(() => {});

  const sc = st.scan;
  const sorters: Record<string, (a: BleDevice, b: BleDevice) => number> = {
    rssi: (a, c) => (c.rssi ?? -999) - (a.rssi ?? -999),
    seen: (a, c) => c.lastSeen - a.lastSeen,
    first: (a, c) => c.firstSeen - a.firstSeen,
    name: (a, c) => (a.name || "￿").localeCompare(c.name || "￿"),
  };
  const matches = (d: BleDevice) => {
    if (prefs.minRssi !== "" && (d.rssi == null || d.rssi < +prefs.minRssi)) return false;
    if (prefs.beacons && !d.beacons.length) return false;
    const q = prefs.q.trim().toLowerCase();
    return !q || [d.name, d.addr, d.addrType, d.kind].some((v) => (v || "").toLowerCase().includes(q));
  };
  const all = snap?.devices || [];
  const list = all.filter(matches).sort(sorters[prefs.sort] || sorters.rssi);

  return (
    <>
      <div className="pr-row">
        {!cs.length ? (
          <span className="pr-pill bad">No Bluetooth controller</span>
        ) : (
          <>
            <label>
              Controller{" "}
              <select
                value={ctrl ?? def?.name}
                onChange={(e) => {
                  setCtrl(e.currentTarget.value);
                  call(bleApi.settings({ controller: e.currentTarget.value }));
                }}
              >
                {cs.map((c) => (
                  <option key={c.name} value={c.name}>
                    {c.name} · {c.bus}
                  </option>
                ))}
              </select>
            </label>
            {cs.map((c) => (
              <span key={c.name} className="pr-note">
                {c.name} {c.address || ""}{" "}
                <span className={`pr-pill ${c.powered ? "on" : c.rfkill?.soft || c.rfkill?.hard ? "warn" : ""}`}>{c.rfkill?.hard ? "hard-blocked" : c.rfkill?.soft ? "rfkill blocked" : c.powered ? "powered" : c.powerState || "off"}</span>
              </span>
            ))}
            <label>
              <input type="checkbox" checked={st.restoreRadio !== false} onChange={(e) => call(bleApi.settings({ restoreRadio: e.currentTarget.checked }))} /> Put radio back to off/blocked after scanning
            </label>
          </>
        )}
      </div>
      <div className="pr-row">
        <button className="btn btn-sm btn-primary" onClick={() => call(bleApi.scan(10, ctrlRef.current))}>
          Scan 10 s
        </button>
        <button className="btn btn-sm" onClick={() => call(bleApi.scan(60, ctrlRef.current))}>
          Scan 60 s
        </button>
        <label>
          <input
            type="checkbox"
            checked={prefs.live}
            onChange={(e) => {
              setPrefs({ live: e.currentTarget.checked });
              ws.current?.send({ live: e.currentTarget.checked, controller: ctrlRef.current });
            }}
          />{" "}
          Live while open
        </label>
        <button
          className="btn btn-sm btn-danger"
          onClick={async () => {
            if (prefsRef.current.live) {
              setPrefs({ live: false });
              ws.current?.send({ live: false });
            }
            await call(bleApi.stop());
          }}
        >
          Stop
        </button>
        <button className="btn btn-sm" onClick={() => call(bleApi.clear())}>
          Clear list
        </button>
      </div>
      <div className="pr-row pr-note">
        {sc ? (
          <>
            <span className="pr-pill on">
              {sc.stopping ? "Stopping" : "Scanning"} {sc.controller}
            </span>{" "}
            {sc.live ? "live while this page is open" : sc.until ? Math.max(0, Math.round((sc.until - Date.now()) / 1000)) + " s left" : ""} · {sc.packets} adverts
          </>
        ) : (
          <>
            <span className="pr-pill">Idle</span>
            {st.lastScan ? ` last scan ${fmtAgo(st.lastScan.ended)} on ${st.lastScan.ctrl}, ${st.lastScan.packets} adverts` : ""}
          </>
        )}
        {` · ${st.recent || 0} heard in the last 5 min`}
        {st.error && <span className="pr-pill bad">{st.error}</span>}
      </div>
      <div className="pr-row">
        <input type="search" placeholder="Filter name, address, type" value={prefs.q} onInput={(e) => setPrefs({ q: e.currentTarget.value })} style={{ flex: 1, minWidth: 140 }} />
        <select value={prefs.minRssi} onChange={(e) => setPrefs({ minRssi: e.currentTarget.value })}>
          {["", "-60", "-70", "-80", "-90"].map((v) => (
            <option key={v} value={v}>
              {v === "" ? "Any signal" : "≥ " + v + " dBm"}
            </option>
          ))}
        </select>
        <select value={prefs.sort} onChange={(e) => setPrefs({ sort: e.currentTarget.value as BlePrefs["sort"] })}>
          {[
            ["rssi", "Strongest"],
            ["seen", "Last seen"],
            ["first", "Newest"],
            ["name", "Name"],
          ].map(([v, l]) => (
            <option key={v} value={v}>
              {l}
            </option>
          ))}
        </select>
        <label>
          <input type="checkbox" checked={prefs.beacons} onChange={(e) => setPrefs({ beacons: e.currentTarget.checked })} /> Beacons/sensors only
        </label>
      </div>
      {snap &&
        (!all.length ? (
          <div className="pr-note">No advertisers yet. Start a scan.</div>
        ) : (
          <>
            <div className="pr-note">
              {list.length} of {all.length} shown · tap a row for details
            </div>
            <table className="pr-tbl">
              <thead>
                <tr>
                  <th>Signal</th>
                  <th>Device</th>
                  <th>What it is</th>
                </tr>
              </thead>
              <tbody>
                {list.map((d) => (
                  <Fragment key={d.addr}>
                    <tr
                      style={{ cursor: "pointer" }}
                      onClick={() =>
                        setOpen((o) => {
                          const n = new Set(o);
                          n.has(d.addr) ? n.delete(d.addr) : n.add(d.addr);
                          return n;
                        })
                      }
                    >
                      <td style={{ whiteSpace: "nowrap" }}>
                        <span className={`pr-pill ${rssiCls(d.rssi)}`}>{d.rssi ?? "–"} dBm</span>
                        <RssiSpark h={d.rssiHist} />
                      </td>
                      <td style={{ wordBreak: "break-word" }}>
                        <b>{d.name || "(no name)"}</b>
                        <br />
                        <span className="pr-endpoint">{d.addr}</span>{" "}
                        <span className="pr-note">
                          {d.addrType}
                          {d.connectable ? " · connectable" : ""} · {fmtAgo(d.lastSeen)}
                        </span>
                      </td>
                      <td style={{ wordBreak: "break-word" }}>{d.kind || ""}</td>
                    </tr>
                    {open.has(d.addr) && (
                      <tr className="ble-detail">
                        <td colSpan={3}>
                          <Detail d={d} />
                        </td>
                      </tr>
                    )}
                  </Fragment>
                ))}
              </tbody>
            </table>
          </>
        ))}
    </>
  );
}

function KV({ k, children }: { k: string; children: ReactNode }) {
  return (
    <div>
      <span className="pr-note">{k}:</span> {children}
    </div>
  );
}

function Detail({ d }: { d: BleDevice }) {
  const brk = { wordBreak: "break-all" } as const;
  return (
    <div className="pr-card" style={{ marginTop: 0 }}>
      <KV k="First / last seen">
        {new Date(d.firstSeen).toLocaleTimeString()} / {new Date(d.lastSeen).toLocaleTimeString()} · {d.count} packets via {d.controller}
      </KV>
      <KV k="PDU">
        {d.lastPdu}
        {d.connectable ? " · connectable" : " · not connectable"}
        {d.scannable ? " · scannable" : ""}
      </KV>
      {d.txPower != null && (
        <KV k="TX power">
          {d.txPower} dBm{d.rssi != null ? ` (path loss ${d.txPower - d.rssi} dB)` : ""}
        </KV>
      )}
      {d.flags != null && (
        <KV k="Flags">
          {fmtHex(d.flags)}
          {(
            [
              [1, "LE limited"],
              [2, "LE general"],
              [4, "no BR/EDR"],
            ] as [number, string][]
          )
            .filter(([bit]) => d.flags! & bit)
            .map(([, l]) => " · " + l)
            .join("")}
        </KV>
      )}
      {d.appearance != null && <KV k="Appearance">{fmtHex(d.appearance, 4)}</KV>}
      {d.beacons.map((bc, i) => (
        <Fragment key={i}>
          <KV k={bc.kind}>
            <b>{bc.label}</b>
          </KV>
          {bc.fields && (
            <div className="pr-note" style={{ marginLeft: 12 }}>
              {Object.entries(bc.fields)
                .map(([k, v]) => `${k} = ${typeof v === "object" ? JSON.stringify(v) : v}`)
                .join(" · ")}
            </div>
          )}
        </Fragment>
      ))}
      {d.mfr.map((m, i) => (
        <KV key={"m" + i} k="Manufacturer">
          {m.companyName || "unknown"} {fmtHex(m.company, 4)}{" "}
          <span className="pr-endpoint" style={brk}>
            {m.data}
          </span>
        </KV>
      ))}
      {d.uuids.length > 0 && <KV k="Services">{d.uuids.map((u) => u.uuid + (u.name ? ` (${u.name})` : "")).join(", ")}</KV>}
      {d.svc.map((s, i) => (
        <KV key={"s" + i} k="Service data">
          {s.uuid}
          {s.name ? ` (${s.name})` : ""}{" "}
          <span className="pr-endpoint" style={brk}>
            {s.data}
          </span>
        </KV>
      ))}
      {d.adv && (
        <KV k="Raw adv">
          <span className="pr-endpoint" style={brk}>
            {d.adv}
          </span>
        </KV>
      )}
      {d.scanRsp && (
        <KV k="Raw scan rsp">
          <span className="pr-endpoint" style={brk}>
            {d.scanRsp}
          </span>
        </KV>
      )}
    </div>
  );
}
