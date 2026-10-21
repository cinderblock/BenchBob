//! REST routes and MCP tools (serial.ts "Routes" and "MCP tools").

use std::future::Future;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderValue, Method, header};
use axum::response::Response;
use benchbob_core::http::{js_number, js_string, json, truthy};
use benchbob_core::timeline::now_ms;
use benchbob_core::{HttpError, Reply, Request, Route, RouteResult, Tool};
use serde_json::{Map, Value, json as j};

use crate::Inner;
use crate::bridge::BridgeKind;
use crate::flash::{AVR_PRESETS, ESP_CHIPS, FlashOpts};
use crate::flashjob::{FlashRequest, flash_request, job_view, json_sources};
use crate::ident::manual_id;
use crate::port::{Lock, exists, http};
use crate::util::iso_time;
use crate::ws::line_arg;

fn req_host(inner: &Inner, r: &Request) -> String {
    let h = r.headers.get(header::HOST).and_then(|h| h.to_str().ok()).map(String::from).unwrap_or_else(|| inner.hostname());
    // replace(/:\d+$/, "")
    match h.rfind(':') {
        Some(i) if i + 1 < h.len() && h[i + 1..].bytes().all(|c| c.is_ascii_digit()) => h[..i].to_string(),
        _ => h,
    }
}

/// The query string as an object (`Object.fromEntries(url.searchParams)`: the last value wins).
fn query_obj(r: &Request) -> Value {
    let mut m = Map::new();
    for (k, v) in &r.query {
        m.insert(k.clone(), Value::String(v.clone()));
    }
    Value::Object(m)
}

