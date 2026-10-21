//! peripherals/drivers/netdiag.test.ts with the same fixtures (real output captured on desky,
//! Pi 4, bcmgenet, eth0, unless noted), plus slowJson's stream, the HTTP probe and the
//! driver's JSON.

use serde_json::{Value, json};

use crate::parse::*;

const ETHTOOL: &str = "netlink error: Operation not permitted
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
";

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn ethtool_link_parsing() {
    let l = parse_ethtool(ETHTOOL);
    assert_eq!(l.speed, Some(1000));
    assert_eq!(l.duplex, "Full");
    assert_eq!((l.autoneg, l.link), (Some(true), Some(true)));
    assert_eq!(l.port, "Twisted Pair");
    assert_eq!(l.supported, s(&["10baseT/Half", "10baseT/Full", "100baseT/Half", "100baseT/Full", "1000baseT/Half", "1000baseT/Full"]));
    assert_eq!(l.partner, s(&["10baseT/Half", "10baseT/Full", "100baseT/Half", "100baseT/Full", "1000baseT/Full"]));
    assert_eq!(l.partner_autoneg, Some(true));
    assert_eq!(l.master_slave, "slave");
    // No link: speed and duplex are "Unknown!"
    let down = parse_ethtool("Settings for eth1:\n\tSpeed: Unknown!\n\tDuplex: Unknown! (255)\n\tLink detected: no\n");
    assert_eq!((down.speed, down.duplex.as_str(), down.link), (None, "", Some(false)));
    // The JSON the UI reads, in ethtool.ts's key order.
    let v = serde_json::to_string(&down).unwrap();
    assert_eq!(
        v,
        r#"{"speed":null,"duplex":"","autoneg":null,"link":false,"port":"","supported":[],"advertised":[],"partner":[],"partnerAutoneg":null,"partnerPause":"","masterSlave":"","mdix":""}"#
    );
}

#[test]
fn ethtool_info() {
    let i = parse_key_colon(
        "driver: bcmgenet\nversion: 6.18.50+rpt-rpi-v8\nfirmware-version: \nbus-info: fd580000.ethernet\nsupports-statistics: yes\n",
    );
    assert_eq!(i["driver"], "bcmgenet");
    assert_eq!(i["firmware-version"], "");
    assert_eq!(i["bus-info"], "fd580000.ethernet");
}

const STATS: &str = "NIC statistics:
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
";

#[test]
fn ethtool_stats_and_error_counters() {
    let st = parse_ethtool_stats(STATS);
    assert_eq!(st["rx_bytes"], 17515802); // first of the duplicate pair
    assert_eq!(st["mdf_err_cnt"], 6368);
    let errs: Vec<&str> = st.keys().filter(|k| is_error_counter(k)).map(String::as_str).collect();
    assert_eq!(
        errs,
        [
            "rx_errors",
            "rx_dropped",
            "rx_crc_errors",
            "rx_fcs",
            "tx_late_col",
            "rx_runt_pkts",
            "rbuf_ovflow_cnt",
            "rxq0_errors",
            "rxq0_dropped"
        ]
    );
    assert!(!is_error_counter("rx_packets"));
    assert!(!is_error_counter("rx_1522_2047_oct"));
}

#[test]
fn eee_parsing() {
    let e = parse_eee(
        "EEE settings for eth0:
\tEEE status: enabled - inactive
\tTx LPI: 34 (us)
\tSupported EEE link modes:  100baseT/Full
\t                           1000baseT/Full
\tAdvertised EEE link modes:  100baseT/Full
\t                            1000baseT/Full
\tLink partner advertised EEE link modes:  Not reported
",
    );
    assert_eq!(
        e,
        Some(Eee {
            supported: true,
            enabled: true,
            active: false,
            tx_lpi_us: Some(34),
            advertised: s(&["100baseT/Full", "1000baseT/Full"]),
            partner: vec![]
        })
    );
    assert_eq!(parse_eee("netlink error: Operation not supported"), None);
}

