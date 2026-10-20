// Sparklines, as the old dashboard drew them: fixed-slot bar histograms for status
// history (newest on the right) and min–max scaled polylines for sensor readings.

import { HIST_N } from "../lib/store";
import { wifiColor } from "../lib/format";

interface BarsProps {
  data: number[] | undefined;
  w: number;
  h: number;
  color: string;
  /** Fixed full-scale value; otherwise the data's own maximum. */
  max?: number;
  /** log1p scaling, for rates that span orders of magnitude. */
  log?: boolean;
  slots?: number;
}

const svgProps = (w: number, h: number) => ({ width: w, height: h, viewBox: `0 0 ${w} ${h}`, preserveAspectRatio: "none", style: { display: "block" } as const });

export function Bars({ data = [], w, h, color, max, log, slots = HIST_N }: BarsProps) {
  if (!data.length) return <svg width={w} height={h} />;
  const mx = max || Math.max(...data, 0.001);
  const lmx = Math.log1p(mx);
  const bw = w / slots;
  return (
    <svg {...svgProps(w, h)}>
      {data.map((v, i) => {
        const frac = log ? (lmx > 0 ? Math.log1p(v) / lmx : 0) : v / mx;
        const bh = Math.max(v > 0 ? 1 : 0, frac * h);
        return <rect key={i} x={(slots - data.length + i) * bw} y={h - bh} width={Math.max(bw - 0.5, 0.5)} height={bh} fill={color} rx={0.5} />;
      })}
    </svg>
  );
}

/** Memory: percentage bars, with the change from one sample to the next drawn over them in red. */
export function MemBars({ data = [], w, h, color }: { data?: number[]; w: number; h: number; color: string }) {
  if (!data.length) return <svg width={w} height={h} />;
  const bw = w / HIST_N;
  const dv = data.slice(1).map((v, i) => Math.abs(v - data[i]));
  const dMax = Math.max(...dv, 0.001);
  return (
    <svg {...svgProps(w, h)}>
      {data.map((v, i) => {
        const bh = Math.max(v > 0 ? 1 : 0, (v / 100) * h);
        return <rect key={"b" + i} x={(HIST_N - data.length + i) * bw} y={h - bh} width={Math.max(bw - 0.5, 0.5)} height={bh} fill={color} opacity={0.7} rx={0.5} />;
      })}
      {dv.map((v, i) => {
        const bh = Math.max(v > 0 ? 1 : 0, (v / dMax) * h);
        return <rect key={"d" + i} x={(HIST_N - data.length + 1 + i) * bw} y={h - bh} width={Math.max(bw - 0.5, 0.5)} height={bh} fill="#ff5c5c" opacity={0.8} rx={0.5} />;
      })}
    </svg>
  );
}

/** Signal in dBm on a fixed window (lo..lo+span), each bar coloured by quality. */
export function SignalBars({ data = [], w, h, lo = -90, span = 60, slots = HIST_N, inline }: { data?: number[]; w: number; h: number; lo?: number; span?: number; slots?: number; inline?: boolean }) {
  if (!data.length) return inline ? null : <svg width={w} height={h} />;
  const bw = w / slots;
  const d = data.slice(-slots);
  return (
    <svg width={w} height={h} viewBox={`0 0 ${w} ${h}`} style={inline ? { display: "inline-block", verticalAlign: "middle" } : { display: "block" }}>
      {d.map((v, i) => {
        const bh = Math.max(1, Math.min(1, Math.max(0, (v - lo) / span)) * h);
        return <rect key={i} x={(slots - d.length + i) * bw} y={h - bh} width={Math.max(bw - 0.5, 0.5)} height={bh} fill={wifiColor(v)} rx={0.5} />;
      })}
    </svg>
  );
}

/** Min–max scaled line (sensor readings). */
export function Line({ data, w = 90, h = 22, color = "var(--accent)", inline }: { data: number[] | undefined; w?: number; h?: number; color?: string; inline?: boolean }) {
  if (!data || data.length < 2) return null;
  const lo = Math.min(...data),
    hi = Math.max(...data),
    span = hi - lo || 1;
  const pts = data.map((v, i) => `${((i / (data.length - 1)) * w).toFixed(1)},${(h - 2 - ((v - lo) / span) * (h - 4)).toFixed(1)}`).join(" ");
  return (
    <svg width={w} height={h} viewBox={`0 0 ${w} ${h}`} style={inline ? { display: "inline-block", verticalAlign: "middle" } : { display: "block" }}>
      <polyline points={pts} fill="none" stroke={color} strokeWidth={1.5} />
    </svg>
  );
}
