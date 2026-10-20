// Wi-Fi charts: per-band channel occupancy (a trapezoid "hump" per BSS across its whole,
// possibly bonded, channel) and signal over time.

import type { ReactNode } from "react";
import { wifiColor } from "../../lib/format";
import type { Bss, Channel } from "./api";

export const bandName: Record<string, string> = { "2.4": "2.4 GHz", "5": "5 GHz", "6": "6 GHz", "60": "60 GHz" };
const hue = (s: string) => {
  let h = 0;
  for (const c of s || "?") h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return h % 360;
};
export const ssidColor = (b: Bss) => (b.hidden ? "#7a7f94" : `hsl(${hue(b.ssid)},70%,62%)`);
export const ssidLabel = (b: Bss) => (b.hidden ? "(hidden)" : b.ssid);
export const chOf = (f: number) => (f === 2484 ? 14 : f < 2500 ? (f - 2407) / 5 : f === 5935 ? 2 : f >= 5950 ? (f - 5950) / 5 : (f - 5000) / 5);

/** Bar sparkline of a BSS's signal: [t, dBm] pairs or plain numbers, −95..−30 dBm. */
export function WSpark({ hist, w = 54, h = 14 }: { hist: ([number, number] | number)[]; w?: number; h?: number }) {
  const d = (hist || []).slice(-30);
  if (!d.length) return null;
  const bw = w / 30;
  return (
    <svg width={w} height={h} viewBox={`0 0 ${w} ${h}`} style={{ display: "inline-block", verticalAlign: "middle" }}>
      {d.map((p, i) => {
        const v = Array.isArray(p) ? p[1] : p;
        const bh = Math.max(1, Math.min(1, Math.max(0, (v + 95) / 65)) * h);
        return <rect key={i} x={(30 - d.length + i) * bw} y={h - bh} width={Math.max(bw - 0.4, 0.5)} height={bh} fill={wifiColor(v)} rx={0.4} />;
      })}
    </svg>
  );
}

// x is frequency (MHz) mapped through segments, y is signal (-95..-25 dBm).
const SEGS: Record<string, { a: number; b: number; name: string; dfs?: boolean }[]> = {
  "2.4": [{ a: 2400, b: 2495, name: "" }],
  "5": [
    { a: 5170, b: 5250, name: "U-NII-1" },
    { a: 5250, b: 5330, name: "U-NII-2A", dfs: true },
    { a: 5490, b: 5730, name: "U-NII-2C", dfs: true },
    { a: 5735, b: 5835, name: "U-NII-3" },
    { a: 5835, b: 5895, name: "U-NII-4" },
  ],
  "6": [
    { a: 5925, b: 6425, name: "U-NII-5" },
    { a: 6425, b: 6525, name: "U-NII-6" },
    { a: 6525, b: 6875, name: "U-NII-7" },
    { a: 6875, b: 7125, name: "U-NII-8" },
  ],
};

