//! The USB panel (peripherals/drivers/usb.ts).
//!
//! Inventory tree (controllers → buses → hubs → devices, with interfaces, bound drivers and
//! the device nodes each one created), a hotplug timeline (udev plug/unplug plus the
//! USB-relevant part of the kernel log), port power switching through uhubctl, and network
//! sharing of single devices over USB/IP (usbipd) or usbredir. Pure logic lives in
//! [`model`].
//!
//! Safety model for anything that cuts power or detaches a device from this machine: work
//! out what is downstream first ([`model::plan_power`]), and refuse, or require an explicit
//! confirmation level, when that includes the root filesystem, a drive BenchBob is serving,
//! the management network adapter, an internal hub link, or sibling ports on a ganged hub.
//!
//! Shares survive a restart: devices stay bound to usbip-host when the service stops (the
//! kernel keeps established attachments), and init adopts every existing binding and starts
//! usbipd again for new clients. Only turning the panel off unshares.

pub mod model;
mod routes;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use benchbob_core::http::{js_number, js_string, truthy};
use benchbob_core::run::{Opts, read_text, run, which};
use benchbob_core::timeline::now_ms;
use benchbob_core::udev::{Props, UdevEvent, usb_name};
use benchbob_core::{Ctx, Driver, HttpError, Json, Route, StopReason, Tool};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

use crate::model::{Inventory, Peer, PowerPlan, PowerTopology, RawUsbDevice, UhubHub, UsbDevice};

const TIMELINE_IN_STATUS: usize = 100;

/// A spawned task, aborted when this handle is dropped.
struct Task(tokio::task::JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// SIGTERM, which sudo passes on to the program it runs (SIGKILL would orphan it).
fn term(pid: Option<u32>) {
    if let Some(p) = pid {
        // SAFETY: kill(2) on a child we spawned.
        unsafe {
            libc::kill(p as libc::pid_t, libc::SIGTERM);
        }
    }
}

fn hostname() -> String {
    let mut b = [0u8; 256];
    // SAFETY: gethostname writes at most b.len() bytes.
    let r = unsafe { libc::gethostname(b.as_mut_ptr().cast(), b.len()) };
    if r != 0 {
        return String::new();
    }
    let n = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

fn bin(name: &str) -> Option<String> {
    which(name).map(|p| p.to_string_lossy().into_owned())
}

/// Feed a child's output to `cb` line by line (CR stripped, blank lines skipped).
async fn lines<R: AsyncRead + Unpin>(r: Option<R>, mut cb: impl FnMut(String)) {
    let Some(r) = r else { return };
    let mut l = BufReader::new(r).lines();
    while let Ok(Some(line)) = l.next_line().await {
        let line = line.strip_suffix('\r').unwrap_or(&line).to_string();
        if !line.trim().is_empty() {
            cb(line);
        }
    }
}

/// `String(v)` for a body field, "undefined" when it's missing.
fn arg_str(v: Option<&Value>) -> String {
    v.map(js_string).unwrap_or_else(|| "undefined".into())
}

/// The usbredir server program found: usbredirect (usbredir 0.13+) or usbredirserver.
enum RedirTool {
    Direct(String),
    Server(String),
}

impl RedirTool {
    fn argv(&self, port: u16, dev: &str) -> Vec<String> {
        match self {
            RedirTool::Direct(d) => vec![d.clone(), "--device".into(), dev.into(), "--as".into(), format!("0.0.0.0:{port}")],
            RedirTool::Server(s) => vec![s.clone(), "-p".into(), port.to_string(), dev.into()],
        }
    }
}

/// A long-lived child we started (usbipd, a usbredir session).
struct Proc {
    seq: u64,
    pid: Option<u32>,
    port: u16,
    since: u64,
}

fn keep(log: &Mutex<Vec<String>>, l: String) {
    let mut v = log.lock().unwrap();
    v.push(l);
    if v.len() > 30 {
        v.remove(0);
    }
}

#[derive(Default)]
struct State {
    stopped: bool,
    model: String,
    root_source: String,
    fqdn: String,
    raw: Vec<RawUsbDevice>,
    inv: Inventory,
    protect: IndexMap<String, Vec<String>>,
    hubs: Vec<UhubHub>,
    topo: PowerTopology,
    uhub_bin: Option<String>,
    uhub_error: String,
    uhub_at: u64,
    view: Option<View>,
    pending_auto: Vec<String>,
    usbipd: Option<Proc>,
    usbipd_error: String,
    usbipd_fails: u32,
    loaded_usbip_host: bool,
    peers: Vec<Peer>,
    redir: IndexMap<String, Proc>,
    dmesg_error: String,
    /// The kernel log follower's sudo, while it runs.
    dmesg_pid: Option<u32>,
}

struct View {
    controllers: Value,
    devices: Vec<Value>,
    groups: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Rule {
    pub kind: String,
    pub value: String,
}

impl Rule {
    fn json(&self) -> Value {
        json!({ "match": self.kind, "value": self.value })
    }
    fn matches(&self, d: &UsbDevice) -> bool {
        !d.hub && if self.kind == "vidpid" { self.value == d.vidpid } else { self.value == d.busid }
    }
}

pub(crate) struct Inner {
    ctx: OnceLock<Ctx>,
    st: Mutex<State>,
    /// Where sysfs is (tests point it at a fixture tree).
    sys: PathBuf,
    rescan_lock: tokio::sync::Mutex<()>,
    /// One uhubctl at a time: a listing racing a power change would read half-switched hubs.
    uhub_lock: tokio::sync::Mutex<()>,
    seq: AtomicU64,
    /// Bumped on stop, so stale loops exit.
    epoch: AtomicU64,
    loops: Mutex<Vec<Task>>,
    rescan_timer: Mutex<Option<Task>>,
    hub_timer: Mutex<Option<Task>>,
}

/// The USB driver.
pub struct Usb {
    inner: Arc<Inner>,
}

impl Default for Usb {
    fn default() -> Self {
        Self::new()
    }
}

impl Usb {
    pub fn new() -> Self {
        Self::with_sys(PathBuf::from("/sys"))
    }

    pub fn with_sys(sys: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                ctx: OnceLock::new(),
                st: Mutex::new(State::default()),
                sys,
                rescan_lock: Default::default(),
                uhub_lock: Default::default(),
                seq: AtomicU64::new(1),
                epoch: AtomicU64::new(0),
                loops: Mutex::new(Vec::new()),
                rescan_timer: Mutex::new(None),
                hub_timer: Mutex::new(None),
            }),
        }
    }
}

