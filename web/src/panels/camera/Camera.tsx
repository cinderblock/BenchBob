// Camera section: one card per camera with an opt-in live view, snapshot/download, stream
// settings and image controls. The live <img> only exists while the user has asked for it
// AND the card is on screen in a visible tab, because every open view keeps the camera
// (and ustreamer) running and costs tens of Mbit/s.

import { useEffect, useRef, useState } from "react";
import { useBob } from "../../bob/context";
import type { PanelProps } from "../../registry";
import { readLocal, writeLocal } from "../../lib/storage";
import { camPaths, type CameraControl, type CameraDetail, type CameraPublic, type CameraState, type StreamChoice } from "./api";

const fmtStream = (s: StreamChoice | null) => (s ? `${s.fourcc} ${s.w}×${s.h} @ ${s.fps} fps` : "no streamable format");

function usePageVisible() {
  const [v, setV] = useState(() => document.visibilityState === "visible");
  useEffect(() => {
    const f = () => setV(document.visibilityState === "visible");
    document.addEventListener("visibilitychange", f);
    return () => document.removeEventListener("visibilitychange", f);
  }, []);
  return v;
}

export default function Camera({ st }: PanelProps<CameraState>) {
  const b = useBob();
  const key = b.key("cameraLive");
  const [live, setLive] = useState<Record<string, boolean>>(() => readLocal(key, {}));
  const setWanted = (id: string, on: boolean) =>
    setLive((l) => {
      const n = { ...l, [id]: on };
      writeLocal(key, JSON.stringify(n));
      return n;
    });
  return (
    <>
      {st.cameras.length > 0 && (
        <div className="cam-list">
          {st.cameras.map((c) => (
            <CamCard key={c.id} c={c} wanted={!!live[c.id]} setWanted={(on) => setWanted(c.id, on)} />
          ))}
        </div>
      )}
      {!st.cameras.length && !st.csi.length && <div className="pr-note">No camera connected. Plug in a USB webcam or HDMI capture stick.</div>}
      {st.csi.length > 0 && (
        <div className="pr-card">
          <h4>
            CSI cameras <small>libcamera</small>
          </h4>
          {st.csi.map((x) => (
            <div key={x.index} className="pr-note">
              #{x.index} {x.sensor} · {x.detail}
            </div>
          ))}
          <div className="pr-note">Listed only; streaming CSI cameras isn't supported yet.</div>
        </div>
      )}
    </>
  );
}

/** The live stream. Clearing src on removal closes the HTTP stream, so the server-side idle timer can stop the camera. */
function LiveImg({ src, onError }: { src: string; onError(): void }) {
  const ref = useRef<HTMLImageElement>(null);
  useEffect(() => {
    const img = ref.current;
    return () => {
      if (img) img.src = "";
    };
  }, []);
  return <img ref={ref} className="cam-live" alt="Live view" src={src} onError={onError} style={{ width: "100%", maxHeight: "70vh", objectFit: "contain", background: "#000", borderRadius: 4, display: "block" }} />;
}