export function OccupancyChart({ band, list, radioChans }: { band: string; list: Bss[]; radioChans: Channel[] }) {
  const VW = 640,
    top = 22,
    bottom = 26,
    H = 190,
    left = 30,
    right = 8,
    gap = 10;
  let segs = SEGS[band];
  if (band === "5" && !list.some((b) => b.freq > 5845) && !radioChans.some((c) => c.mhz > 5845 && !c.disabled)) segs = segs.slice(0, 4);
  const total = segs.reduce((n, s) => n + (s.b - s.a), 0);
  const pxPerMHz = (VW - left - right - gap * (segs.length - 1)) / total;
  const xs: number[] = [];
  let x0 = left;
  for (const s of segs) {
    xs.push(x0);
    x0 += (s.b - s.a) * pxPerMHz + gap;
  }
  const fx = (f: number) => {
    for (let i = 0; i < segs.length; i++) if (f <= segs[i].b || i === segs.length - 1) return xs[i] + (Math.max(segs[i].a, Math.min(segs[i].b, f)) - segs[i].a) * pxPerMHz;
    return left;
  };
  const fy = (dbm: number) => top + (1 - Math.max(0, Math.min(1, (dbm + 95) / 70))) * (H - top - bottom);
  const yb = H - bottom;
  const g: ReactNode[] = [];
  segs.forEach((s, i) => {
    const w = (s.b - s.a) * pxPerMHz;
    g.push(<rect key={"s" + i} x={xs[i]} y={top} width={w} height={yb - top} fill={s.dfs ? "rgba(245,166,35,.06)" : "rgba(255,255,255,.015)"} />);
    if (s.name)
      g.push(
        <text key={"sn" + i} x={xs[i] + w / 2} y={top - 8} fill={s.dfs ? "#f5a623" : "#7a7f94"} fontSize={9} textAnchor="middle">
          {s.name}
          {s.dfs ? " · DFS" : ""}
        </text>,
      );
  });
  for (const d of [-90, -80, -70, -60, -50, -40, -30])
    g.push(
      <g key={"y" + d}>
        <line x1={left} x2={VW - right} y1={fy(d)} y2={fy(d)} stroke="#2a2f40" strokeWidth={0.5} />
        <text x={left - 3} y={fy(d) + 3} fill="#7a7f94" fontSize={8} textAnchor="end">
          {d}
        </text>
      </g>,
    );
  // Channel ticks, greyed where the regulatory domain disables them for this radio.
  const ticks = band === "2.4" ? [...Array(14)].map((_, i) => i + 1) : band === "5" ? [36, 40, 44, 48, 52, 56, 60, 64, 100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144, 149, 153, 157, 161, 165, 169, 173, 177] : [1, 17, 33, 49, 65, 81, 97, 113, 129, 145, 161, 177, 193, 209, 225];
  for (const ch of ticks) {
    const f = band === "2.4" ? (ch === 14 ? 2484 : 2407 + ch * 5) : band === "5" ? 5000 + ch * 5 : 5950 + ch * 5;
    if (!segs.some((s) => f >= s.a && f <= s.b)) continue;
    const rc = radioChans.find((c) => c.ch === ch);
    const off = radioChans.length && (!rc || rc.disabled);
    const x = fx(f);
    g.push(
      <g key={"t" + ch}>
        <line x1={x} x2={x} y1={yb} y2={yb + 3} stroke="#7a7f94" />
        <text x={x} y={yb + 13} fill={off ? "#454a5c" : rc?.radar ? "#f5a623" : "#aab"} fontSize={band === "2.4" ? 9 : 8} textAnchor="middle" textDecoration={off ? "line-through" : undefined}>
          {ch}
        </text>
      </g>,
    );
  }
  g.push(
    <text key="axis" x={VW - right} y={H - 3} fill="#7a7f94" fontSize={8} textAnchor="end">
      channel · dBm
    </text>,
  );
  // Humps, weakest first so strong networks draw on top.
  const sorted = [...list].sort((a, b) => (a.signal ?? -100) - (b.signal ?? -100));
  for (const b of sorted) {
    if (b.signal == null) continue;
    const w = b.width === "80+80" ? 80 : b.width;
    const xa = fx(b.centerFreq - w / 2),
      xz = fx(b.centerFreq + w / 2),
      yp = fy(b.signal),
      sl = Math.min(6, (xz - xa) / 4);
    const c = ssidColor(b);
    g.push(<path key={"h" + b.bssid} d={`M${xa},${yb} L${xa + sl},${yp} L${xz - sl},${yp} L${xz},${yb}`} fill={c} fillOpacity={0.08} stroke={c} strokeWidth={b.associated ? 2 : 1.1} strokeOpacity={0.9} strokeDasharray={b.hidden ? "3,2" : undefined} />);
  }
  // Label the strongest BSS per SSID+centre, the 10 strongest, staggered to limit overlaps.
  const seen = new Set<string>();
  const labels: Bss[] = [];
  for (const b of [...sorted].reverse()) {
    const k = ssidLabel(b) + "@" + b.centerFreq;
    if (seen.has(k) || b.signal == null) continue;
    seen.add(k);
    labels.push(b);
    if (labels.length >= 10) break;
  }
  // Stack labels down from each hump's top, testing real text extents (~5.3 px per char at
  // 9 px). Where a crowded channel runs out of room above the axis, the rest fold into a
  // "+N more" note instead of spilling over the channel numbers.
  const placed: { x: number; y: number; half: number }[] = [];
  const more = new Map<number, number>();
  for (const b of labels) {
    const x = fx(b.centerFreq);
    const text = ssidLabel(b).slice(0, 22);
    const half = (text.length * 5.3) / 2;
    let y = fy(b.signal!) - 3;
    while (placed.some((p) => Math.abs(p.x - x) < p.half + half + 4 && Math.abs(p.y - y) < 10)) y += 10;
    if (y > yb - 4) {
      const k = Math.round(x);
      more.set(k, (more.get(k) || 0) + 1);
      continue;
    }
    placed.push({ x, y, half });
    g.push(
      <text key={"l" + b.bssid} x={x} y={y} fill={ssidColor(b)} fontSize={9} textAnchor="middle">
        {text}
      </text>,
    );
  }
  for (const [x, n] of more) {
    const t = Math.min(...placed.filter((p) => Math.abs(p.x - x) < 2).map((p) => p.y), yb);
    g.push(
      <text key={"m" + x} x={x} y={Math.max(t - 11, 10)} fill="#7a7f94" fontSize={8} textAnchor="middle">
        +{n} more
      </text>,
    );
  }
  return (
    <svg className="wf-chart" viewBox={`0 0 ${VW} ${H}`} role="img" aria-label={`${bandName[band]} channel occupancy`}>
      {g}
      {!list.length && (
        <text x={VW / 2} y={(top + yb) / 2} fill="#7a7f94" fontSize={11} textAnchor="middle">
          nothing seen on {bandName[band]}
        </text>
      )}
    </svg>
  );
}

