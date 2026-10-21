//! The UPS panel, ported from `peripherals/drivers/ups.ts`.
//!
//! A USB UPS plugged into BenchBob is driven by NUT, run entirely as our own children: the
//! right driver (usbhid-ups, nutdrv_qx… whatever NUT's udev table says) plus upsd, with
//! config and state in a private temp dir. The system NUT units stay masked and /etc/nut is
//! never read, so nothing here survives BenchBob stopping. `upsc` is polled every 5 s, the
//! status flags decoded into words, charge/load kept for sparklines, and power events logged
//! to the hardware timeline. "Share on network" opens upsd on 0.0.0.0 so NUT clients
//! elsewhere (Home Assistant, upsmon on other servers) can follow the same UPS.
//!
//! Automatic shutdown on low battery is deliberately not configured (no upsmon).
//!
//! Testing without hardware: `BENCHBOB_UPS_SIMULATE=/path/to.dev` runs NUT's dummy-ups on
//! that file; edit `ups.status:` in it to drive OL → OB → LB. Only that variable creates a
//! fake UPS.

pub mod nut;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::Method;
use benchbob_core::http::{HttpError, js_number, js_string, json as reply, num, truthy};
use benchbob_core::paths::{env, port_offset};
use benchbob_core::proc::{LineFn, Proc, SIGTERM, Split};
use benchbob_core::run::{Opts, read_text, run};
use benchbob_core::timeline::now_ms;
use benchbob_core::udev::{Props, UdevEvent};
use benchbob_core::util::{js_round, prop, usb_name};
use benchbob_core::{Ctx, Driver, Json, Route, StopReason, Tool};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio::process::Command;

use crate::nut::{
    UpsId, UpsSection, builtin_ids, decode_status, fmt_runtime, is_power_device_descriptor, match_ups, parse_nut_udev_rules, parse_upsc,
    status_events, ups_conf, upsd_conf, upsd_users,
};

const NUT: &str = "/lib/nut";
const RULES: &str = "/lib/udev/rules.d/62-nut-usbups.rules";
const POLL: Duration = Duration::from_secs(5);
/// 10 minutes at 5 s.
const HIST_N: usize = 120;
const MON_USER: &str = "monuser";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Where things are; from the environment, or set by tests.
#[derive(Clone)]
pub struct Config {
    /// Private NUT config and state: `/tmp/benchbob-nut-<PORT>`.
    pub dir: PathBuf,
    /// `BENCHBOB_UPS_SIMULATE`: a dummy-ups .dev file.
    pub sim: String,
    pub port_offset: u16,
    /// NUT's programs (`/lib/nut`).
    pub nut: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            dir: PathBuf::from(format!("/tmp/benchbob-nut-{}", env("PORT").unwrap_or_else(|| "80".into()))),
            sim: env("BENCHBOB_UPS_SIMULATE").unwrap_or_default(),
            port_offset: port_offset(),
            nut: PathBuf::from(NUT),
        }
    }
}

struct UpsDev {
    /// Unique per detection, so a timer can tell whether it's still the same device.
    uid: u64,
    /// Stable id: udev ID_SERIAL, else USB port path; "sim" for the simulator.
    key: String,
    /// NUT section name: benchbob, benchbob-2…
    name: String,
    driver: String,
    vid: String,
    pid: String,
    vendor: String,
    product: String,
    serial: String,
    bus: String,
    usb_port: String,
    simulated: bool,
    proc: Option<Arc<Proc>>,
    driver_log: Arc<Mutex<Vec<String>>>,
    vars: IndexMap<String, String>,
    status: String,
    reachable: bool,
    error: String,
    last_poll: u64,
    charge: Vec<f64>,
    load: Vec<f64>,
    events: Vec<(u64, String)>,
}

#[derive(Default)]
struct St {
    table: Vec<UpsId>,
    upses: IndexMap<String, UpsDev>,
    /// hid DEVPATH → descriptor says Power Device.
    power_hid: HashMap<String, bool>,
    upsd: Option<Arc<Proc>>,
    upsd_log: Arc<Mutex<Vec<String>>>,
    next_uid: u64,
}

struct Inner {
    cfg: Config,
    ctx: OnceLock<Ctx>,
    st: Mutex<St>,
    stopping: AtomicBool,
    epoch: AtomicU64,
    rescan_gen: AtomicU64,
    restart_gen: AtomicU64,
    restart_lock: tokio::sync::Mutex<()>,
}

pub struct Ups {
    inner: Arc<Inner>,
}

