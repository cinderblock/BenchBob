//! The serial panel (peripherals/drivers/serial.ts).
//!
//! USB-serial adapters (FTDI, CP210x, CH34x, PL2303, CDC-ACM boards) and the onboard UARTs
//! (serial0/serial1, plus any uart2–5 a Pi 4 overlay adds, named from the device tree).
//! BenchBob is the single owner of each open port: it holds the fd and fans the received
//! bytes out to every client at once (dashboard terminal, WebSocket, raw TCP, RFC 2217,
//! REST/MCP agents), keeping a timestamped ring of recent traffic so a late joiner gets
//! history. Any client may write; an optional lock reserves writing for one holder.
//!
//! Ports open on demand, never just because they were plugged in (other drivers, e.g.
//! CAN's slcan, may want the same tty). A port is held open while any of these is true: the
//! user opened it (`want`), it is set to open on plug, or a bridge/WebSocket client holds a
//! lease.
//!
//! I/O: an O_NONBLOCK fd configured with termios2 ioctls ([`termios`]), read and written
//! through tokio's `AsyncFd` (epoll), so a full transmit buffer waits for the tty to drain
//! rather than polling.

pub mod autobaud;
mod bridge;
pub mod flash;
mod flashjob;
pub mod ident;
mod ops;
mod port;
pub mod rfc2217;
pub mod ring;
mod routes;
pub mod termios;
pub mod util;
mod ws;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use benchbob_core::udev::{Props, UdevEvent};
use benchbob_core::{Ctx, Driver, Json, Route, Socket, StopReason, Tool};
use serde_json::{Map, Value, json};

use crate::bridge::BridgeKind;
use crate::port::{Info, Port, PortState};
use crate::termios::{self as T, DEFAULT_CONFIG};

pub struct Serial {
    inner: Arc<Inner>,
}

impl Default for Serial {
    fn default() -> Self {
        Self::new()
    }
}

impl Serial {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                ctx: OnceLock::new(),
                ports: Mutex::new(Vec::new()),
                cmdline: Mutex::new(String::new()),
                rescan_gen: AtomicU64::new(0),
                job_seq: AtomicU64::new(0),
                offset: benchbob_core::paths::port_offset(),
                host: OnceLock::new(),
            }),
        }
    }
}

pub(crate) struct Inner {
    ctx: OnceLock<Ctx>,
    /// In insertion order, as the TypeScript's Map.
    ports: Mutex<Vec<Arc<Port>>>,
    cmdline: Mutex<String>,
    rescan_gen: AtomicU64,
    job_seq: AtomicU64,
    offset: u16,
    host: OnceLock<String>,
}

impl Inner {
    fn ctx(&self) -> &Ctx {
        self.ctx.get().expect("serial driver used before init")
    }

    pub(crate) fn log(&self, m: &str) {
        match self.ctx.get() {
            Some(c) => c.log(m),
            None => tracing::info!("[serial] {m}"),
        }
    }

    pub(crate) fn changed(&self) {
        if let Some(c) = self.ctx.get() {
            c.changed();
        }
    }

    pub(crate) fn event(&self, text: &str, detail: Option<Json>) {
        if let Some(c) = self.ctx.get() {
            c.event(text, detail);
        }
    }

    pub(crate) fn hostname(&self) -> String {
        self.host.get_or_init(util::hostname).clone()
    }

    // ---------- settings ----------

    fn setting(&self, key: &str) -> Option<Value> {
        self.ctx.get().and_then(|c| c.setting(key))
    }

    pub(crate) fn setting_num(&self, key: &str, dflt: f64) -> f64 {
        self.setting(key).and_then(|v| v.as_f64()).unwrap_or(dflt)
    }

    fn all_port_settings(&self) -> Map<String, Value> {
        self.setting("ports").and_then(|v| v.as_object().cloned()).unwrap_or_default()
    }

    pub(crate) fn port_settings(&self, id: &str) -> Value {
        self.all_port_settings().get(id).cloned().filter(Value::is_object).unwrap_or_else(|| json!({}))
    }

