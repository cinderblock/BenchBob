// UPS driver (peripherals/drivers/ups.ts): status shapes the panel reads.

export interface Ups {
  name: string;
  vendor: string;
  model: string;
  simulated?: boolean;
  driverRunning: boolean;
  reachable: boolean;
  flags: { word: string; severity: "ok" | "warn" | "bad" | string }[];
  error?: string;
  charge?: number | null;
  runtimeText?: string;
  load?: number | null;
  inputVoltage?: number | null;
  inputFrequency?: number | null;
  outputVoltage?: number | null;
  realpower?: number | null;
  batteryVoltage?: number | null;
  batteryType?: string;
  batteryDate?: string;
  lowCharge?: number | null;
  lowRuntime?: number | null;
  serial?: string;
  firmware?: string;
  beeper?: string;
  usb?: string;
  nutDriver?: string;
  history: { charge: number[]; load: number[] };
  events: { t: number; text: string }[];
}

export interface UpsState {
  upses: Ups[];
  nut: { running: boolean; shared: boolean; port: number; user: string; password?: string; log?: string[] };
}
