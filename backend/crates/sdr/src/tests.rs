//! The driver with stand-in tools: scripts that print what rtl_433, rtl_tcp, rtl_test and
//! rtl_eeprom print (the rtl_433 one replays the captured JSON stream), and sticks fed into
//! the udev database.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use axum::http::Method;
use benchbob_core::Reply;
use benchbob_core::udev::Props;
use benchbob_core::util::{call_route, call_tool, test_ctx};

use super::*;

const RTL_TEST: &str = "Found 1 device(s):
  0:  Realtek, RTL2838UHIDIR, SN: 00000001

Using device 0: Generic RTL2832U OEM
Found Rafael Micro R820T tuner
Supported gain values (29): 0.0 0.9 1.4 2.7 3.7 7.7 8.7 12.5 14.4 15.7 16.6 19.7 20.7 22.9 25.4 28.0 29.7 32.8 33.8 36.4 37.2 38.6 40.2 42.1 43.4 43.9 44.5 48.0 49.6";

const RTL_EEPROM: &str = "Current configuration:
__________________________________________
Vendor ID:\t\t0x0bda
Product ID:\t\t0x2838
Manufacturer:\t\tRealtek
Product:\t\tRTL2838UHIDIR
Serial number:\t\t00000001
__________________________________________";

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

struct T {
    d: Sdr,
    ctx: Ctx,
    _dir: tempfile::TempDir,
}

async fn start(tcp: &str) -> T {
    let dir = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/rtl433.jsonl");
    std::fs::create_dir_all(dir.path().join("modprobe.d")).unwrap();
    std::fs::write(dir.path().join("modprobe.d/blacklist-rtl.conf"), "# rtl-sdr\nblacklist dvb_usb_rtl28xxu\n").unwrap();
    std::fs::create_dir_all(dir.path().join("net")).unwrap();
    let p = dir.path();
    let cfg = Config {
        rtl_tcp: script(p, "rtl_tcp", tcp),
        rtl_433: script(p, "rtl_433", &format!("echo \"args: $*\"\ncat '{}'\nexec sleep 30", fixture.display())),
        rtl_test: script(p, "rtl_test", &format!("cat <<'EOF' >&2\n{RTL_TEST}\nEOF")),
        rtl_eeprom: script(p, "rtl_eeprom", &format!("cat <<'EOF'\n{RTL_EEPROM}\nEOF")),
        port_offset: 3000,
        modprobe_dirs: vec![p.join("modprobe.d")],
        sys_module: p.join("module"),
        proc_net: p.join("net"),
    };
    let d = Sdr::with_config(cfg);
    let ctx = test_ctx("sdr", p);
    d.init(ctx.clone()).await.unwrap();
    T { d, ctx, _dir: dir }
}

async fn call(t: &T, method: Method, path: &str, query: &[(&str, &str)], body: Value) -> Result<Value, HttpError> {
    match call_route(&t.d.routes(), method, path, query, body).await? {
        Reply::Json(v) => Ok(v),
        Reply::Response(_) => panic!("not JSON"),
    }
}

