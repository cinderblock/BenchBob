//! The Bun server behind us, on `127.0.0.1:$BENCHBOB_LEGACY_PORT`, until every panel is
//! ported. Any route this server doesn't implement is reverse-proxied to it (HTTP and
//! WebSocket upgrades, both streamed), and the documents that span every panel (status,
//! API docs, MCP tools, the event timeline) merge its part in.
//!
//! Bun runs with `BENCHBOB_NATIVE_DRIVERS=<ids>`, so it leaves the drivers this server
//! owns alone (no double GPIO watchers, no double settings writes).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Uri, header};
use axum::response::Response;
use benchbob_core::http::json_response;
use benchbob_core::registry::ListItem;
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::{Map, Value, json};

/// Where each Bun driver sorts on the dashboard (their `order`), so a merged list keeps
/// the order the Bun server would give it. Unknown ids sort at 100, as in core.ts.
const LEGACY_ORDER: &[(&str, i32)] = &[
    ("usb", 10),
    ("serial", 20),
    ("can", 30),
    ("i2c", 40),
    ("wifi", 40),
    ("onewire", 41),
    ("gpio", 42),
    ("ble", 60),
    ("camera", 60),
    ("probes", 60),
    ("sdr", 62),
    ("gps", 64),
    ("ups", 70),
    ("netdiag", 80),
];

/// Status fields only the Bun server has (drives, flashing, rpiboot), with what to send
/// when it can't be reached.
pub fn legacy_status_fields() -> Vec<(&'static str, Value)> {
    vec![
        ("rootDevice", Value::Null),
        ("drives", json!([])),
        ("nbdExports", json!([])),
        ("bootDevices", json!([])),
        ("rpibootInstalled", Value::Bool(false)),
        ("rpibootResults", json!([])),
        (
            "filesRoot",
            Value::String(
                std::env::var("FILES_ROOT")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "/srv/nfs/drives".into())
                    .trim_end_matches('/')
                    .to_string(),
            ),
        ),
        ("operations", json!([])),
    ]
}

/// Headers that belong to one hop, not the request (RFC 9110 §7.6.1).
const HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn strip_hop(h: &mut HeaderMap) {
    // Plus whatever the Connection header names.
    let named: Vec<String> = h
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|s| s.trim().to_lowercase())
        .collect();
    for n in HOP.iter().map(|s| s.to_string()).chain(named) {
        if let Ok(name) = HeaderName::from_bytes(n.as_bytes()) {
            h.remove(name);
        }
    }
}

pub struct Legacy {
    pub port: u16,
    client: Client<HttpConnector, Body>,
    status: Mutex<Option<(Instant, Value)>>,
    periph: Mutex<Option<Value>>,
    docs: Mutex<Option<(Instant, Value)>>,
    /// The Bun server's buildVersion, folded into ours.
    pub build: Mutex<String>,
}

impl Legacy {
    pub fn new(port: u16) -> Arc<Self> {
        let mut conn = HttpConnector::new();
        conn.set_nodelay(true);
        let client = Client::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build(conn);
        Arc::new(Self {
            port,
            client,
            status: Mutex::new(None),
            periph: Mutex::new(None),
            docs: Mutex::new(None),
            build: Mutex::new(String::new()),
        })
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn uri(&self, path_and_query: &str) -> Uri {
        format!("{}{path_and_query}", self.base()).parse().unwrap_or_else(|_| Uri::from_static("http://127.0.0.1/"))
    }

    /// A JSON request of our own (not a proxied one), with a timeout.
    pub async fn request_json(&self, method: Method, path: &str, body: Option<&Value>, timeout: Duration) -> Option<(u16, Value)> {
        let mut req = Request::builder()
            .method(method)
            .uri(self.uri(path))
            // Bun rewrites example URLs to the host it was asked by; this one is the
            // placeholder our own docs rewrite.
            .header(header::HOST, "benchbob.local")
            .header(header::ACCEPT, "application/json");
        if body.is_some() {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        let req = req.body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty)).ok()?;
        let fut = async {
            let r = self.client.request(req).await.ok()?;
            let status = r.status().as_u16();
            let b = r.into_body().collect().await.ok()?.to_bytes();
            Some((status, serde_json::from_slice(&b).unwrap_or(Value::Null)))
        };
        tokio::time::timeout(timeout, fut).await.ok().flatten()
    }

    fn remember(&self, s: &Value) {
        if let Some(b) = s.get("buildVersion").and_then(Value::as_str) {
            *self.build.lock().unwrap() = b.to_string();
        }
        if let Some(p) = s.get("peripherals") {
            *self.periph.lock().unwrap() = Some(p.clone());
        }
        *self.status.lock().unwrap() = Some((Instant::now(), s.clone()));
    }

