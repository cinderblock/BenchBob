// 1-Wire panel: masters (search, noise diagnosis), slave cards decoded by family with
// sparklines, I/O switches, EEPROM viewer, iButton touch log, DS2482 bridge binding.

import { useEffect, useState } from "react";
import { useBob } from "../../bob/context";
import type { PanelProps } from "../../registry";
import { fmtAgo, hexdump } from "../../lib/format";
import { AsyncButton } from "../../ui/common";
import { ActionLine, useAction } from "../../ui/status";
import { IfaceButton } from "../gpio/IfaceButton";
import type { OwSlave, OwState, OwValue } from "./api";

const fmt = (v: OwValue) => (v.v == null ? "—" : typeof v.v === "number" ? (v.d != null ? v.v.toFixed(v.d) : String(v.v)) + (v.unit ? " " + v.unit : "") : String(v.v) + (v.unit ? " " + v.unit : ""));

function Spark({ arr, w = 110, h = 24 }: { arr?: number[]; w?: number; h?: number }) {
  if (!arr || arr.length < 2) return null;
  const lo = Math.min(...arr),
    hi = Math.max(...arr),
    span = hi - lo || 1;
  const pts = arr.map((v, i) => `${((i / (arr.length - 1)) * w).toFixed(1)},${(h - 2 - ((v - lo) / span) * (h - 4)).toFixed(1)}`).join(" ");
  return (
    <>
      <svg width={w} height={h} viewBox={`0 0 ${w} ${h}`} style={{ display: "block" }}>
        <polyline points={pts} fill="none" stroke="var(--accent)" strokeWidth={1.5} />
      </svg>
      <div className="pr-note" style={{ fontSize: ".65rem" }}>
        {lo.toFixed(2)} – {hi.toFixed(2)}
      </div>
    </>
  );
}

function SlaveCard({ s }: { s: OwSlave }) {
  const b = useBob();
  const big = s.values.filter((v) => !["res", "alarm", "power", "addr"].includes(v.k));
  const small = s.values.filter((v) => !big.includes(v));
  const pinA = useAction();
  const pin = async (p: { name: string; out: boolean }) => {
    const name = p.name.replace("PIO-", "");
    const on = !p.out;
    if (!(await b.confirm("Switch 1-Wire output", `${on ? "Turn ON" : "Turn OFF"} PIO ${name} of ${s.id}?`, on ? "ON = output transistor conducts: the pin is pulled to GND." : "OFF = transistor released: the pin floats (or rises with a pull-up)."))) return;
    await pinA.run(() => b.api("POST", `/api/onewire/${s.id}/output`, { pin: name, on, confirm: true }), { busy: `Switching PIO ${name} ${on ? "on" : "off"}…` });
  };
  return (
    <div className="pr-card" style={s.present ? undefined : { opacity: 0.55 }}>
      <h4>
        {s.label || s.name}
        <small>
          {s.label ? s.name + " · " : ""}
          {s.id} · {s.desc}
        </small>
        {!s.present && <span className="pr-pill">gone</span>}
        {s.romOk === false && <span className="pr-pill bad">ROM CRC bad</span>}
      </h4>
      {big.length > 0 && (
        <div style={{ display: "flex", flexWrap: "wrap", gap: "6px 18px" }}>
          {big.map((v) => (
            <div key={v.k} style={{ minWidth: 110 }}>
              <div className="spark-lbl">{v.label}</div>
              <div className="spark-val">{fmt(v)}</div>
              <Spark arr={s.hist[v.k]} />
            </div>
          ))}
        </div>
      )}
      {small.length > 0 && <div className="pr-note">{small.map((v) => `${v.label}: ${fmt(v)}`).join(" · ")}</div>}
      {s.pins && (
        <>
          <div style={{ display: "flex", flexWrap: "wrap", gap: 4, marginTop: 6 }}>
            {s.pins.map((p) => (
              <button key={p.name} className={`pr-pill${p.out ? " warn" : p.level ? " on" : ""}`} style={{ border: "none", cursor: "pointer", fontFamily: "monospace" }} disabled={pinA.busy} onClick={() => pin(p)}>
                {p.name}: {p.level ? "high" : "low"}
                {p.out ? " · driven low" : ""}
              </button>
            ))}
            <ActionLine a={pinA} />
          </div>
          <div className="pr-note">Tap a pin to toggle its output transistor (asks first). "Driven low" = transistor on.</div>
        </>
      )}
      {s.warn.map((w) => (
        <div key={w} className="pr-note" style={{ color: "var(--warning)" }}>
          {w}
        </div>
      ))}
      {s.error && (
        <div className="pr-note" style={{ color: "var(--danger)" }}>
          {s.error}
          {s.module ? ` (kernel module ${s.module})` : ""}
        </div>
      )}
      <div className="pr-row">
        <span className="pr-note">
          {s.readAt ? "Read " + fmtAgo(s.readAt) : "Not read yet"} · first seen {fmtAgo(s.firstSeen)}
        </span>
        <AsyncButton onClick={() => b.api("POST", `/api/onewire/${s.id}/read`)}>Read now</AsyncButton>
        {s.eepromSize ? (
          <button className="btn btn-sm" onClick={() => b.openModal(<EepromDialog id={s.id} />, { cls: "wide" })}>
            EEPROM ({s.eepromSize} B)
          </button>
        ) : null}
        <button className="btn btn-sm" onClick={() => b.openModal(<EditDialog s={s} />)}>
          Name{s.resolution ? " / resolution" : ""}
        </button>
      </div>
    </div>
  );
}

