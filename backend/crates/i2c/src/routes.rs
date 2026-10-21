//! REST routes, raw register operations and MCP tools (i2c.ts "raw operations", "routes",
//! "MCP tools").

use std::sync::Arc;

use axum::http::Method;
use benchbob_core::http::{HttpError, js_number, js_string, json as reply, num, truthy};
use benchbob_core::{Request, Route, RouteResult, Tool};
use benchbob_i2c_chips::io::{errno, func};
use benchbob_i2c_chips::time::iso;
use benchbob_i2c_chips::{I2cError, Msg, func_names, hex};
use serde_json::{Map, Value, json};

use crate::json::{byte_arr, int_arg, parse_addr, v_json, v_string};
use crate::{Bus, Inner, POINTER_UNSAFE, lock};

fn bus_err(e: I2cError) -> HttpError {
    HttpError::new(502, e.message())
}

fn get<'a>(a: &'a Value, k: &str) -> Option<&'a Value> {
    a.get(k).filter(|v| !v.is_null())
}

fn find_dev(b: &Bus, addr: u8) -> Result<(), HttpError> {
    if lock(&b.st).devices.contains_key(&addr) {
        Ok(())
    } else {
        Err(HttpError::new(404, format!("Nothing found at {} on i2c-{} in the last scan", hex(addr as u32), b.bus)))
    }
}

fn need_confirm(a: &Value, what: &str) -> Result<(), HttpError> {
    if a.get("confirm") != Some(&Value::Bool(true)) {
        return Err(HttpError::bad_request(format!("{what} writes to the device; repeat with \"confirm\": true")));
    }
    Ok(())
}

/// `a.size || default`, as a string.
fn size_of(a: &Value, default: &str) -> String {
    match a.get("size") {
        Some(v) if truthy(v) => js_string(v),
        _ => default.into(),
    }
}

// ---------- raw operations ----------

pub(crate) async fn raw_read(b: &Arc<Bus>, addr: u8, a: &Value) -> Result<Value, HttpError> {
    let size = size_of(a, if get(a, "register").is_none() { "raw" } else { "byte" });
    let force = a.get("force").is_some_and(truthy);
    let reg = get(a, "register").map(|r| int_arg(Some(r), "register", 0, 255)).transpose()?.map(|r| r as u8);
    let len = match get(a, "length") {
        None => 1,
        Some(l) => int_arg(Some(l), "length", 1, if size == "raw" { 256 } else { 32 })? as usize,
    };
    Inner::with_bus(b, move |io| -> Result<Value, HttpError> {
        if size == "raw" {
            if len == 1 {
                return Ok(json!({ "bytes": [io.receive(addr, force).map_err(bus_err)?] }));
            }
            let r = io.transfer(addr, &[Msg::R(len)], force).map_err(bus_err)?;
            return Ok(json!({ "bytes": r.into_iter().next().unwrap_or_default() }));
        }
        let Some(reg) = reg else { return Err(HttpError::bad_request(format!("size \"{size}\" needs a register"))) };
        match size.as_str() {
            "byte" => Ok(json!({ "register": reg, "bytes": [io.read_byte(addr, reg, force).map_err(bus_err)?] })),
            "word" => {
                let w = io.read_word(addr, reg, force).map_err(bus_err)?;
                Ok(json!({ "register": reg, "word": w, "wordBigEndian": w.swap_bytes(), "bytes": [w & 0xff, w >> 8] }))
            }
            "block" => Ok(json!({ "register": reg, "bytes": io.read_block(addr, reg, len, force).map_err(bus_err)? })),
            _ => Err(HttpError::bad_request("size must be byte, word, block or raw")),
        }
    })
    .await?
}

