// A node's addressable LED strings (firmware/README.md "LED strings"): the shared LED
// strips body, polled like the other node sections.

import { LedStrips } from "../../panels/leds/LedStrips";

export default function NodeLeds() {
  return (
    <div className="section pr-sec">
      <div className="section-title">
        <span>LED strings</span>
      </div>
      <LedStrips />
    </div>
  );
}
