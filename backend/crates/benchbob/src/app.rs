//! The request handler: routing by hand, as server.ts and core.ts do, because driver routes
//! are dynamic ("most literal wins") and everything unmatched goes to the Bun server.
//!
//! Order: CORS preflight, /mcp, the registry's routes (/api/periph/*, /ws/*, driver
//! routes), the page and web/dist, /api docs, /api/update*, /api/status, /api/events,
//! /api/boards; anything else is proxied (or 404 without a legacy server).
//!
//! Every response carries permissive CORS (`Access-Control-Allow-Origin: *`, no
//! credentials) so a desktop app on another origin can use any bob, and
//! `X-BenchBob-Backend: native|legacy` saying who answered.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use benchbob_boards::{Board, Boards};
use benchbob_core::driver::{WsIn, WsOut};
use benchbob_core::http::{HttpError, Reply, json_response, parse_query, text_response, truthy};
use benchbob_core::mcp::{self, ToolSource};
use benchbob_core::{Registry, WsClient, WsSession};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::health::Health;
use crate::legacy::{Legacy, merge_periph, merge_timeline};
use crate::sse::Sse;
use crate::status::Sampler;
use crate::ui::{Ui, accepts_gzip, gzip};
use crate::update::Update;

/// Request bodies a native route reads in full: up to 64 MB, the debug probes' firmware
/// upload limit (Bun's own default was 128 MB).
const BODY_LIMIT: usize = 64 << 20;

pub struct App {
    pub registry: Registry,
    pub board: Arc<Board>,
    pub boards: Arc<Boards>,
    pub legacy: Option<Arc<Legacy>>,
    pub sampler: tokio::sync::Mutex<Sampler>,
    pub health: Health,
    pub sse: Sse,
    pub update: Arc<Update>,
    pub ui: Ui,
}

enum Who {
    Native,
    Legacy,
}

impl App {
    fn legacy_build(&self) -> String {
        self.legacy.as_ref().map(|l| l.build.lock().unwrap().clone()).unwrap_or_default()
    }

    /// The drives service (block devices, flashing, rpiboot), for its status fields.
    pub fn drives(&self) -> Option<&benchbob_drives::Drives> {
        let l = self.registry.get("drives")?;
        l.driver.as_any()?.downcast_ref()
    }

    pub fn build_version(&self) -> String {
        self.ui.build(&self.legacy_build()).version
    }

    /// `peripherals` in the status: our drivers and the Bun server's.
    pub fn periph(&self) -> Value {
        let items = self.registry.status_items();
        match &self.legacy {
            Some(l) => merge_periph(items, l.periph().as_ref()),
            None => benchbob_core::registry::assemble(items),
        }
    }

    /// The full status (server.ts getStatus), keys in the same order.
    pub async fn status(&self) -> Value {
        // Refreshes the Bun server's peripherals (and build version), merged in below.
        if let Some(l) = &self.legacy {
            let _ = l.status(Duration::from_millis(1500)).await;
        }
        let temp = self.health.cpu_temp().await;
        let power = self.health.power().await;
        let (sensors, _, _) = self.health.sensors().await;
        let mut s = self.sampler.lock().await;
        s.sample_io();
        let system = s.system(&self.board.name, temp);
        let network = s.network().await;
        let root_storage = s.root_storage().await;
        s.push_hist("mem", system["memPercent"].clone());
        if let Some(w) = network.as_array().and_then(|a| a.iter().find(|n| n.get("wifi").is_some_and(truthy))) {
            let sig = w["wifi"]["signal"].clone();
            s.push_hist("wifi", sig);
        }
        let build = self.build_version();
        let mut m = Map::new();
        m.insert("hostname".into(), crate::status::hostname().into());
        m.insert("uptime".into(), crate::status::uptime());
        m.insert("timestamp".into(), benchbob_core::timeline::now_ms().into());
        m.insert("version".into(), benchbob_core::paths::version().into());
        m.insert("update".into(), self.update.summary());
        m.insert("buildVersion".into(), build.clone().into());
        // Alias kept so dashboards still open from an old deploy (which compare
        // `uiVersion`) see the change and reload themselves once.
        m.insert("uiVersion".into(), build.into());
        m.insert("board".into(), self.board.info());
        m.insert("system".into(), system);
        m.insert("power".into(), power);
        m.insert("sensors".into(), sensors.to_json());
        m.insert("network".into(), network);
        m.insert("rootStorage".into(), root_storage);
        if let Some(d) = self.drives() {
            let root = s.root_disk_name();
            for (k, v) in d.status_fields(&s.disk_rates, root.as_deref()).await {
                m.insert(k.into(), v);
            }
        }
        m.insert("peripherals".into(), self.periph());
        m.insert("history".into(), s.history());
        Value::Object(m)
    }

