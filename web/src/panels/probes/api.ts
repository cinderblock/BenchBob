// Debug probes (peripherals/drivers/probes.ts): OpenOCD-driven probes and MCUs in
// bootloader mode (RP2 BOOTSEL, STM32 DFU).

export interface ProbeJob {
  kind: string;
  stage: string;
  pct?: number | null;
  ok?: boolean;
  message?: string;
  done?: number;
}

export interface ProbeConfig {
  target: string;
  transport: string;
  speed: number;
  ports: { gdb: number; telnet: number; tcl: number };
  effectivePorts: { gdb: number; telnet: number; tcl: number };
}

export interface Probe {
  id: string;
  name: string;
  model?: string;
  serial?: string;
  vidpid?: string;
  usb?: string;
  sim?: boolean;
  note?: string;
  gdbBuiltIn?: boolean;
  iface?: string | null;
  espSerial?: boolean;
  ttys: { dev: string; iface?: string }[];
  transports: string[];
  config: ProbeConfig;
  server?: { state: string; ports: { gdb: number; telnet: number; tcl: number }; since?: number; stopped?: boolean; exitCode?: number | null } | null;
  ocd: { adapter?: string; voltage?: number | null; dpidr?: string; flash?: string; gdbClients: number; cpus: string[]; errors: string[]; hint?: string };
  job?: ProbeJob | null;
  lastJob?: ProbeJob | null;
}

export interface BootMcu {
  id: string;
  name: string;
  kind: "rp2-bootsel" | "stm32-dfu" | string;
  serial?: string;
  vidpid?: string;
  usb?: string;
  sim?: boolean;
  mount?: string;
  label?: string;
  readOnly?: boolean;
  block?: string;
  info?: { bootloader?: string; fields: Record<string, string> } | null;
  tool: { path?: string | null; pkg: string };
  job?: ProbeJob | null;
  lastJob?: ProbeJob | null;
}

export interface ProbesState {
  probes: Probe[];
  bootloaders: BootMcu[];
  openocd: { path?: string | null; version?: string };
}

export interface Targets {
  presets: { cfg: string; label: string }[];
  all: string[];
}

export interface Clients {
  gdb: string;
  telnet: string;
  cortexDebug: string;
  platformio: string;
}
