//! Network bridges (serial.ts "Network bridges"): a raw TCP byte socket and an RFC 2217
//! (Telnet COM-PORT-OPTION) server per port, each on its own listening port. A client
//! holds a lease on the port, so it opens on connect and closes after the last one leaves
//! (unless something else wants it open).

use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use benchbob_core::HttpError;
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc, watch};

use crate::Inner;
use crate::port::{Port, PortState, ensure_writable, http};
use crate::rfc2217::{PortHooks, Rfc2217Session, escape_iac};
use crate::termios::{self as T, ModemLines, Queue, SerialConfig};

const NET_PENDING_MAX: usize = 1 << 20;
static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeKind {
    Tcp = 0,
    Rfc2217 = 1,
}

impl BridgeKind {
    pub const ALL: [BridgeKind; 2] = [BridgeKind::Tcp, BridgeKind::Rfc2217];
    pub fn as_str(self) -> &'static str {
        match self {
            BridgeKind::Tcp => "tcp",
            BridgeKind::Rfc2217 => "rfc2217",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tcp" => Some(BridgeKind::Tcp),
            "rfc2217" => Some(BridgeKind::Rfc2217),
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct Bridge {
    pub listener: Option<tokio::task::JoinHandle<()>>,
    pub clients: Vec<Arc<NetClient>>,
    pub error: String,
    pub port: u16,
}

pub struct NetClient {
    pub id: u64,
    pub kind: BridgeKind,
    pub remote: String,
    /// Protocol replies: never held back by flow control.
    ctrl: mpsc::UnboundedSender<Vec<u8>>,
    data: mpsc::UnboundedSender<Bytes>,
    pending: AtomicUsize,
    dropped: AtomicUsize,
    suspended: AtomicBool,
    /// Resume after a flow-control pause.
    wake: Notify,
    closing: watch::Sender<bool>,
    pub session: Option<Mutex<Rfc2217Session>>,
}

impl NetClient {
    /// Serial data out to the client (IAC-escaped for RFC 2217). Past 1 MB queued (a
    /// client that stopped reading, or asked us to pause), data is dropped and counted.
    pub fn send_data(&self, b: &[u8]) {
        if b.is_empty() {
            return;
        }
        let b = if self.session.is_some() { Bytes::from(escape_iac(b)) } else { Bytes::copy_from_slice(b) };
        if self.pending.load(Ordering::Relaxed) + b.len() > NET_PENDING_MAX {
            self.dropped.fetch_add(b.len(), Ordering::Relaxed);
            return;
        }
        self.pending.fetch_add(b.len(), Ordering::Relaxed);
        let _ = self.data.send(b);
    }

    fn send_ctrl(&self, b: Vec<u8>) {
        let _ = self.ctrl.send(b);
    }

    /// Flush what's queued for the client and close the connection (`socket.end()`).
    pub fn end(&self) {
        self.closing.send_replace(true);
    }

    fn suspend(&self, on: bool) {
        self.suspended.store(on, Ordering::SeqCst);
        self.wake.notify_one();
    }
}

/// Listen on [::]:port dual-stack, else 0.0.0.0 (but not when the port is simply taken).
fn bind(port: u16) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let v6 = (|| -> std::io::Result<Socket> {
        let s = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
        s.set_only_v6(false)?;
        s.set_reuse_address(true)?;
        s.bind(&SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0)).into())?;
        Ok(s)
    })();
    let s = match v6 {
        Ok(s) => s,
        Err(e) if e.raw_os_error() == Some(libc::EADDRINUSE) => return Err(e),
        Err(_) => {
            let s = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
            s.set_reuse_address(true)?;
            s.bind(&SocketAddr::from(([0, 0, 0, 0], port)).into())?;
            s
        }
    };
    s.listen(64)?;
    s.set_nonblocking(true)?;
    Ok(s.into())
}

/// The port as a session sees it. Each call takes the port lock briefly.
struct Hooks<'a> {
    inner: &'a Arc<Inner>,
    port: &'a Arc<Port>,
    client: &'a NetClient,
}

