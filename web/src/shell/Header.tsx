import { useState, type ReactNode } from "react";
import { useBob, useSnapshot } from "../bob/context";
import { fmtRam, fmtSz, fmtUp } from "../lib/format";
import { HoldLed } from "../ui/HoldLed";

export function Header() {
  const { conn } = useBob();
  const snap = useSnapshot();
  const [boardOpen, setBoardOpen] = useState(false);
  const s = snap.status;
  const n = snap.node;
  const remote = !conn.sameOrigin;
  const leds = s?.onboardLeds ?? n?.onboardLeds ?? [];

  let title = "BenchBob";
  let board = "";
  let boardInfo = "";
  let right: ReactNode = null;
  if (s) {
    title = s.hostname || "BenchBob";
    board = s.system.model + (s.system.ramMB ? ` · ${fmtRam(s.system.ramMB)} RAM` : "") + (s.board && !s.board.tested ? " · untested board" : "");
    boardInfo = s.board ? `Board file: ${s.board.id}${s.board.tested ? "" : " (nobody has run BenchBob on this board yet; its pin map comes from vendor documentation)"}` : "";
    const r = s.rootStorage;
    right = (
      <>
        {r && (
          <span>
            <b>{r.kind}</b> {r.kind === "Netboot" ? r.source : r.size ? fmtSz(r.size) : r.source}
            {(r.usePercent ?? 0) >= 85 && <span className="hdr-warn"> {r.usePercent}% full</span>}
          </span>
        )}
        <span className="mono">{s.network.find((x) => x.ip)?.ip || ""}</span>
        <span>{fmtUp(s.uptime)}</span>
      </>
    );
  } else if (n) {
    title = n.hostname || "BenchBob node";
    board = `${n.boardName || n.board} · ${n.chip} · node firmware ${n.version}`;
    boardInfo = `Board: ${n.board}. A microcontroller node: GPIO and I²C over the same API as a BenchBob server.`;
    right = (
      <>
        <span className="mono">{n.network.ip || ""}</span>
        <span>{fmtUp(n.uptime)}</span>
      </>
    );
  }

  return (
    <header>
      <div className="hdr-id">
        <h1 id="title">{title}</h1>
        {board && (
          <button type="button" className="hdr-board" aria-expanded={boardOpen} onClick={() => setBoardOpen(!boardOpen)}>
            {board}
          </button>
        )}
      </div>
      <div className="hdr-right">
        {remote && <span className="bob-origin">via {conn.host}</span>}
        {leds.length > 0 && <HoldLed leds={leds} />}
        {right}
        <span className={`dot${snap.connected ? "" : " off"}`} role="status" aria-label={snap.connected ? "Connected" : "Disconnected"} />
      </div>
      {boardOpen && boardInfo && <div className="hdr-boardinfo">{boardInfo}</div>}
    </header>
  );
}
