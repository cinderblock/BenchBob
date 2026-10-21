//! The BLE scanner panel (peripherals/drivers/ble.ts).
//!
//! Passive view of every Bluetooth LE advertiser in range, on the onboard hci0 or a USB
//! dongle, with decoders for the common beacon and sensor formats ([`decode`]).
//!
//! How a scan runs (chosen after trying D-Bus/busctl, an interactive bluetoothctl, and
//! btmgmt; see plans/peripherals-radios.md):
//!  - `bluetoothctl` stays running as a child and holds a BlueZ discovery session with
//!    `duplicate-data on`, so the controller reports every advertisement (RSSI history needs
//!    them) and other BlueZ clients keep working. Discovery is tied to the D-Bus connection
//!    that started it, which is why a one-shot `busctl call StartDiscovery` doesn't work.
//!  - `sudo btmon -w <fifo>` streams raw HCI traffic as btsnoop. The LE advertising reports
//!    in it carry what BlueZ's D-Bus view throws away: the PDU type (connectable or not),
//!    scan responses as separate packets, and the exact AD bytes for the decoders.
//!
//! btmon writes to a FIFO (as the Bun server's does: btmon reopens `/dev/stdout` by path,
//! which fails on Bun's socketpair). The FIFO is opened read-write so the open never waits
//! for a writer, so the read ends when btmon exits rather than at EOF.
//!
//! The radio is left as it was found: if a scan had to lift an rfkill soft block or power the
//! adapter on, it is put back when scanning stops.

pub mod decode;
pub mod ids;
mod routes;

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use benchbob_core::http::{js_string, num, truthy};
use benchbob_core::run::{Opts, run};
use benchbob_core::udev::{Props, UdevEvent};
use benchbob_core::{Ctx, Driver, HttpError, Json, Route, Socket, StopReason, Tool, WsClient};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::watch;

use crate::decode::{BleDevice, BleTracker, BtsnoopReader, device_kind, mon, parse_hci_event};

/// A compiled-once regex.
#[macro_export]
#[doc(hidden)]
macro_rules! re {
    ($s:expr) => {{
        static R: std::sync::LazyLock<::regex::Regex> = std::sync::LazyLock::new(|| ::regex::Regex::new($s).unwrap());
        &*R
    }};
}

const BTMON: &str = "/usr/bin/btmon";
const BTCTL: &str = "/usr/bin/bluetoothctl";
const BUSCTL: &str = "/usr/bin/busctl";
const RFKILL: &str = "/usr/sbin/rfkill";

/// "active" while anything was heard this recently
const RECENT_MS: f64 = 5.0 * 60_000.0;
/// drop advertisers silent for this long
const FORGET_MS: f64 = 30.0 * 60_000.0;
const MAX_SCAN_S: f64 = 600.0;
/// a page reload shouldn't bounce the radio
const LIVE_GRACE_MS: u64 = 4000;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A spawned task, aborted when this handle is dropped.
struct Task(tokio::task::JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// ---------- Controllers ----------

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Rfkill {
    index: i64,
    soft: bool,
    hard: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Controller {
    /// hci0
    name: String,
    index: Option<i64>,
    devpath: String,
    /// "onboard (UART)" or the USB product
    bus: String,
    address: String,
    powered: Option<bool>,
    /// BlueZ: on / off / off-blocked / …
    power_state: String,
    rfkill: Option<Rfkill>,
}

impl Controller {
    fn json(&self) -> Value {
        json!({
            "name": self.name,
            "index": self.index.map(Value::from).unwrap_or(Value::Null),
            "devpath": self.devpath,
            "bus": self.bus,
            "address": self.address,
            "powered": self.powered,
            "powerState": self.power_state,
            "rfkill": self.rfkill.as_ref().map(|r| json!({ "index": r.index, "soft": r.soft, "hard": r.hard })),
        })
    }
}

fn read_rfkill(name: &str) -> Option<Rfkill> {
    let dir = format!("/sys/class/bluetooth/{name}");
    let rf = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|f| f.strip_prefix("rfkill").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())))?;
    let rd = |f: &str| benchbob_core::run::read_text(format!("{dir}/{rf}/{f}"));
    Some(Rfkill { index: benchbob_core::http::parse_int(&rd("index")).unwrap_or(0), soft: rd("soft") == "1", hard: rd("hard") == "1" })
}

async fn adapter_props(name: &str) -> (String, Option<bool>, String) {
    let path = format!("/org/bluez/{name}");
    let r = run(
        &[BUSCTL, "--json=short", "get-property", "org.bluez", &path, "org.bluez.Adapter1", "Address", "Powered", "PowerState"],
        Opts::timeout_ms(3000),
    )
    .await;
    if r.code != 0 {
        return (String::new(), None, if r.err.contains("ServiceUnknown") { "bluetoothd not running".into() } else { "unknown".into() });
    }
    let vals: Vec<Value> = r
        .out
        .split('\n')
        .map(|l| serde_json::from_str::<Value>(l).ok().and_then(|v| v.get("data").cloned()).unwrap_or(Value::Null))
        .collect();
    let g = |i: usize| vals.get(i).cloned().unwrap_or(Value::Null);
    let address = g(0).as_str().map(String::from).filter(|s| !s.is_empty()).unwrap_or_default();
    let power_state = g(2).as_str().map(String::from).unwrap_or_default();
    (address, g(1).as_bool(), power_state)
}

