// GPIO panel: live 40-pin header (or table) over /ws/gpio, opt-in output control, and the
// config.txt interface switches (stage → diff → confirm → backup + write → reboot confirm).
// The WebSocket only exists while this section is open, so the server only polls then.
//
// Boards other than the Pi (state.mode 'gpiochip'): the board's headers mapped onto
// gpiochip lines, every chip's lines, per-line watching for levels, and boot overlays
// (same stage → diff → apply → reboot flow). Untested boards say so above the header.

import { useEffect, useState, type ReactNode } from "react";
import "./style.css";
import { useBob, type BobApi } from "../../bob/context";
import type { PanelProps } from "../../registry";
import { readLocalStr, writeLocal } from "../../lib/storage";
import type { Chip, ChipHeader, ChipLine, ChipPin, GpioState, HeaderPos, Level, OverlayInfo, PiPin, Staged, WsState } from "./api";
import { openInterfaces } from "./IfaceButton";
import { StagedChange, openReboot } from "./Stage";

const cls = (p?: PiPin) => (!p ? "gp-off" : p.fsel === "op" ? "gp-out" : p.fsel === "ip" ? "gp-in" : /^a\d/.test(p.fsel) ? "gp-alt" : "gp-off");
const funcText = (p: { fsel: string; func: string }) => (p.fsel === "ip" ? "input" : p.fsel === "op" ? "output" : `${p.func} (${p.fsel.toUpperCase()})`);
const pullText = (p: { pull?: string }) => (p.pull && p.pull !== "none" ? ` · pull-${p.pull}` : "");
const Dot = ({ level }: { level: number | null | undefined }) => (level != null ? <span className={`gp-dot${level ? " hi" : ""}`} /> : null);

function Key({ items }: { items: [string, ReactNode][] }) {
  return (
    <div className="gp-key">
      {items.map(([c, l]) => (
        <span key={c}>
          {c === "claimed" ? <i style={{ border: "2px solid var(--warning)" }} /> : c === "high" ? <span className="gp-dot hi" style={{ margin: 0 }} /> : <i className={c} />}
          {l}
        </span>
      ))}
    </div>
  );
}

function BoardNotes({ b }: { b?: GpioState["board"] }) {
  if (!b) return null;
  return (
    <>
      {!b.tested && (
        <div className="pr-note" style={{ color: "var(--warning)" }}>
          Untested board: BenchBob's {b.capabilities?.headers?.length ? "pin map" : "support"} for {b.name} comes from the vendor's documentation and the kernel device tree, but nobody has run BenchBob on one yet. Check pins against the vendor's pinout before wiring anything.
          {b.sources?.length ? (
            <>
              {" "}
              Sources:{" "}
              {b.sources.map((u, i) => (
                <a key={u} href={u} target="_blank" rel="noopener" style={{ marginRight: 4 }}>
                  {i + 1}
                </a>
              ))}
            </>
          ) : null}
        </div>
      )}
      {(b.notes || []).map((n) => (
        <div key={n} className="pr-note">
          {n}
        </div>
      ))}
    </>
  );
}

