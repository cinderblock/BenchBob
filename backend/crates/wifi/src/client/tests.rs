//! wifi-client.test.ts: the pure parts of the netns client-test runner. The orchestration
//! itself is exercised on desky (against mac80211_hwsim radios).

use super::*;

fn spec(f: impl FnOnce(&mut TestSpec)) -> TestSpec {
    let mut s = TestSpec::basic("Lab", "psk");
    f(&mut s);
    s
}

fn routes(r: &[(&str, f64)]) -> Vec<(String, f64)> {
    r.iter().map(|(d, m)| (d.to_string(), *m)).collect()
}

#[test]
fn borrow_refusal_rules() {
    let desky = routes(&[("eth0", 100.0), ("wlan0", 600.0)]);
    let wlan0: HashSet<String> = ["wlan0".to_string()].into();
    // desky today: eth0 primary and up → allowed
    assert_eq!(borrow_refusal(&desky, &wlan0, |_| true), None);
    assert!(borrow_refusal(&desky, &wlan0, |_| false).unwrap().contains("eth0 is down"));
    assert!(borrow_refusal(&routes(&[("eth0", 700.0), ("wlan0", 600.0)]), &wlan0, |_| true).unwrap().contains("uplink"));
    // equal metrics → refused (ambiguous)
    assert!(borrow_refusal(&routes(&[("eth0", 600.0), ("wlan0", 600.0)]), &wlan0, |_| true).unwrap().contains("uplink"));
    assert!(borrow_refusal(&routes(&[("wlan0", 600.0)]), &wlan0, |_| true).unwrap().contains("only default route"));
    // radio with no default route (dongle) → allowed even if eth0 is down
    assert_eq!(borrow_refusal(&desky, &["wlan1".to_string()].into(), |_| false), None);
}

#[test]
fn keys() {
    assert_eq!(valid_key("open", None), None);
    assert!(valid_key("psk", None).unwrap().contains("required"));
    assert!(valid_key("psk", Some("short")).unwrap().contains("8–63"));
    assert_eq!(valid_key("psk", Some(&"a".repeat(64))), None); // 64 hex = PMK
    assert!(valid_key("psk", Some(&"g".repeat(64))).unwrap().contains("8–63"));
    assert!(valid_key("psk", Some("pass\nword")).unwrap().contains("printable"));
    assert!(valid_key("sae", Some(&"a".repeat(64))).unwrap().contains("passphrase itself"));
    assert_eq!(valid_key("sae", Some("correct horse")), None);
}

#[test]
fn hosts_and_urls_cant_become_options() {
    assert!(!valid_host("-oProxyCommand=x"));
    assert!(valid_host("example.com"));
    assert!(valid_host("2606:4700::1"));
    assert!(!valid_host("a b"));
    assert!(valid_url("https://example.com/x?y=1"));
    assert!(!valid_url("file:///etc/shadow"));
    assert!(!valid_url("-K/etc/x"));
    // Beyond the TypeScript tests: what `new URL` refuses or accepts.
    assert!(!valid_url("https://"));
    assert!(!valid_url("http://exa mple.com/"));
    assert!(valid_url("HTTP://[2606:4700::1]:8080/"));
    assert!(!valid_url("http://host:99999/"));
}

#[test]
fn wpa_conf_psk_and_hex_ssid() {
    let c = wpa_conf(&spec(|s| s.ssid = "we\"ird\nssid".into()), Some("pa\"ss word"));
    assert!(c.contains(&format!("ssid={}", hex("we\"ird\nssid"))));
    assert!(c.contains("psk=\"pa\"ss word\"")); // wpa_supplicant takes up to the last quote
    assert!(c.contains("key_mgmt=WPA-PSK"));
    assert!(c.contains("p2p_disabled=1"));
}

#[test]
fn wpa_conf_pmk() {
    let k = "AB".repeat(32);
    assert!(wpa_conf(&spec(|_| {}), Some(&k)).contains(&format!("psk={}\n", k.to_lowercase())));
    // transition mode with a PMK degrades to WPA2-PSK: SAE needs the passphrase
    let t = wpa_conf(&spec(|s| s.security = "psk-sae".into()), Some(&k));
    assert!(t.contains("key_mgmt=WPA-PSK\n"));
    assert!(!t.contains("sae_password"));
}