/// Vendor and product for display (core.ts `usbName`).
fn usb_name(p: &Props) -> [String; 2] {
    let dec = |k: &str| {
        let s = p.get(k).map(String::as_str).unwrap_or("");
        re!(r"\\x([0-9a-fA-F]{2})")
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
    [pick("ID_VENDOR_FROM_DATABASE", "ID_VENDOR_ENC", "ID_VENDOR"), pick("ID_MODEL_FROM_DATABASE", "ID_MODEL_ENC", "ID_MODEL")]
}

// ---------- Scan session ----------

#[derive(Debug, Clone, Default)]
pub(crate) struct Restore {
    rfkill: Option<i64>,
    power: bool,
}

pub(crate) struct Spawned {
    pid: Option<u32>,
    exit: watch::Receiver<Option<i32>>,
    stdin: Option<ChildStdin>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
}

pub(crate) struct Scan {
    id: u64,
    ctrl: Controller,
    started: u64,
    /// 0 = open-ended (live watchers)
    until: u64,
    btmon_pid: Option<u32>,
    btctl_pid: Option<u32>,
    btmon_exit: watch::Receiver<Option<i32>>,
    btctl_exit: watch::Receiver<Option<i32>>,
    stdin: Option<ChildStdin>,
    restore: Restore,
    packets: u64,
    stopping: bool,
}

impl Scan {
    fn info(&self) -> Value {
        json!({
            "controller": self.ctrl.name,
            "started": self.started,
            "until": if self.until != 0 { Value::from(self.until) } else { Value::Null },
            "live": self.until == 0,
            "packets": self.packets,
            "stopping": self.stopping,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LastScan {
    ctrl: String,
    started: u64,
    ended: u64,
    packets: u64,
}

impl LastScan {
    fn json(&self) -> Value {
        json!({ "ctrl": self.ctrl, "started": self.started, "ended": self.ended, "packets": self.packets })
    }
}

#[derive(Default)]
pub(crate) struct State {
    controllers: Vec<Controller>,
    pub tracker: BleTracker,
    scan: Option<Scan>,
    last_error: String,
    last_scan: Option<LastScan>,
    /// The pending "stop the live scan" timer, if any (its generation).
    stop_timer: Option<u64>,
    /// WebSocket watchers: id → (client, live)
    watchers: IndexMap<u64, (WsClient, bool)>,
    last_push_sig: String,
    next_id: u64,
}

impl State {
    fn scan_info(&self) -> Value {
        self.scan.as_ref().map(Scan::info).unwrap_or(Value::Null)
    }
    fn live_watchers(&self) -> usize {
        self.watchers.values().filter(|w| w.1).count()
    }
}

pub(crate) struct Inner {
    ctx: OnceLock<Ctx>,
    pub st: Mutex<State>,
    /// Serialises start/stop.
    busy: tokio::sync::Mutex<()>,
    /// Bumped by every stopScan (scanAndWait's waiters).
    stopped: watch::Sender<u64>,
    epoch: AtomicU64,
    loops: Mutex<Vec<Task>>,
}

/// The BLE driver.
pub struct Ble {
    inner: Arc<Inner>,
}

impl Default for Ble {
    fn default() -> Self {
        Self::new()
    }
}

impl Ble {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                ctx: OnceLock::new(),
                st: Mutex::new(State::default()),
                busy: tokio::sync::Mutex::new(()),
                stopped: watch::channel(0).0,
                epoch: AtomicU64::new(0),
                loops: Mutex::new(Vec::new()),
            }),
        }
    }
}

/// `x ? Number(x) …`: a query or argument as a JavaScript number (None for missing/empty/NaN).
pub(crate) fn num_arg(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    if v.is_null() || v.as_str() == Some("") {
        return None;
    }
    let n = benchbob_core::http::js_number(v);
    (!n.is_nan()).then_some(n)
}

impl Inner {
    fn changed(&self) {
        if let Some(c) = self.ctx.get() {
            c.changed();
        }
    }
    fn event(&self, t: &str) {
        if let Some(c) = self.ctx.get() {
            c.event(t, None);
        }
    }
    fn log(&self, m: &str) {
        if let Some(c) = self.ctx.get() {
            c.log(m);
        }
    }
    fn setting(&self, k: &str) -> Option<Value> {
        self.ctx.get().and_then(|c| c.setting(k))
    }
    fn default_controller(&self) -> String {
        self.setting("controller").map(|v| if v.is_string() { v.as_str().unwrap().to_string() } else { js_string(&v) }).unwrap_or_default()
    }
    fn restore_radio(&self) -> Value {
        self.setting("restoreRadio").unwrap_or(Value::Bool(true))
    }

    async fn refresh_controllers(&self) {
        let Some(ctx) = self.ctx.get() else { return };
        let hosts = ctx.devices(Some(&|p: &Props| {
            p.get("SUBSYSTEM").map(String::as_str) == Some("bluetooth") && p.get("DEVTYPE").map(String::as_str) == Some("host")
        }));
        let mut next = Vec::new();
        for p in hosts {
            let devpath = p.get("DEVPATH").cloned().unwrap_or_default();
            let name = devpath.rsplit('/').next().unwrap_or("").to_string();
            let bus = match ctx.ancestor(&devpath, "usb", Some("usb_device")) {
                Some(u) => {
                    let n = usb_name(&u).into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
                    if n.is_empty() {
                        let g = |k: &str| u.get(k).cloned().unwrap_or_else(|| "undefined".into());
                        format!("USB {}:{}", g("ID_VENDOR_ID"), g("ID_MODEL_ID"))
                    } else {
                        n
                    }
                }
                None if devpath.contains("serial") => "onboard (UART)".into(),
                None => "onboard".into(),
            };
            let (address, powered, power_state) = adapter_props(&name).await;
            next.push(Controller {
                index: benchbob_core::http::parse_int(name.get(3..).unwrap_or("")),
                rfkill: read_rfkill(&name),
                name,
                devpath,
                bus,
                address,
                powered,
                power_state,
            });
        }
        // NaN indexes compare as equal in JavaScript's sort, keeping their order.
        next.sort_by(|a, b| match (a.index, b.index) {
            (Some(x), Some(y)) => x.cmp(&y),
            _ => std::cmp::Ordering::Equal,
        });
        self.st.lock().unwrap().controllers = next;
        self.changed();
    }

    pub(crate) fn pick_controller(&self, want: Option<&str>) -> Result<Controller, HttpError> {
        let want = want.filter(|w| !w.is_empty());
        let pref = want.map(String::from).unwrap_or_else(|| self.default_controller());
        let st = self.st.lock().unwrap();
        let c = st
            .controllers
            .iter()
            .find(|c| c.name == pref || (!c.address.is_empty() && c.address == pref.to_uppercase()))
            .or(if want.is_none() { st.controllers.first() } else { None });
        c.cloned().ok_or_else(|| match want {
            Some(w) => HttpError::new(404, format!("No Bluetooth controller \"{w}\"")),
            None => HttpError::new(409, "No Bluetooth controller found"),
        })
    }

    async fn power_up(&self, c: &Controller) -> Result<Restore, HttpError> {
        let mut restore = Restore::default();
        let rf = read_rfkill(&c.name);
        if rf.as_ref().is_some_and(|r| r.hard) {
            return Err(HttpError::new(409, format!("{} is hard-blocked (hardware switch); can't scan", c.name)));
        }
        if let Some(rf) = rf.filter(|r| r.soft) {
            let r = run(&["sudo", "-n", RFKILL, "unblock", &rf.index.to_string()], Opts::timeout_ms(5000)).await;
            if r.code != 0 {
                return Err(HttpError::new(500, format!("rfkill unblock {}: {}", c.name, if r.err.is_empty() { r.out } else { r.err })));
            }
            restore.rfkill = Some(rf.index);
        }
        // bluetoothd needs a moment after an unblock before Powered=true sticks.
        let path = format!("/org/bluez/{}", c.name);
        for i in 0..10 {
            if adapter_props(&c.name).await.1 == Some(true) {
                break;
            }
            let r =
                run(&[BUSCTL, "set-property", "org.bluez", &path, "org.bluez.Adapter1", "Powered", "b", "true"], Opts::timeout_ms(5000))
                    .await;
            if r.code == 0 {
                restore.power = true;
                break;
            }
            if i == 9 {
                return Err(HttpError::new(500, format!("Couldn't power on {}: {}", c.name, r.err)));
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Ok(restore)
    }

    async fn put_radio_back(&self, c: &Controller, restore: &Restore) {
        if !truthy(&self.restore_radio()) {
            return;
        }
        if restore.power {
            let path = format!("/org/bluez/{}", c.name);
            run(&[BUSCTL, "set-property", "org.bluez", &path, "org.bluez.Adapter1", "Powered", "b", "false"], Opts::timeout_ms(5000)).await;
        }
        if let Some(i) = restore.rfkill {
            run(&["sudo", "-n", RFKILL, "block", &i.to_string()], Opts::timeout_ms(5000)).await;
        }
    }

    /// Spawn a child (killed on driver stop, through the registry's handle when there is a
    /// Ctx), returning its pid, a receiver for its exit code, and its pipes.
    fn spawn(&self, cmd: &mut tokio::process::Command) -> std::io::Result<Spawned> {
        let (tx, exit) = watch::channel(None);
        let code_of = |s: std::io::Result<std::process::ExitStatus>| {
            use std::os::unix::process::ExitStatusExt;
            s.ok().map(|s| s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))).unwrap_or(-1)
        };
        let sp = match self.ctx.get() {
            Some(ctx) => {
                let mut c = ctx.spawn(cmd)?;
                let sp = Spawned {
                    pid: c.child.id(),
                    exit,
                    stdin: c.child.stdin.take(),
                    stdout: c.child.stdout.take(),
                    stderr: c.child.stderr.take(),
                };
                tokio::spawn(async move {
                    let code = code_of(c.child.wait().await);
                    let _ = tx.send(Some(code));
                });
                sp
            }
            None => {
                let mut c = cmd.kill_on_drop(true).spawn()?;
                let sp = Spawned { pid: c.id(), exit, stdin: c.stdin.take(), stdout: c.stdout.take(), stderr: c.stderr.take() };
                tokio::spawn(async move {
                    let code = code_of(c.wait().await);
                    let _ = tx.send(Some(code));
                });
                sp
            }
        };
        Ok(sp)
    }

    pub(crate) async fn start_scan(self: &Arc<Self>, controller: Option<String>, duration_s: Option<f64>) -> Result<(), HttpError> {
        let _g = self.busy.lock().await;
        let dur_until = match duration_s.filter(|d| *d != 0.0 && !d.is_nan()) {
            Some(d) => now_ms() + (d.min(MAX_SCAN_S) * 1000.0) as u64,
            None => 0,
        };
        {
            let mut st = self.st.lock().unwrap();
            st.stop_timer = None;
            if let Some(s) = st.scan.as_mut().filter(|s| !s.stopping) {
                if let Some(c) = controller.as_deref().filter(|c| !c.is_empty())
                    && c != s.ctrl.name
                    && c != s.ctrl.address
                {
                    return Err(HttpError::new(409, format!("Already scanning on {}; stop it first", s.ctrl.name)));
                }
                // Extend a timed scan; an open-ended (live) one already outlasts it.
                if s.until != 0 && dur_until != 0 {
                    s.until = s.until.max(dur_until);
                } else if dur_until == 0 {
                    s.until = 0;
                }
                return Ok(());
            }
        }
        self.refresh_controllers().await;
        let c = self.pick_controller(controller.as_deref())?;
        self.st.lock().unwrap().last_error.clear();
        let restore = self.power_up(&c).await?;

        // Raw HCI first, so the first advertisements after discovery starts aren't missed.
        let dir = match mkdtemp(&format!("{}/benchbob-ble-", tmpdir())) {
            Ok(d) => d,
            Err(e) => {
                self.put_radio_back(&c, &restore).await;
                return Err(HttpError::new(500, format!("mkdtemp: {e}")));
            }
        };
        let fifo = format!("{dir}/hci.btsnoop");
        let mk = run(&["mkfifo", "-m", "600", &fifo], Opts::default()).await;
        if mk.code != 0 {
            let _ = std::fs::remove_dir_all(&dir);
            self.put_radio_back(&c, &restore).await;
            return Err(HttpError::new(500, format!("mkfifo: {}", mk.err)));
        }
        let rx = match tokio::net::unix::pipe::OpenOptions::new().read_write(true).open_receiver(&fifo) {
            Ok(r) => r,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                self.put_radio_back(&c, &restore).await;
                return Err(HttpError::new(500, format!("open {fifo}: {e}")));
            }
        };
        let mut btmon_cmd = tokio::process::Command::new("sudo");
        btmon_cmd.args(["-n", BTMON, "-w", &fifo]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut btctl_cmd = tokio::process::Command::new(BTCTL);
        btctl_cmd.env("TERM", "dumb").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let (btmon, btctl) = match (self.spawn(&mut btmon_cmd), self.spawn(&mut btctl_cmd)) {
            (Ok(a), Ok(b)) => (a, b),
            (a, b) => {
                for p in [a.as_ref().ok(), b.as_ref().ok()].into_iter().flatten().filter_map(|x| x.pid) {
                    kill(p);
                }
                let _ = std::fs::remove_dir_all(&dir);
                self.put_radio_back(&c, &restore).await;
                let e = a.err().or(b.err()).map(|e| e.to_string()).unwrap_or_default();
                return Err(HttpError::new(500, e));
            }
        };
        let (btmon_pid, btmon_exit) = (btmon.pid, btmon.exit.clone());
        let (btctl_pid, btctl_exit) = (btctl.pid, btctl.exit.clone());
        let mut stdin = btctl.stdin;
        if let Some(i) = stdin.as_mut() {
            let _ = i.write_all(format!("select {}\nmenu scan\nduplicate-data on\nback\nscan le\n", c.address).as_bytes()).await;
            let _ = i.flush().await;
        }
        let id = {
            let mut st = self.st.lock().unwrap();
            st.next_id += 1;
            let id = st.next_id;
            st.scan = Some(Scan {
                id,
                ctrl: c.clone(),
                started: now_ms(),
                until: dur_until,
                btmon_pid,
                btctl_pid,
                btmon_exit: btmon_exit.clone(),
                btctl_exit: btctl_exit.clone(),
                stdin,
                restore,
                packets: 0,
                stopping: false,
            });
            id
        };
        tokio::spawn(self.clone().read_btmon(id, c.clone(), rx, btmon.stderr, btmon_exit, dir));
        tokio::spawn(self.clone().read_btctl(id, btctl.stdout, btctl.stderr));
        let secs = if dur_until != 0 {
            format!(" for {} s", ((dur_until.saturating_sub(now_ms())) as f64 / 1000.0).round())
        } else {
            " (live)".into()
        };
        self.event(&format!("BLE scan started on {}{secs}", c.name));
        self.changed();
        Ok(())
    }

    pub(crate) async fn stop_scan(self: &Arc<Self>, reason: &str) {
        let _g = self.busy.lock().await;
        let (stdin, ctrl, btmon_pid, btctl_pid, mut btmon_exit, mut btctl_exit) = {
            let mut st = self.st.lock().unwrap();
            let Some(s) = st.scan.as_mut().filter(|s| !s.stopping) else { return };
            s.stopping = true;
            (s.stdin.take(), s.ctrl.clone(), s.btmon_pid, s.btctl_pid, s.btmon_exit.clone(), s.btctl_exit.clone())
        };
        if let Some(mut i) = stdin {
            let _ = i.write_all(b"scan off\nquit\n").await;
            let _ = i.flush().await;
        }
        let wait = |rx: &mut watch::Receiver<Option<i32>>| {
            let mut rx = rx.clone();
            async move {
                let _ = tokio::time::timeout(Duration::from_millis(1500), async {
                    while rx.borrow_and_update().is_none() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                })
                .await;
            }
        };
        wait(&mut btctl_exit).await;
        if let Some(p) = btctl_pid {
            kill(p);
        }
        if let Some(p) = btmon_pid {
            kill(p);
        }
        wait(&mut btmon_exit).await;
        let restore = self.st.lock().unwrap().scan.as_ref().map(|s| s.restore.clone()).unwrap_or_default();
        self.put_radio_back(&ctrl, &restore).await;
        let packets = {
            let mut st = self.st.lock().unwrap();
            let s = st.scan.take();
            let packets = s.as_ref().map(|s| s.packets).unwrap_or(0);
            st.last_scan =
                Some(LastScan { ctrl: ctrl.name.clone(), started: s.as_ref().map(|s| s.started).unwrap_or(0), ended: now_ms(), packets });
            if !reason.is_empty() {
                st.last_error = reason.into();
            }
            packets
        };
        self.stopped.send_modify(|n| *n += 1);
        self.event(&format!(
            "BLE scan stopped on {}{} ({packets} adverts)",
            ctrl.name,
            if reason.is_empty() { String::new() } else { format!(": {reason}") }
        ));
        self.refresh_controllers().await;
    }

    async fn read_btmon(
        self: Arc<Self>,
        id: u64,
        ctrl: Controller,
        mut fifo: tokio::net::unix::pipe::Receiver,
        stderr: Option<tokio::process::ChildStderr>,
        mut exited: watch::Receiver<Option<i32>>,
        dir: String,
    ) {
        let err_buf = Arc::new(Mutex::new(String::new()));
        if let Some(mut e) = stderr {
            let eb = err_buf.clone();
            tokio::spawn(async move {
                let mut s = Vec::new();
                let _ = e.read_to_end(&mut s).await;
                eb.lock().unwrap().push_str(&String::from_utf8_lossy(&s));
            });
        }
        let mut reader = BtsnoopReader::default();
        let mut err = String::new();
        let mut buf = vec![0u8; 64 * 1024];
        let idx = ctrl.index.unwrap_or(-1);
        loop {
            // Holding the FIFO open read-write means no EOF when btmon exits; end on exit instead.
            let n = tokio::select! {
                r = fifo.read(&mut buf) => match r {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => { err = e.to_string(); break; }
                },
                _ = async { while exited.borrow_and_update().is_none() { if exited.changed().await.is_err() { break; } } } => break,
            };
            match reader.push(&buf[..n]) {
                Ok(recs) => {
                    let mut st = self.st.lock().unwrap();
                    let st = &mut *st;
                    let mut count = 0;
                    for rec in recs {
                        if rec.opcode != mon::EVENT || rec.index as i64 != idx {
                            continue;
                        }
                        for r in parse_hci_event(&rec.data, rec.ts, rec.index) {
                            st.tracker.ingest(&r, &ctrl.name);
                            count += 1;
                        }
                    }
                    if let Some(s) = st.scan.as_mut().filter(|s| s.id == id) {
                        s.packets += count;
                    }
                }
                Err(e) => {
                    err = e;
                    break;
                }
            }
        }
        // Wait for btmon's exit code (and stderr) before reporting.
        let code = loop {
            if let Some(c) = *exited.borrow_and_update() {
                break c;
            }
            if exited.changed().await.is_err() {
                break -1;
            }
        };
        drop(fifo);
        let _ = std::fs::remove_dir_all(&dir);
        tokio::time::sleep(Duration::from_millis(50)).await;
        if err.is_empty() {
            err = err_buf.lock().unwrap().clone();
        }
        let ours = self.st.lock().unwrap().scan.as_ref().is_some_and(|s| s.id == id && !s.stopping);
        if ours {
            let last = err.trim().split('\n').next_back().unwrap_or("").to_string();
            let why = format!("btmon exited ({code}){}", if err.trim().is_empty() { String::new() } else { format!(": {last}") });
            let me = self.clone();
            tokio::spawn(async move { me.stop_scan(&why).await });
        }
    }

    async fn read_btctl(
        self: Arc<Self>,
        id: u64,
        stdout: Option<tokio::process::ChildStdout>,
        stderr: Option<tokio::process::ChildStderr>,
    ) {
        // Drain stderr too, so a chatty bluetoothctl can never block on a full pipe.
        if let Some(mut e) = stderr {
            tokio::spawn(async move {
                let mut sink = vec![0u8; 4096];
                while matches!(e.read(&mut sink).await, Ok(n) if n > 0) {}
            });
        }
        // Drain stdout (it prints a line per device change; a full pipe would stall it) and
        // pick out discovery failures.
        if let Some(mut o) = stdout {
            let mut buf = String::new();
            let mut chunk = vec![0u8; 8192];
            loop {
                let n = match o.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.push_str(&re!(r"\x1b\[[0-9;]*[A-Za-z]|\r").replace_all(&String::from_utf8_lossy(&chunk[..n]), ""));
                while let Some(i) = buf.find('\n') {
                    let line = buf[..i].trim().to_string();
                    buf.drain(..=i);
                    if re!(r"(?i)Failed to start discovery|No default controller|Controller .* not available").is_match(&line) {
                        let e = re!(r"^\[.*?\]>?\s*").replace(&line, "").into_owned();
                        self.log(&e);
                        self.st.lock().unwrap().last_error = e;
                    }
                }
            }
        }
        let ours = self.st.lock().unwrap().scan.as_ref().is_some_and(|s| s.id == id && !s.stopping);
        if ours {
            let me = self.clone();
            tokio::spawn(async move { me.stop_scan("bluetoothctl exited").await });
        }
    }

    /// Timed scans end on their own; live scans end when the last watcher has been gone a while.
    fn on_tick(self: &Arc<Self>) {
        let now = now_ms();
        let mut stop_now = false;
        {
            let mut st = self.st.lock().unwrap();
            st.tracker.prune(now as f64, FORGET_MS);
            let live = st.live_watchers() > 0;
            let (until, active) = st.scan.as_ref().map(|s| (s.until, !s.stopping)).unwrap_or((0, false));
            if active {
                if until != 0 && now >= until && !live {
                    stop_now = true;
                } else if until == 0 && !live && st.stop_timer.is_none() {
                    st.next_id += 1;
                    let g = st.next_id;
                    st.stop_timer = Some(g);
                    let me = self.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(LIVE_GRACE_MS)).await;
                        let go = {
                            let mut st = me.st.lock().unwrap();
                            if st.stop_timer != Some(g) {
                                return;
                            }
                            st.stop_timer = None;
                            st.scan.as_ref().is_some_and(|s| s.until == 0) && st.live_watchers() == 0
                        };
                        if go {
                            me.stop_scan("").await;
                        }
                    });
                }
            }
        }
        if stop_now {
            let me = self.clone();
            tokio::spawn(async move { me.stop_scan("").await });
        }
        self.push_devices();
    }

    // ---------- WebSocket watchers ----------

    pub(crate) fn snapshot(st: &State) -> Value {
        json!({
            "type": "devices",
            "t": now_ms(),
            "scan": st.scan_info(),
            "packets": st.tracker.packets,
            "devices": st.tracker.list(None, None, Some(300)).into_iter().map(device_payload).collect::<Vec<_>>(),
        })
    }

    fn push_devices(&self) {
        let mut st = self.st.lock().unwrap();
        if st.watchers.is_empty() {
            return;
        }
        let snap = Self::snapshot(&st);
        let sig = format!(
            "{}|{}|{}",
            st.tracker.packets,
            snap["devices"].as_array().map(Vec::len).unwrap_or(0),
            serde_json::to_string(&snap["scan"]).unwrap()
        );
        if sig == st.last_push_sig {
            return;
        }
        st.last_push_sig = sig;
        let msg = serde_json::to_string(&snap).unwrap();
        for (c, _) in st.watchers.values() {
            c.send(msg.clone());
        }
    }

    // ---------- MCP/REST shapes ----------

    pub(crate) fn list_devices(
        &self,
        since: Option<f64>,
        seconds: Option<f64>,
        min_rssi: Option<f64>,
        limit: Option<f64>,
        beacons_only: bool,
    ) -> Vec<Value> {
        let st = self.st.lock().unwrap();
        let since = since.or_else(|| seconds.filter(|s| *s != 0.0).map(|s| now_ms() as f64 - s * 1000.0));
        let mut l = st.tracker.list(since, min_rssi, None);
        if beacons_only {
            l.retain(|d| !d.beacons.is_empty());
        }
        let lim = limit.filter(|l| *l != 0.0 && !l.is_nan()).unwrap_or(100.0).min(500.0);
        let n = if lim < 0.0 { (l.len() as f64 + lim.trunc()).max(0.0) as usize } else { (lim.trunc() as usize).min(l.len()) };
        l[..n].iter().map(|d| compact(d)).collect()
    }

    pub(crate) async fn scan_and_wait(self: &Arc<Self>, args: &Value) -> Result<Value, HttpError> {
        let d = num_arg(args.get("duration")).filter(|d| *d != 0.0).unwrap_or(10.0);
        let dur = d.clamp(1.0, 120.0);
        let since = now_ms() as f64;
        let mut stopped = self.stopped.subscribe();
        stopped.borrow_and_update();
        self.start_scan(args.get("controller").filter(|v| truthy(v)).map(js_string), Some(dur)).await?;
        let _ = tokio::time::timeout(Duration::from_millis((dur * 1000.0 + 500.0) as u64), stopped.changed()).await;
        let devices = self.list_devices(Some(since), None, num_arg(args.get("minRssi")), num_arg(args.get("limit")), false);
        let st = self.st.lock().unwrap();
        let controller = st
            .scan
            .as_ref()
            .map(|s| s.ctrl.name.clone())
            .filter(|n| !n.is_empty())
            .or_else(|| st.last_scan.as_ref().map(|l| l.ctrl.clone()));
        let mut m = Map::new();
        m.insert("controller".into(), controller.map(Value::from).unwrap_or(Value::Null));
        m.insert("duration_s".into(), num(dur));
        m.insert("count".into(), devices.len().into());
        if !st.last_error.is_empty() {
            m.insert("error".into(), st.last_error.clone().into());
        }
        m.insert("devices".into(), devices.into());
        // JSON.stringify drops an undefined controller.
        if m["controller"].is_null() {
            m.shift_remove("controller");
        }
        Ok(Value::Object(m))
    }

    pub(crate) fn status(&self) -> Value {
        let restore = self.restore_radio();
        let default = self.default_controller();
        let st = self.st.lock().unwrap();
        let now = now_ms() as f64;
        let recent = st.tracker.devices.values().filter(|d| now - d.last_seen < RECENT_MS).count();
        let s = st.scan.as_ref();
        let pl = |n: usize| if n == 1 { "" } else { "s" };
        let summary = if let Some(s) = s {
            format!("Scanning {} · {recent} device{}", s.ctrl.name, pl(recent))
        } else if recent > 0 {
            format!("{recent} device{} heard recently", pl(recent))
        } else if !st.controllers.is_empty() {
            format!("{} idle", st.controllers.iter().map(|c| c.name.clone()).collect::<Vec<_>>().join(", "))
        } else {
            "No controller".into()
        };
        let mut m = Map::new();
        m.insert("active".into(), (s.is_some() || recent > 0).into());
        m.insert("summary".into(), summary.into());
        m.insert("controllers".into(), st.controllers.iter().map(Controller::json).collect::<Vec<_>>().into());
        m.insert("defaultController".into(), default.into());
        m.insert("restoreRadio".into(), restore);
        m.insert("scan".into(), st.scan_info());
        m.insert("lastScan".into(), st.last_scan.as_ref().map(LastScan::json).unwrap_or(Value::Null));
        if !st.last_error.is_empty() {
            m.insert("error".into(), st.last_error.clone().into());
        }
        m.insert("recent".into(), recent.into());
        m.insert("known".into(), st.tracker.devices.len().into());
        Value::Object(m)
    }

    pub(crate) fn controllers_short(&self) -> Value {
        self.st
            .lock()
            .unwrap()
            .controllers
            .iter()
            .map(|c| json!({ "name": c.name, "address": c.address, "bus": c.bus, "powered": c.powered, "powerState": c.power_state }))
            .collect::<Vec<_>>()
            .into()
    }
}

