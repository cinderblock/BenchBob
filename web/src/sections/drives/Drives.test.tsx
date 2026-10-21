import { describe, expect, it, vi, afterEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/preact";
import { Bob } from "../../bob/Bob";
import { BobConnection } from "../../lib/connection";
import { BobStore } from "../../lib/store";
import type { Status } from "../../lib/types";

class FakeStore extends BobStore {
  start() {}
  stop() {}
  feed(s: Status) {
    (this as any).onStatus(s);
  }
}

const status = (): Status => ({
  hostname: "testbob", uptime: 1, timestamp: 0, version: "0", buildVersion: "a",
  system: { model: "Pi 4", cpuUsage: 1, cpuTemp: 40, cores: 4, memPercent: 10, memTotal: 1, memAvailable: 1 },
  network: [], nbdExports: [], bootDevices: [], rpibootInstalled: false, rpibootResults: [], operations: [],
  drives: [{ name: "sdb", model: "SD Reader", size: 32e9, readRate: 0, writeRate: 0, hasPartitions: false, operations: [] }],
});

function start(scan: () => Promise<Response>) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init?: RequestInit) => (init?.method === "POST" && new URL(url).pathname === "/api/scan" ? scan() : Response.json([]))),
  );
  const conn = new BobConnection("http://bob.test");
  const store = new FakeStore(conn);
  render(<Bob connection={conn} store={store} setTitle={false} />);
  store.feed(status());
}

afterEach(() => vi.unstubAllGlobals());

describe("drive actions", () => {
  it("show progress and the server's answer on the drive card", async () => {
    let answer!: (r: Response) => void;
    start(() => new Promise((r) => (answer = r)));
    fireEvent.click(await screen.findByRole("button", { name: "Scan" }, { timeout: 3000 }));
    expect(await screen.findByText("Scanning…")).toBeTruthy();
    expect((screen.getByRole("button", { name: "Scan" }) as HTMLButtonElement).disabled).toBe(true);
    answer(Response.json({ ok: true, message: "Partition table re-read" }));
    await waitFor(() => expect(document.querySelector("[data-drive=sdb] .act-stat.ok")?.textContent).toMatch(/^✓ Partition table re-read at /));
  });

  it("show a refusal on the drive card", async () => {
    start(async () => Response.json({ ok: false, message: "Device is busy" }, { status: 409 }));
    fireEvent.click(await screen.findByRole("button", { name: "Scan" }, { timeout: 3000 }));
    await waitFor(() => expect(document.querySelector("[data-drive=sdb] [role=alert]")?.textContent).toBe("✗ Device is busy×"));
  });
});
