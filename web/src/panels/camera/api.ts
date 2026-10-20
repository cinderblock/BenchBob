// Camera panel types (peripherals/drivers/camera.ts).

export interface StreamChoice {
  fourcc: string;
  w: number;
  h: number;
  fps: number;
}

export interface CameraPublic {
  id: string;
  dev: string;
  name: string;
  vendor: string;
  vid: string;
  pid: string;
  serial: string;
  kind: "hdmi" | "webcam";
  usbPort: string;
  driver: string;
  stream: StreamChoice | null;
  streaming: boolean;
  viewers: number;
  fps: number;
  signal: boolean | null;
  share: boolean;
  sharePort: number;
  lastSnapshot: number;
  error: string;
}

export interface CameraFormat {
  fourcc: string;
  description: string;
  compressed?: boolean;
  stepwise?: boolean;
  sizes: { w: number; h: number; fps: number[] }[];
}

export interface CameraControl {
  name: string;
  type: string;
  min?: number;
  max?: number;
  step?: number;
  value: number;
  default?: number;
  flags: string[];
  menu?: { value: number; label: string }[];
}

export interface CameraDetail extends CameraPublic {
  formats: CameraFormat[];
  controls: CameraControl[];
}

export interface CameraState {
  active: boolean;
  summary: string;
  cameras: CameraPublic[];
  csi: { index: number; sensor: string; detail: string }[];
}

const e = encodeURIComponent;
export const camPaths = {
  detail: (id: string) => `/api/camera/${e(id)}`,
  stream: (id: string) => `/api/camera/${e(id)}/stream`,
  snapshot: (id: string) => `/api/camera/${e(id)}/snapshot`,
  config: (id: string) => `/api/camera/${e(id)}/config`,
  controls: (id: string) => `/api/camera/${e(id)}/controls`,
  controlsReset: (id: string) => `/api/camera/${e(id)}/controls/reset`,
  stop: (id: string) => `/api/camera/${e(id)}/stop`,
};
