// The config.txt interfaces dialog, whole or focused on one group: the I²C, Serial and
// 1-Wire panels open it on their own group. A focused dialog only sends its own group to
// /stage, so it can't change the others. The form is uncontrolled (read on Review), so the
// status push never disturbs what is being typed.

import { useRef, useState, type ReactNode } from "react";
import { useBob, useSnapshot, type BobApi } from "../../bob/context";
import type { IfaceGroup } from "./IfaceButton";
import type { HeaderPos, IfaceInfo, Staged } from "./api";
import { StagedChange } from "./Stage";
import { ActionLine, useAction } from "../../ui/status";

const SPEEDS = [10000, 50000, 100000, 400000, 1000000];
const GROUPS: Record<IfaceGroup, string> = { i2c: "I²C buses", spi: "SPI", uart: "UARTs", w1: "1-Wire" };
const kHz = (hz?: number) => (hz ? `${hz / 1000} kHz` : "");

// Throws when the settings can't be read; the caller shows why.
export async function openInterfaces(b: BobApi, focus?: IfaceGroup) {
  const info = await b.api<IfaceInfo>("GET", "/api/gpio/interfaces");
  b.openModal(<Interfaces info={info} focus={focus} />, { cls: "wide" });
}

function SpeedSel({ id, cur }: { id: string; cur?: number }) {
  return (
    <label>
      Speed{" "}
      <select id={id} defaultValue={String(cur || 100000)}>
        {SPEEDS.map((v) => (
          <option key={v} value={v}>
            {v / 1000} kHz{v === 100000 ? " (default)" : ""}
          </option>
        ))}
        {cur && !SPEEDS.includes(cur) ? <option value={cur}>{cur / 1000} kHz</option> : null}
      </select>
    </label>
  );
}

function Row({ id, name, on, live, children }: { id: string; name: ReactNode; on: boolean; live: ReactNode; children?: ReactNode }) {
  return (
    <div className="pr-card">
      <h4>
        <label style={{ display: "flex", gap: 6, alignItems: "center" }}>
          <input type="checkbox" id={`gi-${id}`} defaultChecked={on} /> {name}
        </label>
      </h4>
      <div className="pr-note">Now: {live}</div>
      {children}
    </div>
  );
}

