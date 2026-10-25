//! DMX512 outputs: one universe out of a serial port, fed by an sACN or Art-Net universe
//! (the sACN → DMX bridge) or by levels set through the API.
//!
//! - **raw**: a plain RS-485 adapter (FTDI-based "Open DMX" dongles, CH340/CP210x + a
//!   MAX485, a Pi's UART + transceiver). BenchBob times the frame itself: 250 kbaud 8N2,
//!   a break of ≥ 100 µs and a mark-after-break, then the start code and 512 slots, at up to
//!   44 frames a second (a full frame takes 22.7 ms on the wire).
//! - **enttec-pro**: an Enttec DMX USB Pro (or a compatible widget): its framed "send DMX"
//!   message (label 6); the widget does the timing and repeats the last frame itself.
//!
//! A network-fed output holds the last look when packets stop (fixtures don't black out
//! on a network blip); sACN is arbitrated like the strings' (priority, sequence). Frames
//! only go out while GPIO output control is on, like every other output, so a fog machine
//! doesn't start by itself after a reboot. The port is opened exclusively (TIOCEXCL), so
//! the serial panel sees it as busy rather than fighting over it.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use benchbob_core::http::HttpError;
use benchbob_pixels::net::{self as pnet, ARTNET_PORT, SACN_PORT, SacnArbiter};
use benchbob_serial::termios::{self as T, Flow, Parity, SerialConfig};
use serde_json::{Value, json};

use crate::Inner;

pub const MAX_OUTPUTS: usize = 8;
pub const SLOTS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Raw,
    EnttecPro,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Raw => "raw",
            Mode::EnttecPro => "enttec-pro",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Sacn,
    ArtNet,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutConfig {
    pub name: String,
    pub port: String,
    pub mode: Mode,
    pub refresh_hz: u32,
    pub net: Option<(Proto, u16)>,
}

#[derive(Clone)]
pub struct Out {
    pub id: u16,
    pub cfg: OutConfig,
    /// Bumped on every config change: the sender restarts.
    pub version: u32,
    manual: [u8; SLOTS],
    net: [u8; SLOTS],
    /// Following the network (data arrived since the last manual set).
    from_net: bool,
    arb: SacnArbiter,
    last_rx: Option<(Instant, String)>,
    packets: u32,
    pub state: Result<bool, String>,
}

impl Out {
    /// The 512 levels going out now.
    pub fn levels(&self) -> &[u8; SLOTS] {
        if self.from_net { &self.net } else { &self.manual }
    }
}

#[derive(Default)]
pub struct Dmx {
    pub outs: Vec<Out>,
    next_id: u16,
    version: u32,
}

fn bad(m: impl Into<String>) -> HttpError {
    HttpError::bad_request(m)
}

/// The serial ports a DMX output can use: stable by-id names first, then the rest.
pub fn ports() -> Vec<Value> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/dev/serial/by-id") {
        let mut names: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        names.sort();
        for p in names {
            let target = std::fs::canonicalize(&p).ok().map(|t| t.display().to_string());
            v.push(json!({ "path": p.display().to_string(), "device": target, "name": p.file_name().map(|n| n.to_string_lossy().to_string()) }));
        }
    }
    if let Ok(rd) = std::fs::read_dir("/dev") {
        let mut names: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("ttyUSB") || n.starts_with("ttyACM") || n.starts_with("ttyAMA") || n == "serial0" || n == "serial1")
            .collect();
        names.sort();
        for n in names {
            v.push(json!({ "path": format!("/dev/{n}"), "device": null, "name": n }));
        }
    }
    v
}

impl Dmx {
    fn bump(&mut self) -> u32 {
        self.version = self.version.wrapping_add(1);
        self.version
    }

    fn index(&self, id: i64) -> Result<usize, HttpError> {
        self.outs
            .iter()
            .position(|o| o.id as i64 == id)
            .ok_or_else(|| HttpError::new(404, format!("No DMX output {id} (see GET /api/dmx)")))
    }

