//! The network receivers: sACN (UDP 5568, multicast per universe), Art-Net (6454) and
//! DDP (4048), on a thread of their own. A port is only bound while some string listens
//! on its protocol, so BenchBob doesn't sit on 6454 for nothing (and another program can
//! have it). Packets go to the shared string model ([`benchbob_proto::api::Leds::receive`]);
//! ArtPolls get an ArtPollReply naming the port-addresses strings listen on.

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use benchbob_pixels::net::{self as pnet, ARTNET_PORT, DDP_PORT, NodeInfo, SACN_PORT};
use serde_json::{Value, json};
use socket2::{Domain, Protocol, Socket, Type};

use crate::Inner;

const PROTOCOLS: [(u8, u16, &str); 3] = [(1, SACN_PORT, "sacn"), (2, ARTNET_PORT, "artnet"), (4, DDP_PORT, "ddp")];

/// What the receivers are doing, for `capabilities.receivers`.
#[derive(Default, Clone)]
pub struct RxStatus {
    /// Per protocol name: listening, or why not.
    pub ports: Vec<(&'static str, Result<(), String>)>,
    pub joined: Vec<u16>,
}

impl RxStatus {
    pub fn to_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        for (name, r) in &self.ports {
            m.insert((*name).into(), json!({ "listening": r.is_ok(), "error": r.as_ref().err() }));
        }
        m.insert("sacnGroups".into(), json!(self.joined));
        Value::Object(m)
    }
}

fn bind(port: u16) -> Result<UdpSocket, String> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(|e| e.to_string())?;
    // Other listeners (another sACN receiver on the same box) can share the port.
    s.set_reuse_address(true).map_err(|e| e.to_string())?;
    s.set_broadcast(true).map_err(|e| e.to_string())?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into()).map_err(|e| format!("UDP port {port}: {e}"))?;
    s.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok(s.into())
}

/// The machine's IPv4 addresses (not loopback), for multicast joins on every interface.
fn ipv4_addrs() -> Vec<Ipv4Addr> {
    let mut v = Vec::new();
    let mut ifs: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a list we walk and free.
    if unsafe { libc::getifaddrs(&mut ifs) } != 0 {
        return v;
    }
    let mut p = ifs;
    while !p.is_null() {
        // SAFETY: p is a node of the list getifaddrs returned.
        let a = unsafe { &*p };
        if !a.ifa_addr.is_null() && unsafe { (*a.ifa_addr).sa_family } as i32 == libc::AF_INET {
            // SAFETY: an AF_INET address is a sockaddr_in.
            let sin = unsafe { &*(a.ifa_addr as *const libc::sockaddr_in) };
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            if !ip.is_loopback() && !v.contains(&ip) {
                v.push(ip);
            }
        }
        p = a.ifa_next;
    }
    // SAFETY: freeing what getifaddrs gave us.
    unsafe { libc::freeifaddrs(ifs) };
    v
}

/// The local address a reply to `to` would come from.
fn local_ip_toward(to: SocketAddr) -> Option<Ipv4Addr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect(to).ok()?;
    match s.local_addr().ok()? {
        SocketAddr::V4(a) => Some(*a.ip()),
        SocketAddr::V6(_) => None,
    }
}

