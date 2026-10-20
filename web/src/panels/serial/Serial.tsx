// Serial ports: one card per port. The card that has its terminal open holds a WebSocket
// to /ws/serial/:id (binary = raw bytes both ways, JSON text = control/status); every other
// part of the section is driven by the 2 s status push. Controls go through the REST API so
// failures toast the same way as the rest of the dashboard.

import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { useBob } from "../../bob/context";
import type { PanelProps } from "../../registry";
import { fmtN } from "../../lib/format";
import { readLocal, readLocalStr, writeLocal } from "../../lib/storage";
import { IfaceButton, useIfaceAvailable } from "../gpio/IfaceButton";
import type { FlashJob, SerialPort, SerialState, SerialWsMsg } from "./api";
import { Term } from "./term";
import "./style.css";

const BAUDS = [300, 1200, 2400, 4800, 9600, 14400, 19200, 38400, 57600, 74880, 115200, 230400, 250000, 460800, 500000, 921600, 1000000, 1500000, 2000000, 3000000];
const RESET_LABELS: Record<string, string> = { esp: "Reset (ESP)", "esp-boot": "Reset into bootloader", dtr: "Reset (DTR pulse)", touch1200: "1200 bps touch (bootloader)" };
const EOLS: Record<string, string> = { none: "", cr: "\r", lf: "\n", crlf: "\r\n" };
const KEYS: Record<string, string | null> = { Enter: null, Backspace: "\x7f", Tab: "\t", Escape: "\x1b", ArrowUp: "\x1b[A", ArrowDown: "\x1b[B", ArrowRight: "\x1b[C", ArrowLeft: "\x1b[D", Home: "\x1b[H", End: "\x1b[F", Delete: "\x1b[3~", PageUp: "\x1b[5~", PageDown: "\x1b[6~", Insert: "\x1b[2~" };
const enc = new TextEncoder();
const eid = encodeURIComponent;
function b64(s: string) {
  const bin = atob(s);
  const u = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) u[i] = bin.charCodeAt(i);
  return u;
}

export default function Serial({ st }: PanelProps<SerialState>) {
  const b = useBob();
  const [selected, setSel] = useState(() => readLocalStr(b.key("serialTerm")) || "");
  const select = (id: string) => {
    writeLocal(b.key("serialTerm"), id || null);
    setSel(id);
  };
  // The open terminal's socket pushes fresher port state than the 2 s status; it wins until
  // the next status push.
  const [patch, setPatch] = useState<SerialPort | null>(null);
  useEffect(() => setPatch(null), [st]);
  const [manual, setManual] = useState("");
  const iface = useIfaceAvailable();

  return (
    <div>
      <div className="ser-list">
        {!st.ports.length && (
          <div className="empty">
            No serial ports<div className="empty-sub">Plug in a USB-serial adapter or dev board</div>
          </div>
        )}
        {st.ports.map((p0) => {
          const p = patch && patch.id === p0.id ? patch : p0;
          const isSel = selected === p.id;
          return (
            <div key={p.id} className={`pr-card ser-card${p.present ? "" : " gone"}`}>
              <Head p={p} isSel={isSel} toggle={() => select(isSel ? "" : p.id)} />
              {isSel && p.present && <Terminal key={p.id} p={p} onPort={setPatch} />}
            </div>
          );
        })}
      </div>
      <div className="pr-row" style={{ marginTop: 12 }}>
        <label>
          Add a tty by path <input placeholder="/dev/pts/3" style={{ width: 150 }} value={manual} onInput={(e) => setManual(e.currentTarget.value)} />
        </label>
        <button
          className="btn btn-sm"
          onClick={async () => {
            if (!manual.trim()) return;
            try {
              const r = await b.api<{ id: string }>("POST", "/api/serial/manual", { path: manual.trim() });
              setManual("");
              select(r.id);
            } catch {}
          }}
        >
          Add
        </button>
        <span className="pr-note">For ptys, socat links and other ports udev doesn't list as USB serial</span>
      </div>
      {iface && (
        <div className="pr-row">
          <IfaceButton focus="uart" label="Onboard UARTs…" />
          <span className="pr-note">Turn on the header UART or extra ones (UART2–5 on a Pi 4); config.txt, needs a reboot</span>
        </div>
      )}
    </div>
  );
}

