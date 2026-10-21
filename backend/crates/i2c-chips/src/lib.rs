//! I²C chip identification and decoding, ported from `peripherals/lib/i2c-chips.ts`.
//!
//! A scan only says "something ACKed at 0x76". This crate turns that into "BME280,
//! 23.4 °C". It is `no_std` (with `alloc`) so BenchBob's firmware nodes can use the same
//! table; the server crate (`benchbob-i2c`) supplies the bus and turns results into JSON.
//!
//! Safety model (the reason the code is ordered the way it is):
//!  - Identification never writes data. The only bytes sent are register pointers and
//!    read-type commands (e.g. SHT3x "read status").
//!  - A register pointer is itself a command on some chips: on a PCF8574 it sets the output
//!    pins, on a TCA9548A it selects mux channels, on an MCP4725 it starts a DAC write, on an
//!    HTU21D 0xFE is soft reset. So at addresses where such chips live, probes start with
//!    *pure reads* (no write phase at all) and only move on to pointer probes once those
//!    rule the command chip out. [`chips::CHIPS`] is tried in order per address; the order
//!    is the safety mechanism, so read the comments before reordering.
//!  - `read` decodes using reads only and is what auto-polling calls. `measure` is allowed
//!    to write (start a conversion, wake a sleeping chip) and only runs on an explicit
//!    Measure or the per-device auto-measure opt-in. Where `measure` wakes a chip it
//!    restores the previous power state afterwards.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod chips;
pub mod decode;
pub mod dev;
pub mod fake;
pub mod fixtures;
pub mod io;
pub mod num;
pub mod time;
pub mod types;

pub use chips::{CHIPS, ChipDef, Ident, ProbeCtx, address_hints, chip_by_id, cross_hints, identify};
pub use dev::Dev;
pub use io::{I2cError, I2cIo, Msg, func, func_names, hex};
pub use types::*;