    /// Which backend answers what, for `GET /api` and the CLI.
    pub fn backend_info(&self) -> Value {
        let mut native: Vec<String> = [
            "GET /",
            "GET /api",
            "GET /api/status",
            "GET /api/events",
            "GET /api/boards",
            "* /api/update*",
            "POST /mcp",
            "POST /api/periph/driver",
            "GET /api/periph/timeline",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if self.drives().is_some() {
            native.extend(benchbob_drives::docs().keys().cloned());
        }
        for k in self.registry.docs().keys() {
            native.push(k.clone());
        }
        json!({
            "server": "benchbob (Rust)",
            "version": env!("CARGO_PKG_VERSION"),
            "nativeDrivers": self.registry.ids(),
            "native": native,
            "legacy": self.legacy.as_ref().map(|l| l.base()),
            "note": if self.legacy.is_some() {
                "Routes not listed under native are proxied to the Bun server (X-BenchBob-Backend: legacy)"
            } else {
                "No legacy server configured (BENCHBOB_LEGACY_PORT): only native routes exist"
            },
        })
    }

    pub async fn handle(self: Arc<Self>, req: Request<Body>, peer: Option<SocketAddr>) -> Response {
        let preflight = req.method() == Method::OPTIONS && req.headers().contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
        if preflight {
            return cors(preflight_response(req.headers()));
        }
        let accept_gzip = accepts_gzip(req.headers());
        let (who, res) = self.route(req, peer).await;
        let mut res = match who {
            Who::Native => compress(accept_gzip, res).await,
            Who::Legacy => res,
        };
        res.headers_mut()
            .insert("X-BenchBob-Backend", HeaderValue::from_static(if matches!(who, Who::Native) { "native" } else { "legacy" }));
        cors(res)
    }

    async fn route(self: &Arc<Self>, req: Request<Body>, peer: Option<SocketAddr>) -> (Who, Response) {
        let path = req.uri().path().to_string();
        let method = req.method().clone();
        let query = parse_query(req.uri().query().unwrap_or(""));

        if path == "/mcp" {
            let body = match crate::legacy::read_body(req.into_body(), BODY_LIMIT).await {
                Ok(b) => b,
                Err(e) => return (Who::Native, HttpError::bad_request(e).into_response()),
            };
            return (Who::Native, mcp::handle(&method, &body, &Tools { app: self.clone() }).await);
        }

        if path == "/api/periph/driver" && method == Method::POST {
            let body = crate::legacy::read_body(req.into_body(), BODY_LIMIT).await.unwrap_or_default();
            let a: Value = if body.is_empty() { json!({}) } else { serde_json::from_slice(&body).unwrap_or(Value::Null) };
            if a.is_null() {
                return (Who::Native, HttpError::bad_request("Body is not valid JSON").into_response());
            }
            let id = a.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            if self.registry.get(&id).is_none()
                && let Some(l) = &self.legacy
            {
                let r = l.request_json(Method::POST, "/api/periph/driver", Some(&a), Duration::from_secs(30)).await;
                return match r {
                    Some((code, v)) => (Who::Legacy, json_response(code, &v)),
                    None => {
                        (Who::Legacy, json_response(502, &json!({ "ok": false, "message": "The legacy BenchBob server isn't answering" })))
                    }
                };
            }
            let disabled = a.get("disabled").is_some_and(truthy);
            return match self.registry.set_disabled(&id, disabled).await {
                Ok(()) => (Who::Native, json_response(200, &json!({ "ok": true }))),
                Err(e) => (Who::Native, e.into_response()),
            };
        }

        if path == "/api/periph/timeline" {
            let ours: Vec<Value> = self.registry.hub().timeline.list().into_iter().map(|e| serde_json::to_value(e).unwrap()).collect();
            let theirs = match &self.legacy {
                Some(l) => l.request_json(Method::GET, "/api/periph/timeline", None, Duration::from_secs(5)).await.map(|x| x.1),
                None => None,
            };
            return (Who::Native, json_response(200, &merge_timeline(ours, theirs)));
        }

        if path.starts_with("/ws/") {
            if let Some((handler, params)) = self.registry.find_socket(&path) {
                let (mut parts, _) = req.into_parts();
                return match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
                    Ok(ws) => (Who::Native, ws.on_upgrade(move |socket| bridge(socket, handler, params, query)).into_response()),
                    Err(e) => (Who::Native, text_response(400, format!("WebSocket upgrade failed: {e}"))),
                };
            }
            if self.registry.owns_socket(&path) {
                return (Who::Native, text_response(404, "Not Found"));
            }
            return match &self.legacy {
                Some(l) => (Who::Legacy, l.proxy_ws(req, peer).await),
                None => (Who::Native, text_response(404, "Not Found")),
            };
        }

        if let Some(m) = self.registry.find_route(&method, &path) {
            if !m.loaded.started() {
                return (
                    Who::Native,
                    json_response(409, &json!({ "ok": false, "message": format!("{} is disabled", m.loaded.driver.title()) })),
                );
            }
            let (parts, body) = req.into_parts();
            let body = match crate::legacy::read_body(body, BODY_LIMIT).await {
                Ok(b) => b,
                Err(e) if e.contains("length limit") => {
                    return (
                        Who::Native,
                        HttpError::new(413, format!("Request body too large (max {} MB)", BODY_LIMIT >> 20)).into_response(),
                    );
                }
                Err(e) => return (Who::Native, HttpError::bad_request(e).into_response()),
            };
            let r = benchbob_core::Request { method, path, params: m.params.clone(), query, headers: parts.headers, body };
            let res = match (m.route().handler)(r).await {
                Ok(Reply::Json(v)) => json_response(200, &v),
                Ok(Reply::Response(r)) => r,
                Err(e) => e.into_response(),
            };
            return (Who::Native, res);
        }

        let web = self.ui.web_dir().is_some();
        if (path == "/" || path == "/index.html") && (method == Method::GET || method == Method::HEAD) {
            let b = self.ui.build(&self.legacy_build());
            let page = if web { b.web_html.as_ref().map(|h| (h, "web")) } else { b.legacy_html.as_ref().map(|h| (h, "legacy")) };
            if let Some((html, which)) = page {
                return (Who::Native, self.ui.page(html, &b.version, which, req.headers()));
            }
        }
        if (path == "/legacy" || path == "/legacy/") && method == Method::GET {
            let b = self.ui.build(&self.legacy_build());
            if let Some(html) = &b.legacy_html {
                return (Who::Native, self.ui.page(html, &b.version, "legacy", req.headers()));
            }
        }
        if web
            && !path.starts_with("/api")
            && let Some(r) = self.ui.static_file(&method, &path, req.headers())
        {
            return (Who::Native, r);
        }

        if path == "/api" || path == "/api/" {
            let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost").to_string();
            let theirs = match &self.legacy {
                Some(l) => l.docs().await,
                None => None,
            };
            let mut native = crate::docs::own_endpoints();
            if self.drives().is_some() {
                native.extend(benchbob_drives::docs());
            }
            for (k, v) in crate::update::docs().as_object().unwrap() {
                native.insert(k.clone(), v.clone());
            }
            native.extend(crate::docs::periph_endpoints());
            native.extend(self.registry.docs());
            let text = crate::docs::assemble(theirs, native, self.board.info(), self.backend_info(), &format!("http://{host}"));
            let mut r = Response::new(Body::from(text));
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            return (Who::Native, r);
        }

        if path.starts_with("/api/update") {
            let (parts, body) = req.into_parts();
            let body = crate::legacy::read_body(body, BODY_LIMIT).await.unwrap_or_default();
            if let Some(r) = self.update.route(&parts.method, &path, &body).await {
                return (Who::Native, r);
            }
            return self.fallback(Request::from_parts(parts, Body::from(body)), peer).await;
        }

        if path == "/api/status" {
            return (Who::Native, json_response(200, &self.status().await));
        }

        if path == "/api/events" {
            let (id, r) = self.sse.subscribe(accepts_gzip(req.headers()));
            let me = self.clone();
            tokio::spawn(async move {
                let s = me.status().await;
                me.sse.send_to(id, &s, None);
            });
            return (Who::Native, r);
        }

        if path == "/api/boards" && method == Method::GET {
            return (Who::Native, json_response(200, &self.boards_doc()));
        }

        self.fallback(req, peer).await
    }

