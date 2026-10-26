//! The IR panel: consumer infrared through the kernel's LIRC devices. Capture and decode
//! what a remote sends (NEC, Samsung, Sony, RC5, RC6, JVC through the shared
//! `benchbob_ir` codec, anything else as raw timings and Pronto hex), keep a named-codes
//! library (devices with buttons) in settings, and send codes, learned or typed.
//!
//! - Receivers and transmitters are the `gpio-ir` / `gpio-ir-tx` / `pwm-ir-tx` overlays,
//!   which the GPIO panel's interfaces stage (with `rc-map-name=rc-empty`: received codes
//!   stay inside BenchBob and never become keypresses).
//! - Sending is an output: it needs GPIO output control on, like every other output.
//! - A thread per receiver reads mode2 samples and splits them into frames at long spaces;
//!   the last [`CAPTURES`] frames are kept, and `capture` waits for the next one.
//!
//! `BENCHBOB_LIRC_DIR` / `BENCHBOB_LIRC_SYS` point at test copies of /dev and
//! /sys/class/lirc.

pub mod lirc;

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::Method;
use benchbob_core::http::{HttpError, json as reply};
use benchbob_core::timeline::now_ms;
use benchbob_core::{Ctx, Driver, Json, Request, Route, StopReason, Tool};
use benchbob_gpio::GpioHandle;
use benchbob_ir::{Code, MAX_FRAME, Protocol, pronto};
use serde_json::{Value, json};

use crate::lirc::{Dev, Framer, Rx, Sample};

/// Frames kept for the dashboard and `ir_capture`.
pub const CAPTURES: usize = 32;
/// A space this long ends a frame (NEC's gap is ~40 ms, Sony's ~25 ms).
const GAP_US: u32 = 15_000;
const DUTY: u32 = 33;
const LIBRARY: &str = "library";

#[derive(Clone, Debug)]
pub struct Capture {
    pub seq: u64,
    pub at_ms: u64,
    pub device: String,
    pub timings: Vec<u32>,
    pub code: Option<Code>,
}

pub struct Ir {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    gpio: GpioHandle,
    ctx: OnceLock<Ctx>,
    dev_dir: PathBuf,
    sys_dir: PathBuf,
    captures: Mutex<VecDeque<Capture>>,
    seq: AtomicU64,
    notify: tokio::sync::Notify,
    epoch: AtomicU64,
    /// Receivers with a reader thread, by path.
    reading: Mutex<HashSet<PathBuf>>,
}

impl Ir {
    pub fn new(gpio: GpioHandle) -> Self {
        let env = |k, d: &str| PathBuf::from(benchbob_core::paths::env(k).unwrap_or_else(|| d.into()));
        Self::with_dirs(gpio, env("BENCHBOB_LIRC_DIR", "/dev"), env("BENCHBOB_LIRC_SYS", "/sys/class/lirc"))
    }

    pub fn with_dirs(gpio: GpioHandle, dev_dir: PathBuf, sys_dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                gpio,
                ctx: OnceLock::new(),
                dev_dir,
                sys_dir,
                captures: Mutex::new(VecDeque::new()),
                seq: AtomicU64::new(0),
                notify: tokio::sync::Notify::new(),
                epoch: AtomicU64::new(0),
                reading: Mutex::new(HashSet::new()),
            }),
        }
    }
}

fn code_json(c: &Code) -> Value {
    json!({ "protocol": c.protocol.name(), "address": c.address, "command": c.command, "toggle": c.toggle })
}

