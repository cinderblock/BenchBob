//! The GPS panel, ported from `peripherals/drivers/gps.ts`.
//!
//! USB GPS receivers, run through gpsd for as long as someone is using them. A bench
//! experiment, not a time service: gpsd and gpsd.socket stay masked in systemd, and the
//! driver starts `gpsd -N` as its own child only after the user (or an agent) asks, and stops
//! it on request, on unplug, and on shutdown.
//!
//! The driver is also a gpsd client: it connects to its child over the JSON protocol
//! (?WATCH) and folds TPV/SKY/PPS/TOFF into the state the panel and tools show. Other
//! programs (cgps, gpsmon, QGIS, a laptop with "Share on network") can attach to the same
//! gpsd at the same time.
//!
//! Serial ports are shared with the serial panel, which lists the same ttys. Whoever opens a
//! port first keeps it: before starting, the driver looks for another process holding the
//! tty (/proc/*/fd) and refuses rather than fight over it; an EBUSY from gpsd is reported the
//! same way.

pub mod parse;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::Method;
use benchbob_core::http::{HttpError, js_number, js_string, json as reply, truthy};
use benchbob_core::paths::port_offset;
use benchbob_core::proc::{Proc, SIGKILL, SIGTERM, Split};
use benchbob_core::timeline::now_ms;
use benchbob_core::udev::{Props, UdevEvent};
use benchbob_core::util::{hostname, natural_cmp, parse_json, prop, usb_name};
use benchbob_core::{Ctx, Driver, Json, Route, StopReason, Tool};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;

use crate::parse::{GpsdState, classify_usb, ingest, summarize};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Clone)]
pub struct Config {
    pub gpsd: PathBuf,
    pub port_offset: u16,
    /// Where `/proc` is (holders of a tty).
    pub proc_dir: PathBuf,
    pub dev_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        Self { gpsd: "/usr/sbin/gpsd".into(), port_offset: port_offset(), proc_dir: "/proc".into(), dev_dir: "/dev".into() }
    }
    fn default_port(&self) -> u32 {
        2947 + self.port_offset as u32
    }
}

#[derive(Clone, PartialEq)]
struct Holder {
    pid: u32,
    comm: String,
}

#[derive(Clone)]
struct Candidate {
    /// Stable id for remembering "use as GPS".
    key: String,
    path: String,
    /// /dev/serial/by-id/… when udev made one.
    by_id: String,
    usb_id: String,
    vendor: String,
    model: String,
    serial: String,
    kind: &'static str,
    label: &'static str,
    chosen: bool,
    holders: Vec<Holder>,
}

impl Candidate {
    fn is_gps(&self) -> bool {
        self.kind == "gps" || self.chosen
    }
    fn to_json(&self) -> Value {
        json!({
            "key": self.key, "path": self.path, "byId": self.by_id, "usbId": self.usb_id, "vendor": self.vendor,
            "model": self.model, "serial": self.serial, "kind": self.kind, "label": self.label, "chosen": self.chosen,
            "holders": self.holders.iter().map(|h| json!({ "pid": h.pid, "comm": h.comm })).collect::<Vec<_>>(),
        })
    }
}

struct Run {
    path: String,
    key: Option<String>,
    port: u32,
    share: bool,
    read_only: bool,
    pps: Vec<String>,
    proc: Arc<Proc>,
    since: u64,
    connected: AtomicBool,
    state: Mutex<GpsdState>,
    stopping: AtomicBool,
    /// The gpsd client connection's task, aborted on stop.
    client: Mutex<Option<tokio::task::AbortHandle>>,
    /// Set once the exit handler has run.
    done: watch::Sender<bool>,
}

#[derive(Default)]
struct St {
    candidates: Vec<Candidate>,
    run: Option<Arc<Run>>,
    last_exit: Option<Value>,
    log: Vec<(u64, String)>,
}

struct Inner {
    cfg: Config,
    ctx: OnceLock<Ctx>,
    st: Mutex<St>,
    /// `serial()`: start/stop one at a time.
    busy: tokio::sync::Mutex<()>,
    epoch: AtomicU64,
}

pub struct Gps {
    inner: Arc<Inner>,
}

impl Default for Gps {
    fn default() -> Self {
        Self::new()
    }
}

impl Gps {
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
                epoch: AtomicU64::new(0),
            }),
        }
    }
}

