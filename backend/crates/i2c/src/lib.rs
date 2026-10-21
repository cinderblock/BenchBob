//! The I²C panel, ported from `peripherals/drivers/i2c.ts`: user I²C buses (the header's
//! i2c-1, the Pi 4's extra controllers, USB adapters, i2c-stub), an i2cdetect-style scan,
//! identification of what answered, live decoded readings, and raw register tools. Chip
//! knowledge is in `benchbob-i2c-chips`; bus access (ioctls) in [`linux`].
//!
//! Write policy: nothing here writes to a device unless a person or agent asked for it.
//! Auto-polling only calls the read-only decoders; Measure and the per-device auto-measure
//! opt-in are the only paths that may start conversions or wake chips, and raw writes
//! require `confirm: true` over REST. Scans use i2cdetect's own safe defaults.
//!
//! Bus work runs on blocking threads holding the bus's mutex (the TypeScript `withBus`
//! lock), so a multi-step sequence (write a command, sleep, read) is never interleaved with
//! another one on the same bus.

pub mod json;
pub mod linux;
mod routes;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use benchbob_core::http::{HttpError, js_number, js_string, truthy};
use benchbob_core::timeline::now_ms;
use benchbob_core::udev::UdevEvent;
use benchbob_core::{Ctx, Driver, Json, Route, StopReason, Tool};
use benchbob_i2c_chips::io::{errno, func};
use benchbob_i2c_chips::{
    ChipDef, ChipState, Dev, I2cError, I2cIo, ProbeCtx, Readout, V, address_hints, cross_hints, func_names, hex, identify,
};
use serde_json::{Map, json};

use crate::json::{opt_json, readout_json};

const HIST_N: usize = 60;
const SCAN_FIRST: u8 = 0x08;
const SCAN_LAST: u8 = 0x77;

/// Chips where register pointers are commands (a dump or blind register read would execute them).
const POINTER_UNSAFE: [&str; 11] =
    ["expander", "bh1750", "aht2x", "sht3x", "sht4x", "htu21d", "scd4x", "mcp4725", "tca9548", "ssd1306", "ds2482"];

pub type BoxIo = Box<dyn I2cIo + Send>;
type Opener = Box<dyn Fn(u32) -> Result<BoxIo, String> + Send + Sync>;

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// What the probe of one address found, for the views (the full probe is in `cs`).
#[derive(Clone, Default)]
pub(crate) struct ProbeView {
    variant: Option<String>,
    confidence: Option<&'static str>,
    note: Option<String>,
}

pub(crate) struct DevState {
    pub addr: u8,
    /// Kernel driver bound to it (i2cdetect "UU").
    pub claimed: Option<String>,
    pub chip: Option<&'static ChipDef>,
    pub cs: Option<Arc<Mutex<ChipState>>>,
    pub probe: ProbeView,
    pub readout: Option<Readout>,
    pub read_at: u64,
    pub error: String,
    pub hist: Vec<(String, Vec<f64>)>,
    pub busy: bool,
    pub next: u64,
    pub first_seen: u64,
    pub hint: String,
}

impl DevState {
    fn new(addr: u8, claimed: Option<String>) -> Self {
        Self {
            addr,
            claimed,
            chip: None,
            cs: None,
            probe: ProbeView::default(),
            readout: None,
            read_at: 0,
            error: String::new(),
            hist: Vec::new(),
            busy: false,
            next: 0,
            first_seen: now_ms(),
            hint: String::new(),
        }
    }

    /// `cs?.probe.variant || chip?.name || ""`
    pub fn chip_name(&self) -> String {
        self.probe.variant.clone().filter(|v| !v.is_empty()).or_else(|| self.chip.map(|c| c.name.to_string())).unwrap_or_default()
    }

    fn pollable(&self) -> bool {
        self.chip.is_some_and(ChipDef::pollable)
    }

