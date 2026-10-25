//! Network input for LED strings: sACN (UDP 5568, plus the multicast group of each
//! universe a string listens on), Art-Net (6454, with ArtPollReply so consoles find the
//! node) and DDP (4048). Every packet goes to `Api::leds.receive`; the board's LED task
//! renders what changed and calls `tick`, which hands a quiet string back to its pattern.
//!
//! The sockets are small (one packet each in flight); multicast groups are what the
//! stack has room for after mDNS, so a node joins at most [`MAX_GROUPS`] universes' groups
//! (unicast sACN works for any number).

use crate::StrBuf;
use crate::http::Shared;
use crate::net;
use benchbob_pixels::net::{self as pnet, ARTNET_PORT, DDP_PORT, NodeInfo, SACN_PORT};
use benchbob_proto::api::Node;
use core::fmt::Write;
use embassy_futures::select::{Either4, select4};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, Ipv4Address, Stack};
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Timer};

/// sACN multicast groups a node keeps joined (smoltcp's table also holds mDNS's).
pub const MAX_GROUPS: usize = 3;
const PKT: usize = 1472;

pub struct Buffers {
    meta: [[PacketMetadata; 2]; 6],
    sacn_rx: [u8; PKT],
    art_rx: [u8; 1024],
    art_tx: [u8; 512],
    ddp_rx: [u8; PKT],
    tiny: [[u8; 16]; 2],
    pkt: [u8; PKT],
}

impl Buffers {
    pub const fn new() -> Self {
        Buffers {
            meta: [[PacketMetadata::EMPTY; 2]; 6],
            sacn_rx: [0; PKT],
            art_rx: [0; 1024],
            art_tx: [0; 512],
            ddp_rx: [0; PKT],
            tiny: [[0; 16]; 2],
            pkt: [0; PKT],
        }
    }
}

impl Default for Buffers {
    fn default() -> Self {
        Self::new()
    }
}

fn group(u: u16) -> Ipv4Address {
    let g = pnet::sacn_multicast(u);
    Ipv4Address::new(g[0], g[1], g[2], g[3])
}

pub async fn run<M: RawMutex, N: Node>(stack: Stack<'_>, shared: &Mutex<M, Shared<N>>, bufs: &mut Buffers) -> ! {
    let Buffers { meta, sacn_rx, art_rx, art_tx, ddp_rx, tiny, pkt } = bufs;
    let [m0, m1, m2, m3, m4, m5] = meta;
    let [t0, t1] = tiny;
    stack.wait_config_up().await;
    let mut sacn = UdpSocket::new(stack, m0, sacn_rx, m1, t0);
    let mut art = UdpSocket::new(stack, m2, art_rx, m3, art_tx);
    let mut ddp = UdpSocket::new(stack, m4, ddp_rx, m5, t1);
    for (s, port) in [(&mut sacn, SACN_PORT), (&mut art, ARTNET_PORT), (&mut ddp, DDP_PORT)] {
        if s.bind(port).is_err() {
            warn!("lights: could not bind UDP {}", port);
        }
    }
    info!("lights: listening for sACN, Art-Net and DDP");
    let mut joined: [Option<u16>; MAX_GROUPS] = [None; MAX_GROUPS];
    // One receive at a time keeps a single packet buffer: wait for whichever socket has
    // something, then read it.
    loop {
        let ready =
            select4(sacn.wait_recv_ready(), art.wait_recv_ready(), ddp.wait_recv_ready(), Timer::after(Duration::from_secs(1))).await;
        let (sock, port) = match ready {
            Either4::First(()) => (&mut sacn, SACN_PORT),
            Either4::Second(()) => (&mut art, ARTNET_PORT),
            Either4::Third(()) => (&mut ddp, DDP_PORT),
            Either4::Fourth(()) => {
                sync_groups(stack, shared, &mut joined).await;
                continue;
            }
        };
        let Ok((n, meta)) = sock.recv_from(pkt).await else { continue };
        let pkt = &pkt[..n];
        if port == ARTNET_PORT && pnet::is_artpoll(pkt) {
            let mut u = [0u16; 8];
            let k = shared.lock().await.api.leds.artnet_universes(&mut u);
            if k > 0
                && let Some(ip) = stack.config_v4().map(|c| c.address.address().octets())
            {
                let st = net::get();
                let mut long = StrBuf::<64>::new();
                let _ = write!(long, "BenchBob {} LED strings", st.hostname.as_str());
                let node = NodeInfo { ip, mac: st.mac, short_name: st.hostname.as_str(), long_name: long.as_str() };
                let to = IpEndpoint::new(meta.endpoint.addr, ARTNET_PORT);
                let mut replies: [[u8; pnet::ARTPOLL_REPLY_LEN]; 2] = [[0; pnet::ARTPOLL_REPLY_LEN]; 2];
                let mut count = 0;
                pnet::artpoll_reply(&node, &u[..k], |r| {
                    if count < 2 {
                        replies[count] = *r;
                        count += 1;
                    }
                });
                for r in &replies[..count] {
                    let _ = art.send_to(r, to).await;
                }
            }
            continue;
        }
        let mut from = StrBuf::<16>::new();
        let IpAddress::Ipv4(a) = meta.endpoint.addr;
        let o = a.octets();
        let _ = write!(from, "{}.{}.{}.{}", o[0], o[1], o[2], o[3]);
        let now = Instant::now().as_millis();
        shared.lock().await.api.leds.receive(port, pkt, from.as_str(), now);
    }
}

/// Join the groups of the sACN universes strings listen on now (and leave the rest).
async fn sync_groups<M: RawMutex, N: Node>(stack: Stack<'_>, shared: &Mutex<M, Shared<N>>, joined: &mut [Option<u16>; MAX_GROUPS]) {
    let mut want = [0u16; MAX_GROUPS];
    let n = shared.lock().await.api.leds.sacn_universes(&mut want);
    let want = &want[..n];
    for slot in joined.iter_mut() {
        if let Some(u) = *slot
            && !want.contains(&u)
        {
            let _ = stack.leave_multicast_group(group(u));
            *slot = None;
        }
    }
    for &u in want {
        if joined.contains(&Some(u)) {
            continue;
        }
        let Some(slot) = joined.iter_mut().find(|s| s.is_none()) else { break };
        match stack.join_multicast_group(group(u)) {
            Ok(()) => *slot = Some(u),
            Err(_) => warn!("lights: no room to join sACN universe {}'s group (unicast still works)", u),
        }
    }
}