impl Default for Ups {
    fn default() -> Self {
        Self::new()
    }
}

impl Ups {
    pub fn new() -> Self {
        Self::with_config(Config::from_env())
    }

    pub fn with_config(cfg: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                cfg,
                ctx: OnceLock::new(),
                st: Mutex::new(St { table: builtin_ids().to_vec(), ..Default::default() }),
                stopping: AtomicBool::new(false),
                epoch: AtomicU64::new(0),
                rescan_gen: AtomicU64::new(0),
                restart_gen: AtomicU64::new(0),
                restart_lock: tokio::sync::Mutex::new(()),
            }),
        }
    }
}

/// `tail()`: keep the last 20 meaningful lines of a NUT program's output.
fn tail(buf: &Mutex<Vec<String>>, line: &str) {
    let l = line.trim();
    if l.is_empty() || l.contains("upsnotify") || l.contains("Init SSL") {
        return;
    }
    let mut b = lock(buf);
    b.push(l.to_string());
    if b.len() > 20 {
        let n = b.len() - 20;
        b.drain(..n);
    }
}

fn last(buf: &Mutex<Vec<String>>, n: usize) -> Vec<String> {
    let b = lock(buf);
    b[b.len().saturating_sub(n)..].to_vec()
}

/// `num()`: a NUT value as a number, None when empty or not numeric.
fn nut_num(v: Option<&String>) -> Option<f64> {
    let v = v?;
    if v.is_empty() {
        return None;
    }
    let n = js_number(&Value::String(v.clone()));
    n.is_finite().then_some(n)
}

fn opt_num(n: Option<f64>) -> Value {
    n.map(num).unwrap_or(Value::Null)
}

impl Inner {
    fn ctx(&self) -> &Ctx {
        self.ctx.get().expect("ups driver not initialised")
    }

    fn running(&self) -> bool {
        !self.stopping.load(Ordering::SeqCst)
    }

    // ---------- settings ----------

    fn nut_port(&self) -> Value {
        self.ctx().setting("port").unwrap_or_else(|| json!(3493 + self.cfg.port_offset as u32))
    }

    fn shared(&self) -> Value {
        self.ctx().setting("share").unwrap_or(Value::Bool(false))
    }

    fn mon_password(&self) -> String {
        match self.ctx().setting("monPassword") {
            Some(v) if truthy(&v) => js_string(&v),
            _ => {
                let pw: String = uuid::Uuid::new_v4().simple().to_string().chars().take(16).collect();
                self.ctx().set_setting("monPassword", Value::String(pw.clone()));
                pw
            }
        }
    }

    // ---------- detection ----------

    fn hid_is_power(&self, p: &Props) -> bool {
        let dp = prop(p, "DEVPATH").to_string();
        if let Some(v) = lock(&self.st).power_hid.get(&dp) {
            return *v;
        }
        let yes = std::fs::read(format!("/sys{dp}/report_descriptor")).map(|d| is_power_device_descriptor(&d)).unwrap_or(false);
        lock(&self.st).power_hid.insert(dp, yes);
        yes
    }

    fn blank(&self, key: String, driver: String, usb: Option<&Props>) -> UpsDev {
        let (vendor, product, vid, pid, serial, bus, usb_port, simulated) = match usb {
            Some(usb) => {
                let (vendor, model) = usb_name(usb);
                let dp = prop(usb, "DEVPATH");
                (
                    vendor,
                    model,
                    prop(usb, "ID_VENDOR_ID").to_string(),
                    prop(usb, "ID_MODEL_ID").to_string(),
                    prop(usb, "ID_SERIAL_SHORT").to_string(),
                    format!("{:0>3}", prop(usb, "BUSNUM")),
                    dp.rsplit('/').next().unwrap_or("").to_string(),
                    false,
                )
            }
            None => {
                ("BenchBob".into(), "Simulated UPS".into(), String::new(), String::new(), String::new(), String::new(), String::new(), true)
            }
        };
        UpsDev {
            uid: 0,
            key,
            name: String::new(),
            driver,
            vid,
            pid,
            vendor,
            product,
            serial,
            bus,
            usb_port,
            simulated,
            proc: None,
            driver_log: Default::default(),
            vars: IndexMap::new(),
            status: String::new(),
            reachable: false,
            error: String::new(),
            last_poll: 0,
            charge: Vec::new(),
            load: Vec::new(),
            events: Vec::new(),
        }
    }

