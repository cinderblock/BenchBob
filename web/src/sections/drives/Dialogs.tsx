// Drive dialogs: erase, raw probe, deep scan results.

import { useState } from "react";
import { useBob } from "../../bob/context";
import { fmtSz } from "../../lib/format";
import type { Operation } from "../../lib/types";
import type { ProbeResult } from "./api";

export function EraseDialog({ dev, model, size, onDone }: { dev: string; model: string; size: number; onDone(mode: string | null): void }) {
  const [mode, setMode] = useState("quick");
  const [text, setText] = useState("");
  return (
    <>
      <h3 style={{ color: "var(--danger)" }}>Erase Device</h3>
      <p>Permanently destroy all data on:</p>
      <div className="modal-dev">
        {model}
        <br />
        /dev/{dev} · {fmtSz(size)}
      </div>
      <div className="radio-group">
        {[
          ["quick", "Quick", "clear partition table (instant)"],
          ["full", "Full", "write zeros (slow)"],
          ["discard", "Discard", "TRIM for SSDs (fast)"],
        ].map(([v, t, d]) => (
          <label key={v}>
            <input type="radio" name="em" value={v} checked={mode === v} onChange={() => setMode(v)} /> <strong>{t}</strong> — {d}
          </label>
        ))}
      </div>
      <div className="confirm-box">
        <label htmlFor="ec">Type ERASE to confirm:</label>
        <input type="text" id="ec" autoComplete="off" autoFocus value={text} onInput={(e) => setText(e.currentTarget.value)} />
      </div>
      <div className="modal-btns">
        <button className="btn" onClick={() => onDone(null)}>
          Cancel
        </button>
        <button className="btn btn-danger" disabled={text !== "ERASE"} onClick={() => text === "ERASE" && onDone(mode)}>
          Erase
        </button>
      </div>
    </>
  );
}

// A filesystem reporting "empty" says nothing about what is still on the media. Probe
// samples raw blocks; Deep Scan reads everything.
export const KIND_COLORS: Record<string, { c: string; label: string }> = {
  zero: { c: "#39404f", label: "zeroed" },
  ones: { c: "#7a5c2e", label: "0xFF (erased flash)" },
  text: { c: "#4ea1ff", label: "text" },
  compressed: { c: "#ff9f43", label: "compressed / encrypted" },
  structured: { c: "#5ad18c", label: "structured data" },
};