    /// Create (`id` None) or change an output. Returns its id.
    pub fn configure(&mut self, id: Option<i64>, a: &Value) -> Result<u16, HttpError> {
        let existing = id.map(|id| self.index(id)).transpose()?;
        let cur = existing.map(|i| self.outs[i].cfg.clone());
        if cur.is_none() && self.outs.len() >= MAX_OUTPUTS {
            return Err(HttpError::new(409, format!("At most {MAX_OUTPUTS} DMX outputs")));
        }
        let s = |k: &str| a.get(k).and_then(Value::as_str);
        let port = match s("port") {
            Some(p) if p.starts_with("/dev/") && !p.contains("..") => p.to_string(),
            Some(p) => return Err(bad(format!("port must be a serial device path under /dev (got {p})"))),
            None => {
                cur.as_ref().map(|c| c.port.clone()).ok_or_else(|| bad("port is required (a serial device, see ports in GET /api/dmx)"))?
            }
        };
        if let Some(o) = self.outs.iter().find(|o| o.cfg.port == port && Some(o.id as i64) != id) {
            return Err(HttpError::new(409, format!("{port} is already DMX output {} ({})", o.id, o.cfg.name)));
        }
        let mode = match s("mode") {
            Some("raw") => Mode::Raw,
            Some("enttec-pro") => Mode::EnttecPro,
            Some(m) => return Err(bad(format!("mode must be raw (a plain RS-485 adapter) or enttec-pro (got {m})"))),
            None => cur.as_ref().map_or(Mode::Raw, |c| c.mode),
        };
        let refresh_hz = match a.get("refreshHz") {
            Some(v) if !v.is_null() => match v.as_u64() {
                Some(n @ 1..=44) => n as u32,
                _ => return Err(bad("refreshHz must be 1–44 (a full DMX frame takes 22.7 ms)")),
            },
            _ => cur.as_ref().map_or(40, |c| c.refresh_hz),
        };
        let net = match a.get("network") {
            None => cur.as_ref().and_then(|c| c.net),
            Some(Value::Null) => None,
            Some(n) => {
                let proto = match n.get("protocol").and_then(Value::as_str) {
                    Some("sacn" | "e131") => Proto::Sacn,
                    Some("artnet") => Proto::ArtNet,
                    _ => return Err(bad("network.protocol must be sacn or artnet")),
                };
                let (lo, hi, def) = if proto == Proto::Sacn { (1, 63999, 1) } else { (0, 32767, 0) };
                let u = match n.get("universe") {
                    None | Some(Value::Null) => def,
                    Some(v) => match v.as_u64() {
                        Some(u) if (lo..=hi).contains(&u) => u as u16,
                        _ => return Err(bad(format!("network.universe must be {lo}–{hi}"))),
                    },
                };
                Some((proto, u))
            }
        };
        let version = self.bump();
        let id = match existing {
            Some(i) => {
                let o = &mut self.outs[i];
                o.cfg.port = port;
                o.cfg.mode = mode;
                o.cfg.refresh_hz = refresh_hz;
                if o.cfg.net != net {
                    o.from_net = false;
                    o.arb = SacnArbiter::new();
                }
                o.cfg.net = net;
                if let Some(n) = s("name") {
                    o.cfg.name = n.chars().take(32).collect();
                }
                o.version = version;
                o.id
            }
            None => {
                self.next_id = self.next_id.wrapping_add(1).max(1);
                while self.outs.iter().any(|o| o.id == self.next_id) {
                    self.next_id = self.next_id.wrapping_add(1).max(1);
                }
                let id = a
                    .get("id")
                    .and_then(Value::as_u64)
                    .filter(|&i| (1..=u16::MAX as u64).contains(&i) && !self.outs.iter().any(|o| o.id as u64 == i))
                    .map_or(self.next_id, |i| i as u16);
                let name = s("name").map(|n| n.chars().take(32).collect()).unwrap_or_else(|| format!("DMX {id}"));
                self.outs.push(Out {
                    id,
                    cfg: OutConfig { name, port, mode, refresh_hz, net },
                    version,
                    manual: [0; SLOTS],
                    net: [0; SLOTS],
                    from_net: false,
                    arb: SacnArbiter::new(),
                    last_rx: None,
                    packets: 0,
                    state: Ok(false),
                });
                id
            }
        };
        Ok(id)
    }

    pub fn remove(&mut self, id: i64) -> Result<(), HttpError> {
        let i = self.index(id)?;
        self.outs.remove(i);
        self.bump();
        Ok(())
    }