fn add_stick(ctx: &Ctx, dvb: bool) {
    let p = |kv: &[(&str, &str)]| -> Props { kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
    ctx.hub.udev.apply(&p(&[
        ("ACTION", "add"),
        ("DEVPATH", "/devices/usb1/1-1/1-1.2"),
        ("SUBSYSTEM", "usb"),
        ("DEVTYPE", "usb_device"),
        ("ID_VENDOR_ID", "0bda"),
        ("ID_MODEL_ID", "2838"),
        ("ID_VENDOR_ENC", "Realtek"),
        ("ID_MODEL_ENC", "RTL2838UHIDIR"),
        ("ID_SERIAL_SHORT", "00000001"),
        ("ID_PATH", "platform-fd500000.pcie-pci-0000:01:00.0-usb-0:1.2"),
        ("BUSNUM", "001"),
        ("DEVNUM", "004"),
    ]));
    ctx.hub.udev.apply(&p(&[
        ("ACTION", "add"),
        ("DEVPATH", "/devices/usb1/1-1/1-1.2/1-1.2:1.0"),
        ("SUBSYSTEM", "usb"),
        ("DEVTYPE", "usb_interface"),
        ("DRIVER", if dvb { "dvb_usb_rtl28xxu" } else { "" }),
    ]));
}

#[tokio::test]
async fn idle_status_and_validation() {
    let t = start("exit 1").await;
    let v = call(&t, Method::GET, "/api/sdr", &[], Value::Null).await.unwrap();
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["active", "summary", "sticks", "dvb", "mode", "lastExit", "probing", "presets", "defaults", "rtl433"]);
    assert_eq!(v["summary"], "No RTL-SDR stick");
    assert_eq!(v["dvb"]["blacklisted"], true);
    assert_eq!(v["dvb"]["fix"], "");
    assert_eq!(v["dvb"]["loaded"], false);
    // JSON.stringify puts integer-like keys first.
    let pk: Vec<&String> = v["presets"].as_object().unwrap().keys().collect();
    assert_eq!(pk, ["345", "433", "868", "915", "433+868", "433+915"]);
    assert_eq!(v["defaults"], json!({ "tcpPort": 4234, "settings": {} }));

    let err = |b: Value| {
        let t = &t;
        async move { call(t, Method::POST, "/api/sdr/mode", &[], b).await.unwrap_err() }
    };
    let e = err(json!({ "mode": "nope" })).await;
    assert_eq!((e.status, e.message.as_str()), (400, "mode must be off, rtl_tcp or rtl_433"));
    let e = err(json!({ "mode": "rtl_433" })).await;
    assert_eq!((e.status, e.message.as_str()), (409, "No RTL-SDR stick detected. Plug one in, or pass force:true to try anyway."));
    let e = err(json!({ "mode": "rtl_433", "force": true, "preset": "999" })).await;
    assert_eq!(e.message, "Unknown preset \"999\"; use one of 345, 433, 868, 915, 433+868, 433+915 or pass freqs");
    let e = err(json!({ "mode": "rtl_433", "force": true, "freqs": ["433.92MHz"] })).await;
    assert_eq!(e.message, "Bad frequency \"433.92MHz\" (e.g. 433.92M)");
    let e = err(json!({ "mode": "rtl_433", "force": true, "gain": "auto" })).await;
    assert_eq!(e.message, "gain is dB (e.g. 40) or empty for auto");
    let e = err(json!({ "mode": "rtl_tcp", "force": true, "sampleRate": 500000 })).await;
    assert_eq!(e.message, "sampleRate must be 225001–300000 or 900001–3200000");
    let e = err(json!({ "mode": "rtl_tcp", "force": true, "bind": "localhost" })).await;
    assert_eq!(e.message, "bind must be an IP address");
    let e = err(json!({ "mode": "rtl_tcp", "force": true, "port": null })).await;
    assert_eq!(e.message, "port must be 1–65535");
    let off = call(&t, Method::POST, "/api/sdr/mode", &[], json!({ "mode": "off" })).await.unwrap();
    assert_eq!(off, json!({ "mode": "off", "lastExit": null }));
}

