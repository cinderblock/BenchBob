import { useBob } from "../../bob/context";
import type { Operation } from "../../lib/types";

/** A running or finished operation on a drive or USB-boot device, with Cancel while active. */
export function OpProgress({ op, label, detail }: { op: Operation; label: string; detail?: string }) {
  const { conn } = useBob();
  const cls = op.status === "done" ? "done" : op.status === "error" ? "error" : "active";
  return (
    <div className="drive-op">
      <div className="op-row">
        <span>
          <span className="op-type">{label}</span>
          {detail ? " · " + detail : ""}
        </span>
        {cls === "active" && (
          <button className="btn btn-sm btn-danger" onClick={() => conn.json("POST", "/api/cancel", { operationId: op.id }).catch(() => {})}>
            Cancel
          </button>
        )}
      </div>
      <div className="op-msg">{op.message}</div>
      <div className="op-bar">
        <div className={`op-fill ${cls}`} style={{ width: `${op.progress}%` }} />
      </div>
    </div>
  );
}
