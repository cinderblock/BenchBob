//! The 1-Wire panel, ported from `peripherals/drivers/onewire.ts`: masters (w1-gpio on
//! GPIO4, DS2482 I²C bridges, DS9490R USB adapters) and their slaves, decoded by family
//! through the kernel's w1 slave drivers' sysfs attributes ([`families`]). Readings are
//! polled in the background, never from status(): a DS18B20 conversion takes up to 750 ms
//! per sensor, so several sensors on one bus use therm_bulk_read (one simultaneous
//! conversion) when the master offers it.
//!
//! iButton readers are a classic use, so family-0x01 arrivals/departures are kept in a
//! touch log with timestamps. Family 0x00 "slaves" are not devices: a floating data line
//! (no 4.7 kΩ pull-up, nothing attached) makes the search algorithm read noise that happens
//! to pass the CRC; they're counted as bus noise and hidden.
//!
//! Root-only sysfs writes (search, bulk read, outputs, resolution, bridge binding) fall back
//! to `sudo -n tee`. `BENCHBOB_W1_ROOT` points the driver at a fixture tree
//! ([`fixtures`]) for tests and side instances.

pub mod families;
pub mod fixtures;
mod routes;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use benchbob_core::http::{HttpError, js_number, truthy};
use benchbob_core::run::SYSTEM_PATH;
use benchbob_core::timeline::now_ms;
use benchbob_core::udev::UdevEvent;
use benchbob_core::{Ctx, Driver, Json, Route, StopReason, Tool};
use benchbob_i2c::I2cHandle;
use benchbob_i2c::json::val_json;
use benchbob_i2c_chips::V;
use indexmap::IndexMap;
use serde_json::{Map, json};

use crate::families::{Decoded, Family, OwPin};

const HIST_N: usize = 120;
const TOUCH_MAX: usize = 200;

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub(crate) struct Master {
    pub id: String,
    /// "gpio" | "ds2482" | "ds2490" | "other"
    pub kind: &'static str,
    pub label: String,
    pub search: i64,
    pub pullup: String,
    pub timeout: String,
    pub attempts: i64,
    pub bulk: bool,
    pub slaves: Vec<String>,
    /// Family-00 ids → last seen.
    pub ghosts: IndexMap<String, u64>,
    pub error: String,
}

pub(crate) struct Slave {
    pub id: String,
    pub master: String,
    pub fam: u8,
    pub family: Family,
    pub rom_ok: Option<bool>,
    pub present: bool,
    pub first_seen: u64,
    pub last_seen: u64,
    pub decoded: Option<Decoded>,
    pub read_at: u64,
    pub error: String,
    pub hist: Vec<(String, Vec<f64>)>,
}

impl Slave {
    fn record(&mut self, d: Decoded) {
        self.read_at = now_ms();
        self.error.clear();
        for v in &d.values {
            let V::Num(n) = v.v else { continue };
            let h = match self.hist.iter_mut().position(|(k, _)| *k == v.k) {
                Some(i) => &mut self.hist[i].1,
                None => {
                    self.hist.push((v.k.clone(), Vec::new()));
                    &mut self.hist.last_mut().unwrap().1
                }
            };
            h.push(n);
            if h.len() > HIST_N {
                h.remove(0);
            }
        }
        self.decoded = Some(d);
    }
    fn is_therm(&self) -> bool {
        matches!(self.family.kind, "temp" | "thermocouple")
    }
}

pub(crate) struct Touch {
    pub t: u64,
    pub id: String,
    /// "touch" | "release" | "appear" | "gone"
    pub event: &'static str,
    pub family: String,
}

#[derive(Clone, PartialEq)]
pub(crate) struct Source {
    kind: &'static str,
    key: String,
    label: String,
}

#[derive(Default)]
pub(crate) struct State {
    pub masters: IndexMap<String, Master>,
    pub slaves: IndexMap<String, Slave>,
    pub touch: Vec<Touch>,
    /// Device-tree node → pin, for labelling w1-gpio masters.
    gpio_pins: HashMap<String, u32>,
    source_of: HashMap<String, Source>,
    known_sources: HashSet<String>,
}

pub(crate) struct Inner {
    pub ctx: OnceLock<Ctx>,
    pub root: PathBuf,
    pub fixture: bool,
    /// "/sys" (bus driver bindings, the I²C bridge files); a temp tree in tests.
    pub sys: PathBuf,
    /// "/proc/device-tree"
    pub proc_dt: PathBuf,
    pub i2c: Option<I2cHandle>,
    pub st: Mutex<State>,
    pub stopped: AtomicBool,
    pub epoch: AtomicU64,
    pub poll_gen: AtomicU64,
    pub polling: AtomicBool,
}

/// The 1-Wire driver.
pub struct OneWire {
    inner: Arc<Inner>,
}

