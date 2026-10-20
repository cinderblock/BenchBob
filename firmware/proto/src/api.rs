//! The node's REST API and MCP endpoint, mirroring BenchBob's shapes for the subset a
//! microcontroller has: `/api`, `/api/status`, GPIO and I²C, and `POST /mcp` with the
//! same tool names and arguments as the Linux drivers (`gpio_read`, `gpio_write`,
//! `i2c_buses`, `i2c_scan`, `i2c_read`).
//!
//! Hardware is behind the [`Node`] trait, so all of this runs in host tests against a
//! mock board.

// ApiErr carries its message inline (no heap), which makes Results large; they only live
// for the length of one request.
#![allow(clippy::result_large_err)]

use crate::json::{self, Buf, Json, Kind, Value};
use core::fmt::{self, Write};

pub const MCP_PROTOCOL: &str = "2025-06-18";
/// First and last address of an i2cdetect-style scan (its defaults).
pub const SCAN_FIRST: u8 = 0x08;
pub const SCAN_LAST: u8 = 0x77;
pub const MAX_BUSES: usize = 4;

// ---------------------------------------------------------------------------------------
// What a board provides

pub struct BoardInfo {
    /// Short id, used in mDNS TXT and release file names: `pico2w`, `esp32c6`.
    pub board: &'static str,
    pub board_name: &'static str,
    pub chip: &'static str,
    pub firmware_version: &'static str,
    pub pins: &'static [PinInfo],
    pub i2c: &'static [I2cBusInfo],
}

pub struct PinInfo {
    pub gpio: u8,
    /// Physical pin on the board's header, if it has a fixed one.
    pub header: Option<u8>,
    /// Set when the firmware uses the pin for something else (e.g. "I2C0 SDA"); such pins
    /// are listed but never read or driven.
    pub func: Option<&'static str>,
    /// Shown alongside (strapping pin, onboard LED…); doesn't block writes.
    pub note: Option<&'static str>,
}

pub struct I2cBusInfo {
    pub bus: u8,
    pub label: &'static str,
    pub sda: u8,
    pub scl: u8,
    pub hz: u32,
    /// The controller can do an SMBus quick write (address only). If not, scans use a
    /// one-byte read everywhere, as the Pico SDK's bus_scan example does.
    pub can_quick: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Input,
    Output,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pull {
    Up,
    Down,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinState {
    pub mode: Mode,
    pub level: bool,
    pub pull: Pull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I2cFault {
    /// No acknowledge: nobody at that address, or the device refused the byte.
    Nack,
    Timeout,
    /// Arbitration lost / bus error: usually a stuck or unpowered bus.
    Bus,
    Other,
}

impl I2cFault {
    fn text(self) -> &'static str {
        match self {
            I2cFault::Nack => "no acknowledge",
            I2cFault::Timeout => "timed out (is the bus stuck or missing pull-ups?)",
            I2cFault::Bus => "bus error or arbitration lost (check wiring and pull-ups)",
            I2cFault::Other => "I2C error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiState {
    Unprovisioned,
    Connecting,
    Connected,
}

pub struct Status<'a> {
    pub hostname: &'a str,
    pub uptime_ms: u64,
    pub mac: [u8; 6],
    pub wifi_state: WifiState,
    pub ssid: &'a str,
    pub rssi: Option<i16>,
    pub ip: Option<[u8; 4]>,
    /// (free, used) bytes, when the firmware has a heap.
    pub heap: Option<(u32, u32)>,
}

/// The hardware behind the API. Pins and buses passed in are always ones from
/// [`BoardInfo`] that the firmware doesn't reserve.
pub trait Node {
    fn info(&self) -> &'static BoardInfo;
    fn status(&self) -> Status<'_>;
    fn gpio_read(&mut self, gpio: u8) -> PinState;
    fn gpio_write(&mut self, gpio: u8, mode: Option<(Mode, bool)>, pull: Option<Pull>);
    /// Probe one address: a one-byte read (`read`) or a quick write. Ok(false) = no ACK.
    fn i2c_probe(&mut self, bus: u8, addr: u8, read: bool) -> Result<bool, I2cFault>;
    /// Write `wr` (may be empty), then read `rd` with a repeated start (if `rd` isn't empty).
    fn i2c_write_read(&mut self, bus: u8, addr: u8, wr: &[u8], rd: &mut [u8]) -> Result<(), I2cFault>;
}

// ---------------------------------------------------------------------------------------
// Requests, replies, errors

pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub host: Option<&'a str>,
    pub body: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    /// Add `MCP-Protocol-Version`.
    pub mcp: bool,
    /// `Allow` header (405s).
    pub allow: Option<&'static str>,
}

const JSON: &str = "application/json";

impl Reply {
    fn json(status: u16) -> Self {
        Reply { status, content_type: JSON, mcp: false, allow: None }
    }
}

/// An error with a status and a short formatted message.
pub struct ApiErr {
    pub status: u16,
    msg: [u8; 160],
    len: usize,
}

impl ApiErr {
    pub fn new(status: u16, args: fmt::Arguments<'_>) -> Self {
        let mut e = ApiErr { status, msg: [0; 160], len: 0 };
        let _ = fmt::write(&mut e, args);
        e
    }
    pub fn message(&self) -> &str {
        core::str::from_utf8(&self.msg[..self.len]).unwrap_or("error")
    }
}

impl Write for ApiErr {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            let l = c.len_utf8();
            if self.len + l > self.msg.len() {
                break;
            }
            c.encode_utf8(&mut self.msg[self.len..]);
            self.len += l;
        }
        Ok(())
    }
}