/// Real neighbour learned on desky's eth0 (UniFi switch), lldpd 1.0.18.
fn lldp_real() -> Value {
    json!({
        "lldp": { "interface": { "eth0": {
            "via": "LLDP", "rid": "1", "age": "0 day, 00:00:04",
            "chassis": { "USW-Lite-16-PoE": { "id": { "type": "mac", "value": "74:ac:b9:e3:99:6f" }, "descr": "UBNT-USL16L", "mgmt-ip": ["10.255.0.226", "fe80::76ac:b9ff:fee3:996f"], "capability": { "type": "Bridge", "enabled": true } } },
            "port": { "id": { "type": "local", "value": "Port 15" }, "descr": "Port 15", "ttl": "120" },
            "vlan": { "vlan-id": "1", "pvid": true },
            "lldp-med": { "device-type": "Network Connectivity Device", "capability": [{ "type": "Capabilities", "available": true }, { "type": "Policy", "available": true }] }
        } } }
    })
}

#[test]
fn lldpcli_json_parsing() {
    let real = lldp_real();
    let n = &parse_lldp_neighbors(&real)[0];
    assert_eq!(
        *n,
        json!({
            "iface": "eth0", "protocol": "LLDP", "age": "0 day, 00:00:04", "name": "USW-Lite-16-PoE", "description": "UBNT-USL16L",
            "chassisId": "74:ac:b9:e3:99:6f", "chassisIdType": "mac", "mgmtIps": ["10.255.0.226", "fe80::76ac:b9ff:fee3:996f"],
            "capabilities": [{ "type": "Bridge", "enabled": true }], "portId": "Port 15", "portIdType": "local", "portDescription": "Port 15",
            "ttl": 120, "vlans": [{ "id": 1, "pvid": true, "name": "" }], "mtu": null, "poe": null, "medDeviceType": "Network Connectivity Device"
        })
    );
    // Key order as the TypeScript builds the object.
    let keys: Vec<&str> = n.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "iface",
            "protocol",
            "age",
            "name",
            "description",
            "chassisId",
            "chassisIdType",
            "mgmtIps",
            "capabilities",
            "portId",
            "portIdType",
            "portDescription",
            "ttl",
            "vlans",
            "mtu",
            "poe",
            "medDeviceType"
        ]
    );
    // Representative two-port case: array form, unnamed chassis, CDP, PoE, several VLANs.
    let multi = json!({ "lldp": { "interface": [
        { "eth0": real["lldp"]["interface"]["eth0"] },
        { "enx001122334455": {
            "via": "CDPv2", "age": "0 day, 00:01:10",
            "chassis": { "id": { "type": "local", "value": "sw-core" }, "descr": "Cisco IOS", "mgmt-ip": "192.168.1.2", "capability": [{ "type": "Bridge", "enabled": true }, { "type": "Router", "enabled": false }] },
            "port": { "id": { "type": "ifname", "value": "Gi1/0/7" }, "descr": "GigabitEthernet1/0/7", "ttl": "180", "mfs": "1522", "power": { "supported": false, "enabled": true, "device-type": "PSE", "pairs": "signal", "class": "class 4", "requested": "25500", "allocated": "25500" } },
            "vlan": [{ "vlan-id": "10", "pvid": true, "value": "lab" }, { "vlan-id": "20", "pvid": false }]
        } }
    ] } });
    let m = parse_lldp_neighbors(&multi);
    assert_eq!(m.len(), 2);
    let b = &m[1];
    assert_eq!(b["iface"], "enx001122334455");
    assert_eq!(b["protocol"], "CDPv2");
    assert_eq!(b["name"], "");
    assert_eq!(b["chassisId"], "sw-core");
    assert_eq!(b["mgmtIps"], json!(["192.168.1.2"]));
    assert_eq!(b["portId"], "Gi1/0/7");
    assert_eq!(b["ttl"], 180);
    assert_eq!(b["mtu"], 1522);
    assert_eq!(b["vlans"], json!([{ "id": 10, "pvid": true, "name": "lab" }, { "id": 20, "pvid": false, "name": "" }]));
    assert_eq!(b["poe"]["class"], "class 4");
    assert_eq!(b["poe"]["allocated"], "25500");
    assert_eq!(b["poe"]["device-type"], "PSE");
    assert_eq!(b["poe"]["supported"], "false");
    assert!(parse_lldp_neighbors(&json!({ "lldp": {} })).is_empty());
}