// Signal over time: one line per BSS (the 12 strongest seen in the window, after the band
// filter), coloured by SSID like the humps. A line breaks where a BSS went unseen for more
// than ~2.5 scan intervals, so a dropout reads as a gap rather than a slope.
export function HistoryChart(p: { bss: Bss[]; band: string; win: number; setWin(w: number): void; ivS: number; hl: string; pinned: string; setHover(b: string): void; togglePin(b: string): void }) {
  const VW = 640,
    H = 200,
    top = 10,
    bottom = 22,
    left = 30,
    right = 8;
  const now = Date.now(),
    span = p.win * 60000,
    t0 = now - span;
  const gapMs = Math.max(90000, p.ivS * 2500);
  const cur = (b: Bss) => (now - b.lastSeen < gapMs ? b.signal ?? -200 : -300); // gone ones sort last
  const list = p.bss
    .filter((b) => (!p.band || b.band === p.band) && b.history.some((x) => x[0] >= t0))
    .sort((a, b) => cur(b) - cur(a) || Math.max(...b.history.map((x) => x[1])) - Math.max(...a.history.map((x) => x[1])))
    .slice(0, 12);
  const fx = (t: number) => left + ((t - t0) / span) * (VW - left - right);
  const fy = (dbm: number) => top + (1 - Math.max(0, Math.min(1, (dbm + 95) / 70))) * (H - top - bottom);
  const g: ReactNode[] = [];
  for (const d of [-90, -80, -70, -60, -50, -40, -30])
    g.push(
      <g key={"y" + d}>
        <line x1={left} x2={VW - right} y1={fy(d)} y2={fy(d)} stroke="#2a2f40" strokeWidth={0.5} />
        <text x={left - 3} y={fy(d) + 3} fill="#7a7f94" fontSize={8} textAnchor="end">
          {d}
        </text>
      </g>,
    );
  const step = p.win <= 10 ? 2 : p.win <= 30 ? 5 : 10;
  for (let m = 0; m <= p.win; m += step) {
    const x = fx(now - m * 60000);
    g.push(
      <g key={"x" + m}>
        <line x1={x} x2={x} y1={top} y2={H - bottom} stroke="#2a2f40" strokeWidth={0.5} />
        <text x={x} y={H - bottom + 12} fill="#7a7f94" fontSize={8} textAnchor={m === 0 ? "end" : m === p.win ? "start" : "middle"}>
          {m ? `−${m} min` : "now"}
        </text>
      </g>,
    );
  }
  for (const b of [...list].reverse()) {
    // strongest drawn last, on top
    const c = ssidColor(b),
      on = !p.hl || p.hl === b.bssid;
    const pts = b.history.filter((x) => x[0] >= t0);
    const runs: [number, number][][] = [[]];
    pts.forEach((x, i) => {
      if (i && x[0] - pts[i - 1][0] > gapMs) runs.push([]);
      runs[runs.length - 1].push(x);
    });
    const sw = (b.associated ? 2.2 : 1.3) * (p.hl === b.bssid ? 1.6 : 1);
    const op = on ? 0.95 : 0.15;
    runs.forEach((r, i) => {
      if (r.length === 1) g.push(<circle key={b.bssid + i} cx={fx(r[0][0]).toFixed(1)} cy={fy(r[0][1]).toFixed(1)} r={sw} fill={c} fillOpacity={op} />);
      else if (r.length)
        g.push(<polyline key={b.bssid + i} points={r.map((x) => `${fx(x[0]).toFixed(1)},${fy(x[1]).toFixed(1)}`).join(" ")} fill="none" stroke={c} strokeWidth={sw} strokeOpacity={op} strokeLinejoin="round" strokeDasharray={b.hidden ? "3,2" : undefined} />);
    });
  }
  if (!list.length)
    g.push(
      <text key="empty" x={VW / 2} y={H / 2} fill="#7a7f94" fontSize={11} textAnchor="middle">
        No samples in the last {p.win} min yet. While this panel is open it scans every {p.ivS} s.
      </text>,
    );
  return (
    <>
      <div className="pr-note" style={{ marginTop: 10, display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
        <b style={{ color: "var(--text)" }}>Signal over time</b>
        {p.band ? ` · ${bandName[p.band]}` : ""}
        {list.length ? ` · ${list.length} strongest` : ""}
        <select value={p.win} onChange={(e) => p.setWin(+e.currentTarget.value)}>
          {[10, 30, 60].map((v) => (
            <option key={v} value={v}>
              last {v} min
            </option>
          ))}
        </select>
      </div>
      <div className="wf-scroll">
        <svg className="wf-chart" viewBox={`0 0 ${VW} ${H}`} role="img" aria-label="Signal strength over time">
          {g}
        </svg>
      </div>
      <div className="wf-leg">
        {list.map((b) => (
          <span
            key={b.bssid}
            className={`${p.hl && p.hl !== b.bssid ? "dim" : ""}${p.pinned === b.bssid ? " pin" : ""}`}
            role="button"
            onPointerEnter={(e) => e.pointerType === "mouse" && p.setHover(b.bssid)}
            onPointerLeave={(e) => e.pointerType === "mouse" && p.setHover("")}
            onClick={() => p.togglePin(b.bssid)}
          >
            <span style={{ color: ssidColor(b) }}>━</span> {ssidLabel(b)}{" "}
            <span className="wf-dim">
              ch {b.channel} · {now - b.lastSeen < gapMs ? `${b.signal} dBm` : "gone"} · {b.bssid}
            </span>
          </span>
        ))}
      </div>
      {list.length > 0 && <div className="pr-note">Tap a network (here or in the table) to highlight its line; tap again to clear.</div>}
    </>
  );
}
