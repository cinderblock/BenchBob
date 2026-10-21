// I²C panel: per bus an address map and device cards (identification, live readings with
// sparklines, Measure / auto-poll controls), plus a raw-tools dialog.

import { useEffect, useState, type ReactNode } from "react";
import { useBob } from "../../bob/context";
import type { PanelProps } from "../../registry";
import { fmtAgo, fmtHex, hexdump } from "../../lib/format";
import { Line } from "../../ui/spark";
import { AsyncButton } from "../../ui/common";
import { ActionLine, useAction } from "../../ui/status";
import { IfaceButton, useIfaceAvailable } from "../gpio/IfaceButton";
import type { I2cBus, I2cDevice, I2cState, RawReply, Reading } from "./api";

function fmtVal(v: Reading): string {
  if (v.v == null) return "—";
  if (typeof v.v === "number") return (v.d != null ? v.v.toFixed(v.d) : String(v.v)) + (v.unit ? " " + v.unit : "");
  if (typeof v.v === "boolean") return v.v ? "yes" : "no";
  return String(v.v) + (v.unit ? " " + v.unit : "");
}

const ConfPill = ({ c }: { c?: string }) =>
  c === "id" ? <span className="pr-pill on">ID matched</span> : c === "likely" ? <span className="pr-pill">likely</span> : c === "guess" ? <span className="pr-pill warn">guess</span> : null;

