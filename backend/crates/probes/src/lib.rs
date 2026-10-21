//! The debug probes panel, ported from `peripherals/drivers/probes.ts`.
//!
//! Spots debug probes (ST-Link, J-Link, CMSIS-DAP, ESP-Prog, ESP USB-JTAG, Black Magic
//! Probe, WCH-Link, Atmel-ICE) and chips sitting in their bootloaders (RP2 BOOTSEL, STM32
//! DFU), then turns each probe into a LAN GDB server by running OpenOCD as a managed child
//! (3333/4444/6666, +10 per probe, + BENCHBOB_PORT_OFFSET).
//!
//! Every action (detect, flash, erase, reset, memory reads) goes through OpenOCD's TCL RPC
//! port. If no GDB server is running for the probe, a throwaway OpenOCD is started on a
//! loopback-only port for the duration of the action, so there is one code path and one log
//! parser either way, and a running server is never fought over by a second OpenOCD.
//!
//! `BENCHBOB_PROBES_SIM=<scratch dir>` adds simulated devices (one of them OpenOCD's dummy
//! adapter, so the server lifecycle and TCL path run end to end, and an RP2 BOOTSEL drive
//! backed by the scratch dir).

pub mod logic;
pub mod tcl;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::Method;
use benchbob_core::driver::{Socket, WsClient, WsSession};
use benchbob_core::http::{HttpError, js_string, json as reply, truthy};
use benchbob_core::paths::{env, port_offset};
use benchbob_core::proc::{Proc, SIGKILL, SIGTERM, Split};
use benchbob_core::run::{Opts, read_text, run, which_in};
use benchbob_core::timeline::now_ms;
use benchbob_core::udev::{Props, UdevEvent};
use benchbob_core::util::{base64_decode, hostname, prop, usb_name};
use benchbob_core::{Ctx, Driver, Json, Route, StopReason, Tool};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio::process::Command;
use tokio::sync::watch;

use crate::logic::{
    BootType, GENERIC_TARGET, NO_TARGET, OcdOptions, OcdState, Ports, ProbeType, READY_MARK, SessionPorts, TARGET_PRESETS, boot_type,
    client_snippets, default_speed, default_transport, firmware_kind, hexdump, jnum, openocd_args, parse_dfu_progress, parse_info_uf2,
    parse_num, parse_ocd_line, parse_read_memory, parse_uf2, pin_command, probe_type, shell_quote, stable_id, tcl_balanced, uf2_accepts,
    usb_location, usb_strings,
};
use crate::tcl::{TclClient, TclErr};

const SCRIPTS: &str = "/usr/share/openocd/scripts";
const LOG_MAX: usize = 400;
const UPLOAD_MAX: usize = 64 << 20;
const BASE_PORTS: Ports = Ports { gdb: 3333, telnet: 4444, tcl: 6666 };

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Clone)]
pub struct Config {
    pub port_offset: u16,
    /// `BENCHBOB_PROBES_SIM`: scratch dir for the simulated devices; empty = none.
    pub sim: String,
    /// This server's port (the snippets' upload URL).
    pub web_port: u16,
    /// Uploads, per instance: `/var/tmp/benchbob-probes-<PORT>`.
    pub tmp: PathBuf,
    /// Where tools are looked for: the service's PATH plus the sbin dirs.
    pub path: String,
    pub scripts: PathBuf,
    /// sysfs, for USB interface strings.
    pub sys: PathBuf,
    pub mounts: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        let web_port = env("PORT").and_then(|p| benchbob_core::http::parse_int(&p)).filter(|p| *p != 0).map(|p| p as u16).unwrap_or(80);
        Self {
            port_offset: port_offset(),
            sim: env("BENCHBOB_PROBES_SIM").unwrap_or_default(),
            web_port,
            tmp: PathBuf::from(format!("/var/tmp/benchbob-probes-{web_port}")),
            // sudo-installed tools live in /usr/sbin, which a service's PATH may lack.
            path: format!("{}:/usr/local/sbin:/usr/sbin:/sbin", env("PATH").unwrap_or_else(|| "/usr/bin:/bin".into())),
            scripts: SCRIPTS.into(),
            sys: "/sys".into(),
            mounts: "/proc/mounts".into(),
        }
    }
}

// ---------- state ----------

struct SessCore {
    /// starting | running | stopping | exited
    state: Mutex<&'static str>,
    stopped: AtomicBool,
    exit_code: Mutex<Option<i32>>,
    ready: watch::Sender<Option<Result<(), String>>>,
    /// Set once the exit handler has run (logged the exit, told the waiters).
    done: watch::Sender<bool>,
}

struct Session {
    core: Arc<SessCore>,
    proc: Arc<Proc>,
    /// The user's GDB server, as opposed to a one-shot action.
    persistent: bool,
    ports: SessionPorts,
    target: String,
    since: u64,
    tcl: Arc<TclClient>,
}

impl Session {
    fn state(&self) -> &'static str {
        *lock(&self.core.state)
    }
}

struct Dev {
    id: String,
    devpath: String,
    vid: String,
    pid: String,
    serial: String,
    vendor: String,
    model: String,
    log: Vec<String>,
    sockets: Vec<WsClient>,
    job: Option<Map<String, Value>>,
    last_job: Option<Map<String, Value>>,
    sim: bool,
}

impl Dev {
    fn new(p: &Props, id: &str) -> Self {
        let (vendor, model) = usb_name(p);
        Self {
            id: id.into(),
            devpath: prop(p, "DEVPATH").into(),
            vid: prop(p, "ID_VENDOR_ID").to_lowercase(),
            pid: prop(p, "ID_MODEL_ID").to_lowercase(),
            serial: prop(p, "ID_SERIAL_SHORT").into(),
            vendor,
            model,
            log: Vec::new(),
            sockets: Vec::new(),
            job: None,
            last_job: None,
            sim: !prop(p, "BENCHBOB_SIM").is_empty(),
        }
    }
    fn send(&mut self, msg: &Value) {
        let s = msg.to_string();
        self.sockets.retain(|w| w.send(s.clone()));
    }
    fn job_json(&self) -> (Value, Value) {
        (self.job.clone().map(Value::Object).unwrap_or(Value::Null), self.last_job.clone().map(Value::Object).unwrap_or(Value::Null))
    }
}

struct Probe {
    dev: Dev,
    ty: ProbeType,
    ttys: Vec<(String, String)>,
    session: Option<Arc<Session>>,
    ocd: OcdState,
}

struct Boot {
    dev: Dev,
    ty: BootType,
    /// /dev/sdX1 holding the BOOTSEL FAT.
    block: String,
    label: String,
    mount: String,
    ro: bool,
    /// Our own rw mount when the drive handler hadn't mounted it.
    private_mount: String,
    info: Option<Value>,
}

#[derive(Default)]
struct Tools {
    dfu_util: String,
    picotool: String,
    gdb: String,
}

#[derive(Default)]
struct St {
    probes: IndexMap<String, Probe>,
    boots: IndexMap<String, Boot>,
    openocd: String,
    openocd_version: String,
    tools: Tools,
    targets: Vec<String>,
    /// devpath → USB interface strings.
    iface_strings: HashMap<String, Vec<String>>,
}

impl St {
    fn dev_mut(&mut self, id: &str) -> Option<&mut Dev> {
        if let Some(p) = self.probes.get_mut(id) {
            return Some(&mut p.dev);
        }
        self.boots.get_mut(id).map(|b| &mut b.dev)
    }
}

struct Inner {
    cfg: Config,
    ctx: OnceLock<Ctx>,
    st: Mutex<St>,
    scan_gen: AtomicU64,
    epoch: AtomicU64,
    /// Whether the BOOTSEL mount poll is running.
    polling: AtomicBool,
}

pub struct Probes {
    inner: Arc<Inner>,
}

impl Default for Probes {
    fn default() -> Self {
        Self::new()
    }
}

impl Probes {
    pub fn new() -> Self {
        Self::with_config(Config::from_env())
    }
    pub fn with_config(cfg: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                cfg,
                ctx: OnceLock::new(),
                st: Mutex::new(St::default()),
                scan_gen: AtomicU64::new(0),
                epoch: AtomicU64::new(0),
                polling: AtomicBool::new(false),
            }),
        }
    }
}

/// The probe config (`ProbeConfig`): ports as configured; the offset is added when spawning.
#[derive(Clone)]
struct ProbeConfig {
    target: String,
    transport: String,
    speed: f64,
    ports: Map<String, Value>,
}

impl ProbeConfig {
    fn port(&self, k: &str) -> u32 {
        self.ports.get(k).and_then(Value::as_f64).unwrap_or(0.0) as u32
    }
    fn to_json(&self) -> Value {
        json!({ "target": self.target, "transport": self.transport, "speed": benchbob_core::http::num(self.speed), "ports": self.ports })
    }
}

fn http(e: TclErr) -> HttpError {
    match e {
        TclErr::Tcl(m) => HttpError::new(502, format!("OpenOCD: {m}")),
        TclErr::Io(m) => HttpError::new(502, m),
    }
}

fn hex(n: u64) -> String {
    format!("{n:x}")
}

/// The first non-loopback IPv4 address (`os.networkInterfaces()` order), else the host name.
fn host_addr() -> String {
    #[cfg(unix)]
    {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: getifaddrs allocates a list we walk read-only and free with freeifaddrs.
        unsafe {
            if libc::getifaddrs(&mut ifap) == 0 {
                let mut cur = ifap;
                let mut found = None;
                while !cur.is_null() {
                    let a = (*cur).ifa_addr;
                    if !a.is_null() && (*a).sa_family as i32 == libc::AF_INET && (*cur).ifa_flags & libc::IFF_LOOPBACK as u32 == 0 {
                        let sin = a as *const libc::sockaddr_in;
                        let ip = std::net::Ipv4Addr::from(u32::from_be((*sin).sin_addr.s_addr));
                        found = Some(ip.to_string());
                        break;
                    }
                    cur = (*cur).ifa_next;
                }
                libc::freeifaddrs(ifap);
                if let Some(f) = found {
                    return f;
                }
            }
        }
    }
    hostname()
}

/// A flag from the query string: missing/empty → default, 0/false/no/off → false.
fn flag(v: Option<&str>, dflt: bool) -> bool {
    match v {
        None | Some("") => dflt,
        Some(s) => !matches!(s.to_lowercase().as_str(), "0" | "false" | "no" | "off"),
    }
}

fn base36(mut n: u64) -> String {
    const D: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut s = Vec::new();
    while n > 0 {
        s.push(D[(n % 36) as usize]);
        n /= 36;
    }
    s.reverse();
    String::from_utf8(s).unwrap()
}

impl Inner {
    fn ctx(&self) -> &Ctx {
        self.ctx.get().expect("probes driver not initialised")
    }

    // ---------- logging + streaming ----------

    fn log_line_st(st: &mut St, id: &str, text: &str) {
        if let Some(d) = st.dev_mut(id) {
            d.log.push(text.into());
            if d.log.len() > LOG_MAX {
                let n = d.log.len() - LOG_MAX;
                d.log.drain(..n);
            }
            d.send(&json!({ "type": "line", "text": text }));
        }
    }

    fn log_line(&self, id: &str, text: &str) {
        Self::log_line_st(&mut lock(&self.st), id, text);
    }

    /// `setJob(d, patch)`: merge into the running job (None clears it); tell the sockets.
    fn set_job(&self, id: &str, patch: Option<Value>) {
        {
            let mut st = lock(&self.st);
            let Some(d) = st.dev_mut(id) else { return };
            match patch {
                None => d.job = None,
                Some(p) => {
                    let mut j = d.job.take().unwrap_or_else(|| {
                        json!({ "kind": "", "stage": "", "started": now_ms() }).as_object().cloned().unwrap_or_default()
                    });
                    if let Value::Object(p) = p {
                        for (k, v) in p {
                            j.insert(k, v);
                        }
                    }
                    d.job = Some(j);
                }
            }
            let (job, last) = d.job_json();
            d.send(&json!({ "type": "job", "job": job, "lastJob": last }));
        }
        self.ctx().changed();
    }

