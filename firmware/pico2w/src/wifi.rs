//! The CYW43439: joining, re-joining, provisioning requests, scans, RSSI and the LED.
//!
//! LED (on the CYW43's GPIO 0): slow blink = no Wi-Fi configured (provision over USB),
//! fast blink = connecting, on = connected; on while someone holds the dashboard's
//! onboard-LED button ([`crate::board::LED_HOLD`]).

use crate::board::{LED_HOLD, LED_WAKE};
use benchbob_node::net::{self, Credentials};
use benchbob_node::wifi::{CONNECT_TIMEOUT_SECS, REQUESTS, RESPONSES, Request, Response};
use benchbob_proto::api::WifiState;
use benchbob_proto::config::{FixedStr, MAX_SSID};
use cyw43::{Control, JoinOptions, ScanOptions};
use defmt::{info, warn};
use embassy_futures::select::{Either3, select3};
use embassy_net::Stack;
use embassy_time::{Duration, Instant, Timer, with_timeout};

const TICK: Duration = Duration::from_millis(125);
const RETRY: Duration = Duration::from_secs(10);

/// Join and wait for DHCP. Some(ip) on success.
async fn connect(control: &mut Control<'static>, stack: Stack<'static>, c: &Credentials) -> Option<[u8; 4]> {
    net::update(|n| {
        n.state = WifiState::Connecting;
        n.ssid = c.ssid;
        n.ip = None;
        n.rssi = None;
    });
    let opts = if c.password.is_empty() { JoinOptions::new_open() } else { JoinOptions::new(c.password.as_bytes()) };
    let attempt = async {
        if let Err(e) = control.join(c.ssid.as_str(), opts).await {
            warn!("wifi: join {} failed: {}", c.ssid.as_str(), e);
            return None;
        }
        stack.wait_config_up().await;
        stack.config_v4().map(|cfg| cfg.address.address().octets())
    };
    let ip = with_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), attempt).await.ok().flatten();
    match ip {
        Some(ip) => {
            info!("wifi: connected to {}, ip {}.{}.{}.{}", c.ssid.as_str(), ip[0], ip[1], ip[2], ip[3]);
            let rssi = control.get_rssi().await;
            net::update(|n| {
                n.state = WifiState::Connected;
                n.ip = Some(ip);
                n.rssi = Some(rssi as i16);
            });
        }
        None => {
            control.leave().await;
            net::update(|n| n.state = WifiState::Connecting);
        }
    }
    ip
}

async fn scan(control: &mut Control<'static>) {
    let mut seen: [(FixedStr<MAX_SSID>, i16, bool); 16] = [(FixedStr::new(), i16::MIN, false); 16];
    let mut count = 0;
    {
        let mut scanner = control.scan(ScanOptions::default()).await;
        while let Some(bss) = scanner.next().await {
            let len = (bss.ssid_len as usize).min(32);
            let Some(ssid) = FixedStr::<MAX_SSID>::from_bytes(&bss.ssid[..len]) else { continue };
            if ssid.is_empty() {
                continue; // hidden network
            }
            let secured = bss.capability & 0x0010 != 0; // privacy bit
            match seen[..count].iter_mut().find(|(s, _, _)| *s == ssid) {
                Some(e) => e.1 = e.1.max(bss.rssi),
                None if count < 16 => {
                    seen[count] = (ssid, bss.rssi, secured);
                    count += 1;
                }
                None => {}
            }
        }
    }
    seen[..count].sort_unstable_by_key(|e| core::cmp::Reverse(e.1));
    for &(ssid, rssi, secured) in &seen[..count] {
        RESPONSES.send(Response::Network { ssid, rssi, secured }).await;
    }
    RESPONSES.send(Response::ScanDone).await;
}

#[embassy_executor::task]
pub async fn task(mut control: Control<'static>, stack: Stack<'static>, mut creds: Option<Credentials>) -> ! {
    let mut connected = false;
    let mut next_try = Instant::now();
    let mut tick: u32 = 0;
    let mut led = false;
    if creds.is_none() {
        net::update(|n| n.state = WifiState::Unprovisioned);
    }
    loop {
        match select3(REQUESTS.receive(), Timer::after(TICK), LED_WAKE.wait()).await {
            Either3::First(Request::Connect(c)) => {
                if connected {
                    control.leave().await;
                    connected = false;
                }
                match connect(&mut control, stack, &c).await {
                    Some(ip) => {
                        connected = true;
                        creds = Some(c);
                        RESPONSES.send(Response::Connected(ip)).await;
                    }
                    None => {
                        RESPONSES.send(Response::Failed).await;
                        // Go back to the saved network, if any.
                        next_try = Instant::now();
                        if creds.is_none() {
                            net::update(|n| n.state = WifiState::Unprovisioned);
                        }
                    }
                }
            }
            Either3::First(Request::Scan) => scan(&mut control).await,
            Either3::Third(()) => {}
            Either3::Second(()) => {
                tick = tick.wrapping_add(1);
                if connected && !stack.is_link_up() {
                    warn!("wifi: link lost");
                    connected = false;
                    next_try = Instant::now() + Duration::from_secs(1);
                    net::update(|n| {
                        n.state = WifiState::Connecting;
                        n.ip = None;
                    });
                }
                if let (Some(c), false) = (creds, connected)
                    && Instant::now() >= next_try
                {
                    connected = connect(&mut control, stack, &c).await.is_some();
                    next_try = Instant::now() + RETRY;
                }
                if connected && tick.is_multiple_of(40) {
                    let rssi = control.get_rssi().await;
                    net::update(|n| n.rssi = Some(rssi as i16));
                }
            }
        }
        // LED pattern, in 125 ms ticks, unless someone is holding it on.
        let want = LED_HOLD.held(Instant::now().as_millis())
            || match (creds.is_some(), connected) {
                (_, true) => true,
                (true, false) => tick.is_multiple_of(2),
                (false, false) => tick.is_multiple_of(8),
            };
        if want != led {
            control.gpio_set(0, want).await;
            led = want;
        }
    }
}