    pub(crate) fn save_port_settings(&self, id: &str, patch: &[(&str, Value)]) {
        let Some(c) = self.ctx.get() else { return };
        let mut all = self.all_port_settings();
        let e = all.entry(id.to_string()).or_insert_with(|| json!({}));
        if !e.is_object() {
            *e = json!({});
        }
        for (k, v) in patch {
            e[*k] = v.clone();
        }
        c.set_setting("ports", Value::Object(all));
    }

    pub(crate) fn open_on_plug(&self, id: &str) -> bool {
        self.port_settings(id).get("openOnPlug").is_some_and(benchbob_core::http::truthy)
    }

    /// Deterministic, persisted bridge slot per device: the lowest free one, assigned on first sight.
    pub(crate) fn slot_for(&self, id: &str) -> u32 {
        let all = self.all_port_settings();
        if let Some(s) = all.get(id).and_then(|s| s.get("slot")).and_then(Value::as_u64) {
            return s as u32;
        }
        let used: Vec<u64> = all.values().filter_map(|s| s.get("slot").and_then(Value::as_u64)).collect();
        let mut slot = 0;
        while used.contains(&slot) {
            slot += 1;
        }
        self.save_port_settings(id, &[("slot", json!(slot))]);
        slot as u32
    }

    // ---------- ports ----------

    pub(crate) fn port(&self, id: &str) -> Option<Arc<Port>> {
        self.ports.lock().unwrap().iter().find(|p| p.id == id).cloned()
    }

    pub(crate) fn get_port(&self, id: &str) -> Result<Arc<Port>, benchbob_core::HttpError> {
        self.port(id).ok_or_else(|| port::http(404, format!("No serial port \"{id}\". GET /api/serial lists them.")))
    }

    pub(crate) fn port_status(&self, p: &Port, st: &PortState, host: Option<&str>) -> Value {
        let host = host.map(String::from).unwrap_or_else(|| self.hostname());
        let i = &st.info;
        let ws_count = st.ws.len();
        json!({
            "id": p.id,
            "path": i.path,
            "byId": i.by_id,
            "kind": i.kind,
            "name": i.name,
            "vendor": i.vendor,
            "model": i.model,
            "vidpid": i.vidpid,
            "serial": i.serial,
            "usbDriver": i.usb_driver,
            "family": i.family,
            "resets": if i.console { &[][..] } else { ident::resets_for(i.family) },
            "flashTools": if i.console { &[][..] } else { ident::flash_tools_for(i.family) },
            "console": i.console,
            "present": st.present,
            "state": st.state,
            "open": st.open(),
            "error": st.error,
            "config": st.cfg.to_json(),
            "configLabel": T::config_label(&st.cfg),
            "lines": st.lines.map(|l| l.to_json()),
            "modem": st.lines.is_some(),
            "break": st.brk,
            "rxBytes": st.rx_bytes,
            "txBytes": st.tx_bytes,
            "lastRx": st.last_rx,
            "cursor": st.ring.end,
            "openOnPlug": self.open_on_plug(&p.id),
            "clients": { "ws": ws_count, "tcp": st.bridge(BridgeKind::Tcp).clients.len(), "rfc2217": st.bridge(BridgeKind::Rfc2217).clients.len() },
            "bridges": { "tcp": self.bridge_status(p, st, BridgeKind::Tcp, &host), "rfc2217": self.bridge_status(p, st, BridgeKind::Rfc2217, &host) },
            "lock": st.lock.as_ref().map(|l| json!({ "owner": l.owner, "since": l.since })),
            "flash": flashjob::job_view(st.flash.as_ref()),
            "autobaud": st.autobaud,
        })
    }