// Bind and Unbind swap places when they work, so the status belongs to the row.
function BridgeRow({ br }: { br: OwState["bridges"][number] }) {
  const b = useBob();
  const a = useAction();
  const bridge = async (bind: boolean, bus: number, address: string) => {
    if (!(await b.confirm(bind ? "Bind DS2482" : "Unbind DS2482", bind ? "Load the kernel ds2482 driver for this bridge? It becomes a new 1-Wire master and its I²C address shows as claimed (UU)." : "Remove the ds2482 master? Devices on it disappear.", `i2c-${bus} ${address}`))) return;
    await a.run(() => b.api("POST", "/api/onewire/bridge", { bus, address, bind }), { busy: bind ? "Binding…" : "Unbinding…", ok: bind ? "Bound" : "Unbound" });
  };
  return (
    <div className="pr-row">
      <span>
        {br.type} {br.bus != null ? `i2c-${br.bus} ${br.address}` : ""}
      </span>
      <span className="pr-note">{br.note}</span>
      {br.type === "DS2482" && !br.bound && !/claimed/.test(br.note) && (
        <button className="btn btn-sm btn-primary" disabled={a.busy} onClick={() => bridge(true, br.bus!, br.address)}>
          Bind ds2482 driver
        </button>
      )}
      {br.type === "DS2482" && br.bound && (
        <button className="btn btn-sm" disabled={a.busy} onClick={() => bridge(false, br.bus!, br.address)}>
          Unbind
        </button>
      )}
      <ActionLine a={a} />
    </div>
  );
}

