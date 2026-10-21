// CAN section: one card per SocketCAN interface (state, bitrate, error counters, rates,
// actions), an opt-in live monitor per interface (per-ID table, frame log, send/periodic,
// decoded errors), SLCAN/vcan tools, recordings, and the socketcand/WebSocket endpoints with
// client snippets.
//
// Traffic never comes through the 2 s status push: an open monitor holds a WebSocket with
// batch=1&stats=1, so the server sends frames in 50 ms batches and an aggregated ID table at
// 4 Hz. Drawing is coalesced into one requestAnimationFrame, and the frame log is written
// straight to its element, so thousands of frames/s cost one DOM update per frame-time.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { useBob } from "../../bob/context";
import type { PanelProps } from "../../registry";
import { fmtAgo } from "../../lib/format";
import { readLocal, writeLocal } from "../../lib/storage";
import { ActionLine, useAction, type Action } from "../../ui/status";
import type { AutobitrateResult, CanFrame, CanIface, CanState, CanStats, FrameReq } from "./api";

const enc = encodeURIComponent;
const fmtBr = (b?: number) => (!b ? "?" : b >= 1e6 ? +(b / 1e6).toFixed(3) + "M" : b >= 1e3 ? +(b / 1e3).toFixed(3) + "k" : String(b));
const fmtId = (id: number, ext?: boolean) => id.toString(16).toUpperCase().padStart(ext ? 8 : 3, "0");
const fmtSize = (n: number) => (n < 1024 ? n + " B" : n < 1048576 ? (n / 1024).toFixed(1) + " KB" : (n / 1048576).toFixed(1) + " MB");
const LOG_MAX = 300;

function StPill({ st }: { st?: string }) {
  if (!st) return null;
  const cls = st === "ERROR-ACTIVE" ? "on" : st === "ERROR-WARNING" ? "warn" : st === "STOPPED" ? "" : "bad";
  return <span className={`pr-pill ${cls}`}>{st.toLowerCase()}</span>;
}

export default function Can({ st }: PanelProps<CanState>) {
  const b = useBob();
  const [mon, setMon] = useState<Record<string, boolean>>(() => readLocal(b.key("canMonitor"), {}));
  const toggleMon = (name: string) =>
    setMon((m) => {
      const n = { ...m };
      if (n[name]) delete n[name];
      else n[name] = true;
      writeLocal(b.key("canMonitor"), JSON.stringify(n));
      return n;
    });
  return (
    <div>
      <div className="can-ifaces">
        {!st.ifaces.length && <div className="pr-note">No CAN interfaces. Plug in a candleLight/CANable/PEAK adapter, attach an SLCAN adapter, or create a virtual one below.</div>}
        {st.ifaces.map((i) => (
          <div className="pr-card" key={i.name}>
            <IfaceHead i={i} st={st} monitor={!!mon[i.name]} toggleMon={() => toggleMon(i.name)} />
            {mon[i.name] && <Monitor name={i.name} />}
          </div>
        ))}
      </div>
      <Tools st={st} />
      <Endpoints st={st} />
    </div>
  );
}