    /// USB ports first, then onboard ones, each by path (`localeCompare`, near enough).
    pub(crate) fn list_ports(&self, host: Option<&str>) -> Vec<Value> {
        let ports = self.ports.lock().unwrap().clone();
        let mut rows: Vec<(bool, String, Value)> = ports
            .iter()
            .map(|p| {
                let st = p.lock();
                (st.info.kind == "onboard", st.info.path.clone(), self.port_status(p, &st, host))
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())).then_with(|| a.1.cmp(&b.1)));
        rows.into_iter().map(|r| r.2).collect()
    }

    // ---------- discovery ----------

    fn is_usb_tty(&self, p: &Props) -> bool {
        let g = |k: &str| p.get(k).map(String::as_str).unwrap_or("");
        if g("SUBSYSTEM") != "tty" || g("DEVNAME").is_empty() {
            return false;
        }
        if g("DEVNAME").starts_with("/dev/ttyGS") {
            return false; // gadget side, not an adapter
        }
        g("ID_BUS") == "usb" || self.ctx().ancestor(g("DEVPATH"), "usb", Some("usb_device")).is_some()
    }

    fn describe_usb(&self, p: &Props) -> (String, Info) {
        let g = |k: &str| p.get(k).cloned().unwrap_or_default();
        let mut merged = self.ctx().ancestor(&g("DEVPATH"), "usb", Some("usb_device")).unwrap_or_default();
        for (k, v) in p {
            merged.insert(k.clone(), v.clone());
        }
        let m = |k: &str| merged.get(k).cloned().unwrap_or_default();
        let vid = m("ID_VENDOR_ID").to_lowercase();
        let pid = m("ID_MODEL_ID").to_lowercase();
        let known = ident::identify(&vid, &pid);
        let (vendor, model) = ident::usb_name(&merged);
        let usb_driver = g("ID_USB_DRIVER");
        let devlinks = g("DEVLINKS");
        let mut family = known.map(|k| k.family).unwrap_or(if !usb_driver.is_empty() && usb_driver != "cdc_acm" { "bridge" } else { "other" });
        // A generic Arduino-vendor id with an 8xxx pid is a native-USB board (Leonardo, SAMD…).
        if vid == "2341" && known.is_none_or(|k| k.pid.is_none()) && pid.starts_with('8') {
            family = "arduino-native";
        }
        let ifnum = g("ID_USB_INTERFACE_NUM");
        let iface = if !ifnum.is_empty() && ifnum != "00" {
            format!(" (port {})", i64::from_str_radix(&ifnum, 16).map(|n| n.to_string()).unwrap_or_else(|_| "NaN".into()))
        } else {
            String::new()
        };
        let devname = g("DEVNAME");
        let base = match known {
            Some(k) => k.name.to_string(),
            None if !model.is_empty() => model.clone(),
            None => devname.replacen("/dev/", "", 1),
        };
        (
            ident::stable_id(p),
            Info {
                path: devname.clone(),
                kind: "usb",
                name: base + &iface,
                vendor,
                model,
                vidpid: if !vid.is_empty() && !pid.is_empty() { format!("{vid}:{pid}") } else { String::new() },
                serial: m("ID_SERIAL_SHORT"),
                usb_driver: ident::usb_driver_name(&usb_driver).to_string(),
                by_id: devlinks.split_whitespace().find(|l| l.starts_with("/dev/serial/by-id/")).unwrap_or("").to_string(),
                family,
                console: false,
            },
        )
    }

    /// Is this tty a kernel console (cmdline console=…)? Then we never write to or reconfigure it.
    fn is_console(&self, alias: &str, real: &str) -> bool {
        let cmdline = self.cmdline.lock().unwrap();
        cmdline
            .split_whitespace()
            .filter_map(|w| w.strip_prefix("console="))
            .map(|v| v.split(',').next().unwrap_or(""))
            .any(|n| n == alias || n == real)
    }

    fn onboard(id: &str, path: String, name: String, console: bool) -> (String, Info) {
        (id.into(), Info { path, kind: "onboard", name, family: "onboard", console, ..Default::default() })
    }

    pub(crate) fn rescan(self: &Arc<Self>) {
        let mut found: Vec<(String, Info)> = Vec::new();
        // Filtered here, not inside devices(): the filter looks up ancestors in the same
        // database, and taking its read lock twice can deadlock against a waiting writer.
        let ttys: Vec<Props> = self.ctx().devices(Some(&|p: &Props| p.get("SUBSYSTEM").map(String::as_str) == Some("tty")));
        for d in ttys.iter().filter(|p| self.is_usb_tty(p)) {
            found.push(self.describe_usb(d));
        }
        // Onboard UARTs by their stable aliases.
        let mut aliased = Vec::new();
        for alias in ["serial0", "serial1"] {
            let link = format!("/dev/{alias}");
            if !port::exists(&link) {
                continue;
            }
            let Ok(real) = std::fs::canonicalize(&link) else { continue };
            let real = real.to_string_lossy().replacen("/dev/", "", 1);
            aliased.push(real.clone());
            let cons = self.is_console(alias, &real);
            found.push(Self::onboard(alias, link, format!("Onboard UART {alias} ({real})"), cons));
        }
        // The rest (dtoverlay=uart2…uart5 on a Pi 4) have no /dev alias; the device tree says
        // which controller each ttyAMA is, so they're named and keyed by that, not the tty number.
        for (uart, tty, _node, pins) in benchbob_gpio::config_txt::onboard_uarts() {
            let path = format!("/dev/{tty}");
            if aliased.contains(&tty) || !port::exists(&path) {
                continue;
            }
            let pins = if pins.is_empty() { String::new() } else { format!(" ({pins})") };
            let cons = self.is_console(&tty, &tty);
            found.push(Self::onboard(&uart, path, format!("Onboard {}{pins} {tty}", uart.to_uppercase()), cons));
        }
        let manual = self.setting("manual").and_then(|v| v.as_array().cloned()).unwrap_or_default();
        for path in manual.iter().filter_map(Value::as_str) {
            if !port::exists(path) {
                continue;
            }
            found.push((ident::manual_id(path), Info { path: path.into(), kind: "manual", name: path.into(), family: "other", ..Default::default() }));
        }

        let mut seen = Vec::new();
        for (id, d) in found {
            seen.push(id.clone());
            let existing = self.port(&id);
            match existing {
                None => {
                    let st = self.port_settings(&id);
                    let cfg = st.get("config").and_then(|c| T::normalize_config(c, &DEFAULT_CONFIG).ok()).unwrap_or(DEFAULT_CONFIG);
                    let (usb, name, path, vidpid) = (d.kind == "usb", d.name.clone(), d.path.clone(), d.vidpid.clone());
                    let p = Port::new(&id, d, cfg);
                    self.ports.lock().unwrap().push(p.clone());
                    if usb {
                        self.event(&format!("Serial: {name} on {path}"), Some(json!({ "id": id, "vidpid": vidpid })));
                    }
                    self.plugged(&p);
                }
                Some(p) => {
                    let replug = {
                        let mut st = p.lock();
                        if !st.present || st.info.path != d.path {
                            let (usb, name, path) = (d.kind == "usb", d.name.clone(), d.path.clone());
                            st.info = d;
                            st.present = true;
                            if usb {
                                self.event(&format!("Serial: {name} back on {path}"), Some(json!({ "id": id })));
                            }
                            true
                        } else {
                            false
                        }
                    };
                    if replug {
                        self.plugged(&p);
                    }
                }
            }
        }
        let ports = self.ports.lock().unwrap().clone();
        for p in ports {
            let skip = {
                let st = p.lock();
                // Manual ports (ptys) are only noticed missing when their stream ends; let that path decide.
                seen.contains(&p.id) || !st.present || (st.info.kind == "manual" && st.open())
            };
            if !skip {
                self.unplugged(&p);
            }
        }
        self.changed();
    }

    fn plugged(self: &Arc<Self>, p: &Arc<Port>) {
        let open = {
            let mut st = p.lock();
            self.sync_bridges(p, &mut st);
            st.want || self.open_on_plug(&p.id) || st.leases > 0
        };
        if open {
            // udev has set group and mode by the time we hear the udev (not kernel) event, but
            // cdc_acm can still refuse the first open for a moment after enumeration.
            let (me, p) = (self.clone(), p.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                if let Err(e) = me.ensure_open(&p) {
                    me.log(&format!("{}: open on plug failed: {}", p.id, e.message));
                }
            });
        }
    }

    pub(crate) fn unplugged(self: &Arc<Self>, p: &Arc<Port>) {
        let mut st = p.lock();
        if !st.present {
            return;
        }
        st.present = false;
        if st.info.kind == "usb" {
            self.event(&format!("Serial: {} unplugged", st.info.name), Some(json!({ "id": p.id })));
        }
        self.close_fd_locked(p, &mut st, "unplugged");
        self.sync_bridges(p, &mut st);
        self.notify(p, &st);
    }
}