    /// Its last status, fetched if older than `max_age`; the stale one if it's down.
    pub async fn status(&self, max_age: Duration) -> Option<Value> {
        if let Some((at, s)) = self.status.lock().unwrap().as_ref()
            && at.elapsed() < max_age
        {
            return Some(s.clone());
        }
        match self.request_json(Method::GET, "/api/status", None, Duration::from_secs(10)).await {
            Some((200, s)) if s.is_object() => {
                self.remember(&s);
                Some(s)
            }
            _ => self.status.lock().unwrap().as_ref().map(|(_, s)| s.clone()),
        }
    }

    pub fn periph(&self) -> Option<Value> {
        self.periph.lock().unwrap().clone()
    }

    /// Its `/api` document (cached 10 s).
    pub async fn docs(&self) -> Option<Value> {
        if let Some((at, d)) = self.docs.lock().unwrap().as_ref()
            && at.elapsed() < Duration::from_secs(10)
        {
            return Some(d.clone());
        }
        let (code, d) = self.request_json(Method::GET, "/api", None, Duration::from_secs(10)).await?;
        if code != 200 || !d.is_object() {
            return None;
        }
        *self.docs.lock().unwrap() = Some((Instant::now(), d.clone()));
        Some(d)
    }

    /// Forward one request and stream the answer back.
    pub async fn proxy(&self, req: Request<Body>, client: Option<std::net::SocketAddr>) -> Response {
        let (mut parts, body) = req.into_parts();
        let pq = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
        strip_hop(&mut parts.headers);
        add_forwarded(&mut parts.headers, client);
        let mut out = Request::new(body);
        *out.method_mut() = parts.method;
        *out.uri_mut() = self.uri(&pq);
        *out.headers_mut() = parts.headers;
        match self.client.request(out).await {
            Ok(r) => {
                let (mut p, b) = r.into_parts();
                strip_hop(&mut p.headers);
                Response::from_parts(p, Body::new(b))
            }
            Err(e) => unreachable_response(self.port, &e.to_string()),
        }
    }

