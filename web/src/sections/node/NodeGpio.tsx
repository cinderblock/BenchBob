// A node's GPIO: every pin with its function, level and pull, polled once a second, and
// the same opt-in output control as on a BenchBob server (off after every node reset).

import { useState } from "react";
import { useBob } from "../../bob/context";
import { Note, Pill } from "../../ui/common";
import { ActionLine, useAction } from "../../ui/status";
import type { NodeGpio as Gpio, NodePin } from "./api";
import { usePoll } from "./usePoll";
import "./node.css";

const Dot = ({ p }: { p: NodePin }) => (p.level == null ? null : <span className={`nd-dot${p.level ? " hi" : ""}`} />);

export default function NodeGpio() {
  const b = useBob();
  const { data, error, reload } = usePoll<Gpio>("/api/gpio", 1000);
  const outA = useAction();
  const toggle = async () => {
    if (!data) return;
    if (data.outputEnabled) await outA.run(() => b.api("POST", "/api/gpio/output-control", { enabled: false }), { busy: "Turning off…" });
    else {
      if (!(await b.confirm("Enable GPIO output control", "This lets the dashboard, the REST API and MCP agents drive this node's pins (high/low/input/pulls). Pins the firmware uses stay refused.", "Off again after the node resets; turn it off here any time."))) return;
      await outA.run(() => b.api("POST", "/api/gpio/output-control", { enabled: true, confirm: true }), { busy: "Turning on…" });
    }
    reload();
  };
  return (
    <div className="section pr-sec">
      <div className="section-title">
        <span>GPIO</span>
        <span className="pr-sum">{data ? `${data.pins.length} pins` : ""}</span>
      </div>
      <div className="pr-body">
        <div className="pr-row">
          <Pill cls={data && !error ? "on" : "warn"}>{data && !error ? "live" : "connecting"}</Pill>
          {data && (
            <button className={`btn btn-sm ${data.outputEnabled ? "btn-warn" : ""}`} disabled={outA.busy} onClick={toggle}>
              {data.outputEnabled ? "Output control ON: turn off" : "Enable output control…"}
            </button>
          )}
          <ActionLine a={outA} />
        </div>
        {data && (data.outputEnabled ? <Note color="warning">Output control is on: Set… drives a pin. Pins the firmware uses are refused.</Note> : <Note>Read-only view. Output control is off.</Note>)}
        {error && <Note color="danger">{error}</Note>}
        {!data ? (
          <Note>Waiting for the first reading…</Note>
        ) : (
          <div className="nd-wrap">
            <table className="pr-tbl">
              <thead>
                <tr>
                  <th>GPIO</th>
                  <th>Pin</th>
                  <th>Function</th>
                  <th>Level</th>
                  <th>Pull</th>
                  <th>Notes</th>
                  {data.outputEnabled && <th></th>}
                </tr>
              </thead>
              <tbody>
                {data.pins.map((p) => (
                  <tr key={p.gpio}>
                    <td>{p.gpio}</td>
                    <td>{p.header ?? "—"}</td>
                    <td>{p.func}</td>
                    <td>
                      {p.level == null ? (
                        "—"
                      ) : (
                        <>
                          <Dot p={p} />
                          {p.level ? "high" : "low"}
                        </>
                      )}
                    </td>
                    <td>{p.pull || "—"}</td>
                    <td>
                      {p.refusal && <span className="pr-note">{p.refusal}</span>}
                      {p.note && <span className="pr-note"> {p.note}</span>}
                    </td>
                    {data.outputEnabled && (
                      <td>
                        {!p.refusal && (
                          <button className="btn btn-sm" onClick={() => b.openModal(<PinDialog gpio={p.gpio} onDone={reload} />)}>
                            Set…
                          </button>
                        )}
                      </td>
                    )}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}

function PinDialog({ gpio, onDone }: { gpio: number; onDone(): void }) {
  const b = useBob();
  const [res, setRes] = useState("");
  const set = async (body: Record<string, string>) => {
    try {
      const p = await b.api<NodePin>("POST", `/api/gpio/${gpio}`, body);
      setRes(`Now ${p.func}, ${p.level ? "high" : "low"}${p.pull && p.pull !== "none" ? ` · pull-${p.pull}` : ""}`);
      onDone();
    } catch (e: any) {
      setRes(e?.message || "Request failed");
    }
  };
  return (
    <>
      <h3>GPIO{gpio}</h3>
      <p>This drives the physical pin: make sure nothing connected minds.</p>
      <div className="pr-row">
        <span className="pr-note" style={{ minWidth: 60 }}>
          Mode
        </span>
        {(["high", "low", "input"] as const).map((m) => (
          <button key={m} className="btn btn-sm" onClick={() => set({ mode: m })}>
            {m === "input" ? "Input" : `Output ${m}`}
          </button>
        ))}
      </div>
      <div className="pr-row">
        <span className="pr-note" style={{ minWidth: 60 }}>
          Pull
        </span>
        {(["up", "down", "none"] as const).map((m) => (
          <button key={m} className="btn btn-sm" onClick={() => set({ pull: m })}>
            {m === "none" ? "None" : m === "up" ? "Up" : "Down"}
          </button>
        ))}
      </div>
      <div className="pr-note">{res}</div>
      <div className="modal-btns">
        <button className="btn btn-primary" onClick={b.closeModal}>
          Done
        </button>
      </div>
    </>
  );
}