impl fmt::Debug for ApiErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.status, self.message())
    }
}

macro_rules! err {
    ($status:expr, $($t:tt)*) => { ApiErr::new($status, format_args!($($t)*)) };
}

type R = Result<(), ApiErr>;

fn json_error(out: &mut Buf<'_>, e: &ApiErr) {
    out.clear();
    Json::new(out).obj().kv_bool("ok", false).kv_str("message", e.message()).end_obj();
}

// ---------------------------------------------------------------------------------------
// Argument parsing (same leniency as the Linux drivers: numbers or strings like "0x76")

/// An argument as it arrives: a JSON value (MCP arguments, REST bodies) or a URL path
/// segment (`/api/i2c/0/0x76/...`).
#[derive(Clone, Copy)]
enum Arg<'a> {
    Json(Value<'a>),
    Text(&'a str),
}

impl Arg<'_> {
    fn raw(&self) -> &str {
        match self {
            Arg::Json(v) => v.raw(),
            Arg::Text(s) => s,
        }
    }

    /// With `prefix`, the text form may carry it ("i2c-0" for a bus).
    fn int(&self, prefix: &str) -> Option<i64> {
        let mut buf = [0u8; 24];
        let s = match self {
            Arg::Json(v) => {
                if let Some(n) = v.as_i64() {
                    return Some(n);
                }
                v.as_str_in(&mut buf)?
            }
            Arg::Text(s) => s,
        };
        let s = s.trim();
        let s = if prefix.is_empty() { s } else { s.strip_prefix(prefix).unwrap_or(s) };
        match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            Some(h) => i64::from_str_radix(h, 16).ok(),
            None => s.parse().ok(),
        }
    }
}

fn int_value(v: &Value<'_>) -> Option<i64> {
    Arg::Json(*v).int("")
}

fn arg<'a>(args: Option<&Value<'a>>, key: &str) -> Option<Arg<'a>> {
    args.and_then(|a| a.get(key)).filter(|v| !v.is_null()).map(Arg::Json)
}

fn arg_value<'a>(args: Option<&Value<'a>>, key: &str) -> Option<Value<'a>> {
    args.and_then(|a| a.get(key)).filter(|v| !v.is_null())
}

fn parse_bus(info: &'static BoardInfo, a: Option<Arg<'_>>) -> Result<&'static I2cBusInfo, ApiErr> {
    let Some(a) = a else { return Err(err!(400, "bus is required (see GET /api/i2c)")) };
    match a.int("i2c-") {
        Some(n) => info.i2c.iter().find(|b| b.bus as i64 == n).ok_or_else(|| err!(404, "No I2C bus {}. See GET /api/i2c", n)),
        None => Err(err!(400, "bus must be a number like 0 or \"i2c-0\" (got {})", a.raw())),
    }
}

fn parse_addr(a: Option<Arg<'_>>) -> Result<u8, ApiErr> {
    let Some(a) = a else { return Err(err!(400, "address is required")) };
    match a.int("") {
        Some(n @ 0x03..=0x77) => Ok(n as u8),
        _ => Err(err!(400, "Address must be 0x03-0x77 (got {})", a.raw())),
    }
}

fn int_arg(a: Option<Arg<'_>>, what: &str, min: i64, max: i64) -> Result<Option<i64>, ApiErr> {
    match a {
        None => Ok(None),
        Some(a) => match a.int("") {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            _ => Err(err!(400, "{} must be {}-{} (got {})", what, min, max, a.raw())),
        },
    }
}

