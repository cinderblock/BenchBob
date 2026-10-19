// Run: bun test peripherals/lib/wifi-client.test.ts
// The pure parts of the netns client-test runner. The orchestration itself is exercised on
// desky against a real AP (see plans/peripherals-wifi.md).
import { describe, expect, test } from "bun:test";
import { borrowRefusal, dhcpScript, validHost, validKey, validUrl, wpaConf, type Bins, type TestSpec } from "./wifi-client.ts";

const spec = (o: Partial<TestSpec>): TestSpec => ({ phy: "phy0", ssid: "Lab", security: "psk", ip: "dhcp", pingGateway: true, dns: [], http: [], ping: [], count: 3, holdS: 0, assocTimeoutS: 30, ...o });

describe("borrowRefusal (wlan0 is the uplink?)", () => {
  const desky = [
    { dev: "eth0", metric: 100 },
    { dev: "wlan0", metric: 600 },
  ];
  const wlan0 = new Set(["wlan0"]);
  test("desky today: eth0 primary and up → allowed", () => expect(borrowRefusal(desky, wlan0, () => true)).toBeNull());
  test("eth0 down → refused", () => expect(borrowRefusal(desky, wlan0, () => false)).toContain("eth0 is down"));
  test("wlan0 lower metric → refused", () => expect(borrowRefusal([{ dev: "eth0", metric: 700 }, { dev: "wlan0", metric: 600 }], wlan0, () => true)).toContain("uplink"));
  test("equal metrics → refused (ambiguous)", () => expect(borrowRefusal([{ dev: "eth0", metric: 600 }, { dev: "wlan0", metric: 600 }], wlan0, () => true)).toContain("uplink"));
  test("only route via wlan0 → refused", () => expect(borrowRefusal([{ dev: "wlan0", metric: 600 }], wlan0, () => true)).toContain("only default route"));
  test("radio with no default route (dongle) → allowed even if eth0 is down", () => expect(borrowRefusal(desky, new Set(["wlan1"]), () => false)).toBeNull());
});

describe("keys", () => {
  test("validKey", () => {
    expect(validKey("open", undefined)).toBeNull();
    expect(validKey("psk", undefined)).toContain("required");
    expect(validKey("psk", "short")).toContain("8–63");
    expect(validKey("psk", "a".repeat(64))).toBeNull(); // 64 hex = PMK
    expect(validKey("psk", "g".repeat(64))).toContain("8–63");
    expect(validKey("psk", "pass\nword")).toContain("printable");
    expect(validKey("sae", "a".repeat(64))).toContain("passphrase itself");
    expect(validKey("sae", "correct horse")).toBeNull();
  });
  test("hosts/urls can't become options", () => {
    expect(validHost("-oProxyCommand=x")).toBe(false);
    expect(validHost("example.com")).toBe(true);
    expect(validHost("2606:4700::1")).toBe(true);
    expect(validHost("a b")).toBe(false);
    expect(validUrl("https://example.com/x?y=1")).toBe(true);
    expect(validUrl("file:///etc/shadow")).toBe(false);
    expect(validUrl("-K/etc/x")).toBe(false);
  });
});

describe("wpa_supplicant conf", () => {
  const hex = (s: string) => Buffer.from(s).toString("hex");
  test("PSK passphrase quoted; SSID hex (quotes can't escape)", () => {
    const c = wpaConf(spec({ ssid: 'we"ird\nssid' }), 'pa"ss word');
    expect(c).toContain(`ssid=${hex('we"ird\nssid')}`);
    expect(c).toContain('psk="pa"ss word"'); // wpa_supplicant takes up to the last quote
    expect(c).toContain("key_mgmt=WPA-PSK");
    expect(c).toContain("p2p_disabled=1");
  });
  test("64-hex PMK goes unquoted (NM stores keys this way)", () => {
    const k = "AB".repeat(32);
    expect(wpaConf(spec({}), k)).toContain(`psk=${k.toLowerCase()}\n`);
    // transition mode with a PMK degrades to WPA2-PSK: SAE needs the passphrase
    const t = wpaConf(spec({ security: "psk-sae" }), k);
    expect(t).toContain("key_mgmt=WPA-PSK\n");
    expect(t).not.toContain("sae_password");
  });
  test("SAE / transition / OWE / open / EAP", () => {
    expect(wpaConf(spec({ security: "sae" }), "secret12")).toMatch(/key_mgmt=SAE\n\tieee80211w=2\n\tsae_password=736563726574313\d/);
    expect(wpaConf(spec({ security: "psk-sae" }), "secret12")).toContain("key_mgmt=WPA-PSK SAE");
    expect(wpaConf(spec({ security: "owe" }))).toContain("key_mgmt=OWE");
    expect(wpaConf(spec({ security: "open" }))).toContain("key_mgmt=NONE");
    const e = wpaConf(spec({ security: "eap", eap: { method: "TTLS", identity: "bob" } }), "pw");
    expect(e).toContain("eap=TTLS");
    expect(e).toContain(`identity=${hex("bob")}`);
    expect(e).toContain(`password=${hex("pw")}`);
  });
  test("BSSID pin", () => expect(wpaConf(spec({ security: "open", bssid: "aa:bb:cc:dd:ee:ff" }))).toContain("bssid=aa:bb:cc:dd:ee:ff"));
});

test("dhcp script writes the netns resolv.conf, not the host's", () => {
  const s = dhcpScript("benchbob-wt-3", { ip: "/usr/bin/ip" } as Bins);
  expect(s).toContain("/etc/netns/benchbob-wt-3/resolv.conf");
  expect(s).not.toMatch(/>\s*\/etc\/resolv\.conf/);
  expect(s).toContain('echo "BENCHBOB-DHCP $1|ip=$ip|');
});

test("pidAlive: self and dead pids don't count as another owner", async () => {
  const { pidAlive } = await import("./wifi-client.ts");
  expect(pidAlive(process.pid)).toBe(false);
  expect(pidAlive(0)).toBe(false);
  expect(pidAlive(2 ** 22 + 12345)).toBe(false); // beyond pid_max
  const child = Bun.spawn(["bun", "-e", "setTimeout(()=>{}, 5000)"]);
  expect(pidAlive(child.pid)).toBe(true);
  child.kill();
  await child.exited;
});