#[test]
fn wpa_conf_other_securities() {
    assert!(
        wpa_conf(&spec(|s| s.security = "sae".into()), Some("secret12"))
            .contains("key_mgmt=SAE\n\tieee80211w=2\n\tsae_password=7365637265743132")
    );
    assert!(wpa_conf(&spec(|s| s.security = "psk-sae".into()), Some("secret12")).contains("key_mgmt=WPA-PSK SAE"));
    assert!(wpa_conf(&spec(|s| s.security = "owe".into()), None).contains("key_mgmt=OWE"));
    assert!(wpa_conf(&spec(|s| s.security = "open".into()), None).contains("key_mgmt=NONE"));
    let e = wpa_conf(
        &spec(|s| {
            s.security = "eap".into();
            s.eap = Some(Eap { method: "TTLS", identity: "bob".into(), anonymous_identity: None });
        }),
        Some("pw"),
    );
    assert!(e.contains("eap=TTLS"));
    assert!(e.contains(&format!("identity={}", hex("bob"))));
    assert!(e.contains(&format!("password={}", hex("pw"))));
    let pinned = wpa_conf(
        &spec(|s| {
            s.security = "open".into();
            s.bssid = Some("aa:bb:cc:dd:ee:ff".into());
        }),
        None,
    );
    assert!(pinned.contains("bssid=aa:bb:cc:dd:ee:ff"));
}

#[test]
fn dhcp_script_writes_the_netns_resolv_conf() {
    let s = dhcp_script("benchbob-wt-3", "/usr/bin/ip");
    assert!(s.contains("/etc/netns/benchbob-wt-3/resolv.conf"));
    assert!(!p::re!(r">\s*/etc/resolv\.conf").is_match(&s));
    assert!(s.contains("echo \"BENCHBOB-DHCP $1|ip=$ip|"));
    assert!(s.contains("${broadcast:+broadcast $broadcast}"));
}

#[tokio::test]
async fn pid_alive_rules() {
    assert!(!pid_alive(std::process::id() as i64));
    assert!(!pid_alive(0));
    assert!(!pid_alive((1 << 22) + 12345)); // beyond pid_max
    let mut child = tokio::process::Command::new("sleep").arg("5").spawn().unwrap();
    assert!(pid_alive(child.id().unwrap() as i64));
    child.kill().await.unwrap();
}

/// Beyond the TypeScript tests: crash recovery removes a dead process's temp dir (it holds a
/// key) under both prefixes and leaves a live one's alone.
#[test]
fn stale_tmp_dirs_removed() {
    let dir = std::env::temp_dir().join(format!("bb-wifi-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.display().to_string();
    let dead = format!("{d}/{TMP_PREFIX}{}-abc", (1 << 22) + 777);
    let legacy = format!("{d}/{LEGACY_TMP}{}-xyz", (1 << 22) + 778);
    let live = format!("{d}/{TMP_PREFIX}1-keep"); // pid 1 is always alive (EPERM counts)
    for p in [&dead, &legacy, &live] {
        std::fs::create_dir_all(p).unwrap();
        std::fs::write(format!("{p}/wpa.conf"), "psk=secret").unwrap();
    }
    let out = remove_stale_tmp(&d);
    assert_eq!(out.len(), 2, "{out:?}");
    assert!(!std::path::Path::new(&dead).exists());
    assert!(!std::path::Path::new(&legacy).exists());
    assert!(std::path::Path::new(&live).exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn mkdtemp_is_private() {
    let t = mkdtemp(&format!("{}/{TMP_PREFIX}{}-", tmpdir(), std::process::id())).unwrap();
    let mode = std::fs::metadata(&t).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    assert!(t.contains(&format!("{TMP_PREFIX}{}-", std::process::id())));
    std::fs::remove_dir(&t).unwrap();
}

/// The result's late keys come out in the order they were first set, as in JavaScript.
#[test]
fn result_key_order() {
    let d = Deps { bins: Arc::new(Bins::resolve()), ctx: None, uplink: Arc::new(|| Box::pin(async { Some("eth0".to_string()) })) };
    let t =
        ClientTest::new("x-1".into(), spec(|_| {}), None, RadioCtx { phy: "phy0".into(), ifaces: vec![], nm: vec![], unmanage: vec![] }, d);
    {
        let mut st = t.st.lock().unwrap();
        st.r.uplink_before = Some("eth0".into());
        st.r.touch("uplinkBefore");
        st.r.cancelled = None;
        st.r.touch("cancelled");
        st.r.error = Some("boom".into());
        st.r.touch("error");
    }
    let j = t.with_r(|r| r.json());
    let keys: Vec<&str> = j.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "id",
            "phy",
            "ns",
            "ssid",
            "security",
            "spec",
            "startedAt",
            "stage",
            "ok",
            "timeline",
            "tests",
            "samples",
            "wpaLog",
            "uplinkBefore",
            "error"
        ]
    );
    assert_eq!(j["ns"], "benchbob-wt-0");
    let sk: Vec<String> = j["spec"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(sk, ["phy", "ssid", "security", "ip", "pingGateway", "dns", "http", "ping", "iperf", "count", "holdS", "assocTimeoutS"]);
}
