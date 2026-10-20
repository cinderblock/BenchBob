// Connected drives: one card per USB disk with its partitions (OS detection, NFS/NBD
// badges, the file browser), running and queued operations, and the actions. The flash
// picker and the bootloader editors are separate chunks, loaded on first use.

import { useEffect, useMemo, useRef, useSyncExternalStore } from "react";
import { useBob, useSnapshot, type BobApi } from "../../bob/context";
import { fmtRate, fmtSz } from "../../lib/format";
import type { SectionProps } from "../../registry";
import type { Drive } from "../../lib/types";
import { Bars } from "../../ui/spark";
import { Hint } from "../../ui/common";
import { filesRel, OsInfoCache, partOsLabel, type ProbeResult } from "./api";
import { EraseDialog, ProbeDialog, ScanResultDialog } from "./Dialogs";
import { FileBrowser } from "./FileBrowser";
import { OpProgress } from "./OpProgress";

const DRIVE_OP_NAMES: Record<string, string> = { blcard: "Bootloader card" };
const OS_ICONS: Record<string, string> = { windows: "🪟", linux: "🐧", pi: "🍓", macos: "🍎" };

export default function Drives({ status: s }: SectionProps) {
  const b = useBob();
  useSnapshot();
  const os = useMemo(() => new OsInfoCache(b.conn), [b.conn]);
  useSyncExternalStore(os.subscribe, os.version);
  for (const d of s.drives) os.ensure(d);

  // Deep scan results arrive on the op; show each one once, when it completes.
  const shown = useRef(new Set<string>());
  useEffect(() => {
    for (const d of s.drives)
      for (const o of d.operations || [])
        if (o.type === "deepscan" && o.status === "done" && o.scanResult && !shown.current.has(o.id)) {
          shown.current.add(o.id);
          b.openModal(<ScanResultDialog dev={d.name} res={o.scanResult} />);
        }
  }, [s]);

  let drvMax = 0;
  for (const [k, v] of Object.entries(b.store.hist)) if (k.startsWith("drv:")) for (const x of v) if (x > drvMax) drvMax = x;
  drvMax ||= 1;

  return (
    <div className="section">
      <div className="section-title">Connected Drives</div>
      <div>
        {!s.drives.length ? (
          <div className="empty">
            No USB storage devices connected<div className="empty-sub">Plug in a USB drive to get started</div>
          </div>
        ) : (
          s.drives.map((d) => <DriveCard key={d.name} d={d} os={os} drvMax={drvMax} nbd={s.nbdExports} filesRoot={s.filesRoot} />)
        )}
      </div>
    </div>
  );
}