function AddrMap({ b }: { b: I2cBus }) {
  const at = new Map(b.devices.map((d) => [d.addr, d]));
  const cols = [...Array(16).keys()];
  return (
    <div style={{ overflowX: "auto" }}>
      <table className="pr-tbl" style={{ fontFamily: "'SF Mono',ui-monospace,monospace", fontSize: ".7rem", width: "auto" }}>
        <tbody>
          <tr>
            <th></th>
            {cols.map((c) => (
              <th key={c} style={{ padding: "2px 4px" }}>
                {c.toString(16)}
              </th>
            ))}
          </tr>
          {[...Array(8).keys()].map((r) => (
            <tr key={r}>
              <th style={{ padding: "2px 4px" }}>{r}0</th>
              {cols.map((c) => {
                const a = r * 16 + c;
                const d = at.get(a);
                const out = a < 0x08 || a > 0x77;
                const txt = out ? "" : d ? (d.claimed ? "UU" : a.toString(16).padStart(2, "0")) : "--";
                const style = d ? (d.claimed ? { color: "var(--warning)" } : { color: "var(--success)", fontWeight: 700 }) : { color: "var(--dim)" };
                return (
                  <td key={c} style={{ padding: "2px 4px", ...style }}>
                    {txt}
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function DevCard({ bus, d }: { bus: I2cBus; d: I2cDevice }) {
  const b = useBob();
  const r = d.readout;
  const base = `/api/i2c/${bus.bus}/${d.addr}`;
  const putA = useAction();
  const put = (body: object) => putA.run(() => b.api("PUT", `${base}/settings`, body), { busy: "Saving…" });
  return (
    <div className="pr-card">
      <h4>
        {d.claimed ? (
          <>
            Claimed by kernel driver <b>{d.claimed}</b>
          </>
        ) : (
          d.name || "Unknown device"
        )}
        <small>
          {d.hex}
          {d.what ? " · " + d.what : ""}
          {d.label ? " · " + d.label : ""}
        </small>
        <ConfPill c={d.confidence} />
        {r?.stale && <span className="pr-pill warn">not fresh</span>}
      </h4>
      {d.note && <div className="pr-note">{d.note}</div>}
      {d.hint && (
        <div className="pr-note" style={{ color: "var(--warning)" }}>
          {d.hint}
        </div>
      )}
      {!d.name && !d.claimed && d.hints.length > 0 && <div className="pr-note">Commonly at this address: {d.hints.join(", ")}</div>}
      {r?.values?.length ? (
        <div style={{ display: "flex", flexWrap: "wrap", gap: "6px 18px", marginTop: 6 }}>
          {r.values.map((v) => (
            <div key={v.k} style={{ minWidth: 110 }}>
              <div className="spark-lbl">{v.label}</div>
              <div className="spark-val" style={r.stale ? { color: "var(--dim)" } : undefined}>
                {fmtVal(v)}
              </div>
              <Line data={d.hist[v.k]} />
            </div>
          ))}
        </div>
      ) : null}
      {r?.pins && (
        <div style={{ display: "flex", flexWrap: "wrap", gap: 4, marginTop: 6 }}>
          {r.pins.map((p) => (
            <span key={p.name} className={`pr-pill${p.level ? " on" : ""}`} style={{ fontFamily: "monospace" }}>
              {p.name} {p.level}
              {p.dir ? " " + p.dir : ""}
              {p.pull ? " ↑" : ""}
            </span>
          ))}
        </div>
      )}
      {r?.channels && (
        <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fill,minmax(92px,1fr))", gap: 4, marginTop: 6 }}>
          {r.channels.map((c) => (
            <div key={c.ch} className="pr-note" style={{ background: "var(--surface3)", borderRadius: 4, padding: "2px 6px" }}>
              CH{c.ch} <b style={{ color: "var(--text)" }}>{c.full ? "full " + c.full : c.duty + "%"}</b>
              <div style={{ height: 3, background: "var(--surface)", borderRadius: 2 }}>
                <div style={{ height: 3, width: `${c.duty}%`, background: "var(--accent)", borderRadius: 2 }} />
              </div>
            </div>
          ))}
        </div>
      )}
      {r?.note && (
        <div className="pr-note" style={{ marginTop: 4 }}>
          {r.note}
        </div>
      )}
      {d.error && (
        <div className="pr-note" style={{ color: "var(--danger)" }}>
          {d.error}
        </div>
      )}
      {d.readAt && <div className="pr-note">Read {fmtAgo(d.readAt)}</div>}
      <div className="pr-row">
        {d.canRead && !d.claimed && <AsyncButton onClick={() => b.api("POST", `${base}/read`)}>Read</AsyncButton>}
        {d.canMeasure && !d.claimed && (
          <AsyncButton className="btn btn-sm btn-primary" onClick={() => b.api("POST", `${base}/measure`)}>
            Measure
          </AsyncButton>
        )}
        {d.pollable && !d.claimed && (
          <label>
            <input type="checkbox" checked={!!d.poll} onChange={(e) => put({ poll: e.currentTarget.checked })} /> auto-read every {d.interval / 1000} s
          </label>
        )}
        {d.canMeasure && !d.claimed && (
          <label>
            <input type="checkbox" checked={!!d.auto} onChange={(e) => put({ auto: e.currentTarget.checked })} /> auto-measure (writes)
          </label>
        )}
        <ActionLine a={putA} time={false} />
        {d.chip === "at24" && (
          <button className="btn btn-sm" onClick={() => b.openModal(<EepromDialog bus={bus.bus} d={d} />, { cls: "wide" })}>
            EEPROM contents
          </button>
        )}
        <button className="btn btn-sm" onClick={() => b.openModal(<DevSettings bus={bus.bus} d={d} />)}>
          Settings
        </button>
        <button className="btn btn-sm" onClick={() => b.openModal(<RawDialog bus={bus.bus} addr={d.addr} d={d} />, { cls: "wide" })}>
          Raw tools
        </button>
      </div>
      {d.canMeasure && d.measureNote && !d.claimed && <div className="pr-note">Measure {d.measureNote}.</div>}
    </div>
  );
}

export default function I2c({ st }: PanelProps<I2cState>) {
  const b = useBob();
  const [mapOpen, setMapOpen] = useState<Set<number>>(new Set());
  const iface = useIfaceAvailable();
  if (!st.buses.length)
    return (
      <>
        <div className="empty">No user I²C buses. Turn on I²C1 (header pins 3/5) or an extra bus, or plug in a USB adapter.</div>
        <div className="pr-row" style={{ justifyContent: "center" }}>
          <IfaceButton focus="i2c" label="Bus speed & more buses…" />
        </div>
      </>
    );
  return (
    <>
      {st.buses.map((bus) => {
        const open = mapOpen.has(bus.bus);
        return (
          <div className="pr-card" style={{ background: "transparent" }} key={bus.bus}>
            <h4>
              i2c-{bus.bus}
              <small>
                {bus.label}
                {bus.hz ? ` · ${bus.hz / 1000} kHz` : ""} · {bus.funcs.join(", ") || "no access"}
              </small>
            </h4>
            <div className="pr-note">
              {bus.scanning
                ? "Scanning…"
                : bus.lastScan
                  ? `Scanned ${fmtAgo(bus.lastScan)} (${bus.scanMs} ms) · ${bus.devices.length} device${bus.devices.length === 1 ? "" : "s"} · ${bus.autoScan ? `rescans every ${bus.autoScan} s` : "rescans on demand only"}`
                  : "Not scanned yet"}
              {bus.plainI2c ? "" : " · SMBus only (no combined transfers)"}
            </div>
            {bus.error && (
              <div className="pr-note" style={{ color: "var(--danger)" }}>
                {bus.error}
              </div>
            )}
            <div className="pr-row">
              <AsyncButton className="btn btn-sm btn-primary" disabled={bus.scanning} onClick={() => b.api("POST", `/api/i2c/${bus.bus}/scan`, { deep: false })}>
                Scan
              </AsyncButton>
              <AsyncButton disabled={bus.scanning} onClick={() => b.api("POST", `/api/i2c/${bus.bus}/scan`, { deep: true })}>
                Deep identify
              </AsyncButton>
              <button
                className="btn btn-sm"
                onClick={() =>
                  setMapOpen((m) => {
                    const n = new Set(m);
                    open ? n.delete(bus.bus) : n.add(bus.bus);
                    return n;
                  })
                }
              >
                {open ? "Hide" : "Show"} address map
              </button>
              <button className="btn btn-sm" onClick={() => b.openModal(<BusSettings bus={bus} />)}>
                Bus settings
              </button>
              <button className="btn btn-sm" onClick={() => b.openModal(<RawDialog bus={bus.bus} addr={null} />, { cls: "wide" })}>
                Raw tools
              </button>
            </div>
            {open && <AddrMap b={bus} />}
            {bus.devices.length ? (
              bus.devices.map((d) => <DevCard key={d.addr} bus={bus} d={d} />)
            ) : (
              <div className="pr-note">Nothing answered. Wire SDA{bus.bus === 1 ? " to pin 3, SCL to pin 5" : ", SCL"}, 3V3 and GND; most breakouts have their own pull-ups.</div>
            )}
          </div>
        );
      })}
      {iface && (
        <div className="pr-row" style={{ marginTop: 6 }}>
          <IfaceButton focus="i2c" label="Bus speed & more buses…" />
          <span className="pr-note">I²C1 speed, extra controllers (I²C3–6) and a software bus: config.txt, needs a reboot</span>
        </div>
      )}
      {st.hiddenBuses?.length ? (
        <div className="pr-note" style={{ marginTop: 6 }}>
          Not shown: {st.hiddenBuses.join(", ")} (HDMI display EDID buses).
        </div>
      ) : null}
    </>
  );
}

// ---------- dialogs ----------

function DevSettings({ bus, d }: { bus: number; d: I2cDevice }) {
  const b = useBob();
  const [label, setLabel] = useState(d.label);
  const [iv, setIv] = useState(d.interval);
  const [opts, setOpts] = useState<Record<string, string>>(() => Object.fromEntries(d.opts.map((o) => [o.key, String(o.value)])));
  const a = useAction();
  return (
    <>
      <h3>
        {d.name || "Device"} at {d.hex} on i2c-{bus}
      </h3>
      <div className="pr-row">
        <label>
          Name <input value={label} placeholder="e.g. Bench temperature" onInput={(e) => setLabel(e.currentTarget.value)} />
        </label>
      </div>
      <div className="pr-row">
        <label>
          Poll / measure interval{" "}
          <select value={iv} onChange={(e) => setIv(Number(e.currentTarget.value))}>
            {[500, 1000, 2000, 5000, 10000, 30000, 60000].map((v) => (
              <option key={v} value={v}>
                {v < 1000 ? v + " ms" : v / 1000 + " s"}
              </option>
            ))}
          </select>
        </label>
      </div>
      {d.opts.length > 0 && (
        <div className="pr-row">
          {d.opts.map((o) => (
            <label key={o.key}>
              {o.label}{" "}
              {o.type === "select" ? (
                <select value={opts[o.key]} onChange={(e) => setOpts({ ...opts, [o.key]: e.currentTarget.value })}>
                  {o.choices!.map((c) => (
                    <option key={String(c.v)} value={String(c.v)}>
                      {c.label}
                    </option>
                  ))}
                </select>
              ) : (
                <>
                  <input type="number" step="any" value={opts[o.key]} style={{ width: 90 }} onInput={(e) => setOpts({ ...opts, [o.key]: e.currentTarget.value })} /> {o.unit || ""}
                </>
              )}
            </label>
          ))}
        </div>
      )}
      <p className="pr-note">Saved for this address on this bus, so it survives restarts and re-plugging.</p>
      <div className="modal-btns">
        <ActionLine a={a} />
        <button className="btn" onClick={b.closeModal}>
          Cancel
        </button>
        <button
          className="btn btn-primary"
          disabled={a.busy}
          onClick={async () => {
            const body = { label, interval: iv, ...(d.opts.length ? { opts: Object.fromEntries(Object.entries(opts).map(([k, v]) => [k, Number(v)])) } : {}) };
            if (await a.run(() => b.api("PUT", `/api/i2c/${bus}/${d.addr}/settings`, body), { busy: "Saving…" })) b.closeModal();
          }}
        >
          Save
        </button>
      </div>
    </>
  );
}

function BusSettings({ bus }: { bus: I2cBus }) {
  const b = useBob();
  const [as, setAs] = useState(bus.autoScan);
  const a = useAction();
  return (
    <>
      <h3>i2c-{bus.bus} settings</h3>
      <p className="pr-note">Background rescans use i2cdetect's safe probes (quick write, or read-byte at 0x30–0x37 and 0x50–0x5F) and pick up boards plugged in while running. Addresses owned by kernel drivers are never probed.</p>
      <div className="pr-row">
        <label>
          Rescan every{" "}
          <select value={as} onChange={(e) => setAs(Number(e.currentTarget.value))}>
            {[0, 10, 30, 60, 300].map((v) => (
              <option key={v} value={v}>
                {v ? v + " s" : "never (manual only)"}
              </option>
            ))}
          </select>
        </label>
      </div>
      <div className="modal-btns">
        <ActionLine a={a} />
        <button className="btn" onClick={b.closeModal}>
          Cancel
        </button>
        <button
          className="btn btn-primary"
          disabled={a.busy}
          onClick={async () => {
            if (await a.run(() => b.api("PUT", `/api/i2c/${bus.bus}/settings`, { autoScan: as }), { busy: "Saving…" })) b.closeModal();
          }}
        >
          Save
        </button>
      </div>
    </>
  );
}

function EepromDialog({ bus, d }: { bus: number; d: I2cDevice }) {
  const b = useBob();
  const size = Number(d.opts.find((o) => o.key === "size")?.value) || 256;
  const [text, setText] = useState("Reading…");
  useEffect(() => {
    b.api<RawReply>("GET", `/api/i2c/${bus}/${d.addr}/eeprom?offset=0&length=${Math.min(size, 4096)}`)
      .then((r) => setText(hexdump(r.bytes || [])))
      .catch((e) => setText(e.message));
  }, []);
  return (
    <>
      <h3>
        EEPROM at {fmtHex(d.addr)} on i2c-{bus}
      </h3>
      <p className="pr-note">Size setting: {size} bytes (change it in Settings if this is wrong; a 24C32+ needs 2-byte addressing). Showing up to 4 KiB.</p>
      <div className="pr-log">{text}</div>
      <div className="modal-btns">
        <button className="btn btn-primary" onClick={b.closeModal}>
          Close
        </button>
      </div>
    </>
  );
}

const RAW_OPS: [string, string, boolean][] = [
  ["rbyte", "Read byte (register)", false],
  ["rword", "Read word (register)", false],
  ["rblock", "Read block (register, length)", false],
  ["recv", "Pure read (length, no register)", false],
  ["dump", "Dump registers 0x00–0xFF", false],
  ["wbyte", "Write byte (register, value)", true],
  ["wword", "Write word (register, value, MSB first)", true],
  ["wblock", "Write block (register, bytes)", true],
  ["xfer", "Write then read (bytes, length)", true],
];

function RawDialog({ bus, addr, d }: { bus: number; addr: number | null; d?: I2cDevice }) {
  const b = useBob();
  const [a, setA] = useState(addr != null ? fmtHex(addr) : "0x");
  const [op, setOp] = useState("rbyte");
  const [reg, setReg] = useState("0x00");
  const [val, setVal] = useState("");
  const [len, setLen] = useState("1");
  const [wc, setWc] = useState(false);
  const [force, setForce] = useState(false);
  const [out, setOut] = useState<ReactNode>("");
  const w = RAW_OPS.find((o) => o[0] === op)![2];
  const run = async () => {
    const n = Number(len) || 1;
    const base = `/api/i2c/${bus}/${encodeURIComponent(a.trim())}`;
    const bytes = val.trim().split(/[\s,]+/).filter(Boolean);
    const confirm = wc;
    if (w && !confirm) return setOut("Tick the confirmation first: this writes to the device.");
    try {
      let r: RawReply;
      if (op === "rbyte") r = await b.api("POST", base + "/raw/read", { register: reg.trim(), size: "byte", force });
      else if (op === "rword") r = await b.api("POST", base + "/raw/read", { register: reg.trim(), size: "word", force });
      else if (op === "rblock") r = await b.api("POST", base + "/raw/read", { register: reg.trim(), size: "block", length: Math.min(32, n), force });
      else if (op === "recv") r = await b.api("POST", base + "/raw/read", { size: "raw", length: n, force });
      else if (op === "dump") {
        const unsafe = !d || d.pointerUnsafe;
        r = await b.api("GET", base + `/dump?${unsafe ? "confirm=1&" : ""}${force ? "force=1" : ""}`);
        return setOut("      " + [...Array(16).keys()].map((i) => " " + i.toString(16)).join(" ") + "\n" + hexdump(r.regs || []).replace(/^0{2}/gm, ""));
      } else if (op === "wbyte") r = await b.api("POST", base + "/raw/write", { register: reg.trim(), size: "byte", value: val.trim(), confirm, force });
      else if (op === "wword") r = await b.api("POST", base + "/raw/write", { register: reg.trim(), size: "word", value: val.trim(), confirm, force });
      else if (op === "wblock") r = await b.api("POST", base + "/raw/write", { register: reg.trim(), size: "block", bytes, confirm, force });
      else r = await b.api("POST", base + "/transfer", { write: bytes, read: n, confirm, force });
      const all = r.reads ? r.reads.flat() : r.bytes || r.wrote || [];
      setOut(
        (r.wrote ? "Wrote " : "") +
          all.map((x) => fmtHex(x)).join(" ") +
          (r.wordBigEndian != null ? `\nword: ${fmtHex(r.word!, 4)} (LE)  ${fmtHex(r.wordBigEndian, 4)} (BE, as most sensors mean it)` : "") +
          (all.length > 1 && !r.wrote ? "\n" + hexdump(all) : ""),
      );
    } catch (e: any) {
      setOut(e.message);
    }
  };
  return (
    <>
      <h3>Raw I²C · i2c-{bus}</h3>
      <div className="pr-row">
        <label>
          Address <input value={a} style={{ width: 70 }} onInput={(e) => setA(e.currentTarget.value)} />
        </label>
        <label>
          Operation{" "}
          <select value={op} onChange={(e) => setOp(e.currentTarget.value)}>
            {RAW_OPS.map(([v, l]) => (
              <option key={v} value={v}>
                {l}
              </option>
            ))}
          </select>
        </label>
      </div>
      <div className="pr-row">
        <label>
          Register <input value={reg} style={{ width: 70 }} onInput={(e) => setReg(e.currentTarget.value)} />
        </label>
        <label>
          Value / bytes <input value={val} placeholder="0x27 or 0x00 0x1f" style={{ width: 150 }} onInput={(e) => setVal(e.currentTarget.value)} />
        </label>
        <label>
          Length <input type="number" min={1} max={256} value={len} style={{ width: 64 }} onInput={(e) => setLen(e.currentTarget.value)} />
        </label>
      </div>
      {d?.pointerUnsafe && (
        <p className="pr-note" style={{ color: "var(--warning)" }}>
          {d.name ? d.name + " treats" : "An unidentified chip may treat"} register pointers as commands: register reads and dumps can change its state.
        </p>
      )}
      {w && (
        <label className="pr-note" style={{ display: "flex", gap: 6, alignItems: "center" }}>
          <input type="checkbox" checked={wc} onChange={(e) => setWc(e.currentTarget.checked)} /> I understand this writes to the device
        </label>
      )}
      <label className="pr-note" style={{ display: "flex", gap: 6, alignItems: "center" }}>
        <input type="checkbox" checked={force} onChange={(e) => setForce(e.currentTarget.checked)} /> Force (address owned by a kernel driver)
      </label>
      <div className="pr-log" style={{ marginTop: 8, minHeight: "3em" }}>
        {out}
      </div>
      <div className="modal-btns">
        <button className="btn" onClick={b.closeModal}>
          Close
        </button>
        <button className={`btn ${w ? "btn-warn" : "btn-primary"}`} onClick={run}>
          {w ? "Write" : "Run"}
        </button>
      </div>
    </>
  );
}
