//! Port operations that take time: target resets over DTR/RTS, auto-baud, and the agent
//! primitives (write and wait for a regex, read with a cursor).

use std::sync::Arc;
use std::time::{Duration, Instant};

use benchbob_core::HttpError;
use benchbob_core::http::{js_number, js_string, num, truthy};
use benchbob_core::timeline::now_ms;
use serde_json::{Value, json};

use crate::Inner;
use crate::autobaud::{AUTOBAUD_RATES, pick_baud};
use crate::ident::{Step, reset_sequence};
use crate::port::{Port, ensure_present, ensure_writable, exists, http};
use crate::termios::{self as T, Queue, SerialConfig};
use crate::util::{base64_encode, decode_utf8, hex_spaced, js_regex, strip_ansi};

/// A long-poll's ceiling. The server has no idle timeout, so a minute-long wait is safe.
pub const MAX_WAIT_MS: f64 = 60000.0;

/// `Number(v) || dflt` for an optional JSON value.
fn num_or(v: Option<&Value>, dflt: f64) -> f64 {
    let n = v.map(js_number).unwrap_or(f64::NAN);
    if n == 0.0 || n.is_nan() { dflt } else { n }
}

fn decode_out(b: &[u8], raw: bool) -> String {
    let s = decode_utf8(b);
    if raw { s } else { strip_ansi(&s) }
}