/// Pronto hex for a captured frame (its gap made explicit).
fn pronto_of(timings: &[u32], carrier: u32) -> Option<String> {
    let mut t = timings.to_vec();
    if t.len() % 2 == 1 {
        t.push(GAP_US * 3);
    }
    let mut buf = vec![0u8; 5 * (t.len() + 4) + 8];
    let n = pronto::format(carrier, &t, &[], &mut buf)?;
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

fn capture_json(c: &Capture) -> Value {
    let carrier = c.code.map_or(38_000, |k| k.protocol.carrier_hz());
    json!({
        "seq": c.seq,
        "at": c.at_ms,
        "device": c.device,
        "code": c.code.as_ref().map(code_json),
        "timings": c.timings,
        "carrier": carrier,
        "pronto": pronto_of(&c.timings, carrier),
    })
}

/// What to send, resolved from a request: frames to send in order, the carrier, and the
/// gap between frame starts (µs).
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub frames: Vec<Vec<u32>>,
    pub carrier: u32,
    pub gap_us: u32,
    pub label: String,
}

fn bad(m: impl Into<String>) -> HttpError {
    HttpError::bad_request(m)
}

fn num(a: &Value, k: &str) -> Option<u64> {
    a.get(k).and_then(|v| {
        v.as_u64().or_else(|| v.as_str().and_then(|s| s.strip_prefix("0x").map_or(s.parse().ok(), |h| u64::from_str_radix(h, 16).ok())))
    })
}

/// A send request into frames: a code (protocol + address + command), raw timings
/// (+ carrier), Pronto hex, or a library button (`device` + `button`).
pub fn plan(a: &Value, library: &Value) -> Result<Plan, HttpError> {
    if let (Some(d), Some(b)) = (a.get("device").and_then(Value::as_str), a.get("button").and_then(Value::as_str)) {
        let code = library
            .as_array()
            .into_iter()
            .flatten()
            .find(|x| x["device"].as_str() == Some(d))
            .and_then(|x| x["buttons"].as_array()?.iter().find(|y| y["name"].as_str() == Some(b)))
            .map(|y| y["code"].clone())
            .ok_or_else(|| HttpError::new(404, format!("No button \"{b}\" on \"{d}\" in the IR library")))?;
        let mut p = plan(&code, &Value::Null)?;
        p.label = format!("{d} / {b}");
        return Ok(p);
    }
    let repeat = num(a, "repeat").unwrap_or(1).clamp(1, 20) as usize;
    if let Some(name) = a.get("protocol").and_then(Value::as_str) {
        let protocol = Protocol::parse(name).ok_or_else(|| {
            bad(format!("protocol must be one of {}", benchbob_ir::PROTOCOLS.iter().map(|p| p.name()).collect::<Vec<_>>().join(", ")))
        })?;
        let c = Code {
            protocol,
            address: num(a, "address").unwrap_or(0) as u32,
            command: num(a, "command").ok_or_else(|| bad("command is required"))? as u32,
            toggle: a.get("toggle").and_then(Value::as_bool).unwrap_or(false),
        };
        let mut t = [0u32; MAX_FRAME];
        let n = benchbob_ir::encode(&c, &mut t).ok_or_else(|| {
            let (am, cm) = protocol.limits();
            bad(format!("{} takes an address up to 0x{am:X} and a command up to 0x{cm:X}", protocol.name()))
        })?;
        let (times, gap) = protocol.repeats();
        // NEC-style: hold = the frame, then repeat codes.
        let mut frames = vec![t[..n].to_vec(); times as usize];
        for _ in 1..repeat {
            if matches!(protocol, Protocol::Nec | Protocol::NecExt) {
                frames.push(vec![9000, 2250, 562]);
            } else {
                frames.extend(std::iter::repeat_n(t[..n].to_vec(), times as usize));
            }
        }
        return Ok(Plan {
            frames,
            carrier: protocol.carrier_hz(),
            gap_us: gap,
            label: format!("{} 0x{:X} 0x{:X}", protocol.name(), c.address, c.command),
        });
    }
    if let Some(p) = a.get("pronto").and_then(Value::as_str) {
        let mut out = vec![0u32; 1024];
        let (carrier, once, rep) =
            pronto::parse(p, &mut out).ok_or_else(|| bad("pronto must be learned-code Pronto hex (starting 0000)"))?;
        let mut frames = Vec::new();
        if once > 0 {
            frames.push(out[..once].to_vec());
        }
        for _ in 0..repeat.max(if once == 0 { 1 } else { 0 }) {
            if rep > 0 {
                frames.push(out[once..once + rep].to_vec());
            }
        }
        let gap = frames.iter().map(|f| f.iter().sum::<u32>()).max().unwrap_or(0) + 10_000;
        return Ok(Plan { frames, carrier, gap_us: gap, label: "Pronto code".into() });
    }
    if let Some(raw) = a.get("timings").or_else(|| a.get("raw")).and_then(Value::as_array) {
        let t: Vec<u32> = raw
            .iter()
            .map(|v| v.as_u64().filter(|&d| (1..=500_000).contains(&d)).map(|d| d as u32))
            .collect::<Option<_>>()
            .ok_or_else(|| bad("timings must be µs, 1–500000"))?;
        if t.is_empty() || t.len() > 1024 {
            return Err(bad("timings must have 1–1024 entries"));
        }
        let carrier = num(a, "carrier").unwrap_or(38_000).clamp(10_000, 500_000) as u32;
        let gap = t.iter().sum::<u32>() + GAP_US * 3;
        return Ok(Plan { frames: vec![t; repeat], carrier, gap_us: gap, label: "raw timings".into() });
    }
    Err(bad("Give protocol + address + command, timings (+ carrier), pronto, or device + button"))
}

