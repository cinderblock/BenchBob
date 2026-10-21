//! The driver end to end against a sysfs fixture tree and FakeBus chips (the TypeScript
//! driver had no tests of its own; these cover what the UI and MCP rely on).

use std::collections::HashMap;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::Arc;

use axum::http::{HeaderMap, Method};
use benchbob_core::driver::Hub;
use benchbob_core::settings::Settings;
use benchbob_core::{Ctx, Driver, HttpError, Json, Reply, Request};
use benchbob_i2c_chips::fake::{FakeBus, FnChip, RegChip, SMBUS_ONLY};
use benchbob_i2c_chips::fixtures::{bme280_regs, mpu6050_regs};
use serde_json::{Value, json};

use super::*;

/// A sysfs tree like desky's: i2c-1 on the header, i2c-20 an HDMI DDC bus, a kernel driver
/// bound at 1-0050, and (optionally) an SMBus-only stub as i2c-11.
fn sys_tree(dir: &Path, stub: bool) {
    let adapter = |n: u32, name: &str, dev: &str, compat: &str, hz: Option<u32>| {
        let real = dir.join(format!("devices/platform/soc/{dev}/i2c-{n}"));
        std::fs::create_dir_all(real.join("of_node")).unwrap();
        std::fs::write(real.join("of_node/compatible"), compat).unwrap();
        if let Some(hz) = hz {
            std::fs::write(real.join("of_node/clock-frequency"), hz.to_be_bytes()).unwrap();
        }
        let class = dir.join(format!("class/i2c-dev/i2c-{n}"));
        std::fs::create_dir_all(&class).unwrap();
        std::fs::write(class.join("name"), format!("{name}\n")).unwrap();
        symlink(&real, class.join("device")).unwrap();
    };
    adapter(1, "bcm2835 (i2c@7e804000)", "fe804000.i2c", "brcm,bcm2711-i2c\0brcm,bcm2835-i2c\0", Some(100_000));
    adapter(20, "fef04500.i2c", "fef04500.i2c", "brcm,bcm2711-hdmi-i2c\0", None);
    if stub {
        adapter(11, "SMBus stub driver", "stub", "", None);
    }
    let client = dir.join("bus/i2c/devices/1-0050");
    std::fs::create_dir_all(&client).unwrap();
    std::fs::create_dir_all(dir.join("bus/i2c/drivers/at24")).unwrap();
    symlink(dir.join("bus/i2c/drivers/at24"), client.join("driver")).unwrap();
}

fn bus1() -> FakeBus {
    FakeBus::default()
        .add(0x76, RegChip::new(bme280_regs()))
        .add(0x68, RegChip::new(mpu6050_regs()))
        .add(0x27, FnChip::reads(|n| vec![0xf7; n]))
        // A DS2482 answers a bare read with its idle status.
        .add(0x18, FnChip::read_only(|_| vec![0x18]))
        // Something unidentifiable: ACKs, NACKs every read.
        .add(0x3a, FnChip { read: None, accept_writes: true, writes: vec![] })
}

struct T {
    drv: I2c,
    _dir: tempfile::TempDir,
}