pub(crate) fn device_payload(d: &BleDevice) -> Value {
    let mut m = d.json();
    m.insert("kind".into(), device_kind(d).into());
    Value::Object(m)
}

fn compact(d: &BleDevice) -> Value {
    let now = now_ms() as f64;
    let mut m = Map::new();
    let r = |x: f64| num((x + 0.5).floor());
    m.insert("addr".into(), d.addr.clone().into());
    m.insert("addrType".into(), d.addr_type.into());
    if !d.name.is_empty() {
        m.insert("name".into(), d.name.clone().into());
    }
    m.insert("rssi".into(), d.rssi.map(Value::from).unwrap_or(Value::Null));
    if let Some(t) = d.tx_power {
        m.insert("txPower".into(), t.into());
    }
    m.insert("connectable".into(), d.connectable.into());
    m.insert("lastSeenAgo_s".into(), r((now - d.last_seen) / 1000.0));
    m.insert("seenFor_s".into(), r((d.last_seen - d.first_seen) / 1000.0));
    m.insert("packets".into(), d.count.into());
    let kind = device_kind(d);
    if !kind.is_empty() {
        m.insert("kind".into(), kind.into());
    }
    if !d.mfr.is_empty() {
        let v: Vec<Value> = d
            .mfr
            .iter()
            .map(|x| {
                let mut o = Map::new();
                o.insert("company".into(), format!("0x{:04x}", x.company).into());
                if !x.company_name.is_empty() {
                    o.insert("name".into(), x.company_name.into());
                }
                o.insert("data".into(), x.data.clone().into());
                Value::Object(o)
            })
            .collect();
        m.insert("manufacturers".into(), v.into());
    }
    if !d.uuids.is_empty() {
        m.insert(
            "services".into(),
            d.uuids.iter().map(|(u, n)| if n.is_empty() { u.clone() } else { format!("{u} ({n})") }).collect::<Vec<_>>().into(),
        );
    }
    if !d.svc.is_empty() {
        let v: Vec<Value> = d
            .svc
            .iter()
            .map(|s| {
                let mut o = Map::new();
                o.insert("uuid".into(), s.uuid.clone().into());
                if !s.name.is_empty() {
                    o.insert("name".into(), s.name.into());
                }
                o.insert("data".into(), s.data.clone().into());
                Value::Object(o)
            })
            .collect();
        m.insert("serviceData".into(), v.into());
    }
    if !d.beacons.is_empty() {
        m.insert("beacons".into(), d.beacons.iter().map(decode::Beacon::json).collect::<Vec<_>>().into());
    }
    Value::Object(m)
}

