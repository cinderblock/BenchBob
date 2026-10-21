import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/preact";
import { BobContext, type BobApi } from "../../bob/context";
import { ApiError } from "../../lib/connection";
import type { PanelProps } from "../../registry";
import Usb from "./Usb";
import type { UsbDev, UsbState } from "./api";

const hub: UsbDev = {
  busid: "1-1",
  bus: 1,
  devnum: 2,
  depth: 1,
  hub: true,
  vendor: "Acme",
  product: "Hub",
  vidpid: "1a40:0101",
  speed: 480,
  nports: 2,
  ports: [{ port: 1 }, { port: 2, child: "1-1.2" }],
  power: { loc: "1-1", mode: "ppps", switchable: true, ports: [{ port: 1, power: true }, { port: 2, power: true }] },
  protect: [],
  interfaces: [],
  nodes: [],
  rules: [],
  cls: "09",
  driver: "hub",
};

const state: UsbState = {
  active: true,
  summary: "",
  host: "bob",
  controllers: [{ name: "xhci", driver: "xhci_hcd", id: "0000:01:00.0", buses: [1] }],
  devices: [hub],
  uhubctl: { available: true },
  usbip: { running: false, available: true, port: 3240, peers: [] },
  redir: { available: true, apt: "usbredirserver", basePort: 4000 },
  rules: [],
};

const plan = { action: "on", needs: "none", ports: ["1-1.1"], devices: [], warnings: [] };

function make(json: (method: string, path: string, body?: unknown) => Promise<unknown>) {
  const b = { conn: { hostname: "bob", json }, toast: vi.fn(), api: vi.fn(), openModal: vi.fn(), closeModal: vi.fn() } as unknown as BobApi;
  render(
    <BobContext.Provider value={b}>
      <Usb {...({ st: state } as PanelProps<UsbState>)} />
    </BobContext.Provider>,
  );
  fireEvent.click(screen.getByRole("button", { name: /^P1 on/ }));
  return b;
}

describe("USB port power", () => {
  it("shows progress and the result beside the buttons, not as toasts", async () => {
    let finish!: () => void;
    const json = vi.fn(async (method: string) => (method === "GET" ? plan : new Promise<object>((r) => (finish = () => r({ ok: true })))));
    const b = make(json);
    fireEvent.click(screen.getByRole("button", { name: "On" }));
    expect(await screen.findByText("switching on…")).toBeTruthy();
    expect(screen.getByRole("button", { name: /^P1 switching on…/ })).toBeTruthy();
    expect((screen.getByRole("button", { name: "Off" }) as HTMLButtonElement).disabled).toBe(true);
    finish();
    expect(await screen.findByText(/✓ Switched on at/)).toBeTruthy();
    expect(b.toast).not.toHaveBeenCalled();
    expect(json).toHaveBeenCalledWith("POST", "/api/usb/power", { action: "on", hub: "1-1", port: 1, delay: 2 });
  });

  it("shows a failure inline until dismissed", async () => {
    const b = make(async (method) => {
      if (method === "GET") return plan;
      throw new ApiError("uhubctl on failed: permission denied", 500);
    });
    fireEvent.click(screen.getByRole("button", { name: "On" }));
    expect(await screen.findByText("✗ Power on failed: uhubctl on failed: permission denied")).toBeTruthy();
    expect(b.toast).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText(/failed/)).toBeNull();
  });
});