/// A string argument matched case-insensitively against a fixed set of words.
fn word(a: Option<Arg<'_>>, words: &[&'static str], what: &str) -> Result<Option<&'static str>, ApiErr> {
    let Some(a) = a else { return Ok(None) };
    let mut buf = [0u8; 16];
    let s = match a {
        Arg::Json(v) => v.as_str_in(&mut buf),
        Arg::Text(s) => Some(s),
    };
    match s.and_then(|s| words.iter().find(|w| w.eq_ignore_ascii_case(s))) {
        Some(w) => Ok(Some(w)),
        None => {
            let mut e = err!(400, "{} must be one of:", what);
            for w in words {
                let _ = write!(e, " {}", w);
            }
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------------------
// The API

#[derive(Clone, Copy, Default)]
struct ScanResult {
    /// Bit n set: address n answered.
    found: u128,
    at_ms: Option<u64>,
    ms: u32,
}

/// Request handler state: output control and the last scan of each bus. It lives in RAM,
/// so output control is off again after every reset.
pub struct Api {
    output_enabled: bool,
    scans: [ScanResult; MAX_BUSES],
}

impl Default for Api {
    fn default() -> Self {
        Self::new()
    }
}

impl Api {
    pub const fn new() -> Self {
        Api { output_enabled: false, scans: [ScanResult { found: 0, at_ms: None, ms: 0 }; MAX_BUSES] }
    }

    pub fn output_enabled(&self) -> bool {
        self.output_enabled
    }

    /// Handle one request. The response body goes into `out`; `scratch` is working space
    /// for MCP (a tool result is written there first, then embedded twice).
    pub fn handle<N: Node>(&mut self, node: &mut N, req: &Request<'_>, out: &mut Buf<'_>, scratch: &mut [u8]) -> Reply {
        out.clear();
        let reply = self.route(node, req, out, scratch);
        let reply = match reply {
            Ok(r) => r,
            Err(e) => {
                json_error(out, &e);
                Reply::json(e.status)
            }
        };
        if out.overflowed() {
            let e = err!(500, "response too large for the node's buffer");
            json_error(out, &e);
            return Reply::json(500);
        }
        reply
    }

    fn route<N: Node>(&mut self, node: &mut N, req: &Request<'_>, out: &mut Buf<'_>, scratch: &mut [u8]) -> Result<Reply, ApiErr> {
        let path = req.path.trim_end_matches('/');
        let mut seg = path.trim_start_matches('/').split('/');
        let parts: [&str; 6] = core::array::from_fn(|_| seg.next().unwrap_or(""));
        if seg.next().is_some() {
            return Err(err!(404, "Not found: {}", req.path));
        }
        let m = req.method;
        let info = node.info();
        match (m, parts) {
            (_, ["mcp", "", ..]) => return Ok(self.mcp(node, req, out, scratch)),
            ("GET", ["" | "api", "", ..]) => self.docs(node, req, out),
            ("GET", ["api", "status", "", ..]) => status_view(node, out),
            ("GET", ["api", "gpio", "", ..]) => self.gpio_state(node, None, out),
            ("POST", ["api", "gpio", "output-control", "", ..]) => {
                let b = parse_body(req.body)?;
                let enabled = arg_value(b.as_ref(), "enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                let confirm = arg_value(b.as_ref(), "confirm").and_then(|v| v.as_bool()) == Some(true);
                if enabled && !confirm {
                    return Err(err!(400, "Enabling output control lets this API drive pins; repeat with \"confirm\": true"));
                }
                self.output_enabled = enabled;
                Json::new(out).obj().kv_bool("outputEnabled", enabled).end_obj();
                Ok(())
            }
            ("POST", ["api", "gpio", g, "", ..]) => {
                let digits = g.trim_start_matches(|c: char| c.is_ascii_alphabetic());
                let n = digits.parse::<i64>().map_err(|_| err!(400, "GPIO number required, e.g. /api/gpio/5"))?;
                let b = parse_body(req.body)?;
                self.gpio_write(node, n, b.as_ref(), out)
            }
            ("GET", ["api", "i2c", "", ..]) => {
                let mut j = Json::new(out);
                j.obj().key("buses").arr();
                for b in info.i2c {
                    self.bus_view(b, &mut j);
                }
                j.end_arr().end_obj();
                Ok(())
            }
            ("POST", ["api", "i2c", bus, "scan", "", ..]) => {
                let b = parse_bus(info, Some(Arg::Text(bus)))?;
                self.scan(node, b)?;
                let mut j = Json::new(out);
                self.bus_view(b, &mut j);
                Ok(())
            }
            ("POST", ["api", "i2c", bus, addr, "raw", "read"]) => {
                let b = parse_bus(info, Some(Arg::Text(bus)))?;
                let a = parse_addr(Some(Arg::Text(addr)))?;
                let args = parse_body(req.body)?;
                i2c_read(node, b, a, args.as_ref(), out)
            }
            _ if route_exists(&parts) => {
                json_error(out, &err!(405, "Method {} not allowed on {}", m, req.path));
                return Ok(Reply { allow: Some("GET, POST"), ..Reply::json(405) });
            }
            _ => Err(err!(404, "Not found: {} {}", m, req.path)),
        }?;
        Ok(Reply::json(200))
    }

    fn docs<N: Node>(&self, node: &N, req: &Request<'_>, out: &mut Buf<'_>) -> R {
        let info = node.info();
        let st = node.status();
        let mut j = Json::new(out);
        j.obj().kv_str("name", "BenchBob node API").kv_str("version", "1.0").kv_str("appVersion", info.firmware_version);
        j.kv_str("board", info.board).kv_str("boardName", info.board_name).kv_str("chip", info.chip);
        match req.host {
            Some(h) => j.key("base").str_fmt(format_args!("http://{}", h)),
            None => j.key("base").str_fmt(format_args!("http://{}.local", st.hostname)),
        };
        j.key("endpoints").obj();
        for (k, v) in ENDPOINTS {
            j.key(k).raw(v);
        }
        j.end_obj();
        j.key("mcpTools").arr();
        for t in TOOLS {
            j.str(t.name);
        }
        j.end_arr();
        j.end_obj();
        Ok(())
    }

    fn gpio_state<N: Node>(&self, node: &mut N, filter: Option<&Value<'_>>, out: &mut Buf<'_>) -> R {
        let info = node.info();
        // Validate the filter before writing anything.
        if let Some(f) = filter {
            for g in f.items() {
                let n = int_value(&g).ok_or_else(|| err!(400, "gpios must be integers"))?;
                if !info.pins.iter().any(|p| p.gpio as i64 == n) {
                    return Err(err!(404, "No GPIO {} on this board (see GET /api/gpio)", n));
                }
            }
        }
        let mut j = Json::new(out);
        j.obj().kv_bool("outputEnabled", self.output_enabled).kv_str("board", info.board);
        j.key("pins").arr();
        for p in info.pins {
            let excluded = filter.is_some_and(|f| f.items().count() > 0 && !f.items().any(|g| int_value(&g) == Some(p.gpio as i64)));
            if !excluded {
                pin_view(node, p, &mut j);
            }
        }
        j.end_arr().end_obj();
        Ok(())
    }

    fn gpio_write<N: Node>(&mut self, node: &mut N, gpio: i64, args: Option<&Value<'_>>, out: &mut Buf<'_>) -> R {
        let info = node.info();
        let pin = info.pins.iter().find(|p| p.gpio as i64 == gpio).ok_or_else(|| err!(404, "No GPIO {} on this board (see GET /api/gpio)", gpio))?;
        if !self.output_enabled {
            return Err(err!(403, "GPIO output control is off. Enable it first (POST /api/gpio/output-control with enabled: true, confirm: true)."));
        }
        if let Some(f) = pin.func {
            return Err(err!(409, "GPIO{}: used by {}", gpio, f));
        }
        let mode = word(arg(args, "mode"), &["high", "low", "input"], "mode")?.map(|w| match w {
            "high" => (Mode::Output, true),
            "low" => (Mode::Output, false),
            _ => (Mode::Input, false),
        });
        let pull = word(arg(args, "pull"), &["up", "down", "none"], "pull")?.map(|w| match w {
            "up" => Pull::Up,
            "down" => Pull::Down,
            _ => Pull::None,
        });
        if mode.is_none() && pull.is_none() {
            return Err(err!(400, "Nothing to set: give mode and/or pull"));
        }
        node.gpio_write(pin.gpio, mode, pull);
        let mut j = Json::new(out);
        pin_view(node, pin, &mut j);
        Ok(())
    }

    fn bus_view(&self, b: &I2cBusInfo, j: &mut Json<'_, '_>) {
        let s = self.scans.get(b.bus as usize).copied().unwrap_or_default();
        j.obj().kv_u64("bus", b.bus as u64).key("dev").str_fmt(format_args!("i2c-{}", b.bus)).kv_str("label", b.label);
        j.kv_u64("sda", b.sda as u64).kv_u64("scl", b.scl as u64).kv_u64("hz", b.hz as u64).kv_bool("quickWrite", b.can_quick);
        match s.at_ms {
            Some(t) => j.kv_u64("lastScanUptimeMs", t),
            None => j.key("lastScanUptimeMs").null(),
        };
        j.kv_u64("scanMs", s.ms as u64);
        j.key("devices").arr();
        for a in SCAN_FIRST..=SCAN_LAST {
            if s.found & (1u128 << a) != 0 {
                j.obj().kv_u64("addr", a as u64).key("hex").str_fmt(format_args!("0x{:02x}", a)).key("address").str_fmt(format_args!("0x{:02x}", a)).key("chip").null().end_obj();
            }
        }
        j.end_arr().end_obj();
    }

    /// i2cdetect's auto mode: read-byte at 0x30–0x37 and 0x50–0x5F (where a quick write
    /// can corrupt some EEPROMs), quick write elsewhere; read-byte everywhere when the
    /// controller can't do a quick write. A timeout or bus error stops the scan.
    fn scan<N: Node>(&mut self, node: &mut N, b: &I2cBusInfo) -> R {
        let t0 = node.status().uptime_ms;
        let mut found = 0u128;
        for a in SCAN_FIRST..=SCAN_LAST {
            let read_first = (0x30..=0x37).contains(&a) || (0x50..=0x5f).contains(&a);
            match node.i2c_probe(b.bus, a, read_first || !b.can_quick) {
                Ok(true) => found |= 1u128 << a,
                Ok(false) | Err(I2cFault::Nack) | Err(I2cFault::Other) => {}
                Err(e) => return Err(err!(502, "i2c-{} scan stopped at 0x{:02x}: {}", b.bus, a, e.text())),
            }
        }
        let t1 = node.status().uptime_ms;
        if let Some(s) = self.scans.get_mut(b.bus as usize) {
            *s = ScanResult { found, at_ms: Some(t1), ms: t1.saturating_sub(t0) as u32 };
        }
        Ok(())
    }

    fn scan_summary(&self, b: &I2cBusInfo, out: &mut Buf<'_>) {
        let s = self.scans.get(b.bus as usize).copied().unwrap_or_default();
        let mut j = Json::new(out);
        j.obj().kv_u64("bus", b.bus as u64).key("devices").arr();
        for a in SCAN_FIRST..=SCAN_LAST {
            if s.found & (1u128 << a) != 0 {
                j.obj().key("address").str_fmt(format_args!("0x{:02x}", a)).key("claimedBy").null().key("chip").null().end_obj();
            }
        }
        j.end_arr().kv_u64("scanMs", s.ms as u64).end_obj();
    }

    // -----------------------------------------------------------------------------------
    // MCP

    fn mcp<N: Node>(&mut self, node: &mut N, req: &Request<'_>, out: &mut Buf<'_>, scratch: &mut [u8]) -> Reply {
        let mut r = Reply { status: 200, content_type: JSON, mcp: true, allow: None };
        match req.method {
            "POST" => {}
            "GET" => {
                out.push(b"This MCP server is stateless; use POST");
                return Reply { status: 405, content_type: "text/plain", mcp: false, allow: Some("POST") };
            }
            _ => return Reply { status: 405, content_type: "text/plain", mcp: false, allow: Some("POST") },
        }
        let parsed = core::str::from_utf8(req.body).ok().and_then(|s| json::parse(s).ok());
        let Some(msg) = parsed else {
            out.push(br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}"#);
            r.status = 400;
            return r;
        };
        if msg.kind() == Kind::Array {
            out.push(b"[");
            let mut any = false;
            for m in msg.items() {
                let before = out.len();
                if any {
                    out.push(b",");
                }
                if self.mcp_one(node, &m, out, scratch) {
                    any = true;
                } else {
                    out.truncate(before);
                }
            }
            if !any {
                out.clear();
                r.status = 202;
                return r;
            }
            out.push(b"]");
        } else if !self.mcp_one(node, &msg, out, scratch) {
            r.status = 202;
        }
        r
    }

    /// Answer one JSON-RPC message into `out`; false for a notification (no reply).
    fn mcp_one<N: Node>(&mut self, node: &mut N, msg: &Value<'_>, out: &mut Buf<'_>, scratch: &mut [u8]) -> bool {
        let Some(id) = msg.get("id") else { return false }; // notification
        let method = msg.get("method");
        let params = msg.get("params");
        let mut j = Json::new(out);
        j.obj().kv_str("jsonrpc", "2.0").key("id").raw(id.raw());
        let m = |s: &str| method.map(|v| v.str_eq(s)).unwrap_or(false);
        if m("initialize") {
            let info = node.info();
            j.key("result").obj();
            match params.and_then(|p| p.get("protocolVersion")).filter(|v| v.kind() == Kind::String) {
                Some(v) => j.key("protocolVersion").raw(v.raw()),
                None => j.kv_str("protocolVersion", MCP_PROTOCOL),
            };
            j.key("capabilities").obj().key("tools").obj().kv_bool("listChanged", false).end_obj().end_obj();
            j.key("serverInfo").obj().kv_str("name", "benchbob-node").kv_str("version", info.firmware_version).end_obj();
            j.key("instructions").str_fmt(format_args!(
                "This is a BenchBob node: a {} ({}) on the network. These tools reach its GPIO pins and I2C buses. GPIO writes only work after the user enables output control (POST /api/gpio/output-control with confirm: true). Call i2c_buses or gpio_read first.",
                info.board_name, info.chip
            ));
            j.end_obj();
        } else if m("ping") {
            j.key("result").obj().end_obj();
        } else if m("tools/list") {
            j.key("result").obj().key("tools").arr();
            for t in TOOLS {
                j.obj().kv_str("name", t.name).kv_str("description", t.description).key("inputSchema").raw(t.schema).end_obj();
            }
            j.end_arr().end_obj();
        } else if m("tools/call") {
            let name = params.and_then(|p| p.get("name"));
            let args = params.and_then(|p| p.get("arguments"));
            let tool = TOOLS.iter().find(|t| name.map(|n| n.str_eq(t.name)).unwrap_or(false));
            match tool {
                None => {
                    let name = name.map(|n| n.raw()).unwrap_or("null");
                    j.key("error").obj().key("code").i64(-32602).key("message").str_fmt(format_args!("Unknown tool: {}", name)).end_obj();
                }
                Some(t) => {
                    let mut sb = Buf::new(scratch);
                    let res = self.call_tool(node, t.name, args.as_ref(), &mut sb);
                    j.key("result").obj().key("content").arr().obj().kv_str("type", "text");
                    match res {
                        Ok(()) if !sb.overflowed() => {
                            j.kv_str("text", sb.as_str()).end_obj().end_arr();
                            if sb.as_bytes().first() == Some(&b'{') {
                                j.key("structuredContent").raw(sb.as_str());
                            }
                        }
                        Ok(()) => {
                            j.kv_str("text", "result too large for the node's buffer").end_obj().end_arr().kv_bool("isError", true);
                        }
                        Err(e) => {
                            j.kv_str("text", e.message()).end_obj().end_arr().kv_bool("isError", true);
                        }
                    }
                    j.end_obj();
                }
            }
        } else {
            let method = method.map(|v| v.raw()).unwrap_or("null");
            j.key("error").obj().key("code").i64(-32601).key("message").str_fmt(format_args!("Method not found: {}", method)).end_obj();
        }
        j.end_obj();
        true
    }

    fn call_tool<N: Node>(&mut self, node: &mut N, name: &str, args: Option<&Value<'_>>, out: &mut Buf<'_>) -> R {
        match name {
            "gpio_read" => {
                let f = arg_value(args, "gpios").filter(|v| v.kind() == Kind::Array);
                self.gpio_state(node, f.as_ref(), out)
            }
            "gpio_write" => {
                let g = int_arg(arg(args, "gpio"), "gpio", 0, 255)?.ok_or_else(|| err!(400, "gpio is required"))?;
                self.gpio_write(node, g, args, out)
            }
            "i2c_buses" => {
                let mut j = Json::new(out);
                j.arr();
                for b in node.info().i2c {
                    let s = self.scans.get(b.bus as usize).copied().unwrap_or_default();
                    j.obj().kv_u64("bus", b.bus as u64).kv_str("label", b.label).kv_str("adapter", node.info().chip);
                    j.key("devices").u64(s.found.count_ones() as u64);
                    match s.at_ms {
                        Some(t) => j.kv_u64("lastScanUptimeMs", t),
                        None => j.key("lastScanUptimeMs").null(),
                    };
                    j.end_obj();
                }
                j.end_arr();
                Ok(())
            }
            "i2c_scan" => {
                let b = parse_bus(node.info(), arg(args, "bus"))?;
                self.scan(node, b)?;
                self.scan_summary(b, out);
                Ok(())
            }
            "i2c_read" => {
                let b = parse_bus(node.info(), arg(args, "bus"))?;
                let a = parse_addr(arg(args, "address"))?;
                i2c_read(node, b, a, args, out)
            }
            _ => Err(err!(400, "Unknown tool: {}", name)),
        }
    }
}

/// A path the API serves with some method (for 405 instead of 404).
fn route_exists(parts: &[&str; 6]) -> bool {
    matches!(
        parts,
        ["" | "api", "", ..]
            | ["api", "status", "", ..]
            | ["api", "gpio", "", ..]
            | ["api", "gpio", _, "", ..]
            | ["api", "i2c", "", ..]
            | ["api", "i2c", _, "scan", "", ..]
            | ["api", "i2c", _, _, "raw", "read"]
    )
}

fn parse_body(body: &[u8]) -> Result<Option<Value<'_>>, ApiErr> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(None);
    }
    let s = core::str::from_utf8(body).map_err(|_| err!(400, "Body must be UTF-8 JSON"))?;
    let v = json::parse(s).map_err(|_| err!(400, "Body is not valid JSON"))?;
    Ok(Some(v))
}

fn status_view<N: Node>(node: &N, out: &mut Buf<'_>) -> R {
    let info = node.info();
    let s = node.status();
    let mut j = Json::new(out);
    j.obj().kv_str("hostname", s.hostname).key("uptime").fixed(s.uptime_ms, 3);
    j.kv_str("version", info.firmware_version).kv_str("board", info.board).kv_str("boardName", info.board_name).kv_str("chip", info.chip);
    let m = s.mac;
    j.key("network").obj();
    j.key("mac").str_fmt(format_args!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]));
    j.kv_str(
        "wifi",
        match s.wifi_state {
            WifiState::Unprovisioned => "unprovisioned",
            WifiState::Connecting => "connecting",
            WifiState::Connected => "connected",
        },
    );
    if s.ssid.is_empty() {
        j.key("ssid").null();
    } else {
        j.kv_str("ssid", s.ssid);
    }
    match s.rssi {
        Some(r) => j.key("rssi").i64(r as i64),
        None => j.key("rssi").null(),
    };
    match s.ip {
        Some(ip) => j.key("ip").str_fmt(format_args!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])),
        None => j.key("ip").null(),
    };
    j.end_obj();
    match s.heap {
        Some((free, used)) => j.key("heap").obj().kv_u64("free", free as u64).kv_u64("used", used as u64).end_obj(),
        None => j.key("heap").null(),
    };
    j.end_obj();
    Ok(())
}