    fn detect(&self) -> IndexMap<String, UpsDev> {
        let ctx = self.ctx();
        let mut found: IndexMap<String, UpsDev> = IndexMap::new();
        let add = |found: &mut IndexMap<String, UpsDev>, usb: &Props, id: Option<UpsId>| {
            let key = match prop(usb, "ID_SERIAL") {
                "" => format!("port:{}", prop(usb, "DEVPATH").rsplit('/').next().unwrap_or("")),
                s => s.to_string(),
            };
            if found.contains_key(&key) {
                return;
            }
            let mut driver = id.as_ref().map(|i| i.driver.clone()).unwrap_or_else(|| "usbhid-ups".into());
            // e.g. apc_modbus isn't built into Debian's NUT
            if !self.cfg.nut.join(&driver).exists() {
                driver = "usbhid-ups".into();
            }
            found.insert(key.clone(), self.blank(key, driver, Some(usb)));
        };
        let table = lock(&self.st).table.clone();
        for usb in ctx.devices(Some(&|p: &Props| prop(p, "SUBSYSTEM") == "usb" && prop(p, "DEVTYPE") == "usb_device")) {
            if let Some(id) = match_ups(prop(&usb, "ID_VENDOR_ID"), prop(&usb, "ID_MODEL_ID"), &table) {
                add(&mut found, &usb, Some(id.clone()));
            }
        }
        // Vendors nobody listed: any HID interface whose report descriptor is the Power Device class.
        for hid in ctx.devices(Some(&|p: &Props| prop(p, "SUBSYSTEM") == "hid")) {
            if !self.hid_is_power(&hid) {
                continue;
            }
            if let Some(usb) = ctx.ancestor(prop(&hid, "DEVPATH"), "usb", Some("usb_device")) {
                add(&mut found, &usb, None);
            }
        }
        if !self.cfg.sim.is_empty() {
            let u = self.blank("sim".into(), "dummy-ups".into(), None);
            found.insert("sim".into(), u);
        }
        found
    }

    async fn rescan(self: &Arc<Self>) {
        let found = self.detect();
        let found_keys: Vec<String> = found.keys().cloned().collect();
        let ctx = self.ctx();
        let mut changed = false;
        let mut gone = Vec::new();
        {
            let mut st = lock(&self.st);
            for (key, mut u) in found {
                if st.upses.contains_key(&key) {
                    continue;
                }
                // Names are stable per run: the first UPS is "benchbob", as clients elsewhere expect.
                let mut name = "benchbob".to_string();
                let mut i = 2;
                while st.upses.values().any(|x| x.name == name) {
                    name = format!("benchbob-{i}");
                    i += 1;
                }
                u.name = name;
                st.next_uid += 1;
                u.uid = st.next_uid;
                ctx.event(
                    &format!(
                        "UPS connected: {} {} ({}, NUT driver {})",
                        u.vendor,
                        u.product,
                        if u.simulated { "simulated".to_string() } else { format!("{}:{}", u.vid, u.pid) },
                        u.driver
                    ),
                    None,
                );
                st.upses.insert(key, u);
                changed = true;
            }
            // Removal: anything not found this time.
            let keys: Vec<String> = st.upses.keys().cloned().collect();
            for key in keys {
                if found_keys.contains(&key) {
                    continue;
                }
                if let Some(u) = st.upses.shift_remove(&key) {
                    if let Some(p) = &u.proc {
                        gone.push(p.clone());
                    }
                    ctx.event(&format!("UPS disconnected: {} {}", u.vendor, u.product), None);
                    changed = true;
                }
            }
        }
        for p in gone {
            p.kill(SIGTERM);
        }
        if changed {
            self.schedule_restart();
        }
        ctx.changed();
    }

