//! The REST routes and MCP tools, as netdiag.ts declares them.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, Method, header};
use axum::response::Response;
use benchbob_core::http::{HttpError, js_number, js_string, json, truthy};
use benchbob_core::{Reply, Request, Route, RouteResult, Tool};
use bytes::Bytes;
use serde_json::{Map, Value, json as j};

use crate::{Inner, parse};

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

/// A JSON answer for work that may take a while (netdiag.ts `slowJson`): headers go out at
/// once and a space every 4 s keeps the connection alive through proxies, then the JSON
/// follows. Leading whitespace is valid JSON, so clients just parse the body. A failure
/// becomes `{ ok: false, message }` (the status is already 200 by then). The work carries
/// on if the client goes away.
pub(crate) fn slow_json<F>(work: F) -> Response
where
    F: std::future::Future<Output = Result<Value, HttpError>> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
    tokio::spawn(async move {
        let mut work = tokio::spawn(work);
        let mut iv = tokio::time::interval(Duration::from_secs(4));
        iv.tick().await;
        loop {
            tokio::select! {
                r = &mut work => {
                    let v = match r {
                        Ok(Ok(Value::Null)) => j!({ "ok": true }),
                        Ok(Ok(v)) => v,
                        Ok(Err(e)) => j!({ "ok": false, "message": e.message }),
                        Err(e) => j!({ "ok": false, "message": e.to_string() }),
                    };
                    let _ = tx.send(Bytes::from(serde_json::to_vec(&v).unwrap_or_default())).await;
                    return;
                }
                _ = iv.tick() => {
                    if tx.send(Bytes::from_static(b" ")).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (Ok::<_, std::io::Error>(b), rx)) });
    let mut r = Response::new(Body::from_stream(stream));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // The keep-alive spaces must flow, so the server must not buffer this to gzip it.
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    r
}

fn slow<F>(work: F) -> RouteResult
where
    F: std::future::Future<Output = Result<Value, HttpError>> + Send + 'static,
{
    Ok(Reply::Response(slow_json(work)))
}

fn run_404(id: &str) -> HttpError {
    HttpError::new(404, format!("No run \"{id}\""))
}

pub(crate) fn routes(i: &Arc<Inner>) -> Vec<Route> {
    vec![
        route(
            i,
            Method::GET,
            "/api/netdiag",
            j!(
                "Everything at once: wired interfaces with link details, LLDP neighbours, iperf3 server/client state, last quick-check results"
            ),
            |i, _| async move { json(i.status()) },
        ),
        route(
            i,
            Method::GET,
            "/api/netdiag/link",
            j!(
                "Wired interfaces (eth0 + USB adapters): speed/duplex/autoneg, link-partner modes, EEE, non-zero error counters (with change since BenchBob started), driver/firmware, carrier changes, IPs, gateway, DNS. ?refresh=1 re-runs ethtool first"
            ),
            |i, r| async move {
                if r.query("refresh").is_some_and(|v| !v.is_empty()) {
                    i.poll_all().await;
                }
                json(i.link_view(None)?)
            },
        ),
        route(i, Method::GET, "/api/netdiag/link/:iface/stats", j!("All ethtool -S counters for one interface"), |i, r| async move {
            json(i.iface_stats(r.param("iface"))?)
        }),
        route(
            i,
            Method::GET,
            "/api/netdiag/neighbors",
            j!(
                "LLDP/CDP/FDP/EDP/SONMP neighbours heard on wired ports: switch name/description, chassis id, port id/description, VLANs, PoE, management IPs, capabilities, TTL/age. Receive-only: BenchBob never advertises itself"
            ),
            |i, _| async move {
                i.lldp_query().await;
                json(i.neighbors_view())
            },
        ),
        route(
            i,
            Method::POST,
            "/api/netdiag/lldp",
            j!({ "body": { "enabled": true }, "description": "Listen for switch info (receive-only lldpd). Default on; remembered" }),
            |i, r| async move {
                let on = r.json()?.get("enabled").is_some_and(truthy);
                i.ctx().set_setting("lldp", j!(on));
                if on {
                    i.lldp_start().await;
                } else {
                    i.lldp_stop().await;
                }
                json(j!({ "ok": true, "enabled": on }))
            },
        ),
        route(
            i,
            Method::POST,
            "/api/netdiag/iperf/server",
            j!({ "body": { "enabled": true, "port": 5201, "autoOffMin": 60 }, "description": "Start/stop the iperf3 server (iperf3 -s). It turns itself off after autoOffMin minutes (0 = never). Recent client tests appear in GET /api/netdiag" }),
            |i, r| async move {
                let b = r.json()?;
                if let Some(p) = b.get("port").filter(|v| !v.is_null()) {
                    let port = js_number(p);
                    if !(port.is_finite() && port.fract() == 0.0 && (1024.0..=65535.0).contains(&port)) {
                        return Err(HttpError::bad_request("port must be 1024–65535"));
                    }
                    i.ctx().set_setting("iperfPort", j!(port as i64));
                }
                if let Some(m) = b.get("autoOffMin").filter(|v| !v.is_null()) {
                    let m = js_number(m);
                    if !(0.0..=1440.0).contains(&m) {
                        return Err(HttpError::bad_request("autoOffMin must be 0–1440"));
                    }
                    i.ctx().set_setting("iperfAutoOffMin", benchbob_core::http::num(m));
                }
                let enabled = b.get("enabled");
                let want = enabled.is_some_and(truthy);
                let (running, port) = {
                    let st = i.st.lock().unwrap();
                    (st.server.seq != 0, st.server.port)
                };
                if enabled == Some(&Value::Bool(false)) || (want && running && port != js_number(&i.iperf_port()) as i64) {
                    i.server_stop();
                }
                if want {
                    i.server_start();
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                let st = i.st.lock().unwrap();
                if want && st.server.seq == 0 {
                    let l = &st.server.log;
                    return Err(HttpError::new(500, format!("iperf3 server failed: {}", l[l.len().saturating_sub(2)..].join(" / "))));
                }
                let port = if st.server.port != 0 { j!(st.server.port) } else { i.iperf_port() };
                json(j!({ "ok": true, "running": st.server.seq != 0, "port": port, "offAt": st.server.off_at }))
            },
        ),
        route(
            i,
            Method::POST,
            "/api/netdiag/iperf",
            j!({ "body": { "host": "10.0.0.5", "port": 5201, "duration": 5, "reverse": false, "udp": false, "bitrate": "100M", "parallel": 1, "wait": false }, "description": "Run an iperf3 client test from BenchBob (reverse = server sends). Returns the run id at once; progress and result appear in GET /api/netdiag/iperf/:id. wait=true answers when the test is done" }),
            |i, r| async move {
                let b = r.json()?;
                let id = i.start_run(&b)?;
                if !b.get("wait").is_some_and(truthy) {
                    return json(j!({ "ok": true, "run": i.public_run(&id) }));
                }
                slow(async move {
                    i.wait_run(&id, None).await;
                    let run = i.public_run(&id).unwrap_or(Value::Null);
                    let mut m = Map::new();
                    m.insert("ok".into(), j!(run["status"] == j!("done")));
                    if let Some(e) = run["error"].as_str().filter(|e| !e.is_empty()) {
                        m.insert("message".into(), j!(e));
                    }
                    m.insert("run".into(), run);
                    Ok(Value::Object(m))
                })
            },
        ),
        route(
            i,
            Method::GET,
            "/api/netdiag/iperf/:id",
            j!("One iperf3 client run: per-second throughput points and the final summary"),
            |i, r| async move {
                let id = r.param("id");
                json(i.public_run(id).ok_or_else(|| run_404(id))?)
            },
        ),
        route(i, Method::POST, "/api/netdiag/iperf/:id/stop", j!("Abort a running iperf3 client test"), |i, r| async move {
            let id = r.param("id");
            if !i.stop_run(id) {
                return Err(run_404(id));
            }
            json(j!({ "ok": true }))
        }),
        route(
            i,
            Method::PUT,
            "/api/netdiag/presets",
            j!({ "body": { "iperf": "lab-server.lan Lab server · LAN\n10.0.0.5", "ping": [{ "host": "gateway", "label": "Gateway" }, { "host": "1.1.1.1", "label": "1.1.1.1" }], "reset": false }, "description": "Quick-pick hosts for the iperf3 client and the ping buttons: text (\"host label…\" per line) or a list of {host, label}. In ping targets, host \"gateway\" means the default gateway. reset: true restores the defaults" }),
            |i, r| async move {
                let b = r.json()?;
                let reset = b.get("reset").is_some_and(truthy);
                let list = |v: Vec<parse::Preset>| Value::Array(v.iter().map(parse::Preset::json).collect());
                let pick = |k: &str, gw: bool, def: Vec<parse::Preset>| -> Result<Option<Value>, HttpError> {
                    if reset {
                        return Ok(Some(list(def)));
                    }
                    match b.get(k).filter(|v| !v.is_null()) {
                        Some(v) => parse::presets_from(v, gw).map(|p| Some(list(p))).map_err(HttpError::bad_request),
                        None => Ok(None),
                    }
                };
                let iperf = pick("iperf", false, parse::default_iperf_presets())?;
                let ping = pick("ping", true, parse::default_ping_targets())?;
                if let Some(v) = iperf {
                    i.ctx().set_setting("iperfPresets", v);
                }
                if let Some(v) = ping {
                    i.ctx().set_setting("pingTargets", v);
                }
                i.changed();
                json(j!({ "ok": true, "iperf": i.iperf_presets(), "ping": i.ping_targets() }))
            },
        ),
        route(
            i,
            Method::PUT,
            "/api/netdiag/iperf/auth",
            j!({ "body": { "host": "iperf.example.net", "username": "benchbob", "password": "(or generate: true)", "generate": false, "publicKey": "-----BEGIN PUBLIC KEY-----…", "pkcs1": false }, "description": "Set the login for an iperf3 server that requires one (iperf3 --username/--rsa-public-key-path). Omitted fields keep their value. generate: true makes a new random password on BenchBob; the password is never returned. pkcs1: true for servers running iperf3 3.16 or older (they reject 3.17+'s default OAEP padding with \"authorization failed\"). Answers with usersLine, the `username,sha256` line for the server's --authorized-users-path file" }),
            |i, r| async move {
                let b = r.json()?;
                let host = parse::valid_host_value(b.get("host")).map_err(HttpError::bad_request)?;
                let mut all = i.iperf_auths();
                let mut cur = Map::new();
                cur.insert("username".into(), j!("benchbob"));
                cur.insert("password".into(), j!(""));
                cur.insert("publicKey".into(), j!(""));
                if let Some(Value::Object(old)) = all.get(&host) {
                    for (k, v) in old {
                        cur.insert(k.clone(), v.clone());
                    }
                }
                if let Some(u) = b.get("username").filter(|v| !v.is_null()) {
                    let u = js_string(u);
                    if !parse::valid_iperf_user(&u) {
                        return Err(HttpError::bad_request("username: 1–32 of A–Z a–z 0–9 _ . -"));
                    }
                    cur.insert("username".into(), j!(u));
                }
                if b.get("generate").is_some_and(truthy) {
                    cur.insert("password".into(), j!(parse::base64url(&random_bytes::<24>())));
                } else if let Some(p) = b.get("password").filter(|v| !v.is_null()) {
                    // A comma or newline would break the server's users file; iperf3 also caps
                    // credentials at ~250 bytes.
                    let ok = p.as_str().is_some_and(|s| !s.is_empty() && s.chars().count() <= 128 && !s.contains(['\r', '\n', ',']));
                    if !ok {
                        return Err(HttpError::bad_request("password: 1–128 characters, no commas or newlines"));
                    }
                    cur.insert("password".into(), p.clone());
                }
                if let Some(p) = b.get("pkcs1").filter(|v| !v.is_null()) {
                    cur.insert("pkcs1".into(), j!(truthy(p)));
                }
                if let Some(k) = b.get("publicKey").filter(|v| !v.is_null()) {
                    let key =
                        if truthy(k) { parse::iperf_public_key(&js_string(k)).map_err(HttpError::bad_request)? } else { String::new() };
                    cur.insert("publicKey".into(), j!(key));
                }
                let view = Inner::auth_view(&host, &cur);
                all.insert(host, Value::Object(cur));
                i.ctx().set_setting("iperfAuth", Value::Object(all));
                i.changed();
                let mut m = Map::new();
                m.insert("ok".into(), j!(true));
                m.extend(view);
                json(Value::Object(m))
            },
        ),
        route(
            i,
            Method::DELETE,
            "/api/netdiag/iperf/auth",
            j!({ "body": { "host": "iperf.example.net" }, "description": "Forget the iperf3 login for a host" }),
            |i, r| async move {
                let b = r.json()?;
                let host = b.get("host").filter(|h| truthy(h)).map(js_string).unwrap_or_default();
                let mut all = i.iperf_auths();
                all.shift_remove(&host);
                i.ctx().set_setting("iperfAuth", Value::Object(all));
                i.changed();
                json(j!({ "ok": true }))
            },
        ),
        route(
            i,
            Method::POST,
            "/api/netdiag/ping",
            j!({ "body": { "host": "(default: the gateway; \"gateway\" also works)", "count": 10 }, "description": "Ping (0.2 s apart): loss and min/avg/max/mdev latency. The latest result per host is kept in checks.pings" }),
            |i, r| async move {
                let b = r.json()?;
                let count = b.get("count").filter(|v| !v.is_null()).map(js_number).unwrap_or(10.0);
                slow(async move { i.ping(b.get("host"), count).await })
            },
        ),
        route(
            i,
            Method::POST,
            "/api/netdiag/dns",
            j!({ "body": { "names": ["google.com", "debian.org"] }, "description": "Resolve names against each configured nameserver directly and time each answer" }),
            |i, r| async move {
                let b = r.json()?;
                slow(async move { i.dns_check(b.get("names").and_then(Value::as_array)).await })
            },
        ),
        route(
            i,
            Method::POST,
            "/api/netdiag/http",
            j!({ "body": { "urls": ["https://example.com/"] }, "description": "HTTP(S) reachability: status code and time to first response for each URL (redirects not followed)" }),
            |i, r| async move {
                let b = r.json()?;
                slow(async move { Ok(i.http_check(b.get("urls").and_then(Value::as_array)).await) })
            },
        ),
        route(
            i,
            Method::POST,
            "/api/netdiag/mtu",
            j!({ "body": { "host": "1.1.1.1" }, "description": "Path MTU probe: binary search with don't-fragment pings (takes a few seconds)" }),
            |i, r| async move {
                let b = r.json()?;
                slow(async move { i.mtu_probe(b.get("host")).await })
            },
        ),
    ]
}

/// Random bytes from the kernel.
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    let mut got = 0;
    while got < N {
        // SAFETY: getrandom writes at most the remaining length into the buffer.
        let n = unsafe { libc::getrandom(b[got..].as_mut_ptr().cast(), N - got, 0) };
        if n <= 0 {
            break;
        }
        got += n as usize;
    }
    assert_eq!(got, N, "getrandom failed");
    b
}

pub(crate) fn tools(i: &Arc<Inner>) -> Vec<Tool> {
    vec![
        tool(
            i,
            "net_link",
            "Wired Ethernet link state on BenchBob (eth0 and any USB Ethernet adapters): negotiated speed/duplex, autonegotiation and link-partner modes, EEE, error counters that are non-zero or increasing, driver/firmware, carrier change count, IP addresses, gateway and DNS servers.",
            j!({ "type": "object", "properties": { "iface": { "type": "string", "description": "Only this interface, e.g. eth0" }, "refresh": { "type": "boolean", "description": "Re-run ethtool before answering (default: data up to 15 s old)" } } }),
            |i, a| async move {
                if a.get("refresh").is_some_and(truthy) {
                    i.poll_all().await;
                }
                let only = a.get("iface").filter(|v| truthy(v)).map(js_string);
                i.link_view(only.as_deref())
            },
        ),
        tool(
            i,
            "net_neighbors",
            "Which switch and port BenchBob's wired interfaces are plugged into, learned passively from LLDP/CDP (receive-only; switches send about every 30 s, so it can be empty for a short while after startup). Includes switch name/description, chassis id, port id and description, VLANs, PoE, management IPs, capabilities.",
            j!({ "type": "object", "properties": {} }),
            |i, _| async move {
                i.lldp_query().await;
                Ok(i.neighbors_tool())
            },
        ),
        tool(
            i,
            "net_iperf",
            "Measure throughput between BenchBob and an iperf3 server with iperf3 (TCP by default; reverse=true makes the server send; udp with a bitrate measures loss/jitter). Tests of up to 6 s return the result directly; longer ones return a runId — call again with {runId} to get the result.",
            j!({
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "iperf3 server host or IP" },
                    "port": { "type": "integer", "default": 5201 },
                    "duration": { "type": "number", "default": 5, "minimum": 1, "maximum": 60, "description": "Seconds" },
                    "reverse": { "type": "boolean", "description": "Server sends, BenchBob receives (-R)" },
                    "udp": { "type": "boolean" },
                    "bitrate": { "type": "string", "description": "Target rate for UDP (or a TCP cap), e.g. 100M" },
                    "parallel": { "type": "integer", "default": 1, "minimum": 1, "maximum": 16 },
                    "runId": { "type": "string", "description": "Fetch the result of an earlier run instead of starting one" }
                }
            }),
            |i, a| async move {
                if let Some(id) = a.get("runId").filter(|v| truthy(v)).map(js_string) {
                    return i.public_run(&id).ok_or_else(|| HttpError::new(500, format!("No run \"{id}\"")));
                }
                if !a.get("host").is_some_and(truthy) {
                    return Err(HttpError::new(500, "host is required"));
                }
                let id = i.start_run(&a)?;
                let duration = i.public_run(&id).map(|r| js_number(&r["duration"])).unwrap_or(5.0);
                if duration <= 6.0 {
                    i.wait_run(&id, Some(Duration::from_secs(9))).await;
                    if i.run_status(&id) != "running" {
                        return Ok(i.public_run(&id).unwrap_or(Value::Null));
                    }
                }
                Ok(
                    j!({ "runId": id, "status": "running", "note": format!("Running for {} s; call net_iperf with {{\"runId\":\"{id}\"}} afterwards", js_string(&benchbob_core::http::num(duration))) }),
                )
            },
        ),
        tool(
            i,
            "net_ping",
            "Ping a host from BenchBob (default: its default gateway) and report packet loss and min/avg/max/mdev latency.",
            j!({ "type": "object", "properties": { "host": { "type": "string", "description": "Host or IP; default is the gateway" }, "count": { "type": "integer", "default": 10, "minimum": 1, "maximum": 30 } } }),
            |i, a| async move {
                let c = a.get("count").filter(|v| truthy(v)).map(js_number).unwrap_or(10.0);
                i.ping(a.get("host"), c.min(30.0)).await
            },
        ),
        tool(
            i,
            "net_dns",
            "Resolve hostnames against each of BenchBob's configured DNS servers directly and report the answers and per-server response time.",
            j!({ "type": "object", "properties": { "names": { "type": "array", "items": { "type": "string" }, "description": "Names to resolve (default: google.com, cloudflare.com, debian.org)" } } }),
            |i, a| async move { i.dns_check(a.get("names").and_then(Value::as_array)).await },
        ),
    ]
}
