//! The LED strips panel: addressable LED strings (WS2812 family, APA102, WS2801) on a
//! Linux board's header, with the same API and MCP tools as a BenchBob node
//! (`/api/leds`, `leds_*`).
//!
//! The string model (config validation, patterns, colour order, gamma, power budget,
//! rendering) is the nodes' own (`benchbob_proto::api::Leds`, `benchbob_pixels`), so both
//! behave the same. This crate adds what Linux needs:
//!
//! - **Hardware** ([`hw`], [`pwm`]): spidev for one-wire strings (a WS2812 waveform on
//!   MOSI) and clocked ones on a bus's MOSI + SCLK; on a Pi 1–4, one-wire strings on the
//!   PWM pins (GPIO12/18, GPIO13/19) through a privileged PWM + DMA helper; clocked strings
//!   bit-banged through the GPIO uAPI on any free header GPIO. Pi 5 PIO comes later
//!   (plans/gpio-led-strips.md).
//! - **Pins** are BCM GPIO numbers on a Raspberry Pi and physical header pins elsewhere
//!   (the GPIO panel's numbering). A running string claims its pins in the GPIO panel,
//!   which then shows them as "used by LED strip <name>" and won't drive them.
//! - **Output control** is the GPIO panel's switch: strings light only while it's on.
//! - **Settings**: string configs (and their pattern) persist in peripherals.json, like
//!   the other panels' settings; pixels set through the API don't. After a restart the
//!   strings come back, dark until output control is turned on again.
//!
//! - **Network input** ([`net`]): strings with a `network` input listen for sACN, Art-Net
//!   or DDP; the ports are only bound while some string wants them.
//!
//! A worker thread renders at up to 50 fps and does the (blocking) device writes.

// The nodes' ApiErr carries its message inline (no heap), so its Results are large.
#![allow(clippy::result_large_err)]

pub mod hw;
pub mod net;
pub mod pwm;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::http::Method;
use benchbob_core::http::{HttpError, json as reply};
use benchbob_core::{Ctx, Driver, Json, Request, Route, StopReason, Tool};
use benchbob_gpio::GpioHandle;
use benchbob_pixels::encode;
use benchbob_pixels::timing::{SpiFit, Timing};
use benchbob_proto::api::leds::{MAX_PIXELS, MAX_STRINGS, StringConfig};
use benchbob_proto::api::{ApiErr, BoardInfo, Leds as Model, LedsInfo, PinInfo};
use benchbob_proto::json::{self as pjson, Buf, Json as PJson};
use serde_json::{Value, json};

use crate::hw::{BitBang, SpiBus, SpiClock, SpiOut};

/// How often animated strings get a new frame (and the gap that latches a frame).
const FRAME_MS: u64 = 20;
const SETTING: &str = "strings";

static LEDS_INFO: LedsInfo = LedsInfo {
    max_strings: MAX_STRINGS as u8,
    max_pixels: MAX_PIXELS as u16,
    one_wire: true,
    clocked: true,
    check_timing,
    max_clock_hz: 20_000_000,
};

/// Any timing SPI could draw with an exact clock; the real controller's fit is checked
/// when the string is set up.
fn check_timing(t: &Timing) -> Result<(), &'static str> {
    SpiFit::find(t, |hz| hz).map(|_| ()).ok_or("SPI can't draw this timing within 150 ns at any clock")
}

pub struct Leds {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    gpio: GpioHandle,
    info: &'static BoardInfo,
    model: Mutex<Model>,
    ctx: OnceLock<Ctx>,
    started: Instant,
    /// Bumped on stop, so the worker exits.
    epoch: AtomicU64,
    /// Where spidev devices are (`BENCHBOB_SPIDEV_DIR` for tests).
    dev_dir: std::path::PathBuf,
    rx: Mutex<net::RxStatus>,
}

impl Leds {
    pub fn new(gpio: GpioHandle) -> Self {
        let dev_dir = benchbob_core::paths::env("BENCHBOB_SPIDEV_DIR").unwrap_or_else(|| "/dev".into()).into();
        Self::with_dev_dir(gpio, dev_dir)
    }

