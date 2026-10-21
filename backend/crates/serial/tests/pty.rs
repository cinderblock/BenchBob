//! The serial driver end to end against pseudo-terminals: the test holds the master side
//! (the "device"), BenchBob opens the slave as a manually added port. Everything goes
//! through the driver's own routes, WebSocket handler, MCP tools and bridges.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, Method};
use benchbob_core::driver::{Hub, WsOut};
use benchbob_core::http::match_path;
use benchbob_core::settings::Settings;
use benchbob_core::{Ctx, Driver, Reply, Request, WsClient, WsIn, WsSession};
use benchbob_serial::Serial;
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// One test at a time: the kernel reuses a freed pty number at once, so a test opening a
/// pty in parallel could make another test's closed `/dev/pts/N` exist again.
static PTYS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Pty {
    master: tokio::fs::File,
    path: String,
}

fn pty() -> Pty {
    // SAFETY: plain libc pty calls; the master fd is owned by the File.
    unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(m >= 0);
        assert_eq!(libc::grantpt(m), 0);
        assert_eq!(libc::unlockpt(m), 0);
        let mut buf = [0 as libc::c_char; 128];
        assert_eq!(libc::ptsname_r(m, buf.as_mut_ptr(), buf.len()), 0);
        let path = std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
        // Raw mode on the slave side's line discipline, so bytes pass untouched both ways.
        let s = libc::open(std::ffi::CString::new(path.clone()).unwrap().as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        let mut t: libc::termios = std::mem::zeroed();
        libc::tcgetattr(s, &mut t);
        libc::cfmakeraw(&mut t);
        libc::tcsetattr(s, libc::TCSANOW, &t);
        libc::close(s);
        Pty { master: tokio::fs::File::from_std(std::fs::File::from(OwnedFd::from_raw_fd(m))), path }
    }
}

struct Bob {
    drv: Serial,
    _dir: tempfile::TempDir,
}

async fn bob() -> Bob {
    let dir = tempfile::tempdir().unwrap();
    let hub = Arc::new(Hub::new(Settings::load(dir.path().join("peripherals.json"))));
    hub.settings.own("serial");
    let drv = Serial::new();
    drv.init(Ctx::new("serial", hub)).await.unwrap();
    Bob { drv, _dir: dir }
}

impl Bob {
    async fn call(&self, method: Method, path: &str, body: Value) -> (u16, Value) {
        let (q, b) = match method {
            Method::GET => (
                body.as_object()
                    .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect())
                    .unwrap_or_default(),
                Bytes::new(),
            ),
            _ => (vec![], Bytes::from(body.to_string())),
        };
        let r = self.raw(method, path, q, b, HeaderMap::new()).await;
        match r {
            Ok(Reply::Json(v)) => (200, v),
            Ok(Reply::Response(_)) => (200, Value::Null),
            Err(e) => (e.status, json!({ "message": e.message })),
        }
    }

    async fn raw(
        &self,
        method: Method,
        path: &str,
        query: Vec<(String, String)>,
        body: Bytes,
        headers: HeaderMap,
    ) -> benchbob_core::RouteResult {
        let routes = self.drv.routes();
        let mut best: Option<(usize, std::collections::HashMap<String, String>)> = None;
        for (i, r) in routes.iter().enumerate() {
            if r.method != method {
                continue;
            }
            if let Some(p) = match_path(&r.path, path)
                && best.as_ref().is_none_or(|b| p.len() < b.1.len())
            {
                best = Some((i, p));
            }
        }
        let (i, params) = best.unwrap_or_else(|| panic!("no route {method} {path}"));
        (routes[i].handler)(Request { method, path: path.into(), params, query, headers, body }).await
    }

    async fn tool(&self, name: &str, args: Value) -> Result<Value, String> {
        let t = self.drv.tools().into_iter().find(|t| t.name == name).unwrap();
        match (t.handler)(args).await {
            Ok(benchbob_core::ToolResult::Value(v)) => Ok(v),
            Ok(_) => Err("content".into()),
            Err(e) => Err(e.message),
        }
    }

    async fn manual(&self, p: &Pty) -> String {
        let (s, v) = self.call(Method::POST, "/api/serial/manual", json!({ "path": p.path })).await;
        assert_eq!(s, 200, "{v}");
        v["id"].as_str().unwrap().to_string()
    }
}

