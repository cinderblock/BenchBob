//! The REST routes, the /ws/can/:iface monitor and the MCP tools, as can.ts declares them.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderValue, Method, header};
use axum::response::Response;
use benchbob_core::http::{HttpError, js_number, json, truthy};
use benchbob_core::{Reply, Request, Route, RouteResult, Socket, Tool, WsIn, WsSession};
use serde_json::{Value, json as j};

use crate::frame::{self, CanFrame, IdFilter};
use crate::hub::{OutFrame, WsState};
use crate::json::{O, spread};
use crate::{Inner, MIN_PERIOD_MS, arg_num, arg_str};

fn frame_from(x: &Value) -> Result<CanFrame, HttpError> {
    frame::spec_from_json(x).and_then(|s| frame::make_frame(&s)).map_err(HttpError::bad_request)
}

fn route<F, Fut>(i: &Arc<Inner>, method: Method, path: &str, doc: Value, f: F) -> Route
where
    F: Fn(Arc<Inner>, Request) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = RouteResult> + Send + 'static,
{
    let i = i.clone();
    Route::new(method, path, Some(doc), move |r| f(i.clone(), r))
}

fn tool<F, Fut>(i: &Arc<Inner>, name: &str, description: &str, schema: Value, f: F) -> Tool
where
    F: Fn(Arc<Inner>, Value) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Value, HttpError>> + Send + 'static,
{
    let i = i.clone();
    Tool::new(name, description, schema, move |a| f(i.clone(), a))
}

/// `iface(params)`: the interface must exist.
fn iface_name(i: &Inner, name: &str) -> Result<String, HttpError> {
    i.st.lock().unwrap().iface(name).map(|x| x.name.clone())
}

pub(crate) fn send_now(i: &Inner, name: &str, f: &CanFrame) -> Result<(), HttpError> {
    i.st.lock().unwrap().send(name, f)
}

fn iface_detail(i: &Inner, name: &str) -> Result<Value, HttpError> {
    let mut st = i.st.lock().unwrap();
    let ifc = st.iface(name)?.clone();
    let out = i.iface_out(&st, &ifc);
    let snap = {
        let hub = st.hub_for(name)?;
        let hub = &*hub;
        hub.snapshot(Some(&ifc), true)
    };
    Ok(spread(out, snap))
}

fn download(path: std::path::PathBuf, file: String) -> Result<Response, HttpError> {
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let stream = futures_util::stream::unfold(None::<tokio::fs::File>, move |f| {
        let path = path.clone();
        async move {
            use tokio::io::AsyncReadExt;
            let mut f = match f {
                Some(f) => f,
                None => match tokio::fs::File::open(&path).await {
                    Ok(f) => f,
                    Err(e) => return Some((Err(e), None)),
                },
            };
            let mut buf = vec![0u8; 64 * 1024];
            match f.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(bytes::Bytes::from(buf)), Some(f)))
                }
                Err(e) => Some((Err(e), None)),
            }
        }
    });
    let mut r = Response::new(Body::from_stream(stream));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{file}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    Ok(r)
}

