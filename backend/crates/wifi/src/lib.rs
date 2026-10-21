//! The Wi-Fi panel (peripherals/drivers/wifi.ts).
//!
//! Radios (every phy: driver, bus, bands/channels under the current regulatory domain,
//! iftypes, what it's doing right now), a scanner with per-BSS history, and client tests that
//! borrow a radio into an isolated network namespace ([`client`]).
//!
//! The onboard brcmfmac wlan0 on desky is NetworkManager's backup uplink on the same /20 as
//! eth0, so the rules here are conservative: scans never change its connection, a test only
//! borrows it when the main default route is via something else that's up, and the runner
//! always gives it back and checks NM reconnected it. USB dongles show up as new phys; when
//! BenchBob uses one it tells NM (runtime only) to leave it alone, so NM's autoconnect
//! doesn't grab it mid-test.
//!
//! Status stays small: the bulky lists (radio details, BSS table, results) are fetched over
//! REST when their sequence numbers change. The running test's timeline rides in status so
//! progress streams to the dashboard.

pub mod client;
pub mod json;
pub mod parse;
mod routes;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use benchbob_core::run::{Opts, run};
use benchbob_core::udev::{Props, UdevEvent};
use benchbob_core::{Ctx, Driver, HttpError, Json, Route, StopReason, Tool};
use indexmap::{IndexMap, IndexSet};
use serde_json::{Map, Value};

use crate::client::{Bins, ClientTest, Deps, RadioCtx, TestSpec, sudo};
use crate::json::{O, js_number, js_string, now_ms, num, spread, str_or_empty, truthy};
use crate::parse::{self as p, Bss, IwIface, PhyInfo, RegDomain, Survey};

pub(crate) const RESULTS_MAX: usize = 20;
/// 1 h at the 15 s minimum interval; trimmed to BSS_MAX_AGE_MS too
pub(crate) const HISTORY_MAX: usize = 360;
pub(crate) const BSS_MAX_AGE_MS: f64 = 60.0 * 60.0 * 1000.0;
/// Viewers with the panel open hold a lease this long; scans run while any is fresh.
pub(crate) const WATCH_LEASE_MS: u64 = 60_000;