impl OneWire {
    /// `i2c`: the I²C driver, for DS2482 bridges (None when that driver isn't built in).
    pub fn new(i2c: Option<I2cHandle>) -> Self {
        let root = benchbob_core::paths::env("BENCHBOB_W1_ROOT");
        let fixture = root.is_some();
        Self::with(
            PathBuf::from(root.unwrap_or_else(|| "/sys/bus/w1/devices".into())),
            fixture,
            PathBuf::from("/sys"),
            PathBuf::from("/proc/device-tree"),
            i2c,
        )
    }

    pub(crate) fn with(root: PathBuf, fixture: bool, sys: PathBuf, proc_dt: PathBuf, i2c: Option<I2cHandle>) -> Self {
        Self {
            inner: Arc::new(Inner {
                ctx: OnceLock::new(),
                root,
                fixture,
                sys,
                proc_dt,
                i2c,
                st: Mutex::new(State::default()),
                stopped: AtomicBool::new(true),
                epoch: AtomicU64::new(0),
                poll_gen: AtomicU64::new(0),
                polling: AtomicBool::new(false),
            }),
        }
    }
}

// ---------- sysfs I/O ----------

pub(crate) fn rd(path: &Path) -> String {
    benchbob_core::run::read_text(path)
}

pub(crate) fn rd_bin(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

/// Write a sysfs attribute, falling back to `sudo -n tee` for root-only files.
pub(crate) fn wr(path: &Path, data: &[u8]) -> Result<(), HttpError> {
    let p = path.display().to_string();
    match std::fs::write(path, data) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(e) => return Err(HttpError::new(502, format!("{p}: {}", benchbob_i2c::linux::open_error(&e, &p)))),
    }
    let child = Command::new("sudo")
        .args(["-n", "tee", &p])
        .env("PATH", SYSTEM_PATH)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = child.map_err(|e| HttpError::new(502, format!("{p}: {e}")))?;
    if let Some(mut i) = child.stdin.take() {
        let _ = i.write_all(data);
    }
    let out = child.wait_with_output().map_err(|e| HttpError::new(502, format!("{p}: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(HttpError::new(502, format!("{p}: {}", if err.is_empty() { "write failed".into() } else { err })));
    }
    Ok(())
}

/// Run a command with a time limit (blocking): (exit code, stdout, stderr), all trimmed.
pub(crate) fn run_sync(cmd: &[&str], timeout: Duration) -> (i32, String, String) {
    let child = Command::new(cmd[0])
        .args(&cmd[1..])
        .env("PATH", SYSTEM_PATH)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (127, String::new(), format!("{}: {e}", cmd[0])),
    };
    let t0 = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if t0.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return (-1, String::new(), format!("{}: timed out", cmd[0]));
            }
        }
    }
    match child.wait_with_output() {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).trim().to_string(),
            String::from_utf8_lossy(&o.stderr).trim().to_string(),
        ),
        Err(e) => (-1, String::new(), e.to_string()),
    }
}

/// `/^[0-9a-f]{2}-[0-9a-f]{12}$/i`
fn ok_id(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() == 15 && b[2] == b'-' && b.iter().enumerate().all(|(i, c)| i == 2 || c.is_ascii_hexdigit())
}