    /// With spidev devices looked for in `dev_dir` instead of /dev.
    pub fn with_dev_dir(gpio: GpioHandle, dev_dir: std::path::PathBuf) -> Self {
        let board = gpio.board();
        let pins: Vec<PinInfo> = gpio
            .header_gpios()
            .into_iter()
            .filter(|&(id, _)| id <= u8::MAX as u32)
            .map(|(id, phys)| PinInfo { gpio: id as u8, header: Some(phys as u8), func: None, note: None })
            .collect();
        let info: &'static BoardInfo = Box::leak(Box::new(BoardInfo {
            board: Box::leak(board.id.clone().into_boxed_str()),
            board_name: Box::leak(board.name.clone().into_boxed_str()),
            chip: Box::leak(board.def.soc_of(&board.input).unwrap_or_default().into_boxed_str()),
            firmware_version: "",
            build: "",
            pins: Box::leak(pins.into_boxed_slice()),
            i2c: &[],
            leds: Some(&LEDS_INFO),
            onboard_leds: &[],
        }));
        Self {
            inner: Arc::new(Inner {
                gpio,
                info,
                model: Mutex::new(Model::new()),
                ctx: OnceLock::new(),
                started: Instant::now(),
                epoch: AtomicU64::new(0),
                dev_dir,
                rx: Mutex::default(),
            }),
        }
    }
}

fn api_err(e: ApiErr) -> HttpError {
    HttpError::new(e.status, e.message())
}

/// Run `f` with a proto JSON writer and parse what it wrote.
fn write_json(f: impl FnOnce(&mut PJson<'_, '_>) -> Result<(), ApiErr>) -> Result<Value, HttpError> {
    let mut mem = vec![0u8; 64 * 1024];
    let mut buf = Buf::new(&mut mem);
    f(&mut PJson::new(&mut buf)).map_err(api_err)?;
    if buf.overflowed() {
        return Err(HttpError::new(500, "LED view too large"));
    }
    serde_json::from_str(buf.as_str()).map_err(|e| HttpError::new(500, format!("LED view: {e}")))
}

impl Inner {
    /// Push a status refresh (no-op before init, as in tests).
    fn changed(&self) {
        if let Some(c) = self.ctx.get() {
            c.changed();
        }
    }

