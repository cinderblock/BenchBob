import type { BobApi } from "../../bob/context";
import type { IfaceGroup } from "./IfaceButton";

// Placeholder: the interfaces dialog is ported with the GPIO panel.
export async function openInterfaces(b: BobApi, focus?: IfaceGroup) {
  b.toast(`Interfaces${focus ? " (" + focus + ")" : ""}: not ported yet`, "info");
}