export function ProbeDialog({ r, onDeepScan }: { r: ProbeResult; onDeepScan(): void }) {
  const { closeModal } = useBob();
  const [tap, setTap] = useState<number | null>(null);
  const total = r.sampledBlocks || 1;
  const gaps = r.unallocated || { ranges: [], sampled: 0, withData: 0 };
  const gapBytes = gaps.ranges.reduce((a, g) => a + g.bytes, 0);
  const gapWarn = gaps.withData > 0;
  const tapped = tap != null ? r.map[tap] : null;
  return (
    <>
      <h3>Raw Probe — /dev/{r.device}</h3>
      <p style={{ fontSize: ".8rem", color: "var(--dim)", marginTop: -4 }}>
        Sampled {r.sampledBlocks} blocks ({fmtSz(r.bytesRead)}) across {fmtSz(r.sizeBytes)} — <b style={{ color: "var(--text)" }}>{r.emptyPct}% empty</b>
      </p>
      <div style={{ display: "flex", height: 14, borderRadius: 4, overflow: "hidden", margin: "10px 0 6px" }}>
        {Object.entries(KIND_COLORS).map(([k, v]) => (r.counts[k] ? <div key={k} style={{ width: `${((r.counts[k] / total) * 100).toFixed(2)}%`, background: v.c }} /> : null))}
      </div>
      <div style={{ fontSize: ".72rem", color: "var(--dim)", marginBottom: 12 }}>
        {Object.entries(KIND_COLORS)
          .filter(([k]) => r.counts[k])
          .map(([k, v]) => (
            <span key={k} style={{ display: "inline-flex", alignItems: "center", gap: 5, marginRight: 12, whiteSpace: "nowrap" }}>
              <i style={{ width: 9, height: 9, borderRadius: 2, background: v.c, display: "inline-block" }} />
              {v.label} <b>{((r.counts[k] / total) * 100).toFixed(0)}%</b> <span>({r.counts[k]} blocks)</span>
            </span>
          ))}
      </div>
      <div style={{ fontSize: ".72rem", color: "var(--dim)", marginBottom: 3 }}>Layout (start → end of device). Tap a sample for its offset.</div>
      {/* Device-order strip: each sample in place, so clusters of data are visible. */}
      <div className="probe-strip" style={{ display: "flex", height: 22, gap: 1, borderRadius: 3, overflow: "hidden", marginBottom: 4 }}>
        {r.map.map((m, i) => (
          <i key={i} className={tap === i ? "on" : ""} style={{ flex: 1, minWidth: 1, background: KIND_COLORS[m.k]?.c || "#555" }} onClick={() => setTap(tap === i ? null : i)} />
        ))}
      </div>
      <div style={{ fontSize: ".72rem", color: "var(--dim)", minHeight: "1.2em", marginBottom: 8 }}>{tapped && `${fmtSz(tapped.o)} — ${KIND_COLORS[tapped.k]?.label || tapped.k}, entropy ${tapped.e}`}</div>
      <div style={{ fontSize: ".78rem", marginBottom: 10, padding: "8px 10px", borderRadius: 5, background: gapWarn ? "rgba(255,159,67,.12)" : "rgba(255,255,255,.04)" }}>
        <b>Outside any partition:</b> {gaps.ranges.length} region(s), {fmtSz(gapBytes)} — {gaps.sampled} block(s) sampled, <b style={{ color: gapWarn ? "#ff9f43" : "inherit" }}>{gaps.withData} with data</b>
        {gapWarn && <div style={{ color: "var(--dim)", marginTop: 3 }}>Leftover content from a previous image is still readable here.</div>}
      </div>
      <div style={{ fontSize: ".72rem", color: "var(--dim)", marginBottom: 3 }}>Signatures found</div>
      {!r.signatures.length ? (
        <div style={{ color: "var(--dim)", fontSize: ".8rem" }}>No known file signatures in the sampled blocks.</div>
      ) : (
        <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
          {r.signatures.map((s) => (
            <div key={s.name} style={{ display: "flex", justifyContent: "space-between", gap: 10, fontSize: ".78rem" }}>
              <span>{s.name}</span>
              <span style={{ color: "var(--dim)", fontFamily: "monospace" }}>
                {s.offsets
                  .slice(0, 4)
                  .map((o) => fmtSz(o))
                  .join(", ")}
                {s.offsets.length > 4 ? " …" : ""}
              </span>
            </div>
          ))}
        </div>
      )}
      <div className="modal-btns" style={{ marginTop: 14 }}>
        <button className="btn" onClick={closeModal}>
          Close
        </button>
        <button className="btn btn-primary" onClick={onDeepScan}>
          Deep Scan
        </button>
      </div>
    </>
  );
}

export function ScanResultDialog({ dev, res }: { dev: string; res: NonNullable<Operation["scanResult"]> }) {
  const { closeModal } = useBob();
  return (
    <>
      <h3>Deep Scan — /dev/{dev}</h3>
      <p style={{ fontSize: ".8rem", color: "var(--dim)", marginTop: -4 }}>
        Read {fmtSz(res.bytesScanned)} · {res.signatures.length} signature type(s)
      </p>
      <div style={{ maxHeight: "min(50vh,420px)", overflow: "auto", marginTop: 10 }}>
        {res.signatures.length ? (
          res.signatures.map((s) => (
            <div key={s.name} style={{ display: "flex", justifyContent: "space-between", gap: 10, fontSize: ".78rem", padding: "3px 0", borderBottom: "1px solid rgba(255,255,255,.06)" }}>
              <span>{s.name}</span>
              <span style={{ color: "var(--dim)", whiteSpace: "nowrap" }}>
                <b style={{ color: "var(--text)" }}>{s.count}</b> ·{" "}
                <span style={{ fontFamily: "monospace" }}>
                  {s.first
                    .slice(0, 3)
                    .map((o) => fmtSz(o))
                    .join(", ")}
                  {s.first.length > 3 ? " …" : ""}
                </span>
              </span>
            </div>
          ))
        ) : (
          <div style={{ color: "var(--dim)", fontSize: ".8rem" }}>No known file signatures anywhere on the device — it really is blank.</div>
        )}
      </div>
      <div className="modal-btns" style={{ marginTop: 14 }}>
        <button className="btn btn-primary" onClick={closeModal}>
          Close
        </button>
      </div>
    </>
  );
}