    /// Forward a WebSocket upgrade: send the handshake to Bun on a fresh connection and,
    /// once it answers 101, splice the two upgraded connections together.
    pub async fn proxy_ws(&self, mut req: Request<Body>, client: Option<std::net::SocketAddr>) -> Response {
        let on_client_upgrade = hyper::upgrade::on(&mut req);
        let pq = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
        let tcp = match tokio::net::TcpStream::connect(("127.0.0.1", self.port)).await {
            Ok(t) => t,
            Err(e) => return unreachable_response(self.port, &e.to_string()),
        };
        let _ = tcp.set_nodelay(true);
        let (mut sender, conn) = match hyper::client::conn::http1::handshake::<_, Body>(TokioIo::new(tcp)).await {
            Ok(x) => x,
            Err(e) => return unreachable_response(self.port, &e.to_string()),
        };
        tokio::spawn(async move {
            let _ = conn.with_upgrades().await;
        });
        let mut headers = req.headers().clone();
        add_forwarded(&mut headers, client);
        let mut out = Request::new(Body::empty());
        *out.method_mut() = req.method().clone();
        *out.uri_mut() = pq.parse().unwrap_or_else(|_| Uri::from_static("/"));
        *out.headers_mut() = headers;
        let mut resp = match sender.send_request(out).await {
            Ok(r) => r,
            Err(e) => return unreachable_response(self.port, &e.to_string()),
        };
        if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
            let (p, b) = resp.into_parts();
            return Response::from_parts(p, Body::new(b));
        }
        let on_server_upgrade = hyper::upgrade::on(&mut resp);
        tokio::spawn(async move {
            let (Ok(c), Ok(s)) = (on_client_upgrade.await, on_server_upgrade.await) else { return };
            let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(c), &mut TokioIo::new(s)).await;
        });
        let (p, _) = resp.into_parts();
        Response::from_parts(p, Body::empty())
    }

    /// Follow Bun's /api/events while `wanted()` (we have SSE clients): every full status
    /// is cached, every `periph` event cached and handed to `on_periph`.
    pub fn follow_events(self: &Arc<Self>, wanted: Arc<dyn Fn() -> bool + Send + Sync>, on_periph: Arc<dyn Fn() + Send + Sync>) {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                while !wanted() {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                let req = Request::builder().uri(me.uri("/api/events")).header(header::HOST, "benchbob.local").body(Body::empty()).unwrap();
                if let Ok(r) = me.client.request(req).await {
                    let mut body = r.into_body();
                    let mut buf: Vec<u8> = Vec::new();
                    loop {
                        if !wanted() {
                            break;
                        }
                        let frame = match tokio::time::timeout(Duration::from_secs(10), body.frame()).await {
                            Ok(Some(Ok(f))) => f,
                            _ => break,
                        };
                        let Ok(data) = frame.into_data() else { continue };
                        buf.extend_from_slice(&data);
                        while let Some(i) = find(&buf, b"\n\n") {
                            let block: Vec<u8> = buf.drain(..i + 2).collect();
                            let (event, data) = parse_event(&String::from_utf8_lossy(&block));
                            let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
                            match event.as_deref() {
                                None => me.remember(&v),
                                Some("periph") => {
                                    *me.periph.lock().unwrap() = Some(v);
                                    on_periph();
                                }
                                _ => {}
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

fn parse_event(block: &str) -> (Option<String>, String) {
    let mut event = None;
    let mut data = Vec::new();
    for l in block.lines() {
        if let Some(e) = l.strip_prefix("event:") {
            event = Some(e.trim().to_string());
        } else if let Some(d) = l.strip_prefix("data:") {
            data.push(d.strip_prefix(' ').unwrap_or(d).to_string());
        }
    }
    (event, data.join("\n"))
}

fn add_forwarded(h: &mut HeaderMap, client: Option<std::net::SocketAddr>) {
    if let Some(a) = client
        && let Ok(v) = HeaderValue::from_str(&a.ip().to_string())
    {
        h.insert("X-Forwarded-For", v);
    }
    h.insert("X-Forwarded-Proto", HeaderValue::from_static("http"));
}

fn unreachable_response(port: u16, why: &str) -> Response {
    json_response(502, &json!({ "ok": false, "message": format!("The legacy BenchBob server (127.0.0.1:{port}) isn't answering: {why}") }))
}

/// `{ list, state }`: our drivers' rows and the Bun server's, in dashboard order.
pub fn merge_periph(native: Vec<ListItem>, legacy: Option<&Value>) -> Value {
    let ours: Vec<String> = native.iter().map(|i| i.id.clone()).collect();
    let mut items = native;
    if let Some(l) = legacy {
        let state = l.get("state").and_then(Value::as_object).cloned().unwrap_or_default();
        for e in l.get("list").and_then(Value::as_array).into_iter().flatten() {
            let Some(id) = e.get("id").and_then(Value::as_str) else { continue };
            if ours.iter().any(|o| o == id) {
                continue;
            }
            let order = LEGACY_ORDER.iter().find(|(k, _)| *k == id).map(|x| x.1).unwrap_or(100);
            items.push(ListItem { id: id.into(), order, entry: e.clone(), state: state.get(id).cloned() });
        }
    }
    // core.ts loads drivers in file-name order and sorts stably by order.
    items.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.id.cmp(&b.id)));
    benchbob_core::registry::assemble(items)
}

/// Bun's timeline and ours, oldest first.
pub fn merge_timeline(native: Vec<Value>, legacy: Option<Value>) -> Value {
    let mut all = native;
    if let Some(Value::Array(l)) = legacy {
        all.extend(l);
    }
    all.sort_by_key(|e| e.get("t").and_then(Value::as_u64).unwrap_or(0));
    Value::Array(all)
}

/// Overlay `ours` onto `theirs` key by key: our values replace theirs in place, our new
/// keys go at the end.
pub fn overlay(theirs: &Map<String, Value>, ours: &Map<String, Value>) -> Map<String, Value> {
    let mut out = theirs.clone();
    for (k, v) in ours {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// Collect a small request body (route handlers, MCP).
pub async fn read_body(body: Body, limit: usize) -> Result<Bytes, String> {
    let b = axum::body::to_bytes(body, limit).await.map_err(|e| e.to_string())?;
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periph_lists_merge_in_dashboard_order() {
        let gpio = ListItem { id: "gpio".into(), order: 42, entry: json!({ "id": "gpio" }), state: Some(json!({ "mode": "rpi" })) };
        let legacy = json!({
            "list": [{ "id": "usb" }, { "id": "serial" }, { "id": "i2c" }, { "id": "wifi" }, { "id": "onewire" }, { "id": "gpio" }, { "id": "ble" }],
            "state": { "usb": { "a": 1 }, "gpio": { "stale": true } },
        });
        let m = merge_periph(vec![gpio], Some(&legacy));
        let ids: Vec<&str> = m["list"].as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["usb", "serial", "i2c", "wifi", "onewire", "gpio", "ble"]);
        assert_eq!(m["state"]["gpio"], json!({ "mode": "rpi" }));
        assert_eq!(m["state"]["usb"], json!({ "a": 1 }));
        let keys: Vec<&String> = m["state"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["usb", "gpio"]);
    }

    #[test]
    fn sse_blocks() {
        assert_eq!(parse_event("event: periph\ndata: {\"a\":1}\n\n"), (Some("periph".into()), "{\"a\":1}".into()));
        assert_eq!(parse_event("data: {}\n\n"), (None, "{}".into()));
    }

    #[test]
    fn hop_headers_are_dropped() {
        let mut h = HeaderMap::new();
        h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive, x-foo"));
        h.insert("x-foo", HeaderValue::from_static("1"));
        h.insert(header::TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        strip_hop(&mut h);
        assert_eq!(h.len(), 1);
    }
}
