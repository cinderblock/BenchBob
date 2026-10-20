// The config.txt interfaces dialog lives in the GPIO panel, but the I²C, Serial and 1-Wire
// panels open it on their own group. The routes belong to the gpio driver, so there's no
// button while that driver is turned off, and none on boards without config.txt (other
// boards' overlays are edited from the GPIO panel).

import { useBob, useSnapshot, type BobApi } from "../../bob/context";

export type IfaceGroup = "i2c" | "spi" | "uart" | "w1";

/** Whether this bob can show the interfaces dialog. */
export function useIfaceAvailable(): boolean {
  const P = useSnapshot().status?.peripherals;
  return !!P?.list.some((d) => d.id === "gpio" && !d.disabled) && (P.state?.gpio?.bootConfig?.kind ?? "config.txt") === "config.txt";
}

/** Open the dialog, whole or focused on one group (its code loads on first use). */
export async function openInterfaces(b: BobApi, focus?: IfaceGroup) {
  (await import("./Interfaces")).openInterfaces(b, focus);
}

export function IfaceButton({ focus, label }: { focus: IfaceGroup; label: string }) {
  const b = useBob();
  if (!useIfaceAvailable()) return null;
  return (
    <button className="btn btn-sm" onClick={() => openInterfaces(b, focus).catch(() => {})}>
      {label}
    </button>
  );
}
