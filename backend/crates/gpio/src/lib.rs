//! The GPIO panel (peripherals/drivers/gpio.ts), on every board.
//!
//! Raspberry Pi (board `gpio.panel = "rpi"`): live state of the SoC's header bank
//! (function, pull from `pinctrl`; levels from a passive register reader at 10 Hz; kernel
//! consumers from the v2 uAPI), opt-in output control through `pinctrl set` (it writes the
//! registers, so a pin keeps its state after the command exits), and the I²C / SPI / UART /
//! 1-Wire switches in config.txt, plus (on BCM2711) the extra I²C3–6 / UART2–5 controllers
//! and a software i2c-gpio bus, with a pin-conflict check across all of them.
//!
//! Every other board (`"gpiochip"`): every gpiochip line (name, owner, flags) through the
//! uAPI, mapped onto the board's header where it has a map; levels for watched lines (an
//! as-is request, released on unwatch); boot overlays in armbianEnv.txt / orangepiEnv.txt /
//! extlinux.conf / uEnv.txt or through a board script. Watching also works on the Pi for
//! chips other than the header bank (USB adapters, the firmware's expander).
//!
//! New here: **output on every board**. With output control on (the same off-by-default
//! switch as the Pi's, enabling needs `confirm: true`), `POST /api/gpio/line` requests a
//! line as an output through the uAPI and holds it (or reconfigures it as an input with a
//! pull). Lines owned by anyone else are refused. Turning output control off (or the
//! driver) releases every line we drive; stopping the service leaves them alone, so the
//! kernel releases them as the process exits. A released line's state is the chip
//! driver's business (the uAPI promises nothing): on desky's BCM2711 it stays an output at
//! its last value, both after a release and after a shutdown.
//!
//! Cost model: nothing polls unless someone watches. /ws/gpio streams at ~10 Hz while a
//! client is connected and stops with the last one; REST reads are on demand. status()
//! only reports cached state.
//!
//! Safety: output control defaults off, and is refused for lines a kernel driver holds,
//! and on the Pi for pins muxed to a peripheral, GPIO0/1 (HAT ID EEPROM) and anything past
//! GPIO27. Boot-config edits are staged as a diff, applied only with an explicit confirm,
//! backed up first, and need a reboot, which is a second, separate confirm.
//! BENCHBOB_CONFIG_TXT / BENCHBOB_BOOT_ENV (and BENCHBOB_OVERLAY_DIR) point the editors at
//! test copies, and while one is set reboot is refused outright.

pub mod boot_env;
pub mod config_txt;
pub mod interfaces;
pub mod levels;
mod overlays;
pub mod script;
pub mod uapi;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::http::Method;
use benchbob_boards::{Board, PinKind, Reader, resolve_chips};
use benchbob_core::http::{HttpError, js_number, js_string, json, truthy};
use benchbob_core::run::{Opts, run, which};
use benchbob_core::timeline::now_ms;
use benchbob_core::{Ctx, Driver, Json, Route, Socket, StopReason, Tool, WsClient, WsIn, WsSession};
use serde_json::{Map, Value, json as j};

use crate::config_txt::PinState;
use crate::levels::Levels;
use crate::uapi::{Chip, LineHandle, LineInfo, Mode, Pull};

const STREAM_MS: u64 = 100;
const CONSUMERS_MS: u64 = 5000;
/// With direct level reads, pinctrl (function, pull) only needs to run this often.
const FULL_MS: u64 = 1000;
const MAX_WATCHES: usize = 64;
const CONSUMER: &str = "benchbob";

pub(crate) struct ChipState {
    pub chip: Chip,
    pub info: Vec<LineInfo>,
}

/// A line we hold: watched (as-is) or driven/reconfigured.
pub(crate) struct Held {
    pub chip: String,
    pub line: u32,
    pub handle: LineHandle,
    pub level: Option<u8>,
    pub error: Option<String>,
}

#[derive(Default)]
pub(crate) struct Snap {
    pub at: u64,
    pub pins: Vec<PinState>,
    pub lines_at: u64,
    pub error: String,
}

#[derive(Default)]
pub(crate) struct ChipSnap {
    pub at: u64,
    pub list: Vec<ChipState>,
    pub error: String,
}

pub(crate) struct Staged {
    pub token: String,
    pub text: String,
    pub base: String,
    pub path: String,
    pub at: u64,
    pub summary: Vec<String>,
    /// Script backends: the change to apply.
    pub change: Option<Value>,
}

#[derive(Default)]
pub(crate) struct State {
    pub snap: Snap,
    pub chips: ChipSnap,
    pub watches: indexmap_compat::Map<Held>,
    pub last_sent: String,
    pub last_sent_at: Option<Instant>,
    pub last_levels: String,
    pub staged: Option<Staged>,
    pub staged_ov: Option<Staged>,
    pub reboot_required: bool,
    pub levels: Option<Levels>,
    pub levels_tried: bool,
}

/// Insertion-ordered string-keyed map (JavaScript Map semantics for the watches).
mod indexmap_compat {
    pub struct Map<V>(pub Vec<(String, V)>);
    impl<V> Default for Map<V> {
        fn default() -> Self {
            Self(Vec::new())
        }
    }
    impl<V> Map<V> {
        pub fn get(&self, k: &str) -> Option<&V> {
            self.0.iter().find(|(x, _)| x == k).map(|(_, v)| v)
        }
        pub fn get_mut(&mut self, k: &str) -> Option<&mut V> {
            self.0.iter_mut().find(|(x, _)| x == k).map(|(_, v)| v)
        }
        pub fn contains(&self, k: &str) -> bool {
            self.get(k).is_some()
        }
        pub fn insert(&mut self, k: String, v: V) {
            match self.0.iter().position(|(x, _)| *x == k) {
                Some(i) => self.0[i].1 = v,
                None => self.0.push((k, v)),
            }
        }
        pub fn remove(&mut self, k: &str) -> Option<V> {
            self.0.iter().position(|(x, _)| x == k).map(|i| self.0.remove(i).1)
        }
        pub fn keys(&self) -> impl Iterator<Item = &String> {
            self.0.iter().map(|(k, _)| k)
        }
        pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
            self.0.iter_mut().map(|(_, v)| v)
        }
        pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
            self.0.iter().map(|(k, v)| (k, v))
        }
        pub fn len(&self) -> usize {
            self.0.len()
        }
        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
    }
}

/// Paths and overrides from the environment (test copies).
pub(crate) struct Env {
    pub config: PathBuf,
    pub config_overridden: bool,
    pub cmdline: PathBuf,
    pub boot_env: Option<String>,
    pub overlay_dir: Option<String>,
}

impl Env {
    fn from_env() -> Self {
        let e = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        Self {
            config: PathBuf::from(e("BENCHBOB_CONFIG_TXT").unwrap_or_else(|| "/boot/firmware/config.txt".into())),
            config_overridden: e("BENCHBOB_CONFIG_TXT").is_some(),
            cmdline: PathBuf::from(e("BENCHBOB_CMDLINE_TXT").unwrap_or_else(|| "/boot/firmware/cmdline.txt".into())),
            boot_env: e("BENCHBOB_BOOT_ENV"),
            overlay_dir: e("BENCHBOB_OVERLAY_DIR"),
        }
    }
}

pub(crate) struct Inner {
    pub board: Arc<Board>,
    pub ctx: OnceLock<Ctx>,
    pub pinctrl: Mutex<Option<PathBuf>>,
    pub st: Mutex<State>,
    pub watchers: Mutex<HashMap<u64, WsClient>>,
    pub looping: AtomicBool,
    pub env: Env,
    /// Bumped on stop, so a stale stream loop exits.
    pub epoch: std::sync::atomic::AtomicU64,
}

/// The GPIO driver.
pub struct Gpio {
    inner: Arc<Inner>,
}

impl Gpio {
    pub fn new(board: Arc<Board>) -> Self {
        Self {
            inner: Arc::new(Inner {
                board,
                ctx: OnceLock::new(),
                pinctrl: Mutex::new(None),
                st: Mutex::new(State::default()),
                watchers: Mutex::new(HashMap::new()),
                looping: AtomicBool::new(false),
                env: Env::from_env(),
                epoch: Default::default(),
            }),
        }
    }
}

fn line_key(chip: &str, line: u32) -> String {
    format!("{chip}:{line}")
}

/// The Pi's header bank: shown through pinctrl, so not listed (or watchable) as a chip.
fn is_soc_bank(label: &str) -> bool {
    label.starts_with("pinctrl-bcm") || label.starts_with("pinctrl-rp1")
}

