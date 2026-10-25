// Full build: everything a Linux bob can show, plus the node sections (so the full UI can
// also be pointed at a node with ?bob=).
import { lazy } from "react";
import type { Registry } from "./registry";

const registry: Registry = {
  linux: {
    system: lazy(() => import("./sections/system/System")),
    boot: lazy(() => import("./sections/boot/Boot")),
    drives: lazy(() => import("./sections/drives/Drives")),
  },
  panels: {
    serial: lazy(() => import("./panels/serial/Serial")),
    can: lazy(() => import("./panels/can/Can")),
    i2c: lazy(() => import("./panels/i2c/I2c")),
    onewire: lazy(() => import("./panels/onewire/OneWire")),
    gpio: lazy(() => import("./panels/gpio/Gpio")),
    wifi: lazy(() => import("./panels/wifi/Wifi")),
    ble: lazy(() => import("./panels/ble/Ble")),
    camera: lazy(() => import("./panels/camera/Camera")),
    usb: lazy(() => import("./panels/usb/Usb")),
    netdiag: lazy(() => import("./panels/netdiag/Netdiag")),
    sdr: lazy(() => import("./panels/sdr/Sdr")),
    gps: lazy(() => import("./panels/gps/Gps")),
    ups: lazy(() => import("./panels/ups/Ups")),
    probes: lazy(() => import("./panels/probes/Probes")),
  },
  node: {
    status: lazy(() => import("./sections/node/NodeStatus")),
    gpio: lazy(() => import("./sections/node/NodeGpio")),
    i2c: lazy(() => import("./sections/node/NodeI2c")),
    leds: lazy(() => import("./sections/node/NodeLeds")),
  },
};

export default registry;
