//! Hardware-independent parts of the BenchBob node firmware, shared by every board and
//! tested on the host with `cargo test`:
//!
//! - [`api`]: REST routes and the MCP endpoint, over a [`api::Node`] trait the boards implement
//! - [`json`]: zero-allocation JSON reader/writer
//! - [`http`]: request-head parsing and response framing
//! - [`improv`]: Improv Wi-Fi serial packets
//! - [`config`]: the Wi-Fi/hostname record kept in the reserved flash region
//! - [`uf2`]: UF2 blocks, for the Pico `config.uf2`
//! - [`mdns`]: a minimal mDNS responder (hostname and `_benchbob._tcp` service)

#![no_std]

pub mod api;
pub mod config;
pub mod http;
pub mod improv;
pub mod json;
pub mod mdns;
pub mod uf2;

/// Firmware version, from this crate's Cargo.toml (all firmware crates share it).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