export default function Gpio({ st }: PanelProps<GpioState>) {
  const b = useBob();
  const [view, setViewS] = useState(() => readLocalStr("gpioView") || "header");
  const setView = (v: string) => {
    writeLocal("gpioView", v);
    setViewS(v);
  };
  const [pins, setPins] = useState<PiPin[]>([]);
  const [gs, setGs] = useState<WsState | null>(null);
  const [chips, setChips] = useState<Chip[]>([]);
  const [levels, setLevels] = useState<Record<string, Level>>({});
  const [outEnabled, setOutEnabled] = useState<boolean | undefined>(undefined);
  const [live, setLive] = useState(0);
  const [err, setErr] = useState("");
  const [, tick] = useState(0);

  useEffect(() => {
    const sock = b.conn.ws("/ws/gpio", {
      onmessage: (m: any) => {
        if (m?.type === "levels") {
          setLevels(m.levels || {});
          return;
        }
        if (m?.type !== "state") return;
        const s = m as WsState;
        setLive(Date.now());
        setErr(s.error || "");
        setChips(s.chips || []);
        if (s.mode === "gpiochip") setGs(s);
        else {
          setPins(s.pins || []);
          setOutEnabled(s.outputEnabled);
        }
        // Lines no longer watched drop their last level.
        const w = new Set(s.watched || []);
        setLevels((l) => Object.fromEntries(Object.entries(l).filter(([k]) => w.has(k))));
      },
    });
    const t = setInterval(() => tick((x) => x + 1), 2000);
    return () => {
      sock.close();
      clearInterval(t);
    };
  }, [b.conn]);

  const isLive = live && Date.now() - live < 4000;
  const livePill = <span className={`pr-pill ${isLive ? "on" : "warn"}`}>{isLive ? "live" : "connecting"}</span>;
  const lv = (chip: string, line: number) => levels[`${chip}:${line}`];
  const watch = (chip: string, line: number, on: boolean) => b.api("POST", "/api/gpio/watch", { chip, line, watch: on }).catch(() => {});

  if (st.mode === "gpiochip") {
    if (!gs) return <div className="pr-note">Waiting for the first reading…</div>;
    const brd = gs.board || st.board;
    const heads = gs.headers || [];
    return (
      <>
        <div className="pr-row">
          {livePill}
          {st.bootConfig && (
            <button className="btn btn-sm" onClick={() => openOverlays(b)}>
              Boot overlays…
            </button>
          )}
          <span className="pr-note">
            {brd?.name || ""}
            {brd?.soc ? ` · ${brd.soc}` : ""}
          </span>
        </div>
        <BoardNotes b={brd} />
        <div className="pr-note">
          {heads.length ? "Tap a pin for its line; levels are shown for lines you watch." : "No header map for this board: every GPIO line the kernel has is listed below."} Watching requests a free line as-is (nothing about the pin changes) and releases it when you stop. Driving pins isn't available on this board yet.
        </div>
        {st.rebootRequired && (
          <div className="pr-note" style={{ color: "var(--warning)" }}>
            Boot overlays changed: reboot required.{" "}
            <button className="btn btn-sm btn-warn" onClick={() => openReboot(b)}>
              Reboot now…
            </button>
          </div>
        )}
        {(gs.error || err) && (
          <div className="pr-note" style={{ color: "var(--danger)" }}>
            {gs.error || err}
          </div>
        )}
        {heads.map((h) => (
          <div key={h.name}>
            <h4 style={{ margin: "10px 0 0" }}>{h.name}</h4>
            <ChipHeaderView h={h} lv={lv} onPin={(p) => openLine(b, p.chipName!, p.line!, chips, heads, lv(p.chipName!, p.line!))} />
          </div>
        ))}
        {chips.length ? (
          <details open={!heads.length} style={{ marginTop: 10 }}>
            <summary className="pr-note" style={{ cursor: "pointer" }}>
              All GPIO lines ({chips.length} chip{chips.length === 1 ? "" : "s"}
              {chips.some((c) => c.usb) ? ", incl. USB adapters" : ""})
            </summary>
            <Chips list={chips} lv={lv} watch={watch} />
          </details>
        ) : (
          <div className="pr-note">No GPIO chips (/dev/gpiochip*) on this machine. USB GPIO adapters (FT232H, CH341, MCP2221) appear here when plugged in.</div>
        )}
      </>
    );
  }

  const outputEnabled = outEnabled ?? st.outputEnabled;
  const header = st.header || [];
  const byGpio = new Map(pins.map((p) => [p.gpio, p]));
  const bank = pins.filter((p) => p.gpio <= 27),
    soc = pins.filter((p) => p.gpio > 27);
  const canDrive = (p?: PiPin) => !!outputEnabled && !!p && !p.refusal;
  const pinDlg = (g: number) => {
    const p = byGpio.get(g);
    if (p) b.openModal(<PinDialog p={p} />);
  };
  const outputToggle = async () => {
    try {
      if (outputEnabled) {
        await b.api("POST", "/api/gpio/output-control", { enabled: false });
        return;
      }
      if (!(await b.confirm("Enable GPIO output control", "This lets the dashboard, the REST API and MCP agents drive header pins (high/low/input/pulls). Pins used by the kernel or a peripheral stay refused.", "Setting is saved; turn it off here any time."))) return;
      await b.api("POST", "/api/gpio/output-control", { enabled: true, confirm: true });
    } catch {}
  };

  return (
    <>
      <div className="pr-row">
        <button className={`btn btn-sm${view === "header" ? " btn-primary" : ""}`} onClick={() => setView("header")}>
          Header
        </button>
        <button className={`btn btn-sm${view === "table" ? " btn-primary" : ""}`} onClick={() => setView("table")}>
          Table
        </button>
        {livePill}
        <button className={`btn btn-sm ${outputEnabled ? "btn-warn" : ""}`} onClick={outputToggle}>
          {outputEnabled ? "Output control ON: turn off" : "Enable output control…"}
        </button>
        <button className="btn btn-sm" onClick={() => openInterfaces(b).catch(() => {})}>
          Interfaces (I²C, SPI, UART, 1-Wire)…
        </button>
      </div>
      {outputEnabled ? (
        <div className="pr-note" style={{ color: "var(--warning)" }}>
          Output control is on: tap a free GPIO to drive it. Pins used by the kernel or a peripheral are refused.
        </div>
      ) : (
        <div className="pr-note">Read-only view. Output control is off.</div>
      )}
      {st.rebootRequired && (
        <div className="pr-note" style={{ color: "var(--warning)" }}>
          config.txt changed: reboot required.{" "}
          <button className="btn btn-sm btn-warn" onClick={() => openReboot(b)}>
            Reboot now…
          </button>
        </div>
      )}
      {err && (
        <div className="pr-note" style={{ color: "var(--danger)" }}>
          {err}
        </div>
      )}
      <BoardNotes b={st.board} />
      {!pins.length ? (
        <div className="pr-note">Waiting for the first reading…</div>
      ) : view === "header" ? (
        <PiHeader header={header} byGpio={byGpio} canDrive={canDrive} onPin={pinDlg} />
      ) : (
        <PinTable list={bank} outputEnabled={!!outputEnabled} canDrive={canDrive} onPin={pinDlg} />
      )}
      {soc.length > 0 && (
        <details style={{ marginTop: 10 }}>
          <summary className="pr-note" style={{ cursor: "pointer" }}>
            SoC-internal GPIO 28–{soc[soc.length - 1].gpio} (Ethernet, SD, Wi-Fi/BT, LEDs; read-only)
          </summary>
          <PinTable list={soc} outputEnabled={!!outputEnabled} canDrive={canDrive} onPin={pinDlg} />
        </details>
      )}
      {chips.length > 0 && (
        <details style={{ marginTop: 10 }}>
          <summary className="pr-note" style={{ cursor: "pointer" }}>
            Other GPIO chips: {chips.map((c) => c.label || c.name).join(", ")} (lines can be watched, not driven)
          </summary>
          <Chips list={chips} lv={lv} watch={watch} />
        </details>
      )}
    </>
  );
}

