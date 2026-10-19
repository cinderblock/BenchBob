import { expect, test } from "bun:test";
import { DEFAULT_PING_TARGETS, formatPresets, iperfError, mtuSearch, fmtBps, iperfSummary, parsePresets, presetsFrom, isErrorCounter, parseEee, parseEthtool, parseEthtoolStats, parseKeyColon, parseLldpNeighbors, parsePing, parseResolvConf, slowJson, validHost } from "../lib/netdiag.ts";

// All fixtures below are real output captured on desky (Pi 4, bcmgenet, eth0) unless noted.
const ethtool = `netlink error: Operation not permitted
Settings for eth0:
\tSupported ports: [ TP\t MII ]
\tSupported link modes:   10baseT/Half 10baseT/Full
\t                        100baseT/Half 100baseT/Full
\t                        1000baseT/Half 1000baseT/Full
\tSupported pause frame use: Symmetric Receive-only
\tSupports auto-negotiation: Yes
\tSupported FEC modes: Not reported
\tAdvertised link modes:  10baseT/Half 10baseT/Full
\t                        100baseT/Half 100baseT/Full
\t                        1000baseT/Half 1000baseT/Full
\tAdvertised pause frame use: Symmetric Receive-only
\tAdvertised auto-negotiation: Yes
\tAdvertised FEC modes: Not reported
\tLink partner advertised link modes:  10baseT/Half 10baseT/Full
\t                                     100baseT/Half 100baseT/Full
\t                                     1000baseT/Full
\tLink partner advertised pause frame use: No
\tLink partner advertised auto-negotiation: Yes
\tLink partner advertised FEC modes: Not reported
\tSpeed: 1000Mb/s
\tDuplex: Full
\tAuto-negotiation: on
\tmaster-slave cfg: preferred slave
\tmaster-slave status: slave
\tPort: Twisted Pair
\tPHYAD: 1
\tTransceiver: external
\tMDI-X: Unknown
        Current message level: 0x00000007 (7)
                               drv probe link
\tLink detected: yes
`;

test("ethtool link parsing", () => {
  const l = parseEthtool(ethtool);
  expect(l.speed).toBe(1000);
  expect(l.duplex).toBe("Full");
  expect(l.autoneg).toBe(true);
  expect(l.link).toBe(true);
  expect(l.port).toBe("Twisted Pair");
  expect(l.supported).toEqual(["10baseT/Half", "10baseT/Full", "100baseT/Half", "100baseT/Full", "1000baseT/Half", "1000baseT/Full"]);
  expect(l.partner).toEqual(["10baseT/Half", "10baseT/Full", "100baseT/Half", "100baseT/Full", "1000baseT/Full"]);
  expect(l.partnerAutoneg).toBe(true);
  expect(l.masterSlave).toBe("slave");
  // No link: speed and duplex are "Unknown!"
  const down = parseEthtool("Settings for eth1:\n\tSpeed: Unknown!\n\tDuplex: Unknown! (255)\n\tLink detected: no\n");
  expect(down).toMatchObject({ speed: null, duplex: "", link: false });
});

test("ethtool -i", () => {
  const i = parseKeyColon("driver: bcmgenet\nversion: 6.18.50+rpt-rpi-v8\nfirmware-version: \nbus-info: fd580000.ethernet\nsupports-statistics: yes\n");
  expect(i).toMatchObject({ driver: "bcmgenet", "firmware-version": "", "bus-info": "fd580000.ethernet" });
});

const stats = `NIC statistics:
     rx_packets: 56313
     tx_packets: 9525
     rx_bytes: 17515802
     rx_errors: 0
     rx_dropped: 5
     multicast: 7405
     rx_crc_errors: 0
     rx_1522_2047_oct: 80
     rx_bytes: 17790957
     rx_fcs: 0
     tx_late_col: 0
     rx_runt_pkts: 0
     rbuf_ovflow_cnt: 0
     mdf_err_cnt: 6368
     rxq0_bytes: 17515802
     rxq0_errors: 0
     rxq0_dropped: 0
`;

