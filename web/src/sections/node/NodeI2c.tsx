// A node's I²C buses: pins and clock, the last scan's addresses (a node identifies no
// chips), Scan, and raw reads. Nodes don't write to I²C.

import { useState } from "react";
import { useBob, useSnapshot } from "../../bob/context";
import { fmtHex, hexdump } from "../../lib/format";
import { AsyncButton, Note } from "../../ui/common";
import type { NodeI2cBus, NodeI2cRead } from "./api";
import { usePoll } from "./usePoll";
import "./node.css";

export default function NodeI2c() {
  const b = useBob();
  const uptime = useSnapshot().node?.uptime ?? 0;
  const { data, error, reload } = usePoll<{ buses: NodeI2cBus[] }>("/api/i2c", 5000);
  const [maps, setMaps] = useState<Set<number>>(new Set());
  return (
    <div className="section pr-sec">
      <div className="section-title">
        <span>I²C</span>
        <span className="pr-sum">{data ? data.buses.map((x) => `${x.dev} · ${x.devices.length} device${x.devices.length === 1 ? "" : "s"}`).join(" · ") : ""}</span>
      </div>
      <div className="pr-body">
        {error && <Note color="danger">{error}</Note>}
        {!data && !error && <Note>Loading…</Note>}
        {data?.buses.map((bus) => (
          <div className="pr-card" style={{ background: "transparent" }} key={bus.bus}>
            <h4>
              {bus.dev}
              <small>
                {bus.label} · SDA GPIO{bus.sda}, SCL GPIO{bus.scl} · {bus.hz / 1000} kHz{bus.quickWrite ? "" : " · scans read a byte at every address"}
              </small>
            </h4>
            <Note>{bus.lastScanUptimeMs == null ? "Not scanned yet" : `Scanned ${Math.max(0, Math.round(uptime - bus.lastScanUptimeMs / 1000))} s ago (${bus.scanMs} ms) · ${bus.devices.length} device${bus.devices.length === 1 ? "" : "s"}`}</Note>
            <div className="pr-row">
              <AsyncButton
                className="btn btn-sm btn-primary"
                busyText="Scanning…"
                onClick={async () => {
                  await b.api("POST", `/api/i2c/${bus.bus}/scan`);
                  await reload();
                }}
              >
                Scan
              </AsyncButton>
              <button className="btn btn-sm" onClick={() => setMaps((m) => (m.has(bus.bus) ? new Set([...m].filter((x) => x !== bus.bus)) : new Set(m).add(bus.bus)))}>
                {maps.has(bus.bus) ? "Hide" : "Show"} address map
              </button>
              <button className="btn btn-sm" onClick={() => b.openModal(<RawRead bus={bus.bus} addr={null} />, { cls: "wide" })}>
                Raw read
              </button>
            </div>
            {maps.has(bus.bus) && <AddrMap bus={bus} />}
            {bus.devices.length ? (
              bus.devices.map((d) => (
                <div className="pr-card" key={d.addr}>
                  <h4>
                    Device at {d.hex}
                    <small>nodes report addresses only, no chip identification</small>
                  </h4>
                  <div className="pr-row">
                    <button className="btn btn-sm" onClick={() => b.openModal(<RawRead bus={bus.bus} addr={d.addr} />, { cls: "wide" })}>
                      Raw read
                    </button>
                  </div>
                </div>
              ))
            ) : (
              <Note>{bus.lastScanUptimeMs == null ? "Scan to find devices." : "Nothing answered. Wire SDA, SCL, 3V3 and GND; most breakouts have their own pull-ups."}</Note>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}

function AddrMap({ bus }: { bus: NodeI2cBus }) {
  const at = new Set(bus.devices.map((d) => d.addr));
  return (
    <div className="nd-wrap">
      <table className="pr-tbl" style={{ fontFamily: "'SF Mono',ui-monospace,monospace", fontSize: ".7rem", width: "auto" }}>
        <tbody>
          <tr>
            <th></th>
            {[...Array(16).keys()].map((c) => (
              <th key={c} style={{ padding: "2px 4px" }}>
                {c.toString(16)}
              </th>
            ))}
          </tr>
          {[...Array(8).keys()].map((r) => (
            <tr key={r}>
              <th style={{ padding: "2px 4px" }}>{r}0</th>
              {[...Array(16).keys()].map((c) => {
                const a = r * 16 + c;
                const out = a < 0x08 || a > 0x77;
                return (
                  <td key={c} style={{ padding: "2px 4px", ...(at.has(a) ? { color: "var(--success)", fontWeight: 700 } : { color: "var(--dim)" }) }}>
                    {out ? "" : at.has(a) ? a.toString(16).padStart(2, "0") : "--"}
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

function RawRead({ bus, addr }: { bus: number; addr: number | null }) {
  const b = useBob();
  const [a, setA] = useState(addr != null ? fmtHex(addr) : "0x");
  const [size, setSize] = useState("byte");
  const [reg, setReg] = useState("0x00");
  const [len, setLen] = useState("1");
  const [out, setOut] = useState("");
  const run = async () => {
    try {
      const body: Record<string, unknown> = { size, length: Number(len) || 1 };
      if (size !== "raw") body.register = reg;
      const r = await b.api<NodeI2cRead>("POST", `/api/i2c/${bus}/${encodeURIComponent(a.trim())}/raw/read`, body);
      setOut(r.bytes.map((x) => fmtHex(x)).join(" ") + (r.wordBigEndian != null ? `\nword: ${fmtHex(r.word!, 4)} (LE)  ${fmtHex(r.wordBigEndian, 4)} (BE, as most sensors mean it)` : "") + (r.bytes.length > 1 ? "\n" + hexdump(r.bytes) : ""));
    } catch (e: any) {
      setOut(e?.message || "Request failed");
    }
  };
  return (
    <>
      <h3>Raw I²C read · i2c-{bus}</h3>
      <div className="pr-row">
        <label>
          Address <input value={a} onInput={(e) => setA(e.currentTarget.value)} style={{ width: 70 }} />
        </label>
        <label>
          Read{" "}
          <select value={size} onChange={(e) => setSize(e.currentTarget.value)}>
            <option value="byte">Byte (register)</option>
            <option value="word">Word (register)</option>
            <option value="block">Block (register, length)</option>
            <option value="raw">Pure read (length, no register)</option>
          </select>
        </label>
      </div>
      <div className="pr-row">
        {size !== "raw" && (
          <label>
            Register <input value={reg} onInput={(e) => setReg(e.currentTarget.value)} style={{ width: 70 }} />
          </label>
        )}
        {(size === "block" || size === "raw") && (
          <label>
            Length <input type="number" min={1} max={size === "raw" ? 256 : 32} value={len} onInput={(e) => setLen(e.currentTarget.value)} style={{ width: 64 }} />
          </label>
        )}
      </div>
      <Note>A register read writes the register pointer first. Nodes don't do I²C writes.</Note>
      <div className="pr-log" style={{ marginTop: 8, minHeight: "3em" }}>
        {out}
      </div>
      <div className="modal-btns">
        <button className="btn" onClick={b.closeModal}>
          Close
        </button>
        <button className="btn btn-primary" onClick={run}>
          Run
        </button>
      </div>
    </>
  );
}
