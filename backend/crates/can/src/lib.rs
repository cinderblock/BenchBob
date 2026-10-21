//! The CAN panel (peripherals/drivers/can.ts).
//!
//! Every SocketCAN netdev (USB adapters such as gs_usb candleLight/CANable 2, peak_usb,
//! slcan, vcan) gets: bit-timing config through `ip link`, a safe listen-only auto-bitrate,
//! live stats (frames/s, bus load, controller state, error counters), a per-ID table, a frame
//! log, send/periodic TX, candump-format recording, a WebSocket stream, a socketcand-protocol
//! TCP server (python-can, SavvyCAN, Kayak) with optional UDP discovery beacon, and MCP
//! tools.
//!
//! Traffic comes from one raw socket per interface (the hub, see [`hub`]) opened with
//! RECV_OWN_MSGS and every error class, so a single reader feeds stats, the UI, captures and
//! recordings. Frames we sent come back through it once the controller confirms them and are
//! tagged dir:"tx". Browsers get a 4 Hz ID-table snapshot plus batched frames over the
//! WebSocket, never one SSE push per frame.

pub mod frame;
pub mod hub;
pub mod iplink;
pub mod json;
mod ops;
mod routes;
pub mod sock;
mod socketcand;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use benchbob_core::http::{js_number, js_string, truthy};
use benchbob_core::run::{Opts, run};
use benchbob_core::udev::{Props, UdevEvent};
use benchbob_core::{Ctx, Driver, HttpError, Json, Route, Socket, StopReason, Tool};
use futures_util::FutureExt;
use futures_util::future::Shared;
use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::frame::fmt_bitrate;
use crate::hub::Hub;
pub use crate::iplink::Iface;
use crate::json::{O, num};

pub(crate) const SOCKETCAND_PORT: u16 = 29536;
pub(crate) const BEACON_PORT: u16 = 42000;
pub(crate) const MIN_PERIOD_MS: f64 = 5.0;

/// Serial adapters running SLCAN (Lawicel ASCII) firmware, by USB VID:PID.
pub(crate) fn slcan_usb(vidpid: &str) -> Option<&'static str> {
    match vidpid {
        "16d0:117e" => Some("CANable (slcan firmware)"),
        "0403:ffa8" => Some("Lawicel CANUSB"),
        _ => None,
    }
}

/// slcand -s<N> codes, by bitrate (ascending, as JavaScript lists integer keys).
pub(crate) const SLCAN_SPEED: [(u32, u8); 9] =
    [(10000, 0), (20000, 1), (50000, 2), (100000, 3), (125000, 4), (250000, 5), (500000, 6), (800000, 7), (1000000, 8)];

/// A spawned task, aborted when this handle is dropped.
pub struct Task(pub tokio::task::JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) fn ip_bin() -> &'static str {
    static IP: OnceLock<&'static str> = OnceLock::new();
    IP.get_or_init(|| if std::path::Path::new("/usr/sbin/ip").exists() { "/usr/sbin/ip" } else { "ip" })
}

pub(crate) struct Slcan {
    pub name: String,
    pub tty: String,
    pub tty_key: String,
    pub label: String,
    pub bitrate: f64,
    pub pid: Option<u32>,
    pub since: u64,
    /// Set once slcand exits.
    pub exited: tokio::sync::watch::Receiver<Option<i32>>,
    pub generation: u64,
}

pub(crate) struct AbProgress {
    pub step: usize,
    pub of: usize,
    pub bitrate: f64,
    pub trials: Vec<frame::BitrateTrial>,
}

pub(crate) fn trial_json(t: &frame::BitrateTrial) -> Value {
    O::new()
        .k("bitrate", num(t.bitrate))
        .k("frames", t.frames)
        .k("errors", t.errors)
        .k("busErrors", t.bus_errors)
        .o("skipped", t.skipped.clone())
        .v()
}

impl AbProgress {
    fn json(&self) -> Value {
        O::new()
            .k("step", self.step)
            .k("of", self.of)
            .k("bitrate", num(self.bitrate))
            .k("trials", self.trials.iter().map(trial_json).collect::<Vec<_>>())
            .v()
    }
}

#[derive(Default)]
pub(crate) struct State {
    pub ifaces: IndexMap<String, Iface>,
    /// iface → what's running (configuring / auto-bitrate)
    pub busy: HashMap<String, String>,
    pub hubs: IndexMap<String, Hub>,
    pub ab_progress: HashMap<String, AbProgress>,
    pub ab_last: HashMap<String, Value>,
    /// by tty
    pub slcans: IndexMap<String, Slcan>,
    pub recordings: Vec<Value>,
    pub last_sig: String,
    pub initial_scan: bool,
    pub tick_n: u64,
    pub capture_seq: u64,
}