    fn record(&mut self, r: Readout) {
        self.read_at = now_ms();
        self.error.clear();
        let stale = r.stale == Some(true);
        if !stale {
            for v in &r.values {
                let V::Num(n) = v.v else { continue };
                let h = match self.hist.iter_mut().find(|(k, _)| *k == v.k) {
                    Some((_, h)) => h,
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
        }
        self.readout = Some(r);
    }
}

#[derive(Default)]
pub(crate) struct BusSt {
    pub error: String,
    pub scanning: bool,
    pub last_scan: u64,
    pub scan_ms: u64,
    pub devices: BTreeMap<u8, DevState>,
    /// None: only on demand.
    pub next_scan: Option<u64>,
}

pub(crate) struct Bus {
    pub bus: u32,
    /// Adapter name from sysfs.
    pub name: String,
    pub label: String,
    /// "header" | "usb" | "stub" | "ddc" | "other"
    pub kind: &'static str,
    /// Stable id for settings.
    pub key: String,
    /// clock-frequency from the device tree (header controllers).
    pub hz: Option<u32>,
    pub funcs: u32,
    pub io: Option<Mutex<BoxIo>>,
    pub st: Mutex<BusSt>,
}

pub(crate) struct Inner {
    pub ctx: OnceLock<Ctx>,
    /// "/sys", or a fixture tree in tests.
    pub sys: PathBuf,
    pub buses: Mutex<BTreeMap<u32, Arc<Bus>>>,
    pub running: AtomicBool,
    pub epoch: AtomicU64,
    pub udev_gen: AtomicU64,
    pub open: Opener,
}

/// The I²C driver.
pub struct I2c {
    inner: Arc<Inner>,
}

impl Default for I2c {
    fn default() -> Self {
        Self::new()
    }
}

impl I2c {
    pub fn new() -> Self {
        Self::with(PathBuf::from("/sys"), Box::new(|n| linux::LinuxI2cBus::open(n).map(|b| Box::new(b) as BoxIo)))
    }

    pub(crate) fn with(sys: PathBuf, open: Opener) -> Self {
        Self {
            inner: Arc::new(Inner {
                ctx: OnceLock::new(),
                sys,
                buses: Mutex::new(BTreeMap::new()),
                running: AtomicBool::new(false),
                epoch: AtomicU64::new(0),
                udev_gen: AtomicU64::new(0),
                open,
            }),
        }
    }

    /// What the 1-Wire driver uses: DS2482 bridges and rescans.
    pub fn handle(&self) -> I2cHandle {
        I2cHandle(self.inner.clone())
    }
}

/// A DS2482 bridge seen on an I²C bus, bound or not.
#[derive(Debug, Clone, PartialEq)]
pub struct Ds2482 {
    pub bus: u32,
    pub addr: u8,
    pub bound: bool,
    pub driver: Option<String>,
}

/// The I²C driver as other drivers see it (onewire.ts imports these from i2c.ts).
#[derive(Clone)]
pub struct I2cHandle(Arc<Inner>);

impl I2cHandle {
    /// DS2482 bridges seen on any bus, bound or not.
    pub fn ds2482_candidates(&self) -> Vec<Ds2482> {
        let mut out = Vec::new();
        for b in lock(&self.0.buses).values() {
            for d in lock(&b.st).devices.values() {
                let chip = d.chip.is_some_and(|c| c.id == "ds2482");
                let bound = d.claimed.as_deref() == Some("ds2482");
                if chip || bound {
                    out.push(Ds2482 { bus: b.bus, addr: d.addr, bound, driver: d.claimed.clone() });
                }
            }
        }
        out
    }

    /// Force a rescan of a bus (after binding/unbinding a bridge).
    pub async fn rescan(&self, bus: u32) {
        let b = lock(&self.0.buses).get(&bus).cloned();
        if let Some(b) = b {
            self.0.scan(&b, false, false).await;
        }
    }
}

// ---------- bus discovery ----------

fn re(p: &str) -> regex::Regex {
    regex::Regex::new(p).unwrap()
}

/// Classify an adapter from its sysfs name, device path and device-tree compatible.
pub fn classify(name: &str, path: &str, compatible: &str) -> (&'static str, String) {
    static RES: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    let r = RES.get_or_init(|| {
        [
            r"hdmi-i2c|brcmstb-i2c",
            r"(?i)ddc|hdmi|gmbus|dpddc",
            r"i2c@7e205([68ac])00",
            r"(^|\s)i2c-gpio(\s|$)",
            r"i2c0mux|i2c@7e805000",
            r"(?i)SMBus stub",
            r"(?i)CP2112",
            r"(?i)MCP2221",
            r"(?i)tiny-usb",
            r"(?i)CH341",
            r"/usb\d",
        ]
        .iter()
        .map(|p| re(p))
        .collect()
    });
    if r[0].is_match(compatible) || r[1].is_match(name) {
        return ("ddc", "HDMI DDC (display EDID)".into());
    }
    if name.contains("i2c@7e804000") {
        return ("header", "Header pins 3/5 (GPIO2/3)".into());
    }
    // Pi 4 extra controllers (dtoverlay=i2c3…i2c6); which pin pair is in config.txt (GPIO → Interfaces).
    if let Some(c) = r[2].captures(name) {
        let n = match &c[1] {
            "6" => 3,
            "8" => 4,
            "a" => 5,
            _ => 6,
        };
        return ("header", format!("Header I²C{n} (dtoverlay=i2c{n})"));
    }
    if r[3].is_match(compatible) {
        return ("header", "Header software I²C (i2c-gpio)".into());
    }
    if name.contains("i2c@7e205000") {
        return ("other", "i2c0: GPIO0/1 (HAT ID EEPROM, header pins 27/28) or camera".into());
    }
    if r[4].is_match(name) {
        return ("other", "Camera / display (CSI/DSI)".into());
    }
    if r[5].is_match(name) {
        return ("stub", "i2c-stub (simulated chips)".into());
    }
    for (i, l) in [(6, "Silicon Labs CP2112"), (7, "Microchip MCP2221"), (8, "i2c-tiny-usb"), (9, "WCH CH341")] {
        if r[i].is_match(name) {
            return ("usb", format!("USB: {l}"));
        }
    }
    if r[10].is_match(path) {
        return ("usb", format!("USB: {name}"));
    }
    ("other", name.to_string())
}

impl Inner {
    pub(crate) fn ctx(&self) -> &Ctx {
        self.ctx.get().expect("i2c driver used before init")
    }

    fn changed(&self) {
        if let Some(c) = self.ctx.get() {
            c.changed();
        }
    }

    fn event(&self, text: &str) {
        if let Some(c) = self.ctx.get() {
            c.event(text, None);
        }
    }

    pub(crate) fn dev_key(b: &Bus, addr: u8) -> String {
        format!("dev:{}@{}", b.key, hex(addr as u32))
    }

    /// The device's saved settings object (`{}` when none).
    pub(crate) fn dev_cfg(&self, b: &Bus, addr: u8) -> Map<String, Json> {
        self.ctx.get().and_then(|c| c.setting(&Self::dev_key(b, addr))).and_then(|v| v.as_object().cloned()).unwrap_or_default()
    }

    pub(crate) fn set_dev_cfg(&self, b: &Bus, addr: u8, patch: Map<String, Json>) {
        let mut cfg = self.dev_cfg(b, addr);
        for (k, v) in patch {
            cfg.insert(k, v);
        }
        self.ctx().set_setting(&Self::dev_key(b, addr), Json::Object(cfg));
    }

    pub(crate) fn bus_cfg(&self, b: &Bus) -> Map<String, Json> {
        self.ctx.get().and_then(|c| c.setting(&format!("bus:{}", b.key))).and_then(|v| v.as_object().cloned()).unwrap_or_default()
    }

    /// `busCfg(b).autoScan ?? (ddc ? 0 : 30)`, as stored.
    pub(crate) fn auto_scan(&self, b: &Bus) -> Json {
        self.bus_cfg(b).get("autoScan").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!(if b.kind == "ddc" { 0 } else { 30 }))
    }

    pub(crate) fn next_scan_after(&self, b: &Bus, from: u64) -> Option<u64> {
        let s = js_number(&self.auto_scan(b));
        (s > 0.0).then(|| from + (s * 1000.0) as u64)
    }

    /// The chip options as numbers (`Number(opts[k])`), for the decoders.
    fn dev_opts(&self, b: &Bus, addr: u8) -> BTreeMap<String, f64> {
        self.dev_cfg(b, addr)
            .get("opts")
            .and_then(Json::as_object)
            .map(|o| o.iter().map(|(k, v)| (k.clone(), js_number(v))).collect())
            .unwrap_or_default()
    }

    fn read_text(&self, rel: &str) -> String {
        benchbob_core::run::read_text(self.sys.join(rel))
    }

    async fn refresh_buses(self: &Arc<Self>) {
        let names: Vec<String> = std::fs::read_dir(self.sys.join("class/i2c-dev"))
            .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        let mut seen = Vec::new();
        for n in names {
            let Some(bus) = n.strip_prefix("i2c-").and_then(|x| x.parse::<u32>().ok()) else { continue };
            seen.push(bus);
            if lock(&self.buses).contains_key(&bus) {
                continue;
            }
            let base = format!("class/i2c-dev/{n}");
            let name = self.read_text(&format!("{base}/name"));
            let sys_path = std::fs::canonicalize(self.sys.join(&base).join("device")).map(|p| p.display().to_string()).unwrap_or_default();
            let compatible = self.read_text(&format!("{base}/device/of_node/compatible")).replace('\0', " ");
            let (kind, label) = classify(&name, &sys_path, &compatible);
            let clk = std::fs::read(self.sys.join(&base).join("device/of_node/clock-frequency")).unwrap_or_default();
            let hz = (clk.len() >= 4).then(|| u32::from_be_bytes([clk[0], clk[1], clk[2], clk[3]]));
            // Stable settings key: USB serial / port path, else the platform device, else the name.
            let udev_path = format!("{}/i2c-dev/{n}", sys_path.strip_prefix("/sys").unwrap_or(&sys_path));
            let usb = self.ctx.get().and_then(|c| c.ancestor(&udev_path, "usb", Some("usb_device")));
            let key = if kind == "stub" {
                "stub".to_string()
            } else if let Some(u) = usb {
                let pick = ["ID_SERIAL", "ID_PATH", "DEVPATH"]
                    .iter()
                    .find_map(|k| u.get(*k).filter(|v| !v.is_empty()).cloned())
                    .unwrap_or_default();
                format!("usb:{pick}")
            } else {
                let parts: Vec<&str> = sys_path.split('/').collect();
                parts
                    .len()
                    .checked_sub(2)
                    .and_then(|i| parts.get(i))
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| name.clone())
            };
            let (io, error) = match (self.open)(bus) {
                Ok(io) => (Some(io), String::new()),
                Err(e) => (None, e),
            };
            let funcs = io.as_ref().map_or(0, |i| i.funcs());
            let usable = io.is_some();
            let b = Arc::new(Bus {
                bus,
                name,
                label: label.clone(),
                kind,
                key,
                hz,
                funcs,
                io: io.map(Mutex::new),
                st: Mutex::new(BusSt { error, ..Default::default() }),
            });
            if kind != "ddc" && usable {
                lock(&b.st).next_scan = Some(now_ms()); // scan on the next tick
            }
            lock(&self.buses).insert(bus, b);
            if self.running.load(Ordering::SeqCst) && kind != "ddc" {
                self.event(&format!("I²C bus i2c-{bus} appeared: {label}"));
            }
        }
        let gone: Vec<Arc<Bus>> = {
            let mut buses = lock(&self.buses);
            let ids: Vec<u32> = buses.keys().filter(|k| !seen.contains(k)).copied().collect();
            ids.iter().filter_map(|k| buses.remove(k)).collect()
        };
        for b in gone {
            if b.kind != "ddc" {
                self.event(&format!("I²C bus i2c-{} went away ({})", b.bus, b.label));
            }
        }
        self.changed();
    }

