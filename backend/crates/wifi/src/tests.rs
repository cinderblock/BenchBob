//! Driver-level tests: the JSON shapes the UI relies on, request normalisation and profile
//! storage. (The TypeScript driver had no tests of its own; these cover what matters for
//! wire compatibility.)

use std::sync::Arc;

use benchbob_core::driver::Hub;
use benchbob_core::settings::Settings;
use serde_json::json;

use super::*;

fn with_ctx() -> (Arc<Inner>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let hub = Arc::new(Hub::new(Settings::load(dir.path().join("peripherals.json"))));
    let w = Wifi::new();
    let _ = w.inner.ctx.set(Ctx::new("wifi", hub));
    (w.inner, dir)
}

fn fx(name: &str) -> String {
    crate::parse::tests::fx(name)
}

/// A State with desky's radio as `iw` describes it in the fixtures.
fn desky_state() -> State {
    let mut st = State { reg: p::parse_iw_reg(&fx("reg.txt")), ..Default::default() };
    let ifaces: Vec<IwIface> = p::parse_iw_dev(&fx("dev.txt")).into_iter().filter(|i| i.phy == "phy0").collect();
    st.radios.insert(
        "phy0".into(),
        Radio {
            phy: "phy0".into(),
            stable_id: "sdio:brcmfmac:/devices/platform/soc/fe300000.mmcnr/mmc_host/mmc1/mmc1:0001/mmc1:0001:1".into(),
            driver: "brcmfmac".into(),
            bus: "sdio",
            onboard: true,
            usb: None,
            ifaces,
            info: Some(p::parse_iw_phy(&fx("phy-brcmfmac.txt"))),
            nm: vec![NmRow { iface: "wlan0".into(), state: "connected".into(), connection: "TwillTech".into() }],
            use_: ("idle", String::new()),
            unmanaged_by_us: vec![],
            busy: None,
            prefs: Map::new(),
        },
    );
    st.routes = vec![RouteRec { dev: "eth0".into(), metric: 100.0 }, RouteRec { dev: "wlan0".into(), metric: 600.0 }];
    let r = st.radios["phy0"].clone();
    let u = st.describe_use(&r);
    st.radios.get_mut("phy0").unwrap().use_ = u;
    st
}

#[test]
fn nm_split_unescapes_colons() {
    assert_eq!(nm_split(r"wlan0:wifi:connected:Net\:5G"), ["wlan0", "wifi", "connected", "Net:5G"]);
    assert_eq!(nm_split(""), [""]);
}

#[test]
fn str_list_rules() {
    assert_eq!(str_list(Some(&json!("a.com, b.com c.com")), "x").unwrap(), ["a.com", "b.com", "c.com"]);
    assert_eq!(str_list(Some(&json!([" a ", "", "b"])), "x").unwrap(), ["a", "b"]);
    assert!(str_list(Some(&json!("")), "x").unwrap().is_empty());
    assert!(str_list(None, "x").unwrap().is_empty());
    let e = str_list(Some(&json!((0..11).map(|i| format!("h{i}")).collect::<Vec<_>>())), "URLs").unwrap_err();
    assert_eq!(e.message, "At most 10 URLs");
}

#[test]
fn backup_uplink_use_and_status_summary() {
    let st = desky_state();
    let r = &st.radios["phy0"];
    assert_eq!(r.use_, ("backup", "Backup uplink, connected to TwillTech".to_string()));
    assert_eq!(managed_iface(r).as_deref(), Some("wlan0"));
}