test("ethtool -S parsing and error-counter selection", () => {
  const s = parseEthtoolStats(stats);
  expect(s.rx_bytes).toBe(17515802); // first of the duplicate pair
  expect(s.mdf_err_cnt).toBe(6368);
  const errs = Object.keys(s).filter(isErrorCounter);
  expect(errs).toEqual(["rx_errors", "rx_dropped", "rx_crc_errors", "rx_fcs", "tx_late_col", "rx_runt_pkts", "rbuf_ovflow_cnt", "rxq0_errors", "rxq0_dropped"]);
  expect(isErrorCounter("rx_packets")).toBe(false);
  expect(isErrorCounter("rx_1522_2047_oct")).toBe(false);
});

test("EEE parsing", () => {
  const e = parseEee(`EEE settings for eth0:
\tEEE status: enabled - inactive
\tTx LPI: 34 (us)
\tSupported EEE link modes:  100baseT/Full
\t                           1000baseT/Full
\tAdvertised EEE link modes:  100baseT/Full
\t                            1000baseT/Full
\tLink partner advertised EEE link modes:  Not reported
`);
  expect(e).toEqual({ supported: true, enabled: true, active: false, txLpiUs: 34, advertised: ["100baseT/Full", "1000baseT/Full"], partner: [] });
  expect(parseEee("netlink error: Operation not supported")).toBeNull();
});

// Real neighbour learned on desky's eth0 (UniFi switch), lldpd 1.0.18.
const lldpReal = {
  lldp: {
    interface: {
      eth0: {
        via: "LLDP", rid: "1", age: "0 day, 00:00:04",
        chassis: { "USW-Lite-16-PoE": { id: { type: "mac", value: "74:ac:b9:e3:99:6f" }, descr: "UBNT-USL16L", "mgmt-ip": ["10.255.0.226", "fe80::76ac:b9ff:fee3:996f"], capability: { type: "Bridge", enabled: true } } },
        port: { id: { type: "local", value: "Port 15" }, descr: "Port 15", ttl: "120" },
        vlan: { "vlan-id": "1", pvid: true },
        "lldp-med": { "device-type": "Network Connectivity Device", capability: [{ type: "Capabilities", available: true }, { type: "Policy", available: true }] },
      },
    },
  },
};
// Representative two-port case: array form, unnamed chassis, CDP, PoE, several VLANs.
const lldpMulti = {
  lldp: {
    interface: [
      { eth0: lldpReal.lldp.interface.eth0 },
      {
        enx001122334455: {
          via: "CDPv2", age: "0 day, 00:01:10",
          chassis: { id: { type: "local", value: "sw-core" }, descr: "Cisco IOS", "mgmt-ip": "192.168.1.2", capability: [{ type: "Bridge", enabled: true }, { type: "Router", enabled: false }] },
          port: { id: { type: "ifname", value: "Gi1/0/7" }, descr: "GigabitEthernet1/0/7", ttl: "180", mfs: "1522", power: { supported: false, enabled: true, "device-type": "PSE", pairs: "signal", class: "class 4", requested: "25500", allocated: "25500" } },
          vlan: [{ "vlan-id": "10", pvid: true, value: "lab" }, { "vlan-id": "20", pvid: false }],
        },
      },
    ],
  },
};

