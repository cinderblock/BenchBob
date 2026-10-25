// Addressable LED strings (WS2812 family, APA102, WS2801): the strings with their state,
// a pattern editor per string, and the add/edit dialog with a pin picker. The same body
// serves a node's section and a Linux bob's panel: both speak /api/leds.

import { useEffect, useState, type ReactNode } from "react";
import { useBob } from "../../bob/context";
import { Hint, Note, Pill } from "../../ui/common";
import { PinPicker, type PickPin } from "../../ui/PinPicker";
import { ActionLine, useAction } from "../../ui/status";
import { usePoll } from "../../sections/node/usePoll";
import { ORDERS, PARAM_LABEL, PATTERN_PARAMS, type LedPattern, type LedString, type LedsState, type LedTiming } from "./api";
import "./leds.css";

const STATE_PILL: Record<LedString["state"], [string, string]> = {
  running: ["on", "running"],
  off: ["", "dark: output control off"],
  pending: ["warn", "starting"],
  error: ["bad", "error"],
};

const isColour = (k: string) => ["color", "background", "a", "b"].includes(k);
/** #rrggbb for <input type=color> (which has no white channel). */
const rgb = (c: string | undefined) => (c && /^#[0-9a-f]{6}/i.test(c) ? c.slice(0, 7) : "#ffffff");

/**
 * `fix`: what to offer beside a string's error (the Linux panel's "SPI settings…"). A prop
 * so the node build doesn't carry the GPIO panel's dialog.
 */
export function LedStrips({ fix }: { fix?: (error: string) => ReactNode } = {}) {
  const b = useBob();
  const { data, error, reload } = usePoll<LedsState>("/api/leds", 2000);
  const caps = data?.capabilities;
  const full = !!caps && !!data && data.strings.length >= caps.maxStrings;
  return (
    <div className="pr-body">
      {error && <Note color="danger">{error}</Note>}
      {!data ? (
        <Note>Waiting for the first reading…</Note>
      ) : !caps ? (
        <Note>This board has no LED string driver.</Note>
      ) : (
        <>
          <div className="pr-row">
            <button className="btn btn-sm" disabled={full} onClick={() => b.openModal(<StringDialog caps={caps} others={data.strings} onDone={reload} />, { cls: "wide" })}>
              Add string…
            </button>
            <span className="pr-note">
              {data.strings.length} of {caps.maxStrings} strings · up to {caps.maxPixels} pixels each
            </span>
            <Hint>
              WS2812B / SK6812 / WS2813 / WS2811 (one data pin, timed) and APA102 / SK9822 / WS2801 (data + clock). Set a string up any time; it lights up only while GPIO output control is on, and goes dark when it's turned off. Use the order-test pattern to check the colour order, and the ruler to check the length.
            </Hint>
          </div>
          {!data.outputEnabled && data.strings.length > 0 && <Note>Output control is off, so the strings are dark. Turn GPIO output control on to light them.</Note>}
          {data.strings.map((s) => (
            <StringCard key={s.id} s={s} caps={caps} others={data.strings} onDone={reload} fix={fix} />
          ))}
        </>
      )}
    </div>
  );
}

function StringCard({
  s,
  caps,
  others,
  onDone,
  fix,
}: {
  s: LedString;
  caps: NonNullable<LedsState["capabilities"]>;
  others: LedString[];
  onDone(): void;
  fix?: (error: string) => ReactNode;
}) {
  const b = useBob();
  const a = useAction();
  const [cls, text] = STATE_PILL[s.state];
  const quick = (name: string) => a.run(() => b.api("POST", `/api/leds/${s.id}/pattern`, { name }), { busy: "Setting…" }).then(onDone);
  const remove = async () => {
    if (!(await b.confirm(`Remove ${s.name}`, "The string goes dark and its pins are free again.", `${s.chip} on GPIO ${s.pin}${s.clockPin != null ? ` + ${s.clockPin}` : ""}`))) return;
    await a.run(() => b.api("DELETE", `/api/leds/${s.id}`), { busy: "Removing…" });
    onDone();
  };
  return (
    <div className="led-card">
      <div className="pr-row">
        <strong>{s.name}</strong>
        <Pill cls={cls}>{text}</Pill>
        <span className="pr-note">
          {s.chip} · GPIO {s.pin}
          {s.clockPin != null && ` (clock ${s.clockPin})`} · {s.pixels} px · {s.order} · brightness {s.brightness}
          {s.estimateMa != null && ` · ~${s.estimateMa} mA`}
        </span>
      </div>
      {s.error && <Note color="danger">{s.error}</Note>}
      {s.error && fix?.(s.error)}
      <div className="pr-row">
        <button className="btn btn-sm" onClick={() => quick("order-test")}>
          Order test
        </button>
        <button className="btn btn-sm" onClick={() => quick("ruler")}>
          Ruler
        </button>
        <button className="btn btn-sm" onClick={() => quick("off")}>
          Off
        </button>
        <button className="btn btn-sm" onClick={() => b.openModal(<StringDialog caps={caps} others={others} edit={s} onDone={onDone} />, { cls: "wide" })}>
          Edit…
        </button>
        <button className="btn btn-sm btn-warn" onClick={remove}>
          Remove
        </button>
        <ActionLine a={a} />
      </div>
      <PatternEditor s={s} patterns={caps.patterns} onDone={onDone} />
    </div>
  );
}

function PatternEditor({ s, patterns, onDone }: { s: LedString; patterns: string[]; onDone(): void }) {
  const b = useBob();
  const a = useAction();
  const [p, setP] = useState<LedPattern>(() => s.pattern ?? { name: "solid", color: "#ffffff" });
  const params = PATTERN_PARAMS[p.name] ?? [];
  const apply = (next: LedPattern) => a.run(() => b.api("POST", `/api/leds/${s.id}/pattern`, next), { busy: "Applying…" }).then(onDone);
  return (
    <div className="pr-row">
      <label>
        Pattern{" "}
        <select value={p.name} onChange={(e) => setP({ name: e.currentTarget.value })}>
          {patterns.map((n) => (
            <option key={n} value={n}>
              {n}
            </option>
          ))}
        </select>
      </label>
      {params.map((k) => (
        <label key={k}>
          {PARAM_LABEL[k] ?? k}{" "}
          {isColour(k) ? (
            <input type="color" value={rgb(p[k] as string | undefined)} onInput={(e) => setP({ ...p, [k]: e.currentTarget.value })} />
          ) : (
            <input type="number" min={0} style={{ width: 80 }} value={(p[k] as number | undefined) ?? ""} onInput={(e) => setP({ ...p, [k]: e.currentTarget.value === "" ? undefined : Number(e.currentTarget.value) })} />
          )}
        </label>
      ))}
      <button className="btn btn-sm btn-primary" disabled={a.busy} onClick={() => apply(p)}>
        Show
      </button>
      {s.source === "pixels" && <span className="pr-note">Showing pixels set through the API.</span>}
      <ActionLine a={a} />
    </div>
  );
}

const DEFAULT_TIMING: LedTiming = { t0hNs: 400, t1hNs: 800, periodNs: 1250, resetUs: 300 };

function StringDialog({ caps, others, edit, onDone }: { caps: NonNullable<LedsState["capabilities"]>; others: LedString[]; edit?: LedString; onDone(): void }) {
  const b = useBob();
  const a = useAction();
  const [name, setName] = useState(edit?.name ?? "");
  const [chip, setChip] = useState(edit?.chip ?? caps.chips[0]?.name ?? "ws2812b");
  const [pin, setPin] = useState<number | null>(edit?.pin ?? null);
  const [clockPin, setClockPin] = useState<number | null>(edit?.clockPin ?? null);
  const [pixels, setPixels] = useState(String(edit?.pixels ?? 60));
  const [order, setOrder] = useState(edit?.order ?? "");
  const [brightness, setBrightness] = useState(edit?.brightness ?? 128);
  const [gamma, setGamma] = useState(edit?.gamma ?? true);
  const [invert, setInvert] = useState(edit?.invert ?? false);
  const [timing, setTiming] = useState<LedTiming>(edit?.timing ?? DEFAULT_TIMING);
  const [clockHz, setClockHz] = useState(String(edit?.clockHz ?? 2_000_000));
  const [budget, setBudget] = useState(!!edit?.power);
  const [maxMa, setMaxMa] = useState(String(edit?.power?.maxMa ?? 2000));
  const [maPer, setMaPer] = useState(String(edit?.power?.maPerChannel ?? 20));
  const [gpio, setGpio] = useState<PickPin[] | null>(null);
  const kind = caps.chips.find((c) => c.name === chip)?.kind ?? "one-wire";

  // The board's pins with what holds them: a Linux bob says per pin (SPI or not); a node's
  // come from its GPIO view. This string's own pins are fine either way.
  useEffect(() => {
    if (caps.pinInfo) {
      setGpio(caps.pinInfo.map((p) => ({ gpio: p.gpio, header: p.header, refusal: p.refusal })));
      return;
    }
    b.api<{ pins: PickPin[] }>("GET", "/api/gpio").then(
      (g) => setGpio(g.pins.filter((p) => caps.pins.includes(p.gpio) || p.refusal).map((p) => ({ gpio: p.gpio, header: p.header, refusal: p.refusal }))),
      () => setGpio(caps.pins.map((g) => ({ gpio: g, header: null, refusal: null }))),
    );
  }, []);
  const mine = new Set(edit ? [edit.pin, edit.clockPin].filter((g): g is number => g != null) : []);
  const taken = (g: number, also: number | null) => (g === also ? "used for the other pin of this string" : null);
  const oneWireOnly = (g: number) =>
    kind === "one-wire" && caps.pinInfo && !caps.pinInfo.find((p) => p.gpio === g)?.oneWire ? `one-wire strings need an SPI MOSI pin${caps.oneWireHint ? ` (${caps.oneWireHint})` : ""}` : null;
  const pickPins = (also: number | null, data = false): PickPin[] =>
    (gpio ?? []).map((p) => ({
      ...p,
      refusal: (data ? oneWireOnly(p.gpio) : null) ?? (mine.has(p.gpio) ? taken(p.gpio, also) : (p.refusal ?? taken(p.gpio, also))),
    }));

  const save = async () => {
    const body: Record<string, unknown> = {
      name: name || undefined,
      chip,
      pin,
      pixels: Number(pixels),
      brightness,
      gamma,
      invert,
      power: budget ? { maxMa: Number(maxMa), maPerChannel: Number(maPer) } : null,
    };
    if (order) body.order = order;
    if (kind === "clocked") Object.assign(body, { clockPin, clockHz: Number(clockHz) });
    if (chip === "custom") body.timing = timing;
    const ok = await a.run(() => b.api("POST", edit ? `/api/leds/${edit.id}` : "/api/leds", body), { busy: "Saving…" });
    if (ok) {
      onDone();
      b.closeModal();
    }
  };
  const num = (v: string, set: (s: string) => void, w = 90) => <input type="number" min={0} style={{ width: w }} value={v} onInput={(e) => set(e.currentTarget.value)} />;
  return (
    <>
      <h3>{edit ? `Edit ${edit.name}` : "Add an LED string"}</h3>
      <div className="pr-row">
        <label>
          Name <input value={name} placeholder="e.g. Shelf" onInput={(e) => setName(e.currentTarget.value)} />
        </label>
      </div>
      <div className="pr-row">
        <label>
          Chip{" "}
          <select value={chip} onChange={(e) => setChip(e.currentTarget.value)}>
            {caps.chips.map((c) => (
              <option key={c.name} value={c.name}>
                {c.description}
              </option>
            ))}
          </select>
        </label>
      </div>
      <div className="stat-label">{kind === "clocked" ? "Data pin" : "Pin"}</div>
      <PinPicker pins={pickPins(kind === "clocked" ? clockPin : null, true)} value={pin} onChange={setPin} label="Data pin" />
      {caps.pinInfo && kind === "one-wire" && caps.oneWireHint && <div className="pr-note">{caps.oneWireHint}</div>}
      {kind === "clocked" && (
        <>
          <div className="stat-label">Clock pin</div>
          <PinPicker pins={pickPins(pin)} value={clockPin} onChange={setClockPin} label="Clock pin" />
        </>
      )}
      <div className="pr-row">
        <label>
          Pixels {num(pixels, setPixels, 80)}
        </label>
        <label>
          Colour order{" "}
          <input list="led-orders" value={order} placeholder={edit ? edit.order : "chip default"} style={{ width: 90 }} onInput={(e) => setOrder(e.currentTarget.value.toUpperCase())} />
          <datalist id="led-orders">
            {ORDERS.map((o) => (
              <option key={o} value={o} />
            ))}
          </datalist>
        </label>
        <label>
          Brightness <input type="range" min={0} max={255} value={brightness} onInput={(e) => setBrightness(Number(e.currentTarget.value))} /> {brightness}
        </label>
      </div>
      {chip === "custom" && (
        <div className="pr-row">
          {(["t0hNs", "t1hNs", "periodNs", "resetUs"] as const).map((k) => (
            <label key={k}>
              {{ t0hNs: "T0H ns", t1hNs: "T1H ns", periodNs: "Bit ns", resetUs: "Reset µs" }[k]} {num(String(timing[k]), (v) => setTiming({ ...timing, [k]: Number(v) }), 70)}
            </label>
          ))}
          <Hint>High time for a 0 bit, high time for a 1 bit, the whole bit, and how long the line stays low to latch. From the chip's datasheet; the board rejects timings it can't make within 150 ns.</Hint>
        </div>
      )}
      {kind === "clocked" && (
        <div className="pr-row">
          <label>
            Clock (Hz) {num(clockHz, setClockHz, 110)}
          </label>
          <span className="pr-note">up to {caps.maxClockHz.toLocaleString()} Hz; lower for long wires</span>
        </div>
      )}
      <div className="pr-row">
        <label>
          <input type="checkbox" checked={gamma} onChange={(e) => setGamma(e.currentTarget.checked)} /> gamma
        </label>
        <label>
          <input type="checkbox" checked={invert} onChange={(e) => setInvert(e.currentTarget.checked)} /> inverted data (inverting level shifter)
        </label>
      </div>
      <div className="pr-row">
        <label>
          <input type="checkbox" checked={budget} onChange={(e) => setBudget(e.currentTarget.checked)} /> power budget
        </label>
        {budget && (
          <>
            <label>
              max mA {num(maxMa, setMaxMa, 80)}
            </label>
            <label>
              mA per channel at full {num(maPer, setMaPer, 60)}
            </label>
          </>
        )}
      </div>
      <p className="pr-note">
        3.3 V data into a 5 V strip is marginal: use a level shifter (74AHCT125) if the first pixel misbehaves. {others.length > 0 && !edit && "Pins held by other strings are greyed out."}
      </p>
      <div className="modal-btns">
        <ActionLine a={a} />
        <button className="btn" onClick={b.closeModal}>
          Cancel
        </button>
        <button className="btn btn-primary" disabled={a.busy || pin == null || (kind === "clocked" && clockPin == null)} onClick={save}>
          {edit ? "Save" : "Add"}
        </button>
      </div>
    </>
  );
}