pub(crate) async fn raw_write(b: &Arc<Bus>, addr: u8, a: &Value) -> Result<Value, HttpError> {
    let force = a.get("force").is_some_and(truthy);
    let reg = get(a, "register").map(|r| int_arg(Some(r), "register", 0, 255)).transpose()?.map(|r| r as u8);
    let has_bytes = a.get("bytes").is_some_and(truthy);
    let size = size_of(a, if has_bytes { if reg.is_none() { "raw" } else { "block" } } else { "byte" });
    let a = a.clone();
    let funcs = b.funcs;
    Inner::with_bus(b, move |io| -> Result<Value, HttpError> {
        if size == "raw" {
            let src = match get(&a, "bytes") {
                Some(v) => v.clone(),
                None => Value::Array(vec![a.get("value").cloned().unwrap_or(Value::Null)]),
            };
            let bytes = byte_arr(Some(&src), "bytes")?;
            if bytes.len() == 1 && reg.is_none() && funcs & func::I2C == 0 {
                io.send(addr, bytes[0], force).map_err(bus_err)?;
            } else {
                let w = match reg {
                    None => bytes.clone(),
                    Some(r) => std::iter::once(r).chain(bytes.iter().copied()).collect(),
                };
                io.transfer(addr, &[Msg::W(w)], force).map_err(bus_err)?;
            }
            return Ok(json!({ "wrote": bytes }));
        }
        let Some(reg) = reg else { return Err(HttpError::bad_request(format!("size \"{size}\" needs a register"))) };
        match size.as_str() {
            "byte" => {
                let v = int_arg(a.get("value"), "value", 0, 255)? as u8;
                io.write_byte(addr, reg, v, force).map_err(bus_err)?;
                Ok(json!({ "register": reg, "wrote": [v] }))
            }
            "word" => {
                let v = int_arg(a.get("value"), "value", 0, 0xffff)? as u16;
                // `value` is the register's logical value; bigEndian (default true) puts the MSB
                // on the wire first, as most sensors expect.
                let w = if a.get("bigEndian") == Some(&Value::Bool(false)) { v } else { v.swap_bytes() };
                io.write_word(addr, reg, w, force).map_err(bus_err)?;
                Ok(json!({ "register": reg, "wrote": [w & 0xff, w >> 8] }))
            }
            "block" => {
                let bytes = byte_arr(a.get("bytes"), "bytes")?;
                if bytes.len() > 32 {
                    return Err(HttpError::bad_request("block writes are at most 32 bytes"));
                }
                io.write_block(addr, reg, &bytes, force).map_err(bus_err)?;
                Ok(json!({ "register": reg, "wrote": bytes }))
            }
            _ => Err(HttpError::bad_request("size must be byte, word, block or raw")),
        }
    })
    .await?
}

fn transfer_msgs(a: &Value) -> Result<Vec<Msg>, HttpError> {
    let mut msgs = Vec::new();
    if let Some(list) = a.get("msgs").and_then(Value::as_array) {
        for m in list {
            if m.get("r").is_some() {
                msgs.push(Msg::R(int_arg(m.get("r"), "r", 1, 256)? as usize));
            } else {
                msgs.push(Msg::W(byte_arr(m.get("w"), "w")?));
            }
        }
    } else {
        let w = match get(a, "write") {
            None => vec![],
            Some(w) => byte_arr(Some(w), "write")?,
        };
        if !w.is_empty() {
            msgs.push(Msg::W(w));
        }
        let r = match get(a, "read") {
            None => 0,
            Some(r) => int_arg(Some(r), "read", 0, 256)?,
        };
        if r > 0 {
            msgs.push(Msg::R(r as usize));
        }
    }
    Ok(msgs)
}

/// Whether a transfer request writes (and so needs confirm over REST).
fn transfer_writes(a: &Value) -> Result<bool, HttpError> {
    if let Some(list) = a.get("msgs").and_then(Value::as_array) {
        return Ok(list.iter().any(|m| m.get("w").is_some()));
    }
    Ok(match get(a, "write") {
        Some(w) => !byte_arr(Some(w), "write")?.is_empty(),
        None => false,
    })
}

