//! The RTL-SDR panel, ported from `peripherals/drivers/sdr.ts`.
//!
//! RTL2832U DVB-T sticks used as software-defined radios. A stick can only be opened by one
//! program at a time, so the driver is a small state machine over one child process:
//!
//! ```text
//!   off ──▶ rtl_tcp  (raw I/Q over TCP for SDR++ / GQRX / SDR# on another machine)
//!    ▲  ──▶ rtl_433  (decodes 433/868/915/345 MHz sensors: weather stations, TPMS, …)
//!    └──────────┘
//! ```
//!
//! Switching modes stops the running child and waits for it to exit before the next opens
//! the stick. A probe (`rtl_test -t`) also needs the stick, so it only runs while off.
//!
//! The kernel's DVB driver (dvb_usb_rtl28xxu) grabs these sticks on plug-in unless it is
//! blacklisted. Debian's rtl-sdr package doesn't ship that blacklist, so the panel checks
//! modprobe.d and shows the fix rather than silently failing to open the device.

pub mod parse;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::Method;
use benchbob_core::http::{HttpError, js_number, js_string, json as reply, num, truthy};
use benchbob_core::paths::port_offset;
use benchbob_core::proc::{Proc, SIGINT, SIGKILL, Split};
use benchbob_core::run::{Opts, read_text, run};
use benchbob_core::timeline::now_ms;
use benchbob_core::udev::{Props, UdevEvent};
use benchbob_core::util::{hostname, js_round, prop, udev_decode};
use benchbob_core::{Ctx, Driver, Json, Route, StopReason, Tool};
use regex::Regex;
use serde_json::{Map, Value, json};
use tokio::process::Command;

use crate::parse::{DVB_MODULE, Rtl433Aggregator, find_blacklist, js_slice_last, parse_rtl_eeprom, parse_rtl_test, rtl_stick, tcp_peers};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// rtl_433 frequency presets, in `Object.keys` order (integer-like keys first).
pub const PRESETS: [(&str, &str, &[&str]); 6] = [
    ("345", "345 MHz (US security sensors)", &["345M"]),
    ("433", "433.92 MHz (EU/US ISM)", &["433.92M"]),
    ("868", "868.3 MHz (EU SRD)", &["868.3M"]),
    ("915", "915 MHz (US ISM)", &["915M"]),
    ("433+868", "Hop 433.92 / 868.3 MHz", &["433.92M", "868.3M"]),
    ("433+915", "Hop 433.92 / 915 MHz", &["433.92M", "915M"]),
];

fn presets_json() -> Value {
    Value::Object(PRESETS.iter().map(|(k, label, f)| (k.to_string(), json!({ "label": label, "freqs": f }))).collect())
}

fn preset_keys() -> Vec<&'static str> {
    PRESETS.iter().map(|p| p.0).collect()
}

fn fix_hint() -> String {
    format!("echo 'blacklist {DVB_MODULE}' | sudo tee /etc/modprobe.d/blacklist-rtl-sdr.conf && sudo modprobe -r {DVB_MODULE}")
}

#[derive(Clone)]
pub struct Config {
    pub rtl_tcp: PathBuf,
    pub rtl_433: PathBuf,
    pub rtl_test: PathBuf,
    pub rtl_eeprom: PathBuf,
    pub port_offset: u16,
    pub modprobe_dirs: Vec<PathBuf>,
    pub sys_module: PathBuf,
    pub proc_net: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            rtl_tcp: "/usr/bin/rtl_tcp".into(),
            rtl_433: "/usr/bin/rtl_433".into(),
            rtl_test: "/usr/bin/rtl_test".into(),
            rtl_eeprom: "/usr/bin/rtl_eeprom".into(),
            port_offset: port_offset(),
            modprobe_dirs: ["/etc/modprobe.d", "/run/modprobe.d", "/usr/lib/modprobe.d", "/lib/modprobe.d", "/usr/local/lib/modprobe.d"]
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            sys_module: "/sys/module".into(),
            proc_net: "/proc/net".into(),
        }
    }
    fn default_tcp_port(&self) -> u32 {
        1234 + self.port_offset as u32
    }
}

#[derive(Clone)]
struct Stick {
    /// Stable: serial if set, else the USB port path.
    key: String,
    devpath: String,
    usb_id: String,
    known_as: String,
    manufacturer: String,
    product: String,
    serial: String,
    port: String,
    bus: String,
    drivers: Vec<String>,
    dvb_claimed: bool,
    probe: Option<Value>,
}

impl Stick {
    fn to_json(&self) -> Value {
        json!({
            "key": self.key, "devpath": self.devpath, "usbId": self.usb_id, "knownAs": self.known_as,
            "manufacturer": self.manufacturer, "product": self.product, "serial": self.serial, "port": self.port,
            "bus": self.bus, "drivers": self.drivers, "dvbClaimed": self.dvb_claimed, "probe": self.probe,
        })
    }
}

struct Running {
    mode: &'static str,
    stick: Option<Stick>,
    proc: Arc<Proc>,
    since: u64,
    opts: Value,
    /// rtl_tcp's port.
    port: u32,
    cmd: Vec<String>,
    done: tokio::sync::watch::Sender<bool>,
}