function Head({ p, isSel, toggle }: { p: SerialPort; isSel: boolean; toggle(): void }) {
  const b = useBob();
  const api = (what: string, body?: unknown) => b.api("POST", `/api/serial/${eid(p.id)}/${what}`, body).catch(() => {});
  const pill = !p.present ? (
    <span className="pr-pill">unplugged</span>
  ) : p.state === "flashing" ? (
    <span className="pr-pill warn">flashing {p.flash?.progress ?? 0}%</span>
  ) : p.state === "autobaud" ? (
    <span className="pr-pill warn">auto-baud…</span>
  ) : p.open ? (
    <span className="pr-pill on">open</span>
  ) : (
    <span className="pr-pill">closed</span>
  );
  const clients = p.clients.ws + p.clients.tcp + p.clients.rfc2217;
  const br = (["tcp", "rfc2217"] as const).filter((k) => p.bridges[k].enabled).map((k) => `${k === "tcp" ? "TCP" : "RFC 2217"} :${p.bridges[k].port}${p.bridges[k].error ? " (failed)" : ""}`);
  const meta = [p.path, p.byId && p.byId !== p.path ? p.byId.replace("/dev/serial/by-id/", "by-id/") : "", p.vidpid, p.usbDriver, p.serial && "S/N " + p.serial].filter(Boolean) as string[];
  return (
    <div className="ser-head">
      <h4>
        {p.name} {pill}
        {p.console && <span className="pr-pill warn">console · read-only</span>}
        {p.lock && <span className="pr-pill warn">locked by {p.lock.owner}</span>}
      </h4>
      <div className="ser-meta">
        {meta.map((m) => (
          <span key={m} className="mono">
            {m}
          </span>
        ))}
      </div>
      <div className="pr-row">
        {p.present &&
          (p.open ? (
            <button className="btn btn-sm" onClick={() => api("close")}>
              Close
            </button>
          ) : (
            <button className="btn btn-sm btn-primary" disabled={p.state === "flashing"} onClick={() => api("open")}>
              Open
            </button>
          ))}
        {p.present && (
          <button className="btn btn-sm" onClick={toggle}>
            {isSel ? "Hide terminal" : "Terminal"}
          </button>
        )}
        <span className="pr-note">
          {p.configLabel} · rx {fmtN(p.rxBytes)} · tx {fmtN(p.txBytes)}
          {clients ? ` · ${clients} client${clients > 1 ? "s" : ""}` : ""}
          {br.length ? " · " + br.join(", ") : ""}
        </span>
      </div>
      {p.error && (
        <div className="pr-note" style={{ color: "var(--danger)" }}>
          {p.error}
        </div>
      )}
    </div>
  );
}

interface ViewPref {
  view?: "text" | "hex";
  ts?: boolean;
  echo?: boolean;
  eol?: string;
}

