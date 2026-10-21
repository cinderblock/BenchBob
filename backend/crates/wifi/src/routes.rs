//! The REST routes and MCP tools, as wifi.ts declares them.

use std::sync::Arc;

use axum::http::Method;
use benchbob_core::http::{HttpError, json};
use benchbob_core::{Request, Route, RouteResult, Tool};
use serde_json::{Map, Value, json as j};

use crate::json::{O, js_number, js_slice, js_string, now_ms, num, spread, truthy};
use crate::{Inner, ScanFilter, WATCH_LEASE_MS, mask_profile, result_summary};

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

/// `"x" in methods ? x : …` for scan methods.
fn is_method(v: &Value) -> bool {
    matches!(v.as_str(), Some("auto" | "iw" | "nm"))
}

/// A string argument, `undefined` (None) for a missing or empty one.
fn arg(v: &Value, k: &str) -> Option<String> {
    v.get(k).filter(|x| truthy(x)).map(js_string)
}

pub(crate) fn test_schema() -> Value {
    j!({
        "type": "object",
        "properties": {
            "radio": { "type": "string", "description": "phy (e.g. phy0) or interface (wlan0); default: the first radio" },
            "ssid": { "type": "string", "description": "Network name" },
            "security": { "type": "string", "enum": ["open", "owe", "psk", "sae", "psk-sae", "eap"], "description": "Default: what the last scan saw for this SSID" },
            "psk": { "type": "string", "description": "Passphrase (8–63 chars) or 64-hex PSK. Never echoed back." },
            "useSavedKey": { "type": "boolean", "description": "Use the key NetworkManager has saved for this SSID (stays on BenchBob)" },
            "profile": { "type": "string", "description": "Saved test profile id or name to start from" },
            "bssid": { "type": "string", "description": "Pin to one AP" },
            "eap": { "type": "object", "properties": { "method": { "type": "string", "enum": ["PEAP", "TTLS"] }, "identity": { "type": "string" }, "anonymousIdentity": { "type": "string" } }, "description": "Enterprise (MSCHAPv2 inner; server certificate NOT validated). Password goes in psk." },
            "ip": { "type": "string", "enum": ["dhcp", "static", "none"], "description": "Default dhcp; none = association only" },
            "static": { "type": "object", "properties": { "address": { "type": "string" }, "gateway": { "type": "string" }, "dns": { "type": "array", "items": { "type": "string" } } } },
            "pingGateway": { "type": "boolean", "description": "Default true" },
            "dns": { "type": "array", "items": { "type": "string" }, "description": "Names to resolve (default [example.com])" },
            "http": { "type": "array", "items": { "type": "string" }, "description": "URLs to fetch (default [https://example.com/])" },
            "ping": { "type": "array", "items": { "type": "string" }, "description": "Extra hosts to ping" },
            "iperf": { "type": "object", "properties": { "server": { "type": "string" }, "port": { "type": "number" }, "seconds": { "type": "number" }, "reverse": { "type": "boolean" } } },
            "count": { "type": "number", "description": "Pings per target (default 5)" },
            "holdS": { "type": "number", "description": "Stay connected this many seconds afterwards, sampling RSSI/bitrate" },
            "assocTimeoutS": { "type": "number", "description": "Give up associating after this long (default 30)" },
            "saveAs": { "type": "string", "description": "Also save these settings as a named profile" }
        },
        "required": ["ssid"]
    })
}

/// `Math.min(hi, Math.max(lo, +x || d))`
fn clamp_or(v: Option<&Value>, d: f64, lo: f64, hi: f64) -> f64 {
    let x = v.map(js_number).unwrap_or(f64::NAN);
    (if x.is_nan() || x == 0.0 { d } else { x }).max(lo).min(hi)
}

fn find_result(i: &Inner, id: &str) -> Option<Value> {
    i.st.lock().unwrap().results.iter().find(|x| x.get("id").and_then(Value::as_str) == Some(id)).cloned()
}

