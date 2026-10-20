// The part both bootloader dialogs share: release picker, starting points, the settings
// form and the raw boot.conf, kept in sync (the text is the source of truth).

import { useMemo, useState } from "react";
import {
  BL_GROUPS,
  BL_ORDER_PRESETS,
  BL_SETTINGS,
  addableModes,
  blRest,
  blSet,
  blValues,
  boardKey,
  intOk,
  modeLabel,
  orderAdd,
  orderHex,
  orderItems,
  orderWarnings,
  settingDefault,
  settingsFor,
  type BlBoard,
  type BlSetting,
} from "./bootconf";

export interface EepromImage {
  id: string;
  label: string;
  buildDate: string;
  config?: string;
}

/** [id, button label, text, "changes from …" label] */
export type Start = [string, string, string, string];

export interface EditorState {
  board: BlBoard;
  images: EepromImage[];
  installedBuild?: string;
  imageId: string;
  setImageId(id: string): void;
  text: string;
  setText(t: string): void;
  base: string;
  baseLabel: string;
  start: string;
  load(id: string): void;
  starts: Start[];
  /** Keys whose field holds a value that can't be written. */
  bad: Set<string>;
  setBad(key: string, bad: boolean): void;
}

/** The selected release's own built-in config is always one of the starting points. */
export function imageStart(img: EepromImage | undefined): Start[] {
  return img?.config ? [["image", "Release built-in", img.config, `the ${img.buildDate} release's built-in config`]] : [];
}

export function useBlEditor(o: { board: BlBoard; images: EepromImage[]; installedBuild?: string; starts: (img: EepromImage | undefined) => Start[]; prefer: string[] }): EditorState {
  const [imageId, setImageId] = useState(() => (o.images.find((i) => i.id === "default") || o.images[0])?.id || "");
  const img = o.images.find((i) => i.id === imageId) || o.images[0];
  const starts = o.starts(img);
  const [init] = useState(() => {
    const ids = starts.map((s) => s[0]);
    return starts.find((s) => s[0] === (o.prefer.find((id) => ids.includes(id)) || ids[0]));
  });
  const [text, setText] = useState(init?.[2] || "");
  const [base, setBase] = useState(init?.[2] || "");
  const [baseLabel, setBaseLabel] = useState(init?.[3] || "");
  const [start, setStart] = useState(init?.[0] || "");
  const [bad, setBadSet] = useState<Set<string>>(new Set());
  return {
    board: o.board,
    images: o.images,
    installedBuild: o.installedBuild,
    imageId,
    setImageId,
    text,
    setText,
    base,
    baseLabel,
    start,
    starts,
    load(id) {
      const s = starts.find((x) => x[0] === id);
      if (!s) return;
      setStart(id);
      setBase(s[2]);
      setBaseLabel(s[3]);
      setText(s[2]);
      setBadSet(new Set());
    },
    bad,
    setBad(key, b) {
      setBadSet((p) => {
        if (p.has(key) === b) return p;
        const n = new Set(p);
        b ? n.add(key) : n.delete(key);
        return n;
      });
    },
  };
}

/** The edited boot.conf, or an error message when it can't be written yet. */
export function editorReady(ed: EditorState): { conf: string } | { error: string } {
  if (ed.bad.size) return { error: "Fix the highlighted settings first" };
  if (!ed.text.trim()) return { error: "boot.conf is empty" };
  return { conf: ed.text };
}