function Terminal({ p, onPort }: { p: SerialPort; onPort(p: SerialPort): void }) {
  const b = useBob();
  const id = p.id;
  const ro = p.console;
  const api = (what: string, body?: unknown) => b.api<any>("POST", `/api/serial/${eid(id)}/${what}`, body);
  const quiet = () => {};

  const screen = useRef<HTMLDivElement>(null);
  const term = useRef<Term | null>(null);
  const ws = useRef<ReturnType<typeof b.conn.ws> | null>(null);
  const [pref, setPref] = useState<ViewPref>(() => readLocal(b.key("serialView"), {}));
  const savePref = (n: ViewPref) => {
    writeLocal(b.key("serialView"), JSON.stringify(n));
    setPref(n);
  };
  const [raw, setRawState] = useState(false);
  const rawRef = useRef(false);
  const [locked, setLocked] = useState(false);
  const [flashLog, setFlashLog] = useState<string[]>([]);
  const [wsFlash, setWsFlash] = useState<FlashJob | null | undefined>(undefined);
  const portRef = useRef(p);
  portRef.current = p;
  const eol = pref.eol || "lf";
  const eolRef = useRef(eol);
  eolRef.current = eol;

  useEffect(() => {
    const t = (term.current = new Term(screen.current!));
    t.hex = pref.view === "hex";
    t.ts = !!pref.ts;
    t.echo = pref.echo !== false;
    const sock = (ws.current = b.conn.ws(`/ws/serial/${eid(id)}?history=262144&ts=1`, {
      onmessage(m: ArrayBuffer | SerialWsMsg) {
        if (m instanceof ArrayBuffer) return t.feed(new Uint8Array(m), Date.now(), false);
        if (!m || typeof m !== "object") return;
        if (m.type === "hello") {
          t.clear();
          onPort(m.port);
        } else if (m.type === "history") for (const [ts, d] of m.chunks) t.feed(b64(d), ts, false);
        else if (m.type === "status") onPort(m.port);
        else if (m.type === "lines") onPort({ ...portRef.current, lines: m.lines });
        else if (m.type === "flash") {
          const line = m.line;
          if (line) setFlashLog((l) => [...l, line].slice(-300));
          setWsFlash(m.job);
        } else if (m.type === "error") b.toast(m.message, "error");
      },
    }));
    return () => {
      sock.close();
      t.dispose();
      term.current = null;
    };
  }, [id]);
  useEffect(() => setWsFlash(undefined), [p.flash]);

  const setView = (n: ViewPref, rerender: boolean) => {
    savePref(n);
    const t = term.current;
    if (!t) return;
    t.hex = n.view === "hex";
    t.ts = !!n.ts;
    t.echo = n.echo !== false;
    if (rerender) t.rerender();
  };

  const sendBytes = (u8: Uint8Array) => {
    if (!portRef.current.open) return b.toast("Open the port first", "error");
    if (ws.current?.ready) ws.current.send(u8);
    else api("write", { base64: btoa(String.fromCharCode(...u8)) }).catch(quiet);
    term.current?.feed(u8, Date.now(), true);
  };

  const [line, setLine] = useState("");
  const [hexIn, setHexIn] = useState(false);
  const sendLine = () => {
    let bytes: Uint8Array;
    if (hexIn) {
      const h = line.replace(/0x/gi, "").replace(/[\s,:]/g, "");
      if (!/^([0-9a-f]{2})*$/i.test(h)) return b.toast("Hex must be pairs of hex digits", "error");
      bytes = new Uint8Array((h.match(/../g) || []).map((x) => parseInt(x, 16)));
    } else bytes = enc.encode(line + EOLS[eol]);
    if (!bytes.length) return;
    sendBytes(bytes);
    setLine("");
  };

  const setRaw = (on: boolean) => {
    rawRef.current = on;
    setRawState(on);
    if (on) screen.current?.focus();
  };
  const rawKey = (e: KeyboardEvent<HTMLDivElement>) => {
    if (!rawRef.current) return;
    if (e.metaKey || (e.ctrlKey && e.shiftKey)) return; // leave Cmd/Ctrl-Shift shortcuts (copy/paste) to the browser
    let s: string | null = null;
    if (e.key === "Enter") s = { none: "\r", cr: "\r", lf: "\n", crlf: "\r\n" }[eolRef.current] ?? "\n";
    else if (e.key in KEYS) s = KEYS[e.key];
    else if (e.ctrlKey && e.key.length === 1) {
      const c = e.key.toUpperCase().charCodeAt(0);
      if (c >= 64 && c <= 95) s = String.fromCharCode(c - 64);
    } else if (e.key.length === 1 && !e.altKey) s = e.key;
    if (s == null) return;
    e.preventDefault();
    // Raw mode is for interactive shells, which echo themselves: skip local echo.
    if (ws.current?.ready) ws.current.send(enc.encode(s));
  };

  const a = p.autobaud;
  const abTxt = p.state === "autobaud" ? "listening…" : a ? (a.best ? `best guess ${a.best.baud} (confidence ${Math.round(a.best.confidence * 100)}%)${a.applied ? ", applied" : ""}` : "no data heard; the device must be sending") : "";

  return (
    <div className="ser-body">
      <LineSettings p={p} abTxt={abTxt} api={api} />
      <ModemRow p={p} api={api} />
      <div className="pr-row">
        <select value={pref.view === "hex" ? "hex" : "text"} onChange={(e) => setView({ ...pref, view: e.currentTarget.value as "hex" | "text" }, true)}>
          <option value="text">Text</option>
          <option value="hex">Hex</option>
        </select>
        <label>
          <input type="checkbox" checked={!!pref.ts} onChange={(e) => setView({ ...pref, ts: e.currentTarget.checked }, true)} /> Timestamps
        </label>
        <label>
          <input type="checkbox" checked={pref.echo !== false} onChange={(e) => setView({ ...pref, echo: e.currentTarget.checked }, true)} /> Local echo
        </label>
        <label>
          Line end{" "}
          <select value={eol} onChange={(e) => setView({ ...pref, eol: e.currentTarget.value }, false)}>
            <option value="lf">LF</option>
            <option value="cr">CR</option>
            <option value="crlf">CR+LF</option>
            <option value="none">none</option>
          </select>
        </label>
        <button className={`btn btn-sm ser-tog${raw ? " on" : ""}`} disabled={ro} onClick={() => setRaw(!raw)}>
          Raw keys
        </button>
        <button className="btn btn-sm" onClick={() => term.current?.clear()}>
          Clear
        </button>
        <a className="btn btn-sm" href={b.conn.url(`/api/serial/${eid(id)}/read?format=raw&download=1`)} download>
          Download log
        </a>
      </div>
      {raw && <div className="pr-note">Raw keys: every keystroke, including Ctrl-C, goes to the device. Tap outside the terminal or press the button again to stop.</div>}
      <div
        className={`ser-screen${raw ? " raw" : ""}`}
        tabIndex={0}
        ref={screen}
        onKeyDown={rawKey}
        onPaste={(e) => {
          if (!rawRef.current) return;
          e.preventDefault();
          sendBytes(enc.encode(e.clipboardData.getData("text")));
        }}
        onBlur={() => rawRef.current && setRaw(false)}
      />
      <div className="pr-row ser-in">
        <input
          className="ser-line"
          placeholder={ro ? "Console port: read-only" : "Type and press Enter"}
          disabled={ro}
          autoComplete="off"
          autoCapitalize="off"
          spellCheck={false}
          value={line}
          onInput={(e) => setLine(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              sendLine();
            }
          }}
        />
        <label>
          <input type="checkbox" checked={hexIn} onChange={(e) => setHexIn(e.currentTarget.checked)} /> Hex
        </label>
        <button className="btn btn-sm btn-primary" disabled={ro} onClick={sendLine}>
          Send
        </button>
      </div>
      <details className="ser-more">
        <summary>Network bridges</summary>
        <Bridges p={p} api={api} />
      </details>
      {p.flashTools.length > 0 && (
        <details className="ser-more">
          <summary>Flash firmware</summary>
          <FlashForm p={p} api={api} onStart={() => setFlashLog([])} />
          <FlashProgress job={wsFlash !== undefined ? wsFlash : p.flash} log={flashLog} />
        </details>
      )}
      <details className="ser-more">
        <summary>Port options</summary>
        <div className="pr-row">
          <label>
            <input type="checkbox" checked={p.openOnPlug} onChange={(e) => api("settings", { openOnPlug: e.currentTarget.checked }).catch(quiet)} /> Open automatically when plugged in (and when BenchBob starts)
          </label>
        </div>
        {!p.console && (
          <div className="pr-row">
            {p.lock ? (
              locked ? (
                <>
                  <span className="pr-pill on">You hold the write lock</span>
                  <button
                    className="btn btn-sm"
                    onClick={() => {
                      ws.current?.send({ type: "unlock" });
                      setLocked(false);
                    }}
                  >
                    Release
                  </button>
                </>
              ) : (
                <>
                  <span className="pr-pill warn">Writing locked by {p.lock.owner}</span>
                  <button className="btn btn-sm btn-danger" onClick={() => api("unlock", { force: true }).catch(quiet)}>
                    Break lock
                  </button>
                </>
              )
            ) : (
              <>
                <button
                  className="btn btn-sm"
                  onClick={() => {
                    ws.current?.send({ type: "lock", owner: "a dashboard tab" });
                    setLocked(true);
                  }}
                >
                  Lock writing to this tab
                </button>
                <span className="pr-note">Other clients (bridges, agents, other tabs) can still read but not write</span>
              </>
            )}
          </div>
        )}
        <div className="pr-note">
          Port id <span className="pr-endpoint">{p.id}</span> for the API and MCP tools (serial_write, serial_read…).
        </div>
      </details>
    </div>
  );
}

