//! esp-radio station: joining, re-joining, provisioning requests, scans and RSSI. The
//! status LED shows the state this keeps in `net` ([`crate::leds::status_task`]).

use alloc::string::String;
use benchbob_node::net::{self, Credentials};
use benchbob_node::wifi::{CONNECT_TIMEOUT_SECS, REQUESTS, RESPONSES, Request, Response};
use benchbob_proto::api::WifiState;
use benchbob_proto::config::{FixedStr, MAX_SSID};
use embassy_futures::select::{Either, select};
use embassy_net::Stack;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_radio::wifi::scan::ScanConfig;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{AuthenticationMethod, Config, WifiController};
use log::{info, warn};

const TICK: Duration = Duration::from_millis(250);
const RETRY: Duration = Duration::from_secs(10);

fn station(c: &Credentials) -> Config {
    let mut s = StationConfig::default().with_ssid(c.ssid.as_str()).with_password(String::from(c.password.as_str()));
    if c.password.is_empty() {
        s = s.with_auth_method(AuthenticationMethod::None);
    }
    Config::Station(s)
}

async fn connect(ctl: &mut WifiController<'static>, stack: Stack<'static>, c: &Credentials) -> Option<[u8; 4]> {
    net::update(|n| {
        n.state = WifiState::Connecting;
        n.ssid = c.ssid;
        n.ip = None;
        n.rssi = None;
    });
    if let Err(e) = ctl.set_config(&station(c)) {
        warn!("wifi: config rejected: {:?}", e);
        return None;
    }
    let attempt = async {
        if let Err(e) = ctl.connect_async().await {
            warn!("wifi: join {} failed: {:?}", c.ssid.as_str(), e);
            return None;
        }
        stack.wait_config_up().await;
        stack.config_v4().map(|cfg| cfg.address.address().octets())
    };
    let ip = with_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), attempt).await.ok().flatten();
    match ip {
        Some(ip) => {
            info!("wifi: connected to {}, ip {}.{}.{}.{}", c.ssid.as_str(), ip[0], ip[1], ip[2], ip[3]);
            let rssi = ctl.rssi().ok().map(|r| r as i16);
            net::update(|n| {
                n.state = WifiState::Connected;
                n.ip = Some(ip);
                n.rssi = rssi;
            });
        }
        None => {
            let _ = ctl.disconnect_async().await;
        }
    }
    ip
}

async fn scan(ctl: &mut WifiController<'static>) {
    match ctl.scan_async(&ScanConfig::default()).await {
        Ok(mut aps) => {
            aps.sort_unstable_by_key(|a| core::cmp::Reverse(a.signal_strength));
            let mut sent: heapless::Vec<FixedStr<MAX_SSID>, 16> = heapless::Vec::new();
            for ap in aps {
                let Some(ssid) = FixedStr::<MAX_SSID>::from_text(ap.ssid.as_str()) else { continue };
                if ssid.is_empty() || sent.contains(&ssid) {
                    continue;
                }
                let secured = !matches!(ap.auth_method, None | Some(AuthenticationMethod::None));
                RESPONSES.send(Response::Network { ssid, rssi: ap.signal_strength as i16, secured }).await;
                if sent.push(ssid).is_err() {
                    break;
                }
            }
        }
        Err(e) => warn!("wifi: scan failed: {:?}", e),
    }
    RESPONSES.send(Response::ScanDone).await;
}

#[embassy_executor::task]
pub async fn task(mut ctl: WifiController<'static>, stack: Stack<'static>, mut creds: Option<Credentials>) -> ! {
    let mut connected = false;
    let mut next_try = Instant::now();
    let mut tick: u32 = 0;
    if creds.is_none() {
        net::update(|n| n.state = WifiState::Unprovisioned);
    }
    loop {
        match select(REQUESTS.receive(), Timer::after(TICK)).await {
            Either::First(Request::Connect(c)) => {
                if connected {
                    let _ = ctl.disconnect_async().await;
                    connected = false;
                }
                match connect(&mut ctl, stack, &c).await {
                    Some(ip) => {
                        connected = true;
                        creds = Some(c);
                        RESPONSES.send(Response::Connected(ip)).await;
                    }
                    None => {
                        RESPONSES.send(Response::Failed).await;
                        next_try = Instant::now();
                        if creds.is_none() {
                            net::update(|n| n.state = WifiState::Unprovisioned);
                        }
                    }
                }
            }
            Either::First(Request::Scan) => scan(&mut ctl).await,
            Either::Second(()) => {
                tick = tick.wrapping_add(1);
                if connected && !ctl.is_connected() {
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
                    connected = connect(&mut ctl, stack, &c).await.is_some();
                    next_try = Instant::now() + RETRY;
                }
                if connected && tick.is_multiple_of(20) {
                    let rssi = ctl.rssi().ok().map(|r| r as i16);
                    net::update(|n| n.rssi = rssi);
                }
            }
        }
    }
}
