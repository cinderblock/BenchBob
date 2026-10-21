//! Driver-level tests through the routes, the WebSocket and the socketcand server. The ones
//! that need a bus run on a vcan named by BENCHBOB_CAN_TEST_VCAN (up, FD MTU; no root
//! needed once it exists), and skip otherwise.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, Method};
use benchbob_core::driver::{Hub as CoreHub, WsOut};
use benchbob_core::settings::Settings;
use benchbob_core::{Ctx, Driver, Reply, Request, WsClient, WsIn, WsSession};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::*;

fn vcan() -> Option<String> {
    std::env::var("BENCHBOB_CAN_TEST_VCAN").ok().filter(|s| !s.is_empty())
}

struct T {
    can: Can,
    _dir: tempfile::TempDir,
}

async fn start(sc_port: u16) -> T {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings::load(dir.path().join("peripherals.json"));
    settings.own("can");
    settings.set("can", "socketcandPort", json!(sc_port));
    let ctx = Ctx::new("can", Arc::new(CoreHub::new(settings)));
    let can = Can::with_rec_dir(dir.path().join("rec"));
    can.init(ctx).await.unwrap();
    T { can, _dir: dir }
}

impl T {
    async fn call(&self, method: Method, path: &str, body: Value) -> (u16, Value) {
        let (p, q) = path.split_once('?').unwrap_or((path, ""));
        let routes = self.can.routes();
        let mut best: Option<(usize, HashMap<String, String>)> = None;
        for (n, r) in routes.iter().enumerate() {
            if r.method != method {
                continue;
            }
            if let Some(params) = benchbob_core::http::match_path(&r.path, p)
                && best.as_ref().is_none_or(|(_, b)| params.len() < b.len())
            {
                best = Some((n, params));
            }
        }
        let (n, params) = best.unwrap_or_else(|| panic!("no route {method} {p}"));
        let req = Request {
            method,
            path: p.into(),
            params,
            query: benchbob_core::http::parse_query(q),
            headers: HeaderMap::new(),
            body: if body.is_null() { Default::default() } else { body.to_string().into() },
        };
        match (routes[n].handler)(req).await {
            Ok(Reply::Json(v)) => (200, v),
            Ok(Reply::Response(r)) => {
                let s = r.status().as_u16();
                let b = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
                (s, Value::String(String::from_utf8_lossy(&b).into_owned()))
            }
            Err(e) => (e.status, json!({ "ok": false, "message": e.message })),
        }
    }

    async fn tool(&self, name: &str, args: Value) -> Value {
        let t = self.can.tools().into_iter().find(|t| t.name == name).unwrap();
        benchbob_core::mcp::call_tool(&t, args).await
    }

    fn ws(&self, iface: &str, query: &[(&str, &str)]) -> (mpsc::UnboundedSender<WsIn>, mpsc::UnboundedReceiver<WsOut>) {
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let s = WsSession {
            client: WsClient::new(out_tx),
            params: HashMap::from([("iface".to_string(), iface.to_string())]),
            query: query.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            incoming: in_rx,
        };
        let h = self.can.sockets().remove(0).handler;
        tokio::spawn(h(s));
        (in_tx, out_rx)
    }
}

async fn next_json(rx: &mut mpsc::UnboundedReceiver<WsOut>) -> Value {
    match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.expect("ws message") {
        Some(WsOut::Text(t)) => serde_json::from_str(&t).unwrap(),
        Some(WsOut::Close) | Some(WsOut::CloseWith(..)) => json!({ "closed": true }),
        Some(WsOut::Binary(b)) => json!({ "binary": b.len() }),
        None => json!({ "gone": true }),
    }
}