pub(crate) fn run(inner: Arc<Inner>) {
    let epoch = inner.epoch.load(Ordering::SeqCst);
    let mut socks: [Option<UdpSocket>; 3] = [None, None, None];
    let mut errors: [Option<String>; 3] = [None, None, None];
    let mut joined: BTreeSet<u16> = BTreeSet::new();
    let mut addrs = ipv4_addrs();
    let mut refreshed = std::time::Instant::now();
    let mut buf = vec![0u8; 2048];
    while inner.epoch.load(Ordering::SeqCst) == epoch {
        // Which ports and groups the strings want now.
        let (want, mut universes) = {
            let m = inner.model.lock().unwrap();
            let mut u = [0u16; 64];
            let n = m.sacn_universes(&mut u);
            (m.protocols(), u[..n].to_vec())
        };
        universes.sort_unstable();
        if refreshed.elapsed().as_secs() >= 30 {
            addrs = ipv4_addrs(); // interfaces come and go
            refreshed = std::time::Instant::now();
            joined.clear(); // rejoin everywhere (joins on an interface that's still there just repeat)
        }
        for (k, (bit, port, name)) in PROTOCOLS.iter().enumerate() {
            let on = want & bit != 0;
            match (&socks[k], on) {
                (None, true) => match bind(*port) {
                    Ok(s) => {
                        tracing::info!("[leds] listening for {name} on UDP {port}");
                        socks[k] = Some(s);
                        errors[k] = None;
                        if k == 0 {
                            joined.clear();
                        }
                    }
                    Err(e) => {
                        if errors[k].as_deref() != Some(&e) {
                            tracing::warn!("[leds] {name}: {e}");
                        }
                        errors[k] = Some(e);
                    }
                },
                (Some(_), false) => {
                    socks[k] = None;
                    errors[k] = None;
                    if k == 0 {
                        joined.clear();
                    }
                }
                _ => {}
            }
        }
        // sACN multicast groups, on every interface.
        if let Some(s) = &socks[0] {
            let want: BTreeSet<u16> = universes.iter().copied().collect();
            let sock = socket2::SockRef::from(s);
            for &u in want.difference(&joined.clone()) {
                let g = Ipv4Addr::from(pnet::sacn_multicast(u));
                for a in &addrs {
                    let _ = sock.join_multicast_v4(&g, a);
                }
                joined.insert(u);
            }
            for &u in joined.clone().difference(&want) {
                let g = Ipv4Addr::from(pnet::sacn_multicast(u));
                for a in &addrs {
                    let _ = sock.leave_multicast_v4(&g, a);
                }
                joined.remove(&u);
            }
        }
        *inner.rx.lock().unwrap() = RxStatus {
            ports: PROTOCOLS
                .iter()
                .enumerate()
                .filter(|(_, (bit, ..))| want & bit != 0)
                .map(|(k, (_, _, name))| (*name, errors[k].clone().map_or(Ok(()), Err)))
                .collect(),
            joined: joined.iter().copied().collect(),
        };

        // Wait for packets on whichever sockets are open (100 ms, then re-check the config).
        let mut fds: Vec<libc::pollfd> =
            socks.iter().flatten().map(|s| libc::pollfd { fd: s.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
        if fds.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(200));
            continue;
        }
        // SAFETY: polling our own sockets.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 100) };
        if n <= 0 {
            continue;
        }
        for (k, s) in socks.iter().enumerate() {
            let Some(s) = s else { continue };
            let port = PROTOCOLS[k].1;
            // Drain everything waiting: a show controller sends bursts of universes.
            while let Ok((len, from)) = s.recv_from(&mut buf) {
                let pkt = &buf[..len];
                if port == ARTNET_PORT && pnet::is_artpoll(pkt) {
                    reply_artpoll(&inner, s, from);
                    continue;
                }
                let who = from.ip().to_string();
                let now = inner.now();
                // The worker renders what changed on its next frame; the dashboard polls.
                inner.model.lock().unwrap().receive(port, pkt, &who, now);
            }
        }
    }
}

fn reply_artpoll(inner: &Inner, s: &UdpSocket, from: SocketAddr) {
    let universes = {
        let m = inner.model.lock().unwrap();
        let mut u = [0u16; 32];
        let n = m.artnet_universes(&mut u);
        u[..n].to_vec()
    };
    if universes.is_empty() {
        return;
    }
    let ip = local_ip_toward(from).unwrap_or(Ipv4Addr::UNSPECIFIED);
    let host = benchbob_core::util::hostname();
    let long = format!("BenchBob {host} LED strips");
    let node = NodeInfo { ip: ip.octets(), mac: [0; 6], short_name: &host, long_name: &long };
    // Art-Net 4: reply to the controller directly, on the Art-Net port.
    let to = match from {
        SocketAddr::V4(a) => SocketAddr::V4(SocketAddrV4::new(*a.ip(), ARTNET_PORT)),
        other => other,
    };
    pnet::artpoll_reply(&node, &universes, |r| {
        let _ = s.send_to(r, to);
    });
}