test("lldpcli JSON parsing", () => {
  const [n] = parseLldpNeighbors(lldpReal);
  expect(n).toMatchObject({
    iface: "eth0", protocol: "LLDP", name: "USW-Lite-16-PoE", description: "UBNT-USL16L", chassisId: "74:ac:b9:e3:99:6f",
    mgmtIps: ["10.255.0.226", "fe80::76ac:b9ff:fee3:996f"], portId: "Port 15", portDescription: "Port 15", ttl: 120,
    vlans: [{ id: 1, pvid: true, name: "" }], poe: null, medDeviceType: "Network Connectivity Device",
  });
  expect(n.capabilities).toEqual([{ type: "Bridge", enabled: true }]);
  const m = parseLldpNeighbors(lldpMulti);
  expect(m.length).toBe(2);
  expect(m[1]).toMatchObject({ iface: "enx001122334455", protocol: "CDPv2", name: "", chassisId: "sw-core", mgmtIps: ["192.168.1.2"], portId: "Gi1/0/7", ttl: 180, mtu: 1522 });
  expect(m[1].vlans).toEqual([{ id: 10, pvid: true, name: "lab" }, { id: 20, pvid: false, name: "" }]);
  expect(m[1].poe).toMatchObject({ class: "class 4", allocated: "25500", "device-type": "PSE" });
  expect(parseLldpNeighbors({ lldp: {} })).toEqual([]);
});

test("ping parsing", () => {
  const ok = parsePing(`PING 10.255.0.1 (10.255.0.1) 56(84) bytes of data.
64 bytes from 10.255.0.1: icmp_seq=1 ttl=64 time=0.182 ms
64 bytes from 10.255.0.1: icmp_seq=2 ttl=64 time=0.167 ms
64 bytes from 10.255.0.1: icmp_seq=3 ttl=64 time=0.167 ms

--- 10.255.0.1 ping statistics ---
3 packets transmitted, 3 received, 0% packet loss, time 407ms
rtt min/avg/max/mdev = 0.167/0.172/0.182/0.007 ms`, "10.255.0.1");
  expect(ok).toMatchObject({ sent: 3, received: 3, lossPct: 0, min: 0.167, avg: 0.172, max: 0.182, mdev: 0.007, times: [0.182, 0.167, 0.167], tooBig: false });
  const big = parsePing(`PING 10.255.0.1 (10.255.0.1) 1600(1628) bytes of data.
ping: sendmsg: Message too long

--- 10.255.0.1 ping statistics ---
1 packets transmitted, 0 received, +1 errors, 100% packet loss, time 0ms
`);
  expect(big).toMatchObject({ sent: 1, received: 0, errors: 1, lossPct: 100, tooBig: true, avg: null });
  expect(parsePing("--- 10.255.99.99 ping statistics ---\n2 packets transmitted, 0 received, 100% packet loss, time 1032ms\n")).toMatchObject({ sent: 2, received: 0, lossPct: 100 });
  expect(parsePing("ping: unknown host nope")).toMatchObject({ sent: 0, lossPct: 100 });
});

test("iperf3 end-event summaries (real 3.18 --json-stream data, trimmed)", () => {
  const tcpStart = { test_start: { protocol: "TCP", reverse: 0 } };
  const clientEnd = { sum_sent: { seconds: 2.001935, bytes: 3379036160, bits_per_second: 13503080409.7, retransmits: 0 }, sum_received: { seconds: 2.002267, bytes: 3378118656, bits_per_second: 13497175575.5 } };
  expect(iperfSummary(clientEnd, tcpStart, "client")).toMatchObject({ protocol: "TCP", reverse: false, bps: 13497175575.5, retransmits: 0 });
  // Server under -R: it sent, so its sum_received is empty and sum_sent carries the numbers.
  const revStart = { test_start: { protocol: "TCP", reverse: 1 } };
  const srvRevEnd = { sum_sent: { seconds: 1.00238, bytes: 941096960, bits_per_second: 7510899738.6, retransmits: 1 }, sum_received: { seconds: 1.00238, bytes: 0, bits_per_second: 0 } };
  expect(iperfSummary(srvRevEnd, revStart, "server")).toMatchObject({ reverse: true, bps: 7510899738.6, retransmits: 1, bytes: 941096960 });
  const udpEnd = { sum: { seconds: 1.001101, bytes: 1277952, bits_per_second: 10212372, jitter_ms: 0.0247, lost_percent: 0 } };
  expect(iperfSummary(udpEnd, { test_start: { protocol: "UDP", reverse: 1 } }, "client")).toMatchObject({ protocol: "UDP", bps: 10212372, jitterMs: 0.0247, lostPct: 0, retransmits: null });
});