#[tokio::test]
async fn unknown_interfaces_and_bad_input() {
    let t = start(0).await;
    let (s, v) = t.call(Method::GET, "/api/can/nosuch0", Value::Null).await;
    assert_eq!((s, v["message"].as_str().unwrap()), (404, "No CAN interface \"nosuch0\""));
    let (s, v) = t.call(Method::POST, "/api/can/nosuch0/send", json!({ "id": "zz" })).await;
    assert_eq!((s, v["message"].as_str().unwrap()), (400, "bad id \"zz\""));
    let (s, _) = t.call(Method::POST, "/api/can/nosuch0/send", json!({ "id": 1 })).await;
    assert_eq!(s, 404);
    let (s, v) = t.call(Method::GET, "/api/can/recordings/..%2Fx.log", Value::Null).await;
    assert_eq!((s, v["message"].as_str().unwrap()), (400, "Bad recording name"));
    let (s, v) = t.call(Method::POST, "/api/can/socketcand", json!({ "port": 80 })).await;
    assert_eq!((s, v["message"].as_str().unwrap()), (400, "port must be 1024–65535"));
    let (s, v) = t.call(Method::POST, "/api/can/vcan", json!({ "name": "1bad" })).await;
    assert_eq!((s, v["message"].as_str().unwrap()), (400, "Interface names are 1–15 letters, digits, - or _"));
    let r = t.tool("can_stats", json!({})).await;
    assert_eq!(r["content"][0]["text"], "No CAN interface \"undefined\"");
    let (_, l) = t.call(Method::GET, "/api/can", Value::Null).await;
    let keys: Vec<&String> = l.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["ifaces", "slcan", "socketcand", "recordings"]);
    let st = t.can.status().unwrap();
    let keys: Vec<&String> = st.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["active", "summary", "ifaces", "slcan", "socketcand", "recordings", "recDir", "standardBitrates", "slcanBitrates"]);
    assert_eq!(st["slcanBitrates"], json!([10000, 20000, 50000, 100000, 125000, 250000, 500000, 800000, 1000000]));
    t.can.stop(StopReason::Shutdown).await;
}