fn http(status: u16, m: impl Into<String>) -> HttpError {
    HttpError::new(status, m)
}

impl Inner {
    fn ctx(&self) -> &Ctx {
        self.ctx.get().expect("gpio driver used before init")
    }

    pub(crate) fn is_pi(&self) -> bool {
        self.board.def.gpio.panel == benchbob_boards::Panel::Rpi
    }

    fn output_enabled(&self) -> bool {
        self.ctx.get().and_then(|c| c.setting("outputEnabled")).is_some_and(|v| truthy(&v))
    }

    fn board_info(&self) -> Value {
        self.board.info()
    }

    /// The Pi header: board pins with a BCM number → physical pin.
    fn gpio_to_pin(&self, gpio: u32) -> Option<u32> {
        self.board.def.headers.first()?.pins.iter().find(|p| p.kind == PinKind::Gpio && p.gpio == Some(gpio)).map(|p| p.pin)
    }

    fn header_json(&self) -> Value {
        serde_json::to_value(self.board.def.headers.first().map(|h| h.pins.clone()).unwrap_or_default()).unwrap()
    }

    // ---------- reading ----------

    async fn read_pins(&self) -> Result<Vec<PinState>, HttpError> {
        let bin = self.pinctrl.lock().unwrap().clone().ok_or_else(|| http(503, "pinctrl not found (package raspi-utils)"))?;
        let b = bin.to_string_lossy().into_owned();
        let r = run(&[&b, "get", "0-57"], Opts::timeout_ms(3000)).await;
        if r.code != 0 {
            return Err(http(502, format!("pinctrl: {}", if r.err.is_empty() { r.out } else { r.err })));
        }
        Ok(config_txt::parse_pinctrl(&r.out))
    }

    /// pinctrl now (and, with `with_lines`, every chip's line info).
    async fn refresh(&self, with_lines: bool) {
        let res = self.read_pins().await;
        let mut st = self.st.lock().unwrap();
        match res {
            Ok(pins) => {
                st.snap.pins = pins;
                st.snap.at = now_ms();
                st.snap.error.clear();
            }
            Err(e) => st.snap.error = e.message,
        }
        if with_lines {
            drop(st);
            self.refresh_chips();
            self.st.lock().unwrap().snap.lines_at = now_ms();
        }
    }

    /// Every chip's line info from the uAPI. A chip that went away takes its watches with it.
    pub(crate) fn refresh_chips(&self) {
        let list: Vec<ChipState> = uapi::list_chips()
            .into_iter()
            .map(|c| {
                if c.error.is_some() {
                    return ChipState { chip: c, info: vec![] };
                }
                match uapi::read_lines(&c) {
                    Ok(info) => ChipState { chip: c, info },
                    Err(e) => ChipState { chip: Chip { error: Some(e), ..c }, info: vec![] },
                }
            })
            .collect();
        let mut st = self.st.lock().unwrap();
        st.chips.list = list;
        st.chips.error.clear();
        st.chips.at = now_ms();
        let gone: Vec<String> =
            st.watches.iter().filter(|(_, w)| !st.chips.list.iter().any(|c| c.chip.info.name == w.chip)).map(|(k, _)| k.clone()).collect();
        for k in gone {
            st.watches.remove(&k);
        }
    }

    /// The SoC bank's line info by offset (the chip with the most lines, at least 54).
    fn soc_lines(st: &State) -> HashMap<u32, LineInfo> {
        let chip = st
            .chips
            .list
            .iter()
            .filter(|c| c.info.len() >= 54)
            .max_by_key(|c| c.info.len())
            .or_else(|| st.chips.list.iter().find(|c| c.chip.info.name == "gpiochip0"));
        chip.map(|c| c.info.iter().map(|l| (l.offset, l.clone())).collect()).unwrap_or_default()
    }

    /// Why a Pi header pin can't be driven, or None if it can.
    fn refusal(p: Option<&PinState>, line: Option<&LineInfo>) -> Option<String> {
        let Some(p) = p else { return Some("unknown GPIO".into()) };
        if p.gpio <= 1 {
            return Some("GPIO0/1 are the HAT ID EEPROM bus (ID_SD/ID_SC)".into());
        }
        if p.gpio > 27 {
            return Some("SoC-internal pin (not on the header)".into());
        }
        if let Some(c) = line.and_then(|l| l.consumer.as_ref()) {
            return Some(format!("claimed by the kernel ({c})"));
        }
        if p.fsel.len() == 2 && p.fsel.starts_with('a') && p.fsel.as_bytes()[1].is_ascii_digit() {
            return Some(format!("muxed to {} ({})", p.func, p.fsel.to_uppercase()));
        }
        None
    }

    fn pin_view(&self, p: &PinState, lines: &HashMap<u32, LineInfo>) -> Value {
        let line = lines.get(&p.gpio);
        j!({
            "gpio": p.gpio,
            "header": self.gpio_to_pin(p.gpio),
            "name": p.name,
            "fsel": p.fsel,
            "func": p.func,
            "level": p.level,
            "pull": p.pull,
            "drive": p.drive,
            "consumer": line.and_then(|l| l.consumer.clone()),
            "lineName": line.map(|l| l.name.clone()).filter(|n| !n.is_empty()),
            "refusal": Self::refusal(Some(p), line),
        })
    }

    fn held_json(h: &Held) -> Option<Value> {
        match h.handle.mode {
            Mode::AsIs => None,
            Mode::Output(v) => Some(j!({ "mode": "output", "value": v, "pull": Pull::as_str(h.handle.pull) })),
            Mode::Input => Some(j!({ "mode": "input", "pull": Pull::as_str(h.handle.pull) })),
        }
    }

    fn chips_view(st: &State, other_only: bool) -> Value {
        Value::Array(
            st.chips
                .list
                .iter()
                .filter(|c| !other_only || !is_soc_bank(&c.chip.info.label))
                .map(|c| {
                    let name = &c.chip.info.name;
                    j!({
                        "name": name,
                        "label": c.chip.info.label,
                        "device": c.chip.device,
                        "usb": c.chip.usb,
                        "error": c.chip.error,
                        "count": c.chip.info.lines,
                        "lines": c.info.iter().map(|l| {
                            let k = line_key(name, l.offset);
                            let mut v = j!({ "offset": l.offset, "name": l.name, "consumer": l.consumer, "used": l.used, "direction": l.direction, "flags": l.flags, "watched": st.watches.contains(&k) });
                            if let Some(h) = st.watches.get(&k).and_then(Self::held_json) {
                                v["held"] = h;
                            }
                            v
                        }).collect::<Vec<_>>(),
                    })
                })
                .collect(),
        )
    }

    /// The board's headers with each GPIO pin's live line info (null where the chip isn't found).
    fn headers_view(&self, st: &State) -> Value {
        let def = &self.board.def;
        let chips = resolve_chips(def, &st.chips.list, |c| c.chip.info.label.as_str(), |c| c.chip.device.as_str());
        Value::Array(
            def.headers
                .iter()
                .map(|h| {
                    j!({
                        "id": h.id,
                        "name": h.name,
                        "pins": h.pins.iter().map(|p| {
                            let mut v = serde_json::to_value(p).unwrap();
                            if p.kind != PinKind::Gpio {
                                return v;
                            }
                            let c = p.chip.as_ref().and_then(|k| chips.get(k));
                            let li = c.and_then(|c| p.line.and_then(|l| c.info.get(l as usize)));
                            let key = c.and_then(|c| p.line.map(|l| line_key(&c.chip.info.name, l)));
                            let o = v.as_object_mut().unwrap();
                            o.insert("chipName".into(), j!(c.map(|c| c.chip.info.name.clone())));
                            o.insert("lineName".into(), j!(li.map(|l| l.name.clone())));
                            o.insert("consumer".into(), j!(li.and_then(|l| l.consumer.clone())));
                            o.insert("used".into(), j!(li.map(|l| l.used)));
                            o.insert("direction".into(), j!(li.map(|l| l.direction)));
                            o.insert("flags".into(), j!(li.map(|l| l.flags.clone()).unwrap_or_default()));
                            o.insert("watched".into(), j!(key.as_ref().is_some_and(|k| st.watches.contains(k))));
                            if let Some(h) = key.as_ref().and_then(|k| st.watches.get(k)).and_then(Self::held_json) {
                                o.insert("held".into(), h);
                            }
                            v
                        }).collect::<Vec<_>>(),
                    })
                })
                .collect(),
        )
    }

    fn levels_view(st: &State) -> Value {
        Value::Object(
            st.watches
                .iter()
                .map(|(k, x)| {
                    (k.clone(), if let Some(e) = &x.error { j!({ "level": null, "error": e }) } else { j!({ "level": x.level }) })
                })
                .collect(),
        )
    }