    async fn fallback(&self, req: Request<Body>, peer: Option<SocketAddr>) -> (Who, Response) {
        match &self.legacy {
            Some(l) => (Who::Legacy, l.proxy(req, peer).await),
            None => (Who::Native, text_response(404, "Not Found")),
        }
    }

    fn boards_doc(&self) -> Value {
        let b = &self.boards;
        json!({
            "current": self.board.id,
            "forced": self.board.forced,
            "dirs": b.dirs,
            "boards": b.defs.iter().zip(&b.origin).map(|(d, f)| json!({
                "id": d.id, "name": d.name, "vendor": d.vendor, "soc": d.soc, "tested": d.tested, "file": f,
                "priority": d.detect.priority,
                "headers": d.headers.iter().map(|h| json!({ "id": h.id, "name": h.name, "pins": h.pins.len() })).collect::<Vec<_>>(),
                "gpio": d.gpio.panel.as_str(),
                "health": d.health.as_str(),
                "bootConfig": d.boot.as_ref().map(|x| x.files.iter().map(|f| json!({ "kind": f.kind.as_str(), "path": f.path })).collect::<Vec<_>>()),
            })).collect::<Vec<_>>(),
            "errors": b.errors.iter().map(|e| json!({ "file": e.file, "error": e.error })).collect::<Vec<_>>(),
        })
    }
}

