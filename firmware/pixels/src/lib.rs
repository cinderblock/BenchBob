//! Addressable LED strings, independent of the hardware that drives them. Shared by the
//! Linux server (`backend/crates/leds`) and the node firmware, and tested on the host
//! with `cargo test`:
//!
//! - [`color`]: pixels, colour order (RGB permutations, RGBW), gamma, brightness, power limit
//! - [`timing`]: one-wire chip timings (WS2812B, SK6812…), and fitting them to an SPI
//!   clock or a PIO program
//! - [`encode`]: pixels to wire bytes: WS2812-over-SPI bit streams, APA102/SK9822 and
//!   WS2801 frames
//! - [`pattern`]: built-in patterns as functions of time and pixel index
//! - [`net`]: sACN (E1.31), Art-Net and DDP packets, source arbitration, universe mapping
//!
//! Nothing here allocates: callers own every buffer.

#![no_std]

pub mod color;
pub mod encode;
pub mod net;
pub mod pattern;
pub mod timing;

pub use color::{Order, Rgbw};
pub use timing::Timing;