pub(crate) fn routes(i: &Arc<Inner>) -> Vec<Route> {
    vec![
        route(
            i,
            Method::GET,
            "/api/wifi",
            j!("Everything at once: status, radios (full), regulatory domain, profiles (keys masked)"),
            |i, _| async move {
                let radios = i.st.lock().unwrap().radios_json();
                let profiles: Vec<Value> = i.profiles().iter().map(mask_profile).collect();
                json(spread(spread(i.status(), radios), j!({ "profiles": profiles })))
            },
        ),
        route(
            i,
            Method::GET,
            "/api/wifi/radios",
            j!(
                "Radios: driver, onboard/USB, bands and channels (disabled / DFS / max power), iftypes, current use, NM state. ?refresh=1 re-reads iw phy."
            ),
            |i, r| async move {
                if r.query("refresh").is_some_and(|v| !v.is_empty()) {
                    i.refresh_radios(true).await;
                }
                json(i.st.lock().unwrap().radios_json())
            },
        ),
        route(
            i,
            Method::PUT,
            "/api/wifi/radios/:phy",
            j!({ "body": { "label": "Alfa AWUS036ACM", "keepUnmanaged": true, "scanMethod": "auto|iw|nm" }, "description": "Per-radio preferences, stored by stable id (USB serial/port or onboard path)" }),
            |i, r| async move {
                let phy = i.st.lock().unwrap().find_radio(Some(r.param("phy")))?;
                let b = r.json()?;
                let (stable, mut cur) = {
                    let st = i.st.lock().unwrap();
                    let x = &st.radios[&phy];
                    (x.stable_id.clone(), x.prefs.clone())
                };
                if let Some(l) = b.get("label") {
                    cur.insert("label".into(), Value::from(js_slice(&js_string(l), 60)));
                }
                if let Some(k) = b.get("keepUnmanaged") {
                    cur.insert("keepUnmanaged".into(), Value::Bool(truthy(k)));
                }
                if let Some(m) = b.get("scanMethod") {
                    if !is_method(m) {
                        return Err(HttpError::bad_request("scanMethod must be auto, iw or nm"));
                    }
                    cur.insert("scanMethod".into(), m.clone());
                }
                let mut all = i.prefs_all();
                all.insert(stable, Value::Object(cur.clone()));
                i.set_setting("radioPrefs", Value::Object(all));
                if let Some(x) = i.st.lock().unwrap().radios.get_mut(&phy) {
                    x.prefs = cur.clone();
                }
                if cur.get("keepUnmanaged").is_some_and(truthy) {
                    i.unmanage(&phy).await?;
                }
                i.st.lock().unwrap().radios_seq += 1;
                i.changed();
                json(j!({ "ok": true, "prefs": cur }))
            },
        ),
        route(
            i,
            Method::POST,
            "/api/wifi/radios/:phy/managed",
            j!({ "body": { "managed": false }, "description": "Runtime NetworkManager control for a radio's interfaces (nmcli dev set … managed yes|no). Refuses to unmanage the connection NM is using." }),
            |i, r| async move {
                let phy = i.st.lock().unwrap().find_radio(Some(r.param("phy")))?;
                let managed = r.json()?.get("managed").is_some_and(truthy);
                let (busy, nm) = {
                    let st = i.st.lock().unwrap();
                    let x = &st.radios[&phy];
                    (x.busy, x.nm.clone())
                };
                if busy == Some("test") {
                    return Err(HttpError::new(409, format!("{phy} is in a client test")));
                }
                if managed {
                    for n in &nm {
                        let x = crate::client::sudo(&i.bins, &[&i.bins.nmcli, "dev", "set", &n.iface, "managed", "yes"], 20000).await;
                        if x.code != 0 {
                            return Err(HttpError::new(500, x.err));
                        }
                    }
                    if let Some(x) = i.st.lock().unwrap().radios.get_mut(&phy) {
                        x.unmanaged_by_us.clear();
                    }
                } else {
                    if nm.iter().any(|n| n.state == "connected") {
                        return Err(HttpError::new(409, "NetworkManager is using this radio; disconnect it first"));
                    }
                    i.unmanage(&phy).await?;
                }
                i.refresh_radios(false).await;
                let st = i.st.lock().unwrap();
                let radio = st.radios.get(&phy).map(|x| st.radio_json(x, false)).unwrap_or(Value::Null);
                json(j!({ "ok": true, "radio": radio }))
            },
        ),
        route(
            i,
            Method::GET,
            "/api/wifi/reg",
            j!("Regulatory domain: global country + rules, and any per-phy (self-managed) domains"),
            |i, _| async move { json(i.st.lock().unwrap().reg_json()) },
        ),
        route(
            i,
            Method::POST,
            "/api/wifi/reg",
            j!({ "body": { "country": "US" }, "description": "Temporarily set the regulatory country (iw reg set; reverts on reboot). Self-managed radios may ignore it." }),
            |i, r| async move {
                let b = r.json()?;
                json(i.set_reg(b.get("country")).await?)
            },
        ),
        route(
            i,
            Method::POST,
            "/api/wifi/scan",
            j!({ "body": { "radio": "phy0", "method": "auto|iw|nm" }, "description": "Scan now and return every BSS seen (with history). auto = `iw scan`, retried on EBUSY, falling back to an NM rescan + `iw scan dump`. Never disturbs an existing connection." }),
            |i, r| async move {
                let b = r.json()?;
                json(i.do_scan(arg(&b, "radio"), arg(&b, "method")).await?)
            },
        ),
        route(
            i,
            Method::GET,
            "/api/wifi/scan",
            j!(
                "Accumulated scan results (?ssid=substring&band=2.4|5|6&limit=N): per BSS SSID, BSSID, channel/width/centre, signal + history, security, standards, BSS load, country, WPS; plus survey data. ?historySince=<ms> (a previous answer's `now`) returns only newer history samples, for merging into a copy you already have"
            ),
            |i, r| async move {
                let n =
                    |k: &str| r.query(k).filter(|v| !v.is_empty()).map(|v| js_number(&Value::from(v))).filter(|v| *v != 0.0 && !v.is_nan());
                let f = ScanFilter {
                    ssid: r.query("ssid").filter(|v| !v.is_empty()).map(String::from),
                    band: r.query("band").filter(|v| !v.is_empty()).map(String::from),
                    limit: n("limit"),
                    history_since: n("historySince"),
                };
                json(Inner::scan_json(&mut i.st.lock().unwrap(), &f))
            },
        ),
        route(i, Method::DELETE, "/api/wifi/scan", j!("Forget all scan history"), |i, _| async move {
            {
                let mut st = i.st.lock().unwrap();
                st.bss.clear();
                st.scan.seq += 1;
                st.scan.count = 0;
            }
            i.changed();
            json(j!({ "ok": true }))
        }),
        route(
            i,
            Method::POST,
            "/api/wifi/watch",
            j!({ "body": { "id": "any client id" }, "description": "Lease: \"I'm looking at the scanner\". While any lease from the last 60 s is live, BenchBob scans every watchIntervalS (default 30 s), shared by all viewers. Renew every ~20 s; the first watch scans at once if the last scan is over 20 s old" }),
            |i, r| async move {
                let b = r.json()?;
                let id = js_slice(&b.get("id").filter(|v| truthy(v)).map(js_string).unwrap_or_else(|| "anon".into()), 64);
                let (fresh, n, stale) = {
                    let mut st = i.st.lock().unwrap();
                    let now = now_ms();
                    let fresh = !st.watching(now);
                    st.watchers.insert(id, now + WATCH_LEASE_MS);
                    (fresh, st.watchers.len(), now.saturating_sub(st.last_auto_scan.max(st.scan.at)) > 20_000)
                };
                if fresh {
                    i.changed();
                }
                if stale {
                    i.tick_scans(true);
                }
                let interval = i.auto_scan().get("watchIntervalS").cloned().unwrap_or(Value::Null);
                json(j!({ "ok": true, "watchers": n, "intervalS": interval, "leaseMs": WATCH_LEASE_MS }))
            },
        ),
        route(
            i,
            Method::DELETE,
            "/api/wifi/watch",
            j!({ "body": { "id": "the id used for POST" }, "description": "End a watch lease early (panel closed)" }),
            |i, r| async move {
                let b = r.json()?;
                let id = b.get("id").filter(|v| truthy(v)).map(js_string).unwrap_or_else(|| "anon".into());
                let n = {
                    let mut st = i.st.lock().unwrap();
                    st.watchers.shift_remove(&id);
                    if st.watching(now_ms()) { st.watchers.len() } else { 0 }
                };
                i.changed();
                json(j!({ "ok": true, "watchers": n }))
            },
        ),
        route(
            i,
            Method::PUT,
            "/api/wifi/autoscan",
            j!({ "body": { "enabled": true, "intervalS": 60, "radio": "phy0", "method": "auto", "watchIntervalS": 30 }, "description": "Periodic background scanning (off by default; minimum interval 15 s). watchIntervalS: how often to scan while someone has the panel open (POST /api/wifi/watch)" }),
            |i, r| async move {
                let b = r.json()?;
                let mut a = i.auto_scan();
                if let Some(v) = b.get("enabled") {
                    a.insert("enabled".into(), Value::Bool(truthy(v)));
                }
                if let Some(v) = b.get("intervalS") {
                    a.insert("intervalS".into(), num(clamp_or(Some(v), 60.0, 15.0, 3600.0)));
                }
                if let Some(v) = b.get("radio") {
                    let radio = if truthy(v) { i.st.lock().unwrap().find_radio(Some(&js_string(v)))? } else { String::new() };
                    a.insert("radio".into(), Value::from(radio));
                }
                if let Some(v) = b.get("method") {
                    a.insert("method".into(), if is_method(v) { v.clone() } else { Value::from("auto") });
                }
                if let Some(v) = b.get("watchIntervalS") {
                    a.insert("watchIntervalS".into(), num(clamp_or(Some(v), 30.0, 15.0, 3600.0)));
                }
                i.set_setting("autoScan", Value::Object(a.clone()));
                i.st.lock().unwrap().last_auto_scan = 0;
                i.changed();
                json(j!({ "ok": true, "autoScan": a }))
            },
        ),
        route(
            i,
            Method::POST,
            "/api/wifi/test",
            j!({ "body": test_schema()["properties"].clone(), "description": "Start a client test in an isolated netns. Returns {id} at once; ?wait=1 returns the finished result. Refuses (409) to borrow a radio that carries the primary default route." }),
            |i, r| async move {
                let t = i.start_test(&r.json()?).await?;
                if r.query("wait").is_some_and(|v| !v.is_empty()) {
                    t.wait_done().await;
                    return json(t.with_r(|r| r.json()));
                }
                let (id, phy, ns) = t.with_r(|r| (r.id.clone(), r.phy.clone(), r.ns.clone()));
                json(j!({ "ok": true, "id": id, "phy": phy, "ns": ns }))
            },
        ),
        route(
            i,
            Method::GET,
            "/api/wifi/test/:id",
            j!("A running or finished test: stage, timeline, link, lease, tests, RSSI/bitrate samples"),
            |i, r| async move {
                let id = r.param("id").to_string();
                let t = i.st.lock().unwrap().tests.get(&id).cloned();
                if let Some(t) = t {
                    return json(spread(t.with_r(|r| r.json()), j!({ "running": true })));
                }
                find_result(&i, &id).map(benchbob_core::http::Reply::Json).ok_or_else(|| HttpError::new(404, "No such test"))
            },
        ),
        route(
            i,
            Method::POST,
            "/api/wifi/test/:id/cancel",
            j!("Cancel a running test (cleanup and NM restore still run)"),
            |i, r| async move {
                let t = i.st.lock().unwrap().tests.get(r.param("id")).cloned();
                let Some(t) = t else { return Err(HttpError::new(404, "No running test with that id")) };
                t.cancel();
                json(j!({ "ok": true }))
            },
        ),
        route(i, Method::GET, "/api/wifi/results", j!("The last 20 client-test results (full timelines)"), |i, _| async move {
            let results = i.st.lock().unwrap().results.clone();
            json(j!({ "results": results }))
        }),
        route(i, Method::GET, "/api/wifi/results/:id", j!("One client-test result"), |i, r| async move {
            find_result(&i, r.param("id")).map(benchbob_core::http::Reply::Json).ok_or_else(|| HttpError::new(404, "No such result"))
        }),
        route(i, Method::DELETE, "/api/wifi/results", j!("Clear stored results"), |i, _| async move {
            {
                let mut st = i.st.lock().unwrap();
                st.results.clear();
                st.results_seq += 1;
            }
            i.set_setting("results", Value::Array(vec![]));
            i.changed();
            json(j!({ "ok": true }))
        }),
        route(i, Method::GET, "/api/wifi/profiles", j!("Saved test profiles (keys masked: hasPsk)"), |i, _| async move {
            json(j!({ "profiles": i.profiles().iter().map(mask_profile).collect::<Vec<_>>() }))
        }),
        route(
            i,
            Method::POST,
            "/api/wifi/profiles",
            j!({ "body": { "id": "(optional, to edit)", "name": "Lab AP", "ssid": "LabNet", "security": "psk", "psk": "(omit to keep the stored key; null to clear)", "useSavedKey": false, "dns": ["example.com"], "http": ["https://example.com/"] }, "description": "Create or update a test profile" }),
            |i, r| async move {
                let b = r.json()?;
                if !b.get("name").is_some_and(truthy) || !b.get("ssid").is_some_and(truthy) {
                    return Err(HttpError::bad_request("name and ssid are required"));
                }
                if let Some(psk) = b.get("psk").filter(|v| truthy(v)) {
                    let sec = b.get("security").filter(|v| truthy(v)).map(js_string).unwrap_or_else(|| "psk".into());
                    if let Some(e) = crate::client::valid_key(&sec, Some(&js_string(psk))) {
                        return Err(HttpError::bad_request(e));
                    }
                }
                const ALLOWED: [&str; 17] = [
                    "id",
                    "name",
                    "ssid",
                    "security",
                    "psk",
                    "useSavedKey",
                    "eap",
                    "bssid",
                    "ip",
                    "static",
                    "pingGateway",
                    "dns",
                    "http",
                    "ping",
                    "iperf",
                    "count",
                    "holdS",
                ];
                let entries: Vec<(&str, Option<Value>)> = b
                    .as_object()
                    .map(|m| m.iter().filter_map(|(k, v)| ALLOWED.iter().find(|a| **a == k).map(|a| (*a, Some(v.clone())))).collect())
                    .unwrap_or_default();
                let p = i.save_profile(entries);
                json(j!({ "ok": true, "profile": mask_profile(&p) }))
            },
        ),
        route(i, Method::DELETE, "/api/wifi/profiles/:id", j!("Delete a test profile"), |i, r| async move {
            let id = r.param("id").to_string();
            let left: Vec<Value> = i.profiles().into_iter().filter(|x| x.get("id").and_then(Value::as_str) != Some(id.as_str())).collect();
            i.set_setting("profiles", Value::Array(left));
            i.changed();
            json(j!({ "ok": true }))
        }),
    ]
}

