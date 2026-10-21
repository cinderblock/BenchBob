//! The socketcand-protocol TCP server (python-can `interface="socketcand"`, SavvyCAN, Kayak)
//! and its UDP discovery beacon, as can.ts runs them.
//!
//! Each client gets its own raw socket on the bus it opens (no FD, no own frames; other
//! local senders, our hub included, are seen through the kernel's loopback). Writes go
//! through a per-client queue; past 1 MB of unsent text, frames are dropped and counted.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use benchbob_core::http::{js_number, truthy};
use benchbob_core::run::{Opts, read_text, run};
use benchbob_core::timeline::now_ms;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::frame::{self, CanFrame};
use crate::json::O;
use crate::sock::{CanSocket, Options};
use crate::{BEACON_PORT, Inner, MIN_PERIOD_MS, SOCKETCAND_PORT, Task, ip_bin};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    NoBus,
    Bcm,
    Raw,
    Control,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::NoBus => "nobus",
            Mode::Bcm => "bcm",
            Mode::Raw => "raw",
            Mode::Control => "control",
        }
    }
}

struct Sub {
    throttle_ms: f64,
    last: u64,
    mask: Option<Vec<u8>>,
    last_masked: Option<String>,
}

enum Out {
    Text(String),
    End,
}

struct Writer {
    tx: mpsc::UnboundedSender<Out>,
    pending: Arc<AtomicUsize>,
}

pub(crate) struct Client {
    n: u64,
    peer: String,
    mode: Mode,
    iface: String,
    sock: Option<CanSocket>,
    hold_until: u64,
    held: String,
    subs: HashMap<String, Sub>,
    jobs: HashMap<String, (Task, Arc<Mutex<CanFrame>>)>,
    stat_task: Option<Task>,
    rx: u64,
    tx: u64,
    dropped: u64,
    since: u64,
    out: Writer,
    ended: bool,
}

type ClientRef = Arc<Mutex<Client>>;

#[derive(Default)]
pub(crate) struct Sc {
    server: Option<Task>,
    error: String,
    seq: u64,
    clients: Vec<ClientRef>,
    beacon: Option<Task>,
    beacon_to: String,
}

fn write(c: &mut Client, s: &str) {
    if c.out.pending.load(Ordering::Relaxed) > 1 << 20 {
        c.dropped += 1;
        return;
    }
    c.out.pending.fetch_add(s.len(), Ordering::Relaxed);
    let _ = c.out.tx.send(Out::Text(s.to_string()));
}

fn end(c: &mut Client) {
    c.ended = true;
    let _ = c.out.tx.send(Out::End);
}

fn on_frames(cl: &ClientRef, frames: Vec<CanFrame>) {
    let mut g = cl.lock().unwrap();
    let c = &mut *g;
    let mut out = String::new();
    let now = now_ms();
    for f in &frames {
        if c.mode == Mode::Raw {
            if f.err {
                out += &frame::sc_error(f);
            } else if !f.rtr {
                out += &frame::sc_frame_raw(f); // socketcand doesn't forward RTR in raw mode
            }
            c.rx += 1;
        } else if c.mode == Mode::Bcm && !f.err {
            let Some(sub) = c.subs.get_mut(&frame::frame_key(f.id, f.ext)) else { continue };
            if let Some(mask) = &sub.mask {
                let masked: Vec<u8> = f.data.iter().enumerate().map(|(i, b)| b & mask.get(i).copied().unwrap_or(0)).collect();
                let m = frame::to_hex(&masked);
                if sub.last_masked.as_deref() == Some(m.as_str()) {
                    continue;
                }
                sub.last_masked = Some(m);
            }
            if sub.throttle_ms != 0.0 && !sub.throttle_ms.is_nan() && ((now - sub.last) as f64) < sub.throttle_ms {
                continue;
            }
            sub.last = now;
            out += &frame::sc_frame_bcm(f);
            c.rx += 1;
        }
    }
    if out.is_empty() {
        return;
    }
    // python-can reads "< ok >" with an exact-match recv(), so frames that arrive right after
    // the rawmode reply are held back briefly (not dropped) to keep them out of its segment.
    if now < c.hold_until || !c.held.is_empty() {
        // (While a flush is pending, later frames queue behind the held ones to keep order.)
        if c.held.is_empty() {
            let w = Arc::downgrade(cl);
            let wait = c.hold_until.saturating_sub(now);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(wait)).await;
                if let Some(cl) = w.upgrade() {
                    let mut c = cl.lock().unwrap();
                    let h = std::mem::take(&mut c.held);
                    if !h.is_empty() && !c.ended {
                        write(&mut c, &h);
                    }
                }
            });
        }
        c.held += &out;
    } else {
        write(c, &out);
    }
}

