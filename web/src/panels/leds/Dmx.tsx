// DMX512 outputs (backend/crates/leds/src/dmx.rs, `/api/dmx`): a universe out of a serial
// port, bridged from sACN / Art-Net or set by hand. Linux bobs only, so it lives beside the
// LED strings in the Linux panel and not in the node build.

import { useState } from "react";
import { useBob } from "../../bob/context";
import { Hint, Note, Pill } from "../../ui/common";
import { ActionLine, useAction } from "../../ui/status";
import { usePoll } from "../../sections/node/usePoll";

export interface DmxOut {
  id: number;
  name: string;
  port: string;
  mode: "raw" | "enttec-pro";
  refreshHz: number;
  network: { protocol: "sacn" | "artnet"; universe: number } | null;
  source: "network" | "manual";
  receiving: { from: string; packets: number; ageMs: number } | null;
  state: "running" | "off" | "pending" | "error";
  error: string | null;
  /** 512 levels as hex. */
  levels: string;
}

export interface DmxState {
  outputEnabled: boolean;
  maxOutputs: number;
  ports: { path: string; device: string | null; name: string }[];
  outputs: DmxOut[];
}

const STATE: Record<DmxOut["state"], [string, string]> = {
  running: ["on", "sending"],
  off: ["", "not sending: output control off"],
  pending: ["warn", "starting"],
  error: ["bad", "error"],
};

const levelsOf = (hex: string) => (hex.match(/../g) ?? []).map((h) => parseInt(h, 16));

export function DmxOutputs() {
  const b = useBob();
  const { data, error, reload } = usePoll<DmxState>("/api/dmx", 2000);
  if (error) return <Note color="danger">DMX: {error}</Note>;
  if (!data) return null;
  return (
    <div className="pr-body">
      <div className="pr-row">
        <strong>DMX512 outputs</strong>
        <button className="btn btn-sm" disabled={data.outputs.length >= data.maxOutputs} onClick={() => b.openModal(<DmxDialog st={data} onDone={reload} />)}>
          Add DMX output…
        </button>
        <Hint>
          A DMX universe out of a serial port: a plain RS-485 adapter (FTDI "Open DMX" dongles, any USB-RS485 with a MAX485; BenchBob makes the break and timing) or an Enttec DMX USB Pro. Feed it from sACN or Art-Net (a console or xLights sends the universe; the last look holds if it stops) or set levels by hand. Frames go out only while GPIO output control is on.
        </Hint>
      </div>
      {data.outputs.length === 0 && <div className="pr-note">No DMX outputs.</div>}
      {data.outputs.map((o) => (
        <DmxCard key={o.id} o={o} st={data} onDone={reload} />
      ))}
    </div>
  );
}

function DmxCard({ o, st, onDone }: { o: DmxOut; st: DmxState; onDone(): void }) {
  const b = useBob();
  const a = useAction();
  const [ch, setCh] = useState(1);
  const lv = levelsOf(o.levels);
  const set = (body: unknown) => a.run(() => b.api("POST", `/api/dmx/${o.id}/levels`, body), { busy: "Setting…" }).then(onDone);
  const remove = async () => {
    if (!(await b.confirm(`Remove ${o.name}`, "Frames stop and the serial port is released.", o.port))) return;
    await a.run(() => b.api("DELETE", `/api/dmx/${o.id}`), { busy: "Removing…" });
    onDone();
  };
  const [cls, text] = STATE[o.state];
  return (
    <div className="led-card">
      <div className="pr-row">
        <strong>{o.name}</strong>
        <Pill cls={cls}>{text}</Pill>
        <span className="pr-note">
          {o.port} · {o.mode === "raw" ? "RS-485" : "Enttec Pro"} · {o.refreshHz} Hz
          {o.network && ` · ${o.network.protocol === "sacn" ? "sACN" : "Art-Net"} universe ${o.network.universe}`} · {o.source === "network" ? "following the network" : "levels by hand"}
          {o.receiving && ` · from ${o.receiving.from}, ${o.receiving.packets.toLocaleString()} packets, last ${o.receiving.ageMs < 1500 ? "just now" : `${Math.round(o.receiving.ageMs / 1000)} s ago`}`}
        </span>
      </div>
      {o.error && <Note color="danger">{o.error}</Note>}
      <div className="dmx-levels" role="img" aria-label={`Levels of channels 1–512 (${lv.filter((v) => v > 0).length} above zero)`}>
        {lv.map((v, k) => (
          <span key={k} style={{ height: `${(v / 255) * 100}%` }} />
        ))}
      </div>
      <div className="pr-row">
        <label>
          Channel <input type="number" min={1} max={512} value={ch} style={{ width: 64 }} onInput={(e) => setCh(Math.min(512, Math.max(1, Number(e.currentTarget.value) || 1)))} />
        </label>
        <input type="range" min={0} max={255} value={lv[ch - 1] ?? 0} aria-label={`Channel ${ch} level`} onChange={(e) => set({ start: ch, values: [Number(e.currentTarget.value)] })} />
        <span className="pr-note">{lv[ch - 1] ?? 0}</span>
        <button className="btn btn-sm" onClick={() => set({ fill: 0 })}>
          All 0
        </button>
        <button className="btn btn-sm" onClick={() => set({ fill: 255 })}>
          All 255
        </button>
        <button className="btn btn-sm" onClick={() => b.openModal(<DmxDialog st={st} edit={o} onDone={onDone} />)}>
          Edit…
        </button>
        <button className="btn btn-sm btn-warn" onClick={remove}>
          Remove
        </button>
        <ActionLine a={a} />
      </div>
    </div>
  );
}

