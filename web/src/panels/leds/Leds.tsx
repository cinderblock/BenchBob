// A Linux bob's LED strips panel (backend/crates/leds): the shared LED strips body. Its
// pins are claimed in the GPIO panel and it follows that panel's output control.

import type { PanelProps } from "../../registry";
import { LedStrips } from "./LedStrips";

export default function Leds(_: PanelProps) {
  return <LedStrips />;
}
