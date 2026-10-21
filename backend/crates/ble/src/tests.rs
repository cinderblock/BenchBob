//! Driver-level tests: the status, list and WebSocket snapshot shapes the UI reads.

use std::sync::Arc;

use benchbob_core::driver::Hub;
use benchbob_core::settings::Settings;
use serde_json::json;

use super::*;
use crate::decode::{from_hex, parse_hci_event};

fn with_ctx() -> (Arc<Inner>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let hub = Arc::new(Hub::new(Settings::load(dir.path().join("peripherals.json"))));
    let b = Ble::new();
    let _ = b.inner.ctx.set(Ctx::new("ble", hub));
    (b.inner, dir)
}

fn feed(i: &Inner, hex: &str, ts: f64) {
    let mut st = i.st.lock().unwrap();
    for r in parse_hci_event(&from_hex(hex), ts, 0) {
        st.tracker.ingest(&r, "hci0");
    }
}

#[test]
fn status_shape_without_controllers() {
    let (i, _d) = with_ctx();
    let s = i.status();
    assert_eq!(
        serde_json::to_string(&s).unwrap(),
        r#"{"active":false,"summary":"No controller","controllers":[],"defaultController":"","restoreRadio":true,"scan":null,"lastScan":null,"recent":0,"known":0}"#
    );
    i.st.lock().unwrap().controllers.push(Controller {
        name: "hci0".into(),
        index: Some(0),
        devpath: "/devices/platform/soc/fe201000.serial/serial0/serial0-0/bluetooth/hci0".into(),
        bus: "onboard (UART)".into(),
        address: "DC:A6:32:03:FC:38".into(),
        powered: Some(true),
        power_state: "on".into(),
        rfkill: Some(Rfkill { index: 0, soft: false, hard: false }),
    });
    assert_eq!(i.status()["summary"], "hci0 idle");
    assert_eq!(
        serde_json::to_string(&i.status()["controllers"][0]).unwrap(),
        r#"{"name":"hci0","index":0,"devpath":"/devices/platform/soc/fe201000.serial/serial0/serial0-0/bluetooth/hci0","bus":"onboard (UART)","address":"DC:A6:32:03:FC:38","powered":true,"powerState":"on","rfkill":{"index":0,"soft":false,"hard":false}}"#
    );
    assert_eq!(i.pick_controller(None).unwrap().name, "hci0");
    assert_eq!(i.pick_controller(Some("dc:a6:32:03:fc:38")).unwrap().name, "hci0");
    assert_eq!(i.pick_controller(Some("hci9")).unwrap_err().message, "No Bluetooth controller \"hci9\"");
}

#[test]
fn compact_listing_and_snapshot() {
    let (i, _d) = with_ctx();
    let now = now_ms() as f64;
    // A connectable advertiser with flags, then an iBeacon (non-connectable).
    feed(&i, "3e 0f 02 01 00 01 66 55 44 33 22 c1 03 02 01 06 b5", now - 2000.0);
    feed(
        &i,
        "3e 2a 02 01 03 00 01 02 03 04 05 06 1b 1a ff 4c 00 02 15 e2 c5 6d b5 df fb 48 d2 b0 60 d0 f5 a7 10 96 e0 00 01 00 02 c5 c0",
        now,
    );
    let l = i.list_devices(None, Some(60.0), None, None, false);
    assert_eq!(l.len(), 2);
    // strongest first: -64 (0xc0) before -75 (0xb5)
    assert_eq!(l[0]["addr"], "06:05:04:03:02:01");
    assert_eq!(
        l[0]["manufacturers"],
        json!([{ "company": "0x004c", "name": "Apple", "data": "0215e2c56db5dffb48d2b060d0f5a71096e000010002c5" }])
    );
    assert!(l[0]["kind"].as_str().unwrap().starts_with("iBeacon E2C56DB5-DFFB-48D2-B060-D0F5A71096E0 major 1 minor 2"));
    let keys: Vec<&str> = l[1].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["addr", "addrType", "rssi", "connectable", "lastSeenAgo_s", "seenFor_s", "packets"]);
    assert_eq!(l[1]["lastSeenAgo_s"], 2);
    assert_eq!(i.list_devices(None, None, None, None, true).len(), 1);
    assert_eq!(i.list_devices(None, None, Some(-70.0), None, false).len(), 1);
    assert_eq!(i.list_devices(None, None, None, Some(1.0), false).len(), 1);
    let snap = Inner::snapshot(&i.st.lock().unwrap());
    assert_eq!(snap["type"], "devices");
    assert_eq!(snap["packets"], 2);
    assert_eq!(snap["devices"][1]["kind"], "");
    assert_eq!(snap["devices"][0]["beacons"][0]["kind"], "ibeacon");
    assert_eq!(i.status()["recent"], 2);
    assert_eq!(i.status()["summary"], "2 devices heard recently");
}
