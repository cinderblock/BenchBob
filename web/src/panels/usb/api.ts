// USB panel types (peripherals/drivers/usb.ts).

export interface HubPort {
  port: number;
  power: boolean;
  overCurrent?: boolean;
  internal?: string | null;
}

export interface HubPower {
  loc: string;
  mode?: string;
  switchable: boolean;
  why?: string;
  group?: { id: string; label: string; hubs: string[] } | null;
  ports?: HubPort[];
}

export interface UsbDev {
  busid: string;
  bus: number;
  devnum: number;
  depth: number;
  root?: boolean;
  hub?: boolean;
  vendor?: string;
  product?: string;
  vidpid: string;
  version?: string;
  speed: number;
  speedLabel?: string;
  maxPowerMa?: number;
  config?: string;
  nports: number;
  ports: { port: number; child?: string | null; overCurrent?: boolean }[];
  power?: HubPower | null;
  usbip?: "available" | "in use" | "error" | string | null;
  redir?: { port: number; peers: string[] } | null;
  protect: string[];
  interfaces: { clsName: string; driver?: string | null }[];
  nodes: { dev: string; label?: string; panel?: string }[];
  hint?: { label: string; panel?: string } | null;
  shareBlock?: string | null;
  rules: string[];
  serial?: string;
  cls: string;
  driver: string;
  idPath?: string;
  upstream?: { hub: string; port: number } | null;
}

export interface TimelineEvent {
  t: number;
  source: string;
  text: string;
  detail?: { kind?: string; level?: string };
}

export interface UsbState {
  active: boolean;
  summary: string;
  host: string;
  controllers: { name: string; driver: string; id: string; buses: number[] }[];
  devices: UsbDev[];
  uhubctl: { available: boolean; error?: string };
  usbip: { running: boolean; available: boolean; port: number; peers: string[]; error?: string };
  redir: { available: boolean; apt: string; basePort: number };
  rules: { match: "vidpid" | "port"; value: string }[];
  timeline?: TimelineEvent[];
  kernelLog?: { error?: string };
}

export interface PowerArgs {
  action: "on" | "off" | "cycle";
  hub?: string;
  port?: number;
  busid?: string;
  delay?: number;
  confirm?: string;
}

export interface PowerPlan {
  action: string;
  needs: "none" | "confirm" | "unsafe" | string;
  ports: string[];
  devices: string[];
  warnings: string[];
  group?: unknown;
}

export interface DmesgLine {
  ts: string;
  level: string;
  text: string;
}