function IfaceHead({ i, st, monitor, toggleMon }: { i: CanIface; st: CanState; monitor: boolean; toggleMon(): void }) {
  const b = useBob();
  // Everything done to this interface (here or from its dialogs) reports on the action row.
  const act = useAction();
  const post = <T,>(path: string, body: unknown, busy: string, ok?: string | ((r: T) => string)) => act.run(() => b.api<T>("POST", `/api/can/${enc(i.name)}/${path}`, body), { busy, ok });
  const timing =
    i.kind === "can"
      ? i.bitrate
        ? `${fmtBr(i.bitrate)}${i.samplePoint ? ` @ ${(i.samplePoint * 100).toFixed(1)}%` : ""}${i.fdOn && i.dbitrate ? ` · FD data ${fmtBr(i.dbitrate)}${i.dsamplePoint ? ` @ ${(i.dsamplePoint * 100).toFixed(1)}%` : ""}` : ""}`
        : "bitrate not set"
      : i.fdOn
        ? "virtual, CAN FD MTU"
        : "virtual, classic MTU";
  const r = i.rates || {};
  const xs = i.xstats
    ? ["bus_error", "error_warning", "error_passive", "bus_off", "restarts", "arbitration_lost"]
        .filter((k) => i.xstats![k])
        .map((k) => `${k.replace("_", " ")} ${i.xstats![k]}`)
        .join(" · ")
    : "";
  const dev = [i.label, i.driver && i.driver !== i.kind ? i.driver : "", i.usb?.serial ? "S/N " + i.usb.serial : "", i.slcanTty ? "on " + i.slcanTty : "", i.clock ? `clock ${fmtBr(i.clock)}Hz` : "", i.fdCapable && i.kind === "can" ? "FD capable" : ""].filter(Boolean).join(" · ");
  const dis = !!i.busy || act.busy;
  return (
    <>
      <h4>
        {i.name} <span className={`pr-pill ${i.up ? "on" : ""}`}>{i.up ? "up" : "down"}</span> {i.up && <StPill st={i.state} />} {i.fdOn && <span className="pr-pill">FD</span>}{" "}
        {i.ctrl
          .filter((f) => f !== "FD")
          .map((f) => (
            <span key={f} className={`pr-pill${f === "LISTEN-ONLY" ? " warn" : ""}`}>
              {f.toLowerCase()}
            </span>
          ))}{" "}
        {i.busy && <span className="pr-pill warn">{i.busy}</span>}
      </h4>
      <div className="pr-note">{dev}</div>
      <div style={{ fontSize: ".8rem", marginTop: 4 }}>
        {timing}
        {i.up && (
          <>
            {" · "}
            {r.fps || 0} fps{r.txps ? ` (${r.txps} tx)` : ""}
            {r.load != null ? ` · load ${r.load}%` : ""}
            {r.errps ? <span style={{ color: "var(--danger)" }}> · {r.errps} err/s</span> : null}
          </>
        )}
        {i.berr ? ` · TEC ${i.berr.tx} · REC ${i.berr.rx}` : ""}
      </div>
      {xs && <div className="pr-note">{xs}</div>}
      {i.bringUpOnPlug && <div className="pr-note">Brought up automatically when plugged in.</div>}
      {i.autobitrate ? (
        <div className="pr-note">
          Auto-bitrate: trying {fmtBr(i.autobitrate.bitrate)} ({i.autobitrate.step}/{i.autobitrate.of}), listen-only
        </div>
      ) : i.lastAutobitrate ? (
        <div className="pr-note">
          Last auto-bitrate ({fmtAgo(i.lastAutobitrate.at)}): <b>{i.lastAutobitrate.text}</b>
          {i.lastAutobitrate.applied ? " Applied." : ""}
          {i.lastAutobitrate.note ? " " + i.lastAutobitrate.note : ""}
        </div>
      ) : null}
      {i.recording && (
        <div className="pr-note">
          <span className="pr-pill bad">● REC</span> {i.recording.file}: {i.recording.frames} frames, {fmtSize(i.recording.bytes)}
        </div>
      )}
      {i.up && i.reader !== "open" && (
        <div className="pr-note" style={{ color: "var(--danger)" }}>
          Reader: {i.reader}
        </div>
      )}
      <div className="pr-row">
        <button className="btn btn-sm" disabled={dis} onClick={() => post("config", { up: !i.up }, i.up ? "Bringing down…" : "Bringing up…")}>
          {i.up ? "Down" : "Up"}
        </button>
        <button className="btn btn-sm" disabled={dis} onClick={() => b.openModal(<ConfigDlg i={i} st={st} act={act} />)}>
          Configure…
        </button>
        {(i.kind === "can" || i.kind === "vcan") && (
          <button className="btn btn-sm" disabled={dis} onClick={() => b.openModal(<AutoDlg i={i} st={st} act={act} />)}>
            Auto-bitrate
          </button>
        )}
        {i.state === "BUS-OFF" && (
          <button className="btn btn-sm btn-warn" disabled={act.busy} onClick={() => post("restart", undefined, "Restarting…", "Controller restarted")}>
            Restart controller
          </button>
        )}
        {i.up && (
          <button
            className={`btn btn-sm${i.recording ? " btn-danger" : ""}`}
            disabled={act.busy}
            onClick={() => {
              const on = !i.recording;
              post<{ file: string; frames: number }>("record", { on }, on ? "Starting recording…" : "Stopping recording…", (r) => (on ? `Recording to ${r.file}` : `Saved ${r.file} (${r.frames} frames)`));
            }}
          >
            {i.recording ? "Stop recording" : "Record"}
          </button>
        )}
        <button className={`btn btn-sm${monitor ? " btn-primary" : ""}`} onClick={toggleMon}>
          {monitor ? "Hide monitor" : "Monitor"}
        </button>
        {i.kind === "vcan" && (
          <button className="btn btn-sm btn-danger" disabled={act.busy} onClick={() => act.run(() => b.api("DELETE", `/api/can/vcan/${enc(i.name)}`), { busy: "Removing…" })}>
            Remove vcan
          </button>
        )}
        {i.slcanTty && (
          <button className="btn btn-sm btn-danger" disabled={act.busy} onClick={() => act.run(() => b.api("DELETE", `/api/can/slcan/${enc(i.name)}`), { busy: "Detaching…" })}>
            Detach SLCAN
          </button>
        )}
        <ActionLine a={act} />
      </div>
    </>
  );
}

// ---------- live monitor ----------

function fmtTs(ts: number) {
  const d = new Date(ts * 1000);
  return d.toTimeString().slice(0, 8) + "." + String(d.getMilliseconds()).padStart(3, "0");
}

function frameLine(f: CanFrame) {
  if (f.err) return `${fmtTs(f.ts)} ERR ${f.text || ""} [${f.data}]`;
  const data = f.rtr ? `remote request, len ${f.len}` : f.data.replace(/(..)(?=.)/g, "$1 ");
  return `${fmtTs(f.ts)} ${f.dir} ${fmtId(f.id, f.ext).padStart(8)} ${f.fd ? (f.brs ? "FD+BRS" : "FD    ") : "      "} [${String(f.len).padStart(2)}] ${data}`;
}