/// Bridge an upgraded WebSocket to a driver's socket handler.
async fn bridge(
    socket: WebSocket,
    handler: benchbob_core::driver::SocketHandler,
    params: std::collections::HashMap<String, String>,
    query: Vec<(String, String)>,
) {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsOut>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<WsIn>();
    let client = WsClient::new(out_tx);
    let session = WsSession { client: client.clone(), params, query, incoming: in_rx };
    let task = tokio::spawn(handler(session));
    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            out = out_rx.recv() => {
                let n = out.as_ref().map_or(0, WsOut::len);
                let r = match out {
                    Some(WsOut::Text(t)) => sink.send(Message::Text(t.into())).await,
                    Some(WsOut::Binary(b)) => sink.send(Message::Binary(b)).await,
                    Some(WsOut::CloseWith(code, reason)) => {
                        let _ = sink.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await;
                        break;
                    }
                    Some(WsOut::Close) | None => {
                        let _ = sink.send(Message::Close(None)).await;
                        break;
                    }
                };
                client.sent(n);
                if r.is_err() { break }
            },
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(t))) => { let _ = in_tx.send(WsIn::Text(t.to_string())); }
                Some(Ok(Message::Binary(b))) => { let _ = in_tx.send(WsIn::Binary(b)); }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => break,
            },
        }
    }
    // The handler sees its incoming stream end and cleans up.
    drop(in_tx);
    let _ = task.await;
}