#[test]
fn ping_parsing() {
    let ok = parse_ping(
        "PING 10.255.0.1 (10.255.0.1) 56(84) bytes of data.
64 bytes from 10.255.0.1: icmp_seq=1 ttl=64 time=0.182 ms
64 bytes from 10.255.0.1: icmp_seq=2 ttl=64 time=0.167 ms
64 bytes from 10.255.0.1: icmp_seq=3 ttl=64 time=0.167 ms

--- 10.255.0.1 ping statistics ---
3 packets transmitted, 3 received, 0% packet loss, time 407ms
rtt min/avg/max/mdev = 0.167/0.172/0.182/0.007 ms",
        "10.255.0.1",
    );
    assert_eq!((ok.sent, ok.received, ok.loss_pct), (3, 3, 0.0));
    assert_eq!((ok.min, ok.avg, ok.max, ok.mdev), (Some(0.167), Some(0.172), Some(0.182), Some(0.007)));
    assert_eq!(ok.times, [0.182, 0.167, 0.167]);
    assert!(!ok.too_big);
    assert_eq!(
        serde_json::to_string(&Value::Object(ok.to_json())).unwrap(),
        r#"{"host":"10.255.0.1","sent":3,"received":3,"lossPct":0,"min":0.167,"avg":0.172,"max":0.182,"mdev":0.007,"errors":0,"times":[0.182,0.167,0.167],"tooBig":false}"#
    );
    let big = parse_ping(
        "PING 10.255.0.1 (10.255.0.1) 1600(1628) bytes of data.
ping: sendmsg: Message too long

--- 10.255.0.1 ping statistics ---
1 packets transmitted, 0 received, +1 errors, 100% packet loss, time 0ms
",
        "",
    );
    assert_eq!((big.sent, big.received, big.errors, big.loss_pct, big.too_big, big.avg), (1, 0, 1, 100.0, true, None));
    let lost = parse_ping("--- 10.255.99.99 ping statistics ---\n2 packets transmitted, 0 received, 100% packet loss, time 1032ms\n", "");
    assert_eq!((lost.sent, lost.received, lost.loss_pct), (2, 0, 100.0));
    let unknown = parse_ping("ping: unknown host nope", "");
    assert_eq!((unknown.sent, unknown.loss_pct), (0, 100.0));
}

#[test]
fn iperf3_end_summaries() {
    // Real 3.18 --json-stream data, trimmed.
    let tcp_start = json!({ "test_start": { "protocol": "TCP", "reverse": 0 } });
    let client_end = json!({ "sum_sent": { "seconds": 2.001935, "bytes": 3379036160u64, "bits_per_second": 13503080409.7, "retransmits": 0 }, "sum_received": { "seconds": 2.002267, "bytes": 3378118656u64, "bits_per_second": 13497175575.5 } });
    let c = iperf_summary(&client_end, &tcp_start, false);
    assert_eq!(
        (c["protocol"].clone(), c["reverse"].clone(), c["bps"].clone(), c["retransmits"].clone()),
        (json!("TCP"), json!(false), json!(13497175575.5), json!(0))
    );
    // Server under -R: it sent, so its sum_received is empty and sum_sent carries the numbers.
    let rev_start = json!({ "test_start": { "protocol": "TCP", "reverse": 1 } });
    let srv_rev_end = json!({ "sum_sent": { "seconds": 1.00238, "bytes": 941096960, "bits_per_second": 7510899738.6, "retransmits": 1 }, "sum_received": { "seconds": 1.00238, "bytes": 0, "bits_per_second": 0 } });
    let r = iperf_summary(&srv_rev_end, &rev_start, true);
    assert_eq!(
        (r["reverse"].clone(), r["bps"].clone(), r["retransmits"].clone(), r["bytes"].clone()),
        (json!(true), json!(7510899738.6), json!(1), json!(941096960))
    );
    let udp_end =
        json!({ "sum": { "seconds": 1.001101, "bytes": 1277952, "bits_per_second": 10212372, "jitter_ms": 0.0247, "lost_percent": 0 } });
    let u = iperf_summary(&udp_end, &json!({ "test_start": { "protocol": "UDP", "reverse": 1 } }), false);
    assert_eq!(
        serde_json::to_string(&u).unwrap(),
        r#"{"protocol":"UDP","reverse":true,"seconds":1.001101,"bytes":1277952,"bps":10212372,"retransmits":null,"jitterMs":0.0247,"lostPct":0}"#
    );
}