pub(crate) fn routes(i: &Arc<Inner>) -> Vec<Route> {
    let rec_dir = i.rec_dir.display().to_string();
    vec![
        route(
            i,
            Method::GET,
            "/api/can",
            j!("CAN interfaces (bitrate, controller state, error counters, rates), SLCAN adapters, socketcand server, recordings"),
            |i, _| async move { json(i.list()) },
        ),
        route(
            i,
            Method::POST,
            "/api/can/vcan",
            j!({ "body": { "name": "vcan0 (optional)", "fd": true }, "description": "Create a virtual CAN interface for testing (FD MTU by default) and bring it up" }),
            |i, r| async move {
                let b = r.json()?;
                let name = b.get("name").filter(|v| truthy(v)).map(benchbob_core::http::js_string);
                json(i.create_vcan(name, b.get("fd") != Some(&Value::Bool(false))).await?)
            },
        ),
        route(i, Method::DELETE, "/api/can/vcan/:name", j!("Remove a virtual CAN interface"), |i, r| async move {
            json(i.remove_vcan(r.param("name")).await?)
        }),
        route(
            i,
            Method::POST,
            "/api/can/slcan",
            j!({ "body": { "tty": "/dev/ttyACM0", "bitrate": 500000, "name": "slcan0 (optional)", "serialBaud": 3000000 }, "description": "Attach a serial SLCAN adapter as a SocketCAN interface (runs slcand). While attached, the serial port is busy and the serial panel can't open it." }),
            |i, r| async move { json(i.attach_slcan(&r.json()?).await?) },
        ),
        route(
            i,
            Method::DELETE,
            "/api/can/slcan/:name",
            j!("Detach an SLCAN adapter (by interface name or tty), freeing the serial port"),
            |i, r| async move { json(i.detach_slcan(r.param("name")).await?) },
        ),
        route(
            i,
            Method::POST,
            "/api/can/socketcand",
            j!({ "body": { "enabled": true, "port": 29536, "beacon": false, "errMask": 0 }, "description": "socketcand-protocol TCP server settings (python-can interface='socketcand', SavvyCAN, Kayak) and its UDP discovery beacon on port 42000" }),
            |i, r| async move {
                let b = r.json()?;
                let ctx = i.ctx();
                if b.get("port").is_some_and(|v| !v.is_null()) {
                    let p = arg_num(&b, "port");
                    if !(1024.0..=65535.0).contains(&p) {
                        return Err(HttpError::bad_request("port must be 1024–65535"));
                    }
                    ctx.set_setting("socketcandPort", crate::json::num(p));
                }
                if let Some(v) = b.get("enabled").filter(|v| !v.is_null()) {
                    ctx.set_setting("socketcand", Value::Bool(truthy(v)));
                }
                if let Some(v) = b.get("beacon").filter(|v| !v.is_null()) {
                    ctx.set_setting("beacon", Value::Bool(truthy(v)));
                }
                if b.get("errMask").is_some_and(|v| !v.is_null()) {
                    ctx.set_setting("socketcandErrMask", Value::from(frame::to_uint32(arg_num(&b, "errMask"))));
                }
                i.start_socketcand().await;
                let epoch = i.epoch.load(std::sync::atomic::Ordering::SeqCst);
                i.start_beacon(epoch).await;
                json(i.socketcand_out())
            },
        ),
        route(i, Method::GET, "/api/can/recordings", j!(format!("candump -L format recordings in {rec_dir}")), |i, _| async move {
            i.refresh_recordings();
            json(Value::from(i.st.lock().unwrap().recordings.clone()))
        }),
        route(
            i,
            Method::GET,
            "/api/can/recordings/:file",
            j!("Download a recording (candump -L format: (ts) can0 123#DEADBEEF; replay with canplayer -I file)"),
            |i, r| async move {
                let file = r.param("file").to_string();
                let p = i.rec_path(&file)?;
                Ok(Reply::Response(download(p, file)?))
            },
        ),
        route(i, Method::DELETE, "/api/can/recordings/:file", j!("Delete a recording"), |i, r| async move {
            let file = r.param("file");
            if i.st.lock().unwrap().hubs.values().any(|h| h.rec.as_ref().is_some_and(|x| x.file == file)) {
                return Err(HttpError::new(409, "Stop the recording first"));
            }
            let p = i.rec_path(file)?;
            std::fs::remove_file(&p).map_err(|e| HttpError::new(500, format!("{e}")))?;
            i.refresh_recordings();
            json(j!({ "ok": true }))
        }),
        route(
            i,
            Method::GET,
            "/api/can/:iface",
            j!(
                "One interface: config, capabilities (FD, clock, ctrlmodes), state, counters, per-ID table, last error frames, periodic jobs"
            ),
            |i, r| async move { json(iface_detail(&i, r.param("iface"))?) },
        ),
        route(
            i,
            Method::POST,
            "/api/can/:iface/config",
            j!({
                "body": { "bitrate": 500000, "samplePoint": 0.875, "fd": false, "dbitrate": 2000000, "dsamplePoint": 0.75, "listenOnly": false, "loopback": false, "tripleSampling": false, "oneShot": false, "berrReporting": false, "restartMs": 100, "bringUpOnPlug": true, "up": true },
                "description": "Configure with ip link (down, set, up). Every field is optional. Saved per adapter (USB serial/path) and re-applied on hotplug when bringUpOnPlug is set. vcan only takes fd (MTU) and up.",
            }),
            |i, r| async move {
                let name = iface_name(&i, r.param("iface"))?;
                json(i.configure(&name, &r.json()?, true).await?)
            },
        ),
        route(
            i,
            Method::POST,
            "/api/can/:iface/restart",
            j!("Restart the controller after bus-off (ip link set … type can restart)"),
            |i, r| async move {
                let name = iface_name(&i, r.param("iface"))?;
                json(i.restart_bus(&name).await?)
            },
        ),
        route(
            i,
            Method::POST,
            "/api/can/:iface/autobitrate",
            j!({ "body": { "rates": frame::STD_BITRATES, "listenMs": 400, "apply": false }, "description": "Detect the bus bitrate: tries each rate in listen-only mode (never ACKs, safe on a live bus), counting valid vs error frames. Restores the old config unless apply is true. Takes ~0.6 s per rate." }),
            |i, r| async move {
                let name = iface_name(&i, r.param("iface"))?;
                json(i.autobitrate(&name, &r.json()?).await?)
            },
        ),
        route(
            i,
            Method::POST,
            "/api/can/:iface/send",
            j!({ "body": { "id": "123", "ext": false, "rtr": false, "fd": false, "brs": false, "data": "DEADBEEF" }, "description": "Send one frame. id may be a number or hex string; or pass {cansend:'123#DEADBEEF'}" }),
            |i, r| async move {
                let f = frame_from(&r.json()?)?;
                let name = iface_name(&i, r.param("iface"))?;
                send_now(&i, &name, &f)?;
                json(j!({ "ok": true, "sent": frame::candump_body(&f) }))
            },
        ),
        route(
            i,
            Method::GET,
            "/api/can/:iface/capture",
            j!("Collect frames for ms (default 1000, max 60000), optionally filtered: ?ms=2000&filter=7E8,x18DAF100:1FFFFF00&max=1000"),
            |i, r| async move {
                let filters = frame::parse_filter_string(r.query("filter")).map_err(HttpError::bad_request)?;
                let q = |k: &str| r.query(k).map(|v| js_number(&Value::from(v))).unwrap_or(0.0);
                let name = iface_name(&i, r.param("iface"))?;
                json(i.capture(&name, q("ms"), filters, q("max")).await?)
            },
        ),
        route(
            i,
            Method::POST,
            "/api/can/:iface/periodic",
            j!({ "body": { "periodMs": 100, "id": "123", "data": "0011223344556677" }, "description": format!("Start a periodic transmit job (min {} ms; timer based, ~1 ms jitter). Frame fields as for /send.", crate::fmt_num(MIN_PERIOD_MS)) }),
            |i, r| async move {
                let b = r.json()?;
                let name = iface_name(&i, r.param("iface"))?;
                i.st.lock().unwrap().hub_for(&name)?;
                let f = frame_from(&b)?;
                json(i.start_job(&name, f, arg_num(&b, "periodMs"))?)
            },
        ),
        route(i, Method::DELETE, "/api/can/:iface/periodic/:job", j!("Stop a periodic transmit job"), |i, r| async move {
            let job = r.param("job");
            if !i.stop_job(r.param("iface"), js_number(&Value::from(job)))? {
                return Err(HttpError::new(404, format!("No job {job}")));
            }
            json(j!({ "ok": true }))
        }),
        route(
            i,
            Method::POST,
            "/api/can/:iface/record",
            j!({ "body": { "on": true }, "description": "Start or stop recording everything on the interface (incl. error frames) to a candump -L log" }),
            |i, r| async move {
                let name = iface_name(&i, r.param("iface"))?;
                i.st.lock().unwrap().hub_for(&name)?;
                let b = r.json()?;
                if b.get("on").is_some_and(truthy) {
                    let file = i.start_recording(&name)?;
                    return json(j!({ "ok": true, "file": file }));
                }
                match i.stop_recording(&name)? {
                    Some(v) => json(spread(j!({ "ok": true }), v)),
                    None => Err(HttpError::new(409, "Not recording")),
                }
            },
        ),
        route(i, Method::POST, "/api/can/:iface/clear", j!("Reset the per-ID table, counters and frame log"), |i, r| async move {
            let name = iface_name(&i, r.param("iface"))?;
            i.st.lock().unwrap().hub_for(&name)?.clear();
            json(j!({ "ok": true }))
        }),
    ]
}