// ---------- Pi header / table ----------

function Label({ h, p, side }: { h: HeaderPos; p?: PiPin; side: "l" | "r" }) {
  if (h.kind !== "gpio") return <div className={`gp-${side}`}>{h.kind === "gnd" ? "GND" : h.kind === "3v3" ? "3.3 V" : "5 V"}</div>;
  if (!p) return <div className={`gp-${side}`}>GPIO{h.gpio}</div>;
  const name = <b>GPIO{h.gpio}</b>;
  const sub = [p.consumer ? `used by ${p.consumer}` : "", pullText(p).slice(3)].filter(Boolean).join(" · ");
  return (
    <div className={`gp-${side}`}>
      {side === "l" ? (
        <>
          {funcText(p)} <Dot level={p.level} />
          {name}
        </>
      ) : (
        <>
          {name}
          <Dot level={p.level} /> {funcText(p)}
        </>
      )}
      {sub && <small>{sub}</small>}
    </div>
  );
}

function PinDot({ h, p, drive, onPin }: { h: HeaderPos; p?: PiPin; drive: boolean; onPin(g: number): void }) {
  const k = h.kind === "gpio" ? cls(p) : "gp-" + h.kind;
  const claimed = p?.consumer || p?.refusal?.startsWith("muxed");
  return (
    <button className={`gp-pin ${k}${claimed ? " gp-claimed" : ""}${drive ? " click" : ""}`} tabIndex={drive ? 0 : -1} aria-label={`Pin ${h.pin}`} onClick={drive ? () => onPin(h.gpio) : undefined}>
      {h.pin}
    </button>
  );
}

