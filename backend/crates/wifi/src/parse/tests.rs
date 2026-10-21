//! wifi-parse.test.ts, against the same fixtures: real iw output captured on desky
//! (2026-10-17), plus scan-synthetic.txt for cases the bench's airspace doesn't have (hidden
//! NUL SSID, WEP, OWE, 80/160 MHz, 6 GHz, WPS, WPA1/TKIP, Suite-B enterprise).

use super::*;

pub(crate) fn fx(name: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../peripherals/lib/wifi-fixtures").join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())).replace("\r\n", "\n")
}

#[test]
fn freq_channel_across_bands() {
    assert_eq!(freq_to_channel(2412.0), 1.0);
    assert_eq!(freq_to_channel(2484.0), 14.0);
    assert_eq!(freq_to_channel(5180.0), 36.0);
    assert_eq!(freq_to_channel(5825.0), 165.0);
    assert_eq!(freq_to_channel(5955.0), 1.0);
    assert_eq!(freq_to_channel(5935.0), 2.0);
    assert_eq!(freq_to_channel(7115.0), 233.0);
    assert_eq!(channel_to_freq(42.0, "5"), 5210.0);
    assert_eq!(channel_to_freq(14.0, "2.4"), 2484.0);
    assert_eq!(band_of(5220.0), Some("5"));
    assert_eq!(band_of(6115.0), Some("6"));
    assert_eq!(unii_of(5500.0), Some(("U-NII-2C", true)));
    assert_eq!(unii_of(5745.0).map(|u| u.1), Some(false));
}

