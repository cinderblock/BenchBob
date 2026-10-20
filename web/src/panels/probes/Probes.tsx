// Debug probes section: one card per probe (OpenOCD GDB server, target config, flash/erase/
// reset/memory actions, live log) and one per MCU in bootloader mode (RP2 UF2, STM32 DFU).
// Config fields save as soon as they're changed; while one is being edited the status push
// leaves it alone.

import { useEffect, useRef, useState, type InputHTMLAttributes, type ReactNode, type RefObject } from "react";
import { useBob, type BobApi } from "../../bob/context";
import type { PanelProps } from "../../registry";
import { fmtAgo } from "../../lib/format";
import type { BootMcu, Clients, Probe, ProbeJob, ProbesState, Targets } from "./api";

const enc = encodeURIComponent;
const fmtBytes = (n: number) => (n >= 1048576 ? (n / 1048576).toFixed(1) + " MB" : n >= 1024 ? (n / 1024).toFixed(1) + " KB" : n + " B");
const Pill = ({ cls = "", children }: { cls?: string; children: ReactNode }) => <span className={`pr-pill ${cls}`}>{children}</span>;

export default function Probes({ st }: PanelProps<ProbesState>) {
  const b = useBob();
  const [targets, setTargets] = useState<Targets | null>(null);
  useEffect(() => {
    b.api<Targets>("GET", "/api/probes/targets").then(setTargets, () => {});
  }, []);
  const presets = new Set(targets?.presets.map((t) => t.cfg));
  const empty = !st.probes.length && !st.bootloaders.length;
  return (
    <div>
      <div className="pr-note">
        {st.openocd.path ? `OpenOCD ${st.openocd.version}` : <span style={{ color: "var(--warning)" }}>OpenOCD isn't installed (apt package openocd); probes are detected but can't be used.</span>}
      </div>
      {empty && (
        <div className="pr-note" style={{ marginTop: 8 }}>
          No debug probe or bootloader-mode MCU is connected. Plug in an ST-Link, J-Link, CMSIS-DAP (e.g. Raspberry Pi Debug Probe), ESP-Prog or Black Magic Probe, or hold BOOTSEL while plugging in a Pico.
        </div>
      )}
      {targets && (
        <datalist id="pb-targets">
          {targets.presets.map((t) => (
            <option key={t.cfg} value={t.cfg} label={t.label} />
          ))}
          {targets.all
            .filter((t) => !presets.has(t))
            .map((t) => (
              <option key={t} value={t} />
            ))}
        </datalist>
      )}
      {st.probes.map((p) => (
        <ProbeCard key={p.id} p={p} />
      ))}
      {st.bootloaders.map((m) => (
        <BootCard key={m.id} m={m} />
      ))}
    </div>
  );
}

type Jobs = { job?: ProbeJob | null; lastJob?: ProbeJob | null };

function JobLine({ d }: { d: Jobs }) {
  if (d.job)
    return (
      <div className="pr-note">
        <Pill cls="warn">{d.job.kind}</Pill> {d.job.stage}
        {d.job.pct != null ? ` · ${d.job.pct}%` : ""}…
      </div>
    );
  const j = d.lastJob;
  if (!j) return null;
  return (
    <div className="pr-note">
      <Pill cls={j.ok ? "on" : "bad"}>{j.kind}</Pill> {j.message || j.stage}
      {j.done ? ` · ${fmtAgo(j.done)}` : ""}
    </div>
  );
}

/** The OpenOCD (or bootloader tool) log over /ws/probes/:id, plus live job updates. */
function useProbeLog(b: BobApi, id: string, pre: RefObject<HTMLPreElement | null>, enabled = true) {
  const [liveJob, setLiveJob] = useState<Jobs | null>(null);
  useEffect(() => {
    if (!enabled) return;
    const add = (text: string) => {
      const el = pre.current;
      if (!el) return;
      const atEnd = el.scrollTop + el.clientHeight >= el.scrollHeight - 4;
      el.textContent += text + "\n";
      if (el.textContent!.length > 200000) el.textContent = el.textContent!.slice(-150000);
      if (atEnd) el.scrollTop = el.scrollHeight;
    };
    const ws = b.conn.ws(`/ws/probes/${enc(id)}`, {
      onmessage(m) {
        if (m?.type === "log") {
          if (pre.current) pre.current.textContent = "";
          for (const l of m.lines) add(l);
        } else if (m?.type === "line") add(m.text);
        else if (m?.type === "job") setLiveJob(m);
      },
    });
    return () => ws.close();
  }, [id, enabled]);
  return [liveJob, setLiveJob] as const;
}