function PiHeader({ header, byGpio, canDrive, onPin }: { header: HeaderPos[]; byGpio: Map<number, PiPin>; canDrive(p?: PiPin): boolean; onPin(g: number): void }) {
  const cells: ReactNode[] = [];
  for (let i = 0; i + 1 < header.length && i < 40; i += 2) {
    const a = header[i],
      c = header[i + 1];
    const pa = byGpio.get(a.gpio),
      pc = byGpio.get(c.gpio);
    cells.push(
      <Label key={"la" + i} h={a} p={pa} side="l" />,
      <PinDot key={"da" + i} h={a} p={pa} drive={a.kind === "gpio" && canDrive(pa)} onPin={onPin} />,
      <PinDot key={"dc" + i} h={c} p={pc} drive={c.kind === "gpio" && canDrive(pc)} onPin={onPin} />,
      <Label key={"lc" + i} h={c} p={pc} side="r" />,
    );
  }
  return (
    <>
      <div className="gp-hdr">{cells}</div>
      <Key
        items={[
          ["gp-in", "input"],
          ["gp-out", "output"],
          ["gp-alt", "peripheral (ALT)"],
          ["claimed", "claimed"],
          ["high", "high"],
          ["gp-3v3", "3.3 V"],
          ["gp-5v", "5 V"],
          ["gp-gnd", "GND"],
        ]}
      />
    </>
  );
}