pub(crate) async fn raw_transfer(b: &Arc<Bus>, addr: u8, a: &Value) -> Result<Value, HttpError> {
    let msgs = transfer_msgs(a)?;
    if msgs.is_empty() {
        return Err(HttpError::bad_request("Nothing to do: give write bytes and/or a read length"));
    }
    if b.funcs & func::I2C == 0 {
        return Err(HttpError::new(
            409,
            format!("i2c-{} only supports SMBus transfers ({}); use read/write instead", b.bus, func_names(b.funcs).join(", ")),
        ));
    }
    let force = a.get("force").is_some_and(truthy);
    let reads = Inner::with_bus(b, move |io| io.transfer(addr, &msgs, force)).await?.map_err(bus_err)?;
    Ok(json!({ "reads": reads }))
}

/// i2cdump-style 256-register grid. null = that register didn't answer.
async fn dump(b: &Arc<Bus>, addr: u8, force: bool) -> Result<Vec<Option<u8>>, HttpError> {
    Inner::with_bus(b, move |io| {
        let mut out = vec![None; 256];
        for base in (0..256usize).step_by(32) {
            match io.read_block(addr, base as u8, 32, force) {
                Ok(v) => {
                    for (i, x) in v.into_iter().enumerate().take(256 - base) {
                        out[base + i] = Some(x);
                    }
                    continue;
                }
                Err(e) if e.errno == errno::EBUSY => return Err(HttpError::new(409, e.message())),
                Err(_) => {}
            }
            for (r, slot) in out.iter_mut().enumerate().skip(base).take(32) {
                if let Ok(v) = io.read_byte(addr, r as u8, force) {
                    *slot = Some(v);
                }
            }
        }
        Ok(out)
    })
    .await?
}