struct Tools {
    app: Arc<App>,
}

#[async_trait]
impl ToolSource for Tools {
    async fn list(&self) -> Vec<Value> {
        let native: Vec<(&str, Value)> = self.app.registry.tools().into_iter().map(|(d, t)| (d, mcp::describe(&t))).collect();
        let Some(l) = &self.app.legacy else { return native.into_iter().map(|x| x.1).collect() };
        let ours = self.app.registry.all_tool_names();
        let theirs: Vec<Value> = l
            .request_json(
                Method::POST,
                "/mcp",
                Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })),
                Duration::from_secs(10),
            )
            .await
            .and_then(|(_, v)| v.get("result")?.get("tools")?.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.get("name").and_then(Value::as_str).is_none_or(|n| !ours.iter().any(|o| o == n)))
            .collect();
        // Bun lists tools in driver file-name order; put ours where their driver would be.
        let mut out: Vec<(String, Value)> =
            theirs.into_iter().map(|t| (t["name"].as_str().unwrap_or("").split('_').next().unwrap_or("").to_string(), t)).collect();
        for (driver, t) in native {
            let at = out.iter().position(|(d, _)| d.as_str() > driver).unwrap_or(out.len());
            out.insert(at, (driver.to_string(), t));
        }
        out.into_iter().map(|x| x.1).collect()
    }

    async fn call(&self, name: &str, args: Value) -> Option<Value> {
        if let Some((_, t)) = self.app.registry.tools().into_iter().find(|(_, t)| t.name == name) {
            return Some(mcp::call_tool(&t, args).await);
        }
        let l = self.app.legacy.as_ref()?;
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } });
        // Tools may legitimately wait (a serial prompt, a CAN capture): allow 2 minutes.
        let (_, v) = l.request_json(Method::POST, "/mcp", Some(&msg), Duration::from_secs(120)).await?;
        if let Some(r) = v.get("result") {
            return Some(r.clone());
        }
        if v.pointer("/error/code").and_then(Value::as_i64) == Some(-32602) {
            return None;
        }
        let m = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("legacy server error").to_string();
        Some(json!({ "content": [{ "type": "text", "text": m }], "isError": true }))
    }
}

fn preflight_response(h: &HeaderMap) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::NO_CONTENT;
    let o = r.headers_mut();
    o.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"));
    let allow = h.get(header::ACCESS_CONTROL_REQUEST_HEADERS).cloned().unwrap_or(HeaderValue::from_static("*"));
    o.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, allow);
    o.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400"));
    // Chrome's Private Network Access: a page on a public origin calling a bob on the LAN.
    if h.get("Access-Control-Request-Private-Network").is_some_and(|v| v == "true") {
        o.insert("Access-Control-Allow-Private-Network", HeaderValue::from_static("true"));
    }
    o.insert(header::VARY, HeaderValue::from_static("Origin, Access-Control-Request-Method, Access-Control-Request-Headers"));
    r
}

/// Permissive CORS without credentials: any origin may read any response.
fn cors(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("*"));
    r
}

/// server.ts `compress()`: JSON answers over 1 KB when the client takes gzip. Streams are
/// left alone (SSE has its own; a body that streams on purpose says X-Accel-Buffering: no).
async fn compress(accept: bool, res: Response) -> Response {
    let h = res.headers();
    let json = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/json"));
    if res.status() != StatusCode::OK
        || h.contains_key(header::CONTENT_ENCODING)
        || h.get("x-accel-buffering").is_some_and(|v| v == "no")
        || !json
        || !accept
    {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    let Ok(raw) = axum::body::to_bytes(body, usize::MAX).await else {
        return text_response(500, "body error");
    };
    parts.headers.append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    if raw.len() < 1024 {
        return Response::from_parts(parts, Body::from(raw));
    }
    parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(gzip(&raw)))
}