#[test]
fn radio_json_shape() {
    let st = desky_state();
    let full = st.radio_json(&st.radios["phy0"], true);
    let keys: Vec<&str> = full.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "phy",
            "stableId",
            "label",
            "driver",
            "bus",
            "onboard",
            "ifaces",
            "nm",
            "use",
            "busy",
            "unmanagedByUs",
            "bands",
            "monitor",
            "ap",
            "netns",
            "prefs",
            "regCountry",
            "iftypes",
            "ciphers",
            "sae",
            "bandInfo",
            "reg"
        ]
    );
    assert_eq!(full["regCountry"], "99");
    assert_eq!(full["bands"], json!(["2.4", "5"]));
    // wlan0 plus the P2P wdev (name null)
    assert_eq!(full["ifaces"][0]["name"], "wlan0");
    assert_eq!(full["ifaces"][1]["name"], Value::Null);
    let ch = &full["bandInfo"][0]["channels"][0];
    assert_eq!(
        serde_json::to_string(ch).unwrap(),
        r#"{"ch":1,"mhz":2412,"disabled":false,"radar":false,"noIR":false,"maxPower":20,"flags":[]}"#
    );
    let short = st.radio_json(&st.radios["phy0"], false);
    assert!(short.get("bandInfo").is_none());
    let rj = st.radios_json();
    assert_eq!(rj["uplink"], "eth0");
    assert_eq!(rj["reg"]["country"], "US");
    assert_eq!(rj["reg"]["changedFrom"], Value::Null);
    assert_eq!(rj["reg"]["phys"][0]["scope"], "phy0");
}

#[test]
fn bss_records_and_history_since() {
    let mut st = desky_state();
    for b in p::parse_iw_scan(&fx("scan-desky-wlan0.txt")) {
        st.bss.insert(
            b.bssid.clone(),
            BssRec {
                radio: "phy0".into(),
                vendor: None,
                first_seen: 1000,
                last_seen: 2000.0,
                history: vec![(1000.0, -60.0), (2000.0, -61.0)],
                b,
            },
        );
    }
    let j = Inner::scan_json(&mut st, &ScanFilter { history_since: Some(1500.0), limit: Some(3.0), ..Default::default() });
    let bss = j["bss"].as_array().unwrap();
    assert_eq!(bss.len(), 3);
    let keys: Vec<&str> = bss[0].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(&keys[..4], ["firstSeen", "history", "lastSeen", "bssid"]);
    assert_eq!(keys.last(), Some(&"radio"));
    assert_eq!(bss[0]["history"], json!([[2000, -61]]));
    // strongest first
    let s: Vec<f64> = bss.iter().map(|b| b["signal"].as_f64().unwrap()).collect();
    assert!(s.windows(2).all(|w| w[0] >= w[1]));
    assert_eq!(j["historySince"], 1500);
    assert_eq!(j["historyMaxAgeMs"], 3600000);
    assert_eq!(j["historyMax"], 360);
    let keys: Vec<&str> = j.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["scan", "now", "historySince", "historyMaxAgeMs", "historyMax", "bss", "ouiSource"]);
    let all = Inner::scan_json(&mut st, &ScanFilter { band: Some("2.4".into()), ..Default::default() });
    assert!(all["bss"].as_array().unwrap().iter().all(|b| b["band"] == "2.4"));
}

#[test]
fn status_shape() {
    let (i, _d) = with_ctx();
    *i.st.lock().unwrap() = desky_state();
    let s = i.status();
    let keys: Vec<&str> = s.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["active", "summary", "radios", "radiosSeq", "reg", "scan", "running", "resultsSeq", "lastResult", "profilesCount"]);
    assert_eq!(s["summary"], "wlan0 backup uplink");
    assert_eq!(s["active"], false);
    let sk: Vec<&str> = s["scan"].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(sk, ["seq", "at", "radio", "count", "method", "durationMs", "scanning", "bssTotal", "auto", "watchers"]);
    assert_eq!(
        serde_json::to_string(&s["scan"]["auto"]).unwrap(),
        r#"{"enabled":false,"intervalS":60,"radio":"","method":"auto","watchIntervalS":30}"#
    );
}

