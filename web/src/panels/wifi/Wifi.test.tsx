import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/preact";
import { BobContext, type BobApi } from "../../bob/context";
import { ApiError } from "../../lib/connection";
import type { PanelProps } from "../../registry";
import Wifi from "./Wifi";
import type { WifiState } from "./api";

const state: WifiState = { active: true, summary: "", radiosSeq: 1, resultsSeq: 1, profilesCount: 0, scan: { seq: 1 }, running: [] };

describe("Wi-Fi panel", () => {
  it("notes a background fetch that failed instead of dropping it", async () => {
    const b = {
      conn: { hostname: "bob", fetch: async () => new Response("{}") },
      key: (k: string) => k,
      api: async (_m: string, path: string) => {
        if (path === "/api/wifi/radios") throw new ApiError("iw not found", 500);
        return path.startsWith("/api/wifi/scan") ? { bss: [], scan: {}, now: 1 } : { results: [], profiles: [] };
      },
    } as unknown as BobApi;
    render(
      <BobContext.Provider value={b}>
        <Wifi {...({ st: state } as PanelProps<WifiState>)} />
      </BobContext.Provider>,
    );
    expect(await screen.findByText("Couldn’t load radios: iw not found")).toBeTruthy();
  });
});