test("formatting and validation", () => {
  expect(fmtBps(941_000_000)).toBe("941.0 Mbit/s");
  expect(fmtBps(13_497_175_575)).toBe("13.5 Gbit/s");
  expect(fmtBps(1_500_000)).toBe("1.50 Mbit/s");
  expect(parseResolvConf("# Generated by NetworkManager\nsearch tsl\nnameserver 10.255.0.1\n")).toEqual({ nameservers: ["10.255.0.1"], search: ["tsl"] });
  expect(validHost("10.255.0.1")).toBe("10.255.0.1");
  expect(validHost("fe80::1%eth0")).toBe("fe80::1%eth0");
  expect(() => validHost("-oProxyCommand=x")).toThrow();
  expect(() => validHost("a b")).toThrow();
});

test("slowJson sends parseable JSON", async () => {
  const r = slowJson(Promise.resolve({ ok: true, x: 1 }));
  expect(JSON.parse(await r.text())).toEqual({ ok: true, x: 1 });
  const e = slowJson(Promise.reject(new Error("boom")));
  expect(JSON.parse(await e.text())).toEqual({ ok: false, message: "boom" });
});

test("path MTU binary search", async () => {
  // A PPPoE-style path: 1492 gets through, 1493 doesn't.
  const r = await mtuSearch(async (p) => p + 28 <= 1492, 1500);
  expect(r.mtu).toBe(1492);
  expect(r.steps[0]).toEqual({ size: 1500, ok: false });
  expect(r.steps.length).toBeLessThanOrEqual(13);
  expect((await mtuSearch(async () => true, 1500)).mtu).toBe(1500);
  expect((await mtuSearch(async () => false, 1500)).mtu).toBeNull();
  expect((await mtuSearch(async (p) => p + 28 <= 9000, 9000)).steps.length).toBe(1);
});

test("iperf3 connect errors in plain words", () => {
  // Real iperf3 3.18 "error" events from desky, 2026-10-18.
  const pre = "unable to connect to server - server may have stopped running or use a different port, firewall issue, etc.: ";
  expect(iperfError(pre + "Connection refused", "steamboat.tsl", 5201)).toBe("connection refused: no iperf3 server listening on steamboat.tsl:5201");
  expect(iperfError(pre + "Connection timed out", "10.255.0.77", 5201)).toMatch(/^no answer from 10\.255\.0\.77:5201 \(timed out\)/);
  expect(iperfError(pre + "Invalid argument", "nosuch.invalid", 5201)).toBe("couldn't connect to nosuch.invalid:5201 (Invalid argument): check the host name");
  expect(iperfError("the server is busy running a test. try again later", "h", 5201)).toMatch(/busy/);
  expect(iperfError("", "h", 1)).toBe("iperf3 gave no result");
});

test("presets: text and list forms", () => {
  const t = "# mine\nsteamboat.tsl steamboat · LAN\n\n10.0.0.5\n";
  expect(parsePresets(t)).toEqual([{ host: "steamboat.tsl", label: "steamboat · LAN" }, { host: "10.0.0.5", label: "10.0.0.5" }]);
  expect(formatPresets(parsePresets(t))).toBe("steamboat.tsl steamboat · LAN\n10.0.0.5");
  expect(parsePresets("gateway Gateway", true)).toEqual([{ host: "gateway", label: "Gateway" }]);
  expect(() => parsePresets("-oX=y bad")).toThrow();
  expect(presetsFrom([{ host: "1.1.1.1", label: "CF\n#x" }])).toEqual([{ host: "1.1.1.1", label: "CF x" }]);
  expect(presetsFrom(formatPresets(DEFAULT_PING_TARGETS), true)).toEqual(DEFAULT_PING_TARGETS);
});