    fn now(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn pin_name(&self, p: u32) -> String {
        if self.gpio.is_pi() { format!("GPIO{p}") } else { format!("pin {p}") }
    }

    fn bcm2711(&self) -> bool {
        self.soc().as_deref() == Some("BCM2711")
    }

    fn soc(&self) -> Option<String> {
        let b = self.gpio.board();
        b.def.soc_of(&b.input)
    }

    fn buses(&self) -> Vec<SpiBus> {
        if !self.gpio.is_pi() {
            return vec![]; // no SPI pin maps for other boards yet
        }
        let b = self.bcm2711();
        hw::spi_buses(&self.dev_dir, |n| hw::pi_spi_pins(n, b))
    }

    fn spi_clock(&self) -> Result<SpiClock, String> {
        match self.soc().as_deref() {
            Some("BCM2712") => Ok(SpiClock::Rp1),
            // Every other Pi (and one whose SoC wasn't recognised) has spi-bcm2835.
            _ if self.gpio.is_pi() => hw::pi_fixed_core_hz().map(|core_hz| SpiClock::Bcm2835 { core_hz }),
            _ => Ok(SpiClock::Exact),
        }
    }

    /// Why `c` can't run here, before it's accepted (the worker checks pins again when it
    /// sets the string up).
    fn check(&self, c: &StringConfig, model: &Model, me: Option<i64>) -> Result<(), String> {
        let buses = self.buses();
        let bus = buses.iter().find(|b| b.mosi == c.pin as u32 && (!c.chip.clocked() || c.clock_pin.map(u32::from) == Some(b.sclk)));
        match bus {
            Some(b) => {
                if c.invert {
                    return Err("SPI can't invert its output: drop invert, or use a non-inverting level shifter".into());
                }
                // The bus's other pin is busy too.
                if let Some(other) = model.owner(b.sclk as u8)
                    && model.slot_of(me.unwrap_or(-1)).and_then(|i| model.job(i)).is_none_or(|j| j.config.name.as_str() != other)
                {
                    return Err(format!("{} (SPI{} SCLK) is used by LED string {other}", self.pin_name(b.sclk), b.bus));
                }
                Ok(())
            }
            None if c.chip.clocked() => Ok(()),
            None if self.pwm_ok() && pwm::pwm_channel(c.pin as u32).is_some() => {
                // One string per PWM channel (GPIO12 and 18 are both PWM0).
                let ch = pwm::pwm_channel(c.pin as u32);
                let mine = model.slot_of(me.unwrap_or(-1));
                for i in (0..MAX_STRINGS).filter(|&i| Some(i) != mine) {
                    if let Some(j) = model.job(i)
                        && !j.config.chip.clocked()
                        && pwm::pwm_channel(j.config.pin as u32) == ch
                    {
                        return Err(format!(
                            "PWM{} already drives LED string {} (GPIO12/18 are PWM0, GPIO13/19 PWM1)",
                            ch.unwrap_or(0),
                            j.config.name.as_str()
                        ));
                    }
                }
                Ok(())
            }
            None => Err(self.one_wire_hint(&buses)),
        }
    }

    /// A Pi 1–4: its PWM can drive one-wire strings (the Pi 5's RP1 PWM can't, this way).
    fn pwm_ok(&self) -> bool {
        self.gpio.is_pi() && pwm::soc_has_pwm_dma(self.soc().as_deref())
    }

    fn one_wire_hint(&self, buses: &[SpiBus]) -> String {
        if !self.gpio.is_pi() {
            return "one-wire strings (WS2812 family) need SPI, and BenchBob has no SPI pin map for this board yet; clocked strings (APA102, WS2801) work on any free GPIO".into();
        }
        let mut pins: Vec<String> = buses.iter().map(|b| format!("GPIO{} (SPI{})", b.mosi, b.bus)).collect();
        if self.pwm_ok() {
            pins.push("GPIO12/18 (PWM0)".into());
            pins.push("GPIO13/19 (PWM1)".into());
        }
        if buses.is_empty() && !self.pwm_ok() {
            return "one-wire strings (WS2812 family) need an SPI MOSI pin: enable SPI0 in the GPIO panel (then reboot) and use GPIO10"
                .into();
        }
        let more = if buses.is_empty() { "; or enable SPI0 in the GPIO panel for GPIO10" } else { "" };
        format!("one-wire strings (WS2812 family) need an SPI MOSI or PWM pin: {}{more}", pins.join(", "))
    }

    /// `capabilities.pinInfo`: every header GPIO, what kind of string it can carry and
    /// why not, for the dashboard's pin picker.
    fn pin_info(&self, model: &Model) -> Value {
        let buses = self.buses();
        self.gpio.refresh_lines();
        Value::Array(
            self.gpio
                .header_gpios()
                .into_iter()
                .map(|(id, phys)| {
                    let owner = model.owner(id as u8).map(|n| format!("LED string {n}"));
                    let bus = buses.iter().find(|b| b.mosi == id || b.sclk == id);
                    let pwm_ch = if self.pwm_ok() { pwm::pwm_channel(id) } else { None };
                    // Our own strings' pins are claimed; that's not a refusal for them.
                    let refusal = match (&owner, bus) {
                        (Some(o), _) => Some(format!("used by {o}")),
                        // A bus pin is muxed to SPI, which is what a string there wants.
                        (None, Some(_)) => None,
                        (None, None) => self.gpio.refusal(id, "LED strip").filter(|r| !r.starts_with("used by LED strip")),
                    };
                    json!({
                        "gpio": id,
                        "header": phys,
                        "oneWire": buses.iter().any(|b| b.mosi == id) || pwm_ch.is_some(),
                        "clocked": true,
                        "via": bus.map(|b| format!("SPI{} {}", b.bus, if b.mosi == id { "MOSI" } else { "SCLK" })).or(pwm_ch.map(|c| format!("PWM{c}"))),
                        "refusal": refusal,
                    })
                })
                .collect(),
        )
    }

    fn view(&self) -> Result<Value, HttpError> {
        let m = self.model.lock().unwrap();
        let mut v = write_json(|j| {
            m.view(self.info, j);
            Ok(())
        })?;
        if let Some(c) = v.get_mut("capabilities").and_then(Value::as_object_mut) {
            c.insert("pinInfo".into(), self.pin_info(&m));
            c.insert("oneWireHint".into(), json!(self.one_wire_hint(&self.buses())));
            c.insert("receivers".into(), self.rx.lock().unwrap().to_json());
        }
        Ok(v)
    }

    fn string(&self, m: &Model, id: i64) -> Result<Value, HttpError> {
        write_json(|j| m.string_json(id, j))
    }

    /// Create (`id` None) or change a string: tried on a copy, checked against this
    /// board's hardware, then kept.
    fn configure(&self, id: Option<i64>, body: &[u8]) -> Result<Value, HttpError> {
        let text = std::str::from_utf8(body).map_err(|_| HttpError::bad_request("Body must be UTF-8 JSON"))?;
        let parsed = if text.trim().is_empty() {
            None
        } else {
            Some(pjson::parse(text).map_err(|_| HttpError::bad_request("Body is not valid JSON"))?)
        };
        let now = self.now();
        let mut m = self.model.lock().unwrap();
        let mut trial = m.clone();
        let id = trial.configure(self.info, id, parsed.as_ref(), now).map_err(api_err)?;
        let c = trial.slot_of(id).and_then(|i| trial.job(i)).map(|j| j.config).ok_or_else(|| HttpError::new(500, "string vanished"))?;
        self.check(&c, &trial, Some(id)).map_err(|e| HttpError::new(400, e))?;
        *m = trial;
        let v = self.string(&m, id)?;
        drop(m);
        self.save();
        self.changed();
        Ok(v)
    }

    fn with_body(
        &self,
        body: &[u8],
        f: impl FnOnce(&mut Model, Option<&pjson::Value<'_>>, u64) -> Result<i64, ApiErr>,
    ) -> Result<Value, HttpError> {
        let text = std::str::from_utf8(body).map_err(|_| HttpError::bad_request("Body must be UTF-8 JSON"))?;
        let parsed = if text.trim().is_empty() {
            None
        } else {
            Some(pjson::parse(text).map_err(|_| HttpError::bad_request("Body is not valid JSON"))?)
        };
        let now = self.now();
        let mut m = self.model.lock().unwrap();
        let id = f(&mut m, parsed.as_ref(), now).map_err(api_err)?;
        self.string(&m, id)
    }

    fn remove(&self, id: i64) -> Result<Value, HttpError> {
        self.model.lock().unwrap().remove(id).map_err(api_err)?;
        self.save();
        self.changed();
        Ok(json!({ "ok": true }))
    }

    /// The strings as settings: their config and pattern (pixels set through the API
    /// are runtime only).
    fn saved(&self) -> Value {
        let m = self.model.lock().unwrap();
        let ids: Vec<u16> = m.ids().collect();
        Value::Array(
            ids.into_iter()
                .filter_map(|id| self.string(&m, id as i64).ok())
                .map(|mut v| {
                    if let Some(o) = v.as_object_mut() {
                        for k in ["kind", "state", "error", "estimateMa", "source", "receiving"] {
                            o.remove(k);
                        }
                        if o.get("chip").and_then(Value::as_str) != Some("custom") {
                            o.remove("timing");
                        }
                        if o.get("clockHz").is_some_and(Value::is_null) {
                            o.remove("clockHz");
                        }
                    }
                    v
                })
                .collect(),
        )
    }

    fn save(&self) {
        let v = self.saved();
        if let Some(c) = self.ctx.get() {
            c.set_setting(SETTING, v);
        }
    }

    fn load(&self, saved: &Value) {
        let now = self.now();
        let mut m = self.model.lock().unwrap();
        for s in saved.as_array().into_iter().flatten() {
            let Some(id) = s.get("id").and_then(Value::as_u64).filter(|&i| (1..=u16::MAX as u64).contains(&i)) else { continue };
            let mut cfg = s.clone();
            let pattern = cfg.as_object_mut().and_then(|o| {
                o.remove("id");
                o.remove("pattern")
            });
            let text = cfg.to_string();
            let Ok(a) = pjson::parse(&text) else { continue };
            match m.restore(self.info, id as u16, Some(&a), now) {
                Ok(id) => {
                    if let Some(p) = pattern.filter(|p| p.is_object()) {
                        let t = p.to_string();
                        if let Ok(pv) = pjson::parse(&t) {
                            let _ = m.set_pattern(id, Some(&pv), now);
                        }
                    }
                }
                Err(e) => tracing::warn!("[leds] saved string {id} not restored: {}", e.message()),
            }
        }
    }

    fn summary(&self) -> Value {
        let m = self.model.lock().unwrap();
        let n = m.ids().count();
        json!({ "active": n > 0, "summary": if n == 0 { "no strings".to_string() } else if n == 1 { "1 string".to_string() } else { format!("{n} strings") }, "strings": n })
    }
}

// ---------- the worker: hardware setup and frames ----------

enum Out {
    Spi(SpiOut),
    Bang(BitBang),
    /// A channel of the shared PWM helper ([`PwmHub`]).
    Pwm(usize),
}

/// The PWM + DMA helper both PWM strings share: one clock and one DMA transfer carry both
/// channels, so a change to either restarts it with the new pair.
#[derive(Default)]
struct PwmHub {
    client: Option<pwm::PwmClient>,
    chans: [Option<PwmChan>; 2],
    frames: [Vec<u8>; 2],
    dirty: bool,
}

#[derive(Clone)]
struct PwmChan {
    pin: u32,
    invert: bool,
    fit: SpiFit,
    max_bytes: usize,
    reset_us: u32,
}

impl PwmHub {
    fn config(&self, osc: u32) -> Option<pwm::HelperConfig> {
        let used: Vec<&PwmChan> = self.chans.iter().flatten().collect();
        let first = used.first()?;
        Some(pwm::HelperConfig {
            pins: [self.chans[0].as_ref().map(|c| c.pin), self.chans[1].as_ref().map(|c| c.pin)],
            invert: [self.chans[0].as_ref().is_some_and(|c| c.invert), self.chans[1].as_ref().is_some_and(|c| c.invert)],
            clock_div: osc / first.fit.clock_hz.max(1),
            max_bytes: used.iter().map(|c| c.max_bytes).max().unwrap_or(0),
            reset_us: used.iter().map(|c| c.reset_us).max().unwrap_or(300),
        })
    }

    /// Stop the helper and start it again for the channels in use (if any).
    fn restart(&mut self, osc: u32) -> Result<(), String> {
        self.client = None;
        self.frames = Default::default();
        if let Some(cfg) = self.config(osc) {
            self.client = Some(pwm::PwmClient::start(cfg)?);
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), String> {
        if !self.dirty {
            return Ok(());
        }
        self.dirty = false;
        match self.client.as_mut() {
            Some(c) => c.frame([&self.frames[0], &self.frames[1]]),
            None => Ok(()),
        }
    }
}

#[derive(Clone, Copy)]
enum Encoding {
    OneWire(SpiFit),
    Apa102,
    Raw,
}

struct Lane {
    config_gen: Option<u32>,
    content_gen: u32,
    active: Option<(StringConfig, Encoding)>,
    out: Option<Out>,
    pins: Vec<u32>,
    frame: Vec<u8>,
    go: bool,
}

impl Lane {
    fn new() -> Self {
        Lane { config_gen: None, content_gen: 0, active: None, out: None, pins: vec![], frame: vec![], go: false }
    }

    /// Send the frame (a PWM string's goes to the hub, which sends both channels at once).
    fn write(&mut self, hub: &mut PwmHub) -> Result<(), String> {
        match &mut self.out {
            Some(Out::Spi(s)) => s.write(&self.frame),
            Some(Out::Bang(b)) => b.write(&self.frame),
            Some(Out::Pwm(ch)) => {
                hub.frames[*ch].clone_from(&self.frame);
                hub.dirty = true;
                Ok(())
            }
            None => Ok(()),
        }
    }
}

fn encode_frame(e: Encoding, c: &StringConfig, wire: &[u8], out: &mut Vec<u8>) {
    match e {
        Encoding::OneWire(f) => {
            out.resize(encode::ws_spi_len(&f, wire.len(), c.timing.reset_us), 0);
            encode::ws_spi(&f, wire, c.timing.reset_us, out);
        }
        Encoding::Apa102 => {
            out.resize(encode::apa102_len(wire.len() / 3), 0);
            encode::apa102(wire, 31, out);
        }
        Encoding::Raw => {
            out.clear();
            out.extend_from_slice(wire);
        }
    }
}

impl Inner {
    /// Set a string's hardware up; returns its output, encoding and the pins it holds.
    fn open(&self, c: &StringConfig, owner: &str, hub: &mut PwmHub) -> Result<(Out, Encoding, Vec<u32>), String> {
        let buses = self.buses();
        let on_bus = buses.iter().any(|b| b.mosi == c.pin as u32);
        if !c.chip.clocked()
            && !on_bus
            && self.pwm_ok()
            && let Some(ch) = pwm::pwm_channel(c.pin as u32)
        {
            return self.open_pwm(c, owner, ch, hub);
        }
        let bus = buses.iter().find(|b| b.mosi == c.pin as u32 && (!c.chip.clocked() || c.clock_pin.map(u32::from) == Some(b.sclk)));
        let (out, enc, pins) = match bus {
            Some(b) => {
                if c.invert {
                    return Err("SPI can't invert its output".into());
                }
                let (hz, enc, len) = if c.chip.clocked() {
                    let enc = if c.chip.is_apa102() { Encoding::Apa102 } else { Encoding::Raw };
                    let len = if c.chip.is_apa102() { encode::apa102_len(c.pixels as usize) } else { c.wire_len() };
                    (c.clock_hz, enc, len)
                } else {
                    let clock = self.spi_clock()?;
                    let f =
                        SpiFit::find(&c.timing, |hz| clock.actual(hz)).ok_or("this SPI controller can't make the timing within 150 ns")?;
                    (f.clock_hz, Encoding::OneWire(f), encode::ws_spi_len(&f, c.wire_len(), c.timing.reset_us))
                };
                let max = hw::spidev_bufsiz();
                if len > max {
                    return Err(format!(
                        "a frame is {len} bytes, more than spidev's {max}-byte transfer limit: raise spidev.bufsiz (GPIO panel → SPI settings, cmdline.txt) and reboot, or use fewer pixels"
                    ));
                }
                (Out::Spi(SpiOut::open(&b.dev, hz)?), enc, vec![b.mosi, b.sclk])
            }
            None if c.chip.clocked() => {
                let clock = c.clock_pin.ok_or("a clocked string needs a clock pin")? as u32;
                let data = c.pin as u32;
                self.gpio.refresh_lines();
                for p in [data, clock] {
                    if let Some(r) = self.gpio.refusal(p, owner) {
                        return Err(format!("{}: {r}", self.pin_name(p)));
                    }
                }
                let dl = self.gpio.line_of(data)?;
                let cl = self.gpio.line_of(clock)?;
                let bang = BitBang::open((&dl.path, dl.line), (&cl.path, cl.line), c.invert)?;
                let enc = if c.chip.is_apa102() { Encoding::Apa102 } else { Encoding::Raw };
                (Out::Bang(bang), enc, vec![data, clock])
            }
            None => return Err(self.one_wire_hint(&buses)),
        };
        for &p in &pins {
            if let Err(e) = self.gpio.claim(p, owner) {
                for &q in &pins {
                    if self.gpio.claims().iter().any(|c| c.pin == q && c.owner == owner) {
                        self.gpio.release(q);
                    }
                }
                return Err(format!("{}: {e}", self.pin_name(p)));
            }
        }
        Ok((out, enc, pins))
    }

    /// A one-wire string on PWM channel `ch`: joins the helper (restarting it with both
    /// channels), which needs both strings to share one clock.
    fn open_pwm(&self, c: &StringConfig, owner: &str, ch: usize, hub: &mut PwmHub) -> Result<(Out, Encoding, Vec<u32>), String> {
        if hub.chans[ch].is_some() {
            return Err(format!("PWM{ch} is already driving another string"));
        }
        let osc = pwm::osc_hz(self.bcm2711());
        let f = pwm::fit(&c.timing, osc).ok_or("the PWM clock can't make this timing within 150 ns")?;
        if let Some(o) = &hub.chans[1 - ch]
            && (o.fit.clock_hz, o.fit.bits) != (f.clock_hz, f.bits)
        {
            return Err("both PWM strings share one clock: give them the same chip timing".into());
        }
        let pin = c.pin as u32;
        self.gpio.refresh_lines();
        if let Some(r) = self.gpio.refusal(pin, owner).filter(|r| !r.starts_with("muxed to PWM")) {
            return Err(format!("{}: {r}", self.pin_name(pin)));
        }
        self.gpio.claim(pin, owner).map_err(|e| format!("{}: {e}", self.pin_name(pin)))?;
        hub.chans[ch] = Some(PwmChan {
            pin,
            invert: c.invert,
            fit: f,
            max_bytes: encode::ws_spi_len(&f, c.wire_len(), c.timing.reset_us),
            reset_us: c.timing.reset_us,
        });
        if let Err(e) = hub.restart(osc) {
            // Put the other channel's string back as it was.
            hub.chans[ch] = None;
            if let Err(e2) = hub.restart(osc) {
                tracing::warn!("[leds] PWM helper didn't come back for the other string: {e2}");
            }
            self.gpio.release(pin);
            return Err(e);
        }
        Ok((Out::Pwm(ch), Encoding::OneWire(f), vec![pin]))
    }

    fn release(&self, lane: &mut Lane, hub: &mut PwmHub) {
        if let Some((c, enc)) = lane.active.take() {
            // Blank it before letting go, so a removed string goes dark.
            let zeros = vec![0u8; c.wire_len()];
            encode_frame(enc, &c, &zeros, &mut lane.frame);
            let _ = lane.write(hub);
            let _ = hub.flush();
        }
        if let Some(Out::Pwm(ch)) = lane.out {
            hub.chans[ch] = None;
            if let Err(e) = hub.restart(pwm::osc_hz(self.bcm2711())) {
                tracing::warn!("[leds] PWM helper didn't restart for the other string: {e}");
            }
        }
        lane.out = None;
        for p in lane.pins.drain(..) {
            self.gpio.release(p);
        }
    }

    fn worker(self: &Arc<Self>) {
        let epoch = self.epoch.load(Ordering::SeqCst);
        let mut lanes: Vec<Lane> = (0..MAX_STRINGS).map(|_| Lane::new()).collect();
        let mut wire = vec![0u8; MAX_PIXELS * 4];
        let mut hub = PwmHub::default();
        while self.epoch.load(Ordering::SeqCst) == epoch {
            let output = self.gpio.output_enabled();
            let mut changed = false;
            {
                let mut m = self.model.lock().unwrap();
                m.set_output(output);
                // Strings whose network input went quiet get their pattern back.
                changed |= m.tick(self.now());
                for (i, lane) in lanes.iter_mut().enumerate() {
                    let job = m.job(i);
                    let want = job.map(|j| j.config_gen);
                    let reconfigured = want != lane.config_gen;
                    if reconfigured {
                        self.release(lane, &mut hub);
                        lane.config_gen = want;
                        if let Some(j) = job {
                            let owner = format!("LED strip {}", j.config.name.as_str());
                            let r = self.open(&j.config, &owner, &mut hub);
                            let r = match r {
                                Ok((out, enc, pins)) => {
                                    lane.out = Some(out);
                                    lane.pins = pins;
                                    lane.active = Some((j.config, enc));
                                    Ok(())
                                }
                                Err(e) => {
                                    tracing::warn!("[leds] string {} not started: {e}", j.id);
                                    Err(e)
                                }
                            };
                            m.report(i, j.config_gen, r.as_ref().map(|_| ()).map_err(String::as_str));
                            changed = true;
                        }
                    }
                    lane.go = false;
                    if let (Some(j), Some((c, enc))) = (job, lane.active)
                        && (reconfigured || j.content_gen != lane.content_gen || j.animated)
                    {
                        let n = m.render(i, self.now(), &mut wire);
                        encode_frame(enc, &c, &wire[..n], &mut lane.frame);
                        lane.content_gen = j.content_gen;
                        lane.go = true;
                    }
                }
            }
            let mut failed: Vec<(usize, String)> = Vec::new();
            for (i, lane) in lanes.iter_mut().enumerate() {
                if lane.go
                    && let Err(e) = lane.write(&mut hub)
                {
                    failed.push((i, e));
                }
            }
            if let Err(e) = hub.flush() {
                for (i, lane) in lanes.iter().enumerate() {
                    if matches!(lane.out, Some(Out::Pwm(_))) {
                        failed.push((i, e.clone()));
                    }
                }
            }
            if !failed.is_empty() {
                let mut m = self.model.lock().unwrap();
                for (i, e) in failed {
                    if let Some(j) = m.job(i) {
                        m.report(i, j.config_gen, Err(&e));
                    }
                }
                changed = true;
            }
            if changed && let Some(c) = self.ctx.get() {
                c.changed();
            }
            std::thread::sleep(Duration::from_millis(FRAME_MS));
        }
        for lane in &mut lanes {
            self.release(lane, &mut hub);
        }
    }
}

// ---------- the driver ----------

fn param_id(r: &Request) -> Result<i64, HttpError> {
    r.params
        .get("id")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| HttpError::bad_request("LED string id must be a number, e.g. /api/leds/1"))
}

fn doc(key: &str) -> Option<Json> {
    benchbob_proto::api::led_endpoint_docs().find(|(k, _)| *k == key).and_then(|(_, v)| serde_json::from_str(v).ok())
}

/// Tool arguments as a body for the shared handlers.
fn args_body(a: &Value) -> Vec<u8> {
    serde_json::to_vec(a).unwrap_or_default()
}

#[async_trait]
impl Driver for Leds {
    fn id(&self) -> &'static str {
        "leds"
    }
    fn title(&self) -> &'static str {
        "LED strips"
    }
    fn order(&self) -> i32 {
        65
    }
    fn description(&self) -> &'static str {
        "Addressable LED strings (WS2812, SK6812, APA102, WS2801) on header pins: patterns, colour order, timing, power budget"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let i = &self.inner;
        let _ = i.ctx.set(ctx.clone());
        if let Some(saved) = ctx.setting(SETTING) {
            i.load(&saved);
        }
        let me = i.clone();
        std::thread::Builder::new().name("leds".into()).spawn(move || me.worker())?;
        let me = i.clone();
        std::thread::Builder::new().name("leds-net".into()).spawn(move || net::run(me))?;
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) {
        self.inner.epoch.fetch_add(1, Ordering::SeqCst);
        // Let the worker blank the strings and free their pins.
        tokio::time::sleep(Duration::from_millis(FRAME_MS * 3)).await;
    }

    fn status(&self) -> Option<Json> {
        Some(self.inner.summary())
    }

    fn routes(&self) -> Vec<Route> {
        let i = self.inner.clone();
        let route = |m: Method, path: &str, key: &str, f: fn(Arc<Inner>, Request) -> Result<Value, HttpError>| {
            let i = i.clone();
            Route::new(m, path, doc(key), move |r| {
                let i = i.clone();
                async move {
                    tokio::task::spawn_blocking(move || f(i, r)).await.map_err(|e| HttpError::new(500, e.to_string()))?.and_then(reply)
                }
            })
        };
        vec![
            route(Method::GET, "/api/leds", "GET /api/leds", |i, _| i.view()),
            route(Method::POST, "/api/leds", "POST /api/leds", |i, r| i.configure(None, &r.body)),
            route(Method::POST, "/api/leds/:id", "POST /api/leds/:id", |i, r| i.configure(Some(param_id(&r)?), &r.body)),
            route(Method::DELETE, "/api/leds/:id", "DELETE /api/leds/:id", |i, r| i.remove(param_id(&r)?)),
            route(Method::POST, "/api/leds/:id/pattern", "POST /api/leds/:id/pattern", |i, r| {
                let id = param_id(&r)?;
                let v = i.with_body(&r.body, |m, a, now| m.set_pattern(id, a, now).map(|_| id))?;
                i.save();
                Ok(v)
            }),
            route(Method::POST, "/api/leds/:id/pixels", "POST /api/leds/:id/pixels", |i, r| {
                let id = param_id(&r)?;
                i.with_body(&r.body, |m, a, _| m.set_pixels(id, a).map(|_| id))
            }),
        ]
    }

    fn tools(&self) -> Vec<Tool> {
        benchbob_proto::api::led_tools()
            .map(|(name, desc, schema)| {
                let i = self.inner.clone();
                let n = name.to_string();
                Tool::new(name, desc, serde_json::from_str(schema).unwrap_or(json!({ "type": "object" })), move |a: Value| {
                    let i = i.clone();
                    let n = n.clone();
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let id = a.get("id").and_then(Value::as_i64);
                            let need = || id.ok_or_else(|| HttpError::bad_request("id is required"));
                            match n.as_str() {
                                "leds_list" => i.view(),
                                "leds_configure" if a.get("delete").and_then(Value::as_bool) == Some(true) => {
                                    let id = id.ok_or_else(|| HttpError::bad_request("delete needs an id"))?;
                                    i.remove(id).map(|_| json!({ "ok": true, "removed": id }))
                                }
                                "leds_configure" => i.configure(id, &args_body(&a)),
                                "leds_pattern" => {
                                    let id = need()?;
                                    let v = i.with_body(&args_body(&a), |m, b, now| m.set_pattern(id, b, now).map(|_| id))?;
                                    i.save();
                                    Ok(v)
                                }
                                "leds_set_pixels" => {
                                    let id = need()?;
                                    i.with_body(&args_body(&a), |m, b, _| m.set_pixels(id, b).map(|_| id))
                                }
                                _ => Err(HttpError::bad_request(format!("Unknown tool: {n}"))),
                            }
                        })
                        .await
                        .map_err(|e| HttpError::new(500, e.to_string()))?
                    }
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
