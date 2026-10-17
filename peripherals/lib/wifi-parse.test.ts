// Run: bun test peripherals/lib/wifi-parse.test.ts
// Fixtures in wifi-fixtures/ are real iw output captured on desky (2026-10-17), plus
// scan-synthetic.txt for cases the bench's airspace doesn't have (hidden NUL SSID, WEP,
// OWE, 80/160 MHz, 6 GHz, WPS, WPA1/TKIP, Suite-B enterprise).
import { describe, expect, test } from "bun:test";
import * as P from "./wifi-parse.ts";

const fx = (name: string) => Bun.file(import.meta.dir + "/wifi-fixtures/" + name).text();

describe("channels", () => {
  test("freq ↔ channel across bands", () => {
    expect(P.freqToChannel(2412)).toBe(1);
    expect(P.freqToChannel(2484)).toBe(14);
    expect(P.freqToChannel(5180)).toBe(36);
    expect(P.freqToChannel(5825)).toBe(165);
    expect(P.freqToChannel(5955)).toBe(1);
    expect(P.freqToChannel(5935)).toBe(2);
    expect(P.freqToChannel(7115)).toBe(233);
    expect(P.channelToFreq(42, "5")).toBe(5210);
    expect(P.channelToFreq(14, "2.4")).toBe(2484);
    expect(P.bandOf(5220)).toBe("5");
    expect(P.bandOf(6115)).toBe("6");
    expect(P.uniiOf(5500)).toEqual({ name: "U-NII-2C", dfs: true });
    expect(P.uniiOf(5745)?.dfs).toBe(false);
  });
});

describe("iw dev / phy / reg", () => {
  test("iw dev: wlan0 + P2P wdev", async () => {
    const d = P.parseIwDev(await fx("dev.txt"));
    const w = d.find((x) => x.name === "wlan0")!;
    expect(w.phy).toBe("phy0");
    expect(w.ssid).toBe("TwillTech");
    expect(w.type).toBe("managed");
    expect(w.channel).toBe(44);
    expect(w.freq).toBe(5220);
    expect(w.width).toBe("40 MHz");
    expect(w.center1).toBe(5230);
    expect(d.find((x) => x.name === null)?.type).toBe("P2P-device");
  });

  test("iw phy: brcmfmac bands, DFS, disabled channels, iftypes", async () => {
    const p = P.parseIwPhy(await fx("phy-brcmfmac.txt"));
    expect(p.phy).toBe("phy0");
    expect(p.bands.map((b) => b.band)).toEqual(["2.4", "5"]);
    const b24 = p.bands[0];
    expect(b24.channels).toHaveLength(14);
    expect(b24.channels.filter((c) => c.disabled).map((c) => c.channel)).toEqual([12, 13, 14]);
    expect(b24.channels[0].maxPower).toBe(20);
    expect(b24.maxWidth).toBe(40);
    const b5 = p.bands[1];
    expect(b5.vht).toBe(true);
    expect(b5.maxWidth).toBe(80); // "neither 160 nor 80+80"
    expect(b5.channels.find((c) => c.channel === 52)?.radar).toBe(true);
    expect(b5.channels.find((c) => c.channel === 36)?.radar).toBe(false);
    expect(p.iftypes).toContain("AP");
    expect(p.monitor).toBe(false);
    expect(p.ap).toBe(true);
    expect(p.netns).toBe(true);
    expect(p.sae).toBe(true);
    expect(p.ciphers).toContain("CCMP-128");
  });

  test("iw reg get: global + self-managed phy", async () => {
    const r = P.parseIwReg(await fx("reg.txt"));
    expect(r[0]).toMatchObject({ scope: "global", country: "US", dfsRegion: "DFS-FCC" });
    const dfs = r[0].rules.find((x) => x.startMhz === 5250)!;
    expect(dfs.flags).toContain("DFS");
    expect(dfs.dfsCacMs).toBe(0);
    expect(dfs.maxEirp).toBe(24);
    expect(r[1]).toMatchObject({ scope: "phy0", country: "99" });
    expect(r[1].rules[0]).toMatchObject({ startMhz: 2402, endMhz: 2482, maxBwMhz: 40, maxEirp: 20 });
  });
});