    /// Run a long action as the device's one job; records the outcome for the card.
    async fn job<F, Fut>(&self, id: &str, kind: &str, f: F) -> Result<Value, HttpError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Value, HttpError>>,
    {
        {
            let mut st = lock(&self.st);
            let d = st.dev_mut(id).ok_or_else(|| HttpError::new(404, format!("No probe \"{id}\"")))?;
            if let Some(j) = &d.job {
                return Err(HttpError::new(409, format!("Busy: {} in progress", js_string(&j["kind"]))));
            }
        }
        self.set_job(id, Some(json!({ "kind": kind, "stage": "starting", "started": now_ms() })));
        let r = f().await;
        {
            let mut st = lock(&self.st);
            if let Some(d) = st.dev_mut(id) {
                let mut last = d.job.clone().unwrap_or_default();
                last.insert("done".into(), now_ms().into());
                match &r {
                    Ok(v) => {
                        last.insert("stage".into(), "done".into());
                        last.insert("ok".into(), true.into());
                        last.insert("message".into(), v.get("message").and_then(Value::as_str).unwrap_or("").into());
                    }
                    Err(e) => {
                        last.insert("stage".into(), "failed".into());
                        last.insert("ok".into(), false.into());
                        last.insert("message".into(), e.message.clone().into());
                    }
                }
                // `{ ...job, stage, done, ok, message }`: stage keeps its place, the rest follow.
                let mut ordered = Map::new();
                for (k, v) in &last {
                    if !matches!(k.as_str(), "done" | "ok" | "message") {
                        ordered.insert(k.clone(), v.clone());
                    }
                }
                for k in ["done", "ok", "message"] {
                    ordered.insert(k.into(), last[k].clone());
                }
                d.last_job = Some(ordered);
            }
            if let Err(e) = &r {
                Self::log_line_st(&mut st, id, &format!("!! {kind} failed: {}", e.message));
            }
        }
        self.set_job(id, None);
        r
    }

    // ---------- settings ----------

    /// Each probe gets a slot so its default ports stay put: 3333/4444/6666, then +10 per slot.
    fn slot_for(&self, id: &str) -> u32 {
        let mut slots = self.ctx().setting("slots").and_then(|v| v.as_object().cloned()).unwrap_or_default();
        if let Some(n) = slots.get(id).and_then(Value::as_f64) {
            return n as u32;
        }
        let used: Vec<f64> = slots.values().filter_map(Value::as_f64).collect();
        let mut n = 0u32;
        while used.contains(&(n as f64)) {
            n += 1;
        }
        slots.insert(id.into(), n.into());
        self.ctx().set_setting("slots", Value::Object(slots));
        n
    }

    fn valid_target(st: &St, t: &str) -> bool {
        t == GENERIC_TARGET || t == NO_TARGET || st.targets.iter().any(|x| x == t)
    }

    fn default_target(t: &ProbeType) -> String {
        match t.kind {
            "sim-dummy" => NO_TARGET.into(),
            "esp-usb-jtag" => "target/esp32s3.cfg".into(),
            "ftdi" => "target/esp32.cfg".into(),
            _ if t.name.starts_with("Raspberry Pi Debug Probe") => "target/rp2040.cfg".into(),
            _ => GENERIC_TARGET.into(),
        }
    }

    fn config(&self, st: &St, id: &str, ty: &ProbeType) -> ProbeConfig {
        let saved = self.ctx().setting(&format!("probe:{id}")).and_then(|v| v.as_object().cloned()).unwrap_or_default();
        let slot = self.slot_for(id);
        let target = match saved.get("target").and_then(Value::as_str) {
            Some(t) if !t.is_empty() && Self::valid_target(st, t) => t.to_string(),
            _ => Self::default_target(ty),
        };
        let transport = match saved.get("transport").and_then(Value::as_str) {
            Some(t) if ty.transports.contains(&t) => t.to_string(),
            _ => default_transport(ty, &target).to_string(),
        };
        let speed = saved.get("speed").and_then(Value::as_f64).filter(|s| *s != 0.0).unwrap_or_else(|| default_speed(ty, &target) as f64);
        let mut ports = Map::new();
        ports.insert("gdb".into(), (BASE_PORTS.gdb + 10 * slot).into());
        ports.insert("telnet".into(), (BASE_PORTS.telnet + 10 * slot).into());
        ports.insert("tcl".into(), (BASE_PORTS.tcl + 10 * slot).into());
        if let Some(Value::Object(p)) = saved.get("ports") {
            for (k, v) in p {
                ports.insert(k.clone(), v.clone());
            }
        }
        ProbeConfig { target, transport, speed, ports }
    }

    fn effective_ports(&self, c: &ProbeConfig) -> Ports {
        let o = self.cfg.port_offset as u32;
        Ports { gdb: c.port("gdb") + o, telnet: c.port("telnet") + o, tcl: c.port("tcl") + o }
    }

    fn set_config(&self, id: &str, b: &Value) -> Result<ProbeConfig, HttpError> {
        let st = lock(&self.st);
        let p = st.probes.get(id).ok_or_else(|| HttpError::new(404, format!("No probe \"{id}\"")))?;
        let ty = p.ty.clone();
        let cur = self.config(&st, id, &ty);
        let mut next = cur.clone();
        if let Some(t) = b.get("target") {
            let ts = t.as_str().unwrap_or("");
            if !t.is_string() || !Self::valid_target(&st, ts) {
                return Err(HttpError::bad_request(format!("Unknown target script \"{}\" (see GET /api/probes/targets)", js_string(t))));
            }
            next.target = ts.into();
            // A target change resets transport/speed to what suits it unless also given.
            if b.get("transport").is_none() {
                next.transport = default_transport(&ty, &next.target).into();
            }
            if b.get("speed").is_none() {
                next.speed = default_speed(&ty, &next.target) as f64;
            }
        }
        if let Some(t) = b.get("transport") {
            match t.as_str().filter(|t| ty.transports.contains(t)) {
                Some(t) => next.transport = t.into(),
                None => return Err(HttpError::bad_request(format!("{} supports {} only", ty.name, ty.transports.join("/")))),
            }
        }
        if b.get("speed").is_some() {
            match parse_num(b.get("speed")) {
                Some(s) if (1.0..=100000.0).contains(&s) => next.speed = s,
                _ => return Err(HttpError::bad_request("speed is in kHz, 1–100000")),
            }
        }
        for k in ["gdb", "telnet", "tcl"] {
            let Some(v) = b.get("ports").and_then(|p| p.get(k)) else { continue };
            match parse_num(Some(v)) {
                Some(n) if (1024.0..=65000.0).contains(&n) => {
                    next.ports.insert(k.into(), benchbob_core::http::num(n));
                }
                _ => return Err(HttpError::bad_request(format!("{k} port must be 1024–65000"))),
            }
        }
        let ps = [next.port("gdb"), next.port("telnet"), next.port("tcl")];
        if ps[0] == ps[1] || ps[1] == ps[2] || ps[0] == ps[2] {
            return Err(HttpError::bad_request("The three ports must differ"));
        }
        for (oid, o) in &st.probes {
            if oid == id || o.ty.iface.is_none() {
                continue;
            }
            let oc = self.config(&st, oid, &o.ty);
            if [oc.port("gdb"), oc.port("telnet"), oc.port("tcl")].iter().any(|x| ps.contains(x)) {
                return Err(HttpError::bad_request(format!("Port clash with {} ({oid})", o.ty.name)));
            }
        }
        drop(st);
        self.ctx().set_setting(&format!("probe:{id}"), next.to_json());
        self.ctx().changed();
        Ok(next)
    }

    // ---------- detection ----------

    fn sim_devices(&self) -> Vec<Props> {
        let p = |kv: Vec<(&str, String)>| -> Props { kv.into_iter().map(|(k, v)| (k.to_string(), v)).collect() };
        let usb = |n: &str, vid: &str, pid: &str, model: &str, serial: &str| {
            p(vec![
                ("DEVPATH", format!("/devices/sim/usb9/9-1.{n}")),
                ("SUBSYSTEM", "usb".into()),
                ("DEVTYPE", "usb_device".into()),
                ("ID_VENDOR_ID", vid.into()),
                ("ID_MODEL_ID", pid.into()),
                ("ID_MODEL", model.replace(' ', "_")),
                ("ID_MODEL_ENC", model.replace(' ', "\\x20")),
                ("ID_SERIAL_SHORT", serial.into()),
                ("ID_SERIAL", format!("{}_{serial}", model.replace(' ', "_"))),
                ("BENCHBOB_SIM", "1".into()),
            ])
        };
        let tty = |n: &str, iface: &str, dev: &str| {
            let num: u32 = iface.parse().unwrap_or(0);
            p(vec![
                ("DEVPATH", format!("/devices/sim/usb9/9-1.{n}/9-1.{n}:1.{num}/tty/{dev}")),
                ("SUBSYSTEM", "tty".into()),
                ("DEVNAME", format!("/dev/{dev}")),
                ("ID_USB_INTERFACE_NUM", iface.into()),
            ])
        };
        vec![
            usb("1", "0483", "374b", "STM32 STLink", "066DFF383133524157185615"),
            usb("2", "2e8a", "000c", "Debugprobe on Pico (CMSIS-DAP)", "E6614C311B597D32"),
            usb("3", "1d50", "6018", "Black Magic Probe v1.10.0", "7BB180B4"),
            tty("3", "00", "ttyACM90"),
            tty("3", "02", "ttyACM91"),
            usb("4", "303a", "1001", "USB JTAG/serial debug unit", "F4:12:FA:5C:3B:A8"),
            tty("4", "00", "ttyACM92"),
            usb("5", "2e8a", "0003", "RP2 Boot", "E0C9125B0D9B"),
            p(vec![
                ("DEVPATH", "/devices/sim/usb9/9-1.5/host0/block/sdz/sdz1".into()),
                ("SUBSYSTEM", "block".into()),
                ("DEVTYPE", "partition".into()),
                ("DEVNAME", "/dev/sdz1".into()),
                ("ID_FS_TYPE", "vfat".into()),
                ("ID_FS_LABEL", "RPI-RP2".into()),
            ]),
            usb("6", "0483", "df11", "STM32 BOOTLOADER", "205D35A3424B"),
            // Not a real probe: OpenOCD's dummy adapter, so the server lifecycle and TCL path
            // run end to end.
            usb("7", "dead", "beef", "BenchBob dummy probe", "SIM0001"),
        ]
    }

    fn all_devices(&self) -> Vec<Props> {
        let mut v = self.ctx().devices(None);
        if !self.cfg.sim.is_empty() {
            v.extend(self.sim_devices());
        }
        v
    }

    fn classify(&self, st: &St, p: &Props) -> (Option<ProbeType>, Option<BootType>) {
        let mut vid = prop(p, "ID_VENDOR_ID").to_lowercase();
        let mut pid = prop(p, "ID_MODEL_ID").to_lowercase();
        if vid.is_empty() && !prop(p, "PRODUCT").is_empty() {
            // PRODUCT=483/3748/100 (hex, unpadded)
            let mut it = prop(p, "PRODUCT").split('/');
            vid = format!("{:0>4}", it.next().unwrap_or(""));
            pid = format!("{:0>4}", it.next().unwrap_or(""));
        }
        if vid.is_empty() {
            return (None, None);
        }
        if !prop(p, "BENCHBOB_SIM").is_empty() && vid == "dead" {
            return (
                Some(ProbeType {
                    kind: "sim-dummy",
                    name: "Dummy probe (simulated)",
                    iface: Some("interface/dummy.cfg"),
                    transports: vec!["jtag"],
                    note: Some("OpenOCD's dummy adapter, for testing BenchBob itself."),
                    gdb_built_in: false,
                    esp_serial: false,
                }),
                None,
            );
        }
        if let Some(b) = boot_type(&vid, &pid) {
            return (None, Some(b));
        }
        let mut strings = usb_strings(p);
        strings.extend(st.iface_strings.get(prop(p, "DEVPATH")).cloned().unwrap_or_default());
        (probe_type(&vid, &pid, &strings), None)
    }

