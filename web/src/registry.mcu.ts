// MCU build: what a microcontroller node serves (firmware/README.md "API"): status with
// Wi-Fi, GPIO and I²C. Nothing else is bundled.
import { lazy } from "react";
import type { Registry } from "./registry";

const registry: Registry = {
  linux: null,
  panels: {},
  node: {
    status: lazy(() => import("./sections/node/NodeStatus")),
    gpio: lazy(() => import("./sections/node/NodeGpio")),
    i2c: lazy(() => import("./sections/node/NodeI2c")),
  },
};

export default registry;