impl PortHooks for Hooks<'_> {
    fn config(&self) -> SerialConfig {
        self.port.lock().cfg
    }
    // Remote changes are live but not saved as the port's default.
    fn set_config(&mut self, c: &Value) -> Result<SerialConfig, String> {
        self.inner.apply_config(self.port, c, false).map_err(|e| e.message)
    }
    fn lines(&self) -> Option<ModemLines> {
        self.port.lock().lines
    }
    fn set_lines(&mut self, dtr: Option<bool>, rts: Option<bool>) {
        if let Err(e) = self.inner.set_lines(self.port, dtr, rts) {
            self.inner.log(&format!("{}: rfc2217: {}", self.port.id, e.message));
        }
    }
    fn break_state(&self) -> bool {
        self.port.lock().brk
    }
    fn set_break(&mut self, on: bool) {
        let mut st = self.port.lock();
        if let Err(e) = ensure_writable(&st) {
            drop(st);
            self.inner.log(&format!("{}: rfc2217: {}", self.port.id, e.message));
            return;
        }
        if let Some(fd) = st.fd()
            && let Err(e) = T::set_break(fd, on)
        {
            drop(st);
            self.inner.log(&format!("{}: rfc2217: {e}", self.port.id));
            return;
        }
        st.brk = on;
    }
    fn purge(&mut self, which: Queue) {
        let mut st = self.port.lock();
        if let Some(fd) = st.fd() {
            let _ = T::flush(fd, which);
        }
        if which != Queue::Rx {
            st.txq.clear();
            st.txq_bytes = 0;
        }
    }
    fn suspend(&mut self, on: bool) {
        self.client.suspend(on);
    }
    fn log(&mut self, msg: &str) {
        self.inner.log(&format!("{}: rfc2217 {}: {msg}", self.port.id, self.client.remote));
    }
}

impl Inner {
    pub(crate) fn rfc_notify_modem(self: &Arc<Self>, p: &Arc<Port>, c: &Arc<NetClient>) {
        let Some(s) = &c.session else { return };
        let sent = {
            let mut s = s.lock().unwrap();
            let h = Hooks { inner: self, port: p, client: c };
            s.notify_modem(false, &h);
            s.take_sent()
        };
        for b in sent {
            c.send_ctrl(b);
        }
    }

    /// The listening port: an explicit (persisted) one, else base + the device's slot.
    pub(crate) fn bridge_port(&self, id: &str, kind: BridgeKind) -> u16 {
        let off = self.offset as u32;
        let explicit = self.port_settings(id).get("bridgePorts").and_then(|b| b.get(kind.as_str())).and_then(Value::as_u64).filter(|n| *n != 0);
        if let Some(e) = explicit {
            return (e as u32 + off) as u16;
        }
        let base = match kind {
            BridgeKind::Tcp => self.setting_num("tcpBase", 4000.0),
            BridgeKind::Rfc2217 => self.setting_num("rfc2217Base", 4100.0),
        };
        (base as u32 + self.slot_for(id) + off) as u16
    }

    pub(crate) fn start_bridge(self: &Arc<Self>, p: &Arc<Port>, st: &mut PortState, kind: BridgeKind) {
        if st.bridge(kind).listener.is_some() {
            return;
        }
        let port = self.bridge_port(&p.id, kind);
        st.bridge_mut(kind).port = port;
        let l = bind(port).and_then(TcpListener::from_std);
        match l {
            Ok(l) => {
                let (me, pp) = (self.clone(), p.clone());
                let h = tokio::spawn(async move {
                    loop {
                        match l.accept().await {
                            Ok((s, peer)) => me.clone().net_client(pp.clone(), kind, s, peer),
                            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                        }
                    }
                });
                let br = st.bridge_mut(kind);
                br.listener = Some(h);
                br.error.clear();
                self.log(&format!("{}: {} bridge on :{port}", p.id, kind.as_str()));
            }
            Err(e) => {
                let br = st.bridge_mut(kind);
                br.listener = None;
                br.error = format!("port {port}: {}", T::io_code(&e));
                let msg = format!("{}: {} bridge failed: {}", p.id, kind.as_str(), br.error);
                self.log(&msg);
            }
        }
    }