impl Inner {
    pub(crate) async fn reset(self: &Arc<Self>, p: &Arc<Port>, mode: Option<&Value>) -> Result<(), HttpError> {
        let mode_s = match mode {
            None => "undefined".to_string(),
            Some(v) => js_string(v),
        };
        let (family, was_open) = {
            let st = p.lock();
            ensure_writable(&st)?;
            ensure_present(&st)?;
            (st.info.family, st.open())
        };
        if mode_s == "touch1200" {
            // Native-USB Arduinos and the Pico SDK reboot into their bootloader when the port
            // is opened at 1200 baud and closed (DTR drops). The device usually re-enumerates.
            self.close_fd(p, "closed").await;
            let (path, cfg) = {
                let st = p.lock();
                (st.info.path.clone(), st.cfg)
            };
            let fd = T::open_port(&path, false).map_err(|e| http(500, format!("{path}: {}", T::io_code(&e))))?;
            {
                use std::os::fd::AsRawFd;
                let raw = fd.as_raw_fd();
                let r = T::configure(raw, &SerialConfig { baud: 1200, ..cfg });
                let _ = T::set_modem(raw, Some(false), None);
                drop(fd);
                r.map_err(|e| http(500, e.to_string()))?;
            }
            if was_open {
                let (me, p) = (self.clone(), p.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    if exists(&path) {
                        let _ = me.ensure_open(&p);
                    }
                });
            }
            return Ok(());
        }
        let seq = reset_sequence(&mode_s, family);
        if seq.is_empty() {
            return Err(HttpError::bad_request(format!("Unknown reset mode \"{mode_s}\" (esp, esp-boot, dtr, touch1200)")));
        }
        self.ensure_open(p)?;
        let prev = {
            let mut st = p.lock();
            let prev = st.state;
            st.state = "resetting";
            self.notify(p, &st);
            prev
        };
        let mut result = Ok(());
        for step in seq {
            let (dtr, rts) = match *step {
                Step::Ms(ms) => {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    continue;
                }
                Step::Dtr(v) => (Some(v), None),
                Step::Rts(v) => (None, Some(v)),
            };
            let st = p.lock();
            if let Some(fd) = st.fd()
                && let Err(e) = T::set_modem(fd, dtr, rts)
            {
                result = Err(http(409, format!("{} has no controllable DTR/RTS ({})", st.info.name, e.code())));
                break;
            }
        }
        let mut st = p.lock();
        if st.state == "resetting" {
            st.state = if prev == "resetting" { "open" } else { prev };
        }
        if let Some(fd) = st.fd()
            && let Ok(l) = T::get_modem(fd)
        {
            st.lines = Some(l);
        }
        self.notify(p, &st);
        result
    }

    pub(crate) async fn autobaud(self: &Arc<Self>, p: &Arc<Port>, o: &Value) -> Result<Value, HttpError> {
        {
            let st = p.lock();
            ensure_writable(&st)?;
            if st.state == "autobaud" {
                return Err(http(409, "Auto-baud already running"));
            }
        }
        let given: Vec<f64> = o.get("rates").and_then(Value::as_array).filter(|a| !a.is_empty()).map(|a| a.iter().map(js_number).collect()).unwrap_or_default();
        let rates: Vec<u32> = if given.is_empty() {
            AUTOBAUD_RATES.to_vec()
        } else {
            given.into_iter().filter(|r| r.fract() == 0.0 && *r >= 50.0 && *r <= u32::MAX as f64).map(|r| r as u32).collect()
        };
        if rates.is_empty() {
            return Err(HttpError::bad_request("No valid rates"));
        }
        // Keep the whole sweep inside one HTTP request.
        let ms = num_or(o.get("ms"), 400.0).min((8500 / rates.len()) as f64).max(100.0);
        self.ensure_open(p)?;
        let saved = {
            let mut st = p.lock();
            st.state = "autobaud";
            st.tap = Some(Vec::new());
            self.notify(p, &st);
            st.cfg
        };
        let restore = |me: &Arc<Self>| {
            let mut st = p.lock();
            st.tap = None;
            if let Some(fd) = st.fd() {
                let _ = T::configure(fd, &saved);
            }
            if st.state == "autobaud" {
                st.state = if st.open() { "open" } else { "closed" };
            }
            me.notify(p, &st);
        };
        let mut samples: Vec<(u32, Vec<u8>)> = Vec::new();
        for &baud in &rates {
            let r = {
                let mut st = p.lock();
                match st.fd() {
                    None => Err(http(409, format!("{} closed during auto-baud", st.info.name))),
                    Some(fd) => {
                        let r = T::configure(fd, &SerialConfig { baud, ..saved }).and_then(|_| T::flush(fd, Queue::Rx));
                        st.tap = Some(Vec::new());
                        r.map_err(|e| http(500, e.to_string()))
                    }
                }
            };
            if let Err(e) = r {
                restore(self);
                return Err(e);
            }
            tokio::time::sleep(Duration::from_millis(ms as u64)).await;
            samples.push((baud, p.lock().tap.replace(Vec::new()).unwrap_or_default()));
        }
        let r = pick_baud(&samples);
        let best = r.best.as_ref().filter(|b| b.confidence >= 0.5).map(|b| b.baud);
        let apply = o.get("apply").is_some_and(truthy) && best.is_some();
        // Restore (or apply) the rate and drop whatever arrived at the last trial rate before
        // the tap comes off, so no garbage lands in the ring.
        let target = if apply { SerialConfig { baud: best.unwrap(), ..saved } } else { saved };
        let was_open = {
            let st = p.lock();
            if let Some(fd) = st.fd() {
                let _ = T::configure(fd, &target);
                let _ = T::flush(fd, Queue::Rx);
            }
            st.open()
        };
        if was_open {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let mut st = p.lock();
        st.tap = None;
        if st.state == "autobaud" {
            st.state = if st.open() { "open" } else { "closed" };
        }
        if apply {
            st.cfg = target;
            self.save_port_settings(&p.id, &[("config", st.cfg.to_json())]);
        }
        let mut view = r.to_json();
        view["at"] = json!(now_ms());
        view["applied"] = json!(apply);
        st.autobaud = Some(view);
        self.notify(p, &st);
        let mut out = r.to_json();
        out["msPerRate"] = num(ms);
        out["applied"] = json!(apply);
        out["config"] = st.cfg.to_json();
        Ok(out)
    }

    /// Write, then optionally wait for a regex (or just collect for timeout_ms) and return
    /// what came back.
    pub(crate) async fn write_and_wait(self: &Arc<Self>, p: &Arc<Port>, a: &Value) -> Result<Value, HttpError> {
        let data = crate::util::payload(a)?;
        let open = p.lock().open();
        if !open {
            {
                let mut st = p.lock();
                ensure_writable(&st)?;
                st.want = true; // an agent writing expects the port to stay open for the reply
            }
            self.ensure_open(p)?;
        }
        let re = match a.get("wait").filter(|v| truthy(v)) {
            Some(w) => {
                let flags = a.get("flags").filter(|v| !v.is_null()).map(js_string).unwrap_or_else(|| "m".into());
                Some(js_regex(&js_string(w), &flags).map_err(|e| HttpError::bad_request(format!("Bad wait regex: {e}")))?)
            }
            None => None,
        };
        let t = match a.get("timeout_ms").filter(|v| !v.is_null()) {
            Some(v) => js_number(v),
            None if re.is_some() => 3000.0,
            None => 0.0,
        };
        let timeout = if t.is_nan() { 0.0 } else { t.clamp(0.0, MAX_WAIT_MS) };
        let raw = a.get("raw").is_some_and(truthy);
        let n = data.len();
        let mut tick = p.tick.subscribe();
        let start = {
            let mut st = p.lock();
            let start = st.ring.end;
            self.write_locked(p, &mut st, data, a.get("token").and_then(Value::as_str))?;
            start
        };
        if timeout == 0.0 {
            return Ok(json!({ "written": n, "cursor": p.lock().ring.end }));
        }
        let t0 = Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout as u64);
        let result = |p: &Port| {
            let st = p.lock();
            let got = st.ring.since(start as f64, 64.0 * 1024.0);
            let text = decode_out(&got.data, raw);
            (got, text, st.open())
        };
        let mut matched: Option<Vec<Value>> = None;
        loop {
            let (_, text, open) = result(p);
            if let Some(re) = &re
                && let Some(c) = re.captures(&text)
            {
                matched = Some(c.iter().map(|m| m.map(|m| Value::String(m.as_str().into())).unwrap_or(Value::Null)).collect());
                break;
            }
            if !open {
                break;
            }
            tokio::select! {
                r = tick.changed() => if r.is_err() { break },
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        let (got, text, _) = result(p);
        let mut out = json!({ "written": n });
        if re.is_some() {
            out["matched"] = json!(matched.is_some());
        }
        if let Some(m) = matched {
            out["match"] = Value::Array(m);
        }
        out["elapsed_ms"] = json!(t0.elapsed().as_millis() as u64);
        out["output"] = json!(text);
        out["bytes"] = json!(got.data.len());
        out["cursor"] = json!(got.to);
        Ok(out)
    }

    pub(crate) async fn read_since(&self, p: &Port, a: &Value) -> Result<Value, HttpError> {
        let max = num_or(a.get("max_bytes"), 16.0 * 1024.0).clamp(1.0, 256.0 * 1024.0);
        // No cursor: the most recent max bytes.
        let cursor = match a.get("cursor").filter(|v| !v.is_null() && v.as_str() != Some("")) {
            None => {
                let st = p.lock();
                (st.ring.start() as f64).max(st.ring.end as f64 - max)
            }
            Some(v) => js_number(v),
        };
        if !cursor.is_finite() {
            return Err(HttpError::bad_request("cursor must be a number"));
        }
        let wait = a.get("wait_ms").map(js_number).filter(|n| !n.is_nan()).unwrap_or(0.0).clamp(0.0, MAX_WAIT_MS);
        let mut tick = p.tick.subscribe();
        let need_wait = {
            let st = p.lock();
            wait > 0.0 && st.ring.end as f64 <= cursor && st.open()
        };
        if need_wait {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(wait as u64);
            loop {
                tokio::select! {
                    r = tick.changed() => if r.is_err() { break },
                    _ = tokio::time::sleep_until(deadline) => break,
                }
                let st = p.lock();
                if st.ring.end as f64 > cursor || !st.open() {
                    break;
                }
            }
        }
        let st = p.lock();
        let got = st.ring.since(cursor, max);
        let fmt = a.get("format").filter(|v| truthy(v)).map(js_string).unwrap_or_else(|| "text".into());
        let data = match fmt.as_str() {
            "hex" => hex_spaced(&got.data),
            "base64" => base64_encode(&got.data),
            _ => decode_out(&got.data, a.get("raw").is_some_and(truthy)),
        };
        Ok(json!({
            "id": p.id,
            "from": got.from,
            "cursor": got.to,
            "more": got.to < st.ring.end,
            "truncated": got.truncated,
            "bytes": got.data.len(),
            "open": st.open(),
            "format": fmt,
            "data": data,
        }))
    }
}
