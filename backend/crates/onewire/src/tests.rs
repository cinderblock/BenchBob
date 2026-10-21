//! w1-families.test.ts "onewire driver on a fixture tree", plus the bridge binding and
//! views the TypeScript didn't test.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::http::{HeaderMap, Method};
use benchbob_core::driver::Hub;
use benchbob_core::settings::Settings;
use benchbob_core::{Ctx, Driver, HttpError, Reply, Request, StopReason, ToolResult, udev::UdevEvent};
use serde_json::{Value, json};

use super::*;
use crate::fixtures::{FIX, build_w1_fixture};

struct T {
    drv: OneWire,
    dir: tempfile::TempDir,
    hub: Arc<Hub>,
}

impl T {
    fn root(&self) -> std::path::PathBuf {
        self.dir.path().join("w1")
    }
}

async fn start() -> T {
    let dir = tempfile::tempdir().unwrap();
    build_w1_fixture(&dir.path().join("w1")).unwrap();
    let sys = dir.path().join("sys");
    std::fs::create_dir_all(sys.join("bus/i2c/devices/i2c-1")).unwrap();
    let drv = OneWire::with(dir.path().join("w1"), true, sys, dir.path().join("dt"), None);
    let hub = Arc::new(Hub::new(Settings::load(dir.path().join("peripherals.json"))));
    hub.settings.own("onewire");
    drv.init(Ctx::new("onewire", hub.clone())).await.unwrap();
    // Wait for the first background cycle to read every slave.
    for _ in 0..100 {
        let st = drv.status().unwrap();
        if st["slaves"].as_array().unwrap().iter().all(|s| s["readAt"] != 0 || s["error"] != "") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    T { drv, dir, hub }
}

async fn call(t: &T, method: Method, path: &str, body: Value, query: Vec<(&str, &str)>) -> Result<Value, HttpError> {
    let routes = t.drv.routes();
    let (r, params) = routes
        .iter()
        .filter(|r| r.method == method)
        .filter_map(|r| benchbob_core::http::match_path(&r.path, path).map(|p| (r, p)))
        .min_by_key(|(_, p)| p.len())
        .unwrap_or_else(|| panic!("no route {method} {path}"));
    let req = Request {
        method,
        path: path.into(),
        params: params.into_iter().collect::<HashMap<_, _>>(),
        query: query.into_iter().map(|(k, v)| (k.into(), v.into())).collect(),
        headers: HeaderMap::new(),
        body: if body.is_null() { Default::default() } else { body.to_string().into() },
    };
    match (r.handler)(req).await? {
        Reply::Json(v) => Ok(v),
        Reply::Response(_) => panic!("not JSON"),
    }
}

async fn tool(t: &T, name: &str, args: Value) -> Result<Value, HttpError> {
    let tools = t.drv.tools();
    match (tools.iter().find(|x| x.name == name).unwrap().handler)(args).await? {
        ToolResult::Value(v) => Ok(v),
        _ => panic!("content"),
    }
}

fn slave(t: &T, id: &str) -> Value {
    t.drv.status().unwrap()["slaves"].as_array().unwrap().iter().find(|s| s["id"] == id).cloned().unwrap_or(Value::Null)
}

fn v(t: &T, id: &str, k: &str) -> Value {
    slave(t, id)["values"].as_array().unwrap().iter().find(|x| x["k"] == k).map(|x| x["v"].clone()).unwrap_or(Value::Null)
}

fn read(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn ghost_family_00_ids_are_hidden_and_counted_as_noise() {
    let t = start().await;
    let st = t.drv.status().unwrap();
    assert!(slave(&t, FIX.ghost).is_null());
    assert_eq!(st["masters"][0]["ghosts"], 1);
    assert_eq!(st["active"], true);
    assert!(st["summary"].as_str().unwrap().contains("devices: 23.1 °C"), "{}", st["summary"]);
    assert_eq!(st["summary"].as_str().unwrap().split(" · ").next(), Some("Fixture bus (BENCHBOB_W1_ROOT)"));
    let keys: Vec<&String> = st.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["active", "summary", "root", "fixture", "interval", "bulk", "masters", "slaves", "touchLog", "bridges"]);
    let m = &st["masters"][0];
    assert_eq!(
        *m,
        json!({ "id": "w1_bus_master1", "kind": "gpio", "label": "Fixture bus (BENCHBOB_W1_ROOT)", "slaves": 19, "search": -1, "searchText": "continuous", "pullup": true, "timeout": "10", "ghosts": 1, "bulk": true, "error": "" })
    );
    t.drv.stop(StopReason::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn every_family_decodes() {
    let t = start().await;
    assert_eq!(v(&t, FIX.ds18b20, "t"), json!(23.125));
    assert!(slave(&t, FIX.ds18b20por)["warn"].to_string().contains("power-on"));
    assert_eq!(v(&t, FIX.ds18s20, "t"), json!(25));
    assert_eq!(v(&t, FIX.ds1822, "t"), json!(-10.5));
    assert_eq!(v(&t, FIX.max31850, "cj"), json!(24.0625));
    assert_eq!(v(&t, FIX.ds28ea00, "t"), json!(21.5));
    assert_eq!(v(&t, FIX.ds2438, "vdd"), json!(4.98));
    assert_eq!(slave(&t, FIX.ds2408)["pins"].as_array().unwrap().len(), 8);
    assert_eq!(slave(&t, FIX.ds2413)["pins"][1]["out"], true);
    assert_eq!(slave(&t, FIX.ds2406)["pins"][0]["out"], false);
    assert_eq!(v(&t, FIX.ds2423, "c14"), json!(12345));
    assert_eq!(v(&t, FIX.ds1904, "time"), json!("2026-10-17 12:00:00"));
    assert_eq!(slave(&t, FIX.ibutton)["name"], "DS2401 / DS1990A");
    assert!(slave(&t, FIX.ds1420)["values"][0]["v"].as_str().unwrap().contains("DS9490R"));
    assert_eq!(slave(&t, FIX.unknown)["name"], "EDS00xx");
    for s in t.drv.status().unwrap()["slaves"].as_array().unwrap() {
        assert_eq!(s["romOk"], true, "{}", s["id"]);
        assert_eq!(s["error"], "", "{}", s["id"]);
    }
    let keys: Vec<String> = slave(&t, FIX.ds18b20).as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        [
            "id",
            "family",
            "name",
            "kind",
            "desc",
            "module",
            "label",
            "master",
            "present",
            "romOk",
            "firstSeen",
            "lastSeen",
            "readAt",
            "values",
            "pins",
            "warn",
            "error",
            "hist",
            "outputs",
            "eepromSize",
            "resolution"
        ]
    );
    assert_eq!(slave(&t, FIX.ds18b20)["values"][3], json!({ "k": "power", "label": "Power", "v": "external" }));
    t.drv.stop(StopReason::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bulk_conversion_was_triggered() {
    let t = start().await;
    assert_eq!(read(&t.root().join("w1_bus_master1/therm_bulk_read")), b"trigger\n");
    t.drv.stop(StopReason::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn eeprom_read_via_rest() {
    let t = start().await;
    let out = call(&t, Method::POST, &format!("/api/onewire/{}/read", FIX.ds28ec20), Value::Null, vec![("eeprom", "1")]).await.unwrap();
    let e = out["eeprom"].as_array().unwrap();
    assert_eq!(e.len(), 2560);
    let tag: String = e[..16].iter().map(|x| x.as_u64().unwrap() as u8 as char).collect();
    assert_eq!(tag, "DS28EC20 fixture");
    t.drv.stop(StopReason::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn outputs_need_confirm_then_write_the_latch_byte() {
    let t = start().await;
    let p = format!("/api/onewire/{}/output", FIX.ds2413);
    let e = call(&t, Method::POST, &p, json!({ "pin": "A", "on": true }), vec![]).await.unwrap_err();
    assert!(e.message.contains("confirm"));
    let d = call(&t, Method::POST, &p, json!({ "pin": "A", "on": true, "confirm": true }), vec![]).await.unwrap();
    // A was off (latch 1), B on (latch 0): turning A on → bits 00 → 0xFC
    assert_eq!(read(&t.root().join(FIX.ds2413).join("output")), [0xfc]);
    let keys: Vec<&String> = d.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["values", "pins", "warn"]);
    call(&t, Method::POST, &format!("/api/onewire/{}/output", FIX.ds2408), json!({ "pin": 3, "on": true, "confirm": true }), vec![])
        .await
        .unwrap();
    assert_eq!(read(&t.root().join(FIX.ds2408).join("output")), [0xf7]);
    let e = call(&t, Method::POST, &format!("/api/onewire/{}/output", FIX.ds2408), json!({ "pin": 9, "confirm": true }), vec![])
        .await
        .unwrap_err();
    assert_eq!(e.message, "pin must be 0–7");
    let e = call(&t, Method::POST, &format!("/api/onewire/{}/output", FIX.ds18b20), json!({ "pin": 1, "confirm": true }), vec![])
        .await
        .unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (409, "DS18B20 has no switchable outputs"));
    t.drv.stop(StopReason::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ibutton_touch_and_release_land_in_the_touch_log() {
    let t = start().await;
    let list = t.root().join("w1_bus_master1/w1_master_slaves");
    let all = String::from_utf8(read(&list)).unwrap();
    let ev = |a: &str| UdevEvent { action: a.into(), props: [("SUBSYSTEM".to_string(), "w1".to_string())].into_iter().collect() };
    std::fs::write(&list, all.replace(&format!("{}\n", FIX.ibutton), "")).unwrap();
    t.drv.udev(&ev("remove"));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    std::fs::write(&list, &all).unwrap();
    t.drv.udev(&ev("add"));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let st = t.drv.status().unwrap();
    let log: Vec<&str> =
        st["touchLog"].as_array().unwrap().iter().filter(|x| x["id"] == FIX.ibutton).map(|x| x["event"].as_str().unwrap()).collect();
    assert_eq!(log, ["touch", "release"]); // newest first
    assert!(t.hub.timeline.list().iter().any(|e| e.text.contains("touched")));
    let l = tool(&t, "onewire_list", json!({})).await.unwrap();
    assert!(l["touchLog"][0]["t"].as_str().unwrap().ends_with('Z'));
    t.drv.stop(StopReason::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn search_writes_the_current_mode_back() {
    let t = start().await;
    let r = tool(&t, "onewire_search", json!({})).await.unwrap();
    assert_eq!(read(&t.root().join("w1_bus_master1/w1_master_search")), b"-1\n");
    assert_eq!(r.as_array().unwrap().len(), 19);
    // mode off writes 0, then the immediate search writes the old mode back (as the TypeScript does).
    call(&t, Method::POST, "/api/onewire/search", json!({ "mode": "off" }), vec![]).await.unwrap();
    assert_eq!(read(&t.root().join("w1_bus_master1/w1_master_search")), b"-1\n");
    let e = tool(&t, "onewire_search", json!({ "master": "w1_bus_master9" })).await.unwrap_err();
    assert_eq!(e.message, "No 1-Wire master \"w1_bus_master9\"");
    t.drv.stop(StopReason::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resolution_change_and_label() {
    let t = start().await;
    call(&t, Method::PUT, &format!("/api/onewire/{}/resolution", FIX.ds18b20), json!({ "bits": 10 }), vec![]).await.unwrap();
    assert_eq!(read(&t.root().join(FIX.ds18b20).join("resolution")), b"10\n");
    let e = call(&t, Method::PUT, &format!("/api/onewire/{}/resolution", FIX.ds18s20), json!({ "bits": 10 }), vec![]).await.unwrap_err();
    assert_eq!(e.message, "DS18S20 has no adjustable resolution");
    let e = call(&t, Method::PUT, &format!("/api/onewire/{}/resolution", FIX.ds18b20), json!({ "bits": 13 }), vec![]).await.unwrap_err();
    assert_eq!(e.message, "bits must be 9–12");
    call(&t, Method::PUT, &format!("/api/onewire/{}/label", FIX.ds18b20.to_uppercase()), json!({ "label": "Freezer" }), vec![])
        .await
        .unwrap();
    assert_eq!(slave(&t, FIX.ds18b20)["label"], "Freezer");
    let r = tool(&t, "onewire_read", json!({ "id": FIX.ds18b20 })).await.unwrap();
    assert_eq!(r["label"], "Freezer");
    assert_eq!(r["readings"]["Temperature"], "23.125 °C");
    let e = tool(&t, "onewire_read", json!({ "id": "28-nope" })).await.unwrap_err();
    assert_eq!(e.message, "No 1-Wire slave \"28-nope\". IDs look like 28-0000075b1a2c; see GET /api/onewire.");
    t.drv.stop(StopReason::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_validate_and_bridges_bind_through_sysfs() {
    let t = start().await;
    let e = call(&t, Method::PUT, "/api/onewire/settings", json!({ "interval": 100 }), vec![]).await.unwrap_err();
    assert_eq!(e.message, "interval must be 2000–3600000 ms");
    let v = call(&t, Method::PUT, "/api/onewire/settings", json!({ "interval": 5000, "bulk": false }), vec![]).await.unwrap();
    assert_eq!((v["interval"].clone(), v["bulk"].clone()), (json!(5000), json!(false)));
    let e = call(&t, Method::POST, "/api/onewire/bridge", json!({ "bus": 1, "address": "0x30" }), vec![]).await.unwrap_err();
    assert_eq!(e.message, "bus and address (0x18–0x1B) required");
    let i2c = t.dir.path().join("sys/bus/i2c/devices/i2c-1");
    call(&t, Method::POST, "/api/onewire/bridge", json!({ "bus": "i2c-1", "address": "0x18" }), vec![]).await.unwrap();
    assert_eq!(read(&i2c.join("new_device")), b"ds2482 0x18\n");
    call(&t, Method::POST, "/api/onewire/bridge", json!({ "bus": 1, "address": 0x19, "bind": false }), vec![]).await.unwrap();
    assert_eq!(read(&i2c.join("delete_device")), b"0x19\n");
    assert!(t.hub.timeline.list().iter().any(|e| e.text == "Unbound ds2482 from i2c-1 0x19"));
    t.drv.stop(StopReason::Shutdown).await;
}

#[test]
fn numbers_and_labels_format_like_javascript() {
    assert_eq!(to_fixed1(23.125), "23.1");
    assert_eq!(to_fixed1(23.25), "23.3");
    assert_eq!(to_fixed1(-10.5), "-10.5");
    assert_eq!(to_fixed1(-0.04), "-0.0");
    assert_eq!(strip_header_pin("GPIO4 (header pin 7)"), "GPIO4");
    assert_eq!(strip_header_pin("DS2482 on i2c-1 @0x18"), "DS2482 on i2c-1 @0x18");
    assert!(ok_id("28-0000075B1A2C") && !ok_id("w1_bus_master1"));
    assert_eq!(header_pin(4), Some(7));
}

/// Writes the fixture tree for a side instance when BENCHBOB_W1_FIXTURE_OUT is set.
#[test]
fn write_fixture_tree() {
    if let Ok(out) = std::env::var("BENCHBOB_W1_FIXTURE_OUT") {
        build_w1_fixture(Path::new(&out)).unwrap();
    }
}
