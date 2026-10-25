//! Addressable LED strings on a node (`/api/leds`, `leds_*` MCP tools), with the same
//! shapes as the Linux server's LED strips panel. The colour, timing, encoding and
//! pattern logic is `benchbob_pixels`; this is the configuration, validation and JSON.
//!
//! The API only edits [`Leds`]. A board's LED task does the hardware: it polls
//! [`Leds::job`] for each slot, (re)claims its PIO/RMT/SPI when `config_gen` changes and
//! reports the outcome with [`Leds::report`], and calls [`Leds::render`] for a fresh
//! frame whenever `content_gen` changes or the slot is animated. With output control
//! off, `render` gives an all-black frame once and then nothing is animated, so strings
//! go dark and stay dark until output control is on again.
//!
//! A string can also take its picture from the network ([`NetIn`]: sACN, Art-Net or DDP).
//! The board hands every packet to [`Leds::receive`]; while packets arrive the string shows
//! them, and [`Leds::tick`] puts its own pattern back once they stop for
//! [`NET_TIMEOUT_MS`] (as WLED does), so a show controller can take over and let go.

use super::{ApiErr, Arg, BoardInfo, R, arg, arg_value, int_arg};
use crate::config::FixedStr;
use crate::json::{Json, Kind, Value};
use benchbob_pixels::color::PowerBudget;
use benchbob_pixels::net::{self, ARTNET_PORT, DDP_PORT, Patch, SACN_PORT, SacnArbiter};
use benchbob_pixels::pattern::{self, Pattern};
use benchbob_pixels::timing::{PRESETS, Timing};
use benchbob_pixels::{Order, Rgbw};
use core::fmt::Write;

/// Slots the API keeps (a board may offer fewer: [`LedsInfo::max_strings`]).
pub const MAX_STRINGS: usize = 4;
/// Pixels per string the API keeps a buffer for (a board may offer fewer).
pub const MAX_PIXELS: usize = 1024;
/// A network-fed string goes back to its own pattern after this long without packets.
pub const NET_TIMEOUT_MS: u64 = 2500;
/// Universes one string can span (1024 RGBW pixels at 128 a universe).
const MAX_UNIVERSES: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    Sacn,
    ArtNet,
    Ddp,
}

impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Sacn => "sacn",
            Protocol::ArtNet => "artnet",
            Protocol::Ddp => "ddp",
        }
    }
    pub fn port(self) -> u16 {
        match self {
            Protocol::Sacn => SACN_PORT,
            Protocol::ArtNet => ARTNET_PORT,
            Protocol::Ddp => DDP_PORT,
        }
    }
}

/// Where a string listens on the network. Channels arrive in R, G, B(, W) order, three or
/// four per pixel by the string's order; `universe` is the first universe (sACN 1–63999,
/// Art-Net's 15-bit port-address 0–32767) and the string runs on into the next ones.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NetIn {
    pub protocol: Protocol,
    pub universe: u16,
    /// 1-based, in the first universe.
    pub start_channel: u16,
    /// DDP: byte offset of the first pixel.
    pub ddp_offset: u32,
}

/// What a board's LED hardware can do.
pub struct LedsInfo {
    pub max_strings: u8,
    pub max_pixels: u16,
    /// WS2812-family (timed, one data pin).
    pub one_wire: bool,
    /// APA102 / WS2801 (data + clock).
    pub clocked: bool,
    /// Why a one-wire timing can't be produced here, if it can't.
    pub check_timing: fn(&Timing) -> Result<(), &'static str>,
    /// Highest clock for clocked strings.
    pub max_clock_hz: u32,
}

/// The chip names, in the UI's order: the one-wire presets first (same order as
/// `benchbob_pixels::timing::PRESETS`), then custom one-wire, then the clocked chips.
pub const CHIPS: &[(&str, &str)] = &[
    ("ws2812b", "WS2812B / WS2812 / WS2811 at 800 kHz (most strips)"),
    ("sk6812", "SK6812 (RGB and RGBW)"),
    ("ws2813", "WS2813 / WS2815 (12 V, backup data line)"),
    ("ws2811-400k", "WS2811 in 400 kHz (slow) mode"),
    ("custom", "Any other one-wire chip: set the timing"),
    ("apa102", "APA102 / SK9822 / HD107S (data + clock)"),
    ("ws2801", "WS2801 (data + clock)"),
];
const APA102: u8 = 5;
const WS2801: u8 = 6;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Chip(u8);

impl Chip {
    fn parse(s: &str) -> Option<Chip> {
        CHIPS.iter().position(|c| c.0.eq_ignore_ascii_case(s)).map(|i| Chip(i as u8))
    }
    pub fn name(self) -> &'static str {
        CHIPS[self.0 as usize].0
    }
    /// Data + clock (APA102, WS2801) rather than timed one-wire.
    pub fn clocked(self) -> bool {
        self.0 >= APA102
    }
    pub fn is_apa102(self) -> bool {
        self.0 == APA102
    }
    fn preset(self) -> Option<Timing> {
        PRESETS.get(self.0 as usize).map(|p| p.2)
    }
    fn default_order(self) -> Order {
        match self.0 {
            APA102 => Order::BGR,
            WS2801 | 3 => Order::RGB,
            _ => Order::GRB,
        }
    }
    fn default_clock_hz(self) -> u32 {
        if self.0 == APA102 { 2_000_000 } else { 1_000_000 }
    }
}

#[derive(Clone, Copy)]
pub struct StringConfig {
    pub name: FixedStr<24>,
    pub chip: Chip,
    pub pin: u8,
    pub clock_pin: Option<u8>,
    pub pixels: u16,
    pub order: Order,
    /// One-wire chips: the preset's timing, or the custom one.
    pub timing: Timing,
    /// Clocked chips.
    pub clock_hz: u32,
    pub brightness: u8,
    pub gamma: bool,
    /// Drive the data line inverted (for an inverting level shifter).
    pub invert: bool,
    pub power: Option<PowerBudget>,
    pub net: Option<NetIn>,
}

