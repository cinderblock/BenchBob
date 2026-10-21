import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/preact";
import { BobContext, type BobApi } from "../../bob/context";
import { ApiError } from "../../lib/connection";
import type { PanelProps } from "../../registry";
import Can from "./Can";
import type { CanState } from "./api";

const state = {
  ifaces: [],
  slcan: { candidates: [] },
  recordings: [],
  socketcand: { enabled: false, port: 29536, beacon: false, listening: false, clients: [] },
  standardBitrates: [500000],
  slcanBitrates: [500000],
} as unknown as CanState;

function make(api: (method: string, path: string, body?: unknown) => Promise<unknown>) {
  const b = {
    conn: { hostname: "bob", url: (p: string) => p, wsUrl: (p: string) => p },
    api,
    key: (k: string) => k,
    openModal: vi.fn(),
    closeModal: vi.fn(),
  } as unknown as BobApi;
  render(
    <BobContext.Provider value={b}>
      <Can {...({ st: state } as PanelProps<CanState>)} />
    </BobContext.Provider>,
  );
}

describe("CAN tools", () => {
  it("says what it created beside the button", async () => {
    make(async () => ({ name: "vcan0" }));
    fireEvent.click(screen.getByRole("button", { name: /Create virtual CAN/ }));
    await waitFor(() => expect(screen.getByRole("status").textContent).toMatch(/^✓ Created vcan0 at /));
  });

  it("asks for a device before attaching one by path", () => {
    const api = vi.fn();
    make(api);
    fireEvent.click(screen.getByRole("button", { name: "Attach" }));
    expect(screen.getByRole("alert").textContent).toBe("✗ Enter the serial device, e.g. /dev/ttyACM0×");
    expect(api).not.toHaveBeenCalled();
  });

  it("shows a socketcand failure beside Apply", async () => {
    make(async () => {
      throw new ApiError("port 29536 in use", 500);
    });
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));
    expect((await screen.findByRole("alert")).textContent).toBe("✗ port 29536 in use×");
  });
});