#[tokio::test]
async fn vcan_traffic_ws_capture_jobs_recording() {
    let Some(name) = vcan() else { return eprintln!("BENCHBOB_CAN_TEST_VCAN not set: skipped") };
    let _g = VCAN_LOCK.lock().await;
    let t = start(0).await;
    let (_, l) = t.call(Method::GET, "/api/can", Value::Null).await;
    let i = l["ifaces"].as_array().unwrap().iter().find(|i| i["name"] == name.as_str()).expect("vcan listed").clone();
    assert_eq!(i["key"], format!("vcan:{name}"));
    assert_eq!(i["label"], "Virtual CAN");
    assert_eq!(i["reader"], "open");
    assert_eq!(i["fdOn"], true);
    let keys: Vec<&String> = i.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "name",
            "kind",
            "driver",
            "label",
            "key",
            "up",
            "fdCapable",
            "fdOn",
            "ctrl",
            "ctrlSupported",
            "stats",
            "bringUpOnPlug",
            "saved",
            "busy",
            "autobitrate",
            "lastAutobitrate",
            "reader",
            "rates",
            "totals",
            "drops",
            "idCount",
            "jobs",
            "recording",
            "wsClients"
        ]
    );

    // WebSocket: hello, then frames one by one.
    let (ws_in, mut ws) = t.ws(&name, &[("stats", "1")]);
    let hello = next_json(&mut ws).await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["iface"]["name"], name.as_str());
    assert_eq!(next_json(&mut ws).await["type"], "stats");

    // A capture sees our own frame come back as tx.
    let cap = {
        let i = t.can.inner.clone();
        let n = name.clone();
        tokio::spawn(async move { i.capture(&n, 500.0, None, 2.0).await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (s, v) = t.call(Method::POST, &format!("/api/can/{name}/send"), json!({ "cansend": "123#DEADBEEF" })).await;
    assert_eq!((s, v), (200, json!({ "ok": true, "sent": "123#DEADBEEF" })));
    let (s, _) = t
        .call(
            Method::POST,
            &format!("/api/can/{name}/send"),
            json!({ "id": "0x18DAF110", "fd": true, "brs": true, "data": "00112233445566778899" }),
        )
        .await;
    assert_eq!(s, 200);
    let c = cap.await.unwrap();
    assert_eq!(c["count"], 2);
    assert_eq!(c["truncated"], true);
    let f = &c["frames"][0];
    assert_eq!((f["id"].clone(), f["dir"].clone(), f["data"].clone()), (json!(0x123), json!("tx"), json!("DEADBEEF")));
    let keys: Vec<&String> = f.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["id", "ext", "rtr", "fd", "brs", "len", "data", "ts", "dir"]);
    assert_eq!(c["frames"][1]["len"], 12);

    // The WebSocket got both as {type: frame}.
    let mut got = Vec::new();
    while got.len() < 2 {
        let m = next_json(&mut ws).await;
        if m["type"] == "frame" {
            got.push(m);
        }
    }
    assert_eq!(got[0]["id"], 0x123);
    // ...and can send.
    ws_in.send(WsIn::Text(json!({ "send": { "cansend": "7FF#01" } }).to_string())).unwrap();
    ws_in.send(WsIn::Text("not json".into())).unwrap();
    let mut saw_err = false;
    let mut saw_7ff = false;
    while !(saw_err && saw_7ff) {
        let m = next_json(&mut ws).await;
        saw_err |= m["type"] == "error" && m["message"] == "Messages must be JSON";
        saw_7ff |= m["type"] == "frame" && m["id"] == 0x7ff;
    }

    // Per-ID table and stats.
    let (_, d) = t.call(Method::GET, &format!("/api/can/{name}"), Value::Null).await;
    assert_eq!(d["totals"]["tx"], 3);
    assert_eq!(d["ids"].as_array().unwrap().len(), 3);
    assert_eq!(d["ids"][0]["id"], 0x123); // standard ids first
    assert_eq!(d["ids"][2]["ext"], true);
    let st = t.tool("can_stats", json!({ "iface": name, "limit": 1 })).await;
    let sc = &st["structuredContent"];
    assert_eq!(sc["ids"][0]["id"], "123");
    assert!(sc["ids"][0]["last"].as_str().unwrap().ends_with('Z'));
    assert_eq!(sc["idsTruncated"], true);
    assert!(sc["ids"][0].get("hot").is_none());

    // Periodic job.
    let (s, j) = t.call(Method::POST, &format!("/api/can/{name}/periodic"), json!({ "periodMs": 10, "id": "321", "data": "AA" })).await;
    assert_eq!(s, 200, "{j}");
    assert_eq!(j["frame"], "321#AA");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (_, d) = t.call(Method::GET, &format!("/api/can/{name}"), Value::Null).await;
    assert!(d["jobs"][0]["sent"].as_u64().unwrap() >= 5, "{}", d["jobs"]);
    let (s, _) = t.call(Method::DELETE, &format!("/api/can/{name}/periodic/{}", j["id"]), Value::Null).await;
    assert_eq!(s, 200);
    let (s, v) = t.call(Method::DELETE, &format!("/api/can/{name}/periodic/{}", j["id"]), Value::Null).await;
    assert_eq!((s, v["message"].clone()), (404, json!(format!("No job {}", j["id"]))));
    let (s, v) = t.call(Method::POST, &format!("/api/can/{name}/periodic"), json!({ "periodMs": 1, "id": 1 })).await;
    assert_eq!((s, v["message"].clone()), (400, json!("periodMs must be 5–3600000")));

    // Recording.
    let (s, r) = t.call(Method::POST, &format!("/api/can/{name}/record"), json!({ "on": true })).await;
    assert_eq!(s, 200);
    let file = r["file"].as_str().unwrap().to_string();
    t.call(Method::POST, &format!("/api/can/{name}/send"), json!({ "cansend": "123#R2" })).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (s, v) = t.call(Method::DELETE, &format!("/api/can/recordings/{file}"), Value::Null).await;
    assert_eq!((s, v["message"].clone()), (409, json!("Stop the recording first")));
    let (s, r) = t.call(Method::POST, &format!("/api/can/{name}/record"), json!({ "on": false })).await;
    assert_eq!((s, r["frames"].clone()), (200, json!(1)), "{r}");
    let keys: Vec<&String> = r.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["ok", "file", "frames", "bytes"]);
    let (_, list) = t.call(Method::GET, "/api/can/recordings", Value::Null).await;
    assert_eq!(list[0]["file"], file.as_str());
    let (s, body) = t.call(Method::GET, &format!("/api/can/recordings/{file}"), Value::Null).await;
    assert_eq!(s, 200);
    let line = body.as_str().unwrap();
    assert!(line.ends_with(&format!(" {name} 123#R2\n")), "{line}");
    let p = frame::parse_candump_line(line.trim()).unwrap().unwrap();
    assert!(p.frame.rtr && p.frame.len == 2);
    let (s, _) = t.call(Method::DELETE, &format!("/api/can/recordings/{file}"), Value::Null).await;
    assert_eq!(s, 200);

    // Clear.
    t.call(Method::POST, &format!("/api/can/{name}/clear"), Value::Null).await;
    let (_, d) = t.call(Method::GET, &format!("/api/can/{name}"), Value::Null).await;
    assert_eq!((d["idCount"].clone(), d["totals"]["tx"].clone()), (json!(0), json!(0)));
    t.can.stop(StopReason::Shutdown).await;
}