fn kill(pid: u32) {
    // SAFETY: kill(2) on a child we spawned (sudo relays SIGTERM to btmon).
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
}

/// `os.tmpdir()`
fn tmpdir() -> String {
    std::env::var("TMPDIR").ok().map(|t| t.trim_end_matches('/').to_string()).filter(|t| !t.is_empty()).unwrap_or_else(|| "/tmp".into())
}

/// `mkdtempSync(prefix)`: prefix + 6 random characters, mode 0700.
fn mkdtemp(prefix: &str) -> std::io::Result<String> {
    let t = std::ffi::CString::new(format!("{prefix}XXXXXX")).map_err(std::io::Error::other)?;
    let mut buf = t.into_bytes_with_nul();
    // SAFETY: buf is a NUL-terminated template that mkdtemp rewrites in place.
    if unsafe { libc::mkdtemp(buf.as_mut_ptr().cast()) }.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    buf.pop();
    String::from_utf8(buf).map_err(std::io::Error::other)
}

#[async_trait]
impl Driver for Ble {
    fn id(&self) -> &'static str {
        "ble"
    }
    fn title(&self) -> &'static str {
        "Bluetooth LE"
    }
    fn order(&self) -> i32 {
        60
    }
    fn description(&self) -> &'static str {
        "Scan BLE advertisers on hci0 or USB dongles; decodes iBeacon, Eddystone, BTHome, Ruuvi, Xiaomi, Govee and more"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let i = &self.inner;
        let _ = i.ctx.set(ctx);
        let epoch = i.epoch.load(Ordering::SeqCst);
        i.refresh_controllers().await;
        let me = i.clone();
        i.loops.lock().unwrap().push(Task(tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_millis(1000));
            iv.tick().await;
            loop {
                iv.tick().await;
                if me.epoch.load(Ordering::SeqCst) != epoch {
                    return;
                }
                me.on_tick();
            }
        })));
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) {
        let i = &self.inner;
        i.epoch.fetch_add(1, Ordering::SeqCst);
        i.loops.lock().unwrap().clear();
        i.stop_scan("").await;
        let ws: Vec<WsClient> = i.st.lock().unwrap().watchers.values().map(|w| w.0.clone()).collect();
        for w in ws {
            w.close();
        }
    }

    fn udev(&self, ev: &UdevEvent) {
        let p = &ev.props;
        let g = |k: &str| p.get(k).map(String::as_str).unwrap_or("");
        if g("SUBSYSTEM") != "bluetooth" || g("DEVTYPE") != "host" || (ev.action != "add" && ev.action != "remove") {
            return;
        }
        let name = g("DEVPATH").rsplit('/').next().unwrap_or("").to_string();
        let i = self.inner.clone();
        i.event(&format!("Bluetooth controller {name} {}", if ev.action == "add" { "appeared" } else { "removed" }));
        if ev.action == "remove" && i.st.lock().unwrap().scan.as_ref().is_some_and(|s| s.ctrl.name == name) {
            let me = i.clone();
            tokio::spawn(async move { me.stop_scan("controller removed").await });
        }
        // bluetoothd registers the adapter a moment after udev announces it.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            i.refresh_controllers().await;
        });
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

#[cfg(test)]
mod tests;