#[tokio::test]
async fn rtl_433_decodes_into_the_sensor_table() {
    let t = start("exit 1").await;
    let v = call(&t, Method::POST, "/api/sdr/mode", &[], json!({ "mode": "rtl_433", "force": true, "freqs": ["433.92M", "868.3M"] }))
        .await
        .unwrap();
    assert_eq!(v["mode"], "rtl_433");
    assert_eq!(v["stick"], Value::Null);
    assert_eq!(v["opts"], json!({ "preset": "custom", "freqs": ["433.92M", "868.3M"], "hop": 60, "gain": "", "ppm": 0 }));
    assert!(
        v["cmd"].as_str().unwrap().ends_with("rtl_433 -F json -F log -M time:unix:usec -M level -M protocol -f 433.92M -f 868.3M -H 60")
    );
    assert_eq!(v["decoded"], 5);
    assert_eq!(v["messages"], 7);
    let s = call(&t, Method::GET, "/api/sdr", &[], Value::Null).await.unwrap();
    assert_eq!(s["summary"], "rtl_433 · 5 sensors");
    assert_eq!(
        s["defaults"]["settings"],
        json!({ "rtl433": { "preset": "custom", "freqs": ["433.92M", "868.3M"], "hop": 60, "gain": "", "ppm": 0 } })
    );

    let r = call(&t, Method::GET, "/api/sdr/rtl433", &[("since", "5"), ("limit", "1")], Value::Null).await.unwrap();
    assert_eq!(r["seq"], 7);
    assert_eq!(r["devices"].as_array().unwrap().len(), 5);
    assert_eq!(r["devices"][0]["key"], "Acurite-Tower/9120/C");
    assert!(r["devices"][0]["hist"].is_object() && r["devices"][0]["lastSeenAgo_s"].is_number());
    assert_eq!(r["messages"].as_array().unwrap().len(), 1);
    assert_eq!(r["messages"][0]["seq"], 7);
    // The raw message keeps rtl_433's key order, with numbers as JavaScript prints them.
    let m = r["messages"][0]["msg"].to_string();
    assert!(m.starts_with(r#"{"time":"1792226061.000000","protocol":40,"model":"Acurite-Tower","id":9120,"channel":"C","battery_ok":1,"temperature_C":-3.1,"#), "{m}");
    assert!(r["log"].as_array().unwrap().iter().any(|l| l["line"] == "Tuned to 433.920MHz."));

    let tl = call_tool(&t.d.tools(), "sdr_messages", json!({ "limit": 2, "sensors": 2 })).await.unwrap();
    assert_eq!(tl["mode"], "rtl_433");
    assert_eq!(tl["sensors"].as_array().unwrap().len(), 2);
    assert!(tl["sensors"][0].get("hist").is_none());
    assert_eq!(tl["messages"].as_array().unwrap().len(), 2);
    assert_eq!(tl["messages"][1]["id"], 9120);

    // Turning it off: the child's exit is recorded, then the stop.
    let off = call(&t, Method::POST, "/api/sdr/mode", &[], json!({ "mode": "off" })).await.unwrap();
    assert_eq!(off["mode"], "off");
    assert_eq!(off["lastExit"]["code"], 128 + libc::SIGINT);
    let texts: Vec<String> = t.ctx.hub.timeline.list().into_iter().map(|e| e.text).collect();
    assert_eq!(texts[0], "SDR rtl_433 started");
    assert!(texts[1].starts_with(&format!("SDR rtl_433 exited ({}): ", 128 + libc::SIGINT)), "{texts:?}");
    assert_eq!(texts[2], "SDR rtl_433 stopped");

    call(&t, Method::DELETE, "/api/sdr/rtl433", &[], Value::Null).await.unwrap();
    let r = call(&t, Method::GET, "/api/sdr/rtl433", &[], Value::Null).await.unwrap();
    assert_eq!((r["devices"].clone(), r["messages"].clone()), (json!([]), json!([])));
}

#[tokio::test]
async fn rtl_tcp_failure_is_reported() {
    let t = start("echo 'No supported devices found.' >&2; exit 1").await;
    let e = call(&t, Method::POST, "/api/sdr/mode", &[], json!({ "mode": "rtl_tcp", "force": true })).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (503, "rtl_tcp exited: No supported devices found."));
    let s = call(&t, Method::GET, "/api/sdr", &[], Value::Null).await.unwrap();
    assert_eq!(s["lastExit"]["mode"], "rtl_tcp");
    assert_eq!(
        s["defaults"]["settings"]["tcp"],
        json!({ "port": 4234, "bind": "0.0.0.0", "gain": 0, "ppm": 0, "sampleRate": 2048000, "freq": 100000000, "biasT": false })
    );
}

#[tokio::test]
async fn rtl_tcp_runs_and_shows_how_to_connect() {
    let t = start("echo 'Listening...'; exec sleep 30").await;
    add_stick(&t.ctx, false);
    t.d.inner.scan_sticks();
    let v = call(&t, Method::POST, "/api/sdr/mode", &[], json!({ "mode": "rtl_tcp", "port": 4321, "gain": 40.2 })).await.unwrap();
    assert_eq!(v["stick"], "path:platform-fd500000.pcie-pci-0000:01:00.0-usb-0:1.2");
    assert!(v["cmd"].as_str().unwrap().ends_with("rtl_tcp -a 0.0.0.0 -p 4321 -s 2048000 -f 100000000 -g 40.2 -P 0 -d 00000001"));
    let host = hostname();
    assert_eq!(v["connect"]["gqrx"], format!("rtl_tcp={host}:4321"));
    assert_eq!(v["clients"], json!([]));
    // A client in /proc/net/tcp.
    std::fs::write(
        t._dir.path().join("net/tcp"),
        "  sl  local_address rem_address   st\n   1: 2D07FF0A:10E1 0507FF0A:D431 01 00000000:00000000\n",
    )
    .unwrap();
    t.d.inner.poll_tcp_clients();
    let s = call(&t, Method::GET, "/api/sdr", &[], Value::Null).await.unwrap();
    assert_eq!(s["summary"], "rtl_tcp on :4321 · 1 client");
    assert_eq!(s["clients"], json!(["10.255.7.5:54321"]));
    let e = call(&t, Method::POST, "/api/sdr/probe", &[], json!({})).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (409, "The stick is in use by rtl_tcp; turn it off to probe"));
    t.d.stop(StopReason::Shutdown).await;
    assert!(lock(&t.d.inner.st).cur.is_none());
}