    // ---------- scan + identify ----------

    /// Kernel driver bound to bus-addr, if any (i2cdetect's "UU").
    fn bound_driver(&self, bus: u32, addr: u8) -> Option<String> {
        let p = self.sys.join(format!("bus/i2c/devices/{bus}-{addr:04x}/driver"));
        let l = std::fs::read_link(p).ok()?;
        Some(l.file_name().map(|n| n.to_string_lossy().into_owned()).filter(|s| !s.is_empty()).unwrap_or_else(|| "bound".into()))
    }

    pub(crate) async fn scan(self: &Arc<Self>, b: &Arc<Bus>, deep: bool, reidentify: bool) {
        if b.io.is_none() {
            return;
        }
        {
            let mut st = lock(&b.st);
            if st.scanning {
                return;
            }
            st.scanning = true;
        }
        self.changed();
        let t0 = now_ms();
        let (me, bus) = (self.clone(), b.clone());
        let r = tokio::task::spawn_blocking(move || me.scan_blocking(&bus, deep, reidentify)).await;
        let now = now_ms();
        let next = self.next_scan_after(b, now);
        {
            let mut st = lock(&b.st);
            st.error = match r {
                Ok(Ok(())) => String::new(),
                Ok(Err(e)) => e,
                Err(e) => format!("scan failed: {e}"),
            };
            st.scanning = false;
            st.last_scan = now;
            st.scan_ms = now - t0;
            st.next_scan = next;
        }
        self.changed();
    }

