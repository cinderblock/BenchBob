// The two bootloader dialogs: rewriting a Compute Module's EEPROM over USB (rpiboot), and
// writing a bootloader-update SD card for an ordinary Pi 4/400 or 5/500. Loaded on demand.

import { useEffect, useState } from "react";
import { useBob, type BobApi } from "../../bob/context";
import { fmtSz } from "../../lib/format";
import { readLocalStr, writeLocal } from "../../lib/storage";
import { BlEditor, editorReady, imageStart, useBlEditor, type EepromImage, type Start } from "./Editor";

export interface RpibootOptions {
  ok: boolean;
  message?: string;
  chip: string;
  images: EepromImage[];
  bootConf: string;
  checkedAt?: number;
  installed?: { label: string; bootConf?: string | null; buildDate?: string; lastWrite?: { imageLabel: string; at: number; bootConf: string } } | null;
}

async function post(b: BobApi, url: string, body: unknown): Promise<boolean> {
  try {
    await b.conn.json("POST", url, body);
    return true;
  } catch (e: any) {
    b.toast(e?.message || "Request failed", "error");
    return false;
  }
}

/** Bootloader settings & update for a CM in USB boot mode. */
export async function openRpibootWrite(b: BobApi, path: string) {
  let o: RpibootOptions;
  try {
    o = await b.conn.json("GET", "/api/rpiboot/eeprom/options?path=" + encodeURIComponent(path));
  } catch (e: any) {
    return b.toast(e?.message || "Request failed", "error");
  }
  if (!o.images.length) return b.toast("No bootloader images found (is rpi-eeprom installed?)", "error");
  b.openModal(<RpibootWrite path={path} o={o} />, { cls: "wide" });
}

function Installed({ o }: { o: RpibootOptions }) {
  const inst = o.installed;
  if (!inst)
    return (
      <div className="modal-dev">
        Installed bootloader: <b>not checked</b>
        <div className="img-detail" style={{ marginTop: 4 }}>
          Nothing can read the bootloader back over USB, but <b>Bootloader info</b> can often recognise it. Run it first, power-cycle the module, then come back here to start from its current settings.
        </div>
      </div>
    );
  return (
    <div className="modal-dev">
      Installed bootloader: <b>{inst.label}</b>
      {o.checkedAt && <span style={{ color: "var(--dim)" }}> (checked {new Date(o.checkedAt).toLocaleTimeString()})</span>}
      {inst.bootConf == null && (
        <div className="img-detail" style={{ marginTop: 4 }}>
          Its settings are unknown, so whatever is written here replaces them completely.
        </div>
      )}
      {inst.lastWrite && (
        <div className="img-detail" style={{ marginTop: 4 }}>
          BenchBob last wrote {inst.lastWrite.imageLabel} to this module on {new Date(inst.lastWrite.at).toLocaleString()}; something has replaced it since.
        </div>
      )}
    </div>
  );
}

function RpibootWrite({ path, o }: { path: string; o: RpibootOptions }) {
  const b = useBob();
  // Starting points: what's installed (when recognised), BenchBob's last write to this
  // module, rpiboot's CM default, or the chosen release's own built-in config.
  const ed = useBlEditor({
    board: { chip: o.chip, cm: true },
    images: o.images,
    installedBuild: o.installed?.buildDate,
    prefer: ["installed", "last", "rpiboot"],
    starts: (img) => {
      const inst = o.installed,
        s: Start[] = [];
      if (inst?.bootConf != null) s.push(["installed", "Installed settings", inst.bootConf, "the installed settings"]);
      if (inst?.lastWrite) s.push(["last", "BenchBob's last write", inst.lastWrite.bootConf, "BenchBob's last write"]);
      s.push(["rpiboot", "rpiboot's CM default", o.bootConf, "rpiboot's CM default"]);
      return [...s, ...imageStart(img)];
    },
  });
  const go = async (imageId: string, bootConf: string) => {
    b.closeModal();
    await post(b, "/api/rpiboot/eeprom/write", { path, imageId, bootConf });
  };
  return (
    <>
      <h3>Bootloader settings &amp; update</h3>
      <p>
        Rewrites the SPI bootloader EEPROM of the BCM{o.chip} on USB port <span className="mono">{path}</span> with the chosen release and settings. A bad write isn't permanent: the module can always be recovered again from here.
      </p>
      <Installed o={o} />
      <BlEditor ed={ed} />
      <div className="modal-btns">
        <button className="btn" onClick={b.closeModal}>
          Cancel
        </button>
        <button
          className="btn"
          onClick={() => {
            if (!o.images.some((i) => i.id === "rpiboot")) return b.toast("rpiboot's stock image is missing", "error");
            go("rpiboot", o.bootConf);
          }}
        >
          Recover (stock image + config)
        </button>
        <button
          className="btn btn-warn"
          onClick={() => {
            const r = editorReady(ed);
            if ("error" in r) return b.toast(r.error, "error");
            go(ed.imageId, r.conf);
          }}
        >
          Write bootloader
        </button>
      </div>
      <div className="img-detail" style={{ textAlign: "right", marginTop: 6 }}>
        Recover writes rpiboot's stock release with its stock config: the known-good choice for a module that won't boot.
      </div>
    </>
  );
}

