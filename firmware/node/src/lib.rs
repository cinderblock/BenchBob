//! Embassy glue shared by the BenchBob node boards. Board crates provide the hardware
//! (a [`benchbob_proto::api::Node`] impl, a flash driver, a serial port, a Wi-Fi task)
//! and spawn these:
//!
//! - [`http::serve`]: the REST API and MCP endpoint on port 80
//! - [`mdns::run`]: `<hostname>.local` and `_benchbob._tcp` discovery
//! - [`improv::run`]: Improv Wi-Fi provisioning over the board's serial port
//!
//! [`store::ConfigStore`] keeps the Wi-Fi credentials and hostname in flash; [`net`] is
//! the network state the board's Wi-Fi task publishes; [`wifi`] carries provisioning
//! requests to that task.

#![no_std]

#[macro_use]
mod fmt;

pub mod http;
pub mod improv;
pub mod mdns;
pub mod net;
pub mod store;
pub mod wifi;

/// A small inline string for formatting (IP addresses, numbers).
pub struct StrBuf<const N: usize> {
    b: [u8; N],
    n: usize,
}

impl<const N: usize> StrBuf<N> {
    pub const fn new() -> Self {
        StrBuf { b: [0; N], n: 0 }
    }
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.b[..self.n]).unwrap_or("")
    }
}

impl<const N: usize> Default for StrBuf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> core::fmt::Write for StrBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let s = s.as_bytes();
        if self.n + s.len() > N {
            return Err(core::fmt::Error);
        }
        self.b[self.n..self.n + s.len()].copy_from_slice(s);
        self.n += s.len();
        Ok(())
    }
}