fn is_tty(dev: &str) -> bool {
    let Some(rest) = dev.strip_prefix("/dev/tty") else { return false };
    let num = rest.strip_prefix("ACM").or_else(|| rest.strip_prefix("USB"));
    num.is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
}

/// The `path` argument's allowed shapes: /dev/tty*, /dev/pts/N, /dev/gnssN, /dev/serial/by-*.
fn path_ok(p: &str) -> bool {
    let word = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_');
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    if let Some(r) = p.strip_prefix("/dev/tty") {
        return word(r);
    }
    if let Some(r) = p.strip_prefix("/dev/pts/") {
        return digits(r);
    }
    if let Some(r) = p.strip_prefix("/dev/gnss") {
        return digits(r);
    }
    if let Some(r) = p.strip_prefix("/dev/serial/by-id/").or_else(|| p.strip_prefix("/dev/serial/by-path/")) {
        return !r.is_empty() && r.bytes().all(|c| c.is_ascii_alphanumeric() || b"_:.,+-".contains(&c));
    }
    false
}

/// Bun.spawn's error for a program that can't be started.
fn spawn_error(e: &std::io::Error, prog: &std::path::Path) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("ENOENT: no such file or directory, posix_spawn '{}'", prog.display())
    } else {
        format!("{}: {e}", prog.display())
    }
}

impl Inner {
    fn ctx(&self) -> &Ctx {
        self.ctx.get().expect("gps driver not initialised")
    }

    fn setting(&self, k: &str, default: Value) -> Value {
        self.ctx().setting(k).unwrap_or(default)
    }

    fn add_log(&self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        let mut st = lock(&self.st);
        st.log.push((now_ms(), line.trim().to_string()));
        if st.log.len() > 100 {
            st.log.remove(0);
        }
    }

    // ---------- candidate ttys ----------

    fn scan_ttys(&self) {
        let ctx = self.ctx();
        let chosen = self.setting("useAsGps", json!({}));
        let ttys = ctx.devices(Some(&|p: &Props| prop(p, "SUBSYSTEM") == "tty" && is_tty(prop(p, "DEVNAME"))));
        let mut st = lock(&self.st);
        let prev: Vec<(String, Vec<Holder>)> = st.candidates.iter().map(|c| (c.path.clone(), c.holders.clone())).collect();
        let mut list: Vec<Candidate> = ttys
            .iter()
            .map(|p| {
                let usb = ctx.ancestor(prop(p, "DEVPATH"), "usb", Some("usb_device"));
                let usb_id = usb.as_ref().map(|u| format!("{}:{}", prop(u, "ID_VENDOR_ID"), prop(u, "ID_MODEL_ID"))).unwrap_or_default();
                let (kind, label) = classify_usb(&usb_id);
                let key = if !prop(p, "ID_SERIAL").is_empty() && !prop(p, "ID_SERIAL_SHORT").is_empty() {
                    let ifn = prop(p, "ID_USB_INTERFACE_NUM");
                    format!("sn:{}{}", prop(p, "ID_SERIAL"), if ifn.is_empty() { String::new() } else { format!(":{ifn}") })
                } else {
                    format!("path:{}", if prop(p, "ID_PATH").is_empty() { prop(p, "DEVNAME") } else { prop(p, "ID_PATH") })
                };
                let (vendor, model) = usb.as_ref().map(usb_name).unwrap_or_default();
                let by_id = prop(p, "DEVLINKS").split(' ').find(|l| l.starts_with("/dev/serial/by-id/")).unwrap_or("").to_string();
                let path = prop(p, "DEVNAME").to_string();
                let holders = prev.iter().find(|(pp, _)| *pp == path).map(|(_, h)| h.clone()).unwrap_or_default();
                Candidate {
                    chosen: chosen.get(&key).is_some_and(truthy),
                    key,
                    path,
                    by_id,
                    usb_id,
                    vendor,
                    model,
                    serial: prop(p, "ID_SERIAL_SHORT").to_string(),
                    kind,
                    label,
                    holders,
                }
            })
            .collect();
        list.sort_by(|a, b| natural_cmp(&a.path, &b.path));
        st.candidates = list;
    }