fn ls(d: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(d)
        .map(|r| r.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// BCM GPIO → physical pin on the Pi's 40-pin header.
fn header_pin(gpio: u32) -> Option<u32> {
    const MAP: [(u32, u32); 28] = [
        (2, 3),
        (3, 5),
        (4, 7),
        (17, 11),
        (27, 13),
        (22, 15),
        (10, 19),
        (9, 21),
        (11, 23),
        (0, 27),
        (5, 29),
        (6, 31),
        (13, 33),
        (19, 35),
        (26, 37),
        (14, 8),
        (15, 10),
        (18, 12),
        (23, 16),
        (24, 18),
        (25, 22),
        (8, 24),
        (7, 26),
        (1, 28),
        (12, 32),
        (16, 36),
        (20, 38),
        (21, 40),
    ];
    MAP.iter().find(|(g, _)| *g == gpio).map(|(_, p)| *p)
}

/// "1.5" for 1.46: `toFixed(1)` (ties away from zero on the exact value, as JavaScript does).
pub(crate) fn to_fixed1(x: f64) -> String {
    let a = x.abs();
    let s = a * 10.0;
    let txt = if s.fract() == 0.5 && s / 10.0 == a { format!("{:.1}", (s.floor() + 1.0) / 10.0) } else { format!("{a:.1}") };
    if x < 0.0 { format!("-{txt}") } else { txt }
}

impl Inner {
    pub(crate) fn ctx(&self) -> &Ctx {
        self.ctx.get().expect("onewire driver used before init")
    }

    pub(crate) fn changed(&self) {
        if let Some(c) = self.ctx.get() {
            c.changed();
        }
    }

    pub(crate) fn setting(&self, k: &str) -> Option<Json> {
        self.ctx.get().and_then(|c| c.setting(k))
    }

    pub(crate) fn labels(&self) -> Map<String, Json> {
        self.setting("labels").and_then(|v| v.as_object().cloned()).unwrap_or_default()
    }

    /// `Math.max(2000, settings.interval ?? 10000)`
    pub(crate) fn interval(&self) -> f64 {
        let v = self.setting("interval").map(|v| js_number(&v)).unwrap_or(10_000.0);
        if v.is_nan() { v } else { v.max(2000.0) }
    }

    pub(crate) fn use_bulk(&self) -> Json {
        self.setting("bulk").unwrap_or(Json::Bool(true))
    }

    pub(crate) fn slave_label(&self, s: &Slave) -> String {
        match self.labels().get(&s.id).filter(|v| truthy(v)) {
            Some(l) => format!("{} ({})", benchbob_core::http::js_string(l), s.id),
            None => format!("{} {}", s.family.name, s.id),
        }
    }

    // ---------- discovery ----------

    fn load_gpio_pins(&self) {
        let mut pins = HashMap::new();
        for n in ls(&self.proc_dt) {
            if !n.starts_with("onewire") {
                continue;
            }
            if let Some(g) = rd_bin(&self.proc_dt.join(&n).join("gpios"))
                && g.len() >= 8
            {
                pins.insert(n, u32::from_be_bytes([g[4], g[5], g[6], g[7]]));
            }
        }
        lock(&self.st).gpio_pins = pins;
    }

    fn list_sources(&self, st: &State) -> Vec<Source> {
        let skip = ["bind", "unbind", "uevent", "module", "new_id", "remove_id"];
        let ls2 = |d: &str| ls(&self.sys.join(d)).into_iter().filter(|n| !skip.contains(&n.as_str())).collect::<Vec<_>>();
        let mut out = Vec::new();
        for n in ls2("bus/platform/drivers/w1-gpio") {
            let label = match st.gpio_pins.get(&n) {
                Some(pin) => format!("GPIO{pin} (header pin {})", header_pin(*pin).map_or("?".into(), |p| p.to_string())),
                None => format!("w1-gpio {n}"),
            };
            out.push(Source { kind: "gpio", key: format!("gpio:{n}"), label });
        }
        for n in ls2("bus/i2c/drivers/ds2482") {
            if let Some((bus, a)) = n.split_once("-00")
                && !bus.is_empty()
                && bus.bytes().all(|c| c.is_ascii_digit())
                && a.len() == 2
                && a.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            {
                out.push(Source { kind: "ds2482", key: format!("ds2482:{n}"), label: format!("DS2482 on i2c-{bus} @0x{a}") });
            }
        }
        for n in ls2("bus/usb/drivers/ds2490") {
            if n.contains(':') {
                out.push(Source {
                    kind: "ds2490",
                    key: format!("ds2490:{n}"),
                    label: format!("DS9490R USB adapter ({})", n.split(':').next().unwrap_or("")),
                });
            }
        }
        out
    }

    // Master sysfs nodes have no parent device (/sys/devices/w1_bus_masterN for every kind),
    // so which bus driver created a master is inferred from the drivers' own bindings:
    // w1-gpio platform devices, ds2482 I²C clients, ds2490 USB interfaces. A master that
    // appears right after a new binding belongs to it; at start-up masters are matched in
    // registration order (device-tree w1-gpio first, then bridges as they were bound).
    fn attribute_masters(&self, st: &mut State, ids: &[String]) {
        if self.fixture {
            return;
        }
        let sources = self.list_sources(st);
        let fresh: Vec<&Source> = sources.iter().filter(|s| !st.known_sources.contains(&s.key)).collect();
        st.known_sources = sources.iter().map(|s| s.key.clone()).collect();
        let live: HashSet<&String> = sources.iter().map(|s| &s.key).collect();
        st.source_of.retain(|id, src| live.contains(&src.key) && ids.contains(id));
        let taken: HashSet<String> = st.source_of.values().map(|s| s.key.clone()).collect();
        let num = |id: &str| id.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse::<u64>().unwrap_or(0);
        let mut unassigned: Vec<&String> = ids.iter().filter(|id| !st.source_of.contains_key(*id)).collect();
        unassigned.sort_by_key(|id| num(id));
        // Prefer just-bound sources for the newest masters, then registration order.
        let mut pool: Vec<Source> = sources.iter().filter(|s| !taken.contains(&s.key) && !fresh.contains(s)).cloned().collect();
        pool.extend(fresh.iter().filter(|s| !taken.contains(&s.key)).map(|s| (*s).clone()));
        let mut pool = pool.into_iter();
        for id in unassigned {
            if let Some(src) = pool.next() {
                st.source_of.insert(id.clone(), src);
            }
        }
    }

    fn classify_master(&self, st: &State, id: &str) -> (&'static str, String) {
        if let Some(s) = st.source_of.get(id) {
            return (s.kind, s.label.clone());
        }
        if self.fixture { ("gpio", "Fixture bus (BENCHBOB_W1_ROOT)".into()) } else { ("other", id.to_string()) }
    }

    /// Presence pass: master attributes + slave lists. No bus traffic. Blocking (sysfs).
    pub(crate) fn refresh(self: &Arc<Self>) {
        let mut ids: Vec<String> = ls(&self.root)
            .into_iter()
            .filter(|n| n.strip_prefix("w1_bus_master").is_some_and(|d| !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit())))
            .collect();
        ids.sort_by_key(|n| n["w1_bus_master".len()..].parse::<u64>().unwrap_or(0));
        let now = now_ms();
        let stopped = self.stopped.load(Ordering::SeqCst);
        let mut st = lock(&self.st);
        self.attribute_masters(&mut st, &ids);
        let mut poll_soon = false;
        for id in &ids {
            let (kind, label) = self.classify_master(&st, id);
            let fresh_master = !st.masters.contains_key(id);
            let dir = self.root.join(id);
            let list: Vec<String> =
                rd(&dir.join("w1_master_slaves")).split('\n').map(|s| s.trim().to_string()).filter(|s| ok_id(s)).collect();
            {
                let m = st.masters.entry(id.clone()).or_insert_with(|| Master {
                    id: id.clone(),
                    kind,
                    label: label.clone(),
                    search: -1,
                    pullup: String::new(),
                    timeout: String::new(),
                    attempts: 0,
                    bulk: false,
                    slaves: vec![],
                    ghosts: IndexMap::new(),
                    error: String::new(),
                });
                m.kind = kind;
                m.label = label.clone();
                m.search = benchbob_core::http::parse_int(&rd(&dir.join("w1_master_search"))).unwrap_or(0);
                m.pullup = rd(&dir.join("w1_master_pullup"));
                m.timeout = rd(&dir.join("w1_master_timeout"));
                m.attempts = benchbob_core::http::parse_int(&rd(&dir.join("w1_master_attempts"))).unwrap_or(0);
                m.bulk = !rd(&dir.join("therm_bulk_read")).is_empty();
                for s in &list {
                    if families::family_of(s) == Some(0) {
                        m.ghosts.insert(s.clone(), now);
                    }
                }
                m.ghosts.retain(|_, t| now - *t <= 600_000);
                m.slaves = list.iter().filter(|s| families::family_of(s) != Some(0)).cloned().collect();
            }
            if fresh_master && !stopped {
                self.ctx().event(&format!("1-Wire master {id} appeared ({label})"), None);
            }
            let slaves = st.masters[id].slaves.clone();
            for sid in slaves {
                let fresh = !st.slaves.contains_key(&sid);
                let back = st.slaves.get(&sid).is_some_and(|s| !s.present);
                if fresh {
                    let fam = families::family_of(&sid).unwrap_or(0);
                    let rom = rd_bin(&self.root.join(&sid).join("id"));
                    st.slaves.insert(
                        sid.clone(),
                        Slave {
                            id: sid.clone(),
                            master: id.clone(),
                            fam,
                            family: families::family(fam),
                            rom_ok: rom.map(|r| families::rom_ok(&r)),
                            present: true,
                            first_seen: now,
                            last_seen: now,
                            decoded: None,
                            read_at: 0,
                            error: String::new(),
                            hist: vec![],
                        },
                    );
                }
                if fresh || back {
                    let s = st.slaves.get_mut(&sid).unwrap();
                    s.present = true;
                    s.master = id.clone();
                    let (label, fam_name, ib) = (self.slave_label(s), s.family.name.clone(), s.fam == 0x01);
                    self.log_presence(&mut st, &sid, &label, &fam_name, ib, true, fresh, stopped);
                    poll_soon = true; // read the newcomer soon
                }
                st.slaves.get_mut(&sid).unwrap().last_seen = now;
            }
        }
        let gone: Vec<String> = st.masters.keys().filter(|k| !ids.contains(k)).cloned().collect();
        for id in gone {
            let m = st.masters.shift_remove(&id).unwrap();
            if !stopped {
                self.ctx().event(&format!("1-Wire master {id} went away ({})", m.label), None);
            }
        }
        let leaving: Vec<String> = st
            .slaves
            .values()
            .filter(|s| s.present && !st.masters.get(&s.master).is_some_and(|m| m.slaves.contains(&s.id)))
            .map(|s| s.id.clone())
            .collect();
        for sid in leaving {
            let s = st.slaves.get_mut(&sid).unwrap();
            s.present = false;
            let (label, fam_name, ib) = (self.slave_label(s), s.family.name.clone(), s.fam == 0x01);
            self.log_presence(&mut st, &sid, &label, &fam_name, ib, false, false, stopped);
        }
        drop(st);
        if poll_soon {
            self.schedule_poll(50);
        }
        self.changed();
    }

    #[allow(clippy::too_many_arguments)]
    fn log_presence(&self, st: &mut State, id: &str, label: &str, family: &str, ibutton: bool, on: bool, fresh: bool, stopped: bool) {
        if stopped && on {
            return; // initial enumeration at start-up isn't an event
        }
        let event = match (ibutton, on) {
            (true, true) => "touch",
            (true, false) => "release",
            (false, true) => "appear",
            (false, false) => "gone",
        };
        st.touch.push(Touch { t: now_ms(), id: id.into(), event, family: family.into() });
        if st.touch.len() > TOUCH_MAX {
            let n = st.touch.len() - TOUCH_MAX;
            st.touch.drain(..n);
        }
        let what = match (ibutton, on, fresh) {
            (true, true, _) => "touched",
            (true, false, _) => "released",
            (false, true, true) => "appeared",
            (false, true, false) => "came back",
            (false, false, _) => "went away",
        };
        if let Some(c) = self.ctx.get() {
            c.event(&format!("1-Wire {label} {what}"), Some(json!({ "id": id })));
        }
    }

    // ---------- reading ----------

    /// Read one slave's attributes and decode them. Blocking (a conversion takes up to 750 ms).
    pub(crate) fn read_slave(&self, id: &str, eeprom: bool) -> Result<Decoded, String> {
        let (fam, family, present, rom_ok) = {
            let st = lock(&self.st);
            let s = st.slaves.get(id).ok_or_else(|| format!("{id} went away"))?;
            (s.fam, s.family.clone(), s.present, s.rom_ok)
        };
        let dir = self.root.join(id);
        let k = family.kind;
        let mut d = if k == "temp" || k == "thermocouple" {
            let text = rd(&dir.join("w1_slave"));
            let Some(ts) = families::parse_therm_slave(&text) else {
                if let Some(m) = family.module
                    && text.is_empty()
                {
                    return Err(format!("no w1_slave attribute (is {m} loaded?)"));
                }
                return Err(if text.is_empty() {
                    "no reading (sensor didn't answer)".into()
                } else {
                    format!("unexpected w1_slave: {}", text.chars().take(60).collect::<String>())
                });
            };
            let mut d = families::decode_therm(fam, &ts);
            match rd(&dir.join("ext_power")).as_str() {
                "0" => d.values.push(benchbob_i2c_chips::Val::new("power", "Power", "parasite")),
                "1" => d.values.push(benchbob_i2c_chips::Val::new("power", "Power", "external")),
                _ => {}
            }
            d
        } else if fam == 0x26 {
            let page0 = rd_bin(&dir.join("page0"));
            let rsens = self.setting(&format!("rsens:{id}")).map(|v| js_number(&v)).filter(|x| *x != 0.0 && !x.is_nan());
            families::decode_ds2438(&rd(&dir.join("temperature")), &rd(&dir.join("vad")), &rd(&dir.join("vdd")), page0.as_deref(), rsens)
        } else if fam == 0x29 {
            let (st, out, act) = (rd_bin(&dir.join("state")), rd_bin(&dir.join("output")), rd_bin(&dir.join("activity")));
            match (st.as_deref(), out.as_deref()) {
                (Some([s, ..]), Some([o, ..])) => families::decode_ds2408(*s, *o, act.and_then(|a| a.first().copied())),
                _ => return Err("couldn't read state/output (is w1_ds2408 loaded?)".into()),
            }
        } else if fam == 0x3a {
            match rd_bin(&dir.join("state")).as_deref() {
                Some([s, ..]) => families::decode_ds2413(*s),
                _ => return Err("couldn't read state (is w1_ds2413 loaded?)".into()),
            }
        } else if fam == 0x12 {
            match rd_bin(&dir.join("state")).as_deref() {
                Some([s, ..]) => families::decode_ds2406(*s),
                _ => return Err("couldn't read state (w1_ds2406 has no autoload alias: modprobe it)".into()),
            }
        } else if fam == 0x1d {
            families::parse_ds2423(&rd(&dir.join("w1_slave")))
        } else if let (true, Some(size)) = (k == "eeprom", family.eeprom_size) {
            let mut d = Decoded::values(vec![benchbob_i2c_chips::Val::new("size", "Size", format!("{size} bytes"))]);
            if eeprom {
                let b = rd_bin(&dir.join("eeprom")).ok_or("couldn't read eeprom")?;
                let v = if b.iter().all(|x| *x == 0xff) { "blank (all 0xFF)".to_string() } else { format!("{} bytes read", b.len()) };
                d.values.push(benchbob_i2c_chips::Val::new("blank", "Contents", v));
                d.eeprom = Some(b);
            }
            d
        } else if k == "rtc" && (fam == 0x24 || fam == 0x27) {
            families::decode_rtc(&self.read_clock(id)?, now_ms() as i64)
        } else if fam == 0x01 {
            Decoded::values(vec![benchbob_i2c_chips::Val::new("present", "On the reader", if present { "yes" } else { "no" })])
        } else if fam == 0x81 {
            Decoded::values(vec![benchbob_i2c_chips::Val::new("adapter", "Identifies", "DS9490R USB 1-Wire adapter")])
        } else {
            Decoded::values(vec![benchbob_i2c_chips::Val::new("family", "Family", format!("0x{fam:02x}"))])
        };
        if rom_ok == Some(false) {
            d.add_warning("ROM ID CRC mismatch: this ID may be bus noise.");
        }
        Ok(d)
    }

    /// DS1904/DS2415/DS2417 have no kernel driver: select + "read clock" (0x66) through `rw`.
    fn read_clock(&self, id: &str) -> Result<Vec<u8>, String> {
        let path = self.root.join(id).join("rw");
        if self.fixture {
            return Ok(rd_bin(&path).unwrap_or_default()); // fixture rw holds the 5-byte reply
        }
        // Write and read in one root shell so the select and the read are back to back.
        let p = path.display().to_string();
        let script = format!("printf '\\146' > '{p}' && head -c 5 '{p}' | od -An -tx1");
        let (code, out, err) = run_sync(&["sudo", "-n", "sh", "-c", &script], Duration::from_secs(5));
        if code != 0 {
            return Err(if err.is_empty() { "clock read failed".into() } else { err });
        }
        Ok(out.split_whitespace().filter_map(|h| u8::from_str_radix(h, 16).ok()).collect())
    }

    /// One background cycle over every present slave. Blocking.
    fn poll_all(&self) {
        let mut by_master: IndexMap<String, Vec<(String, bool, bool)>> = IndexMap::new();
        {
            let st = lock(&self.st);
            for s in st.slaves.values().filter(|s| s.present) {
                by_master.entry(s.master.clone()).or_default().push((s.id.clone(), s.is_therm(), s.family.kind == "rtc"));
            }
        }
        for (mid, list) in by_master {
            let therm = list.iter().filter(|x| x.1).count();
            let bulk = lock(&self.st).masters.get(&mid).is_some_and(|m| m.bulk);
            if bulk && truthy(&self.use_bulk()) && therm > 1 {
                // One simultaneous conversion; w1_slave reads afterwards return the converted values.
                let p = self.root.join(&mid).join("therm_bulk_read");
                match wr(&p, b"trigger\n") {
                    Ok(()) => {
                        if !self.fixture {
                            for _ in 0..20 {
                                std::thread::sleep(Duration::from_millis(100));
                                if rd(&p) != "-1" {
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if let Some(m) = lock(&self.st).masters.get_mut(&mid) {
                            m.error = format!("bulk read: {}", e.message);
                        }
                    }
                }
            }
            for (sid, _, rtc) in list {
                if self.stopped.load(Ordering::SeqCst) {
                    return;
                }
                if rtc {
                    let recent = lock(&self.st).slaves.get(&sid).is_some_and(|s| s.read_at != 0 && now_ms() - s.read_at < 60_000);
                    if recent {
                        continue; // clocks: once a minute
                    }
                }
                let r = self.read_slave(&sid, false);
                if let Some(s) = lock(&self.st).slaves.get_mut(&sid) {
                    match r {
                        Ok(d) => s.record(d),
                        Err(e) => s.error = e,
                    }
                }
                self.changed();
            }
        }
    }

    pub(crate) fn schedule_poll(self: &Arc<Self>, ms: u64) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let g = self.poll_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let me = self.clone();
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            if me.poll_gen.load(Ordering::SeqCst) != g || me.stopped.load(Ordering::SeqCst) {
                return;
            }
            if me.polling.swap(true, Ordering::SeqCst) {
                me.schedule_poll(500);
                return;
            }
            let m2 = me.clone();
            let _ = tokio::task::spawn_blocking(move || m2.poll_all()).await;
            me.polling.store(false, Ordering::SeqCst);
            let iv = me.interval();
            me.schedule_poll(if iv.is_finite() { iv as u64 } else { 10_000 });
        });
    }

    // ---------- views ----------

    fn pins_json(pins: &[OwPin]) -> Json {
        Json::Array(
            pins.iter()
                .map(|p| {
                    let mut m = Map::new();
                    m.insert("name".into(), json!(p.name));
                    m.insert("level".into(), json!(p.level));
                    if let Some(l) = p.latch {
                        m.insert("latch".into(), json!(l));
                    }
                    if let Some(o) = p.out {
                        m.insert("out".into(), json!(o));
                    }
                    Json::Object(m)
                })
                .collect(),
        )
    }

    /// A Decoded as the TypeScript object (`{ values, pins?, warn?, eeprom? }`).
    pub(crate) fn decoded_json(d: &Decoded) -> Json {
        let mut m = Map::new();
        m.insert("values".into(), Json::Array(d.values.iter().map(val_json).collect()));
        if let Some(p) = &d.pins {
            m.insert("pins".into(), Self::pins_json(p));
        }
        if let Some(w) = &d.warn {
            m.insert("warn".into(), json!(w));
        }
        if let Some(e) = &d.eeprom {
            m.insert("eeprom".into(), json!(e));
        }
        Json::Object(m)
    }

    pub(crate) fn slave_view(&self, s: &Slave, labels: &Map<String, Json>) -> Json {
        let hist: Map<String, Json> =
            s.hist.iter().map(|(k, h)| (k.clone(), Json::Array(h.iter().map(|x| benchbob_core::http::num(*x)).collect()))).collect();
        json!({
            "id": s.id,
            "family": format!("0x{:02x}", s.fam),
            "name": s.family.name,
            "kind": s.family.kind,
            "desc": s.family.desc,
            "module": s.family.module,
            "label": labels.get(&s.id).filter(|v| truthy(v)).cloned().unwrap_or(json!("")),
            "master": s.master,
            "present": s.present,
            "romOk": s.rom_ok,
            "firstSeen": s.first_seen,
            "lastSeen": s.last_seen,
            "readAt": s.read_at,
            "values": s.decoded.as_ref().map_or(json!([]), |d| Json::Array(d.values.iter().map(val_json).collect())),
            "pins": s.decoded.as_ref().and_then(|d| d.pins.as_deref()).map_or(Json::Null, Self::pins_json),
            "warn": s.decoded.as_ref().map_or(&[][..], |d| d.warnings()),
            "error": s.error,
            "hist": hist,
            "outputs": matches!(s.fam, 0x29 | 0x3a | 0x12),
            "eepromSize": s.family.eeprom_size.unwrap_or(0),
            "resolution": s.family.kind == "temp" && s.fam != 0x10,
        })
    }

    fn bridges(&self, st: &State) -> Vec<Json> {
        let mut out: Vec<Json> = self
            .i2c
            .as_ref()
            .map(|h| h.ds2482_candidates())
            .unwrap_or_default()
            .into_iter()
            .map(|c| {
                let note = if c.bound {
                    "bound to the ds2482 driver".to_string()
                } else {
                    match &c.driver {
                        Some(d) => format!("claimed by {d}"),
                        None => "found on I²C, not bound".into(),
                    }
                };
                json!({ "type": "DS2482", "bus": c.bus, "address": format!("0x{:x}", c.addr), "bound": c.bound, "note": note })
            })
            .collect();
        if let Some(ctx) = self.ctx.get() {
            let f = |p: &benchbob_core::udev::Props| {
                let g = |k: &str| p.get(k).map(String::as_str);
                g("SUBSYSTEM") == Some("usb")
                    && g("DEVTYPE") == Some("usb_device")
                    && g("ID_VENDOR_ID") == Some("04fa")
                    && g("ID_MODEL_ID") == Some("2490")
            };
            for p in ctx.devices(Some(&f)) {
                let has = st.masters.values().any(|m| m.kind == "ds2490");
                out.push(json!({
                    "type": "DS9490R",
                    "usb": p.get("DEVPATH"),
                    "bound": has,
                    "note": if has { "ds2490 master present" } else { "USB adapter present but no master (is the ds2490 module loaded?)" },
                }));
            }
        }
        out
    }

    pub(crate) fn masters_json(st: &State) -> Vec<Json> {
        st.masters
            .values()
            .map(|m| {
                let text = match m.search {
                    -1 => "continuous".to_string(),
                    0 => "off".to_string(),
                    n => format!("{n} left"),
                };
                json!({ "id": m.id, "kind": m.kind, "label": m.label, "slaves": m.slaves.len(), "search": m.search, "searchText": text, "pullup": m.pullup == "1", "timeout": m.timeout, "ghosts": m.ghosts.len(), "bulk": m.bulk, "error": m.error })
            })
            .collect()
    }

    pub(crate) fn view(&self) -> Json {
        let labels = self.labels();
        let st = lock(&self.st);
        let mut slaves: Vec<&Slave> = st.slaves.values().collect();
        slaves.sort_by(|a, b| b.present.cmp(&a.present).then_with(|| a.id.cmp(&b.id)));
        json!({
            "root": self.root.display().to_string(),
            "fixture": self.fixture,
            "interval": benchbob_core::http::num(self.interval()),
            "bulk": self.use_bulk(),
            "masters": Self::masters_json(&st),
            "slaves": slaves.iter().map(|s| self.slave_view(s, &labels)).collect::<Vec<_>>(),
            "touchLog": st.touch.iter().rev().take(50).map(|t| json!({ "t": t.t, "id": t.id, "event": t.event, "family": t.family })).collect::<Vec<_>>(),
            "bridges": self.bridges(&st),
        })
    }

    pub(crate) fn slave_summary(&self, s: &Slave, labels: &Map<String, Json>) -> Json {
        let mut m = Map::new();
        m.insert("id".into(), json!(s.id));
        m.insert("chip".into(), json!(s.family.name));
        if let Some(l) = labels.get(&s.id).filter(|v| truthy(v)) {
            m.insert("label".into(), l.clone());
        }
        m.insert("present".into(), json!(s.present));
        m.insert("master".into(), json!(s.master));
        let mut readings = Map::new();
        for v in s.decoded.as_ref().map_or(&[][..], |d| &d.values[..]) {
            let val = match (v.unit, &v.v) {
                (Some(u), x) if *x != V::Null => json!(format!("{} {u}", benchbob_i2c::json::v_string(x))),
                (_, x) => benchbob_i2c::json::v_json(x),
            };
            readings.insert(v.label.clone(), val);
        }
        m.insert("readings".into(), Json::Object(readings));
        if let Some(p) = s.decoded.as_ref().and_then(|d| d.pins.as_deref()) {
            m.insert("pins".into(), Self::pins_json(p));
        }
        if let Some(w) = s.decoded.as_ref().map(|d| d.warnings()).filter(|w| !w.is_empty()) {
            m.insert("warnings".into(), json!(w));
        }
        if !s.error.is_empty() {
            m.insert("error".into(), json!(s.error));
        }
        m.insert("readAt".into(), if s.read_at != 0 { json!(benchbob_i2c_chips::time::iso(s.read_at as i64)) } else { Json::Null });
        Json::Object(m)
    }
}

// ---------- driver ----------

#[async_trait]
impl Driver for OneWire {
    fn id(&self) -> &'static str {
        "onewire"
    }
    fn title(&self) -> &'static str {
        "1-Wire"
    }
    fn order(&self) -> i32 {
        41
    }
    fn description(&self) -> &'static str {
        "1-Wire buses (GPIO4, DS2482, DS9490R): DS18B20 and other sensors, I/O switches, iButton touch log"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let _ = self.inner.ctx.set(ctx);
        self.inner.stopped.store(true, Ordering::SeqCst); // suppress events for the initial enumeration
        let i = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            i.load_gpio_pins();
            i.refresh();
        })
        .await?;
        self.inner.stopped.store(false, Ordering::SeqCst);
        // Slaves come and go through udev (see udev() below), so this only catches master
        // attribute changes (search mode, pull-up); the kernel itself only searches the bus
        // every ~10 s.
        let (me, epoch) = (self.inner.clone(), self.inner.epoch.load(Ordering::SeqCst));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                if me.epoch.load(Ordering::SeqCst) != epoch {
                    return;
                }
                let m = me.clone();
                let _ = tokio::task::spawn_blocking(move || m.refresh()).await;
            }
        });
        self.inner.schedule_poll(200);
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.epoch.fetch_add(1, Ordering::SeqCst);
        self.inner.poll_gen.fetch_add(1, Ordering::SeqCst);
        let mut st = lock(&self.inner.st);
        st.masters.clear();
        st.slaves.clear();
    }

    fn udev(&self, ev: &UdevEvent) {
        // Slave added/removed: react immediately (iButton touches).
        if ev.props.get("SUBSYSTEM").map(String::as_str) == Some("w1") {
            let me = self.inner.clone();
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn_blocking(move || me.refresh());
            }
        }
    }

    fn status(&self) -> Option<Json> {
        let i = &self.inner;
        let (where_, present, temps) = {
            let st = lock(&i.st);
            let w: Vec<String> = st.masters.values().map(|m| strip_header_pin(&m.label)).collect();
            let present: Vec<&Slave> = st.slaves.values().filter(|s| s.present).collect();
            let temps: Vec<String> = present
                .iter()
                .flat_map(|s| s.decoded.iter().flat_map(|d| d.values.iter()))
                .filter(|v| v.k == "t")
                .filter_map(|v| v.v.num())
                .map(|t| format!("{} °C", to_fixed1(t)))
                .collect();
            (if w.is_empty() { "no master".to_string() } else { w.join(", ") }, present.len(), temps)
        };
        let summary = if present == 0 {
            format!("{where_} · no devices")
        } else {
            let t = if temps.is_empty() {
                String::new()
            } else {
                format!(": {}", temps.iter().take(4).cloned().collect::<Vec<_>>().join(", "))
            };
            format!("{where_} · {present} device{}{t}", if present > 1 { "s" } else { "" })
        };
        let mut out = Map::new();
        out.insert("active".into(), json!(present > 0));
        out.insert("summary".into(), json!(summary));
        if let Json::Object(v) = i.view() {
            out.extend(v);
        }
        Some(Json::Object(out))
    }

    fn routes(&self) -> Vec<Route> {
        routes::routes(&self.inner)
    }

    fn tools(&self) -> Vec<Tool> {
        routes::tools(&self.inner)
    }
}

/// `label.replace(/ \(header pin \d+\)/, "")`
fn strip_header_pin(label: &str) -> String {
    if let Some(i) = label.find(" (header pin ") {
        let rest = &label[i + " (header pin ".len()..];
        let n = rest.bytes().take_while(|c| c.is_ascii_digit()).count();
        if n > 0 && rest.as_bytes().get(n) == Some(&b')') {
            return format!("{}{}", &label[..i], &rest[n + 1..]);
        }
    }
    label.to_string()
}
