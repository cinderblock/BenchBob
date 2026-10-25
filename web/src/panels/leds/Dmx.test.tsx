import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/preact";
import type { ReactNode } from "react";
import { BobContext, type BobApi } from "../../bob/context";
import { DmxOutputs, type DmxState } from "./Dmx";

const st: DmxState = {
  outputEnabled: false,
  maxOutputs: 8,
  ports: [{ path: "/dev/serial/by-id/usb-FTDI_FT232R-if00", device: "/dev/ttyUSB0", name: "usb-FTDI_FT232R-if00" }],
  outputs: [
    {
      id: 1,
      name: "stage",
      port: "/dev/ttyUSB1",
      mode: "raw",
      refreshHz: 40,
      network: { protocol: "sacn", universe: 3 },
      source: "network",
      receiving: { from: "QLC+", packets: 4321, ageMs: 200 },
      state: "off",
      error: null,
      levels: "ff80".padEnd(1024, "0"),
    },
  ],
};

function make() {
  const calls: [string, string, unknown][] = [];
  let modal: ReactNode = null;
  const b = {
    conn: { hostname: "bob", json: async () => st },
    api: vi.fn(async (m: string, p: string, body?: unknown) => (calls.push([m, p, body]), { ok: true })),
    openModal: (c: ReactNode) => (modal = c),
    closeModal: () => (modal = null),
    confirm: async () => true,
  } as unknown as BobApi;
  render(
    <BobContext.Provider value={b}>
      <DmxOutputs />
    </BobContext.Provider>,
  );
  const openModal = () =>
    render(
      <BobContext.Provider value={b}>
        <div className="modal">{modal}</div>
      </BobContext.Provider>,
    );
  return { calls, openModal };
}

describe("DMX outputs", () => {
  it("shows an output's source, state and levels, and sets levels by hand", async () => {
    const { calls } = make();
    expect(await screen.findByText("stage")).toBeTruthy();
    expect(screen.getByText("not sending: output control off")).toBeTruthy();
    expect(screen.getByText(/sACN universe 3 · following the network · from QLC\+, 4,321 packets, last just now/)).toBeTruthy();
    expect(screen.getByRole("img", { name: "Levels of channels 1–512 (2 above zero)" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "All 255" }));
    await waitFor(() => expect(calls).toContainEqual(["POST", "/api/dmx/1/levels", { fill: 255 }]));
  });

  it("adds an output on a port, bridging an Art-Net universe", async () => {
    const { calls, openModal } = make();
    fireEvent.click(await screen.findByRole("button", { name: "Add DMX output…" }));
    openModal();
    const sel = screen.getByRole("combobox", { name: "DMX input" }) as HTMLSelectElement;
    sel.value = "artnet";
    sel.dispatchEvent(new Event("change", { bubbles: true }));
    fireEvent.click(await screen.findByRole("button", { name: "Add" }));
    await waitFor(() => expect(calls.find((c) => c[1] === "/api/dmx")).toBeTruthy());
    expect(calls.find((c) => c[1] === "/api/dmx")![2]).toEqual({
      name: undefined,
      port: "/dev/serial/by-id/usb-FTDI_FT232R-if00",
      mode: "raw",
      refreshHz: 40,
      network: { protocol: "artnet", universe: 1 },
    });
  });
});