// ---------- Bootloader update SD cards ----------
// For ordinary Pi 4/400 and Pi 5/500 boards: a card holding recovery.bin and the built
// EEPROM image, which the boot ROM runs at power-on. The server builds it to fit any card,
// so it works on the small cards Imager refuses (its image is 258 MiB).

export function BlCardDialog({ dev, model, size, hasParts }: { dev: string; model: string; size: number; hasParts: boolean }) {
  const b = useBob();
  const [chip, setChip] = useState(() => (readLocalStr("blcard-chip") === "2711" ? "2711" : "2712"));
  const [opts, setOpts] = useState<{ chip: string; images?: EepromImage[]; error?: string } | null>(null);

  useEffect(() => {
    let live = true;
    setOpts(null);
    b.conn
      .json<{ images: EepromImage[] }>("GET", "/api/blcard/options?chip=" + chip)
      .then((o) => live && setOpts(o.images.length ? { chip, images: o.images } : { chip, error: "No bootloader releases found (is rpi-eeprom installed?)" }))
      .catch((e) => live && setOpts({ chip, error: e?.message || "Request failed" }));
    return () => {
      live = false;
    };
  }, [chip]);

  const ready = opts && opts.chip === chip && opts.images;
  return (
    <>
      <h3>Bootloader update card</h3>
      <p>Erases this card and writes one that updates a Raspberry Pi's bootloader when the Pi boots from it, with the release and settings chosen here. Any card from 16 MB up works, including ones too small for Imager's 258 MB image.</p>
      <div className="modal-dev">
        {model}
        <br />
        /dev/{dev} · {fmtSz(size)}
      </div>
      <div className="fg">
        <label htmlFor="blc-board">Board</label>
        <select
          id="blc-board"
          value={chip}
          onChange={(e) => {
            const c = e.currentTarget.value;
            writeLocal("blcard-chip", c);
            setChip(c);
          }}
        >
          <option value="2711">Raspberry Pi 4 / 400</option>
          <option value="2712">Raspberry Pi 5 / 500</option>
        </select>
        <div className="img-detail">
          Compute Modules can't update from a card: put them in USB boot mode and use <b>Bootloader settings &amp; update</b> instead.
        </div>
      </div>
      {ready ? (
        <CardEditor key={chip} chip={chip} images={opts.images!} dev={dev} hasParts={hasParts} />
      ) : (
        <>
          <div style={{ marginTop: 12 }}>{opts?.error ? <div className="bl-warn">{opts.error}</div> : <div className="img-detail">Loading releases…</div>}</div>
          <div className="modal-btns">
            <button className="btn" onClick={b.closeModal}>
              Cancel
            </button>
            <button className={`btn ${hasParts ? "btn-danger" : "btn-warn"}`} onClick={() => b.toast("Still loading the releases", "error")}>
              Erase &amp; write card
            </button>
          </div>
        </>
      )}
    </>
  );
}

function CardEditor({ chip, images, dev, hasParts }: { chip: string; images: EepromImage[]; dev: string; hasParts: boolean }) {
  const b = useBob();
  const [confirmText, setConfirmText] = useState("");
  // The last card's settings for this board, so a batch of cards comes out the same.
  const [last] = useState(() => readLocalStr("blcard-conf-" + chip));
  const ed = useBlEditor({
    board: { chip, cm: false },
    images,
    prefer: ["last", "image"],
    starts: (img) => [...(last ? ([["last", "Last card", last, "the last card written here"]] as Start[]) : []), ...imageStart(img)],
  });
  const write = async () => {
    if (hasParts && confirmText !== "ERASE") return;
    const r = editorReady(ed);
    if ("error" in r) return b.toast(r.error, "error");
    b.closeModal();
    try {
      await b.conn.json("POST", "/api/blcard", { device: dev, chip, imageId: ed.imageId, bootConf: r.conf });
      writeLocal("blcard-conf-" + chip, r.conf);
    } catch (e: any) {
      b.toast(e?.message || "Request failed", "error");
    }
  };
  return (
    <>
      <div style={{ marginTop: 12 }}>
        <BlEditor ed={ed} />
      </div>
      {hasParts && (
        <div className="confirm-box">
          <label htmlFor="ec">This card has partitions. Type ERASE to confirm:</label>
          <input type="text" id="ec" autoComplete="off" value={confirmText} onInput={(e) => setConfirmText(e.currentTarget.value)} />
        </div>
      )}
      <div className="modal-btns">
        <button className="btn" onClick={b.closeModal}>
          Cancel
        </button>
        <button className={`btn ${hasParts ? "btn-danger" : "btn-warn"}`} disabled={hasParts && confirmText !== "ERASE"} onClick={write}>
          Erase &amp; write card
        </button>
      </div>
    </>
  );
}