impl StringConfig {
    /// Wire bytes one frame needs (before any SPI/APA102 framing).
    pub fn wire_len(&self) -> usize {
        self.pixels as usize * self.order.channels()
    }

    /// The string's place in universe / DDP space, if it listens.
    pub fn patch(&self) -> Option<Patch> {
        let n = self.net?;
        let cpp = if self.order.has_white() { 4 } else { 3 };
        let mut p = Patch::new(self.pixels, cpp, n.universe);
        p.start_channel = n.start_channel;
        p.ddp_offset = n.ddp_offset;
        Some(p)
    }
}

#[derive(Clone, Copy)]
enum Source {
    Pattern {
        pattern: Pattern,
        since_ms: u64,
    },
    Pixels,
    /// Showing network data (in `pixels`); `NetRx::prev` is what comes back after.
    Network,
}

/// A string's receiving state.
#[derive(Clone, Copy)]
struct NetRx {
    /// One per universe the string spans: sACN sequence numbers and priorities are
    /// per universe.
    arb: [SacnArbiter; MAX_UNIVERSES],
    last_ms: Option<u64>,
    packets: u32,
    /// The sACN source's name, or the sender's address for the others.
    from: FixedStr<40>,
    prev: Option<Source>,
    /// This DDP sender marks frame ends (push), so only those render.
    push_seen: bool,
}

impl NetRx {
    const fn new() -> Self {
        NetRx { arb: [SacnArbiter::new(); MAX_UNIVERSES], last_ms: None, packets: 0, from: FixedStr::new(), prev: None, push_seen: false }
    }
}

const ERROR_MAX: usize = 240;

// No heap here: an error carries its text inline, and every slot is allocated anyway.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy)]
enum Hw {
    Pending,
    Running,
    /// Long enough for the Linux server's how-to-fix messages.
    Error(FixedStr<ERROR_MAX>),
}

#[derive(Clone)]
struct Slot {
    id: u16,
    cfg: StringConfig,
    source: Source,
    config_gen: u32,
    content_gen: u32,
    hw: Hw,
    estimate_ma: Option<u32>,
    net: NetRx,
    pixels: [Rgbw; MAX_PIXELS],
}

/// What a board's LED task needs for one slot.
#[derive(Clone, Copy)]
pub struct Job {
    pub id: u16,
    pub config: StringConfig,
    /// Changes whenever the config does: reclaim the hardware.
    pub config_gen: u32,
    /// Changes whenever the picture does: render a frame.
    pub content_gen: u32,
    /// Render every frame (a moving pattern with output on).
    pub animated: bool,
    /// Output control is on.
    pub output: bool,
}

/// Every string's config and content. Clone is for trying a change before keeping it
/// (the Linux server checks a config against its hardware that way).
#[derive(Clone)]
pub struct Leds {
    slots: [Option<Slot>; MAX_STRINGS],
    next_id: u16,
    counter: u32,
    output: bool,
    /// The time of the last tick/receive, for "how long ago" in the view.
    now_ms: u64,
    /// Changes when something worth saving does (a string's config, its pattern, a
    /// string added or removed); not for pixels or network data.
    persist_gen: u32,
}

impl Default for Leds {
    fn default() -> Self {
        Self::new()
    }
}

impl Leds {
    pub const fn new() -> Self {
        Leds { slots: [const { None }; MAX_STRINGS], next_id: 1, counter: 0, output: false, now_ms: 0, persist_gen: 0 }
    }

    fn bump(&mut self) -> u32 {
        self.counter = self.counter.wrapping_add(1);
        self.counter
    }

    /// Output control changed: every string redraws (black when off).
    pub fn set_output(&mut self, on: bool) {
        if self.output != on {
            self.output = on;
            for i in 0..MAX_STRINGS {
                let g = self.bump();
                if let Some(s) = self.slots[i].as_mut() {
                    s.content_gen = g;
                }
            }
        }
    }

    pub fn job(&self, i: usize) -> Option<Job> {
        let s = self.slots.get(i)?.as_ref()?;
        let moving = matches!(s.source, Source::Pattern { pattern, .. } if !pattern.is_static());
        Some(Job {
            id: s.id,
            config: s.cfg,
            config_gen: s.config_gen,
            content_gen: s.content_gen,
            animated: self.output && moving,
            output: self.output,
        })
    }

    /// Slot `i`'s frame at `now_ms`, as wire bytes (colour order, gamma, brightness, power
    /// limit applied) into `out`; returns the length. All zeros with output control off.
    pub fn render(&mut self, i: usize, now_ms: u64, out: &mut [u8]) -> usize {
        let output = self.output;
        let Some(s) = self.slots.get_mut(i).and_then(|s| s.as_mut()) else { return 0 };
        let c = s.cfg;
        let n = (c.pixels as usize).min(MAX_PIXELS);
        let ch = c.order.channels();
        let len = (n * ch).min(out.len() / ch * ch);
        let out = &mut out[..len];
        if !output {
            out.fill(0);
            s.estimate_ma = None;
            return len;
        }
        for (k, o) in out.chunks_exact_mut(ch).enumerate() {
            let px = match s.source {
                Source::Pattern { pattern, since_ms } => pattern.pixel(now_ms.saturating_sub(since_ms) as u32, k, n),
                Source::Pixels | Source::Network => s.pixels[k],
            };
            c.order.write(px, c.gamma, c.brightness, o);
        }
        s.estimate_ma = c.power.map(|p| p.limit(out, n));
        len
    }

    /// The board's outcome of claiming hardware for `config_gen` (ignored when stale).
    pub fn report(&mut self, i: usize, config_gen: u32, result: Result<(), &str>) {
        if let Some(s) = self.slots.get_mut(i).and_then(|s| s.as_mut())
            && s.config_gen == config_gen
        {
            s.hw = match result {
                Ok(()) => Hw::Running,
                Err(e) => {
                    let mut n = e.len().min(ERROR_MAX);
                    while !e.is_char_boundary(n) {
                        n -= 1;
                    }
                    Hw::Error(FixedStr::from_bytes(&e.as_bytes()[..n]).unwrap_or(FixedStr::new()))
                }
            };
        }
    }

