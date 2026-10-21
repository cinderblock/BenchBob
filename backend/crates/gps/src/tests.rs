//! The driver against a stand-in gpsd: a script as the child (it only has to run and exit)
//! and a TCP listener speaking the JSON watch protocol with the captured gpsd stream.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use axum::http::Method;
use benchbob_core::Reply;
use benchbob_core::util::{call_route, call_tool, test_ctx};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

use super::*;

const WATCH: &str = include_str!("../fixtures/gpsd-watch.jsonl");

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// A pseudo-terminal: (master fd, slave fd, /dev/pts/N).
fn pty() -> (i32, i32, String) {
    let (mut m, mut s) = (0, 0);
    // SAFETY: openpty fills the two fds; null name/termios/winsize are allowed.
    let r = unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null(), std::ptr::null()) };
    assert_eq!(r, 0);
    let path = std::fs::read_link(format!("/proc/self/fd/{s}")).unwrap().display().to_string();
    (m, s, path)
}

struct T {
    d: Gps,
    ctx: Ctx,
    dir: tempfile::TempDir,
}

async fn start(gpsd: &str, real_proc: bool) -> T {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("proc")).unwrap();
    std::fs::create_dir_all(dir.path().join("dev")).unwrap();
    std::fs::write(dir.path().join("dev/pps0"), "").unwrap();
    let cfg = Config {
        gpsd: script(dir.path(), "gpsd", gpsd),
        port_offset: 3000,
        proc_dir: if real_proc { "/proc".into() } else { dir.path().join("proc") },
        dev_dir: dir.path().join("dev"),
    };
    let d = Gps::with_config(cfg);
    let ctx = test_ctx("gps", dir.path());
    d.init(ctx.clone()).await.unwrap();
    T { d, ctx, dir }
}

async fn call(t: &T, method: Method, path: &str, body: Value) -> Result<Value, HttpError> {
    match call_route(&t.d.routes(), method, path, &[], body).await? {
        Reply::Json(v) => Ok(v),
        Reply::Response(_) => panic!("not JSON"),
    }
}

#[tokio::test]
async fn idle_status() {
    let t = start("exec sleep 30", false).await;
    let v = call(&t, Method::GET, "/api/gps", Value::Null).await.unwrap();
    assert_eq!(
        v,
        json!({
            "active": false, "summary": "No GPS receiver", "candidates": [], "ppsDevices": ["/dev/pps0"],
            "settings": { "port": 5947, "share": false, "readOnly": false, "pps": true }, "log": [],
            "running": false, "lastExit": null,
        })
    );
    let e = call(&t, Method::POST, "/api/gps/start", json!({})).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (409, "No GPS receiver or USB serial device found"));
    let e = call(&t, Method::POST, "/api/gps/start", json!({ "path": "/etc/passwd" })).await.unwrap_err();
    assert_eq!(e.status, 400);
    let e = call(&t, Method::POST, "/api/gps/start", json!({ "path": "/dev/ttyUSB99" })).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (404, "/dev/ttyUSB99 doesn't exist"));
    let e = call(&t, Method::POST, "/api/gps/use", json!({ "device": "x" })).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (404, "No serial device \"x\""));
}

#[tokio::test]
async fn refuses_a_tty_someone_holds() {
    let t = start("exec sleep 30", true).await;
    let (m, s, path) = pty();
    let e = call(&t, Method::POST, "/api/gps/start", json!({ "path": path })).await.unwrap_err();
    assert_eq!(e.status, 409);
    assert!(
        e.message.starts_with(&format!("{path} is already open by BenchBob (serial panel) (pid {})", std::process::id())),
        "{}",
        e.message
    );
    // SAFETY: closing fds we opened.
    unsafe {
        libc::close(s);
        libc::close(m);
    }
}