type Api = (what: string, body?: unknown) => Promise<any>;

/** Baud and framing: follow the port's config, except while the user is editing them. */
function LineSettings({ p, abTxt, api }: { p: SerialPort; abTxt: string; api: Api }) {
  const b = useBob();
  const row = useRef<HTMLDivElement>(null);
  const c = p.config;
  const [baud, setBaud] = useState(String(c.baud));
  const [bits, setBits] = useState(String(c.dataBits));
  const [par, setPar] = useState(c.parity);
  const [stop, setStop] = useState(String(c.stopBits));
  const [flow, setFlow] = useState(c.flow);
  useEffect(() => {
    if (row.current?.contains(document.activeElement)) return;
    setBaud(String(c.baud));
    setBits(String(c.dataBits));
    setPar(c.parity);
    setStop(String(c.stopBits));
    setFlow(c.flow);
  }, [JSON.stringify(c)]);
  const apply = () =>
    api("config", { baud: Number(baud), dataBits: Number(bits), parity: par, stopBits: Number(stop), flow })
      .then(() => b.toast("Line settings applied", "success"))
      .catch(() => {});
  const ro = p.console;
  return (
    <div className="pr-row" ref={row}>
      <label>
        Baud{" "}
        <input
          list={`ser-bauds-${p.id}`}
          inputMode="numeric"
          style={{ width: 96 }}
          disabled={ro}
          value={baud}
          onInput={(e) => setBaud(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              apply();
            }
          }}
        />
      </label>
      <datalist id={`ser-bauds-${p.id}`}>
        {BAUDS.map((x) => (
          <option key={x} value={x} />
        ))}
      </datalist>
      <label>
        Bits{" "}
        <select disabled={ro} value={bits} onChange={(e) => setBits(e.currentTarget.value)}>
          {[8, 7, 6, 5].map((n) => (
            <option key={n}>{n}</option>
          ))}
        </select>
      </label>
      <label>
        Parity{" "}
        <select disabled={ro} value={par} onChange={(e) => setPar(e.currentTarget.value as typeof par)}>
          {["none", "even", "odd", "mark", "space"].map((n) => (
            <option key={n}>{n}</option>
          ))}
        </select>
      </label>
      <label>
        Stop{" "}
        <select disabled={ro} value={stop} onChange={(e) => setStop(e.currentTarget.value)}>
          <option>1</option>
          <option>2</option>
        </select>
      </label>
      <label>
        Flow{" "}
        <select disabled={ro} value={flow} onChange={(e) => setFlow(e.currentTarget.value as typeof flow)}>
          <option value="none">none</option>
          <option value="rtscts">RTS/CTS</option>
          <option value="xonxoff">XON/XOFF</option>
        </select>
      </label>
      <button className="btn btn-sm" disabled={ro} onClick={apply}>
        Apply
      </button>
      <button
        className="btn btn-sm"
        disabled={ro}
        onClick={() =>
          api("autobaud", { apply: true })
            .then((r) => b.toast(r.best ? `Best guess ${r.best.baud} baud (${Math.round(r.best.confidence * 100)}%)${r.applied ? ", applied" : ", not applied (low confidence)"}` : "Heard nothing at any rate", r.applied ? "success" : "info"))
            .catch(() => {})
        }
      >
        Auto-baud
      </button>
      <span className="pr-note">{abTxt}</span>
    </div>
  );
}