function Interfaces({ info, focus }: { info: IfaceInfo; focus?: IfaceGroup }) {
  const b = useBob();
  const header: HeaderPos[] = useSnapshot().status?.peripherals?.state?.gpio?.header || [];
  const form = useRef<HTMLDivElement>(null);
  const [out, setOut] = useState<ReactNode>(null);
  const [applied, setApplied] = useState(false);
  const all = useAction();
  const c = info.configured,
    L = info.live,
    cat = info.catalog;
  const show = (g: IfaceGroup) => !focus || focus === g;
  const hdrPin = (g: number) => header.find((h) => h.gpio === g)?.pin;
  const gp = (...gs: number[]) => `GPIO${gs.join("/")}${gs.every(hdrPin) ? ` (pin${gs.length > 1 ? "s" : ""} ${gs.map(hdrPin).join("/")})` : ""}`;

  const review = async () => {
    const q = (id: string) => form.current!.querySelector<HTMLInputElement & HTMLSelectElement>(`#gi-${id}`)!;
    const on = (id: string) => q(id).checked;
    const num = (id: string) => (q(id).value === "" ? null : Number(q(id).value));
    const req: Record<string, any> = {};
    if (show("i2c")) {
      req.i2c = { enabled: on("i2c"), baudrate: num("baud") };
      if (cat) {
        req.extraI2c = Object.fromEntries(Object.keys(cat.extraI2c).map((n) => [n, { enabled: on(n), pins: q(`${n}-pins`).value, baudrate: num(`${n}-baud`) }]));
        req.i2cGpio = { enabled: on("i2cgpio"), ...(on("i2cgpio") ? { sda: num("ig-sda"), scl: num("ig-scl"), bus: num("ig-bus"), delayUs: num("ig-delay") } : {}) };
      }
    }
    if (show("spi")) req.spi = { enabled: on("spi") };
    if (show("uart")) {
      req.uart = { enabled: on("uart") };
      if (cat) req.extraUart = Object.fromEntries(Object.keys(cat.extraUart).map((n) => [n, { enabled: on(n), ctsrts: on(`${n}-cts`) }]));
    }
    if (show("w1")) req.onewire = { enabled: on("w1"), ...(on("w1") ? { pin: num("w1pin") } : {}) };
    let r: Staged;
    try {
      r = await b.api<Staged>("POST", "/api/gpio/interfaces/stage", req);
    } catch (e: any) {
      setOut(
        <p className="pr-note" style={{ color: "var(--danger)" }}>
          {e.message}
        </p>,
      );
      return;
    }
    if (!r.diff) {
      setOut(
        <>
          <p className="pr-note">{r.message}</p>
          {r.warnings.map((w) => (
            <div key={w} className="pr-note" style={{ color: "var(--warning)" }}>
              {w}
            </div>
          ))}
        </>,
      );
      return;
    }
    setOut(<StagedChange key={r.token} r={r} applyPath="/api/gpio/interfaces/apply" onApplied={() => setApplied(true)} />);
  };

  return (
    <>
      <h3>{focus ? `Interfaces: ${GROUPS[focus]}` : "Interfaces"}</h3>
      <p className="pr-note">
        Edits {info.configPath}
        {info.testCopy ? " (test copy)" : ""}: only lines in the [all] context, new lines under an [all] block. You review the diff before anything is written. Changes need a reboot.
        {focus && (
          <>
            {" "}
            <a
              href="#"
              onClick={(e) => {
                e.preventDefault();
                all.run(() => openInterfaces(b), { busy: "Loading…" });
              }}
            >
              All interfaces…
            </a>{" "}
            <ActionLine a={all} />
          </>
        )}
      </p>
      {!cat && (show("i2c") || show("uart")) && <p className="pr-note">Extra I²C buses and UARTs are offered on Pi 4 / 400 / CM4 (BCM2711) only; this is {L.board?.model || "another board"}.</p>}
      <div ref={form}>
        {show("i2c") && (
          <>
            <Row id="i2c" name={`I²C1 on ${gp(2, 3)}`} on={c.i2c.enabled} live={L.i2c.enabled ? `on, ${L.i2c.dev || "no /dev/i2c-1"}${L.i2c.hz ? ", " + kHz(L.i2c.hz) : ""}` : "off"}>
              <div className="pr-row">
                <SpeedSel id="gi-baud" cur={c.i2c.baudrate} />
              </div>
            </Row>
            {cat &&
              Object.entries(cat.extraI2c).map(([n, x]) => {
                const cf = c.extraI2c[n],
                  lv = L.extraI2c?.[n];
                return (
                  <Row
                    key={n}
                    id={n}
                    name={
                      <>
                        {n.replace("i2c", "I²C")} <small>/dev/i2c-{n.slice(3)}</small>
                      </>
                    }
                    on={cf.enabled}
                    live={lv?.enabled ? `on, ${lv.dev || "no device node"}${lv.hz ? ", " + kHz(lv.hz) : ""}` : "off"}
                  >
                    <div className="pr-row">
                      <label>
                        Pins{" "}
                        <select id={`gi-${n}-pins`} defaultValue={cf.pins}>
                          {Object.entries(x.choices).map(([k, p]) => (
                            <option key={k} value={k}>
                              {gp(...p)}
                              {k === x.def ? " (default)" : ""}
                            </option>
                          ))}
                        </select>
                      </label>
                      <SpeedSel id={`gi-${n}-baud`} cur={cf.baudrate} />
                    </div>
                  </Row>
                );
              })}
            {cat && (
              <Row id="i2cgpio" name="Software I²C (i2c-gpio, any two pins)" on={c.i2cGpio.enabled} live={L.i2cGpio?.nodes?.length ? `on (${L.i2cGpio.nodes.join(", ")})` : "off"}>
                <div className="pr-row">
                  <label>
                    SDA GPIO <input id="gi-ig-sda" type="number" min={0} max={27} defaultValue={c.i2cGpio.sda} style={{ width: 56 }} />
                  </label>
                  <label>
                    SCL GPIO <input id="gi-ig-scl" type="number" min={0} max={27} defaultValue={c.i2cGpio.scl} style={{ width: 56 }} />
                  </label>
                  <label>
                    Bus <input id="gi-ig-bus" type="number" min={1} max={255} placeholder="auto" defaultValue={c.i2cGpio.bus ?? ""} style={{ width: 60 }} />
                  </label>
                  <label>
                    Delay µs <input id="gi-ig-delay" type="number" min={1} max={100} placeholder="2" defaultValue={c.i2cGpio.delayUs ?? ""} style={{ width: 56 }} />
                  </label>
                </div>
                <div className="pr-note">Bit-banged by the CPU: slower (2 µs ≈ 100 kHz) but works on any two free pins.</div>
              </Row>
            )}
          </>
        )}
        {show("spi") && <Row id="spi" name={`SPI0 on ${gp(7, 8, 9, 10, 11)}`} on={c.spi.enabled} live={L.spi.enabled ? `on, ${L.spi.devices.join(", ") || "no spidev nodes"}` : "off"} />}
        {show("uart") && (
          <>
            <Row
              id="uart"
              name={`UART serial0 on ${gp(14, 15)}`}
              on={c.uart.enabled}
              live={
                <>
                  {L.uart.serial0 ? "serial0 → " + L.uart.serial0 : "no serial0"}
                  {L.uart.consoleOnSerial ? (
                    <>
                      ; <b>serial console on {L.uart.consoleOnSerial}</b> (cmdline.txt; not changed here)
                    </>
                  ) : (
                    "; no serial console"
                  )}
                  {L.uart.note ? ". " + L.uart.note : ""}
                </>
              }
            />
            {cat &&
              Object.entries(cat.extraUart).map(([n, p]) => {
                const cf = c.extraUart[n],
                  lv = L.extraUart?.[n];
                return (
                  <Row
                    key={n}
                    id={n}
                    name={
                      <>
                        {n.toUpperCase()} on {gp(p.tx, p.rx)} <small>TX/RX</small>
                      </>
                    }
                    on={cf.enabled}
                    live={lv?.enabled ? `on${lv.tty ? `, /dev/${lv.tty}` : ", no tty yet"}` : "off"}
                  >
                    <div className="pr-row">
                      <label>
                        <input type="checkbox" id={`gi-${n}-cts`} defaultChecked={cf.ctsrts} /> CTS/RTS on {gp(p.cts, p.rts)}
                      </label>
                    </div>
                  </Row>
                );
              })}
          </>
        )}
        {show("w1") && (
          <Row id="w1" name="1-Wire (w1-gpio)" on={c.onewire.enabled} live={L.onewire.nodes.length ? L.onewire.nodes.map((n: { gpio: number }) => `GPIO${n.gpio}`).join(", ") + `, ${L.onewire.masters} master(s)` : "off"}>
            <div className="pr-row">
              <label>
                Data pin GPIO <input id="gi-w1pin" type="number" min={2} max={27} defaultValue={c.onewire.pin ?? 4} style={{ width: 60 }} />
              </label>
              <span className="pr-note">needs a 4.7 kΩ pull-up to 3V3</span>
            </div>
          </Row>
        )}
      </div>
      <p className="pr-note">Pins in use: {info.pins.map((u) => `GPIO${u.gpio} ${u.owner} ${u.role}`).join(" · ") || "none"}</p>
      {info.conflicts.length > 0 && (
        <p className="pr-note" style={{ color: "var(--danger)" }}>
          Pin conflicts in config.txt now: {info.conflicts.map((x) => `GPIO${x.gpio} (${x.users.join(", ")})`).join("; ")}
        </p>
      )}
      {c.conditional?.length > 0 && (
        <p className="pr-note" style={{ color: "var(--warning)" }}>
          Also set in conditional sections: {c.conditional.map((x: { section: string; line: string }) => `[${x.section}] ${x.line}`).join("; ")}
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