    pub(crate) fn stop_bridge(&self, st: &mut PortState, kind: BridgeKind) {
        let clients = std::mem::take(&mut st.bridge_mut(kind).clients);
        for c in clients {
            st.leases = st.leases.saturating_sub(1);
            c.end();
        }
        if let Some(h) = st.bridge_mut(kind).listener.take() {
            h.abort();
        }
    }

    pub(crate) fn sync_bridges(self: &Arc<Self>, p: &Arc<Port>, st: &mut PortState) {
        let want = self.port_settings(&p.id).get("bridges").cloned().unwrap_or(Value::Null);
        for kind in BridgeKind::ALL {
            if want.get(kind.as_str()).is_some_and(benchbob_core::http::truthy) && st.present {
                self.start_bridge(p, st, kind);
            } else {
                self.stop_bridge(st, kind);
            }
        }
        if st.leases == 0 && !st.want && !self.open_on_plug(&p.id) && st.open() && st.state == "open" {
            self.close_fd_locked(p, st, "closed");
        }
    }

    fn net_client(self: Arc<Self>, p: Arc<Port>, kind: BridgeKind, s: TcpStream, peer: SocketAddr) {
        let _ = s.set_nodelay(true);
        let (ctrl, ctrl_rx) = mpsc::unbounded_channel();
        let (data, data_rx) = mpsc::unbounded_channel();
        let c = Arc::new(NetClient {
            id: NEXT_CLIENT.fetch_add(1, Ordering::Relaxed),
            kind,
            remote: peer.ip().to_string(),
            ctrl,
            data,
            pending: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            suspended: AtomicBool::new(false),
            wake: Notify::new(),
            closing: watch::channel(false).0,
            session: (kind == BridgeKind::Rfc2217).then(|| Mutex::new(Rfc2217Session::new())),
        });
        if let Some(sess) = &c.session {
            let mut s = sess.lock().unwrap();
            s.start();
            for b in s.take_sent() {
                c.send_ctrl(b);
            }
        }
        let (rd, wr) = s.into_split();
        tokio::spawn(writer(c.clone(), wr, ctrl_rx, data_rx));
        {
            let mut st = p.lock();
            st.bridge_mut(kind).clients.push(c.clone());
            st.leases += 1;
            if let Err(e) = self.ensure_open_locked(&p, &mut st) {
                c.send_ctrl(format!("BenchBob: cannot open {}: {}\r\n", st.info.path, e.message).into_bytes());
                c.end();
            }
            self.log(&format!("{}: {} client {}", p.id, kind.as_str(), c.remote));
            self.notify(&p, &st);
        }
        tokio::spawn(self.reader(p, c, rd));
    }

    async fn reader(self: Arc<Self>, p: Arc<Port>, c: Arc<NetClient>, mut rd: tokio::net::tcp::OwnedReadHalf) {
        let mut buf = vec![0u8; 16 * 1024];
        let mut closing = c.closing.subscribe();
        loop {
            let n = tokio::select! {
                n = rd.read(&mut buf) => match n { Ok(0) | Err(_) => break, Ok(n) => n },
                _ = until_closed(&mut closing) => break,
            };
            let d = match &c.session {
                Some(sess) => {
                    let (d, sent) = {
                        let mut s = sess.lock().unwrap();
                        let mut h = Hooks { inner: &self, port: &p, client: &c };
                        let d = s.feed(&buf[..n], &mut h);
                        (d, s.take_sent())
                    };
                    for b in sent {
                        c.send_ctrl(b);
                    }
                    d
                }
                None => buf[..n].to_vec(),
            };
            let mut st = p.lock();
            // Locked to another writer: network clients can't hold the lock.
            if d.is_empty() || st.info.console || st.state == "flashing" || st.lock.is_some() || !st.open() {
                continue;
            }
            let _ = self.write_locked(&p, &mut st, d, None);
        }
        c.end();
        let mut st = p.lock();
        let br = st.bridge_mut(c.kind);
        let Some(i) = br.clients.iter().position(|x| x.id == c.id) else { return };
        br.clients.remove(i);
        self.release(&p, &mut st);
        self.notify(&p, &st);
    }