    /// The name of the string holding `gpio` (as data or clock), if any.
    pub fn owner(&self, gpio: u8) -> Option<&str> {
        self.slots.iter().flatten().find(|s| s.cfg.pin == gpio || s.cfg.clock_pin == Some(gpio)).map(|s| s.cfg.name.as_str())
    }

    /// A UDP packet that arrived on `port` (5568 sACN, 6454 Art-Net, 4048 DDP) from
    /// `from` (shown as the source when the protocol has no name of its own). True when a
    /// string's picture changed.
    pub fn receive(&mut self, port: u16, packet: &[u8], from: &str, now_ms: u64) -> bool {
        self.now_ms = now_ms;
        let mut changed = false;
        for i in 0..MAX_STRINGS {
            let g = self.counter.wrapping_add(1);
            let Some(s) = self.slots[i].as_mut() else { continue };
            let (Some(n), Some(patch)) = (s.cfg.net, s.cfg.patch()) else { continue };
            if n.protocol.port() != port {
                continue;
            }
            let mut hit = false;
            let mut show = true;
            let mut name: Option<&[u8]> = None;
            let pixels = &mut s.pixels;
            let mut put = |k: usize, ch: &[u8]| {
                if let Some(px) = pixels.get_mut(k) {
                    *px = Rgbw { r: ch[0], g: ch[1], b: ch[2], w: ch.get(3).copied().unwrap_or(0) };
                    hit = true;
                }
            };
            match n.protocol {
                Protocol::Sacn => {
                    let Ok(p) = net::parse_sacn(packet) else { continue };
                    let (first, last) = patch.universes();
                    if p.dmx.universe < first || p.dmx.universe > last {
                        continue;
                    }
                    let u = (p.dmx.universe - first) as usize;
                    if u >= MAX_UNIVERSES || !s.net.arb[u].accept(&p, now_ms as u32) {
                        continue;
                    }
                    patch.apply_dmx(&p.dmx, &mut put);
                    name = Some(p.source_name);
                }
                Protocol::ArtNet => {
                    let Ok(d) = net::parse_artnet(packet) else { continue };
                    patch.apply_dmx(&d, &mut put);
                }
                Protocol::Ddp => {
                    let Ok(d) = net::parse_ddp(packet) else { continue };
                    patch.apply_ddp(&d, &mut put);
                    s.net.push_seen |= d.push;
                    // Senders that mark frame ends get one render per frame.
                    show = d.push || !s.net.push_seen;
                }
            }
            if !hit {
                continue;
            }
            if !matches!(s.source, Source::Network) {
                s.net.prev = Some(s.source);
                s.source = Source::Network;
            }
            s.net.last_ms = Some(now_ms);
            s.net.packets = s.net.packets.wrapping_add(1);
            s.net.from = FixedStr::from_bytes(name.filter(|n| !n.is_empty()).map(|n| &n[..n.len().min(40)]).unwrap_or(from.as_bytes()))
                .unwrap_or_else(|| FixedStr::from_bytes(&from.as_bytes()[..from.len().min(40)]).unwrap_or(FixedStr::new()));
            if show {
                self.counter = g;
                s.content_gen = g;
                changed = true;
            }
        }
        changed
    }

    /// Call every frame: strings whose network input went quiet go back to their own
    /// picture. True when one did.
    pub fn tick(&mut self, now_ms: u64) -> bool {
        self.now_ms = now_ms;
        let mut changed = false;
        for i in 0..MAX_STRINGS {
            let g = self.counter.wrapping_add(1);
            let Some(s) = self.slots[i].as_mut() else { continue };
            if matches!(s.source, Source::Network) && s.net.last_ms.is_none_or(|t| now_ms.saturating_sub(t) > NET_TIMEOUT_MS) {
                s.source = match s.net.prev.take() {
                    Some(Source::Pattern { pattern, .. }) => Source::Pattern { pattern, since_ms: now_ms },
                    // Pixels set earlier were overwritten by the network data: dark.
                    _ => Source::Pattern { pattern: Pattern::Off, since_ms: now_ms },
                };
                self.counter = g;
                s.content_gen = g;
                changed = true;
            }
        }
        changed
    }

    /// The protocols some string listens on (bits: 1 sACN, 2 Art-Net, 4 DDP).
    pub fn protocols(&self) -> u8 {
        self.slots.iter().flatten().filter_map(|s| s.cfg.net).fold(0, |m, n| {
            m | match n.protocol {
                Protocol::Sacn => 1,
                Protocol::ArtNet => 2,
                Protocol::Ddp => 4,
            }
        })
    }

    /// Every sACN universe some string listens on (for multicast joins), at most `out.len()`.
    pub fn sacn_universes(&self, out: &mut [u16]) -> usize {
        let mut n = 0;
        for s in self.slots.iter().flatten() {
            if let (Some(NetIn { protocol: Protocol::Sacn, .. }), Some(p)) = (s.cfg.net, s.cfg.patch()) {
                let (a, b) = p.universes();
                for u in a..=b {
                    if n < out.len() && !out[..n].contains(&u) {
                        out[n] = u;
                        n += 1;
                    }
                }
            }
        }
        n
    }

    /// Art-Net port-addresses strings listen on (for ArtPollReply), at most `out.len()`.
    pub fn artnet_universes(&self, out: &mut [u16]) -> usize {
        let mut n = 0;
        for s in self.slots.iter().flatten() {
            if let (Some(NetIn { protocol: Protocol::ArtNet, .. }), Some(p)) = (s.cfg.net, s.cfg.patch()) {
                let (a, b) = p.universes();
                for u in a..=b {
                    if n < out.len() && !out[..n].contains(&u) {
                        out[n] = u;
                        n += 1;
                    }
                }
            }
        }
        n
    }

    /// The slot holding string `id`.
    pub fn slot_of(&self, id: i64) -> Option<usize> {
        self.slots.iter().position(|s| s.as_ref().is_some_and(|s| s.id as i64 == id))
    }