    fn scan_blocking(&self, b: &Bus, deep: bool, reidentify: bool) -> Result<(), String> {
        let mut io = lock(b.io.as_ref().unwrap());
        let io: &mut dyn I2cIo = &mut **io;
        let mut present: BTreeMap<u8, Option<String>> = BTreeMap::new();
        let mut faults = 0;
        for a in SCAN_FIRST..=SCAN_LAST {
            if let Some(drv) = self.bound_driver(b.bus, a) {
                present.insert(a, Some(drv));
                continue;
            }
            match probe_addr(io, a) {
                Ok(true) => {
                    present.insert(a, None);
                }
                Ok(false) => {}
                Err(e) if e.errno == errno::EBUSY => {
                    present.insert(a, Some("kernel".into()));
                }
                Err(e) => {
                    faults += 1;
                    if faults >= 3 {
                        return Err(format!("bus fault: {} (is SDA or SCL held low?)", e.message()));
                    }
                }
            }
        }
        // Departures, and what to identify (arrivals, released addresses, re-identification).
        struct Plan {
            addr: u8,
            drv: Option<String>,
            fresh: bool,
            identify: bool,
            chip: Option<&'static str>,
        }
        let plans: Vec<Plan> = {
            let mut st = lock(&b.st);
            let gone: Vec<u8> = st.devices.keys().filter(|a| !present.contains_key(a)).copied().collect();
            for a in gone {
                let d = st.devices.remove(&a).unwrap();
                let n = d.chip_name();
                self.event(&format!("I²C i2c-{} {}: {} no longer answers", b.bus, hex(a as u32), if n.is_empty() { "device" } else { &n }));
            }
            present
                .iter()
                .map(|(a, drv)| {
                    let d = st.devices.get(a);
                    let fresh = d.is_none();
                    // A driver released the address (e.g. ds2482 unbound): identify what's really there.
                    let released = d.is_some_and(|d| d.claimed.is_some()) && drv.is_none();
                    let identify = drv.is_none() && (fresh || released || reidentify || deep);
                    Plan { addr: *a, drv: drv.clone(), fresh, identify, chip: d.and_then(|d| d.chip).map(|c| c.id) }
                })
                .collect()
        };
        // Identify in ascending address order, so cross-address probes (PCA9685 All Call)
        // can see what's below them.
        let mut found: BTreeMap<u8, &'static str> = BTreeMap::new();
        let mut results = Vec::new();
        for p in &plans {
            let res = if p.identify {
                let ctx = ProbeCtx { found: found.clone(), deep };
                let id = identify(io, p.addr, &ctx).map_err(|e| e.message())?;
                Some(id)
            } else {
                None
            };
            let chip = match &res {
                Some(id) => id.as_ref().map(|i| i.chip.id),
                None => p.chip,
            };
            if let Some(c) = chip {
                found.insert(p.addr, c);
            }
            results.push(res);
        }
        let had_scan = lock(&b.st).last_scan != 0;
        let opts: Vec<BTreeMap<String, f64>> = plans.iter().map(|p| self.dev_opts(b, p.addr)).collect();
        let mut st = lock(&b.st);
        for ((p, res), opts) in plans.into_iter().zip(results).zip(opts) {
            let d = st.devices.entry(p.addr).or_insert_with(|| DevState::new(p.addr, p.drv.clone()));
            d.claimed = p.drv.clone();
            if let Some(id) = res {
                d.chip = id.as_ref().map(|i| i.chip);
                d.probe = id
                    .as_ref()
                    .map(|i| ProbeView {
                        variant: i.probe.variant.clone(),
                        confidence: Some(i.probe.confidence.as_str()),
                        note: i.probe.note.clone(),
                    })
                    .unwrap_or_default();
                d.cs = id.map(|i| {
                    let mut cs = ChipState::new(i.probe);
                    cs.opts = opts;
                    Arc::new(Mutex::new(cs))
                });
                d.readout = None;
                d.hist.clear();
                d.next = 0;
            }
            if p.fresh && had_scan {
                let what = match &p.drv {
                    Some(drv) => format!("claimed by {drv}"),
                    None => Some(d.chip_name()).filter(|n| !n.is_empty()).unwrap_or_else(|| "unknown device".into()),
                };
                self.event(&format!("I²C i2c-{} {}: {what} appeared", b.bus, hex(p.addr as u32)));
            }
        }
        let hints = cross_hints(&found);
        for d in st.devices.values_mut() {
            d.hint = hints.get(&d.addr).map(|h| h.to_string()).unwrap_or_default();
        }
        Ok(())
    }