#[test]
fn formatting_and_validation() {
    assert_eq!(fmt_bps(941_000_000.0), "941.0 Mbit/s");
    assert_eq!(fmt_bps(13_497_175_575.0), "13.5 Gbit/s");
    assert_eq!(fmt_bps(1_500_000.0), "1.50 Mbit/s");
    assert_eq!(fmt_bps(0.0), "0 bit/s");
    assert_eq!(fmt_bps(512.0), "512 bit/s");
    assert_eq!(parse_resolv_conf("# Generated by NetworkManager\nsearch tsl\nnameserver 10.255.0.1\n"), (s(&["10.255.0.1"]), s(&["tsl"])));
    assert_eq!(valid_host("10.255.0.1").unwrap(), "10.255.0.1");
    assert_eq!(valid_host("fe80::1%eth0").unwrap(), "fe80::1%eth0");
    assert!(valid_host("-oProxyCommand=x").is_err());
    assert_eq!(valid_host("a b").unwrap_err(), "\"a b\" isn't a hostname or IP address");
}

#[test]
fn mtu_binary_search() {
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(async {
        // A PPPoE-style path: 1492 gets through, 1493 doesn't.
        let (mtu, steps) = mtu_search(|p| async move { p + 28 <= 1492 }, 1500).await;
        assert_eq!(mtu, Some(1492));
        assert_eq!(steps[0], (1500, false));
        assert!(steps.len() <= 13);
        assert_eq!(mtu_search(|_| async { true }, 1500).await.0, Some(1500));
        assert_eq!(mtu_search(|_| async { false }, 1500).await.0, None);
        assert_eq!(mtu_search(|p| async move { p + 28 <= 9000 }, 9000).await.1.len(), 1);
    });
}

#[test]
fn iperf3_connect_errors_in_plain_words() {
    // Real iperf3 3.18 "error" events from desky, 2026-10-18.
    let pre = "unable to connect to server - server may have stopped running or use a different port, firewall issue, etc.: ";
    assert_eq!(
        iperf_error(&format!("{pre}Connection refused"), "lab-server.lan", 5201),
        "connection refused: no iperf3 server listening on lab-server.lan:5201"
    );
    assert!(
        iperf_error(&format!("{pre}Connection timed out"), "10.255.0.77", 5201).starts_with("no answer from 10.255.0.77:5201 (timed out)")
    );
    assert_eq!(
        iperf_error(&format!("{pre}Invalid argument"), "nosuch.invalid", 5201),
        "couldn't connect to nosuch.invalid:5201 (Invalid argument): check the host name"
    );
    assert!(iperf_error("the server is busy running a test. try again later", "h", 5201).contains("busy"));
    assert_eq!(iperf_error("", "h", 1), "iperf3 gave no result");
    assert!(iperf_error("test authorization failed", "iperf.example.net", 5201).contains("refused the login"));
}

