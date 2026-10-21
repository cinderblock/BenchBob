import { useState } from "react";
import { useBob, useSnapshot } from "../bob/context";
import { readLocalStr, writeLocal } from "../lib/storage";
import type { Power } from "../lib/types";
import { ActionLine, useAction } from "../ui/status";

// Under-voltage on this Pi browns out every USB drive (and any Compute Module) hanging off
// its ports, so it gets a banner rather than a footnote.
export function PowerBanner({ p }: { p: Power | null | undefined }) {
  if (!p || !(p.now.length || p.sinceBoot.length)) return null;
  const live = p.now.length > 0;
  const ago = (t: number) => {
    const m = Math.round((Date.now() - t) / 60000);
    return m < 1 ? "just now" : m < 120 ? `${m} min ago` : `${Math.round(m / 60)} h ago`;
  };
  const sub: string[] = [];
  if (p.undervoltageEvents) sub.push(`${p.undervoltageEvents} under-voltage event${p.undervoltageEvents === 1 ? "" : "s"} since boot, last ${ago(p.lastUndervoltage!)}`);
  if (p.inputVolts) sub.push(`input ${p.inputVolts.toFixed(2)} V`);
  sub.push(`get_throttled=${p.raw}`);
  return (
    <div className={`power-banner ${live ? "now" : "past"}`}>
      <span className="pb-icon">⚡</span>
      <div>
        {live ? (
          <>
            <strong>Host power problem now:</strong> {p.now.join(", ")}. The supply to this Pi is sagging, so USB drives and flashes may fail or corrupt.
          </>
        ) : (
          <>
            <strong>Host power problems since boot:</strong> {p.sinceBoot.join(", ")}. Check the power supply and cable. Connected USB drives draw from the same supply.
          </>
        )}
        <div className="pb-sub">{sub.join(" · ")}</div>
      </div>
    </div>
  );
}

// A newer release on GitHub. Dismissing hides it until the next release.
export function UpdateBanner() {
  const { openModal, closeModal, api, key } = useBob();
  const u = useSnapshot().status?.update;
  const [dismissed, setDismissed] = useState(() => readLocalStr(key("bb-update-dismissed")));
  // The install request's progress and the server's answer, shown in the banner.
  const act = useAction();
  if (!u?.available || dismissed === u.latest) return null;
  const install = () =>
    openModal(
      <>
        <h3>Update BenchBob to {u.latest}</h3>
        <p>apt installs the new release, then BenchBob restarts. The dashboard, bridges and captures drop for a few seconds; this page reloads itself when it's back. USB drives and shares stay as they are.</p>
        <div className="modal-btns">
          <button className="btn" onClick={closeModal}>
            Cancel
          </button>
          <button
            className="btn btn-primary"
            onClick={() => {
              closeModal();
              act.run(() => api<{ message: string }>("POST", "/api/update/install", { confirm: true }), { busy: "Starting the update…", ok: (r) => r.message || "Update started" });
            }}
          >
            Update now
          </button>
        </div>
      </>,
    );
  return (
    <div className="power-banner update">
      <span className="pb-icon">⬆</span>
      <div>
        <strong>BenchBob {u.latest} is available.</strong> This is {u.current}.
        {u.url && (
          <>
            {" "}
            <a href={u.url} target="_blank" rel="noopener">
              Release notes
            </a>
          </>
        )}
        {act.status && (
          <div className="pb-sub">
            <ActionLine a={act} />
          </div>
        )}
      </div>
      <div className="ub-btns">
        <button
          className="btn btn-sm"
          onClick={() => {
            writeLocal(key("bb-update-dismissed"), u.latest);
            setDismissed(u.latest ?? null);
          }}
        >
          Later
        </button>
        <button className="btn btn-sm btn-primary" disabled={act.busy} onClick={install}>
          Update…
        </button>
      </div>
    </div>
  );
}

/** The bob's version changed under a page it didn't serve: say so instead of reloading. */
export function VersionNotice() {
  const { store, conn } = useBob();
  const v = useSnapshot().versionNotice;
  if (!v) return null;
  return (
    <div className="power-banner update">
      <span className="pb-icon">↻</span>
      <div>
        <strong>This bob's backend changed version</strong> ({v.from.slice(0, 8)} → {v.to.slice(0, 8)}).
        <div className="pb-sub">This page isn't served by {conn.host}, so it doesn't reload itself. If something looks wrong, reload it or update the UI.</div>
      </div>
      <div className="ub-btns">
        <button className="btn btn-sm" onClick={() => store.dismissVersionNotice()}>
          Dismiss
        </button>
        <button className="btn btn-sm btn-primary" onClick={() => location.reload()}>
          Reload page
        </button>
      </div>
    </div>
  );
}
