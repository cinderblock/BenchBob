import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/preact";
import type { ReactNode } from "react";
import { BobContext, type BobApi } from "../../bob/context";
import { LedStrips } from "./LedStrips";
import type { LedString, LedsState } from "./api";

const caps: NonNullable<LedsState["capabilities"]> = {
  maxStrings: 4,
  maxPixels: 1024,
  maxClockHz: 20_000_000,
  chips: [
    { name: "ws2812b", kind: "one-wire", description: "WS2812B" },
    { name: "custom", kind: "one-wire", description: "Custom one-wire" },
    { name: "apa102", kind: "clocked", description: "APA102" },
  ],
  patterns: ["off", "solid", "rainbow", "order-test", "ruler"],
  pins: [2, 3, 6],
};

const desk: LedString = {
  id: 1, name: "desk", chip: "ws2812b", kind: "one-wire", pin: 2, clockPin: null, pixels: 60, order: "GRB",
  timing: { t0hNs: 400, t1hNs: 800, periodNs: 1250, resetUs: 300 }, clockHz: null, brightness: 128, gamma: true, invert: false,
  power: null, source: "pattern", pattern: { name: "rainbow", spread: 60, periodMs: 5000 }, state: "off", error: null, estimateMa: null,
};

const gpio = {
  pins: [
    { gpio: 2, header: 4, refusal: "used by LED string desk" },
    { gpio: 3, header: 5, refusal: null },
    { gpio: 4, header: 6, refusal: "used by I2C0 SDA" },
    { gpio: 6, header: 9, refusal: null },
  ],
};

function make(state: LedsState) {
  const calls: [string, string, unknown][] = [];
  let modal: ReactNode = null;
  const api = vi.fn(async (method: string, path: string, body?: unknown) => {
    calls.push([method, path, body]);
    if (path === "/api/gpio") return gpio;
    return { ok: true };
  });
  const b = {
    conn: { hostname: "node", json: async () => state },
    api,
    openModal: (c: ReactNode) => (modal = c),
    closeModal: () => (modal = null),
    confirm: async () => true,
  } as unknown as BobApi;
  const r = render(
    <BobContext.Provider value={b}>
      <LedStrips />
    </BobContext.Provider>,
  );
  const openModal = () =>
    render(
      <BobContext.Provider value={b}>
        <div className="modal">{modal}</div>
      </BobContext.Provider>,
    );
  return { calls, api, openModal, ...r };
}

describe("LED strings", () => {
  it("lists strings, says why they're dark, and offers quick patterns", async () => {
    const { calls } = make({ outputEnabled: false, capabilities: caps, strings: [desk] });
    expect(await screen.findByText("desk")).toBeTruthy();
    expect(screen.getByText("dark: output control off")).toBeTruthy();
    expect(screen.getByText(/Output control is off, so the strings are dark/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Order test" }));
    await waitFor(() => expect(calls).toContainEqual(["POST", "/api/leds/1/pattern", { name: "order-test" }]));
  });

  it("adds a clocked string with both pins picked, skipping refused pins", async () => {
    const { calls, openModal } = make({ outputEnabled: true, capabilities: caps, strings: [desk] });
    fireEvent.click(await screen.findByRole("button", { name: "Add string…" }));
    openModal();
    // fireEvent.change doesn't reach preact's handler on a happy-dom <select>; a real
    // change event does.
    const sel = screen.getByRole("combobox", { name: /Chip/ }) as HTMLSelectElement;
    sel.value = "apa102";
    sel.dispatchEvent(new Event("change", { bubbles: true }));
    expect(await screen.findByRole("group", { name: "Clock pin" })).toBeTruthy();
    expect(await screen.findByRole("group", { name: "Clock pin" })).toBeTruthy();
    const data = await screen.findByRole("group", { name: "Data pin" });
    await waitFor(() => expect(data.querySelectorAll(".pin-btn").length).toBe(4));
    // GPIO 2 belongs to another string: picking it only explains why.
    fireEvent.click(data.querySelectorAll(".pin-btn")[0]);
    expect(screen.getByText("GPIO 2: used by LED string desk")).toBeTruthy();
    fireEvent.click(data.querySelectorAll(".pin-btn")[1]);
    const clock = screen.getByRole("group", { name: "Clock pin" });
    fireEvent.click(clock.querySelectorAll(".pin-btn")[3]);
    fireEvent.click(screen.getByRole("button", { name: "Add" }));
    await waitFor(() => expect(calls.find((c) => c[1] === "/api/leds")).toBeTruthy());
    const body = calls.find((c) => c[1] === "/api/leds")![2] as Record<string, unknown>;
    expect(body).toMatchObject({ chip: "apa102", pin: 3, clockPin: 6, pixels: 60, power: null });
  });

  it("explains a board without LED support", async () => {
    make({ outputEnabled: false, capabilities: null, strings: [] });
    expect(await screen.findByText("This board has no LED string driver.")).toBeTruthy();
  });
});