function DataBytes({ hex, hot }: { hex: string; hot: number }) {
  const out: ReactNode[] = [];
  for (let i = 0; i * 2 < hex.length; i++) {
    const byte = hex.substr(i * 2, 2);
    if (i) out.push(" ");
    out.push(
      hot & (1 << i) ? (
        <b key={i} style={{ color: "var(--warning)" }}>
          {byte}
        </b>
      ) : (
        byte
      ),
    );
  }
  return <>{out}</>;
}

function Monitor({ name }: { name: string }) {
  const b = useBob();
  const logEl = useRef<HTMLDivElement>(null);
  const [stats, setStats] = useState<CanStats | null>(null);
  const [conn, setConn] = useState("Connecting…");
  const [paused, setPaused] = useState(false);
  const pausedRef = useRef(false);
  const [filter, setFilter] = useState("");
  const filterRef = useRef("");
  const ws = useRef<ReturnType<typeof b.conn.ws> | null>(null);
  const m = useRef({ log: [] as string[], dirtyLog: false, stats: null as CanStats | null, dirtyStats: false, raf: 0 });

  // Send form.
  const [sid, setSid] = useState("");
  const [sdata, setSdata] = useState("");
  const [ext, setExt] = useState(false);
  const [rtr, setRtr] = useState(false);
  const [fd, setFd] = useState(false);
  const [brs, setBrs] = useState(false);
  const [per, setPer] = useState("100");
  // The log row (stream errors, Clear), the send row, and the periodic jobs list each report in place.
  const log = useAction();
  const tx = useAction();
  const jobs = useAction();
  const logFail = log.fail;

  useEffect(() => {
    const st = m.current;
    const draw = () => {
      st.raf = 0;
      if (st.dirtyLog && logEl.current) {
        st.dirtyLog = false;
        const el = logEl.current;
        const atBottom = el.scrollTop + el.clientHeight >= el.scrollHeight - 30;
        el.textContent = st.log.slice(-LOG_MAX).join("\n");
        if (atBottom) el.scrollTop = el.scrollHeight;
      }
      if (st.dirtyStats) {
        st.dirtyStats = false;
        setStats(st.stats);
      }
    };
    const schedule = () => {
      if (!st.raf) st.raf = requestAnimationFrame(draw);
    };
    const add = (frames: CanFrame[], dropped?: number) => {
      for (const f of frames) st.log.push(frameLine(f));
      if (dropped) st.log.push(`… ${dropped} frames not shown (browser or link too slow)`);
      if (st.log.length > LOG_MAX * 2) st.log.splice(0, st.log.length - LOG_MAX);
      st.dirtyLog = true;
      schedule();
    };
    const sock = (ws.current = b.conn.ws(`/ws/can/${enc(name)}?batch=1&stats=1`, {
      onopen: () => {
        setConn("");
        if (filterRef.current.trim()) sock.send({ filter: filterRef.current.trim() });
        if (pausedRef.current) sock.send({ frames: false });
      },
      onmessage: (msg) => {
        if (msg.type === "hello") {
          if (!pausedRef.current) {
            st.log = [];
            add(msg.recent || []);
          }
        } else if (msg.type === "frames") {
          if (!pausedRef.current) add(msg.frames, msg.dropped);
        } else if (msg.type === "stats") {
          st.stats = msg;
          st.dirtyStats = true;
          schedule();
        } else if (msg.type === "error") logFail(msg.message);
      },
      onclose: () => setConn("Disconnected, retrying…"),
    }));
    return () => {
      sock.close();
      cancelAnimationFrame(st.raf);
    };
  }, [name]);

  // Filter goes to the server (debounced), so the log only carries what matches.
  useEffect(() => {
    filterRef.current = filter;
    const t = setTimeout(() => ws.current?.send({ filter: filter.trim() || null }), 400);
    return () => clearTimeout(t);
  }, [filter]);

  const frame = (): FrameReq | null => {
    const id = sid.trim();
    if (!id) {
      tx.fail("Enter an ID (hex)");
      return null;
    }
    if (id.includes("#")) return { cansend: id };
    return { id, data: sdata.trim(), ext: ext || undefined, rtr: rtr || undefined, fd: fd || undefined, brs: brs || undefined };
  };
  // A successful send shows up in the log as tx, so it clears the line rather than adding to it.
  const send = () => {
    const f = frame();
    if (f) tx.run(() => b.api("POST", `/api/can/${enc(name)}/send`, f), { busy: "Sending…" });
  };
  const periodic = () => {
    const f = frame();
    if (f) tx.run(() => b.api("POST", `/api/can/${enc(name)}/periodic`, { ...f, periodMs: Number(per) }), { busy: "Starting…", ok: "Periodic job started" });
  };
  const s = stats,
    r = s?.rates || {};
  const now = Date.now();
  const errs = (s?.errors || []).slice(-5).reverse();
  return (
    <div>
      <div className="pr-note" style={{ marginTop: 10 }}>
        {conn ||
          (s ? (
            <>
              {!s.up && <b style={{ color: "var(--danger)" }}>Interface is down. </b>}
              {r.fps || 0} frames/s{r.load != null ? ` · bus load ${r.load}%` : ""} · {s.idCount} IDs · total rx {s.totals.rx}, tx {s.totals.tx}, errors {s.totals.err}
              {s.drops ? <span style={{ color: "var(--danger)" }}> · {s.drops} dropped by the kernel (receive queue overflow)</span> : null}
              {paused && <b> · log paused</b>}
            </>
          ) : (
            "Connecting…"
          ))}
      </div>
      <div style={{ overflowX: "auto", maxHeight: 340, overflowY: "auto", marginTop: 6 }}>
        <table className="pr-tbl">
          <thead>
            <tr>
              <th>ID</th>
              <th>Len</th>
              <th>Count</th>
              <th>Period</th>
              <th>Last data</th>
              <th>Seen</th>
            </tr>
          </thead>
          <tbody>
            {s?.ids?.length ? (
              s.ids.map((x) => (
                <tr key={`${x.ext ? "x" : ""}${x.id}`}>
                  <td style={{ fontFamily: "monospace" }}>
                    {fmtId(x.id, x.ext)}
                    {x.ext && <span className="pr-pill"> ext</span>}
                    {x.fd && <span className="pr-pill"> FD</span>}
                    {x.dir === "tx" && <span className="pr-pill on"> tx</span>}
                  </td>
                  <td>{x.len}</td>
                  <td>{x.count}</td>
                  <td>{x.count > 1 ? (x.period >= 1000 ? (x.period / 1000).toFixed(2) + " s" : x.period.toFixed(1) + " ms") : "—"}</td>
                  <td style={{ fontFamily: "monospace", whiteSpace: "nowrap" }}>{x.rtr ? "RTR" : <DataBytes hex={x.data} hot={x.hot} />}</td>
                  <td style={{ whiteSpace: "nowrap" }}>{now - x.last < 1500 ? "now" : fmtAgo(x.last)}</td>
                </tr>
              ))
            ) : (
              <tr>
                <td colSpan={6} className="pr-note">
                  No frames yet
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      {errs.length > 0 && (
        <>
          <div className="pr-note" style={{ marginTop: 8 }}>
            <b>Recent error frames</b>
          </div>
          {errs.map((e, i) => (
            <div key={i} className="pr-note">
              {new Date(e.ts).toLocaleTimeString()}: {e.text}
              {e.tec != null ? ` (TEC ${e.tec}, REC ${e.rec})` : ""}
            </div>
          ))}
        </>
      )}
      <div className="pr-row" style={{ marginTop: 12 }}>
        <label>
          Log filter <input placeholder="7E8, 100:700, x18DAF100:1FFFFF00" style={{ width: 220, maxWidth: "100%" }} value={filter} onInput={(e) => setFilter(e.currentTarget.value)} />
        </label>
        <button
          className="btn btn-sm"
          onClick={() => {
            const p = !paused;
            pausedRef.current = p;
            setPaused(p);
            ws.current?.send({ frames: !p }); // stop the server sending frames we'd throw away
          }}
        >
          {paused ? "Resume" : "Pause"}
        </button>
        <button
          className="btn btn-sm"
          onClick={() => {
            m.current.log = [];
            if (logEl.current) logEl.current.textContent = "";
            log.run(() => b.api("POST", `/api/can/${enc(name)}/clear`), { busy: "Clearing…" });
          }}
        >
          Clear
        </button>
        <ActionLine a={log} />
      </div>
      <div className="pr-note">Filter: hex IDs, optional :mask, x prefix for extended. Applied on the server, so the log only carries what matches; the ID table always shows everything.</div>
      <div className="pr-log can-log" style={{ height: 220 }} ref={logEl} />
      <div className="pr-row" style={{ marginTop: 12 }}>
        <label>
          ID <input placeholder="123" style={{ width: 90 }} value={sid} onInput={(e) => setSid(e.currentTarget.value)} onKeyDown={(e) => e.key === "Enter" && send()} />
        </label>
        <label>
          Data <input placeholder="DE AD BE EF" style={{ width: 170, maxWidth: "100%" }} value={sdata} onInput={(e) => setSdata(e.currentTarget.value)} onKeyDown={(e) => e.key === "Enter" && send()} />
        </label>
        <label>
          <input type="checkbox" checked={ext} onChange={(e) => setExt(e.currentTarget.checked)} /> ext
        </label>
        <label>
          <input type="checkbox" checked={rtr} onChange={(e) => setRtr(e.currentTarget.checked)} /> RTR
        </label>
        <label>
          <input type="checkbox" checked={fd} onChange={(e) => setFd(e.currentTarget.checked)} /> FD
        </label>
        <label>
          <input type="checkbox" checked={brs} onChange={(e) => setBrs(e.currentTarget.checked)} /> BRS
        </label>
        <button className="btn btn-sm btn-primary" onClick={send}>
          Send
        </button>
        <label>
          every <input type="number" min={5} style={{ width: 70 }} value={per} onInput={(e) => setPer(e.currentTarget.value)} /> ms
        </label>
        <button className="btn btn-sm" disabled={tx.busy} onClick={periodic}>
          Start periodic
        </button>
        <ActionLine a={tx} />
      </div>
      <div className="pr-note">
        ID and data are hex. You can also type cansend syntax in ID, e.g. <span className="pr-endpoint">123#DEADBEEF</span> or <span className="pr-endpoint">123##1AABBCC</span> (FD+BRS). Frames you send appear in the log as <b>tx</b> once the
        controller confirms them; with nothing else on the bus to ACK, they never do.
      </div>
      {s?.jobs?.length ? (
        <>
          <div className="pr-note" style={{ marginTop: 8 }}>
            <b>Periodic jobs</b> <ActionLine a={jobs} />
          </div>
          {s.jobs.map((j) => (
            <div key={j.id} className="pr-row">
              <span className="pr-endpoint">{j.frame}</span>
              <span className="pr-note">
                every {j.periodMs} ms · sent {j.sent}
                {j.errors ? ` · ${j.errors} errors${j.lastErr ? ": " + j.lastErr : ""}` : ""}
              </span>
              <button className="btn btn-sm" disabled={jobs.busy} onClick={() => jobs.run(() => b.api("DELETE", `/api/can/${enc(name)}/periodic/${j.id}`), { busy: "Stopping…" })}>
                Stop
              </button>
            </div>
          ))}
        </>
      ) : null}
    </div>
  );
}

// ---------- dialogs ----------

// `act` is the interface card's action line: the run is reported there once the dialog closes.
function AutoDlg({ i, st, act }: { i: CanIface; st: CanState; act: Action }) {
  const b = useBob();
  const [ms, setMs] = useState("400");
  const [apply, setApply] = useState(false);
  const go = async () => {
    b.closeModal();
    const r = await act.run(() => b.api<AutobitrateResult>("POST", `/api/can/${enc(i.name)}/autobitrate`, { listenMs: Number(ms) || 400, apply }), { busy: "Auto-bitrate running…", ok: (r) => `Auto-bitrate: ${r.text}` });
    if (r) b.openModal(<AutoResult name={i.name} r={r} />);
  };
  return (
    <>
      <h3>Auto-detect bitrate on {i.name}</h3>
      <p className="pr-note">
        Tries each standard rate (1M down to 10k) in <b>listen-only</b> mode: the controller never transmits or ACKs, so this is safe on a live bus. At each rate it listens briefly and counts clean frames against error frames and bus errors. It takes
        about {((st.standardBitrates?.length || 11) * 0.6) | 0} s, and the interface goes down and up once per rate.
        {i.kind === "vcan" && (
          <>
            {" "}
            <b>vcan has no bit timing</b>: every rate sees the same traffic, so this only exercises the procedure.
          </>
        )}
      </p>
      <div className="pr-row">
        <label>
          Listen per rate <input type="number" min={100} max={5000} value={ms} onInput={(e) => setMs(e.currentTarget.value)} style={{ width: 80 }} /> ms
        </label>
      </div>
      <div className="pr-row">
        <label>
          <input type="checkbox" checked={apply} onChange={(e) => setApply(e.currentTarget.checked)} /> Apply the detected bitrate (otherwise the old config is restored)
        </label>
      </div>
      <div className="modal-btns">
        <button className="btn" onClick={b.closeModal}>
          Cancel
        </button>
        <button className="btn btn-primary" onClick={go}>
          Start
        </button>
      </div>
    </>
  );
}

function AutoResult({ name, r }: { name: string; r: AutobitrateResult }) {
  const b = useBob();
  return (
    <>
      <h3>Auto-bitrate: {name}</h3>
      <p>
        <b>{r.text}</b>
      </p>
      {r.note && <p className="pr-note">{r.note}</p>}
      <div style={{ overflowX: "auto" }}>
        <table className="pr-tbl">
          <thead>
            <tr>
              <th>Bitrate</th>
              <th>Frames</th>
              <th>Error frames</th>
              <th>Bus errors</th>
            </tr>
          </thead>
          <tbody>
            {r.trials.map((t) => (
              <tr key={t.bitrate} style={t.bitrate === r.bitrate ? { color: "var(--success)" } : undefined}>
                <td>{fmtBr(t.bitrate)}</td>
                {t.skipped ? (
                  <td colSpan={3} className="pr-note">
                    skipped: {t.skipped}
                  </td>
                ) : (
                  <>
                    <td>{t.frames}</td>
                    <td>{t.errors}</td>
                    <td>{t.busErrors}</td>
                  </>
                )}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="modal-btns">
        <button className="btn" onClick={b.closeModal}>
          Close
        </button>
      </div>
    </>
  );
}

function Chk({ label, checked, set, note }: { label: string; checked: boolean; set(v: boolean): void; note?: ReactNode }) {
  return (
    <label style={{ display: "flex", gap: 6, alignItems: "flex-start", fontSize: ".82rem", margin: "4px 0" }}>
      <input type="checkbox" checked={checked} onChange={(e) => set(e.currentTarget.checked)} />
      <span>
        {label}
        {note && (
          <>
            <br />
            <span className="pr-note">{note}</span>
          </>
        )}
      </span>
    </label>
  );
}

function ConfigDlg({ i, st, act }: { i: CanIface; st: CanState; act: Action }) {
  const b = useBob();
  const std = st.standardBitrates || [];
  const custom = !!i.bitrate && !std.includes(i.bitrate);
  const [br, setBr] = useState(custom ? "custom" : String(i.bitrate || 500000));
  const [brc, setBrc] = useState(custom ? String(i.bitrate) : "");
  const [sp, setSp] = useState(i.samplePoint ? (i.samplePoint * 100).toFixed(1) : "");
  const [fd, setFd] = useState(!!i.fdOn);
  const [dbr, setDbr] = useState(String(i.dbitrate || ""));
  const [dsp, setDsp] = useState(i.dsamplePoint ? (i.dsamplePoint * 100).toFixed(1) : "");
  const [lo, setLo] = useState(i.ctrl.includes("LISTEN-ONLY"));
  const [lb, setLb] = useState(i.ctrl.includes("LOOPBACK"));
  const [ts, setTs] = useState(i.ctrl.includes("TRIPLE-SAMPLING"));
  const [rms, setRms] = useState(String(i.restartMs ?? 0));
  const [plug, setPlug] = useState(!!i.saved?.bringUpOnPlug);
  const sup = (f: string) => !i.ctrlSupported || i.ctrlSupported.includes(f);
  const isCan = i.kind === "can";
  const go = async (up: boolean) => {
    const c: Record<string, any> = { up, bringUpOnPlug: plug };
    if (isCan ? i.fdCapable : true) c.fd = fd;
    if (isCan) {
      c.bitrate = Number(br === "custom" ? brc : br) || undefined;
      if (sp) c.samplePoint = Number(sp) / 100;
      if (c.fd) {
        c.dbitrate = Number(dbr) || undefined;
        if (dsp) c.dsamplePoint = Number(dsp) / 100;
      }
      if (sup("LISTEN-ONLY")) c.listenOnly = lo;
      if (sup("LOOPBACK")) c.loopback = lb;
      if (sup("TRIPLE-SAMPLING")) c.tripleSampling = ts;
      c.restartMs = Number(rms) || 0;
    }
    b.closeModal();
    act.run(() => b.api("POST", `/api/can/${enc(i.name)}/config`, c), { busy: "Configuring…", ok: "Configured" });
  };
  return (
    <>
      <h3>Configure {i.name}</h3>
      {isCan ? (
        <>
          <div className="pr-row">
            <label>
              Bitrate{" "}
              <select value={br} onChange={(e) => setBr(e.currentTarget.value)}>
                {std.map((x) => (
                  <option key={x} value={x}>
                    {fmtBr(x)}
                  </option>
                ))}
                <option value="custom">custom…</option>
              </select>
            </label>
            <label>
              custom <input type="number" placeholder="bit/s" style={{ width: 100 }} value={brc} onInput={(e) => setBrc(e.currentTarget.value)} />
            </label>
          </div>
          <div className="pr-row">
            <label>
              Sample point <input type="number" step={0.1} min={50} max={95} placeholder="auto" value={sp} onInput={(e) => setSp(e.currentTarget.value)} style={{ width: 80 }} /> %
            </label>
            <span className="pr-note">Empty lets the kernel pick (CiA recommends 87.5%)</span>
          </div>
          {i.fdCapable ? (
            <>
              <Chk label="CAN FD" checked={fd} set={setFd} />
              <div className="pr-row">
                <label>
                  Data bitrate <input type="number" placeholder="2000000" value={dbr} onInput={(e) => setDbr(e.currentTarget.value)} style={{ width: 110 }} />
                </label>
                <label>
                  Data sample point <input type="number" step={0.1} placeholder="auto" value={dsp} onInput={(e) => setDsp(e.currentTarget.value)} style={{ width: 70 }} /> %
                </label>
              </div>
            </>
          ) : (
            <p className="pr-note">This controller does not support CAN FD.</p>
          )}
          {sup("LISTEN-ONLY") && <Chk label="Listen-only" checked={lo} set={setLo} note="Never transmits or ACKs. Safe for sniffing a live bus." />}
          {sup("LOOPBACK") && <Chk label="Loopback" checked={lb} set={setLb} note="Controller internal loopback: test sending without a bus." />}
          {sup("TRIPLE-SAMPLING") && <Chk label="Triple sampling" checked={ts} set={setTs} />}
          <div className="pr-row">
            <label>
              Auto-restart after bus-off <input type="number" min={0} value={rms} onInput={(e) => setRms(e.currentTarget.value)} style={{ width: 80 }} /> ms
            </label>
            <span className="pr-note">0 = stay bus-off until restarted by hand</span>
          </div>
        </>
      ) : (
        <>
          <p className="pr-note">A virtual interface has no bit timing. FD here only sets the 72-byte MTU.</p>
          <Chk label="CAN FD frames" checked={fd} set={setFd} />
        </>
      )}
      <Chk label="Bring up on plug" checked={plug} set={setPlug} note={`Re-apply this config whenever this adapter appears. Saved per adapter (${i.key}).`} />
      <div className="modal-btns">
        <button className="btn" onClick={b.closeModal}>
          Cancel
        </button>
        <button className="btn" onClick={() => go(false)}>
          Apply, leave down
        </button>
        <button className="btn btn-primary" onClick={() => go(true)}>
          Apply and bring up
        </button>
      </div>
    </>
  );
}

// ---------- tools, recordings, endpoints ----------

function BitrateSel({ rates, value, set }: { rates: number[]; value: string; set(v: string): void }) {
  return (
    <select value={value} onChange={(e) => set(e.currentTarget.value)}>
      {rates.map((x) => (
        <option key={x} value={x}>
          {fmtBr(x)}
        </option>
      ))}
    </select>
  );
}

function Candidate({ c, rates }: { c: CanState["slcan"]["candidates"][number]; rates: number[] }) {
  const b = useBob();
  const [br, setBr] = useState("500000");
  // Attach and Detach swap places, so the row holds their status.
  const act = useAction();
  return (
    <div className="pr-row">
      <span>
        <b>{c.kind}</b>{" "}
        <span className="pr-note">
          {c.tty}
          {c.serial ? " · S/N " + c.serial : ""}
        </span>
      </span>
      {c.attached ? (
        <>
          <span className="pr-pill on">attached as {c.attached}</span>
          <button className="btn btn-sm" disabled={act.busy} onClick={() => act.run(() => b.api("DELETE", `/api/can/slcan/${enc(c.attached!)}`), { busy: "Detaching…", ok: "Detached" })}>
            Detach
          </button>
        </>
      ) : (
        <>
          <BitrateSel rates={rates} value={br} set={setBr} />
          <AttachBtn tty={c.tty} bitrate={br} label="Attach as SLCAN" primary act={act} />
        </>
      )}
      <ActionLine a={act} />
    </div>
  );
}

function AttachBtn({ tty, bitrate, label, primary, act }: { tty: string; bitrate: string; label: string; primary?: boolean; act: Action }) {
  const b = useBob();
  return (
    <button
      className={`btn btn-sm${primary ? " btn-primary" : ""}`}
      disabled={act.busy}
      onClick={() => {
        if (!tty) return act.fail("Enter the serial device, e.g. /dev/ttyACM0");
        act.run(() => b.api<{ name: string }>("POST", "/api/can/slcan", { tty, bitrate: Number(bitrate) || 500000 }), { busy: "Attaching…", ok: (r) => `Attached as ${r.name}` });
      }}
    >
      {label}
    </button>
  );
}

function Tools({ st }: { st: CanState }) {
  const b = useBob();
  const rates = st.slcanBitrates || [];
  const [mtty, setMtty] = useState("");
  const [mbr, setMbr] = useState("500000");
  const attach = useAction();
  const vcan = useAction();
  const recs = useAction();
  return (
    <div className="can-tools">
      <div className="pr-card">
        <h4>Adapters and test interfaces</h4>
        {st.slcan.candidates.length ? (
          st.slcan.candidates.map((c) => <Candidate key={c.tty} c={c} rates={rates} />)
        ) : (
          <div className="pr-note">No SLCAN serial adapters detected (CANable slcan firmware 16d0:117e, Lawicel CANUSB 0403:ffa8).</div>
        )}
        <div className="pr-note">
          Attaching runs <span className="pr-endpoint">slcand</span> on the serial port and creates a SocketCAN interface. While attached, the serial panel can't open that port; detach to use it as a serial port again.
        </div>
        <details style={{ marginTop: 6 }}>
          <summary className="pr-note" style={{ cursor: "pointer" }}>
            Attach another serial device as SLCAN
          </summary>
          <div className="pr-row">
            <input placeholder="/dev/ttyACM0" style={{ width: 150 }} value={mtty} onInput={(e) => setMtty(e.currentTarget.value)} />
            <BitrateSel rates={rates} value={mbr} set={setMbr} />
            <AttachBtn tty={mtty.trim()} bitrate={mbr} label="Attach" act={attach} />
            <ActionLine a={attach} />
          </div>
        </details>
        <div className="pr-row" style={{ marginTop: 8 }}>
          <button className="btn btn-sm" disabled={vcan.busy} onClick={() => vcan.run(() => b.api<{ name: string }>("POST", "/api/can/vcan", { fd: true }), { busy: "Creating…", ok: (r) => `Created ${r.name}` })}>
            Create virtual CAN (vcan) for testing
          </button>
          <span className="pr-note">A software bus with no hardware: everything sent on it is received by every listener on it.</span>
          <ActionLine a={vcan} />
        </div>
      </div>
      {st.recordings.length > 0 && (
        <div className="pr-card">
          <h4>
            Recordings <small>candump -L format; replay with canplayer -I file</small>
          </h4>
          <ActionLine a={recs} />
          {st.recordings.map((r) => (
            <div key={r.file} className="pr-row">
              <a className="btn btn-sm" href={b.conn.url(`/api/can/recordings/${enc(r.file)}`)} download>
                {r.file}
              </a>
              <span className="pr-note">
                {fmtSize(r.size)} · {fmtAgo(r.mtime)}
              </span>
              <button className="btn btn-sm btn-danger" disabled={recs.busy} onClick={() => recs.run(() => b.api("DELETE", `/api/can/recordings/${enc(r.file)}`), { busy: `Deleting ${r.file}…`, ok: `Deleted ${r.file}` })}>
                Delete
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function Endpoints({ st }: { st: CanState }) {
  const b = useBob();
  const sc = st.socketcand;
  const host = b.conn.hostname;
  const bus = st.ifaces[0]?.name || "can0";
  const box = useRef<HTMLDivElement>(null);
  const [en, setEn] = useState(sc.enabled);
  const [port, setPort] = useState(String(sc.port));
  const [bc, setBc] = useState(sc.beacon);
  const save = useAction();
  // Follow the server's settings, but not while the user is editing them.
  const sig = JSON.stringify([sc.enabled, sc.port, sc.beacon]);
  useEffect(() => {
    if (box.current?.contains(document.activeElement) && document.activeElement?.tagName === "INPUT") return;
    setEn(sc.enabled);
    setPort(String(sc.port));
    setBc(sc.beacon);
  }, [sig]);
  const py = `import can\nbus = can.Bus(interface="socketcand", host="${host}", port=${sc.port}, channel="${bus}")\nbus.send(can.Message(arbitration_id=0x123, data=[0xDE, 0xAD], is_extended_id=False))\nfor msg in bus:\n    print(msg)`;
  const wsjs = `const ws = new WebSocket("${b.conn.wsUrl(`/ws/can/${bus}?filter=7E8`)}");\nws.onmessage = e => console.log(JSON.parse(e.data));\nws.onopen = () => ws.send(JSON.stringify({ send: { id: "7DF", data: "0201000000000000" } }));`;
  return (
    <div className="pr-card" ref={box}>
      <h4>
        Network access <small>for programs and other machines</small>
      </h4>
      <div className="pr-row">
        <label>
          <input type="checkbox" checked={en} onChange={(e) => setEn(e.currentTarget.checked)} /> socketcand server
        </label>
        <label>
          port <input type="number" value={port} onInput={(e) => setPort(e.currentTarget.value)} style={{ width: 80 }} />
        </label>
        <label>
          <input type="checkbox" checked={bc} onChange={(e) => setBc(e.currentTarget.checked)} /> discovery beacon (UDP 42000)
        </label>
        <button className="btn btn-sm" disabled={save.busy} onClick={() => save.run(() => b.api("POST", "/api/can/socketcand", { enabled: en, port: Number(port), beacon: bc }), { busy: "Applying…", ok: "socketcand settings saved" })}>
          Apply
        </button>
        <ActionLine a={save} />
      </div>
      <div className="pr-note">
        {sc.listening ? (
          <>
            Listening on{" "}
            <span className="pr-endpoint">
              {host}:{sc.port}
            </span>
          </>
        ) : sc.enabled ? (
          <span style={{ color: "var(--danger)" }}>Not listening: {sc.error || "starting"}</span>
        ) : (
          "Off"
        )}
        {sc.beacon && sc.beaconTo ? ` · beacon to ${sc.beaconTo}` : ""}. Raw and BCM modes of the socketcand protocol, classic CAN frames only (no FD), no bus configuration from the client.
      </div>
      {sc.clients.length ? (
        sc.clients.map((c) => (
          <div key={c.peer} className="pr-note">
            Client {c.peer} on {c.iface || "(no bus)"} · {c.mode} mode · rx {c.rx} · tx {c.tx}
            {c.dropped ? ` · ${c.dropped} dropped` : ""}
          </div>
        ))
      ) : (
        <div className="pr-note">No clients connected.</div>
      )}
      <details style={{ marginTop: 8 }}>
        <summary className="pr-note" style={{ cursor: "pointer" }}>
          python-can
        </summary>
        <pre className="pr-log" style={{ userSelect: "all" }}>
          {py}
        </pre>
        <div className="pr-note">
          Needs python-can 4.x. With the beacon on, <span className="pr-endpoint">can.detect_available_configs("socketcand")</span> finds BenchBob by itself.
        </div>
      </details>
      <details>
        <summary className="pr-note" style={{ cursor: "pointer" }}>
          SavvyCAN
        </summary>
        <div className="pr-note" style={{ margin: "6px 0" }}>
          Connection → Add New Device Connection → <b>socketcand</b>. Enter host <span className="pr-endpoint">{host}</span>, port <span className="pr-endpoint">{sc.port}</span> and bus <span className="pr-endpoint">{bus}</span> (or pick BenchBob from the list when
          the beacon is on). SavvyCAN reconnects if the bus is silent for 10 s; that's its socketcand client, not a fault here.
        </div>
      </details>
      <details>
        <summary className="pr-note" style={{ cursor: "pointer" }}>
          WebSocket and REST
        </summary>
        <pre className="pr-log" style={{ userSelect: "all" }}>
          {wsjs}
        </pre>
        <div className="pr-note">
          Each message is <span className="pr-endpoint">{"{type:\"frame\", id, ext, rtr, fd, brs, len, data, ts, dir}"}</span>. Add <span className="pr-endpoint">batch=1</span> for 50 ms batches and <span className="pr-endpoint">stats=1</span> for the ID table. One-shot
          capture: <span className="pr-endpoint">curl '{b.conn.url(`/api/can/${bus}/capture?ms=2000&filter=7E8`)}'</span>. Full list at{" "}
          <a href={b.conn.url("/api")} target="_blank" rel="noopener">
            /api
          </a>
          ; agents use the MCP tools at <span className="pr-endpoint">/mcp</span> (can_list, can_send, can_capture…).
        </div>
      </details>
    </div>
  );
}