fn route<F, Fut>(inner: &Arc<Inner>, method: Method, path: &str, doc: Value, f: F) -> Route
where
    F: Fn(Arc<Inner>, Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = RouteResult> + Send + 'static,
{
    let inner = inner.clone();
    Route::new(method, path, Some(doc), move |r| f(inner.clone(), r))
}

const CONFIG_DOC: &str = "none|even|odd|mark|space";

fn config_doc() -> Map<String, Value> {
    let v = j!({ "baud": 115200, "dataBits": 8, "parity": CONFIG_DOC, "stopBits": 1, "flow": "none|rtscts|xonxoff" });
    v.as_object().unwrap().clone()
}

fn with(mut m: Map<String, Value>, k: &str, v: Value) -> Value {
    m.insert(k.into(), v);
    Value::Object(m)
}

pub(crate) fn routes(inner: &Arc<Inner>) -> Vec<Route> {
    vec![
        route(inner, Method::GET, "/api/serial", j!("List serial ports: USB adapters, the onboard UART and manually added paths, with state, config, modem lines, bridges and flash jobs"), |i, r| async move {
            let host = req_host(&i, &r);
            json(j!({ "ports": i.list_ports(Some(&host)) }))
        }),
        route(inner, Method::POST, "/api/serial/manual", j!({ "body": { "path": "/dev/pts/3", "remove": false }, "description": "Add (or remove) a tty that udev doesn't report as USB serial: a pty, socat link, gadget port. Persisted." }), |i, r| async move {
            let b = r.json()?;
            let path = b.get("path").filter(|v| truthy(v)).map(js_string).unwrap_or_default();
            let remove = b.get("remove").is_some_and(truthy);
            if !path.starts_with("/dev/") {
                return Err(HttpError::bad_request("path must be a /dev/ node"));
            }
            let mut list: Vec<Value> =
                i.ctx().setting("manual").and_then(|v| v.as_array().cloned()).unwrap_or_default().into_iter().filter(|x| x.as_str() != Some(&path)).collect();
            if !remove {
                if !exists(&path) {
                    return Err(http(404, format!("{path} doesn't exist")));
                }
                list.push(Value::String(path.clone()));
            }
            i.ctx().set_setting("manual", Value::Array(list));
            let id = manual_id(&path);
            if remove && let Some(p) = i.port(&id) {
                let h = {
                    let mut st = p.lock();
                    st.want = false;
                    let h = i.close_fd_locked(&p, &mut st, "closed");
                    for k in BridgeKind::ALL {
                        i.stop_bridge(&mut st, k);
                    }
                    h
                };
                i.ports.lock().unwrap().retain(|x| x.id != id);
                if let Some(h) = h {
                    let _ = h.await;
                }
            }
            i.rescan();
            json(j!({ "ok": true, "id": id }))
        }),
        route(inner, Method::GET, "/api/serial/:id", j!("One port's status"), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            let host = req_host(&i, &r);
            let st = p.lock();
            json(i.port_status(&p, &st, Some(&host)))
        }),
        route(inner, Method::POST, "/api/serial/:id/open", with(config_doc(), "(all optional)", j!(true)).pipe(|b| j!({ "body": b, "description": "Open the port (BenchBob holds it and fans data out to every client); an optional config is applied and saved as the default" })), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            let b = r.json()?;
            if b.as_object().is_some_and(|o| !o.is_empty()) {
                i.apply_config(&p, &b, true)?;
            }
            p.lock().want = true;
            i.ensure_open(&p)?;
            let host = req_host(&i, &r);
            let st = p.lock();
            json(i.port_status(&p, &st, Some(&host)))
        }),
        route(inner, Method::POST, "/api/serial/:id/close", j!("Close the port (bridges stay listening and reopen it when a client connects)"), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            let mut st = p.lock();
            st.want = false;
            st.leases = (st.ws.iter().filter(|w| w.lease).count() + st.bridge(BridgeKind::Tcp).clients.len() + st.bridge(BridgeKind::Rfc2217).clients.len()) as u32;
            if st.leases == 0 {
                i.close_fd_locked(&p, &mut st, "closed");
            }
            let host = req_host(&i, &r);
            json(i.port_status(&p, &st, Some(&host)))
        }),
        route(inner, Method::POST, "/api/serial/:id/config", with(config_doc(), "save", j!(true)).pipe(|b| j!({ "body": b, "description": "Change line settings (any baud rate). Applied live if open; save (default true) makes it the port's default" })), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            let b = r.json()?;
            let c = i.apply_config(&p, &b, b.get("save") != Some(&Value::Bool(false)))?;
            json(j!({ "ok": true, "config": c.to_json() }))
        }),
        route(inner, Method::POST, "/api/serial/:id/settings", j!({ "body": { "openOnPlug": true }, "description": "Per-port options: open automatically when plugged in (and at startup)" }), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            let b = r.json()?;
            if let Some(v) = b.get("openOnPlug") {
                let on = truthy(v);
                i.save_port_settings(&p.id, &[("openOnPlug", j!(on))]);
                if on && p.lock().present {
                    let _ = i.ensure_open(&p);
                }
            }
            let host = req_host(&i, &r);
            let st = p.lock();
            i.notify(&p, &st);
            json(i.port_status(&p, &st, Some(&host)))
        }),
        route(inner, Method::POST, "/api/serial/:id/lines", j!({ "body": { "dtr": true, "rts": false }, "description": "Assert (true) or drop (false) DTR/RTS" }), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            let b = r.json()?;
            i.set_lines(&p, line_arg(b.get("dtr")), line_arg(b.get("rts")))?;
            let lines = p.lock().lines.map(|l| l.to_json());
            json(j!({ "ok": true, "lines": lines }))
        }),
        route(inner, Method::POST, "/api/serial/:id/break", j!({ "body": { "ms": 250 }, "description": "Send a break (10-5000 ms)" }), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            let ms = js_number(r.json()?.get("ms").unwrap_or(&Value::Null));
            i.send_break(&p, if ms == 0.0 || ms.is_nan() { 250.0 } else { ms }).await?;
            json(j!({ "ok": true }))
        }),
        route(inner, Method::POST, "/api/serial/:id/reset", j!({ "body": { "mode": "esp | esp-boot | dtr | touch1200" }, "description": "Reset the target over DTR/RTS: esp (run), esp-boot (ROM bootloader; classic two-transistor circuit or ESP USB-JTAG sequence), dtr (Arduino auto-reset pulse), touch1200 (open at 1200 baud and close: native-USB Arduino / Pico bootloader)" }), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            let b = r.json()?;
            i.reset(&p, b.get("mode")).await?;
            json(j!({ "ok": true }))
        }),
        route(inner, Method::POST, "/api/serial/:id/write", j!({ "body": { "text": "help", "hex": "(or) de ad be ef", "base64": "(or)", "eol": "none|cr|lf|crlf", "wait": "regex, optional", "flags": "m", "timeout_ms": 3000, "raw": false, "token": "(lock token, if you hold the lock)" }, "description": "Write to the port (opens it if closed). With wait, returns the output that arrived after the write up to the first regex match or timeout_ms (max 60000). raw keeps ANSI escapes." }), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            json(i.write_and_wait(&p, &r.json()?).await?)
        }),
        route(inner, Method::GET, "/api/serial/:id/read", j!("Received data from the ring buffer (~256 KB). ?cursor=N (absolute byte offset; omit for the latest), max_bytes, wait_ms (long-poll up to 60000), format=text|hex|base64|raw (raw returns the bytes; add download=1 for a log file)"), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            let q = query_obj(&r);
            if q.get("format").and_then(Value::as_str) == Some("raw") {
                let st = p.lock();
                let cursor = q.get("cursor").and_then(Value::as_str).filter(|s| !s.is_empty()).map(|s| js_number(&Value::String(s.into()))).unwrap_or(st.ring.start() as f64);
                let max = q.get("max_bytes").map(js_number).filter(|n| *n != 0.0 && !n.is_nan()).unwrap_or(f64::INFINITY);
                let got = st.ring.since(cursor, max);
                let mut resp = Response::new(Body::from(got.data));
                let h = resp.headers_mut();
                h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
                h.insert("X-Serial-Cursor", HeaderValue::from(got.to));
                h.insert("X-Serial-From", HeaderValue::from(got.from));
                if q.get("download").is_some_and(truthy) {
                    let stamp = iso_time(now_ms()).replace([':', '.'], "-");
                    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"serial-{}-{stamp}.log\"", p.id)) {
                        h.insert(header::CONTENT_DISPOSITION, v);
                    }
                }
                return Ok(Reply::Response(resp));
            }
            json(i.read_since(&p, &q).await?)
        }),
        route(inner, Method::POST, "/api/serial/:id/autobaud", j!({ "body": { "rates": [9600, 115200], "ms": 400, "apply": false }, "description": "Guess the baud rate by listening only (never transmits): ~400 ms per rate, scored on printable/line-structured output. apply sets the best rate when confidence >= 0.5" }), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            json(i.autobaud(&p, &r.json()?).await?)
        }),
        route(inner, Method::POST, "/api/serial/:id/bridge", j!({ "body": { "kind": "tcp | rfc2217", "enabled": true, "port": "(optional, persisted)" }, "description": "Turn a network bridge on or off (persisted). Raw TCP: plain bytes. RFC 2217: Telnet COM-PORT-OPTION, so remote clients (pyserial rfc2217://, esptool) can change baud and lines. Default ports 4000+slot and 4100+slot, slot fixed per device" }), |i, r| async move {
            let b = r.json()?;
            let p = i.get_port(r.param("id"))?;
            let host = req_host(&i, &r);
            let kind = b.get("kind").and_then(Value::as_str).and_then(BridgeKind::parse);
            json(i.set_bridge(&p, kind, b.get("enabled").is_some_and(truthy), b.get("port"), &host)?)
        }),
        route(inner, Method::POST, "/api/serial/:id/lock", j!({ "body": { "owner": "my-script" }, "description": "Reserve writing for the caller; returns a token to pass to /write. Other writers get 423 until unlocked" }), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            let b = r.json()?;
            let mut st = p.lock();
            if let Some(l) = &st.lock {
                return Err(http(423, format!("Already locked by {}", l.owner)));
            }
            let owner = b.get("owner").filter(|v| truthy(v)).map(js_string).unwrap_or_else(|| "an API client".into());
            let token = uuid::Uuid::new_v4().to_string();
            st.lock = Some(Lock { owner, token: token.clone(), since: now_ms() });
            i.notify(&p, &st);
            json(j!({ "ok": true, "token": token }))
        }),
        route(inner, Method::POST, "/api/serial/:id/unlock", j!({ "body": { "token": "...", "force": false }, "description": "Release the write lock (force breaks someone else's)" }), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            let b = r.json()?;
            let mut st = p.lock();
            if let Some(l) = &st.lock
                && b.get("token").and_then(Value::as_str) != Some(l.token.as_str())
                && !b.get("force").is_some_and(truthy)
            {
                return Err(http(423, format!("Locked by {}; pass its token or force", l.owner)));
            }
            st.lock = None;
            i.notify(&p, &st);
            json(j!({ "ok": true }))
        }),
        route(inner, Method::POST, "/api/serial/:id/flash", j!({
            "query": { "tool": "esptool | avrdude", "action": "write | erase | info", "offset": "0x10000 (raw body)", "chip": "auto", "baud": 460800, "preset": "uno | nano | nano-old | mega" },
            "body": "Raw firmware bytes; or multipart with file fields plus offset fields in the same order (or file fields named by offset, e.g. 0x1000); or JSON {tool, action, files:[{offset, url | base64, name}]}",
            "description": "Flash a board through the port. BenchBob releases the port, runs the tool, streams progress (status + WebSocket 'flash' messages), then reopens. Returns at once with the job; poll GET .../flash",
        }), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            let ct = r.headers.get(header::CONTENT_TYPE).and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
            let req = flash_request(&ct, &r.query, &r.body)?;
            json(j!({ "ok": true, "job": i.start_flash(&p, req)? }))
        }),
        route(inner, Method::GET, "/api/serial/:id/flash", j!("Current or last flash job, with the full tool log"), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            let st = p.lock();
            json(j!({ "job": job_view(st.flash.as_ref()), "log": st.flash.as_ref().map(|f| f.log.clone()).unwrap_or_default() }))
        }),
        route(inner, Method::POST, "/api/serial/:id/flash/cancel", j!("Stop a running flash job"), |i, r| async move {
            let p = i.get_port(r.param("id"))?;
            i.cancel_flash(&p)?;
            json(j!({ "ok": true }))
        }),
    ]
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl Pipe for Value {}