#[async_trait]
impl Driver for Serial {
    fn id(&self) -> &'static str {
        "serial"
    }
    fn title(&self) -> &'static str {
        "Serial"
    }
    fn order(&self) -> i32 {
        20
    }
    fn description(&self) -> &'static str {
        "USB-serial adapters and the onboard UART: browser terminal, auto-baud, RFC 2217/TCP/WebSocket bridges, ESP and AVR flashing"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let _ = self.inner.ctx.set(ctx);
        *self.inner.cmdline.lock().unwrap() = benchbob_core::run::read_text("/boot/firmware/cmdline.txt");
        let _ = std::fs::create_dir_all(flashjob::FLASH_DIR);
        self.inner.rescan();
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) {
        let ports: Vec<Arc<Port>> = std::mem::take(&mut *self.inner.ports.lock().unwrap());
        let mut closing = Vec::new();
        for p in ports {
            let mut st = p.lock();
            for k in BridgeKind::ALL {
                self.inner.stop_bridge(&mut st, k);
            }
            for w in &st.ws {
                w.client.close_with(1001, "serial driver stopped");
            }
            if let Some(pid) = st.flash.as_ref().and_then(|j| j.pid) {
                // SAFETY: kill(2) on the flash tool we spawned.
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
            }
            if let Some(h) = self.inner.close_fd_locked(&p, &mut st, "closed") {
                closing.push(h);
            }
        }
        for h in closing {
            let _ = h.await;
        }
    }

    fn udev(&self, ev: &UdevEvent) {
        if ev.props.get("SUBSYSTEM").map(String::as_str) != Some("tty") {
            return;
        }
        let g = self.inner.rescan_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let me = self.inner.clone();
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if me.rescan_gen.load(Ordering::SeqCst) == g && me.ctx.get().is_some() {
                me.rescan();
            }
        });
    }

    fn status(&self) -> Option<Json> {
        let i = &self.inner;
        let ports = i.ports.lock().unwrap().clone();
        let mut shown = 0;
        let mut open: Vec<(String, String)> = Vec::new();
        let mut flashing: Option<(String, f64)> = None;
        for p in &ports {
            let st = p.lock();
            if st.present && st.info.kind != "onboard" {
                shown += 1;
            }
            if st.open() {
                open.push((st.info.name.clone(), T::config_label(&st.cfg)));
            }
            if flashing.is_none() && st.state == "flashing" {
                flashing = Some((st.info.name.clone(), st.flash.as_ref().map(|j| j.progress).unwrap_or(0.0)));
            }
        }
        let summary = if let Some((n, pr)) = flashing {
            format!("Flashing {n} {}%", util::js_round(pr))
        } else if open.len() == 1 {
            format!("{} open · {}", open[0].0, open[0].1)
        } else if shown > 0 || !open.is_empty() {
            format!(
                "{shown} port{}{}",
                if shown == 1 { "" } else { "s" },
                if open.is_empty() { String::new() } else { format!(" · {} open", open.len()) }
            )
        } else {
            "No USB serial ports".into()
        };
        Some(json!({ "active": shown > 0 || !open.is_empty(), "summary": summary, "offset": i.offset, "ports": i.list_ports(None) }))
    }

    fn routes(&self) -> Vec<Route> {
        routes::routes(&self.inner)
    }

    fn sockets(&self) -> Vec<Socket> {
        let inner = self.inner.clone();
        vec![Socket::new("/ws/serial/:id", Some(ws::WS_DOC), move |s| ws::session(inner.clone(), s))]
    }

    fn tools(&self) -> Vec<Tool> {
        routes::tools(&self.inner)
    }
}
