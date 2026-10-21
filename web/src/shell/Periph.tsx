// Peripherals: one section per driver, shown while its driver reports `active` (a device
// present, something running) or after the user opens it from the Tools strip. Idle
// sections fold away into that strip. A panel's code is only loaded once it is shown.

import { Suspense, useState } from "react";
import { ActionLine, useAction } from "../ui/status";
import { useBob, useSnapshot } from "../bob/context";
import type { Registry } from "../registry";
import type { PeriphEntry, Status } from "../lib/types";
import { ErrorBoundary } from "./ErrorBoundary";

export function PeriphHost({ registry, status, open }: { registry: Registry; status: Status; open: Set<string> }) {
  const { hidePanel, showPanel } = useBob();
  const P = status.peripherals;
  if (!P) return null;
  const shown: PeriphEntry[] = [];
  const idle: PeriphEntry[] = [];
  for (const d of P.list) {
    const def = registry.panels[d.id];
    const st = P.state[d.id];
    if (!def || d.disabled || !st) continue;
    (d.active || open.has(d.id) ? shown : idle).push(d);
  }
  return (
    <>
      {shown.map((d) => {
        const Panel = registry.panels[d.id];
        return (
          <div className="section pr-sec" key={d.id} data-panel={d.id}>
            <div className="section-title">
              <span>{d.title}</span>
              <span className="pr-sum">{d.summary || ""}</span>
              {/* Only idle sections the user opened can be folded; an active one would just come back. */}
              <button className="pr-x" aria-label="Fold into Tools" style={{ visibility: d.active ? "hidden" : "visible" }} onClick={() => hidePanel(d.id)}>
                &times;
              </button>
            </div>
            <div className="pr-body">
              <ErrorBoundary name={d.id}>
                <Suspense fallback={<div className="pr-note">Loading…</div>}>
                  <Panel st={P.state[d.id]} entry={d} status={status} />
                </Suspense>
              </ErrorBoundary>
            </div>
          </div>
        );
      })}
      <div className="section">
        <div className="pr-tools">
          <span className="pr-lbl">Tools</span>
          {idle.map((d) => (
            <button key={d.id} className="pr-tool" onClick={() => showPanel(d.id)}>
              {d.title}
              {d.summary && <small>{d.summary}</small>}
            </button>
          ))}
          <PeriphSettingsButton />
        </div>
      </div>
    </>
  );
}

function PeriphSettingsButton() {
  const { openModal } = useBob();
  return (
    <button className="btn btn-sm pr-gear" onClick={() => openModal(<PeriphSettings />)}>
      Peripherals…
    </button>
  );
}

function PeriphSettings() {
  const { api, closeModal } = useBob();
  const list = useSnapshot().status?.peripherals?.list || [];
  // The last switch's outcome, under the one it was.
  const act = useAction();
  const [last, setLast] = useState("");
  return (
    <>
      <h3>Peripherals</h3>
      <p className="pr-note">Turn off anything you don't use. A disabled peripheral releases its devices, stops its bridges and captures, and disappears from the dashboard, the API and MCP.</p>
      <div style={{ display: "flex", flexDirection: "column", gap: 8, margin: "12px 0" }}>
        {list.map((d) => (
          <label key={d.id} style={{ display: "flex", gap: 8, alignItems: "flex-start", fontSize: ".85rem" }}>
            <input
              type="checkbox"
              checked={!d.disabled}
              disabled={act.busy}
              onChange={(e) => {
                const on = e.currentTarget.checked;
                setLast(d.id);
                act.run(() => api("POST", "/api/periph/driver", { id: d.id, disabled: !on }), { busy: on ? "Turning on…" : "Turning off…" });
              }}
            />
            <span>
              <b>{d.title}</b>
              {d.description && (
                <>
                  <br />
                  <span className="pr-note">{d.description}</span>
                </>
              )}
              {last === d.id && act.status && (
                <>
                  <br />
                  <ActionLine a={act} />
                </>
              )}
            </span>
          </label>
        ))}
      </div>
      <div className="modal-btns">
        <button className="btn" onClick={closeModal}>
          Done
        </button>
      </div>
    </>
  );
}