// ---------- WebSocket ----------

const WS_DOC: &str = "Live traffic for one interface. Query: filter=123,7E0:7F8,x18DAF100:1FFFFF00 (hex id[:mask], x = extended); batch=1 to get {type:'frames',frames:[…],dropped} every 50 ms instead of one {type:'frame',…} message per frame; stats=1 for a {type:'stats',…} snapshot (rates, controller state, per-ID table, errors) at 4 Hz; frames=0 for stats only. Frame: {id, ext, rtr, fd, brs, esi?, err?, len, data (hex), ts (Unix s, µs), dir ('rx'|'tx'), text? (decoded error)}. Send {send:{id:'123'|291, ext?, rtr?, fd?, brs?, data:'DEADBEEF'}} or {send:{cansend:'123#DEADBEEF'}}, {filter:'…'|[{id,mask?,ext?}]|null}, {frames:false|true}, {clear:true}.";

fn ws_error(m: &str) -> String {
    serde_json::to_string(&j!({ "type": "error", "message": m })).unwrap()
}

pub(crate) fn sockets(i: &Arc<Inner>) -> Vec<Socket> {
    let i = i.clone();
    vec![Socket::new("/ws/can/:iface", Some(WS_DOC), move |s| ws_session(i.clone(), s))]
}

async fn ws_session(i: Arc<Inner>, mut s: WsSession) {
    let name = s.params.get("iface").cloned().unwrap_or_default();
    let q = |k: &str| s.query.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
    let id = s.client.id();
    {
        let mut st = i.st.lock().unwrap();
        let st = &mut *st;
        let Some(ifc) = st.ifaces.get(&name).cloned().filter(|_| st.hubs.contains_key(&name)) else {
            s.client.send(ws_error(&format!("No CAN interface \"{name}\"")));
            s.client.close();
            return;
        };
        let filters = match frame::parse_filter_string(q("filter").as_deref()) {
            Ok(f) => f,
            Err(e) => {
                s.client.send(ws_error(&e));
                None
            }
        };
        let ws = WsState {
            client: s.client.clone(),
            filters: filters.clone(),
            frames: q("frames").as_deref() != Some("0"),
            batch: q("batch").as_deref() == Some("1"),
            stats: q("stats").as_deref() == Some("1"),
            q: Vec::new(),
            dropped: 0,
        };
        let stats = ws.stats;
        let iface = i.iface_out(st, &ifc);
        let hub = st.hubs.get_mut(&name).unwrap();
        hub.clients.insert(id, ws);
        let n = hub.ring.len().saturating_sub(200);
        let recent: Vec<Value> =
            hub.ring[n..].iter().filter(|f| frame::match_filters(f.id, f.ext, f.err, filters.as_deref())).map(OutFrame::json).collect();
        s.client.send(serde_json::to_string(&j!({ "type": "hello", "iface": iface, "recent": recent })).unwrap());
        if stats {
            s.client.send(serde_json::to_string(&spread(j!({ "type": "stats" }), hub.snapshot(Some(&ifc), true))).unwrap());
        }
    }
    // Switch the tick to 50 ms now rather than after the current idle second.
    i.tick_wake.notify_one();
    while let Some(m) = s.incoming.recv().await {
        let text = match m {
            WsIn::Text(t) => t,
            WsIn::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
        };
        if let Err(e) = ws_message(&i, &name, id, &text) {
            s.client.send(ws_error(&e));
        }
    }
    if let Some(h) = i.st.lock().unwrap().hubs.get_mut(&name) {
        h.clients.shift_remove(&id);
    }
}

