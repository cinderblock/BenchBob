//! Wire encodings. Every function writes into a caller's buffer and returns the length
//! used, or `None` when the buffer is too small; the `*_len` functions size it.
//!
//! - [`wire`]: pixels → chip-ordered bytes (colour order, gamma, brightness). PIO and
//!   RMT drivers shift these out as they are.
//! - [`ws_spi`]: chip bytes → a WS2812 waveform for SPI MOSI ([`SpiFit`]).
//! - [`apa102`]: chip bytes → an APA102 / SK9822 / HD107S frame.
//! - WS2801 needs no framing: send the chip bytes, then hold the clock low ≥ 500 µs.

use crate::color::{Order, Rgbw};
use crate::timing::SpiFit;

/// How long a WS2801 needs its clock held low to latch.
pub const WS2801_LATCH_US: u32 = 500;

/// Chip bytes for `pixels`: `order.channels()` bytes each.
pub fn wire(pixels: &[Rgbw], order: Order, gamma: bool, brightness: u8, out: &mut [u8]) -> Option<usize> {
    let c = order.channels();
    let len = pixels.len() * c;
    let out = out.get_mut(..len)?;
    for (px, o) in pixels.iter().zip(out.chunks_exact_mut(c)) {
        order.write(*px, gamma, brightness, o);
    }
    Some(len)
}

pub fn ws_spi_len(fit: &SpiFit, data_bytes: usize, reset_us: u32) -> usize {
    // 8 data bits per byte, `bits` SPI bits each: `bits` SPI bytes per data byte.
    1 + data_bytes * fit.bits as usize + fit.reset_bytes(reset_us)
}

/// The SPI bytes for `data`: one leading zero byte (some controllers glitch MOSI on their
/// first bit, and the chips ignore a short low), each data bit MSB first as `fit.bits`
/// SPI bits, then zeros holding MOSI low for the reset time. A Linux driver can send this
/// in one transfer; a node's DMA likewise.
pub fn ws_spi(fit: &SpiFit, data: &[u8], reset_us: u32, out: &mut [u8]) -> Option<usize> {
    let len = ws_spi_len(fit, data.len(), reset_us);
    let out = out.get_mut(..len)?;
    out.fill(0);
    let mut bit = 8usize; // skip the leading zero byte
    for &byte in data {
        for i in (0..8).rev() {
            let ones = if byte >> i & 1 != 0 { fit.ones1 } else { fit.ones0 };
            for k in 0..fit.bits {
                if k < ones {
                    out[bit / 8] |= 0x80 >> (bit % 8);
                }
                bit += 1;
            }
        }
    }
    Some(len)
}

pub fn apa102_len(leds: usize) -> usize {
    4 + leds * 4 + 4 + leds.div_ceil(16)
}

/// An APA102-family frame for `data` (3 chip bytes per pixel; the family's native order
/// is BGR). `global` is the 5-bit per-pixel current setting, 0–31; brightness is better
/// done in the colour bytes (31 here), which keeps the chip's fast PWM.
///
/// Start frame: 32 zero bits. Each pixel: `0b111` + `global`, then its 3 bytes. Then 32
/// zero bits (the SK9822's reset frame) and n/2 more clock edges (zeros: the data is
/// delayed by half a clock per pixel), so the last pixel latches. Zeros instead of the
/// datasheet's ones work on both APA102 and SK9822.
pub fn apa102(data: &[u8], global: u8, out: &mut [u8]) -> Option<usize> {
    let leds = data.len() / 3;
    let len = apa102_len(leds);
    let out = out.get_mut(..len)?;
    out.fill(0);
    for (px, o) in data.as_chunks::<3>().0.iter().zip(out[4..].as_chunks_mut::<4>().0) {
        o[0] = 0xE0 | (global & 0x1F);
        o[1..].copy_from_slice(px);
    }
    Some(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timing::Timing;

    #[test]
    fn wire_bytes() {
        let px = [Rgbw::rgb(1, 2, 3), Rgbw::rgb(4, 5, 6)];
        let mut out = [0; 6];
        assert_eq!(wire(&px, Order::GRB, false, 255, &mut out), Some(6));
        assert_eq!(out, [2, 1, 3, 5, 4, 6]);
        assert_eq!(wire(&px, Order::GRB, false, 255, &mut [0; 5]), None);
    }

    #[test]
    fn ws_spi_golden() {
        // 3-bit encoding: 0 → 100, 1 → 110. 0xA5 = 1010 0101 →
        // 110 100 110 100 100 110 100 110 = 1101 0011 0100 1001 1010 0110 → D3 49 A6.
        let fit = SpiFit::find(&Timing::WS2812B, |hz| hz).unwrap();
        let mut out = [0xFFu8; 128];
        let n = ws_spi(&fit, &[0xA5], 300, &mut out).unwrap();
        assert_eq!(n, 1 + 3 + 90);
        assert_eq!(out[..4], [0x00, 0xD3, 0x49, 0xA6]);
        assert!(out[4..n].iter().all(|&b| b == 0));
        assert_eq!(ws_spi(&fit, &[0xA5], 300, &mut [0; 10]), None);
    }

    #[test]
    fn ws_spi_four_bit() {
        // A 4-bit fit (0 → 1000, 1 → 1100): 0xF0 → 1100×4 1000×4 = CC CC 88 88.
        let fit = SpiFit { clock_hz: 3_200_000, bits: 4, ones0: 1, ones1: 2, error_ns: 0 };
        let mut out = [0; 64];
        ws_spi(&fit, &[0xF0], 10, &mut out).unwrap();
        assert_eq!(out[..5], [0x00, 0xCC, 0xCC, 0x88, 0x88]);
    }

    #[test]
    fn apa102_frame() {
        let data = [3, 2, 1, 6, 5, 4];
        let mut out = [0xAAu8; 32];
        let n = apa102(&data, 31, &mut out).unwrap();
        assert_eq!(n, 4 + 8 + 4 + 1);
        assert_eq!(out[..n], [0, 0, 0, 0, 0xFF, 3, 2, 1, 0xFF, 6, 5, 4, 0, 0, 0, 0, 0]);
        assert_eq!(apa102_len(32), 4 + 128 + 4 + 2);
    }
}