#[derive(Default)]
struct St {
    sticks: Vec<Stick>,
    probes: Vec<(String, Value)>,
    dvb: (bool, bool, Vec<String>),
    cur: Option<Arc<Running>>,
    last_exit: Option<Value>,
    tcp_clients: Vec<String>,
    rtl433: Rtl433Aggregator,
    tcp_log: Vec<(u64, String)>,
}

struct Inner {
    cfg: Config,
    ctx: OnceLock<Ctx>,
    st: Mutex<St>,
    busy: tokio::sync::Mutex<()>,
    probing: AtomicBool,
    epoch: AtomicU64,
}

pub struct Sdr {
    inner: Arc<Inner>,
}

impl Default for Sdr {
    fn default() -> Self {
        Self::new()
    }
}

impl Sdr {
    pub fn new() -> Self {
        Self::with_config(Config::from_env())
    }
    pub fn with_config(cfg: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                cfg,
                ctx: OnceLock::new(),
                st: Mutex::new(St::default()),
                busy: tokio::sync::Mutex::new(()),
                probing: AtomicBool::new(false),
                epoch: AtomicU64::new(0),
            }),
        }
    }
}

/// `{ ...a, ...b }` for JSON objects.
fn spread(base: &mut Map<String, Value>, over: Option<&Value>) {
    if let Some(Value::Object(o)) = over {
        for (k, v) in o {
            base.insert(k.clone(), v.clone());
        }
    }
}

fn trunc_or(v: Option<&Value>, d: f64) -> f64 {
    let n = v.map(js_number).unwrap_or(f64::NAN).trunc();
    if n.is_nan() || n == 0.0 { d } else { n }
}

impl Inner {
    fn ctx(&self) -> &Ctx {
        self.ctx.get().expect("sdr driver not initialised")
    }

    // ---------- sticks ----------

