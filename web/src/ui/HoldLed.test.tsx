import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/preact";
import { BobContext, type BobApi } from "../bob/context";
import { HOLD_MS, HoldLed, RENEW_MS } from "./HoldLed";

const leds = [{ id: 0, name: "LED", color: "green", normally: "Wi-Fi status", held: false }];

function make() {
  const calls: unknown[] = [];
  const api = vi.fn(async (_m: string, _p: string, body?: unknown) => {
    calls.push(body);
    return { onboardLeds: [] };
  });
  render(
    <BobContext.Provider value={{ api } as unknown as BobApi}>
      <HoldLed leds={leds} />
    </BobContext.Provider>,
  );
  return { calls, api, btn: screen.getByRole("button", { name: "Hold to light the board's LED" }) };
}

describe("HoldLed", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("lights while held, renews, and releases on let-go", async () => {
    const { calls, api, btn } = make();
    fireEvent.pointerDown(btn, { button: 0, pointerId: 1 });
    expect(calls).toEqual([{ on: true, ms: HOLD_MS }]);
    expect(api).toHaveBeenCalledWith("POST", "/api/onboard-leds", { on: true, ms: HOLD_MS });
    expect(btn.getAttribute("aria-pressed")).toBe("true");
    await act(() => {
      vi.advanceTimersByTime(RENEW_MS * 2 + 10);
    });
    expect(calls.length).toBe(3);
    fireEvent.pointerUp(btn, { pointerId: 1 });
    expect(calls.at(-1)).toEqual({ on: false });
    expect(btn.getAttribute("aria-pressed")).toBe("false");
    await act(() => {
      vi.advanceTimersByTime(RENEW_MS * 3);
    });
    expect(calls.length).toBe(4);
  });

  it("works from the keyboard, ignores key repeat, and lets go when the window loses focus", () => {
    const { calls, btn } = make();
    fireEvent.keyDown(btn, { key: " " });
    fireEvent.keyDown(btn, { key: " ", repeat: true });
    expect(calls).toEqual([{ on: true, ms: HOLD_MS }]);
    fireEvent.blur(window);
    expect(calls.at(-1)).toEqual({ on: false });
    // Already released: the key-up sends nothing more.
    fireEvent.keyUp(btn, { key: " " });
    expect(calls.length).toBe(2);
  });

  it("ignores other mouse buttons", () => {
    const { calls, btn } = make();
    fireEvent.pointerDown(btn, { button: 2, pointerId: 1 });
    expect(calls).toEqual([]);
  });
});