export function BlEditor({ ed }: { ed: EditorState }) {
  const vals = useMemo(() => blValues(ed.text), [ed.text]);
  const newest = ed.images.reduce((a, i) => (i.buildDate > a ? i.buildDate : a), "");
  const have = ed.installedBuild;
  const known = new Set(["BOOT_ORDER", ...BL_SETTINGS.map((s) => s.key)]);
  const extra = Object.keys(vals).filter((k) => !known.has(k));
  const ss = settingsFor(ed.board);
  return (
    <div className="flash-form">
      <div className="fg">
        <label htmlFor="bl-img">Bootloader release</label>
        <select id="bl-img" value={ed.imageId} onChange={(e) => ed.setImageId(e.currentTarget.value)}>
          {ed.images.map((im) => {
            const tag = !have ? "" : im.buildDate === have ? " · installed" : im.buildDate > have ? " · newer than installed" : " · older than installed";
            return (
              <option key={im.id} value={im.id}>
                {im.label}
                {tag}
                {im.buildDate === newest && ed.images.length > 1 ? " · newest" : ""}
              </option>
            );
          })}
        </select>
      </div>
      <div className="bl-starts">
        Start from:{" "}
        {ed.starts.map(([id, label]) => (
          <button key={id} type="button" className={`btn btn-sm${ed.start === id ? " btn-primary" : ""}`} onClick={() => ed.load(id)}>
            {label}
          </button>
        ))}
      </div>
      <div className="bl-grid">
        <div className="bl-form">
          {BL_GROUPS.map((g) => {
            const gs = ss.filter((s) => s.g === g);
            const set = gs.some((s) => s.key in vals);
            return (
              <details key={g} open={g === "Boot" || g === "Power" || set}>
                <summary>{g}</summary>
                {g === "Boot" && (
                  <div className="fg">
                    <label>Boot order</label>
                    <BootOrder ed={ed} raw={vals.BOOT_ORDER} />
                  </div>
                )}
                <div className="settings-grid">
                  {gs.map((s) => (
                    <Field key={s.key} s={s} ed={ed} value={vals[s.key] ?? ""} />
                  ))}
                </div>
              </details>
            );
          })}
          <div className="img-detail">
            {extra.length > 0 && (
              <>
                Also set, edit in boot.conf: <span className="mono">{extra.join(", ")}</span>
              </>
            )}
          </div>
        </div>
        <div className="bl-raw">
          <div className="fg">
            <label htmlFor="bl-conf">boot.conf</label>
          </div>
          <textarea id="bl-conf" spellCheck={false} value={ed.text} onInput={(e) => ed.setText(e.currentTarget.value)} />
          <div className="img-detail">
            Embedded into the image. Edits here and in the form stay in sync; conditional sections such as <span className="mono">[gpio4=0]</span> are kept as written.
          </div>
          <Changes ed={ed} />
        </div>
      </div>
    </div>
  );
}

function Changes({ ed }: { ed: EditorState }) {
  const a = blValues(ed.base),
    b = blValues(ed.text);
  const rows = [...new Set([...Object.keys(a), ...Object.keys(b)])]
    .filter((k) => a[k] !== b[k])
    .map((k) => (
      <div key={k}>
        {k}: {k in a && <span className="old">{a[k]}</span>} {k in b ? <span className="new">{b[k]}</span> : "(default)"}
      </div>
    ));
  if (blRest(ed.base) !== blRest(ed.text)) rows.push(<div key="__rest">Conditional sections changed (see boot.conf)</div>);
  return (
    <div className="bl-diff">
      <div style={{ marginBottom: 2 }}>Changes from {ed.baseLabel}:</div>
      {rows.length ? rows : <div>none</div>}
    </div>
  );
}

