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

const IMAGES = [
  { key: "rpios/current/64/lite", name: "Raspberry Pi OS Lite (64-bit)", description: "No desktop", url: "https://x/lite64.img.xz", devices: ["pi4-64bit"], path: ["Raspberry Pi OS"], downloadSize: 5e8, extractSize: 2.8e9, releaseDate: "2026-09-01", initFormat: "systemd", rpios: { edition: "lite", arch: "64", legacy: false, release: "trixie" } },
  { key: "other/ubuntu/server", name: "Ubuntu Server 26.04", description: "Server", url: "https://x/ubuntu.img.xz", devices: ["pi4-64bit"], path: ["Other general-purpose OS", "Ubuntu"], downloadSize: 1e9, extractSize: 4e9, releaseDate: "2026-04-20", initFormat: "cloudinit-rpi" },
];

const status = (): Status => ({
  hostname: "testbob", uptime: 1, timestamp: 0, version: "0", buildVersion: "a",
  system: { model: "Pi 4", cpuUsage: 1, cpuTemp: 40, cores: 4, memPercent: 10, memTotal: 1, memAvailable: 1 },
  network: [], nbdExports: [], bootDevices: [], rpibootInstalled: false, rpibootResults: [], operations: [],
  drives: [{ name: "sdb", model: "SD Reader", size: 32e9, readRate: 0, writeRate: 0, hasPartitions: false, operations: [] }],
});

afterEach(() => vi.unstubAllGlobals());

describe("flash picker", () => {
  it("flashes the chosen image with the form's first-boot settings", async () => {
    const posts: Record<string, any> = {};
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        const p = new URL(url).pathname;
        if (init?.method === "POST") {
          posts[p] = JSON.parse(init.body as string);
          return Response.json({ ok: true, message: "Flash started" });
        }
        if (p === "/api/catalog") return Response.json({ images: IMAGES, devices: [{ name: "Raspberry Pi 4", tags: ["pi4-64bit"] }] });
        if (p === "/api/defaults") return Response.json({ username: "bench", sshKeys: ["ssh-ed25519 AAAA test"], enableSsh: true });
        if (p === "/api/quicklist") return Response.json([{ key: "rpios/current/64/lite", label: "Pi OS Lite 64-bit" }]);
        return Response.json([]);
      }),
    );
    const conn = new BobConnection("http://bob.test");
    const store = new FakeStore(conn);
    render(<Bob connection={conn} store={store} setTitle={false} />);
    store.feed(status());
    fireEvent.click(await screen.findByRole("button", { name: "Flash" }));
    expect(await screen.findByText("Flash Image")).toBeTruthy();

    // Nothing picked yet: the dialog's Flash is disabled.
    const go = () => screen.getAllByRole("button", { name: "Flash" }).at(-1) as HTMLButtonElement;
    expect(go().disabled).toBe(true);
    fireEvent.click(screen.getByText("Other OSes"));
    fireEvent.click(await screen.findByText("Ubuntu Server 26.04"));
    expect(go().disabled).toBe(false);

    // A bad hostname blocks it again.
    const host = document.getElementById("sf-host") as HTMLInputElement;
    fireEvent.input(host, { target: { value: "bad_name!" } });
    expect(go().disabled).toBe(true);
    fireEvent.input(host, { target: { value: "garage-pi" } });
    fireEvent.click(document.getElementById("sf-bb")!);
    fireEvent.click(go());

    await waitFor(() => expect(posts["/api/flash"]).toBeTruthy());
    const r = posts["/api/flash"];
    expect(r).toMatchObject({ device: "sdb", imageKey: "other/ubuntu/server", imageUrl: "https://x/ubuntu.img.xz" });
    expect(r.settings).toMatchObject({ hostname: "garage-pi", username: "bench", installBenchBob: true, enableSsh: true, sshKeys: ["ssh-ed25519 AAAA test"] });
  });

  it("Quick Pi asks only for a hostname and uses the saved defaults", async () => {
    let flashed: any = null;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        const p = new URL(url).pathname;
        if (init?.method === "POST") {
          if (p === "/api/flash") flashed = JSON.parse(init.body as string);
          return Response.json({ ok: true });
        }
        if (p === "/api/catalog") return Response.json({ images: IMAGES, devices: [] });
        if (p === "/api/defaults") return Response.json({ sshKeys: ["ssh-ed25519 AAAA test"], installBenchBob: true, wifiNetworks: [{ ssid: "Lab", password: "pw", country: "US" }, { ssid: "NoPw" }] });
        if (p === "/api/quicklist") return Response.json([{ key: "rpios/current/64/lite", label: "Pi OS Lite 64-bit" }]);
        return Response.json([]);
      }),
    );
    const conn = new BobConnection("http://bob.test");
    const store = new FakeStore(conn);
    render(<Bob connection={conn} store={store} setTitle={false} />);
    store.feed(status());
    fireEvent.click(await screen.findByRole("button", { name: "⚡ Quick Pi" }));
    expect(await screen.findByText(/skipped NoPw/)).toBeTruthy();
    fireEvent.input(document.getElementById("qp-host")!, { target: { value: "bench-7" } });
    fireEvent.click(screen.getAllByRole("button", { name: "Flash" }).at(-1)!);
    await waitFor(() => expect(flashed).toBeTruthy());
    expect(flashed.imageKey).toBe("rpios/current/64/lite");
    expect(flashed.settings).toMatchObject({ hostname: "bench-7", password: "", installBenchBob: true, wifiNetworks: [{ ssid: "Lab", password: "pw", country: "US" }] });
  });
});
