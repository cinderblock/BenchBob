import { describe, expect, it, vi, afterEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/preact";
import { Bob } from "./Bob";
import { BobConnection } from "../lib/connection";
import { BobStore } from "../lib/store";
import type { Status } from "../lib/types";

/** A store fed by the test instead of a network stream. */
class FakeStore extends BobStore {
  start() {}
  stop() {}
  feed(s: Status) {
    (this as any).onStatus(s);
  }
  node(p: object) {
    (this as any).set({ kind: "node", connected: true, ...p });
  }
}

const status = (over: Partial<Status> = {}): Status => ({
  hostname: "testbob",
  uptime: 3600,
  timestamp: Date.now(),
  version: "0.0.0",
  buildVersion: "abc",
  system: { model: "Raspberry Pi 4 Model B", ramMB: 4096, cpuUsage: 5, cpuTemp: 40, cores: 4, memPercent: 20, memTotal: 4000000, memAvailable: 3000000 },
  network: [{ name: "eth0", state: "UP", ip: "10.0.0.2", rxRate: 100, txRate: 10 }],
  drives: [],
  nbdExports: [],
  bootDevices: [],
  rpibootInstalled: true,
  rpibootResults: [],
  operations: [],
  peripherals: {
    list: [
      { id: "ups", title: "UPS", description: "", disabled: false, active: true, summary: "1 UPS" },
      { id: "sdr", title: "RTL-SDR", description: "", disabled: false, active: false, summary: "No stick" },
      { id: "gps", title: "GPS", description: "", disabled: true, active: false, summary: "" },
    ],
    state: {
      ups: { active: true, upses: [], nut: { running: false, shared: false, port: 3493, user: "u", password: "", log: [] } },
      sdr: { active: false, mode: "off", sticks: [], dvb: {} },
    },
  },
  ...over,
});

const make = () => {
  const conn = new BobConnection("http://bob.test");
  const store = new FakeStore(conn);
  const r = render(<Bob connection={conn} store={store} setTitle={false} />);
  return { conn, store, ...r };
};

afterEach(() => vi.unstubAllGlobals());

describe("<Bob>", () => {
  it("shows active panels as sections, idle ones in Tools, and leaves disabled ones out", async () => {
    const { store, container } = make();
    store.feed(status());
    expect(await screen.findByText("testbob")).toBeTruthy();
    await waitFor(() => expect(container.querySelector('[data-panel="ups"]')).toBeTruthy());
    expect(container.querySelector('[data-panel="sdr"]')).toBeNull();
    expect(screen.getByRole("button", { name: /RTL-SDR/ })).toBeTruthy();
    expect(screen.queryByText("GPS")).toBeNull();
  });

  it("unfolds an idle panel from the Tools strip", async () => {
    const { store, container } = make();
    store.feed(status());
    fireEvent.click(await screen.findByRole("button", { name: /RTL-SDR/ }));
    await waitFor(() => expect(container.querySelector('[data-panel="sdr"]')).toBeTruthy());
  });

  it("says which bob it talks to when it isn't the page's own", async () => {
    const { store } = make();
    store.feed(status());
    expect(await screen.findByText("via bob.test")).toBeTruthy();
  });

  it("shows a version notice, not a reload, when a remote bob's build changes", async () => {
    const { store } = make();
    store.feed(status({ buildVersion: "aaaaaaaa" }));
    store.feed(status({ buildVersion: "bbbbbbbb" }));
    expect(await screen.findByText(/backend changed version/)).toBeTruthy();
    expect(store.get().reloadPending).toBe(false);
  });

  it("erases only after ERASE is typed", async () => {
    const fetch = vi.fn(async () => new Response(JSON.stringify({ ok: true, message: "Erase started" })));
    vi.stubGlobal("fetch", fetch);
    const { store } = make();
    store.feed(status({ drives: [{ name: "sda", model: "Test Stick", size: 8e9, readRate: 0, writeRate: 0, hasPartitions: false, operations: [] }] }));
    fireEvent.click(await screen.findByRole("button", { name: "Erase" }));
    const go = (await screen.findAllByRole("button", { name: "Erase" })).find((b) => b.closest(".modal"))! as HTMLButtonElement;
    expect(go.disabled).toBe(true);
    fireEvent.input(screen.getByLabelText(/Type ERASE/), { target: { value: "ERASE" } });
    expect(go.disabled).toBe(false);
    fireEvent.click(go);
    await waitFor(() => expect(fetch).toHaveBeenCalled());
    const [url, init] = fetch.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("http://bob.test/api/erase");
    expect(JSON.parse(init.body as string)).toEqual({ device: "sda", mode: "quick" });
  });

  it("shows only the node sections a node lists", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify({ outputEnabled: false, board: "pico2w", pins: [{ gpio: 2, header: 4, name: "GPIO2", func: "input", level: 1, pull: "none", refusal: null, note: null }] }))),
    );
    const { store, container } = make();
    store.node({
      node: { hostname: "benchbob-0a1b2c", uptime: 10, version: "0.1.0", board: "pico2w", boardName: "Raspberry Pi Pico 2 W", chip: "RP2350", network: { mac: "00:11:22:33:44:55", wifi: "connected", ssid: "Lab", rssi: -50, ip: "10.0.0.9" }, heap: null },
      nodeInfo: { name: "BenchBob node API", endpoints: { "GET /api/gpio": "" } },
    });
    expect(await screen.findByText("Node Status")).toBeTruthy();
    expect(await screen.findByText("GPIO")).toBeTruthy();
    expect(screen.queryByText("I²C")).toBeNull();
    expect(container.querySelector(".pr-tools")).toBeNull();
  });
});
