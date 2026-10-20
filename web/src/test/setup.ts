import { afterEach } from "vitest";
import { cleanup } from "@testing-library/preact";

(globalThis as any).__MCU__ = false;
afterEach(() => cleanup());