fn id_key(tok: Option<&String>) -> Result<String, String> {
    let tok = tok.ok_or("Cannot read properties of undefined (reading 'length')")?;
    let id = frame::to_uint32(frame::parse_int_hex(tok).unwrap_or(0.0)) & frame::CAN_EFF_MASK;
    Ok(frame::frame_key(id, tok.len() == 8 || id > frame::CAN_SFF_MASK))
}

/// `Number(w)` of a protocol word; NaN when missing.
fn wnum(w: &[String], i: usize) -> f64 {
    w.get(i).map(|s| js_number(&Value::from(s.as_str()))).unwrap_or(f64::NAN)
}

fn send_frame(c: &mut Client, f: &CanFrame) -> Result<(), String> {
    match &c.sock {
        Some(s) => s.send(f).map(|_| c.tx += 1),
        None => Ok(()),
    }
}

impl Inner {
    fn sc_port(&self) -> u16 {
        let p = self.setting("socketcandPort").map(|v| js_number(&v)).filter(|p| p.is_finite()).unwrap_or(SOCKETCAND_PORT as f64);
        (p as u32 + benchbob_core::paths::port_offset() as u32) as u16
    }

    pub(crate) fn socketcand_out(&self) -> Value {
        let sc = self.sc.lock().unwrap();
        let clients: Vec<Value> = sc
            .clients
            .iter()
            .map(|c| {
                let c = c.lock().unwrap();
                O::new()
                    .k("n", c.n)
                    .k("peer", c.peer.clone())
                    .k("iface", c.iface.clone())
                    .k("mode", c.mode.as_str())
                    .k("rx", c.rx)
                    .k("tx", c.tx)
                    .k("dropped", c.dropped)
                    .k("since", c.since)
                    .v()
            })
            .collect();
        O::new()
            .k("enabled", self.setting("socketcand").unwrap_or(Value::Bool(true)))
            .k("port", self.sc_port())
            .k("listening", sc.server.is_some())
            .k("error", sc.error.clone())
            .k("errMask", self.setting("socketcandErrMask").unwrap_or(Value::from(0)))
            .k("beacon", self.setting("beacon").unwrap_or(Value::Bool(false)))
            .k("beaconTo", sc.beacon_to.clone())
            .k("clients", clients)
            .v()
    }

    pub(crate) async fn start_socketcand(self: &Arc<Self>) {
        self.stop_socketcand().await;
        if !self.setting("socketcand").is_none_or(|v| truthy(&v)) {
            return;
        }
        let port = self.sc_port();
        let l = std::net::TcpListener::bind(("0.0.0.0", port)).and_then(|l| {
            l.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(l)
        });
        let mut sc = self.sc.lock().unwrap();
        match l {
            Ok(l) => {
                let w = Arc::downgrade(self);
                sc.server = Some(Task(tokio::spawn(accept_loop(w, l))));
                sc.error.clear();
            }
            Err(e) => {
                sc.server = None;
                sc.error = format!(
                    "port {port}: {}",
                    if e.kind() == std::io::ErrorKind::AddrInUse { "already in use".to_string() } else { e.to_string() }
                );
                self.ctx().log(&format!("socketcand: {}", sc.error));
            }
        }
        drop(sc);
        self.ctx().changed();
    }

    /// Close the listener (waiting until its port is free again) and end every client.
    pub(crate) async fn stop_socketcand(&self) {
        let (server, clients) = {
            let mut sc = self.sc.lock().unwrap();
            (sc.server.take(), std::mem::take(&mut sc.clients))
        };
        if let Some(mut t) = server {
            t.0.abort();
            let _ = (&mut t.0).await;
        }
        for c in clients {
            cleanup(&mut c.lock().unwrap());
            end(&mut c.lock().unwrap());
        }
    }

    fn sc_remove(&self, cl: &ClientRef) {
        let removed = {
            let mut sc = self.sc.lock().unwrap();
            let n = sc.clients.len();
            sc.clients.retain(|c| !Arc::ptr_eq(c, cl));
            n != sc.clients.len()
        };
        if removed {
            cleanup(&mut cl.lock().unwrap());
            if let Some(c) = self.ctx.get() {
                c.changed();
            }
        }
    }