function DriveCard({ d, os, drvMax, nbd, filesRoot }: { d: Drive; os: OsInfoCache; drvMax: number; nbd: string[]; filesRoot?: string }) {
  const b = useBob();
  const h = b.store.hist;
  const ops = d.operations || [];
  const activeOp = ops.find((o) => !["done", "error", "queued"].includes(o.status));
  const queued = ops.filter((o) => o.status === "queued");
  const busy = !!activeOp;
  const hasParts = d.hasPartitions;
  const model = d.model || d.name;
  const info = os.get(d.name);
  const isErasing = activeOp?.type === "erase";

  return (
    <div className="drive-card" data-drive={d.name}>
      <div className="drive-hdr">
        <div className="drive-info">
          <span className="drive-model">{model}</span>
          <span className="drive-meta">
            /dev/{d.name} · {fmtSz(d.size)}
            {d.serial ? " · S/N: " + d.serial : ""}
            {d.rev ? " · FW: " + d.rev : ""}
            {d.usbVersion ? " · " + d.usbVersion : ""}
          </span>
          {info && <DriveOs info={info} />}
        </div>
        <div className="drive-bw" style={{ display: "flex", gap: 10, alignItems: "center" }}>
          <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
            <div style={{ flexShrink: 0, width: 50 }}>
              <Bars data={h[`drv:${d.name}:r`]} w={50} h={16} color="#6cb4ee" max={drvMax} log />
            </div>
            <span className="bw-rx" style={{ fontSize: ".68rem", minWidth: 62, textAlign: "right" }}>
              R {fmtRate(d.readRate)}
            </span>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
            <div style={{ flexShrink: 0, width: 50 }}>
              <Bars data={h[`drv:${d.name}:w`]} w={50} h={16} color="#ee8a6c" max={drvMax} log />
            </div>
            <span className="bw-tx" style={{ fontSize: ".68rem", minWidth: 62, textAlign: "right" }}>
              W {fmtRate(d.writeRate)}
            </span>
          </div>
        </div>
      </div>

      {hasParts && d.children ? (
        <div className="parts">
          {d.children.map((p) => {
            const rel = filesRel(filesRoot, p.mountpoint);
            const pi = os.part(d.name, p.name);
            const osTag = pi ? partOsLabel(pi) : null;
            return (
              <div className="part" key={p.name}>
                <div className="part-left">
                  <span className="part-name">{p.name}</span>
                  <span className="part-info">
                    {fmtSz(p.size)} · {p.fstype || "?"}
                    {p.label ? " · " + p.label : ""}
                    {p.mountpoint ? " → " + p.mountpoint : ""}
                    {pi?.note ? " · " + pi.note : ""}
                  </span>
                </div>
                <div className="badges" style={{ alignItems: "center" }}>
                  {osTag && <span className="badge badge-os">{osTag}</span>}
                  {pi?.role && <span className="badge badge-role">{pi.role}</span>}
                  {p.mountpoint?.startsWith("/srv/nfs/") && <span className="badge badge-nfs">NFS</span>}
                  {nbd.includes(p.name) && <span className="badge badge-nbd">NBD</span>}
                  {rel !== null && (
                    <button className="btn btn-sm" onClick={() => b.openModal(<FileBrowser start={rel} os={os} />, { cls: "wide" })}>
                      Browse
                    </button>
                  )}
                </div>
              </div>
            );
          })}
        </div>
      ) : !hasParts && !busy ? (
        <div className="no-parts">No partition table detected</div>
      ) : null}

      {ops
        .filter((o) => o.status !== "queued")
        .map((op) => (
          <OpProgress key={op.id} op={op} label={DRIVE_OP_NAMES[op.type] || op.type} detail={op.imageName} />
        ))}
      {queued.map((op) => (
        <div key={op.id} className="op-queued">
          ⏳ {DRIVE_OP_NAMES[op.type] || op.type} queued{op.imageName ? ": " + op.imageName : ""}
        </div>
      ))}

      <div className="drive-actions">
        {hasParts && !busy && (
          <button className="btn btn-primary" onClick={() => b.toast("Backup coming soon", "info")}>
            Backup
          </button>
        )}
        {!hasParts && !busy && (
          <>
            <button className="btn btn-primary" onClick={() => flash(b, d.name)}>
              Flash
            </button>
            <button className="btn" onClick={() => quickPi(b, d.name)}>
              ⚡ Quick Pi
            </button>
            <button className="btn" onClick={() => scan(b, d.name)}>
              Scan
            </button>
          </>
        )}
        {/* Optimistic: allow queueing a flash during an erase. */}
        {isErasing && !queued.length && (
          <>
            <button className="btn btn-primary" onClick={() => flash(b, d.name)}>
              Flash (queue)
            </button>
            <button className="btn" onClick={() => quickPi(b, d.name)}>
              ⚡ Quick Pi (queue)
            </button>
          </>
        )}
        {/* Raw-sector inspection: useful whether or not a filesystem claims to be empty. */}
        {!busy && (
          <>
            <button className="btn" onClick={() => probe(b, d.name)}>
              Probe
            </button>
            <button className="btn" onClick={() => deepScan(b, d.name, model, d.size)}>
              Deep Scan
            </button>
            <button className="btn btn-warn" onClick={() => erase(b, d.name, model, d.size)}>
              Erase
            </button>
            <button className="btn" onClick={() => blCard(b, d)}>
              Bootloader card…
            </button>
          </>
        )}
        <button className="btn" disabled={busy} onClick={() => eject(b, d.name, model)}>
          Eject
        </button>
        <Hint label="What do these do?">
          {!hasParts && (
            <>
              <b>Quick Pi</b> flashes an image from the Quick Pi list with the saved defaults and asks only for a hostname.{" "}
            </>
          )}
          <b>Probe</b> samples raw sectors across the device (fast). <b>Deep Scan</b> reads the whole device looking for file signatures (read-only, as long as a full read). <b>Bootloader card</b> turns the card into a Raspberry Pi bootloader update card.
          {hasParts && filesRoot && (
            <>
              {" "}
              <b>Browse</b> shows a partition's files, read-only.
            </>
          )}
        </Hint>
      </div>
    </div>
  );
}