    fn scan_sticks(&self) {
        let ctx = self.ctx();
        let usbs = ctx.devices(Some(&|p: &Props| {
            prop(p, "SUBSYSTEM") == "usb"
                && prop(p, "DEVTYPE") == "usb_device"
                && rtl_stick(&format!("{}:{}", prop(p, "ID_VENDOR_ID"), prop(p, "ID_MODEL_ID"))).is_some()
        }));
        let ifaces = ctx.devices(Some(&|i: &Props| prop(i, "SUBSYSTEM") == "usb" && prop(i, "DEVTYPE") == "usb_interface"));
        let mut st = lock(&self.st);
        let sticks = usbs
            .iter()
            .map(|p| {
                let dp = prop(p, "DEVPATH");
                let mut drivers: Vec<String> = Vec::new();
                for i in ifaces.iter().filter(|i| prop(i, "DEVPATH").starts_with(&format!("{dp}/"))) {
                    let d = prop(i, "DRIVER");
                    if !d.is_empty() && !drivers.iter().any(|x| x == d) {
                        drivers.push(d.into());
                    }
                }
                let serial = prop(p, "ID_SERIAL_SHORT").to_string();
                let key = if !serial.is_empty() && serial != "00000001" {
                    format!("sn:{serial}")
                } else {
                    format!("path:{}", if prop(p, "ID_PATH").is_empty() { dp } else { prop(p, "ID_PATH") })
                };
                let usb_id = format!("{}:{}", prop(p, "ID_VENDOR_ID"), prop(p, "ID_MODEL_ID"));
                let pick = |enc: &str, raw: &str| {
                    let d = udev_decode(prop(p, enc));
                    if d.is_empty() { prop(p, raw).to_string() } else { d }
                };
                let (bn, dn) = (prop(p, "BUSNUM"), prop(p, "DEVNUM"));
                Stick {
                    probe: st.probes.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone()),
                    key,
                    devpath: dp.into(),
                    known_as: rtl_stick(&usb_id).unwrap_or("").into(),
                    usb_id,
                    manufacturer: pick("ID_VENDOR_ENC", "ID_VENDOR"),
                    product: pick("ID_MODEL_ENC", "ID_MODEL"),
                    serial,
                    port: prop(p, "ID_PATH").into(),
                    bus: if !bn.is_empty() && !dn.is_empty() { format!("{bn}/{dn}") } else { String::new() },
                    dvb_claimed: drivers.iter().any(|d| d.starts_with("dvb_usb") || d == DVB_MODULE),
                    drivers,
                }
            })
            .collect();
        st.sticks = sticks;
    }

    fn check_dvb(&self) {
        let mut files = Vec::new();
        for dir in &self.cfg.modprobe_dirs {
            let Ok(rd) = std::fs::read_dir(dir) else { continue };
            let mut names: Vec<String> = rd.flatten().filter_map(|e| e.file_name().to_str().map(String::from)).collect();
            names.sort();
            for f in names.into_iter().filter(|f| f.ends_with(".conf")) {
                let p = dir.join(&f);
                files.push((p.display().to_string(), read_text(&p)));
            }
        }
        let (blacklisted, where_) = find_blacklist(&files, DVB_MODULE);
        let mut uniq: Vec<String> = Vec::new();
        for w in where_ {
            if !uniq.contains(&w) {
                uniq.push(w);
            }
        }
        lock(&self.st).dvb = (self.cfg.sys_module.join(DVB_MODULE).exists(), blacklisted, uniq);
    }

    /// The -d argument: a unique serial when there is one (stable across replugs), else the index.
    fn device_arg(st: &St, s: Option<&Stick>, for_rtl433: bool) -> Vec<String> {
        let Some(s) = s else { return Vec::new() };
        let dup = st.sticks.iter().filter(|x| x.serial == s.serial).count() > 1;
        if !s.serial.is_empty() && !dup {
            return vec!["-d".into(), if for_rtl433 { format!(":{}", s.serial) } else { s.serial.clone() }];
        }
        let idx = st.sticks.iter().position(|x| x.devpath == s.devpath).unwrap_or(0);
        vec!["-d".into(), idx.to_string()]
    }

    fn pick_stick(&self, key: Option<&Value>, force: bool) -> Result<Option<Stick>, HttpError> {
        let st = lock(&self.st);
        if let Some(k) = key.filter(|v| truthy(v)) {
            let k = js_string(k);
            return st
                .sticks
                .iter()
                .find(|s| s.key == k || s.serial == k)
                .cloned()
                .map(Some)
                .ok_or_else(|| HttpError::new(404, format!("No RTL-SDR stick \"{k}\"")));
        }
        if let Some(s) = st.sticks.first() {
            return Ok(Some(s.clone()));
        }
        if force {
            // Let the tool search on its own (e.g. a clone with an unlisted USB id).
            return Ok(None);
        }
        Err(HttpError::new(409, "No RTL-SDR stick detected. Plug one in, or pass force:true to try anyway."))
    }

    fn stick_settings(&self, s: Option<&Stick>) -> Value {
        self.ctx().setting(&format!("stick:{}", s.map(|s| s.key.as_str()).unwrap_or("default"))).unwrap_or_else(|| json!({}))
    }

    fn save_stick_settings(&self, s: Option<&Stick>, k: &str, v: Value) {
        let mut cur = self.stick_settings(s).as_object().cloned().unwrap_or_default();
        cur.insert(k.into(), v);
        self.ctx().set_setting(&format!("stick:{}", s.map(|s| s.key.as_str()).unwrap_or("default")), Value::Object(cur));
    }

    fn tcp_opts(&self, s: Option<&Stick>, o: &Value) -> Result<Value, HttpError> {
        let mut d = json!({ "port": self.cfg.default_tcp_port(), "bind": "0.0.0.0", "gain": 0, "ppm": 0, "sampleRate": 2048000, "freq": 100_000_000, "biasT": false })
            .as_object()
            .cloned()
            .unwrap();
        spread(&mut d, self.stick_settings(s).get("tcp"));
        spread(&mut d, Some(o));
        let port = d.get("port").map(js_number).unwrap_or(f64::NAN).trunc();
        if !(port > 0.0 && port < 65536.0) {
            return Err(HttpError::bad_request("port must be 1–65535"));
        }
        let sr = d.get("sampleRate").map(js_number).unwrap_or(f64::NAN).trunc();
        // librtlsdr accepts 225001–300000 and 900001–3200000 S/s.
        if !((sr > 225000.0 && sr <= 300000.0) || (sr > 900000.0 && sr <= 3200000.0)) {
            return Err(HttpError::bad_request("sampleRate must be 225001–300000 or 900001–3200000"));
        }
        let bind = d.get("bind").map(js_string).unwrap_or_else(|| "undefined".into());
        if bind.is_empty() || !bind.bytes().all(|c| c.is_ascii_hexdigit() || c == b':' || c == b'.') {
            return Err(HttpError::bad_request("bind must be an IP address"));
        }
        let gain = d.get("gain").map(js_number).unwrap_or(f64::NAN);
        Ok(json!({
            "port": num(port),
            "bind": bind,
            "gain": num(if gain.is_nan() { 0.0 } else { gain }),
            "ppm": num(trunc_or(d.get("ppm"), 0.0)),
            "sampleRate": num(sr),
            "freq": num(trunc_or(d.get("freq"), 100_000_000.0)),
            "biasT": d.get("biasT").is_some_and(truthy),
        }))
    }

    fn r433_opts(&self, s: Option<&Stick>, o: &Value) -> Result<Value, HttpError> {
        static FREQ: OnceLock<Regex> = OnceLock::new();
        static GAIN: OnceLock<Regex> = OnceLock::new();
        let mut d = json!({ "preset": "433", "hop": 0, "gain": "", "ppm": 0 }).as_object().cloned().unwrap();
        spread(&mut d, self.stick_settings(s).get("rtl433"));
        spread(&mut d, Some(o));
        let custom: Option<Vec<String>> =
            d.get("freqs").and_then(Value::as_array).filter(|a| !a.is_empty()).map(|a| a.iter().map(js_string).collect());
        let preset = d.get("preset").cloned().unwrap_or(Value::Null);
        let freqs: Vec<String> = match &custom {
            Some(f) => f.clone(),
            None => {
                let p = if preset.is_null() { "undefined".to_string() } else { js_string(&preset) };
                match PRESETS.iter().find(|x| x.0 == p) {
                    Some(x) => x.2.iter().map(|s| s.to_string()).collect(),
                    None => {
                        return Err(HttpError::bad_request(format!(
                            "Unknown preset \"{p}\"; use one of {} or pass freqs",
                            preset_keys().join(", ")
                        )));
                    }
                }
            }
        };
        let fre = FREQ.get_or_init(|| Regex::new(r"^\d+(\.\d+)?[kMG]?$").unwrap());
        if let Some(f) = freqs.iter().find(|f| !fre.is_match(f)) {
            return Err(HttpError::bad_request(format!("Bad frequency \"{f}\" (e.g. 433.92M)")));
        }
        let hop = trunc_or(d.get("hop"), if freqs.len() > 1 { 60.0 } else { 0.0 }).max(0.0);
        let gain = match d.get("gain") {
            None | Some(Value::Null) => String::new(),
            Some(v) => js_string(v),
        };
        if !gain.is_empty() && !GAIN.get_or_init(|| Regex::new(r"^\d+(\.\d+)?$").unwrap()).is_match(&gain) {
            return Err(HttpError::bad_request("gain is dB (e.g. 40) or empty for auto"));
        }
        Ok(json!({
            "preset": if custom.is_some() { json!("custom") } else { preset },
            "freqs": freqs,
            "hop": num(hop),
            "gain": gain,
            "ppm": num(trunc_or(d.get("ppm"), 0.0)),
        }))
    }

    async fn stop_mode(&self) {
        let Some(r) = lock(&self.st).cur.clone() else { return };
        // Both tools close the device cleanly on SIGINT.
        r.proc.kill(SIGINT);
        if r.proc.wait_for(Duration::from_secs(3)).await.is_none() {
            r.proc.kill(SIGKILL);
            r.proc.wait().await;
        }
        let mut d = r.done.subscribe();
        let _ = tokio::time::timeout(Duration::from_secs(2), d.wait_for(|v| *v)).await;
        {
            let mut st = lock(&self.st);
            if st.cur.as_ref().is_some_and(|c| Arc::ptr_eq(c, &r)) {
                st.cur = None;
            }
            st.tcp_clients.clear();
        }
        self.ctx().event(&format!("SDR {} stopped", r.mode), None);
        self.ctx().changed();
    }

    async fn set_mode(self: &Arc<Self>, mode: &Value, o: &Value) -> Result<Value, HttpError> {
        let _g = self.busy.lock().await;
        let mode: &'static str = match mode.as_str() {
            Some("off") => "off",
            Some("rtl_tcp") => "rtl_tcp",
            Some("rtl_433") => "rtl_433",
            _ => return Err(HttpError::bad_request("mode must be off, rtl_tcp or rtl_433")),
        };
        if self.probing.load(Ordering::SeqCst) {
            return Err(HttpError::new(409, "A probe is using the stick; try again in a few seconds"));
        }
        if mode == "off" {
            self.stop_mode().await;
            return Ok(self.status_mode());
        }
        let s = self.pick_stick(o.get("stick"), o.get("force").is_some_and(truthy))?;
        if s.as_ref().is_some_and(|s| s.dvb_claimed) {
            return Err(HttpError::new(409, format!("The kernel DVB driver has this stick. Fix: {}", fix_hint())));
        }
        let opts = if mode == "rtl_tcp" { self.tcp_opts(s.as_ref(), o)? } else { self.r433_opts(s.as_ref(), o)? };
        self.stop_mode().await;

        let st_args = |rtl433: bool| Self::device_arg(&lock(&self.st), s.as_ref(), rtl433);
        let (prog, args, port) = if mode == "rtl_tcp" {
            let t = &opts;
            let mut a: Vec<String> = vec![
                "-a".into(),
                js_string(&t["bind"]),
                "-p".into(),
                js_string(&t["port"]),
                "-s".into(),
                js_string(&t["sampleRate"]),
                "-f".into(),
                js_string(&t["freq"]),
                "-g".into(),
                js_string(&t["gain"]),
                "-P".into(),
                js_string(&t["ppm"]),
            ];
            if t["biasT"] == true {
                a.push("-T".into());
            }
            a.extend(st_args(false));
            self.save_stick_settings(s.as_ref(), "tcp", opts.clone());
            (self.cfg.rtl_tcp.clone(), a, t["port"].as_u64().unwrap_or(0) as u32)
        } else {
            let r = &opts;
            // -F log puts rtl_433's own messages on stdout as plain text; with only -F json
            // they vanish.
            let mut a = st_args(true);
            a.extend(["-F", "json", "-F", "log", "-M", "time:unix:usec", "-M", "level", "-M", "protocol"].map(String::from));
            let freqs: Vec<String> = r["freqs"].as_array().into_iter().flatten().map(js_string).collect();
            for f in &freqs {
                a.push("-f".into());
                a.push(f.clone());
            }
            if freqs.len() > 1 {
                a.push("-H".into());
                a.push(js_string(&r["hop"]));
            }
            if r["gain"] != "" {
                a.push("-g".into());
                a.push(js_string(&r["gain"]));
            }
            if r["ppm"] != 0 {
                a.push("-p".into());
                a.push(js_string(&r["ppm"]));
            }
            let mut saved = Map::new();
            saved.insert("preset".into(), r["preset"].clone());
            if r["preset"] == "custom" {
                saved.insert("freqs".into(), r["freqs"].clone());
            }
            for k in ["hop", "gain", "ppm"] {
                saved.insert(k.into(), r[k].clone());
            }
            self.save_stick_settings(s.as_ref(), "rtl433", Value::Object(saved));
            (self.cfg.rtl_433.clone(), a, 0)
        };
        let mut cmd_line = vec![prog.display().to_string()];
        cmd_line.extend(args.iter().cloned());
        let me = self.clone();
        let on_line = Arc::new(move |l: &str| {
            if mode == "rtl_433" {
                lock(&me.st).rtl433.line(l, now_ms() as f64);
            } else {
                me.tcp_line(l);
            }
        });
        let proc = Proc::spawn(self.ctx(), Command::new(&prog).args(&args), Split::Lf, on_line)
            .map_err(|e| HttpError::new(500, spawn_error(&e, &prog)))?;
        let (done, _) = tokio::sync::watch::channel(false);
        let r = Arc::new(Running { mode, stick: s.clone(), proc, since: now_ms(), opts, port, cmd: cmd_line, done });
        {
            let mut st = lock(&self.st);
            st.cur = Some(r.clone());
            st.last_exit = None;
        }
        let me = self.clone();
        let r2 = r.clone();
        tokio::spawn(async move {
            let code = r2.proc.wait().await;
            let msg = {
                let mut st = lock(&me.st);
                if !st.cur.as_ref().is_some_and(|c| Arc::ptr_eq(c, &r2)) {
                    None
                } else {
                    let log: Vec<String> = if mode == "rtl_433" {
                        let l = &st.rtl433.log;
                        l[l.len().saturating_sub(3)..].iter().map(|(_, x)| x.clone()).collect()
                    } else {
                        let l = &st.tcp_log;
                        l[l.len().saturating_sub(3)..].iter().map(|(_, x)| x.clone()).collect()
                    };
                    let message = log.join(" · ");
                    st.last_exit = Some(json!({ "mode": mode, "code": code, "at": now_ms(), "message": message }));
                    st.cur = None;
                    st.tcp_clients.clear();
                    Some(message)
                }
            };
            if let Some(m) = msg {
                me.ctx().event(&format!("SDR {mode} exited ({code}){}", if m.is_empty() { String::new() } else { format!(": {m}") }), None);
                me.ctx().changed();
            }
            let _ = r2.done.send(true);
        });
        let on = s.as_ref().map(|s| format!(" on {}", if s.product.is_empty() { &s.known_as } else { &s.product })).unwrap_or_default();
        self.ctx().event(&format!("SDR {mode} started{on}"), None);
        self.ctx().changed();
        // Most failures (no device, busy) show up within a second; report them to the caller.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        {
            let st = lock(&self.st);
            if !st.cur.as_ref().is_some_and(|c| Arc::ptr_eq(c, &r))
                && let Some(e) = &st.last_exit
            {
                let m = e["message"].as_str().unwrap_or("");
                return Err(HttpError::new(
                    503,
                    format!("{mode} exited: {}", if m.is_empty() { format!("code {}", js_string(&e["code"])) } else { m.into() }),
                ));
            }
        }
        Ok(self.status_mode())
    }

    fn tcp_line(&self, l: &str) {
        if l.trim().is_empty() {
            return;
        }
        {
            let mut st = lock(&self.st);
            st.tcp_log.push((now_ms(), l.trim().to_string()));
            if st.tcp_log.len() > 100 {
                st.tcp_log.remove(0);
            }
        }
        let low = l.to_lowercase();
        if low.contains("client accepted") || low.contains("all threads dead") {
            self.ctx().changed();
        }
    }

    fn poll_tcp_clients(&self) {
        let port = {
            let st = lock(&self.st);
            match &st.cur {
                Some(c) if c.mode == "rtl_tcp" => c.port,
                _ => return,
            }
        };
        let mut next = tcp_peers(&read_text(self.cfg.proc_net.join("tcp")), port as u16);
        next.extend(tcp_peers(&read_text(self.cfg.proc_net.join("tcp6")), port as u16));
        let event = {
            let mut st = lock(&self.st);
            if next.join(",") == st.tcp_clients.join(",") {
                return;
            }
            let ev = !next.is_empty() && st.tcp_clients.is_empty();
            st.tcp_clients = next.clone();
            ev
        };
        if event {
            self.ctx().event(&format!("rtl_tcp client connected: {}", next.join(", ")), None);
        }
        self.ctx().changed();
    }

    // ---------- probe ----------

    async fn probe(self: &Arc<Self>, key: Option<&Value>) -> Result<Value, HttpError> {
        let _g = self.busy.lock().await;
        if let Some(c) = &lock(&self.st).cur {
            return Err(HttpError::new(409, format!("The stick is in use by {}; turn it off to probe", c.mode)));
        }
        let Some(s) = self.pick_stick(key, false)? else { return Ok(Value::Null) };
        self.probing.store(true, Ordering::SeqCst);
        let darg = Self::device_arg(&lock(&self.st), Some(&s), false);
        let run_tool = |prog: &PathBuf| {
            let mut c = vec![prog.display().to_string()];
            if prog == &self.cfg.rtl_test {
                c.push("-t".into());
            }
            c.extend(darg.iter().cloned());
            c
        };
        // rtl_test -t only tunes and lists gains; it exits by itself.
        let c = run_tool(&self.cfg.rtl_test);
        let t = run(&c.iter().map(String::as_str).collect::<Vec<_>>(), Opts::timeout_ms(10000)).await;
        let info = parse_rtl_test(&format!("{}\n{}", t.out, t.err));
        let c = run_tool(&self.cfg.rtl_eeprom);
        let e = run(&c.iter().map(String::as_str).collect::<Vec<_>>(), Opts::timeout_ms(10000)).await;
        let eeprom = parse_rtl_eeprom(&format!("{}\n{}", e.out, e.err));
        let mut p = info.to_map();
        p.insert("t".into(), now_ms().into());
        if !eeprom.is_empty() {
            p.insert("eeprom".into(), json!(eeprom));
        }
        let p = Value::Object(p);
        {
            let mut st = lock(&self.st);
            st.probes.retain(|(k, _)| *k != s.key);
            st.probes.push((s.key.clone(), p.clone()));
            if let Some(x) = st.sticks.iter_mut().find(|x| x.key == s.key) {
                x.probe = Some(p.clone());
            }
        }
        self.probing.store(false, Ordering::SeqCst);
        self.ctx().changed();
        Ok(p)
    }

    // ---------- views ----------

    fn status_mode(&self) -> Value {
        let st = lock(&self.st);
        let Some(c) = &st.cur else { return json!({ "mode": "off", "lastExit": st.last_exit }) };
        let mut m = Map::new();
        m.insert("mode".into(), c.mode.into());
        m.insert("since".into(), c.since.into());
        m.insert("stick".into(), c.stick.as_ref().map(|s| Value::from(s.key.clone())).unwrap_or(Value::Null));
        m.insert("opts".into(), c.opts.clone());
        m.insert("cmd".into(), c.cmd.join(" ").into());
        if c.mode == "rtl_tcp" {
            let host = hostname();
            let p = c.port;
            m.insert("clients".into(), json!(st.tcp_clients));
            m.insert(
                "connect".into(),
                json!({ "sdrpp": format!("Source \"RTL-TCP\", host {host}, port {p}"), "gqrx": format!("rtl_tcp={host}:{p}"), "address": format!("{host}:{p}") }),
            );
        } else {
            m.insert("decoded".into(), st.rtl433.devices.len().into());
            m.insert("messages".into(), st.rtl433.seq.into());
        }
        Value::Object(m)
    }

    fn devices_view(&self, limit: usize, hist: bool) -> Vec<Value> {
        let st = lock(&self.st);
        let now = now_ms() as f64;
        st.rtl433
            .list()
            .into_iter()
            .take(limit)
            .map(|d| {
                let mut m = d.to_json(hist);
                m.insert("lastSeenAgo_s".into(), num(js_round((now - d.last_seen) / 1000.0)));
                Value::Object(m)
            })
            .collect()
    }

    fn status(&self) -> Value {
        let m = self.status_mode();
        let st = lock(&self.st);
        let now = now_ms() as f64;
        let recent = st.rtl433.devices.values().filter(|d| now - d.last_seen < 300_000.0).count();
        let n = st.sticks.len();
        let mut summary = if n == 0 { "No RTL-SDR stick".to_string() } else { format!("{n} stick{}", if n > 1 { "s" } else { "" }) };
        match m["mode"].as_str() {
            Some("rtl_tcp") => {
                let c = st.tcp_clients.len();
                summary = format!(
                    "rtl_tcp on :{} · {}",
                    st.cur.as_ref().map(|c| c.port).unwrap_or(0),
                    if c > 0 { format!("{c} client") } else { "no client".into() }
                );
            }
            Some("rtl_433") => {
                let k = st.rtl433.devices.len();
                summary = format!("rtl_433 · {k} sensor{}", if k == 1 { "" } else { "s" });
            }
            _ => {
                if st.sticks.iter().any(|s| s.dvb_claimed) {
                    summary += " · held by DVB driver";
                }
            }
        }
        let (loaded, blacklisted, where_) = &st.dvb;
        let mut o = Map::new();
        o.insert("active".into(), (n > 0 || m["mode"] != "off" || recent > 0).into());
        o.insert("summary".into(), summary.into());
        o.insert("sticks".into(), st.sticks.iter().map(Stick::to_json).collect::<Vec<_>>().into());
        o.insert(
            "dvb".into(),
            json!({ "loaded": loaded, "blacklisted": blacklisted, "where": where_, "fix": if *blacklisted { String::new() } else { fix_hint() } }),
        );
        if let Value::Object(mm) = m {
            for (k, v) in mm {
                o.insert(k, v);
            }
        }
        o.insert("probing".into(), self.probing.load(Ordering::SeqCst).into());
        o.insert("presets".into(), presets_json());
        let first = st.sticks.first().cloned();
        let rtl = json!({ "sensors": st.rtl433.devices.len(), "recent": recent, "messages": st.rtl433.seq });
        drop(st);
        o.insert("defaults".into(), json!({ "tcpPort": self.cfg.default_tcp_port(), "settings": self.stick_settings(first.as_ref()) }));
        o.insert("rtl433".into(), rtl);
        Value::Object(o)
    }

    fn rtl433_view(&self, since: f64, limit: f64) -> Value {
        let devices = self.devices_view(200, true);
        let st = lock(&self.st);
        let msgs: Vec<&parse::Rtl433Message> = st.rtl433.messages.iter().filter(|m| (m.seq as f64) > since).collect();
        let log = &st.rtl433.log;
        json!({
            "seq": st.rtl433.seq,
            "devices": devices,
            "messages": js_slice_last(&msgs, limit).iter().map(|m| m.to_json()).collect::<Vec<_>>(),
            "log": log[log.len().saturating_sub(50)..].iter().map(|(t, l)| json!({ "t": num(*t), "line": l })).collect::<Vec<_>>(),
        })
    }
}