    /// USB interface strings live only in sysfs; CMSIS-DAP probes put "CMSIS-DAP" there.
    fn read_iface_strings(&self, devpath: &str) -> Vec<String> {
        let dir = self.cfg.sys.join(devpath.trim_start_matches('/'));
        let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
        let mut subs: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .filter(|n| {
                let Some((a, b)) = n.split_once(':') else { return false };
                let ok_a = a.split_once('-').is_some_and(|(x, y)| {
                    !x.is_empty()
                        && x.bytes().all(|c| c.is_ascii_digit())
                        && !y.is_empty()
                        && y.bytes().all(|c| c.is_ascii_digit() || c == b'.')
                });
                let ok_b = b
                    .split_once('.')
                    .is_some_and(|(x, y)| !x.is_empty() && !y.is_empty() && (x.to_string() + y).bytes().all(|c| c.is_ascii_digit()));
                ok_a && ok_b
            })
            .collect();
        subs.sort();
        subs.iter().map(|s| read_text(dir.join(s).join("interface"))).filter(|s| !s.is_empty()).collect()
    }

    async fn scan(self: &Arc<Self>) {
        let all = self.all_devices();
        let usb: Vec<&Props> = all.iter().filter(|p| prop(p, "SUBSYSTEM") == "usb" && prop(p, "DEVTYPE") == "usb_device").collect();
        // Interface strings only matter for devices the VID:PID table doesn't already know.
        let missing: Vec<String> = {
            let st = lock(&self.st);
            usb.iter()
                .filter(|p| prop(p, "BENCHBOB_SIM").is_empty() && !st.iface_strings.contains_key(prop(p, "DEVPATH")))
                .map(|p| prop(p, "DEVPATH").to_string())
                .collect()
        };
        let fresh: Vec<(String, Vec<String>)> = missing.into_iter().map(|d| (d.clone(), self.read_iface_strings(&d))).collect();
        let ctx = self.ctx();
        let mut to_stop: Vec<Arc<Session>> = Vec::new();
        let mut unmount: Vec<String> = Vec::new();
        {
            let mut st = lock(&self.st);
            st.iface_strings.extend(fresh);
            let mut seen_p: Vec<String> = Vec::new();
            let mut seen_b: Vec<String> = Vec::new();
            for p in &usb {
                let (probe, boot) = self.classify(&st, p);
                if probe.is_none() && boot.is_none() {
                    continue;
                }
                let id = stable_id(p);
                let dp = prop(p, "DEVPATH").to_string();
                if let Some(ty) = probe {
                    seen_p.push(id.clone());
                    if !st.probes.contains_key(&id) {
                        let d = Dev::new(p, &id);
                        if !d.sim {
                            ctx.event(
                                &format!(
                                    "Debug probe connected: {}{}",
                                    ty.name,
                                    if d.serial.is_empty() { String::new() } else { format!(" ({})", d.serial) }
                                ),
                                Some(json!({ "id": id, "vid": d.vid, "pid": d.pid })),
                            );
                        }
                        st.probes.insert(id.clone(), Probe { dev: d, ty, ttys: Vec::new(), session: None, ocd: OcdState::default() });
                    }
                    let mut ttys: Vec<(String, String)> = all
                        .iter()
                        .filter(|t| {
                            prop(t, "SUBSYSTEM") == "tty"
                                && !prop(t, "DEVNAME").is_empty()
                                && prop(t, "DEVPATH").starts_with(&format!("{dp}/"))
                        })
                        .map(|t| (prop(t, "DEVNAME").to_string(), prop(t, "ID_USB_INTERFACE_NUM").to_string()))
                        .collect();
                    ttys.sort_by(|a, b| benchbob_core::util::locale_cmp(&a.1, &b.1));
                    let pr = st.probes.get_mut(&id).unwrap();
                    pr.dev.devpath = dp;
                    pr.ttys = ttys;
                } else if let Some(ty) = boot {
                    seen_b.push(id.clone());
                    if !st.boots.contains_key(&id) {
                        let d = Dev::new(p, &id);
                        if !d.sim {
                            ctx.event(&format!("{} connected", ty.name), Some(json!({ "id": id })));
                        }
                        st.boots.insert(
                            id.clone(),
                            Boot {
                                dev: d,
                                ty,
                                block: String::new(),
                                label: String::new(),
                                mount: String::new(),
                                ro: false,
                                private_mount: String::new(),
                                info: None,
                            },
                        );
                    }
                    let parts: Vec<&Props> = all
                        .iter()
                        .filter(|x| {
                            prop(x, "SUBSYSTEM") == "block"
                                && prop(x, "DEVPATH").starts_with(&format!("{dp}/"))
                                && !prop(x, "ID_FS_TYPE").is_empty()
                        })
                        .collect();
                    let part = parts.iter().find(|x| prop(x, "DEVTYPE") == "partition").or(parts.first());
                    let b = st.boots.get_mut(&id).unwrap();
                    b.dev.devpath = dp;
                    b.block = part.map(|x| prop(x, "DEVNAME").to_string()).unwrap_or_default();
                    b.label = part.map(|x| prop(x, "ID_FS_LABEL").to_string()).unwrap_or_default();
                }
            }
            let gone: Vec<String> = st.probes.keys().filter(|k| !seen_p.contains(k)).cloned().collect();
            for id in gone {
                let mut pr = st.probes.shift_remove(&id).unwrap();
                if let Some(s) = pr.session.take() {
                    to_stop.push(s);
                }
                for w in &pr.dev.sockets {
                    w.close();
                }
                ctx.event(&format!("Debug probe removed: {}", pr.ty.name), Some(json!({ "id": id })));
            }
            let gone: Vec<String> = st.boots.keys().filter(|k| !seen_b.contains(k)).cloned().collect();
            for id in gone {
                let b = st.boots.shift_remove(&id).unwrap();
                if !b.private_mount.is_empty() {
                    unmount.push(b.private_mount.clone());
                }
                for w in &b.dev.sockets {
                    w.close();
                }
                ctx.event(&format!("{} left bootloader mode", b.ty.name), Some(json!({ "id": id })));
            }
            let live: Vec<String> = usb.iter().map(|p| prop(p, "DEVPATH").to_string()).collect();
            st.iface_strings.retain(|k, _| live.contains(k));
        }
        for s in to_stop {
            tokio::spawn(async move { stop_session(&s).await });
        }
        for m in unmount {
            tokio::spawn(async move {
                run(&["sudo", "umount", "-l", &m], Opts::default()).await;
                run(&["sudo", "rmdir", &m], Opts::default()).await;
            });
        }
        self.refresh_mounts();
        // BOOTSEL drives get mounted a second or two after they appear; poll until they are.
        let any_boots = !lock(&self.st).boots.is_empty();
        if any_boots && !self.polling.swap(true, Ordering::SeqCst) {
            let me = self.clone();
            let epoch = self.epoch.load(Ordering::SeqCst);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(2500)).await;
                    if me.epoch.load(Ordering::SeqCst) != epoch || lock(&me.st).boots.is_empty() {
                        me.polling.store(false, Ordering::SeqCst);
                        return;
                    }
                    me.refresh_mounts();
                }
            });
        }
        ctx.changed();
    }

    fn schedule_scan(self: &Arc<Self>) {
        let g = self.scan_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let me = self.clone();
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if me.scan_gen.load(Ordering::SeqCst) == g {
                me.scan().await;
            }
        });
    }

    fn refresh_mounts(&self) {
        let text = read_text(&self.cfg.mounts);
        let mounts: Vec<Vec<&str>> = text.split('\n').map(|l| l.split(' ').collect()).collect();
        let mut changed = false;
        let mut st = lock(&self.st);
        for b in st.boots.values_mut() {
            if b.ty.kind != "rp2-bootsel" {
                continue;
            }
            let (mut mount, mut ro) = (String::new(), false);
            if b.dev.sim {
                mount = format!("{}/{}", self.cfg.sim, b.label);
                let _ = std::fs::create_dir_all(&mount);
                let info = Path::new(&mount).join("INFO_UF2.TXT");
                if !info.exists() {
                    let _ = std::fs::write(&info, "UF2 Bootloader v3.0\r\nModel: Raspberry Pi RP2\r\nBoard-ID: RPI-RP2\r\n");
                }
            } else if !b.block.is_empty()
                && let Some(m) = mounts.iter().find(|f| f.first() == Some(&b.block.as_str()))
            {
                mount = unescape_mount(m.get(1).unwrap_or(&""));
                ro = m.get(3).is_some_and(|o| o.split(',').any(|x| x == "ro"));
            }
            if mount != b.mount || ro != b.ro {
                b.info = (!mount.is_empty()).then(|| parse_info_uf2(&read_text(Path::new(&mount).join("INFO_UF2.TXT"))));
                b.mount = mount;
                b.ro = ro;
                changed = true;
            }
        }
        drop(st);
        if changed {
            self.ctx().changed();
        }
    }

    // ---------- OpenOCD sessions ----------

    fn ocd_options(&self, id: &str, c: &ProbeConfig, bindto: &str, ports: SessionPorts) -> Result<OcdOptions, HttpError> {
        let st = lock(&self.st);
        let p = st.probes.get(id).ok_or_else(|| HttpError::new(404, format!("No probe \"{id}\"")))?;
        let Some(iface) = p.ty.iface else {
            return Err(HttpError::bad_request(format!("{}: {}", p.ty.name, p.ty.note.unwrap_or("OpenOCD can't drive this probe"))));
        };
        Ok(OcdOptions {
            iface: iface.into(),
            target: c.target.clone(),
            transport: c.transport.clone(),
            speed: c.speed,
            pin: if p.dev.sim { None } else { pin_command(p.ty.kind, &p.dev.pid, &p.dev.serial, &p.dev.devpath) },
            bindto: bindto.into(),
            ports,
        })
    }

    fn spawn_session(self: &Arc<Self>, id: &str, o: OcdOptions, persistent: bool) -> Result<Arc<Session>, HttpError> {
        let openocd = lock(&self.st).openocd.clone();
        if openocd.is_empty() {
            return Err(HttpError::new(503, "openocd isn't installed (apt package: openocd)"));
        }
        let args = openocd_args(&o);
        {
            let mut st = lock(&self.st);
            if let Some(p) = st.probes.get_mut(id) {
                p.ocd = OcdState::default();
            }
            Self::log_line_st(&mut st, id, &format!("$ openocd {}", args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")));
        }
        let (ready_tx, _) = watch::channel(None);
        let core = Arc::new(SessCore {
            state: Mutex::new("starting"),
            stopped: AtomicBool::new(false),
            exit_code: Mutex::new(None),
            ready: ready_tx,
            done: watch::channel(false).0,
        });
        let init_done = Arc::new(AtomicBool::new(false));
        let me = self.clone();
        let core2 = core.clone();
        let pid = id.to_string();
        let tcl_port = o.ports.tcl;
        let on_line = Arc::new(move |line: &str| {
            let mut vis;
            {
                let mut st = lock(&me.st);
                Self::log_line_st(&mut st, &pid, line);
                let Some(p) = st.probes.get_mut(&pid) else { return };
                vis = parse_ocd_line(&mut p.ocd, line);
                if line.trim() == READY_MARK {
                    init_done.store(true, Ordering::SeqCst);
                }
                let mut state = lock(&core2.state);
                if *state == "starting" && init_done.load(Ordering::SeqCst) && p.ocd.listening.get("tcl") == Some(&tcl_port) {
                    *state = "running";
                    core2.ready.send_if_modified(|r| {
                        if r.is_none() {
                            *r = Some(Ok(()));
                            true
                        } else {
                            false
                        }
                    });
                    vis = true;
                }
            }
            if vis {
                me.ctx().changed();
            }
        });
        let mut cmd = Command::new(&openocd);
        cmd.args(&args).env("PATH", &self.cfg.path);
        let proc = Proc::spawn(self.ctx(), &mut cmd, Split::CrLf, on_line).map_err(|e| HttpError::new(500, format!("{openocd}: {e}")))?;
        let s = Arc::new(Session {
            core: core.clone(),
            proc: proc.clone(),
            persistent,
            ports: o.ports,
            target: o.target.clone(),
            since: now_ms(),
            tcl: Arc::new(TclClient::new(o.ports.tcl)),
        });
        let me = self.clone();
        let s2 = s.clone();
        let pid = id.to_string();
        tokio::spawn(async move {
            let code = proc.wait().await;
            *lock(&s2.core.exit_code) = Some(code);
            let was = std::mem::replace(&mut *lock(&s2.core.state), "exited");
            s2.tcl.close_now();
            let (why, name, mine) = {
                let mut st = lock(&me.st);
                Self::log_line_st(&mut st, &pid, &format!("-- openocd exited ({code})"));
                match st.probes.get(&pid) {
                    Some(p) => {
                        let why = p.ocd.errors.last().cloned().unwrap_or_else(|| format!("openocd exited with code {code}"));
                        let full = if p.ocd.hint.is_empty() { why.clone() } else { format!("{why} — {}", p.ocd.hint) };
                        let mine = p.session.as_ref().is_some_and(|x| Arc::ptr_eq(x, &s2));
                        (full, p.ty.name, mine)
                    }
                    None => (format!("openocd exited with code {code}"), "", false),
                }
            };
            s2.core.ready.send_if_modified(|r| {
                if r.is_none() {
                    *r = Some(Err(why.clone()));
                    true
                } else {
                    false
                }
            });
            if s2.persistent && was != "stopping" && mine {
                let short = why.split(" — ").next().unwrap_or(&why).to_string();
                me.ctx().event(&format!("GDB server for {name} stopped: {short}"), Some(json!({ "id": pid })));
            }
            me.ctx().changed();
            let _ = s2.core.done.send(true);
        });
        Ok(s)
    }

    async fn wait_ready(s: &Session) -> Result<(), String> {
        let mut rx = s.core.ready.subscribe();
        let r = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(r) = rx.borrow_and_update().clone() {
                    return r;
                }
                if rx.changed().await.is_err() {
                    return Err("OpenOCD stopped".to_string());
                }
            }
        })
        .await;
        r.unwrap_or_else(|_| Err("OpenOCD didn't become ready within 20 s".into()))
    }

    async fn start_server(self: &Arc<Self>, id: &str, over: Map<String, Value>) -> Result<Value, HttpError> {
        let (busy, running, ty) = {
            let st = lock(&self.st);
            let p = st.probes.get(id).ok_or_else(|| HttpError::new(404, format!("No probe \"{id}\"")))?;
            (p.dev.job.as_ref().map(|j| js_string(&j["kind"])), p.session.as_ref().is_some_and(|s| s.state() != "exited"), p.ty.clone())
        };
        if running {
            return Err(HttpError::new(409, "A GDB server is already running for this probe"));
        }
        if let Some(k) = busy {
            return Err(HttpError::new(409, format!("Busy: {k} in progress")));
        }
        let c = if over.is_empty() {
            let st = lock(&self.st);
            self.config(&st, id, &ty)
        } else {
            self.set_config(id, &Value::Object(over))?
        };
        let eff = self.effective_ports(&c);
        let o = self.ocd_options(id, &c, "0.0.0.0", SessionPorts { gdb: Some(eff.gdb), telnet: Some(eff.telnet), tcl: eff.tcl })?;
        let s = self.spawn_session(id, o, true)?;
        if let Some(p) = lock(&self.st).probes.get_mut(id) {
            p.session = Some(s.clone());
        }
        self.ctx().changed();
        if let Err(e) = Self::wait_ready(&s).await {
            if s.state() != "exited" {
                stop_session(&s).await;
            }
            return Err(HttpError::new(502, e));
        }
        self.ctx().event(&format!("GDB server for {} on :{}", ty.name, eff.gdb), Some(json!({ "id": id, "target": c.target })));
        Ok(self.probe_status_of(id))
    }

    async fn stop_server(&self, id: &str) -> Value {
        let s = lock(&self.st).probes.get(id).and_then(|p| p.session.clone());
        if let Some(s) = s
            && s.state() != "exited"
        {
            stop_session(&s).await;
        }
        self.probe_status_of(id)
    }

    /// Run `f` against the probe's TCL port: the GDB server's, or a throwaway loopback OpenOCD's.
    async fn with_tcl<F, Fut>(self: &Arc<Self>, id: &str, f: F) -> Result<Value, HttpError>
    where
        F: FnOnce(Arc<TclClient>) -> Fut,
        Fut: std::future::Future<Output = Result<Value, HttpError>>,
    {
        let (s, ty) = {
            let st = lock(&self.st);
            let p = st.probes.get(id).ok_or_else(|| HttpError::new(404, format!("No probe \"{id}\"")))?;
            (p.session.clone(), p.ty.clone())
        };
        if let Some(s) = &s {
            match s.state() {
                "running" => return f(s.tcl.clone()).await,
                st @ ("starting" | "stopping") => {
                    return Err(HttpError::new(409, format!("The GDB server is {st}; try again in a moment")));
                }
                _ => {}
            }
        }
        self.set_job(id, Some(json!({ "stage": "connecting" })));
        let c = {
            let st = lock(&self.st);
            self.config(&st, id, &ty)
        };
        let port = free_port().map_err(|e| HttpError::new(500, e.to_string()))?;
        let o = self.ocd_options(id, &c, "127.0.0.1", SessionPorts { gdb: None, telnet: None, tcl: port })?;
        let tmp = self.spawn_session(id, o, false)?;
        let r = match Self::wait_ready(&tmp).await {
            Ok(()) => f(tmp.tcl.clone()).await,
            Err(e) => Err(HttpError::new(502, e)),
        };
        // Let the last log lines land before tearing down.
        let _ = tmp.tcl.cmd("shutdown", false, Duration::from_secs(3)).await;
        stop_session(&tmp).await;
        r
    }

    // ---------- actions ----------

    fn probe_id(&self, id: Option<&Value>) -> Result<String, HttpError> {
        let st = lock(&self.st);
        match id.filter(|v| truthy(v)) {
            None => {
                if st.probes.len() == 1 {
                    return Ok(st.probes.keys().next().unwrap().clone());
                }
                Err(HttpError::bad_request(if st.probes.is_empty() {
                    "No debug probe connected".to_string()
                } else {
                    format!("Several probes are connected; give one of: {}", st.probes.keys().cloned().collect::<Vec<_>>().join(", "))
                }))
            }
            Some(v) => {
                let id = js_string(v);
                if st.probes.contains_key(&id) {
                    return Ok(id);
                }
                st.probes
                    .values()
                    .find(|p| p.dev.serial == id)
                    .map(|p| p.dev.id.clone())
                    .ok_or_else(|| HttpError::new(404, format!("No probe \"{id}\"")))
            }
        }
    }

    fn boot_id(&self, id: Option<&Value>) -> Result<String, HttpError> {
        let st = lock(&self.st);
        match id.filter(|v| truthy(v)) {
            None => {
                if st.boots.len() == 1 {
                    return Ok(st.boots.keys().next().unwrap().clone());
                }
                Err(HttpError::bad_request(if st.boots.is_empty() {
                    "No device in bootloader mode".to_string()
                } else {
                    format!("Several bootloader devices; give one of: {}", st.boots.keys().cloned().collect::<Vec<_>>().join(", "))
                }))
            }
            Some(v) => {
                let id = js_string(v);
                if st.boots.contains_key(&id) {
                    return Ok(id);
                }
                st.boots
                    .values()
                    .find(|b| b.dev.serial == id)
                    .map(|b| b.dev.id.clone())
                    .ok_or_else(|| HttpError::new(404, format!("No bootloader device \"{id}\"")))
            }
        }
    }

    fn need_ocd(&self, id: &str) -> Result<(), HttpError> {
        let st = lock(&self.st);
        let Some(p) = st.probes.get(id) else { return Ok(()) };
        if p.ty.gdb_built_in {
            let tty = p.ttys.first().map(|t| t.0.as_str()).unwrap_or("its first serial port");
            return Err(HttpError::bad_request(format!("{} is its own GDB server: connect GDB to {tty}", p.ty.name)));
        }
        Ok(())
    }

    async fn examine(self: &Arc<Self>, id: &str) -> Result<Value, HttpError> {
        self.need_ocd(id)?;
        let me = self.clone();
        let id2 = id.to_string();
        self.job(id, "detect", || async move {
            let me2 = me.clone();
            let id3 = id2.clone();
            me.with_tcl(&id2, |tcl| async move {
                me2.set_job(&id3, Some(json!({ "stage": "examining" })));
                let out = tcl.cmd("targets", true, Duration::from_secs(30)).await.map_err(http)?;
                let st = lock(&me2.st);
                let o = &st.probes.get(&id3).map(|p| p.ocd.clone()).unwrap_or_default();
                Ok(json!({
                    "targets": out.trim(), "adapter": o.adapter, "voltage": jnum(o.voltage), "dpidr": o.dpidr, "cpus": o.cpus,
                    "flash": o.flash, "errors": o.errors, "hint": o.hint,
                    "message": if o.cpus.is_empty() { "no core detected".to_string() } else { o.cpus.join(", ") },
                }))
            })
            .await
        })
        .await
    }

    fn save_upload(&self, buf: &[u8], name: &str) -> Result<PathBuf, HttpError> {
        if buf.is_empty() {
            return Err(HttpError::bad_request("Empty file"));
        }
        if buf.len() > UPLOAD_MAX {
            return Err(HttpError::new(413, format!("File too large (max {} MB)", UPLOAD_MAX >> 20)));
        }
        std::fs::create_dir_all(&self.cfg.tmp).map_err(|e| HttpError::new(500, e.to_string()))?;
        let ext = name
            .rfind('.')
            .map(|i| &name[i..])
            .filter(|e| (2..=6).contains(&e.len()) && e[1..].bytes().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or("")
            .to_lowercase();
        let rnd: String = base36(uuid::Uuid::new_v4().as_u128() as u64 & 0xffff_ffff_ffff).chars().take(6).collect();
        let path = self.cfg.tmp.join(format!("{}-{rnd}{ext}", base36(now_ms())));
        std::fs::write(&path, buf).map_err(|e| HttpError::new(500, e.to_string()))?;
        Ok(path)
    }

    async fn flash(
        self: &Arc<Self>,
        id: &str,
        buf: Vec<u8>,
        name: &str,
        address: Option<&Value>,
        verify: bool,
        reset: bool,
    ) -> Result<Value, HttpError> {
        self.need_ocd(id)?;
        let name = if name.is_empty() { "firmware.bin" } else { name };
        let kind = firmware_kind(&buf, name);
        if kind == "uf2" {
            return Err(HttpError::bad_request("That's a UF2 file: put the chip in BOOTSEL/bootloader mode and use the UF2 flash instead"));
        }
        let addr = parse_num(address);
        if let Some(a) = address.filter(|a| !a.is_null() && a.as_str() != Some(""))
            && addr.is_none()
        {
            return Err(HttpError::bad_request(format!("Bad address \"{}\"", js_string(a))));
        }
        if kind == "bin" && addr.is_none() {
            return Err(HttpError::bad_request("A .bin file needs a base address (e.g. 0x08000000 for STM32, 0x10000000 for RP2040)"));
        }
        let path = self.save_upload(&buf, name)?;
        let len = buf.len();
        let me = self.clone();
        let id2 = id.to_string();
        let p2 = path.display().to_string();
        let r = self
            .job(id, "flash", || async move {
                let me2 = me.clone();
                let id3 = id2.clone();
                me.with_tcl(&id2, |tcl| async move {
                    me2.set_job(&id3, Some(json!({ "stage": format!("programming {kind} ({len} bytes)") })));
                    let a = if kind == "bin" { format!(" 0x{}", hex(addr.unwrap_or(0.0) as u64)) } else { String::new() };
                    let cmd = format!("program {{{p2}}}{a}{}{}", if verify { " verify" } else { "" }, if reset { " reset" } else { "" });
                    me2.log_line(&id3, &format!("> {cmd}"));
                    let out = tcl.cmd(&cmd, true, Duration::from_secs(600)).await.map_err(http)?;
                    Ok(json!({
                        "ok": true, "kind": kind, "bytes": len, "address": jnum(addr), "verified": verify, "reset": reset, "output": out.trim(),
                        "message": format!("Programmed {len} bytes{}", if verify { ", verified" } else { "" }),
                    }))
                })
                .await
            })
            .await;
        let _ = std::fs::remove_file(&path);
        r
    }

    async fn erase(self: &Arc<Self>, id: &str) -> Result<Value, HttpError> {
        self.need_ocd(id)?;
        let me = self.clone();
        let id2 = id.to_string();
        self.job(id, "erase", || async move {
            let me2 = me.clone();
            let id3 = id2.clone();
            me.with_tcl(&id2, |tcl| async move {
                me2.set_job(&id3, Some(json!({ "stage": "halting" })));
                let t = Duration::from_secs(30);
                tcl.cmd("reset halt", true, t).await.map_err(http)?;
                let banks = tcl.cmd("flash list", false, t).await.map_err(http)?;
                let drv = banks.split("name ").nth(1).and_then(|r| r.split_whitespace().next()).unwrap_or("").to_string();
                if drv.is_empty() {
                    return Err(HttpError::bad_request("No flash bank: pick a real target (not the generic Cortex-M one)"));
                }
                me2.set_job(&id3, Some(json!({ "stage": format!("erasing ({drv})") })));
                // Fast whole-chip erase commands by flash driver; anything else erases by sector.
                let mass = match drv.as_str() {
                    "stm32f1x" => Some("stm32f1x mass_erase 0"),
                    "stm32f2x" => Some("stm32f2x mass_erase 0"),
                    "stm32l4x" => Some("stm32l4x mass_erase 0"),
                    "stm32h7x" => Some("stm32h7x mass_erase 0"),
                    "nrf5" => Some("nrf5 mass_erase"),
                    "at91samd" => Some("at91samd chip-erase"),
                    "atsame5" => Some("atsame5 chip-erase"),
                    _ => None,
                };
                let mut how = mass.unwrap_or("").to_string();
                let ok = match mass {
                    Some(m) => tcl.cmd(m, true, Duration::from_secs(120)).await.is_ok(),
                    None => false,
                };
                if !ok {
                    how = "flash erase_sector 0 0 last".into();
                    tcl.cmd(&how, true, Duration::from_secs(300)).await.map_err(http)?;
                }
                Ok(json!({ "ok": true, "driver": drv, "command": how, "message": format!("Erased with \"{how}\"") }))
            })
            .await
        })
        .await
    }

    async fn control(self: &Arc<Self>, id: &str, cmd: &'static str) -> Result<Value, HttpError> {
        self.need_ocd(id)?;
        let me = self.clone();
        let id2 = id.to_string();
        self.job(id, cmd, || async move {
            me.with_tcl(&id2, |tcl| async move {
                let out = tcl.cmd(cmd, true, Duration::from_secs(30)).await.map_err(http)?;
                Ok(json!({ "ok": true, "output": out.trim(), "message": cmd }))
            })
            .await
        })
        .await
    }

    async fn read_memory(self: &Arc<Self>, id: &str, address: Option<&Value>, length: Option<&Value>) -> Result<Value, HttpError> {
        self.need_ocd(id)?;
        let a = parse_num(address);
        let n = parse_num(length).unwrap_or(256.0);
        let Some(a) = a.filter(|a| (0.0..=4294967295.0).contains(a)) else {
            return Err(HttpError::bad_request("address must be a number (hex 0x… or decimal)"));
        };
        if !(1.0..=65536.0).contains(&n) {
            return Err(HttpError::bad_request("length must be 1–65536 bytes"));
        }
        let me = self.clone();
        let id2 = id.to_string();
        self.job(id, "read memory", || async move {
            me.with_tcl(&id2, |tcl| async move {
                let out = tcl
                    .cmd(
                        &format!("read_memory 0x{} 8 {}", hex(a as u64), benchbob_core::http::js_string(&benchbob_core::http::num(n))),
                        false,
                        Duration::from_secs(60),
                    )
                    .await
                    .map_err(http)?;
                let bytes = parse_read_memory(&out);
                Ok(json!({
                    "address": benchbob_core::http::num(a),
                    "length": bytes.len(),
                    "hex": bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                    "dump": hexdump(&bytes, a as u64),
                }))
            })
            .await
        })
        .await
    }

    async fn command(self: &Arc<Self>, id: &str, cmd: Option<&Value>) -> Result<Value, HttpError> {
        self.need_ocd(id)?;
        let cmd = cmd.and_then(Value::as_str).unwrap_or("").to_string();
        if cmd.trim().is_empty() {
            return Err(HttpError::bad_request("command is required"));
        }
        if !tcl_balanced(&cmd) {
            return Err(HttpError::bad_request("Unbalanced braces in command"));
        }
        let me = self.clone();
        let id2 = id.to_string();
        self.job(id, "command", || async move {
            let me2 = me.clone();
            let id3 = id2.clone();
            me.with_tcl(&id2, |tcl| async move {
                me2.log_line(&id3, &format!("> {cmd}"));
                let out = tcl.cmd(&cmd, true, Duration::from_secs(120)).await.map_err(http)?;
                Ok(json!({ "output": out }))
            })
            .await
        })
        .await
    }

    // ---------- bootloaders ----------

    async fn flash_uf2(self: &Arc<Self>, id: &str, buf: Vec<u8>, name: Option<&str>, force: bool) -> Result<Value, HttpError> {
        let (ty, sim, block, mount0, ro) = {
            let st = lock(&self.st);
            let b = st.boots.get(id).ok_or_else(|| HttpError::new(404, format!("No bootloader device \"{id}\"")))?;
            (b.ty.clone(), b.dev.sim, b.block.clone(), b.mount.clone(), b.ro)
        };
        if ty.kind != "rp2-bootsel" {
            return Err(HttpError::bad_request(format!("{} doesn't take UF2 files", ty.name)));
        }
        let info = parse_uf2(&buf).map_err(HttpError::bad_request)?;
        // The bootrom silently ignores blocks of a foreign family, so "success" would mean nothing.
        if !force && !uf2_accepts(ty.chip, &info.family_ids) {
            return Err(HttpError::bad_request(format!(
                "This UF2 is for {}; a {} would ignore it (pass force to copy anyway)",
                info.families.join(", "),
                ty.chip.to_uppercase()
            )));
        }
        let file = uf2_file_name(name.unwrap_or("firmware.uf2"));
        let me = self.clone();
        let id2 = id.to_string();
        self.job(id, "uf2", || async move {
            let fams = if info.families.is_empty() { "none".to_string() } else { info.families.join(", ") };
            me.log_line(
                &id2,
                &format!(
                    "UF2: {} blocks, {} bytes for 0x{}–0x{}, family {fams}",
                    info.blocks,
                    info.payload,
                    hex(info.start),
                    hex(info.end)
                ),
            );
            let result = |path: String, rebooted: bool, message: &str| {
                let mut m = Map::new();
                m.insert("ok".into(), true.into());
                for (k, v) in info.fields() {
                    m.insert(k, v);
                }
                m.insert("path".into(), path.into());
                m.insert("rebooted".into(), rebooted.into());
                m.insert("message".into(), message.into());
                Value::Object(m)
            };
            if sim {
                me.set_job(&id2, Some(json!({ "stage": "copying" })));
                let dst = format!("{mount0}/{file}");
                std::fs::write(&dst, &buf).map_err(|e| HttpError::new(500, e.to_string()))?;
                me.log_line(&id2, &format!("Copied to {dst} (simulated device: no reboot)"));
                return Ok(result(dst, false, "Copied (simulated)"));
            }
            if block.is_empty() {
                return Err(HttpError::new(409, "The BOOTSEL drive has no block device yet; wait a second and retry"));
            }
            let mut mount = mount0.clone();
            // The drive handler mounts every USB drive read-only on purpose. A BOOTSEL drive
            // is a fake FAT in the bootrom, so flipping it to rw is harmless; it's gone after
            // the reboot.
            if mount.is_empty() {
                mount = format!("/run/benchbob-uf2/{id2}");
                me.set_job(&id2, Some(json!({ "stage": "mounting" })));
                must_ok(&["sudo", "mkdir", "-p", &mount]).await?;
                must_ok(&["sudo", "mount", "-o", "rw,sync", &block, &mount]).await?;
                if let Some(b) = lock(&me.st).boots.get_mut(&id2) {
                    b.private_mount = mount.clone();
                }
            } else if ro {
                me.set_job(&id2, Some(json!({ "stage": "remounting rw" })));
                must_ok(&["sudo", "mount", "-o", "remount,rw", &mount]).await?;
            }
            let src = me.save_upload(&buf, &file)?;
            let src_s = src.display().to_string();
            me.set_job(&id2, Some(json!({ "stage": "copying" })));
            let dst = format!("{mount}/{file}");
            me.log_line(&id2, &format!("$ sudo dd if={src_s} of={dst} bs=64k conv=fsync"));
            let r = run(
                &["sudo", "dd", &format!("if={src_s}"), &format!("of={dst}"), "bs=64k", "conv=fsync", "status=none"],
                Opts::timeout_ms(120_000),
            )
            .await;
            // The chip reboots as the last block lands, often before dd's fsync returns, so an
            // I/O error isn't a failure if the device then disappears.
            me.set_job(&id2, Some(json!({ "stage": "waiting for reboot" })));
            let gone = me.wait_gone(&id2, 8000).await;
            let _ = std::fs::remove_file(&src);
            if r.code != 0 && !gone {
                let why = if !r.err.is_empty() {
                    r.err
                } else if !r.out.is_empty() {
                    r.out
                } else {
                    format!("exit {}", r.code)
                };
                return Err(HttpError::new(500, format!("Copy failed: {why}")));
            }
            me.log_line(
                &id2,
                if gone {
                    "Device rebooted into the new firmware"
                } else {
                    "Copied, but the device is still in BOOTSEL (wrong family or address?)"
                },
            );
            Ok(result(dst, gone, if gone { "Flashed; the board rebooted" } else { "Copied; the board didn't reboot" }))
        })
        .await
    }

    async fn wait_gone(&self, id: &str, ms: u64) -> bool {
        let end = now_ms() + ms;
        while now_ms() < end {
            if !lock(&self.st).boots.contains_key(id) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        !lock(&self.st).boots.contains_key(id)
    }

    async fn flash_dfu(self: &Arc<Self>, id: &str, buf: Vec<u8>, address: Option<&Value>, name: Option<&str>) -> Result<Value, HttpError> {
        let (ty, vid, pid, serial, dfu) = {
            let st = lock(&self.st);
            let b = st.boots.get(id).ok_or_else(|| HttpError::new(404, format!("No bootloader device \"{id}\"")))?;
            (b.ty.clone(), b.dev.vid.clone(), b.dev.pid.clone(), b.dev.serial.clone(), st.tools.dfu_util.clone())
        };
        if ty.kind != "stm32-dfu" {
            return Err(HttpError::bad_request(format!("{} isn't a DFU device", ty.name)));
        }
        if dfu.is_empty() {
            return Err(HttpError::new(503, "dfu-util isn't installed (apt package: dfu-util)"));
        }
        let addr = parse_num(address).unwrap_or(134217728.0) as u64;
        let name = name.unwrap_or("firmware.bin");
        if firmware_kind(&buf, name) != "bin" {
            return Err(HttpError::bad_request("DFU takes a raw .bin (convert with arm-none-eabi-objcopy -O binary)"));
        }
        let path = self.save_upload(&buf, name)?;
        let len = buf.len();
        let me = self.clone();
        let id2 = id.to_string();
        let p2 = path.display().to_string();
        let r = self
            .job(id, "dfu", || async move {
                let mut cmd: Vec<String> = vec![dfu.clone(), "-d".into(), format!("{vid}:{pid}")];
                if !serial.is_empty() {
                    cmd.push("-S".into());
                    cmd.push(serial.clone());
                }
                cmd.extend(["-a".into(), "0".into(), "-s".into(), format!("0x{}:leave", hex(addr)), "-D".into(), p2.clone()]);
                me.log_line(&id2, &format!("$ {}", cmd.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")));
                let me2 = me.clone();
                let id3 = id2.clone();
                let on_line = Arc::new(move |l: &str| match parse_dfu_progress(l) {
                    Some((phase, pct)) => me2.set_job(&id3, Some(json!({ "stage": phase.to_lowercase(), "pct": pct }))),
                    None => me2.log_line(&id3, l),
                });
                let mut c = Command::new(&cmd[0]);
                c.args(&cmd[1..]).env("PATH", &me.cfg.path);
                let proc = Proc::spawn(me.ctx(), &mut c, Split::CrLf, on_line).map_err(|e| HttpError::new(500, e.to_string()))?;
                let code = proc.wait().await;
                // dfu-util's ":leave" makes the chip jump to the app, which often trips a final error.
                let ok_tail = {
                    let st = lock(&me.st);
                    st.boots
                        .get(&id2)
                        .map(|b| {
                            b.dev.log[b.dev.log.len().saturating_sub(5)..]
                                .iter()
                                .any(|l| l.contains("File downloaded successfully") || l.contains("Submitting leave request"))
                        })
                        .unwrap_or(false)
                };
                if code != 0 && !ok_tail {
                    return Err(HttpError::new(500, format!("dfu-util exited {code}")));
                }
                Ok(json!({ "ok": true, "address": addr, "bytes": len, "message": format!("Downloaded {len} bytes to 0x{}", hex(addr)) }))
            })
            .await;
        let _ = std::fs::remove_file(&path);
        r
    }

    // ---------- status ----------

    fn session_status(s: &Session) -> Value {
        json!({
            "state": s.state(),
            "stopped": s.core.stopped.load(Ordering::SeqCst),
            "persistent": s.persistent,
            "ports": s.ports.to_json(),
            "target": s.target,
            "since": s.since,
            "exitCode": *lock(&s.core.exit_code),
            "pid": s.proc.pid(),
        })
    }

    fn probe_status(&self, st: &St, p: &Probe) -> Value {
        // Probes OpenOCD can't drive (BMP, WCH RISC-V mode) have no config and take no port slot.
        let config = p.ty.iface.map(|_| {
            let c = self.config(st, &p.dev.id, &p.ty);
            let mut v = c.to_json();
            v["effectivePorts"] = self.effective_ports(&c).to_json();
            v
        });
        let o = &p.ocd;
        let (job, last) = p.dev.job_json();
        json!({
            "id": p.dev.id,
            "kind": p.ty.kind,
            "name": p.ty.name,
            "vendor": p.dev.vendor,
            "model": p.dev.model,
            "vidpid": format!("{}:{}", p.dev.vid, p.dev.pid),
            "serial": p.dev.serial,
            "usb": usb_location(&p.dev.devpath).unwrap_or_default(),
            "note": p.ty.note.unwrap_or(""),
            "iface": p.ty.iface,
            "transports": p.ty.transports,
            "gdbBuiltIn": p.ty.gdb_built_in,
            "espSerial": p.ty.esp_serial,
            "ttys": p.ttys.iter().map(|(dev, iface)| json!({ "dev": dev, "iface": iface })).collect::<Vec<_>>(),
            "sim": p.dev.sim,
            "config": config,
            // A one-shot session is internal; only the user's GDB server shows as "the server".
            "server": p.session.as_ref().filter(|s| s.persistent).map(|s| Self::session_status(s)),
            "ocd": {
                "adapter": o.adapter, "voltage": jnum(o.voltage), "cpus": o.cpus, "dpidr": o.dpidr, "flash": o.flash,
                "gdbClients": o.gdb_clients, "errors": o.errors[o.errors.len().saturating_sub(3)..], "hint": o.hint,
            },
            "job": job,
            "lastJob": last,
        })
    }

    fn probe_status_of(&self, id: &str) -> Value {
        let st = lock(&self.st);
        st.probes.get(id).map(|p| self.probe_status(&st, p)).unwrap_or(Value::Null)
    }

    fn boot_status(st: &St, b: &Boot) -> Value {
        let (job, last) = b.dev.job_json();
        json!({
            "id": b.dev.id,
            "kind": b.ty.kind,
            "name": b.ty.name,
            "chip": b.ty.chip,
            "vendor": b.dev.vendor,
            "model": b.dev.model,
            "vidpid": format!("{}:{}", b.dev.vid, b.dev.pid),
            "serial": b.dev.serial,
            "usb": usb_location(&b.dev.devpath).unwrap_or_default(),
            "sim": b.dev.sim,
            "block": b.block,
            "label": b.label,
            "mount": b.mount,
            "readOnly": b.ro,
            "info": b.info,
            "tool": { "name": b.ty.tool, "pkg": b.ty.pkg, "path": if b.ty.tool == "dfu-util" { &st.tools.dfu_util } else { &st.tools.picotool } },
            "job": job,
            "lastJob": last,
        })
    }

    fn list_all(&self) -> Map<String, Value> {
        let st = lock(&self.st);
        let v = json!({
            "openocd": { "path": st.openocd, "version": st.openocd_version, "scripts": self.cfg.scripts.display().to_string() },
            "tools": { "dfuUtil": st.tools.dfu_util, "picotool": st.tools.picotool, "gdb": st.tools.gdb },
            "portOffset": self.cfg.port_offset,
            "probes": st.probes.values().map(|p| self.probe_status(&st, p)).collect::<Vec<_>>(),
            "bootloaders": st.boots.values().map(|b| Self::boot_status(&st, b)).collect::<Vec<_>>(),
        });
        v.as_object().cloned().unwrap_or_default()
    }

    fn summary(&self) -> String {
        let st = lock(&self.st);
        let mut parts: Vec<String> = Vec::new();
        let n = st.probes.len();
        if n > 0 {
            parts.push(format!("{n} probe{}", if n > 1 { "s" } else { "" }));
        }
        for p in st.probes.values() {
            if let Some(s) = p.session.as_ref().filter(|s| s.persistent && s.state() == "running") {
                let cpu = p.ocd.cpus.first().and_then(|c| c.split(": ").nth(1)).map(|c| format!(" {c}")).unwrap_or_default();
                parts.push(format!("GDB :{}{cpu}", s.ports.gdb.map(|g| g.to_string()).unwrap_or_else(|| "null".into())));
            }
        }
        for b in st.boots.values() {
            parts.push(b.ty.name.into());
        }
        // The Tools strip shows this under the button when idle, so say what it's waiting for.
        if parts.is_empty() { "No probe connected".into() } else { parts.join(" · ") }
    }

    fn status(&self) -> Value {
        let active = {
            let st = lock(&self.st);
            let running = st.probes.values().any(|p| p.session.as_ref().is_some_and(|s| s.state() != "exited"));
            !st.probes.is_empty() || !st.boots.is_empty() || running
        };
        let mut m = Map::new();
        m.insert("active".into(), active.into());
        m.insert("summary".into(), self.summary().into());
        for (k, v) in self.list_all() {
            m.insert(k, v);
        }
        Value::Object(m)
    }

    /// MCP tools take firmware as base64 or a URL BenchBob fetches (with curl).
    async fn file_arg(&self, a: &Value) -> Result<(Vec<u8>, String), HttpError> {
        let name = a.get("name").filter(|v| truthy(v)).map(js_string);
        if let Some(d) = a.get("data_base64").filter(|v| truthy(v)) {
            return Ok((base64_decode(&js_string(d)), name.unwrap_or_default()));
        }
        if let Some(u) = a.get("url").filter(|v| truthy(v)) {
            let url = js_string(u);
            std::fs::create_dir_all(&self.cfg.tmp).map_err(|e| HttpError::new(500, e.to_string()))?;
            let tmp = self.cfg.tmp.join(format!("fetch-{}", base36(now_ms())));
            let t = tmp.display().to_string();
            let r = run(&["curl", "-sSL", "-o", &t, "-w", "%{http_code}", "--", &url], Opts::default()).await;
            let data = std::fs::read(&tmp).unwrap_or_default();
            let _ = std::fs::remove_file(&tmp);
            if r.code != 0 {
                return Err(HttpError::new(500, if r.err.is_empty() { format!("Fetching {url}: curl exit {}", r.code) } else { r.err }));
            }
            if !r.out.starts_with('2') {
                return Err(HttpError::new(500, format!("Fetching {url}: HTTP {}", r.out)));
            }
            let from_url = url.split(['?', '#']).next().unwrap_or("").rsplit('/').next().unwrap_or("").to_string();
            return Ok((data, name.unwrap_or(from_url)));
        }
        Err(HttpError::new(500, "Give the firmware as data_base64 or url"))
    }

    fn targets_view(&self) -> Value {
        let st = lock(&self.st);
        let mut presets = vec![json!({ "cfg": GENERIC_TARGET, "label": "Any Cortex-M (detect)" })];
        presets
            .extend(TARGET_PRESETS.iter().filter(|(c, _)| st.targets.iter().any(|t| t == c)).map(|(c, l)| json!({ "cfg": c, "label": l })));
        presets.push(json!({ "cfg": NO_TARGET, "label": "No target (telnet setup)" }));
        json!({ "presets": presets, "all": st.targets })
    }

    fn clients(&self, id: &str, host: Option<&str>, web: Option<&str>) -> Result<Value, HttpError> {
        let host = host.filter(|h| !h.is_empty()).map(String::from).unwrap_or_else(host_addr);
        let p = self.cfg.web_port;
        let web = web
            .filter(|w| !w.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("http://{host}{}", if p == 80 { String::new() } else { format!(":{p}") }));
        let st = lock(&self.st);
        let pr = st.probes.get(id).ok_or_else(|| HttpError::new(404, format!("No probe \"{id}\"")))?;
        if pr.ty.gdb_built_in {
            let tty =
                pr.ttys.iter().find(|t| t.1 == "00").or(pr.ttys.first()).map(|t| t.0.clone()).unwrap_or_else(|| "/dev/ttyACM0".into());
            return Ok(json!({
                "local": format!("target extended-remote {tty}"),
                "remote": format!("Start a raw TCP bridge on {tty} in the Serial panel, then: target extended-remote {host}:<bridge port>"),
                "session": "monitor swdp_scan\nattach 1\nload\nrun",
            }));
        }
        if pr.ty.iface.is_none() {
            return Err(HttpError::bad_request(format!("{}: {}", pr.ty.name, pr.ty.note.unwrap_or("undefined"))));
        }
        let c = self.config(&st, id, &pr.ty);
        Ok(client_snippets(&host, &self.effective_ports(&c), &c.target, id, &web))
    }
}

/// `(name.split(/[\\/]/).pop() || "firmware").replace(/[^A-Za-z0-9_.-]/g, "_").replace(/(\.uf2)?$/i, ".uf2")`
fn uf2_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().filter(|s| !s.is_empty()).unwrap_or("firmware");
    let clean: String = base.chars().map(|c| if c.is_ascii_alphanumeric() || "_.-".contains(c) { c } else { '_' }).collect();
    if clean.to_lowercase().ends_with(".uf2") { format!("{}.uf2", &clean[..clean.len() - 4]) } else { format!("{clean}.uf2") }
}

/// /proc/mounts escapes spaces etc. as \040.
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push(u8::from_str_radix(&s[i + 1..i + 4], 8).unwrap_or(b'?'));
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn must_ok(cmd: &[&str]) -> Result<(), HttpError> {
    let r = run(cmd, Opts::timeout_ms(30_000)).await;
    if r.code != 0 {
        let why = if !r.err.is_empty() {
            r.err
        } else if !r.out.is_empty() {
            r.out
        } else {
            format!("exit {}", r.code)
        };
        return Err(HttpError::new(500, format!("{}: {why}", cmd.join(" "))));
    }
    Ok(())
}

async fn stop_session(s: &Session) {
    {
        let mut st = lock(&s.core.state);
        if *st == "exited" {
            return;
        }
        *st = "stopping";
    }
    s.core.stopped.store(true, Ordering::SeqCst);
    s.tcl.close_now();
    // OpenOCD shuts down cleanly on SIGTERM.
    s.proc.kill(SIGTERM);
    if s.proc.wait_for(Duration::from_secs(3)).await.is_none() {
        s.proc.kill(SIGKILL);
    }
    s.proc.wait().await;
    // As `await proc.exited` did: the exit handler has run by the time this returns.
    let mut d = s.core.done.subscribe();
    let _ = tokio::time::timeout(Duration::from_secs(2), d.wait_for(|v| *v)).await;
}

fn free_port() -> std::io::Result<u32> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port() as u32)
}