#[test]
fn presets_text_and_list_forms() {
    let t = "# mine\nlab-server.lan Lab server · LAN\n\n10.0.0.5\n";
    let p = parse_presets(t, false).unwrap();
    assert_eq!(
        p,
        vec![
            Preset { host: "lab-server.lan".into(), label: "Lab server · LAN".into() },
            Preset { host: "10.0.0.5".into(), label: "10.0.0.5".into() }
        ]
    );
    assert_eq!(format_presets(&p), "lab-server.lan Lab server · LAN\n10.0.0.5");
    assert_eq!(parse_presets("gateway Gateway", true).unwrap(), vec![Preset { host: "gateway".into(), label: "Gateway".into() }]);
    assert!(parse_presets("-oX=y bad", false).is_err());
    assert_eq!(
        presets_from(&json!([{ "host": "1.1.1.1", "label": "CF\n#x" }]), false).unwrap(),
        vec![Preset { host: "1.1.1.1".into(), label: "CF x".into() }]
    );
    assert_eq!(presets_from(&json!(format_presets(&default_ping_targets())), true).unwrap(), default_ping_targets());
    assert!(presets_from(&json!(5), false).is_err());
    let many: String = (0..17).map(|i| format!("10.0.0.{i}\n")).collect();
    assert_eq!(parse_presets(&many, false).unwrap_err(), "At most 16 presets");
}

#[test]
fn iperf_logins() {
    // username,sha256 of {username}password (iperf3's users-file format)
    assert_eq!(iperf_auth_line("mario", "rossi"), "mario,bf7a49a846d44b454a5d11e7acfaf13d138bbe0b7483aa3e050879700572709b");
    assert!(valid_iperf_user("benchbob"));
    assert!(!valid_iperf_user("a,b"));
    assert!(!valid_iperf_user(""));
    // openssl -pubout PEM (CRLF too); private keys refused
    let pem = "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAuS1=\nIDAQAB\n-----END PUBLIC KEY-----";
    assert_eq!(iperf_public_key(pem).unwrap(), format!("{pem}\n"));
    assert_eq!(iperf_public_key(&(pem.replace('\n', "\r\n") + "\r\n")).unwrap(), format!("{pem}\n"));
    assert!(iperf_public_key(&pem.replace("PUBLIC", "RSA PRIVATE")).is_err());
    assert!(iperf_public_key("").is_err());
    assert_eq!(base64url(&[0xfb, 0xff, 0xbf]), "-_-_");
    assert_eq!(base64url(&[0u8; 24]).len(), 32);
    assert_eq!(base64url(b"ab"), "YWI");
}

#[test]
fn lldp_multicast_check() {
    let mc = "2    eth0            1     0     333300000001\n2    eth0            1     0     01005e000001\n2    eth0            1     0     0180c200000e\n3    wlan0           1     0     333300000001\n";
    assert!(has_lldp_multicast(mc, "eth0"));
    assert!(!has_lldp_multicast(mc, "wlan0"));
    assert!(!has_lldp_multicast(&mc.replace("0180c200000e", "0180c2000003"), "eth0"));
}

#[test]
fn radix36_ids() {
    assert_eq!(crate::radix36(1792225996360), "mvc531ag");
    assert_eq!(crate::radix36(35), "z");
}

#[tokio::test]
async fn slow_json_sends_parseable_json() {
    use benchbob_core::HttpError;
    let body = |r: axum::response::Response| async move {
        assert_eq!(r.headers()["content-type"], "application/json");
        assert_eq!(r.headers()["x-accel-buffering"], "no");
        let b = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice::<Value>(&b).unwrap()
    };
    assert_eq!(body(crate::routes::slow_json(async { Ok(json!({ "ok": true, "x": 1 })) })).await, json!({ "ok": true, "x": 1 }));
    assert_eq!(body(crate::routes::slow_json(async { Err(HttpError::new(400, "boom")) })).await, json!({ "ok": false, "message": "boom" }));
    assert_eq!(body(crate::routes::slow_json(async { Ok(Value::Null) })).await, json!({ "ok": true }));
}

