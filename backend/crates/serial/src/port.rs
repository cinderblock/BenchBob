//! The port model and its I/O (serial.ts "Port model" and "Open / close / I/O").
//!
//! BenchBob is the single owner of each open port: it holds the fd and fans received bytes
//! out to every client (WebSocket, raw TCP, RFC 2217, REST/MCP waiters), keeping a
//! timestamped ring of recent traffic. Each port's state sits behind one mutex that is only
//! held for short, non-blocking work (ioctls, queue pushes); nothing awaits while holding it.

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use benchbob_core::{HttpError, WsClient};
use benchbob_core::timeline::now_ms;
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::io::unix::AsyncFd;
use tokio::sync::watch;

use crate::bridge::{Bridge, BridgeKind};
use crate::flashjob::FlashJob;
use crate::ring::Ring;
use crate::termios::{self as T, ModemLines, SerialConfig};
use crate::Inner;

pub const TXQ_MAX: usize = 1 << 20;
/// A browser tab that can't keep up is skipped rather than buffered without bound.
const WS_BUFFER_MAX: usize = 4 << 20;

#[derive(Debug, Clone, Default)]
pub struct Info {
    pub path: String,
    /// usb | onboard | manual
    pub kind: &'static str,
    pub name: String,
    pub vendor: String,
    pub model: String,
    pub vidpid: String,
    pub serial: String,
    pub usb_driver: String,
    pub by_id: String,
    pub family: &'static str,
    pub console: bool,
}

pub struct WsEntry {
    pub client: WsClient,
    pub quiet: bool,
    pub lease: bool,
    pub token: String,
}

pub struct Lock {
    pub owner: String,
    pub token: String,
    pub since: u64,
}

/// An open fd and the task reading it.
pub struct Io {
    pub fd: Arc<AsyncFd<OwnedFd>>,
    pub stop: watch::Sender<bool>,
    pub reader: Option<tokio::task::JoinHandle<()>>,
}

impl Io {
    pub fn raw(&self) -> i32 {
        self.fd.get_ref().as_raw_fd()
    }
}

pub struct PortState {
    pub info: Info,
    pub present: bool,
    pub io: Option<Io>,
    /// closed | open | autobaud | flashing | resetting
    pub state: &'static str,
    pub error: String,
    pub want: bool,
    pub leases: u32,
    pub cfg: SerialConfig,
    pub lines: Option<ModemLines>,
    pub brk: bool,
    pub ring: Ring,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub last_rx: u64,
    pub txq: VecDeque<Vec<u8>>,
    pub txq_bytes: usize,
    /// A retry (writable wait or the auto-baud hold) is pending.
    pub tx_waiting: bool,
    /// Auto-baud diverts rx here.
    pub tap: Option<Vec<u8>>,
    pub ws: Vec<WsEntry>,
    pub bridges: [Bridge; 2],
    pub lock: Option<Lock>,
    pub flash: Option<FlashJob>,
    pub autobaud: Option<Value>,
    /// Bumped on every open, so tasks of an earlier open know they're stale.
    pub epoch: u64,
}

pub struct Port {
    pub id: String,
    pub st: std::sync::Mutex<PortState>,
    /// Bumped on rx and on close: what REST/MCP waiters wait on.
    pub tick: watch::Sender<u64>,
}

impl Port {
    pub fn new(id: &str, info: Info, cfg: SerialConfig) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            st: std::sync::Mutex::new(PortState {
                info,
                present: true,
                io: None,
                state: "closed",
                error: String::new(),
                want: false,
                leases: 0,
                cfg,
                lines: None,
                brk: false,
                ring: Ring::default(),
                rx_bytes: 0,
                tx_bytes: 0,
                last_rx: 0,
                txq: VecDeque::new(),
                txq_bytes: 0,
                tx_waiting: false,
                tap: None,
                ws: Vec::new(),
                bridges: [Bridge::default(), Bridge::default()],
                lock: None,
                flash: None,
                autobaud: None,
                epoch: 0,
            }),
            tick: watch::channel(0).0,
        })
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, PortState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn wake(&self) {
        self.tick.send_modify(|v| *v += 1);
    }
}