impl State {
    pub fn hub_for(&mut self, name: &str) -> Result<&mut Hub, HttpError> {
        if !self.ifaces.contains_key(name) {
            return Err(HttpError::new(404, format!("No CAN interface \"{name}\"")));
        }
        self.hubs.get_mut(name).ok_or_else(|| HttpError::new(404, format!("No CAN interface \"{name}\"")))
    }
    /// hubFor(name).send(frame)
    pub fn send(&mut self, name: &str, f: &frame::CanFrame) -> Result<(), HttpError> {
        self.hub_for(name)?;
        self.hubs[name].send(self.ifaces.get(name), f)
    }
    pub fn iface(&self, name: &str) -> Result<&Iface, HttpError> {
        self.ifaces.get(name).ok_or_else(|| HttpError::new(404, format!("No CAN interface \"{name}\"")))
    }
}

type RefreshFut = Shared<Pin<Box<dyn Future<Output = ()> + Send>>>;

pub(crate) struct Inner {
    pub ctx: OnceLock<Ctx>,
    pub st: Mutex<State>,
    refreshing: Mutex<Option<RefreshFut>>,
    /// Bumped on stop, so stale loops exit.
    pub epoch: AtomicU64,
    pub rec_dir: PathBuf,
    pub sc: Mutex<socketcand::Sc>,
    pub tick_wake: tokio::sync::Notify,
    pub job_seq: AtomicU64,
    pub slcan_seq: AtomicU64,
    loops: Mutex<Vec<Task>>,
}

/// The CAN driver.
pub struct Can {
    inner: Arc<Inner>,
}

impl Default for Can {
    fn default() -> Self {
        Self::new()
    }
}

impl Can {
    pub fn new() -> Self {
        let rec_dir =
            benchbob_core::paths::env("BENCHBOB_CAN_REC_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/var/tmp/benchbob-can"));
        Self::with_rec_dir(rec_dir)
    }

    pub fn with_rec_dir(rec_dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                ctx: OnceLock::new(),
                st: Mutex::new(State::default()),
                refreshing: Mutex::new(None),
                epoch: AtomicU64::new(0),
                rec_dir,
                sc: Mutex::new(socketcand::Sc::default()),
                tick_wake: tokio::sync::Notify::new(),
                job_seq: AtomicU64::new(1),
                slcan_seq: AtomicU64::new(1),
                loops: Mutex::new(Vec::new()),
            }),
        }
    }
}

