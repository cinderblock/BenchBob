//! REST routes, actions (search, outputs, resolution, labels, DS2482 binding) and MCP tools
//! (onewire.ts "actions", "routes", "MCP").

use std::sync::Arc;
use std::time::Duration;

use axum::http::Method;
use benchbob_core::http::{HttpError, js_number, js_string, json as reply, truthy};
use benchbob_core::{Request, Route, RouteResult, Tool};
use serde_json::{Value, json};

use crate::families::{decode_ds2406, decode_ds2413};
use crate::{Inner, lock, rd_bin, run_sync, wr};

/// Run blocking driver work (sysfs reads that wait for conversions, sudo) off the runtime.
async fn blocking<T: Send + 'static>(
    i: &Arc<Inner>,
    f: impl FnOnce(&Arc<Inner>) -> Result<T, HttpError> + Send + 'static,
) -> Result<T, HttpError> {
    let i = i.clone();
    tokio::task::spawn_blocking(move || f(&i)).await.map_err(|e| HttpError::new(500, format!("1-Wire worker failed: {e}")))?
}

/// onewire.ts `mustSlave`: the slave id (lower-cased), or 404.
fn must_slave(i: &Inner, id: Option<&Value>) -> Result<String, HttpError> {
    let key = id.filter(|v| truthy(v)).map(js_string).unwrap_or_default().to_lowercase();
    if lock(&i.st).slaves.contains_key(&key) {
        return Ok(key);
    }
    let shown = id.map(js_string).unwrap_or_else(|| "undefined".into());
    Err(HttpError::new(404, format!("No 1-Wire slave \"{shown}\". IDs look like 28-0000075b1a2c; see GET /api/onewire.")))
}

/// onewire.ts `mustMaster`: the named master, or the first.
fn must_master(i: &Inner, id: Option<&Value>) -> Result<(String, i64), HttpError> {
    let st = lock(&i.st);
    let want = id.filter(|v| truthy(v)).map(js_string);
    let m = match &want {
        Some(w) => st.masters.get(w),
        None => st.masters.values().next(),
    };
    match m {
        Some(m) => Ok((m.id.clone(), m.search)),
        None => Err(HttpError::new(
            404,
            match want {
                Some(w) => format!("No 1-Wire master \"{w}\""),
                None => "No 1-Wire master present".into(),
            },
        )),
    }
}

/// Read a slave now and record the result (or the error).
fn read_record(i: &Arc<Inner>, id: &str, eeprom: bool) -> Result<crate::families::Decoded, HttpError> {
    let r = i.read_slave(id, eeprom);
    let mut st = lock(&i.st);
    let s = st.slaves.get_mut(id);
    match r {
        Ok(d) => {
            if let Some(s) = s {
                s.record(d.clone());
            }
            Ok(d)
        }
        Err(e) => {
            if let Some(s) = s {
                s.error = e.clone();
            }
            Err(HttpError::new(500, e))
        }
    }
}

fn slave_view(i: &Inner, id: &str) -> Value {
    let labels = i.labels();
    let st = lock(&i.st);
    st.slaves.get(id).map(|s| i.slave_view(s, &labels)).unwrap_or(Value::Null)
}

fn slave_summary(i: &Inner, id: &str) -> Value {
    let labels = i.labels();
    let st = lock(&i.st);
    st.slaves.get(id).map(|s| i.slave_summary(s, &labels)).unwrap_or(Value::Null)
}

fn search_now(i: &Arc<Inner>, master: &str, search: i64) -> Result<(), HttpError> {
    // Writing w1_master_search wakes the master thread for an immediate search. Writing the
    // current value keeps continuous mode (-1) as it is; when searching is off, do one search.
    wr(&i.root.join(master).join("w1_master_search"), format!("{}\n", if search == 0 { 1 } else { search }).as_bytes())?;
    if !i.fixture {
        std::thread::sleep(Duration::from_millis(1500));
    }
    i.refresh();
    Ok(())
}

