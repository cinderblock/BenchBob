//! The REST routes, /ws/ble and the MCP tools, as ble.ts declares them.

use std::sync::Arc;

use axum::http::Method;
use benchbob_core::http::{HttpError, js_number, js_string, json, truthy};
use benchbob_core::{Request, Route, RouteResult, Socket, Tool, WsIn, WsSession};
use serde_json::{Value, json as j};

use crate::{Inner, device_payload, num_arg};

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

fn q(r: &Request, k: &str) -> Option<f64> {
    num_arg(r.query(k).map(Value::from).as_ref())
}

/// A controller argument as the TypeScript passes it on (any truthy value, as a string).
fn ctrl_arg(v: Option<&Value>) -> Option<String> {
    v.filter(|v| truthy(v)).map(js_string)
}

pub(crate) fn routes(i: &Arc<Inner>) -> Vec<Route> {
    vec![
        route(
            i,
            Method::GET,
            "/api/ble",
            j!("BLE scanner state: controllers (hci0 + USB dongles, power/rfkill), the running scan, device counts"),
            |i, _| async move { json(i.status()) },
        ),
        route(
            i,
            Method::GET,
            "/api/ble/devices",
            j!({ "description": "Advertisers heard recently, strongest first, with decoded beacons", "query": { "seconds": "only those heard in the last N s", "minRssi": "e.g. -70", "limit": "default 100", "beacons": "1 = only decoded beacons/sensors" } }),
            |i, r| async move {
                json(Value::from(i.list_devices(None, q(&r, "seconds"), q(&r, "minRssi"), q(&r, "limit"), r.query("beacons") == Some("1"))))
            },
        ),
        route(
            i,
            Method::GET,
            "/api/ble/devices/:addr",
            j!("One advertiser in full: RSSI history (1 s slots), raw advertising + scan-response bytes, every AD field"),
            |i, r| async move {
                let addr = r.param("addr");
                let st = i.st.lock().unwrap();
                match st.tracker.devices.get(&addr.to_uppercase()) {
                    Some(d) => json(device_payload(d)),
                    None => Err(HttpError::new(404, format!("No device {addr} heard recently"))),
                }
            },
        ),
        route(
            i,
            Method::POST,
            "/api/ble/scan",
            j!({ "body": { "duration": 10, "controller": "hci0 (optional)", "wait": false }, "description": "Scan for `duration` s (max 600; extends a running scan). With wait:true, responds when done with the devices heard (max 120 s)." }),
            |i, r| async move {
                let b = r.json()?;
                if b.get("wait").is_some_and(truthy) {
                    return json(i.scan_and_wait(&b).await?);
                }
                let d = b.get("duration").map(js_number).filter(|d| *d != 0.0 && !d.is_nan()).unwrap_or(10.0).max(1.0);
                i.start_scan(ctrl_arg(b.get("controller")), Some(d)).await?;
                let scan = i.st.lock().unwrap().scan_info();
                json(j!({ "ok": true, "scan": scan }))
            },
        ),
        route(
            i,
            Method::POST,
            "/api/ble/stop",
            j!("Stop scanning now (restores the radio's previous power/rfkill state)"),
            |i, _| async move {
                i.stop_scan("").await;
                json(j!({ "ok": true }))
            },
        ),
        route(i, Method::DELETE, "/api/ble/devices", j!("Forget all advertisers seen so far"), |i, _| async move {
            {
                let mut st = i.st.lock().unwrap();
                st.tracker.devices.clear();
                st.last_push_sig.clear();
            }
            i.changed();
            json(j!({ "ok": true }))
        }),
        route(
            i,
            Method::POST,
            "/api/ble/settings",
            j!({ "body": { "controller": "hci1 or its address", "restoreRadio": true }, "description": "Default controller (remembered by address) and whether to put the radio back to off/blocked after scanning" }),
            |i, r| async move {
                let b = r.json()?;
                let ctx = i.ctx.get().ok_or_else(|| HttpError::new(500, "not started"))?;
                if let Some(c) = b.get("controller") {
                    let c = i.pick_controller(Some(&js_string(c)))?;
                    ctx.set_setting("controller", Value::from(if c.address.is_empty() { c.name } else { c.address }));
                }
                if let Some(v) = b.get("restoreRadio") {
                    ctx.set_setting("restoreRadio", Value::Bool(truthy(v)));
                }
                i.changed();
                json(j!({ "ok": true }))
            },
        ),
    ]
}