    fn state_view(&self, st: &State) -> Value {
        let watched: Vec<&String> = st.watches.keys().collect();
        if !self.is_pi() {
            return j!({
                "mode": "gpiochip",
                "at": st.chips.at,
                "error": st.chips.error,
                "outputEnabled": self.output_enabled(),
                "board": self.board_info(),
                "headers": self.headers_view(st),
                "chips": Self::chips_view(st, false),
                "watched": watched,
            });
        }
        let lines = Self::soc_lines(st);
        j!({
            "mode": "rpi",
            "at": st.snap.at,
            "error": st.snap.error,
            "outputEnabled": self.output_enabled(),
            "pins": st.snap.pins.iter().map(|p| self.pin_view(p, &lines)).collect::<Vec<_>>(),
            "chips": Self::chips_view(st, true),
            "watched": watched,
        })
    }

    // ---------- watching and driving lines ----------

    fn find_chip<'a>(st: &'a State, chip: &str) -> Option<&'a ChipState> {
        st.chips.list.iter().find(|c| c.chip.info.name == chip)
    }

    fn read_held(x: &mut Held) {
        match x.handle.read() {
            Ok(v) => {
                x.level = Some(v);
                x.error = None;
            }
            Err(e) => {
                x.level = None;
                x.error = Some(e.to_string());
            }
        }
    }

    fn watch_view(st: &State, k: &str) -> Value {
        let x = st.watches.get(k).unwrap();
        j!({ "key": k, "chip": x.chip, "line": x.line, "level": x.level, "error": x.error })
    }

    /// Checks shared by watch and drive; returns the line's info.
    fn check_line(&self, st: &State, chip: &str, line: f64, what: &str) -> Result<LineInfo, HttpError> {
        let c = Self::find_chip(st, chip).ok_or_else(|| http(404, format!("No GPIO chip {chip}")))?;
        if let Some(e) = &c.chip.error {
            return Err(http(503, e.clone()));
        }
        if self.is_pi() && is_soc_bank(&c.chip.info.label) {
            return Err(http(
                409,
                if what == "watch" {
                    format!("{chip} is the Pi's header bank: its levels are already live in the GPIO view")
                } else {
                    format!("{chip} is the Pi's header bank: drive it with POST /api/gpio/<gpio> (pinctrl)")
                },
            ));
        }
        if !(line.fract() == 0.0 && line >= 0.0 && line < c.chip.info.lines as f64) {
            return Err(http(400, format!("{chip} has lines 0–{}", c.chip.info.lines as i64 - 1)));
        }
        Ok(c.info[line as usize].clone())
    }

    fn watch(self: &Arc<Self>, chip: &str, line: f64) -> Result<Value, HttpError> {
        if Self::find_chip(&self.st.lock().unwrap(), chip).is_none() {
            self.refresh_chips();
        }
        let li = {
            let st = self.st.lock().unwrap();
            let li = self.check_line(&st, chip, line, "watch")?;
            let k = line_key(chip, line as u32);
            if st.watches.contains(&k) {
                return Ok(Self::watch_view(&st, &k));
            }
            if st.watches.len() >= MAX_WATCHES {
                return Err(http(409, format!("At most {MAX_WATCHES} lines can be watched at once")));
            }
            if li.used {
                let n = if li.name.is_empty() { String::new() } else { format!(" ({})", li.name) };
                return Err(http(
                    409,
                    format!(
                        "{chip} line {}{n} is in use by {}; watching it would mean taking it from its owner",
                        line as u32,
                        li.consumer.clone().unwrap_or_default()
                    ),
                ));
            }
            li
        };
        let path = format!("/dev/{chip}");
        let path = Self::find_chip(&self.st.lock().unwrap(), chip).map(|c| c.chip.path.clone()).unwrap_or(path);
        let handle = LineHandle::request_as_is(&path, line as u32, CONSUMER)
            .map_err(|e| http(if e.errno == 16 { 409 } else { 502 }, e.to_string()))?;
        let k = line_key(chip, line as u32);
        {
            let mut st = self.st.lock().unwrap();
            let mut x = Held { chip: chip.into(), line: line as u32, handle, level: None, error: None };
            Self::read_held(&mut x);
            st.watches.insert(k.clone(), x);
        }
        let n = if li.name.is_empty() { String::new() } else { format!(" ({})", li.name) };
        self.ctx().event(&format!("Watching {chip} line {}{n}", line as u32), None);
        self.refresh_chips();
        self.ctx().changed();
        self.start_loop_if_watched();
        Ok(Self::watch_view(&self.st.lock().unwrap(), &k))
    }

    fn unwatch(&self, k: &str) -> bool {
        self.st.lock().unwrap().watches.remove(k).is_some()
    }

    /// Drive (or reconfigure, or release) a gpiochip line through the uAPI.
    fn drive(self: &Arc<Self>, chip: &str, line: f64, mode: Option<&str>, pull: Option<&str>) -> Result<Value, HttpError> {
        if !self.output_enabled() {
            return Err(http(
                403,
                "GPIO output control is off. Enable it first (POST /api/gpio/output-control with enabled: true, confirm: true).",
            ));
        }
        let mode = mode.map(str::to_lowercase);
        let pull = match pull.map(str::to_lowercase).as_deref() {
            None => None,
            Some("up") => Some(Pull::Up),
            Some("down") => Some(Pull::Down),
            Some("none") => Some(Pull::Off),
            Some(_) => return Err(http(400, "pull must be up, down or none")),
        };
        if Self::find_chip(&self.st.lock().unwrap(), chip).is_none() {
            self.refresh_chips();
        }
        // Fresh line info: someone may have claimed the line since the last refresh.
        self.refresh_chips();
        let k = line_key(chip, line as u32);
        let mut st = self.st.lock().unwrap();
        let li = self.check_line(&st, chip, line, "drive")?;
        let ours = st.watches.contains(&k);
        if li.used && !ours {
            return Err(http(
                409,
                format!("{chip} line {}: claimed by the kernel ({})", line as u32, li.consumer.clone().unwrap_or_default()),
            ));
        }
        let label =
            if li.name.is_empty() { format!("{chip} line {}", line as u32) } else { format!("{chip} line {} ({})", line as u32, li.name) };
        let what: String = match mode.as_deref() {
            Some("release") => {
                if st.watches.remove(&k).is_none() {
                    return Err(http(409, format!("{label} isn't held by BenchBob")));
                }
                "released".into()
            }
            Some(m @ ("high" | "low")) => {
                let v = if m == "high" { 1 } else { 0 };
                match st.watches.get_mut(&k) {
                    Some(x) => x.handle.drive(v, pull).map_err(|e| http(502, e.to_string()))?,
                    None => {
                        if st.watches.len() >= MAX_WATCHES {
                            return Err(http(409, format!("At most {MAX_WATCHES} lines can be held at once")));
                        }
                        let path = Self::find_chip(&st, chip).unwrap().chip.path.clone();
                        let h = LineHandle::request_output(&path, line as u32, CONSUMER, v, pull)
                            .map_err(|e| http(if e.errno == 16 { 409 } else { 502 }, e.to_string()))?;
                        st.watches.insert(k.clone(), Held { chip: chip.into(), line: line as u32, handle: h, level: None, error: None });
                    }
                }
                format!("driven {m}{}", Pull::as_str(pull).map(|p| format!(", pull {p}")).unwrap_or_default())
            }
            Some("input") => {
                match st.watches.get_mut(&k) {
                    Some(x) => x.handle.input(pull).map_err(|e| http(502, e.to_string()))?,
                    None => {
                        let path = Self::find_chip(&st, chip).unwrap().chip.path.clone();
                        let mut h = LineHandle::request_as_is(&path, line as u32, CONSUMER)
                            .map_err(|e| http(if e.errno == 16 { 409 } else { 502 }, e.to_string()))?;
                        h.input(pull).map_err(|e| http(502, e.to_string()))?;
                        st.watches.insert(k.clone(), Held { chip: chip.into(), line: line as u32, handle: h, level: None, error: None });
                    }
                }
                format!("input{}", Pull::as_str(pull).map(|p| format!(", pull {p}")).unwrap_or_default())
            }
            None if pull.is_some() => {
                let x = st.watches.get_mut(&k).ok_or_else(|| http(400, "Nothing to set: give mode (high, low, input or release)"))?;
                match x.handle.mode {
                    Mode::Output(v) => x.handle.drive(v, pull).map_err(|e| http(502, e.to_string()))?,
                    _ => x.handle.input(pull).map_err(|e| http(502, e.to_string()))?,
                }
                format!("pull {}", Pull::as_str(pull).unwrap())
            }
            None => return Err(http(400, "Nothing to set: give mode (high, low, input or release) and/or pull")),
            Some(_) => return Err(http(400, "mode must be high, low, input or release")),
        };
        let view = match st.watches.get_mut(&k) {
            Some(x) => {
                Self::read_held(x);
                j!({ "key": k, "chip": chip, "line": line as u32, "name": li.name, "held": Self::held_json(x), "level": x.level, "error": x.error })
            }
            None => j!({ "key": k, "chip": chip, "line": line as u32, "name": li.name, "held": null, "level": null }),
        };
        drop(st);
        self.ctx().event(&format!("{label} {what}"), None);
        self.refresh_chips();
        self.ctx().changed();
        self.start_loop_if_watched();
        Ok(view)
    }

    /// Let go of every line we drive or reconfigured (watches stay).
    fn release_outputs(&self) -> usize {
        let mut st = self.st.lock().unwrap();
        let keys: Vec<String> = st.watches.iter().filter(|(_, x)| x.handle.mode != Mode::AsIs).map(|(k, _)| k.clone()).collect();
        for k in &keys {
            st.watches.remove(k);
        }
        keys.len()
    }

    /// A header pin (physical number, first header unless `header` names one) → chip + line.
    fn pin_line(&self, header: Option<&str>, pin: f64) -> Result<(String, u32), HttpError> {
        let def = &self.board.def;
        let h = match header {
            Some(id) => {
                def.headers.iter().find(|h| h.id == id).ok_or_else(|| http(404, format!("{} has no header {id}", self.board.name)))?
            }
            None => {
                def.headers.first().ok_or_else(|| http(409, format!("{} has no mapped header: give chip and line", self.board.name)))?
            }
        };
        let p = h.pins.iter().find(|p| p.pin as f64 == pin).ok_or_else(|| http(400, format!("{} has pins 1–{}", h.name, h.pins.len())))?;
        if p.kind != PinKind::Gpio {
            return Err(http(
                400,
                format!(
                    "Pin {} of the {} is {}",
                    p.pin,
                    h.name,
                    p.label.clone().unwrap_or_else(|| serde_json::to_value(p.kind).unwrap().as_str().unwrap_or("").to_string())
                ),
            ));
        }
        let (Some(key), Some(line)) = (p.chip.as_ref(), p.line) else {
            return Err(http(
                409,
                format!("Pin {} is GPIO{} on the Pi's header bank: use POST /api/gpio/{}", p.pin, p.gpio.unwrap_or(0), p.gpio.unwrap_or(0)),
            ));
        };
        let st = self.st.lock().unwrap();
        let chips = resolve_chips(def, &st.chips.list, |c| c.chip.info.label.as_str(), |c| c.chip.device.as_str());
        let c =
            chips.get(key).ok_or_else(|| http(503, format!("Pin {}'s controller ({key}) isn't among this machine's gpiochips", p.pin)))?;
        Ok((c.chip.info.name.clone(), line))
    }

    fn read_once(self: &Arc<Self>, chip: &str, line: f64) -> Value {
        let k = line_key(chip, line as u32);
        let mut st = self.st.lock().unwrap();
        let name = Self::find_chip(&st, chip).and_then(|c| c.info.get(line as usize)).map(|l| l.name.clone()).filter(|n| !n.is_empty());
        if let Some(x) = st.watches.get_mut(&k) {
            Self::read_held(x);
            let mut v = j!({ "chip": chip, "line": line as u32, "name": name, "level": x.level });
            if let Some(e) = &x.error {
                v["error"] = e.clone().into();
            }
            return v;
        }
        let res: Result<u8, String> = (|| {
            let c = Self::find_chip(&st, chip).ok_or_else(|| format!("No GPIO chip {chip}"))?;
            if self.is_pi() && is_soc_bank(&c.chip.info.label) {
                return Err("the Pi's header bank: use gpios instead".into());
            }
            let li = c
                .info
                .get(line as usize)
                .filter(|_| line.fract() == 0.0 && line >= 0.0)
                .ok_or_else(|| format!("{chip} has lines 0–{}", c.chip.info.lines as i64 - 1))?;
            if li.used {
                return Err(format!("in use by {}", li.consumer.clone().unwrap_or_default()));
            }
            let h = LineHandle::request_as_is(&c.chip.path, line as u32, CONSUMER).map_err(|e| e.to_string())?;
            h.read().map_err(|e| e.to_string())
        })();
        st.watches.len(); // (keeps the lock until the one-shot request is closed)
        match res {
            Ok(v) => j!({ "chip": chip, "line": line as u32, "name": name, "level": v }),
            Err(e) => j!({ "chip": chip, "line": line as u32, "name": name, "level": null, "error": e }),
        }
    }

    // ---------- streaming ----------

    fn start_loop_if_watched(self: &Arc<Self>) {
        if !self.watchers.lock().unwrap().is_empty() {
            self.start_loop();
        }
    }

    fn start_loop(self: &Arc<Self>) {
        if self.looping.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        let epoch = self.epoch.load(Ordering::SeqCst);
        tokio::spawn(async move {
            loop {
                if me.watchers.lock().unwrap().is_empty() || me.epoch.load(Ordering::SeqCst) != epoch {
                    break;
                }
                me.tick().await;
                tokio::time::sleep(Duration::from_millis(STREAM_MS)).await;
            }
            me.looping.store(false, Ordering::SeqCst);
            // A client may have connected just as the loop decided to stop.
            if !me.watchers.lock().unwrap().is_empty() && me.epoch.load(Ordering::SeqCst) == epoch {
                me.start_loop();
            }
        });
    }

    async fn tick(self: &Arc<Self>) {
        let now = now_ms();
        if self.is_pi() {
            let (have_levels, snap_at, lines_at) = {
                let mut st = self.st.lock().unwrap();
                if !st.levels_tried {
                    st.levels_tried = true;
                    st.levels = self.board.reader().and_then(Levels::open);
                }
                (st.levels.is_some(), st.snap.at, st.snap.lines_at)
            };
            if !have_levels || now.saturating_sub(snap_at) > FULL_MS {
                self.refresh(now.saturating_sub(lines_at) > CONSUMERS_MS).await;
            } else {
                let mut st = self.st.lock().unwrap();
                let s = st.levels.as_ref().unwrap().read();
                for p in st.snap.pins.iter_mut() {
                    if p.gpio < 58 && (s.known >> p.gpio) & 1 == 1 {
                        p.level = Some(((s.bits >> p.gpio) & 1) as u8);
                    } else if p.gpio < levels::RP1_BANK0 && s.known != (1u64 << 58) - 1 {
                        // RP1 with the pad's input disabled: the level isn't knowable.
                        p.level = None;
                    }
                }
            }
            // Other chips (USB adapters) change rarely: their line info at the consumers' pace.
            if now.saturating_sub(self.st.lock().unwrap().chips.at) > CONSUMERS_MS {
                self.refresh_chips();
            }
        } else if now.saturating_sub(self.st.lock().unwrap().chips.at) > FULL_MS {
            self.refresh_chips();
        }
        let watchers: Vec<WsClient> = self.watchers.lock().unwrap().values().cloned().collect();
        let mut st = self.st.lock().unwrap();
        let mut msg = Map::new();
        msg.insert("type".into(), "state".into());
        if let Value::Object(v) = self.state_view(&st) {
            msg.extend(v);
        }
        let msg = serde_json::to_string(&Value::Object(msg)).unwrap();
        // Only push changes, plus a heartbeat so clients can show "live".
        let body = strip_at(&msg);
        if body != st.last_sent || st.last_sent_at.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            st.last_sent = body;
            st.last_sent_at = Some(Instant::now());
            for w in &watchers {
                w.send(msg.clone());
            }
        }
        // Watched lines' levels go separately, so a toggling input doesn't resend every chip.
        if !st.watches.is_empty() {
            for x in st.watches.values_mut() {
                Self::read_held(x);
            }
            let lv = serde_json::to_string(&j!({ "type": "levels", "levels": Self::levels_view(&st) })).unwrap();
            if lv != st.last_levels {
                st.last_levels = lv.clone();
                for w in &watchers {
                    w.send(lv.clone());
                }
            }
        }
    }

    async fn ws_session(self: Arc<Self>, mut s: WsSession) {
        let id = s.client.id();
        self.watchers.lock().unwrap().insert(id, s.client.clone());
        {
            let st = self.st.lock().unwrap();
            let have = if self.is_pi() { st.snap.at } else { st.chips.at };
            if have != 0 {
                let mut msg = Map::new();
                msg.insert("type".into(), "state".into());
                if let Value::Object(v) = self.state_view(&st) {
                    msg.extend(v);
                }
                s.client.send(serde_json::to_string(&Value::Object(msg)).unwrap());
            }
            if !st.watches.is_empty() {
                s.client.send(serde_json::to_string(&j!({ "type": "levels", "levels": Self::levels_view(&st) })).unwrap());
            }
        }
        self.start_loop();
        while let Some(m) = s.incoming.recv().await {
            // The stream is one-way; nothing a client sends changes it.
            let _ = matches!(m, WsIn::Text(_) | WsIn::Binary(_));
        }
        self.watchers.lock().unwrap().remove(&id);
    }

    // ---------- Pi output control ----------

    async fn write_pin(self: &Arc<Self>, gpio: f64, mode: Option<&str>, pull: Option<&str>) -> Result<Value, HttpError> {
        if !self.is_pi() {
            return Err(http(
                409,
                format!(
                    "{} isn't a Raspberry Pi: drive gpiochip lines with POST /api/gpio/line ({{chip, line}} or {{pin}}, mode high|low|input|release)",
                    self.board.name
                ),
            ));
        }
        if !self.output_enabled() {
            return Err(http(
                403,
                "GPIO output control is off. Enable it first (POST /api/gpio/output-control with enabled: true, confirm: true).",
            ));
        }
        let bin = self.pinctrl.lock().unwrap().clone().ok_or_else(|| http(503, "pinctrl not found"))?;
        self.refresh(true).await;
        {
            let st = self.st.lock().unwrap();
            let p = st.snap.pins.iter().find(|x| x.gpio as f64 == gpio);
            if let Some(why) = Self::refusal(p, Self::soc_lines(&st).get(&(gpio as u32))) {
                return Err(http(409, format!("GPIO{}: {why}", js_string(&j!(gpio)))));
            }
        }
        let mut args: Vec<&str> = Vec::new();
        let mode = mode.map(str::to_lowercase);
        match mode.as_deref() {
            Some("high") => args.extend(["op", "dh"]),
            Some("low") => args.extend(["op", "dl"]),
            Some("input") => args.push("ip"),
            Some(_) => return Err(http(400, "mode must be high, low or input")),
            None => {}
        }
        let pull = pull.map(str::to_lowercase);
        match pull.as_deref() {
            Some("up") => args.push("pu"),
            Some("down") => args.push("pd"),
            Some("none") => args.push("pn"),
            Some(_) => return Err(http(400, "pull must be up, down or none")),
            None => {}
        }
        if args.is_empty() {
            return Err(http(400, "Nothing to set: give mode and/or pull"));
        }
        let g = (gpio as u32).to_string();
        let b = bin.to_string_lossy().into_owned();
        let mut cmd = vec![b.as_str(), "set", g.as_str()];
        cmd.extend(args);
        // pinctrl writes the GPIO registers directly, so the state persists after it exits
        // and nothing holds the line (kernel drivers and other tools can still claim it).
        let r = run(&cmd, Opts::timeout_ms(3000)).await;
        if r.code != 0 {
            return Err(http(502, format!("pinctrl set: {}", if r.err.is_empty() { r.out } else { r.err })));
        }
        self.refresh(false).await;
        let what: Vec<String> = [mode.clone(), pull.as_ref().map(|p| format!("pull {p}"))].into_iter().flatten().collect();
        self.ctx().event(
            &format!(
                "GPIO{g} (pin {}) set: {}",
                self.gpio_to_pin(gpio as u32).map(|p| p.to_string()).unwrap_or_else(|| "undefined".into()),
                what.join(", ")
            ),
            None,
        );
        self.ctx().changed();
        let st = self.st.lock().unwrap();
        let p = st.snap.pins.iter().find(|x| x.gpio == gpio as u32).ok_or_else(|| http(502, "pinctrl lost the pin"))?;
        Ok(self.pin_view(p, &Self::soc_lines(&st)))
    }
}