#[test]
fn iw_dev_wlan0_and_p2p_wdev() {
    let d = parse_iw_dev(&fx("dev.txt"));
    let w = d.iter().find(|x| x.name.as_deref() == Some("wlan0")).unwrap();
    assert_eq!(w.phy, "phy0");
    assert_eq!(w.ssid.as_deref(), Some("TwillTech"));
    assert_eq!(w.r#type.as_deref(), Some("managed"));
    assert_eq!(w.channel, Some(44.0));
    assert_eq!(w.freq, Some(5220.0));
    assert_eq!(w.width.as_deref(), Some("40 MHz"));
    assert_eq!(w.center1, Some(5230.0));
    assert_eq!(d.iter().find(|x| x.name.is_none()).and_then(|x| x.r#type.as_deref()), Some("P2P-device"));
}

#[test]
fn iw_phy_brcmfmac() {
    let p = parse_iw_phy(&fx("phy-brcmfmac.txt"));
    assert_eq!(p.phy, "phy0");
    assert_eq!(p.bands.iter().map(|b| b.band).collect::<Vec<_>>(), ["2.4", "5"]);
    let b24 = &p.bands[0];
    assert_eq!(b24.channels.len(), 14);
    assert_eq!(b24.channels.iter().filter(|c| c.disabled).map(|c| c.channel).collect::<Vec<_>>(), [12.0, 13.0, 14.0]);
    assert_eq!(b24.channels[0].max_power, Some(20.0));
    assert_eq!(b24.max_width, 40.0);
    let b5 = &p.bands[1];
    assert!(b5.vht);
    assert_eq!(b5.max_width, 80.0); // "neither 160 nor 80+80"
    assert!(b5.channels.iter().find(|c| c.channel == 52.0).unwrap().radar);
    assert!(!b5.channels.iter().find(|c| c.channel == 36.0).unwrap().radar);
    assert!(p.iftypes.iter().any(|x| x == "AP"));
    assert!(!p.monitor);
    assert!(p.ap);
    assert!(p.netns);
    assert!(p.sae);
    assert!(p.ciphers.iter().any(|x| x == "CCMP-128"));
}

#[test]
fn iw_reg_global_and_self_managed_phy() {
    let r = parse_iw_reg(&fx("reg.txt"));
    assert_eq!((r[0].scope.as_str(), r[0].country.as_str(), r[0].dfs_region.as_deref()), ("global", "US", Some("DFS-FCC")));
    let dfs = r[0].rules.iter().find(|x| x.start_mhz == 5250.0).unwrap();
    assert!(dfs.flags.iter().any(|f| f == "DFS"));
    assert_eq!(dfs.dfs_cac_ms, Some(0.0));
    assert_eq!(dfs.max_eirp, Some(24.0));
    assert_eq!((r[1].scope.as_str(), r[1].country.as_str()), ("phy0", "99"));
    let r0 = &r[1].rules[0];
    assert_eq!((r0.start_mhz, r0.end_mhz, r0.max_bw_mhz, r0.max_eirp), (2402.0, 2482.0, 40.0, Some(20.0)));
}

fn desky_scan() -> Vec<Bss> {
    parse_iw_scan(&fx("scan-desky-wlan0.txt"))
}

#[test]
fn scan_every_bss_parsed() {
    let list = desky_scan();
    assert_eq!(list.len(), 30);
    for b in &list {
        assert!(re!(r"^[0-9a-f:]{17}$").is_match(&b.bssid));
        assert!(b.freq > 2400.0);
        assert!(b.channel > 0.0);
        assert!(b.signal.is_some());
    }
}

#[test]
fn scan_associated_bss() {
    let list = desky_scan();
    let a = list.iter().find(|b| b.associated).unwrap();
    assert_eq!(a.bssid, "00:00:00:00:00:01");
    assert_eq!(a.ssid, "TwillTech");
    assert_eq!((a.channel, a.band, a.width.clone(), a.center_freq, a.center_channel), (44.0, Some("5"), Width::Mhz(40.0), 5230.0, 46.0));
    assert_eq!(a.signal, Some(-56.0));
    assert_eq!(a.security, "WPA2-PSK");
    assert_eq!(a.security_kind, "psk");
    assert_eq!(a.standards, ["n", "ac", "ax"]);
    assert_eq!(a.generation, Some("Wi-Fi 6"));
    assert_eq!(a.stations, Some(3.0));
    assert!((a.utilization.unwrap() - 5.5).abs() < 0.05);
    assert_eq!(a.country.as_deref(), Some("US"));
    assert_eq!(a.last_seen_ms, Some(0.0));
}

#[test]
fn scan_security_variants() {
    let list = desky_scan();
    assert_eq!(list.iter().find(|b| b.ssid == "Net-01").unwrap().security, "WPA3-SAE");
    assert_eq!(list.iter().find(|b| b.bssid == "02:00:00:00:00:05").unwrap().security_kind, "psk-sae");
    let ent = list.iter().find(|b| b.bssid == "02:00:00:00:00:02").unwrap();
    assert_eq!(ent.security_kind, "eap");
    assert_eq!(ent.akm, ["FT/802.1X", "802.1X/SHA-256"]);
}

#[test]
fn scan_wpa_rsn_mixed_and_widths() {
    let list = desky_scan();
    let sp = list.iter().find(|b| b.ssid == "Net-05").unwrap();
    assert_eq!(sp.security, "WPA/WPA2-PSK");
    assert_eq!((sp.channel, sp.band, sp.width.clone()), (1.0, Some("2.4"), Width::Mhz(20.0)));
    let b = list.iter().find(|x| x.bssid == "00:00:00:00:00:02").unwrap();
    assert_eq!((b.channel, b.center_channel), (40.0, 38.0));
    let hidden: Vec<_> = list.iter().filter(|b| b.hidden).collect();
    assert_eq!(hidden.len(), 3);
    assert!(hidden.iter().all(|h| h.ssid.is_empty()));
    assert!(list.iter().find(|b| b.bssid == "02:00:00:00:00:01").unwrap().local_mac);
    assert!(!list.iter().find(|b| b.bssid == "00:00:00:00:00:01").unwrap().local_mac);
}

#[test]
fn scan_synthetic_edge_cases() {
    let list = parse_iw_scan(&fx("scan-synthetic.txt"));
    let by = |mac: &str| list.iter().find(|b| b.bssid.ends_with(mac)).unwrap();
    assert_eq!(list.len(), 7);
    let b = by("55");
    assert!(b.hidden);
    assert_eq!((b.ssid.as_str(), b.security, b.standards.clone(), b.channel), ("", "Open", vec!["b"], 6.0));
    let b = by("66");
    assert!(b.ssid.starts_with(" Café "));
    assert!(b.ssid.ends_with(' '));
    assert_eq!(b.security, "WEP");
    assert_eq!(b.standards, ["g"]);
    let b = by("77");
    assert_eq!(b.security, "WPA-PSK");
    assert_eq!(b.pairwise, ["TKIP"]);
    let w = b.wps.as_ref().unwrap();
    assert_eq!(
        (w.state.as_deref(), w.locked, w.manufacturer.as_deref(), w.model.as_deref(), w.device.as_deref()),
        (Some("Configured"), Some(true), Some("ACME"), Some("R100"), Some("ACME Router"))
    );
    assert_eq!((b.width.clone(), b.center_channel), (Width::Mhz(40.0), 9.0));
    let b = by("88");
    assert_eq!((b.security_kind, b.pmf), ("owe", "required"));
    assert_eq!((b.width.clone(), b.center_channel, b.center_freq), (Width::Mhz(80.0), 42.0, 5210.0));
    assert_eq!(b.standards, ["n", "ac"]);
    let b = by("99");
    assert_eq!((b.width.clone(), b.center_channel, b.security), (Width::Mhz(160.0), 114.0, "WPA3-Enterprise"));
    assert_eq!(b.generation, Some("Wi-Fi 6"));
    let b = by("aa");
    assert_eq!(
        (b.band, b.channel, b.width.clone(), b.center_channel, b.security, b.generation),
        (Some("6"), 33.0, Width::Mhz(80.0), 39.0, "WPA3-SAE", Some("Wi-Fi 6E"))
    );
    assert!(b.akm.iter().any(|a| a == "SAE-EXT-KEY"));
    let b = by("bb");
    assert_eq!((b.security, b.channel), ("WEP", 3.0));
}

#[test]
fn link_station_and_survey() {
    let f = fx("link-station.txt");
    let (link, station) = f.split_once("====").unwrap();
    let l = parse_link(link, station);
    assert!(l.connected);
    assert_eq!(l.bssid.as_deref(), Some("00:00:00:00:00:01"));
    assert_eq!(l.ssid.as_deref(), Some("TwillTech"));
    assert_eq!((l.freq, l.signal, l.rx_bitrate, l.tx_bitrate), (Some(5220.0), Some(-57.0), Some(200.0), Some(180.0)));
    assert_eq!((l.tx_failed, l.connected_s), (Some(0.0), Some(49.0)));
    assert!(!parse_link("Not connected.", "").connected);

    let s = parse_survey(
        "Survey data from wlan1\n\tfrequency:\t\t\t2412 MHz [in use]\n\tnoise:\t\t\t\t-95 dBm\n\tchannel active time:\t\t1000 ms\n\tchannel busy time:\t\t250 ms\n\tchannel receive time:\t\t200 ms\n\tchannel transmit time:\t\t10 ms\nSurvey data from wlan1\n\tfrequency:\t\t\t2417 MHz\n",
    );
    assert_eq!(s.len(), 2);
    assert_eq!((s[0].freq, s[0].in_use, s[0].noise, s[0].busy_pct), (2412.0, true, Some(-95.0), Some(25.0)));
    assert_eq!(s[1].busy_pct, None);
    assert!(parse_survey("").is_empty());
}

#[test]
fn wpa_connect_sequence() {
    let wpa = [
        "Successfully initialized wpa_supplicant",
        "Failed to create interface p2p-dev-wlan0: -16 (Device or resource busy)",
        "wlan0: Trying to associate with 00:00:00:00:00:01 (SSID='TwillTech' freq=5220 MHz)",
        "wlan0: Associated with 00:00:00:00:00:01",
        "wlan0: WPA: Key negotiation completed with 00:00:00:00:00:01 [PTK=CCMP GTK=CCMP]",
        "wlan0: CTRL-EVENT-CONNECTED - Connection to 00:00:00:00:00:01 completed [id=0 id_str=]",
    ];
    let kinds: Vec<_> = wpa.iter().map(|l| parse_wpa_line(l, &[]).unwrap().kind).collect();
    assert_eq!(kinds, ["info", "info", "assoc-start", "assoc", "handshake", "connected"]);
    let e = parse_wpa_line(wpa[2], &[]).unwrap();
    assert_eq!((e.bssid.as_deref(), e.freq), (Some("00:00:00:00:00:01"), Some(5220.0)));
    assert_eq!(parse_wpa_line(wpa[4], &[]).unwrap().detail.as_deref(), Some("PTK=CCMP GTK=CCMP"));
}

#[test]
fn wpa_failures() {
    let p = |l: &str| parse_wpa_line(l, &[]).unwrap();
    assert_eq!(p("wlan0: WPA: 4-Way Handshake failed - pre-shared key may be incorrect").kind, "wrong-key");
    let e = p("wlan0: CTRL-EVENT-SSID-TEMP-DISABLED id=0 ssid=\"X\" auth_failures=1 duration=10 reason=WRONG_KEY");
    assert_eq!((e.kind, e.detail.as_deref()), ("temp-disabled", Some("WRONG_KEY")));
    let e = p("wlan0: CTRL-EVENT-DISCONNECTED bssid=aa:bb:cc:dd:ee:ff reason=15");
    assert_eq!((e.kind, e.reason), ("disconnected", Some(15.0)));
    let e = p("wlan0: CTRL-EVENT-ASSOC-REJECT bssid=aa:bb:cc:dd:ee:ff status_code=17");
    assert_eq!((e.kind, e.status, e.detail.as_deref()), ("assoc-reject", Some(17.0), Some("AP full (too many stations)")));
    assert_eq!(p("wlan0: CTRL-EVENT-NETWORK-NOT-FOUND").kind, "not-found");
    // Wrong SAE password, captured from wpa_supplicant against a simulated WPA3 AP on desky
    let e = p("wlan1: CTRL-EVENT-AUTH-REJECT 02:00:00:00:01:00 auth_type=3 auth_transaction=2 status_code=1");
    assert_eq!((e.kind, e.status, e.bssid.as_deref()), ("auth-reject", Some(1.0), Some("02:00:00:00:01:00")));
}

#[test]
fn wpa_secrets_redacted() {
    let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string();
    let e =
        parse_wpa_line(&format!("Line 5: Invalid passphrase length 64 (expected: 8..63) '{key}\"'."), std::slice::from_ref(&key)).unwrap();
    assert_eq!(e.kind, "config-error");
    assert!(!e.text.contains(&key));
    assert!(!parse_wpa_line("Line 5: failed to parse psk=\"hunter22\"", &[]).unwrap().text.contains("hunter22"));
}

#[test]
fn udhcpc_lines() {
    let p = |l: &str| parse_udhcpc_line(l).unwrap();
    assert_eq!(p("udhcpc: broadcasting discover").kind, "discover");
    let e = p("udhcpc: broadcasting select for 10.255.3.139, server 10.255.0.1");
    assert_eq!((e.kind, e.ip.as_deref(), e.server.as_deref()), ("offer", Some("10.255.3.139"), Some("10.255.0.1")));
    let e = p("udhcpc: lease of 10.255.3.139 obtained from 10.255.0.1, lease time 86400");
    assert_eq!((e.kind, e.lease_s), ("ack", Some(86400.0)));
    assert_eq!(p("udhcpc: no lease, failing").kind, "fail");
    let l = p(
        "BENCHBOB-DHCP bound|ip=10.255.3.139|mask=20|router=10.255.0.1|dns=10.255.0.1 1.1.1.1|lease=86400|serverid=10.255.0.1|domain=tsl",
    );
    let lease = l.lease.unwrap();
    assert_eq!(lease["event"], "bound");
    assert_eq!(lease["ip"], "10.255.3.139");
    assert_eq!(lease["mask"], "20");
    assert_eq!(lease["dns"], "10.255.0.1 1.1.1.1");
    assert_eq!(lease["domain"], "tsl");
    assert_eq!(mask_to_prefix("20"), 20.0);
    assert_eq!(mask_to_prefix("255.255.240.0"), 20.0);
}

#[test]
fn ping_output() {
    let p = parse_ping(
        "PING 10.255.0.1 (10.255.0.1) 56(84) bytes of data.\n64 bytes from 10.255.0.1: icmp_seq=1 ttl=64 time=14.9 ms\n\n--- 10.255.0.1 ping statistics ---\n3 packets transmitted, 3 received, 0% packet loss, time 602ms\nrtt min/avg/max/mdev = 4.873/8.225/14.905/4.723 ms",
    );
    assert_eq!((p.sent, p.received, p.loss_pct, p.min, p.avg, p.max), (3.0, 3.0, 0.0, Some(4.873), Some(8.225), Some(14.905)));
    let bad = parse_ping("ping: connect: Network is unreachable");
    assert_eq!((bad.loss_pct, bad.error.as_deref()), (100.0, Some("connect: Network is unreachable")));
    assert_eq!(parse_ping("5 packets transmitted, 3 received, +2 errors, 40% packet loss, time 4005ms").loss_pct, 40.0);
}

#[test]
fn busybox_nslookup() {
    let n = parse_nslookup(
        "Server:\t\t10.255.0.1\nAddress:\t10.255.0.1:53\n\nNon-authoritative answer:\nName:\texample.com\nAddress: 104.20.23.154\nName:\texample.com\nAddress: 172.66.147.243\n\nNon-authoritative answer:\nName:\texample.com\nAddress: 2606:4700:10::6814:179a\n",
    );
    assert_eq!(n.server.as_deref(), Some("10.255.0.1"));
    assert_eq!(n.addresses, ["104.20.23.154", "172.66.147.243", "2606:4700:10::6814:179a"]);
    assert_eq!(n.error, None);
    assert!(parse_nslookup(";; connection timed out; no servers could be reached").error.unwrap().contains("timed out"));
    assert!(
        parse_nslookup("Server:\t\t1.1.1.1\nAddress:\t1.1.1.1:53\n\n** server can't find nope.invalid: NXDOMAIN")
            .error
            .unwrap()
            .contains("NXDOMAIN")
    );
}

#[test]
fn curl_json() {
    let ok = parse_curl_json(
        r#"{"http_code":200,"remote_ip":"1.2.3.4","time_namelookup":0.01,"time_connect":0.02,"time_appconnect":0.05,"time_starttransfer":0.08,"time_total":0.1,"size_download":512,"speed_download":5120,"http_version":"2"}"#,
    );
    assert_eq!((ok.ok, ok.code, ok.dns_ms, ok.tls_ms, ok.total_ms), (true, 200.0, Some(10.0), Some(50.0), Some(100.0)));
    let bad = parse_curl_json(r#"{"http_code":0,"errormsg":"Could not resolve host: example.com"}"#);
    assert_eq!((bad.ok, bad.error.as_deref()), (false, Some("Could not resolve host: example.com")));
    assert!(!parse_curl_json("curl: (6) nope").ok);
    assert_eq!(
        serde_json::to_string(&ok.json()).unwrap(),
        r#"{"code":200,"ok":true,"remoteIp":"1.2.3.4","httpVersion":"2","dnsMs":10,"connectMs":20,"tlsMs":50,"firstByteMs":80,"totalMs":100,"bytes":512,"speedBps":5120}"#
    );
}

#[test]
fn iperf_json() {
    let r = parse_iperf(r#"{"end":{"sum_sent":{"bits_per_second":5e7,"retransmits":3},"sum_received":{"bits_per_second":4.9e7}}}"#);
    assert_eq!(serde_json::to_string(&r.json()).unwrap(), r#"{"sentBps":50000000,"receivedBps":49000000,"retransmits":3}"#);
    assert!(parse_iperf(r#"{"error":"unable to connect to server"}"#).error.unwrap().contains("unable"));
}

#[test]
fn oui_formats() {
    let m = parse_oui(
        "F4-E2-C6   (hex)\t\tUbiquiti Inc\nF4E2C6     (base 16)\t\tUbiquiti Inc\nDC:A6:32\tRaspberr\tRaspberry Pi Trading Ltd\n001122 Cimsys Inc\n",
    );
    assert_eq!(m["f4e2c6"], "Ubiquiti Inc");
    assert_eq!(m["dca632"], "Raspberry Pi Trading Ltd");
    assert_eq!(m["001122"], "Cimsys Inc");
    // IEEE's (and Debian's) oui.txt has CRLF line ends; the TypeScript found nothing in it.
    let crlf = parse_oui("F4-E2-C6   (hex)\t\tUbiquiti Inc\r\nF4E2C6     (base 16)\t\tUbiquiti Inc\r\n");
    assert_eq!(crlf.get("f4e2c6").map(String::as_str), Some("Ubiquiti Inc"));
}

#[test]
fn iw_phy_hwsim() {
    let p = parse_iw_phy(&fx("phy-hwsim.txt"));
    assert_eq!(p.bands.iter().map(|b| b.band).collect::<Vec<_>>(), ["2.4", "5", "6"]);
    assert!(p.monitor);
    let b6 = p.bands.iter().find(|b| b.band == "6").unwrap();
    assert!(b6.he);
    assert!(b6.channels.len() > 50);
    assert_eq!(b6.channels[0].channel, 1.0);
}

#[test]
fn history_trimming() {
    let h = [(0.0, -70.0), (500.0, -71.0), (900.0, -72.0), (1000.0, -73.0)];
    assert_eq!(trim_history(&h, 1000.0, 600.0, 10), vec![(500.0, -71.0), (900.0, -72.0), (1000.0, -73.0)]);
    assert_eq!(trim_history(&h, 1000.0, 5000.0, 2), vec![(900.0, -72.0), (1000.0, -73.0)]);
    assert!(trim_history(&[], 1000.0, 600.0, 10).is_empty());
}

#[test]
fn scan_due_rules() {
    let base = ScanDue {
        now: 100_000.0,
        last_scan: 70_000.0,
        busy: false,
        auto_enabled: false,
        auto_interval_s: 60.0,
        watching: false,
        watch_interval_s: 30.0,
    };
    assert!(!scan_due(&base)); // nobody asked
    assert!(scan_due(&ScanDue { watching: true, ..base })); // 30 s elapsed
    assert!(!scan_due(&ScanDue { watching: true, busy: true, ..base }));
    assert!(!scan_due(&ScanDue { auto_enabled: true, ..base }));
    assert!(scan_due(&ScanDue { auto_enabled: true, watching: true, ..base }));
    assert!(!scan_due(&ScanDue { watching: true, watch_interval_s: 5.0, last_scan: 90_000.0, ..base })); // floor 15 s
}

/// Beyond the TypeScript tests: the BSS JSON keeps parseIwScan's key order and leaves out
/// what it leaves undefined, so the API's bytes match the Bun server's.
#[test]
fn bss_json_shape() {
    let list = parse_iw_scan(&fx("scan-synthetic.txt"));
    let b = list.iter().find(|b| b.bssid.ends_with("77")).unwrap();
    let keys: Vec<String> = b.json().as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys.first().map(String::as_str), Some("bssid"));
    assert_eq!(keys.last().map(String::as_str), Some("localMac"));
    assert!(keys.iter().position(|k| k == "wps") > keys.iter().position(|k| k == "country"));
    let w = &b.json()["wps"];
    assert_eq!(
        serde_json::to_string(w).unwrap(),
        r#"{"state":"Configured","locked":true,"device":"ACME Router","manufacturer":"ACME","model":"R100"}"#
    );
    let h = list.iter().find(|b| b.bssid.ends_with("55")).unwrap().json();
    assert!(h.get("ssidHex").is_none());
    let c = list.iter().find(|b| b.bssid.ends_with("66")).unwrap().json();
    // " Café " decodes as clean text: no ssidHex
    assert!(c.get("ssidHex").is_none());
}

/// The parsers' results for live captures, as the TypeScript objects would serialise, for
/// comparing against `bun` running wifi-parse.ts on the same files (captures can carry
/// neighbours' SSIDs, so they stay out of the repo). Run with
/// `BENCHBOB_WIFI_CAPTURE=<dir> cargo test -p benchbob-wifi capture_dump -- --ignored`;
/// writes <dir>/rust.json.
#[test]
#[ignore]
fn capture_dump() {
    use crate::json::O;
    let dir = std::env::var("BENCHBOB_WIFI_CAPTURE").expect("BENCHBOB_WIFI_CAPTURE");
    let rd = |n: &str| std::fs::read_to_string(format!("{dir}/{n}")).unwrap_or_default();
    let dev: Vec<Value> = parse_iw_dev(&rd("wb-dev.txt"))
        .iter()
        .map(|i| {
            O::new()
                .k("phy", i.phy.clone())
                .k("name", i.name.clone().map(Value::from).unwrap_or(Value::Null))
                .on("ifindex", i.ifindex)
                .o("wdev", i.wdev.clone())
                .o("addr", i.addr.clone())
                .o("ssid", i.ssid.clone())
                .o("type", i.r#type.clone())
                .on("txpower", i.txpower)
                .on("channel", i.channel)
                .on("freq", i.freq)
                .o("width", i.width.clone())
                .on("center1", i.center1)
                .v()
        })
        .collect();
    let p = parse_iw_phy(&rd("wb-phy.txt"));
    let phy = O::new()
        .k("phy", p.phy.clone())
        .k("iftypes", p.iftypes.clone())
        .k("softwareIftypes", p.software_iftypes.clone())
        .k("ciphers", p.ciphers.clone())
        .k("commands", p.commands.clone())
        .k(
            "bands",
            p.bands
                .iter()
                .map(|b| {
                    O::new()
                        .k("band", b.band)
                        .k("ht", b.ht)
                        .k("vht", b.vht)
                        .k("he", b.he)
                        .k("eht", b.eht)
                        .n("maxWidth", b.max_width)
                        .k(
                            "channels",
                            b.channels
                                .iter()
                                .map(|c| {
                                    O::new()
                                        .n("mhz", c.mhz)
                                        .n("channel", c.channel)
                                        .k("band", c.band)
                                        .k("disabled", c.disabled)
                                        .k("radar", c.radar)
                                        .k("noIR", c.no_ir)
                                        .on("maxPower", c.max_power)
                                        .k("flags", c.flags.clone())
                                        .v()
                                })
                                .collect::<Vec<_>>(),
                        )
                        .v()
                })
                .collect::<Vec<_>>(),
        )
        .k("monitor", p.monitor)
        .k("ap", p.ap)
        .k("netns", p.netns)
        .k("sae", p.sae)
        .k("extFeatures", p.ext_features.clone())
        .v();
    let out = O::new()
        .k("dev", dev)
        .k("phy", phy)
        .k("reg", parse_iw_reg(&rd("wb-reg.txt")).iter().map(RegDomain::json).collect::<Vec<_>>())
        .k("scan", parse_iw_scan(&rd("wb-scan.txt")).iter().map(Bss::json).collect::<Vec<_>>())
        .k("survey", parse_survey(&rd("wb-survey.txt")).iter().map(Survey::json).collect::<Vec<_>>())
        .k("link", parse_link(&rd("wb-link.txt"), &rd("wb-sta.txt")).json())
        .v();
    std::fs::write(format!("{dir}/rust.json"), serde_json::to_string_pretty(&out).unwrap()).unwrap();
}