    /// Processes with `dev` open, from /proc/<pid>/fd; root-owned ones we can't read are missed.
    fn holders_of(&self, dev: &str) -> Vec<Holder> {
        let real = std::fs::canonicalize(dev).map(|p| p.display().to_string()).unwrap_or_else(|_| dev.to_string());
        let me = std::process::id();
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.cfg.proc_dir) else { return out };
        let mut pids: Vec<u32> = rd.flatten().filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok())).collect();
        pids.sort_unstable();
        for pid in pids {
            let fd_dir = self.cfg.proc_dir.join(format!("{pid}/fd"));
            let Ok(fds) = std::fs::read_dir(&fd_dir) else { continue };
            for fd in fds.flatten() {
                let Ok(t) = std::fs::read_link(fd.path()) else { continue };
                if t.to_str() == Some(real.as_str()) {
                    let comm = benchbob_core::run::read_text(self.cfg.proc_dir.join(format!("{pid}/comm")));
                    out.push(Holder { pid, comm: if pid == me { "BenchBob (serial panel)".into() } else { comm } });
                    break;
                }
            }
        }
        out
    }

    fn refresh_holders(&self) {
        let (paths, gpsd_pid) = {
            let st = lock(&self.st);
            (st.candidates.iter().map(|c| c.path.clone()).collect::<Vec<_>>(), st.run.as_ref().and_then(|r| r.proc.pid()))
        };
        let fresh: Vec<Vec<Holder>> =
            paths.iter().map(|p| self.holders_of(p).into_iter().filter(|h| Some(h.pid) != gpsd_pid).collect()).collect();
        let mut changed = false;
        {
            let mut st = lock(&self.st);
            for (path, h) in paths.iter().zip(fresh) {
                if let Some(c) = st.candidates.iter_mut().find(|c| c.path == *path)
                    && c.holders != h
                {
                    c.holders = h;
                    changed = true;
                }
            }
        }
        if changed {
            self.ctx().changed();
        }
    }

    fn pps_devices(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(&self.cfg.dev_dir) else { return Vec::new() };
        let mut v: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .filter(|f| f.strip_prefix("pps").is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())))
            .map(|f| format!("/dev/{f}"))
            .collect();
        v.sort();
        v
    }

    // ---------- gpsd child + client ----------

    fn resolve_device(&self, a: &Value) -> Result<(String, Option<String>), HttpError> {
        let st = lock(&self.st);
        let c = &st.candidates;
        if let Some(d) = a.get("device").filter(|v| truthy(v)) {
            let d = js_string(d);
            let f = c
                .iter()
                .find(|x| x.key == d || x.path == d || x.by_id == d)
                .ok_or_else(|| HttpError::new(404, format!("No serial device \"{d}\"")))?;
            return Ok((f.path.clone(), Some(f.key.clone())));
        }
        if let Some(p) = a.get("path").filter(|v| truthy(v)) {
            // Any tty by path, for receivers on odd adapters, ptys (testing) and by-id links.
            let p = js_string(p);
            if !path_ok(&p) {
                return Err(HttpError::bad_request("path must be a /dev/tty*, /dev/pts/N, /dev/gnssN or /dev/serial/by-* device"));
            }
            if !std::path::Path::new(&p).exists() {
                return Err(HttpError::new(404, format!("{p} doesn't exist")));
            }
            let f = c.iter().find(|x| x.path == p || x.by_id == p);
            return Ok((f.map(|x| x.path.clone()).unwrap_or(p), f.map(|x| x.key.clone())));
        }
        let gps: Vec<&Candidate> = c.iter().filter(|x| x.is_gps()).collect();
        match gps.len() {
            1 => Ok((gps[0].path.clone(), Some(gps[0].key.clone()))),
            0 if c.is_empty() => Err(HttpError::new(409, "No GPS receiver or USB serial device found")),
            0 => Err(HttpError::new(
                409,
                format!(
                    "No known GPS receiver; pick one of {} with \"device\", or mark it \"use as GPS\"",
                    c.iter().map(|x| x.path.as_str()).collect::<Vec<_>>().join(", ")
                ),
            )),
            _ => Err(HttpError::new(
                409,
                format!(
                    "Several GPS receivers ({}); pick one with \"device\"",
                    gps.iter().map(|x| x.path.as_str()).collect::<Vec<_>>().join(", ")
                ),
            )),
        }
    }

    fn current(&self) -> Option<Arc<Run>> {
        lock(&self.st).run.clone()
    }

    async fn start(self: &Arc<Self>, a: Value) -> Result<Value, HttpError> {
        let _g = self.busy.lock().await;
        self.scan_ttys();
        let (path, key) = self.resolve_device(&a)?;
        if let Some(r) = self.current().filter(|r| !r.stopping.load(Ordering::SeqCst)) {
            if r.path == path {
                return Ok(self.view());
            }
            self.stop_run().await;
        }
        let holders = self.holders_of(&path);
        if !holders.is_empty() {
            let who: Vec<String> = holders.iter().map(|h| format!("{} (pid {})", h.comm, h.pid)).collect();
            return Err(HttpError::new(409, format!("{path} is already open by {}. Close it there first.", who.join(", "))));
        }
        let pick = |k: &str, default: Value| match a.get(k) {
            Some(v) if !v.is_null() => v.clone(),
            _ => self.setting(k, default),
        };
        let port = js_number(&pick("port", json!(self.cfg.default_port()))).trunc();
        if !(port > 0.0 && port < 65536.0) {
            return Err(HttpError::bad_request("port must be 1–65535"));
        }
        let port = port as u32;
        let share = truthy(&pick("share", json!(false)));
        let read_only = truthy(&pick("readOnly", json!(false)));
        let pps = if truthy(&pick("pps", json!(true))) { self.pps_devices() } else { Vec::new() };
        let ctx = self.ctx();
        ctx.set_setting("port", json!(port));
        ctx.set_setting("share", json!(share));
        ctx.set_setting("readOnly", json!(read_only));
        if let Some(p) = a.get("pps") {
            ctx.set_setting("pps", json!(truthy(p)));
        }

        // -N foreground, -n poll the receiver without waiting for a client, -S port, -G listen
        // on all addresses (else loopback only), -b never reconfigure the receiver. No -F
        // control socket: nothing else should be adding devices to this instance.
        let mut args: Vec<String> = vec!["-N".into(), "-n".into(), "-D".into(), "1".into(), "-S".into(), port.to_string()];
        if share {
            args.push("-G".into());
        }
        if read_only {
            args.push("-b".into());
        }
        args.push(path.clone());
        args.extend(pps.iter().cloned());
        {
            let mut st = lock(&self.st);
            st.last_exit = None;
            st.log.clear();
        }
        self.add_log(&format!("$ {} {}", self.cfg.gpsd.display(), args.join(" ")));
        let state: Arc<Mutex<Option<Arc<Run>>>> = Default::default();
        let me = self.clone();
        let st2 = state.clone();
        let dev = path.clone();
        let on_line = Arc::new(move |l: &str| {
            me.add_log(l);
            let l2 = l.to_lowercase();
            if (l.contains("EBUSY") || l2.contains("device or resource busy"))
                && let Some(r) = lock(&st2).clone()
            {
                lock(&r.state).errors.push(format!("{dev} is busy: another program has it open"));
            }
        });
        let proc = Proc::spawn(ctx, Command::new(&self.cfg.gpsd).args(&args), Split::Lf, on_line)
            .map_err(|e| HttpError::new(500, spawn_error(&e, &self.cfg.gpsd)))?;
        let (done, _) = watch::channel(false);
        let r = Arc::new(Run {
            path: path.clone(),
            key,
            port,
            share,
            read_only,
            pps,
            proc: proc.clone(),
            since: now_ms(),
            connected: AtomicBool::new(false),
            state: Mutex::new(GpsdState::default()),
            stopping: AtomicBool::new(false),
            client: Mutex::new(None),
            done,
        });
        *lock(&state) = Some(r.clone());
        lock(&self.st).run = Some(r.clone());
        // Exit handler: the one place a run ends (a stop request waits for it).
        let me = self.clone();
        let r2 = r.clone();
        tokio::spawn(async move {
            let code = r2.proc.wait().await;
            let mine = {
                let mut st = lock(&me.st);
                if st.run.as_ref().is_some_and(|x| Arc::ptr_eq(x, &r2)) {
                    let msg = st.log[st.log.len().saturating_sub(3)..].iter().map(|(_, l)| l.clone()).collect::<Vec<_>>().join(" · ");
                    st.last_exit = Some(json!({ "at": now_ms(), "code": code, "message": msg }));
                    st.run = None;
                    true
                } else {
                    false
                }
            };
            if let Some(h) = lock(&r2.client).take() {
                h.abort();
            }
            if mine {
                me.ctx().event(&format!("gpsd exited ({code})"), None);
                me.ctx().changed();
            }
            let _ = r2.done.send(true);
        });
        ctx.event(&format!("gpsd started on {path} (port {port}{})", if share { ", shared on the network" } else { ", local only" }), None);
        self.connect(&r);
        // Give gpsd a moment to bind or fail, so the caller hears about a taken port.
        for _ in 0..20 {
            if !self.is_current(&r) || r.connected.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        if !self.is_current(&r) {
            // Let the exit handler record why.
            let mut d = r.done.subscribe();
            let _ = tokio::time::timeout(Duration::from_secs(2), d.wait_for(|v| *v)).await;
            let msg = lock(&self.st).last_exit.as_ref().and_then(|e| e["message"].as_str().map(String::from)).filter(|m| !m.is_empty());
            return Err(HttpError::new(503, format!("gpsd exited: {}", msg.unwrap_or_else(|| "unknown error".into()))));
        }
        ctx.changed();
        Ok(self.view())
    }

    fn is_current(&self, r: &Arc<Run>) -> bool {
        lock(&self.st).run.as_ref().is_some_and(|x| Arc::ptr_eq(x, r))
    }

    /// The gpsd JSON client: retries quickly while gpsd binds, reconnects a second after a drop.
    fn connect(self: &Arc<Self>, r: &Arc<Run>) {
        let me = self.clone();
        let r2 = r.clone();
        let h = tokio::spawn(async move {
            let mut attempt = 0u32;
            loop {
                if !me.is_current(&r2) || r2.stopping.load(Ordering::SeqCst) {
                    return;
                }
                match tokio::net::TcpStream::connect(("127.0.0.1", r2.port as u16)).await {
                    Ok(s) => {
                        attempt = 0;
                        me.follow(&r2, s).await;
                        r2.connected.store(false, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                    }
                    Err(_) => {
                        // gpsd hasn't bound yet; retry quickly at first, then back off.
                        let ms = (150u64 << attempt.min(10)).min(2000);
                        attempt += 1;
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }
                }
            }
        });
        *lock(&r.client) = Some(h.abort_handle());
    }

    async fn follow(&self, r: &Arc<Run>, s: tokio::net::TcpStream) {
        let (rd, mut wr) = s.into_split();
        r.connected.store(true, Ordering::SeqCst);
        // pps:true also enables TOFF (serial time vs system clock at the start of each cycle).
        if wr.write_all(b"?WATCH={\"enable\":true,\"json\":true,\"pps\":true};\n").await.is_err() {
            return;
        }
        let mut lines = BufReader::new(rd).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some(o) = parse_json(line) else { continue };
            let changed = {
                let mut st = lock(&r.state);
                let before = st.tpv.as_ref().and_then(|t| t.get("mode").cloned());
                ingest(&mut st, &o, now_ms());
                o.get("class").and_then(Value::as_str) == Some("TPV") && o.get("mode").cloned() != before
            };
            if changed {
                self.ctx().changed();
            }
        }
    }

    async fn stop_run(&self) {
        let Some(r) = self.current() else { return };
        r.stopping.store(true, Ordering::SeqCst);
        if let Some(h) = lock(&r.client).take() {
            h.abort();
        }
        r.proc.kill(SIGTERM);
        if r.proc.wait_for(Duration::from_secs(3)).await.is_none() {
            r.proc.kill(SIGKILL);
            r.proc.wait().await;
        }
        let mut d = r.done.subscribe();
        let _ = tokio::time::timeout(Duration::from_secs(2), d.wait_for(|v| *v)).await;
        {
            let mut st = lock(&self.st);
            if st.run.as_ref().is_some_and(|x| Arc::ptr_eq(x, &r)) {
                st.run = None;
            }
        }
        self.ctx().event(&format!("gpsd stopped ({})", r.path), None);
        self.ctx().changed();
    }

    async fn stop_serial(&self) {
        let _g = self.busy.lock().await;
        self.stop_run().await;
    }

    // ---------- views ----------

    fn view(&self) -> Value {
        let st = lock(&self.st);
        match &st.run {
            Some(r) => {
                let g = lock(&r.state);
                let errors = &g.errors[g.errors.len().saturating_sub(5)..];
                json!({
                    "running": true,
                    "device": r.path,
                    "key": r.key,
                    "since": r.since,
                    "port": r.port,
                    "share": r.share,
                    "readOnly": r.read_only,
                    "pps": r.pps,
                    "connected": r.connected.load(Ordering::SeqCst),
                    "connect": if r.share { format!("{}:{}", hostname(), r.port) } else { format!("127.0.0.1:{} (this Pi only)", r.port) },
                    "gpsd": { "version": g.version, "devices": g.devices, "errors": errors },
                    "fix": summarize(&g, now_ms()),
                    "satellites": g.sats.iter().map(|s| s.to_json()).collect::<Vec<_>>(),
                })
            }
            None => json!({ "running": false, "lastExit": st.last_exit }),
        }
    }

    fn status(&self) -> Value {
        let v = self.view();
        let (candidates, gps_n, log, summary) = {
            let st = lock(&self.st);
            let gps_n = st.candidates.iter().filter(|c| c.is_gps()).count();
            let summary = match &st.run {
                Some(r) => {
                    let fix = &v["fix"];
                    let f = fix["fix"].as_str().unwrap_or("unknown");
                    format!(
                        "{} · {}/{} sats · {}",
                        if f == "unknown" { "Waiting for data" } else { f },
                        fix["satsUsed"],
                        fix["satsVisible"],
                        r.path
                    )
                }
                None if gps_n > 0 => format!("{gps_n} receiver{}, gpsd off", if gps_n > 1 { "s" } else { "" }),
                None => "No GPS receiver".into(),
            };
            let log: Vec<Value> = st.log[st.log.len().saturating_sub(30)..].iter().map(|(t, l)| json!({ "t": t, "line": l })).collect();
            (st.candidates.iter().map(Candidate::to_json).collect::<Vec<_>>(), gps_n, log, summary)
        };
        let running = v["running"] == true;
        let mut m = Map::new();
        m.insert("active".into(), (running || gps_n > 0).into());
        m.insert("summary".into(), summary.into());
        m.insert("candidates".into(), candidates.into());
        m.insert("ppsDevices".into(), json!(self.pps_devices()));
        m.insert(
            "settings".into(),
            json!({
                "port": self.setting("port", json!(self.cfg.default_port())),
                "share": self.setting("share", json!(false)),
                "readOnly": self.setting("readOnly", json!(false)),
                "pps": self.setting("pps", json!(true)),
            }),
        );
        m.insert("log".into(), log.into());
        if let Value::Object(o) = v {
            for (k, x) in o {
                m.insert(k, x);
            }
        }
        Value::Object(m)
    }

    fn use_as_gps(&self, b: &Value) -> Result<Value, HttpError> {
        let d = b.get("device").map(js_string).unwrap_or_else(|| "undefined".into());
        let key = {
            let st = lock(&self.st);
            st.candidates.iter().find(|c| c.key == d || c.path == d).map(|c| c.key.clone())
        }
        .ok_or_else(|| HttpError::new(404, format!("No serial device \"{d}\"")))?;
        let mut m = self.setting("useAsGps", json!({})).as_object().cloned().unwrap_or_default();
        if b.get("use") == Some(&Value::Bool(false)) {
            m.shift_remove(&key);
        } else {
            m.insert(key, Value::Bool(true));
        }
        self.ctx().set_setting("useAsGps", Value::Object(m));
        self.scan_ttys();
        self.ctx().changed();
        Ok(json!({ "ok": true }))
    }
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
impl Driver for Gps {
    fn id(&self) -> &'static str {
        "gps"
    }
    fn title(&self) -> &'static str {
        "GPS"
    }
    fn order(&self) -> i32 {
        64
    }
    fn description(&self) -> &'static str {
        "USB GPS receivers via a temporary gpsd: fix, sky plot, PPS (experimental, nothing runs unless started)"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let i = &self.inner;
        let _ = i.ctx.set(ctx);
        i.scan_ttys();
        // Who holds each candidate tty: only worth the /proc walk while there are candidates.
        let epoch = i.epoch.load(Ordering::SeqCst);
        let me = i.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if me.epoch.load(Ordering::SeqCst) != epoch {
                    break;
                }
                if !lock(&me.st).candidates.is_empty() {
                    let m = me.clone();
                    let _ = tokio::task::spawn_blocking(move || m.refresh_holders()).await;
                }
            }
        });
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) {
        self.inner.epoch.fetch_add(1, Ordering::SeqCst);
        self.inner.stop_run().await;
    }

    fn udev(&self, ev: &UdevEvent) {
        let p = &ev.props;
        if prop(p, "SUBSYSTEM") != "tty" || !is_tty(prop(p, "DEVNAME")) {
            return;
        }
        let i = &self.inner;
        let before: Vec<String> = lock(&i.st).candidates.iter().filter(|c| c.is_gps()).map(|c| c.path.clone()).collect();
        i.scan_ttys();
        let dev = prop(p, "DEVNAME").to_string();
        if ev.action == "add" {
            let c = lock(&i.st).candidates.iter().find(|c| c.path == dev).cloned();
            if let Some(c) = c
                && c.is_gps()
                && !before.contains(&c.path)
            {
                i.ctx().event(
                    &format!("GPS receiver plugged in: {} on {}", if c.label.is_empty() { &c.model } else { c.label }, c.path),
                    None,
                );
            }
        }
        if ev.action == "remove" && i.current().is_some_and(|r| r.path == dev) {
            i.ctx().event(&format!("GPS receiver on {dev} unplugged; stopping gpsd"), None);
            let me = i.clone();
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move { me.stop_serial().await });
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
        let d = i.cfg.default_port();
        vec![
            route(
                i,
                Method::GET,
                "/api/gps",
                json!(
                    "GPS: candidate ttys (known receivers, serial adapters, who has them open), gpsd state, fix, DOPs, satellites, PPS/time offsets"
                ),
                |i, _r| async move { reply(i.status()) },
            ),
            route(
                i,
                Method::POST,
                "/api/gps/start",
                json!({ "body": { "device": "candidate key or /dev/ttyACM0 (optional when there's exactly one GPS)", "path": "any tty path instead, e.g. /dev/serial/by-id/…", "port": d, "share": false, "readOnly": false, "pps": true }, "description": "Start gpsd on a receiver as a temporary child (share:true listens on all addresses with -G; readOnly adds -b so gpsd never reconfigures the receiver). Settings are remembered." }),
                |i, r| async move {
                    let b = r.json()?;
                    reply(i.start(b).await?)
                },
            ),
            route(i, Method::POST, "/api/gps/stop", json!("Stop gpsd"), |i, _r| async move {
                i.stop_serial().await;
                reply(json!({ "ok": true }))
            }),
            route(
                i,
                Method::POST,
                "/api/gps/use",
                json!({ "body": { "device": "candidate key or path", "use": true }, "description": "Mark a generic USB serial device as a GPS receiver (remembered by its udev serial/port)" }),
                |i, r| async move { reply(i.use_as_gps(&r.json()?)?) },
            ),
        ]
    }

    fn tools(&self) -> Vec<Tool> {
        let i = &self.inner;
        let d = i.cfg.default_port();
        vec![
            tool(
                i,
                "gps_status",
                "GPS fix and satellites from the GPS receiver on BenchBob: fix mode (no fix/2D/3D), lat/lon/altitude, speed/track, UTC time and its offset from the system clock, HDOP/PDOP, satellites used/visible per constellation (GPS/GLONASS/Galileo/BeiDou) with elevation/azimuth/SNR, PPS offset. Also lists candidate serial devices when gpsd isn't running.",
                json!({ "type": "object", "properties": { "satellites": { "type": "boolean", "description": "Include the per-satellite list (default false)" } } }),
                |i, a| async move {
                    let mut s = i.status();
                    let o = s.as_object_mut().unwrap();
                    o.shift_remove("log");
                    let sats = o.shift_remove("satellites");
                    if a.get("satellites").is_some_and(truthy)
                        && let Some(sats) = sats
                    {
                        o.insert("satellites".into(), sats);
                    }
                    Ok(s)
                },
            ),
            tool(
                i,
                "gps_start",
                "Start gpsd (temporary, stops on gps_stop/unplug) on a GPS receiver so its fix can be read with gps_status and other machines can use it as a gpsd server.",
                json!({
                    "type": "object",
                    "properties": {
                        "device": { "type": "string", "description": "Candidate key or tty path from gps_status; optional when exactly one known GPS is plugged in" },
                        "port": { "type": "number", "description": format!("gpsd TCP port (default {d})") },
                        "share": { "type": "boolean", "description": "Listen on all network interfaces instead of localhost only" },
                        "readOnly": { "type": "boolean", "description": "Never send configuration commands to the receiver (gpsd -b)" },
                    },
                }),
                |i, a| async move { i.start(a).await },
            ),
            tool(
                i,
                "gps_stop",
                "Stop gpsd and release the GPS receiver's serial port.",
                json!({ "type": "object", "properties": {} }),
                |i, _a| async move {
                    i.stop_serial().await;
                    Ok(json!({ "ok": true }))
                },
            ),
        ]
    }
}

#[cfg(test)]
mod tests;