async fn start(stub: bool) -> T {
    let dir = tempfile::tempdir().unwrap();
    sys_tree(dir.path(), stub);
    let drv = I2c::with(
        dir.path().to_path_buf(),
        Box::new(|n| match n {
            1 => Ok(Box::new(bus1()) as BoxIo),
            11 => Ok(Box::new(FakeBus::new(SMBUS_ONLY).add(0x76, RegChip::new(bme280_regs()))) as BoxIo),
            _ => Err(format!("EACCES: permission denied, open '/dev/i2c-{n}'")),
        }),
    );
    let hub = Arc::new(Hub::new(Settings::load(dir.path().join("peripherals.json"))));
    hub.settings.own("i2c");
    drv.init(Ctx::new("i2c", hub)).await.unwrap();
    // The first scan runs on the first tick.
    for _ in 0..100 {
        if lock(&drv.inner.buses).values().filter(|b| b.io.is_some()).all(|b| lock(&b.st).last_scan != 0) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    T { drv, _dir: dir }
}

async fn call(t: &T, method: Method, path: &str, body: Value) -> Result<Value, HttpError> {
    call_q(t, method, path, body, vec![]).await
}

async fn call_q(t: &T, method: Method, path: &str, body: Value, query: Vec<(&str, &str)>) -> Result<Value, HttpError> {
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

async fn tool(t: &T, name: &str, args: Value) -> Result<Json, HttpError> {
    let tools = t.drv.tools();
    let tl = tools.iter().find(|x| x.name == name).unwrap();
    match (tl.handler)(args).await? {
        benchbob_core::ToolResult::Value(v) => Ok(v),
        _ => panic!("content"),
    }
}

#[test]
fn adapters_are_classified_like_the_typescript_server() {
    assert_eq!(classify("bcm2835 (i2c@7e804000)", "", "").0, "header");
    assert_eq!(classify("fef04500.i2c", "", "brcm,bcm2711-hdmi-i2c ").0, "ddc");
    assert_eq!(classify("bcm2835 (i2c@7e205a00)", "", ""), ("header", "Header I²C5 (dtoverlay=i2c5)".into()));
    assert_eq!(classify("i2c@10", "", "i2c-gpio ").1, "Header software I²C (i2c-gpio)");
    assert_eq!(classify("bcm2835 (i2c@7e205000)", "", "").0, "other");
    assert_eq!(classify("SMBus stub driver", "", "").0, "stub");
    assert_eq!(classify("CP2112 SMBus Bridge on hidraw0", "", ""), ("usb", "USB: Silicon Labs CP2112".into()));
    assert_eq!(classify("i2c-ch341-usb", "", ""), ("usb", "USB: WCH CH341".into()));
    assert_eq!(classify("Some adapter", "/sys/devices/pci0000:00/usb1/1-1", ""), ("usb", "USB: Some adapter".into()));
    assert_eq!(classify("OMAP I2C adapter", "/sys/devices/platform/ocp/4802a000.i2c/i2c-1", ""), ("other", "OMAP I2C adapter".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_identifies_and_the_view_matches_the_typescript_shape() {
    let t = start(false).await;
    let v = call(&t, Method::GET, "/api/i2c", Value::Null).await.unwrap();
    let buses = v["buses"].as_array().unwrap();
    assert_eq!(buses.len(), 1, "the DDC bus is hidden");
    let b = &buses[0];
    let keys: Vec<&String> = b.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "bus", "dev", "name", "label", "kind", "hz", "funcs", "plainI2c", "error", "scanning", "lastScan", "scanMs", "autoScan",
            "devices"
        ]
    );
    assert_eq!(b["label"], "Header pins 3/5 (GPIO2/3)");
    assert_eq!(b["hz"], 100000);
    assert_eq!(b["autoScan"], 30);
    assert_eq!(b["funcs"], json!(["I2C", "quick", "byte", "word", "i2c-block"]));
    let devs = b["devices"].as_array().unwrap();
    let by = |hexs: &str| devs.iter().find(|d| d["hex"] == hexs).unwrap_or_else(|| panic!("{hexs} missing: {devs:?}"));
    assert_eq!(by("0x76")["name"], "BME280");
    assert_eq!(by("0x76")["confidence"], "id");
    assert_eq!(by("0x68")["name"], "MPU-6050");
    assert_eq!(by("0x27")["name"], "PCF8574");
    assert_eq!(by("0x18")["chip"], "ds2482");
    assert_eq!(by("0x50")["claimed"], "at24");
    assert_eq!(by("0x3A")["chip"], Value::Null);
    assert_eq!(by("0x3A")["hints"], json!(["GPIO expander"]));
    assert_eq!(by("0x3A")["pointerUnsafe"], true);
    let dk: Vec<&String> = by("0x76").as_object().unwrap().keys().collect();
    assert_eq!(
        dk,
        [
            "addr",
            "hex",
            "claimed",
            "chip",
            "name",
            "what",
            "confidence",
            "note",
            "hint",
            "hints",
            "label",
            "readout",
            "readAt",
            "error",
            "canRead",
            "canMeasure",
            "measureNote",
            "pollable",
            "poll",
            "auto",
            "interval",
            "opts",
            "pointerUnsafe",
            "hist",
            "firstSeen"
        ]
    );
    let st = t.drv.status().unwrap();
    assert_eq!(st["active"], true);
    assert_eq!(st["summary"], "i2c-1 · DS2482, PCF8574, 0x3A, 0x50 (at24) +2");
    assert_eq!(st["hiddenBuses"], json!(["i2c-20"]));
    let h = t.drv.handle().ds2482_candidates();
    assert_eq!(h, vec![Ds2482 { bus: 1, addr: 0x18, bound: false, driver: None }]);
    t.drv.stop(StopReason::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_decode_record_history_and_measure_wakes_and_restores() {
    let t = start(false).await;
    let r = call(&t, Method::POST, "/api/i2c/1/0x76/read", Value::Null).await.unwrap();
    assert_eq!(r["values"][0]["k"], "t");
    assert_eq!(r["values"][0]["v"], 25.08);
    let v = call(&t, Method::GET, "/api/i2c", Value::Null).await.unwrap();
    let d = v["buses"][0]["devices"].as_array().unwrap().iter().find(|d| d["hex"] == "0x76").unwrap().clone();
    assert!(d["hist"]["t"].as_array().unwrap().contains(&json!(25.08)));
    assert!(d["readAt"].as_u64().unwrap() > 0);
    // The MPU is awake in the fixture: read decodes 1 g on Z.
    let m = call(&t, Method::POST, "/api/i2c/1/104/measure", Value::Null).await.unwrap();
    assert_eq!(m["values"][2]["v"], 1);
    // Not identified → 409; nothing there → 404; a chip without a decoder → 409.
    assert_eq!(
        call(&t, Method::POST, "/api/i2c/1/0x3a/read", Value::Null).await.unwrap_err().message,
        "0x3A isn't identified; use the raw tools"
    );
    assert_eq!(
        call(&t, Method::POST, "/api/i2c/1/0x40/read", Value::Null).await.unwrap_err().message,
        "Nothing found at 0x40 on i2c-1 in the last scan"
    );
    assert_eq!(
        call(&t, Method::POST, "/api/i2c/1/0x27/measure", Value::Null).await.unwrap_err().message,
        "PCF8574 has no measure operation"
    );
    assert_eq!(call(&t, Method::POST, "/api/i2c/9/0x27/read", Value::Null).await.unwrap_err().status, 404);
    assert_eq!(
        call(&t, Method::POST, "/api/i2c/20/0x27/read", Value::Null).await.unwrap_err().message,
        "i2c-20 is not usable: EACCES: permission denied, open '/dev/i2c-20'"
    );
    t.drv.stop(StopReason::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_writes_need_confirm_and_reach_the_chip() {
    let t = start(false).await;
    let w = json!({ "register": "0xF4", "size": "byte", "value": "0x24" });
    let e = call(&t, Method::POST, "/api/i2c/1/0x76/raw/write", w.clone()).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (400, "A raw write writes to the device; repeat with \"confirm\": true"));
    let mut wc = w.clone();
    wc["confirm"] = json!(true);
    assert_eq!(call(&t, Method::POST, "/api/i2c/1/0x76/raw/write", wc).await.unwrap(), json!({ "register": 0xf4, "wrote": [0x24] }));
    assert_eq!(
        call(&t, Method::POST, "/api/i2c/1/0x76/raw/read", json!({ "register": 0xf4 })).await.unwrap(),
        json!({ "register": 0xf4, "bytes": [0x24] })
    );
    // Word: the logical value goes MSB first unless bigEndian is false.
    let ww = json!({ "register": 0x10, "size": "word", "value": "0x1234", "confirm": true });
    assert_eq!(call(&t, Method::POST, "/api/i2c/1/0x76/raw/write", ww).await.unwrap(), json!({ "register": 16, "wrote": [0x12, 0x34] }));
    let rw = call(&t, Method::POST, "/api/i2c/1/0x76/raw/read", json!({ "register": 0x10, "size": "word" })).await.unwrap();
    assert_eq!(rw, json!({ "register": 16, "word": 0x3412, "wordBigEndian": 0x1234, "bytes": [0x12, 0x34] }));
    // Transfers: writes need confirm, reads don't.
    let tr = call(&t, Method::POST, "/api/i2c/1/0x76/transfer", json!({ "write": ["0xD0"], "read": 1 })).await.unwrap_err();
    assert!(tr.message.starts_with("This transfer writes"));
    let tr = call(&t, Method::POST, "/api/i2c/1/0x76/transfer", json!({ "write": ["0xD0"], "read": 1, "confirm": true })).await.unwrap();
    assert_eq!(tr, json!({ "reads": [[0x60]] }));
    assert_eq!(
        call(&t, Method::POST, "/api/i2c/1/0x76/transfer", json!({})).await.unwrap_err().message,
        "Nothing to do: give write bytes and/or a read length"
    );
    // Errors from the bus are 502 with the errno text.
    let e = call(&t, Method::POST, "/api/i2c/1/0x41/raw/read", json!({ "register": 0 })).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (502, "read: no ACK from the device (EREMOTEIO)"));
    // MCP i2c_write needs no confirm (agents are told it changes device state).
    assert_eq!(
        tool(&t, "i2c_write", json!({ "bus": "i2c-1", "address": "0x76", "register": 0xf4, "value": 0x27 })).await.unwrap()["wrote"],
        json!([0x27])
    );
    t.drv.stop(StopReason::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn smbus_only_buses_refuse_transfers_and_dumps_guard_command_chips() {
    let t = start(true).await;
    let e = call(&t, Method::POST, "/api/i2c/11/0x76/transfer", json!({ "read": 2 })).await.unwrap_err();
    assert_eq!(e.status, 409);
    assert!(e.message.starts_with("i2c-11 only supports SMBus transfers (quick, byte, word, i2c-block)"));
    let v = call(&t, Method::GET, "/api/i2c", Value::Null).await.unwrap();
    let stub = v["buses"].as_array().unwrap().iter().find(|b| b["bus"] == 11).unwrap().clone();
    assert_eq!(stub["kind"], "stub");
    assert_eq!(stub["devices"][0]["name"], "BME280");
    // A dump of an unidentified address or a command-style chip needs ?confirm=1.
    let e = call(&t, Method::GET, "/api/i2c/1/0x27/dump", Value::Null).await.unwrap_err();
    assert_eq!(e.message, "PCF8574 treats register pointers as commands; add ?confirm=1 to dump anyway");
    let e = call(&t, Method::GET, "/api/i2c/1/0x3a/dump", Value::Null).await.unwrap_err();
    assert!(e.message.starts_with("An unidentified chip may treat"));
    let d = call(&t, Method::GET, "/api/i2c/1/0x76/dump", Value::Null).await.unwrap();
    assert_eq!(d["regs"][0xd0], 0x60);
    assert_eq!(d["regs"].as_array().unwrap().len(), 256);
    let all = call_q(&t, Method::GET, "/api/i2c", Value::Null, vec![("all", "1")]).await.unwrap();
    assert_eq!(all["buses"].as_array().unwrap().len(), 3);
    t.drv.stop(StopReason::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn device_and_bus_settings_validate_and_persist() {
    let t = start(false).await;
    let e = call(&t, Method::PUT, "/api/i2c/1/0x76/settings", json!({ "opts": { "shunt": 1 } })).await.unwrap_err();
    assert_eq!(e.message, "BME280 has no option \"shunt\"");
    let e = call(&t, Method::PUT, "/api/i2c/1/0x27/settings", json!({ "auto": true })).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (409, "PCF8574 has no measure operation"));
    let v =
        call(&t, Method::PUT, "/api/i2c/1/0x76/settings", json!({ "poll": false, "interval": 5000, "label": "Bench air" })).await.unwrap();
    assert_eq!((v["poll"].clone(), v["interval"].clone(), v["label"].clone()), (json!(false), json!(5000), json!("Bench air")));
    let e = call(&t, Method::PUT, "/api/i2c/1/0x76/settings", json!({ "interval": 10 })).await.unwrap_err();
    assert_eq!(e.message, "interval must be 250–3600000");
    let b = call(&t, Method::PUT, "/api/i2c/1/settings", json!({ "autoScan": 0 })).await.unwrap();
    assert_eq!(b["autoScan"], 0);
    assert_eq!(t.drv.inner.ctx().setting("dev:fe804000.i2c@0x76").unwrap()["label"], "Bench air");
    assert_eq!(t.drv.inner.ctx().setting("bus:fe804000.i2c").unwrap(), json!({ "autoScan": 0 }));
    t.drv.stop(StopReason::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_tools_list_scan_and_summarise_devices() {
    let t = start(false).await;
    let names: Vec<String> = t.drv.tools().iter().map(|x| x.name.clone()).collect();
    assert_eq!(names, ["i2c_buses", "i2c_scan", "i2c_devices", "i2c_read", "i2c_write", "i2c_transfer"]);
    let b = tool(&t, "i2c_buses", json!({})).await.unwrap();
    assert_eq!(b[0]["adapter"], "bcm2835 (i2c@7e804000)");
    assert_eq!(b[0]["devices"], 6);
    assert!(b[0]["lastScan"].as_str().unwrap().ends_with('Z'));
    let s = tool(&t, "i2c_scan", json!({ "bus": 1 })).await.unwrap();
    assert_eq!(s["devices"].as_array().unwrap().len(), 6);
    let d = tool(&t, "i2c_devices", json!({ "address": "0x76" })).await.unwrap();
    let dev = &d[0]["devices"][0];
    assert_eq!(dev["chip"], "BME280");
    assert_eq!(dev["readings"]["Temperature"], "25.08 °C");
    let keys: Vec<&String> = dev.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["address", "claimedBy", "chip", "what", "confidence", "note", "readings", "readAt", "canMeasure", "measureNote"]);
    assert_eq!(
        tool(&t, "i2c_scan", json!({})).await.unwrap_err().message,
        "No I²C bus \"undefined\". See GET /api/i2c (buses are numbered like the /dev/i2c-N nodes)."
    );
    t.drv.stop(StopReason::Shutdown).await;
}