/// EEPROM contents using the per-device size setting (1- or 2-byte word addresses).
async fn eeprom(i: &Arc<Inner>, b: &Arc<Bus>, addr: u8, offset: usize, len: usize) -> Result<Value, HttpError> {
    let size_v = i.dev_cfg(b, addr).get("opts").and_then(|o| o.get("size")).filter(|v| !v.is_null()).cloned().unwrap_or(json!(256));
    let size = js_number(&size_v);
    if offset as f64 >= size || size.is_nan() {
        return Err(HttpError::bad_request(format!("offset beyond the {}-byte EEPROM", js_string(&num(size)))));
    }
    let len = len.min((size as usize).saturating_sub(offset)).min(4096);
    let plain = b.funcs & func::I2C != 0;
    let bytes = Inner::with_bus(b, move |io| -> Result<Vec<u8>, HttpError> {
        let mut out = Vec::new();
        if size > 2048.0 {
            if !plain {
                return Err(HttpError::new(409, "2-byte-address EEPROMs need a plain-I²C adapter"));
            }
            let mut o = offset;
            while o < offset + len {
                let n = 256.min(offset + len - o);
                let r = io.transfer(addr, &[Msg::W(vec![(o >> 8) as u8, o as u8]), Msg::R(n)], false).map_err(bus_err)?;
                out.extend(r.into_iter().next().unwrap_or_default());
                o += 256;
            }
        } else {
            // 24C04/08/16 put address bits 8–10 in the I²C address (blocks of 256 at addr+1, +2…).
            let mut o = offset;
            while o < offset + len {
                let blk = (o >> 8) as u8;
                let n = 32.min(offset + len - o).min(256 - (o & 0xff));
                out.extend(io.read_block(addr.wrapping_add(blk), o as u8, n, false).map_err(bus_err)?);
                o += n;
            }
        }
        Ok(out)
    })
    .await??;
    Ok(json!({ "offset": offset, "size": num(size), "bytes": bytes }))
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
            "/api/i2c",
            json!(
                "I²C buses (HDMI DDC buses excluded unless ?all=1), each with the devices from the last scan: identification, confidence, live readings and history"
            ),
            |i, r| async move {
                let list = if r.query("all").is_some_and(|v| !v.is_empty()) { i.all_buses() } else { i.user_buses() };
                reply(json!({ "buses": list.iter().map(|b| i.bus_view(b)).collect::<Vec<_>>() }))
            },
        ),
        route(
            i,
            Method::POST,
            "/api/i2c/:bus/scan",
            json!({ "description": "Rescan a bus (i2cdetect auto mode, skips UU) and identify new devices. deep: re-identify everything and allow pointer probes at ambiguous addresses (e.g. 0x39 PCF8574A vs TSL2561)", "body": { "deep": false, "reidentify": false } }),
            |i, r| async move {
                let b = i.parse_bus(Some(&param(&r, "bus")))?;
                let a = r.json()?;
                let deep = a.get("deep").is_some_and(truthy);
                i.scan(&b, deep, a.get("reidentify").is_some_and(truthy) || deep).await;
                reply(i.bus_view(&b))
            },
        ),
        route(
            i,
            Method::PUT,
            "/api/i2c/:bus/settings",
            json!({ "description": "Bus settings. autoScan: seconds between background rescans (0 = only on demand)", "body": { "autoScan": 30 } }),
            |i, r| async move {
                let b = i.parse_bus(Some(&param(&r, "bus")))?;
                let a = r.json()?;
                if let Some(v) = get(&a, "autoScan") {
                    let s = int_arg(Some(v), "autoScan", 0, 86400)?;
                    let mut cfg = i.bus_cfg(&b);
                    cfg.insert("autoScan".into(), json!(s));
                    i.ctx().set_setting(&format!("bus:{}", b.key), Value::Object(cfg));
                    lock(&b.st).next_scan = (s > 0).then(|| benchbob_core::timeline::now_ms() + s as u64 * 1000);
                }
                i.ctx().changed();
                reply(i.bus_view(&b))
            },
        ),
        route(
            i,
            Method::POST,
            "/api/i2c/:bus/:addr/read",
            json!("Run the device's read-only decoder now (no writes) and return the readings"),
            |i, r| async move {
                let b = i.parse_bus(Some(&param(&r, "bus")))?;
                let addr = parse_addr(Some(&param(&r, "addr")))?;
                find_dev(&b, addr)?;
                reply(i.read_dev(&b, addr, false).await?)
            },
        ),
        route(
            i,
            Method::POST,
            "/api/i2c/:bus/:addr/measure",
            json!("Take a measurement. May write to the chip (start a conversion, wake it briefly); see measureNote in GET /api/i2c"),
            |i, r| async move {
                let b = i.parse_bus(Some(&param(&r, "bus")))?;
                let addr = parse_addr(Some(&param(&r, "addr")))?;
                find_dev(&b, addr)?;
                reply(i.read_dev(&b, addr, true).await?)
            },
        ),
        route(
            i,
            Method::PUT,
            "/api/i2c/:bus/:addr/settings",
            json!({ "description": "Per-device settings, saved by bus + address. poll: auto-read with the read-only decoder; auto: run Measure every interval (writes); interval in ms; opts: chip options such as shunt (Ω) or EEPROM size; label: your name for it", "body": { "poll": true, "auto": false, "interval": 2000, "opts": { "shunt": 0.1 }, "label": "" } }),
            |i, r| async move {
                let b = i.parse_bus(Some(&param(&r, "bus")))?;
                let addr = parse_addr(Some(&param(&r, "addr")))?;
                find_dev(&b, addr)?;
                let a = r.json()?;
                reply(dev_settings(&i, &b, addr, &a).await?)
            },
        ),
        route(
            i,
            Method::POST,
            "/api/i2c/:bus/:addr/raw/read",
            json!({ "description": "Raw read. size: byte|word|block (need register) or raw (no register: receive-byte, or a pure read of length bytes). Note a register read writes the register pointer first. force: access an address a kernel driver owns", "body": { "register": "0xD0", "size": "byte", "length": 1, "force": false } }),
            |i, r| async move {
                let b = i.parse_bus(Some(&param(&r, "bus")))?;
                let addr = parse_addr(Some(&param(&r, "addr")))?;
                reply(raw_read(&b, addr, &r.json()?).await?)
            },
        ),
        route(
            i,
            Method::POST,
            "/api/i2c/:bus/:addr/raw/write",
            json!({ "description": "Raw write; requires confirm: true. size: byte (value), word (value, big-endian on the wire unless bigEndian: false), block (bytes, ≤32) with a register; raw: bytes as-is", "body": { "register": "0xF4", "size": "byte", "value": "0x27", "confirm": true } }),
            |i, r| async move {
                let b = i.parse_bus(Some(&param(&r, "bus")))?;
                let a = r.json()?;
                need_confirm(&a, "A raw write")?;
                let addr = parse_addr(Some(&param(&r, "addr")))?;
                reply(raw_write(&b, addr, &a).await?)
            },
        ),
        route(
            i,
            Method::POST,
            "/api/i2c/:bus/:addr/transfer",
            json!({ "description": "Combined transaction with repeated starts (i2ctransfer). Either write+read, or msgs: [{w:[…]},{r:n}]. Any write needs confirm: true", "body": { "write": ["0xFA", "0x0F"], "read": 8, "confirm": true } }),
            |i, r| async move {
                let b = i.parse_bus(Some(&param(&r, "bus")))?;
                let a = r.json()?;
                if transfer_writes(&a)? {
                    need_confirm(&a, "This transfer")?;
                }
                let addr = parse_addr(Some(&param(&r, "addr")))?;
                reply(raw_transfer(&b, addr, &a).await?)
            },
        ),
        route(
            i,
            Method::GET,
            "/api/i2c/:bus/:addr/dump",
            json!(
                "i2cdump-style read of registers 0x00–0xFF (null = no answer). Each read sets the register pointer, which on command-style chips (PCF8574, TCA9548A, HTU21D…) executes a command: those need ?confirm=1"
            ),
            |i, r| async move {
                let b = i.parse_bus(Some(&param(&r, "bus")))?;
                let addr = parse_addr(Some(&param(&r, "addr")))?;
                let (chip, name) = {
                    let st = lock(&b.st);
                    let d = st.devices.get(&addr);
                    (d.and_then(|d| d.chip), d.map(|d| d.chip_name()).unwrap_or_default())
                };
                let unsafe_ = chip.is_none_or(|c| POINTER_UNSAFE.contains(&c.id));
                if unsafe_ && !r.query("confirm").is_some_and(|v| !v.is_empty()) {
                    let who = if chip.is_some() { format!("{name} treats") } else { "An unidentified chip may treat".into() };
                    return Err(HttpError::bad_request(format!("{who} register pointers as commands; add ?confirm=1 to dump anyway")));
                }
                let regs = dump(&b, addr, r.query("force").is_some_and(|v| !v.is_empty())).await?;
                reply(json!({ "addr": addr, "regs": regs }))
            },
        ),
        route(
            i,
            Method::GET,
            "/api/i2c/:bus/:addr/eeprom",
            json!(
                "Read EEPROM contents (?offset=0&length=256, ≤4096 per call) using the device's size setting to pick 1- or 2-byte addressing"
            ),
            |i, r| async move {
                let b = i.parse_bus(Some(&param(&r, "bus")))?;
                let addr = parse_addr(Some(&param(&r, "addr")))?;
                let q = |k: &str, d: i64| r.query(k).map(|v| Value::String(v.into())).unwrap_or(json!(d));
                let off = int_arg(Some(&q("offset", 0)), "offset", 0, 65535)? as usize;
                let len = int_arg(Some(&q("length", 256)), "length", 1, 4096)? as usize;
                reply(eeprom(&i, &b, addr, off, len).await?)
            },
        ),
    ]
}