function CamCard({ c, wanted, setWanted }: { c: CameraPublic; wanted: boolean; setWanted(on: boolean): void }) {
  const b = useBob();
  const el = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);
  const pageVisible = usePageVisible();
  const [streamT, setStreamT] = useState(() => Date.now());
  const [ended, setEnded] = useState(false);
  const [snap, setSnap] = useState<{ url: string; at: Date } | { error: string } | "loading" | null>(null);
  const [detail, setDetail] = useState<CameraDetail | null>(null);
  const [detOpen, setDetOpen] = useState(false);

  // Stop pulling frames while the card is scrolled out of view.
  useEffect(() => {
    const io = new IntersectionObserver((es) => setVisible(es[0].isIntersecting));
    if (el.current) io.observe(el.current);
    return () => io.disconnect();
  }, []);
  useEffect(
    () => () => {
      if (snap && typeof snap === "object" && "url" in snap) URL.revokeObjectURL(snap.url);
    },
    [snap],
  );

  const on = wanted && visible && pageVisible;
  const loadDetail = async () => {
    try {
      setDetail(await b.api<CameraDetail>("GET", camPaths.detail(c.id)));
    } catch {}
  };
  const restartView = () => setStreamT(Date.now());

  const snapshot = async () => {
    setSnap("loading");
    try {
      const r = await b.conn.fetch(camPaths.snapshot(c.id));
      if (!r.ok) throw new Error((await r.json().catch(() => ({}))).message || r.statusText);
      setSnap({ url: URL.createObjectURL(await r.blob()), at: new Date() });
    } catch (e: any) {
      setSnap({ error: e.message });
    }
  };

  return (
    <div className="pr-card" ref={el}>
      <div className="cam-hdr">
        <h4>
          {c.name} <span className="pr-pill">{c.kind === "hdmi" ? "HDMI capture" : "webcam"}</span>{" "}
          {c.streaming ? (
            <span className="pr-pill on">
              live · {c.viewers} viewer{c.viewers === 1 ? "" : "s"}
              {c.fps ? ` · ${c.fps} fps` : ""}
            </span>
          ) : (
            <span className="pr-pill">asleep</span>
          )}
          {c.streaming && c.signal === false && (
            <>
              {" "}
              <span className="pr-pill warn">no signal</span>
            </>
          )}
          {c.share && (
            <>
              {" "}
              <span className="pr-pill warn">shared :{c.sharePort}</span>
            </>
          )}
        </h4>
        <div className="pr-note">
          {c.dev} · USB {c.vid}:{c.pid} on port {c.usbPort}
          {c.serial ? ` · S/N ${c.serial}` : ""} · {fmtStream(c.stream)}
        </div>
        {c.error && (
          <div className="pr-note" style={{ color: "var(--danger)" }}>
            {c.error}
          </div>
        )}
      </div>
      <div className="pr-row">
        <button
          className="btn btn-sm btn-primary cam-livebtn"
          onClick={() => {
            setEnded(false);
            restartView();
            setWanted(!wanted);
          }}
        >
          {wanted ? "Stop live view" : "Live view"}
        </button>
        <button className="btn btn-sm" onClick={snapshot}>
          Snapshot
        </button>
        <a className="btn btn-sm" href={b.conn.url(camPaths.snapshot(c.id) + "?download=1")}>
          Download JPEG
        </a>
        {c.streaming && (
          <button className="btn btn-sm cam-stop" onClick={() => b.api("POST", camPaths.stop(c.id)).catch(() => {})}>
            Stop camera
          </button>
        )}
      </div>
      <div className="cam-view">
        {on && !ended ? (
          <LiveImg
            key={streamT}
            src={b.conn.url(`${camPaths.stream(c.id)}?t=${streamT}`)}
            onError={() => {
              setEnded(true);
              setWanted(false);
            }}
          />
        ) : ended ? (
          <div className="pr-note">Stream ended. Press Live view to retry.</div>
        ) : wanted ? (
          <div className="pr-note">Live view paused while off screen.</div>
        ) : null}
      </div>
      <div className="cam-snap">
        {snap === "loading" && <div className="pr-note">Capturing…</div>}
        {snap && typeof snap === "object" && "error" in snap && (
          <div className="pr-note" style={{ color: "var(--danger)" }}>
            {snap.error}
          </div>
        )}
        {snap && typeof snap === "object" && "url" in snap && (
          <>
            <div className="pr-row">
              <span className="pr-note">Snapshot {snap.at.toLocaleTimeString()}</span>
              <a className="btn btn-sm" href={snap.url} download={`${c.id}-${snap.at.toISOString().replace(/[:.]/g, "-")}.jpg`}>
                Download
              </a>
              <button className="btn btn-sm" onClick={() => setSnap(null)}>
                Close
              </button>
            </div>
            <img alt="Snapshot" src={snap.url} style={{ width: "100%", maxHeight: "50vh", objectFit: "contain", background: "#000", borderRadius: 4, display: "block" }} />
          </>
        )}
      </div>
      <details
        className="cam-det"
        open={detOpen}
        onToggle={(e) => {
          const o = (e.currentTarget as HTMLDetailsElement).open;
          setDetOpen(o);
          if (o) loadDetail();
        }}
      >
        <summary className="pr-note" style={{ cursor: "pointer", marginTop: 8 }}>
          Stream settings and image controls
        </summary>
        {detail && (
          <>
            <Settings
              id={c.id}
              d={detail}
              after={() => {
                // The old stream was cut by the restart; reopen it if the user was watching.
                restartView();
                loadDetail();
              }}
            />
            <div className="cam-ctls" style={{ marginTop: 8 }}>
              <Controls id={c.id} controls={detail.controls} setControls={(controls) => setDetail({ ...detail, controls })} />
            </div>
          </>
        )}
      </details>
    </div>
  );
}