    /// Set levels by hand (and stop following the network until its next packet).
    /// `start` is the 1-based channel; `values` an array of 0–255 or a hex string; or
    /// `fill` for `count` channels.
    pub fn set_levels(&mut self, id: i64, a: &Value) -> Result<(), HttpError> {
        let i = self.index(id)?;
        let o = &mut self.outs[i];
        let start = match a.get("start") {
            None | Some(Value::Null) => 1,
            Some(v) => v.as_u64().filter(|s| (1..=SLOTS as u64).contains(s)).ok_or_else(|| bad("start must be a channel, 1–512"))? as usize,
        };
        let mut vals: Vec<u8> = Vec::new();
        if let Some(f) = a.get("fill").filter(|v| !v.is_null()) {
            let f = f.as_u64().filter(|&v| v <= 255).ok_or_else(|| bad("fill must be 0–255"))? as u8;
            let count = a.get("count").and_then(Value::as_u64).unwrap_or((SLOTS - start + 1) as u64) as usize;
            vals.resize(count.min(SLOTS - start + 1), f);
        } else {
            match a.get("values") {
                Some(Value::Array(xs)) => {
                    for x in xs {
                        vals.push(x.as_u64().filter(|&v| v <= 255).ok_or_else(|| bad("values must be 0–255"))? as u8);
                    }
                }
                Some(Value::String(h)) => {
                    let h: String = h.chars().filter(|c| !c.is_whitespace()).collect();
                    if !h.len().is_multiple_of(2) || !h.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Err(bad("values as a string must be hex, two digits a channel"));
                    }
                    vals = (0..h.len()).step_by(2).map(|k| u8::from_str_radix(&h[k..k + 2], 16).unwrap_or(0)).collect();
                }
                _ => return Err(bad("Give values (an array of 0–255 or a hex string) or fill")),
            }
            if start - 1 + vals.len() > SLOTS {
                return Err(bad(format!("{} values from channel {start} run past channel 512", vals.len())));
            }
        }
        o.manual[start - 1..start - 1 + vals.len()].copy_from_slice(&vals);
        o.from_net = false;
        Ok(())
    }

    /// A UDP packet from the receivers. True when an output took it.
    pub fn receive(&mut self, port: u16, pkt: &[u8], from: &str, now_ms: u32) -> bool {
        let mut took = false;
        let sacn = if port == SACN_PORT { pnet::parse_sacn(pkt).ok() } else { None };
        let art = if port == ARTNET_PORT { pnet::parse_artnet(pkt).ok() } else { None };
        for o in &mut self.outs {
            let dmx = match (o.cfg.net, &sacn, &art) {
                (Some((Proto::Sacn, u)), Some(p), _) if p.dmx.universe == u => {
                    if !o.arb.accept(p, now_ms) {
                        continue;
                    }
                    p.dmx
                }
                (Some((Proto::ArtNet, u)), _, Some(d)) if d.universe == u => *d,
                _ => continue,
            };
            let n = dmx.data.len().min(SLOTS);
            o.net[..n].copy_from_slice(&dmx.data[..n]);
            o.from_net = true;
            o.packets = o.packets.wrapping_add(1);
            let who = match &sacn {
                Some(p) if !p.source_name.is_empty() => String::from_utf8_lossy(p.source_name).to_string(),
                _ => from.to_string(),
            };
            o.last_rx = Some((Instant::now(), who));
            took = true;
        }
        took
    }

    /// Protocol bits (1 sACN, 2 Art-Net) and the sACN universes to join.
    pub fn wants(&self) -> (u8, Vec<u16>) {
        let mut bits = 0;
        let mut u = Vec::new();
        for o in &self.outs {
            match o.cfg.net {
                Some((Proto::Sacn, n)) => {
                    bits |= 1;
                    u.push(n);
                }
                Some((Proto::ArtNet, _)) => bits |= 2,
                None => {}
            }
        }
        (bits, u)
    }

    pub fn out_json(o: &Out, output: bool) -> Value {
        let (state, error) = match (&o.state, output) {
            (Err(e), _) => ("error", Some(e.clone())),
            (_, false) => ("off", None),
            (Ok(true), true) => ("running", None),
            (Ok(false), true) => ("pending", None),
        };
        let levels: String = o.levels().iter().map(|b| format!("{b:02x}")).collect();
        json!({
            "id": o.id,
            "name": o.cfg.name,
            "port": o.cfg.port,
            "mode": o.cfg.mode.name(),
            "refreshHz": o.cfg.refresh_hz,
            "network": o.cfg.net.map(|(p, u)| json!({ "protocol": if p == Proto::Sacn { "sacn" } else { "artnet" }, "universe": u })),
            "source": if o.from_net { "network" } else { "manual" },
            "receiving": o.last_rx.as_ref().map(|(t, who)| json!({ "from": who, "packets": o.packets, "ageMs": t.elapsed().as_millis() as u64 })),
            "state": state,
            "error": error,
            "levels": levels,
        })
    }

    /// The outputs as settings (no levels, no live state).
    pub fn saved(&self) -> Value {
        Value::Array(
            self.outs
                .iter()
                .map(|o| {
                    json!({
                        "id": o.id, "name": o.cfg.name, "port": o.cfg.port, "mode": o.cfg.mode.name(), "refreshHz": o.cfg.refresh_hz,
                        "network": o.cfg.net.map(|(p, u)| json!({ "protocol": if p == Proto::Sacn { "sacn" } else { "artnet" }, "universe": u })),
                    })
                })
                .collect(),
        )
    }

    pub fn load(&mut self, saved: &Value) {
        for s in saved.as_array().into_iter().flatten() {
            if let Err(e) = self.configure(None, s) {
                tracing::warn!("[leds] saved DMX output not restored: {}", e.message);
            }
        }
    }
}

