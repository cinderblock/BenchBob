// IR panel (backend/crates/ir): what the receivers catch (decoded, raw, Pronto), Learn
// (capture a button and name it), the library of named codes with send buttons, and a
// send form. Receivers and transmitters are kernel overlays the GPIO panel stages.

import { useState } from "react";
import { useBob } from "../../bob/context";
import type { PanelProps } from "../../registry";
import { Hint, Note } from "../../ui/common";
import { ActionLine, useAction } from "../../ui/status";
import { usePoll } from "../../sections/node/usePoll";
import { IfaceButton } from "../gpio/IfaceButton";
import "../leds/leds.css";
import "./ir.css";

export interface IrCode {
  protocol: string;
  address: number;
  command: number;
  toggle: boolean;
}

export interface IrCapture {
  seq: number;
  at: number;
  device: string;
  code: IrCode | null;
  timings: number[];
  carrier: number;
  pronto: string | null;
}

export interface IrState {
  outputEnabled: boolean;
  receivers: { path: string; driver: string | null }[];
  transmitters: { path: string; driver: string | null; carrier: boolean }[];
  protocols: { name: string; carrier: number; maxAddress: number; maxCommand: number }[];
  captures: IrCapture[];
  library: { device: string; buttons: { name: string; code: Record<string, unknown> }[] }[];
}

const hex = (n: number) => "0x" + n.toString(16).toUpperCase().padStart(2, "0");
const codeText = (c: IrCode | null, t: number[]) => (c ? `${c.protocol} ${hex(c.address)} ${hex(c.command)}${c.protocol === "rc5" || c.protocol === "rc6" ? (c.toggle ? " (toggle 1)" : " (toggle 0)") : ""}` : `raw, ${t.length} timings`);
const time = (ms: number) => new Date(ms).toLocaleTimeString([], { hour: "numeric", minute: "2-digit", second: "2-digit" });