async fn read_some(f: &mut tokio::fs::File, want: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    while out.len() < want {
        match tokio::time::timeout(Duration::from_secs(3), f.read(&mut buf)).await.expect("device read timed out") {
            Ok(n) => out.extend_from_slice(&buf[..n]),
            // EIO until BenchBob has the slave side open.
            Err(e) if e.raw_os_error() == Some(libc::EIO) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(e) => panic!("{e}"),
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn manual_port_open_read_write_and_cursor() {
    let _one_at_a_time = PTYS.lock().await;
    let b = bob().await;
    let mut dev = pty();
    let id = b.manual(&dev).await;
    let (_, list) = b.call(Method::GET, "/api/serial", json!({})).await;
    let port = list["ports"].as_array().unwrap().iter().find(|p| p["id"] == id).unwrap().clone();
    assert_eq!((port["kind"].as_str(), port["open"].as_bool(), port["state"].as_str()), (Some("manual"), Some(false), Some("closed")));
    let keys: Vec<&String> = port.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "id",
            "path",
            "byId",
            "kind",
            "name",
            "vendor",
            "model",
            "vidpid",
            "serial",
            "usbDriver",
            "family",
            "resets",
            "flashTools",
            "console",
            "present",
            "state",
            "open",
            "error",
            "config",
            "configLabel",
            "lines",
            "modem",
            "break",
            "rxBytes",
            "txBytes",
            "lastRx",
            "cursor",
            "openOnPlug",
            "clients",
            "bridges",
            "lock",
            "flash",
            "autobaud"
        ]
    );

    let (s, v) = b.call(Method::POST, &format!("/api/serial/{id}/open"), json!({ "baud": 9600 })).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!((v["open"].as_bool(), v["configLabel"].as_str(), v["modem"].as_bool()), (Some(true), Some("9600 8N1"), Some(false)));

    dev.master.write_all(b"\x1b[32mhello\x1b[0m\r\n").await.unwrap();
    let (_, r) = b.call(Method::GET, &format!("/api/serial/{id}/read"), json!({ "wait_ms": "2000", "cursor": "0" })).await;
    assert_eq!(r["data"], "hello\r\n");
    assert_eq!(r["cursor"], 16);
    let (_, r) = b.call(Method::GET, &format!("/api/serial/{id}/read"), json!({ "cursor": "16", "format": "hex" })).await;
    assert_eq!((r["bytes"].as_u64(), r["more"].as_bool()), (Some(0), Some(false)));
    let (_, r) = b
        .call(Method::GET, &format!("/api/serial/{id}/read"), json!({ "cursor": "0", "max_bytes": "4", "format": "hex", "raw": "1" }))
        .await;
    assert_eq!((r["data"].as_str(), r["more"].as_bool()), (Some("1b 5b 33 32"), Some(true)));

    let (s, v) = b.call(Method::POST, &format!("/api/serial/{id}/write"), json!({ "text": "AT", "eol": "crlf" })).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v, json!({ "written": 4, "cursor": 16 }));
    assert_eq!(read_some(&mut dev.master, 4).await, b"AT\r\n");

    // Lines on a pty: no modem control.
    let (s, v) = b.call(Method::POST, &format!("/api/serial/{id}/lines"), json!({ "dtr": true })).await;
    assert_eq!(s, 409);
    assert!(v["message"].as_str().unwrap().contains("has no controllable modem lines (ENOTTY)"), "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn write_waits_for_a_regex_and_locks_are_honoured() {
    let _one_at_a_time = PTYS.lock().await;
    let b = bob().await;
    let dev = pty();
    let id = b.manual(&dev).await;
    // The "device": answers a command with a prompt.
    let mut m = dev.master;
    tokio::spawn(async move {
        let got = read_some(&mut m, 6).await;
        assert_eq!(got, b"help\r\n");
        m.write_all(b"commands: a b c\r\n").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        m.write_all(b"> ").await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    // Opens the port itself.
    let v = b.tool("serial_write", json!({ "id": id, "text": "help", "eol": "crlf", "wait": "\\n> ?$" })).await.unwrap();
    assert_eq!(v["matched"], true, "{v}");
    assert_eq!(v["match"], json!(["\n> "]));
    assert_eq!(v["output"], "commands: a b c\r\n> ");
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["written", "matched", "match", "elapsed_ms", "output", "bytes", "cursor"]);
    // No match: whatever arrived by the timeout.
    let v = b.tool("serial_write", json!({ "id": id, "text": "x", "wait": "never", "timeout_ms": 150 })).await.unwrap();
    assert_eq!((v["matched"].as_bool(), v.get("match")), (Some(false), None));
    let e = b.tool("serial_write", json!({ "id": id, "text": "x", "wait": "(" })).await.unwrap_err();
    assert!(e.starts_with("Bad wait regex: Invalid regular expression: /(/m: "), "{e}");

    let (_, l) = b.call(Method::POST, &format!("/api/serial/{id}/lock"), json!({ "owner": "ci" })).await;
    let token = l["token"].as_str().unwrap().to_string();
    let (s, v) = b.call(Method::POST, &format!("/api/serial/{id}/write"), json!({ "text": "x" })).await;
    assert_eq!((s, v["message"].as_str()), (423, Some(format!("Writing to {} is locked by ci", dev.path).as_str())));
    let (s, _) = b.call(Method::POST, &format!("/api/serial/{id}/write"), json!({ "text": "x", "token": token })).await;
    assert_eq!(s, 200);
    let (s, v) = b.call(Method::POST, &format!("/api/serial/{id}/unlock"), json!({})).await;
    assert_eq!((s, v["message"].as_str()), (423, Some("Locked by ci; pass its token or force")));
    let (s, _) = b.call(Method::POST, &format!("/api/serial/{id}/unlock"), json!({ "force": true })).await;
    assert_eq!(s, 200);
}

fn ws(b: &Bob, id: &str, query: &[(&str, &str)]) -> (mpsc::UnboundedSender<WsIn>, mpsc::UnboundedReceiver<WsOut>) {
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let sock = b.drv.sockets().pop().unwrap();
    let mut params = std::collections::HashMap::new();
    params.insert("id".to_string(), id.to_string());
    let s = WsSession {
        client: WsClient::new(out_tx),
        params,
        query: query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        incoming: in_rx,
    };
    tokio::spawn((sock.handler)(s));
    (in_tx, out_rx)
}

async fn next(rx: &mut mpsc::UnboundedReceiver<WsOut>) -> WsOut {
    tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.expect("ws timed out").expect("ws closed")
}

async fn next_json(rx: &mut mpsc::UnboundedReceiver<WsOut>, ty: &str) -> Value {
    loop {
        if let WsOut::Text(t) = next(rx).await {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["type"] == ty {
                return v;
            }
        }
    }
}

async fn next_binary(rx: &mut mpsc::UnboundedReceiver<WsOut>) -> Bytes {
    loop {
        if let WsOut::Binary(b) = next(rx).await {
            return b;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_terminal_protocol() {
    let _one_at_a_time = PTYS.lock().await;
    let b = bob().await;
    let mut dev = pty();
    let id = b.manual(&dev).await;

    let (_, mut rx) = ws(&b, "nope", &[]);
    assert_eq!(next_json(&mut rx, "error").await["message"], "No serial port \"nope\"");
    assert!(matches!(next(&mut rx).await, WsOut::CloseWith(4004, r) if r == "no such port"));

    let (tx, mut rx) = ws(&b, &id, &[("open", "1"), ("ts", "1")]);
    let hello = next_json(&mut rx, "hello").await;
    assert_eq!((hello["cursor"].as_u64(), hello["port"]["id"].as_str()), (Some(0), Some(id.as_str())));
    assert_eq!(next_json(&mut rx, "history").await["chunks"], json!([]));
    // Opened for as long as the connection lasts.
    let st = next_json(&mut rx, "status").await;
    assert_eq!(st["port"]["open"], true);

    dev.master.write_all(b"from device").await.unwrap();
    assert_eq!(&next_binary(&mut rx).await[..], b"from device");
    tx.send(WsIn::Binary(Bytes::from_static(b"\x00\x01binary"))).unwrap();
    assert_eq!(read_some(&mut dev.master, 8).await, b"\x00\x01binary");
    tx.send(WsIn::Text("plain text".into())).unwrap();
    assert_eq!(read_some(&mut dev.master, 10).await, b"plain text");
    tx.send(WsIn::Text(json!({ "type": "write", "hex": "41 42", "eol": "lf" }).to_string())).unwrap();
    assert_eq!(read_some(&mut dev.master, 3).await, b"AB\n");
    tx.send(WsIn::Text(json!({ "type": "config", "baud": 57600, "save": false }).to_string())).unwrap();
    let r = next_json(&mut rx, "result").await;
    assert_eq!((r["for"].as_str(), r["ok"].as_bool(), r["config"]["baud"].as_u64()), (Some("config"), Some(true), Some(57600)));
    tx.send(WsIn::Text(json!({ "type": "nope" }).to_string())).unwrap();
    assert_eq!(next_json(&mut rx, "error").await["message"], "Unknown message type \"nope\"");
    tx.send(WsIn::Text(json!({ "type": "lock", "owner": "tab 1" }).to_string())).unwrap();
    let st = loop {
        let s = next_json(&mut rx, "status").await;
        if !s["port"]["lock"].is_null() {
            break s;
        }
    };
    assert_eq!(st["port"]["lock"]["owner"], "tab 1");
    // Another writer is refused; the tab holding the lock isn't.
    let (s, _) = b.call(Method::POST, &format!("/api/serial/{id}/write"), json!({ "text": "x" })).await;
    assert_eq!(s, 423);
    tx.send(WsIn::Text("mine".into())).unwrap();
    assert_eq!(read_some(&mut dev.master, 4).await, b"mine");

    // History for a late joiner, as one binary frame.
    let (_, mut rx2) = ws(&b, &id, &[("quiet", "1")]);
    assert_eq!(&next_binary(&mut rx2).await[..], b"from device");

    // Disconnecting releases the lock and the lease (the port closes).
    drop(tx);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (_, v) = b.call(Method::GET, &format!("/api/serial/{id}"), json!({})).await;
    assert_eq!((v["lock"].clone(), v["open"].as_bool(), v["clients"]["ws"].as_u64()), (Value::Null, Some(false), Some(0)));
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_bridge_carries_bytes_both_ways_and_holds_a_lease() {
    let _one_at_a_time = PTYS.lock().await;
    let b = bob().await;
    let mut dev = pty();
    let id = b.manual(&dev).await;
    let port = free_port();
    let (s, v) = b.call(Method::POST, &format!("/api/serial/{id}/bridge"), json!({ "kind": "tcp", "enabled": true, "port": port })).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!((v["port"].as_u64(), v["listening"].as_bool()), (Some(port as u64), Some(true)));
    assert_eq!(v["commands"][0], format!("nc {} {port}", benchbob_serial::util::hostname()));
    let (s, v) = b.call(Method::POST, &format!("/api/serial/{id}/bridge"), json!({ "kind": "udp", "enabled": true })).await;
    assert_eq!((s, v["message"].as_str()), (400, Some("kind must be \"tcp\" or \"rfc2217\"")));

    let mut c = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    c.write_all(b"to device").await.unwrap();
    assert_eq!(read_some(&mut dev.master, 9).await, b"to device");
    dev.master.write_all(b"\xffto client").await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(3), c.read(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"\xffto client");
    let (_, v) = b.call(Method::GET, &format!("/api/serial/{id}"), json!({})).await;
    assert_eq!((v["open"].as_bool(), v["clients"]["tcp"].as_u64()), (Some(true), Some(1)));
    assert_eq!(v["bridges"]["tcp"]["clients"][0].as_str().map(|s| s.ends_with("127.0.0.1")), Some(true));
    drop(c);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (_, v) = b.call(Method::GET, &format!("/api/serial/{id}"), json!({})).await;
    assert_eq!((v["open"].as_bool(), v["clients"]["tcp"].as_u64()), (Some(false), Some(0)));
    // Off: the listener goes away.
    let (_, v) = b.call(Method::POST, &format!("/api/serial/{id}/bridge"), json!({ "kind": "tcp", "enabled": false })).await;
    assert_eq!(v["listening"], false);
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn rfc2217_bridge_negotiates_and_sets_the_baud_rate() {
    let _one_at_a_time = PTYS.lock().await;
    use benchbob_serial::rfc2217::{DO, IAC, OPT_COM_PORT, SB, SE, WILL};
    let b = bob().await;
    let mut dev = pty();
    let id = b.manual(&dev).await;
    let port = free_port();
    let (s, _) =
        b.call(Method::POST, &format!("/api/serial/{id}/bridge"), json!({ "kind": "rfc2217", "enabled": true, "port": port })).await;
    assert_eq!(s, 200);
    let mut c = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 256];
    while got.len() < 15 {
        let n = tokio::time::timeout(Duration::from_secs(3), c.read(&mut buf)).await.unwrap().unwrap();
        got.extend_from_slice(&buf[..n]);
    }
    assert_eq!(&got[..15], &[IAC, WILL, 3, IAC, WILL, 1, IAC, WILL, 0, IAC, DO, 0, IAC, WILL, OPT_COM_PORT]);
    // DO COM-PORT, then SET-BAUDRATE 9600 and some data with a doubled IAC.
    c.write_all(&[IAC, DO, OPT_COM_PORT, IAC, SB, OPT_COM_PORT, 1, 0, 0, 0x25, 0x80, IAC, SE, b'h', IAC, IAC, b'i']).await.unwrap();
    assert_eq!(read_some(&mut dev.master, 3).await, b"h\xffi");
    let mut got = Vec::new();
    while !got.windows(9).any(|w| w == [IAC, SB, OPT_COM_PORT, 101, 0, 0, 0x25, 0x80, IAC]) {
        let n = tokio::time::timeout(Duration::from_secs(3), c.read(&mut buf)).await.unwrap().unwrap();
        got.extend_from_slice(&buf[..n]);
    }
    let (_, v) = b.call(Method::GET, &format!("/api/serial/{id}"), json!({})).await;
    assert_eq!(v["config"]["baud"], 9600);
    // A remote change is live, not saved as the default.
    let (_, v) = b.call(Method::POST, &format!("/api/serial/{id}/close"), json!({})).await;
    assert_eq!(v["open"], true, "a bridge client still holds it");
    // Serial data is IAC-escaped towards the client.
    dev.master.write_all(b"\xff").await.unwrap();
    let mut got = Vec::new();
    while !got.ends_with(&[IAC, IAC]) {
        let n = tokio::time::timeout(Duration::from_secs(3), c.read(&mut buf)).await.unwrap().unwrap();
        got.extend_from_slice(&buf[..n]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn device_going_away_closes_and_unplugs_the_port() {
    let _one_at_a_time = PTYS.lock().await;
    let b = bob().await;
    let dev = pty();
    let id = b.manual(&dev).await;
    let (s, _) = b.call(Method::POST, &format!("/api/serial/{id}/open"), json!({})).await;
    assert_eq!(s, 200);
    drop(dev.master);
    // The driver notices on its next read: poll for it rather than guessing a delay.
    let mut v = json!({});
    for _ in 0..50 {
        v = b.call(Method::GET, &format!("/api/serial/{id}"), json!({})).await.1;
        if v["open"] == false && v["present"] == false {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!((v["open"].as_bool(), v["present"].as_bool()), (Some(false), Some(false)), "{v}");
    let (s, v) = b.call(Method::POST, &format!("/api/serial/{id}/open"), json!({})).await;
    assert_eq!((s, v["message"].as_str().map(|m| m.ends_with("is unplugged"))), (409, Some(true)));
}

#[tokio::test(flavor = "multi_thread")]
async fn flash_releases_the_port_and_reopens_it() {
    let _one_at_a_time = PTYS.lock().await;
    let b = bob().await;
    let mut dev = pty();
    let id = b.manual(&dev).await;
    let (s, _) = b.call(Method::POST, &format!("/api/serial/{id}/open"), json!({})).await;
    assert_eq!(s, 200);
    // No body, no files: esptool info.
    let Ok(Reply::Json(v)) = b.raw(Method::POST, &format!("/api/serial/{id}/flash"), vec![], Bytes::new(), HeaderMap::new()).await else {
        panic!()
    };
    let job = &v["job"];
    assert_eq!((job["tool"].as_str(), job["action"].as_str(), job["state"].as_str()), (Some("esptool"), Some("info"), Some("preparing")));
    // esptool may or may not be installed here; either way the job ends and the port reopens.
    let mut done = Value::Null;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (_, v) = b.call(Method::GET, &format!("/api/serial/{id}/flash"), json!({})).await;
        if v["job"]["ended"].is_number() {
            done = v;
            break;
        }
    }
    assert!(matches!(done["job"]["state"].as_str(), Some("error" | "done")), "{done}");
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let (_, v) = b.call(Method::GET, &format!("/api/serial/{id}"), json!({})).await;
    assert_eq!((v["open"].as_bool(), v["state"].as_str()), (Some(true), Some("open")), "{v}");
    dev.master.write_all(b"back").await.unwrap();
    let (_, r) = b.call(Method::GET, &format!("/api/serial/{id}/read"), json!({ "wait_ms": "2000", "cursor": "0" })).await;
    assert!(r["data"].as_str().unwrap().ends_with("back"), "{r}");

    let r = b
        .raw(
            Method::POST,
            &format!("/api/serial/{id}/flash"),
            vec![("tool".into(), "avrdude".into()), ("preset".into(), "due".into())],
            Bytes::from_static(b"\x00"),
            HeaderMap::new(),
        )
        .await;
    assert!(r.is_err_and(|e| e.status == 400 && e.message.starts_with("Unknown avrdude preset \"due\"")));
    let (s, v) = b.call(Method::POST, &format!("/api/serial/{id}/flash/cancel"), json!({})).await;
    assert_eq!((s, v["message"].as_str()), (409, Some("Nothing is flashing")));
}

#[tokio::test(flavor = "multi_thread")]
async fn autobaud_listens_and_restores() {
    let _one_at_a_time = PTYS.lock().await;
    let b = bob().await;
    let dev = pty();
    let id = b.manual(&dev).await;
    let (s, _) = b.call(Method::POST, &format!("/api/serial/{id}/open"), json!({})).await;
    assert_eq!(s, 200);
    let mut m = dev.master;
    tokio::spawn(async move {
        for _ in 0..100 {
            if m.write_all(
                b"I (29) boot: hello from the device
",
            )
            .await
            .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_, p0) = b.call(Method::GET, &format!("/api/serial/{id}"), json!({})).await;
    let v = b.tool("serial_autobaud", json!({ "id": id, "rates": [9600, 115200], "ms": 100 })).await.unwrap();
    let (_, p1) = b.call(Method::GET, &format!("/api/serial/{id}"), json!({})).await;
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["best", "results", "msPerRate", "applied", "config"]);
    assert_eq!(v["msPerRate"], 100);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r["bytes"].as_u64().unwrap() > 0), "{v}");
    // A pty ignores the rate, so both look the same: no clear winner, nothing applied.
    assert_eq!(v["applied"], false);
    assert_eq!(v["config"]["baud"], 115200);
    assert_eq!((p1["state"].as_str(), p1["autobaud"]["applied"].as_bool()), (Some("open"), Some(false)));
    // What arrived during the sweep went to the sampler, not the ring.
    let grew = p1["rxBytes"].as_u64().unwrap() - p0["rxBytes"].as_u64().unwrap();
    assert!(grew <= 72, "{grew}");
}

#[tokio::test(flavor = "multi_thread")]
async fn status_and_settings() {
    let _one_at_a_time = PTYS.lock().await;
    let b = bob().await;
    let dev = pty();
    let id = b.manual(&dev).await;
    let st = b.drv.status().unwrap();
    assert_eq!(st["summary"], "1 port");
    assert_eq!(st["active"], true);
    let (_, v) = b.call(Method::POST, &format!("/api/serial/{id}/config"), json!({ "baud": 74880, "parity": "e" })).await;
    assert_eq!(v["config"], json!({ "baud": 74880, "dataBits": 8, "parity": "even", "stopBits": 1, "flow": "none" }));
    let (s, v) = b.call(Method::POST, &format!("/api/serial/{id}/config"), json!({ "baud": "fast" })).await;
    assert_eq!((s, v["message"].as_str()), (400, Some("Invalid baud rate fast")));
    let (_, _) = b.call(Method::POST, &format!("/api/serial/{id}/open"), json!({})).await;
    // Open reads back what the tty accepted (as the TypeScript does): a pty drops parity.
    assert_eq!(b.drv.status().unwrap()["summary"], format!("{} open · 74880 8N1", dev.path));
    let (s, v) = b.call(Method::GET, "/api/serial/nope", json!({})).await;
    assert_eq!((s, v["message"].as_str()), (404, Some("No serial port \"nope\". GET /api/serial lists them.")));
    let (s, v) = b.call(Method::POST, "/api/serial/manual", json!({ "path": "/tmp/x" })).await;
    assert_eq!((s, v["message"].as_str()), (400, Some("path must be a /dev/ node")));
    let (s, _) = b.call(Method::POST, "/api/serial/manual", json!({ "path": dev.path, "remove": true })).await;
    assert_eq!(s, 200);
    assert_eq!(b.drv.status().unwrap()["summary"], "No USB serial ports");
    drop(dev);
}