function PinTable({ list, outputEnabled, canDrive, onPin }: { list: PiPin[]; outputEnabled: boolean; canDrive(p?: PiPin): boolean; onPin(g: number): void }) {
  return (
    <div style={{ overflowX: "auto" }}>
      <table className="pr-tbl">
        <tbody>
          <tr>
            <th>GPIO</th>
            <th>Pin</th>
            <th>Function</th>
            <th>Level</th>
            <th>Pull</th>
            <th>Used by</th>
            {outputEnabled && <th></th>}
          </tr>
          {list.map((p) => (
            <tr key={p.gpio}>
              <td>{p.gpio}</td>
              <td>{p.header ?? "—"}</td>
              <td>{funcText(p)}</td>
              <td>
                {p.level == null ? (
                  "—"
                ) : (
                  <>
                    <Dot level={p.level} />
                    {p.level ? "high" : "low"}
                  </>
                )}
              </td>
              <td>{p.pull || "—"}</td>
              <td>
                {p.consumer || ""}
                {p.refusal && !p.consumer && <span className="pr-note">{p.refusal}</span>}
              </td>
              {outputEnabled && (
                <td>
                  {canDrive(p) && (
                    <button className="btn btn-sm" onClick={() => onPin(p.gpio)}>
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
  );
}

function PinDialog({ p }: { p: PiPin }) {
  const b = useBob();
  const [res, setRes] = useState("");
  const act = async (body: object) => {
    try {
      const r = await b.conn.json<PiPin>("POST", `/api/gpio/${p.gpio}`, body);
      setRes(`Now ${funcText(r)}, ${r.level ? "high" : "low"}${pullText(r)}`);
    } catch (e: any) {
      setRes(e.message);
    }
  };
  return (
    <>
      <h3>
        GPIO{p.gpio} · header pin {p.header}
      </h3>
      <p>
        Now: {funcText(p)}, {p.level ? "high" : "low"}
        {pullText(p)}. This drives the physical pin: make sure nothing connected minds.
      </p>
      <div className="pr-row">
        <span className="pr-note" style={{ minWidth: 60 }}>
          Mode
        </span>
        <button className="btn btn-sm" onClick={() => act({ mode: "high" })}>
          Output high
        </button>
        <button className="btn btn-sm" onClick={() => act({ mode: "low" })}>
          Output low
        </button>
        <button className="btn btn-sm" onClick={() => act({ mode: "input" })}>
          Input
        </button>
      </div>
      <div className="pr-row">
        <span className="pr-note" style={{ minWidth: 60 }}>
          Pull
        </span>
        <button className="btn btn-sm" onClick={() => act({ pull: "up" })}>
          Up
        </button>
        <button className="btn btn-sm" onClick={() => act({ pull: "down" })}>
          Down
        </button>
        <button className="btn btn-sm" onClick={() => act({ pull: "none" })}>
          None
        </button>
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

// ---------- gpiochip boards ----------

const gcls = (p: ChipPin) => (!p.chipName || !p.direction ? "gp-off" : p.direction === "output" ? "gp-out" : "gp-in");
const lineText = (p: ChipPin) => [p.direction, p.consumer ? `used by ${p.consumer}` : "", p.flags?.filter((f) => !["used", "input", "output"].includes(f)).join(" ")].filter(Boolean).join(" · ");
const LvDot = ({ l }: { l?: Level | null }) => (l && l.level != null ? <span className={`gp-dot${l.level ? " hi" : ""}`} /> : null);

function ChipHeaderView({ h, lv, onPin }: { h: ChipHeader; lv(c: string, l: number): Level | undefined; onPin(p: ChipPin): void }) {
  // Two-row headers (26/40/46 pins) as on the board; a single-row one (the Zero 3's 13-pin) as a list.
  if (h.pins.length % 2)
    return (
      <div style={{ overflowX: "auto" }}>
        <table className="pr-tbl">
          <tbody>
            <tr>
              <th>Pin</th>
              <th>Name</th>
              <th>Line</th>
              <th>State</th>
              <th>Functions</th>
            </tr>
            {h.pins.map((p) => (
              <tr key={p.pin}>
                <td>{p.pin}</td>
                <td>{p.soc || p.label || p.kind.toUpperCase()}</td>
                <td>{p.chipName ? `${p.chipName}:${p.line}` : p.kind === "gpio" ? "—" : ""}</td>
                <td>
                  {p.kind === "gpio" && (
                    <>
                      <LvDot l={p.chipName ? lv(p.chipName, p.line!) : null} />
                      {lineText(p)}
                    </>
                  )}
                </td>
                <td>
                  {(p.funcs || []).join(", ")}
                  {p.note && <span className="pr-note">{p.note}</span>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    );
  const label = (p: ChipPin, side: "l" | "r") => {
    if (p.kind !== "gpio") return <div className={`gp-${side}`}>{p.kind === "gnd" ? "GND" : p.kind === "3v3" ? "3.3 V" : p.kind === "5v" ? "5 V" : p.kind === "nc" ? "NC" : p.label || ""}</div>;
    const l = p.chipName ? lv(p.chipName, p.line!) : null;
    const sub = [p.chipName ? `${p.chipName} line ${p.line}${p.lineName && p.lineName !== p.soc ? ` "${p.lineName}"` : ""}` : "chip not found", p.chipName ? lineText(p) : "", p.funcs?.length ? p.funcs.join(", ") : "", p.note || ""].filter(Boolean).join(" · ");
    return (
      <div className={`gp-${side}`}>
        {side === "l" ? (
          <>
            <LvDot l={l} />
            <b>{p.soc}</b>
          </>
        ) : (
          <>
            <b>{p.soc}</b>
            <LvDot l={l} />
          </>
        )}
        <small>{sub}</small>
      </div>
    );
  };
  const pin = (p: ChipPin) => {
    const k = p.kind === "gpio" ? gcls(p) : p.kind === "other" || p.kind === "nc" ? "gp-off" : "gp-" + p.kind;
    const click = p.kind === "gpio" && !!p.chipName;
    return (
      <button className={`gp-pin ${k}${p.used ? " gp-claimed" : ""}${click ? " click" : ""}`} tabIndex={click ? 0 : -1} aria-label={`Pin ${p.pin}`} onClick={click ? () => onPin(p) : undefined}>
        {p.pin}
      </button>
    );
  };
  const cells: ReactNode[] = [];
  for (let i = 0; i < h.pins.length; i += 2)
    cells.push(
      <span key={i} style={{ display: "contents" }}>
        {label(h.pins[i], "l")}
        {pin(h.pins[i])}
        {pin(h.pins[i + 1])}
        {label(h.pins[i + 1], "r")}
      </span>,
    );
  return (
    <>
      <div className="gp-hdr">{cells}</div>
      <Key
        items={[
          ["gp-in", "input"],
          ["gp-out", "output"],
          ["claimed", "in use"],
          ["high", "high (watched lines)"],
          ["gp-3v3", "3.3 V"],
          ["gp-5v", "5 V"],
          ["gp-gnd", "GND"],
        ]}
      />
    </>
  );
}

function Chips({ list, lv, watch }: { list: Chip[]; lv(c: string, l: number): Level | undefined; watch(chip: string, line: number, on: boolean): void }) {
  return (
    <>
      {list.map((c) => (
        <div className="pr-card" key={c.name}>
          <h4>
            {c.name}{" "}
            <small>
              {c.label}
              {c.device ? ` · ${c.device}` : ""}
              {c.usb ? " · USB" : ""} · {c.count} lines
            </small>
          </h4>
          {c.error ? (
            <div className="pr-note" style={{ color: "var(--danger)" }}>
              {c.error}
            </div>
          ) : (
            <>
              <div style={{ overflowX: "auto" }}>
                <table className="pr-tbl">
                  <tbody>
                    <tr>
                      <th>Line</th>
                      <th>Name</th>
                      <th>Used by</th>
                      <th>State</th>
                      <th>Level</th>
                      <th></th>
                    </tr>
                    {c.lines.map((l: ChipLine) => {
                      const x = lv(c.name, l.offset);
                      return (
                        <tr key={l.offset}>
                          <td>{l.offset}</td>
                          <td>{l.name || ""}</td>
                          <td>{l.watched ? "benchbob (watching)" : l.consumer || ""}</td>
                          <td>{l.flags.filter((f) => f !== "used").join(" ")}</td>
                          <td>
                            {x ? (
                              x.error ? (
                                <span className="pr-note">{x.error}</span>
                              ) : (
                                <>
                                  <LvDot l={x} />
                                  {x.level ? "high" : "low"}
                                </>
                              )
                            ) : (
                              "—"
                            )}
                          </td>
                          <td>
                            {l.watched ? (
                              <button className="btn btn-sm" onClick={() => watch(c.name, l.offset, false)}>
                                Stop
                              </button>
                            ) : l.used ? null : (
                              <button className="btn btn-sm" onClick={() => watch(c.name, l.offset, true)}>
                                Watch
                              </button>
                            )}
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
              <div className="pr-note">Watch requests the line as-is (direction, bias and drive unchanged) to read its level; it's released when you stop.</div>
            </>
          )}
        </div>
      ))}
    </>
  );
}

function openLine(b: BobApi, chip: string, line: number, chips: Chip[], heads: ChipHeader[], x: Level | undefined) {
  const c = chips.find((y) => y.name === chip);
  const l = c?.lines[line];
  if (!c || !l) return;
  const p = heads.flatMap((h) => h.pins.map((q) => ({ ...q, header: h.name }))).find((q) => q.chipName === chip && q.line === line);
  b.openModal(<LineDialog chip={c} l={l} line={line} p={p} x={x} />);
}

function LineDialog({ chip, l, line, p, x }: { chip: Chip; l: ChipLine; line: number; p?: ChipPin & { header: string }; x?: Level }) {
  const b = useBob();
  const [res, setRes] = useState("");
  return (
    <>
      <h3>{p ? `${p.header} pin ${p.pin} · ${p.soc}` : `${chip.name} line ${line}`}</h3>
      <p>
        {chip.name} ({chip.label}) line {line}
        {l.name ? ` "${l.name}"` : ""}: {l.flags.join(", ") || "input"}
        {l.consumer ? `, used by ${l.consumer}` : ""}.{x ? ` Level: ${x.error ? x.error : x.level ? "high" : "low"}.` : ""}
      </p>
      {p?.funcs?.length ? <p className="pr-note">Alternate functions: {p.funcs.join(", ")}</p> : null}
      {p?.note && <p className="pr-note">{p.note}</p>}
      {!l.watched && !l.used && <p className="pr-note">Watch level requests the line as-is to read its level.</p>}
      <div className="pr-note">{res}</div>
      <div className="modal-btns">
        {(l.watched || !l.used) && (
          <button
            className="btn"
            onClick={async () => {
              try {
                await b.conn.json("POST", "/api/gpio/watch", { chip: chip.name, line, watch: !l.watched });
                b.closeModal();
              } catch (e: any) {
                setRes(e.message);
              }
            }}
          >
            {l.watched ? "Stop watching" : "Watch level"}
          </button>
        )}
        <button className="btn btn-primary" onClick={b.closeModal}>
          Done
        </button>
      </div>
    </>
  );
}

// ---------- boot overlays (gpiochip boards) ----------

async function openOverlays(b: BobApi) {
  let info: OverlayInfo;
  try {
    info = await b.api<OverlayInfo>("GET", "/api/gpio/overlays");
  } catch {
    return;
  }
  b.openModal(<Overlays info={info} />, { cls: "wide" });
}

function Overlays({ info }: { info: OverlayInfo }) {
  const b = useBob();
  const on = new Set(info.enabled.map((e) => e.name));
  const [checked, setChecked] = useState<Set<string>>(() => new Set(on));
  const [q, setQ] = useState("");
  const [out, setOut] = useState<ReactNode>(null);
  const [applied, setApplied] = useState(false);
  const known = info.installed.filter((o) => o.what),
    other = info.installed.filter((o) => !o.what);
  const missing = info.enabled.filter((e) => !e.installed);
  const row = (o: { name: string; what?: string }, hide = false) => (
    <label key={o.name} className="pr-note" style={{ display: hide ? "none" : "flex", gap: 6, alignItems: "baseline" }}>
      <input
        type="checkbox"
        checked={checked.has(o.name)}
        onChange={(e) => {
          const v = e.currentTarget.checked;
          setChecked((s) => {
            const n = new Set(s);
            v ? n.add(o.name) : n.delete(o.name);
            return n;
          });
        }}
      />{" "}
      <span>
        <b>{o.name}</b>
        {o.what ? ` · ${o.what}` : ""}
      </span>
    </label>
  );
  const review = async () => {
    const all = [...new Set([...info.installed.map((o) => o.name), ...missing.map((m) => m.name)])];
    const enable = all.filter((n) => checked.has(n) && !on.has(n));
    const disable = all.filter((n) => !checked.has(n) && on.has(n));
    let r: Staged;
    try {
      r = await b.api<Staged>("POST", "/api/gpio/overlays/stage", { enable, disable });
    } catch (e: any) {
      setOut(
        <p className="pr-note" style={{ color: "var(--danger)" }}>
          {e.message}
        </p>,
      );
      return;
    }
    if (!r.diff) {
      setOut(<p className="pr-note">{r.message}</p>);
      return;
    }
    setOut(<StagedChange key={r.token} r={r} applyPath="/api/gpio/overlays/apply" onApplied={() => setApplied(true)} />);
  };
  return (
    <>
      <h3>Boot overlays</h3>
      <p className="pr-note">
        Edits {info.path}
        {info.testCopy ? " (test copy)" : ""}
        {info.label ? ` (label "${info.label}")` : ""}: the device-tree overlays applied at boot{info.prefix ? ` (prefix ${info.prefix})` : ""}. You review the diff before anything is written. Changes need a reboot.
      </p>
      {!info.installed.length && (
        <p className="pr-note" style={{ color: "var(--warning)" }}>
          No overlays found in {info.overlayDirs.join(", ") || "any overlay directory"}.
        </p>
      )}
      {missing.length > 0 && (
        <>
          <p className="pr-note" style={{ color: "var(--warning)" }}>
            Enabled but not installed on this image: {missing.map((e) => e.name).join(", ")}. Untick to remove.
          </p>
          {missing.map((m) => row(m))}
        </>
      )}
      {known.length > 0 && (
        <div className="pr-card">
          <h4>Header overlays for this board</h4>
          {known.map((o) => row(o))}
        </div>
      )}
      {other.length > 0 && (
        <details open={!known.length}>
          <summary className="pr-note" style={{ cursor: "pointer" }}>
            Other installed overlays ({other.length})
          </summary>
          <input type="search" placeholder="Filter" style={{ margin: "6px 0", width: "100%" }} value={q} onInput={(e) => setQ(e.currentTarget.value)} />
          <div>{other.map((o) => row(o, !!q && !o.name.toLowerCase().includes(q.toLowerCase())))}</div>
        </details>
      )}
      {info.user.length > 0 && <p className="pr-note">user_overlays (not changed here): {info.user.join(", ")}</p>}
      {info.conflicts.length > 0 && (
        <p className="pr-note" style={{ color: "var(--danger)" }}>
          Pin conflicts now: {info.conflicts.map((c) => `pin ${c.pin} (${c.users.join(", ")})`).join("; ")}
        </p>
      )}
      <div>{out}</div>
      <div className="modal-btns">
        <button className="btn" onClick={b.closeModal}>
          Close
        </button>
        {!applied && (
          <button className="btn btn-primary" onClick={review}>
            Review changes
          </button>
        )}
      </div>
    </>
  );
}