function ModemRow({ p, api }: { p: SerialPort; api: Api }) {
  if (!p.open)
    return (
      <div className="pr-row">
        <span className="pr-note">Open the port for modem lines and resets.</span>
      </div>
    );
  const l = p.lines;
  const lamp = (k: keyof NonNullable<typeof l>, label: string) => (
    <span key={k} className={`ser-lamp ${l?.[k] ? "on" : ""}`}>
      <i />
      {label}
    </span>
  );
  const tog = (k: "dtr" | "rts", label: string) => (
    <button key={k} className={`btn btn-sm ser-tog ${l?.[k] ? "on" : ""}`} disabled={!l || p.console} onClick={() => api("lines", { [k]: !l?.[k] }).catch(() => {})}>
      {label}
    </button>
  );
  return (
    <div className="pr-row">
      {l ? [tog("dtr", "DTR"), tog("rts", "RTS"), lamp("cts", "CTS"), lamp("dsr", "DSR"), lamp("dcd", "DCD"), lamp("ri", "RI")] : <span className="pr-note">No modem lines on this port</span>}
      {!p.console && (
        <>
          <span className="ser-sep" />
          <button className="btn btn-sm" onClick={() => api("break", { ms: 250 }).catch(() => {})}>
            Break
          </button>
        </>
      )}
      {p.resets.length > 0 && (
        <>
          <span className="ser-sep" />
          {p.resets.map((r) => (
            <button key={r} className="btn btn-sm btn-warn" disabled={!l && r !== "touch1200"} onClick={() => api("reset", { mode: r }).catch(() => {})}>
              {RESET_LABELS[r]}
            </button>
          ))}
        </>
      )}
    </div>
  );
}