    async fn sc_command(self: &Arc<Self>, cl: &ClientRef, cmd: &str) -> Result<(), String> {
        let w = frame::sc_words(cmd);
        let verb = w.first().map(String::as_str).unwrap_or("");
        let mode = cl.lock().unwrap().mode;
        if mode == Mode::NoBus {
            if verb == "open"
                && let Some(name) = w.get(1)
            {
                let ifc = self.st.lock().unwrap().ifaces.get(name).cloned();
                let Some(ifc) = ifc else {
                    let mut c = cl.lock().unwrap();
                    write(&mut c, "< error could not open bus >");
                    end(&mut c);
                    return Ok(());
                };
                // Like socketcand's canctl_start_iface: a down bus with a saved bitrate (or a vcan) is brought up.
                let saved = self.saved_cfg(&ifc.key);
                if !ifc.up && (ifc.kind != "can" || saved.as_ref().and_then(|s| s.get("bitrate")).is_some_and(truthy)) {
                    let mut raw = saved.unwrap_or_else(|| Value::Object(Default::default()));
                    raw["up"] = Value::Bool(true);
                    let _ = crate::ops::configure_boxed(self.clone(), name.clone(), raw, false).await;
                }
                let err_mask = self.setting("socketcandErrMask").map(|v| frame::to_uint32(js_number(&v))).unwrap_or(0);
                let (w1, w2) = (Arc::downgrade(cl), Arc::downgrade(cl));
                let s = CanSocket::open(
                    name,
                    &Options { err_mask, ..Default::default() },
                    Box::new(move |f| {
                        if let Some(cl) = w1.upgrade() {
                            on_frames(&cl, f);
                        }
                    }),
                    Some(Box::new(move |_| {
                        if let Some(cl) = w2.upgrade() {
                            end(&mut cl.lock().unwrap());
                        }
                    })),
                );
                let mut c = cl.lock().unwrap();
                match s {
                    Ok(s) => {
                        c.sock = Some(s);
                        c.iface = name.clone();
                        c.mode = Mode::Bcm;
                        write(&mut c, "< ok >");
                    }
                    Err(_) => {
                        write(&mut c, "< error could not open bus >");
                        end(&mut c);
                        return Ok(());
                    }
                }
                drop(c);
                self.ctx().changed();
            } else if w.len() >= 2 && (w[1] == "B" || w[1] == "C") {
                write(&mut cl.lock().unwrap(), "< error configuration operation not supported >");
            } else {
                write(&mut cl.lock().unwrap(), "< error unknown command >");
            }
            return Ok(());
        }
        let mut g = cl.lock().unwrap();
        let c = &mut *g;
        if matches!(verb, "rawmode" | "bcmmode" | "controlmode") {
            c.mode = match verb {
                "rawmode" => Mode::Raw,
                "bcmmode" => Mode::Bcm,
                _ => Mode::Control,
            };
            if c.mode != Mode::Control {
                c.stat_task = None;
            }
            c.hold_until = now_ms() + 50;
            write(c, "< ok >");
            drop(g);
            self.ctx().changed();
            return Ok(());
        }
        match verb {
            "isotpmode" => {
                write(c, "< error isotp mode not supported >");
                return Ok(());
            }
            "echo" => {
                write(c, "< echo >");
                return Ok(());
            }
            "send" if c.mode != Mode::Control => {
                // socketcand ignores bad sends silently; an "< error … >" line would be misread as
                // an error frame by python-can's parser.
                return frame::sc_parse_id_len_data(&w[1..]).and_then(|f| send_frame(c, &f));
            }
            _ => {}
        }
        if c.mode == Mode::Bcm {
            let r: Result<(), String> = (|| {
                match verb {
                    "add" => {
                        // < add SEC USEC ID LEN DATA… >
                        let period = wnum(&w, 1) * 1000.0 + wnum(&w, 2) / 1000.0;
                        if period.is_nan() || period < 1.0 {
                            return Err("period too short".into());
                        }
                        let f = frame::sc_parse_id_len_data(w.get(3..).unwrap_or(&[]))?;
                        let k = frame::frame_key(f.id, f.ext);
                        let shared = Arc::new(Mutex::new(f));
                        let (w2, s2) = (Arc::downgrade(cl), shared.clone());
                        let p = Duration::from_micros((period.max(MIN_PERIOD_MS) * 1000.0) as u64);
                        let task = Task(tokio::spawn(async move {
                            let mut t = tokio::time::interval_at(tokio::time::Instant::now() + p, p);
                            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                            loop {
                                t.tick().await;
                                let Some(cl) = w2.upgrade() else { return };
                                let f = s2.lock().unwrap().clone();
                                let _ = send_frame(&mut cl.lock().unwrap(), &f);
                            }
                        }));
                        c.jobs.insert(k, (task, shared));
                    }
                    "update" => {
                        let f = frame::sc_parse_id_len_data(&w[1..])?;
                        if let Some((_, j)) = c.jobs.get(&frame::frame_key(f.id, f.ext)) {
                            *j.lock().unwrap() = f.clone();
                        }
                        send_frame(c, &f)?;
                    }
                    "delete" => {
                        let k = id_key(w.get(1))?;
                        c.jobs.remove(&k);
                    }
                    "subscribe" => {
                        // < subscribe SEC USEC ID >
                        let k = id_key(w.get(3))?;
                        c.subs.insert(
                            k,
                            Sub { throttle_ms: wnum(&w, 1) * 1000.0 + wnum(&w, 2) / 1000.0, last: 0, mask: None, last_masked: None },
                        );
                    }
                    "unsubscribe" => {
                        let k = id_key(w.get(1))?;
                        c.subs.remove(&k);
                    }
                    "filter" => {
                        // < filter SEC USEC ID LEN MASK… >: deliver when the masked content changes.
                        let f = frame::sc_parse_id_len_data(w.get(3..).unwrap_or(&[]))?;
                        let k = id_key(w.get(3))?;
                        c.subs.insert(
                            k,
                            Sub {
                                throttle_ms: wnum(&w, 1) * 1000.0 + wnum(&w, 2) / 1000.0,
                                last: 0,
                                mask: Some(f.data),
                                last_masked: None,
                            },
                        );
                    }
                    "muxfilter" => write(c, "< error muxfilter not supported >"),
                    _ => write(c, "< error unknown command >"),
                }
                Ok(())
            })();
            return r;
        }
        if c.mode == Mode::Control && verb == "statistics" {
            let ival = wnum(&w, 1);
            let ival = if ival.is_nan() { 0.0 } else { ival };
            c.stat_task = None;
            if ival > 0.0 {
                let (w2, iface) = (Arc::downgrade(cl), c.iface.clone());
                let p = Duration::from_millis(ival.max(100.0) as u64);
                c.stat_task = Some(Task(tokio::spawn(async move {
                    let mut t = tokio::time::interval_at(tokio::time::Instant::now() + p, p);
                    loop {
                        t.tick().await;
                        let st = |k: &str| {
                            let v = read_text(format!("/sys/class/net/{iface}/statistics/{k}"));
                            let n = js_number(&Value::from(v));
                            if n.is_nan() { 0 } else { n as u64 }
                        };
                        let line = format!("< stat {} {} {} {} >", st("rx_bytes"), st("rx_packets"), st("tx_bytes"), st("tx_packets"));
                        let Some(cl) = w2.upgrade() else { return };
                        write(&mut cl.lock().unwrap(), &line);
                    }
                })));
            }
            return Ok(());
        }
        write(c, "< error unknown command >");
        Ok(())
    }