    // ---------- reading ----------

    /// Run a closure on the bus's blocking thread with the bus locked.
    pub(crate) async fn with_bus<T: Send + 'static>(
        b: &Arc<Bus>,
        f: impl FnOnce(&mut dyn I2cIo) -> T + Send + 'static,
    ) -> Result<T, HttpError> {
        let bus = b.clone();
        tokio::task::spawn_blocking(move || {
            let mut io = lock(bus.io.as_ref().expect("bus is open"));
            f(&mut **io)
        })
        .await
        .map_err(|e| HttpError::new(500, format!("I²C worker failed: {e}")))
    }

    pub(crate) async fn read_dev(self: &Arc<Self>, b: &Arc<Bus>, addr: u8, measure: bool) -> Result<Json, HttpError> {
        let (f, cs, name) = {
            let mut st = lock(&b.st);
            let d = st
                .devices
                .get_mut(&addr)
                .ok_or_else(|| HttpError::new(404, format!("Nothing found at {} on i2c-{} in the last scan", hex(addr as u32), b.bus)))?;
            let (Some(chip), Some(cs)) = (d.chip, d.cs.clone()) else {
                return Err(HttpError::new(409, format!("{} isn't identified; use the raw tools", hex(addr as u32))));
            };
            let f = if measure { chip.measure } else { chip.read };
            let Some(f) = f else {
                return Err(HttpError::new(
                    409,
                    format!("{} has no {}", d.chip_name(), if measure { "measure operation" } else { "read-only decoder" }),
                ));
            };
            d.busy = true;
            (f, cs, d.chip_name())
        };
        let opts = self.dev_opts(b, addr);
        let r = Self::with_bus(b, move |io| {
            let mut cs = lock(&cs);
            cs.opts = opts;
            f(&mut Dev::new(io, addr), &mut cs)
        })
        .await;
        let out = {
            let mut st = lock(&b.st);
            let d = st.devices.get_mut(&addr);
            match r {
                Ok(Ok(r)) => {
                    let j = readout_json(&r);
                    if let Some(d) = d {
                        d.busy = false;
                        d.record(r);
                    }
                    Ok(j)
                }
                Ok(Err(e)) => {
                    if let Some(d) = d {
                        d.busy = false;
                        d.error = e.message();
                    }
                    Err(HttpError::new(502, format!("{name} {}: {}", hex(addr as u32), e.message())))
                }
                Err(e) => {
                    if let Some(d) = d {
                        d.busy = false;
                        d.error = e.message.clone();
                    }
                    Err(e)
                }
            }
        };
        self.changed();
        out
    }

    /// 250 ms housekeeping: due scans and due polls. A busy device is skipped.
    fn tick(self: &Arc<Self>) {
        let now = now_ms();
        let buses: Vec<Arc<Bus>> = lock(&self.buses).values().cloned().collect();
        for b in buses {
            if b.io.is_none() || b.kind == "ddc" {
                continue;
            }
            let due_scan = {
                let st = lock(&b.st);
                st.next_scan.is_some_and(|t| now >= t) && !st.scanning
            };
            if due_scan {
                let (me, bus) = (self.clone(), b.clone());
                tokio::spawn(async move { me.scan(&bus, false, false).await });
            }
            let addrs: Vec<u8> = {
                let st = lock(&b.st);
                st.devices
                    .values()
                    .filter(|d| !d.busy && d.chip.is_some() && d.claimed.is_none() && now >= d.next)
                    .map(|d| d.addr)
                    .collect()
            };
            for a in addrs {
                let cfg = self.dev_cfg(&b, a);
                let mut st = lock(&b.st);
                let Some(d) = st.devices.get_mut(&a) else { continue };
                let chip = d.chip.unwrap();
                let auto = cfg.get("auto").is_some_and(truthy) && chip.measure.is_some();
                let poll = cfg.get("poll").filter(|v| !v.is_null()).is_none_or(truthy) && d.pollable();
                if !auto && !poll {
                    continue;
                }
                let iv = cfg.get("interval").filter(|v| !v.is_null()).map(js_number).unwrap_or(2000.0).max(250.0);
                d.next = now + iv as u64;
                drop(st);
                let (me, bus) = (self.clone(), b.clone());
                tokio::spawn(async move {
                    let _ = me.read_dev(&bus, a, auto).await;
                });
            }
        }
    }

    fn start_tick(self: &Arc<Self>) {
        let me = self.clone();
        let epoch = self.epoch.load(Ordering::SeqCst);
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_millis(250));
            loop {
                t.tick().await;
                if me.epoch.load(Ordering::SeqCst) != epoch {
                    return;
                }
                me.tick();
            }
        });
    }

    // ---------- views ----------

    pub(crate) fn dev_view(&self, b: &Bus, d: &DevState) -> Json {
        let cfg = self.dev_cfg(b, d.addr);
        let c = d.chip;
        let name = d.chip_name();
        let label = cfg.get("label").filter(|v| truthy(v)).cloned().unwrap_or_else(|| json!(""));
        let poll_cfg = cfg.get("poll").filter(|v| !v.is_null()).is_none_or(truthy);
        let cfg_opts = cfg.get("opts").and_then(Json::as_object);
        let opts: Vec<Json> = c
            .map(|c| c.opts)
            .unwrap_or(&[])
            .iter()
            .map(|o| {
                opt_json(
                    o,
                    cfg_opts
                        .and_then(|m| m.get(o.key))
                        .filter(|v| !v.is_null())
                        .cloned()
                        .unwrap_or_else(|| benchbob_core::http::num(o.default)),
                )
            })
            .collect();
        let hist: Map<String, Json> =
            d.hist.iter().map(|(k, h)| (k.clone(), Json::Array(h.iter().map(|x| benchbob_core::http::num(*x)).collect()))).collect();
        json!({
            "addr": d.addr,
            "hex": hex(d.addr as u32),
            "claimed": d.claimed,
            "chip": c.map(|c| c.id),
            "name": if name.is_empty() { Json::Null } else { json!(name) },
            "what": c.map_or("", |c| c.what),
            "confidence": d.probe.confidence,
            "note": d.probe.note.clone().unwrap_or_default(),
            "hint": d.hint,
            "hints": if c.is_some() { vec![] } else { address_hints(d.addr) },
            "label": label,
            "readout": d.readout.as_ref().map(readout_json),
            "readAt": d.read_at,
            "error": d.error,
            "canRead": c.is_some_and(|c| c.read.is_some()),
            "canMeasure": c.is_some_and(|c| c.measure.is_some()),
            "measureNote": c.map_or("", |c| c.measure_note),
            "pollable": d.pollable(),
            "poll": poll_cfg && d.pollable(),
            "auto": cfg.get("auto").is_some_and(truthy) && c.is_some_and(|c| c.measure.is_some()),
            "interval": cfg.get("interval").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!(2000)),
            "opts": opts,
            "pointerUnsafe": c.is_none_or(|c| POINTER_UNSAFE.contains(&c.id)),
            "hist": hist,
            "firstSeen": d.first_seen,
        })
    }

    pub(crate) fn bus_view(&self, b: &Bus) -> Json {
        let st = lock(&b.st);
        json!({
            "bus": b.bus,
            "dev": format!("/dev/i2c-{}", b.bus),
            "name": b.name,
            "label": b.label,
            "kind": b.kind,
            "hz": b.hz,
            "funcs": func_names(b.funcs),
            "plainI2c": b.funcs & func::I2C != 0,
            "error": st.error,
            "scanning": st.scanning,
            "lastScan": st.last_scan,
            "scanMs": st.scan_ms,
            "autoScan": self.auto_scan(b),
            "devices": st.devices.values().map(|d| self.dev_view(b, d)).collect::<Vec<_>>(),
        })
    }

    pub(crate) fn user_buses(&self) -> Vec<Arc<Bus>> {
        lock(&self.buses).values().filter(|b| b.kind != "ddc").cloned().collect()
    }

    pub(crate) fn all_buses(&self) -> Vec<Arc<Bus>> {
        lock(&self.buses).values().cloned().collect()
    }

    /// i2c.ts `parseBus`: a bus number, "1" or "i2c-1", that exists and is open.
    pub(crate) fn parse_bus(&self, s: Option<&Json>) -> Result<Arc<Bus>, HttpError> {
        let text = match s {
            None | Some(Json::Null) => String::new(),
            Some(v) => js_string(v),
        };
        let n = json::parse_int_radix(text.strip_prefix("i2c-").unwrap_or(&text), 10);
        let b = n.and_then(|n| u32::try_from(n).ok()).and_then(|n| lock(&self.buses).get(&n).cloned());
        let Some(b) = b else {
            return Err(HttpError::new(
                404,
                format!("No I²C bus \"{}\". See GET /api/i2c (buses are numbered like the /dev/i2c-N nodes).", json::arg_string(s)),
            ));
        };
        if b.io.is_none() {
            let e = lock(&b.st).error.clone();
            return Err(HttpError::new(409, format!("i2c-{} is not usable: {}", b.bus, if e.is_empty() { "not open" } else { &e })));
        }
        Ok(b)
    }
}

