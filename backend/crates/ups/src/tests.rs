//! The driver without hardware: an empty bus, then a UPS whose `upsc` variables come from
//! the dummy-ups fixture, checked through the REST routes, the status and the MCP tool.

use axum::http::Method;
use benchbob_core::Reply;
use benchbob_core::util::{call_route, call_tool, test_ctx};

use super::*;

const UPSC: &str = "battery.charge: 87
battery.runtime: 1800
device.mfr: SimCorp
device.model: Sim 1500
device.type: ups
driver.name: dummy-ups
driver.parameter.port: /tmp/nutx.zEhg/sim.dev
driver.version: 2.8.1
input.voltage: 121.0
ups.load: 23
ups.mfr: SimCorp
ups.model: Sim 1500
ups.realpower.nominal: 900
ups.status: OL CHRG";

async fn start(dir: &tempfile::TempDir) -> Ups {
    let d = Ups::with_config(Config { dir: dir.path().join("nut"), sim: String::new(), port_offset: 3000, nut: dir.path().join("no-nut") });
    d.init(test_ctx("ups", dir.path())).await.unwrap();
    d
}

async fn get(d: &Ups, path: &str) -> Result<Value, HttpError> {
    match call_route(&d.routes(), Method::GET, path, &[], Value::Null).await? {
        Reply::Json(v) => Ok(v),
        Reply::Response(_) => panic!("not JSON"),
    }
}

/// A UPS as detection would add it, with polled variables.
fn add_fixture(d: &Ups) {
    let i = &d.inner;
    let mut u = i.blank("CPS_CP1500_CXXKZ200".into(), "usbhid-ups".into(), None);
    u.name = "benchbob".into();
    u.simulated = false;
    u.vid = "0764".into();
    u.pid = "0501".into();
    u.usb_port = "1-1.4".into();
    u.vars = parse_upsc(UPSC);
    u.status = "OL CHRG".into();
    u.reachable = true;
    u.charge = vec![86.0, 87.0];
    u.load = vec![23.0];
    lock(&i.st).upses.insert(u.key.clone(), u);
}

#[tokio::test]
async fn nothing_connected() {
    let dir = tempfile::tempdir().unwrap();
    let d = start(&dir).await;
    let v = get(&d, "/api/ups").await.unwrap();
    assert_eq!(
        v,
        json!({ "upses": [], "nut": { "running": false, "port": 3493 + 3000, "shared": false, "user": "monuser", "log": [], "autoShutdown": false } })
    );
    let s = d.status().unwrap();
    assert_eq!((s["active"].clone(), s["summary"].clone()), (json!(false), json!("No UPS")));
    let e = get(&d, "/api/ups/benchbob").await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (404, "No UPS \"benchbob\" (none connected)"));
    assert_eq!(call_tool(&d.tools(), "ups_status", json!({})).await.unwrap(), json!({ "upses": [], "note": "No UPS connected" }));
    d.stop(StopReason::Shutdown).await;
}

#[tokio::test]
async fn views_of_a_polled_ups() {
    let dir = tempfile::tempdir().unwrap();
    let d = start(&dir).await;
    add_fixture(&d);
    let v = get(&d, "/api/ups").await.unwrap();
    let u = &v["upses"][0];
    let keys: Vec<&String> = u.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "name",
            "key",
            "simulated",
            "vendor",
            "model",
            "serial",
            "firmware",
            "usb",
            "nutDriver",
            "driverRunning",
            "reachable",
            "error",
            "status",
            "flags",
            "charge",
            "runtime",
            "runtimeText",
            "load",
            "realpower",
            "inputVoltage",
            "outputVoltage",
            "inputFrequency",
            "batteryVoltage",
            "batteryDate",
            "batteryType",
            "lowRuntime",
            "lowCharge",
            "beeper",
            "lastPoll",
            "history",
            "events"
        ]
    );
    assert_eq!(u["vendor"], "SimCorp");
    assert_eq!(u["model"], "Sim 1500");
    assert_eq!(u["usb"], "0764:0501 on port 1-1.4");
    assert_eq!(u["charge"], 87);
    assert_eq!(u["runtimeText"], "30 min");
    assert_eq!(serde_json::to_string(&u["inputVoltage"]).unwrap(), "121");
    // realpower from the nominal rating and the load: round(900 * 23 / 100)
    assert_eq!(u["realpower"], 207);
    assert_eq!(u["outputVoltage"], Value::Null);
    assert_eq!(
        u["flags"],
        json!([{ "flag": "OL", "word": "On mains", "severity": "ok" }, { "flag": "CHRG", "word": "Charging", "severity": "info" }])
    );
    assert_eq!(u["history"], json!({ "charge": [86, 87], "load": [23] }));

    let s = d.status().unwrap();
    assert_eq!(s["summary"], "Sim 1500 · On mains · 87% · 30 min · 23% load");
    assert_eq!(s["active"], true);

    let one = get(&d, "/api/ups/benchbob").await.unwrap();
    assert_eq!(one["vars"]["ups.status"], "OL CHRG");
    assert_eq!(one.as_object().unwrap().keys().next_back().unwrap(), "vars");
    assert!(get(&d, "/api/ups/CPS_CP1500_CXXKZ200").await.is_ok());

    let t = call_tool(&d.tools(), "ups_status", json!({ "raw": true })).await.unwrap();
    let u = t["upses"][0].as_object().unwrap();
    assert!(!u.contains_key("history"));
    assert_eq!(u.keys().next_back().unwrap(), "vars");
    assert!(t["nut"].is_object());
    let t = call_tool(&d.tools(), "ups_status", json!({ "name": "benchbob" })).await.unwrap();
    assert!(!t["upses"][0].as_object().unwrap().contains_key("vars"));
    d.stop(StopReason::Shutdown).await;
}

#[tokio::test]
async fn sharing_settings() {
    let dir = tempfile::tempdir().unwrap();
    let d = start(&dir).await;
    let r = call_route(&d.routes(), Method::POST, "/api/ups/share", &[], json!({ "port": 70000 })).await;
    assert_eq!(r.err().unwrap().message, "port must be 1–65535");
    let r = call_route(&d.routes(), Method::POST, "/api/ups/share", &[], json!({ "port": "13493", "share": 1 })).await.unwrap();
    let Reply::Json(v) = r else { panic!() };
    let nut = &v["nut"];
    assert_eq!((nut["port"].clone(), nut["shared"].clone()), (json!(13493), json!(true)));
    // The password only shows while shared.
    assert_eq!(nut["password"].as_str().unwrap().len(), 16);
    let r = call_route(&d.routes(), Method::POST, "/api/ups/share", &[], json!({ "share": false })).await.unwrap();
    let Reply::Json(v) = r else { panic!() };
    assert!(!v["nut"].as_object().unwrap().contains_key("password"));
    d.stop(StopReason::Shutdown).await;
}

#[test]
fn ssl_noise_is_dropped() {
    assert_eq!(ssl_noise("Init SSL without certificate database\nError: Data stale"), "Error: Data stale");
}