describe("iw scan (real, desky wlan0)", async () => {
  const list = P.parseIwScan(await fx("scan-desky-wlan0.txt"));
  test("every BSS parsed", () => {
    expect(list).toHaveLength(30);
    for (const b of list) {
      expect(b.bssid).toMatch(/^[0-9a-f:]{17}$/);
      expect(b.freq).toBeGreaterThan(2400);
      expect(b.channel).toBeGreaterThan(0);
      expect(typeof b.signal).toBe("number");
    }
  });
  test("associated BSS: TwillTech ch44 40 MHz, WPA2-PSK, ax, BSS load", () => {
    const a = list.find((b) => b.associated)!;
    expect(a).toMatchObject({ bssid: "00:00:00:00:00:01", ssid: "TwillTech", channel: 44, band: "5", width: 40, centerFreq: 5230, centerChannel: 46, signal: -56 });
    expect(a.security).toBe("WPA2-PSK");
    expect(a.securityKind).toBe("psk");
    expect(a.standards).toEqual(["n", "ac", "ax"]);
    expect(a.generation).toBe("Wi-Fi 6");
    expect(a.stations).toBe(3);
    expect(a.utilization).toBeCloseTo(5.5, 1);
    expect(a.country).toBe("US");
    expect(a.lastSeenMs).toBe(0);
  });
  test("security variants: SAE, transition, enterprise (FT + SHA-256)", () => {
    expect(list.find((b) => b.ssid === "Net-01")?.security).toBe("WPA3-SAE");
    expect(list.find((b) => b.bssid === "02:00:00:00:00:05")?.securityKind).toBe("psk-sae");
    const ent = list.find((b) => b.bssid === "02:00:00:00:00:02")!;
    expect(ent.securityKind).toBe("eap");
    expect(ent.akm).toEqual(["FT/802.1X", "802.1X/SHA-256"]);
  });
  test("WPA+RSN mixed, 2.4 GHz 20 MHz", () => {
    const sp = list.find((b) => b.ssid === "Net-05")!;
    expect(sp.security).toBe("WPA/WPA2-PSK");
    expect(sp.channel).toBe(1);
    expect(sp.band).toBe("2.4");
    expect(sp.width).toBe(20);
  });
  test("40 MHz below", () => {
    const b = list.find((x) => x.bssid === "00:00:00:00:00:02")!;
    expect(b.channel).toBe(40);
    expect(b.centerChannel).toBe(38);
  });
  test("BSSes with no SSID element are hidden", () => {
    const hidden = list.filter((b) => b.hidden);
    expect(hidden).toHaveLength(3);
    for (const h of hidden) expect(h.ssid).toBe("");
  });
  test("locally-administered BSSIDs flagged", () => {
    expect(list.find((b) => b.bssid === "02:00:00:00:00:01")?.localMac).toBe(true);
    expect(list.find((b) => b.bssid === "00:00:00:00:00:01")?.localMac).toBe(false);
  });
});