const STREAMABLE = ["MJPG", "JPEG", "YUYV", "UYVY", "RGBP", "RGB3"];

function Settings({ id, d, after }: { id: string; d: CameraDetail; after(): void }) {
  const b = useBob();
  const fmts = d.formats.filter((f) => STREAMABLE.includes(f.fourcc));
  const cur = d.stream || ({} as Partial<StreamChoice>);
  // What the user is choosing, before Apply: re-filters sizes and rates as they go.
  const [pend, setPend] = useState<Partial<StreamChoice>>(cur);
  const [share, setShare] = useState(d.share);
  const portRef = useRef<HTMLInputElement>(null);
  useEffect(() => {
    setPend(d.stream || {});
    setShare(d.share);
  }, [d]);
  if (!fmts.length) return <div className="pr-note">This camera offers no format ustreamer can stream.</div>;
  const f = fmts.find((x) => x.fourcc === pend.fourcc) || fmts[0];
  const sizes = f.sizes;
  const size = sizes.find((s) => s.w === pend.w && s.h === pend.h) || sizes[0];
  const fpsList = size ? size.fps : [];
  const fps = fpsList.includes(pend.fps as number) ? pend.fps : fpsList[0] ?? cur.fps ?? 30;
  const sizeVal = size ? `${size.w}x${size.h}` : `${cur.w}x${cur.h}`;

  const apply = async () => {
    const [w, h] = sizeVal.split("x").map(Number);
    await b.api("POST", camPaths.config(id), { fourcc: f.fourcc, w, h, fps: Number(fps) }).catch(() => {});
    after();
  };
  const doShare = async (on: boolean) => {
    setShare(on);
    await b.api("POST", camPaths.config(id), { share: on, sharePort: Number(portRef.current?.value) }).catch(() => {});
    after();
  };
  return (
    <div className="cam-settings">
      <div className="pr-row">
        <label>
          Format{" "}
          <select value={f.fourcc} onChange={(e) => setPend({ ...pend, fourcc: e.currentTarget.value, w: 0, h: 0 })}>
            {fmts.map((x) => (
              <option key={x.fourcc} value={x.fourcc}>
                {x.fourcc} · {x.description}
              </option>
            ))}
          </select>
        </label>
        <label>
          Size{" "}
          <select
            value={sizeVal}
            onChange={(e) => {
              const [w, h] = e.currentTarget.value.split("x").map(Number);
              setPend({ ...pend, fourcc: f.fourcc, w, h });
            }}
          >
            {sizes.map((s) => (
              <option key={`${s.w}x${s.h}`} value={`${s.w}x${s.h}`}>
                {s.w}×{s.h}
              </option>
            ))}
            {!sizes.length && f.stepwise && <option value={sizeVal}>{sizeVal}</option>}
          </select>
        </label>
        <label>
          FPS{" "}
          <select value={String(fps)} onChange={(e) => setPend({ ...pend, fourcc: f.fourcc, w: size?.w, h: size?.h, fps: Number(e.currentTarget.value) })}>
            {fpsList.map((v) => (
              <option key={v} value={v}>
                {v}
              </option>
            ))}
            {!fpsList.length && <option value={String(fps)}>{fps}</option>}
          </select>
        </label>
        <button className="btn btn-sm" onClick={apply}>
          Apply
        </button>
      </div>
      <div className="pr-note">{!f.compressed ? "Uncompressed: the Pi JPEG-encodes every frame, so keep it modest." : "MJPEG is forwarded as the camera sends it."}</div>
      <div className="pr-row">
        <label>
          <input type="checkbox" checked={share} onChange={(e) => doShare(e.currentTarget.checked)} /> Share directly on port
        </label>
        {/* Sent when the edit is committed (blur or Enter), like the old change event, not per keystroke. */}
        <input
          type="number"
          ref={portRef}
          min={1024}
          max={65535}
          defaultValue={d.sharePort}
          key={d.sharePort}
          style={{ width: "6em" }}
          onBlur={(e) => Number(e.currentTarget.value) !== d.sharePort && doShare(share)}
          onKeyDown={(e) => e.key === "Enter" && e.currentTarget.blur()}
        />
      </div>
      <div className="pr-note">
        {d.share ? (
          <>
            ustreamer listens on all interfaces: <span className="pr-endpoint">http://{b.conn.hostname}:{d.sharePort}/stream</span> (also /snapshot, /state). It stays running until you turn this off.
          </>
        ) : (
          "Off: the camera is reachable only through BenchBob and sleeps when nobody watches."
        )}
      </div>
    </div>
  );
}