// ---------- driver ----------

fn route<F, Fut>(i: &Arc<Inner>, method: Method, path: &str, doc: Json, f: F) -> Route
where
    F: Fn(Arc<Inner>, benchbob_core::Request) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Value, HttpError>> + Send + 'static,
{
    let i = i.clone();
    Route::new(method, path, Some(doc), move |r| {
        let fut = f(i.clone(), r);
        async move { reply(fut.await?) }
    })
}

fn tool<F, Fut>(i: &Arc<Inner>, name: &str, desc: &str, schema: Json, f: F) -> Tool
where
    F: Fn(Arc<Inner>, Value) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Value, HttpError>> + Send + 'static,
{
    let i = i.clone();
    Tool::new(name, desc, schema, move |a| f(i.clone(), a))
}

/// `rawBody`: the upload, refused over 64 MB.
fn raw_body(r: &benchbob_core::Request) -> Result<Vec<u8>, HttpError> {
    let len = r.headers.get("content-length").and_then(|v| v.to_str().ok()).and_then(benchbob_core::http::parse_int).unwrap_or(0);
    if len > UPLOAD_MAX as i64 {
        return Err(HttpError::new(413, format!("File too large (max {} MB)", UPLOAD_MAX >> 20)));
    }
    Ok(r.body.to_vec())
}