    // ---------- discovery beacon (UDP 42000) ----------

    pub(crate) async fn start_beacon(self: &Arc<Self>, epoch: u64) {
        self.stop_beacon();
        if !self.setting("beacon").is_some_and(|v| truthy(&v)) {
            return;
        }
        let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| {
            s.set_broadcast(true)?;
            s.set_nonblocking(true)?;
            tokio::net::UdpSocket::from_std(s)
        }) else {
            self.ctx().log("socketcand beacon: can't open a UDP socket");
            return;
        };
        let w: Weak<Inner> = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            let mut target = beacon_target().await;
            let mut n = 0u64;
            let mut t = tokio::time::interval(Duration::from_secs(3)); // socketcand's interval; python-can waits 3.1 s
            loop {
                t.tick().await;
                let Some(me) = w.upgrade() else { return };
                if me.epoch.load(Ordering::SeqCst) != epoch {
                    return;
                }
                n += 1;
                if n.is_multiple_of(20) {
                    target = beacon_target().await.or(target); // follow DHCP changes
                }
                let Some((ip, bcast)) = target.clone() else { continue };
                if me.sc.lock().unwrap().server.is_none() {
                    continue;
                }
                me.sc.lock().unwrap().beacon_to = format!("{bcast}:{BEACON_PORT}");
                let buses: Vec<String> = me.st.lock().unwrap().ifaces.keys().cloned().collect();
                let xml = frame::beacon_xml(&crate::hostname(), &ip, me.sc_port(), &buses);
                let _ = sock.send_to(xml.as_bytes(), (bcast.as_str(), BEACON_PORT)).await;
            }
        });
        self.sc.lock().unwrap().beacon = Some(Task(task));
    }

    pub(crate) fn stop_beacon(&self) {
        let mut sc = self.sc.lock().unwrap();
        sc.beacon = None;
        sc.beacon_to.clear();
    }
}