describe("iw scan (synthetic edge cases)", async () => {
  const list = P.parseIwScan(await fx("scan-synthetic.txt"));
  const by = (mac: string) => list.find((b) => b.bssid.endsWith(mac))!;
  test("all parsed", () => expect(list).toHaveLength(7));
  test("NUL SSID is hidden; 802.11b; open", () => {
    expect(by("55")).toMatchObject({ hidden: true, ssid: "", security: "Open", standards: ["b"], channel: 6 });
  });
  test("escaped SSID bytes decode as UTF-8; WEP; 802.11g", () => {
    const b = by("66");
    expect(b.ssid.startsWith(" Café ")).toBe(true);
    expect(b.ssid.endsWith(" ")).toBe(true);
    expect(b.security).toBe("WEP");
    expect(b.standards).toEqual(["g"]);
  });
  test("WPA1/TKIP + WPS, HT40 below", () => {
    const b = by("77");
    expect(b.security).toBe("WPA-PSK");
    expect(b.pairwise).toEqual(["TKIP"]);
    expect(b.wps).toMatchObject({ state: "Configured", locked: true, manufacturer: "ACME", model: "R100", device: "ACME Router" });
    expect(b.width).toBe(40);
    expect(b.centerChannel).toBe(9);
  });
  test("OWE via numeric AKM, VHT80", () => {
    const b = by("88");
    expect(b.securityKind).toBe("owe");
    expect(b.pmf).toBe("required");
    expect(b).toMatchObject({ width: 80, centerChannel: 42, centerFreq: 5210 });
    expect(b.standards).toEqual(["n", "ac"]);
  });
  test("VHT160 (new-style segments), Suite-B enterprise", () => {
    const b = by("99");
    expect(b).toMatchObject({ width: 160, centerChannel: 114, security: "WPA3-Enterprise" });
    expect(b.generation).toBe("Wi-Fi 6");
  });
  test("6 GHz HE operation width, SAE-EXT-KEY", () => {
    const b = by("aa");
    expect(b).toMatchObject({ band: "6", channel: 33, width: 80, centerChannel: 39, security: "WPA3-SAE", generation: "Wi-Fi 6E" });
    expect(b.akm).toContain("SAE-EXT-KEY");
  });
  test("Privacy bit with no RSN/WPA = WEP; 2.4 with no rates = b", () => {
    expect(by("bb")).toMatchObject({ security: "WEP", channel: 3 });
  });
});

describe("link / station / survey", () => {
  test("iw link + station dump", async () => {
    const [link, station] = (await fx("link-station.txt")).split("====");
    const l = P.parseLink(link, station);
    expect(l).toMatchObject({ connected: true, bssid: "00:00:00:00:00:01", ssid: "TwillTech", freq: 5220, signal: -57, rxBitrate: 200, txBitrate: 180, txFailed: 0, connectedS: 49 });
    expect(P.parseLink("Not connected.").connected).toBe(false);
  });
  test("survey dump", () => {
    const s = P.parseSurvey(`Survey data from wlan1
	frequency:			2412 MHz [in use]
	noise:				-95 dBm
	channel active time:		1000 ms
	channel busy time:		250 ms
	channel receive time:		200 ms
	channel transmit time:		10 ms
Survey data from wlan1
	frequency:			2417 MHz
`);
    expect(s).toHaveLength(2);
    expect(s[0]).toMatchObject({ freq: 2412, inUse: true, noise: -95, busyPct: 25 });
    expect(s[1].busyPct).toBeUndefined();
    expect(P.parseSurvey("")).toEqual([]);
  });
});

