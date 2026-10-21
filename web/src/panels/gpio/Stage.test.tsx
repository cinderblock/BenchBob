import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/preact";
import type { ReactNode } from "react";
import { BobContext, type BobApi } from "../../bob/context";
import { openReboot } from "./Stage";

function reboot(api: BobApi["api"]) {
  let dialog: ReactNode = null;
  const b = { api, openModal: (c: ReactNode) => (dialog = c), closeModal: vi.fn() } as unknown as BobApi;
  openReboot(b);
  render(<BobContext.Provider value={b}>{dialog}</BobContext.Provider>);
  fireEvent.input(screen.getByLabelText("Type REBOOT to confirm:"), { target: { value: "REBOOT" } });
  fireEvent.click(screen.getByRole("button", { name: "Reboot" }));
  return b;
}

describe("reboot dialog", () => {
  it("says it's rebooting in the dialog and offers Close", async () => {
    const b = reboot(vi.fn(async () => ({ ok: true })) as any);
    await waitFor(() => expect(screen.getByRole("status").textContent).toBe("✓ Rebooting: back in about a minute."));
    expect(screen.queryByRole("button", { name: "Reboot" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    expect(b.closeModal).toHaveBeenCalled();
  });

  it("shows a refusal in the dialog and stays open", async () => {
    const b = reboot(vi.fn(async () => Promise.reject(new Error("sudo: a password is required"))) as any);
    expect((await screen.findByRole("alert")).textContent).toBe("✗ sudo: a password is required×");
    expect(screen.getByRole("button", { name: "Reboot" })).toBeTruthy();
    expect(b.closeModal).not.toHaveBeenCalled();
  });
});