#[tokio::test]
async fn sticks_probe_and_the_dvb_driver() {
    let t = start("exit 1").await;
    add_stick(&t.ctx, false);
    t.d.inner.scan_sticks();
    let p = call(&t, Method::POST, "/api/sdr/probe", &[], json!({})).await.unwrap();
    assert_eq!(p["tuner"], "Rafael Micro R820T");
    assert_eq!(p["devices"][0]["serial"], "00000001");
    assert_eq!(p["eeprom"]["Manufacturer"], "Realtek");
    let keys: Vec<&String> = p.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["devices", "using", "tuner", "gains", "blogV4", "claimed", "detachedKernel", "error", "t", "eeprom"]);
    let s = call(&t, Method::GET, "/api/sdr", &[], Value::Null).await.unwrap();
    let st = &s["sticks"][0];
    assert_eq!(st["knownAs"], "Generic RTL2832U OEM");
    assert_eq!(st["bus"], "001/004");
    assert_eq!(st["probe"]["tuner"], "Rafael Micro R820T");
    assert_eq!(s["summary"], "1 stick");

    add_stick(&t.ctx, true);
    t.d.inner.scan_sticks();
    let s = call(&t, Method::GET, "/api/sdr", &[], Value::Null).await.unwrap();
    assert_eq!(s["summary"], "1 stick · held by DVB driver");
    assert_eq!(s["sticks"][0]["drivers"], json!(["dvb_usb_rtl28xxu"]));
    let e = call(&t, Method::POST, "/api/sdr/mode", &[], json!({ "mode": "rtl_433" })).await.unwrap_err();
    assert_eq!(e.status, 409);
    assert!(e.message.starts_with("The kernel DVB driver has this stick. Fix: echo 'blacklist dvb_usb_rtl28xxu'"));
    let e = call(&t, Method::POST, "/api/sdr/mode", &[], json!({ "mode": "rtl_433", "stick": "nope" })).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (404, "No RTL-SDR stick \"nope\""));
}