describe("wpa_supplicant + udhcpc (lines seen on desky)", () => {
  const wpa = [
    "Successfully initialized wpa_supplicant",
    "Failed to create interface p2p-dev-wlan0: -16 (Device or resource busy)",
    "wlan0: Trying to associate with 00:00:00:00:00:01 (SSID='TwillTech' freq=5220 MHz)",
    "wlan0: Associated with 00:00:00:00:00:01",
    "wlan0: WPA: Key negotiation completed with 00:00:00:00:00:01 [PTK=CCMP GTK=CCMP]",
    "wlan0: CTRL-EVENT-CONNECTED - Connection to 00:00:00:00:00:01 completed [id=0 id_str=]",
  ];
  test("connect sequence", () => {
    const kinds = wpa.map((l) => P.parseWpaLine(l)?.kind);
    expect(kinds).toEqual(["info", "info", "assoc-start", "assoc", "handshake", "connected"]);
    expect(P.parseWpaLine(wpa[2])).toMatchObject({ bssid: "00:00:00:00:00:01", freq: 5220 });
    expect(P.parseWpaLine(wpa[4])?.detail).toBe("PTK=CCMP GTK=CCMP");
  });
  test("failures", () => {
    expect(P.parseWpaLine("wlan0: WPA: 4-Way Handshake failed - pre-shared key may be incorrect")?.kind).toBe("wrong-key");
    expect(P.parseWpaLine('wlan0: CTRL-EVENT-SSID-TEMP-DISABLED id=0 ssid="X" auth_failures=1 duration=10 reason=WRONG_KEY')).toMatchObject({ kind: "temp-disabled", detail: "WRONG_KEY" });
    expect(P.parseWpaLine("wlan0: CTRL-EVENT-DISCONNECTED bssid=aa:bb:cc:dd:ee:ff reason=15")).toMatchObject({ kind: "disconnected", reason: 15 });
    expect(P.parseWpaLine("wlan0: CTRL-EVENT-ASSOC-REJECT bssid=aa:bb:cc:dd:ee:ff status_code=17")).toMatchObject({ kind: "assoc-reject", status: 17, detail: "AP full (too many stations)" });
    expect(P.parseWpaLine("wlan0: CTRL-EVENT-NETWORK-NOT-FOUND")?.kind).toBe("not-found");
    // Wrong SAE password, captured from wpa_supplicant against a simulated WPA3 AP on desky
    expect(P.parseWpaLine("wlan1: CTRL-EVENT-AUTH-REJECT 02:00:00:00:01:00 auth_type=3 auth_transaction=2 status_code=1")).toMatchObject({ kind: "auth-reject", status: 1, bssid: "02:00:00:00:01:00" });
  });
  test("secrets are redacted, including from config parse errors", () => {
    const key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const e = P.parseWpaLine(`Line 5: Invalid passphrase length 64 (expected: 8..63) '${key}"'.`, [key])!;
    expect(e.kind).toBe("config-error");
    expect(e.text).not.toContain(key);
    expect(P.parseWpaLine('Line 5: failed to parse psk="hunter22"')!.text).not.toContain("hunter22");
  });
  test("udhcpc", () => {
    expect(P.parseUdhcpcLine("udhcpc: broadcasting discover")?.kind).toBe("discover");
    expect(P.parseUdhcpcLine("udhcpc: broadcasting select for 10.255.3.139, server 10.255.0.1")).toMatchObject({ kind: "offer", ip: "10.255.3.139", server: "10.255.0.1" });
    expect(P.parseUdhcpcLine("udhcpc: lease of 10.255.3.139 obtained from 10.255.0.1, lease time 86400")).toMatchObject({ kind: "ack", leaseS: 86400 });
    expect(P.parseUdhcpcLine("udhcpc: no lease, failing")?.kind).toBe("fail");
    const l = P.parseUdhcpcLine("DESKYPI-DHCP bound|ip=10.255.3.139|mask=20|router=10.255.0.1|dns=10.255.0.1 1.1.1.1|lease=86400|serverid=10.255.0.1|domain=tsl")!;
    expect(l.lease).toMatchObject({ event: "bound", ip: "10.255.3.139", mask: "20", dns: "10.255.0.1 1.1.1.1", domain: "tsl" });
    expect(P.maskToPrefix("20")).toBe(20);
    expect(P.maskToPrefix("255.255.240.0")).toBe(20);
  });
});

