//! The driver with the simulated camera and a stand-in ustreamer: the child is a script that
//! only has to run, and the test itself answers ustreamer's HTTP API (/state, /snapshot,
//! /stream) on the unix socket the driver tells ustreamer to use.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use axum::http::Method;
use benchbob_core::util::{base64_decode, call_route, call_tool, test_ctx};
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

use super::*;

const JPEG: &[u8] = b"\xff\xd8\xff\xe0fake jpeg\xff\xd9";

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// ustreamer's HTTP API, enough of it: one request per connection.
fn fake_ustreamer(sock: &Path) -> tokio::task::JoinHandle<()> {
    let _ = std::fs::remove_file(sock);
    let l = UnixListener::bind(sock).unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut c, _)) = l.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 2048];
                let n = c.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                match path.as_str() {
                    "/state" => {
                        let b = r#"{"ok":true,"result":{"source":{"online":false,"captured_fps":0.0},"stream":{"clients":0}}}"#;
                        let _ = c
                            .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len()).as_bytes())
                            .await;
                    }
                    "/snapshot" => {
                        let _ = c
                            .write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    JPEG.len()
                                )
                                .as_bytes(),
                            )
                            .await;
                        let _ = c.write_all(JPEG).await;
                    }
                    "/stream" => {
                        let _ = c
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace;boundary=boundarydonotcross\r\nConnection: close\r\n\r\n")
                            .await;
                        // Frames until the viewer hangs up.
                        loop {
                            let part =
                                format!("--boundarydonotcross\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", JPEG.len());
                            if c.write_all(part.as_bytes()).await.is_err() || c.write_all(JPEG).await.is_err() {
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                    _ => {
                        let _ = c.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                    }
                }
            });
        }
    })
}

struct T {
    d: Camera,
    ctx: Ctx,
    dir: tempfile::TempDir,
}

async fn start(ustreamer: &str) -> T {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    let cfg = Config {
        run_dir: p.join("run"),
        sim: "/dev/video99".into(),
        port_offset: 3000,
        web_port: 8097,
        ustreamer: script(p, "ustreamer", ustreamer),
        v4l2_ctl: script(p, "v4l2-ctl", "exit 1"),
        // Missing: no CSI list, no crash.
        rpicam_hello: p.join("no-rpicam-hello"),
    };
    let d = Camera::with_config(cfg);
    let ctx = test_ctx("camera", p);
    d.init(ctx.clone()).await.unwrap();
    T { d, ctx, dir }
}

async fn call(t: &T, method: Method, path: &str, query: &[(&str, &str)], body: Value) -> Result<Reply, HttpError> {
    call_route(&t.d.routes(), method, path, query, body).await
}

async fn get_json(t: &T, path: &str) -> Value {
    match call(t, Method::GET, path, &[], Value::Null).await.unwrap() {
        Reply::Json(v) => v,
        Reply::Response(_) => panic!("not JSON"),
    }
}

async fn body_of(r: Reply) -> (axum::http::HeaderMap, Bytes) {
    let Reply::Response(r) = r else { panic!("not a response") };
    let h = r.headers().clone();
    (h, r.into_body().collect().await.unwrap().to_bytes())
}

#[tokio::test]
async fn simulated_camera_listing() {
    let t = start("exec sleep 30").await;
    let v = get_json(&t, "/api/camera").await;
    assert_eq!(
        v,
        json!({ "cameras": [{
            "id": "sim", "dev": "/dev/video99", "name": "Simulated camera", "vendor": "BenchBob", "vid": "0000", "pid": "0000",
            "serial": "", "kind": "webcam", "usbPort": "-", "driver": "simulated",
            "stream": { "fourcc": "MJPG", "w": 640, "h": 480, "fps": 30 },
            "streaming": false, "viewers": 0, "fps": 0, "signal": null, "share": false, "sharePort": 11080, "lastSnapshot": 0, "error": "",
        }], "csi": [] })
    );
    let s = t.d.status().unwrap();
    assert_eq!((s["active"].clone(), s["summary"].clone()), (json!(true), json!("Simulated camera")));
    let one = get_json(&t, "/api/camera/sim").await;
    assert_eq!(
        one["formats"],
        json!([{ "fourcc": "MJPG", "description": "Motion-JPEG, compressed", "compressed": true, "sizes": [{ "w": 640, "h": 480, "fps": [30, 15] }] }])
    );
    assert_eq!(one["controls"], json!([]));
    let e = call(&t, Method::GET, "/api/camera/video5", &[], Value::Null).await.err().unwrap();
    assert_eq!((e.status, e.message.as_str()), (404, "No camera \"video5\" (have: sim)"));
    let e = call(&t, Method::GET, "/api/camera/sim/last.jpg", &[], Value::Null).await.err().unwrap();
    assert_eq!((e.status, e.message.as_str()), (404, "No snapshot taken yet"));
    let l = call_tool(&t.d.tools(), "camera_list", json!({})).await.unwrap();
    assert_eq!(l["cameras"][0]["streamUrl"], format!("http://{}:8097/api/camera/sim/stream", hostname()));
}