    /// The ids of every string, in slot order.
    pub fn ids(&self) -> impl Iterator<Item = u16> + '_ {
        self.slots.iter().flatten().map(|s| s.id)
    }

    /// Re-create a saved string under its old id (`a` as `string_json` wrote it, minus
    /// the pattern, which goes through `set_pattern`).
    pub fn restore(&mut self, info: &BoardInfo, id: u16, a: Option<&Value<'_>>, now_ms: u64) -> Result<i64, ApiErr> {
        if self.slot_of(id as i64).is_some() {
            return Err(err!(409, "LED string {} already exists", id));
        }
        let next = self.next_id;
        self.next_id = id.max(1);
        let r = self.configure(info, None, a, now_ms);
        self.next_id = next.max(id.wrapping_add(1)).max(1);
        r
    }

    fn index(&self, id: i64) -> Result<usize, ApiErr> {
        self.slots
            .iter()
            .position(|s| s.as_ref().is_some_and(|s| s.id as i64 == id))
            .ok_or_else(|| err!(404, "No LED string {} (see GET /api/leds)", id))
    }

    // -----------------------------------------------------------------------------------
    // API

    pub fn view(&self, info: &BoardInfo, j: &mut Json<'_, '_>) {
        j.obj().kv_bool("outputEnabled", self.output);
        j.key("capabilities");
        match info.leds {
            None => j.null(),
            Some(l) => {
                j.obj()
                    .kv_u64("maxStrings", l.max_strings.min(MAX_STRINGS as u8) as u64)
                    .kv_u64("maxPixels", l.max_pixels.min(MAX_PIXELS as u16) as u64);
                j.kv_u64("maxClockHz", l.max_clock_hz as u64).key("chips").arr();
                for (k, (name, desc)) in CHIPS.iter().enumerate() {
                    let clocked = Chip(k as u8).clocked();
                    if (clocked && l.clocked) || (!clocked && l.one_wire) {
                        j.obj()
                            .kv_str("name", name)
                            .kv_str("kind", if clocked { "clocked" } else { "one-wire" })
                            .kv_str("description", desc)
                            .end_obj();
                    }
                }
                j.end_arr().key("patterns").arr();
                for (name, _) in pattern::NAMES {
                    j.str(name);
                }
                j.end_arr().key("pins").arr();
                for p in info.pins.iter().filter(|p| p.func.is_none()) {
                    j.u64(p.gpio as u64);
                }
                j.end_arr().end_obj()
            }
        };
        j.key("strings").arr();
        for s in self.slots.iter().flatten() {
            self.string_view(s, j);
        }
        j.end_arr().end_obj();
    }

    fn string_view(&self, s: &Slot, j: &mut Json<'_, '_>) {
        let c = &s.cfg;
        j.obj().kv_u64("id", s.id as u64).kv_str("name", c.name.as_str()).kv_str("chip", c.chip.name());
        j.kv_str("kind", if c.chip.clocked() { "clocked" } else { "one-wire" }).kv_u64("pin", c.pin as u64);
        match c.clock_pin {
            Some(p) => j.kv_u64("clockPin", p as u64),
            None => j.key("clockPin").null(),
        };
        let mut name = [0u8; 4];
        let n = c.order.name(&mut name);
        j.kv_u64("pixels", c.pixels as u64).kv_str("order", core::str::from_utf8(&name[..n]).unwrap_or("?"));
        if c.chip.clocked() {
            j.key("timing").null().kv_u64("clockHz", c.clock_hz as u64);
        } else {
            let t = c.timing;
            j.key("timing")
                .obj()
                .kv_u64("t0hNs", t.t0h_ns as u64)
                .kv_u64("t1hNs", t.t1h_ns as u64)
                .kv_u64("periodNs", t.period_ns as u64)
                .kv_u64("resetUs", t.reset_us as u64)
                .end_obj();
            j.key("clockHz").null();
        }
        j.kv_u64("brightness", c.brightness as u64).kv_bool("gamma", c.gamma).kv_bool("invert", c.invert);
        match c.power {
            Some(p) => j
                .key("power")
                .obj()
                .kv_u64("maPerChannel", p.ma_per_channel as u64)
                .kv_u64("idleMa", p.idle_ma as u64)
                .kv_u64("maxMa", p.max_ma as u64)
                .end_obj(),
            None => j.key("power").null(),
        };
        match s.source {
            Source::Pattern { pattern, .. } => {
                j.kv_str("source", "pattern").key("pattern");
                pattern_view(&pattern, j);
            }
            Source::Pixels => {
                j.kv_str("source", "pixels").key("pattern").null();
            }
            Source::Network => {
                j.kv_str("source", "network").key("pattern");
                match s.net.prev {
                    Some(Source::Pattern { pattern, .. }) => pattern_view(&pattern, j),
                    _ => {
                        j.null();
                    }
                }
            }
        }
        match c.net {
            Some(n) => {
                j.key("network").obj().kv_str("protocol", n.protocol.name()).kv_u64("universe", n.universe as u64);
                j.kv_u64("startChannel", n.start_channel as u64).kv_u64("ddpOffset", n.ddp_offset as u64).end_obj();
            }
            None => {
                j.key("network").null();
            }
        }
        match s.net.last_ms {
            Some(t) if c.net.is_some() => {
                j.key("receiving").obj().kv_bool("active", matches!(s.source, Source::Network)).kv_str("from", s.net.from.as_str());
                j.kv_u64("packets", s.net.packets as u64).kv_u64("ageMs", self.now_ms.saturating_sub(t)).end_obj();
            }
            _ => {
                j.key("receiving").null();
            }
        }
        let (state, error) = match s.hw {
            // A string that couldn't be set up says why even while output control is off.
            Hw::Error(e) => ("error", Some(e)),
            _ if !self.output => ("off", None),
            Hw::Pending => ("pending", None),
            Hw::Running => ("running", None),
        };
        j.kv_str("state", state);
        match &error {
            Some(e) => j.kv_str("error", e.as_str()),
            None => j.key("error").null(),
        };
        match s.estimate_ma {
            Some(m) => j.kv_u64("estimateMa", m as u64),
            None => j.key("estimateMa").null(),
        };
        j.end_obj();
    }

    /// Bumps whenever the settings [`Self::persist_json`] writes change.
    pub fn persist_gen(&self) -> u32 {
        self.persist_gen
    }

    /// String `id` as settings to store: its config and pattern, nothing live (no state,
    /// no receiving, no timing for a preset chip). [`Self::restore_json`] reads it back.
    pub fn persist_json(&self, id: i64, j: &mut Json<'_, '_>) -> R {
        let i = self.index(id)?;
        let Some(s) = &self.slots[i] else { return Ok(()) };
        let c = &s.cfg;
        j.obj().kv_u64("id", s.id as u64).kv_str("name", c.name.as_str()).kv_str("chip", c.chip.name()).kv_u64("pin", c.pin as u64);
        if let Some(p) = c.clock_pin {
            j.kv_u64("clockPin", p as u64);
        }
        let mut name = [0u8; 4];
        let n = c.order.name(&mut name);
        j.kv_u64("pixels", c.pixels as u64).kv_str("order", core::str::from_utf8(&name[..n]).unwrap_or("GRB"));
        if c.chip.clocked() {
            j.kv_u64("clockHz", c.clock_hz as u64);
        } else if c.chip.preset().is_none() {
            let t = c.timing;
            j.key("timing").obj().kv_u64("t0hNs", t.t0h_ns as u64).kv_u64("t1hNs", t.t1h_ns as u64);
            j.kv_u64("periodNs", t.period_ns as u64).kv_u64("resetUs", t.reset_us as u64).end_obj();
        }
        j.kv_u64("brightness", c.brightness as u64).kv_bool("gamma", c.gamma).kv_bool("invert", c.invert);
        if let Some(p) = c.power {
            j.key("power").obj().kv_u64("maPerChannel", p.ma_per_channel as u64).kv_u64("idleMa", p.idle_ma as u64);
            j.kv_u64("maxMa", p.max_ma as u64).end_obj();
        }
        if let Some(n) = c.net {
            j.key("network").obj().kv_str("protocol", n.protocol.name()).kv_u64("universe", n.universe as u64);
            j.kv_u64("startChannel", n.start_channel as u64).kv_u64("ddpOffset", n.ddp_offset as u64).end_obj();
        }
        let pattern = match s.source {
            Source::Pattern { pattern, .. } => Some(pattern),
            Source::Network => match s.net.prev {
                Some(Source::Pattern { pattern, .. }) => Some(pattern),
                _ => Some(Pattern::Off),
            },
            // Pixels set through the API aren't settings: such a string comes back dark.
            Source::Pixels => Some(Pattern::Off),
        };
        if let Some(p) = pattern {
            j.key("pattern");
            pattern_view(&p, j);
        }
        j.end_obj();
        Ok(())
    }

    /// Re-create a string from what [`Self::persist_json`] wrote. Returns its id.
    pub fn restore_json(&mut self, info: &BoardInfo, text: &[u8], now_ms: u64) -> Result<i64, ApiErr> {
        let text = core::str::from_utf8(text).map_err(|_| err!(400, "stored LED string isn't UTF-8"))?;
        let v = crate::json::parse(text).map_err(|_| err!(400, "stored LED string isn't valid JSON"))?;
        let id = v
            .get("id")
            .and_then(|i| i.as_i64())
            .filter(|&i| (1..=u16::MAX as i64).contains(&i))
            .ok_or_else(|| err!(400, "stored LED string has no id"))?;
        let id = self.restore(info, id as u16, Some(&v), now_ms)?;
        if let Some(p) = v.get("pattern").filter(|p| p.kind() == Kind::Object) {
            self.set_pattern(id, Some(&p), now_ms)?;
        }
        Ok(id)
    }

    pub fn string_json(&self, id: i64, j: &mut Json<'_, '_>) -> R {
        let i = self.index(id)?;
        if let Some(s) = &self.slots[i] {
            self.string_view(s, j);
        }
        Ok(())
    }

    /// Create (`id` None) or change a string. Returns its id.
    pub fn configure(&mut self, info: &BoardInfo, id: Option<i64>, a: Option<&Value<'_>>, now_ms: u64) -> Result<i64, ApiErr> {
        let Some(l) = info.leds else { return Err(err!(404, "This board has no LED string support")) };
        let existing = match id {
            Some(id) => Some(self.index(id)?),
            None => None,
        };
        let mut c = match existing {
            Some(i) => self.slots[i].as_ref().map(|s| s.cfg).ok_or_else(|| err!(404, "No LED string {}", id.unwrap_or(0)))?,
            None => {
                let max = (l.max_strings as usize).min(MAX_STRINGS);
                if self.slots.iter().flatten().count() >= max {
                    return Err(err!(409, "This board drives at most {} LED strings; remove one first", max));
                }
                let chip = text::<16>(arg(a, "chip"), "chip")?
                    .ok_or_else(|| err!(400, "chip is required (see capabilities.chips in GET /api/leds)"))?;
                let chip = parse_chip(chip.as_str())?;
                let pin = int_arg(arg(a, "pin"), "pin", 0, 255)?.ok_or_else(|| err!(400, "pin is required"))?;
                let pixels = int_arg(arg(a, "pixels"), "pixels", 1, 65535)?.ok_or_else(|| err!(400, "pixels is required"))?;
                StringConfig {
                    name: FixedStr::new(),
                    chip,
                    pin: pin as u8,
                    clock_pin: None,
                    pixels: pixels as u16,
                    order: chip.default_order(),
                    timing: chip.preset().unwrap_or(Timing::WS2812B),
                    clock_hz: chip.default_clock_hz(),
                    brightness: 255,
                    gamma: true,
                    invert: false,
                    power: None,
                    net: None,
                }
            }
        };
        if let Some(chip) = text::<16>(arg(a, "chip"), "chip")? {
            let chip = parse_chip(chip.as_str())?;
            if chip != c.chip {
                if let Some(t) = chip.preset() {
                    c.timing = t;
                }
                if chip.clocked() != c.chip.clocked() || existing.is_none() {
                    c.clock_hz = chip.default_clock_hz();
                }
                c.chip = chip;
            }
        }
        if let Some(n) = text::<24>(arg(a, "name"), "name")? {
            c.name = n;
        }
        if let Some(p) = int_arg(arg(a, "pin"), "pin", 0, 255)? {
            c.pin = p as u8;
        }
        match arg_value(a, "clockPin") {
            Some(_) => c.clock_pin = int_arg(arg(a, "clockPin"), "clockPin", 0, 255)?.map(|p| p as u8),
            None if a.and_then(|a| a.get("clockPin")).is_some() => c.clock_pin = None, // explicit null
            None => {}
        }
        if let Some(n) = int_arg(arg(a, "pixels"), "pixels", 1, l.max_pixels.min(MAX_PIXELS as u16) as i64)? {
            c.pixels = n as u16;
        }
        if let Some(o) = text::<8>(arg(a, "order"), "order")? {
            c.order = Order::parse(o.as_str())
                .ok_or_else(|| err!(400, "order must be R, G and B once each, plus an optional W (e.g. GRB, GRBW), got {}", o.as_str()))?;
        }
        if let Some(t) = arg_value(a, "timing").filter(|v| v.kind() == Kind::Object) {
            if c.chip.preset().is_some() {
                return Err(err!(400, "timing is fixed by the {} preset; use chip \"custom\" to set it", c.chip.name()));
            }
            let t = Some(&t);
            let field = |k: &str, max: i64, cur: u32| -> Result<u32, ApiErr> {
                Ok(int_arg(arg(t, k), k, 1, max)?.map(|v| v as u32).unwrap_or(cur))
            };
            c.timing = Timing {
                t0h_ns: field("t0hNs", 10_000, c.timing.t0h_ns)?,
                t1h_ns: field("t1hNs", 10_000, c.timing.t1h_ns)?,
                period_ns: field("periodNs", 10_000, c.timing.period_ns)?,
                reset_us: field("resetUs", 10_000, c.timing.reset_us)?,
            };
        }
        if let Some(hz) = int_arg(arg(a, "clockHz"), "clockHz", 10_000, l.max_clock_hz as i64)? {
            c.clock_hz = hz as u32;
        }
        if let Some(b) = int_arg(arg(a, "brightness"), "brightness", 0, 255)? {
            c.brightness = b as u8;
        }
        if let Some(g) = bool_arg(a, "gamma")? {
            c.gamma = g;
        }
        if let Some(v) = bool_arg(a, "invert")? {
            c.invert = v;
        }
        if let Some(p) = a.and_then(|a| a.get("power")) {
            c.power = match p.kind() {
                Kind::Null => None,
                Kind::Object => {
                    let p = Some(&p);
                    let cur = c.power.unwrap_or(PowerBudget { ma_per_channel: 20, idle_ma: 1, max_ma: 2000 });
                    Some(PowerBudget {
                        ma_per_channel: int_arg(arg(p, "maPerChannel"), "power.maPerChannel", 1, 1000)?
                            .map(|v| v as u16)
                            .unwrap_or(cur.ma_per_channel),
                        idle_ma: int_arg(arg(p, "idleMa"), "power.idleMa", 0, 1000)?.map(|v| v as u16).unwrap_or(cur.idle_ma),
                        max_ma: int_arg(arg(p, "maxMa"), "power.maxMa", 1, 1_000_000)?.map(|v| v as u32).unwrap_or(cur.max_ma),
                    })
                }
                _ => return Err(err!(400, "power must be an object {{maPerChannel, idleMa, maxMa}} or null")),
            };
        }
        if let Some(v) = a.and_then(|a| a.get("network")) {
            c.net = match v.kind() {
                Kind::Null => None,
                Kind::Object => {
                    let v = Some(&v);
                    let cur = c.net;
                    let protocol = match text::<8>(arg(v, "protocol"), "network.protocol")? {
                        Some(p) => match p.as_str() {
                            "sacn" | "e131" => Protocol::Sacn,
                            "artnet" => Protocol::ArtNet,
                            "ddp" => Protocol::Ddp,
                            _ => return Err(err!(400, "network.protocol must be sacn, artnet or ddp")),
                        },
                        None => cur.map(|n| n.protocol).ok_or_else(|| err!(400, "network.protocol is required (sacn, artnet or ddp)"))?,
                    };
                    let (lo, hi, def) = match protocol {
                        Protocol::Sacn => (1, 63999, 1),
                        _ => (0, 32767, 0),
                    };
                    let same = cur.filter(|n| n.protocol == protocol);
                    NetIn {
                        protocol,
                        universe: int_arg(arg(v, "universe"), "network.universe", lo, hi)?
                            .map(|u| u as u16)
                            .or(same.map(|n| n.universe))
                            .unwrap_or(def),
                        start_channel: int_arg(arg(v, "startChannel"), "network.startChannel", 1, 512)?
                            .map(|u| u as u16)
                            .or(same.map(|n| n.start_channel))
                            .unwrap_or(1),
                        ddp_offset: int_arg(arg(v, "ddpOffset"), "network.ddpOffset", 0, u32::MAX as i64)?
                            .map(|u| u as u32)
                            .or(same.map(|n| n.ddp_offset))
                            .unwrap_or(0),
                    }
                    .into()
                }
                _ => return Err(err!(400, "network must be an object {{protocol, universe, startChannel, ddpOffset}} or null")),
            };
        }
        self.check(info, l, &c, existing)?;
        let config_gen = self.bump();
        let content_gen = self.bump();
        self.persist_gen = self.persist_gen.wrapping_add(1);
        let i = match existing {
            Some(i) => i,
            None => {
                let i = self.slots.iter().position(|s| s.is_none()).ok_or_else(|| err!(409, "No free LED slot"))?;
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1).max(1);
                if c.name.is_empty() {
                    c.name = default_name(id);
                }
                let pattern = Pattern::Rainbow { spread: 60, period_ms: 5000 };
                self.slots[i] = Some(Slot {
                    id,
                    cfg: c,
                    source: Source::Pattern { pattern, since_ms: now_ms },
                    config_gen,
                    content_gen,
                    hw: Hw::Pending,
                    estimate_ma: None,
                    net: NetRx::new(),
                    pixels: [Rgbw::OFF; MAX_PIXELS],
                });
                i
            }
        };
        let s = self.slots[i].as_mut().ok_or_else(|| err!(500, "slot vanished"))?;
        s.cfg = c;
        s.config_gen = config_gen;
        s.content_gen = content_gen;
        s.hw = Hw::Pending;
        Ok(s.id as i64)
    }

    fn check(&self, info: &BoardInfo, l: &LedsInfo, c: &StringConfig, me: Option<usize>) -> R {
        if c.chip.clocked() && !l.clocked || !c.chip.clocked() && !l.one_wire {
            return Err(err!(400, "This board can't drive {} strings", c.chip.name()));
        }
        let mut pins = [Some(c.pin), c.clock_pin];
        if !c.chip.clocked() {
            pins[1] = None;
        } else if c.clock_pin.is_none() {
            return Err(err!(400, "{} needs a clockPin as well as the data pin", c.chip.name()));
        }
        if pins[1] == Some(c.pin) {
            return Err(err!(400, "The data and clock pins must differ"));
        }
        for g in pins.into_iter().flatten() {
            let p = info.pins.iter().find(|p| p.gpio == g).ok_or_else(|| err!(404, "No GPIO {} on this board (see GET /api/gpio)", g))?;
            if let Some(f) = p.func {
                return Err(err!(409, "GPIO{}: used by {}", g, f));
            }
            for (k, s) in self.slots.iter().enumerate() {
                if let Some(s) = s
                    && Some(k) != me
                    && (s.cfg.pin == g || s.cfg.clock_pin == Some(g) && s.cfg.chip.clocked())
                {
                    return Err(err!(409, "GPIO{}: used by LED string {} ({})", g, s.id, s.cfg.name.as_str()));
                }
            }
        }
        if !c.chip.clocked() {
            c.timing.check().map_err(|e| err!(400, "timing: {}", e))?;
            (l.check_timing)(&c.timing).map_err(|e| err!(400, "timing: {}", e))?;
        }
        Ok(())
    }

    pub fn remove(&mut self, id: i64) -> R {
        let i = self.index(id)?;
        self.slots[i] = None;
        self.persist_gen = self.persist_gen.wrapping_add(1);
        Ok(())
    }

    pub fn set_pattern(&mut self, id: i64, a: Option<&Value<'_>>, now_ms: u64) -> R {
        let i = self.index(id)?;
        let g = self.bump();
        let s = self.slots[i].as_mut().ok_or_else(|| err!(404, "No LED string {}", id))?;
        let current = match s.source {
            Source::Pattern { pattern, .. } => Some(pattern),
            Source::Network => match s.net.prev {
                Some(Source::Pattern { pattern, .. }) => Some(pattern),
                _ => None,
            },
            Source::Pixels => None,
        };
        let mut p = match text::<16>(arg(a, "name"), "name")? {
            Some(n) => match current.filter(|p| p.name() == n.as_str()) {
                Some(p) => p,
                None => Pattern::default_for(n.as_str()).ok_or_else(|| {
                    let mut e = err!(400, "name must be one of:");
                    for (n, _) in pattern::NAMES {
                        let _ = write!(e, " {}", n);
                    }
                    e
                })?,
            },
            None => current.ok_or_else(|| err!(400, "name is required (the string is showing pixels, not a pattern)"))?,
        };
        apply_params(&mut p, a, s.cfg.pixels)?;
        self.persist_gen = self.persist_gen.wrapping_add(1);
        let pat = Source::Pattern { pattern: p, since_ms: now_ms };
        if matches!(s.source, Source::Network) {
            // The network has the string; this is what comes back when it lets go.
            s.net.prev = Some(pat);
            return Ok(());
        }
        s.source = pat;
        s.content_gen = g;
        Ok(())
    }

    pub fn set_pixels(&mut self, id: i64, a: Option<&Value<'_>>) -> R {
        let i = self.index(id)?;
        let g = self.bump();
        let s = self.slots[i].as_mut().ok_or_else(|| err!(404, "No LED string {}", id))?;
        let n = (s.cfg.pixels as usize).min(MAX_PIXELS);
        let start = int_arg(arg(a, "start"), "start", 0, n as i64 - 1)?.unwrap_or(0) as usize;
        if matches!(s.source, Source::Network) {
            return Err(err!(409, "LED string {} is showing network data ({}); set pixels once it stops", id, s.net.from.as_str()));
        }
        if matches!(s.source, Source::Pattern { .. }) {
            s.pixels = [Rgbw::OFF; MAX_PIXELS];
        }
        if let Some(f) = arg_value(a, "fill") {
            let color = color_value(&f, "fill")?;
            let count = int_arg(arg(a, "count"), "count", 0, n as i64)?.map(|c| c as usize).unwrap_or(n - start);
            for px in s.pixels[start..(start + count).min(n)].iter_mut() {
                *px = color;
            }
        } else {
            let v = arg_value(a, "pixels").ok_or_else(|| err!(400, "Give pixels (a hex string or an array of \"#rrggbb\") or fill"))?;
            let mut k = start;
            match v.kind() {
                Kind::Array => {
                    for item in v.items() {
                        if k >= n {
                            return Err(err!(400, "More pixels than the string has ({} from start {})", n, start));
                        }
                        s.pixels[k] = color_value(&item, "pixels[]")?;
                        k += 1;
                    }
                }
                Kind::String => {
                    let hex = v.as_plain_str().ok_or_else(|| err!(400, "pixels: plain hex digits only"))?;
                    let per = if s.cfg.order.has_white() { 8 } else { 6 };
                    let mut digits = [0u8; 8];
                    let mut d = 0;
                    for ch in hex.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b',' && *b != b'#') {
                        if !ch.is_ascii_hexdigit() {
                            return Err(err!(400, "pixels: not a hex digit: {}", ch as char));
                        }
                        digits[d] = ch;
                        d += 1;
                        if d == per {
                            if k >= n {
                                return Err(err!(400, "More pixels than the string has ({} from start {})", n, start));
                            }
                            let txt = core::str::from_utf8(&digits[..per]).unwrap_or("");
                            s.pixels[k] = Rgbw::parse_hex(txt).ok_or_else(|| err!(400, "pixels: bad colour {}", txt))?;
                            k += 1;
                            d = 0;
                        }
                    }
                    if d != 0 {
                        return Err(err!(400, "pixels: {} hex digits per pixel for this string", per));
                    }
                }
                _ => return Err(err!(400, "pixels must be a hex string or an array of colours")),
            }
        }
        s.source = Source::Pixels;
        s.content_gen = g;
        Ok(())
    }
}

