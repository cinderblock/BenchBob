//! Requests from provisioning to the board's Wi-Fi task, and its answers. Each board
//! implements the task against its own radio driver (cyw43, esp-radio).

use crate::net::Credentials;
use benchbob_proto::config::{FixedStr, MAX_SSID};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;

pub enum Request {
    /// Join this network (leaving any current one). Answered with Connected or Failed.
    Connect(Credentials),
    /// Scan. Answered with zero or more Network, then ScanDone.
    Scan,
}

pub enum Response {
    Connected([u8; 4]),
    Failed,
    Network { ssid: FixedStr<MAX_SSID>, rssi: i16, secured: bool },
    ScanDone,
}

pub static REQUESTS: Channel<CriticalSectionRawMutex, Request, 1> = Channel::new();
pub static RESPONSES: Channel<CriticalSectionRawMutex, Response, 4> = Channel::new();

/// How long the Wi-Fi task gives a join plus DHCP before reporting Failed.
pub const CONNECT_TIMEOUT_SECS: u64 = 25;