function Bridges({ p, api }: { p: SerialPort; api: Api }) {
  const b = useBob();
  const host = b.conn.hostname;
  const wsUrl = b.conn.wsUrl(`/ws/serial/${p.id}`);
  const Block = ({ k, title, about, cmds }: { k: "tcp" | "rfc2217"; title: string; about: string; cmds: (port: number) => string[] }) => {
    const br = p.bridges[k];
    return (
      <>
        <div className="pr-row">
          <label>
            <input type="checkbox" checked={br.enabled} onChange={(e) => api("bridge", { kind: k, enabled: e.currentTarget.checked }).catch(() => {})} /> <b>{title}</b>
          </label>
          <BridgePort k={k} port={br.port} enabled={br.enabled} api={api} />{" "}
          {br.error ? (
            <span className="pr-pill bad">{br.error}</span>
          ) : br.listening ? (
            <>
              <span className="pr-pill on">listening on :{br.port}</span>
              {br.clients.length > 0 && (
                <span className="pr-note">
                  {" "}
                  {br.clients.length} connected: {br.clients.join(", ")}
                </span>
              )}
            </>
          ) : (
            <span className="pr-pill">off</span>
          )}
        </div>
        <div className="pr-note">{about}</div>
        {br.enabled && (
          <div className="ser-cmds">
            {cmds(br.port).map((c) => (
              <span key={c} className="pr-endpoint">
                {c}
              </span>
            ))}
          </div>
        )}
      </>
    );
  };
  return (
    <div>
      {Block({ k: "tcp", title: "Raw TCP", about: "Plain bytes both ways. Line settings stay as set here.", cmds: (port) => [`nc ${host} ${port}`, `socat -,raw,echo=0 TCP:${host}:${port}`, `socat pty,link=/tmp/ttyBenchBob,raw,echo=0 TCP:${host}:${port}`] })}
      {Block({
        k: "rfc2217",
        title: "RFC 2217",
        about: "Telnet COM-PORT-OPTION: remote tools can change baud, framing, DTR/RTS and send breaks. Works with pyserial and esptool.",
        cmds: (port) => [`python -m serial.tools.miniterm rfc2217://${host}:${port} ${p.config.baud}`, `esptool --port 'rfc2217://${host}:${port}?ign_set_control' flash_id`, `pio device monitor --port rfc2217://${host}:${port} -b ${p.config.baud}`],
      })}
      <div className="pr-row">
        <b>WebSocket</b> <span className="pr-note">always on: binary frames are raw bytes; quiet=1 drops the JSON status frames, open=1 holds the port open while connected</span>
      </div>
      <div className="ser-cmds">
        <span className="pr-endpoint">{`websocat --binary '${wsUrl}?quiet=1&open=1'`}</span>
      </div>
    </div>
  );
}

