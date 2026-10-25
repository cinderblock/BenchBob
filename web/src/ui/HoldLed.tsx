// Press and hold to light the board's own LEDs (POST /api/onboard-leds), so you can see
// which board on the bench this page is talking to. The header shows it whenever the bob
// reports onboard LEDs (a Pi's ACT/PWR, a node's status LED).
//
// The bob only lights them for a short hold that this renews while the button is down,
// then releases on let-go; a lost connection or a closed tab lets the hold run out.

import { useEffect, useRef, useState } from "react";
import { useBob } from "../bob/context";
import type { OnboardLed } from "../lib/types";

/** How long each request lights the LEDs for, and how often a held button renews it. */
export const HOLD_MS = 1500;
export const RENEW_MS = 400;

const SWATCH: Record<string, string> = { green: "#3fb950", red: "#f85149", blue: "#58a6ff", white: "#e6edf3", amber: "#d29922", yellow: "#e3b341" };

export function HoldLed({ leds }: { leds: OnboardLed[] }) {
  const { api } = useBob();
  const [held, setHeld] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const timer = useRef<ReturnType<typeof setInterval> | null>(null);
  const send = useRef<(on: boolean) => void>(() => {});
  send.current = (on) => {
    api("POST", "/api/onboard-leds", on ? { on: true, ms: HOLD_MS } : { on: false }).then(
      () => setErr(null),
      (e) => setErr(String(e?.message || e)),
    );
  };
  const start = () => {
    if (timer.current) return;
    setHeld(true);
    send.current(true);
    timer.current = setInterval(() => send.current(true), RENEW_MS);
  };
  const stop = useRef(() => {});
  stop.current = () => {
    if (!timer.current) return;
    clearInterval(timer.current);
    timer.current = null;
    setHeld(false);
    send.current(false);
  };

  // Letting go somewhere the button can't see (another window, a hidden tab) still counts.
  useEffect(() => {
    const off = () => stop.current();
    addEventListener("blur", off);
    document.addEventListener("visibilitychange", off);
    return () => {
      removeEventListener("blur", off);
      document.removeEventListener("visibilitychange", off);
      off();
    };
  }, []);

  const label = leds.length === 1 ? "LED" : "LEDs";
  return (
    <span className="hold-led-wrap">
      <button
        type="button"
        className={`hold-led${held ? " on" : ""}`}
        aria-pressed={held}
        aria-label={`Hold to light the board's ${label}`}
        onPointerDown={(e) => {
          if (e.button !== 0) return;
          e.currentTarget.setPointerCapture?.(e.pointerId);
          start();
        }}
        onPointerUp={() => stop.current()}
        onPointerCancel={() => stop.current()}
        onLostPointerCapture={() => stop.current()}
        onKeyDown={(e) => {
          if (e.key !== " " && e.key !== "Enter") return;
          e.preventDefault();
          if (!e.repeat) start();
        }}
        onKeyUp={(e) => {
          if (e.key === " " || e.key === "Enter") stop.current();
        }}
        onContextMenu={(e) => e.preventDefault()}
      >
        {leds.map((l) => (
          <span
            key={l.id}
            className="hold-led-dot"
            style={{ background: l.color === "rgb" ? "conic-gradient(#f85149, #3fb950, #58a6ff, #f85149)" : SWATCH[l.color] || "#e6edf3" }}
          />
        ))}
        {held ? `${label} on` : `Hold: ${label}`}
      </button>
      {err && <span className="hdr-warn"> {err}</span>}
    </span>
  );
}