/// The message without its first `"at":<n>,` (what changes every tick).
fn strip_at(msg: &str) -> String {
    if let Some(i) = msg.find("\"at\":") {
        let rest = &msg[i + 5..];
        let n = rest.bytes().take_while(|c| c.is_ascii_digit()).count();
        if n > 0 && rest.as_bytes().get(n) == Some(&b',') {
            return format!("{}{}", &msg[..i], &rest[n + 1..]);
        }
    }
    msg.to_string()
}

/// Write a file we may not own (/boot is root's): directly if we can, else `sudo -n tee`.
pub(crate) async fn write_root(path: &str, data: &str) -> Result<(), HttpError> {
    if std::fs::OpenOptions::new().write(true).open(path).is_ok() && std::fs::write(path, data).is_ok() {
        return Ok(());
    }
    let r = run(&["sudo", "-n", "tee", path], Opts { stdin: Some(data.as_bytes().to_vec()), ..Opts::timeout_ms(10_000) }).await;
    if r.code != 0 {
        return Err(http(502, format!("writing {path}: {}", if r.err.is_empty() { "failed".into() } else { r.err })));
    }
    Ok(())
}

/// UTC "YYYY-MM-DD-HH-MM-SS" (`new Date().toISOString().replace(/[:T]/g, "-").slice(0, 19)`).
pub(crate) fn backup_stamp(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}-{:02}-{:02}-{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Copy `path` to path.bak-<timestamp> (as root if need be), then write `text`.
pub(crate) async fn backup_and_write(path: &str, text: &str) -> Result<String, HttpError> {
    let backup = format!("{path}.bak-{}", backup_stamp(now_ms()));
    if run(&["cp", "-p", path, &backup], Opts::default()).await.code != 0 {
        let s = run(&["sudo", "-n", "cp", "-p", path, &backup], Opts::default()).await;
        if s.code != 0 {
            return Err(http(502, format!("backup failed: {}", s.err)));
        }
    }
    write_root(path, text).await?;
    Ok(backup)
}

// ---------- routes ----------

fn num_arg(a: &Value, k: &str) -> f64 {
    a.get(k).map(js_number).unwrap_or(f64::NAN)
}

fn str_arg<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str)
}