    pub(crate) fn set_bridge(self: &Arc<Self>, p: &Arc<Port>, kind: Option<BridgeKind>, enabled: bool, port: Option<&Value>, host: &str) -> Result<Value, HttpError> {
        let Some(kind) = kind else { return Err(HttpError::bad_request("kind must be \"tcp\" or \"rfc2217\"")) };
        let mut st = p.lock();
        if let Some(v) = port.filter(|v| !v.is_null()) {
            let n = benchbob_core::http::js_number(v);
            if !(n.fract() == 0.0 && (1024.0..=65535.0).contains(&n)) {
                return Err(HttpError::bad_request("port must be 1024-65535"));
            }
            let mut cur = self.port_settings(&p.id).get("bridgePorts").cloned().filter(Value::is_object).unwrap_or(json!({}));
            cur[kind.as_str()] = json!(n as i64 - self.offset as i64);
            self.save_port_settings(&p.id, &[("bridgePorts", cur)]);
            self.stop_bridge(&mut st, kind);
        }
        let mut cur = self.port_settings(&p.id).get("bridges").cloned().filter(Value::is_object).unwrap_or(json!({}));
        cur[kind.as_str()] = json!(enabled);
        self.save_port_settings(&p.id, &[("bridges", cur)]);
        self.sync_bridges(p, &mut st);
        self.notify(p, &st);
        let err = st.bridge(kind).error.clone();
        if enabled && !err.is_empty() {
            return Err(http(409, format!("Bridge failed: {err}")));
        }
        Ok(self.bridge_status(p, &st, kind, host))
    }

    pub(crate) fn bridge_status(&self, p: &Port, st: &PortState, kind: BridgeKind, host: &str) -> Value {
        let br = st.bridge(kind);
        let port = if br.port != 0 { br.port } else { self.bridge_port(&p.id, kind) };
        let enabled = self.port_settings(&p.id).get("bridges").and_then(|b| b.get(kind.as_str())).is_some_and(benchbob_core::http::truthy);
        let baud = st.cfg.baud;
        let commands = match kind {
            BridgeKind::Tcp => vec![
                format!("nc {host} {port}"),
                format!("socat -,raw,echo=0 TCP:{host}:{port}"),
                format!("socat pty,link=/tmp/ttyBenchBob,raw,echo=0 TCP:{host}:{port}"),
            ],
            BridgeKind::Rfc2217 => vec![
                format!("python -m serial.tools.miniterm rfc2217://{host}:{port} {baud}"),
                format!("esptool --port 'rfc2217://{host}:{port}?ign_set_control' flash_id"),
                format!("pio device monitor --port rfc2217://{host}:{port} -b {baud}"),
            ],
        };
        json!({
            "enabled": enabled,
            "port": port,
            "listening": br.listener.is_some(),
            "clients": br.clients.iter().map(|c| c.remote.clone()).collect::<Vec<_>>(),
            "error": br.error,
            "commands": commands,
        })
    }
}

async fn writer(c: Arc<NetClient>, mut wr: tokio::net::tcp::OwnedWriteHalf, mut ctrl: mpsc::UnboundedReceiver<Vec<u8>>, mut data: mpsc::UnboundedReceiver<Bytes>) {
    let mut closing = c.closing.subscribe();
    loop {
        let paused = c.suspended.load(Ordering::SeqCst);
        tokio::select! {
            biased;
            b = ctrl.recv() => match b {
                Some(b) => if wr.write_all(&b).await.is_err() { break },
                None => break,
            },
            b = data.recv(), if !paused => match b {
                Some(b) => {
                    c.pending.fetch_sub(b.len(), Ordering::Relaxed);
                    if wr.write_all(&b).await.is_err() { break }
                }
                None => break,
            },
            _ = c.wake.notified() => {}
            _ = until_closed(&mut closing) => {
                // Flush what's queued, then close.
                while let Ok(b) = ctrl.try_recv() {
                    let _ = wr.write_all(&b).await;
                }
                if !c.suspended.load(Ordering::SeqCst) {
                    while let Ok(b) = data.try_recv() {
                        let _ = wr.write_all(&b).await;
                    }
                }
                break;
            },
        }
    }
    let _ = wr.shutdown().await;
    // Stop the reader too, if the client went away first.
    c.closing.send_replace(true);
}

async fn until_closed(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|v| *v).await;
}
