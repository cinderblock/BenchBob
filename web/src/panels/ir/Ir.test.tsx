import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/preact";
import { BobContext, type BobApi } from "../../bob/context";
import Ir, { type IrState } from "./Ir";

const st: IrState = {
  outputEnabled: true,
  receivers: [{ path: "/dev/lirc0", driver: "gpio_ir_recv" }],
  transmitters: [{ path: "/dev/lirc1", driver: "gpio-ir-tx", carrier: true }],
  protocols: [
    { name: "nec", carrier: 38000, maxAddress: 255, maxCommand: 255 },
    { name: "samsung", carrier: 38000, maxAddress: 255, maxCommand: 255 },
  ],
  captures: [
    { seq: 3, at: Date.UTC(2026, 9, 25, 23, 0, 0), device: "/dev/lirc0", code: { protocol: "nec", address: 4, command: 8, toggle: false }, timings: [9000, 4500, 562], carrier: 38000, pronto: "0000 006D 0002 0000" },
    { seq: 2, at: Date.UTC(2026, 9, 25, 22, 59, 0), device: "/dev/lirc0", code: null, timings: [1000, 1000, 1000], carrier: 38000, pronto: null },
  ],
  library: [{ device: "TV", buttons: [{ name: "power", code: { protocol: "samsung", address: 7, command: 2 } }] }],
};

function make(state: IrState) {
  const calls: [string, string, unknown][] = [];
  const api = vi.fn(async (m: string, p: string, body?: unknown) => {
    calls.push([m, p, body]);
    if (p === "/api/ir/capture") return state.captures[0];
    return { ok: true };
  });
  // The IR setup button reads the live status (is the GPIO panel there, with config.txt?).
  const snap = { status: { peripherals: { list: [{ id: "gpio", disabled: false }], state: { gpio: { bootConfig: { kind: "config.txt" } } } } } };
  const store = { subscribe: () => () => {}, get: () => snap };
  const b = { conn: { hostname: "bob", json: async () => state }, api, store, confirm: async () => true } as unknown as BobApi;
  render(
    <BobContext.Provider value={b}>
      <Ir st={{}} entry={{} as never} status={{} as never} />
    </BobContext.Provider>,
  );
  return calls;
}

describe("IR panel", () => {
  it("lists captures decoded or raw, and sends a library button", async () => {
    const calls = make(st);
    expect(await screen.findByText("nec 0x04 0x08")).toBeTruthy();
    expect(screen.getByText("raw, 3 timings")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "power" }));
    await waitFor(() => expect(calls).toContainEqual(["POST", "/api/ir/send", { device: "TV", button: "power" }]));
  });

  it("learns a button and saves it under a name", async () => {
    const calls = make(st);
    fireEvent.click(await screen.findByRole("button", { name: "Learn a button…" }));
    expect(await screen.findByText("Got nec 0x04 0x08")).toBeTruthy();
    fireEvent.input(screen.getByRole("textbox", { name: "Button" }), { target: { value: "volume up" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls).toContainEqual(["POST", "/api/ir/library", { device: "TV", button: "volume up", fromCapture: 3 }]));
  });

  it("points at the GPIO panel when there's no IR hardware", async () => {
    make({ ...st, receivers: [], transmitters: [], captures: [], library: [] });
    expect(await screen.findByText(/No IR receiver or transmitter yet/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "IR setup…" })).toBeTruthy();
  });
});