/// The Enttec DMX USB Pro "send DMX packet" message (label 6): start code 0 and the slots.
pub fn enttec_frame(levels: &[u8; SLOTS], out: &mut Vec<u8>) {
    out.clear();
    let len = (SLOTS + 1) as u16;
    out.extend_from_slice(&[0x7E, 6, len as u8, (len >> 8) as u8, 0x00]);
    out.extend_from_slice(levels);
    out.push(0xE7);
}

// ---------- the senders ----------

fn open(o: &OutConfig) -> Result<OwnedFd, String> {
    let fd = T::open_port(&o.port, false).map_err(|e| format!("{}: {e}", o.port))?;
    T::set_exclusive(fd.as_raw_fd()).map_err(|e| format!("{}: {}", o.port, e.code()))?;
    let cfg = match o.mode {
        Mode::Raw => SerialConfig { baud: 250_000, data_bits: 8, parity: Parity::None, stop_bits: 2, flow: Flow::None },
        // The widget's USB side ignores the rate; 57600 is what Enttec's own tools set.
        Mode::EnttecPro => SerialConfig { baud: 57_600, data_bits: 8, parity: Parity::None, stop_bits: 1, flow: Flow::None },
    };
    let got = T::configure(fd.as_raw_fd(), &cfg).map_err(|e| format!("{}: {}", o.port, e.code()))?;
    if o.mode == Mode::Raw && got.baud != 250_000 {
        return Err(format!("{}: the adapter won't do 250 kbaud (it gave {})", o.port, got.baud));
    }
    Ok(fd)
}