/** A text/number field that shows the server's value unless it's being edited, and saves on blur/Enter. */
function CfgInput({ value, onSave, disabled, ...rest }: { value: string | number; onSave(v: string): void; disabled?: boolean } & Omit<InputHTMLAttributes<HTMLInputElement>, "value" | "onChange">) {
  const [draft, setDraft] = useState<string | null>(null);
  const commit = () => {
    if (draft != null && draft !== String(value)) onSave(draft);
    setDraft(null);
  };
  return (
    <input
      {...rest}
      disabled={disabled}
      value={draft ?? String(value ?? "")}
      onFocus={() => setDraft(String(value ?? ""))}
      onInput={(e) => setDraft(e.currentTarget.value)}
      onBlur={commit}
      onKeyDown={(e) => e.key === "Enter" && (e.currentTarget as HTMLInputElement).blur()}
    />
  );
}

function ProbeCard({ p }: { p: Probe }) {
  const b = useBob();
  const pre = useRef<HTMLPreElement>(null);
  const logBox = useRef<HTMLDetailsElement>(null);
  const hasLog = !p.gdbBuiltIn && !!p.iface;
  const [liveJob, setLiveJob] = useProbeLog(b, p.id, pre, hasLog);
  useEffect(() => setLiveJob(null), [JSON.stringify([p.job, p.lastJob])]);
  const [out, setOut] = useState("");
  const [clients, setClients] = useState<Clients | null>(null);
  const [starting, setStarting] = useState(false);
  const fileRef = useRef<HTMLInputElement>(null);
  const [addr, setAddr] = useState("");
  const [verify, setVerify] = useState(true);
  const [reset, setReset] = useState(true);
  const [maddr, setMaddr] = useState("");
  const [mlen, setMlen] = useState("256");

  const s = p.server;
  const live = !!s && s.state !== "exited";
  const runningSince = s?.state === "running" ? s.since || 1 : 0;
  useEffect(() => {
    if (!runningSince) return setClients(null);
    b.conn
      .json<Clients>("GET", `/api/probes/${enc(p.id)}/clients?host=${enc(b.conn.hostname)}&web=${enc(b.conn.baseUrl)}`)
      .then(setClients, () => setClients(null));
  }, [runningSince]);

  const meta = [p.model && p.model !== p.name ? p.model : "", p.serial ? "S/N " + p.serial : "", p.vidpid, p.usb ? "USB " + p.usb : ""].filter(Boolean).join(" · ");
  const cfg = (body: unknown) => b.api("PUT", `/api/probes/${enc(p.id)}/config`, body).catch(() => {});
  const act = async (verb: string, body?: unknown) => {
    try {
      const r = await b.api<any>("POST", `/api/probes/${enc(p.id)}/${verb}`, body);
      if (verb === "examine") setOut([r.cpus.length ? "Cores: " + r.cpus.join(", ") : "No core detected", r.targets].filter(Boolean).join("\n\n"));
      else if (r.output) setOut(r.output);
    } catch {}
  };
  const actDisabled = !!p.job || (!!s && s.state !== "running" && s.state !== "exited");

  let pill: ReactNode;
  if (p.gdbBuiltIn) pill = <Pill cls="on">built-in GDB server</Pill>;
  else if (!p.iface) pill = <Pill cls="warn">unsupported</Pill>;
  else if (!s) pill = <Pill>idle</Pill>;
  else if (s.state === "running") pill = <Pill cls="on">GDB :{s.ports.gdb}</Pill>;
  else if (s.state === "exited") pill = s.stopped ? <Pill>stopped</Pill> : <Pill cls={s.exitCode ? "bad" : ""}>exited ({s.exitCode})</Pill>;
  else pill = <Pill cls="warn">{s.state}</Pill>;

  const head = (
    <h4>
      {p.name} {pill}
      <small>
        {meta}
        {p.sim ? " · simulated" : ""}
      </small>
    </h4>
  );

  if (p.gdbBuiltIn) {
    const gdbTty = p.ttys.find((t) => t.iface === "00")?.dev || p.ttys[0]?.dev || "";
    const uart = p.ttys.find((t) => t.iface === "02")?.dev || "";
    return (
      <div className="pr-card">
        {head}
        {p.note && <div className="pr-note">{p.note}</div>}
        <div className="pr-row">
          <span className="pr-note">On BenchBob:</span>
          <span className="pr-endpoint">target extended-remote {gdbTty || "/dev/ttyACM?"}</span>
        </div>
        <div className="pr-note">
          From another machine: in the Serial panel, start a raw TCP bridge on {gdbTty || "the first port"}, then <span className="pr-endpoint">target extended-remote {b.conn.hostname}:&lt;bridge port&gt;</span>, followed by{" "}
          <span className="pr-endpoint">monitor swdp_scan</span> and <span className="pr-endpoint">attach 1</span>.{uart ? ` The target's UART is on ${uart}.` : ""}
        </div>
      </div>
    );
  }
  if (!p.iface)
    return (
      <div className="pr-card">
        {head}
        {p.note && <div className="pr-note">{p.note}</div>}
      </div>
    );

  const c = p.config,
    o = p.ocd;
  const bits: string[] = [];
  if (o.adapter) bits.push(o.adapter);
  if (o.voltage != null) bits.push(`target ${o.voltage.toFixed(2)} V`);
  if (o.dpidr) bits.push("DPIDR " + o.dpidr);
  if (o.flash) bits.push("flash " + o.flash);
  if (s?.state === "running") bits.push(`${o.gdbClients} GDB client${o.gdbClients === 1 ? "" : "s"}`);

  const flash = async () => {
    const f = fileRef.current?.files?.[0];
    if (!f) return b.toast("Choose a firmware file first", "error");
    const qs = new URLSearchParams({ name: f.name, verify: verify ? "1" : "0", reset: reset ? "1" : "0" });
    if (addr.trim()) qs.set("address", addr.trim());
    if (logBox.current) logBox.current.open = true;
    try {
      const r = await b.conn.upload<any>(`/api/probes/${enc(p.id)}/flash?${qs}`, f);
      b.toast(r.message || "Flashed", "success");
      setOut(r.output || "");
    } catch (e: any) {
      b.toast(e.message, "error");
    }
  };
  const read = async () => {
    const qs = new URLSearchParams({ address: maddr.trim(), length: mlen });
    try {
      const r = await b.api<any>("GET", `/api/probes/${enc(p.id)}/memory?${qs}`);
      setOut(r.dump || "(no data)");
    } catch {}
  };
  const erase = () =>
    b.openModal(
      <>
        <h3>Mass erase?</h3>
        <p className="pr-note">This erases the target's whole flash, including the firmware and any stored data.</p>
        <div className="modal-btns">
          <button className="btn" onClick={b.closeModal}>
            Cancel
          </button>
          <button
            className="btn btn-danger"
            onClick={() => {
              b.closeModal();
              act("erase");
            }}
          >
            Erase
          </button>
        </div>
      </>,
    );

  return (
    <div className="pr-card">
      {head}
      {p.note && <div className="pr-note">{p.note}</div>}
      {p.espSerial && p.ttys.length > 0 && <div className="pr-note">Serial port: {p.ttys.map((t) => t.dev).join(", ")} (esptool flashing via the Serial panel).</div>}
      <div>
        {o.cpus.length > 0 && (
          <div className="pr-row">
            {o.cpus.map((x) => (
              <Pill key={x} cls="on">
                {x}
              </Pill>
            ))}
          </div>
        )}
        {bits.length > 0 && <div className="pr-note">{bits.join(" · ")}</div>}
        {o.errors.length > 0 && (
          <div className="pr-note" style={{ color: "var(--danger)", whiteSpace: "pre-line" }}>
            {o.errors.join("\n")}
          </div>
        )}
        {o.hint && (
          <div className="pr-note" style={{ color: "var(--warning)" }}>
            {o.hint}
          </div>
        )}
      </div>
      <div className="pr-row">
        <label>
          Target <CfgInput className="pb-target" list="pb-targets" placeholder="target/….cfg" style={{ width: "15em" }} spellCheck={false} value={c.target} disabled={live} onSave={(v) => cfg({ target: v })} />
        </label>
        <label>
          Transport{" "}
          <select value={c.transport} disabled={live} onChange={(e) => cfg({ transport: e.currentTarget.value })}>
            {p.transports.map((t) => (
              <option key={t} value={t}>
                {t.toUpperCase()}
              </option>
            ))}
          </select>
        </label>
        <label>
          kHz <CfgInput type="number" min={1} max={100000} style={{ width: "6em" }} value={c.speed} disabled={live} onSave={(v) => cfg({ speed: +v })} />
        </label>
      </div>
      <div className="pr-row">
        {(["gdb", "telnet", "tcl"] as const).map((k) => (
          <label key={k}>
            {k === "gdb" ? "GDB" : k === "telnet" ? "Telnet" : "TCL"} <CfgInput type="number" style={{ width: "5.5em" }} value={c.ports[k]} disabled={live} onSave={(v) => cfg({ ports: { [k]: +v } })} />
          </label>
        ))}
        {!live ? (
          <button
            className="btn btn-sm btn-primary"
            disabled={starting}
            onClick={async () => {
              setStarting(true);
              try {
                await b.api("POST", `/api/probes/${enc(p.id)}/start`, {});
                b.toast("GDB server running", "success");
              } catch {}
              setStarting(false);
            }}
          >
            {starting ? "Starting…" : "Start GDB server"}
          </button>
        ) : (
          <button className="btn btn-sm" onClick={() => b.api("POST", `/api/probes/${enc(p.id)}/stop`).catch(() => {})}>
            Stop
          </button>
        )}
      </div>
      {c.effectivePorts.gdb !== c.ports.gdb && (
        <div className="pr-note">
          This instance adds {c.effectivePorts.gdb - c.ports.gdb} to every port: GDB {c.effectivePorts.gdb}, telnet {c.effectivePorts.telnet}, TCL {c.effectivePorts.tcl}.
        </div>
      )}
      {clients && (
        <div>
          <div className="pr-row">
            <span className="pr-note">GDB</span>
            <span className="pr-endpoint">{clients.gdb}</span>
          </div>
          <div className="pr-row">
            <span className="pr-note">Telnet</span>
            <span className="pr-endpoint">{clients.telnet}</span>
          </div>
          <details>
            <summary>VS Code (cortex-debug) and PlatformIO</summary>
            <div className="pr-note">launch.json configuration:</div>
            <pre className="pr-log">{clients.cortexDebug}</pre>
            <div className="pr-note">platformio.ini (debug through this server; upload through BenchBob's API):</div>
            <pre className="pr-log">{clients.platformio}</pre>
          </details>
        </div>
      )}
      <div className="pr-row">
        <button className="btn btn-sm" disabled={actDisabled} onClick={() => act("examine")}>
          Detect target
        </button>
        <button className="btn btn-sm" disabled={actDisabled} onClick={() => act("reset", { mode: "run" })}>
          Reset
        </button>
        <button className="btn btn-sm" disabled={actDisabled} onClick={() => act("reset", { mode: "halt" })}>
          Reset + halt
        </button>
        <button className="btn btn-sm" disabled={actDisabled} onClick={() => act("halt")}>
          Halt
        </button>
        <button className="btn btn-sm" disabled={actDisabled} onClick={() => act("resume")}>
          Resume
        </button>
        <button className="btn btn-sm btn-danger" disabled={actDisabled} onClick={erase}>
          Mass erase
        </button>
      </div>
      <div className="pr-row">
        <input ref={fileRef} type="file" accept=".elf,.hex,.bin,.srec,.s19,.axf,.out" style={{ maxWidth: "100%" }} />
        <label>
          Address <input placeholder="0x08000000 (.bin only)" style={{ width: "11em" }} spellCheck={false} value={addr} onInput={(e) => setAddr(e.currentTarget.value)} />
        </label>
        <label>
          <input type="checkbox" checked={verify} onChange={(e) => setVerify(e.currentTarget.checked)} /> verify
        </label>
        <label>
          <input type="checkbox" checked={reset} onChange={(e) => setReset(e.currentTarget.checked)} /> reset
        </label>
        <button className="btn btn-sm btn-primary" disabled={actDisabled} onClick={flash}>
          Flash
        </button>
      </div>
      <div className="pr-row">
        <label>
          Read memory <input placeholder="0x20000000" style={{ width: "8em" }} spellCheck={false} value={maddr} onInput={(e) => setMaddr(e.currentTarget.value)} />
        </label>
        <label>
          bytes <input type="number" min={1} max={65536} style={{ width: "6em" }} value={mlen} onInput={(e) => setMlen(e.currentTarget.value)} />
        </label>
        <button className="btn btn-sm" disabled={actDisabled} onClick={read}>
          Read
        </button>
      </div>
      {out && <pre className="pr-log">{out}</pre>}
      <JobLine d={liveJob ?? p} />
      {hasLog && (
        <details ref={logBox}>
          <summary>OpenOCD log</summary>
          <pre className="pr-log" ref={pre} />
        </details>
      )}
    </div>
  );
}

function BootCard({ m }: { m: BootMcu }) {
  const b = useBob();
  const pre = useRef<HTMLPreElement>(null);
  const logBox = useRef<HTMLDetailsElement>(null);
  const [liveJob, setLiveJob] = useProbeLog(b, m.id, pre);
  useEffect(() => setLiveJob(null), [JSON.stringify([m.job, m.lastJob])]);
  const fileRef = useRef<HTMLInputElement>(null);
  const [addr, setAddr] = useState("0x08000000");
  const meta = [m.serial ? "S/N " + m.serial : "", m.vidpid, m.usb ? "USB " + m.usb : ""].filter(Boolean).join(" · ");
  const busy = !!m.job;

  const upload = async (url: string, okMsg: (r: any) => string, openLog: boolean) => {
    const f = fileRef.current?.files?.[0];
    if (!f) return b.toast(m.kind === "rp2-bootsel" ? "Choose a .uf2 file first" : "Choose a .bin file first", "error");
    if (openLog && logBox.current) logBox.current.open = true;
    try {
      const r = await b.conn.upload<any>(url + (url.includes("?") ? "&" : "?") + "name=" + enc(f.name), f);
      b.toast(okMsg(r), "success");
    } catch (e: any) {
      b.toast(e.message, "error");
    }
  };

  return (
    <div className="pr-card">
      <h4>
        {m.name} <Pill cls="warn">bootloader</Pill>
        <small>
          {meta}
          {m.sim ? " · simulated" : ""}
        </small>
      </h4>
      <div>
        {m.kind === "rp2-bootsel" && (
          <>
            {m.mount ? (
              <div className="pr-note">
                Drive {m.label} at {m.mount}
                {m.readOnly ? " (mounted read-only; flashing remounts it read-write)" : ""}
              </div>
            ) : (
              <div className="pr-note">{m.block ? `Drive ${m.block} isn't mounted yet; flashing mounts it.` : "Waiting for its USB drive…"}</div>
            )}
            {m.info && <div className="pr-note">{[m.info.bootloader, ...Object.entries(m.info.fields).map(([k, v]) => `${k}: ${v}`)].filter(Boolean).join(" · ")}</div>}
            <div className="pr-note">{m.tool.path ? "picotool is installed." : `picotool isn't installed (apt package ${m.tool.pkg}); UF2 copy works without it.`}</div>
          </>
        )}
        {m.kind === "stm32-dfu" &&
          (m.tool.path ? (
            <div className="pr-note">Flashing uses dfu-util, alt setting 0 (internal flash), and starts the app afterwards.</div>
          ) : (
            <div className="pr-note" style={{ color: "var(--warning)" }}>
              dfu-util isn't installed, so this chip can't be flashed over DFU yet (apt package {m.tool.pkg}).
            </div>
          ))}
      </div>
      {m.kind === "rp2-bootsel" && (
        <div className="pr-row">
          <input ref={fileRef} type="file" accept=".uf2" style={{ maxWidth: "100%" }} />
          <button className="btn btn-sm btn-primary" disabled={busy} onClick={() => upload(`/api/probes/boot/${enc(m.id)}/uf2`, (r) => `${r.message} (${fmtBytes(r.payload)})`, false)}>
            Flash UF2
          </button>
        </div>
      )}
      {m.kind === "stm32-dfu" && m.tool.path && (
        <div className="pr-row">
          <input ref={fileRef} type="file" accept=".bin" style={{ maxWidth: "100%" }} />
          <label>
            Address <input style={{ width: "8em" }} spellCheck={false} value={addr} onInput={(e) => setAddr(e.currentTarget.value)} />
          </label>
          <button className="btn btn-sm btn-primary" disabled={busy} onClick={() => upload(`/api/probes/boot/${enc(m.id)}/dfu?address=${enc(addr.trim())}`, (r) => r.message, true)}>
            Flash .bin
          </button>
        </div>
      )}
      <JobLine d={liveJob ?? m} />
      <details ref={logBox}>
        <summary>Log</summary>
        <pre className="pr-log" ref={pre} />
      </details>
    </div>
  );
}