impl PortState {
    pub fn open(&self) -> bool {
        self.io.is_some()
    }
    pub fn fd(&self) -> Option<i32> {
        self.io.as_ref().map(Io::raw)
    }
    pub fn bridge(&self, k: BridgeKind) -> &Bridge {
        &self.bridges[k as usize]
    }
    pub fn bridge_mut(&mut self, k: BridgeKind) -> &mut Bridge {
        &mut self.bridges[k as usize]
    }
}

pub fn http(status: u16, m: impl Into<String>) -> HttpError {
    HttpError::new(status, m)
}

pub fn ensure_present(st: &PortState) -> Result<(), HttpError> {
    if !st.present {
        return Err(http(409, format!("{} is unplugged", st.info.name)));
    }
    Ok(())
}

pub fn ensure_writable(st: &PortState) -> Result<(), HttpError> {
    if st.info.console {
        return Err(http(409, format!("{} is the kernel console (cmdline.txt console=); BenchBob only reads it", st.info.path)));
    }
    if st.state == "flashing" {
        return Err(http(409, format!("{} is being flashed", st.info.name)));
    }
    Ok(())
}

pub fn not_open(st: &PortState) -> HttpError {
    http(409, format!("{} is not open", st.info.name))
}

pub fn exists(path: &str) -> bool {
    std::path::Path::new(path).exists()
}

impl Inner {
    /// Push a port's status to its WebSocket clients and the dashboards.
    pub(crate) fn notify(&self, p: &Port, st: &PortState) {
        if !st.ws.is_empty() {
            let m = json!({ "type": "status", "port": self.port_status(p, st, None) }).to_string();
            for w in st.ws.iter().filter(|w| !w.quiet) {
                w.client.send(m.clone());
            }
        }
        self.changed();
    }

    /// Open the port if it isn't. Synchronous: nothing else can sneak in between the check
    /// and the open.
    pub(crate) fn ensure_open(self: &Arc<Self>, p: &Arc<Port>) -> Result<(), HttpError> {
        let mut st = p.lock();
        self.ensure_open_locked(p, &mut st)
    }

    pub(crate) fn ensure_open_locked(self: &Arc<Self>, p: &Arc<Port>, st: &mut PortState) -> Result<(), HttpError> {
        if st.open() {
            return Ok(());
        }
        if st.state == "flashing" {
            return Err(http(409, format!("{} is being flashed", st.info.name)));
        }
        ensure_present(st)?;
        let fd = match T::open_port(&st.info.path, st.info.console) {
            Ok(fd) => fd,
            Err(e) => {
                let code = T::io_code(&e);
                st.error = format!("open {}: {code}", st.info.path);
                self.notify(p, st);
                let status = match code.as_str() {
                    "EBUSY" => 409,
                    "EACCES" => 403,
                    _ => 500,
                };
                return Err(http(status, st.error.clone()));
            }
        };
        let raw = fd.as_raw_fd();
        st.error.clear();
        let setup = (|| -> Result<SerialConfig, T::TtyError> {
            if st.info.console {
                return Ok(T::decode_termios(&T::get_termios(raw)?));
            }
            // Exclusive so nothing else opens the adapter behind our back. Not for manual
            // paths: the flag lives on the tty until its *last* fd closes, so with a pty's
            // other end (or anything else) still holding it, a BenchBob that dies without
            // clearing it would leave the port refusing every non-root open, its own restart
            // included. A USB adapter only we hold resets on our close, even a crash.
            if st.info.kind != "manual" {
                let _ = T::set_exclusive(raw);
            }
            let eff = T::configure(raw, &st.cfg)?;
            Ok(SerialConfig { flow: st.cfg.flow, ..eff })
        })();
        let cfg = match setup {
            Ok(c) => c,
            Err(e) => {
                drop(fd);
                st.error = e.to_string();
                self.notify(p, st);
                return Err(http(500, format!("{}: {e}", st.info.path)));
            }
        };
        // SAFETY: we own this fd (it was just opened) and register it nowhere else.
        let afd = match unsafe { AsyncFd::register(fd) } {
            Ok(a) => Arc::new(a),
            Err(e) => {
                let e = e.to_string();
                st.error = e.clone();
                self.notify(p, st);
                return Err(http(500, format!("{}: {e}", st.info.path)));
            }
        };
        st.cfg = cfg;
        st.state = "open";
        st.brk = false;
        st.epoch += 1;
        let epoch = st.epoch;
        let (stop, stop_rx) = watch::channel(false);
        let reader = tokio::spawn(self.clone().read_loop(p.clone(), afd.clone(), stop_rx, epoch));
        st.io = Some(Io { fd: afd, stop, reader: Some(reader) });
        self.start_modem_poll(p, st, epoch);
        self.log(&format!("{}: opened {} {}", p.id, st.info.path, T::config_label(&st.cfg)));
        self.notify(p, st);
        Ok(())
    }

