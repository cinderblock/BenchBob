// USB Boot Mode (Compute Modules). A CM with nRPIBOOT fitted shows up as a bare boot ROM.
// From here it can be turned into a USB disk (mass-storage gadget) or have its bootloader
// EEPROM read or rewritten.

import { useBob } from "../../bob/context";
import type { SectionProps } from "../../registry";
import type { BootDevice, Operation, RpibootResult } from "../../lib/types";
import { ActionLine, useAction } from "../../ui/status";
import { OpProgress } from "../drives/OpProgress";

const RPIBOOT_OP_NAMES: Record<string, string> = { msd: "Mass storage", "eeprom-check": "Bootloader info", "eeprom-write": "Bootloader write" };

export default function Boot({ status: s }: SectionProps) {
  const devs = s.bootDevices || [];
  const ops = (s.operations || []).filter((o) => o.device.startsWith("usb:"));
  const results = s.rpibootResults || [];
  if (!devs.length && !ops.length && !results.length) return null;

  const paths = [...new Set([...devs.map((d) => d.path), ...ops.map((o) => o.device.slice(4)), ...results.map((r) => r.path)])].sort();
  return (
    <div className="section">
      <div className="section-title">USB Boot Mode</div>
      <div>
        {!s.rpibootInstalled && (
          <div className="no-parts" style={{ padding: "12px 18px" }}>
            rpiboot is not installed on this host. Run <span className="mono">sudo apt install rpiboot</span>.
          </div>
        )}
        {paths.map((path) => (
          <BootCard key={path} path={path} d={devs.find((x) => x.path === path)} ops={ops.filter((o) => o.device === "usb:" + path)} r={results.find((x) => x.path === path)} installed={!!s.rpibootInstalled} />
        ))}
      </div>
    </div>
  );
}

/** One module (or where one was): its operations, last result and actions. */
function BootCard({ path, d, ops, r, installed }: { path: string; d?: BootDevice; ops: Operation[]; r?: RpibootResult; installed: boolean }) {
  const b = useBob();
  // The last action's request: starting it, or why the server refused. Once started, the
  // job shows as its operation's progress bar.
  const act = useAction();
  const busy = ops.some((o) => !["done", "error", "queued"].includes(o.status)) || act.busy;
  const eeprom = d && d.chip !== "2835";
  const post = (url: string, busyText: string) => act.run(() => b.api("POST", url, { path }), { busy: busyText });
  return (
    <div className="drive-card">
      <div className="drive-hdr">
        <div className="drive-info">
          <span className="drive-model">{d ? d.label : "Left USB boot mode"}</span>
          <span className="drive-meta">
            USB port {path}
            {d?.serial ? " · S/N: " + d.serial : ""}
            {d?.product ? " · " + d.product : ""}
          </span>
        </div>
      </div>
      {ops.map((op) => (
        <OpProgress key={op.id} op={op} label={RPIBOOT_OP_NAMES[op.type] || op.type} />
      ))}
      {r && <BootResult r={r} onDismiss={() => post("/api/rpiboot/results/clear", "Dismissing…")} />}
      {d && installed ? (
        <>
          <div className="drive-actions">
            <button className="btn btn-primary" disabled={busy} onClick={() => post("/api/rpiboot/msd", "Starting rpiboot…")}>
              Load mass-storage gadget
            </button>
            {eeprom && (
              <>
                <button className="btn" disabled={busy} onClick={() => post("/api/rpiboot/eeprom/check", "Starting rpiboot…")}>
                  Bootloader info
                </button>
                <button
                  className="btn btn-warn"
                  disabled={busy}
                  onClick={async () => {
                    const m = await act.run(() => import("../bootloader/Dialogs"), { busy: "Loading…" });
                    if (m) m.openRpibootWrite(b, act, path);
                  }}
                >
                  Bootloader settings &amp; update…
                </button>
              </>
            )}
            <ActionLine a={act} />
          </div>
          <div className="img-detail" style={{ marginTop: 6 }}>
            Mass storage exposes the module's eMMC/NVMe here as a drive to flash, browse or erase.
            {eeprom ? " Bootloader info reads the board, MAC and installed bootloader without writing anything." : ""}
          </div>
        </>
      ) : (
        act.status && (
          <div className="drive-actions">
            <ActionLine a={act} />
          </div>
        )
      )}
    </div>
  );
}

function BootResult({ r, onDismiss }: { r: RpibootResult; onDismiss(): void }) {
  const m = r.metadata || {};
  const rows: [string, string][] = [];
  if (r.installed) rows.push(["Bootloader", r.installed.label + (r.installed.bootConf ? " · settings known" : "")]);
  if (r.installed?.lastWrite) rows.push(["Last BenchBob write", `${r.installed.lastWrite.imageLabel}, ${new Date(r.installed.lastWrite.at).toLocaleString()} (replaced since)`]);
  if (r.board) rows.push(["Board", `${r.board.model} · ${r.board.memory} · rev ${r.board.revision}`]);
  if (m.USER_BOARDREV) rows.push(["Board revision", m.USER_BOARDREV]);
  if (m.MAC_ADDR) rows.push(["MAC", m.MAC_ADDR]);
  if (m.CUSTOMER_KEY_HASH) rows.push(["Secure boot", /^0*$/.test(m.CUSTOMER_KEY_HASH) ? "not provisioned" : "customer key programmed: " + m.CUSTOMER_KEY_HASH.slice(0, 16) + "…"]);
  if (m.JTAG_LOCKED) rows.push(["JTAG", m.JTAG_LOCKED === "1" ? "locked" : "unlocked"]);
  if (m.EEPROM_UPDATE) rows.push(["EEPROM update", m.EEPROM_UPDATE]);
  if (m.EEPROM_HASH) rows.push(["EEPROM hash", m.EEPROM_HASH]);
  for (const [k, v] of Object.entries(m)) if (!["USER_BOARDREV", "MAC_ADDR", "CUSTOMER_KEY_HASH", "JTAG_LOCKED", "EEPROM_UPDATE", "EEPROM_HASH"].includes(k)) rows.push([k, v]);
  return (
    <div className="drive-op">
      <div className="op-row">
        <span>
          <span className="op-type">{RPIBOOT_OP_NAMES[r.kind]}</span> · {new Date(r.at).toLocaleTimeString()}
        </span>
        <button className="btn btn-sm" onClick={onDismiss}>
          Dismiss
        </button>
      </div>
      <div className="op-msg" style={{ color: r.ok ? "var(--success)" : "var(--danger)" }}>
        {r.message}
      </div>
      {rows.length > 0 && (
        <table className="net-tbl mono" style={{ marginTop: 6, fontSize: ".72rem" }}>
          <tbody>
            {rows.map(([k, v]) => (
              <tr key={k}>
                <td style={{ color: "var(--dim)", paddingRight: 12, whiteSpace: "nowrap", width: "1%" }}>{k}</td>
                <td style={{ wordBreak: "break-all" }}>{v}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
