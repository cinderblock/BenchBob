// A Linux bob's LED strips panel (backend/crates/leds): the shared LED strips body. Its
// pins are claimed in the GPIO panel and it follows that panel's output control. Errors
// that a config.txt / cmdline.txt change fixes get a button to the GPIO panel's SPI settings.

import type { PanelProps } from "../../registry";
import { IfaceButton } from "../gpio/IfaceButton";
import { LedStrips } from "./LedStrips";

const NEEDS_SPI_SETUP = /core_freq|spidev\.bufsiz|enable SPI0|dtparam=audio/;

export default function Leds(_: PanelProps) {
  return (
    <LedStrips
      fix={(e) =>
        NEEDS_SPI_SETUP.test(e) && (
          <div className="pr-row">
            <IfaceButton focus="spi" label="SPI settings…" />
          </div>
        )
      }
    />
  );
}