impl Gpio {
    fn route<F, Fut>(&self, method: Method, path: &str, doc: Json, f: F) -> Route
    where
        F: Fn(Arc<Inner>, benchbob_core::Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = benchbob_core::RouteResult> + Send + 'static,
    {
        let inner = self.inner.clone();
        Route::new(method, path, Some(doc), move |r| f(inner.clone(), r))
    }
}

/// Resolve `{chip, line}` or `{pin, header?}` in a request body.
fn line_target(i: &Arc<Inner>, a: &Value) -> Result<(String, f64), HttpError> {
    if a.get("pin").is_some_and(|p| !p.is_null()) {
        let (c, l) = i.pin_line(str_arg(a, "header"), num_arg(a, "pin"))?;
        return Ok((c, l as f64));
    }
    let chip = a.get("chip").map(js_string).unwrap_or_default();
    if !(chip.len() > 8 && chip.starts_with("gpiochip") && chip[8..].bytes().all(|c| c.is_ascii_digit())) {
        return Err(http(400, "chip must be a gpiochip name, e.g. \"gpiochip2\" (or give pin, a physical header pin)"));
    }
    Ok((chip, num_arg(a, "line")))
}

#[async_trait]
impl Driver for Gpio {
    fn id(&self) -> &'static str {
        "gpio"
    }
    fn title(&self) -> &'static str {
        "GPIO"
    }
    fn order(&self) -> i32 {
        42
    }
    fn description(&self) -> &'static str {
        "Live GPIO header view (Raspberry Pi and mapped boards) or gpiochip lines, opt-in output control on any board, and the boot-time bus switches (config.txt, or device-tree overlays elsewhere)"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let _ = self.inner.ctx.set(ctx);
        *self.inner.pinctrl.lock().unwrap() = which("pinctrl");
        self.inner.refresh_chips();
        Ok(())
    }

    async fn stop(&self, reason: StopReason) {
        self.inner.epoch.fetch_add(1, Ordering::SeqCst);
        for w in self.inner.watchers.lock().unwrap().drain() {
            w.1.close();
        }
        let mut st = self.inner.st.lock().unwrap();
        st.levels = None;
        st.levels_tried = false;
        // Disabled: release every line (watches and outputs). Shutdown: leave held lines
        // alone; the kernel releases them as the process exits, and driven pins keep their
        // last value where the chip's driver allows it.
        if reason == StopReason::Disable {
            st.watches = Default::default();
        }
    }

    fn status(&self) -> Option<Json> {
        let i = &self.inner;
        let on = i.output_enabled();
        let b = i.board_info();
        let tested = b["tested"].as_bool().unwrap_or(false);
        let st = i.st.lock().unwrap();
        let watchers = i.watchers.lock().unwrap().len();
        let driving = st.watches.iter().filter(|(_, x)| x.handle.mode != Mode::AsIs).count();
        let reboot = st.reboot_required;
        if !i.is_pi() {
            let h = &i.board.def.headers;
            let n = st.chips.list.len();
            let target = overlays::target(i);
            let mut summary = if h.is_empty() {
                format!("{n} GPIO chip{}", if n == 1 { "" } else { "s" })
            } else {
                h.iter().map(|x| x.name.clone()).collect::<Vec<_>>().join(" + ")
            };
            if !st.watches.is_empty() {
                summary += &format!(" · watching {}", st.watches.len());
            }
            if on {
                summary += " · output control ON";
            }
            if driving > 0 {
                summary += &format!(" · driving {driving}");
            }
            if !tested {
                summary += " · untested board";
            }
            if reboot {
                summary += " · reboot required";
            }
            return Some(j!({
                "active": !st.watches.is_empty() || on,
                "summary": summary,
                "mode": "gpiochip",
                "board": b,
                "watchers": watchers,
                "watching": st.watches.len(),
                "rebootRequired": reboot,
                "bootConfig": target.map(|t| j!({ "kind": t.kind.as_str(), "path": t.path, "testCopy": t.test_copy })),
                "header": h.first().map(|h| serde_json::to_value(&h.pins).unwrap()).unwrap_or(j!([])),
                "outputEnabled": on,
                "driving": driving,
            }));
        }
        let cfg = i.env.config.display().to_string();
        let mut summary = "40-pin header".to_string();
        if on {
            summary += " · output control ON";
        }
        if reboot {
            summary += " · reboot required";
        }
        if !tested {
            summary += " · untested board";
        }
        Some(j!({
            "active": on,
            "summary": summary,
            "mode": "rpi",
            "board": b,
            "outputEnabled": on,
            "watchers": watchers,
            "watching": st.watches.len(),
            "rebootRequired": reboot,
            "configPath": cfg,
            "testCopy": i.env.config_overridden,
            "bootConfig": { "kind": "config.txt", "path": cfg, "testCopy": i.env.config_overridden },
            "tools": { "pinctrl": i.pinctrl.lock().unwrap().is_some(), "gpioinfo": which("gpioinfo").is_some() },
            "header": i.header_json(),
        }))
    }

    fn sockets(&self) -> Vec<Socket> {
        let inner = self.inner.clone();
        vec![Socket::new(
            "/ws/gpio",
            Some(
                "Live GPIO state while connected: {type:'state', mode:'rpi', pins:[{gpio, header, func, level, pull, consumer, refusal}], outputEnabled, chips} on a Pi, or {type:'state', mode:'gpiochip', headers, chips} elsewhere (as GET /api/gpio), pushed on change (polled at 10 Hz) plus a 2 s heartbeat; {type:'levels', levels:{'gpiochip2:17':{level}}} when a watched line changes. Polling stops when the last client disconnects.",
            ),
            move |s| inner.clone().ws_session(s),
        )]
    }

    fn routes(&self) -> Vec<Route> {
        vec![
            self.route(Method::GET, "/api/gpio", j!("GPIO state now. Raspberry Pi (mode rpi): the SoC bank from pinctrl + the uAPI (function/alt name, level, pull, kernel consumer, why a pin can't be driven) and the 40-pin header map, plus other gpiochips (USB adapters). Other boards (mode gpiochip): every gpiochip line (name, consumer, direction, flags) and the board's headers with each pin's line. levels: the watched lines. board: which board this is and whether it's been tested"), |i, _| async move {
                i.refresh_chips();
                if !i.is_pi() {
                    let st = i.st.lock().unwrap();
                    let mut v = i.state_view(&st);
                    v["levels"] = Inner::levels_view(&st);
                    v["levelReader"] = i.reader_view(&st);
                    return json(v);
                }
                i.refresh(true).await;
                let mut st = i.st.lock().unwrap();
                if !st.levels_tried {
                    st.levels_tried = true;
                    st.levels = i.board.reader().and_then(Levels::open);
                }
                let mut v = i.state_view(&st);
                v["header"] = i.header_json();
                v["board"] = i.board_info();
                v["levels"] = Inner::levels_view(&st);
                v["levelReader"] = i.reader_view(&st);
                json(v)
            }),
            self.route(Method::POST, "/api/gpio/watch", j!({ "description": "Start or stop watching a gpiochip line's level (streamed on /ws/gpio as {type:'levels'}). The line is requested as-is, so its direction, bias and drive don't change; lines in use by a driver are refused. Not for the Pi's header bank, whose levels are always live", "body": { "chip": "gpiochip2", "line": 17, "watch": true } }), |i, r| async move {
                let a = r.json()?;
                let chip = a.get("chip").map(js_string).unwrap_or_default();
                let line = num_arg(&a, "line");
                if !(chip.len() > 8 && chip.starts_with("gpiochip") && chip[8..].bytes().all(|c| c.is_ascii_digit())) {
                    return Err(http(400, "chip must be a gpiochip name, e.g. \"gpiochip2\""));
                }
                if a.get("watch") == Some(&Value::Bool(false)) {
                    // A driven line isn't let go by an unwatch: that's POST /api/gpio/line, mode release.
                    let k = line_key(&chip, line as u32);
                    if i.st.lock().unwrap().watches.get(&k).is_some_and(|x| x.handle.mode != Mode::AsIs) {
                        return Err(http(409, format!("{chip} line {} is driven by BenchBob: release it with POST /api/gpio/line (mode: release)", line as u32)));
                    }
                    if i.unwatch(&k) {
                        i.refresh_chips();
                        i.ctx().changed();
                    }
                    return json(j!({ "ok": true, "watching": false }));
                }
                let mut v = j!({ "ok": true, "watching": true });
                if let Value::Object(w) = i.watch(&chip, line)? {
                    v.as_object_mut().unwrap().extend(w);
                }
                json(v)
            }),
            self.route(Method::POST, "/api/gpio/line", j!({ "description": "Drive a gpiochip line on any board (needs output control on): mode high|low (requested as an output through the v2 uAPI and held), input (with pull up|down|none), or release. Give {chip, line} or {pin} (a physical pin of the board's first header, or of header: id). Lines owned by a driver are refused; on a Raspberry Pi the header bank is driven with POST /api/gpio/:gpio instead. Turning output control off releases every driven line", "body": { "chip": "gpiochip0", "line": 17, "mode": "high", "pull": "none" } }), |i, r| async move {
                let a = r.json()?;
                if a.get("pin").is_some_and(|p| !p.is_null()) || a.get("chip").is_some() {
                    i.refresh_chips();
                }
                let (chip, line) = line_target(&i, &a)?;
                let v = i.drive(&chip, line, str_arg(&a, "mode"), str_arg(&a, "pull"))?;
                let mut out = j!({ "ok": true });
                if let Value::Object(m) = v {
                    out.as_object_mut().unwrap().extend(m);
                }
                json(out)
            }),
            self.route(Method::GET, "/api/gpio/overlays", j!("Boards other than the Pi: the device-tree overlays the boot file enables (armbianEnv.txt, orangepiEnv.txt, extlinux.conf, uEnv.txt, or the board's script), the ones installed on this image, which header pins each documented one takes, and conflicts"), |i, _| async move { json(overlays::info(&i).await?) }),
            self.route(Method::POST, "/api/gpio/overlays/stage", j!({ "description": "Compute a boot-overlay change and return a unified diff + token. Nothing is written. Only installed overlays can be enabled; a change that puts two documented overlays on one pin is refused (400)", "body": { "enable": ["i2c5-m3"], "disable": ["uart1-m1"] } }), |i, r| async move { json(overlays::stage(&i, &r.json()?).await?) }),
            self.route(Method::POST, "/api/gpio/overlays/apply", j!({ "description": "Write the staged overlay change (backup first: <file>.bak-<timestamp>). Needs the stage token and confirm: true. Takes effect after a reboot", "body": { "token": "…", "confirm": true } }), |i, r| async move { json(overlays::apply(&i, &r.json()?).await?) }),
            self.route(Method::POST, "/api/gpio/output-control", j!({ "description": "Turn GPIO output control on/off (default off). Enabling requires confirm: true. Turning it off releases every gpiochip line BenchBob drives", "body": { "enabled": true, "confirm": true } }), |i, r| async move {
                let a = r.json()?;
                let en = a.get("enabled").is_some_and(truthy);
                if en && a.get("confirm") != Some(&Value::Bool(true)) {
                    return Err(http(400, "Enabling output control lets this API drive header pins; repeat with \"confirm\": true"));
                }
                i.ctx().set_setting("outputEnabled", Value::Bool(en));
                let released = if en { 0 } else { i.release_outputs() };
                let extra = if released > 0 { format!(" ({released} driven line{} released)", if released == 1 { "" } else { "s" }) } else { String::new() };
                i.ctx().event(&format!("GPIO output control {}{extra}", if en { "enabled" } else { "disabled" }), None);
                if released > 0 {
                    i.refresh_chips();
                }
                i.ctx().changed();
                json(j!({ "outputEnabled": en }))
            }),
            self.route(Method::GET, "/api/gpio/interfaces", j!("Raspberry Pi: I²C / SPI / UART / 1-Wire and the extra I²C/UART controllers: what config.txt says ([all] context), what is live now, the GPIOs each uses and any pin conflicts. Other boards: see /api/gpio/overlays"), |i, _| async move {
                overlays::pi_only(&i)?;
                json(overlays::interfaces(&i).await?)
            }),
            self.route(Method::POST, "/api/gpio/interfaces/stage", j!({ "description": "Compute a config.txt change and return a unified diff + token. Nothing is written. extraI2c (i2c3–6, pins = pins_a_b), extraUart (uart2–5) and i2cGpio are offered on BCM2711 boards only (see catalog in GET /api/gpio/interfaces). A change that puts two interfaces on one GPIO is refused (400)", "body": { "i2c": { "enabled": true, "baudrate": 400000 }, "spi": { "enabled": true }, "uart": { "enabled": true }, "onewire": { "enabled": true, "pin": 4 }, "extraI2c": { "i2c6": { "enabled": true, "pins": "pins_22_23", "baudrate": 400000 } }, "extraUart": { "uart2": { "enabled": true, "ctsrts": false } }, "i2cGpio": { "enabled": false, "sda": 23, "scl": 24, "bus": 11 } } }), |i, r| async move {
                overlays::pi_only(&i)?;
                json(overlays::stage_config(&i, &r.json()?).await?)
            }),
            self.route(Method::POST, "/api/gpio/interfaces/apply", j!({ "description": "Write the staged config.txt (backup first: config.txt.bak-<timestamp>). Needs the stage token and confirm: true. Takes effect after a reboot", "body": { "token": "…", "confirm": true } }), |i, r| async move {
                overlays::pi_only(&i)?;
                json(overlays::apply_config(&i, &r.json()?).await?)
            }),
            self.route(Method::POST, "/api/gpio/reboot", j!({ "description": "Reboot so config.txt or boot-overlay changes take effect. Requires confirm: \"REBOOT\"", "body": { "confirm": "REBOOT" } }), |i, r| async move {
                let a = r.json()?;
                if str_arg(&a, "confirm") != Some("REBOOT") {
                    return Err(http(400, "Rebooting needs confirm: \"REBOOT\""));
                }
                if i.env.config_overridden {
                    return Err(http(409, format!("Refusing to reboot: BENCHBOB_CONFIG_TXT points at a test copy ({})", i.env.config.display())));
                }
                if let Some(e) = &i.env.boot_env {
                    return Err(http(409, format!("Refusing to reboot: BENCHBOB_BOOT_ENV points at a test copy ({e})")));
                }
                i.ctx().event("Reboot requested from the GPIO panel", None);
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    run(&["sudo", "-n", "systemctl", "reboot"], Opts::default()).await;
                });
                json(j!({ "ok": true, "message": "Rebooting in a moment" }))
            }),
            self.route(Method::POST, "/api/gpio/:gpio", j!({ "description": "Drive a header GPIO on the Raspberry Pi (needs output control on): mode high|low|input, pull up|down|none. Refused for GPIO0/1, >27, kernel-claimed lines and pins in an ALT function. Other boards: POST /api/gpio/line", "body": { "mode": "high", "pull": "none" } }), |i, r| async move {
                let p = r.param("gpio");
                let g = benchbob_core::http::parse_int(p.strip_prefix("gpio").or_else(|| p.strip_prefix("GPIO")).unwrap_or(p));
                let Some(g) = g else { return Err(http(400, "GPIO number required, e.g. /api/gpio/17")) };
                let a = r.json()?;
                json(i.write_pin(g as f64, str_arg(&a, "mode"), str_arg(&a, "pull")).await?)
            }),
        ]
    }

    fn tools(&self) -> Vec<Tool> {
        let (i1, i2, i3) = (self.inner.clone(), self.inner.clone(), self.inner.clone());
        vec![
            Tool::new(
                "gpio_read",
                "Read GPIO state. On a Raspberry Pi: for each pin the function (input/output/ALT name like SDA1, TXD1), level, pull, header pin number and kernel consumer; optionally limit to a list of BCM GPIO numbers. On other boards: the board's header pins with their gpiochip line, owner and direction, and every chip's lines in use. `lines` reads levels of specific gpiochip lines (any board, incl. USB GPIO adapters) with an as-is request, so the pin isn't reconfigured; lines a driver holds can't be read.",
                j!({ "type": "object", "properties": {
                    "gpios": { "type": "array", "items": { "type": "integer", "minimum": 0, "maximum": 57 }, "description": "Raspberry Pi: BCM numbers; default all of 0–27" },
                    "lines": { "type": "array", "items": { "type": "object", "properties": { "chip": { "type": "string", "description": "e.g. gpiochip2" }, "line": { "type": "integer", "minimum": 0 } }, "required": ["chip", "line"] }, "description": "gpiochip lines to read once" },
                } }),
                move |a| {
                    let i = i1.clone();
                    async move {
                        let lines = match a.get("lines").and_then(Value::as_array).filter(|l| !l.is_empty()) {
                            Some(l) => {
                                i.refresh_chips();
                                Some(
                                    l.iter()
                                        .take(64)
                                        .map(|x| i.read_once(&x.get("chip").map(js_string).unwrap_or_default(), num_arg(x, "line")))
                                        .collect::<Vec<_>>(),
                                )
                            }
                            None => None,
                        };
                        if !i.is_pi() {
                            i.refresh_chips();
                            let st = i.st.lock().unwrap();
                            let chips = Inner::chips_view(&st, false);
                            let headers = i.headers_view(&st);
                            let mut v = j!({
                                "board": i.board_info(),
                                "headers": headers.as_array().unwrap().iter().map(|h| j!({
                                    "header": h["name"],
                                    "pins": h["pins"].as_array().unwrap().iter().filter(|p| p["kind"] == "gpio").map(|p| j!({ "pin": p["pin"], "soc": p["soc"], "chip": p["chipName"], "line": p["line"], "lineName": p["lineName"], "consumer": p["consumer"], "direction": p["direction"], "funcs": p.get("funcs").cloned().unwrap_or(j!([])) })).collect::<Vec<_>>(),
                                })).collect::<Vec<_>>(),
                                "chips": chips.as_array().unwrap().iter().map(|c| j!({ "name": c["name"], "label": c["label"], "device": c["device"], "usb": c["usb"], "lines": c["count"], "error": c["error"], "inUse": c["lines"].as_array().unwrap().iter().filter(|l| l["used"] == true).map(|l| j!({ "line": l["offset"], "name": l["name"], "consumer": l["consumer"], "direction": l["direction"] })).collect::<Vec<_>>() })).collect::<Vec<_>>(),
                            });
                            if let Some(l) = lines {
                                v["read"] = j!(l);
                            }
                            return Ok(v);
                        }
                        i.refresh(true).await;
                        let st = i.st.lock().unwrap();
                        let want: Option<Vec<u32>> = a
                            .get("gpios")
                            .and_then(Value::as_array)
                            .filter(|g| !g.is_empty())
                            .map(|g| g.iter().map(|x| js_number(x) as u32).collect());
                        let view = i.state_view(&st);
                        let pins: Vec<Value> = view["pins"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter(|p| {
                                let g = p["gpio"].as_u64().unwrap_or(99) as u32;
                                match &want {
                                    Some(w) => w.contains(&g),
                                    None => g <= 27,
                                }
                            })
                            .cloned()
                            .collect();
                        let mut v = j!({ "outputEnabled": i.output_enabled(), "pins": pins });
                        if let Some(l) = lines {
                            v["read"] = j!(l);
                        }
                        Ok(v)
                    }
                },
            ),
            Tool::new(
                "gpio_write",
                "Drive a GPIO: mode high/low (output) or input, and/or pull up/down/none; mode release lets go of a gpiochip line BenchBob holds. Raspberry Pi header: give gpio (BCM number). Any board: give chip + line (gpiochip and offset, e.g. a USB GPIO adapter) or pin (physical header pin). Only works when the user has enabled GPIO output control on the dashboard; refused for pins used by the kernel or a peripheral.",
                j!({ "type": "object", "properties": {
                    "gpio": { "type": "integer", "minimum": 2, "maximum": 27, "description": "Raspberry Pi: BCM GPIO number on the header" },
                    "chip": { "type": "string", "description": "gpiochip name, e.g. gpiochip2" },
                    "line": { "type": "integer", "minimum": 0 },
                    "pin": { "type": "integer", "minimum": 1, "description": "Physical header pin (boards with a mapped header)" },
                    "header": { "type": "string", "description": "Header id when the board has several" },
                    "mode": { "enum": ["high", "low", "input", "release"] },
                    "pull": { "enum": ["up", "down", "none"] },
                } }),
                move |a| {
                    let i = i2.clone();
                    async move {
                        if a.get("gpio").is_some_and(|g| !g.is_null()) {
                            return i.write_pin(num_arg(&a, "gpio"), str_arg(&a, "mode"), str_arg(&a, "pull")).await;
                        }
                        i.refresh_chips();
                        let (chip, line) = line_target(&i, &a)?;
                        i.drive(&chip, line, str_arg(&a, "mode"), str_arg(&a, "pull"))
                    }
                },
            ),
            Tool::new(
                "gpio_interfaces",
                "Raspberry Pi: show whether I²C (and extra buses i2c3–6, i2c-gpio), SPI, UART (extra UARTs uart2–5, and the serial console) and 1-Wire are enabled, which GPIOs each uses, and any pin conflicts, both in /boot/firmware/config.txt and live. Other boards: the boot device-tree overlays (armbianEnv.txt, orangepiEnv.txt, extlinux.conf, uEnv.txt or the board's script), the installed ones and the header pins each takes. Read-only; changing them is done from the dashboard (needs a reboot).",
                j!({ "type": "object", "properties": {} }),
                move |_| {
                    let i = i3.clone();
                    async move {
                        if i.is_pi() {
                            return overlays::interfaces(&i).await;
                        }
                        if overlays::target(&i).is_some() {
                            return overlays::info(&i).await;
                        }
                        Ok(j!({ "board": i.board_info(), "message": overlays::no_overlays(&i) }))
                    }
                },
            ),
        ]
    }
}