pub(crate) fn tools(i: &Arc<Inner>) -> Vec<Tool> {
    vec![
        tool(
            i,
            "wifi_radios",
            "List BenchBob's Wi-Fi radios: driver, onboard or USB, supported bands, enabled/disabled/DFS channels with max power, interface modes (monitor/AP), what each is doing now (e.g. backup uplink), NetworkManager state, and the regulatory domain.",
            j!({ "type": "object", "properties": { "channels": { "type": "boolean", "description": "Include the per-channel list (long). Default false." } } }),
            |i, a| async move {
                i.refresh_radios(false).await;
                let mut j = i.st.lock().unwrap().radios_json();
                if !a.get("channels").is_some_and(truthy)
                    && let Some(radios) = j.get_mut("radios").and_then(Value::as_array_mut)
                {
                    for r in radios {
                        let Some(bands) = r.get_mut("bandInfo").and_then(Value::as_array_mut) else { continue };
                        for b in bands {
                            let Some(m) = b.as_object_mut() else { continue };
                            let ch = m.shift_remove("channels").and_then(|c| c.as_array().cloned()).unwrap_or_default();
                            let pick = |f: &dyn Fn(&Value) -> bool| ch.iter().filter(|c| f(c)).map(|c| c["ch"].clone()).collect::<Vec<_>>();
                            let off = |c: &Value| c.get("disabled").is_some_and(truthy);
                            m.insert("enabled".into(), Value::Array(pick(&|c| !off(c))));
                            m.insert("dfs".into(), Value::Array(pick(&|c| c.get("radar").is_some_and(truthy) && !off(c))));
                        }
                    }
                }
                Ok(j)
            },
        ),
        tool(
            i,
            "wifi_scan",
            "Scan for Wi-Fi networks (BSSes) and return them strongest first: SSID (hidden flagged), BSSID, band, channel, width and centre channel, signal dBm (+ recent history), security (Open/OWE/WEP/WPA2-PSK/WPA3-SAE/transition/Enterprise), 802.11 standards, station count and channel utilisation, country, WPS. Doesn't disturb existing connections.",
            j!({
                "type": "object",
                "properties": {
                    "radio": { "type": "string", "description": "phy or interface; default the first radio" },
                    "fresh": { "type": "boolean", "description": "Run a new scan first (default true); false returns accumulated results" },
                    "ssid": { "type": "string", "description": "Only BSSes whose SSID contains this" },
                    "band": { "type": "string", "enum": ["2.4", "5", "6"] },
                    "limit": { "type": "number", "description": "Max BSSes (default 50)" }
                }
            }),
            |i, a| async move {
                if a.get("fresh") != Some(&Value::Bool(false)) {
                    i.do_scan(arg(&a, "radio"), None).await?;
                }
                let lim = a.get("limit").filter(|v| truthy(v)).map(js_number).unwrap_or(50.0);
                let f = ScanFilter { ssid: arg(&a, "ssid"), band: arg(&a, "band"), limit: Some(lim), history_since: None };
                let j = Inner::scan_json(&mut i.st.lock().unwrap(), &f);
                let bss: Vec<Value> = j["bss"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|b| {
                        let mut m = b.as_object().cloned().unwrap_or_default();
                        let h = m.shift_remove("history").and_then(|h| h.as_array().cloned()).unwrap_or_default();
                        let n = h.len().saturating_sub(10);
                        m.insert("history".into(), Value::Array(h[n..].iter().map(|x| x[1].clone()).collect()));
                        Value::Object(m)
                    })
                    .collect();
                Ok(j!({ "scan": j["scan"].clone(), "bss": bss }))
            },
        ),
        tool(
            i,
            "wifi_test",
            "Run a Wi-Fi client test and wait for it: borrow a radio into an isolated network namespace (never affects eth0 or the main routing table), associate to the SSID, optionally DHCP, then ping the gateway, resolve DNS names, fetch URLs, ping hosts and optionally run iperf3. Returns the timeline (association, 4-way handshake, DHCP discover/offer/ack, each test), the lease, RSSI/bitrates, and whether NetworkManager reconnected the radio afterwards. Refuses to borrow a radio that is the primary uplink. Takes ~10–60 s.",
            test_schema(),
            |i, a| async move {
                let t = i.start_test(&a).await?;
                t.wait_done().await;
                Ok(t.with_r(|r| {
                    let mut m: Map<String, Value> = r.json_with(true);
                    m.insert("samples".into(), Value::from(r.samples.len()));
                    let n = r.wpa_log.len().saturating_sub(15);
                    m.insert("wpaLog".into(), Value::from(r.wpa_log[n..].to_vec()));
                    Value::Object(m)
                }))
            },
        ),
        tool(
            i,
            "wifi_results",
            "Recent Wi-Fi client-test results (newest first). Without id: a summary of each of the last 20. With id: that result in full (timeline, lease, link, tests).",
            j!({ "type": "object", "properties": { "id": { "type": "string" }, "limit": { "type": "number" } } }),
            |i, a| async move {
                if let Some(id) = arg(&a, "id") {
                    if let Some(r) = find_result(&i, &id) {
                        return Ok(r);
                    }
                    let t = i.st.lock().unwrap().tests.get(&id).cloned();
                    return t.map(|t| t.with_r(|r| r.json())).ok_or_else(|| HttpError::new(500, "No such result"));
                }
                let (running, results) = {
                    let st = i.st.lock().unwrap();
                    (st.tests.values().cloned().collect::<Vec<_>>(), st.results.clone())
                };
                let lim = a.get("limit").filter(|v| truthy(v)).map(js_number).unwrap_or(20.0);
                Ok(j!({
                    "running": running.iter().map(|t| t.with_r(|r| O::new().k("id", r.id.clone()).k("ssid", r.ssid.clone()).k("stage", r.stage).v())).collect::<Vec<_>>(),
                    "results": crate::json::slice_n(&results, lim).iter().map(result_summary).collect::<Vec<_>>(),
                }))
            },
        ),
    ]
}