export default function Ir(_: PanelProps) {
  const b = useBob();
  const { data, error, reload } = usePoll<IrState>("/api/ir", 2000);
  const learn = useAction();
  const [learned, setLearned] = useState<IrCapture | null>(null);
  if (error) return <Note color="danger">{error}</Note>;
  if (!data) return <Note>Waiting for the first reading…</Note>;
  const none = data.receivers.length === 0 && data.transmitters.length === 0;
  const doLearn = async () => {
    setLearned(null);
    const c = await learn.run(() => b.api<IrCapture>("POST", "/api/ir/capture", { timeoutMs: 15000 }), { busy: "Point the remote at the receiver and press a button…" });
    if (c) {
      setLearned(c);
      reload();
    }
  };
  return (
    <div className="pr-body">
      {none ? (
        <Note>
          No IR receiver or transmitter yet. Add them in the GPIO panel (they're boot overlays, so a reboot follows). <IfaceButton focus="ir" label="IR setup…" />
        </Note>
      ) : (
        <div className="pr-row">
          <span className="pr-note">
            {data.receivers.length > 0 && `Receiving on ${data.receivers.map((r) => r.path).join(", ")}`}
            {data.receivers.length > 0 && data.transmitters.length > 0 && " · "}
            {data.transmitters.length > 0 && `sending on ${data.transmitters.map((t) => t.path).join(", ")}`}
          </span>
          <IfaceButton focus="ir" label="IR setup…" />
          <Hint>
            A demodulating receiver (TSOP38238 or similar) on one pin, an IR LED through a transistor on another. Codes are decoded here (NEC, Samsung, Sony, RC5, RC6, JVC; anything else stays as raw timings and Pronto hex) and never become keypresses. Sending needs GPIO output control on.
          </Hint>
        </div>
      )}
      {!data.outputEnabled && data.transmitters.length > 0 && <Note>GPIO output control is off, so nothing can be sent.</Note>}
      {data.receivers.length > 0 && (
        <div className="pr-row">
          <button className="btn btn-sm btn-primary" disabled={learn.busy} onClick={doLearn}>
            Learn a button…
          </button>
          <ActionLine a={learn} />
        </div>
      )}
      {learned && <SaveCapture c={learned} st={data} onDone={() => (setLearned(null), reload())} />}
      <Library st={data} onDone={reload} />
      {data.transmitters.length > 0 && <SendForm st={data} />}
      {data.captures.length > 0 && (
        <>
          <div className="stat-label">Recent captures</div>
          <table className="pr-table">
            <tbody>
              {data.captures.slice(0, 12).map((c) => (
                <CaptureRow key={c.seq} c={c} st={data} onSave={() => setLearned(c)} />
              ))}
            </tbody>
          </table>
        </>
      )}
    </div>
  );
}

function sendBody(c: IrCapture) {
  return c.code && c.code.protocol !== "nec-repeat" ? { protocol: c.code.protocol, address: c.code.address, command: c.code.command, toggle: c.code.toggle } : { timings: c.timings, carrier: c.carrier };
}

function CaptureRow({ c, st, onSave }: { c: IrCapture; st: IrState; onSave(): void }) {
  const b = useBob();
  const a = useAction();
  const [raw, setRaw] = useState(false);
  return (
    <>
      <tr>
        <td className="mono">{time(c.at)}</td>
        <td>
          <button type="button" className="net-name-btn" aria-expanded={raw} onClick={() => setRaw(!raw)}>
            {codeText(c.code, c.timings)}
          </button>
        </td>
        <td>
          {st.transmitters.length > 0 && (
            <button className="btn btn-sm" disabled={!st.outputEnabled || a.busy} onClick={() => a.run(() => b.api("POST", "/api/ir/send", sendBody(c)), { busy: "Sending…", ok: "Sent" })}>
              Send again
            </button>
          )}{" "}
          <button className="btn btn-sm" onClick={onSave}>
            Save…
          </button>{" "}
          <ActionLine a={a} />
        </td>
      </tr>
      {raw && (
        <tr>
          <td colSpan={3}>
            <div className="pr-note">Timings (µs, mark first): {c.timings.join(" ")}</div>
            {c.pronto && <div className="pr-note mono" style={{ overflowWrap: "anywhere" }}>Pronto: {c.pronto}</div>}
          </td>
        </tr>
      )}
    </>
  );
}

function SaveCapture({ c, st, onDone }: { c: IrCapture; st: IrState; onDone(): void }) {
  const b = useBob();
  const a = useAction();
  const [device, setDevice] = useState(st.library[0]?.device ?? "");
  const [button, setButton] = useState("");
  const save = async () => {
    if (await a.run(() => b.api("POST", "/api/ir/library", { device, button, fromCapture: c.seq }), { busy: "Saving…" })) onDone();
  };
  return (
    <div className="led-card">
      <div className="pr-row">
        <strong>Got {codeText(c.code, c.timings)}</strong>
      </div>
      <div className="pr-row">
        <label>
          Device <input list="ir-devices" value={device} placeholder="e.g. TV" aria-label="Device" onInput={(e) => setDevice(e.currentTarget.value)} />
          <datalist id="ir-devices">
            {st.library.map((d) => (
              <option key={d.device} value={d.device} />
            ))}
          </datalist>
        </label>
        <label>
          Button <input value={button} placeholder="e.g. power" aria-label="Button" onInput={(e) => setButton(e.currentTarget.value)} />
        </label>
        <button className="btn btn-sm btn-primary" disabled={!device.trim() || !button.trim() || a.busy} onClick={save}>
          Save
        </button>
        <button className="btn btn-sm" onClick={onDone}>
          Discard
        </button>
        <ActionLine a={a} />
      </div>
    </div>
  );
}

function Library({ st, onDone }: { st: IrState; onDone(): void }) {
  const b = useBob();
  const a = useAction();
  if (st.library.length === 0) return null;
  const send = (device: string, button: string) => a.run(() => b.api("POST", "/api/ir/send", { device, button }), { busy: `Sending ${button}…`, ok: `Sent ${device} / ${button}` });
  const del = async (device: string, button?: string) => {
    if (!(await b.confirm(button ? `Delete ${button}` : `Delete ${device}`, button ? `From ${device} in the IR library.` : "And every button saved for it."))) return;
    await a.run(() => b.api("POST", "/api/ir/library", { device, button, delete: true }), { busy: "Deleting…" });
    onDone();
  };
  return (
    <>
      <div className="stat-label">Library</div>
      {st.library.map((d) => (
        <div key={d.device} className="led-card">
          <div className="pr-row">
            <strong>{d.device}</strong>
            {d.buttons.map((x) => (
              <span key={x.name} className="ir-btn">
                <button className="btn btn-sm" disabled={!st.outputEnabled || st.transmitters.length === 0} onClick={() => send(d.device, x.name)}>
                  {x.name}
                </button>
                <button className="btn btn-sm" aria-label={`Delete ${x.name}`} onClick={() => del(d.device, x.name)}>
                  ×
                </button>
              </span>
            ))}
            <button className="btn btn-sm btn-warn" onClick={() => del(d.device)}>
              Delete device
            </button>
          </div>
        </div>
      ))}
      <ActionLine a={a} />
    </>
  );
}

function SendForm({ st }: { st: IrState }) {
  const b = useBob();
  const a = useAction();
  const [protocol, setProtocol] = useState("nec");
  const [address, setAddress] = useState("0x00");
  const [command, setCommand] = useState("0x00");
  const [pronto, setPronto] = useState("");
  const n = (s: string) => (s.trim().toLowerCase().startsWith("0x") ? parseInt(s.trim(), 16) : Number(s));
  return (
    <>
      <div className="stat-label">Send a code</div>
      <div className="pr-row">
        <select value={protocol} aria-label="Protocol" onChange={(e) => setProtocol(e.currentTarget.value)}>
          {st.protocols
            .filter((p) => p.name !== "nec-repeat")
            .map((p) => (
              <option key={p.name} value={p.name}>
                {p.name}
              </option>
            ))}
        </select>
        <label>
          Address <input value={address} style={{ width: 70 }} onInput={(e) => setAddress(e.currentTarget.value)} />
        </label>
        <label>
          Command <input value={command} style={{ width: 70 }} onInput={(e) => setCommand(e.currentTarget.value)} />
        </label>
        <button className="btn btn-sm" disabled={!st.outputEnabled || a.busy} onClick={() => a.run(() => b.api("POST", "/api/ir/send", { protocol, address: n(address), command: n(command) }), { busy: "Sending…", ok: "Sent" })}>
          Send
        </button>
      </div>
      <div className="pr-row">
        <input value={pronto} placeholder="or paste Pronto hex: 0000 006D …" style={{ flex: 1 }} aria-label="Pronto hex" onInput={(e) => setPronto(e.currentTarget.value)} />
        <button className="btn btn-sm" disabled={!st.outputEnabled || !pronto.trim() || a.busy} onClick={() => a.run(() => b.api("POST", "/api/ir/send", { pronto }), { busy: "Sending…", ok: "Sent" })}>
          Send Pronto
        </button>
        <ActionLine a={a} />
      </div>
    </>
  );
}