function BridgePort({ k, port, enabled, api }: { k: string; port: number; enabled: boolean; api: Api }) {
  const [v, setV] = useState(String(port));
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (document.activeElement !== ref.current) setV(String(port));
  }, [port]);
  return (
    <>
      <label>
        port <input ref={ref} type="number" value={v} min={1024} max={65535} style={{ width: 84 }} onInput={(e) => setV(e.currentTarget.value)} />
      </label>
      <button className="btn btn-sm" onClick={() => api("bridge", { kind: k, enabled, port: Number(v) }).catch(() => {})}>
        Set port
      </button>
    </>
  );
}

function FlashForm({ p, api, onStart }: { p: SerialPort; api: Api; onStart(): void }) {
  const b = useBob();
  const [tool, setTool] = useState<string>(p.flashTools[0]);
  const [act, setAct] = useState("write");
  const [chip, setChip] = useState("auto");
  const [baud, setBaud] = useState("460800");
  const [preset, setPreset] = useState("uno");
  const fid = useRef(0);
  const [files, setFiles] = useState(() => [{ id: ++fid.current, off: "0x0", file: null as File | null }]);
  const esp = tool === "esptool";
  useEffect(() => {
    if (tool === "avrdude" && act === "erase") setAct("write");
  }, [tool]);
  const start = async () => {
    const fd = new FormData();
    fd.append("tool", tool);
    fd.append("action", act);
    if (esp) {
      fd.append("chip", chip);
      fd.append("baud", baud);
    } else fd.append("preset", preset);
    if (act === "write") {
      let n = 0;
      for (const r of files) {
        if (!r.file) continue;
        fd.append("offset", r.off || "0");
        fd.append("file", r.file, r.file.name);
        n++;
      }
      if (!n) return b.toast("Choose a firmware file", "error");
    }
    onStart();
    try {
      await b.conn.upload(`/api/serial/${eid(p.id)}/flash`, fd);
    } catch (e: any) {
      b.toast(e?.message || "Upload failed", "error");
    }
  };
  return (
    <div>
      <div className="pr-row">
        <label>
          Tool{" "}
          <select value={tool} onChange={(e) => setTool(e.currentTarget.value)}>
            {p.flashTools.map((t) => (
              <option key={t}>{t}</option>
            ))}
          </select>
        </label>
        <label>
          Action{" "}
          <select value={act} onChange={(e) => setAct(e.currentTarget.value)}>
            <option value="write">Write</option>
            <option value="info">Info</option>
            <option value="erase" disabled={tool === "avrdude"}>
              Erase
            </option>
          </select>
        </label>
        {esp ? (
          <>
            <label>
              Chip{" "}
              <select value={chip} onChange={(e) => setChip(e.currentTarget.value)}>
                {["auto", "esp32", "esp32s2", "esp32s3", "esp32c2", "esp32c3", "esp32c6", "esp32h2", "esp32p4", "esp8266"].map((c) => (
                  <option key={c}>{c}</option>
                ))}
              </select>
            </label>
            <label>
              Baud{" "}
              <select value={baud} onChange={(e) => setBaud(e.currentTarget.value)}>
                <option>115200</option>
                <option>460800</option>
                <option>921600</option>
              </select>
            </label>
          </>
        ) : (
          tool === "avrdude" && (
            <label>
              Board{" "}
              <select value={preset} onChange={(e) => setPreset(e.currentTarget.value)}>
                <option value="uno">Uno (115200)</option>
                <option value="nano">Nano (115200)</option>
                <option value="nano-old">Nano, old bootloader (57600)</option>
                <option value="mega">Mega 2560 (wiring)</option>
              </select>
            </label>
          )
        )}
      </div>
      {act === "write" && (
        <div className="ser-files">
          {files.map((r) => (
            <div className="pr-row" key={r.id}>
              <input type="file" accept=".bin,.hex,.img" onChange={(e) => setFiles((l) => l.map((x) => (x.id === r.id ? { ...x, file: e.currentTarget.files?.[0] || null } : x)))} />
              {esp && (
                <label>
                  offset <input value={r.off} style={{ width: 90 }} onInput={(e) => setFiles((l) => l.map((x) => (x.id === r.id ? { ...x, off: e.currentTarget.value } : x)))} />
                </label>
              )}
            </div>
          ))}
        </div>
      )}
      <div className="pr-row">
        {act === "write" && esp && (
          <button className="btn btn-sm" onClick={() => setFiles((l) => [...l, { id: ++fid.current, off: "0x10000", file: null }])}>
            Add file
          </button>
        )}
        <button className="btn btn-sm btn-primary" onClick={start}>
          Start
        </button>
        <button className="btn btn-sm btn-danger" onClick={() => api("flash/cancel").catch(() => {})}>
          Cancel
        </button>
      </div>
      <div className="pr-note">BenchBob releases the port while the tool runs and reopens it after. ESP32: bootloader 0x1000, partition table 0x8000, app 0x10000 (ESP32-S3/C3/C6 bootloader at 0x0), or one merged image at 0x0.</div>
    </div>
  );
}

