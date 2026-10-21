//! The REST routes and MCP tools, as usb.ts declares them.

use std::sync::Arc;

use axum::http::Method;
use benchbob_core::http::{HttpError, json, json_response, parse_int};
use benchbob_core::run::{Opts, run};
use benchbob_core::{Reply, Request, Route, RouteResult, Tool};
use serde_json::{Map, Value, json as j};

use crate::{Inner, PowerErr, Rule, arg_str, bin, model, needs_message};

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

/// `{match, value}`, validated (usb.ts `validateRule`).
pub(crate) fn validate_rule(r: &Value) -> Result<Rule, HttpError> {
    let kind = r.get("match").and_then(Value::as_str).unwrap_or("");
    let value = arg_str(r.get("value"));
    let vidpid = regex::Regex::new(r"^[0-9a-f]{4}:[0-9a-f]{4}$").unwrap();
    let port = regex::Regex::new(r"^\d+-\d+(\.\d+)*$").unwrap();
    if kind == "vidpid" && vidpid.is_match(&value.to_lowercase()) {
        return Ok(Rule { kind: "vidpid".into(), value: value.to_lowercase() });
    }
    if kind == "port" && port.is_match(&value) {
        return Ok(Rule { kind: "port".into(), value });
    }
    Err(HttpError::bad_request(r#"A rule is {match:"vidpid", value:"0403:6001"} or {match:"port", value:"1-1.2"}"#))
}

fn query_obj(r: &Request) -> Value {
    let mut m = Map::new();
    for (k, v) in &r.query {
        m.insert(k.clone(), Value::String(v.clone()));
    }
    Value::Object(m)
}

pub(crate) fn routes(i: &Arc<Inner>) -> Vec<Route> {
    let busid_param = j!({ "busid": "1-1.2" });
    vec![
        route(
            i,
            Method::GET,
            "/api/usb",
            j!("Full USB state: controllers, the device tree with interfaces/drivers/nodes, hub port power, sharing and protection"),
            |i, _| async move { json(i.status(false)) },
        ),
        route(i, Method::GET, "/api/usb/devices", j!("Flattened USB tree (one entry per device, depth-first)"), |i, _| async move {
            json(i.flat_list())
        }),
        route(
            i,
            Method::GET,
            "/api/usb/timeline",
            j!({ "query": { "limit": "100", "kinds": "plug,unplug,kernel,power,share (optional)" }, "description": "Hardware timeline, newest first (all peripheral drivers' events; USB ones carry detail.kind)" }),
            |i, r| async move {
                let limit = r.query("limit").and_then(parse_int).filter(|n| *n != 0).unwrap_or(100).clamp(0, 300) as usize;
                let kinds: Option<Vec<String>> = r.query("kinds").map(|k| k.split(',').map(str::to_string).collect());
                json(i.timeline_entries(limit, kinds.as_deref()))
            },
        ),
        route(
            i,
            Method::GET,
            "/api/usb/dmesg",
            j!("USB-related kernel log lines since boot (historical; the live timeline only shows new ones)"),
            |_, _| async move {
                let d = bin("dmesg").unwrap_or_else(|| "dmesg".into());
                let r = run(&["sudo", "-n", &d, "--time-format", "iso"], Opts::timeout_ms(10000)).await;
                if r.code != 0 {
                    return Err(HttpError::new(500, if r.err.is_empty() { "dmesg failed".to_string() } else { r.err }));
                }
                let lines: Vec<model::KernelLine> = r.out.split('\n').filter_map(model::filter_kernel_line).collect();
                let start = lines.len().saturating_sub(300);
                json(j!({ "lines": lines[start..] }))
            },
        ),
        route(i, Method::POST, "/api/usb/rescan", j!("Re-read sysfs and uhubctl now"), |i, _| async move {
            i.rescan().await;
            {
                let _g = i.uhub_lock.lock().await;
                i.refresh_hubs().await;
            }
            i.rebuild();
            i.changed();
            json(j!({ "ok": true }))
        }),
        route(
            i,
            Method::GET,
            "/api/usb/power/plan",
            j!({ "query": { "hub": "1-1", "port": "2 (optional)", "busid": "or the device to switch", "action": "on|off|cycle" }, "description": "Dry run: which ports and devices a power action affects, warnings, and the confirm level it needs (none|ganged|unsafe)" }),
            |i, r| async move {
                let p = i.plan(&query_obj(&r)).await?;
                json(serde_json::to_value(p).unwrap_or_default())
            },
        ),
        route(
            i,
            Method::POST,
            "/api/usb/power",
            j!({ "body": { "hub": "1-1", "port": 2, "action": "on|off|cycle", "delay": 2, "confirm": "ganged|unsafe (when the plan needs it)" }, "description": "Switch port power with uhubctl. Answers 409 with the plan when confirmation is needed. busid instead of hub/port switches the port a device is on." }),
            |i, r| async move {
                let b = r.json()?;
                match i.power(&b).await {
                    Ok((plan, output)) => json(j!({ "ok": true, "plan": plan, "output": output })),
                    Err(PowerErr::Http(e)) => Err(e),
                    Err(PowerErr::Needs(p)) => Ok(Reply::Response(json_response(
                        409,
                        &j!({ "ok": false, "message": needs_message(&p), "needs": p.needs, "plan": p }),
                    ))),
                }
            },
        ),
        route(
            i,
            Method::POST,
            "/api/usb/share",
            j!({ "body": busid_param, "description": "Share a device over USB/IP (binds it to usbip-host and runs usbipd). It disappears from this machine's drivers while shared." }),
            |i, r| async move {
                let b = r.json()?;
                json(i.share(&arg_str(b.get("busid")), false).await?)
            },
        ),
        route(
            i,
            Method::POST,
            "/api/usb/unshare",
            j!({ "body": busid_param, "description": "Stop sharing a device over USB/IP (a connected client is dropped); local drivers take it back" }),
            |i, r| async move {
                let b = r.json()?;
                json(i.unshare(&arg_str(b.get("busid"))).await?)
            },
        ),
        route(i, Method::GET, "/api/usb/rules", j!("Auto-share rules"), |i, _| async move {
            json(Value::Array(i.rules().iter().map(Rule::json).collect()))
        }),
        route(
            i,
            Method::POST,
            "/api/usb/rules",
            j!({ "body": { "match": "vidpid|port", "value": "0403:6001 or 1-1.2" }, "description": "Add an auto-share rule: matching devices are shared over USB/IP when plugged in (and now, if present)" }),
            |i, r| async move {
                let rule = validate_rule(&r.json()?)?;
                let mut rules = i.rules();
                if !rules.contains(&rule) {
                    rules.push(rule);
                    i.set_rules(&rules);
                }
                i.rebuild();
                i.apply_rules(true);
                i.changed();
                json(j!({ "ok": true, "rules": i.rules().iter().map(Rule::json).collect::<Vec<_>>() }))
            },
        ),
        route(
            i,
            Method::DELETE,
            "/api/usb/rules",
            j!({ "body": { "match": "vidpid|port", "value": "0403:6001" }, "description": "Remove an auto-share rule (devices already shared stay shared)" }),
            |i, r| async move {
                let rule = validate_rule(&r.json()?)?;
                let rules: Vec<Rule> = i.rules().into_iter().filter(|x| *x != rule).collect();
                i.set_rules(&rules);
                i.rebuild();
                i.changed();
                json(j!({ "ok": true, "rules": i.rules().iter().map(Rule::json).collect::<Vec<_>>() }))
            },
        ),
        route(
            i,
            Method::POST,
            "/api/usb/redir/start",
            j!({ "body": busid_param, "description": "Serve a device with usbredirect (for QEMU/libvirt usb-redir). Exclusive with USB/IP." }),
            |i, r| async move {
                let b = r.json()?;
                json(i.start_redir(&arg_str(b.get("busid")))?)
            },
        ),
        route(
            i,
            Method::POST,
            "/api/usb/redir/stop",
            j!({ "body": busid_param, "description": "Stop a usbredir session" }),
            |i, r| async move {
                let b = arg_str(r.json()?.get("busid"));
                if !i.stop_redir(&b, "stopped") {
                    return Err(HttpError::new(409, format!("No usbredir session for {b}")));
                }
                i.rebuild();
                i.changed();
                json(j!({ "ok": true }))
            },
        ),
        route(
            i,
            Method::POST,
            "/api/usb/config",
            j!({ "body": { "usbipPort": 3240, "redirBasePort": 4200 }, "description": "Listener ports for usbipd and the first usbredir session (BENCHBOB_PORT_OFFSET is added at runtime)" }),
            |i, r| async move {
                let b = r.json()?;
                for k in ["usbipPort", "redirBasePort"] {
                    let Some(v) = b.get(k) else { continue };
                    let n = benchbob_core::http::js_number(v);
                    if !(n.is_finite() && n.fract() == 0.0 && (1.0..=65535.0).contains(&n)) {
                        return Err(HttpError::bad_request(format!("{k} must be a TCP port")));
                    }
                    i.ctx().set_setting(k, j!(n as i64));
                }
                let restart = i.st.lock().unwrap().usbipd.as_ref().is_some_and(|u| u.port != i.usbip_port());
                if restart {
                    i.stop_usbipd("port changed");
                    i.reconcile_usbipd();
                }
                i.changed();
                json(j!({ "ok": true, "usbipPort": i.usbip_port(), "redirBasePort": i.redir_base() }))
            },
        ),
    ]
}

pub(crate) fn tools(i: &Arc<Inner>) -> Vec<Tool> {
    vec![
        tool(
            i,
            "usb_list",
            "List every USB controller, hub and device on BenchBob as a flattened depth-first tree: busid (also the physical port path), VID:PID and names, speed, power draw, each interface's class and bound kernel driver (or 'no driver'), the device nodes it created (/dev/ttyACM0, can0, /dev/sda…) and which BenchBob panel handles them, USB/IP or usbredir sharing state, and protection notes.",
            j!({ "type": "object", "properties": {} }),
            |i, _| async move { Ok(i.flat_list()) },
        ),
        tool(
            i,
            "usb_timeline",
            "Recent hardware events, newest first: USB plug/unplug (with name, speed, port), USB-related kernel messages (enumeration errors, over-current, resets, driver binds), port power changes and sharing changes.",
            j!({ "type": "object", "properties": { "limit": { "type": "integer", "minimum": 1, "maximum": 300, "default": 50 }, "kinds": { "type": "array", "items": { "enum": ["plug", "unplug", "kernel", "power", "share"] }, "description": "Only these event kinds" } } }),
            |i, a| async move {
                let n = a.get("limit").map(benchbob_core::http::js_number).filter(|n| *n > 0.0).unwrap_or(50.0) as usize;
                let kinds: Option<Vec<String>> =
                    a.get("kinds").and_then(Value::as_array).map(|k| k.iter().map(benchbob_core::http::js_string).collect());
                Ok(i.timeline_entries(n, kinds.as_deref()))
            },
        ),
        tool(
            i,
            "usb_power",
            "Switch USB port power with uhubctl: on, off, or cycle (off, wait delay seconds, on; power-cycling is the usual fix for a wedged device). Target a device by busid (switches the port it's plugged into) or a hub location plus port. Some hubs (all Raspberry Pi onboard ports) are ganged: one switch cuts every port. Those calls fail with an explanation until repeated with confirm:'ganged'. Ones that would cut power to the root filesystem, a served drive, the management NIC or an internal hub link need confirm:'unsafe'. Check usb_list first.",
            j!({
                "type": "object",
                "properties": {
                    "busid": { "type": "string", "description": "Device whose port to switch, e.g. 1-1.2" },
                    "hub": { "type": "string", "description": "uhubctl hub location, e.g. 1-1 or 2 (root hub of bus 2)" },
                    "port": { "type": "integer", "minimum": 1, "description": "Port on that hub; omit for every port" },
                    "action": { "enum": ["on", "off", "cycle"] },
                    "delay": { "type": "number", "minimum": 1, "maximum": 30, "default": 2, "description": "Seconds off during cycle" },
                    "confirm": { "enum": ["ganged", "unsafe"], "description": "Only after reading the warning from a previous attempt" }
                },
                "required": ["action"]
            }),
            |i, a| async move {
                match i.power(&a).await {
                    Ok((p, _)) => {
                        Ok(j!({ "ok": true, "action": p.action, "ports": p.ports, "devicesAffected": p.devices, "warnings": p.warnings }))
                    }
                    Err(PowerErr::Http(e)) => Err(e),
                    Err(PowerErr::Needs(p)) => Err(HttpError::new(409, needs_message(&p))),
                }
            },
        ),
        tool(
            i,
            "usb_share",
            "Share one USB device over the network with USB/IP so another computer can attach it as if it were plugged in locally. While shared the device is detached from BenchBob's own drivers. Returns the client commands (Linux: usbip attach; Windows: usbip-win2). Hubs, the root-filesystem device, served drives and the management NIC are refused.",
            j!({ "type": "object", "properties": { "busid": { "type": "string", "description": "e.g. 1-1.2 (from usb_list)" } }, "required": ["busid"] }),
            |i, a| async move { i.share(&arg_str(a.get("busid")), false).await },
        ),
        tool(
            i,
            "usb_unshare",
            "Stop sharing a USB device over USB/IP; any attached remote client is disconnected and BenchBob's local drivers take the device back.",
            j!({ "type": "object", "properties": { "busid": { "type": "string" } }, "required": ["busid"] }),
            |i, a| async move { i.unshare(&arg_str(a.get("busid"))).await },
        ),
    ]
}