impl Inner {
    fn devices(&self) -> Vec<Dev> {
        lirc::list(&self.dev_dir, &self.sys_dir)
    }

    fn library(&self) -> Value {
        self.ctx.get().and_then(|c| c.setting(LIBRARY)).unwrap_or(json!([]))
    }

    fn set_library(&self, v: Value) {
        if let Some(c) = self.ctx.get() {
            c.set_setting(LIBRARY, v);
            c.changed();
        }
    }

    pub(crate) fn record(&self, device: &str, timings: Vec<u32>) {
        let code = benchbob_ir::decode(&timings);
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let mut c = self.captures.lock().unwrap();
        c.push_back(Capture { seq, at_ms: now_ms(), device: device.into(), timings, code });
        while c.len() > CAPTURES {
            c.pop_front();
        }
        drop(c);
        self.notify.notify_waiters();
    }

    fn view(&self) -> Value {
        let devs = self.devices();
        let caps: Vec<Value> = self.captures.lock().unwrap().iter().rev().map(capture_json).collect();
        json!({
            "outputEnabled": self.gpio.output_enabled(),
            "receivers": devs.iter().filter(|d| d.can_receive()).map(|d| json!({ "path": d.path.display().to_string(), "driver": d.driver })).collect::<Vec<_>>(),
            "transmitters": devs.iter().filter(|d| d.can_send()).map(|d| json!({ "path": d.path.display().to_string(), "driver": d.driver, "carrier": d.features & lirc::CAN_SET_SEND_CARRIER != 0 })).collect::<Vec<_>>(),
            "protocols": benchbob_ir::PROTOCOLS.iter().map(|p| json!({ "name": p.name(), "carrier": p.carrier_hz(), "maxAddress": p.limits().0, "maxCommand": p.limits().1 })).collect::<Vec<_>>(),
            "captures": caps,
            "library": self.library(),
        })
    }