#[tokio::test]
async fn snapshots_streams_and_idle_stop() {
    let t = start("exec sleep 30").await;
    let server = fake_ustreamer(&t.dir.path().join("run/sim.sock"));

    let r = call(&t, Method::GET, "/api/camera/sim/snapshot", &[("download", "1")], Value::Null).await.unwrap();
    let (h, b) = body_of(r).await;
    assert_eq!(&b[..], JPEG);
    assert_eq!(h["content-type"], "image/jpeg");
    assert!(h["content-disposition"].to_str().unwrap().starts_with("attachment; filename=\"sim-20"));
    let (h, b) = body_of(call(&t, Method::GET, "/api/camera/sim/last.jpg", &[], Value::Null).await.unwrap()).await;
    assert_eq!(&b[..], JPEG);
    assert!(h["x-taken-at"].to_str().unwrap().ends_with('Z'));
    let c = &get_json(&t, "/api/camera").await["cameras"][0];
    assert_eq!((c["streaming"].clone(), c["signal"].clone(), c["fps"].clone()), (json!(true), json!(false), json!(0)));
    assert!(c["lastSnapshot"].as_u64().unwrap() > 0);
    assert_eq!(t.d.status().unwrap()["summary"], "Simulated camera · live, 0 viewers");

    // A viewer through BenchBob counts until it hangs up.
    let Reply::Response(r) = call(&t, Method::GET, "/api/camera/sim/stream", &[], Value::Null).await.unwrap() else { panic!() };
    assert!(r.headers()["content-type"].to_str().unwrap().starts_with("multipart/x-mixed-replace"));
    let mut body = r.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(first.starts_with(b"--boundarydonotcross"));
    assert_eq!(get_json(&t, "/api/camera").await["cameras"][0]["viewers"], 1);
    drop(body);
    assert_eq!(get_json(&t, "/api/camera").await["cameras"][0]["viewers"], 0);

    // The MCP snapshot returns the picture itself.
    let s = call_tool(&t.d.tools(), "camera_snapshot", json!({})).await.unwrap();
    assert_eq!(s["content"][0]["type"], "image");
    assert_eq!(base64_decode(s["content"][0]["data"].as_str().unwrap()), JPEG);
    let meta = &s["structured"];
    let keys: Vec<&String> = meta.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["url", "path", "camera", "bytes", "width", "height", "takenAt"]);
    assert_eq!(meta["bytes"], JPEG.len());
    assert_eq!(serde_json::from_str::<Value>(s["content"][1]["text"].as_str().unwrap()).unwrap(), *meta);

    // Config: a size the camera doesn't offer, then a frame rate it does (restarts it).
    let e = call(&t, Method::POST, "/api/camera/sim/config", &[], json!({ "w": 800, "h": 600 })).await.err().unwrap();
    assert_eq!((e.status, e.message.as_str()), (400, "Simulated camera doesn't offer MJPG 800x600"));
    let Reply::Json(v) = call(&t, Method::POST, "/api/camera/sim/config", &[], json!({ "fps": 15 })).await.unwrap() else { panic!() };
    assert_eq!(v["camera"]["stream"], json!({ "fourcc": "MJPG", "w": 640, "h": 480, "fps": 15 }));
    assert_eq!(v["camera"]["streaming"], true);
    let e = call(&t, Method::POST, "/api/camera/sim/config", &[], json!({ "sharePort": 80 })).await.err().unwrap();
    assert_eq!(e.message, "sharePort must be 1024–65535");
    assert_eq!(t.ctx.setting("cameras").unwrap()["sim"]["stream"]["fps"], 15);

    // Nobody looking for a minute: the sweep stops it.
    lock(&t.d.inner.st).cams.get_mut("sim").unwrap().last_use = 0;
    t.d.inner.idle_sweep().await;
    assert_eq!(get_json(&t, "/api/camera").await["cameras"][0]["streaming"], false);

    // Stop is idempotent.
    let Reply::Json(v) = call(&t, Method::POST, "/api/camera/sim/stop", &[], Value::Null).await.unwrap() else { panic!() };
    assert_eq!(v, json!({ "ok": true }));
    server.abort();
}

#[tokio::test]
async fn ustreamer_giving_up_is_reported() {
    let t = start("echo \"-- ERROR [12.3 main] Can't open device /dev/video99: No such file\" >&2; exit 1").await;
    let e = call(&t, Method::GET, "/api/camera/sim/snapshot", &[], Value::Null).await.err().unwrap();
    assert_eq!(e.status, 503);
    assert_eq!(e.message, "-- ERROR [12.3 main] Can't open device /dev/video99: No such file");
    let c = &get_json(&t, "/api/camera").await["cameras"][0];
    assert_eq!(c["error"], "-- ERROR [12.3 main] Can't open device /dev/video99: No such file");
    assert_eq!(c["streaming"], false);
}

#[tokio::test]
async fn controls_need_a_camera_that_has_them() {
    let t = start("exec sleep 30").await;
    let e = call(&t, Method::POST, "/api/camera/sim/controls", &[], json!({ "brightness": 1 })).await.err().unwrap();
    assert_eq!((e.status, e.message.as_str()), (400, "Simulated camera has no control \"brightness\""));
    let e = call(&t, Method::POST, "/api/camera/sim/controls", &[], json!([1])).await.err().unwrap();
    assert_eq!(e.message, "Simulated camera has no control \"0\"");
    let e = call(&t, Method::POST, "/api/camera/sim/controls", &[], json!(5)).await.err().unwrap();
    assert_eq!(e.message, "Expected { name: value, … }");
    let Reply::Json(v) = call(&t, Method::POST, "/api/camera/sim/controls/reset", &[], Value::Null).await.unwrap() else { panic!() };
    assert_eq!(v, json!({ "ok": true, "controls": [] }));
}
