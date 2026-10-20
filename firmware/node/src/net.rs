//! Network state shared between the board's Wi-Fi task (which writes it) and the HTTP,
//! mDNS and Improv tasks (which read it).

use benchbob_proto::api::WifiState;
use benchbob_proto::config::{FixedStr, MAX_HOSTNAME, MAX_PASSWORD, MAX_SSID};
use core::cell::RefCell;
use core::fmt::Write;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

#[derive(Clone, Copy)]
pub struct NetState {
    pub hostname: FixedStr<MAX_HOSTNAME>,
    pub ssid: FixedStr<MAX_SSID>,
    pub state: WifiState,
    pub rssi: Option<i16>,
    pub ip: Option<[u8; 4]>,
    pub mac: [u8; 6],
}

static NET: Mutex<CriticalSectionRawMutex, RefCell<NetState>> = Mutex::new(RefCell::new(NetState {
    hostname: FixedStr::new(),
    ssid: FixedStr::new(),
    state: WifiState::Unprovisioned,
    rssi: None,
    ip: None,
    mac: [0; 6],
}));

/// Raised when the address or hostname changes (mDNS re-announces).
pub static NET_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

pub fn get() -> NetState {
    NET.lock(|n| *n.borrow())
}

pub fn update(f: impl FnOnce(&mut NetState)) {
    let changed = NET.lock(|n| {
        let mut n = n.borrow_mut();
        let before = (n.ip, n.hostname);
        f(&mut n);
        before != (n.ip, n.hostname)
    });
    if changed {
        NET_CHANGED.signal(());
    }
}

/// `benchbob-` and the last three bytes of the MAC, e.g. `benchbob-0a1b2c`.
pub fn default_hostname(mac: [u8; 6]) -> FixedStr<MAX_HOSTNAME> {
    let mut b = [0u8; 15];
    let mut w = Cursor { b: &mut b, n: 0 };
    let _ = write!(w, "benchbob-{:02x}{:02x}{:02x}", mac[3], mac[4], mac[5]);
    let n = w.n;
    FixedStr::from_bytes(&b[..n]).unwrap_or_default()
}

struct Cursor<'a> {
    b: &'a mut [u8],
    n: usize,
}

impl Write for Cursor<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let s = s.as_bytes();
        if self.n + s.len() > self.b.len() {
            return Err(core::fmt::Error);
        }
        self.b[self.n..self.n + s.len()].copy_from_slice(s);
        self.n += s.len();
        Ok(())
    }
}

/// Wi-Fi credentials handed from provisioning to the board's Wi-Fi task.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Credentials {
    pub ssid: FixedStr<MAX_SSID>,
    pub password: FixedStr<MAX_PASSWORD>,
}