fn pin_view<N: Node>(node: &mut N, p: &PinInfo, j: &mut Json<'_, '_>) {
    j.obj().kv_u64("gpio", p.gpio as u64);
    match p.header {
        Some(h) => j.kv_u64("header", h as u64),
        None => j.key("header").null(),
    };
    j.key("name").str_fmt(format_args!("GPIO{}", p.gpio));
    match p.func {
        Some(f) => {
            j.kv_str("func", f).key("level").null().key("pull").null();
            j.key("refusal").str_fmt(format_args!("used by {}", f));
        }
        None => {
            let s = node.gpio_read(p.gpio);
            j.kv_str("func", if s.mode == Mode::Output { "output" } else { "input" });
            j.kv_u64("level", s.level as u64);
            j.kv_str(
                "pull",
                match s.pull {
                    Pull::Up => "up",
                    Pull::Down => "down",
                    Pull::None => "none",
                },
            );
            j.key("refusal").null();
        }
    }
    j.key("note").opt_str(p.note);
    j.end_obj();
}

/// Raw read, shaped like the Linux `i2c_read`: size byte/word/block need a register,
/// raw (the default without one) reads `length` bytes with no register write.
fn i2c_read<N: Node>(node: &mut N, b: &I2cBusInfo, addr: u8, args: Option<&Value<'_>>, out: &mut Buf<'_>) -> R {
    let reg = int_arg(arg(args, "register"), "register", 0, 255)?.map(|r| r as u8);
    let default = if reg.is_some() { "byte" } else { "raw" };
    let size = word(arg(args, "size"), &["byte", "word", "block", "raw"], "size")?.unwrap_or(default);
    let max = if size == "raw" { 256 } else { 32 };
    let len = int_arg(arg(args, "length"), "length", 1, max)?.unwrap_or(1) as usize;
    let mut data = [0u8; 256];
    let fail = |e: I2cFault| err!(502, "i2c-{} 0x{:02x}: {}", b.bus, addr, e.text());
    let n = match size {
        "raw" => {
            node.i2c_write_read(b.bus, addr, &[], &mut data[..len]).map_err(fail)?;
            len
        }
        _ => {
            let r = reg.ok_or_else(|| err!(400, "size \"{}\" needs a register", size))?;
            let n = match size {
                "byte" => 1,
                "word" => 2,
                _ => len,
            };
            node.i2c_write_read(b.bus, addr, &[r], &mut data[..n]).map_err(fail)?;
            n
        }
    };
    let mut j = Json::new(out);
    j.obj();
    if let Some(r) = reg.filter(|_| size != "raw") {
        j.kv_u64("register", r as u64);
    }
    if size == "word" {
        let w = data[0] as u64 | (data[1] as u64) << 8;
        j.kv_u64("word", w).kv_u64("wordBigEndian", (data[0] as u64) << 8 | data[1] as u64);
    }
    j.key("bytes").arr();
    for x in &data[..n] {
        j.u64(*x as u64);
    }
    j.end_arr().end_obj();
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Static docs and tool definitions

/// Endpoint docs for GET /api (values are JSON).
const ENDPOINTS: &[(&str, &str)] = &[
    ("GET /api", r#""This documentation""#),
    ("GET /api/status", r#""Node status: hostname, uptime (s), firmware version, board, Wi-Fi (state, SSID, RSSI dBm, IP, MAC), heap""#),
    ("GET /api/gpio", r#""Every GPIO the board exposes: function (input/output, or what the firmware uses it for), level, pull, header pin, and why it can't be driven""#),
    ("POST /api/gpio/output-control", r#"{"description":"Turn GPIO output control on/off (off after every reset). Enabling requires confirm: true","body":{"enabled":true,"confirm":true}}"#),
    ("POST /api/gpio/:gpio", r#"{"description":"Drive a GPIO (needs output control on): mode high|low|input, pull up|down|none. Refused for pins the firmware uses (I2C, Wi-Fi, USB)","body":{"mode":"high","pull":"none"}}"#),
    ("GET /api/i2c", r#""I2C buses with their pins and clock, and the devices found by the last scan""#),
    ("POST /api/i2c/:bus/scan", r#""Scan a bus like i2cdetect (addresses 0x08-0x77; read-byte at 0x30-0x37 and 0x50-0x5F, quick write elsewhere when the controller supports it)""#),
    ("POST /api/i2c/:bus/:addr/raw/read", r#"{"description":"Raw read. size: byte|word|block (need register) or raw (no register: a pure read of length bytes). A register read writes the register pointer first","body":{"register":"0xD0","size":"byte","length":1}}"#),
    ("POST /mcp", r#""Model Context Protocol endpoint (Streamable HTTP, stateless JSON): gpio_read, gpio_write, i2c_buses, i2c_scan, i2c_read""#),
];

struct Tool {
    name: &'static str,
    description: &'static str,
    schema: &'static str,
}

// concat! only takes literals, so the shared schema fragments are macros.
macro_rules! bus_prop {
    () => {
        r#""bus":{"type":["integer","string"],"description":"Bus number, e.g. 0 or \"i2c-0\" (see i2c_buses)"}"#
    };
}
macro_rules! addr_prop {
    () => {
        r#""address":{"type":["integer","string"],"description":"Device address, 7-bit, e.g. 118 or \"0x76\""}"#
    };
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "gpio_read",
        description: "Read the node's GPIO state: for each pin the function (input/output, or what the firmware uses it for, like I2C0 SDA), level, pull and header pin. Optionally limit to a list of GPIO numbers.",
        schema: r#"{"type":"object","properties":{"gpios":{"type":"array","items":{"type":"integer","minimum":0},"description":"GPIO numbers; default all"}}}"#,
    },
    Tool {
        name: "gpio_write",
        description: "Drive a GPIO: mode high/low (output) or input, and/or pull up/down/none. Only works when the user has enabled GPIO output control (POST /api/gpio/output-control with confirm: true, off after every reset); refused for pins the firmware uses.",
        schema: r#"{"type":"object","properties":{"gpio":{"type":"integer","minimum":0},"mode":{"enum":["high","low","input"]},"pull":{"enum":["up","down","none"]}},"required":["gpio"]}"#,
    },
    Tool {
        name: "i2c_buses",
        description: "List the node's I2C buses (number, label, devices found by the last scan).",
        schema: r#"{"type":"object","properties":{}}"#,
    },
    Tool {
        name: "i2c_scan",
        description: "Scan an I2C bus like `i2cdetect -y` (addresses 0x08-0x77; read-byte at 0x30-0x37/0x50-0x5F, quick write elsewhere when supported) and return the addresses that answer. The node doesn't identify chips.",
        schema: concat!(r#"{"type":"object","properties":{"#, bus_prop!(), r#","deep":{"type":"boolean","description":"Accepted for compatibility; ignored"}},"required":["bus"]}"#),
    },
    Tool {
        name: "i2c_read",
        description: "Raw I2C read. With a register: size byte/word/block (word returns both byte orders). Without: a pure read of `length` bytes. Register reads write the register pointer first, which command-style chips (PCF8574, TCA9548A, HTU21D) treat as a command.",
        schema: concat!(
            r#"{"type":"object","properties":{"#,
            bus_prop!(),
            ",",
            addr_prop!(),
            r#","register":{"type":["integer","string"]},"size":{"enum":["byte","word","block","raw"]},"length":{"type":"integer","minimum":1,"maximum":256},"force":{"type":"boolean","description":"Accepted for compatibility; nodes have no kernel drivers"}},"required":["bus","address"]}"#
        ),
    },
];
