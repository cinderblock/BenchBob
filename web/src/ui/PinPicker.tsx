// A pin picker: the board's GPIOs as buttons in header order, the unusable ones greyed
// out with the reason underneath when tapped (no hover tooltips: they don't work on
// touch screens). Shared by every feature that puts something on a pin.

import { useState } from "react";

export interface PickPin {
  gpio: number;
  /** Physical header pin, when the board has one. */
  header: number | null;
  /** Why it can't be picked (e.g. "used by I2C0 SDA"); null when it can. */
  refusal: string | null;
}

export function PinPicker({ pins, value, onChange, label }: { pins: PickPin[]; value: number | null; onChange(gpio: number): void; label?: string }) {
  const [why, setWhy] = useState<PickPin | null>(null);
  const sorted = [...pins].sort((a, b) => (a.header ?? 1000 + a.gpio) - (b.header ?? 1000 + b.gpio));
  return (
    <div className="pin-pick" role="group" aria-label={label || "Pin"}>
      <div className="pin-grid">
        {sorted.map((p) => (
          <button
            type="button"
            key={p.gpio}
            className={`pin-btn${p.gpio === value ? " on" : ""}${p.refusal ? " off" : ""}`}
            aria-pressed={p.gpio === value}
            aria-disabled={!!p.refusal}
            onClick={() => (p.refusal ? setWhy(why?.gpio === p.gpio ? null : p) : (setWhy(null), onChange(p.gpio)))}
          >
            <span className="pin-g">GPIO {p.gpio}</span>
            {p.header != null && <span className="pin-h">pin {p.header}</span>}
          </button>
        ))}
      </div>
      {why && (
        <div className="pr-note">
          GPIO {why.gpio}: {why.refusal}
        </div>
      )}
    </div>
  );
}