export default function OneWire({ st }: PanelProps<OwState>) {
  const b = useBob();
  return (
    <>
      {!st.masters.length && (
        <>
          <div className="empty">No 1-Wire master. Turn on the GPIO 1-Wire master or plug in a DS9490R.</div>
          <div className="pr-row" style={{ justifyContent: "center" }}>
            <IfaceButton focus="w1" label="1-Wire pin…" />
          </div>
        </>
      )}
      {st.masters.map((m) => (
        <div className="pr-card" style={{ background: "transparent" }} key={m.id}>
          <h4>
            {m.label}
            <small>
              {m.id} · {m.slaves} device{m.slaves === 1 ? "" : "s"} · search {m.searchText} every {m.timeout || "?"} s · strong pull-up {m.pullup ? "on" : "off"}
              {m.bulk ? " · bulk conversion available" : ""}
            </small>
          </h4>
          {m.ghosts ? (
            <div className="pr-note" style={{ color: "var(--warning)" }}>
              {m.ghosts} phantom ID{m.ghosts === 1 ? "" : "s"} (family 00) in the last 10 min: the data line is floating. Fit a 4.7 kΩ pull-up from the data pin to 3V3, or ignore this if nothing is connected.
            </div>
          ) : null}
          {m.error && (
            <div className="pr-note" style={{ color: "var(--danger)" }}>
              {m.error}
            </div>
          )}
          <div className="pr-row">
            <AsyncButton className="btn btn-sm btn-primary" busyText="Searching…" ok="Search done" onClick={() => b.api("POST", "/api/onewire/search", { master: m.id })}>
              Search now
            </AsyncButton>
            <button className="btn btn-sm" onClick={() => b.openModal(<SettingsDialog st={st} />)}>
              Polling every {st.interval / 1000} s{st.bulk ? ", simultaneous conversion" : ""}
            </button>
            {m.kind === "gpio" && <IfaceButton focus="w1" label="1-Wire pin…" />}
          </div>
        </div>
      ))}
      {st.fixture && (
        <div className="pr-note" style={{ color: "var(--warning)" }}>
          Reading a fixture tree ({st.root}), not real hardware.
        </div>
      )}
      {st.slaves.map((s) => (
        <SlaveCard key={s.id} s={s} />
      ))}
      {st.touchLog.length > 0 && (
        <div className="pr-card">
          <h4>
            Touch log<small>iButton touches and device arrivals</small>
          </h4>
          <table className="pr-tbl">
            <tbody>
              <tr>
                <th>When</th>
                <th>Device</th>
                <th>Event</th>
              </tr>
              {st.touchLog.slice(0, 15).map((t, i) => (
                <tr key={i}>
                  <td>
                    {new Date(t.t).toLocaleTimeString()} <span className="pr-note">{fmtAgo(t.t)}</span>
                  </td>
                  <td>
                    <code>{t.id}</code> <span className="pr-note">{t.family}</span>
                  </td>
                  <td>{t.event}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {st.bridges.length > 0 && (
        <div className="pr-card">
          <h4>Bridges</h4>
          {st.bridges.map((br, i) => (
            <BridgeRow key={i} br={br} />
          ))}
        </div>
      )}
    </>
  );
}

function EditDialog({ s }: { s: OwSlave }) {
  const b = useBob();
  const cur = s.values.find((v) => v.k === "res");
  const curBits = cur ? String(parseInt(String(cur.v))) : "";
  const [label, setLabel] = useState(s.label);
  const [res, setRes] = useState(curBits || "12");
  const [save, setSave] = useState(false);
  const [rs, setRs] = useState("");
  const a = useAction();
  return (
    <>
      <h3>
        {s.name} <span className="pr-note">{s.id}</span>
      </h3>
      <div className="pr-row">
        <label>
          Name <input value={label} placeholder="e.g. Freezer" onInput={(e) => setLabel(e.currentTarget.value)} />
        </label>
      </div>
      {s.resolution && (
        <>
          <div className="pr-row">
            <label>
              Resolution{" "}
              <select value={res} onChange={(e) => setRes(e.currentTarget.value)}>
                {[9, 10, 11, 12].map((bits) => (
                  <option key={bits} value={bits}>
                    {bits}-bit ({[0.5, 0.25, 0.125, 0.0625][bits - 9]} °C, {[94, 188, 375, 750][bits - 9]} ms)
                  </option>
                ))}
              </select>
            </label>
            <label>
              <input type="checkbox" checked={save} onChange={(e) => setSave(e.currentTarget.checked)} /> also save in the sensor's EEPROM
            </label>
          </div>
          <p className="pr-note">Without saving, the sensor returns to its stored resolution after a power cycle. EEPROM writes are limited (~50k), so don't automate it.</p>
        </>
      )}
      {s.family === "0x26" && (
        <div className="pr-row">
          <label>
            Sense resistor <input type="number" step="any" placeholder="Ω" style={{ width: 80 }} value={rs} onInput={(e) => setRs(e.currentTarget.value)} /> Ω (for current)
          </label>
        </div>
      )}
      <div className="modal-btns">
        <ActionLine a={a} />
        <button className="btn" onClick={b.closeModal}>
          Cancel
        </button>
        <button
          className="btn btn-primary"
          disabled={a.busy}
          onClick={async () => {
            const ok = await a.run(
              async () => {
                await b.api("PUT", `/api/onewire/${s.id}/label`, { label, ...(rs ? { rsens: Number(rs) } : {}) });
                if (s.resolution && curBits !== res) await b.api("PUT", `/api/onewire/${s.id}/resolution`, { bits: Number(res), save });
                return true;
              },
              { busy: "Saving…" },
            );
            if (ok) b.closeModal();
          }}
        >
          Save
        </button>
      </div>
    </>
  );
}

function SettingsDialog({ st }: { st: OwState }) {
  const b = useBob();
  const [iv, setIv] = useState(st.interval);
  const [bulk, setBulk] = useState(st.bulk);
  const a = useAction();
  return (
    <>
      <h3>1-Wire polling</h3>
      <div className="pr-row">
        <label>
          Read every{" "}
          <select value={iv} onChange={(e) => setIv(Number(e.currentTarget.value))}>
            {[2000, 5000, 10000, 30000, 60000, 300000].map((v) => (
              <option key={v} value={v}>
                {v / 1000} s
              </option>
            ))}
          </select>
        </label>
      </div>
      <div className="pr-row">
        <label>
          <input type="checkbox" checked={bulk} onChange={(e) => setBulk(e.currentTarget.checked)} /> Simultaneous conversion when a bus has several temperature sensors (one 750 ms wait instead of one per sensor)
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
            if (await a.run(() => b.api("PUT", "/api/onewire/settings", { interval: iv, bulk }), { busy: "Saving…" })) b.closeModal();
          }}
        >
          Save
        </button>
      </div>
    </>
  );
}

function EepromDialog({ id }: { id: string }) {
  const b = useBob();
  const [text, setText] = useState("Reading…");
  useEffect(() => {
    b.api<{ eeprom?: number[] }>("POST", `/api/onewire/${id}/read?eeprom=1`)
      .then((r) => setText(hexdump(r.eeprom || [])))
      .catch((e) => setText(e.message));
  }, []);
  return (
    <>
      <h3>EEPROM {id}</h3>
      <div className="pr-log">{text}</div>
      <div className="modal-btns">
        <button className="btn btn-primary" onClick={b.closeModal}>
          Close
        </button>
      </div>
    </>
  );
}