#[tokio::test]
async fn runs_gpsd_and_follows_its_json() {
    let t = start("exec sleep 30", false).await;
    let (m, s, path) = pty();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut c, _) = l.accept().await.unwrap();
        let mut buf = [0u8; 256];
        let n = c.read(&mut buf).await.unwrap();
        let watch = String::from_utf8_lossy(&buf[..n]).to_string();
        c.write_all(WATCH.as_bytes()).await.unwrap();
        // Hold the connection open until the test ends.
        tokio::time::sleep(Duration::from_secs(20)).await;
        watch
    });
    let v = call(&t, Method::POST, "/api/gps/start", json!({ "path": path, "port": port, "readOnly": true })).await.unwrap();
    assert_eq!(v["running"], true);
    assert_eq!(v["device"], path);
    assert_eq!(v["key"], Value::Null);
    assert_eq!(v["pps"], json!(["/dev/pps0"]));
    assert_eq!(v["connect"], format!("127.0.0.1:{port} (this Pi only)"));
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        ["running", "device", "key", "since", "port", "share", "readOnly", "pps", "connected", "connect", "gpsd", "fix", "satellites"]
    );
    // The fixture's fix arrives over the socket.
    let mut s3 = Value::Null;
    for _ in 0..100 {
        s3 = call(&t, Method::GET, "/api/gps", Value::Null).await.unwrap();
        if s3["fix"]["satsVisible"] == 19 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(s3["fix"]["fix"], "3D");
    assert_eq!(s3["gpsd"]["version"], "3.25");
    assert_eq!(s3["summary"], format!("3D · 14/19 sats · {path}"));
    assert_eq!(s3["settings"], json!({ "port": port, "share": false, "readOnly": true, "pps": true }));
    assert!(s3["log"][0]["line"].as_str().unwrap().contains(&format!("-N -n -D 1 -S {port} -b {path} /dev/pps0")));

    // Starting again on the same device just answers the view.
    let again = call(&t, Method::POST, "/api/gps/start", json!({ "path": path, "port": port })).await.unwrap();
    assert_eq!(again["since"], v["since"]);

    let tl = call_tool(&t.d.tools(), "gps_status", json!({})).await.unwrap();
    assert!(tl.get("satellites").is_none() && tl.get("log").is_none());
    let tl = call_tool(&t.d.tools(), "gps_status", json!({ "satellites": true })).await.unwrap();
    assert_eq!(tl["satellites"].as_array().unwrap().len(), 19);
    assert_eq!(tl.as_object().unwrap().keys().next_back().unwrap(), "satellites");

    call(&t, Method::POST, "/api/gps/stop", Value::Null).await.unwrap();
    let st = call(&t, Method::GET, "/api/gps", Value::Null).await.unwrap();
    assert_eq!(st["running"], false);
    assert_eq!(st["lastExit"]["code"], 128 + libc::SIGTERM);
    let texts: Vec<String> = t.ctx.hub.timeline.list().into_iter().map(|e| e.text).collect();
    assert_eq!(
        texts,
        [
            format!("gpsd started on {path} (port {port}, local only)"),
            format!("gpsd exited ({})", 128 + libc::SIGTERM),
            format!("gpsd stopped ({path})")
        ]
    );
    server.abort();
    let _ = server.await;
    // SAFETY: closing fds we opened.
    unsafe {
        libc::close(s);
        libc::close(m);
    }
    drop(t.dir);
}

#[tokio::test]
async fn reports_why_gpsd_gave_up() {
    let t = start("echo 'gpsd:ERROR: can not bind to port' >&2; exit 1", false).await;
    let (m, s, path) = pty();
    let e = call(&t, Method::POST, "/api/gps/start", json!({ "path": path, "port": 1 })).await.unwrap_err();
    assert_eq!(e.status, 503);
    assert!(e.message.starts_with("gpsd exited: $ "), "{}", e.message);
    assert!(e.message.ends_with(" · gpsd:ERROR: can not bind to port"), "{}", e.message);
    let st = call(&t, Method::GET, "/api/gps", Value::Null).await.unwrap();
    assert_eq!(st["lastExit"]["code"], 1);
    let e = call(&t, Method::POST, "/api/gps/start", json!({ "path": path, "port": 0 })).await.unwrap_err();
    assert_eq!(e.message, "port must be 1–65535");
    // SAFETY: closing fds we opened.
    unsafe {
        libc::close(s);
        libc::close(m);
    }
}

#[test]
fn tty_paths() {
    assert!(is_tty("/dev/ttyACM0") && is_tty("/dev/ttyUSB12") && !is_tty("/dev/ttyS0") && !is_tty("/dev/ttyACM"));
    assert!(
        path_ok("/dev/ttyS0")
            && path_ok("/dev/pts/3")
            && path_ok("/dev/gnss0")
            && path_ok("/dev/serial/by-id/usb-u-blox_AG_-_www.u-blox.com_u-blox_7-if00")
    );
    assert!(!path_ok("/dev/sda") && !path_ok("/dev/pts/x") && !path_ok("/dev/serial/by-id/../../sda"));
}