pub(crate) fn label(d: &UsbDevice) -> String {
    let n: Vec<&str> = [d.vendor.as_str(), d.product.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
    if n.is_empty() { d.vidpid.clone() } else { n.join(" ") }
}

pub(crate) fn not_found(busid: &str) -> HttpError {
    HttpError::new(404, format!("No USB device \"{busid}\". Busids look like 1-1.2 (bus 1, port path 1.2); see GET /api/usb."))
}

fn usbip_err(r: &benchbob_core::run::Output) -> String {
    let s = if r.err.is_empty() { &r.out } else { &r.err };
    s.strip_prefix("usbip: error: ").unwrap_or(s).to_string()
}

/// Why a power request stopped short.
pub(crate) enum PowerErr {
    Http(HttpError),
    /// Needs confirmation: 409 with the plan.
    Needs(Box<PowerPlan>),
}

impl From<HttpError> for PowerErr {
    fn from(e: HttpError) -> Self {
        PowerErr::Http(e)
    }
}

pub(crate) fn needs_message(p: &PowerPlan) -> String {
    format!("{} Repeat with confirm: \"{}\" to go ahead.", p.warnings.join(" "), p.needs)
}

impl Inner {
    fn ctx(&self) -> &Ctx {
        self.ctx.get().expect("usb driver used before init")
    }

    fn event(&self, text: &str, detail: Value) {
        if let Some(c) = self.ctx.get() {
            c.event(text, Some(detail));
        }
    }

    fn changed(&self) {
        if let Some(c) = self.ctx.get() {
            c.changed();
        }
    }

    fn setting_num(&self, k: &str, def: f64) -> f64 {
        self.ctx.get().and_then(|c| c.setting(k)).filter(|v| !v.is_null()).map(|v| js_number(&v)).unwrap_or(def)
    }

    fn usbip_port(&self) -> u16 {
        (self.setting_num("usbipPort", 3240.0) as i64 + benchbob_core::paths::port_offset() as i64) as u16
    }

    // 4200+: the serial bridges own 4000+ (raw TCP) and 4100+ (RFC 2217).
    fn redir_base(&self) -> u16 {
        (self.setting_num("redirBasePort", 4200.0) as i64 + benchbob_core::paths::port_offset() as i64) as u16
    }

    /// Name clients should use in usbip/QEMU commands (the UI prefers the name it was reached by).
    fn host(&self) -> String {
        let f = self.st.lock().unwrap().fqdn.clone();
        if f.is_empty() { hostname() } else { f }
    }

    pub(crate) fn rules(&self) -> Vec<Rule> {
        let v = self.ctx.get().and_then(|c| c.setting("autoShare"));
        v.and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|r| {
                Some(Rule { kind: r.get("match")?.as_str()?.to_string(), value: r.get("value").map(js_string).unwrap_or_default() })
            })
            .collect()
    }

    pub(crate) fn set_rules(&self, rules: &[Rule]) {
        self.ctx().set_setting("autoShare", Value::Array(rules.iter().map(Rule::json).collect()));
    }

    fn dev(&self, busid: &str) -> Option<UsbDevice> {
        self.st.lock().unwrap().inv.get(busid).cloned()
    }

    pub(crate) fn must_dev(&self, busid: &str) -> Result<UsbDevice, HttpError> {
        self.dev(busid).ok_or_else(|| not_found(busid))
    }

    // ---------- Protection: what must not lose power or be detached ----------

    async fn compute_protect(&self, raw: &[RawUsbDevice]) -> IndexMap<String, Vec<String>> {
        // The deepest USB device whose sysfs path contains `devpath`.
        let usb_owner = |devpath: &str| -> Option<String> {
            let mut best: Option<&RawUsbDevice> = None;
            for r in raw {
                if devpath.starts_with(&format!("{}/", r.devpath)) && best.is_none_or(|b| r.devpath.len() > b.devpath.len()) {
                    best = Some(r);
                }
            }
            best.map(|b| b.name.clone())
        };
        let mut out: IndexMap<String, Vec<String>> = IndexMap::new();
        let mut add = |busid: Option<String>, why: String| {
            if let Some(b) = busid {
                let e = out.entry(b).or_default();
                if !e.contains(&why) {
                    e.push(why);
                }
            }
        };
        let ctx = self.ctx();
        let by_name: std::collections::HashMap<String, Props> = ctx
            .devices(Some(&|p: &Props| p.get("DEVNAME").is_some_and(|d| !d.is_empty())))
            .into_iter()
            .map(|p| (p["DEVNAME"].clone(), p))
            .collect();
        let usb_path = |devname: &str| by_name.get(devname).and_then(|p| p.get("DEVPATH").cloned());
        let root_source = self.st.lock().unwrap().root_source.clone();

        // Root filesystem (and its whole disk).
        if root_source.starts_with("/dev/")
            && let Some(p) = usb_path(&root_source)
        {
            add(usb_owner(&p), format!("holds the root filesystem ({root_source})"));
        }

        // Served drives: anything mounted, or exported over NBD by nfs-drive-handler.sh.
        for l in read_text("/proc/self/mounts").split('\n') {
            let mut f = l.split(' ');
            let (Some(src), mnt) = (f.next(), f.next().unwrap_or("undefined")) else { continue };
            if !src.starts_with("/dev/") || src == root_source {
                continue;
            }
            if let Some(p) = usb_path(src) {
                add(usb_owner(&p), format!("has a mounted filesystem ({src} on {})", mnt.replace("\\040", " ")));
            }
        }
        if let Ok(rd) = std::fs::read_dir("/etc/nbd-server/conf.d") {
            let mut names: Vec<String> = rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            for f in names {
                let Some(stem) = f.strip_suffix(".conf") else { continue };
                let devname = format!("/dev/{stem}");
                if let Some(p) = usb_path(&devname) {
                    add(usb_owner(&p), format!("is served over NBD ({devname})"));
                }
            }
        }

        // The adapter carrying the default route is how everyone reaches this box (USB
        // Ethernet on older Pis, or a USB NIC someone added).
        let r = run(&["ip", "-j", "route", "show", "default"], Opts::timeout_ms(3000)).await;
        if let Ok(Value::Array(mut routes)) = serde_json::from_str::<Value>(if r.out.is_empty() { "[]" } else { &r.out }) {
            routes.sort_by(|a, b| {
                let m = |x: &Value| x.get("metric").and_then(Value::as_f64).unwrap_or(0.0);
                m(a).partial_cmp(&m(b)).unwrap_or(std::cmp::Ordering::Equal)
            });
            if let Some(ifname) = routes.first().and_then(|r| r.get("dev")).and_then(Value::as_str) {
                let n = ctx.devices(Some(&|p: &Props| {
                    p.get("SUBSYSTEM").map(String::as_str) == Some("net") && p.get("INTERFACE").map(String::as_str) == Some(ifname)
                }));
                if let Some(dp) = n.first().and_then(|n| n.get("DEVPATH")) {
                    add(usb_owner(dp), format!("is the management network adapter ({ifname})"));
                }
            }
        }
        out
    }

    // ---------- Scanning ----------

    fn schedule_rescan(self: &Arc<Self>, ms: u64) {
        let me = self.clone();
        let t = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            // Detached, so a newer schedule can't abort a rescan half way.
            tokio::spawn(async move { me.rescan().await });
        });
        *self.rescan_timer.lock().unwrap() = Some(Task(t));
    }

    fn schedule_hub_refresh(self: &Arc<Self>, ms: u64) {
        if self.st.lock().unwrap().uhub_bin.is_none() {
            return;
        }
        let me = self.clone();
        let t = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            tokio::spawn(async move {
                {
                    let _g = me.uhub_lock.lock().await;
                    me.refresh_hubs().await;
                }
                me.rebuild();
                me.changed();
            });
        });
        *self.hub_timer.lock().unwrap() = Some(Task(t));
    }

    pub(crate) async fn rescan(self: &Arc<Self>) {
        {
            let _g = self.rescan_lock.lock().await;
            let sys = self.sys.clone();
            let raw = tokio::task::spawn_blocking(move || model::read_usb_sysfs(&sys)).await.unwrap_or_default();
            let ctrl: HashSet<String> = raw.iter().filter_map(|r| r.controller.as_ref().map(|c| c.devpath.clone())).collect();
            let udev = self
                .ctx()
                .devices(Some(&|p: &Props| p.get("DEVPATH").is_some_and(|d| !d.is_empty() && (d.contains("/usb") || ctrl.contains(d)))));
            let inv = model::build_inventory(&raw, &udev);
            let protect = self.compute_protect(&raw).await;
            let mut st = self.st.lock().unwrap();
            st.raw = raw;
            st.inv = inv;
            st.protect = protect;
        }
        let gone: Vec<String> = {
            let st = self.st.lock().unwrap();
            st.redir.keys().filter(|b| st.inv.get(b).is_none()).cloned().collect()
        };
        for b in gone {
            self.stop_redir(&b, "device unplugged");
        }
        self.rebuild();
        self.reconcile_usbipd();
        self.apply_rules(false);
        self.changed();
    }

    /// Call with `uhub_lock` held.
    async fn refresh_hubs(&self) {
        let Some(b) = self.st.lock().unwrap().uhub_bin.clone() else { return };
        // -f lists ganged/no-switching hubs too (shown, not switched); -N skips reading
        // device string descriptors, so listing never wakes or pokes the devices themselves.
        let r = run(&["sudo", "-n", &b, "-f", "-N"], Opts::timeout_ms(20000)).await;
        let parsed = model::parse_uhubctl(&r.out);
        let mut st = self.st.lock().unwrap();
        if !parsed.is_empty() || r.code == 0 {
            st.hubs = parsed;
            st.uhub_error.clear();
        } else {
            st.hubs.clear();
            let why = if !r.err.is_empty() {
                r.err.clone()
            } else if !r.out.is_empty() {
                r.out.clone()
            } else {
                format!("exit {}", r.code)
            };
            st.uhub_error = why.split('\n').next().unwrap_or("").to_string();
        }
        st.uhub_at = now_ms();
        st.topo = model::power_topology(&st.inv, &st.hubs, &st.model);
    }

    // ---------- The view status() returns ----------

    fn hub_power(st: &State, d: &UsbDevice) -> Value {
        if !d.hub {
            return Value::Null;
        }
        let loc = model::hub_loc(&d.busid);
        let Some(h) = st.hubs.iter().find(|x| x.loc == loc) else {
            let why = if st.uhub_bin.is_some() {
                if !st.uhub_error.is_empty() { format!("uhubctl: {}", st.uhub_error) } else { "uhubctl doesn't list this hub".into() }
            } else {
                "uhubctl isn't installed (apt: uhubctl)".into()
            };
            return json!({ "loc": loc, "switchable": false, "why": why });
        };
        let group = st.topo.groups.iter().find(|g| g.hubs.contains(&loc));
        json!({
            "loc": loc,
            "mode": h.mode,
            "switchable": h.mode != "nops",
            "why": if h.mode == "nops" { "This hub has no power switching" } else { "" },
            "group": group.map(|g| json!({ "id": g.id, "label": g.label, "hubs": g.hubs })),
            "ports": h.ports.iter().map(|p| json!({
                "port": p.port,
                "power": p.power,
                "connect": p.connect,
                "enable": p.enable,
                "suspend": p.suspend,
                "overCurrent": p.over_current,
                "speed": p.speed,
                "link": p.link,
                "internal": st.topo.internal.get(&format!("{loc}:{}", p.port)),
            })).collect::<Vec<_>>(),
        })
    }

    fn share_block(st: &State, d: &UsbDevice) -> Option<String> {
        if d.root {
            return Some("Root hubs can't be shared".into());
        }
        if d.hub {
            return Some("Hubs can't be shared; share the devices on it".into());
        }
        if let Some(p) = st.protect.get(&d.busid) {
            return Some(format!("Refused: this device {}", p.join("; ")));
        }
        if st.redir.contains_key(&d.busid) {
            return Some("In use by usbredir; stop that first".into());
        }
        None
    }

    pub(crate) fn rebuild(&self) {
        let rules = self.rules();
        let mut st = self.st.lock().unwrap();
        st.topo = model::power_topology(&st.inv, &st.hubs, &st.model);
        let st = &mut *st;
        let devices = st
            .inv
            .devices
            .iter()
            .map(|d| {
                let mut m = match serde_json::to_value(d) {
                    Ok(Value::Object(m)) => m,
                    _ => Map::new(),
                };
                m.insert("power".into(), Self::hub_power(st, d));
                m.insert(
                    "upstream".into(),
                    match &d.parent {
                        Some(p) => json!({ "hub": model::hub_loc(p), "port": d.port }),
                        None => Value::Null,
                    },
                );
                m.insert("protect".into(), json!(st.protect.get(&d.busid).cloned().unwrap_or_default()));
                m.insert("shareBlock".into(), json!(Self::share_block(st, d)));
                m.insert(
                    "redir".into(),
                    match st.redir.get(&d.busid) {
                        Some(r) => json!({
                            "port": r.port,
                            "since": r.since,
                            "peers": st.peers.iter().filter(|p| p.local == r.port).map(|p| p.peer.clone()).collect::<Vec<_>>(),
                        }),
                        None => Value::Null,
                    },
                );
                m.insert("rules".into(), json!(rules.iter().filter(|x| x.matches(d)).map(|x| x.kind.clone()).collect::<Vec<_>>()));
                Value::Object(m)
            })
            .collect();
        st.view = Some(View {
            controllers: serde_json::to_value(&st.inv.controllers).unwrap_or_default(),
            devices,
            groups: serde_json::to_value(&st.topo.groups).unwrap_or_default(),
        });
    }

    // ---------- Hotplug ----------

    async fn on_plug(self: &Arc<Self>, p: Props) {
        let devpath = p.get("DEVPATH").cloned().unwrap_or_default();
        let busid = devpath[devpath.rfind('/').map(|i| i + 1).unwrap_or(0)..].to_string();
        let speed = read_text(format!("{}{devpath}/speed", self.sys.display()));
        let (vendor, product) = usb_name(&p);
        let vidpid = format!(
            "{}:{}",
            p.get("ID_VENDOR_ID").map(String::as_str).unwrap_or(""),
            p.get("ID_MODEL_ID").map(String::as_str).unwrap_or("")
        );
        let n: Vec<&str> = [vendor.as_str(), product.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
        let name = if n.is_empty() { vidpid.clone() } else { n.join(" ") };
        let sl = model::speed_label(&speed);
        let detail = json!({ "kind": "plug", "busid": busid, "vidpid": vidpid, "speed": sl });
        if let Some(bus) = busid.strip_prefix("usb") {
            self.event(&format!("New USB bus {bus}: {name}"), detail);
        } else {
            let s = if sl.is_empty() { "speed ?".to_string() } else { sl.clone() };
            self.event(&format!("Plugged in {name} ({vidpid}) · {s} · port {busid}"), detail);
        }
        self.st.lock().unwrap().pending_auto.push(busid);
    }

    fn on_unplug(self: &Arc<Self>, p: &Props) {
        let devpath = p.get("DEVPATH").cloned().unwrap_or_default();
        let busid = devpath[devpath.rfind('/').map(|i| i + 1).unwrap_or(0)..].to_string();
        let d = self.st.lock().unwrap().inv.devices.iter().find(|x| x.devpath == devpath).cloned();
        let name = match &d {
            Some(d) => label(d),
            None => p
                .get("ID_VENDOR_ID")
                .filter(|s| !s.is_empty())
                .or_else(|| p.get("PRODUCT").filter(|s| !s.is_empty()))
                .cloned()
                .unwrap_or_else(|| "device".into()),
        };
        if let Some(bus) = busid.strip_prefix("usb") {
            self.event(&format!("USB bus {bus} removed"), json!({ "kind": "unplug", "busid": busid }));
        } else {
            let mut detail = Map::new();
            detail.insert("kind".into(), json!("unplug"));
            detail.insert("busid".into(), json!(busid));
            if let Some(d) = &d {
                detail.insert("vidpid".into(), json!(d.vidpid));
            }
            self.event(
                &format!(
                    "Unplugged {name}{} · port {busid}{}",
                    d.as_ref().map(|d| format!(" ({})", d.vidpid)).unwrap_or_default(),
                    if d.as_ref().is_some_and(|d| d.usbip.is_some()) { " · was shared over USB/IP" } else { "" }
                ),
                Value::Object(detail),
            );
        }
        if self.st.lock().unwrap().redir.contains_key(&busid) {
            self.stop_redir(&busid, "device unplugged");
        }
    }

    // ---------- Kernel log ----------

    fn start_dmesg(self: &Arc<Self>) {
        let me = self.clone();
        let epoch = self.epoch.load(Ordering::SeqCst);
        let t = tokio::spawn(async move {
            let mut fails = 0u32;
            loop {
                if me.epoch.load(Ordering::SeqCst) != epoch || me.st.lock().unwrap().stopped {
                    return;
                }
                let Some(b) = bin("dmesg") else {
                    me.st.lock().unwrap().dmesg_error = "dmesg not found".into();
                    return;
                };
                // dmesg_restrict is on, hence sudo. -W follows only *new* messages, so the boot
                // backlog never floods the timeline (GET /api/usb/dmesg shows it on demand).
                let started = now_ms();
                let mut cmd = tokio::process::Command::new("sudo");
                cmd.args(["-n", &b, "-W", "--time-format", "iso"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
                let mut child = match me.ctx().spawn(&mut cmd) {
                    Ok(c) => c,
                    Err(e) => {
                        me.st.lock().unwrap().dmesg_error = format!("kernel log follower: {e}");
                        return;
                    }
                };
                {
                    let mut st = me.st.lock().unwrap();
                    st.dmesg_error.clear();
                    st.dmesg_pid = child.child.id();
                }
                let out = child.child.stdout.take();
                let errs = child.child.stderr.take();
                let last_err = Arc::new(Mutex::new(String::new()));
                let le = last_err.clone();
                let m2 = me.clone();
                let ep = epoch;
                tokio::join!(
                    lines(out, |l| {
                        if m2.epoch.load(Ordering::SeqCst) != ep {
                            return;
                        }
                        if let Some(k) = model::filter_kernel_line(&l) {
                            m2.event(&k.text, json!({ "kind": "kernel", "level": k.level, "ts": k.ts }));
                        }
                    }),
                    lines(errs, |l| *le.lock().unwrap() = l),
                );
                let code = child.child.wait().await.ok().and_then(|s| s.code());
                me.st.lock().unwrap().dmesg_pid = None;
                if me.epoch.load(Ordering::SeqCst) != epoch || me.st.lock().unwrap().stopped {
                    return;
                }
                fails = if now_ms() - started < 5000 { fails + 1 } else { 0 };
                let err = last_err.lock().unwrap().clone();
                let msg = format!(
                    "kernel log follower exited ({}){}",
                    code.map(|c| c.to_string()).unwrap_or_else(|| "null".into()),
                    if err.is_empty() { String::new() } else { format!(": {err}") }
                );
                me.ctx().log(&msg);
                me.st.lock().unwrap().dmesg_error = msg;
                me.changed();
                if fails >= 5 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5000 * (fails as u64 + 1))).await;
            }
        });
        // Detached on purpose: stop() SIGTERMs dmesg (through sudo), and the task ends when
        // it exits. (Dropping the handle would end it the same way: see core's `child`.)
        drop(t);
    }

    // ---------- USB/IP ----------

    fn bound_count(st: &State) -> usize {
        st.raw.iter().filter(|r| r.driver == "usbip-host").count()
    }

    fn reconcile_usbipd(self: &Arc<Self>) {
        let (want, running, fails) = {
            let st = self.st.lock().unwrap();
            (!st.stopped && Self::bound_count(&st) > 0, st.usbipd.is_some(), st.usbipd_fails)
        };
        if want && !running && fails < 4 {
            self.start_usbipd();
        } else if !want && running {
            self.stop_usbipd("nothing is shared");
        }
    }

    pub(crate) fn start_usbipd(self: &Arc<Self>) {
        let Some(b) = bin("usbipd") else {
            self.st.lock().unwrap().usbipd_error = "usbipd isn't installed (apt: usbip)".into();
            return;
        };
        let port = self.usbip_port();
        // Foreground (no -D) so it's our child: stopped with us, never left orphaned.
        let mut cmd = tokio::process::Command::new("sudo");
        cmd.args(["-n", &b, "-t", &port.to_string()]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match self.ctx().spawn(&mut cmd) {
            Ok(c) => c,
            Err(e) => {
                self.st.lock().unwrap().usbipd_error = format!("usbipd: {e}");
                return;
            }
        };
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let log = Arc::new(Mutex::new(Vec::new()));
        let since = now_ms();
        {
            let mut st = self.st.lock().unwrap();
            st.usbipd = Some(Proc { seq, pid: child.child.id(), port, since });
            st.usbipd_error.clear();
        }
        self.event(&format!("USB/IP server (usbipd) started on TCP {port}"), json!({ "kind": "share" }));
        let me = self.clone();
        tokio::spawn(async move {
            let (o, e) = (child.child.stdout.take(), child.child.stderr.take());
            let (l1, l2) = (log.clone(), log.clone());
            tokio::join!(lines(o, |l| keep(&l1, l)), lines(e, |l| keep(&l2, l)));
            let code = child.child.wait().await.ok().and_then(|s| s.code());
            let msg = {
                let mut st = me.st.lock().unwrap();
                if st.usbipd.as_ref().map(|u| u.seq) != Some(seq) {
                    return; // stopped on purpose
                }
                st.usbipd = None;
                st.usbipd_fails = if now_ms() - since < 5000 { st.usbipd_fails + 1 } else { 0 };
                let l = log.lock().unwrap();
                let tail = if l.is_empty() { String::new() } else { format!(": {}", l[l.len().saturating_sub(2)..].join(" / ")) };
                st.usbipd_error = format!("usbipd exited ({}){tail}", code.map(|c| c.to_string()).unwrap_or_else(|| "null".into()));
                st.usbipd_error.clone()
            };
            me.event(&msg, json!({ "kind": "share", "level": "error" }));
            if !me.st.lock().unwrap().stopped {
                let m2 = me.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    m2.reconcile_usbipd();
                });
            }
            me.rebuild();
            me.changed();
        });
    }

    pub(crate) fn stop_usbipd(&self, why: &str) {
        let Some(u) = self.st.lock().unwrap().usbipd.take() else { return };
        term(u.pid);
        self.event(&format!("USB/IP server (usbipd) stopped: {why}"), json!({ "kind": "share" }));
    }

    async fn ensure_usbip_host(&self) -> Result<(), HttpError> {
        if self.sys.join("bus/usb/drivers/usbip-host/uevent").exists() {
            return Ok(());
        }
        let mp = bin("modprobe").unwrap_or_else(|| "/usr/sbin/modprobe".into());
        let r = run(&["sudo", "-n", &mp, "usbip-host"], Opts::timeout_ms(15000)).await;
        if r.code != 0 {
            return Err(HttpError::new(
                500,
                format!("Couldn't load the usbip-host module: {}", if r.err.is_empty() { &r.out } else { &r.err }),
            ));
        }
        self.st.lock().unwrap().loaded_usbip_host = true;
        Ok(())
    }

    fn client_help(&self, busid: &str, m: &mut Map<String, Value>) {
        let port = self.usbip_port();
        let host = self.host();
        let pf = if port == 3240 { String::new() } else { format!(" --tcp-port {port}") };
        m.insert("server".into(), json!(format!("{host}:{port}")));
        m.insert("list".into(), json!(format!("usbip{pf} list -r {host}")));
        m.insert("attach".into(), json!(format!("sudo usbip{pf} attach -r {host} -b {busid}")));
        m.insert("windows".into(), json!(format!("usbip.exe{pf} attach -r {host} -b {busid}   (usbip-win2)")));
    }

    pub(crate) async fn share(self: &Arc<Self>, busid: &str, by_rule: bool) -> Result<Value, HttpError> {
        self.rescan().await;
        let d = self.must_dev(busid)?;
        if d.usbip.is_some() {
            let mut m = Map::new();
            m.insert("ok".into(), json!(true));
            m.insert("already".into(), json!(true));
            m.insert("busid".into(), json!(busid));
            self.client_help(busid, &mut m);
            return Ok(Value::Object(m));
        }
        if let Some(block) = Self::share_block(&self.st.lock().unwrap(), &d) {
            return Err(HttpError::new(409, block));
        }
        let b = bin("usbip").ok_or_else(|| HttpError::new(501, "usbip isn't installed (apt: usbip)"))?;
        self.ensure_usbip_host().await?;
        let r = run(&["sudo", "-n", &b, "bind", "-b", busid], Opts::timeout_ms(15000)).await;
        if r.code != 0 {
            return Err(HttpError::new(409, format!("usbip bind -b {busid} failed: {}", usbip_err(&r))));
        }
        self.st.lock().unwrap().usbipd_fails = 0;
        self.event(
            &format!(
                "Shared {} ({busid}) over USB/IP{}; it's gone from this machine's drivers until unshared",
                label(&d),
                if by_rule { " by an auto-share rule" } else { "" }
            ),
            json!({ "kind": "share", "busid": busid }),
        );
        self.rescan().await;
        let mut m = Map::new();
        m.insert("ok".into(), json!(true));
        m.insert("busid".into(), json!(busid));
        self.client_help(busid, &mut m);
        Ok(Value::Object(m))
    }

    pub(crate) async fn unshare(self: &Arc<Self>, busid: &str) -> Result<Value, HttpError> {
        self.rescan().await;
        let d = self.must_dev(busid)?;
        let Some(state) = d.usbip else { return Err(HttpError::new(409, format!("{busid} isn't shared"))) };
        let b = bin("usbip").ok_or_else(|| HttpError::new(501, "usbip isn't installed (apt: usbip)"))?;
        let r = run(&["sudo", "-n", &b, "unbind", "-b", busid], Opts::timeout_ms(15000)).await;
        if r.code != 0 {
            return Err(HttpError::new(409, format!("usbip unbind -b {busid} failed: {}", usbip_err(&r))));
        }
        self.event(
            &format!(
                "Stopped sharing {} ({busid}){}",
                label(&d),
                if state == "in use" { "; the remote client was disconnected" } else { "" }
            ),
            json!({ "kind": "share", "busid": busid }),
        );
        self.rescan().await;
        Ok(json!({ "ok": true, "busid": busid }))
    }

    /// Turning the panel off is a decision to stop sharing: hand every device back.
    async fn unshare_all(&self) {
        let Some(b) = bin("usbip") else { return };
        // Read sysfs directly: rescan() would also restart usbipd and reapply rules.
        let sys = self.sys.clone();
        let bound: Vec<String> = model::read_usb_sysfs(&sys).into_iter().filter(|x| x.driver == "usbip-host").map(|x| x.name).collect();
        for name in bound {
            let r = run(&["sudo", "-n", &b, "unbind", "-b", &name], Opts::timeout_ms(15000)).await;
            if r.code == 0 {
                self.event(&format!("Stopped sharing {name}: USB panel turned off"), json!({ "kind": "share", "busid": name }));
            } else {
                self.event(
                    &format!("Couldn't stop sharing {name}: {}", usbip_err(&r)),
                    json!({ "kind": "share", "level": "error", "busid": name }),
                );
            }
        }
        let raw = model::read_usb_sysfs(&self.sys);
        self.st.lock().unwrap().raw = raw;
    }

    // ---------- Auto-share rules ----------

    pub(crate) fn apply_rules(self: &Arc<Self>, all: bool) {
        let rules = self.rules();
        let todo: Vec<String> = {
            let mut st = self.st.lock().unwrap();
            let p = std::mem::take(&mut st.pending_auto);
            if all { st.inv.devices.iter().map(|d| d.busid.clone()).collect() } else { p }
        };
        if rules.is_empty() {
            return;
        }
        for busid in todo {
            let ok = {
                let st = self.st.lock().unwrap();
                st.inv
                    .get(&busid)
                    .is_some_and(|d| d.usbip.is_none() && Self::share_block(&st, d).is_none() && rules.iter().any(|r| r.matches(d)))
            };
            if !ok {
                continue;
            }
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = me.share(&busid, true).await {
                    me.event(
                        &format!("Auto-share of {busid} failed: {}", e.message),
                        json!({ "kind": "share", "level": "error", "busid": busid }),
                    );
                }
            });
        }
    }

    // ---------- usbredir ----------

    /// usbredir 0.13 replaced usbredirserver with usbredirect (Debian trixie ships only the
    /// latter). Both take busnum-devnum, which pins this exact device (two identical adapters
    /// share a VID:PID).
    fn redir_cmd() -> Option<RedirTool> {
        bin("usbredirect").map(RedirTool::Direct).or_else(|| bin("usbredirserver").map(RedirTool::Server))
    }

    fn redir_snippet(&self, port: u16) -> (String, String) {
        let host = self.host();
        (
            format!("-chardev socket,id=redir0,host={host},port={port} -device usb-redir,chardev=redir0,id=redir0"),
            format!("<redirdev bus='usb' type='tcp'><source mode='connect' host='{host}' service='{port}'/></redirdev>"),
        )
    }

    pub(crate) fn start_redir(self: &Arc<Self>, busid: &str) -> Result<Value, HttpError> {
        let d = self.must_dev(busid)?;
        let cmd = Self::redir_cmd().ok_or_else(|| HttpError::new(501, "usbredir isn't installed (apt: usbredirect)"))?;
        let port = {
            let st = self.st.lock().unwrap();
            if let Some(r) = st.redir.get(busid) {
                return Ok(json!({ "ok": true, "already": true, "port": r.port }));
            }
            if d.usbip.is_some() {
                return Err(HttpError::new(409, "Shared over USB/IP; unshare it first (a device can only go to one remote at a time)"));
            }
            if let Some(b) = Self::share_block(&st, &d) {
                return Err(HttpError::new(409, b));
            }
            let used: HashSet<u16> = st.redir.values().map(|r| r.port).collect();
            let mut port = self.redir_base();
            while used.contains(&port) {
                port += 1;
            }
            port
        };
        let argv = cmd.argv(port, &format!("{}-{}", d.bus, d.devnum));
        let mut c = tokio::process::Command::new("sudo");
        c.arg("-n").args(&argv).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = self.ctx().spawn(&mut c).map_err(|e| HttpError::new(500, format!("usbredir: {e}")))?;
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let log = Arc::new(Mutex::new(Vec::new()));
        self.st.lock().unwrap().redir.insert(busid.to_string(), Proc { seq, pid: child.child.id(), port, since: now_ms() });
        let me = self.clone();
        let b = busid.to_string();
        tokio::spawn(async move {
            let (o, e) = (child.child.stdout.take(), child.child.stderr.take());
            let (l1, l2) = (log.clone(), log.clone());
            tokio::join!(lines(o, |l| keep(&l1, l)), lines(e, |l| keep(&l2, l)));
            let code = child.child.wait().await.ok().and_then(|s| s.code());
            {
                let mut st = me.st.lock().unwrap();
                if st.redir.get(&b).map(|r| r.seq) != Some(seq) {
                    return;
                }
                st.redir.shift_remove(&b);
            }
            let last = log.lock().unwrap().last().cloned();
            me.event(
                &format!(
                    "usbredir for {b} exited ({}){}",
                    code.map(|c| c.to_string()).unwrap_or_else(|| "null".into()),
                    last.map(|l| format!(": {l}")).unwrap_or_default()
                ),
                json!({ "kind": "share", "level": "warn", "busid": b }),
            );
            me.rebuild();
            me.changed();
        });
        self.event(&format!("usbredir serving {} ({busid}) on TCP {port}", label(&d)), json!({ "kind": "share", "busid": busid }));
        self.rebuild();
        self.changed();
        let (qemu, libvirt) = self.redir_snippet(port);
        Ok(json!({ "ok": true, "busid": busid, "port": port, "qemu": qemu, "libvirt": libvirt }))
    }

    pub(crate) fn stop_redir(&self, busid: &str, why: &str) -> bool {
        let Some(r) = self.st.lock().unwrap().redir.shift_remove(busid) else { return false };
        term(r.pid);
        self.event(&format!("usbredir for {busid} {why}"), json!({ "kind": "share", "busid": busid }));
        true
    }

    // ---------- Peers / live usbip state ----------

    async fn poll(self: &Arc<Self>) {
        let ports: Vec<u16> = {
            let mut st = self.st.lock().unwrap();
            if st.usbipd.is_none() && st.redir.is_empty() && Self::bound_count(&st) == 0 {
                st.peers.clear();
                return;
            }
            st.usbipd.iter().map(|u| u.port).chain(st.redir.values().map(|r| r.port)).collect()
        };
        let r = run(&["ss", "-tnH", "state", "established"], Opts::timeout_ms(3000)).await;
        let next = model::parse_ss_peers(&r.out, &ports);
        let mut dirty = false;
        let (prev, usbipd_port) = {
            let st = self.st.lock().unwrap();
            (st.peers.clone(), st.usbipd.as_ref().map(|u| u.port))
        };
        for p in &next {
            if !prev.contains(p) {
                let what = if Some(p.local) == usbipd_port { "USB/IP" } else { "usbredir" };
                self.event(&format!("Remote client {} connected to {what} :{}", p.peer, p.local), json!({ "kind": "share" }));
                dirty = true;
            }
        }
        for p in &prev {
            if !next.contains(p) {
                self.event(&format!("Remote client {} disconnected from :{}", p.peer, p.local), json!({ "kind": "share" }));
                dirty = true;
            }
        }
        // A remote attach/detach changes usbip_status without any udev event.
        let bound: Vec<(String, String, Option<i64>)> = {
            let mut st = self.st.lock().unwrap();
            st.peers = next;
            st.raw.iter().filter(|x| x.driver == "usbip-host").map(|x| (x.name.clone(), x.devpath.clone(), x.usbip_status)).collect()
        };
        for (name, devpath, was) in bound {
            let s = benchbob_core::http::parse_int(&read_text(format!("{}{devpath}/usbip_status", self.sys.display()))).unwrap_or(0);
            if Some(s) != was {
                let mut st = self.st.lock().unwrap();
                if let Some(x) = st.raw.iter_mut().find(|x| x.name == name) {
                    x.usbip_status = Some(s);
                }
                if let Some(d) = st.inv.devices.iter_mut().find(|d| d.busid == name) {
                    d.usbip = Some(match s {
                        2 => "in use",
                        3 => "error",
                        _ => "available",
                    });
                }
                dirty = true;
            }
        }
        if dirty {
            self.rebuild();
            self.changed();
        }
    }

    // ---------- Power ----------

    fn resolve_power_target(&self, a: &Value) -> Result<model::PowerReq, HttpError> {
        let action = a
            .get("action")
            .and_then(Value::as_str)
            .and_then(model::PowerAction::parse)
            .ok_or_else(|| HttpError::bad_request("action must be \"on\", \"off\" or \"cycle\""))?;
        if let Some(b) = a.get("busid").filter(|v| truthy(v)) {
            let d = self.must_dev(&js_string(b))?;
            let (Some(parent), Some(port)) = (&d.parent, d.port) else {
                return Err(HttpError::bad_request(format!("{} is a root hub; switch its ports instead", d.busid)));
            };
            return Ok(model::PowerReq { hub: model::hub_loc(parent), port: Some(port as i64), action });
        }
        let hub = a.get("hub").filter(|v| !v.is_null()).ok_or_else(|| {
            HttpError::bad_request("Give hub (uhubctl location, e.g. 1-1 or 2) and optionally port, or busid of the device to switch")
        })?;
        let port = match a.get("port") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.is_empty() => None,
            Some(v) => {
                let n = js_number(v);
                if !(n.is_finite() && n.fract() == 0.0) {
                    return Err(HttpError::bad_request("port must be an integer"));
                }
                Some(n as i64)
            }
        };
        Ok(model::PowerReq { hub: js_string(hub), port, action })
    }

    pub(crate) async fn plan(self: &Arc<Self>, a: &Value) -> Result<PowerPlan, HttpError> {
        let t = self.resolve_power_target(a)?;
        if self.st.lock().unwrap().uhub_bin.is_none() {
            return Err(HttpError::new(501, "uhubctl isn't installed (apt: uhubctl)"));
        }
        self.rescan().await;
        {
            let _g = self.uhub_lock.lock().await;
            self.refresh_hubs().await;
        }
        self.rebuild();
        let st = self.st.lock().unwrap();
        model::plan_power(&st.inv, &st.hubs, &st.topo, &st.protect, &t).map_err(HttpError::bad_request)
    }

    pub(crate) async fn power(self: &Arc<Self>, a: &Value) -> Result<(PowerPlan, String), PowerErr> {
        let p = self.plan(a).await?;
        let confirm = a.get("confirm").and_then(Value::as_str).unwrap_or("");
        let confirmed = p.needs == "none" || confirm == "unsafe" || (p.needs == "ganged" && confirm == "ganged");
        if !confirmed {
            return Err(PowerErr::Needs(Box::new(p)));
        }
        let d = a.get("delay").map(js_number).filter(|n| *n != 0.0 && !n.is_nan()).unwrap_or(2.0);
        let delay = d.clamp(1.0, 30.0);
        let uh = self.st.lock().unwrap().uhub_bin.clone().unwrap_or_default();
        let mut outputs = Vec::new();
        {
            let _g = self.uhub_lock.lock().await;
            let exec = |cmd: Vec<String>| {
                let uh = uh.clone();
                async move {
                    let mut argv = vec!["sudo".to_string(), "-n".into(), uh];
                    argv.extend(cmd.iter().cloned());
                    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                    let r = run(&refs, Opts::timeout_ms(20000)).await;
                    let out = format!(
                        "$ uhubctl {}\n{}{}",
                        cmd.join(" "),
                        r.out,
                        if r.err.is_empty() { String::new() } else { format!("\n{}", r.err) }
                    );
                    if r.code != 0 {
                        return Err(HttpError::new(
                            500,
                            format!("uhubctl {} failed: {}", cmd.join(" "), if r.err.is_empty() { &r.out } else { &r.err }),
                        ));
                    }
                    Ok(out)
                }
            };
            if p.action == model::PowerAction::Cycle {
                let half = p.commands.len() / 2;
                for c in &p.commands[..half] {
                    outputs.push(exec(c.clone()).await?);
                }
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                for c in &p.commands[half..] {
                    outputs.push(exec(c.clone()).await?);
                }
            } else {
                for c in &p.commands {
                    outputs.push(exec(c.clone()).await?);
                }
            }
            self.refresh_hubs().await;
        }
        let what = match (&p.group, p.port) {
            (Some(g), _) => format!("{} ({} ports, ganged)", g.label.split(':').next().unwrap_or(""), p.ports.len()),
            (None, Some(port)) => format!("hub {} port {port}", p.hub),
            (None, None) => format!("all ports of hub {}", p.hub),
        };
        self.event(
            &format!(
                "Power {}{}: {what}{}",
                p.action.as_str(),
                if p.action == model::PowerAction::Cycle { format!(" ({} s)", model::js_num_str(delay)) } else { String::new() },
                if p.devices.is_empty() { String::new() } else { format!(" · affects {}", p.devices.join(", ")) }
            ),
            json!({ "kind": "power", "hub": p.hub, "port": p.port, "action": p.action.as_str() }),
        );
        self.schedule_rescan(500);
        Ok((p, outputs.join("\n\n")))
    }

    // ---------- Flattened list (REST + MCP) ----------

    pub(crate) fn flat_list(&self) -> Value {
        let st = self.st.lock().unwrap();
        let controllers: Vec<Value> =
            st.inv.controllers.iter().map(|c| json!({ "id": c.id, "name": c.name, "driver": c.driver, "buses": c.buses })).collect();
        let empty = Vec::new();
        let devices: Vec<Value> = st
            .view
            .as_ref()
            .map(|v| &v.devices)
            .unwrap_or(&empty)
            .iter()
            .map(|d| {
                let s = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                let mut m = Map::new();
                m.insert("busid".into(), d["busid"].clone());
                m.insert("depth".into(), d["depth"].clone());
                m.insert("parent".into(), d["parent"].clone());
                m.insert("port".into(), d["port"].clone());
                let n: Vec<String> = [s("vendor"), s("product")].into_iter().filter(|x| !x.is_empty()).collect();
                m.insert("name".into(), json!(if n.is_empty() { s("vidpid") } else { n.join(" ") }));
                m.insert("vidpid".into(), d["vidpid"].clone());
                if !s("serial").is_empty() {
                    m.insert("serial".into(), d["serial"].clone());
                }
                m.insert("speed".into(), d["speedLabel"].clone());
                m.insert("usb".into(), d["version"].clone());
                m.insert("maxPowerMa".into(), d["maxPowerMa"].clone());
                if d["hub"] == json!(true) {
                    let pw = &d["power"];
                    let power = if truthy(&pw["switchable"]) {
                        format!("{}{}", js_string(&pw["mode"]), if truthy(&pw["group"]) { " (ganged group)" } else { "" })
                    } else {
                        "not switchable".into()
                    };
                    m.insert("hub".into(), json!({ "ports": d["nports"], "power": power }));
                }
                let ifs: Vec<String> = d["interfaces"]
                    .as_array()
                    .unwrap_or(&Vec::new())
                    .iter()
                    .map(|i| {
                        format!(
                            "{} {} → {}",
                            js_string(&i["id"]),
                            js_string(&i["clsName"]),
                            i["driver"].as_str().filter(|s| !s.is_empty()).unwrap_or("no driver")
                        )
                    })
                    .collect();
                m.insert("interfaces".into(), json!(ifs));
                let nodes: Vec<String> = d["nodes"]
                    .as_array()
                    .unwrap_or(&Vec::new())
                    .iter()
                    .map(|n| {
                        format!(
                            "{}{}",
                            js_string(&n["dev"]),
                            n.get("panel")
                                .and_then(Value::as_str)
                                .filter(|p| !p.is_empty())
                                .map(|p| format!(" ({p} panel)"))
                                .unwrap_or_default()
                        )
                    })
                    .collect();
                m.insert("nodes".into(), json!(nodes));
                if let Some(h) = d["hint"].get("label") {
                    m.insert("hint".into(), h.clone());
                }
                if truthy(&d["usbip"]) {
                    m.insert("usbip".into(), d["usbip"].clone());
                }
                if let Some(p) = d["redir"].get("port") {
                    m.insert("redir".into(), json!(format!("TCP {p}")));
                }
                if d["protect"].as_array().is_some_and(|a| !a.is_empty()) {
                    m.insert("protected".into(), d["protect"].clone());
                }
                Value::Object(m)
            })
            .collect();
        json!({ "controllers": controllers, "devices": devices })
    }

    pub(crate) fn timeline_entries(&self, limit: usize, kinds: Option<&[String]>) -> Value {
        let all = self.ctx().hub.timeline.list();
        let t: Vec<_> = all
            .into_iter()
            .filter(|e| match kinds {
                None => true,
                Some(k) => {
                    let kind = e.detail.as_ref().and_then(|d| d.get("kind")).and_then(Value::as_str);
                    kind.is_some_and(|x| k.iter().any(|y| y == x))
                }
            })
            .collect();
        let start = t.len().saturating_sub(limit);
        let v: Vec<Value> = t[start..].iter().rev().map(|e| serde_json::to_value(e).unwrap_or_default()).collect();
        Value::Array(v)
    }

    // ---------- Status ----------

    pub(crate) fn status(&self, with_timeline: bool) -> Value {
        let (summary, active, host, view_parts, uhub, usbip, redir) = {
            let st = self.st.lock().unwrap();
            let Some(view) = &st.view else { return json!({ "active": false, "summary": "Scanning…" }) };
            let devices = st.inv.devices.iter().filter(|d| !d.hub).count();
            let buses = st.inv.devices.iter().filter(|d| d.root).count();
            let shared = Self::bound_count(&st);
            let mut parts =
                vec![if devices > 0 { format!("{devices} device{}", if devices > 1 { "s" } else { "" }) } else { "No devices".into() }];
            if shared > 0 {
                parts.push(format!("{shared} shared"));
            }
            if !st.redir.is_empty() {
                parts.push(format!("{} usbredir", st.redir.len()));
            }
            if st.inv.devices.iter().any(|d| d.ports.iter().any(|p| p.over_current != 0)) {
                parts.push("over-current seen".into());
            }
            if devices == 0 {
                parts.push(format!("{buses} bus{}", if buses == 1 { "" } else { "es" }));
            }
            let active = devices > 0 || st.usbipd.is_some() || !st.redir.is_empty();
            let usbip = json!({
                "available": bin("usbip").is_some() && bin("usbipd").is_some(),
                "running": st.usbipd.is_some(),
                "port": self.usbip_port(),
                "since": st.usbipd.as_ref().map(|u| u.since),
                "error": st.usbipd_error,
                "peers": match &st.usbipd {
                    Some(u) => st.peers.iter().filter(|p| p.local == u.port).map(|p| p.peer.clone()).collect::<Vec<_>>(),
                    None => Vec::new(),
                },
                "shared": shared,
            });
            let redir = json!({ "available": Self::redir_cmd().is_some(), "apt": "usbredirect", "basePort": self.redir_base(), "sessions": st.redir.len() });
            let uhub = json!({ "available": st.uhub_bin.is_some(), "error": st.uhub_error, "at": st.uhub_at });
            let fq = if st.fqdn.is_empty() { hostname() } else { st.fqdn.clone() };
            (
                parts.join(" · "),
                active,
                fq,
                (view.controllers.clone(), Value::Array(view.devices.clone()), view.groups.clone()),
                uhub,
                usbip,
                redir,
            )
        };
        let dmesg_error = self.st.lock().unwrap().dmesg_error.clone();
        let mut m = Map::new();
        m.insert("active".into(), json!(active));
        m.insert("summary".into(), json!(summary));
        m.insert("host".into(), json!(host));
        m.insert("controllers".into(), view_parts.0);
        m.insert("devices".into(), view_parts.1);
        m.insert("groups".into(), view_parts.2);
        m.insert("uhubctl".into(), uhub);
        m.insert("usbip".into(), usbip);
        m.insert("redir".into(), redir);
        m.insert("rules".into(), Value::Array(self.rules().iter().map(Rule::json).collect()));
        m.insert("kernelLog".into(), json!({ "error": dmesg_error }));
        if with_timeline {
            m.insert("timeline".into(), self.timeline_entries(TIMELINE_IN_STATUS, None));
        }
        Value::Object(m)
    }
}