fn write_all(fd: i32, mut data: &[u8]) -> Result<(), String> {
    let t0 = Instant::now();
    while !data.is_empty() {
        match T::write_some(fd, data) {
            Ok(0) => {
                if t0.elapsed() > Duration::from_millis(500) {
                    return Err("the port stopped taking data".into());
                }
                std::thread::sleep(Duration::from_micros(200));
            }
            Ok(n) => data = &data[n..],
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

/// One frame out of `fd`. Raw: wait for the last frame to leave, break, mark after
/// break, start code 0 and the slots. Enttec Pro: the widget's "send DMX" message.
pub fn send_frame(fd: i32, mode: Mode, levels: &[u8; SLOTS], frame: &mut [u8; SLOTS + 1], msg: &mut Vec<u8>) -> Result<(), String> {
    match mode {
        Mode::Raw => {
            // SAFETY: tcdrain on our own fd.
            unsafe { libc::tcdrain(fd) };
            T::set_break(fd, true).map_err(|e| e.code())?;
            std::thread::sleep(Duration::from_micros(100)); // break ≥ 92 µs
            T::set_break(fd, false).map_err(|e| e.code())?;
            std::thread::sleep(Duration::from_micros(12)); // mark after break ≥ 12 µs
            frame[0] = 0; // start code: dimmer data
            frame[1..].copy_from_slice(levels);
            write_all(fd, frame)
        }
        Mode::EnttecPro => {
            enttec_frame(levels, msg);
            write_all(fd, msg)
        }
    }
}

/// One output's sender: frames at its refresh rate while output control is on.
fn send_loop(inner: Arc<Inner>, id: u16, version: u32, stop: Arc<AtomicBool>) {
    let cfg = {
        let d = inner.dmx.lock().unwrap();
        match d.outs.iter().find(|o| o.id == id) {
            Some(o) => o.cfg.clone(),
            None => return,
        }
    };
    let set_state = |s: Result<bool, String>| {
        if let Some(o) = inner.dmx.lock().unwrap().outs.iter_mut().find(|o| o.id == id && o.version == version) {
            o.state = s;
        }
    };
    let fd = match open(&cfg) {
        Ok(fd) => fd,
        Err(e) => {
            tracing::warn!("[leds] DMX output {id}: {e}");
            set_state(Err(e));
            return;
        }
    };
    let raw = fd.as_raw_fd();
    let period = Duration::from_micros(1_000_000 / cfg.refresh_hz.max(1) as u64);
    let mut msg = Vec::with_capacity(SLOTS + 6);
    let mut frame = [0u8; SLOTS + 1];
    let mut sending = false;
    while !stop.load(Ordering::SeqCst) {
        let t = Instant::now();
        if !inner.gpio.output_enabled() {
            if sending {
                sending = false;
                set_state(Ok(false));
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let levels = {
            let d = inner.dmx.lock().unwrap();
            match d.outs.iter().find(|o| o.id == id) {
                Some(o) => *o.levels(),
                None => break,
            }
        };
        let r = send_frame(raw, cfg.mode, &levels, &mut frame, &mut msg);
        match r {
            Ok(()) if !sending => {
                sending = true;
                set_state(Ok(true));
            }
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("[leds] DMX output {id}: {e}");
                set_state(Err(format!("{}: {e}", cfg.port)));
                sending = false;
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        if let Some(rest) = period.checked_sub(t.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    let _ = T::clear_exclusive(raw);
}

/// Keep one sender per output, restarted when its config changes.
pub(crate) fn supervise(inner: Arc<Inner>) {
    let epoch = inner.epoch.load(Ordering::SeqCst);
    let mut running: HashMap<u16, (u32, Arc<AtomicBool>, std::thread::JoinHandle<()>)> = HashMap::new();
    while inner.epoch.load(Ordering::SeqCst) == epoch {
        let want: Vec<(u16, u32)> = inner.dmx.lock().unwrap().outs.iter().map(|o| (o.id, o.version)).collect();
        let stale: Vec<u16> = running.iter().filter(|(id, (g, ..))| !want.contains(&(**id, *g))).map(|(id, _)| *id).collect();
        for id in stale {
            if let Some((_, stop, h)) = running.remove(&id) {
                stop.store(true, Ordering::SeqCst);
                let _ = h.join();
            }
        }
        for (id, version) in want {
            if let std::collections::hash_map::Entry::Vacant(e) = running.entry(id) {
                let stop = Arc::new(AtomicBool::new(false));
                let (i, s) = (inner.clone(), stop.clone());
                if let Ok(h) = std::thread::Builder::new().name(format!("dmx-{id}")).spawn(move || send_loop(i, id, version, s)) {
                    e.insert((version, stop, h));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    for (_, (_, stop, h)) in running {
        stop.store(true, Ordering::SeqCst);
        let _ = h.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sacn(universe: u16, seq: u8, data: &[u8]) -> Vec<u8> {
        let end = 126 + data.len();
        let mut p = vec![0u8; end];
        p[0..2].copy_from_slice(&0x0010u16.to_be_bytes());
        p[4..16].copy_from_slice(b"ASC-E1.17\0\0\0");
        p[16..18].copy_from_slice(&(0x7000 | (end - 16) as u16).to_be_bytes());
        p[18..22].copy_from_slice(&4u32.to_be_bytes());
        p[38..40].copy_from_slice(&(0x7000 | (end - 38) as u16).to_be_bytes());
        p[40..44].copy_from_slice(&2u32.to_be_bytes());
        p[44..47].copy_from_slice(b"QLC");
        p[108] = 100;
        p[111] = seq;
        p[113..115].copy_from_slice(&universe.to_be_bytes());
        p[115..117].copy_from_slice(&(0x7000 | (end - 115) as u16).to_be_bytes());
        p[117] = 0x02;
        p[118] = 0xA1;
        p[121..123].copy_from_slice(&1u16.to_be_bytes());
        p[123..125].copy_from_slice(&(1 + data.len() as u16).to_be_bytes());
        p[126..].copy_from_slice(data);
        p
    }

    #[test]
    fn configure_validates_and_keeps_what_isnt_given() {
        let mut d = Dmx::default();
        assert_eq!(d.configure(None, &json!({})).unwrap_err().status, 400);
        assert_eq!(d.configure(None, &json!({ "port": "/etc/passwd/../x" })).unwrap_err().status, 400);
        let id = d.configure(None, &json!({ "port": "/dev/ttyUSB0", "network": { "protocol": "sacn", "universe": 7 } })).unwrap();
        assert_eq!(d.configure(None, &json!({ "port": "/dev/ttyUSB0" })).unwrap_err().status, 409);
        assert_eq!(d.configure(Some(id as i64), &json!({ "refreshHz": 60 })).unwrap_err().status, 400);
        d.configure(Some(id as i64), &json!({ "mode": "enttec-pro", "name": "stage" })).unwrap();
        let o = &d.outs[0];
        assert_eq!((o.cfg.mode, o.cfg.net, o.cfg.refresh_hz, o.cfg.name.as_str()), (Mode::EnttecPro, Some((Proto::Sacn, 7)), 40, "stage"));
        assert_eq!(d.wants(), (1, vec![7]));
        // Settings round-trip under the same ids.
        let mut e = Dmx::default();
        e.load(&d.saved());
        assert_eq!(e.outs[0].id, id);
        assert_eq!(e.outs[0].cfg, d.outs[0].cfg);
    }

    #[test]
    fn levels_by_hand_and_from_the_network() {
        let mut d = Dmx::default();
        let id = d.configure(None, &json!({ "port": "/dev/ttyUSB1", "network": { "protocol": "sacn", "universe": 2 } })).unwrap() as i64;
        d.set_levels(id, &json!({ "start": 10, "values": [1, 2, 3] })).unwrap();
        d.set_levels(id, &json!({ "start": 511, "values": "ff 80" })).unwrap();
        assert!(d.set_levels(id, &json!({ "start": 512, "values": [1, 2] })).is_err());
        assert_eq!(&d.outs[0].levels()[9..12], &[1, 2, 3]);
        assert_eq!(&d.outs[0].levels()[510..], &[0xff, 0x80]);
        // The network takes over (and only its universe).
        assert!(!d.receive(SACN_PORT, &sacn(3, 1, &[9; 4]), "x", 0));
        assert!(d.receive(SACN_PORT, &sacn(2, 1, &[50, 60]), "x", 0));
        assert_eq!(&d.outs[0].levels()[..3], &[50, 60, 0]);
        let j = Dmx::out_json(&d.outs[0], false);
        assert_eq!(
            (j["source"].as_str(), j["state"].as_str(), j["receiving"]["from"].as_str()),
            (Some("network"), Some("off"), Some("QLC"))
        );
        assert!(j["levels"].as_str().unwrap().starts_with("323c00"));
        // A hand-set level takes it back; fill.
        d.set_levels(id, &json!({ "fill": 255, "start": 1, "count": 4 })).unwrap();
        assert_eq!(&d.outs[0].levels()[..5], &[255, 255, 255, 255, 0]);
    }

    #[test]
    fn raw_frames_reach_the_other_end_of_a_pty() {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty fills two fds we own and close below.
        if unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), std::ptr::null()) } != 0 {
            eprintln!("skipping: no pty");
            return;
        }
        let mut name = [0 as libc::c_char; 128];
        // SAFETY: ttyname_r into our buffer.
        unsafe { libc::ttyname_r(slave, name.as_mut_ptr(), name.len()) };
        let path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_string_lossy().to_string();
        let cfg = OutConfig { name: "t".into(), port: path, mode: Mode::Raw, refresh_hz: 40, net: None };
        let fd = open(&cfg).expect("open the pty as a DMX port");
        let mut lv = [0u8; SLOTS];
        lv[0] = 7;
        lv[511] = 9;
        let (mut frame, mut msg) = ([0u8; SLOTS + 1], Vec::new());
        send_frame(fd.as_raw_fd(), Mode::Raw, &lv, &mut frame, &mut msg).unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 1024];
        let t = Instant::now();
        while got.len() < SLOTS + 1 && t.elapsed() < Duration::from_secs(2) {
            // SAFETY: reading the master side into our buffer.
            let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                got.extend_from_slice(&buf[..n as usize]);
            }
        }
        assert_eq!(got.len(), SLOTS + 1);
        assert_eq!((got[0], got[1], got[512]), (0, 7, 9));
        drop(fd);
        // SAFETY: closing what openpty gave us.
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
    }

    #[test]
    fn enttec_pro_framing() {
        let mut lv = [0u8; SLOTS];
        lv[0] = 0xAA;
        let mut m = Vec::new();
        enttec_frame(&lv, &mut m);
        assert_eq!(&m[..6], &[0x7E, 6, 0x01, 0x02, 0x00, 0xAA]);
        assert_eq!(m.len(), 5 + SLOTS + 1);
        assert_eq!(*m.last().unwrap(), 0xE7);
    }
}
