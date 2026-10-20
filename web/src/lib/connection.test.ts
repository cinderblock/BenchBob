import { describe, expect, it } from "vitest";
import { BobConnection, defaultBobUrl } from "./connection";
import { buildCheck } from "./store";

describe("BobConnection", () => {
  it("derives HTTP and WebSocket URLs from the base URL", () => {
    const c = new BobConnection("http://deskypi.local/");
    expect(c.baseUrl).toBe("http://deskypi.local");
    expect(c.url("/api/status")).toBe("http://deskypi.local/api/status");
    expect(c.url("api/status")).toBe("http://deskypi.local/api/status");
    expect(c.wsUrl("/ws/gpio")).toBe("ws://deskypi.local/ws/gpio");
    expect(new BobConnection("https://bob.example:8443").wsUrl("/ws/can/can0")).toBe("wss://bob.example:8443/ws/can/can0");
  });

  it("defaults to the page's origin, which is same-origin", () => {
    const c = new BobConnection();
    expect(c.baseUrl).toBe(location.origin);
    expect(c.sameOrigin).toBe(true);
    expect(new BobConnection("http://elsewhere.invalid").sameOrigin).toBe(false);
  });

  it("takes ?bob= over VITE_BOB, and adds http:// to a bare host", () => {
    expect(defaultBobUrl("?bob=http://a.local", "http://b.local")).toBe("http://a.local");
    expect(defaultBobUrl("?bob=a.local:8090", undefined)).toBe("http://a.local:8090");
    expect(defaultBobUrl("", "http://b.local")).toBe("http://b.local");
    expect(defaultBobUrl("", undefined)).toBeUndefined();
  });
});

describe("buildCheck", () => {
  it("reloads a page its own bob stamped, when the bob's build changes", () => {
    expect(buildCheck({ sameOrigin: true, pageBuild: "abc", first: null, current: "abc" })).toBe("ok");
    expect(buildCheck({ sameOrigin: true, pageBuild: "abc", first: "abc", current: "def" })).toBe("reload");
  });

  it("never reloads a page served from elsewhere: it shows a notice instead", () => {
    expect(buildCheck({ sameOrigin: false, pageBuild: "abc", first: null, current: "abc" })).toBe("ok");
    expect(buildCheck({ sameOrigin: false, pageBuild: "abc", first: "abc", current: "def" })).toBe("notice");
  });

  it("treats an unstamped page (dev server) like a remote one", () => {
    expect(buildCheck({ sameOrigin: true, pageBuild: "__BUILD_VERSION__", first: "a", current: "a" })).toBe("ok");
    expect(buildCheck({ sameOrigin: true, pageBuild: "__BUILD_VERSION__", first: "a", current: "b" })).toBe("notice");
  });
});
