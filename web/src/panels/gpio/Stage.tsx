// The config.txt / boot overlay flow shared by the interfaces and overlays dialogs:
// stage → review the diff → tick → apply (backup first) → offer a reboot.

import { useState } from "react";
import { useBob, type BobApi } from "../../bob/context";
import { Diff } from "../../ui/common";
import { ActionLine, StatusLine, useAction } from "../../ui/status";
import type { Applied, Staged } from "./api";

export function StagedChange({ r, applyPath, onApplied }: { r: Staged; applyPath: string; onApplied(): void }) {
  const b = useBob();
  const [ok, setOk] = useState(false);
  const [done, setDone] = useState<Applied | null>(null);
  const apply = useAction();
  if (done)
    return (
      <div className="pr-card">
        <h4>Saved</h4>
        <div className="pr-note">
          Backup: <span className="pr-endpoint">{done.backup}</span>
        </div>
        <div className="pr-note" style={{ color: "var(--warning)" }}>
          Reboot required for {done.summary.join(", ")}.
        </div>
        <div className="pr-row">
          <button className="btn btn-warn" onClick={() => openReboot(b)}>
            Reboot now…
          </button>
        </div>
      </div>
    );
  return (
    <div className="pr-card">
      <h4>
        Proposed change<small>{r.summary.join(", ")}</small>
      </h4>
      <Diff text={r.diff || ""} />
      {r.warnings.map((w) => (
        <div key={w} className="pr-note" style={{ color: "var(--warning)" }}>
          {w}
        </div>
      ))}
      <label className="pr-note" style={{ display: "flex", gap: 6, alignItems: "center", marginTop: 6 }}>
        <input type="checkbox" checked={ok} onChange={(e) => setOk(e.currentTarget.checked)} /> Write this to {r.configPath} (a timestamped backup is made first)
      </label>
      <div className="pr-row">
        <button
          className="btn btn-warn"
          disabled={!ok || apply.busy}
          onClick={async () => {
            const res = await apply.run(() => b.api<Applied>("POST", applyPath, { token: r.token, confirm: true }), { busy: "Writing…" });
            if (!res) return;
            setDone(res);
            onApplied();
          }}
        >
          Apply
        </button>
        <ActionLine a={apply} />
      </div>
    </div>
  );
}

export function openReboot(b: BobApi) {
  b.openModal(<RebootDialog />);
}

// After a successful request the dialog stays up and says the bob is rebooting, until closed.
function RebootDialog() {
  const b = useBob();
  const [t, setT] = useState("");
  const a = useAction();
  const sent = a.status?.state === "ok";
  return (
    <>
      <h3 style={{ color: "var(--warning)" }}>Reboot BenchBob</h3>
      <p>Everything served from this Pi (drives, bridges, this dashboard) goes away for about a minute.</p>
      {!sent && (
        <div className="confirm-box">
          <label htmlFor="gi-rb">Type REBOOT to confirm:</label>
          <input type="text" id="gi-rb" autoComplete="off" value={t} onInput={(e) => setT(e.currentTarget.value)} />
        </div>
      )}
      <div className="modal-btns">
        <StatusLine s={a.status} onClear={sent ? undefined : a.clear} time={false} />
        {sent ? (
          <button className="btn btn-primary" onClick={b.closeModal}>
            Close
          </button>
        ) : (
          <>
            <button className="btn" onClick={b.closeModal}>
              Cancel
            </button>
            <button className="btn btn-danger" disabled={t !== "REBOOT" || a.busy} onClick={() => a.run(() => b.api("POST", "/api/gpio/reboot", { confirm: "REBOOT" }), { busy: "Asking to reboot…", ok: "Rebooting: back in about a minute." })}>
              Reboot
            </button>
          </>
        )}
      </div>
    </>
  );
}