function DmxDialog({ st, edit, onDone }: { st: DmxState; edit?: DmxOut; onDone(): void }) {
  const b = useBob();
  const a = useAction();
  const [name, setName] = useState(edit?.name ?? "");
  const [port, setPort] = useState(edit?.port ?? st.ports[0]?.path ?? "");
  const [mode, setMode] = useState<DmxOut["mode"]>(edit?.mode ?? "raw");
  const [hz, setHz] = useState(String(edit?.refreshHz ?? 40));
  const [proto, setProto] = useState<"" | "sacn" | "artnet">(edit?.network?.protocol ?? "sacn");
  const [universe, setUniverse] = useState(String(edit?.network?.universe ?? 1));
  const taken = new Set(st.outputs.filter((o) => o.id !== edit?.id).map((o) => o.port));
  const save = async () => {
    const body = { name: name || undefined, port, mode, refreshHz: Number(hz), network: proto ? { protocol: proto, universe: Number(universe) } : null };
    const ok = await a.run(() => b.api("POST", edit ? `/api/dmx/${edit.id}` : "/api/dmx", body), { busy: "Saving…" });
    if (ok) {
      onDone();
      b.closeModal();
    }
  };
  return (
    <>
      <h3>{edit ? `Edit ${edit.name}` : "Add a DMX output"}</h3>
      <div className="pr-row">
        <label>
          Name <input value={name} placeholder="e.g. Stage" onInput={(e) => setName(e.currentTarget.value)} />
        </label>
      </div>
      <div className="pr-row">
        <label>
          Serial port{" "}
          <input list="dmx-ports" value={port} style={{ width: 360 }} aria-label="Serial port" onInput={(e) => setPort(e.currentTarget.value)} />
          <datalist id="dmx-ports">
            {st.ports
              .filter((p) => !taken.has(p.path))
              .map((p) => (
                <option key={p.path} value={p.path}>
                  {p.device ? `${p.name} → ${p.device}` : p.name}
                </option>
              ))}
          </datalist>
        </label>
      </div>
      <div className="pr-row">
        <label>
          Adapter{" "}
          <select value={mode} aria-label="Adapter" onChange={(e) => setMode(e.currentTarget.value as DmxOut["mode"])}>
            <option value="raw">Plain RS-485 (Open DMX, USB-RS485, UART + MAX485)</option>
            <option value="enttec-pro">Enttec DMX USB Pro</option>
          </select>
        </label>
        <label>
          Frames a second <input type="number" min={1} max={44} value={hz} style={{ width: 60 }} onInput={(e) => setHz(e.currentTarget.value)} />
        </label>
      </div>
      <div className="pr-row">
        <label>
          Input{" "}
          <select value={proto} aria-label="DMX input" onChange={(e) => setProto(e.currentTarget.value as typeof proto)}>
            <option value="sacn">sACN universe</option>
            <option value="artnet">Art-Net port-address</option>
            <option value="">levels by hand only</option>
          </select>
        </label>
        {proto && (
          <label>
            Universe <input type="number" min={proto === "sacn" ? 1 : 0} value={universe} style={{ width: 80 }} onInput={(e) => setUniverse(e.currentTarget.value)} />
          </label>
        )}
      </div>
      <p className="pr-note">The port is opened exclusively while the output exists, so the Serial panel shows it as busy. {st.outputEnabled ? "" : "GPIO output control is off: nothing is sent until it's on."}</p>
      <div className="modal-btns">
        <ActionLine a={a} />
        <button className="btn" onClick={b.closeModal}>
          Cancel
        </button>
        <button className="btn btn-primary" disabled={a.busy || !port} onClick={save}>
          {edit ? "Save" : "Add"}
        </button>
      </div>
    </>
  );
}