fn set_output(i: &Arc<Inner>, id: &str, a: &Value) -> Result<Value, HttpError> {
    let (fam, name) = {
        let st = lock(&i.st);
        let s = &st.slaves[id];
        (s.fam, s.family.name.clone())
    };
    let dir = i.root.join(id);
    let byte: u8 = match fam {
        0x29 => match a.get("value").filter(|v| !v.is_null()) {
            Some(v) => (js_number(v) as i64 & 0xff) as u8,
            None => {
                let cur = rd_bin(&dir.join("output")).and_then(|b| b.first().copied()).unwrap_or(0xff);
                let pin = a.get("pin").map(js_number).unwrap_or(f64::NAN);
                if !(0.0..8.0).contains(&pin) {
                    return Err(HttpError::bad_request("pin must be 0–7"));
                }
                let bit = 1u8 << (pin as u32);
                if a.get("on").is_some_and(truthy) { cur & !bit } else { cur | bit }
            }
        },
        0x3a | 0x12 => {
            let st = rd_bin(&dir.join("state")).and_then(|b| b.first().copied());
            let cur = if fam == 0x3a { decode_ds2413(st.unwrap_or(0xff)) } else { decode_ds2406(st.unwrap_or(0x33)) }.pins.unwrap();
            let mut bits = cur[0].latch.unwrap_or(0) | (cur[1].latch.unwrap_or(0) << 1);
            match a.get("value").filter(|v| !v.is_null()) {
                Some(v) => bits = (js_number(v) as i64 & 3) as u8,
                None => {
                    let pin = a
                        .get("pin")
                        .map(js_string)
                        .unwrap_or_else(|| "undefined".into())
                        .to_uppercase()
                        .replacen("PIO-", "", 1)
                        .replacen("PIO", "", 1);
                    let bit = match pin.as_str() {
                        "A" | "0" => 1,
                        "B" | "1" => 2,
                        _ => return Err(HttpError::bad_request("pin must be A or B")),
                    };
                    bits = if a.get("on").is_some_and(truthy) { bits & !bit } else { bits | bit };
                }
            }
            if fam == 0x3a { 0xfc | bits } else { bits }
        }
        _ => return Err(HttpError::new(409, format!("{name} has no switchable outputs"))),
    };
    // "on" = output transistor conducting (pin pulled low), the chips' own convention.
    wr(&dir.join("output"), &[byte])?;
    let d = read_record(i, id, false)?;
    let label = {
        let st = lock(&i.st);
        i.slave_label(&st.slaves[id])
    };
    i.ctx().event(&format!("1-Wire {label} outputs set to 0x{byte:x}"), None);
    i.changed();
    Ok(Inner::decoded_json(&d))
}

/// POST /api/onewire/bridge: bind or unbind the kernel ds2482 driver.
async fn bind_bridge(i: &Arc<Inner>, a: &Value) -> Result<Value, HttpError> {
    let bus_s = a.get("bus").map(js_string).unwrap_or_else(|| "undefined".into());
    let bus = benchbob_core::http::parse_int(bus_s.strip_prefix("i2c-").unwrap_or(&bus_s));
    let addr = match a.get("address") {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        other => benchbob_i2c::json::parse_int_radix(&other.map(js_string).unwrap_or_default(), 16).map_or(f64::NAN, |n| n as f64),
    };
    let (Some(bus), true) = (bus.filter(|b| *b >= 0), (24.0..=27.0).contains(&addr) && addr.fract() == 0.0) else {
        return Err(HttpError::bad_request("bus and address (0x18–0x1B) required"));
    };
    let addr = addr as u8;
    let unbind = a.get("bind") == Some(&Value::Bool(false));
    blocking(i, move |i| {
        let base = i.sys.join(format!("bus/i2c/devices/i2c-{bus}"));
        if unbind {
            wr(&base.join("delete_device"), format!("0x{addr:x}\n").as_bytes())?;
            i.ctx().event(&format!("Unbound ds2482 from i2c-{bus} 0x{addr:x}"), None);
        } else {
            if i.sys == std::path::Path::new("/sys") {
                run_sync(&["sudo", "-n", "/usr/sbin/modprobe", "ds2482"], Duration::from_secs(10));
            }
            wr(&base.join("new_device"), format!("ds2482 0x{addr:x}\n").as_bytes())?;
            i.ctx().event(&format!("Bound ds2482 to i2c-{bus} 0x{addr:x}"), None);
        }
        std::thread::sleep(Duration::from_millis(500));
        Ok(())
    })
    .await?;
    if let Some(h) = &i.i2c {
        h.rescan(bus as u32).await;
    }
    blocking(i, |i| {
        i.refresh();
        Ok(i.view())
    })
    .await
}

// ---------- routes ----------

fn route<F, Fut>(i: &Arc<Inner>, method: Method, path: &str, doc: Value, f: F) -> Route
where
    F: Fn(Arc<Inner>, Request) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = RouteResult> + Send + 'static,
{
    let i = i.clone();
    Route::new(method, path, Some(doc), move |r| f(i.clone(), r))
}

fn param(r: &Request, k: &str) -> Value {
    Value::String(r.param(k).to_string())
}