function DriveOs({ info }: { info: NonNullable<ReturnType<OsInfoCache["get"]>> }) {
  if (info.systems.length)
    return (
      <div className="drive-os">
        {info.systems.map((s, i) => (
          <div key={i}>
            <span>{OS_ICONS[s.kind] || "💽"}</span> <b>{s.name}</b> <span className="sub">{[s.partition, s.detail].filter(Boolean).join(" · ")}</span>
          </div>
        ))}
      </div>
    );
  const loaders = info.partitions.flatMap((p) => p.esp?.loaders || []).filter((l) => l.kind === "os");
  if (loaders.length)
    return (
      <div className="drive-os">
        <div className="sub">No readable OS · EFI loaders for {loaders.map((l) => l.name).join(", ")}</div>
      </div>
    );
  return null;
}

// ---------- actions ----------

async function flash(b: BobApi, dev: string) {
  (await import("../flash/Flash")).flashDrive(b, dev);
}
async function quickPi(b: BobApi, dev: string) {
  (await import("../flash/Flash")).quickPiDrive(b, dev);
}

async function post(b: BobApi, url: string, body: unknown, okKind?: "success" | "info", fail = "Request failed") {
  try {
    const r = await b.conn.json<{ message?: string }>("POST", url, body);
    if (okKind && r.message) b.toast(r.message, okKind);
    return r;
  } catch (e: any) {
    b.toast(e?.message || fail, "error");
    return null;
  }
}

async function eject(b: BobApi, dev: string, model: string) {
  if (!(await b.confirm("Eject Device", "Unmount and power off this device?", `${model}\n/dev/${dev}`))) return;
  await post(b, "/api/eject", { device: dev }, "success", "Eject failed");
}

function erase(b: BobApi, dev: string, model: string, size: number) {
  b.openModal(
    <EraseDialog
      dev={dev}
      model={model}
      size={size}
      onDone={async (mode) => {
        b.closeModal();
        if (mode) await post(b, "/api/erase", { device: dev, mode }, undefined, "Erase failed");
      }}
    />,
  );
}

async function scan(b: BobApi, dev: string) {
  b.toast("Scanning...", "info");
  await post(b, "/api/scan", { device: dev }, "success", "Scan failed");
}

async function probe(b: BobApi, dev: string) {
  b.toast("Sampling raw sectors...", "info");
  const r = (await post(b, "/api/probe", { device: dev }, undefined, "Probe failed")) as ProbeResult | null;
  if (!r) return;
  b.openModal(
    <ProbeDialog
      r={r}
      onDeepScan={() => {
        b.closeModal();
        deepScan(b, r.device, r.device, r.sizeBytes);
      }}
    />,
  );
}

async function deepScan(b: BobApi, dev: string, model: string, size: number) {
  if (!(await b.confirm("Deep Scan", "Read the entire device looking for file signatures? Read-only, but it takes roughly as long as a full read.", `${model}\n/dev/${dev} · ${fmtSz(size)}`))) return;
  await post(b, "/api/deepscan", { device: dev }, "info", "Deep scan failed");
}

async function blCard(b: BobApi, d: Drive) {
  const { BlCardDialog } = await import("../bootloader/Dialogs");
  b.openModal(<BlCardDialog dev={d.name} model={d.model || d.name} size={d.size} hasParts={!!d.hasPartitions} />, { cls: "wide" });
}
