//! Improv Wi-Fi over a serial port (USB CDC on the Pico, USB-Serial-JTAG on the ESP32-C6).
//! Works with any Improv serial client, e.g. https://www.improv-wifi.com/ in Chrome/Edge.

use crate::net::{self, Credentials};
use crate::store::ConfigStore;
use crate::wifi::{self, CONNECT_TIMEOUT_SECS, Request, Response};
use crate::StrBuf;
use benchbob_proto::api::WifiState;
use benchbob_proto::config::{self, Config, FixedStr};
use benchbob_proto::improv::{self, Command, ErrorCode, Packet, ParseError, State};
use core::fmt::Write as _;
use embassy_time::{Duration, with_timeout};
use embedded_io_async::{Read, Write};
use embedded_storage::nor_flash::NorFlash;

pub struct DeviceInfo {
    /// e.g. "BenchBob node"
    pub firmware: &'static str,
    pub version: &'static str,
    /// e.g. "RP2350"
    pub chip: &'static str,
}

struct Session<'a, W, F> {
    tx: &'a mut W,
    store: &'a mut ConfigStore<F>,
    dev: &'a DeviceInfo,
    error: ErrorCode,
    out: [u8; improv::MAX_PACKET],
}

/// Serve Improv on one connection until the port reports an error (e.g. USB unplugged).
pub async fn run<R: Read, W: Write, F: NorFlash>(rx: &mut R, tx: &mut W, store: &mut ConfigStore<F>, dev: &DeviceInfo) {
    let mut parser = improv::Parser::new();
    let mut s = Session { tx, store, dev, error: ErrorCode::None, out: [0; improv::MAX_PACKET] };
    let mut buf = [0u8; 64];
    loop {
        let n = match rx.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            let Some(p) = parser.push(b) else { continue };
            let r = match p {
                Ok(Packet::Rpc(cmd)) => s.command(cmd).await,
                Ok(Packet::Other(_)) | Err(ParseError::Version) => Ok(()),
                Err(ParseError::Checksum | ParseError::InvalidRpc) => s.set_error(ErrorCode::InvalidRpc).await,
            };
            if r.is_err() {
                return;
            }
        }
    }
}

type Io = Result<(), ()>;

fn api_url(ip: [u8; 4]) -> StrBuf<32> {
    let mut u = StrBuf::new();
    let _ = write!(u, "http://{}.{}.{}.{}/api", ip[0], ip[1], ip[2], ip[3]);
    u
}