/// PUT /api/i2c/:bus/:addr/settings
async fn dev_settings(i: &Arc<Inner>, b: &Arc<Bus>, addr: u8, a: &Value) -> Result<Value, HttpError> {
    let (chip, name, cs) = {
        let st = lock(&b.st);
        let d = &st.devices[&addr];
        (d.chip, d.chip_name(), d.cs.clone())
    };
    let who = if name.is_empty() { "This device".to_string() } else { name.clone() };
    let mut patch = Map::new();
    if let Some(p) = get(a, "poll") {
        patch.insert("poll".into(), json!(truthy(p)));
    }
    if let Some(v) = get(a, "interval") {
        patch.insert("interval".into(), json!(int_arg(Some(v), "interval", 250, 3_600_000)?));
    }
    if let Some(l) = get(a, "label") {
        patch.insert("label".into(), json!(js_string(l).chars().take(60).collect::<String>()));
    }
    if let Some(o) = a.get("opts").filter(|v| truthy(v) && (v.is_object() || v.is_array())) {
        let known = chip.map(|c| c.opts).unwrap_or(&[]);
        let mut opts = i.dev_cfg(b, addr).get("opts").and_then(Value::as_object).cloned().unwrap_or_default();
        let entries: Vec<(String, Value)> = match o {
            Value::Object(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Value::Array(l) => l.iter().enumerate().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            _ => vec![],
        };
        for (k, v) in entries {
            let Some(def) = known.iter().find(|x| x.key == k) else {
                return Err(HttpError::bad_request(format!("{who} has no option \"{k}\"")));
            };
            let n = js_number(&v);
            if !n.is_finite() || n <= 0.0 {
                return Err(HttpError::bad_request(format!("{} must be a positive number", def.label)));
            }
            opts.insert(k, num(n));
        }
        patch.insert("opts".into(), Value::Object(opts));
    }
    if let Some(v) = get(a, "auto") {
        let on = truthy(v);
        if on && chip.is_none_or(|c| c.measure.is_none()) {
            return Err(HttpError::new(409, format!("{who} has no measure operation")));
        }
        let was = i.dev_cfg(b, addr).get("auto").is_some_and(truthy);
        patch.insert("auto".into(), json!(on));
        if on != was
            && let (Some(c), Some(cs)) = (chip, cs)
            && let Some(hook) = if on { c.auto_start } else { c.auto_stop }
        {
            Inner::with_bus(b, move |io| {
                let mut cs = lock(&cs);
                hook(&mut benchbob_i2c_chips::Dev::new(io, addr), &mut cs)
            })
            .await?
            .map_err(|e| HttpError::new(502, e.message()))?;
        }
    }
    i.set_dev_cfg(b, addr, patch);
    let mut st = lock(&b.st);
    let Some(d) = st.devices.get_mut(&addr) else { return Err(HttpError::new(404, "device went away")) };
    d.next = 0;
    let v = i.dev_view(b, d);
    drop(st);
    i.ctx().changed();
    Ok(v)
}

// ---------- MCP tools ----------

fn bus_prop() -> Value {
    json!({ "type": ["integer", "string"], "description": "Bus number, e.g. 1 or \"i2c-1\" (see i2c_buses)" })
}
fn addr_prop() -> Value {
    json!({ "type": ["integer", "string"], "description": "Device address, 7-bit, e.g. 118 or \"0x76\"" })
}

fn devices_summary(i: &Inner, b: &Bus) -> Vec<Value> {
    let st = lock(&b.st);
    st.devices
        .values()
        .map(|d| {
            let v = i.dev_view(b, d);
            let mut m = Map::new();
            m.insert("address".into(), v["hex"].clone());
            m.insert("claimedBy".into(), v["claimed"].clone());
            m.insert("chip".into(), v["name"].clone());
            m.insert("what".into(), v["what"].clone());
            m.insert("confidence".into(), v["confidence"].clone());
            m.insert("note".into(), v["note"].clone());
            if v["hints"].as_array().is_some_and(|h| !h.is_empty()) {
                m.insert("possible".into(), v["hints"].clone());
            }
            let readings = d.readout.as_ref().map(|r| {
                let mut o = Map::new();
                for x in &r.values {
                    let val = match x.unit {
                        Some(u) => json!(format!("{} {u}", v_string(&x.v))),
                        None => v_json(&x.v),
                    };
                    o.insert(x.label.clone(), val);
                }
                Value::Object(o)
            });
            m.insert("readings".into(), readings.unwrap_or(Value::Null));
            if let Some(n) = d.readout.as_ref().and_then(|r| r.note.clone()) {
                m.insert("readingNote".into(), json!(n));
            }
            if d.readout.as_ref().is_some_and(|r| r.stale == Some(true)) {
                m.insert("stale".into(), json!(true));
            }
            m.insert("readAt".into(), if d.read_at != 0 { json!(iso(d.read_at as i64)) } else { Value::Null });
            m.insert("canMeasure".into(), v["canMeasure"].clone());
            if v["measureNote"].as_str().is_some_and(|s| !s.is_empty()) {
                m.insert("measureNote".into(), v["measureNote"].clone());
            }
            if !d.error.is_empty() {
                m.insert("error".into(), json!(d.error));
            }
            Value::Object(m)
        })
        .collect()
}

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
            "i2c_buses",
            "List the I²C buses on BenchBob (header pins 3/5 is i2c-1; USB adapters and i2c-stub appear as extra buses). HDMI DDC buses are omitted.",
            json!({ "type": "object", "properties": {} }),
            |i, _| async move {
                Ok(Value::Array(
                    i.user_buses()
                        .iter()
                        .map(|b| {
                            let st = lock(&b.st);
                            let mut m = Map::new();
                            m.insert("bus".into(), json!(b.bus));
                            m.insert("label".into(), json!(b.label));
                            m.insert("adapter".into(), json!(b.name));
                            m.insert("functionality".into(), json!(func_names(b.funcs)));
                            m.insert("devices".into(), json!(st.devices.len()));
                            m.insert("lastScan".into(), if st.last_scan != 0 { json!(iso(st.last_scan as i64)) } else { Value::Null });
                            if !st.error.is_empty() {
                                m.insert("error".into(), json!(st.error));
                            }
                            Value::Object(m)
                        })
                        .collect(),
                ))
            },
        ),
        tool(
            i,
            "i2c_scan",
            "Scan an I²C bus like `i2cdetect -y` (quick-write, read-byte at 0x30–0x37/0x50–0x5F, skips addresses owned by kernel drivers) and identify what answers. Returns the device list.",
            json!({ "type": "object", "properties": { "bus": bus_prop(), "deep": { "type": "boolean", "description": "Re-identify every device and allow register-pointer probes at ambiguous addresses" } }, "required": ["bus"] }),
            |i, a| async move {
                let b = i.parse_bus(a.get("bus"))?;
                let deep = a.get("deep").is_some_and(truthy);
                i.scan(&b, deep, deep).await;
                let e = lock(&b.st).error.clone();
                if !e.is_empty() {
                    return Err(HttpError::new(500, e));
                }
                Ok(json!({ "bus": b.bus, "devices": devices_summary(&i, &b) }))
            },
        ),
        tool(
            i,
            "i2c_devices",
            "Identified I²C devices with their latest decoded readings (°C, %RH, hPa, lux, V/A/W, g, RTC time…). Pass measure: true to take a fresh measurement on chips that need a command to convert (this writes to them).",
            json!({ "type": "object", "properties": { "bus": bus_prop(), "address": addr_prop(), "measure": { "type": "boolean" } } }),
            |i, a| async move {
                let list = match get(&a, "bus") {
                    Some(bv) => vec![i.parse_bus(Some(bv))?],
                    None => i.user_buses().into_iter().filter(|b| b.io.is_some()).collect(),
                };
                let want = get(&a, "address");
                let measure = a.get("measure").is_some_and(truthy);
                if want.is_some() || measure {
                    for b in &list {
                        let addrs: Vec<(u8, bool)> = {
                            let st = lock(&b.st);
                            st.devices
                                .values()
                                .map(|d| (d.addr, d.chip.is_some() && d.claimed.is_none() && d.chip.is_some_and(|c| c.measure.is_some())))
                                .collect()
                        };
                        for (addr, has_measure) in addrs {
                            if let Some(w) = want
                                && addr != parse_addr(Some(w))?
                            {
                                continue;
                            }
                            let ok = {
                                let st = lock(&b.st);
                                st.devices.get(&addr).is_some_and(|d| d.chip.is_some() && d.claimed.is_none())
                            };
                            if !ok {
                                continue;
                            }
                            let _ = i.read_dev(b, addr, measure && has_measure).await;
                        }
                    }
                }
                let mut out = Vec::new();
                for b in &list {
                    let mut devs = devices_summary(&i, b);
                    if let Some(w) = want {
                        let n = parse_addr(Some(w))?;
                        devs.retain(|d| d["address"].as_str() == Some(hex(n as u32).as_str()));
                    }
                    out.push(json!({ "bus": b.bus, "label": b.label, "devices": devs }));
                }
                Ok(Value::Array(out))
            },
        ),
        tool(
            i,
            "i2c_read",
            "Raw I²C read. With a register: size byte/word/block (word returns both byte orders). Without: receive-byte, or a pure read of `length` bytes. Register reads write the register pointer first, which command-style chips (PCF8574, TCA9548A, HTU21D) treat as a command.",
            json!({ "type": "object", "properties": { "bus": bus_prop(), "address": addr_prop(), "register": { "type": ["integer", "string"] }, "size": { "enum": ["byte", "word", "block", "raw"] }, "length": { "type": "integer", "minimum": 1, "maximum": 256 }, "force": { "type": "boolean", "description": "Access an address owned by a kernel driver" } }, "required": ["bus", "address"] }),
            |i, a| async move {
                let b = i.parse_bus(a.get("bus"))?;
                raw_read(&b, parse_addr(a.get("address"))?, &a).await
            },
        ),
        tool(
            i,
            "i2c_write",
            "Raw I²C write (changes device state). size byte: value to register; word: 16-bit value (MSB first on the wire unless bigEndian false); block: bytes to register; raw: bytes as-is with no register.",
            json!({ "type": "object", "properties": { "bus": bus_prop(), "address": addr_prop(), "register": { "type": ["integer", "string"] }, "size": { "enum": ["byte", "word", "block", "raw"] }, "value": { "type": ["integer", "string"] }, "bytes": { "type": "array", "items": { "type": ["integer", "string"] } }, "bigEndian": { "type": "boolean" }, "force": { "type": "boolean" } }, "required": ["bus", "address"] }),
            |i, a| async move {
                let b = i.parse_bus(a.get("bus"))?;
                raw_write(&b, parse_addr(a.get("address"))?, &a).await
            },
        ),
        tool(
            i,
            "i2c_transfer",
            "Combined I²C transaction with repeated start: write bytes then read N bytes (like i2ctransfer w2@0x50 0x00 0x00 r16). Needs a plain-I²C adapter (i2c-1 is). Use msgs for more than one write/read.",
            json!({ "type": "object", "properties": { "bus": bus_prop(), "address": addr_prop(), "write": { "type": "array", "items": { "type": ["integer", "string"] } }, "read": { "type": "integer", "minimum": 0, "maximum": 256 }, "msgs": { "type": "array", "items": { "type": "object", "properties": { "w": { "type": "array", "items": { "type": ["integer", "string"] } }, "r": { "type": "integer" } } } }, "force": { "type": "boolean" } }, "required": ["bus", "address"] }),
            |i, a| async move {
                let b = i.parse_bus(a.get("bus"))?;
                raw_transfer(&b, parse_addr(a.get("address"))?, &a).await
            },
        ),
    ]
}
