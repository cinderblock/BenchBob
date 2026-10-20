// What a build can render. Each section is a lazily loaded module, fetched the first time
// the bob it talks to reports something for it: a node never downloads the drives or CAN
// code, and a Pi with no camera never loads the camera panel. The full and MCU builds pick
// different registries (vite.config.ts aliases `@registry`).

import type { ComponentType, LazyExoticComponent } from "react";
import type { PeriphEntry, Status } from "./lib/types";

export interface PanelProps<S = any> {
  /** This driver's part of status.peripherals.state. */
  st: S;
  entry: PeriphEntry;
  status: Status;
}

export interface SectionProps {
  status: Status;
}

type Lazy<P> = LazyExoticComponent<ComponentType<P>>;

export interface Registry {
  /** Sections of a Linux bob (null in the MCU build). */
  linux: { system: Lazy<SectionProps>; boot: Lazy<SectionProps>; drives: Lazy<SectionProps> } | null;
  /** Peripheral panels by driver id. */
  panels: Record<string, Lazy<PanelProps>>;
  /** Sections of a microcontroller node. */
  node: { status: Lazy<{}>; gpio: Lazy<{}>; i2c: Lazy<{}> };
}