/// Keep-alive spaces flow while the work runs (paused clock: 9 s pass instantly).
#[tokio::test(start_paused = true)]
async fn slow_json_keeps_alive() {
    let r = crate::routes::slow_json(async {
        tokio::time::sleep(std::time::Duration::from_secs(9)).await;
        Ok(json!({ "ok": true }))
    });
    let b = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&b[..], b"  {\"ok\":true}");
}

/// The HTTP check against a local server: the status and timing come from the headers, and
/// a body that never ends doesn't hold it up.
#[tokio::test]
async fn http_probe_stops_at_headers() {
    if benchbob_core::run::which("curl").is_none() {
        return;
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut c, _)) = l.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = c.read(&mut buf).await;
                let _ = c.write_all(b"HTTP/1.1 204 No Content\r\nX-A: b\r\n\r\n").await;
                // An endless trickle that a full download would wait for.
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            });
        }
    });
    let (status, ms) = crate::http_probe(&format!("http://127.0.0.1:{port}/"), std::time::Duration::from_secs(6)).await.unwrap();
    assert_eq!(status, 204);
    assert!(ms < 3000, "{ms}");
    let e = crate::http_probe("http://127.0.0.1:1/", std::time::Duration::from_secs(6)).await.unwrap_err();
    assert!(e.to_lowercase().contains("connect"), "{e}");
}

/// The status JSON before anything has been polled, in netdiag.ts's key order.
#[tokio::test]
async fn status_shape() {
    use std::sync::Arc;

    use benchbob_core::Ctx;
    use benchbob_core::driver::Hub;
    use benchbob_core::settings::Settings;

    let t = tempfile::tempdir().unwrap();
    let nd = crate::Netdiag::new();
    let hub = Arc::new(Hub::new(Settings::load(t.path().join("peripherals.json"))));
    let _ = nd.inner.ctx.set(Ctx::new("netdiag", hub));
    let st = nd.inner.status();
    assert_eq!(
        st,
        json!({
            "active": false, "summary": "no wired interface", "ifaces": [], "gateways": [], "dns": { "nameservers": [], "search": [] },
            "lldp": { "enabled": true, "running": false, "since": 0, "neighbors": [], "error": "" },
            "iperf": { "server": { "running": false, "port": 5201, "startedAt": 0, "offAt": 0, "current": null, "tests": [], "log": [] }, "runs": [] },
            "checks": { "pings": [] },
            "presets": { "iperf": [], "ping": [{ "host": "gateway", "label": "Gateway" }, { "host": "1.1.1.1", "label": "1.1.1.1" }, { "host": "8.8.8.8", "label": "8.8.8.8" }], "auth": [] }
        })
    );
    let keys: Vec<&str> = st.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["active", "summary", "ifaces", "gateways", "dns", "lldp", "iperf", "checks", "presets"]);
    // Validation errors the UI shows.
    let e = nd.inner.start_run(&json!({ "host": "10.0.0.5", "duration": 61 })).unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (400, "duration must be 1–60 s"));
    assert_eq!(nd.inner.start_run(&json!({ "host": "-x" })).unwrap_err().message, "\"-x\" isn't a hostname or IP address");
    assert_eq!(nd.inner.ping(None, 10.0).await.unwrap_err().message, "No default gateway to ping; pass a host");
    assert_eq!(nd.inner.ping(Some(&json!("1.1.1.1")), 60.0).await.unwrap_err().message, "count must be 1–50");
    let a = crate::Inner::auth_view("h", json!({ "username": "mario", "password": "rossi", "publicKey": "" }).as_object().unwrap());
    assert_eq!(
        Value::Object(a),
        json!({ "host": "h", "username": "mario", "pkcs1": false, "hasPassword": true, "hasKey": false, "ready": false, "usersLine": "mario,bf7a49a846d44b454a5d11e7acfaf13d138bbe0b7483aa3e050879700572709b" })
    );
}