    /// Wait for the next frame after now, up to `ms`.
    async fn capture(&self, ms: u64) -> Result<Value, HttpError> {
        if !self.devices().iter().any(Dev::can_receive) {
            return Err(HttpError::new(409, "No IR receiver: add one in the GPIO panel (Interfaces → IR) and reboot"));
        }
        let from = self.seq.load(Ordering::SeqCst);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms.clamp(100, 60_000));
        loop {
            let notified = self.notify.notified();
            if let Some(c) = self.captures.lock().unwrap().iter().find(|c| c.seq > from) {
                return Ok(capture_json(c));
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(HttpError::new(408, "No IR frame arrived; point the remote at the receiver and try again"));
            }
        }
    }

    fn send(&self, a: &Value) -> Result<Value, HttpError> {
        if !self.gpio.output_enabled() {
            return Err(HttpError::new(
                403,
                "GPIO output control is off. Enable it first (POST /api/gpio/output-control with enabled: true, confirm: true).",
            ));
        }
        let p = plan(a, &self.library())?;
        let devs = self.devices();
        let want = a.get("transmitter").and_then(Value::as_str);
        let dev = devs
            .iter()
            .filter(|d| d.can_send())
            .find(|d| want.is_none_or(|w| d.path.display().to_string() == w))
            .ok_or_else(|| HttpError::new(409, "No IR transmitter: add one in the GPIO panel (Interfaces → IR) and reboot"))?;
        let t0 = std::time::Instant::now();
        for (k, f) in p.frames.iter().enumerate() {
            if k > 0 {
                let due = Duration::from_micros(p.gap_us as u64 * k as u64);
                if let Some(rest) = due.checked_sub(t0.elapsed()) {
                    std::thread::sleep(rest);
                }
            }
            lirc::send(dev, p.carrier, DUTY, f).map_err(|e| HttpError::new(502, e))?;
        }
        if let Some(c) = self.ctx.get() {
            c.event(&format!("IR sent: {} ({} frame{})", p.label, p.frames.len(), if p.frames.len() == 1 { "" } else { "s" }), None);
        }
        Ok(
            json!({ "ok": true, "sent": p.label, "frames": p.frames.len(), "carrier": p.carrier, "transmitter": dev.path.display().to_string() }),
        )
    }

    /// Save a button (its code given, or taken from capture `fromCapture`), or delete a
    /// button / device.
    fn codes(&self, a: &Value) -> Result<Value, HttpError> {
        let device =
            a.get("device").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| bad("device is required"))?;
        let button = a.get("button").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
        let mut lib = self.library().as_array().cloned().unwrap_or_default();
        if a.get("delete").and_then(Value::as_bool) == Some(true) {
            let before = lib.clone();
            match button {
                None => lib.retain(|d| d["device"].as_str() != Some(device)),
                Some(b) => {
                    for d in lib.iter_mut().filter(|d| d["device"].as_str() == Some(device)) {
                        if let Some(bs) = d["buttons"].as_array_mut() {
                            bs.retain(|x| x["name"].as_str() != Some(b));
                        }
                    }
                }
            }
            if lib == before {
                return Err(HttpError::new(404, "Nothing like that in the IR library"));
            }
            self.set_library(Value::Array(lib));
            return Ok(json!({ "ok": true, "library": self.library() }));
        }
        let button = button.ok_or_else(|| bad("button is required"))?;
        let code = if let Some(seq) = a.get("fromCapture").and_then(Value::as_u64) {
            let caps = self.captures.lock().unwrap();
            let c = caps
                .iter()
                .find(|c| c.seq == seq)
                .ok_or_else(|| HttpError::new(404, format!("No capture {seq} (only the last {CAPTURES} are kept)")))?;
            match c.code {
                Some(k) if k.protocol != Protocol::NecRepeat => code_json(&k),
                _ => json!({ "timings": c.timings, "carrier": 38_000 }),
            }
        } else {
            let c = a.get("code").cloned().ok_or_else(|| bad("Give code (as for ir_send) or fromCapture"))?;
            plan(&c, &Value::Null)?; // must be sendable
            c
        };
        let entry = json!({ "name": button, "code": code });
        match lib.iter_mut().find(|d| d["device"].as_str() == Some(device)) {
            Some(d) => {
                let bs = d["buttons"].as_array_mut().ok_or_else(|| HttpError::new(500, "library entry has no buttons"))?;
                match bs.iter_mut().find(|x| x["name"].as_str() == Some(button)) {
                    Some(x) => *x = entry,
                    None => bs.push(entry),
                }
            }
            None => lib.push(json!({ "device": device, "buttons": [entry] })),
        }
        self.set_library(Value::Array(lib));
        Ok(json!({ "ok": true, "library": self.library() }))
    }

    /// Start a reader for each receiver that doesn't have one (receivers appear when an
    /// overlay loads, or a USB IR receiver is plugged in).
    fn readers(self: &Arc<Self>) {
        let epoch = self.epoch.load(Ordering::SeqCst);
        while self.epoch.load(Ordering::SeqCst) == epoch {
            for d in self.devices().into_iter().filter(Dev::can_receive) {
                if self.reading.lock().unwrap().insert(d.path.clone()) {
                    let me = self.clone();
                    let _ = std::thread::Builder::new().name("ir-rx".into()).spawn(move || me.read_loop(d.path, epoch));
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    fn read_loop(self: Arc<Self>, path: PathBuf, epoch: u64) {
        let name = path.display().to_string();
        match Rx::open(&path) {
            Ok(mut rx) => {
                tracing::info!("[ir] reading {name}");
                let mut framer = Framer::new(GAP_US);
                let mut buf: Vec<Sample> = Vec::new();
                while self.epoch.load(Ordering::SeqCst) == epoch {
                    buf.clear();
                    match rx.read(200, &mut buf) {
                        Ok(_) => {
                            for s in buf.drain(..) {
                                if let Some(f) = framer.push(s) {
                                    self.record(&name, f);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("[ir] {name}: {e}");
                            break;
                        }
                    }
                }
            }
            Err(e) => tracing::warn!("[ir] {name}: {e}"),
        }
        self.reading.lock().unwrap().remove(&path);
    }

    fn summary(&self) -> Value {
        let devs = self.devices();
        let (rx, tx) = (devs.iter().filter(|d| d.can_receive()).count(), devs.iter().filter(|d| d.can_send()).count());
        let lib = self.library().as_array().map_or(0, Vec::len);
        let parts: Vec<String> = [(rx, "receiver"), (tx, "transmitter")]
            .iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, w)| format!("{n} {w}{}", if *n == 1 { "" } else { "s" }))
            .collect();
        json!({ "active": rx + tx > 0 || lib > 0, "summary": if parts.is_empty() { "no IR devices".to_string() } else { parts.join(", ") } })
    }
}

#[async_trait]
impl Driver for Ir {
    fn id(&self) -> &'static str {
        "ir"
    }
    fn title(&self) -> &'static str {
        "IR"
    }
    fn order(&self) -> i32 {
        66
    }
    fn description(&self) -> &'static str {
        "Infrared remotes: capture and decode (NEC, Samsung, Sony, RC5, RC6, JVC, raw/Pronto), a library of named codes, and sending"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let _ = self.inner.ctx.set(ctx);
        let me = self.inner.clone();
        std::thread::Builder::new().name("ir".into()).spawn(move || me.readers())?;
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) {
        self.inner.epoch.fetch_add(1, Ordering::SeqCst);
    }

    fn status(&self) -> Option<Json> {
        Some(self.inner.summary())
    }

    fn routes(&self) -> Vec<Route> {
        let i = self.inner.clone();
        let blocking = |m: Method, path: &str, doc: Value, f: fn(&Inner, &Value) -> Result<Value, HttpError>| {
            let i = i.clone();
            Route::new(m, path, Some(doc), move |r: Request| {
                let i = i.clone();
                async move {
                    let a = r.json()?;
                    tokio::task::spawn_blocking(move || f(&i, &a)).await.map_err(|e| HttpError::new(500, e.to_string()))?.and_then(reply)
                }
            })
        };
        let cap = i.clone();
        vec![
            blocking(
                Method::GET,
                "/api/ir",
                json!(
                    "IR: receivers and transmitters (LIRC devices), protocols, the last captures (decoded code, raw timings, Pronto) and the library of named codes"
                ),
                |i, _| Ok(i.view()),
            ),
            Route::new(
                Method::POST,
                "/api/ir/capture",
                Some(
                    json!({ "description": "Wait for the next IR frame (up to timeoutMs, default 10000) and return it decoded: code {protocol, address, command, toggle} or null, timings, carrier, pronto", "body": { "timeoutMs": 10000 } }),
                ),
                move |r: Request| {
                    let i = cap.clone();
                    async move {
                        let a = r.json()?;
                        i.capture(a.get("timeoutMs").and_then(Value::as_u64).unwrap_or(10_000)).await.and_then(reply)
                    }
                },
            ),
            blocking(
                Method::POST,
                "/api/ir/send",
                json!({ "description": "Send IR (needs GPIO output control on): protocol + address + command (+ toggle, repeat), timings (µs, mark first) + carrier, pronto, or device + button from the library", "body": { "protocol": "nec", "address": 4, "command": 8 } }),
                |i, a| i.send(a),
            ),
            blocking(
                Method::POST,
                "/api/ir/library",
                json!({ "description": "Save a named code: device + button + code (as for send) or fromCapture (a capture's seq); delete: true removes a button, or the device without a button", "body": { "device": "TV", "button": "power", "fromCapture": 12 } }),
                |i, a| i.codes(a),
            ),
        ]
    }

    fn tools(&self) -> Vec<Tool> {
        let mk = |name: &str, desc: &str, schema: Value, f: fn(&Inner, &Value) -> Result<Value, HttpError>| {
            let i = self.inner.clone();
            Tool::new(name, desc, schema, move |a: Value| {
                let i = i.clone();
                async move { tokio::task::spawn_blocking(move || f(&i, &a)).await.map_err(|e| HttpError::new(500, e.to_string()))? }
            })
        };
        let cap = self.inner.clone();
        vec![
            mk(
                "ir_list",
                "List BenchBob's IR receivers and transmitters, the protocols it decodes and sends, the last captured frames (decoded, raw, Pronto) and the library of named codes (devices with buttons).",
                json!({ "type": "object", "properties": {} }),
                |i, _| Ok(i.view()),
            ),
            Tool::new(
                "ir_capture",
                "Wait for the next infrared frame at a receiver (ask the user to press the remote's button) and return it decoded (protocol, address, command) when it's a known protocol, plus the raw timings and Pronto hex.",
                json!({ "type": "object", "properties": { "timeoutMs": { "type": "integer", "minimum": 100, "maximum": 60000 } } }),
                move |a: Value| {
                    let i = cap.clone();
                    async move { i.capture(a.get("timeoutMs").and_then(Value::as_u64).unwrap_or(10_000)).await }
                },
            ),
            mk(
                "ir_send",
                "Send an infrared code (needs GPIO output control on): protocol (nec, necx, samsung, sony12/15/20, rc5, rc6, jvc) + address + command, or timings in µs (mark first) + carrier Hz, or pronto hex, or device + button from the library. repeat holds the button (NEC sends repeat codes).",
                json!({ "type": "object", "properties": {
                    "protocol": { "type": "string" }, "address": { "type": ["integer", "string"] }, "command": { "type": ["integer", "string"] }, "toggle": { "type": "boolean" },
                    "repeat": { "type": "integer", "minimum": 1, "maximum": 20 }, "timings": { "type": "array", "items": { "type": "integer" } }, "carrier": { "type": "integer" },
                    "pronto": { "type": "string" }, "device": { "type": "string" }, "button": { "type": "string" }, "transmitter": { "type": "string" },
                } }),
                |i, a| i.send(a),
            ),
            mk(
                "ir_codes",
                "Save a named IR code in the library (device + button + code as for ir_send, or fromCapture with a capture's seq from ir_capture/ir_list), or delete a button (delete: true) or a whole device (delete: true, no button).",
                json!({ "type": "object", "properties": {
                    "device": { "type": "string" }, "button": { "type": "string" }, "code": { "type": "object" }, "fromCapture": { "type": "integer" }, "delete": { "type": "boolean" },
                }, "required": ["device"] }),
                |i, a| i.codes(a),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpio() -> GpioHandle {
        let boards = benchbob_boards::Boards::builtin();
        let input = benchbob_boards::DetectInput { compatible: vec!["brcm,bcm2711".into()], ..Default::default() };
        let board = Arc::new(benchbob_boards::describe(boards.get("rpi").unwrap(), &input, true));
        benchbob_gpio::Gpio::new(board).handle()
    }

    #[test]
    fn send_plans() {
        let p = plan(&json!({ "protocol": "nec", "address": 4, "command": "0x08", "repeat": 3 }), &Value::Null).unwrap();
        assert_eq!(p.carrier, 38_000);
        assert_eq!(p.frames.len(), 3);
        assert_eq!(p.frames[0].len(), 67);
        assert_eq!(p.frames[1], [9000, 2250, 562], "held: repeat codes");
        let p = plan(&json!({ "protocol": "sony12", "address": 1, "command": 21 }), &Value::Null).unwrap();
        assert_eq!((p.frames.len(), p.carrier, p.gap_us), (3, 40_000, 45_000));
        assert_eq!(plan(&json!({ "protocol": "nec", "address": 300, "command": 1 }), &Value::Null).unwrap_err().status, 400);
        assert_eq!(plan(&json!({ "protocol": "x", "command": 1 }), &Value::Null).unwrap_err().status, 400);
        let p = plan(&json!({ "timings": [9000, 4500, 560], "carrier": 36000, "repeat": 2 }), &Value::Null).unwrap();
        assert_eq!((p.frames.len(), p.carrier), (2, 36_000));
        let p = plan(&json!({ "pronto": "0000 006D 0002 0000 0156 00AB 0015 0F00" }), &Value::Null).unwrap();
        assert_eq!(p.frames.len(), 1);
        assert_eq!(p.frames[0].len(), 4);
        let lib =
            json!([{ "device": "TV", "buttons": [{ "name": "power", "code": { "protocol": "samsung", "address": 7, "command": 2 } }] }]);
        let p = plan(&json!({ "device": "TV", "button": "power" }), &lib).unwrap();
        assert_eq!(p.label, "TV / power");
        assert_eq!(plan(&json!({ "device": "TV", "button": "mute" }), &lib).unwrap_err().status, 404);
    }

    #[test]
    fn captures_decode_and_feed_the_library() {
        let ir = Ir::with_dirs(gpio(), "/nonexistent".into(), "/nonexistent".into());
        let i = &ir.inner;
        let mut t = [0u32; MAX_FRAME];
        let n = benchbob_ir::encode(&Code { protocol: Protocol::Rc5, address: 5, command: 12, toggle: true }, &mut t).unwrap();
        i.record("/dev/lirc0", t[..n].to_vec());
        i.record("/dev/lirc0", vec![1000, 1000, 1000]);
        let v = i.view();
        let caps = v["captures"].as_array().unwrap();
        assert_eq!(caps.len(), 2);
        assert_eq!(caps[1]["code"]["protocol"], "rc5"); // newest first
        assert_eq!(caps[1]["code"]["command"], 12);
        assert_eq!(caps[0]["code"], Value::Null);
        assert!(caps[0]["pronto"].as_str().unwrap().starts_with("0000 006D 0002 0000"));
        // Without a ctx the library can't persist; the plan for a saved code still checks.
        assert_eq!(i.codes(&json!({ "device": "TV" })).unwrap_err().status, 400);
        assert_eq!(i.codes(&json!({ "device": "TV", "button": "x", "fromCapture": 99 })).unwrap_err().status, 404);
        // Sending needs output control.
        assert_eq!(i.send(&json!({ "protocol": "nec", "command": 1 })).unwrap_err().status, 403);
    }

    #[tokio::test]
    async fn capture_waits_for_the_next_frame() {
        let t = tempfile::tempdir().unwrap();
        let ir = Ir::with_dirs(gpio(), t.path().into(), t.path().into());
        // No receiver: refused straight away.
        assert_eq!(ir.inner.capture(200).await.unwrap_err().status, 409);
    }
}