impl<W: Write, F: NorFlash> Session<'_, W, F> {
    async fn send(&mut self, n: usize) -> Io {
        self.tx.write_all(&self.out[..n]).await.map_err(|_| ())?;
        self.tx.flush().await.map_err(|_| ())
    }

    async fn state(&mut self, st: State) -> Io {
        let n = improv::encode_state(st, &mut self.out);
        self.send(n).await
    }

    async fn set_error(&mut self, e: ErrorCode) -> Io {
        self.error = e;
        let n = improv::encode_error(e, &mut self.out);
        self.send(n).await
    }

    async fn result(&mut self, cmd: u8, strings: &[&str]) -> Io {
        let n = improv::encode_result(cmd, strings, &mut self.out);
        self.send(n).await
    }

    fn hostname(&self) -> FixedStr<{ config::MAX_HOSTNAME }> {
        net::get().hostname
    }

    async fn command(&mut self, cmd: Command<'_>) -> Io {
        if self.error != ErrorCode::None {
            self.set_error(ErrorCode::None).await?;
        }
        let n = net::get();
        let online = n.state == WifiState::Connected && n.ip.is_some();
        match cmd {
            Command::CurrentState => {
                if online {
                    self.state(State::Provisioned).await?;
                    let url = api_url(n.ip.unwrap_or_default());
                    self.result(improv::CMD_CURRENT_STATE, &[url.as_str()]).await
                } else {
                    self.state(State::Authorized).await
                }
            }
            Command::DeviceInfo => {
                let h = self.hostname();
                let (fw, ver, chip) = (self.dev.firmware, self.dev.version, self.dev.chip);
                self.result(improv::CMD_DEVICE_INFO, &[fw, ver, chip, h.as_str()]).await
            }
            Command::WifiSettings { ssid, password } => self.provision(ssid, password).await,
            Command::Scan => self.scan().await,
            Command::Hostname(None) | Command::DeviceName(None) => {
                let h = self.hostname();
                self.result(cmd.id(), &[h.as_str()]).await
            }
            Command::Hostname(Some(h)) => self.set_hostname(h).await,
            Command::NetworkState => {
                let mut flags = StrBuf::<4>::new();
                let _ = write!(flags, "{}", 0x02 | online as u8);
                if online {
                    let url = api_url(n.ip.unwrap_or_default());
                    self.result(improv::CMD_NETWORK_STATE, &[flags.as_str(), url.as_str()]).await
                } else {
                    self.result(improv::CMD_NETWORK_STATE, &[flags.as_str()]).await
                }
            }
            Command::DeviceName(Some(_)) | Command::Unknown(_) => self.set_error(ErrorCode::UnknownRpc).await,
        }
    }

    async fn provision(&mut self, ssid: &[u8], password: &[u8]) -> Io {
        let (Some(ssid), Some(password)) = (FixedStr::from_bytes(ssid), FixedStr::from_bytes(password)) else {
            return self.set_error(ErrorCode::InvalidRpc).await;
        };
        if ssid.is_empty() {
            return self.set_error(ErrorCode::InvalidRpc).await;
        }
        info!("improv: provisioning ssid {}", ssid.as_str());
        self.state(State::Provisioning).await?;
        while wifi::RESPONSES.try_receive().is_ok() {}
        wifi::REQUESTS.send(Request::Connect(Credentials { ssid, password })).await;
        let wait = async {
            loop {
                match wifi::RESPONSES.receive().await {
                    Response::Connected(ip) => return Some(ip),
                    Response::Failed => return None,
                    _ => {}
                }
            }
        };
        let ip = with_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS + 5), wait).await.ok().flatten();
        let Some(ip) = ip else {
            warn!("improv: could not connect");
            self.set_error(ErrorCode::UnableToConnect).await?;
            return self.state(State::Authorized).await;
        };
        let mut c = self.store.config().copied().unwrap_or_default();
        c.ssid = ssid;
        c.password = password;
        if self.store.save(&c).is_err() {
            // Connected now, but it won't survive a reset: say so.
            self.set_error(ErrorCode::Unknown).await?;
        }
        self.state(State::Provisioned).await?;
        let url = api_url(ip);
        self.result(improv::CMD_WIFI_SETTINGS, &[url.as_str()]).await
    }

    async fn scan(&mut self) -> Io {
        while wifi::RESPONSES.try_receive().is_ok() {}
        wifi::REQUESTS.send(Request::Scan).await;
        loop {
            match with_timeout(Duration::from_secs(20), wifi::RESPONSES.receive()).await {
                Ok(Response::Network { ssid, rssi, secured }) => {
                    let mut r = StrBuf::<8>::new();
                    let _ = write!(r, "{}", rssi);
                    self.result(improv::CMD_SCAN, &[ssid.as_str(), r.as_str(), if secured { "YES" } else { "NO" }]).await?;
                }
                Ok(Response::ScanDone) => return self.result(improv::CMD_SCAN, &[]).await,
                Ok(_) => {}
                Err(_) => return self.set_error(ErrorCode::Unknown).await,
            }
        }
    }

    async fn set_hostname(&mut self, h: &[u8]) -> Io {
        let Some(h) = core::str::from_utf8(h).ok().filter(|h| config::valid_hostname(h)).and_then(FixedStr::from_text) else {
            return self.set_error(ErrorCode::BadHostname).await;
        };
        let mut c = self.store.config().copied().unwrap_or_default();
        c.hostname = h;
        if self.store.save(&c).is_err() {
            return self.set_error(ErrorCode::Unknown).await;
        }
        // mDNS picks it up now; the DHCP hostname changes at the next reset.
        net::update(|n| n.hostname = h);
        self.result(improv::CMD_HOSTNAME, &[h.as_str()]).await
    }
}

/// Credentials from a stored config, if it has any.
pub fn credentials(c: &Config) -> Option<Credentials> {
    c.provisioned().then_some(Credentials { ssid: c.ssid, password: c.password })
}