/// "strip 3".
fn default_name(id: u16) -> FixedStr<24> {
    let mut b = *b"strip 00000";
    let mut n = id;
    let mut k = b.len();
    loop {
        k -= 1;
        b[k] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    b.copy_within(k.., 6);
    let len = 6 + (b.len() - k);
    FixedStr::from_bytes(&b[..len]).unwrap_or(FixedStr::new())
}

fn parse_chip(s: &str) -> Result<Chip, ApiErr> {
    Chip::parse(s).ok_or_else(|| {
        let mut e = err!(400, "chip must be one of:");
        for (n, _) in CHIPS {
            let _ = write!(e, " {}", n);
        }
        e
    })
}

/// A string argument of at most N bytes.
fn text<const N: usize>(a: Option<Arg<'_>>, what: &str) -> Result<Option<FixedStr<N>>, ApiErr> {
    let Some(a) = a else { return Ok(None) };
    let mut buf = [0u8; N];
    let s = match a {
        Arg::Json(v) if v.kind() == Kind::String => v.as_str_in(&mut buf),
        Arg::Text(s) => Some(s),
        _ => None,
    };
    let s = s.ok_or_else(|| err!(400, "{} must be a string of at most {} bytes", what, N))?;
    Ok(FixedStr::from_text(s))
}

fn bool_arg(a: Option<&Value<'_>>, key: &str) -> Result<Option<bool>, ApiErr> {
    match arg_value(a, key) {
        None => Ok(None),
        Some(v) => v.as_bool().map(Some).ok_or_else(|| err!(400, "{} must be true or false", key)),
    }
}

fn color_value(v: &Value<'_>, what: &str) -> Result<Rgbw, ApiErr> {
    let mut buf = [0u8; 12];
    v.as_str_in(&mut buf)
        .and_then(Rgbw::parse_hex)
        .ok_or_else(|| err!(400, "{} must be a colour like \"#ff8000\" or \"#ff800040\" (with white)", what))
}

fn color_arg(a: Option<&Value<'_>>, key: &str, cur: &mut Rgbw) -> R {
    if let Some(v) = arg_value(a, key) {
        *cur = color_value(&v, key)?;
    }
    Ok(())
}

fn apply_params(p: &mut Pattern, a: Option<&Value<'_>>, pixels: u16) -> R {
    let num = |k: &str, min: i64, max: i64| int_arg(arg(a, k), k, min, max);
    match p {
        Pattern::Off | Pattern::OrderTest | Pattern::Ruler => {}
        Pattern::Solid { color } => color_arg(a, "color", color)?,
        Pattern::Gradient { a: x, b: y } => {
            color_arg(a, "a", x)?;
            color_arg(a, "b", y)?;
        }
        Pattern::Rainbow { spread, period_ms } => {
            if let Some(v) = num("spread", 1, 10_000)? {
                *spread = v as u16;
            }
            if let Some(v) = num("periodMs", 0, 600_000)? {
                *period_ms = v as u32;
            }
        }
        Pattern::Chase { color, background, width, speed } => {
            color_arg(a, "color", color)?;
            color_arg(a, "background", background)?;
            if let Some(v) = num("width", 1, pixels.max(1) as i64)? {
                *width = v as u16;
            }
            if let Some(v) = num("speed", 0, 10_000)? {
                *speed = v as u16;
            }
        }
        Pattern::Breathe { color, period_ms } => {
            color_arg(a, "color", color)?;
            if let Some(v) = num("periodMs", 100, 600_000)? {
                *period_ms = v as u32;
            }
        }
        Pattern::Twinkle { color, density, period_ms } => {
            color_arg(a, "color", color)?;
            if let Some(v) = num("density", 0, 255)? {
                *density = v as u8;
            }
            if let Some(v) = num("periodMs", 100, 600_000)? {
                *period_ms = v as u32;
            }
        }
    }
    Ok(())
}

fn hex(j: &mut Json<'_, '_>, k: &str, c: Rgbw) {
    if c.w == 0 {
        j.key(k).str_fmt(format_args!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b));
    } else {
        j.key(k).str_fmt(format_args!("#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.w));
    }
}

fn pattern_view(p: &Pattern, j: &mut Json<'_, '_>) {
    j.obj().kv_str("name", p.name());
    match *p {
        Pattern::Off | Pattern::OrderTest | Pattern::Ruler => {}
        Pattern::Solid { color } => hex(j, "color", color),
        Pattern::Gradient { a, b } => {
            hex(j, "a", a);
            hex(j, "b", b);
        }
        Pattern::Rainbow { spread, period_ms } => {
            j.kv_u64("spread", spread as u64).kv_u64("periodMs", period_ms as u64);
        }
        Pattern::Chase { color, background, width, speed } => {
            hex(j, "color", color);
            hex(j, "background", background);
            j.kv_u64("width", width as u64).kv_u64("speed", speed as u64);
        }
        Pattern::Breathe { color, period_ms } => {
            hex(j, "color", color);
            j.kv_u64("periodMs", period_ms as u64);
        }
        Pattern::Twinkle { color, density, period_ms } => {
            hex(j, "color", color);
            j.kv_u64("density", density as u64).kv_u64("periodMs", period_ms as u64);
        }
    }
    j.end_obj();
}