fn cleanup(c: &mut Client) {
    c.sock = None;
    c.jobs.clear();
    c.stat_task = None;
}

async fn beacon_target() -> Option<(String, String)> {
    let r = run(&[ip_bin(), "-j", "route", "get", "1.1.1.1"], Opts::default()).await;
    let dev = serde_json::from_str::<Value>(&r.out).ok()?.get(0)?.get("dev")?.as_str()?.to_string();
    let r = run(&[ip_bin(), "-j", "-4", "addr", "show", "dev", &dev], Opts::default()).await;
    let v: Value = serde_json::from_str(&r.out).ok()?;
    let a = v.get(0)?.get("addr_info")?.as_array()?.iter().find(|a| a["family"] == "inet")?.clone();
    let ip = a.get("local")?.as_str()?.to_string();
    let bcast = a.get("broadcast").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("255.255.255.255").to_string();
    Some((ip, bcast))
}

async fn accept_loop(w: Weak<Inner>, l: tokio::net::TcpListener) {
    loop {
        let Ok((s, peer)) = l.accept().await else { continue };
        let Some(me) = w.upgrade() else { return };
        let _ = s.set_nodelay(true);
        tokio::spawn(serve(me, s, peer.ip().to_string()));
    }
}

async fn serve(me: Arc<Inner>, s: tokio::net::TcpStream, peer: String) {
    let (mut rd, mut wr) = s.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    let pending = Arc::new(AtomicUsize::new(0));
    let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
    let p2 = pending.clone();
    tokio::spawn(async move {
        while let Some(o) = rx.recv().await {
            match o {
                Out::Text(t) => {
                    let ok = wr.write_all(t.as_bytes()).await.is_ok();
                    p2.fetch_sub(t.len(), Ordering::Relaxed);
                    if !ok {
                        break;
                    }
                }
                Out::End => break,
            }
        }
        let _ = wr.shutdown().await;
        let _ = done_tx.send(true);
    });
    let n = {
        let mut sc = me.sc.lock().unwrap();
        sc.seq += 1;
        sc.seq
    };
    let cl: ClientRef = Arc::new(Mutex::new(Client {
        n,
        peer,
        mode: Mode::NoBus,
        iface: String::new(),
        sock: None,
        hold_until: 0,
        held: String::new(),
        subs: HashMap::new(),
        jobs: HashMap::new(),
        stat_task: None,
        rx: 0,
        tx: 0,
        dropped: 0,
        since: now_ms(),
        out: Writer { tx, pending },
        ended: false,
    }));
    me.sc.lock().unwrap().clients.push(cl.clone());
    write(&mut cl.lock().unwrap(), "< hi >");
    me.ctx().changed();
    let mut buf = String::new();
    let mut chunk = vec![0u8; 4096];
    loop {
        let r = tokio::select! {
            r = rd.read(&mut chunk) => r,
            _ = done_rx.changed() => break,
        };
        let n = match r {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        // latin1
        buf.extend(chunk[..n].iter().map(|b| *b as char));
        let (cmds, rest) = frame::sc_take(&buf);
        buf = rest;
        for cmd in cmds {
            if let Err(e) = me.sc_command(&cl, &cmd).await {
                let peer = cl.lock().unwrap().peer.clone();
                me.ctx().log(&format!("socketcand {peer}: {cmd}: {e}"));
            }
        }
        if cl.lock().unwrap().ended {
            // Let the writer flush what's queued, then close.
            let _ = tokio::time::timeout(Duration::from_secs(5), done_rx.wait_for(|d| *d)).await;
            break;
        }
    }
    me.sc_remove(&cl);
    end(&mut cl.lock().unwrap());
}