    async fn read_loop(self: Arc<Self>, p: Arc<Port>, afd: Arc<AsyncFd<OwnedFd>>, mut stop: watch::Receiver<bool>, epoch: u64) {
        let mut buf = vec![0u8; 64 * 1024];
        let err: String = loop {
            tokio::select! {
                _ = stop.changed() => return,
                g = afd.readable() => {
                    let mut g = match g { Ok(g) => g, Err(e) => break T::io_code(&e) };
                    match g.try_io(|f| match T::read_some(f.get_ref().as_raw_fd(), &mut buf) {
                        Ok(None) => Err(std::io::ErrorKind::WouldBlock.into()),
                        Ok(Some(n)) => Ok(n),
                        Err(e) => Err(e),
                    }) {
                        Ok(Ok(0)) => break String::new(),
                        Ok(Ok(n)) => self.on_rx(&p, &buf[..n]),
                        Ok(Err(e)) => break T::io_code(&e),
                        Err(_would_block) => continue,
                    }
                }
            }
        };
        drop(afd);
        // Ended without us asking: the device went away (USB unplug, pty master closed) or EIO.
        let (path, retry) = {
            let mut st = p.lock();
            if st.epoch != epoch || !st.open() {
                return;
            }
            // Our own JoinHandle: don't wait for ourselves.
            if let Some(io) = st.io.as_mut() {
                io.reader = None;
            }
            self.log(&format!("{}: read ended{}", p.id, if err.is_empty() { String::new() } else { format!(" ({err})") }));
            let why = if err.is_empty() { "device went away".to_string() } else { format!("read error: {err}") };
            self.close_fd_locked(&p, &mut st, &why);
            (st.info.path.clone(), st.want || st.leases > 0)
        };
        if !exists(&path) {
            self.unplugged(&p);
        } else if retry {
            let (me, p) = (self.clone(), p.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = me.ensure_open(&p);
            });
        }
    }

    /// Release the fd. The returned task finishes once the fd is really closed, so a tool
    /// spawned next can open the tty.
    pub(crate) fn close_fd_locked(&self, p: &Port, st: &mut PortState, why: &str) -> Option<tokio::task::JoinHandle<()>> {
        let io = st.io.take();
        st.txq.clear();
        st.txq_bytes = 0;
        st.lines = None;
        if st.state != "flashing" {
            st.state = "closed";
        }
        if !why.is_empty() && why != "closed" {
            st.error = if why == "unplugged" { String::new() } else { why.to_string() };
        }
        let mut reader = None;
        if let Some(io) = io {
            if !st.info.console {
                let _ = T::clear_exclusive(io.raw());
            }
            let _ = io.stop.send(true);
            reader = io.reader;
            // Our Arc of the fd goes here; the reader's goes when it returns.
        }
        p.wake();
        self.notify(p, st);
        reader
    }

    pub(crate) async fn close_fd(&self, p: &Port, why: &str) {
        let h = {
            let mut st = p.lock();
            self.close_fd_locked(p, &mut st, why)
        };
        if let Some(h) = h {
            let _ = h.await;
        }
    }

    pub(crate) fn on_rx(&self, p: &Port, data: &[u8]) {
        let mut st = p.lock();
        if let Some(t) = st.tap.as_mut() {
            t.extend_from_slice(data);
            return;
        }
        let now = now_ms();
        st.ring.push(data, now);
        st.rx_bytes += data.len() as u64;
        st.last_rx = now;
        let b = Bytes::copy_from_slice(data);
        for w in &st.ws {
            if w.client.buffered() < WS_BUFFER_MAX {
                w.client.send_binary(b.clone());
            }
        }
        for br in &st.bridges {
            for c in &br.clients {
                c.send_data(data);
            }
        }
        drop(st);
        p.wake();
    }

    pub(crate) fn lock_check(st: &PortState, token: Option<&str>) -> Result<(), HttpError> {
        if let Some(l) = &st.lock
            && Some(l.token.as_str()) != token
        {
            return Err(http(423, format!("Writing to {} is locked by {}", st.info.name, l.owner)));
        }
        Ok(())
    }

    pub(crate) fn write(self: &Arc<Self>, p: &Arc<Port>, data: Vec<u8>, token: Option<&str>) -> Result<(), HttpError> {
        let mut st = p.lock();
        self.write_locked(p, &mut st, data, token)
    }

    pub(crate) fn write_locked(self: &Arc<Self>, p: &Arc<Port>, st: &mut PortState, data: Vec<u8>, token: Option<&str>) -> Result<(), HttpError> {
        ensure_writable(st)?;
        Self::lock_check(st, token)?;
        if !st.open() {
            return Err(not_open(st));
        }
        if st.txq_bytes + data.len() > TXQ_MAX {
            return Err(http(503, "Transmit queue full (the device isn't taking data; flow control?)"));
        }
        st.txq_bytes += data.len();
        st.txq.push_back(data);
        self.pump(p, st);
        Ok(())
    }

    fn pump(self: &Arc<Self>, p: &Arc<Port>, st: &mut PortState) {
        if st.tx_waiting {
            return;
        }
        let Some(fd) = st.fd() else { return };
        // Auto-baud promises to listen only: hold anything queued until it finishes.
        if st.state == "autobaud" {
            st.tx_waiting = true;
            let (me, p) = (self.clone(), p.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let mut st = p.lock();
                st.tx_waiting = false;
                me.pump(&p, &mut st);
            });
            return;
        }
        while let Some(len) = st.txq.front().map(Vec::len) {
            let n = match T::write_some(fd, &st.txq[0]) {
                Ok(n) => n,
                Err(e) => {
                    self.close_fd_locked(p, st, &format!("write error: {}", T::io_code(&e)));
                    return;
                }
            };
            st.tx_bytes += n as u64;
            st.txq_bytes -= n;
            if n < len {
                // Kernel buffer full: wait until the tty takes more.
                st.txq[0].drain(..n);
                st.tx_waiting = true;
                let afd = st.io.as_ref().unwrap().fd.clone();
                let epoch = st.epoch;
                let (me, p) = (self.clone(), p.clone());
                tokio::spawn(async move {
                    let wait = async {
                        if let Ok(mut g) = afd.writable().await {
                            g.clear_ready();
                        }
                    };
                    // A tty that never drains (flow control held off) is retried now and then.
                    let _ = tokio::time::timeout(Duration::from_millis(200), wait).await;
                    drop(afd);
                    let mut st = p.lock();
                    st.tx_waiting = false;
                    if st.epoch == epoch {
                        me.pump(&p, &mut st);
                    }
                });
                return;
            }
            st.txq.pop_front();
        }
    }

    fn start_modem_poll(self: &Arc<Self>, p: &Arc<Port>, st: &mut PortState, epoch: u64) {
        let Some(fd) = st.fd() else { return };
        match T::get_modem(fd) {
            Ok(l) => st.lines = Some(l),
            Err(_) => {
                st.lines = None; // ptys and some CDC devices have no modem lines
                return;
            }
        }
        let (me, p) = (self.clone(), p.clone());
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_millis(200));
            t.tick().await;
            loop {
                t.tick().await;
                let rfc = {
                    let mut st = p.lock();
                    if st.epoch != epoch {
                        return;
                    }
                    let Some(fd) = st.fd() else { return };
                    let l = match T::get_modem(fd) {
                        Ok(l) => l,
                        Err(e) => {
                            if e.errno == libc::EIO {
                                me.close_fd_locked(&p, &mut st, "device went away");
                                return;
                            }
                            continue;
                        }
                    };
                    if st.lines == Some(l) {
                        continue;
                    }
                    st.lines = Some(l);
                    let m = json!({ "type": "lines", "lines": l.to_json() }).to_string();
                    for w in st.ws.iter().filter(|w| !w.quiet) {
                        w.client.send(m.clone());
                    }
                    st.bridge(BridgeKind::Rfc2217).clients.clone()
                };
                for c in rfc {
                    me.rfc_notify_modem(&p, &c);
                }
                me.changed();
            }
        });
    }

    pub(crate) fn set_lines(&self, p: &Port, dtr: Option<bool>, rts: Option<bool>) -> Result<(), HttpError> {
        let mut st = p.lock();
        ensure_writable(&st)?;
        let Some(fd) = st.fd() else { return Err(not_open(&st)) };
        if let Err(e) = T::set_modem(fd, dtr, rts) {
            return Err(http(409, format!("{} has no controllable modem lines ({})", st.info.name, e.code())));
        }
        if let Ok(l) = T::get_modem(fd) {
            st.lines = Some(l);
        }
        let m = json!({ "type": "lines", "lines": st.lines.map(|l| l.to_json()) }).to_string();
        for w in st.ws.iter().filter(|w| !w.quiet) {
            w.client.send(m.clone());
        }
        drop(st);
        self.changed();
        Ok(())
    }

    pub(crate) fn apply_config(&self, p: &Port, c: &Value, save: bool) -> Result<SerialConfig, HttpError> {
        let mut st = p.lock();
        ensure_writable(&st)?;
        let cfg = T::normalize_config(c, &st.cfg).map_err(HttpError::bad_request)?;
        if let Some(fd) = st.fd() {
            let eff = T::configure(fd, &cfg).map_err(|e| http(500, e.to_string()))?;
            // ptys ignore data bits/parity; keep what was asked for so the UI and saved default don't drift.
            st.cfg = if st.info.kind == "manual" { cfg } else { eff };
        } else {
            st.cfg = cfg;
        }
        if save {
            self.save_port_settings(&p.id, &[("config", st.cfg.to_json())]);
        }
        self.notify(p, &st);
        Ok(st.cfg)
    }

    pub(crate) async fn send_break(&self, p: &Port, ms: f64) -> Result<(), HttpError> {
        let ms = ms.clamp(10.0, 5000.0);
        {
            let mut st = p.lock();
            ensure_writable(&st)?;
            let Some(fd) = st.fd() else { return Err(not_open(&st)) };
            T::set_break(fd, true).map_err(|e| http(500, e.to_string()))?;
            st.brk = true;
        }
        tokio::time::sleep(Duration::from_millis(ms as u64)).await;
        let mut st = p.lock();
        if let Some(fd) = st.fd() {
            let _ = T::set_break(fd, false);
        }
        st.brk = false;
        Ok(())
    }

    /// Drop a lease; close when nobody wants the port any more.
    pub(crate) fn release(&self, p: &Port, st: &mut PortState) {
        st.leases = st.leases.saturating_sub(1);
        if st.leases == 0 && !st.want && !self.open_on_plug(&p.id) && st.open() && st.state == "open" {
            self.close_fd_locked(p, st, "closed");
        }
    }
}