/// i2cdetect's auto mode: read-byte where quick-write can corrupt EEPROMs, quick-write
/// elsewhere. Like i2cdetect, any error means "nobody there": adapters disagree on the errno
/// for a missing ACK (bcm2835: EREMOTEIO for quick, EIO for read-byte; i2c-stub: ENODEV).
/// Only EBUSY (kernel driver) and timeouts / lost arbitration (a stuck bus) are passed up.
pub fn probe_addr(io: &mut dyn I2cIo, addr: u8) -> Result<bool, I2cError> {
    let read_first = (0x30..=0x37).contains(&addr) || (0x50..=0x5f).contains(&addr);
    let can_quick = io.funcs() & func::QUICK != 0;
    let can_recv = io.funcs() & func::READ_BYTE != 0;
    let r = if (read_first && can_recv) || !can_quick { io.receive(addr, false).map(|_| ()) } else { io.quick(addr, false, false) };
    match r {
        Ok(()) => Ok(true),
        Err(e) if matches!(e.errno, errno::EBUSY | errno::ETIMEDOUT | errno::EAGAIN) => Err(e),
        Err(_) => Ok(false),
    }
}

// ---------- driver ----------

#[async_trait]
impl Driver for I2c {
    fn id(&self) -> &'static str {
        "i2c"
    }
    fn title(&self) -> &'static str {
        "I²C"
    }
    fn order(&self) -> i32 {
        40
    }
    fn description(&self) -> &'static str {
        "I²C buses (header pins 3/5, USB adapters): scan, identify sensors, live readings, raw register tools"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let _ = self.inner.ctx.set(ctx);
        self.inner.running.store(false, Ordering::SeqCst);
        self.inner.refresh_buses().await;
        self.inner.running.store(true, Ordering::SeqCst);
        self.inner.start_tick();
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) {
        self.inner.running.store(false, Ordering::SeqCst);
        self.inner.epoch.fetch_add(1, Ordering::SeqCst);
        self.inner.udev_gen.fetch_add(1, Ordering::SeqCst);
        // Dropping a bus closes its file descriptor (once any worker holding it is done).
        lock(&self.inner.buses).clear();
    }

    fn udev(&self, ev: &UdevEvent) {
        let sub = ev.props.get("SUBSYSTEM").map(String::as_str).unwrap_or("");
        if sub != "i2c-dev" && sub != "i2c" {
            return;
        }
        // A new adapter (USB, stub) or a client bound/unbound (changes UU): refresh soon.
        let gen_ = self.inner.udev_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let me = self.inner.clone();
        let client = sub == "i2c";
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if me.udev_gen.load(Ordering::SeqCst) != gen_ || !me.running.load(Ordering::SeqCst) {
                return;
            }
            me.refresh_buses().await;
            if client {
                let now = now_ms();
                for b in me.user_buses() {
                    lock(&b.st).next_scan = Some(now);
                }
            }
        });
    }

    fn status(&self) -> Option<Json> {
        let i = &self.inner;
        let list = i.user_buses();
        let mut names = Vec::new();
        for b in &list {
            for d in lock(&b.st).devices.values() {
                names.push(match &d.claimed {
                    Some(c) => format!("{} ({c})", hex(d.addr as u32)),
                    None => Some(d.chip_name()).filter(|n| !n.is_empty()).unwrap_or_else(|| hex(d.addr as u32)),
                });
            }
        }
        let bus_names = if list.is_empty() {
            "no buses".to_string()
        } else {
            list.iter().map(|b| format!("i2c-{}", b.bus)).collect::<Vec<_>>().join(", ")
        };
        let summary = if names.is_empty() {
            format!("{bus_names} · no devices")
        } else {
            let more = if names.len() > 4 { format!(" +{}", names.len() - 4) } else { String::new() };
            format!("{bus_names} · {}{more}", names.iter().take(4).cloned().collect::<Vec<_>>().join(", "))
        };
        Some(json!({
            "active": !names.is_empty(),
            "summary": summary,
            "buses": list.iter().map(|b| i.bus_view(b)).collect::<Vec<_>>(),
            "hiddenBuses": i.all_buses().iter().filter(|b| b.kind == "ddc").map(|b| format!("i2c-{}", b.bus)).collect::<Vec<_>>(),
        }))
    }

    fn routes(&self) -> Vec<Route> {
        routes::routes(&self.inner)
    }

    fn tools(&self) -> Vec<Tool> {
        routes::tools(&self.inner)
    }
}
