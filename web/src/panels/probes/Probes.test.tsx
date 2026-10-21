import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/preact";
import { BobContext, type BobApi } from "../../bob/context";
import type { PanelProps } from "../../registry";
import Probes from "./Probes";
import type { ProbesState } from "./api";

const st: ProbesState = {
  probes: [],
  openocd: { path: "/usr/bin/openocd", version: "0.12" },
  bootloaders: [{ id: "rp2-1", name: "RP2040", kind: "rp2-bootsel", mount: "/media/RPI-RP2", label: "RPI-RP2", tool: { path: null, pkg: "picotool" } }],
};

function make(over: Partial<BobApi> = {}) {
  const b = {
    conn: { hostname: "bob", baseUrl: "http://bob", ws: () => ({ close() {} }), upload: vi.fn() },
    api: vi.fn(async () => ({ presets: [], all: [] })),
    ...over,
  } as unknown as BobApi;
  render(
    <BobContext.Provider value={b}>
      <Probes {...({ st } as PanelProps<ProbesState>)} />
    </BobContext.Provider>,
  );
  return b;
}

describe("Probes bootloader card", () => {
  it("asks for a file on the card instead of a toast", async () => {
    make();
    fireEvent.click(screen.getByRole("button", { name: "Flash UF2" }));
    expect((await screen.findByRole("alert")).textContent).toBe("✗ Choose a .uf2 file first×");
  });

  it("shows the flash result and payload size beside the button", async () => {
    const b = make();
    (b.conn.upload as any).mockResolvedValue({ message: "Copied to RPI-RP2", payload: 2048 });
    const input = document.querySelector('input[type="file"]') as HTMLInputElement;
    Object.defineProperty(input, "files", { value: [new File(["x"], "fw.uf2")] });
    fireEvent.click(screen.getByRole("button", { name: "Flash UF2" }));
    await waitFor(() => expect(screen.getByRole("status").textContent).toMatch(/^✓ Copied to RPI-RP2 \(2\.0 KB\) at /));
  });

  it("says when the target list can't be loaded", async () => {
    make({ api: vi.fn(async () => Promise.reject(new Error("openocd missing"))) as any });
    expect(await screen.findByText("Couldn't load the target list: openocd missing")).toBeTruthy();
  });
});