fn ws_message(i: &Inner, name: &str, id: u64, text: &str) -> Result<(), String> {
    let m: Value = serde_json::from_str(text).map_err(|_| "Messages must be JSON".to_string())?;
    let Value::Object(m) = m else { return Ok(()) };
    let client = {
        let st = i.st.lock().unwrap();
        let Some(ws) = st.hubs.get(name).and_then(|h| h.clients.get(&id)) else { return Ok(()) };
        ws.client.clone()
    };
    if let Some(sv) = m.get("send").filter(|v| truthy(v)) {
        let r = frame_from(sv).and_then(|f| {
            let mut st = i.st.lock().unwrap();
            let st = &mut *st;
            let hub = st.hubs.get(name).ok_or_else(|| HttpError::new(404, format!("No CAN interface {name}")))?;
            hub.send(st.ifaces.get(name), &f)
        });
        if let Err(e) = r {
            client.send(ws_error(&e.message));
        }
    }
    let mut st = i.st.lock().unwrap();
    let Some(hub) = st.hubs.get_mut(name) else { return Ok(()) };
    if let Some(f) = m.get("filter") {
        let parsed: Result<Option<Vec<IdFilter>>, String> = match f {
            Value::String(s) => frame::parse_filter_string(Some(s)),
            other => frame::parse_filters(other),
        };
        match parsed {
            Ok(p) => {
                if let Some(ws) = hub.clients.get_mut(&id) {
                    ws.filters = p;
                }
            }
            Err(e) => {
                client.send(ws_error(&e));
            }
        }
    }
    if let Some(ws) = hub.clients.get_mut(&id) {
        if let Some(f) = m.get("frames") {
            ws.frames = truthy(f);
        }
        if let Some(f) = m.get("stats") {
            ws.stats = truthy(f);
        }
    }
    if m.get("clear").is_some_and(truthy) {
        hub.clear();
    }
    Ok(())
}