function Field({ s, ed, value }: { s: BlSetting; ed: EditorState; value: string }) {
  const id = "bls-" + s.key;
  const def = settingDefault(s, ed.board);
  // While a text field is being typed in, it shows what was typed (even if it can't be
  // written yet); otherwise it shows what the text says.
  const [draft, setDraft] = useState<string | null>(null);
  const write = (raw: string) => {
    const v = raw.replace(/[\r\n]/g, "").trim();
    if (s.t === "int" && !intOk(s, v)) {
      ed.setBad(s.key, true);
      return;
    }
    ed.setBad(s.key, false);
    ed.setText(blSet(ed.text, s.key, v === "" ? null : v));
  };
  let ctl;
  if (s.t === "bool" || s.t === "select") {
    const opts: [string, string][] = s.t === "bool" ? [["1", "On"], ["0", "Off"]] : s.options!;
    const dl = s.t === "bool" ? (def === "1" ? "on" : "off") : opts.find(([v]) => v === def)?.[1];
    ctl = (
      <select id={id} value={value} onChange={(e) => write(e.currentTarget.value)}>
        <option value="">Default{dl ? ` (${dl})` : ""}</option>
        {opts.map(([v, l]) => (
          <option key={v} value={v}>
            {l}
          </option>
        ))}
        {/* Keep a value the form doesn't know as-is. */}
        {value !== "" && !opts.some(([v]) => v === value) && <option value={value}>{value} (custom)</option>}
      </select>
    );
  } else {
    const ph = s.ph || (def !== "" ? `Default: ${def}` : "Not set");
    ctl = (
      <input
        type="text"
        id={id}
        className={ed.bad.has(s.key) ? "bad" : ""}
        inputMode={s.t === "int" ? "numeric" : undefined}
        placeholder={ph}
        spellCheck={false}
        value={draft ?? value}
        onFocus={() => setDraft(value)}
        onBlur={() => setDraft(null)}
        onInput={(e) => {
          setDraft(e.currentTarget.value);
          write(e.currentTarget.value);
        }}
      />
    );
  }
  return (
    <div className="fg">
      <label htmlFor={id}>{s.label}</label>
      {ctl}
      {s.hint && <div className="img-detail">{s.hint}</div>}
    </div>
  );
}

function BootOrder({ ed, raw }: { ed: EditorState; raw: string | undefined }) {
  const b = ed.board;
  const items = orderItems(raw);
  if (items === null) return <div className="bl-warn">BOOT_ORDER={raw} isn't a hex number. Fix it in boot.conf.</div>;
  const save = (it: string[]) => ed.setText(blSet(ed.text, "BOOT_ORDER", orderHex(it)));
  const move = (i: number, d: number) => {
    const it = [...items];
    if (d === 0) it.splice(i, 1);
    else [it[i], it[i + d]] = [it[i + d], it[i]];
    save(it);
  };
  return (
    <>
      <div className="bl-order">
        {!items.length && <span className="img-detail">Not set: the firmware default, {b.cm ? "eMMC/SD" : "SD card"} then USB, repeating (0xf41).</span>}
        {items.map((m, i) => (
          <span key={i} style={{ display: "contents" }}>
            {i > 0 && <span className="bl-sep">→</span>}
            <span className={`bl-pill${m === "e" || m === "f" ? " term" : ""}`}>
              {modeLabel(m, b)}
              <button type="button" onClick={() => move(i, -1)} disabled={i === 0} aria-label="Earlier">
                ‹
              </button>
              <button type="button" onClick={() => move(i, 1)} disabled={i === items.length - 1} aria-label="Later">
                ›
              </button>
              <button type="button" onClick={() => move(i, 0)} aria-label="Remove">
                ✕
              </button>
            </span>
          </span>
        ))}
      </div>
      <div className="bl-tools">
        <select value="" disabled={items.length >= 8} onChange={(e) => e.currentTarget.value && save(orderAdd(items, e.currentTarget.value))}>
          <option value="">+ Add boot mode…</option>
          {addableModes(b).map((m) => (
            <option key={m} value={m}>
              {modeLabel(m, b)}
            </option>
          ))}
        </select>
        <select value="" onChange={(e) => e.currentTarget.value && ed.setText(blSet(ed.text, "BOOT_ORDER", e.currentTarget.value))}>
          <option value="">Presets…</option>
          {(BL_ORDER_PRESETS[boardKey(b)] || []).map(([v, l]) => (
            <option key={v} value={v}>
              {v}: {l}
            </option>
          ))}
        </select>
        {items.length > 0 && (
          <span className="img-detail mono" style={{ alignSelf: "center" }}>
            BOOT_ORDER={orderHex(items)}
          </span>
        )}
      </div>
      {orderWarnings(items, b).map((w) => (
        <div key={w} className="bl-warn">
          ⚠️ {w}
        </div>
      ))}
    </>
  );
}