function FlashProgress({ job: j, log }: { job: FlashJob | null; log: string[] }) {
  const lg = useRef<HTMLDivElement>(null);
  const lines = log.length ? log : j?.logTail || [];
  useEffect(() => {
    if (lg.current) lg.current.scrollTop = lg.current.scrollHeight;
  });
  if (!j) return null;
  const cls = j.state === "done" ? "done" : j.state === "error" || j.state === "cancelled" ? "error" : "active";
  return (
    <div>
      <div className="pr-row">
        <b>
          {j.tool} {j.action}
        </b>
        <span className="pr-note">{j.label}</span>
        <span className={`pr-pill ${cls === "done" ? "on" : cls === "error" ? "bad" : "warn"}`}>
          {j.state}
          {j.state === "running" ? " · " + j.phase : ""}
        </span>
        <span className="pr-note">{j.progress}%</span>
      </div>
      {j.message && (
        <div className="pr-note" style={{ color: cls === "error" ? "var(--danger)" : "var(--text)" }}>
          {j.message}
        </div>
      )}
      <div className="op-bar">
        <div className={`op-fill ${cls}`} style={{ width: `${j.progress}%` }} />
      </div>
      <div className="pr-log" style={{ maxHeight: 160, marginTop: 6 }} ref={lg}>
        {lines.slice(-60).join("\n")}
      </div>
    </div>
  );
}