const WS_DOC: &str = "Live device list (pushed ~1/s while it changes). Send {\"live\":true} to keep scanning while this socket is open; {\"live\":false} to stop watching.";

pub(crate) fn sockets(i: &Arc<Inner>) -> Vec<Socket> {
    let i = i.clone();
    vec![Socket::new("/ws/ble", Some(WS_DOC), move |s| ws_session(i.clone(), s))]
}

async fn ws_session(i: Arc<Inner>, mut s: WsSession) {
    let id = s.client.id();
    {
        let mut st = i.st.lock().unwrap();
        st.watchers.insert(id, (s.client.clone(), false));
        st.last_push_sig.clear();
        s.client.send(serde_json::to_string(&Inner::snapshot(&st)).unwrap());
    }
    while let Some(m) = s.incoming.recv().await {
        let text = match m {
            WsIn::Text(t) => t,
            WsIn::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
        };
        // A message that isn't JSON is ignored (the TypeScript's handler threw on it).
        let Ok(m) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(live) = m.get("live").and_then(Value::as_bool) else { continue };
        if let Some(w) = i.st.lock().unwrap().watchers.get_mut(&id) {
            w.1 = live;
        }
        if live {
            let (i2, c) = (i.clone(), s.client.clone());
            let ctrl = ctrl_arg(m.get("controller"));
            tokio::spawn(async move {
                if let Err(e) = i2.start_scan(ctrl, None).await {
                    c.send(serde_json::to_string(&j!({ "type": "error", "message": e.message })).unwrap());
                }
            });
        }
        i.changed();
    }
    i.st.lock().unwrap().watchers.shift_remove(&id);
}

pub(crate) fn tools(i: &Arc<Inner>) -> Vec<Tool> {
    vec![
        tool(
            i,
            "ble_scan",
            "Scan for Bluetooth LE advertisers for `duration` seconds and return what was heard: address (+public/random type), name, RSSI, TX power, connectable, manufacturer and service data, and decoded beacons/sensors (iBeacon, Eddystone, BTHome, RuuviTag, Xiaomi/ATC/pvvx thermometers, Govee, Apple Continuity labels, Microsoft Swift Pair).",
            j!({
                "type": "object",
                "properties": {
                    "duration": { "type": "number", "description": "Seconds to scan (1–120, default 10)" },
                    "controller": { "type": "string", "description": "hci0 (onboard) or another controller name/address; default is the remembered one" },
                    "minRssi": { "type": "number", "description": "Only devices at least this strong, e.g. -75" },
                    "limit": { "type": "number", "description": "Max devices (default 100)" }
                }
            }),
            |i, a| async move { i.scan_and_wait(&a).await },
        ),
        tool(
            i,
            "ble_devices",
            "List BLE advertisers already heard (from earlier or ongoing scans) without starting a scan. Strongest first.",
            j!({
                "type": "object",
                "properties": {
                    "seconds": { "type": "number", "description": "Only devices heard in the last N seconds" },
                    "minRssi": { "type": "number" },
                    "beaconsOnly": { "type": "boolean", "description": "Only devices with a decoded beacon/sensor payload" },
                    "limit": { "type": "number", "description": "Max devices (default 100)" }
                }
            }),
            |i, a| async move {
                let devices = i.list_devices(
                    None,
                    num_arg(a.get("seconds")),
                    num_arg(a.get("minRssi")),
                    num_arg(a.get("limit")),
                    a.get("beaconsOnly").is_some_and(truthy),
                );
                let (scan, last) = {
                    let st = i.st.lock().unwrap();
                    (st.scan_info(), st.last_scan.as_ref().map(|l| l.json()).unwrap_or(Value::Null))
                };
                Ok(j!({ "scan": scan, "lastScan": last, "controllers": i.controllers_short(), "devices": devices }))
            },
        ),
    ]
}