fn tool<F, Fut>(inner: &Arc<Inner>, name: &str, desc: &str, schema: Value, f: F) -> Tool
where
    F: Fn(Arc<Inner>, Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, HttpError>> + Send + 'static,
{
    let inner = inner.clone();
    Tool::new(name, desc, schema, move |a| f(inner.clone(), a))
}

fn id_arg(a: &Value) -> String {
    a.get("id").map(js_string).unwrap_or_else(|| "undefined".into())
}

pub(crate) fn tools(inner: &Arc<Inner>) -> Vec<Tool> {
    let id_prop = || j!({ "type": "string", "description": "Port id from serial_list" });
    vec![
        tool(
            inner,
            "serial_list",
            "List serial ports on BenchBob (USB-serial adapters, dev boards, the onboard UART) with id, device path, chip/board name, open state, line settings, modem lines, bridge ports and any flash job. Call this first to get port ids.",
            j!({ "type": "object", "properties": {} }),
            |i, _| async move { Ok(j!({ "ports": i.list_ports(None) })) },
        ),
        tool(
            inner,
            "serial_configure",
            "Open, close or reconfigure a serial port: baud (any rate), data bits, parity, stop bits, flow control; set DTR/RTS; send a break; or reset the target (esp, esp-boot, dtr, touch1200). Settings are saved as the port's default unless save=false.",
            j!({
                "type": "object",
                "properties": {
                    "id": id_prop(),
                    "open": { "type": "boolean", "description": "true opens (and keeps open), false closes" },
                    "baud": { "type": "integer" },
                    "dataBits": { "type": "integer", "enum": [5, 6, 7, 8] },
                    "parity": { "type": "string", "enum": ["none", "even", "odd", "mark", "space"] },
                    "stopBits": { "type": "integer", "enum": [1, 2] },
                    "flow": { "type": "string", "enum": ["none", "rtscts", "xonxoff"] },
                    "save": { "type": "boolean", "default": true },
                    "dtr": { "type": "boolean" },
                    "rts": { "type": "boolean" },
                    "break_ms": { "type": "integer", "description": "Send a break of this length" },
                    "reset": { "type": "string", "enum": ["esp", "esp-boot", "dtr", "touch1200"] },
                    "open_on_plug": { "type": "boolean" },
                },
                "required": ["id"],
            }),
            |i, a| async move {
                let p = i.get_port(&id_arg(&a))?;
                let mut cfg = Map::new();
                for k in ["baud", "dataBits", "parity", "stopBits", "flow"] {
                    if let Some(v) = a.get(k) {
                        cfg.insert(k.into(), v.clone());
                    }
                }
                if !cfg.is_empty() {
                    i.apply_config(&p, &Value::Object(cfg), a.get("save") != Some(&Value::Bool(false)))?;
                }
                match a.get("open") {
                    Some(Value::Bool(true)) => {
                        p.lock().want = true;
                        i.ensure_open(&p)?;
                    }
                    Some(Value::Bool(false)) => {
                        let mut st = p.lock();
                        st.want = false;
                        i.close_fd_locked(&p, &mut st, "closed");
                    }
                    _ => {}
                }
                if let Some(v) = a.get("open_on_plug") {
                    i.save_port_settings(&p.id, &[("openOnPlug", j!(truthy(v)))]);
                }
                if a.get("dtr").is_some() || a.get("rts").is_some() {
                    if !p.lock().open() {
                        p.lock().want = true;
                        i.ensure_open(&p)?;
                    }
                    i.set_lines(&p, line_arg(a.get("dtr")), line_arg(a.get("rts")))?;
                }
                if let Some(ms) = a.get("break_ms").filter(|v| truthy(v)) {
                    i.send_break(&p, js_number(ms)).await?;
                }
                if let Some(m) = a.get("reset").filter(|v| truthy(v)) {
                    i.reset(&p, Some(m)).await?;
                }
                let st = p.lock();
                Ok(i.port_status(&p, &st, None))
            },
        ),
        tool(
            inner,
            "serial_write",
            "Write to a serial port and optionally wait for a reply. Opens the port if needed (and leaves it open). Give text (use eol to append a line ending) or hex. With wait (a JavaScript regex), returns the output received after the write up to the first match, or whatever arrived by timeout_ms (max 60000). Without wait, timeout_ms just collects output for that long. ANSI escapes are stripped unless raw. Example: {id, text:'help', eol:'crlf', wait:'\\\\n> ?$'}.",
            j!({
                "type": "object",
                "properties": {
                    "id": id_prop(),
                    "text": { "type": "string" },
                    "hex": { "type": "string", "description": "Bytes as hex, e.g. 'de ad be ef'" },
                    "eol": { "type": "string", "enum": ["none", "cr", "lf", "crlf"], "default": "none" },
                    "wait": { "type": "string", "description": "Regex to wait for in the output" },
                    "flags": { "type": "string", "default": "m" },
                    "timeout_ms": { "type": "integer", "default": 3000, "maximum": 60000 },
                    "raw": { "type": "boolean", "default": false },
                },
                "required": ["id"],
            }),
            |i, a| async move {
                let p = i.get_port(&id_arg(&a))?;
                i.write_and_wait(&p, &a).await
            },
        ),
        tool(
            inner,
            "serial_read",
            "Read received serial data from BenchBob's ring buffer (~256 KB of history, kept while the port is open). Pass the cursor from a previous call to get only newer data; omit it for the most recent output. wait_ms long-polls for new data. Returns {data, cursor, more, truncated}.",
            j!({
                "type": "object",
                "properties": {
                    "id": id_prop(),
                    "cursor": { "type": "integer" },
                    "max_bytes": { "type": "integer", "default": 16384 },
                    "wait_ms": { "type": "integer", "maximum": 60000 },
                    "format": { "type": "string", "enum": ["text", "hex", "base64"], "default": "text" },
                    "raw": { "type": "boolean", "default": false },
                },
                "required": ["id"],
            }),
            |i, a| async move {
                let p = i.get_port(&id_arg(&a))?;
                i.read_since(&p, &a).await
            },
        ),
        tool(
            inner,
            "serial_autobaud",
            "Guess a port's baud rate by listening only (never transmits): tries common rates ~400 ms each and scores how much the data looks like text. The target must be sending something (reset it first if it only talks at boot). apply=true sets the winner when confidence >= 0.5.",
            j!({ "type": "object", "properties": { "id": id_prop(), "rates": { "type": "array", "items": { "type": "integer" } }, "ms": { "type": "integer" }, "apply": { "type": "boolean" } }, "required": ["id"] }),
            |i, a| async move {
                let p = i.get_port(&id_arg(&a))?;
                i.autobaud(&p, &a).await
            },
        ),
        tool(
            inner,
            "serial_bridge",
            "Turn a port's network bridge on or off. kind=tcp is a raw byte socket (nc host port). kind=rfc2217 is RFC 2217 (pyserial rfc2217://host:port, esptool --port rfc2217://..., remote baud and DTR/RTS changes). Returns the port number and client commands.",
            j!({ "type": "object", "properties": { "id": id_prop(), "kind": { "type": "string", "enum": ["tcp", "rfc2217"] }, "enabled": { "type": "boolean" }, "port": { "type": "integer" } }, "required": ["id", "kind", "enabled"] }),
            |i, a| async move {
                let p = i.get_port(&id_arg(&a))?;
                let kind = a.get("kind").and_then(Value::as_str).and_then(BridgeKind::parse);
                let host = i.hostname();
                i.set_bridge(&p, kind, a.get("enabled").is_some_and(truthy), a.get("port"), &host)
            },
        ),
        tool(
            inner,
            "serial_flash",
            "Flash a board through its serial port. tool=esptool (ESP8266/ESP32 family): action write (files at offsets, e.g. bootloader 0x1000 / partitions 0x8000 / app 0x10000 for ESP32, or one merged image at 0x0), erase, or info (chip, MAC, flash size). tool=avrdude with preset uno | nano | nano-old | mega: write (.hex or .bin) or info. Files come from url or base64. Returns at once; poll serial_flash_status.",
            j!({
                "type": "object",
                "properties": {
                    "id": id_prop(),
                    "tool": { "type": "string", "enum": ["esptool", "avrdude"] },
                    "action": { "type": "string", "enum": ["write", "erase", "info"] },
                    "files": { "type": "array", "items": { "type": "object", "properties": { "offset": { "type": ["integer", "string"], "description": "e.g. 65536 or '0x10000'" }, "url": { "type": "string" }, "base64": { "type": "string" }, "name": { "type": "string" } } } },
                    "chip": { "type": "string", "enum": ESP_CHIPS },
                    "baud": { "type": "integer" },
                    "preset": { "type": "string", "enum": AVR_PRESETS.iter().map(|p| p.key).collect::<Vec<_>>() },
                },
                "required": ["id", "tool", "action"],
            }),
            |i, a| async move {
                let sources = json_sources(a.get("files"))?;
                let p = i.get_port(&id_arg(&a))?;
                let s = |k: &str| a.get(k).filter(|v| !v.is_null()).map(js_string);
                let opts = FlashOpts {
                    tool: s("tool").unwrap_or_default(),
                    action: s("action").unwrap_or_default(),
                    files: vec![],
                    baud: a.get("baud").filter(|v| !v.is_null()).map(js_number),
                    chip: s("chip"),
                    preset: s("preset"),
                };
                Ok(j!({ "job": i.start_flash(&p, FlashRequest { opts, sources })? }))
            },
        ),
        tool(
            inner,
            "serial_flash_status",
            "Progress and log of the current or last flash job on a port (state preparing/running/done/error/cancelled, percent, phase, chip info, error message).",
            j!({ "type": "object", "properties": { "id": id_prop(), "log_lines": { "type": "integer", "default": 40 } }, "required": ["id"] }),
            |i, a| async move {
                let p = i.get_port(&id_arg(&a))?;
                let n = a.get("log_lines").map(js_number).filter(|n| *n != 0.0 && !n.is_nan()).unwrap_or(40.0);
                let st = p.lock();
                let log = st.flash.as_ref().map(|f| f.log.clone()).unwrap_or_default();
                // log.slice(-n)
                let from = if n > 0.0 { log.len().saturating_sub(n as usize) } else { (-n as usize).min(log.len()) };
                Ok(j!({ "job": job_view(st.flash.as_ref()), "log": log[from..] }))
            },
        ),
    ]
}