impl Inner {
    /// `levelReader` in GET /api/gpio: the board's passive reader, whether it opened, and
    /// whether it has ever run on hardware (RP1: no).
    fn reader_view(&self, st: &State) -> Value {
        let r = self.board.reader();
        let note = match r {
            Some(Reader::Rp1) => "RP1 (Pi 5) register reader, written from the RP1 datasheet; untested: nobody here has a Pi 5",
            Some(Reader::Bcm2711Gpiomem) => "GPLEV0/1 through /dev/gpiomem",
            Some(Reader::Pinctrl) | None if self.is_pi() => "pinctrl every tick",
            _ => "levels only for watched lines (uAPI line requests)",
        };
        j!({
            "reader": r.map(|r| r.as_str()),
            "active": st.levels.as_ref().map(|l| l.reader.as_str()),
            "tested": !matches!(r, Some(Reader::Rp1)),
            "note": note,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_dedupe_strips_only_the_first_timestamp() {
        assert_eq!(
            strip_at(r#"{"type":"state","mode":"rpi","at":1792532090542,"error":"","x":{"at":5,"y":1}}"#),
            r#"{"type":"state","mode":"rpi","error":"","x":{"at":5,"y":1}}"#
        );
        assert_eq!(strip_at(r#"{"a":1}"#), r#"{"a":1}"#);
    }

    #[test]
    fn backup_names_are_utc_iso_seconds() {
        assert_eq!(backup_stamp(0), "1970-01-01-00-00-00");
        assert_eq!(backup_stamp(1_792_532_090_542), "2026-10-20-21-34-50");
        assert_eq!(backup_stamp(951_782_400_000), "2000-02-29-00-00-00");
    }

    #[test]
    fn pi_refusals() {
        let p = |gpio, fsel: &str, func: &str| PinState {
            gpio,
            fsel: fsel.into(),
            func: func.into(),
            level: None,
            pull: None,
            drive: None,
            name: String::new(),
        };
        assert_eq!(Inner::refusal(None, None).unwrap(), "unknown GPIO");
        assert!(Inner::refusal(Some(&p(1, "ip", "input")), None).unwrap().contains("HAT ID EEPROM"));
        assert!(Inner::refusal(Some(&p(28, "ip", "input")), None).unwrap().contains("SoC-internal"));
        assert_eq!(Inner::refusal(Some(&p(14, "a5", "TXD1")), None).unwrap(), "muxed to TXD1 (A5)");
        assert!(Inner::refusal(Some(&p(17, "ip", "input")), None).is_none());
        let li = LineInfo {
            offset: 4,
            name: "GPIO4".into(),
            consumer: Some("onewire@4".into()),
            used: true,
            direction: "output",
            active_low: false,
            drive: "open-drain",
            bias: None,
            edge: None,
            debounce_us: None,
            flags: vec![],
        };
        assert_eq!(Inner::refusal(Some(&p(4, "ip", "input")), Some(&li)).unwrap(), "claimed by the kernel (onewire@4)");
    }

    fn gpio_for(board: &str) -> Gpio {
        let boards = benchbob_boards::Boards::builtin();
        let i = benchbob_boards::DetectInput::default();
        Gpio::new(Arc::new(benchbob_boards::describe(boards.get(board).unwrap(), &i, true)))
    }

    fn chip(name: &str, label: &str, device: &str, lines: u32) -> ChipState {
        let info = (0..lines)
            .map(|o| LineInfo {
                offset: o,
                name: String::new(),
                consumer: None,
                used: false,
                direction: "input",
                active_low: false,
                drive: "push-pull",
                bias: None,
                edge: None,
                debounce_us: None,
                flags: vec!["input".into()],
            })
            .collect();
        ChipState {
            chip: Chip {
                info: uapi::ChipInfo { name: name.into(), label: label.into(), lines },
                path: format!("/dev/{name}"),
                device: device.into(),
                usb: false,
                error: None,
            },
            info,
        }
    }

    #[test]
    fn header_pins_address_gpiochip_lines() {
        let g = gpio_for("orangepi-5-plus");
        g.inner.st.lock().unwrap().chips.list =
            vec![chip("gpiochip0", "gpio0", "fd8a0000.gpio", 32), chip("gpiochip1", "gpio1", "fec20000.gpio", 32)];
        // Pin 7 is GPIO1_D6: bank gpio1, line 30.
        assert_eq!(g.inner.pin_line(None, 7.0).unwrap(), ("gpiochip1".to_string(), 30));
        assert_eq!(g.inner.pin_line(Some("40pin"), 3.0).unwrap(), ("gpiochip0".to_string(), 16));
        assert!(g.inner.pin_line(None, 1.0).unwrap_err().message.contains("3v3"));
        assert!(g.inner.pin_line(None, 41.0).unwrap_err().message.contains("1–40"));
        // Pin 12 is on gpio3, which this fake machine doesn't have.
        assert_eq!(g.inner.pin_line(None, 12.0).unwrap_err().status, 503);
        assert_eq!(g.inner.pin_line(Some("j1"), 3.0).unwrap_err().status, 404);
        // The headers view carries the resolved chip and line info.
        let st = g.inner.st.lock().unwrap();
        let h = g.inner.headers_view(&st);
        assert_eq!(h[0]["pins"][6]["chipName"], "gpiochip1");
        assert_eq!(h[0]["pins"][11]["chipName"], Value::Null);
        let keys: Vec<&String> = h[0]["pins"][6].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            ["pin", "kind", "soc", "chip", "line", "funcs", "chipName", "lineName", "consumer", "used", "direction", "flags", "watched"]
        );
    }

    #[test]
    fn the_pi_header_bank_is_addressed_by_bcm_number() {
        let g = gpio_for("rpi");
        assert_eq!(g.inner.gpio_to_pin(4), Some(7));
        assert_eq!(g.inner.gpio_to_pin(21), Some(40));
        assert!(g.inner.pin_line(None, 37.0).unwrap_err().message.contains("/api/gpio/26"));
        assert!(is_soc_bank("pinctrl-bcm2711") && is_soc_bank("pinctrl-rp1") && !is_soc_bank("raspberrypi-exp-gpio"));
    }
}