// ---------- MCP tools ----------

fn frame_schema_props() -> serde_json::Map<String, Value> {
    let v = j!({
        "id": { "type": ["integer", "string"], "description": "Arbitration id: a number, or a hex string ('0x123' or '123')" },
        "ext": { "type": "boolean", "description": "29-bit extended id (implied when id > 0x7FF)" },
        "rtr": { "type": "boolean", "description": "Remote transmission request (classic CAN only)" },
        "len": { "type": "integer", "description": "RTR request length (0–8)" },
        "fd": { "type": "boolean", "description": "CAN FD frame (interface must be in FD mode)" },
        "brs": { "type": "boolean", "description": "CAN FD bit-rate switch" },
        "data": { "type": "string", "description": "Payload as hex, e.g. 'DEADBEEF' or 'DE AD BE EF'" },
        "cansend": { "type": "string", "description": "Alternative to the above: cansend syntax, e.g. '123#DEADBEEF', '18DAF110#R', '123##1DEADBEEF'" },
    });
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

pub(crate) fn tools(i: &Arc<Inner>) -> Vec<Tool> {
    let mut send_props = serde_json::Map::new();
    send_props.insert("iface".into(), j!({ "type": "string" }));
    send_props.extend(frame_schema_props());
    send_props.insert(
        "periodMs".into(),
        j!({ "type": "integer", "description": format!("Repeat every N ms (min {})", crate::fmt_num(MIN_PERIOD_MS)) }),
    );
    send_props.insert("stopJob".into(), j!({ "type": "integer", "description": "Stop the periodic job with this id" }));
    vec![
        tool(
            i,
            "can_list",
            "List CAN interfaces on BenchBob with bitrate, FD mode, controller state (error-active/warning/passive/bus-off), error counters, frame rates and bus load; plus attachable SLCAN serial adapters and the socketcand server address.",
            j!({ "type": "object", "properties": {} }),
            |i, _| async move { Ok(i.list()) },
        ),
        tool(
            i,
            "can_configure",
            "Configure a CAN interface (applies with ip link: down, set, up). Set bitrate/samplePoint, CAN FD data bitrate, listen-only, loopback, triple-sampling, restart-ms (bus-off auto-restart), or bring it up/down. Settings persist per adapter. Pass restart:true to restart a controller stuck in bus-off.",
            j!({
                "type": "object",
                "properties": {
                    "iface": { "type": "string", "description": "Interface name, e.g. can0" },
                    "bitrate": { "type": "integer", "description": "Nominal bitrate in bit/s, e.g. 500000" },
                    "samplePoint": { "type": "number", "description": "Sample point, 0–1 (e.g. 0.875)" },
                    "fd": { "type": "boolean", "description": "Enable CAN FD (needs dbitrate)" },
                    "dbitrate": { "type": "integer", "description": "CAN FD data-phase bitrate, e.g. 2000000" },
                    "dsamplePoint": { "type": "number" },
                    "listenOnly": { "type": "boolean", "description": "Never transmit or ACK" },
                    "loopback": { "type": "boolean", "description": "Controller internal loopback (testing without a bus)" },
                    "tripleSampling": { "type": "boolean" },
                    "restartMs": { "type": "integer", "description": "Auto-restart delay after bus-off in ms; 0 disables" },
                    "bringUpOnPlug": { "type": "boolean", "description": "Apply this config and bring the interface up whenever the adapter is plugged in" },
                    "up": { "type": "boolean", "description": "Bring the interface up (true) or down (false)" },
                    "restart": { "type": "boolean", "description": "Restart the controller (bus-off recovery) instead of configuring" },
                },
                "required": ["iface"],
            }),
            |i, a| async move {
                let name = arg_str(&a, "iface");
                if a.get("restart").is_some_and(truthy) { i.restart_bus(&name).await } else { i.configure(&name, &a, true).await }
            },
        ),
        tool(
            i,
            "can_autobitrate",
            "Detect a CAN bus's bitrate safely: tries standard rates (1M…10k) in listen-only mode (the controller never ACKs or transmits), counting valid vs error frames for listenMs each. Returns the winner, 'silent' (no traffic) or 'none' (errors at every rate). Restores the previous config unless apply is true.",
            j!({
                "type": "object",
                "properties": {
                    "iface": { "type": "string" },
                    "rates": { "type": "array", "items": { "type": "integer" }, "description": "Bitrates to try (default: standard list)" },
                    "listenMs": { "type": "integer", "description": "Listen time per rate, default 400" },
                    "apply": { "type": "boolean", "description": "Configure the detected bitrate (normal mode) when found" },
                },
                "required": ["iface"],
            }),
            |i, a| async move { i.autobitrate(&arg_str(&a, "iface"), &a).await },
        ),
        tool(
            i,
            "can_send",
            "Send a CAN frame, or start/stop a periodic one. Give id + data (hex), or cansend syntax ('123#DEADBEEF', '18DAF110#R', '123##1DEADBEEF' for FD+BRS). With periodMs, starts a periodic job and returns its id; stopJob stops one.",
            j!({ "type": "object", "properties": send_props, "required": ["iface"] }),
            |i, a| async move {
                let name = arg_str(&a, "iface");
                i.st.lock().unwrap().hub_for(&name)?;
                if a.get("stopJob").is_some_and(|v| !v.is_null()) {
                    return Ok(j!({ "ok": i.stop_job(&name, arg_num(&a, "stopJob"))? }));
                }
                let f = frame_from(&a)?;
                if a.get("periodMs").is_some_and(truthy) {
                    return i.start_job(&name, f, arg_num(&a, "periodMs"));
                }
                send_now(&i, &name, &f)?;
                Ok(j!({ "ok": true, "sent": frame::candump_body(&f) }))
            },
        ),
        tool(
            i,
            "can_capture",
            "Capture CAN frames for a time window and return them (id, ext, rtr, fd, data hex, ts in Unix seconds, dir rx/tx; error frames include a decoded text). Optional id filter with mask, e.g. id '7E8' mask '7F8' for OBD responses.",
            j!({
                "type": "object",
                "properties": {
                    "iface": { "type": "string" },
                    "ms": { "type": "integer", "description": "Window length, default 1000, max 60000" },
                    "id": { "type": ["integer", "string"], "description": "Only frames matching this id (number or hex)" },
                    "mask": { "type": ["integer", "string"], "description": "Bits of id that must match (default all)" },
                    "ext": { "type": "boolean", "description": "Only extended (true) or standard (false) ids" },
                    "filter": { "type": "string", "description": "Instead of id/mask: several, e.g. '123,7E0:7F8,x18DAF100:1FFFFF00'" },
                    "max": { "type": "integer", "description": "Stop after this many frames (default 1000)" },
                },
                "required": ["iface"],
            }),
            |i, a| async move {
                let filters = if a.get("filter").is_some_and(truthy) {
                    frame::parse_filter_string(Some(&arg_str(&a, "filter")))
                } else if a.get("id").is_some_and(|v| !v.is_null()) {
                    let mut f = serde_json::Map::new();
                    for k in ["id", "mask", "ext"] {
                        if let Some(v) = a.get(k) {
                            f.insert(k.into(), v.clone());
                        }
                    }
                    frame::parse_filters(&Value::Object(f))
                } else {
                    Ok(None)
                }
                .map_err(HttpError::bad_request)?;
                i.capture(&arg_str(&a, "iface"), arg_num(&a, "ms"), filters, arg_num(&a, "max")).await
            },
        ),
        tool(
            i,
            "can_stats",
            "Traffic statistics for a CAN interface: frames/s, TX/s, errors/s, bus load %, totals, controller state and error counters, the per-ID table (count, period, last data) and recent decoded error frames.",
            j!({ "type": "object", "properties": { "iface": { "type": "string" }, "limit": { "type": "integer", "description": "Max ID rows (default 100)" } }, "required": ["iface"] }),
            |i, a| async move {
                let name = arg_str(&a, "iface");
                let mut st = i.st.lock().unwrap();
                let ifc = st.ifaces.get(&name).cloned();
                let hub = &*st.hub_for(&name)?;
                let mut snap = hub.snapshot(ifc.as_ref(), true);
                let l = arg_num(&a, "limit");
                let limit = (if l.is_nan() || l == 0.0 { 100.0 } else { l }).max(1.0) as usize;
                let ids: Vec<Value> = hub
                    .id_rows(limit)
                    .into_iter()
                    .map(|r| {
                        let mut r = serde_json::to_value(r).unwrap();
                        let o = r.as_object_mut().unwrap();
                        let (id, ext) = (o["id"].as_u64().unwrap_or(0) as u32, o["ext"] == true);
                        o.insert("id".into(), Value::from(frame::fmt_id(id, ext)));
                        let period = o["period"].clone();
                        o.shift_remove("hot");
                        o.insert("last".into(), Value::from(crate::json::iso(o["last"].as_f64().unwrap_or(0.0) as i64)));
                        o.insert("periodMs".into(), period);
                        r
                    })
                    .collect();
                let truncated = hub.ids.len() > limit;
                snap["ids"] = Value::from(ids);
                let iface = ifc.map(|x| i.iface_out(&st, &x)).unwrap_or(Value::Null);
                Ok(O(snap.as_object().unwrap().clone()).k("idsTruncated", truncated).k("interface", iface).v())
            },
        ),
        tool(
            i,
            "can_vcan",
            "Create or remove a virtual CAN interface (vcan) for testing without hardware. Frames sent on it are seen by every listener on the same vcan.",
            j!({ "type": "object", "properties": { "action": { "type": "string", "enum": ["create", "remove"] }, "name": { "type": "string", "description": "e.g. vcan0 (default: next free)" }, "fd": { "type": "boolean", "description": "CAN FD MTU (default true)" } }, "required": ["action"] }),
            |i, a| async move {
                if a.get("action").and_then(Value::as_str) == Some("remove") {
                    return i.remove_vcan(&arg_str(&a, "name")).await;
                }
                let name = a.get("name").filter(|v| truthy(v)).map(benchbob_core::http::js_string);
                i.create_vcan(name, a.get("fd") != Some(&Value::Bool(false))).await
            },
        ),
    ]
}