#[tokio::test]
async fn profiles_keep_and_clear_keys() {
    let (i, _d) = with_ctx();
    let p = i.save_profile(vec![("name", Some(json!("Lab"))), ("ssid", Some(json!("LabNet"))), ("psk", Some(json!("secret123")))]);
    let id = p["id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 8);
    assert_eq!(serde_json::to_string(&mask_profile(&p)).unwrap(), format!(r#"{{"name":"Lab","ssid":"LabNet","id":"{id}","hasPsk":true}}"#));
    // An edit without a key keeps the stored one, in its place.
    let p2 = i.save_profile(vec![
        ("id", Some(json!(id))),
        ("name", Some(json!("Lab"))),
        ("ssid", Some(json!("LabNet2"))),
        ("count", Some(json!(3))),
    ]);
    assert_eq!(
        serde_json::to_string(&p2).unwrap(),
        format!(r#"{{"name":"Lab","ssid":"LabNet2","psk":"secret123","id":"{id}","count":3}}"#)
    );
    // null clears it
    let p3 = i.save_profile(vec![("id", Some(json!(id))), ("psk", Some(Value::Null))]);
    assert!(p3.get("psk").is_none());
    assert_eq!(i.profiles().len(), 1);
    // undefined (None) removes a key the stored profile had
    let p4 = i.save_profile(vec![("name", Some(json!("Lab"))), ("count", None)]);
    assert!(p4.get("count").is_none());
    assert_eq!(p4["id"], id);
}

#[tokio::test]
async fn resolve_spec_defaults_and_errors() {
    let (i, _d) = with_ctx();
    *i.st.lock().unwrap() = desky_state();
    // The scanned security wins when none is given; a key is then required.
    {
        let mut st = i.st.lock().unwrap();
        for b in p::parse_iw_scan(&fx("scan-desky-wlan0.txt")) {
            st.bss
                .insert(b.bssid.clone(), BssRec { radio: "phy0".into(), vendor: None, first_seen: 0, last_seen: 0.0, history: vec![], b });
        }
    }
    let e = i.resolve_spec(&json!({ "ssid": "TwillTech" })).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (400, "A key (passphrase) is required for psk"));
    let (spec, key, save) = i.resolve_spec(&json!({ "ssid": "TwillTech", "psk": "correct horse", "saveAs": " Mine " })).await.unwrap();
    assert_eq!(key.as_deref(), Some("correct horse"));
    assert_eq!(save.as_deref(), Some("Mine"));
    assert_eq!(
        serde_json::to_string(&spec.json()).unwrap(),
        r#"{"phy":"phy0","ssid":"TwillTech","security":"psk","ip":"dhcp","pingGateway":true,"dns":["example.com"],"http":["https://example.com/"],"ping":[],"iperf":null,"count":5,"holdS":0,"assocTimeoutS":30,"keySource":"given"}"#
    );
    let open =
        i.resolve_spec(&json!({ "ssid": "Lab", "security": "open", "count": 500, "ip": "none", "dns": "a.com,b.com" })).await.unwrap().0;
    assert_eq!((open.count, open.ip.as_str()), (50.0, "none"));
    assert_eq!(open.dns, ["a.com", "b.com"]);
    let bad = |v: Value| {
        let i = i.clone();
        async move { i.resolve_spec(&v).await.unwrap_err().message }
    };
    assert_eq!(bad(json!({ "ssid": "" })).await, "ssid is required (1–32 bytes)");
    assert_eq!(bad(json!({ "ssid": "x".repeat(33) })).await, "ssid is required (1–32 bytes)");
    assert_eq!(bad(json!({ "ssid": "L", "security": "wep" })).await, "WEP isn't supported");
    assert_eq!(bad(json!({ "ssid": "L", "security": "open", "ping": ["-x"] })).await, "Bad host name \"-x\"");
    assert_eq!(bad(json!({ "ssid": "L", "security": "open", "http": ["ftp://x/"] })).await, "Bad URL \"ftp://x/\" (http or https only)");
    assert_eq!(
        bad(json!({ "ssid": "L", "security": "open", "ip": "static", "static": { "address": "10.0.0.5" } })).await,
        "static.address must look like 192.168.1.50/24"
    );
    assert_eq!(bad(json!({ "ssid": "L", "security": "open", "bssid": "nope" })).await, "bssid must look like aa:bb:cc:dd:ee:ff");
    assert_eq!(bad(json!({ "ssid": "L", "security": "eap", "psk": "pw" })).await, "eap.identity is required for Enterprise");
    assert_eq!(bad(json!({ "ssid": "L", "radio": "wlan9" })).await, "No Wi-Fi radio \"wlan9\"");
    assert_eq!(bad(json!({ "ssid": "L", "profile": "nope" })).await, "No profile \"nope\"");
}

#[test]
fn base36_like_javascript() {
    assert_eq!(base36(0), "0");
    assert_eq!(base36(1792225996360), "mvc531ag");
}
