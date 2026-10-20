//! mDNS over embassy-net: answers for `<hostname>.local` and the `_benchbob._tcp` /
//! `_http._tcp` services (see `benchbob_proto::mdns`), and announces after each address
//! or hostname change. The board's Wi-Fi driver must pass 01:00:5e:00:00:fb through.

use crate::net::{self, NET_CHANGED};
use benchbob_proto::mdns::{self, Service};
use embassy_futures::select::{Either, select};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, Ipv4Address, Stack};
use embassy_time::{Duration, Timer};

/// The multicast MAC for 224.0.0.251, for drivers that filter multicast.
pub const MDNS_MAC: [u8; 6] = [0x01, 0x00, 0x5e, 0x00, 0x00, 0xfb];

pub struct Buffers {
    rx_meta: [PacketMetadata; 4],
    tx_meta: [PacketMetadata; 4],
    rx: [u8; 1536],
    tx: [u8; 1536],
    pkt: [u8; 1536],
    out: [u8; 1024],
}

impl Buffers {
    pub const fn new() -> Self {
        Buffers { rx_meta: [PacketMetadata::EMPTY; 4], tx_meta: [PacketMetadata::EMPTY; 4], rx: [0; 1536], tx: [0; 1536], pkt: [0; 1536], out: [0; 1024] }
    }
}

impl Default for Buffers {
    fn default() -> Self {
        Self::new()
    }
}

/// `txt`: the `_benchbob._tcp` TXT strings, e.g. `["board=pico2w", "version=0.1.0", …]`.
pub async fn run(stack: Stack<'_>, txt: &[&str], bufs: &mut Buffers) -> ! {
    let Buffers { rx_meta, tx_meta, rx, tx, pkt, out } = bufs;
    stack.wait_config_up().await;
    let group = Ipv4Address::new(mdns::GROUP[0], mdns::GROUP[1], mdns::GROUP[2], mdns::GROUP[3]);
    if stack.join_multicast_group(group).is_err() {
        warn!("mdns: could not join 224.0.0.251");
    }
    let mut sock = UdpSocket::new(stack, rx_meta, rx, tx_meta, tx);
    if sock.bind(mdns::PORT).is_err() {
        warn!("mdns: could not bind 5353");
    }
    let to_group = IpEndpoint::new(IpAddress::Ipv4(group), mdns::PORT);
    let mut announce_left = 2u8;
    info!("mdns: running");
    loop {
        let wait = if announce_left > 0 { Duration::from_millis(1000) } else { Duration::from_secs(3600) };
        match select(sock.recv_from(pkt), select(Timer::after(wait), NET_CHANGED.wait())).await {
            Either::First(Ok((n, meta))) => {
                let Some(ip) = current_ip(stack) else { continue };
                let n_state = net::get();
                let svc = Service { hostname: n_state.hostname.as_str(), ip, port: 80, txt };
                let legacy = meta.endpoint.port != mdns::PORT;
                if let Some(len) = mdns::respond(&pkt[..n], legacy, &svc, out) {
                    let dest = if legacy { meta.endpoint } else { to_group };
                    let _ = sock.send_to(&out[..len], dest).await;
                }
            }
            Either::First(Err(_)) => {}
            Either::Second(Either::First(())) => {
                if announce_left > 0 {
                    announce_left -= 1;
                    if let Some(ip) = current_ip(stack) {
                        let n_state = net::get();
                        let svc = Service { hostname: n_state.hostname.as_str(), ip, port: 80, txt };
                        if let Some(len) = mdns::announce(&svc, out) {
                            let _ = sock.send_to(&out[..len], to_group).await;
                        }
                    }
                }
            }
            Either::Second(Either::Second(())) => {
                // Address or hostname changed: (re)join and announce twice more.
                let _ = stack.join_multicast_group(group);
                announce_left = 2;
            }
        }
    }
}

fn current_ip(stack: Stack<'_>) -> Option<[u8; 4]> {
    stack.config_v4().map(|c| c.address.address().octets())
}