describe("ping / nslookup / curl / iperf / oui", () => {
  test("ping", () => {
    const p = P.parsePing(`PING 10.255.0.1 (10.255.0.1) 56(84) bytes of data.
64 bytes from 10.255.0.1: icmp_seq=1 ttl=64 time=14.9 ms

--- 10.255.0.1 ping statistics ---
3 packets transmitted, 3 received, 0% packet loss, time 602ms
rtt min/avg/max/mdev = 4.873/8.225/14.905/4.723 ms`);
    expect(p).toMatchObject({ sent: 3, received: 3, lossPct: 0, min: 4.873, avg: 8.225, max: 14.905 });
    const bad = P.parsePing("ping: connect: Network is unreachable");
    expect(bad).toMatchObject({ lossPct: 100, error: "connect: Network is unreachable" });
    expect(P.parsePing("5 packets transmitted, 3 received, +2 errors, 40% packet loss, time 4005ms").lossPct).toBe(40);
  });
  test("busybox nslookup", () => {
    const n = P.parseNslookup(`Server:		10.255.0.1
Address:	10.255.0.1:53

Non-authoritative answer:
Name:	example.com
Address: 104.20.23.154
Name:	example.com
Address: 172.66.147.243

Non-authoritative answer:
Name:	example.com
Address: 2606:4700:10::6814:179a
`);
    expect(n.server).toBe("10.255.0.1");
    expect(n.addresses).toEqual(["104.20.23.154", "172.66.147.243", "2606:4700:10::6814:179a"]);
    expect(n.error).toBeUndefined();
    expect(P.parseNslookup(";; connection timed out; no servers could be reached").error).toContain("timed out");
    expect(P.parseNslookup("Server:\t\t1.1.1.1\nAddress:\t1.1.1.1:53\n\n** server can't find nope.invalid: NXDOMAIN").error).toContain("NXDOMAIN");
  });
  test("curl %{json}", () => {
    const ok = P.parseCurlJson(JSON.stringify({ http_code: 200, remote_ip: "1.2.3.4", time_namelookup: 0.01, time_connect: 0.02, time_appconnect: 0.05, time_starttransfer: 0.08, time_total: 0.1, size_download: 512, speed_download: 5120, http_version: "2" }));
    expect(ok).toMatchObject({ ok: true, code: 200, dnsMs: 10, tlsMs: 50, totalMs: 100 });
    const bad = P.parseCurlJson(JSON.stringify({ http_code: 0, errormsg: "Could not resolve host: example.com" }));
    expect(bad).toMatchObject({ ok: false, error: "Could not resolve host: example.com" });
    expect(P.parseCurlJson("curl: (6) nope").ok).toBe(false);
  });
  test("iperf3 -J", () => {
    expect(P.parseIperf(JSON.stringify({ end: { sum_sent: { bits_per_second: 5e7, retransmits: 3 }, sum_received: { bits_per_second: 4.9e7 } } }))).toEqual({ sentBps: 5e7, receivedBps: 4.9e7, retransmits: 3 });
    expect(P.parseIperf(JSON.stringify({ error: "unable to connect to server" })).error).toContain("unable");
  });
  test("OUI formats", () => {
    const m = P.parseOui("F4-E2-C6   (hex)\t\tUbiquiti Inc\nF4E2C6     (base 16)\t\tUbiquiti Inc\nDC:A6:32\tRaspberr\tRaspberry Pi Trading Ltd\n001122 Cimsys Inc\n");
    expect(m.get("f4e2c6")).toBe("Ubiquiti Inc");
    expect(m.get("dca632")).toBe("Raspberry Pi Trading Ltd");
    expect(m.get("001122")).toBe("Cimsys Inc");
  });
});

test("iw phy: mac80211_hwsim (6 GHz, monitor, S1G band dropped)", async () => {
  const p = P.parseIwPhy(await fx("phy-hwsim.txt"));
  expect(p.bands.map((b) => b.band)).toEqual(["2.4", "5", "6"]);
  expect(p.monitor).toBe(true);
  const b6 = p.bands.find((b) => b.band === "6")!;
  expect(b6.he).toBe(true);
  expect(b6.channels.length).toBeGreaterThan(50);
  expect(b6.channels[0].channel).toBe(1);
});