/** A slider that sends its value when released (the native change event), showing it live while dragged. */
function Range({ c, send }: { c: CameraControl; send(v: number): void }) {
  const ref = useRef<HTMLInputElement>(null);
  const [v, setV] = useState(c.value);
  const sendRef = useRef(send);
  sendRef.current = send;
  useEffect(() => setV(c.value), [c.value]);
  useEffect(() => {
    const el = ref.current!;
    const f = () => sendRef.current(Number(el.value));
    el.addEventListener("change", f);
    return () => el.removeEventListener("change", f);
  }, []);
  return (
    <>
      <input ref={ref} type="range" min={c.min} max={c.max} step={c.step || 1} value={v} style={{ flex: 1, minWidth: 120 }} onInput={(e) => setV(Number(e.currentTarget.value))} />
      <span style={{ minWidth: "3.5em", textAlign: "right", fontVariantNumeric: "tabular-nums" }}>{v}</span>
    </>
  );
}

function Controls({ id, controls, setControls }: { id: string; controls: CameraControl[]; setControls(c: CameraControl[]): void }) {
  const b = useBob();
  if (!controls.length) return <div className="pr-note">No adjustable controls.</div>;
  const set = async (name: string, value: number) => {
    try {
      const r = await b.api<{ controls: CameraControl[] }>("POST", camPaths.controls(id), { [name]: value });
      setControls(r.controls);
    } catch {}
  };
  return (
    <>
      {controls.map((c) => {
        const inactive = c.flags.includes("inactive");
        let input;
        if (c.type === "bool") input = <input type="checkbox" checked={!!c.value} onChange={(e) => set(c.name, e.currentTarget.checked ? 1 : 0)} />;
        else if (c.menu)
          input = (
            <select value={String(c.value)} onChange={(e) => set(c.name, Number(e.currentTarget.value))}>
              {c.menu.map((m) => (
                <option key={m.value} value={m.value}>
                  {m.label}
                </option>
              ))}
            </select>
          );
        else input = <Range c={c} send={(v) => set(c.name, v)} />;
        return (
          <div key={c.name} className="pr-row" style={{ opacity: inactive ? 0.5 : undefined, margin: "4px 0" }}>
            <span style={{ minWidth: "11em", fontSize: ".8rem" }}>
              {c.name.replace(/_/g, " ")}
              {inactive && <span className="pr-note"> (set by its auto mode)</span>}
            </span>
            {input}
            <span className="pr-note">default {c.default ?? "—"}</span>
          </div>
        );
      })}
      <div className="pr-row">
        <button
          className="btn btn-sm"
          onClick={async () => {
            try {
              const r = await b.api<{ controls: CameraControl[] }>("POST", camPaths.controlsReset(id));
              setControls(r.controls);
            } catch {}
          }}
        >
          Reset to defaults
        </button>
        <span className="pr-note">Your values are saved and re-applied when the camera is plugged in again.</span>
      </div>
    </>
  );
}