    fn schedule_rescan(self: &Arc<Self>) {
        let g = self.rescan_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let me = self.clone();
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(1000)).await;
            if me.rescan_gen.load(Ordering::SeqCst) == g && me.running() {
                me.rescan().await;
            }
        });
    }

    fn schedule_restart(self: &Arc<Self>) {
        let g = self.restart_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let me = self.clone();
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if me.restart_gen.load(Ordering::SeqCst) == g {
                me.restart_nut().await;
            }
        });
    }

    // ---------- NUT children ----------

    fn nut_env(&self) -> Vec<String> {
        let d = self.cfg.dir.display();
        vec!["env".into(), format!("NUT_CONFPATH={d}/conf"), format!("NUT_STATEPATH={d}/state")]
    }

    fn write_config(&self) -> std::io::Result<()> {
        let d = &self.cfg.dir;
        std::fs::create_dir_all(d.join("conf"))?;
        std::fs::create_dir_all(d.join("state"))?;
        let sections: Vec<UpsSection> = lock(&self.st)
            .upses
            .values()
            .map(|u| UpsSection {
                name: u.name.clone(),
                driver: u.driver.clone(),
                port: if u.simulated { self.cfg.sim.clone() } else { "auto".into() },
                vid: (!u.simulated).then(|| u.vid.clone()),
                pid: (!u.simulated).then(|| u.pid.clone()),
                serial: Some(u.serial.clone()),
                bus: if u.serial.is_empty() { Some(u.bus.clone()) } else { None },
                desc: Some(format!("{} {}", u.vendor, u.product).trim().to_string()),
            })
            .collect();
        std::fs::write(d.join("conf/ups.conf"), ups_conf(&sections))?;
        std::fs::write(d.join("conf/upsd.conf"), upsd_conf(&js_string(&self.nut_port()), truthy(&self.shared())))?;
        std::fs::write(d.join("conf/upsd.users"), upsd_users(MON_USER, &self.mon_password()))?;
        std::fs::write(d.join("conf/nut.conf"), "MODE=standalone\n")
    }

    /// Kill NUT processes a previous run left behind (it was killed hard, say). -FF wrote
    /// their pid files.
    async fn kill_stale(&self) {
        let state = self.cfg.dir.join("state");
        let Ok(rd) = std::fs::read_dir(&state) else { return };
        let files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "pid")).collect();
        if files.is_empty() {
            return;
        }
        // Only kill a pid that is still a NUT program; after a reboot the number may be anything.
        let mut pids = Vec::new();
        for f in &files {
            let pid = read_text(f);
            if pid.is_empty() || !pid.bytes().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let comm = read_text(format!("/proc/{pid}/comm"));
            if !comm.is_empty() && self.cfg.nut.join(&comm).exists() {
                pids.push(pid);
            }
        }
        if !pids.is_empty() {
            self.ctx().log(&format!("killing stale NUT processes {}", pids.join(" ")));
            let mut cmd = vec!["sudo", "-n", "kill"];
            cmd.extend(pids.iter().map(String::as_str));
            run(&cmd, Opts::default()).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let names: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
        let mut cmd = vec!["sudo", "-n", "rm", "-f"];
        cmd.extend(names.iter().map(String::as_str));
        run(&cmd, Opts::default()).await;
    }

    async fn stop_nut(&self) {
        let procs: Vec<Arc<Proc>> = {
            let mut st = lock(&self.st);
            let mut v: Vec<Arc<Proc>> = st.upsd.take().into_iter().collect();
            for u in st.upses.values_mut() {
                if let Some(p) = u.proc.take() {
                    v.push(p);
                }
            }
            v
        };
        for p in &procs {
            p.kill(SIGTERM);
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), futures_util::future::join_all(procs.iter().map(|p| p.wait()))).await;
    }

    fn spawn_nut(&self, prog: &str, args: &[&str], log: Arc<Mutex<Vec<String>>>) -> std::io::Result<Arc<Proc>> {
        let mut cmd = Command::new("sudo");
        cmd.arg("-n").args(self.nut_env()).arg(self.cfg.nut.join(prog)).args(args);
        let f: LineFn = Arc::new(move |l: &str| tail(&log, l));
        Proc::spawn(self.ctx(), &mut cmd, Split::Lf, f)
    }

    fn start_driver(self: &Arc<Self>, key: &str) {
        let (driver, name, uid, log) = {
            let mut st = lock(&self.st);
            let Some(u) = st.upses.get_mut(key) else { return };
            u.driver_log = Default::default();
            (u.driver.clone(), u.name.clone(), u.uid, u.driver_log.clone())
        };
        // Drivers need root for the USB device; -u root stops them dropping to the `nut`
        // user, which couldn't write our temp state dir. -FF: foreground, but still write a
        // pid file.
        let p = match self.spawn_nut(&driver, &["-a", &name, "-FF", "-u", "root"], log.clone()) {
            Ok(p) => p,
            Err(e) => {
                if let Some(u) = lock(&self.st).upses.get_mut(key) {
                    u.error = format!("NUT driver {driver}: {e}");
                }
                return;
            }
        };
        if let Some(u) = lock(&self.st).upses.get_mut(key) {
            u.proc = Some(p.clone());
        }
        let me = self.clone();
        let key = key.to_string();
        tokio::spawn(async move {
            let code = p.wait().await;
            {
                let mut st = lock(&me.st);
                let Some(u) = st.upses.get_mut(&key) else { return };
                if !u.proc.as_ref().is_some_and(|x| Arc::ptr_eq(x, &p)) || !me.running() {
                    return;
                }
                u.proc = None;
                let tail2 = last(&log, 2).join(" / ");
                u.error = format!("NUT driver {} exited ({code}): {}", u.driver, if tail2.is_empty() { "no output".into() } else { tail2 });
                me.ctx().log(&u.error);
            }
            me.ctx().changed();
            // Retry: a UPS that was just plugged in sometimes isn't ready for the first claim.
            tokio::time::sleep(Duration::from_secs(30)).await;
            let again = {
                let st = lock(&me.st);
                st.upses.get(&key).is_some_and(|u| u.uid == uid && u.proc.is_none())
            };
            if again && me.running() {
                me.start_driver(&key);
            }
        });
    }

    fn start_upsd(self: &Arc<Self>) {
        let log: Arc<Mutex<Vec<String>>> = Default::default();
        lock(&self.st).upsd_log = log.clone();
        let p = match self.spawn_nut("upsd", &["-FF", "-u", "root"], log.clone()) {
            Ok(p) => p,
            Err(e) => {
                tail(&log, &format!("upsd: {e}"));
                return;
            }
        };
        lock(&self.st).upsd = Some(p.clone());
        let me = self.clone();
        tokio::spawn(async move {
            let code = p.wait().await;
            {
                let mut st = lock(&me.st);
                if !st.upsd.as_ref().is_some_and(|x| Arc::ptr_eq(x, &p)) || !me.running() {
                    return;
                }
                st.upsd = None;
            }
            me.ctx().log(&format!("upsd exited ({code}): {}", last(&log, 3).join(" / ")));
            me.ctx().changed();
        });
    }

    async fn restart_nut(self: &Arc<Self>) {
        let _g = self.restart_lock.lock().await;
        self.stop_nut().await;
        if lock(&self.st).upses.is_empty() || !self.running() {
            self.ctx().changed();
            return;
        }
        if let Err(e) = self.write_config() {
            self.ctx().log(&format!("restart: writing NUT config: {e}"));
        }
        let keys: Vec<String> = {
            let mut st = lock(&self.st);
            st.upses.values_mut().for_each(|u| u.error.clear());
            st.upses.keys().cloned().collect()
        };
        for k in keys {
            self.start_driver(&k);
        }
        // upsd connects to the drivers' sockets at start and retries later, so a short head
        // start is enough.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        if self.running() {
            self.start_upsd();
        }
        self.ctx().changed();
    }

    // ---------- polling ----------

    async fn poll_one(self: &Arc<Self>, key: String, name: String) {
        let target = format!("{name}@127.0.0.1:{}", js_string(&self.nut_port()));
        let r = run(&["upsc", &target], Opts::timeout_ms(4000)).await;
        let mut events = Vec::new();
        {
            let mut st = lock(&self.st);
            let upsd_up = st.upsd.is_some();
            let Some(u) = st.upses.get_mut(&key) else { return };
            u.last_poll = now_ms();
            let was = u.reachable;
            if r.code != 0 {
                u.reachable = false;
                let raw = if r.err.is_empty() { &r.out } else { &r.err };
                let msg = ssl_noise(raw);
                if u.proc.is_some() && upsd_up {
                    let l = msg.to_lowercase();
                    u.error = if l.contains("stale") || l.contains("not connected") {
                        "NUT driver isn't getting data from the UPS (Data stale)".into()
                    } else {
                        msg
                    };
                }
                if was {
                    events.push("Lost contact with the UPS".to_string());
                }
            } else {
                u.vars = parse_upsc(&r.out);
                u.reachable = true;
                u.error.clear();
                if !was && !u.status.is_empty() {
                    events.push("Contact with the UPS restored".into());
                }
                let st_now = u.vars.get("ups.status").cloned().unwrap_or_default();
                events.extend(status_events(&u.status, &st_now).into_iter().map(String::from));
                u.status = st_now;
                let push = |arr: &mut Vec<f64>, v: Option<&String>| {
                    if let Some(n) = nut_num(v) {
                        arr.push(n);
                        if arr.len() > HIST_N {
                            arr.remove(0);
                        }
                    }
                };
                let (c, l) = (u.vars.get("battery.charge").cloned(), u.vars.get("ups.load").cloned());
                push(&mut u.charge, c.as_ref());
                push(&mut u.load, l.as_ref());
            }
        }
        for e in events {
            self.log_event(&key, &e);
        }
    }

    fn log_event(&self, key: &str, text: &str) {
        let (label, name, status) = {
            let mut st = lock(&self.st);
            let Some(u) = st.upses.get_mut(key) else { return };
            u.events.push((now_ms(), text.to_string()));
            if u.events.len() > 50 {
                u.events.remove(0);
            }
            let label = [u.vars.get("device.model"), u.vars.get("ups.model"), Some(&u.product), Some(&u.name)]
                .into_iter()
                .flatten()
                .find(|s| !s.is_empty())
                .cloned()
                .unwrap_or_default();
            (label, u.name.clone(), u.vars.get("ups.status").cloned())
        };
        let mut detail = Map::new();
        detail.insert("ups".into(), Value::String(name));
        if let Some(s) = status {
            detail.insert("status".into(), Value::String(s));
        }
        self.ctx().event(&format!("UPS {label}: {text}"), Some(Value::Object(detail)));
    }

    async fn poll(self: &Arc<Self>) {
        let list: Vec<(String, String)> = {
            let st = lock(&self.st);
            if st.upsd.is_none() {
                return;
            }
            st.upses.values().map(|u| (u.key.clone(), u.name.clone())).collect()
        };
        futures_util::future::join_all(list.into_iter().map(|(k, n)| self.poll_one(k, n))).await;
        self.ctx().changed();
    }

    // ---------- presentation ----------

    fn view(&self, u: &UpsDev) -> Value {
        let v = &u.vars;
        let s = |k: &str| v.get(k).filter(|x| !x.is_empty()).cloned();
        let first = |ks: &[&str], fallback: &str| ks.iter().find_map(|k| s(k)).unwrap_or_else(|| fallback.to_string());
        let n = |k: &str| nut_num(v.get(k));
        let runtime = n("battery.runtime");
        let realpower = n("ups.realpower").or_else(|| match (n("ups.realpower.nominal"), n("ups.load")) {
            (Some(a), Some(b)) => Some(js_round(a * b / 100.0)),
            _ => None,
        });
        let flags: Vec<Value> =
            decode_status(&u.status).into_iter().map(|f| json!({ "flag": f.flag, "word": f.word, "severity": f.severity })).collect();
        let events: Vec<Value> =
            u.events[u.events.len().saturating_sub(20)..].iter().map(|(t, text)| json!({ "t": t, "text": text })).collect();
        json!({
            "name": u.name,
            "key": u.key,
            "simulated": u.simulated,
            "vendor": first(&["device.mfr", "ups.mfr"], &u.vendor),
            "model": first(&["device.model", "ups.model"], &u.product),
            "serial": first(&["device.serial", "ups.serial"], &u.serial),
            "firmware": first(&["ups.firmware", "ups.firmware.aux"], ""),
            "usb": if u.simulated { String::new() } else { format!("{}:{} on port {}", u.vid, u.pid, u.usb_port) },
            "nutDriver": u.driver,
            "driverRunning": u.proc.is_some(),
            "reachable": u.reachable,
            "error": u.error,
            "status": u.status,
            "flags": flags,
            "charge": opt_num(n("battery.charge")),
            "runtime": opt_num(runtime),
            "runtimeText": runtime.map(fmt_runtime).unwrap_or_default(),
            "load": opt_num(n("ups.load")),
            "realpower": opt_num(realpower),
            "inputVoltage": opt_num(n("input.voltage")),
            "outputVoltage": opt_num(n("output.voltage")),
            "inputFrequency": opt_num(n("input.frequency")),
            "batteryVoltage": opt_num(n("battery.voltage")),
            "batteryDate": first(&["battery.mfr.date", "battery.date"], ""),
            "batteryType": first(&["battery.type"], ""),
            "lowRuntime": opt_num(n("battery.runtime.low")),
            "lowCharge": opt_num(n("battery.charge.low")),
            "beeper": first(&["ups.beeper.status"], ""),
            "lastPoll": u.last_poll,
            "history": { "charge": u.charge.iter().map(|x| num(*x)).collect::<Vec<_>>(), "load": u.load.iter().map(|x| num(*x)).collect::<Vec<_>>() },
            "events": events,
        })
    }

    fn views(&self) -> Vec<Value> {
        let st = lock(&self.st);
        st.upses.values().map(|u| self.view(u)).collect()
    }

    fn nut_info(&self) -> Value {
        let (running, log) = {
            let st = lock(&self.st);
            (st.upsd.is_some(), last(&st.upsd_log, 3))
        };
        let shared = self.shared();
        let mut m = Map::new();
        m.insert("running".into(), running.into());
        m.insert("port".into(), self.nut_port());
        m.insert("shared".into(), shared.clone());
        m.insert("user".into(), MON_USER.into());
        // Only worth showing when someone else can connect.
        if truthy(&shared) {
            m.insert("password".into(), self.mon_password().into());
        }
        if !running {
            m.insert("log".into(), json!(log));
        }
        m.insert("autoShutdown".into(), false.into());
        Value::Object(m)
    }

    /// `ups(name)`: by NUT name or key.
    fn find(&self, name: &str) -> Result<String, HttpError> {
        let st = lock(&self.st);
        st.upses
            .values()
            .find(|u| u.name == name || u.key == name)
            .map(|u| u.key.clone())
            .ok_or_else(|| HttpError::new(404, format!("No UPS \"{name}\"{}", if st.upses.is_empty() { " (none connected)" } else { "" })))
    }

    fn view_of(&self, key: &str, with_vars: bool, history: bool) -> Value {
        let st = lock(&self.st);
        let Some(u) = st.upses.get(key) else { return Value::Null };
        let mut v = self.view(u);
        if let Value::Object(m) = &mut v {
            if !history {
                m.shift_remove("history");
            }
            if with_vars {
                m.insert("vars".into(), json!(u.vars));
            }
        }
        v
    }

    fn status(&self) -> Value {
        let list = self.views();
        let mut summary = "No UPS".to_string();
        if let Some(u) = list.first() {
            let words: Vec<String> = u["flags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|f| matches!(f["flag"].as_str(), Some("OL" | "OB" | "LB" | "RB" | "BYPASS" | "OVER")))
                .map(|f| js_string(&f["word"]))
                .collect();
            let words = words.join(", ");
            let reach =
                if u["reachable"] == true { if words.is_empty() { js_string(&u["status"]) } else { words } } else { "no data".into() };
            let parts = [
                js_string(&u["model"]),
                reach,
                if u["charge"].is_null() { String::new() } else { format!("{}%", js_string(&u["charge"])) },
                js_string(&u["runtimeText"]),
                if u["load"].is_null() { String::new() } else { format!("{}% load", js_string(&u["load"])) },
            ];
            summary = parts.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
            if list.len() > 1 {
                summary += &format!(" (+{} more)", list.len() - 1);
            }
        }
        json!({ "active": !list.is_empty(), "summary": summary, "upses": list, "nut": self.nut_info() })
    }
}