fn spawn_error(e: &std::io::Error, prog: &std::path::Path) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("ENOENT: no such file or directory, posix_spawn '{}'", prog.display())
    } else {
        format!("{}: {e}", prog.display())
    }
}

/// `Number(x) || d`
fn number_or(v: Option<&str>, d: f64) -> f64 {
    let n = v.map(|s| js_number(&Value::String(s.into()))).unwrap_or(f64::NAN);
    if n.is_nan() || n == 0.0 { d } else { n }
}

fn route<F, Fut>(i: &Arc<Inner>, method: Method, path: &str, doc: Json, f: F) -> Route
where
    F: Fn(Arc<Inner>, benchbob_core::Request) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = benchbob_core::RouteResult> + Send + 'static,
{
    let i = i.clone();
    Route::new(method, path, Some(doc), move |r| f(i.clone(), r))
}

fn tool<F, Fut>(i: &Arc<Inner>, name: &str, desc: &str, schema: Json, f: F) -> Tool
where
    F: Fn(Arc<Inner>, Value) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Value, HttpError>> + Send + 'static,
{
    let i = i.clone();
    Tool::new(name, desc, schema, move |a| f(i.clone(), a))
}

#[async_trait]
impl Driver for Sdr {
    fn id(&self) -> &'static str {
        "sdr"
    }
    fn title(&self) -> &'static str {
        "RTL-SDR"
    }
    fn order(&self) -> i32 {
        62
    }
    fn description(&self) -> &'static str {
        "RTL2832U sticks: rtl_tcp server for SDR++/GQRX, or rtl_433 sensor decoding"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let i = &self.inner;
        let _ = i.ctx.set(ctx);
        i.check_dvb();
        i.scan_sticks();
        let epoch = i.epoch.load(Ordering::SeqCst);
        let me = i.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if me.epoch.load(Ordering::SeqCst) != epoch {
                    break;
                }
                me.poll_tcp_clients();
            }
        });
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) {
        self.inner.epoch.fetch_add(1, Ordering::SeqCst);
        self.inner.stop_mode().await;
    }

    fn udev(&self, ev: &UdevEvent) {
        let p = &ev.props;
        let dt = prop(p, "DEVTYPE");
        if prop(p, "SUBSYSTEM") != "usb" || (dt != "usb_device" && dt != "usb_interface") {
            return;
        }
        let i = &self.inner;
        let before: Vec<String> = lock(&i.st).sticks.iter().map(|s| s.devpath.clone()).collect();
        i.scan_sticks();
        let dp = prop(p, "DEVPATH").to_string();
        let added: Vec<Stick> = lock(&i.st).sticks.iter().filter(|s| !before.contains(&s.devpath)).cloned().collect();
        let removed = ev.action == "remove" && before.contains(&dp);
        let iface_of_stick = dt == "usb_interface" && lock(&i.st).sticks.iter().any(|s| dp.starts_with(&s.devpath));
        if added.is_empty() && !removed && !iface_of_stick {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let me = i.clone();
        rt.spawn(async move {
            me.check_dvb();
            me.ctx().changed();
        });
        for s in added {
            i.ctx().event(&format!("RTL-SDR stick plugged in: {} {} ({})", s.manufacturer, s.product, s.usb_id), None);
            // Let the DVB driver bind (or not) first, then probe if the stick is free.
            let me = i.clone();
            rt.spawn(async move {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                me.scan_sticks();
                let now = lock(&me.st).sticks.iter().find(|x| x.devpath == s.devpath).cloned();
                match now {
                    Some(n) if n.dvb_claimed => me.ctx().event(
                        &format!("RTL-SDR stick is held by the kernel DVB driver ({DVB_MODULE}); blacklist it to use the stick"),
                        None,
                    ),
                    Some(n) if lock(&me.st).cur.is_none() => {
                        if let Err(e) = me.probe(Some(&Value::String(n.key))).await {
                            me.ctx().log(&format!("probe: {}", e.message));
                        }
                    }
                    _ => {}
                }
            });
        }
        if removed {
            i.ctx().event("RTL-SDR stick unplugged", None);
            let cur_here = lock(&i.st).cur.as_ref().is_some_and(|c| c.stick.as_ref().is_some_and(|s| s.devpath == dp));
            if cur_here {
                let me = i.clone();
                rt.spawn(async move {
                    let _g = me.busy.lock().await;
                    me.stop_mode().await;
                });
            }
        }
        i.ctx().changed();
    }

    fn status(&self) -> Option<Json> {
        self.inner.ctx.get()?;
        Some(self.inner.status())
    }

    fn routes(&self) -> Vec<Route> {
        let i = &self.inner;
        let d = i.cfg.default_tcp_port();
        vec![
            route(
                i,
                Method::GET,
                "/api/sdr",
                json!("RTL-SDR sticks (tuner, EEPROM strings, DVB-driver state), current mode and connection info"),
                |i, _r| async move { reply(i.status()) },
            ),
            route(
                i,
                Method::POST,
                "/api/sdr/mode",
                json!({
                    "body": {
                        "mode": "rtl_tcp | rtl_433 | off",
                        "stick": "key from GET /api/sdr (optional)",
                        "force": false,
                        "rtl_tcp options": { "port": d, "bind": "0.0.0.0", "gain": "0 = auto, else dB", "ppm": 0, "sampleRate": 2048000, "freq": 100000000, "biasT": false },
                        "rtl_433 options": { "preset": preset_keys().join(" | "), "freqs": ["433.92M"], "hop": 60, "gain": "", "ppm": 0 },
                    },
                    "description": "Switch the stick between rtl_tcp server, rtl_433 decoder and off. Options are remembered per stick.",
                }),
                |i, r| async move {
                    let b = r.json()?;
                    reply(i.set_mode(b.get("mode").unwrap_or(&Value::Null), &b).await?)
                },
            ),
            route(
                i,
                Method::POST,
                "/api/sdr/probe",
                json!({ "body": { "stick": "optional key" }, "description": "Run rtl_test -t and rtl_eeprom (only while the mode is off): tuner type, gain table, EEPROM strings" }),
                |i, r| async move {
                    let b = r.json()?;
                    let p = i.probe(b.get("stick")).await?;
                    // `res ?? { ok: true }`
                    reply(if p.is_null() { json!({ "ok": true }) } else { p })
                },
            ),
            route(
                i,
                Method::GET,
                "/api/sdr/rtl433",
                json!({ "description": "rtl_433 decoded sensors (latest fields + histories) and messages", "query": { "since": "message seq; only newer messages", "limit": "messages (default 100)" } }),
                |i, r| async move {
                    let since = number_or(r.query("since"), 0.0);
                    let limit = number_or(r.query("limit"), 100.0).min(300.0);
                    reply(i.rtl433_view(since, limit))
                },
            ),
            route(i, Method::DELETE, "/api/sdr/rtl433", json!("Clear rtl_433 sensors, messages and log"), |i, _r| async move {
                lock(&i.st).rtl433.clear();
                i.ctx().changed();
                reply(json!({ "ok": true }))
            }),
        ]
    }

    fn tools(&self) -> Vec<Tool> {
        let i = &self.inner;
        vec![
            tool(
                i,
                "sdr_status",
                "RTL-SDR state: detected sticks (USB id, EEPROM manufacturer/product/serial, tuner from rtl_test), whether the kernel DVB driver is blacklisted or holding a stick, current mode (off / rtl_tcp / rtl_433) and how to connect to rtl_tcp.",
                json!({ "type": "object", "properties": {} }),
                |i, _a| async move { Ok(i.status()) },
            ),
            tool(
                i,
                "sdr_mode",
                "Switch the RTL-SDR stick. 'rtl_tcp' serves raw I/Q on a TCP port (default 1234) for SDR++/GQRX/SDR#. 'rtl_433' decodes ISM-band sensors (weather stations, TPMS, door sensors); read results with sdr_messages. 'off' releases the stick. Only one mode at a time.",
                json!({
                    "type": "object",
                    "required": ["mode"],
                    "properties": {
                        "mode": { "type": "string", "enum": ["off", "rtl_tcp", "rtl_433"] },
                        "stick": { "type": "string", "description": "Stick key from sdr_status (default: first stick)" },
                        "preset": { "type": "string", "enum": preset_keys(), "description": "rtl_433 frequency preset (default 433)" },
                        "freqs": { "type": "array", "items": { "type": "string" }, "description": "rtl_433 custom frequencies, e.g. [\"433.92M\",\"868.3M\"] (overrides preset)" },
                        "hop": { "type": "number", "description": "rtl_433 seconds per frequency when hopping (default 60)" },
                        "gain": { "description": "Gain in dB; rtl_tcp 0 / rtl_433 empty = auto" },
                        "ppm": { "type": "number", "description": "Frequency correction in ppm" },
                        "port": { "type": "number", "description": "rtl_tcp port (default 1234)" },
                        "bind": { "type": "string", "description": "rtl_tcp listen address (default 0.0.0.0)" },
                        "sampleRate": { "type": "number", "description": "rtl_tcp sample rate in S/s (default 2048000)" },
                        "freq": { "type": "number", "description": "rtl_tcp initial centre frequency in Hz (clients usually retune)" },
                    },
                }),
                |i, a| async move { i.set_mode(a.get("mode").unwrap_or(&Value::Null), &a).await },
            ),
            tool(
                i,
                "sdr_messages",
                "rtl_433 results: one entry per sensor (model, id, channel, latest fields such as temperature_C/humidity/battery_ok, rssi/snr, count, last seen) plus the most recent raw decoded messages.",
                json!({ "type": "object", "properties": { "limit": { "type": "number", "description": "Recent messages to include (default 20)" }, "sensors": { "type": "number", "description": "Max sensors (default 50)" } } }),
                |i, a| async move {
                    let or = |k: &str, d: f64| {
                        let n = a.get(k).map(js_number).unwrap_or(f64::NAN);
                        if n.is_nan() || n == 0.0 { d } else { n }
                    };
                    let sensors = or("sensors", 50.0).min(200.0);
                    let limit = or("limit", 20.0).min(300.0);
                    let mode = i.status_mode()["mode"].clone();
                    let sensors = i.devices_view(if sensors > 0.0 { sensors as usize } else { 0 }, false);
                    let st = lock(&i.st);
                    let msgs: Vec<Value> = js_slice_last(&st.rtl433.messages, limit).iter().map(|m| m.msg.clone()).collect();
                    Ok(json!({ "mode": mode, "sensors": sensors, "messages": msgs }))
                },
            ),
        ]
    }
}

#[cfg(test)]
mod tests;