/// Vendor and product for display: prefers the database names, falls back to the
/// descriptor strings (core.ts `usbName`).
pub(crate) fn usb_name(p: &Props) -> (String, String) {
    let dec = |k: &str| {
        let s = p.get(k).map(String::as_str).unwrap_or("");
        let mut out = String::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\'
                && i + 3 < b.len()
                && b[i + 1] == b'x'
                && let Ok(v) = u8::from_str_radix(&s[i + 2..i + 4], 16)
            {
                out.push(v as char);
                i += 4;
                continue;
            }
            out.push(b[i] as char);
            i += 1;
        }
        out.trim().to_string()
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

pub(crate) fn hostname() -> String {
    let mut b = [0u8; 256];
    // SAFETY: gethostname writes at most b.len() bytes.
    let r = unsafe { libc::gethostname(b.as_mut_ptr().cast(), b.len()) };
    if r != 0 {
        return String::new();
    }
    let n = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

impl Inner {
    pub fn ctx(&self) -> &Ctx {
        self.ctx.get().expect("can driver used before init")
    }

    pub fn setting(&self, k: &str) -> Option<Value> {
        self.ctx.get().and_then(|c| c.setting(k))
    }

    pub fn saved_cfg(&self, key: &str) -> Option<Value> {
        self.setting("ifaces").and_then(|v| v.get(key).cloned()).filter(|v| !v.is_null())
    }

    pub fn save_cfg(&self, key: &str, c: &iplink::CanCfg) {
        let mut all = match self.setting("ifaces") {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let mut e = match all.get(key) {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        if let Value::Object(c) = c.to_json() {
            for (k, v) in c {
                e.insert(k, v);
            }
        }
        e.shift_remove("up");
        all.insert(key.into(), Value::Object(e));
        self.ctx().set_setting("ifaces", Value::Object(all));
    }

    // ---------- interfaces ----------

    fn net_props(&self, name: &str) -> Option<Props> {
        self.ctx()
            .devices(Some(&|p: &Props| {
                p.get("SUBSYSTEM").map(String::as_str) == Some("net") && p.get("INTERFACE").map(String::as_str) == Some(name)
            }))
            .into_iter()
            .next()
    }

    /// Stable identity for persisted config: USB serial (+channel) beats USB path beats the name.
    fn enrich(&self, i: &mut Iface, dev_port: &str, slc: Option<(String, String, f64, String)>) {
        let np = self.net_props(&i.name);
        let usb =
            np.as_ref().and_then(|np| self.ctx().ancestor(np.get("DEVPATH").map(String::as_str).unwrap_or(""), "usb", Some("usb_device")));
        if i.kind == "can" {
            let drv = std::fs::read_link(format!("/sys/class/net/{}/device/driver", i.name))
                .ok()
                .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
                .unwrap_or_default();
            i.driver = if !drv.is_empty() {
                drv
            } else if slc.is_some() {
                "slcan".into()
            } else {
                String::new()
            };
        }
        if let Some((tty, tty_key, bitrate, label)) = slc {
            i.slcan_tty = Some(tty);
            // slcand sets the speed with an "S<n>" command the kernel never sees: it reports bitrate 0.
            if i.bitrate.is_none() {
                i.bitrate = Some(bitrate).filter(|b| *b != 0.0);
            }
            i.key = format!("slcan:{tty_key}");
            i.label = label;
        } else if let Some(usb) = usb {
            let (vendor, model) = usb_name(&usb);
            let g = |k: &str| usb.get(k).cloned().unwrap_or_default();
            let vidpid = format!(
                "{}:{}",
                usb.get("ID_VENDOR_ID").map(String::as_str).unwrap_or("undefined"),
                usb.get("ID_MODEL_ID").map(String::as_str).unwrap_or("undefined")
            );
            let path = np.as_ref().and_then(|n| n.get("ID_PATH").cloned()).unwrap_or_default();
            let chan = if !dev_port.is_empty() && dev_port != "0" { format!("#{dev_port}") } else { String::new() };
            let serial = g("ID_SERIAL_SHORT");
            i.key = if !serial.is_empty() {
                format!("usb:{vidpid}:{serial}{chan}")
            } else if !path.is_empty() {
                format!("path:{path}{chan}")
            } else {
                format!("if:{}", i.name)
            };
            i.label = [vendor.as_str(), model.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ");
            i.usb = Some(iplink::Usb { vendor, model, serial, vidpid, path });
        } else {
            i.key = if i.kind == "can" { format!("if:{}", i.name) } else { format!("{}:{}", i.kind, i.name) };
            i.label = match i.kind.as_str() {
                "vcan" => "Virtual CAN".into(),
                "vxcan" => "Virtual CAN tunnel".into(),
                _ => i.driver.clone(),
            };
        }
    }

    /// Re-read every CAN interface (single flight: callers during a refresh share it).
    pub fn refresh(self: &Arc<Self>) -> RefreshFut {
        let mut r = self.refreshing.lock().unwrap();
        if let Some(f) = &*r {
            return f.clone();
        }
        let me = self.clone();
        let fut: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(async move {
            me.do_refresh().await;
            *me.refreshing.lock().unwrap() = None;
        });
        let f = fut.shared();
        *r = Some(f.clone());
        f
    }

    async fn do_refresh(self: &Arc<Self>) {
        if self.ctx.get().is_none() {
            return;
        }
        let r = run(&[ip_bin(), "-details", "-statistics", "-json", "link", "show"], Opts::timeout_ms(5000)).await;
        let arr: Vec<Value> = serde_json::from_str(if r.out.is_empty() { "[]" } else { &r.out }).unwrap_or_default();
        let mut found = Vec::new();
        for l in &arr {
            let Some(mut i) = iplink::parse_ip_link(l) else { continue };
            let slc = {
                let st = self.st.lock().unwrap();
                st.slcans.values().find(|s| s.name == i.name).map(|s| (s.tty.clone(), s.tty_key.clone(), s.bitrate, s.label.clone()))
            };
            let dev_port = benchbob_core::run::read_text(format!("/sys/class/net/{}/dev_port", i.name));
            self.enrich(&mut i, &dev_port, slc);
            found.push(i);
        }
        let changed = {
            let mut st = self.st.lock().unwrap();
            let mut seen = Vec::new();
            for i in found {
                seen.push(i.name.clone());
                let name = i.name.clone();
                let prev = st.ifaces.insert(name.clone(), i);
                if prev.is_none() {
                    self.on_appear(&mut st, &name);
                }
            }
            let gone: Vec<String> = st.ifaces.keys().filter(|k| !seen.contains(k)).cloned().collect();
            for g in gone {
                self.on_gone(&mut st, &g);
            }
            let names: Vec<String> = st.ifaces.keys().cloned().collect();
            for n in names {
                self.sync_hub(&mut st, &n);
            }
            let sig = st
                .ifaces
                .values()
                .map(|i| {
                    format!(
                        "{}:{}:{}:{}:{}:{}:{}:{}",
                        i.name,
                        i.up,
                        i.state_str().unwrap_or_else(|| "undefined".into()),
                        i.bitrate.map(fmt_num).unwrap_or_else(|| "undefined".into()),
                        i.dbitrate.map(fmt_num).unwrap_or_else(|| "undefined".into()),
                        i.ctrl.join(","),
                        i.mtu,
                        i.restart_ms.map(fmt_num).unwrap_or_else(|| "undefined".into())
                    )
                })
                .collect::<Vec<_>>()
                .join("|");
            if sig != st.last_sig {
                st.last_sig = sig;
                true
            } else {
                false
            }
        };
        if changed {
            self.ctx().changed();
        }
    }

    fn on_appear(self: &Arc<Self>, st: &mut State, name: &str) {
        if !st.hubs.contains_key(name) {
            st.hubs.insert(name.into(), Hub::new(name));
        }
        let i = st.ifaces[name].clone();
        if !st.initial_scan {
            let label = if i.label.is_empty() { String::new() } else { format!(" ({})", i.label) };
            self.ctx().event(&format!("CAN interface {name} appeared{label}"), Some(O::new().k("iface", name).k("key", i.key.clone()).v()));
        }
        // Apply saved config on hotplug, and at startup for adapters still down after a reboot.
        if let Some(cfg) = self.saved_cfg(&i.key)
            && cfg.get("bringUpOnPlug").is_some_and(truthy)
            && !i.up
        {
            let me = self.clone();
            let name = name.to_string();
            let mut raw = cfg.clone();
            raw["up"] = Value::Bool(true);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                match ops::configure_boxed(me.clone(), name.clone(), raw, false).await {
                    Ok(_) => me.ctx().event(&format!("{name}: applied saved config and brought up"), None),
                    Err(e) => me.ctx().event(&format!("{name}: saved config failed: {}", e.message), None),
                }
            });
        }
    }

    fn on_gone(&self, st: &mut State, name: &str) {
        st.ifaces.shift_remove(name);
        if let Some(mut hub) = st.hubs.shift_remove(name) {
            hub.close_sock();
            if let Some(r) = hub.stop_recording() {
                self.rec_stopped(name, &r, "interface removed");
            }
            hub.jobs.clear();
            for ws in hub.clients.values() {
                ws.client
                    .send(serde_json::to_string(&O::new().k("type", "error").k("message", format!("{name} was removed")).v()).unwrap());
                ws.client.close();
            }
            self.ctx().changed();
        }
        self.ctx().event(&format!("CAN interface {name} removed"), Some(O::new().k("iface", name).v()));
    }

    pub fn sync_hub(self: &Arc<Self>, st: &mut State, name: &str) {
        if !st.hubs.contains_key(name) {
            st.hubs.insert(name.into(), Hub::new(name));
        }
        if st.busy.contains_key(name) {
            return;
        }
        let up = st.ifaces.get(name).is_some_and(|i| i.up);
        let hub = st.hubs.get_mut(name).unwrap();
        if up && hub.sock.is_none() {
            self.open_hub(hub, up);
        } else if !up && hub.sock.is_some() {
            hub.close_sock();
        }
    }

    fn open_hub(self: &Arc<Self>, hub: &mut Hub, up: bool) {
        hub.sock_gen += 1;
        let generation = hub.sock_gen;
        let name = hub.name.clone();
        let (w1, w2) = (Arc::downgrade(self), Arc::downgrade(self));
        let (n1, n2) = (name.clone(), name.clone());
        let opts =
            sock::Options { fd_frames: true, err_mask: frame::CAN_ERR_MASK, recv_own: true, rcvbuf: Some(1 << 20), ..Default::default() };
        let r = sock::CanSocket::open(
            &name,
            &opts,
            Box::new(move |frames| {
                let Some(me) = w1.upgrade() else { return };
                let mut st = me.st.lock().unwrap();
                let st = &mut *st;
                let i = st.ifaces.get(&n1);
                let Some(hub) = st.hubs.get_mut(&n1).filter(|h| h.sock_gen == generation) else { return };
                if let Some((why, _)) = hub.on_frames(i, frames)
                    && let Some(r) = hub.stop_recording()
                {
                    me.rec_stopped(&n1, &r, &why);
                }
            }),
            Some(Box::new(move |err| {
                let Some(me) = w2.upgrade() else { return };
                let present = {
                    let mut st = me.st.lock().unwrap();
                    if let Some(h) = st.hubs.get_mut(&n2).filter(|h| h.sock_gen == generation) {
                        // Dropping the socket here aborts this very reader task once it returns.
                        h.sock = None;
                    }
                    st.ifaces.contains_key(&n2)
                };
                if present {
                    me.ctx().log(&format!("{n2}: reader stopped: {err}"));
                }
                // The interface went down (ENETDOWN) or vanished; the poll reopens when it's back.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    me.refresh().await;
                });
            })),
        );
        match r {
            Ok(s) => {
                hub.sock = Some(s);
                hub.open_err.clear();
            }
            Err(e) => {
                if up {
                    self.ctx().log(&format!("{name}: {e}"));
                }
                hub.open_err = e;
            }
        }
    }

    pub fn rec_stopped(&self, name: &str, r: &hub::Rec, why: &str) {
        self.ctx().event(
            &format!(
                "{name}: recording {} stopped ({} frames){}",
                r.file,
                r.frames,
                if why.is_empty() { String::new() } else { format!(": {why}") }
            ),
            None,
        );
        self.refresh_recordings();
    }

    pub fn refresh_recordings(&self) {
        let mut list: Vec<(String, u64, f64)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.rec_dir) {
            for e in rd.flatten() {
                let f = e.file_name().to_string_lossy().into_owned();
                if !f.ends_with(".log") {
                    continue;
                }
                let Ok(m) = std::fs::metadata(e.path()) else { continue };
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as f64 * 1000.0 + d.subsec_nanos() as f64 / 1e6)
                    .unwrap_or(0.0);
                list.push((f, m.len(), mtime));
            }
        }
        list.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        self.st.lock().unwrap().recordings =
            list.into_iter().map(|(f, s, m)| O::new().k("file", f).k("size", s).k("mtime", num(m)).v()).collect();
        if let Some(c) = self.ctx.get() {
            c.changed();
        }
    }

    // ---------- output shapes ----------

    pub fn iface_out(&self, st: &State, i: &Iface) -> Value {
        let hub = st.hubs.get(&i.name);
        let cfg = self.saved_cfg(&i.key).unwrap_or_else(|| Value::Object(Map::new()));
        let reader = match hub {
            Some(h) if h.sock.is_some() => "open".to_string(),
            Some(h) if !h.open_err.is_empty() => h.open_err.clone(),
            _ => (if i.up { "starting" } else { "down" }).to_string(),
        };
        let usb = i.usb.as_ref().map(|u| {
            O::new()
                .k("vendor", u.vendor.clone())
                .k("model", u.model.clone())
                .k("serial", u.serial.clone())
                .k("vidpid", u.vidpid.clone())
                .k("path", u.path.clone())
                .v()
        });
        let s = &i.stats;
        O::new()
            .k("name", i.name.clone())
            .k("kind", i.kind.clone())
            .k("driver", i.driver.clone())
            .k("label", i.label.clone())
            .k("key", i.key.clone())
            .o("usb", usb)
            .o("slcanTty", i.slcan_tty.clone())
            .k("up", i.up)
            .o("state", i.state.clone())
            .o("berr", i.berr.clone().map(|(tx, rx)| O::new().k("tx", tx).k("rx", rx).v()))
            .on("bitrate", i.bitrate)
            .on("samplePoint", i.sample_point)
            .on("dbitrate", i.dbitrate)
            .on("dsamplePoint", i.dsample_point)
            .k("fdCapable", i.fd_capable)
            .k("fdOn", i.fd_on)
            .k("ctrl", i.ctrl.clone())
            .k("ctrlSupported", i.ctrl_supported.clone().map(Value::from).unwrap_or(Value::Null))
            .on("restartMs", i.restart_ms)
            .on("clock", i.clock)
            .o("bitrateConst", i.bitrate_const.clone())
            .o("xstats", i.xstats.clone())
            .k(
                "stats",
                O::new()
                    .k("rxPackets", s.rx_packets.clone())
                    .k("txPackets", s.tx_packets.clone())
                    .k("rxErrors", s.rx_errors.clone())
                    .k("txErrors", s.tx_errors.clone())
                    .k("rxDropped", s.rx_dropped.clone())
                    .k("txDropped", s.tx_dropped.clone())
                    .k("rxBytes", s.rx_bytes.clone())
                    .k("txBytes", s.tx_bytes.clone())
                    .v(),
            )
            .k("bringUpOnPlug", cfg.get("bringUpOnPlug").is_some_and(truthy))
            .k("saved", cfg)
            .k("busy", st.busy.get(&i.name).cloned().map(Value::from).unwrap_or(Value::Null))
            .k("autobitrate", st.ab_progress.get(&i.name).map(AbProgress::json).unwrap_or(Value::Null))
            .k("lastAutobitrate", st.ab_last.get(&i.name).cloned().unwrap_or(Value::Null))
            .k("reader", reader)
            .o("rates", hub.map(|h| h.rates.json()))
            .o("totals", hub.map(|h| h.tot.json()))
            .k("drops", hub.map(|h| h.drops()).unwrap_or(0))
            .k("idCount", hub.map(|h| h.ids.len()).unwrap_or(0))
            .k("jobs", hub.map(|h| h.jobs.values().map(hub::Job::json).collect::<Vec<_>>()).unwrap_or_default())
            .k("recording", hub.map(|h| h.rec_json()).unwrap_or(Value::Null))
            .k("wsClients", hub.map(|h| h.clients.len()).unwrap_or(0))
            .v()
    }

    /// ifaceOut for a name, or the near-empty object the TypeScript makes when slcand's
    /// interface hasn't shown up in `ip` yet.
    pub fn iface_out_named(&self, name: &str) -> Value {
        let st = self.st.lock().unwrap();
        match st.ifaces.get(name) {
            Some(i) => self.iface_out(&st, i),
            None => O::new()
                .k("name", name)
                .k("bringUpOnPlug", false)
                .k("saved", Value::Object(Map::new()))
                .k("busy", Value::Null)
                .k("autobitrate", Value::Null)
                .k("lastAutobitrate", Value::Null)
                .k("reader", "down")
                .k("drops", 0)
                .k("idCount", 0)
                .k("jobs", Vec::<Value>::new())
                .k("recording", Value::Null)
                .k("wsClients", 0)
                .v(),
        }
    }

    pub fn slcan_candidates(&self, st: &State) -> Vec<Value> {
        self.ctx()
            .devices(Some(&|p: &Props| {
                p.get("SUBSYSTEM").map(String::as_str) == Some("tty")
                    && p.get("DEVNAME").is_some_and(|d| !d.is_empty())
                    && slcan_usb(&vidpid(p)).is_some()
            }))
            .into_iter()
            .map(|p| {
                let (vendor, model) = usb_name(&p);
                let tty = p["DEVNAME"].clone();
                let by_id =
                    p.get("DEVLINKS").map(|l| l.split(' ').find(|x| x.contains("/by-id/")).unwrap_or("").to_string()).unwrap_or_default();
                O::new()
                    .k("tty", tty.clone())
                    .k("byId", by_id)
                    .k("kind", slcan_usb(&vidpid(&p)).unwrap())
                    .k("vendor", vendor)
                    .k("model", model)
                    .k("serial", p.get("ID_SERIAL_SHORT").cloned().unwrap_or_default())
                    .k("attached", st.slcans.get(&tty).map(|s| Value::from(s.name.clone())).unwrap_or(Value::Null))
                    .v()
            })
            .collect()
    }

    pub fn list(&self) -> Value {
        let st = self.st.lock().unwrap();
        O::new()
            .k("ifaces", st.ifaces.values().map(|i| self.iface_out(&st, i)).collect::<Vec<_>>())
            .k(
                "slcan",
                O::new()
                    .k("candidates", self.slcan_candidates(&st))
                    .k(
                        "attached",
                        st.slcans
                            .values()
                            .map(|s| {
                                O::new()
                                    .k("name", s.name.clone())
                                    .k("tty", s.tty.clone())
                                    .k("bitrate", num(s.bitrate))
                                    .k("since", s.since)
                                    .v()
                            })
                            .collect::<Vec<_>>(),
                    )
                    .v(),
            )
            .k("socketcand", self.socketcand_out())
            .k("recordings", st.recordings.clone())
            .v()
    }

    // ---------- timers ----------

    /// 50 ms while a WebSocket client watches (frame batches, stats snapshots); otherwise once
    /// a second is plenty for rates, and an idle Pi isn't woken 20 times a second.
    async fn tick_loop(self: Arc<Self>, epoch: u64) {
        loop {
            let watching = self.st.lock().unwrap().hubs.values().any(|h| !h.clients.is_empty());
            let ms = if watching { 50 } else { 1000 };
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
                _ = self.tick_wake.notified(), if !watching => {}
            }
            if self.epoch.load(Ordering::SeqCst) != epoch {
                return;
            }
            self.tick(if watching { 1 } else { 20 });
        }
    }

    fn tick(self: &Arc<Self>, units: u64) {
        let mut st = self.st.lock().unwrap();
        let st = &mut *st;
        let before = st.tick_n;
        st.tick_n += units;
        let now_n = st.tick_n;
        let crossed = |every: u64| now_n / every > before / every;
        // 50 ms: flush batched frames.
        for hub in st.hubs.values_mut() {
            for ws in hub.clients.values_mut() {
                if !ws.batch || (ws.q.is_empty() && ws.dropped == 0) {
                    continue;
                }
                if ws.client.buffered() > 2 << 20 {
                    ws.dropped += ws.q.len() as u64; // a slow browser: drop rather than buffer without bound
                    ws.q.clear();
                    continue;
                }
                let frames: Vec<Value> = ws.q.drain(..).map(|f| f.json()).collect();
                ws.client
                    .send(serde_json::to_string(&O::new().k("type", "frames").k("frames", frames).k("dropped", ws.dropped).v()).unwrap());
                ws.dropped = 0;
            }
        }
        // 250 ms: stats snapshots.
        if crossed(5) {
            for hub in st.hubs.values() {
                let mut snap: Option<String> = None;
                for ws in hub.clients.values() {
                    if !ws.stats {
                        continue;
                    }
                    let s = snap.get_or_insert_with(|| {
                        serde_json::to_string(&json::spread(O::new().k("type", "stats").v(), hub.snapshot(st.ifaces.get(&hub.name), true)))
                            .unwrap()
                    });
                    ws.client.send(s.clone());
                }
            }
        }
        // 1 s: rates; poll `ip` for controller state and error counters while a real
        // controller is up. vcan has neither, so otherwise every 5 s is enough to notice
        // links changed from a shell.
        if crossed(20) {
            let names: Vec<String> = st.hubs.keys().cloned().collect();
            for n in names {
                let i = st.ifaces.get(&n);
                st.hubs.get_mut(&n).unwrap().tick(i);
            }
            let ctrl_up = st.ifaces.values().any(|i| i.up && i.kind == "can");
            // With no interfaces at all, udev hotplug is what notices a new one.
            if ctrl_up || (!st.ifaces.is_empty() && crossed(100)) {
                let me = self.clone();
                tokio::spawn(async move { me.refresh().await });
            }
        }
    }

    pub fn status(&self) -> Value {
        let sc = self.socketcand_out();
        let st = self.st.lock().unwrap();
        let cands = self.slcan_candidates(&st);
        let clients = sc["clients"].as_array().map(Vec::len).unwrap_or(0);
        let busiest: f64 = st.ifaces.values().filter(|i| i.up).map(|i| st.hubs.get(&i.name).map(|h| h.rates.fps).unwrap_or(0.0)).sum();
        let summary = if st.ifaces.is_empty() {
            if cands.is_empty() {
                "No CAN interfaces".to_string()
            } else {
                format!("{} SLCAN adapter{} (not attached)", cands.len(), if cands.len() > 1 { "s" } else { "" })
            }
        } else {
            let mut s = st
                .ifaces
                .values()
                .map(|i| {
                    let state = i
                        .state_str()
                        .filter(|s| !s.is_empty() && s != "ERROR-ACTIVE")
                        .map(|s| format!(" {}", s.to_lowercase()))
                        .unwrap_or_default();
                    format!(
                        "{} {}{state}",
                        i.name,
                        if i.up { i.bitrate.map(fmt_bitrate).unwrap_or_else(|| "up".into()) } else { "down".into() }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            if busiest != 0.0 {
                s += &format!(" · {} fps", fmt_num(busiest));
            }
            if clients > 0 {
                s += &format!(" · {clients} socketcand");
            }
            s
        };
        O::new()
            .k("active", !st.ifaces.is_empty() || !cands.is_empty() || clients > 0)
            .k("summary", summary)
            .k("ifaces", st.ifaces.values().map(|i| self.iface_out(&st, i)).collect::<Vec<_>>())
            .k(
                "slcan",
                O::new()
                    .k("candidates", cands)
                    .k(
                        "attached",
                        st.slcans
                            .values()
                            .map(|s| O::new().k("name", s.name.clone()).k("tty", s.tty.clone()).k("bitrate", num(s.bitrate)).v())
                            .collect::<Vec<_>>(),
                    )
                    .v(),
            )
            .k("socketcand", sc)
            .k("recordings", st.recordings.iter().take(20).cloned().collect::<Vec<_>>())
            .k("recDir", self.rec_dir.display().to_string())
            .k("standardBitrates", frame::STD_BITRATES.to_vec())
            .k("slcanBitrates", SLCAN_SPEED.iter().map(|(b, _)| *b).collect::<Vec<_>>())
            .v()
    }
}

pub(crate) fn vidpid(p: &Props) -> String {
    format!(
        "{}:{}",
        p.get("ID_VENDOR_ID").map(String::as_str).unwrap_or("undefined"),
        p.get("ID_MODEL_ID").map(String::as_str).unwrap_or("undefined")
    )
}

/// A number as JavaScript prints it in a template string.
pub(crate) fn fmt_num(f: f64) -> String {
    js_string(&num(f))
}

/// `Number(x)` of a JSON argument, NaN when missing.
pub(crate) fn arg_num(a: &Value, k: &str) -> f64 {
    a.get(k).map(js_number).unwrap_or(f64::NAN)
}

/// `String(x)` of a JSON argument, "undefined" when missing.
pub(crate) fn arg_str(a: &Value, k: &str) -> String {
    a.get(k).map(js_string).unwrap_or_else(|| "undefined".into())
}

#[async_trait]
impl Driver for Can {
    fn id(&self) -> &'static str {
        "can"
    }
    fn title(&self) -> &'static str {
        "CAN bus"
    }
    fn order(&self) -> i32 {
        30
    }
    fn description(&self) -> &'static str {
        "SocketCAN adapters (candleLight/CANable, PEAK, SLCAN, vcan): bitrate config and auto-detect, live traffic, recording, socketcand server for python-can/SavvyCAN, MCP tools"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let i = &self.inner;
        let _ = i.ctx.set(ctx);
        // Recordings made before the rename: move them over once (best effort; /var/tmp is sticky).
        if benchbob_core::paths::env("BENCHBOB_CAN_REC_DIR").is_none()
            && std::path::Path::new("/var/tmp/deskypi-can").exists()
            && !i.rec_dir.exists()
        {
            let _ = std::fs::rename("/var/tmp/deskypi-can", &i.rec_dir);
        }
        let epoch = i.epoch.load(Ordering::SeqCst);
        i.st.lock().unwrap().initial_scan = true;
        i.refresh().await;
        i.st.lock().unwrap().initial_scan = false;
        i.refresh_recordings();
        i.start_socketcand().await;
        i.start_beacon(epoch).await;
        i.loops.lock().unwrap().push(Task(tokio::spawn(i.clone().tick_loop(epoch))));
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) {
        let i = &self.inner;
        i.epoch.fetch_add(1, Ordering::SeqCst);
        i.loops.lock().unwrap().clear();
        i.stop_beacon();
        i.stop_socketcand().await;
        let slcans: Vec<Slcan> = {
            let mut st = i.st.lock().unwrap();
            let names: Vec<String> = st.hubs.keys().cloned().collect();
            for n in names {
                let hub = st.hubs.get_mut(&n).unwrap();
                hub.jobs.clear();
                let r = hub.stop_recording();
                hub.close_sock();
                for ws in hub.clients.values() {
                    ws.client.close();
                }
                if let Some(r) = r {
                    i.ctx().event(&format!("{n}: recording {} stopped ({} frames): driver stopped", r.file, r.frames), None);
                }
            }
            let s = std::mem::take(&mut st.slcans).into_values().collect();
            *st = State::default();
            s
        };
        for s in slcans {
            if let Some(p) = s.pid {
                // SAFETY: kill(2) on the sudo/slcand child we spawned.
                unsafe { libc::kill(p as libc::pid_t, libc::SIGTERM) };
            }
        }
    }

    fn udev(&self, ev: &UdevEvent) {
        let p = &ev.props;
        let i = self.inner.clone();
        if p.get("SUBSYSTEM").map(String::as_str) == Some("net") && matches!(ev.action.as_str(), "add" | "remove" | "move") {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                i.refresh().await;
            });
        } else if p.get("SUBSYSTEM").map(String::as_str) == Some("tty")
            && let Some(kind) = slcan_usb(&vidpid(p))
        {
            if ev.action == "add" {
                i.ctx().event(&format!("SLCAN adapter on {} ({kind})", p.get("DEVNAME").map(String::as_str).unwrap_or("undefined")), None);
            }
            i.ctx().changed();
        }
    }

    fn status(&self) -> Option<Json> {
        Some(self.inner.status())
    }

    fn routes(&self) -> Vec<Route> {
        routes::routes(&self.inner)
    }

    fn sockets(&self) -> Vec<Socket> {
        routes::sockets(&self.inner)
    }

    fn tools(&self) -> Vec<Tool> {
        routes::tools(&self.inner)
    }
}

/// Tests that share the test vcan run one at a time.
#[cfg(test)]
pub(crate) static VCAN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests;