pub(crate) fn routes(i: &Arc<Inner>) -> Vec<Route> {
    vec![
        route(
            i,
            Method::GET,
            "/api/onewire",
            json!("1-Wire masters, slaves (decoded readings + history), iButton touch log and DS2482/DS9490R bridges"),
            |i, _| async move { reply(i.view()) },
        ),
        route(
            i,
            Method::POST,
            "/api/onewire/search",
            json!({ "description": "Trigger an immediate search on a master (default: the first). mode: continuous|off sets w1_master_search persistently", "body": { "master": "w1_bus_master1", "mode": "continuous" } }),
            |i, r| async move {
                let a = r.json()?;
                let (m, search) = must_master(&i, a.get("master"))?;
                let mode = a.get("mode").and_then(Value::as_str).map(String::from);
                let v = blocking(&i, move |i| {
                    let p = i.root.join(&m).join("w1_master_search");
                    match mode.as_deref() {
                        Some("continuous") => wr(&p, b"-1\n")?,
                        Some("off") => wr(&p, b"0\n")?,
                        _ => {}
                    }
                    search_now(i, &m, search)?;
                    Ok(i.view())
                })
                .await?;
                reply(v)
            },
        ),
        route(
            i,
            Method::POST,
            "/api/onewire/:id/read",
            json!(
                "Read one slave now (a temperature conversion takes up to 750 ms). ?eeprom=1 also returns EEPROM contents for memory chips"
            ),
            |i, r| async move {
                let id = must_slave(&i, Some(&param(&r, "id")))?;
                let eeprom = r.query("eeprom").is_some_and(|v| !v.is_empty());
                let v = blocking(&i, move |i| {
                    let d = read_record(i, &id, eeprom)?;
                    i.changed();
                    let mut v = slave_view(i, &id);
                    if let (Some(e), Some(o)) = (d.eeprom, v.as_object_mut()) {
                        o.insert("eeprom".into(), json!(e));
                    }
                    Ok(v)
                })
                .await?;
                reply(v)
            },
        ),
        route(
            i,
            Method::POST,
            "/api/onewire/:id/output",
            json!({ "description": "Set outputs on DS2408 (pin 0–7), DS2413/DS2406 (pin A/B). on: true = output transistor conducting (pin pulled low). Or value: raw latch byte. Requires confirm: true", "body": { "pin": "A", "on": true, "confirm": true } }),
            |i, r| async move {
                let a = r.json()?;
                if a.get("confirm") != Some(&Value::Bool(true)) {
                    return Err(HttpError::bad_request("Switching outputs drives hardware; repeat with \"confirm\": true"));
                }
                let id = must_slave(&i, Some(&param(&r, "id")))?;
                reply(blocking(&i, move |i| set_output(i, &id, &a)).await?)
            },
        ),
        route(
            i,
            Method::PUT,
            "/api/onewire/:id/resolution",
            json!({ "description": "Set DS18B20/DS1822/DS28EA00 resolution (9–12 bit). save: true also stores it in the sensor's EEPROM (limited write cycles)", "body": { "bits": 12, "save": false } }),
            |i, r| async move {
                let id = must_slave(&i, Some(&param(&r, "id")))?;
                let a = r.json()?;
                let bits = a.get("bits").map(js_number).unwrap_or(f64::NAN);
                {
                    let st = lock(&i.st);
                    let s = &st.slaves[&id];
                    if s.family.kind != "temp" || s.fam == 0x10 {
                        return Err(HttpError::new(409, format!("{} has no adjustable resolution", s.family.name)));
                    }
                }
                if ![9.0, 10.0, 11.0, 12.0].contains(&bits) {
                    return Err(HttpError::bad_request("bits must be 9–12"));
                }
                let save = a.get("save").is_some_and(truthy);
                let v = blocking(&i, move |i| {
                    wr(&i.root.join(&id).join("resolution"), format!("{}\n", bits as u8).as_bytes())?;
                    if save {
                        wr(&i.root.join(&id).join("eeprom_cmd"), b"save\n")?;
                    }
                    read_record(i, &id, false)?;
                    i.changed();
                    Ok(slave_view(i, &id))
                })
                .await?;
                reply(v)
            },
        ),
        route(
            i,
            Method::PUT,
            "/api/onewire/:id/label",
            json!({ "description": "Name a slave (saved by ROM id)", "body": { "label": "Freezer" } }),
            |i, r| async move {
                let id = must_slave(&i, Some(&param(&r, "id")))?;
                let a = r.json()?;
                let mut l = i.labels();
                match a.get("label").filter(|v| truthy(v)) {
                    Some(v) => {
                        l.insert(id.clone(), json!(js_string(v).chars().take(60).collect::<String>()));
                    }
                    None => {
                        l.shift_remove(&id);
                    }
                }
                i.ctx().set_setting("labels", Value::Object(l));
                if let Some(rs) = a.get("rsens").filter(|v| !v.is_null()) {
                    let n = js_number(rs);
                    i.ctx().set_setting(&format!("rsens:{id}"), benchbob_core::http::num(if n.is_nan() { 0.0 } else { n }));
                }
                i.changed();
                reply(slave_view(&i, &id))
            },
        ),
        route(
            i,
            Method::PUT,
            "/api/onewire/settings",
            json!({ "description": "interval: ms between background read cycles (≥2000); bulk: simultaneous conversion of all temperature sensors on a master", "body": { "interval": 10000, "bulk": true } }),
            |i, r| async move {
                let a = r.json()?;
                if let Some(v) = a.get("interval").filter(|v| !v.is_null()) {
                    let n = js_number(v);
                    if !(2000.0..=3_600_000.0).contains(&n) {
                        return Err(HttpError::bad_request("interval must be 2000–3600000 ms"));
                    }
                    i.ctx().set_setting("interval", benchbob_core::http::num(n));
                    i.schedule_poll(100);
                }
                if let Some(b) = a.get("bulk").filter(|v| !v.is_null()) {
                    i.ctx().set_setting("bulk", json!(truthy(b)));
                }
                i.changed();
                reply(i.view())
            },
        ),
        route(
            i,
            Method::POST,
            "/api/onewire/bridge",
            json!({ "description": "Bind (or with bind:false, unbind) the kernel ds2482 driver to a DS2482 I²C→1-Wire bridge, which then appears as another master", "body": { "bus": 1, "address": "0x18", "bind": true } }),
            |i, r| async move {
                let a = r.json()?;
                reply(bind_bridge(&i, &a).await?)
            },
        ),
    ]
}