/// A spawned task, aborted when this handle is dropped.
pub(crate) struct Task(tokio::task::JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Usb {
    vendor: String,
    model: String,
    id: String,
    port: String,
}

#[derive(Debug, Clone)]
pub(crate) struct NmRow {
    iface: String,
    state: String,
    connection: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Radio {
    pub phy: String,
    stable_id: String,
    driver: String,
    /// usb, sdio, pci, platform, virtual, unknown
    bus: &'static str,
    onboard: bool,
    usb: Option<Usb>,
    pub ifaces: Vec<IwIface>,
    pub info: Option<PhyInfo>,
    nm: Vec<NmRow>,
    /// (primary | backup | connected | ap | idle | down | test | scan, text)
    use_: (&'static str, String),
    unmanaged_by_us: Vec<String>,
    /// "test" or "scan"
    pub busy: Option<&'static str>,
    prefs: Map<String, Value>,
}

impl Radio {
    fn pref_str(&self, k: &str) -> Option<String> {
        self.prefs.get(k).and_then(Value::as_str).map(String::from)
    }
    fn keep_unmanaged(&self) -> bool {
        self.prefs.get("keepUnmanaged").is_some_and(truthy)
    }
    fn names(&self) -> Vec<String> {
        self.ifaces.iter().filter_map(|i| i.name.clone()).collect()
    }
}

pub(crate) fn managed_iface(r: &Radio) -> Option<String> {
    r.ifaces
        .iter()
        .find(|i| i.name.is_some() && i.r#type.as_deref() == Some("managed"))
        .or_else(|| r.ifaces.iter().find(|i| i.name.is_some()))
        .and_then(|i| i.name.clone())
}

#[derive(Debug, Clone)]
pub(crate) struct BssRec {
    pub b: Bss,
    vendor: Option<String>,
    radio: String,
    first_seen: u64,
    last_seen: f64,
    /// (t, dBm)
    history: Vec<(f64, f64)>,
}

impl BssRec {
    /// The record as wifi.ts builds it: `{ firstSeen, history, lastSeen }` assigned the
    /// parsed BSS, then `radio`, then `vendor`. `history` is filtered or replaced by the caller.
    fn json(&self, history: Value) -> Value {
        let o = O::new().k("firstSeen", self.first_seen).k("history", history).n("lastSeen", self.last_seen);
        self.b.fields(o).k("radio", self.radio.clone()).o("vendor", self.vendor.clone()).v()
    }
}

fn history_json(h: &[(f64, f64)]) -> Value {
    Value::Array(h.iter().map(|(t, s)| Value::Array(vec![num(*t), num(*s)])).collect())
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ScanMeta {
    seq: u64,
    at: u64,
    radio: String,
    iface: String,
    method: String,
    duration_ms: u64,
    count: usize,
    error: Option<String>,
    survey: Option<Vec<Survey>>,
    survey_supported: bool,
}

impl ScanMeta {
    fn json(&self) -> Value {
        O::new()
            .k("seq", self.seq)
            .k("at", self.at)
            .k("radio", self.radio.clone())
            .k("iface", self.iface.clone())
            .k("method", self.method.clone())
            .k("durationMs", self.duration_ms)
            .k("count", self.count)
            .k("survey", self.survey.as_ref().map(|s| Value::Array(s.iter().map(Survey::json).collect())).unwrap_or(Value::Null))
            .k("surveySupported", self.survey_supported)
            .o("error", self.error.clone())
            .v()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RouteRec {
    dev: String,
    metric: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct NmWifi {
    name: String,
    ssid: String,
    key_mgmt: String,
}

#[derive(Default)]
pub(crate) struct State {
    pub radios: IndexMap<String, Radio>,
    reg: Vec<RegDomain>,
    reg_changed_from: Option<String>,
    radios_seq: u64,
    nm_wifi: Vec<NmWifi>,
    nm_wifi_at: u64,
    routes: Vec<RouteRec>,
    pub bss: IndexMap<String, BssRec>,
    scan: ScanMeta,
    scanning: IndexSet<String>,
    /// running, by id
    tests: IndexMap<String, Arc<ClientTest>>,
    results: Vec<Value>,
    results_seq: u64,
    oui: Option<HashMap<String, String>>,
    oui_source: String,
    /// when the last scheduled (auto or watch) scan started
    last_auto_scan: u64,
    /// client id → lease expiry
    watchers: IndexMap<String, u64>,
    stopping: bool,
    test_counter: u64,
}

impl State {
    fn sorted_routes(&self) -> Vec<RouteRec> {
        let mut r = self.routes.clone();
        r.sort_by(|a, b| a.metric.partial_cmp(&b.metric).unwrap_or(std::cmp::Ordering::Equal));
        r
    }
    fn uplink(&self) -> Option<String> {
        self.sorted_routes().first().map(|r| r.dev.clone())
    }

    fn find_radio(&self, key: Option<&str>) -> Result<String, HttpError> {
        let Some(key) = key.filter(|k| !k.is_empty()) else {
            return self
                .radios
                .values()
                .find(|x| x.ifaces.iter().any(|i| i.name.is_some() && i.r#type.as_deref() == Some("managed")))
                .or_else(|| self.radios.values().next())
                .map(|r| r.phy.clone())
                .ok_or_else(|| {
                    HttpError::new(
                        404,
                        "No Wi-Fi radios in the main network namespace (a radio borrowed into another BenchBob process's test namespace is invisible until it comes back)",
                    )
                });
        };
        if self.radios.contains_key(key) {
            return Ok(key.into());
        }
        self.radios
            .values()
            .find(|x| x.ifaces.iter().any(|i| i.name.as_deref() == Some(key)) || x.stable_id == key)
            .map(|r| r.phy.clone())
            .ok_or_else(|| HttpError::new(404, format!("No Wi-Fi radio \"{key}\"")))
    }

    fn reg_for(&self, phy: &str) -> Option<&RegDomain> {
        self.reg.iter().find(|x| x.scope == phy)
    }

    fn describe_use(&self, r: &Radio) -> (&'static str, String) {
        if r.busy == Some("test") {
            return ("test", "Borrowed for a client test (in its own network namespace)".into());
        }
        let primary = self.sorted_routes().into_iter().next();
        for i in &r.ifaces {
            let Some(name) = &i.name else { continue };
            if i.r#type.as_deref() == Some("AP") {
                return (
                    "ap",
                    format!(
                        "Access point on {name}{}",
                        i.ssid.as_ref().filter(|s| !s.is_empty()).map(|s| format!(" ({s})")).unwrap_or_default()
                    ),
                );
            }
            let nm = r.nm.iter().find(|n| &n.iface == name);
            let via = self.routes.iter().any(|x| &x.dev == name);
            let ssid = i.ssid.clone().filter(|s| !s.is_empty());
            if ssid.is_some() || nm.is_some_and(|n| n.state == "connected") {
                let net = ssid.or_else(|| nm.map(|n| n.connection.clone()).filter(|c| !c.is_empty())).unwrap_or_else(|| "?".into());
                if via && primary.as_ref().is_some_and(|p| &p.dev == name) {
                    return ("primary", format!("Primary uplink, connected to {net}"));
                }
                if via {
                    return ("backup", format!("Backup uplink, connected to {net}"));
                }
                return ("connected", format!("Connected to {net} (no default route)"));
            }
        }
        if r.busy == Some("scan") {
            return ("scan", "Scanning".into());
        }
        let nm = r.nm.first();
        if !r.unmanaged_by_us.is_empty() {
            return ("idle", format!("Idle; BenchBob told NetworkManager to leave {} alone (until reboot)", r.unmanaged_by_us.join(", ")));
        }
        if nm.is_some_and(|n| n.state == "unavailable") {
            return ("down", "Idle (interface down or unavailable)".into());
        }
        ("idle", if nm.is_some_and(|n| n.state == "unmanaged") { "Idle (not managed by NetworkManager)".into() } else { "Idle".into() })
    }

    fn radio_json(&self, r: &Radio, full: bool) -> Value {
        let reg = self.reg_for(&r.phy);
        let mut o = O::new()
            .k("phy", r.phy.clone())
            .k("stableId", r.stable_id.clone())
            .k("label", r.prefs.get("label").filter(|v| truthy(v)).cloned().unwrap_or(Value::from("")))
            .k("driver", r.driver.clone())
            .k("bus", r.bus)
            .k("onboard", r.onboard)
            .o(
                "usb",
                r.usb.as_ref().map(|u| {
                    O::new().k("vendor", u.vendor.clone()).k("model", u.model.clone()).k("id", u.id.clone()).k("port", u.port.clone()).v()
                }),
            )
            .k(
                "ifaces",
                r.ifaces
                    .iter()
                    .map(|i| {
                        O::new()
                            .k("name", i.name.clone().map(Value::from).unwrap_or(Value::Null))
                            .o("type", i.r#type.clone())
                            .o("addr", i.addr.clone())
                            .o("ssid", i.ssid.clone())
                            .on("channel", i.channel)
                            .on("freq", i.freq)
                            .o("width", i.width.clone())
                            .on("txpower", i.txpower)
                            .v()
                    })
                    .collect::<Vec<_>>(),
            )
            .k(
                "nm",
                r.nm.iter()
                    .map(|n| O::new().k("iface", n.iface.clone()).k("state", n.state.clone()).k("connection", n.connection.clone()).v())
                    .collect::<Vec<_>>(),
            )
            .k("use", O::new().k("kind", r.use_.0).k("text", r.use_.1.clone()).v())
            .k("busy", r.busy.map(Value::from).unwrap_or(Value::Null))
            .k("unmanagedByUs", r.unmanaged_by_us.clone())
            .k("bands", r.info.as_ref().map(|i| i.bands.iter().map(|b| b.band).collect::<Vec<_>>()).unwrap_or_default())
            .k("monitor", r.info.as_ref().is_some_and(|i| i.monitor))
            .k("ap", r.info.as_ref().is_some_and(|i| i.ap))
            .k("netns", r.info.as_ref().is_some_and(|i| i.netns))
            .k("prefs", Value::Object(r.prefs.clone()))
            .o("regCountry", reg.map(|g| g.country.clone()));
        if !full {
            return o.v();
        }
        let empty = PhyInfo::default();
        let info = r.info.as_ref().unwrap_or(&empty);
        o = o
            .k("iftypes", info.iftypes.clone())
            .k("ciphers", info.ciphers.clone())
            .k("sae", info.sae)
            .k(
                "bandInfo",
                info.bands
                    .iter()
                    .map(|b| {
                        O::new()
                            .k("band", b.band)
                            .k("ht", b.ht)
                            .k("vht", b.vht)
                            .k("he", b.he)
                            .k("eht", b.eht)
                            .n("maxWidth", b.max_width)
                            .k(
                                "channels",
                                b.channels
                                    .iter()
                                    .map(|c| {
                                        O::new()
                                            .n("ch", c.channel)
                                            .n("mhz", c.mhz)
                                            .k("disabled", c.disabled)
                                            .k("radar", c.radar)
                                            .k("noIR", c.no_ir)
                                            .on("maxPower", c.max_power)
                                            .k("flags", c.flags.clone())
                                            .v()
                                    })
                                    .collect::<Vec<_>>(),
                            )
                            .v()
                    })
                    .collect::<Vec<_>>(),
            )
            .k("reg", reg.map(RegDomain::json).unwrap_or(Value::Null));
        o.v()
    }

    fn reg_json(&self) -> Value {
        let g = self.reg.iter().find(|x| x.scope == "global");
        O::new()
            .k("country", g.map(|g| g.country.clone()).unwrap_or_default())
            .k("dfsRegion", g.and_then(|g| g.dfs_region.clone()).unwrap_or_default())
            .k("rules", g.map(|g| g.rules.iter().map(p::RegRule::json).collect::<Vec<_>>()).unwrap_or_default())
            .k("phys", self.reg.iter().filter(|x| x.scope != "global").map(RegDomain::json).collect::<Vec<_>>())
            .k("changedFrom", self.reg_changed_from.clone().map(Value::from).unwrap_or(Value::Null))
            .v()
    }

    fn radios_json(&self) -> Value {
        O::new()
            .k("radios", self.radios.values().map(|r| self.radio_json(r, true)).collect::<Vec<_>>())
            .k("reg", self.reg_json())
            .k(
                "savedNetworks",
                self.nm_wifi
                    .iter()
                    .map(|n| O::new().k("connection", n.name.clone()).k("ssid", n.ssid.clone()).k("keyMgmt", n.key_mgmt.clone()).v())
                    .collect::<Vec<_>>(),
            )
            .k("ouiSource", if self.oui_source.is_empty() { Value::Null } else { Value::from(self.oui_source.clone()) })
            .k("uplink", self.uplink().map(Value::from).unwrap_or(Value::Null))
            .v()
    }

    fn global_country(&self) -> String {
        self.reg.iter().find(|x| x.scope == "global").map(|g| g.country.clone()).unwrap_or_default()
    }

    /// Prune expired watch leases; is anyone watching?
    fn watching(&mut self, now: u64) -> bool {
        self.watchers.retain(|_, until| *until >= now);
        !self.watchers.is_empty()
    }
}

/// Scan filters (GET /api/wifi/scan, wifi_scan).
#[derive(Default)]
pub(crate) struct ScanFilter {
    ssid: Option<String>,
    band: Option<String>,
    limit: Option<f64>,
    history_since: Option<f64>,
}

pub(crate) struct Inner {
    ctx: OnceLock<Ctx>,
    pub st: Mutex<State>,
    bins: Arc<Bins>,
    epoch: AtomicU64,
    refresh_gen: AtomicU64,
    loops: Mutex<Vec<Task>>,
}

/// The Wi-Fi driver.
pub struct Wifi {
    inner: Arc<Inner>,
}

impl Default for Wifi {
    fn default() -> Self {
        Self::new()
    }
}

impl Wifi {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                ctx: OnceLock::new(),
                st: Mutex::new(State::default()),
                bins: Arc::new(Bins::resolve()),
                epoch: AtomicU64::new(0),
                refresh_gen: AtomicU64::new(0),
                loops: Mutex::new(Vec::new()),
            }),
        }
    }
}

/// nmcli -t output escapes ':' inside values as '\:'.
pub fn nm_split(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' && it.peek().is_some() {
            cur.push(it.next().unwrap());
        } else if c == ':' {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

/// Vendor and product for display (core.ts `usbName`).
fn usb_name(p: &Props) -> (String, String) {
    let dec = |k: &str| {
        let s = p.get(k).map(String::as_str).unwrap_or("");
        p::re!(r"\\x([0-9a-fA-F]{2})")
            .replace_all(s, |c: &regex::Captures| char::from(u8::from_str_radix(&c[1], 16).unwrap_or(b'?')).to_string())
            .trim()
            .to_string()
    };
    let pick = |a: &str, b: &str, c: &str| {
        let x = p.get(a).cloned().unwrap_or_default();
        if !x.is_empty() {
            return x;
        }
        let y = dec(b);
        if !y.is_empty() {
            return y;
        }
        p.get(c).cloned().unwrap_or_default()
    };
    (pick("ID_VENDOR_FROM_DATABASE", "ID_VENDOR_ENC", "ID_VENDOR"), pick("ID_MODEL_FROM_DATABASE", "ID_MODEL_ENC", "ID_MODEL"))
}

struct BusInfo {
    driver: String,
    bus: &'static str,
    onboard: bool,
    usb: Option<Usb>,
    stable_id: String,
}

impl Inner {
    fn changed(&self) {
        if let Some(c) = self.ctx.get() {
            c.changed();
        }
    }
    fn event(&self, text: &str, detail: Option<Value>) {
        if let Some(c) = self.ctx.get() {
            c.event(text, detail);
        }
    }
    fn log(&self, msg: &str) {
        if let Some(c) = self.ctx.get() {
            c.log(msg);
        }
    }
    fn setting(&self, k: &str) -> Option<Value> {
        self.ctx.get().and_then(|c| c.setting(k))
    }
    fn set_setting(&self, k: &str, v: Value) {
        if let Some(c) = self.ctx.get() {
            c.set_setting(k, v);
        }
    }

    /// The autoScan setting over its defaults (`{ enabled: false, …, ...stored }`).
    pub(crate) fn auto_scan(&self) -> Map<String, Value> {
        let mut a = O::new().k("enabled", false).k("intervalS", 60).k("radio", "").k("method", "auto").k("watchIntervalS", 30).0;
        if let Some(Value::Object(m)) = self.setting("autoScan") {
            for (k, v) in m {
                a.insert(k, v);
            }
        }
        a
    }
    pub(crate) fn profiles(&self) -> Vec<Value> {
        match self.setting("profiles") {
            Some(Value::Array(a)) => a,
            _ => vec![],
        }
    }
    fn prefs_all(&self) -> Map<String, Value> {
        match self.setting("radioPrefs") {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        }
    }
    fn borrowed(&self) -> Map<String, Value> {
        match self.setting("borrowed") {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        }
    }

    // ---------- routes & NetworkManager ----------

    async fn load_routes(&self) {
        let r = run(&[&self.bins.ip, "-j", "route", "show", "default"], Opts::default()).await;
        let routes = serde_json::from_str::<Vec<Value>>(if r.out.is_empty() { "[]" } else { &r.out })
            .map(|a| {
                a.iter()
                    .map(|x| RouteRec {
                        dev: x.get("dev").and_then(Value::as_str).unwrap_or("").into(),
                        metric: x.get("metric").and_then(Value::as_f64).unwrap_or(0.0),
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.st.lock().unwrap().routes = routes;
    }

    pub(crate) async fn uplink(&self) -> Option<String> {
        self.load_routes().await;
        self.st.lock().unwrap().uplink()
    }

    async fn refresh_nm_wifi(&self, force: bool) {
        {
            let mut st = self.st.lock().unwrap();
            let now = now_ms();
            if !force && now.saturating_sub(st.nm_wifi_at) < 60000 {
                return;
            }
            st.nm_wifi_at = now;
        }
        let b = &self.bins;
        let r = run(&[&b.nmcli, "-t", "-f", "NAME,TYPE", "con", "show"], Opts::default()).await;
        let mut out = Vec::new();
        for l in r.out.split('\n') {
            let f = nm_split(l);
            if f.get(1).map(String::as_str) != Some("802-11-wireless") {
                continue;
            }
            let name = f[0].clone();
            let d = run(&[&b.nmcli, "-g", "802-11-wireless.ssid,802-11-wireless-security.key-mgmt", "con", "show", &name], Opts::default())
                .await;
            let mut it = d.out.split('\n');
            let ssid = it.next().unwrap_or("").to_string();
            let key_mgmt = it.next().unwrap_or("").to_string();
            out.push(NmWifi { name, ssid, key_mgmt });
        }
        self.st.lock().unwrap().nm_wifi = out;
    }

    // ---------- radios ----------

    fn describe_bus(&self, phy: &str) -> BusInfo {
        let driver = std::fs::read_link(format!("/sys/class/ieee80211/{phy}/device/driver"))
            .ok()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let devpath = std::fs::canonicalize(format!("/sys/class/ieee80211/{phy}"))
            .map(|p| {
                let s = p.display().to_string();
                s.strip_prefix("/sys").map(String::from).unwrap_or(s)
            })
            .unwrap_or_default();
        let usb_dev = if devpath.is_empty() { None } else { self.ctx.get().and_then(|c| c.ancestor(&devpath, "usb", Some("usb_device"))) };
        if let Some(u) = usb_dev {
            let (vendor, model) = usb_name(&u);
            let g = |k: &str| u.get(k).filter(|v| !v.is_empty()).cloned();
            return BusInfo {
                driver,
                bus: "usb",
                onboard: false,
                usb: Some(Usb {
                    vendor,
                    model,
                    id: format!("{}:{}", g("ID_VENDOR_ID").unwrap_or_else(|| "?".into()), g("ID_MODEL_ID").unwrap_or_else(|| "?".into())),
                    port: u.get("DEVPATH").map(|d| d.rsplit('/').next().unwrap_or("").to_string()).unwrap_or_default(),
                }),
                stable_id: format!(
                    "usb:{}",
                    g("ID_SERIAL").or_else(|| g("ID_PATH")).or_else(|| u.get("DEVPATH").cloned()).unwrap_or_else(|| "undefined".into())
                ),
            };
        }
        // mac80211_hwsim radios live under /devices/virtual: treat them like dongles, not onboard.
        let bus = if devpath.contains("/virtual/") {
            "virtual"
        } else if devpath.contains("/mmc") {
            "sdio"
        } else if devpath.contains("/pci") {
            "pci"
        } else if !devpath.is_empty() {
            "platform"
        } else {
            "unknown"
        };
        let base = p::re!(r"/ieee80211/phy\d+$").replace(&devpath, "").into_owned();
        BusInfo { stable_id: format!("{bus}:{driver}:{base}"), driver, bus, onboard: bus != "virtual", usb: None }
    }

    pub(crate) async fn refresh_radios(self: &Arc<Self>, phy_info: bool) {
        let b = self.bins.clone();
        let (c1, c2, c3) =
            ([b.iw.as_str(), "dev"], [b.iw.as_str(), "reg", "get"], [b.nmcli.as_str(), "-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "dev"]);
        let (dev, reg_out, nm_dev, _, _) = tokio::join!(
            run(&c1, Opts::default()),
            run(&c2, Opts::default()),
            run(&c3, Opts::default()),
            self.load_routes(),
            self.refresh_nm_wifi(false),
        );
        let reg = p::parse_iw_reg(&reg_out.out);
        let ifs = p::parse_iw_dev(&dev.out);
        let nm: Vec<Vec<String>> = nm_dev.out.split('\n').map(nm_split).filter(|x| x.len() >= 4).collect();
        // `iw dev` omits phys with no interfaces; `iw phy` lists them all.
        let mut phys: IndexSet<String> = ifs.iter().map(|i| i.phy.clone()).collect();
        let list = run(&[&b.iw, "list"], Opts::default()).await;
        for c in p::re!(r"(?m)^Wiphy (\S+)").captures_iter(&list.out) {
            phys.insert(c[1].to_string());
        }
        let prefs = self.prefs_all();
        self.st.lock().unwrap().reg = reg;
        let mut new_events: Vec<(String, Value)> = Vec::new();
        for phy in &phys {
            let (is_new, need_info) = {
                let mut st = self.st.lock().unwrap();
                let is_new = !st.radios.contains_key(phy);
                if is_new {
                    let bi = self.describe_bus(phy);
                    st.radios.insert(
                        phy.clone(),
                        Radio {
                            phy: phy.clone(),
                            stable_id: bi.stable_id,
                            driver: bi.driver,
                            bus: bi.bus,
                            onboard: bi.onboard,
                            usb: bi.usb,
                            ifaces: vec![],
                            info: None,
                            nm: vec![],
                            use_: ("idle", String::new()),
                            unmanaged_by_us: vec![],
                            busy: None,
                            prefs: Map::new(),
                        },
                    );
                }
                let r = st.radios.get_mut(phy).unwrap();
                if r.busy == Some("test") {
                    continue; // it's in the netns; keep what we knew
                }
                r.prefs = prefs.get(&r.stable_id).and_then(Value::as_object).cloned().unwrap_or_default();
                r.ifaces = ifs.iter().filter(|i| &i.phy == phy).cloned().collect();
                r.nm = r
                    .ifaces
                    .iter()
                    .filter_map(|i| i.name.clone())
                    .map(|name| {
                        let row = nm.iter().find(|x| x[0] == name);
                        NmRow {
                            state: row.map(|r| r[2].clone()).unwrap_or_default(),
                            connection: row.map(|r| r[3].clone()).filter(|c| !c.is_empty() && c != "--").unwrap_or_default(),
                            iface: name,
                        }
                    })
                    .collect();
                (is_new, is_new || phy_info || r.info.is_none())
            };
            if need_info {
                let info = run(&[&b.iw, "phy", phy, "info"], Opts::default()).await;
                if info.code == 0
                    && let Some(r) = self.st.lock().unwrap().radios.get_mut(phy)
                {
                    r.info = Some(p::parse_iw_phy(&info.out));
                }
            }
            let (keep, onboard, label) = {
                let st = self.st.lock().unwrap();
                let r = &st.radios[phy];
                let label = format!(
                    "Wi-Fi radio {phy} appeared: {} {}",
                    r.usb.as_ref().map(|u| u.vendor.clone()).unwrap_or_default(),
                    r.usb.as_ref().map(|u| u.model.clone()).filter(|m| !m.is_empty()).unwrap_or_else(|| r.driver.clone())
                );
                (
                    r.keep_unmanaged(),
                    r.onboard,
                    (label.trim().to_string(), O::new().k("phy", phy.clone()).k("driver", r.driver.clone()).v()),
                )
            };
            if is_new && keep {
                let _ = self.unmanage(phy).await;
            }
            if is_new && !onboard {
                new_events.push(label);
            }
            let mut st = self.st.lock().unwrap();
            if let Some(r) = st.radios.get(phy).cloned() {
                let u = st.describe_use(&r);
                st.radios.get_mut(phy).unwrap().use_ = u;
            }
        }
        for (t, d) in new_events {
            self.event(&t, Some(d));
        }
        let gone: Vec<Radio> = {
            let mut st = self.st.lock().unwrap();
            let g: Vec<String> =
                st.radios.iter().filter(|(k, r)| !phys.contains(*k) && r.busy != Some("test")).map(|(k, _)| k.clone()).collect();
            g.iter().filter_map(|k| st.radios.shift_remove(k)).collect()
        };
        for r in gone {
            let what = r.usb.as_ref().map(|u| u.model.clone()).filter(|m| !m.is_empty()).unwrap_or_else(|| r.driver.clone());
            self.event(&format!("Wi-Fi radio {} removed ({what})", r.phy), Some(O::new().k("phy", r.phy.clone()).v()));
        }
        self.st.lock().unwrap().radios_seq += 1;
        self.changed();
    }

    /// Refresh soon; a later call replaces an earlier pending one (clearTimeout).
    pub(crate) fn schedule_refresh(self: &Arc<Self>, ms: u64) {
        let g = self.refresh_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            if me.refresh_gen.load(Ordering::SeqCst) == g && !me.st.lock().unwrap().stopping {
                me.refresh_radios(false).await;
            }
        });
    }

    /// Tell NM (runtime only) to leave a radio BenchBob is using alone. Never for NM's own
    /// connected radio.
    pub(crate) async fn unmanage(&self, phy: &str) -> Result<(), HttpError> {
        let todo: Vec<String> = {
            let st = self.st.lock().unwrap();
            let Some(r) = st.radios.get(phy) else { return Ok(()) };
            r.nm.iter()
                .filter(|n| n.state != "connected" && n.state != "unmanaged" && !r.unmanaged_by_us.contains(&n.iface))
                .map(|n| n.iface.clone())
                .collect()
        };
        for iface in todo {
            let res = sudo(&self.bins, &[&self.bins.nmcli, "dev", "set", &iface, "managed", "no"], 20000).await;
            if res.code != 0 {
                return Err(HttpError::new(500, format!("nmcli dev set {iface} managed no: {}", res.err)));
            }
            {
                let mut st = self.st.lock().unwrap();
                if let Some(r) = st.radios.get_mut(phy) {
                    r.unmanaged_by_us.push(iface.clone());
                    for n in r.nm.iter_mut().filter(|n| n.iface == iface) {
                        n.state = "unmanaged".into();
                    }
                }
            }
            self.event(&format!("Told NetworkManager to leave {iface} alone (runtime only, BenchBob is using it)"), None);
        }
        Ok(())
    }

    pub(crate) async fn set_reg(self: &Arc<Self>, country: Option<&Value>) -> Result<Value, HttpError> {
        let c = str_or_empty(country).to_uppercase();
        if !p::re!(r"^([A-Z]{2}|00)$").is_match(&c) {
            return Err(HttpError::bad_request("Country must be a two-letter ISO code, or 00 for world"));
        }
        let before = self.st.lock().unwrap().global_country();
        let r = sudo(&self.bins, &[&self.bins.iw, "reg", "set", &c], 20000).await;
        if r.code != 0 {
            return Err(HttpError::new(500, format!("iw reg set {c}: {}", if r.err.is_empty() { &r.out } else { &r.err })));
        }
        // The kernel applies it asynchronously (it asks the regulatory db / CRDA).
        tokio::time::sleep(Duration::from_millis(1500)).await;
        self.refresh_radios(true).await;
        let (now, reg_json, self_managed) = {
            let mut st = self.st.lock().unwrap();
            let now = st.global_country();
            if st.reg_changed_from.is_none() && before != c {
                st.reg_changed_from = Some(before);
            }
            if st.reg_changed_from.as_deref() == Some(now.as_str()) {
                st.reg_changed_from = None;
            }
            (now, st.reg_json(), st.reg.iter().any(|x| x.self_managed))
        };
        self.event(&format!("Wi-Fi regulatory domain set to {c} (now {now}; temporary, until reboot)"), None);
        // Seen on desky: asking for GB while wlan0 was associated to an AP advertising "US" gave
        // "98" (cfg80211's intersection of the user hint and the AP's country IE), and asking
        // for US again kept 98 until wlan0's connection dropped and the core reset itself.
        let mut notes: Vec<String> = Vec::new();
        if now != c {
            notes.push(format!(
                "The kernel applied {now}, not {c}: {}. It recalculates when that connection drops (a client test on the radio does this).",
                if now == "98" {
                    "98 is the intersection of your request with the country an associated AP advertises"
                } else {
                    "another regulatory hint takes precedence"
                }
            ));
        }
        if self_managed {
            notes.push("Some radios are self-managed and keep their own regulatory domain.".into());
        }
        let note = notes.join(" ");
        Ok(spread(
            spread(O::new().k("ok", true).k("requested", c).v(), reg_json),
            O::new().o("note", (!note.is_empty()).then_some(note)).v(),
        ))
    }

    // ---------- scanner ----------

    pub(crate) async fn do_scan(self: &Arc<Self>, radio_key: Option<String>, method_in: Option<String>) -> Result<Value, HttpError> {
        let b = self.bins.clone();
        let (phy, iface, method, nm_own) = {
            let mut st = self.st.lock().unwrap();
            let phy = st.find_radio(radio_key.as_deref())?;
            let r = &st.radios[&phy];
            if r.busy == Some("test") {
                return Err(HttpError::new(409, format!("{phy} is busy with a client test")));
            }
            if st.scanning.contains(&phy) {
                return Err(HttpError::new(409, format!("{phy} is already scanning")));
            }
            let Some(iface) = managed_iface(r) else {
                return Err(HttpError::new(409, format!("{phy} has no network interface to scan with")));
            };
            let pref = r.pref_str("scanMethod").filter(|m| !m.is_empty() && m != "auto");
            let method = match method_in.filter(|m| !m.is_empty() && m != "auto") {
                Some(m) => m,
                None => pref.unwrap_or_else(|| "auto".into()),
            };
            if !["auto", "iw", "nm"].contains(&method.as_str()) {
                return Err(HttpError::bad_request("method must be auto, iw or nm"));
            }
            let nm_own = r.nm.iter().find(|n| n.iface == iface).map(|n| n.state.clone());
            st.scanning.insert(phy.clone());
            st.radios.get_mut(&phy).unwrap().busy = Some("scan");
            (phy, iface, method, nm_own)
        };
        self.changed();
        let t0 = now_ms();
        let res = self.scan_inner(&b, &phy, &iface, &method, nm_own, t0).await;
        if let Err(e) = &res {
            let mut st = self.st.lock().unwrap();
            let now = now_ms();
            st.scan.seq += 1;
            st.scan.at = now;
            st.scan.radio = phy.clone();
            st.scan.iface = iface.clone();
            st.scan.error = Some(e.message.clone());
            st.scan.duration_ms = now.saturating_sub(t0);
        }
        {
            let mut st = self.st.lock().unwrap();
            st.scanning.shift_remove(&phy);
            if let Some(r) = st.radios.get(&phy).cloned() {
                let mut r = r;
                if r.busy == Some("scan") {
                    r.busy = None;
                }
                r.use_ = st.describe_use(&r);
                st.radios.insert(phy.clone(), r);
            }
        }
        self.changed();
        res
    }

    async fn scan_inner(
        self: &Arc<Self>,
        b: &Bins,
        phy: &str,
        iface: &str,
        method: &str,
        nm_own: Option<String>,
        t0: u64,
    ) -> Result<Value, HttpError> {
        let mut used = method.to_string();
        // A radio NM isn't using (a dongle) gets taken away from NM before we touch it.
        if nm_own.as_deref().is_some_and(|s| s != "connected") {
            self.unmanage(phy).await?;
        }
        let link = run(&[&b.ip, "-j", "link", "show", iface], Opts::default()).await;
        if !link.out.contains("\"UP\"") {
            sudo(b, &[&b.ip, "link", "set", iface, "up"], 20000).await;
        }
        let mut out = String::new();
        let mut err = String::new();
        if method != "nm" {
            // `iw scan` works while NM is connected (verified on desky); it can collide with
            // NM's own background scans (EBUSY), so retry before falling back.
            for _ in 0..3 {
                let s = sudo(b, &[&b.iw, "dev", iface, "scan"], 30000).await;
                if s.code == 0 {
                    out = s.out;
                    err.clear();
                    used = "iw".into();
                    break;
                }
                err = if s.err.is_empty() { s.out } else { s.err };
                if !p::re!(r"(?i)busy|-16").is_match(&err) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
        }
        if out.is_empty() && (method == "nm" || (method == "auto" && nm_own.as_deref().is_some_and(|s| s != "unmanaged"))) {
            // NM's rescan only triggers; results land in the kernel BSS cache, which
            // `iw scan dump` reads with full IE detail.
            let t = sudo(b, &[&b.nmcli, "dev", "wifi", "rescan", "ifname", iface], 20000).await;
            tokio::time::sleep(Duration::from_millis(if t.code != 0 { 500 } else { 5000 })).await;
            let d = sudo(b, &[&b.iw, "dev", iface, "scan", "dump"], 20000).await;
            if d.code == 0 {
                out = d.out;
                err.clear();
                used = "nm".into();
            } else if err.is_empty() {
                err = d.err;
            }
        }
        if out.is_empty() && !err.is_empty() {
            return Err(HttpError::new(500, format!("Scan on {iface} failed: {err}")));
        }

        let list = p::parse_iw_scan(&out);
        let now = now_ms();
        {
            let mut st = self.st.lock().unwrap();
            let st = &mut *st;
            for bss in &list {
                let mut rec = st.bss.get(&bss.bssid).cloned().unwrap_or(BssRec {
                    b: bss.clone(),
                    vendor: None,
                    radio: String::new(),
                    first_seen: now,
                    last_seen: 0.0,
                    history: vec![],
                });
                let fresh = bss.last_seen_ms.is_none_or(|l| l < 15000.0);
                rec.b = bss.clone();
                rec.radio = phy.to_string();
                let ago = bss.last_seen_ms.filter(|l| *l != 0.0 && !l.is_nan()).unwrap_or(0.0);
                if fresh {
                    rec.last_seen = now as f64 - ago;
                    if let Some(s) = bss.signal {
                        rec.history.push((now as f64, s));
                    }
                    rec.history = p::trim_history(&rec.history, now as f64, BSS_MAX_AGE_MS, HISTORY_MAX);
                } else if rec.last_seen == 0.0 || rec.last_seen.is_nan() {
                    rec.last_seen = now as f64 - ago;
                }
                if let Some(oui) = &st.oui
                    && !bss.local_mac
                {
                    rec.vendor = oui.get(&bss.bssid.replace(':', "")[..6]).cloned();
                }
                st.bss.insert(bss.bssid.clone(), rec);
            }
            st.bss.retain(|_, v| now as f64 - v.last_seen <= BSS_MAX_AGE_MS);
        }

        let sv = sudo(b, &[&b.iw, "dev", iface, "survey", "dump"], 20000).await;
        let survey: Vec<Survey> = p::parse_survey(&sv.out).into_iter().filter(|s| p::band_of(s.freq).is_some()).collect(); // drop S1G etc.
        let mut st = self.st.lock().unwrap();
        let seq = st.scan.seq + 1;
        let supported = !survey.is_empty();
        st.scan = ScanMeta {
            seq,
            at: now,
            radio: phy.into(),
            iface: iface.into(),
            method: used,
            duration_ms: now.saturating_sub(t0),
            count: list.len(),
            error: None,
            survey: supported.then_some(survey),
            survey_supported: supported,
        };
        Ok(Self::scan_json(&mut st, &ScanFilter::default()))
    }

    /// Start a scan if auto-scan or a watching viewer wants one now (see `scan_due`).
    pub(crate) fn tick_scans(self: &Arc<Self>, force: bool) {
        let a = self.auto_scan();
        let go = {
            let mut st = self.st.lock().unwrap();
            if st.stopping {
                return;
            }
            let now = now_ms();
            let last_scan = st.last_auto_scan.max(st.scan.at);
            let busy = !st.scanning.is_empty();
            let watching = st.watching(now);
            let due = if force {
                !busy
            } else {
                p::scan_due(&p::ScanDue {
                    now: now as f64,
                    last_scan: last_scan as f64,
                    busy,
                    auto_enabled: a.get("enabled").is_some_and(truthy),
                    auto_interval_s: a.get("intervalS").map(js_number).unwrap_or(f64::NAN),
                    watching,
                    watch_interval_s: a.get("watchIntervalS").map(js_number).unwrap_or(f64::NAN),
                })
            };
            if due {
                st.last_auto_scan = now;
            }
            due
        };
        if !go {
            return;
        }
        let radio = a.get("radio").filter(|v| truthy(v)).map(js_string);
        let method = a.get("method").filter(|v| truthy(v)).map(js_string);
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.do_scan(radio, method).await {
                me.log(&format!("scheduled scan: {}", e.message));
            }
        });
    }

    /// historySince: only samples newer than it (ms, from a previous answer's `now`), so a
    /// viewer refreshing every scan gets one sample per BSS instead of the whole hour again.
    pub(crate) fn scan_json(st: &mut State, f: &ScanFilter) -> Value {
        let mut list: Vec<&BssRec> = st.bss.values().collect();
        list.sort_by(|a, b| {
            let (x, y) = (b.b.signal.unwrap_or(-200.0), a.b.signal.unwrap_or(-200.0));
            x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
        });
        if let Some(s) = f.ssid.as_ref().filter(|s| !s.is_empty()) {
            let s = s.to_lowercase();
            list.retain(|b| b.b.ssid.to_lowercase().contains(&s));
        }
        if let Some(band) = f.band.as_ref().filter(|s| !s.is_empty()) {
            list.retain(|b| b.b.band == Some(band.as_str()));
        }
        if let Some(l) = f.limit.filter(|l| *l != 0.0 && !l.is_nan()) {
            list = json::slice_n(&list, l);
        }
        let since = f.history_since.filter(|s| *s != 0.0 && !s.is_nan());
        let bss: Vec<Value> = list
            .iter()
            .map(|b| match since {
                Some(s) => b.json(history_json(&b.history.iter().copied().filter(|h| h.0 > s).collect::<Vec<_>>())),
                None => b.json(history_json(&b.history)),
            })
            .collect();
        O::new()
            .k("scan", st.scan.json())
            .k("now", now_ms())
            .k("historySince", since.map(num).unwrap_or(Value::from(0)))
            .n("historyMaxAgeMs", BSS_MAX_AGE_MS)
            .k("historyMax", HISTORY_MAX)
            .k("bss", bss)
            .k("ouiSource", if st.oui_source.is_empty() { Value::Null } else { Value::from(st.oui_source.clone()) })
            .v()
    }

    // ---------- client tests ----------

    async fn saved_key(&self, ssid: &str) -> Option<(String, String)> {
        self.refresh_nm_wifi(true).await;
        let name = self.st.lock().unwrap().nm_wifi.iter().find(|n| n.ssid == ssid).map(|n| n.name.clone())?;
        let r = sudo(&self.bins, &[&self.bins.nmcli, "-s", "-g", "802-11-wireless-security.psk", "con", "show", &name], 20000).await;
        (!r.out.is_empty()).then_some((r.out, name))
    }

    /// Normalise a test request (REST or MCP) into a spec + key. Errors are HTTP errors.
    pub(crate) async fn resolve_spec(&self, input: &Value) -> Result<(TestSpec, Option<String>, Option<String>), HttpError> {
        let mut base: Map<String, Value> = Map::new();
        if let Some(want) = input.get("profile").filter(|v| truthy(v)) {
            base = self
                .profiles()
                .into_iter()
                .find(|p| p.get("id") == Some(want) || p.get("name") == Some(want))
                .and_then(|p| p.as_object().cloned())
                .ok_or_else(|| HttpError::new(404, format!("No profile \"{}\"", js_string(want))))?;
        }
        let mut o = base.clone();
        if let Some(m) = input.as_object() {
            for (k, v) in m {
                if v.as_str() != Some("") {
                    o.insert(k.clone(), v.clone());
                }
            }
        }
        let g = |k: &str| o.get(k).filter(|v| truthy(v));
        let radio_key = g("radio").map(js_string);
        let (phy, scanned_kind) = {
            let st = self.st.lock().unwrap();
            let phy = st.find_radio(radio_key.as_deref())?;
            let ssid = str_or_empty(o.get("ssid"));
            let mut best: Option<&BssRec> = None;
            for b in st.bss.values().filter(|b| b.b.ssid == ssid) {
                if best.is_none_or(|x| b.b.signal.unwrap_or(-200.0) > x.b.signal.unwrap_or(-200.0)) {
                    best = Some(b);
                }
            }
            (phy, best.map(|b| b.b.security_kind))
        };
        let ssid = str_or_empty(o.get("ssid"));
        if ssid.is_empty() || ssid.len() > 32 {
            return Err(HttpError::bad_request("ssid is required (1–32 bytes)"));
        }
        let security: Value = match g("security") {
            Some(s) => s.clone(),
            None => match scanned_kind.filter(|k| *k != "wep") {
                Some(k) => Value::from(k),
                None => Value::from(if g("psk").is_some() || g("useSavedKey").is_some() { "psk" } else { "open" }),
            },
        };
        if security.as_str() == Some("wep") {
            return Err(HttpError::bad_request("WEP isn't supported"));
        }
        let security = match security.as_str() {
            Some(s @ ("open" | "owe" | "psk" | "sae" | "psk-sae" | "eap")) => s.to_string(),
            _ => return Err(HttpError::bad_request("security must be open, owe, psk, sae, psk-sae or eap")),
        };

        let input_psk = input.get("psk").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
        let base_psk = base.get("psk").filter(|v| truthy(v)).map(js_string);
        let mut key = input_psk.clone().or_else(|| base_psk.clone());
        let mut key_source = if input.get("psk").is_some_and(truthy) {
            "given"
        } else if base_psk.is_some() {
            "profile"
        } else {
            "none"
        };
        if key.is_none() && g("useSavedKey").is_some() && security != "open" && security != "owe" {
            let Some((k, _conn)) = self.saved_key(&ssid).await else {
                return Err(HttpError::bad_request(format!("NetworkManager has no saved key for \"{ssid}\"")));
            };
            // NM keeps WPA2 keys as a 64-hex PMK; SAE needs the passphrase, so fall back to WPA2.
            if k.len() == 64 && k.bytes().all(|c| c.is_ascii_hexdigit()) && security == "sae" {
                return Err(HttpError::bad_request(
                    "NetworkManager stores this network's key as a hashed PSK, which can't be used for WPA3-SAE",
                ));
            }
            key = Some(k);
            key_source = "networkmanager";
        }
        if let Some(e) = client::valid_key(&security, key.as_deref()) {
            return Err(HttpError::bad_request(e));
        }

        let ip = match g("ip") {
            Some(v) => v.clone(),
            None => Value::from(if o.get("dhcp") == Some(&Value::Bool(false)) { "none" } else { "dhcp" }),
        };
        let ip = match ip.as_str() {
            Some(s @ ("dhcp" | "static" | "none")) => s.to_string(),
            _ => return Err(HttpError::bad_request("ip must be dhcp, static or none")),
        };
        let mut stat = None;
        if ip == "static" {
            let s = g("static").cloned().unwrap_or(Value::Object(Map::new()));
            let address = str_or_empty(s.get("address"));
            if !p::re!(r"^\d+\.\d+\.\d+\.\d+/\d+$").is_match(&address) {
                return Err(HttpError::bad_request("static.address must look like 192.168.1.50/24"));
            }
            let gw = s.get("gateway").filter(|v| truthy(v)).map(js_string);
            if let Some(gw) = &gw
                && !p::re!(r"^\d+\.\d+\.\d+\.\d+$").is_match(gw)
            {
                return Err(HttpError::bad_request("static.gateway must be an IPv4 address"));
            }
            let dns = str_list(s.get("dns"), "DNS servers")?;
            if dns.iter().any(|d| !p::re!(r"^[\d.:a-fA-F]+$").is_match(d)) {
                return Err(HttpError::bad_request("static.dns must be IP addresses"));
            }
            stat = Some(client::Static { address, gateway: gw, dns });
        }
        let or_default = |k: &str, d: &str| match o.get(k) {
            None | Some(Value::Null) => Some(Value::Array(vec![Value::from(d)])),
            Some(v) => Some(v.clone()),
        };
        let dns = str_list(or_default("dns", "example.com").as_ref(), "DNS names")?;
        let http = str_list(or_default("http", "https://example.com/").as_ref(), "URLs")?;
        let ping = str_list(o.get("ping"), "ping targets")?;
        for h in dns.iter().chain(ping.iter()) {
            if !client::valid_host(h) {
                return Err(HttpError::bad_request(format!("Bad host name \"{h}\"")));
            }
        }
        for u in &http {
            if !client::valid_url(u) {
                return Err(HttpError::bad_request(format!("Bad URL \"{u}\" (http or https only)")));
            }
        }
        let mut iperf = None;
        if let Some(ip3) = o.get("iperf")
            && let Some(server) = ip3.get("server").filter(|v| truthy(v))
        {
            let server = js_string(server);
            if !client::valid_host(&server) {
                return Err(HttpError::bad_request("Bad iperf3 server"));
            }
            let n = |k: &str, d: f64| {
                let v = ip3.get(k).map(js_number).unwrap_or(f64::NAN);
                if v.is_nan() || v == 0.0 { d } else { v }
            };
            iperf = Some(client::Iperf {
                server,
                port: n("port", 5201.0).clamp(1.0, 65535.0),
                seconds: n("seconds", 5.0).clamp(1.0, 60.0),
                reverse: ip3.get("reverse").is_some_and(truthy),
            });
        }
        let bssid = g("bssid").map(js_string);
        if let Some(b) = &bssid
            && !p::re!(r"(?i)^[0-9a-f]{2}(:[0-9a-f]{2}){5}$").is_match(b)
        {
            return Err(HttpError::bad_request("bssid must look like aa:bb:cc:dd:ee:ff"));
        }
        let eap_v = o.get("eap");
        if security == "eap" && !eap_v.and_then(|e| e.get("identity")).is_some_and(truthy) {
            return Err(HttpError::bad_request("eap.identity is required for Enterprise"));
        }
        let numo = |k: &str, d: f64, lo: f64, hi: f64| {
            let v = o.get(k).map(js_number).unwrap_or(f64::NAN);
            (if v.is_nan() || v == 0.0 { d } else { v }).clamp(lo, hi)
        };
        let spec = TestSpec {
            phy,
            ssid,
            security: security.clone(),
            bssid: bssid.map(|b| b.to_lowercase()).filter(|b| !b.is_empty()),
            eap: (security == "eap").then(|| {
                let e = eap_v.cloned().unwrap_or(Value::Null);
                client::Eap {
                    method: if e.get("method").and_then(Value::as_str) == Some("TTLS") { "TTLS" } else { "PEAP" },
                    identity: e.get("identity").map(js_string).unwrap_or_default(),
                    anonymous_identity: e.get("anonymousIdentity").filter(|v| truthy(v)).map(js_string),
                }
            }),
            ip,
            r#static: stat,
            ping_gateway: o.get("pingGateway") != Some(&Value::Bool(false)),
            dns,
            http,
            ping,
            iperf,
            count: numo("count", 5.0, 1.0, 50.0),
            hold_s: numo("holdS", 0.0, 0.0, 600.0),
            assoc_timeout_s: numo("assocTimeoutS", 30.0, 5.0, 120.0),
            key_source: Some(key_source),
            profile: base.get("id").cloned(),
        };
        let save = input.get("saveAs").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        Ok((spec, key, save))
    }

    /// Refuse to borrow a radio whose interface is the working uplink.
    async fn safety_check(&self, phy: &str) -> Result<(), HttpError> {
        self.load_routes().await;
        let link = run(&[&self.bins.ip, "-j", "link", "show"], Opts::default()).await;
        // Link state of every interface, so the pure rule can ask about the best other route.
        let mut up: HashSet<String> = HashSet::new();
        if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(&link.out) {
            for l in a {
                let no_carrier = l.get("flags").and_then(Value::as_array).is_some_and(|f| f.iter().any(|x| x == "NO-CARRIER"));
                if l.get("operstate").and_then(Value::as_str) == Some("UP") && !no_carrier {
                    up.insert(l.get("ifname").and_then(Value::as_str).unwrap_or("").into());
                }
            }
        }
        let st = self.st.lock().unwrap();
        let mine: HashSet<String> = st.radios.get(phy).map(|r| r.names().into_iter().collect()).unwrap_or_default();
        let routes: Vec<(String, f64)> = st.routes.iter().map(|r| (r.dev.clone(), r.metric)).collect();
        match client::borrow_refusal(&routes, &mine, |d| up.contains(d)) {
            Some(why) => Err(HttpError::new(409, why)),
            None => Ok(()),
        }
    }

    pub(crate) async fn start_test(self: &Arc<Self>, input: &Value) -> Result<Arc<ClientTest>, HttpError> {
        let (spec, key, save) = self.resolve_spec(input).await?;
        let phy = spec.phy.clone();
        if self.st.lock().unwrap().radios.get(&phy).is_some_and(|r| r.busy == Some("test")) {
            return Err(HttpError::new(409, format!("{phy} is already running a client test")));
        }
        // An (auto-)scan takes ~4 s; wait it out rather than fail the caller.
        for _ in 0..80 {
            if !self.st.lock().unwrap().scanning.contains(&phy) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if self.st.lock().unwrap().scanning.contains(&phy) {
            return Err(HttpError::new(409, format!("{phy} is scanning; try again in a few seconds")));
        }
        if !self.st.lock().unwrap().radios.get(&phy).and_then(|r| r.info.as_ref()).is_some_and(|i| i.netns) {
            return Err(HttpError::new(409, format!("{phy}'s driver can't move into a network namespace (no set_wiphy_netns)")));
        }
        self.safety_check(&phy).await?;
        if let Some(name) = save {
            let ks = spec.key_source;
            let entries: Vec<(&str, Option<Value>)> = vec![
                ("name", Some(Value::from(name))),
                ("ssid", Some(Value::from(spec.ssid.clone()))),
                ("security", Some(Value::from(spec.security.clone()))),
                ("psk", if ks == Some("given") { key.clone().map(Value::from) } else { None }),
                ("useSavedKey", (ks == Some("networkmanager")).then_some(Value::Bool(true))),
                ("eap", spec.eap_json()),
                ("bssid", spec.bssid.clone().map(Value::from)),
                ("ip", Some(Value::from(spec.ip.clone()))),
                ("static", spec.static_json()),
                ("pingGateway", Some(Value::Bool(spec.ping_gateway))),
                ("dns", Some(Value::from(spec.dns.clone()))),
                ("http", Some(Value::from(spec.http.clone()))),
                ("ping", Some(Value::from(spec.ping.clone()))),
                ("iperf", Some(spec.iperf_json())),
                ("count", Some(num(spec.count))),
                ("holdS", Some(num(spec.hold_s))),
            ];
            self.save_profile(entries);
        }

        let mut rc = {
            let st = self.st.lock().unwrap();
            let r = &st.radios[&phy];
            let names = r.names();
            let mut unmanage: Vec<String> = r.unmanaged_by_us.clone();
            if r.keep_unmanaged() {
                for n in &names {
                    if !unmanage.contains(n) {
                        unmanage.push(n.clone());
                    }
                }
            }
            RadioCtx {
                phy: phy.clone(),
                ifaces: names,
                nm: r
                    .nm
                    .iter()
                    .filter(|n| n.state == "connected" && !n.connection.is_empty())
                    .map(|n| (n.iface.clone(), n.connection.clone()))
                    .collect(),
                unmanage,
            }
        };
        // A dongle NM isn't using: stop NM grabbing it when it comes back.
        if rc.nm.is_empty() {
            let _ = self.unmanage(&phy).await;
            for n in self.st.lock().unwrap().radios[&phy].unmanaged_by_us.clone() {
                if !rc.unmanage.contains(&n) {
                    rc.unmanage.push(n);
                }
            }
        }
        let id = {
            let mut st = self.st.lock().unwrap();
            if st.radios.get(&phy).is_some_and(|r| r.busy == Some("test")) {
                return Err(HttpError::new(409, format!("{phy} is already running a client test")));
            }
            st.test_counter += 1;
            format!("{}-{}", base36(now_ms()), st.test_counter)
        };
        let me = self.clone();
        let deps = Deps {
            bins: self.bins.clone(),
            ctx: self.ctx.get().cloned(),
            uplink: Arc::new(move || {
                let me = me.clone();
                Box::pin(async move { me.uplink().await })
            }),
        };
        let t = ClientTest::new(id.clone(), spec.clone(), key, rc.clone(), deps);
        {
            let mut st = self.st.lock().unwrap();
            if let Some(r) = st.radios.get(&phy).cloned() {
                let mut r = r;
                r.busy = Some("test");
                r.use_ = st.describe_use(&r);
                st.radios.insert(phy.clone(), r);
            }
            st.tests.insert(id.clone(), t.clone());
        }
        // Remember what to put back if BenchBob dies mid-test (recovered on next init).
        let mut b = self.borrowed();
        b.insert(phy.clone(), rc.json());
        self.set_setting("borrowed", Value::Object(b));
        self.event(&format!("Wi-Fi client test started: {} on {phy}", spec.ssid), None);
        let me = self.clone();
        let t2 = t.clone();
        tokio::spawn(async move {
            let res = t2.run().await;
            let results = {
                let mut st = me.st.lock().unwrap();
                st.tests.shift_remove(&id);
                if let Some(r) = st.radios.get_mut(&phy) {
                    r.busy = None;
                }
                st.results.insert(0, res.json());
                st.results.truncate(RESULTS_MAX);
                st.results_seq += 1;
                st.results.clone()
            };
            let mut b = me.borrowed();
            b.remove(&phy);
            me.set_setting("borrowed", Value::Object(b));
            me.set_setting("results", Value::Array(results));
            me.event(
                &format!(
                    "Wi-Fi client test {}: {} on {phy}{}",
                    if res.ok == Some(true) { "passed" } else { "failed" },
                    spec.ssid,
                    res.error.as_ref().map(|e| format!(" ({e})")).unwrap_or_default()
                ),
                None,
            );
            // As the TypeScript's back-to-back scheduleRefresh(500), scheduleRefresh(5000): the
            // second replaces the first, letting NM settle the route table.
            me.schedule_refresh(5000);
        });
        self.changed();
        Ok(t)
    }

    // ---------- profiles ----------

    /// Create or update a profile from `(key, value)` pairs; None is JavaScript's
    /// `undefined` (it clears the key when spread over the stored profile).
    pub(crate) fn save_profile(&self, p: Vec<(&str, Option<Value>)>) -> Value {
        let list = self.profiles();
        let get = |k: &str| p.iter().find(|(x, _)| *x == k).and_then(|(_, v)| v.clone());
        let pid = get("id").filter(truthy);
        let pname = get("name");
        let existing = list
            .iter()
            .find(|x| (pid.is_some() && x.get("id") == pid.as_ref()) || (x.get("name") == pname.as_ref() && pname.is_some()))
            .cloned();
        let ex = existing.as_ref().and_then(Value::as_object).cloned().unwrap_or_default();
        let mut merged: IndexMap<String, Option<Value>> = ex.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
        for (k, v) in &p {
            merged.insert(k.to_string(), v.clone());
        }
        let id = ex
            .get("id")
            .filter(|v| truthy(v))
            .cloned()
            .or(pid)
            .unwrap_or_else(|| Value::from(uuid::Uuid::new_v4().to_string()[..8].to_string()));
        merged.insert("id".into(), Some(id.clone()));
        // Keep the stored key when an edit doesn't send one (the UI never has it).
        let ppsk = p.iter().find(|(k, _)| *k == "psk").map(|(_, v)| v.clone());
        let p_psk_falsy = ppsk.as_ref().is_none_or(|v| v.as_ref().is_none_or(|v| !truthy(v)));
        let p_psk_null = matches!(&ppsk, Some(Some(Value::Null)));
        if p_psk_falsy
            && !p_psk_null
            && let Some(ep) = ex.get("psk").filter(|v| truthy(v))
        {
            merged.insert("psk".into(), Some(ep.clone()));
        }
        if p_psk_null {
            merged.shift_remove("psk");
        }
        let merged: Map<String, Value> = merged.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect();
        let mut out: Vec<Value> = list.into_iter().filter(|x| x.get("id") != Some(&id)).collect();
        out.push(Value::Object(merged.clone()));
        self.set_setting("profiles", Value::Array(out));
        self.changed();
        Value::Object(merged)
    }

    // ---------- recovery ----------

    async fn recover(&self) {
        let b = self.bins.clone();
        for l in client::recover_namespaces(&b).await {
            self.event(&l, None);
        }
        let borrowed = self.borrowed();
        for rc in borrowed.values().map(RadioCtx::from_json) {
            for iface in &rc.unmanage {
                sudo(&b, &[&b.nmcli, "dev", "set", iface, "managed", "no"], 20000).await;
            }
            for (iface, connection) in &rc.nm {
                let s = run(&[&b.nmcli, "-t", "-f", "GENERAL.STATE,GENERAL.CONNECTION", "dev", "show", iface], Opts::default()).await;
                if s.out.contains("GENERAL.STATE:100") {
                    continue;
                }
                // Give NM its usual few seconds to notice the returned interface first.
                tokio::time::sleep(Duration::from_millis(8000)).await;
                let s2 = run(&[&b.nmcli, "-t", "-f", "GENERAL.STATE", "dev", "show", iface], Opts::default()).await;
                if s2.out.contains("GENERAL.STATE:100") {
                    continue;
                }
                let up = sudo(&b, &[&b.nmcli, "con", "up", "id", connection, "ifname", iface], 60000).await;
                self.event(
                    &format!("Recovery: nmcli con up {connection} on {iface}: {}", if up.code != 0 { up.err } else { "ok".into() }),
                    None,
                );
            }
        }
        if !borrowed.is_empty() {
            self.set_setting("borrowed", Value::Object(Map::new()));
        }
    }

    fn load_oui(self: &Arc<Self>) {
        for p in [
            "/usr/share/ieee-data/oui.txt",
            "/var/lib/ieee-data/oui.txt",
            "/usr/share/misc/oui.txt",
            "/usr/share/nmap/nmap-mac-prefixes",
            "/usr/share/wireshark/manuf",
        ] {
            if !std::path::Path::new(p).exists() {
                continue;
            }
            let me = self.clone();
            tokio::spawn(async move {
                let Ok(Ok(map)) =
                    tokio::task::spawn_blocking(move || std::fs::read(p).map(|b| p::parse_oui(&String::from_utf8_lossy(&b)))).await
                else {
                    return;
                };
                let n = map.len();
                {
                    let mut st = me.st.lock().unwrap();
                    st.oui = Some(map);
                    st.oui_source = p.into();
                }
                me.log(&format!("OUI vendors: {n} from {p}"));
            });
            return;
        }
    }

    // ---------- status ----------

    pub(crate) fn status(&self) -> Value {
        let auto = self.auto_scan();
        let profiles = self.profiles().len();
        let mut st = self.st.lock().unwrap();
        let usb = st.radios.values().filter(|r| !r.onboard).count();
        let running: Vec<Arc<ClientTest>> = st.tests.values().cloned().collect();
        let active = !running.is_empty() || !st.scanning.is_empty() || usb > 0 || auto.get("enabled").is_some_and(truthy);
        let mut parts: Vec<String> = Vec::new();
        if !running.is_empty() {
            parts.push(format!(
                "testing {}",
                running.iter().map(|t| t.with_r(|r| format!("{} on {} ({})", r.ssid, r.phy, r.stage))).collect::<Vec<_>>().join(", ")
            ));
        } else if !st.scanning.is_empty() {
            parts.push(format!("scanning on {}", st.scanning.iter().cloned().collect::<Vec<_>>().join(", ")));
        }
        for r in st.radios.values() {
            let n = managed_iface(r).unwrap_or_else(|| r.phy.clone());
            if r.use_.0 == "primary" || r.use_.0 == "backup" {
                parts.push(format!("{n} {} uplink", r.use_.0));
            }
        }
        if usb > 0 {
            parts.push(format!("{usb} USB radio{}", if usb > 1 { "s" } else { "" }));
        }
        if !st.bss.is_empty() {
            parts.push(format!("{} BSS seen", st.bss.len()));
        }
        let n = st.radios.len();
        let summary = if parts.is_empty() { format!("{n} radio{}", if n == 1 { "" } else { "s" }) } else { parts.join(" · ") };
        let watching = st.watching(now_ms());
        let last = st.results.first().map(|r| {
            let g = |k: &str| r.get(k).cloned();
            O::new().o("id", g("id")).o("ok", g("ok")).o("ssid", g("ssid")).o("endedAt", g("endedAt")).o("error", g("error")).v()
        });
        O::new()
            .k("active", active)
            .k("summary", summary)
            .k("radios", st.radios.values().map(|r| st.radio_json(r, false)).collect::<Vec<_>>())
            .k("radiosSeq", st.radios_seq)
            .k(
                "reg",
                O::new()
                    .k("country", st.global_country())
                    .k("changedFrom", st.reg_changed_from.clone().map(Value::from).unwrap_or(Value::Null))
                    .v(),
            )
            .k(
                "scan",
                O::new()
                    .k("seq", st.scan.seq)
                    .k("at", st.scan.at)
                    .k("radio", st.scan.radio.clone())
                    .k("count", st.scan.count)
                    .o("error", st.scan.error.clone())
                    .k("method", st.scan.method.clone())
                    .k("durationMs", st.scan.duration_ms)
                    .k("scanning", st.scanning.iter().cloned().collect::<Vec<_>>())
                    .k("bssTotal", st.bss.len())
                    .k("auto", Value::Object(auto))
                    .k("watchers", if watching { st.watchers.len() } else { 0 })
                    .v(),
            )
            .k(
                "running",
                running
                    .iter()
                    .map(|t| {
                        t.with_r(|r| {
                            let n = r.samples.len().saturating_sub(60);
                            O::new()
                                .k("id", r.id.clone())
                                .k("phy", r.phy.clone())
                                .k("ssid", r.ssid.clone())
                                .k("stage", r.stage)
                                .k("startedAt", r.started_at)
                                .k("timeline", r.timeline.iter().map(client::Step::json).collect::<Vec<_>>())
                                .o("link", r.link.as_ref().map(p::LinkInfo::json))
                                .k("samples", r.samples[n..].iter().map(client::Sample::json).collect::<Vec<_>>())
                                .v()
                        })
                    })
                    .collect::<Vec<_>>(),
            )
            .k("resultsSeq", st.results_seq)
            .k("lastResult", last.unwrap_or(Value::Null))
            .k("profilesCount", profiles)
            .v()
    }

    async fn tick_loop(self: Arc<Self>, epoch: u64) {
        let mut refresh = tokio::time::interval(Duration::from_millis(15000));
        let mut scans = tokio::time::interval(Duration::from_millis(5000));
        refresh.tick().await;
        scans.tick().await;
        loop {
            tokio::select! {
                _ = refresh.tick() => {
                    // iw/nmcli/ip, no sudo, a few ms each: cheap enough to keep "current use" fresh.
                    if self.epoch.load(Ordering::SeqCst) != epoch { return; }
                    let me = self.clone();
                    tokio::spawn(async move { me.refresh_radios(false).await });
                }
                _ = scans.tick() => {
                    if self.epoch.load(Ordering::SeqCst) != epoch { return; }
                    self.tick_scans(false);
                }
            }
        }
    }
}

/// `n.toString(36)`
fn base36(mut n: u64) -> String {
    const D: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut v = Vec::new();
    while n > 0 {
        v.push(D[(n % 36) as usize]);
        n /= 36;
    }
    v.reverse();
    String::from_utf8(v).unwrap()
}

/// `strList(v, name)`: an array, or a string split on spaces and commas; at most 10.
pub(crate) fn str_list(v: Option<&Value>, name: &str) -> Result<Vec<String>, HttpError> {
    let items: Vec<String> = match v {
        None | Some(Value::Null) => return Ok(vec![]),
        Some(Value::String(s)) if s.is_empty() => return Ok(vec![]),
        Some(Value::Array(a)) => a.iter().map(js_string).collect(),
        Some(x) => p::re!(r"[\s,]+").split(&js_string(x)).map(String::from).collect(),
    };
    let out: Vec<String> = items.iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
    if out.len() > 10 {
        return Err(HttpError::bad_request(format!("At most 10 {name}")));
    }
    Ok(out)
}

pub(crate) fn mask_profile(p: &Value) -> Value {
    let mut m = p.as_object().cloned().unwrap_or_default();
    let has = m.shift_remove("psk").is_some_and(|v| truthy(&v));
    m.insert("hasPsk".into(), Value::Bool(has));
    Value::Object(m)
}

pub(crate) fn result_summary(r: &Value) -> Value {
    let g = |k: &str| r.get(k).cloned();
    O::new()
        .o("id", g("id"))
        .o("phy", g("phy"))
        .o("ssid", g("ssid"))
        .o("security", g("security"))
        .o("ok", g("ok"))
        .o("error", g("error"))
        .o("startedAt", g("startedAt"))
        .o("endedAt", g("endedAt"))
        .o("address", g("address"))
        .o("signal", r.get("link").and_then(|l| l.get("signal")).cloned())
        .k(
            "tests",
            r.get("tests")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|t| {
                            O::new()
                                .o("kind", t.get("kind").cloned())
                                .o("target", t.get("target").cloned())
                                .o("ok", t.get("ok").cloned())
                                .o("summary", t.get("summary").cloned())
                                .v()
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        )
        .o("restore", g("restore"))
        .v()
}

#[async_trait]
impl Driver for Wifi {
    fn id(&self) -> &'static str {
        "wifi"
    }
    fn title(&self) -> &'static str {
        "Wi-Fi"
    }
    fn order(&self) -> i32 {
        40
    }
    fn description(&self) -> &'static str {
        "Radios and regulatory channels, a band scanner with channel charts, and client tests (associate, DHCP, ping/DNS/HTTP/iperf3) in an isolated network namespace. Works with USB dongles."
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let i = &self.inner;
        let _ = i.ctx.set(ctx);
        let epoch = i.epoch.load(Ordering::SeqCst);
        {
            let mut st = i.st.lock().unwrap();
            st.stopping = false;
            st.results = match i.setting("results") {
                Some(Value::Array(a)) => a,
                _ => vec![],
            };
        }
        i.load_oui();
        i.recover().await;
        i.refresh_radios(false).await;
        i.loops.lock().unwrap().push(Task(tokio::spawn(i.clone().tick_loop(epoch))));
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) {
        let i = &self.inner;
        i.epoch.fetch_add(1, Ordering::SeqCst);
        i.refresh_gen.fetch_add(1, Ordering::SeqCst);
        i.loops.lock().unwrap().clear();
        let running: Vec<Arc<ClientTest>> = {
            let mut st = i.st.lock().unwrap();
            st.stopping = true;
            st.tests.values().cloned().collect()
        };
        for t in &running {
            t.fast.store(true, Ordering::SeqCst);
            t.cancel();
        }
        // Let cleanup give the phys back before core kills our children.
        let all = futures_join(running.iter().map(|t| {
            let t = t.clone();
            async move { t.wait_done().await }
        }));
        let _ = tokio::time::timeout(Duration::from_millis(20000), all).await;
    }

    fn udev(&self, ev: &UdevEvent) {
        let p = &ev.props;
        let g = |k: &str| p.get(k).map(String::as_str).unwrap_or("");
        if (g("SUBSYSTEM") == "ieee80211" || (g("SUBSYSTEM") == "net" && (g("DEVTYPE") == "wlan" || g("INTERFACE").starts_with("wl"))))
            && matches!(ev.action.as_str(), "add" | "remove" | "move")
        {
            self.inner.schedule_refresh(800);
        }
    }

    fn status(&self) -> Option<Json> {
        Some(self.inner.status())
    }

    fn routes(&self) -> Vec<Route> {
        routes::routes(&self.inner)
    }

    fn tools(&self) -> Vec<Tool> {
        routes::tools(&self.inner)
    }
}

/// Await every future (a tiny `join_all`, without another dependency).
async fn futures_join<F: std::future::Future<Output = ()>>(fs: impl Iterator<Item = F>) {
    for f in fs {
        f.await;
    }
}

#[cfg(test)]
mod tests;