/// `(r.err || r.out).replace(/Init SSL without certificate database\s*/g, "").trim()`
fn ssl_noise(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    const NOISE: &str = "Init SSL without certificate database";
    while let Some(i) = rest.find(NOISE) {
        out.push_str(&rest[..i]);
        rest = rest[i + NOISE.len()..].trim_start();
    }
    out.push_str(rest);
    out.trim().to_string()
}

// ---------- driver ----------

fn route<F, Fut>(i: &Arc<Inner>, method: Method, path: &str, doc: Json, f: F) -> Route
where
    F: Fn(Arc<Inner>, benchbob_core::Request) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = benchbob_core::RouteResult> + Send + 'static,
{
    let i = i.clone();
    Route::new(method, path, Some(doc), move |r| f(i.clone(), r))
}

#[async_trait]
impl Driver for Ups {
    fn id(&self) -> &'static str {
        "ups"
    }
    fn title(&self) -> &'static str {
        "UPS"
    }
    fn order(&self) -> i32 {
        70
    }
    fn description(&self) -> &'static str {
        "USB UPS via NUT (run privately by BenchBob): battery, runtime, load, power events, optional network sharing for NUT clients"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let i = &self.inner;
        let _ = i.ctx.set(ctx);
        i.stopping.store(false, Ordering::SeqCst);
        let rules = read_text(RULES);
        if !rules.is_empty() {
            lock(&i.st).table = parse_nut_udev_rules(&rules);
        }
        i.kill_stale().await;
        i.rescan().await;
        let epoch = i.epoch.load(Ordering::SeqCst);
        let me = i.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(POLL).await;
                if me.epoch.load(Ordering::SeqCst) != epoch {
                    break;
                }
                me.poll().await;
            }
        });
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) {
        let i = &self.inner;
        i.stopping.store(true, Ordering::SeqCst);
        i.epoch.fetch_add(1, Ordering::SeqCst);
        i.restart_gen.fetch_add(1, Ordering::SeqCst);
        i.rescan_gen.fetch_add(1, Ordering::SeqCst);
        i.stop_nut().await;
        lock(&i.st).upses.clear();
    }

    fn udev(&self, ev: &UdevEvent) {
        let s = prop(&ev.props, "SUBSYSTEM");
        if (s == "usb" && prop(&ev.props, "DEVTYPE") == "usb_device") || s == "hid" {
            if ev.action == "remove" {
                lock(&self.inner.st).power_hid.remove(prop(&ev.props, "DEVPATH"));
            }
            if matches!(ev.action.as_str(), "add" | "remove" | "bind") {
                self.inner.schedule_rescan();
            }
        }
    }

    fn status(&self) -> Option<Json> {
        self.inner.ctx.get()?;
        Some(self.inner.status())
    }

    fn routes(&self) -> Vec<Route> {
        let i = &self.inner;
        vec![
            route(
                i,
                Method::GET,
                "/api/ups",
                json!(
                    "UPSes attached to BenchBob with decoded status (On mains / On battery / Low battery…), charge %, runtime, load, voltages, recent power events; plus the private NUT server's port and sharing state"
                ),
                |i, _r| async move { reply(json!({ "upses": i.views(), "nut": i.nut_info() })) },
            ),
            route(
                i,
                Method::GET,
                "/api/ups/:name",
                json!("Every variable NUT reports for one UPS (raw `upsc` output as JSON); :name is the NUT name, e.g. benchbob"),
                |i, r| async move {
                    let key = i.find(r.param("name"))?;
                    reply(i.view_of(&key, true, true))
                },
            ),
            route(
                i,
                Method::POST,
                "/api/ups/share",
                json!({ "body": { "share": true, "port": 3493 }, "description": "Share on network: upsd listens on 0.0.0.0:port so NUT clients elsewhere can monitor (read-only user monuser, password shown in GET /api/ups while shared). Off = 127.0.0.1 only" }),
                |i, r| async move {
                    let b = r.json()?;
                    if let Some(p) = b.get("port").filter(|v| !v.is_null()) {
                        let port = js_number(p);
                        if port.fract() != 0.0 || !(1.0..=65535.0).contains(&port) {
                            return Err(HttpError::bad_request("port must be 1–65535"));
                        }
                        i.ctx().set_setting("port", json!(port as u32));
                    }
                    if let Some(s) = b.get("share").filter(|v| !v.is_null()) {
                        i.ctx().set_setting("share", Value::Bool(truthy(s)));
                    }
                    let msg = if truthy(&i.shared()) { format!("on (0.0.0.0:{})", js_string(&i.nut_port())) } else { "off".into() };
                    i.ctx().event(&format!("UPS network sharing {msg}"), None);
                    i.restart_nut().await;
                    reply(json!({ "ok": true, "nut": i.nut_info() }))
                },
            ),
            route(
                i,
                Method::POST,
                "/api/ups/restart",
                json!("Restart the private NUT driver(s) and upsd (e.g. after the UPS stopped answering)"),
                |i, _r| async move {
                    i.rescan().await;
                    i.restart_nut().await;
                    reply(json!({ "ok": true, "nut": i.nut_info() }))
                },
            ),
        ]
    }

    fn tools(&self) -> Vec<Tool> {
        let i = self.inner.clone();
        vec![Tool::new(
            "ups_status",
            "Status of the UPS(es) attached to BenchBob via NUT: whether it's on mains or battery (decoded flags), battery charge %, estimated runtime, load %, input/output voltage and frequency, model/serial/firmware, battery date, and recent power events. Automatic shutdown is not configured.",
            json!({ "type": "object", "properties": { "name": { "type": "string", "description": "NUT UPS name (default: all). The first UPS is 'benchbob'." }, "raw": { "type": "boolean", "description": "Also include every raw NUT variable" } } }),
            move |a| {
                let i = i.clone();
                async move {
                    let keys: Vec<String> = match a.get("name").filter(|v| truthy(v)) {
                        Some(n) => vec![i.find(&js_string(n))?],
                        None => lock(&i.st).upses.keys().cloned().collect(),
                    };
                    if keys.is_empty() {
                        return Ok(json!({ "upses": [], "note": "No UPS connected" }));
                    }
                    let raw = a.get("raw").is_some_and(truthy);
                    let list: Vec<Value> = keys.iter().map(|k| i.view_of(k, raw, false)).collect();
                    Ok(json!({ "upses": list, "nut": i.nut_info() }))
                }
            },
        )]
    }
}

#[cfg(test)]
mod tests;