async fn ws_session(i: Arc<Inner>, mut s: WsSession) {
    let id = s.params.get("id").cloned().unwrap_or_default();
    {
        let mut st = lock(&i.st);
        let Some(d) = st.dev_mut(&id) else {
            s.client.close_with(4004, "No such device");
            return;
        };
        d.sockets.push(s.client.clone());
        let (job, last) = d.job_json();
        s.client.send(json!({ "type": "log", "lines": d.log, "job": job, "lastJob": last }).to_string());
    }
    while s.incoming.recv().await.is_some() {}
    let cid = s.client.id();
    if let Some(d) = lock(&i.st).dev_mut(&id) {
        d.sockets.retain(|w| w.id() != cid);
    }
}

#[async_trait]
impl Driver for Probes {
    fn id(&self) -> &'static str {
        "probes"
    }
    fn title(&self) -> &'static str {
        "Debug probes"
    }
    fn order(&self) -> i32 {
        60
    }
    fn description(&self) -> &'static str {
        "ST-Link, J-Link, CMSIS-DAP, ESP and Black Magic probes as LAN GDB servers (OpenOCD); flashing; RP2 UF2 and STM32 DFU bootloaders"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let i = &self.inner;
        let _ = i.ctx.set(ctx);
        let path = i.cfg.path.clone();
        let which = |n: &str| which_in(n, &path).map(|p| p.display().to_string()).unwrap_or_default();
        let openocd = which("openocd");
        let mut version = String::new();
        if !openocd.is_empty() {
            let r = run(&[&openocd, "--version"], Opts::timeout_ms(5000)).await;
            let first = if r.err.is_empty() { r.out } else { r.err };
            let first = first.split('\n').next().unwrap_or("").to_string();
            version = first.strip_prefix("Open On-Chip Debugger").map(|s| s.trim_start().to_string()).unwrap_or(first);
        }
        let gdb = match which("gdb-multiarch") {
            g if !g.is_empty() => g,
            _ => which("gdb"),
        };
        let mut targets = Vec::new();
        collect_cfgs(&i.cfg.scripts, &i.cfg.scripts.join("target"), &mut targets);
        targets.sort();
        {
            let mut st = lock(&i.st);
            st.openocd = openocd;
            st.openocd_version = version;
            st.tools = Tools { dfu_util: which("dfu-util"), picotool: which("picotool"), gdb };
            st.targets = targets;
        }
        // Leftovers from a crash: uploads are only needed for the duration of a flash.
        let _ = std::fs::remove_dir_all(&i.cfg.tmp);
        i.scan().await;
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) {
        let i = &self.inner;
        i.epoch.fetch_add(1, Ordering::SeqCst);
        i.scan_gen.fetch_add(1, Ordering::SeqCst);
        let (sessions, mounts): (Vec<Arc<Session>>, Vec<String>) = {
            let st = lock(&i.st);
            (
                st.probes.values().filter_map(|p| p.session.clone()).collect(),
                st.boots.values().filter(|b| !b.private_mount.is_empty()).map(|b| b.private_mount.clone()).collect(),
            )
        };
        futures_util::future::join_all(sessions.iter().map(|s| stop_session(s))).await;
        for m in mounts {
            run(&["sudo", "umount", "-l", &m], Opts::default()).await;
        }
        let mut st = lock(&i.st);
        st.probes.clear();
        st.boots.clear();
    }

    fn udev(&self, ev: &UdevEvent) {
        let s = prop(&ev.props, "SUBSYSTEM");
        if s == "usb" || s == "tty" || s == "block" {
            self.inner.schedule_scan();
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
                "/api/probes",
                json!("Debug probes and bootloader-mode MCUs, with each probe's OpenOCD config, GDB server state and parsed target info"),
                |i, _r| async move { Ok(Value::Object(i.list_all())) },
            ),
            route(
                i,
                Method::GET,
                "/api/probes/targets",
                json!("OpenOCD target scripts: presets (common chips present in this OpenOCD) and every target/*.cfg"),
                |i, _r| async move { Ok(i.targets_view()) },
            ),
            route(
                i,
                Method::PUT,
                "/api/probes/:id/config",
                json!({ "body": { "target": "target/stm32f4x.cfg", "transport": "swd", "speed": 4000, "ports": { "gdb": 3333, "telnet": 4444, "tcl": 6666 } }, "description": "Set a probe's OpenOCD target, transport, adapter speed (kHz) and ports (persisted per probe)" }),
                |i, r| async move {
                    let id = i.probe_id(Some(&Value::String(r.param("id").into())))?;
                    i.set_config(&id, &r.json()?)?;
                    Ok(i.probe_status_of(&id))
                },
            ),
            route(
                i,
                Method::POST,
                "/api/probes/:id/start",
                json!({ "body": { "target": "(optional) target/rp2040.cfg", "transport": "(optional)", "speed": "(optional)" }, "description": "Start OpenOCD as a LAN GDB server for this probe; waits until it is ready or fails (with OpenOCD's error and a hint)" }),
                |i, r| async move {
                    let id = i.probe_id(Some(&Value::String(r.param("id").into())))?;
                    i.need_ocd(&id)?;
                    let b = r.json()?;
                    i.start_server(&id, b.as_object().cloned().unwrap_or_default()).await
                },
            ),
            route(i, Method::POST, "/api/probes/:id/stop", json!("Stop this probe's GDB server"), |i, r| async move {
                let id = i.probe_id(Some(&Value::String(r.param("id").into())))?;
                Ok(i.stop_server(&id).await)
            }),
            route(
                i,
                Method::GET,
                "/api/probes/:id/log",
                json!("OpenOCD / tool log tail for a probe or bootloader device"),
                |i, r| async move {
                    let id = r.param("id").to_string();
                    let found = lock(&i.st).probes.get(&id).map(|p| p.dev.log.clone());
                    let lines = match found {
                        Some(l) => l,
                        None => {
                            let bid = i.boot_id(Some(&Value::String(id)))?;
                            lock(&i.st).boots.get(&bid).map(|b| b.dev.log.clone()).unwrap_or_default()
                        }
                    };
                    Ok(json!({ "lines": lines }))
                },
            ),
            route(
                i,
                Method::GET,
                "/api/probes/:id/clients",
                json!({ "query": { "host": "(optional) hostname clients should use", "web": "(optional) BenchBob base URL" }, "description": "Copy-paste client configs for this probe's GDB server: gdb, telnet, VS Code cortex-debug, PlatformIO" }),
                |i, r| async move {
                    let id = i.probe_id(Some(&Value::String(r.param("id").into())))?;
                    i.clients(&id, r.query("host"), r.query("web"))
                },
            ),
            route(
                i,
                Method::POST,
                "/api/probes/:id/examine",
                json!("Detect the target: connect with the configured target script and report core(s), DPIDR, voltage, flash size"),
                |i, r| async move {
                    let id = i.probe_id(Some(&Value::String(r.param("id").into())))?;
                    i.examine(&id).await
                },
            ),
            route(
                i,
                Method::POST,
                "/api/probes/:id/flash",
                json!({
                    "query": { "name": "firmware.elf (extension picks the format: .elf/.hex/.bin)", "address": "base address, required for .bin (0x08000000)", "verify": "1 (default) / 0", "reset": "1 (default) / 0" },
                    "body": "raw file bytes",
                    "description": "Program firmware through OpenOCD (uses the running GDB server's TCL port if there is one); progress streams on WS /ws/probes/:id",
                }),
                |i, r| async move {
                    let id = i.probe_id(Some(&Value::String(r.param("id").into())))?;
                    let buf = raw_body(&r)?;
                    let address = r.query("address").map(|a| Value::String(a.into()));
                    i.flash(
                        &id,
                        buf,
                        r.query("name").unwrap_or(""),
                        address.as_ref(),
                        flag(r.query("verify"), true),
                        flag(r.query("reset"), true),
                    )
                    .await
                },
            ),
            route(i, Method::POST, "/api/probes/:id/erase", json!("Mass-erase the target's flash (halts it first)"), |i, r| async move {
                let id = i.probe_id(Some(&Value::String(r.param("id").into())))?;
                i.erase(&id).await
            }),
            route(
                i,
                Method::POST,
                "/api/probes/:id/reset",
                json!({ "body": { "mode": "run | halt | init" }, "description": "Reset the target" }),
                |i, r| async move {
                    let id = i.probe_id(Some(&Value::String(r.param("id").into())))?;
                    let b = r.json()?;
                    let mode = b.get("mode").filter(|v| truthy(v)).map(js_string).unwrap_or_else(|| "run".into());
                    let cmd = match mode.as_str() {
                        "run" => "reset run",
                        "halt" => "reset halt",
                        "init" => "reset init",
                        _ => return Err(HttpError::bad_request("mode must be run, halt or init")),
                    };
                    i.control(&id, cmd).await
                },
            ),
            route(i, Method::POST, "/api/probes/:id/halt", json!("Halt the target CPU"), |i, r| async move {
                let id = i.probe_id(Some(&Value::String(r.param("id").into())))?;
                i.control(&id, "halt").await
            }),
            route(i, Method::POST, "/api/probes/:id/resume", json!("Resume the target CPU"), |i, r| async move {
                let id = i.probe_id(Some(&Value::String(r.param("id").into())))?;
                i.control(&id, "resume").await
            }),
            route(
                i,
                Method::GET,
                "/api/probes/:id/memory",
                json!({ "query": { "address": "0x20000000", "length": "256 (max 65536)" }, "description": "Read target memory: hex string plus a hexdump" }),
                |i, r| async move {
                    let id = i.probe_id(Some(&Value::String(r.param("id").into())))?;
                    let a = r.query("address").map(|v| Value::String(v.into()));
                    let l = r.query("length").map(|v| Value::String(v.into()));
                    i.read_memory(&id, a.as_ref(), l.as_ref()).await
                },
            ),
            route(
                i,
                Method::POST,
                "/api/probes/:id/command",
                json!({ "body": { "command": "flash banks" }, "description": "Run any OpenOCD command over TCL and return its output" }),
                |i, r| async move {
                    let id = i.probe_id(Some(&Value::String(r.param("id").into())))?;
                    let b = r.json()?;
                    i.command(&id, b.get("command")).await
                },
            ),
            route(
                i,
                Method::POST,
                "/api/probes/boot/:id/uf2",
                json!({ "query": { "name": "firmware.uf2", "force": "1 to copy even if the UF2 family doesn't match the chip" }, "body": "raw .uf2 bytes", "description": "Flash an RP2040/RP2350 in BOOTSEL mode by copying the UF2 onto its drive; the board then reboots" }),
                |i, r| async move {
                    let id = i.boot_id(Some(&Value::String(r.param("id").into())))?;
                    let buf = raw_body(&r)?;
                    i.flash_uf2(&id, buf, r.query("name").filter(|n| !n.is_empty()), flag(r.query("force"), false)).await
                },
            ),
            route(
                i,
                Method::POST,
                "/api/probes/boot/:id/dfu",
                json!({ "query": { "address": "0x08000000 (default)", "name": "firmware.bin" }, "body": "raw .bin bytes", "description": "Flash an STM32 in DFU mode with dfu-util (needs the dfu-util package)" }),
                |i, r| async move {
                    let id = i.boot_id(Some(&Value::String(r.param("id").into())))?;
                    let buf = raw_body(&r)?;
                    let a = r.query("address").map(|v| Value::String(v.into()));
                    i.flash_dfu(&id, buf, a.as_ref(), r.query("name").filter(|n| !n.is_empty())).await
                },
            ),
            route(
                i,
                Method::GET,
                "/api/probes/boot/:id/info",
                json!("Bootloader device details; for RP2 also `picotool info -a` when picotool is installed"),
                |i, r| async move {
                    let id = i.boot_id(Some(&Value::String(r.param("id").into())))?;
                    let (mut out, pico) = {
                        let st = lock(&i.st);
                        let b = st.boots.get(&id).ok_or_else(|| HttpError::new(404, format!("No bootloader device \"{id}\"")))?;
                        let pico =
                            (b.ty.kind == "rp2-bootsel" && !st.tools.picotool.is_empty() && !b.dev.sim).then(|| st.tools.picotool.clone());
                        (Inner::boot_status(&st, b), pico)
                    };
                    if let Some(p) = pico {
                        let r = run(&["sudo", &p, "info", "-a"], Opts::timeout_ms(15000)).await;
                        out["picotool"] = (if r.out.is_empty() { r.err } else { r.out }).into();
                    }
                    Ok(out)
                },
            ),
        ]
    }

    fn sockets(&self) -> Vec<Socket> {
        let i = self.inner.clone();
        vec![Socket::new(
            "/ws/probes/:id",
            Some(
                "Live log for a probe or bootloader device: {type:\"log\",lines} on connect, then {type:\"line\",text} and {type:\"job\",job,lastJob}",
            ),
            move |s| ws_session(i.clone(), s),
        )]
    }

    fn tools(&self) -> Vec<Tool> {
        let i = &self.inner;
        let probe = json!({ "type": "string", "description": "Probe id or serial (optional when only one)" });
        vec![
            tool(
                i,
                "probes_list",
                "List debug probes (ST-Link, J-Link, CMSIS-DAP, ESP USB-JTAG, Black Magic Probe…) and MCUs in bootloader mode (RP2 BOOTSEL, STM32 DFU) attached to BenchBob, with each probe's id, OpenOCD target config, GDB server state/ports and detected target core/voltage.",
                json!({ "type": "object", "properties": {} }),
                |i, _a| async move { Ok(Value::Object(i.list_all())) },
            ),
            tool(
                i,
                "probes_server",
                "Start or stop the OpenOCD GDB server for a probe. On start it waits until OpenOCD is ready and returns the ports (connect with `target extended-remote <benchbob>:<gdb port>`), or fails with OpenOCD's error and a wiring/power hint. target is an OpenOCD script such as target/stm32f4x.cfg, target/rp2040.cfg, target/nordic/nrf52.cfg, or generic-cortex-m.",
                json!({
                    "type": "object",
                    "properties": {
                        "probe": { "type": "string", "description": "Probe id or serial from probes_list (optional when only one probe is attached)" },
                        "action": { "type": "string", "enum": ["start", "stop"] },
                        "target": { "type": "string", "description": "OpenOCD target script, e.g. target/stm32f4x.cfg (persisted)" },
                        "transport": { "type": "string", "enum": ["swd", "jtag"] },
                        "speed": { "type": "number", "description": "Adapter speed in kHz" },
                    },
                    "required": ["action"],
                }),
                |i, a| async move {
                    let id = i.probe_id(a.get("probe"))?;
                    i.need_ocd(&id)?;
                    if a.get("action").and_then(Value::as_str) == Some("stop") {
                        return Ok(i.stop_server(&id).await);
                    }
                    let mut over = Map::new();
                    for k in ["target", "transport", "speed"] {
                        if let Some(v) = a.get(k) {
                            over.insert(k.into(), v.clone());
                        }
                    }
                    i.start_server(&id, over).await
                },
            ),
            tool(
                i,
                "probes_examine",
                "Connect to the target through a probe and report what's there: core(s), DPIDR, target voltage, flash size, and OpenOCD errors with hints. Uses the probe's configured target script (generic-cortex-m by default).",
                json!({ "type": "object", "properties": { "probe": probe } }),
                |i, a| async move {
                    let id = i.probe_id(a.get("probe"))?;
                    i.examine(&id).await
                },
            ),
            tool(
                i,
                "probes_flash",
                "Program firmware (.elf, .hex or .bin) into the target through a debug probe with OpenOCD, then verify and reset. Give the file as base64 or a URL BenchBob can fetch. A .bin needs a base address (STM32 0x08000000, RP2040 0x10000000). Set the probe's target first (probes_server start with target, or it must already be configured).",
                json!({
                    "type": "object",
                    "properties": {
                        "probe": probe,
                        "data_base64": { "type": "string", "description": "Firmware file, base64" },
                        "url": { "type": "string", "description": "Or: URL to fetch the firmware from" },
                        "name": { "type": "string", "description": "File name; its extension picks the format if the content doesn't (firmware.hex)" },
                        "address": { "type": "string", "description": "Base address for .bin, e.g. 0x08000000" },
                        "verify": { "type": "boolean", "default": true },
                        "reset": { "type": "boolean", "default": true },
                    },
                }),
                |i, a| async move {
                    let (buf, name) = i.file_arg(&a).await?;
                    let id = i.probe_id(a.get("probe"))?;
                    i.flash(
                        &id,
                        buf,
                        &name,
                        a.get("address"),
                        a.get("verify") != Some(&Value::Bool(false)),
                        a.get("reset") != Some(&Value::Bool(false)),
                    )
                    .await
                },
            ),
            tool(
                i,
                "probes_reset",
                "Reset, halt or resume the target CPU behind a debug probe.",
                json!({
                    "type": "object",
                    "properties": {
                        "probe": probe,
                        "mode": { "type": "string", "enum": ["run", "halt", "init", "halt-only", "resume"], "description": "run = reset and run (default); halt = reset and halt; init = reset-init; halt-only = halt without reset; resume = continue" },
                    },
                }),
                |i, a| async move {
                    let id = i.probe_id(a.get("probe"))?;
                    let m = a.get("mode").filter(|v| truthy(v)).map(js_string).unwrap_or_else(|| "run".into());
                    let cmd = match m.as_str() {
                        "halt-only" => "halt",
                        "resume" => "resume",
                        "run" => "reset run",
                        "halt" => "reset halt",
                        "init" => "reset init",
                        _ => return Err(HttpError::new(500, "mode must be run, halt, init, halt-only or resume")),
                    };
                    i.control(&id, cmd).await
                },
            ),
            tool(
                i,
                "probes_read_memory",
                "Read target memory through a debug probe. Returns hex and a hexdump. Works while the target runs on most Cortex-M parts.",
                json!({
                    "type": "object",
                    "properties": {
                        "probe": probe,
                        "address": { "type": "string", "description": "Start address, hex (0x20000000) or decimal" },
                        "length": { "type": "number", "description": "Bytes to read (1–65536, default 256)" },
                    },
                    "required": ["address"],
                }),
                |i, a| async move {
                    let id = i.probe_id(a.get("probe"))?;
                    i.read_memory(&id, a.get("address"), a.get("length")).await
                },
            ),
            tool(
                i,
                "probes_command",
                "Run any OpenOCD command (e.g. `flash banks`, `mdw 0x08000000 4`, `reg pc`) through a probe and return its output.",
                json!({ "type": "object", "properties": { "probe": { "type": "string" }, "command": { "type": "string" } }, "required": ["command"] }),
                |i, a| async move {
                    let id = i.probe_id(a.get("probe"))?;
                    i.command(&id, a.get("command")).await
                },
            ),
            tool(
                i,
                "probes_uf2_flash",
                "Flash an RP2040/RP2350 that is in BOOTSEL mode (hold BOOTSEL while plugging in) by copying a .uf2 onto its USB drive. The board reboots into the new firmware. Give the UF2 as base64 or a URL.",
                json!({
                    "type": "object",
                    "properties": {
                        "device": { "type": "string", "description": "Bootloader device id from probes_list (optional when only one)" },
                        "data_base64": { "type": "string" },
                        "url": { "type": "string" },
                        "name": { "type": "string", "description": "File name, e.g. blink.uf2" },
                        "force": { "type": "boolean", "description": "Copy even if the UF2 family doesn't match the chip" },
                    },
                }),
                |i, a| async move {
                    let (buf, name) = i.file_arg(&a).await?;
                    let id = i.boot_id(a.get("device"))?;
                    i.flash_uf2(&id, buf, Some(name.as_str()).filter(|n| !n.is_empty()), a.get("force").is_some_and(truthy)).await
                },
            ),
        ]
    }
}

/// Every `*.cfg` under `dir`, relative to `root` ("target/stm32f4x.cfg").
fn collect_cfgs(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_cfgs(root, &p, out);
        } else if p.extension().is_some_and(|x| x == "cfg")
            && let Ok(rel) = p.strip_prefix(root)
        {
            out.push(rel.display().to_string());
        }
    }
}

#[cfg(test)]
mod tests;
