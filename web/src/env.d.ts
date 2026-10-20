/// <reference types="vite/client" />

/** True in the microcontroller-node build (vite build --mode mcu). */
declare const __MCU__: boolean;

interface ImportMetaEnv {
  /** Default bob to talk to, e.g. http://deskypi.local (dev and test only). */
  readonly VITE_BOB?: string;
}