#[async_trait]
impl Driver for Usb {
    fn id(&self) -> &'static str {
        "usb"
    }
    fn title(&self) -> &'static str {
        "USB"
    }
    fn order(&self) -> i32 {
        10
    }
    fn description(&self) -> &'static str {
        "USB device tree, hotplug timeline, port power switching (uhubctl), and sharing devices over the network (USB/IP, usbredir)"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let i = &self.inner;
        let _ = i.ctx.set(ctx);
        let model = read_text("/proc/device-tree/model").replace('\0', "");
        let root = run(&["findmnt", "-no", "SOURCE", "/"], Opts::timeout_ms(5000)).await.out;
        let fqdn = run(&["hostname", "-f"], Opts::timeout_ms(3000)).await.out;
        {
            let mut st = i.st.lock().unwrap();
            st.stopped = false;
            st.uhub_bin = bin("uhubctl");
            st.model = model;
            st.root_source = root.trim().to_string();
            st.fqdn = fqdn.trim().to_string();
            st.usbipd_fails = 0;
        }
        // Devices still bound to usbip-host from before a restart are adopted here: the
        // rescan sees them and starts usbipd for them.
        i.rescan().await;
        {
            let _g = i.uhub_lock.lock().await;
            i.refresh_hubs().await;
        }
        i.rebuild();
        i.start_dmesg();
        i.apply_rules(true);
        let me = i.clone();
        let epoch = i.epoch.load(Ordering::SeqCst);
        let t = tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs(3));
            iv.tick().await;
            loop {
                iv.tick().await;
                if me.epoch.load(Ordering::SeqCst) != epoch {
                    return;
                }
                me.poll().await;
            }
        });
        i.loops.lock().unwrap().push(Task(t));
        i.changed();
        Ok(())
    }

    async fn stop(&self, reason: StopReason) {
        let i = &self.inner;
        i.st.lock().unwrap().stopped = true;
        i.epoch.fetch_add(1, Ordering::SeqCst);
        i.loops.lock().unwrap().clear();
        *i.rescan_timer.lock().unwrap() = None;
        *i.hub_timer.lock().unwrap() = None;
        // On a restart, devices stay bound to usbip-host: established USB/IP attachments live
        // in the kernel and survive, and the next init restarts usbipd for new clients.
        // Turning the panel off is a decision to stop sharing, so hand the devices back.
        if reason == StopReason::Disable {
            i.unshare_all().await;
        }
        let (usbipd, redirs, dmesg, unload) = {
            let mut st = i.st.lock().unwrap();
            let u = st.usbipd.take();
            let r: Vec<Proc> = st.redir.drain(..).map(|(_, p)| p).collect();
            let unload = st.loaded_usbip_host && Inner::bound_count(&st) == 0;
            (u, r, st.dmesg_pid, unload)
        };
        // SIGTERM our sudo children so sudo passes it on (and their tasks reap them).
        term(dmesg);
        term(usbipd.and_then(|u| u.pid));
        for r in redirs {
            term(r.pid);
        }
        if unload {
            let mp = bin("modprobe").unwrap_or_else(|| "/usr/sbin/modprobe".into());
            run(&["sudo", "-n", &mp, "-r", "usbip-host"], Opts::timeout_ms(10000)).await;
            i.st.lock().unwrap().loaded_usbip_host = false;
        }
    }

    fn udev(&self, ev: &UdevEvent) {
        let p = &ev.props;
        if !p.get("DEVPATH").is_some_and(|d| d.contains("/usb")) {
            return;
        }
        let i = &self.inner;
        if p.get("SUBSYSTEM").map(String::as_str) == Some("usb") && p.get("DEVTYPE").map(String::as_str) == Some("usb_device") {
            if ev.action == "add" {
                let me = i.clone();
                let p = p.clone();
                tokio::spawn(async move { me.on_plug(p).await });
            } else if ev.action == "remove" {
                i.on_unplug(p);
            }
            if ev.action == "add" || ev.action == "remove" {
                i.schedule_hub_refresh(1000);
            }
        }
        i.schedule_rescan(250);
    }

    fn status(&self) -> Option<Json> {
        Some(self.inner.status(true))
    }

    fn routes(&self) -> Vec<Route> {
        routes::routes(&self.inner)
    }

    fn tools(&self) -> Vec<Tool> {
        routes::tools(&self.inner)
    }
}
