import { useBob } from "../../bob/context";
import type { Operation } from "../../lib/types";
import { ActionLine, useAction } from "../../ui/status";

/** A running or finished operation on a drive or USB-boot device, with Cancel while active. */
export function OpProgress({ op, label, detail }: { op: Operation; label: string; detail?: string }) {
  const b = useBob();
  const cancel = useAction();
  const cls = op.status === "done" ? "done" : op.status === "error" ? "error" : "active";
  return (
    <div className="drive-op">
      <div className="op-row">
        <span>
          <span className="op-type">{label}</span>
          {detail ? " · " + detail : ""}
        </span>
        {cls === "active" && (
          <span>
            <ActionLine a={cancel} />{" "}
            <button className="btn btn-sm btn-danger" disabled={cancel.busy} onClick={() => cancel.run(() => b.api("POST", "/api/cancel", { operationId: op.id }), { busy: "Cancelling…" })}>
              Cancel
            </button>
          </span>
        )}
      </div>
      <div className="op-msg">{op.message}</div>
      <div className="op-bar">
        <div className={`op-fill ${cls}`} style={{ width: `${op.progress}%` }} />
      </div>
    </div>
  );
}
