import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/preact";
import { BobContext, type BobApi } from "../../bob/context";
import { ApiError } from "../../lib/connection";
import type { PanelProps } from "../../registry";
import Ups from "./Ups";
import type { UpsState } from "./api";

const state: UpsState = { upses: [], nut: { running: true, shared: false, port: 3493, user: "u" } };

function make(api: (method: string, path: string, body?: unknown) => Promise<unknown>) {
  const b = { conn: { hostname: "bob" }, api } as unknown as BobApi;
  render(
    <BobContext.Provider value={b}>
      <Ups {...({ st: state } as PanelProps<UpsState>)} />
    </BobContext.Provider>,
  );
}

describe("UPS NUT server", () => {
  it("shows a failed restart beside the button", async () => {
    make(async () => {
      throw new ApiError("upsd failed to start", 500);
    });
    fireEvent.click(screen.getByRole("button", { name: "Restart NUT" }));
    expect((await screen.findByRole("alert")).textContent).toBe("✗ upsd failed to start×");
  });

  it("confirms a restart that worked", async () => {
    const api = vi.fn(async () => ({ ok: true }));
    make(api);
    fireEvent.click(screen.getByRole("button", { name: "Restart NUT" }));
    await waitFor(() => expect(screen.getByRole("status").textContent).toMatch(/^✓ NUT restarted at /));
    expect(api).toHaveBeenCalledWith("POST", "/api/ups/restart");
  });

  it("shows a failed share switch on the same line", async () => {
    make(async () => {
      throw new ApiError("port 3493 is in use", 409);
    });
    fireEvent.click(screen.getByRole("checkbox"));
    expect((await screen.findByRole("alert")).textContent).toBe("✗ port 3493 is in use×");
  });
});