async fn read_until(s: &mut tokio::net::TcpStream, buf: &mut String, want: &str) {
    let mut b = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !buf.contains(want) {
        let n = tokio::time::timeout_at(deadline, s.read(&mut b))
            .await
            .unwrap_or_else(|_| panic!("waiting for {want:?}, got {buf:?}"))
            .unwrap();
        assert!(n > 0, "closed while waiting for {want:?}, got {buf:?}");
        buf.push_str(&String::from_utf8_lossy(&b[..n]));
    }
}

#[tokio::test]
async fn socketcand_raw_and_bcm() {
    let Some(name) = vcan() else { return eprintln!("BENCHBOB_CAN_TEST_VCAN not set: skipped") };
    let _g = VCAN_LOCK.lock().await;
    let port = 29536 + 7000 + (std::process::id() % 1000) as u16;
    let t = start(port).await;
    assert_eq!(t.can.inner.socketcand_out()["listening"], true);
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut buf = String::new();
    read_until(&mut s, &mut buf, "< hi >").await;
    s.write_all(b"< open nosuch9 >").await.unwrap();
    read_until(&mut s, &mut buf, "< error could not open bus >").await;

    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut buf = String::new();
    read_until(&mut s, &mut buf, "< hi >").await;
    s.write_all(format!("< open {name} >").as_bytes()).await.unwrap();
    read_until(&mut s, &mut buf, "< ok >").await;
    s.write_all(b"< rawmode >").await.unwrap();
    read_until(&mut s, &mut buf, "< ok >< ok >").await;
    let out = t.can.inner.socketcand_out();
    assert_eq!(out["clients"][0]["mode"], "raw");
    assert_eq!(out["clients"][0]["iface"], name.as_str());
    // A frame from the hub reaches the client (through the kernel's loopback).
    t.call(Method::POST, &format!("/api/can/{name}/send"), json!({ "cansend": "1ABCDEF0#0102" })).await;
    read_until(&mut s, &mut buf, "< frame 1ABCDEF0 ").await;
    assert!(buf.contains(" 0102 >"), "{buf}");
    // And the client's send reaches the hub.
    let cap = {
        let i = t.can.inner.clone();
        let n = name.clone();
        tokio::spawn(async move { i.capture(&n, 1000.0, frame::parse_filter_string(Some("456")).unwrap(), 1.0).await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    s.write_all(b"< send 456 2 aa b >").await.unwrap();
    let c = cap.await.unwrap();
    assert_eq!(c["frames"][0]["data"], "AA0B");
    assert_eq!(c["frames"][0]["dir"], "rx");
    // BCM: subscribe, then only that id arrives, space-separated.
    s.write_all(b"< bcmmode >< subscribe 0 0 789 >").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    buf.clear();
    t.call(Method::POST, &format!("/api/can/{name}/send"), json!({ "cansend": "788#01" })).await;
    t.call(Method::POST, &format!("/api/can/{name}/send"), json!({ "cansend": "789#0203" })).await;
    read_until(&mut s, &mut buf, "< frame 789 ").await;
    assert!(buf.contains(" 02 03  >") && !buf.contains("788"), "{buf}");
    s.write_all(b"< echo >").await.unwrap();
    read_until(&mut s, &mut buf, "< echo >").await;
    drop(s);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(t.can.inner.socketcand_out()["clients"].as_array().unwrap().len(), 0);
    // Settings changes restart the listener on the same port: it must be free again by then.
    let (_, v) = t.call(Method::POST, "/api/can/socketcand", json!({ "errMask": 4 })).await;
    assert_eq!((v["listening"].clone(), v["error"].clone(), v["errMask"].clone()), (json!(true), json!(""), json!(4)));
    t.can.stop(StopReason::Shutdown).await;
}

#[test]
fn out_frames_put_esi_err_and_text_last() {
    // toOut() builds the object, then adds esi/err (and the driver adds text): they follow dir.
    let mut f = frame::make_frame(&frame::spec_from_json(&json!({ "cansend": "456##3AABB" })).unwrap()).unwrap();
    f.ts = 1700000000123.4567;
    let o = hub::to_out(&f);
    assert_eq!(
        serde_json::to_string(&o.json()).unwrap(),
        r#"{"id":1110,"ext":false,"rtr":false,"fd":true,"brs":true,"len":2,"data":"AABB","ts":1700000000.123457,"dir":"rx","esi":true}"#
    );
    let mut e = o.clone();
    (e.err, e.esi, e.text) = (true, false, Some("bus off".into()));
    let keys: Vec<String> = e.json().as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["id", "ext", "rtr", "fd", "brs", "len", "data", "ts", "dir", "err", "text"]);
}