// ---------- MCP ----------

fn tool<F, Fut>(i: &Arc<Inner>, name: &str, desc: &str, schema: Value, f: F) -> Tool
where
    F: Fn(Arc<Inner>, Value) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Value, HttpError>> + Send + 'static,
{
    let i = i.clone();
    Tool::new(name, desc, schema, move |a| f(i.clone(), a))
}

pub(crate) fn tools(i: &Arc<Inner>) -> Vec<Tool> {
    vec![
        tool(
            i,
            "onewire_list",
            "List 1-Wire masters and slaves (DS18B20 temperatures, DS2438, DS2408/DS2413 I/O, iButtons, EEPROMs…) with their latest readings, plus the recent iButton touch log.",
            json!({ "type": "object", "properties": {} }),
            |i, _| async move {
                let labels = i.labels();
                let st = lock(&i.st);
                let n = st.touch.len();
                Ok(json!({
                    "masters": Inner::masters_json(&st),
                    "slaves": st.slaves.values().map(|s| i.slave_summary(s, &labels)).collect::<Vec<_>>(),
                    "touchLog": st.touch[n.saturating_sub(20)..].iter().map(|t| json!({ "t": benchbob_i2c_chips::time::iso(t.t as i64), "id": t.id, "event": t.event, "family": t.family })).collect::<Vec<_>>(),
                }))
            },
        ),
        tool(
            i,
            "onewire_read",
            "Read one 1-Wire slave now by ROM id (e.g. 28-0000075b1a2c). Temperature sensors take up to 750 ms. eeprom: true returns memory contents for EEPROM chips.",
            json!({ "type": "object", "properties": { "id": { "type": "string" }, "eeprom": { "type": "boolean" } }, "required": ["id"] }),
            |i, a| async move {
                let id = must_slave(&i, a.get("id"))?;
                let eeprom = a.get("eeprom").is_some_and(truthy);
                blocking(&i, move |i| {
                    let d = read_record(i, &id, eeprom)?;
                    let mut v = slave_summary(i, &id);
                    if let (Some(e), Some(o)) = (d.eeprom, v.as_object_mut()) {
                        o.insert("eeprom".into(), json!(e));
                    }
                    Ok(v)
                })
                .await
            },
        ),
        tool(
            i,
            "onewire_search",
            "Trigger an immediate 1-Wire bus search (new devices otherwise appear within the master's search interval, ~10 s) and return the slave list.",
            json!({ "type": "object", "properties": { "master": { "type": "string", "description": "e.g. w1_bus_master1 (default: first)" } } }),
            |i, a| async move {
                let (m, search) = must_master(&i, a.get("master"))?;
                blocking(&i, move |i| {
                    search_now(i, &m, search)?;
                    let labels = i.labels();
                    let st = lock(&i.st);
                    Ok(Value::Array(st.slaves.values().filter(|s| s.present).map(|s| i.slave_summary(s, &labels)).collect()))
                })
                .await
            },
        ),
    ]
}
