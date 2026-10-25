//! One-wire LED chip timing, and fitting it to the hardware that makes the waveform.
//!
//! Each data bit is a high pulse followed by a low one, `period_ns` long in total: short
//! high (`t0h_ns`) for a 0, long high (`t1h_ns`) for a 1. Holding the line low for
//! `reset_us` latches the frame. That is the whole "custom" surface: four numbers, plus
//! the presets below for the common chips.
//!
//! Datasheets allow ±150 ns on the high times ([`TOLERANCE_NS`]). Reset times are set
//! to 300 µs for every preset: newer WS2812B batches need 280 µs, older chips 50 µs, and
//! a longer reset only costs frame rate.

/// The high-time tolerance a fitted waveform must stay inside.
pub const TOLERANCE_NS: u32 = 150;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Timing {
    pub t0h_ns: u32,
    pub t1h_ns: u32,
    pub period_ns: u32,
    pub reset_us: u32,
}

/// Named presets, in the order the UI lists them.
pub const PRESETS: &[(&str, &str, Timing)] = &[
    ("ws2812b", "WS2812B / WS2812 / WS2811 at 800 kHz (most strips)", Timing { t0h_ns: 400, t1h_ns: 800, period_ns: 1250, reset_us: 300 }),
    ("sk6812", "SK6812 (RGB and RGBW)", Timing { t0h_ns: 300, t1h_ns: 600, period_ns: 1250, reset_us: 300 }),
    ("ws2813", "WS2813 / WS2815 (12 V, backup data line)", Timing { t0h_ns: 300, t1h_ns: 750, period_ns: 1250, reset_us: 300 }),
    ("ws2811-400k", "WS2811 in 400 kHz (slow) mode", Timing { t0h_ns: 500, t1h_ns: 1200, period_ns: 2500, reset_us: 300 }),
];

impl Timing {
    pub const WS2812B: Timing = PRESETS[0].2;

    pub fn preset(name: &str) -> Option<Timing> {
        PRESETS.iter().find(|p| p.0.eq_ignore_ascii_case(name)).map(|p| p.2)
    }

    /// Why this timing can't work, if it can't: each phase needs to exist, and the
    /// numbers have to be in the range any of these chips use.
    pub fn check(&self) -> Result<(), &'static str> {
        if self.t0h_ns == 0 || self.t0h_ns >= self.t1h_ns {
            return Err("t0h must be shorter than t1h, and above 0");
        }
        if self.t1h_ns >= self.period_ns {
            return Err("t1h must be shorter than the bit period");
        }
        if !(500..=10_000).contains(&self.period_ns) {
            return Err("bit period must be 500–10000 ns");
        }
        if !(10..=10_000).contains(&self.reset_us) {
            return Err("reset must be 10–10000 µs");
        }
        Ok(())
    }

    /// Duration in ticks of a `tick_hz` clock, rounded (for RMT items).
    pub fn ticks(ns: u32, tick_hz: u32) -> u32 {
        ((ns as u64 * tick_hz as u64 + 500_000_000) / 1_000_000_000) as u32
    }

    /// How long one frame of `bits` data bits takes, reset included, in µs.
    pub fn frame_us(&self, bits: usize) -> u32 {
        (bits as u64 * self.period_ns as u64 / 1000) as u32 + self.reset_us
    }
}

/// A WS2812 waveform drawn with SPI MOSI: each data bit becomes `bits` SPI bits at
/// `clock_hz`, of which the first `ones0` (data 0) or `ones1` (data 1) are high.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpiFit {
    pub clock_hz: u32,
    pub bits: u8,
    pub ones0: u8,
    pub ones1: u8,
    /// Worst high-time error against the requested timing.
    pub error_ns: u32,
}

impl SpiFit {
    /// Find the SPI encoding closest to `t`, given what the controller can actually do:
    /// `actual(hz)` returns the clock it would give for a requested `hz` (a Pi's SPI
    /// divides its core clock; a node's SPI has its own divider). Tries 3–8 SPI bits per
    /// data bit and keeps the smallest error, preferring fewer bits (less memory) on a
    /// tie. `None` when nothing fits inside [`TOLERANCE_NS`].
    pub fn find(t: &Timing, actual: impl Fn(u32) -> u32) -> Option<SpiFit> {
        let mut best: Option<SpiFit> = None;
        for bits in 3u8..=8 {
            let want = (bits as u64 * 1_000_000_000 / t.period_ns as u64) as u32;
            let hz = actual(want);
            if hz == 0 {
                continue;
            }
            let bit_ns = 1_000_000_000f32 / hz as f32;
            let ones = |ns: u32| -> u8 { ((ns as f32 / bit_ns) + 0.5) as u8 };
            let (o0, o1) = (ones(t.t0h_ns).max(1), ones(t.t1h_ns));
            if o1 <= o0 || o1 >= bits {
                continue;
            }
            let err = |o: u8, ns: u32| ((o as f32 * bit_ns) - ns as f32).abs() as u32;
            // The period stretches or shrinks with the clock too; chips care less about
            // it, but keep it inside the same tolerance.
            let period_err = ((bits as f32 * bit_ns) - t.period_ns as f32).abs() as u32;
            let error_ns = err(o0, t.t0h_ns).max(err(o1, t.t1h_ns));
            if error_ns > TOLERANCE_NS || period_err > 2 * TOLERANCE_NS {
                continue;
            }
            if best.is_none_or(|b| error_ns < b.error_ns) {
                best = Some(SpiFit { clock_hz: hz, bits, ones0: o0, ones1: o1, error_ns });
            }
        }
        best
    }

    /// Zero bytes that hold MOSI low for at least `reset_us`.
    pub fn reset_bytes(&self, reset_us: u32) -> usize {
        ((reset_us as u64 * self.clock_hz as u64).div_ceil(8_000_000)) as usize
    }
}

/// The three phases of the Pico SDK's ws2812 PIO program, in PIO cycles: high for `t1`,
/// then high (data 1) or low (data 0) for `t2`, then low for `t3`. T0H = t1,
/// T1H = t1 + t2, period = t1 + t2 + t3, at `cycle_hz` (the state machine's clock,
/// which the board makes with its clock divider).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PioFit {
    pub t1: u8,
    pub t2: u8,
    pub t3: u8,
    pub cycle_hz: u32,
    pub error_ns: u32,
}

impl PioFit {
    /// Longest phase the program allows: one instruction plus a 4-bit delay (one of the
    /// five delay/side-set bits is the side-set pin).
    pub const MAX_PHASE: u8 = 16;

    /// Fit `t` with up to `MAX_PHASE` cycles per phase, keeping the smallest high-time
    /// error; on a tie, the fewest cycles (the slowest clock, easiest to divide down to).
    /// The cycle clock is exact here; the board's divider error is the board's to check.
    pub fn find(t: &Timing) -> Option<PioFit> {
        let mut best: Option<PioFit> = None;
        for n in 3u32..=3 * Self::MAX_PHASE as u32 {
            let cycle_ns = t.period_ns as f32 / n as f32;
            let c = |ns: u32| ((ns as f32 / cycle_ns) + 0.5) as u32;
            let (t1, t1h) = (c(t.t0h_ns).max(1), c(t.t1h_ns));
            if t1h <= t1 || t1h >= n {
                continue;
            }
            let (t2, t3) = (t1h - t1, n - t1h);
            let max = Self::MAX_PHASE as u32;
            if t1 > max || t2 > max || t3 > max {
                continue;
            }
            let err = |cycles: u32, ns: u32| ((cycles as f32 * cycle_ns) - ns as f32).abs() as u32;
            let error_ns = err(t1, t.t0h_ns).max(err(t1h, t.t1h_ns));
            if error_ns > TOLERANCE_NS {
                continue;
            }
            if best.is_none_or(|b| error_ns < b.error_ns) {
                let cycle_hz = (n as u64 * 1_000_000_000 / t.period_ns as u64) as u32;
                best = Some(PioFit { t1: t1 as u8, t2: t2 as u8, t3: t3 as u8, cycle_hz, error_ns });
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_valid_and_findable() {
        for (name, _, t) in PRESETS {
            assert_eq!(t.check(), Ok(()), "{name}");
            assert_eq!(Timing::preset(name), Some(*t));
            assert!(SpiFit::find(t, |hz| hz).is_some(), "{name}: no SPI fit");
            assert!(PioFit::find(t).is_some(), "{name}: no PIO fit");
        }
        assert_eq!(Timing::preset("WS2812B"), Some(Timing::WS2812B));
        assert_eq!(Timing::preset("nope"), None);
    }

    #[test]
    fn rejects_impossible_timings() {
        let t = Timing { t0h_ns: 800, t1h_ns: 400, period_ns: 1250, reset_us: 300 };
        assert!(t.check().is_err());
        let t = Timing { t0h_ns: 400, t1h_ns: 1300, period_ns: 1250, reset_us: 300 };
        assert!(t.check().is_err());
    }

    #[test]
    fn ws2812b_over_an_ideal_spi_clock() {
        // 3 bits at 2.4 MHz: 417 ns per SPI bit; 0 → 100, 1 → 110.
        let f = SpiFit::find(&Timing::WS2812B, |hz| hz).unwrap();
        assert_eq!((f.bits, f.ones0, f.ones1, f.clock_hz), (3, 1, 2, 2_400_000));
        assert!(f.error_ns <= 40, "{f:?}");
        // 300 µs at 2.4 MHz = 720 bits = 90 bytes.
        assert_eq!(f.reset_bytes(300), 90);
    }

    #[test]
    fn spi_fit_follows_a_pi_divider() {
        // A Pi 4 at core_freq=500: spi-bcm2835 rounds the divider up to even, and the
        // clock is 500 MHz / divider.
        let pi = |hz: u32| {
            let div = 500_000_000u32.div_ceil(hz);
            500_000_000 / (div + div % 2).max(2)
        };
        let f = SpiFit::find(&Timing::WS2812B, pi).unwrap();
        assert!(f.error_ns <= TOLERANCE_NS);
        assert!((2..1000).step_by(2).any(|d| 500_000_000 / d == f.clock_hz), "{f:?}");
    }

    #[test]
    fn spi_fit_fails_when_the_clock_is_hopeless() {
        assert_eq!(SpiFit::find(&Timing::WS2812B, |_| 100_000), None);
    }

    #[test]
    fn pio_fit_ws2812b() {
        let f = PioFit::find(&Timing::WS2812B).unwrap();
        let n = (f.t1 + f.t2 + f.t3) as u32;
        let cycle = 1250.0 / n as f32;
        assert!((f.t1 as f32 * cycle - 400.0).abs() <= f.error_ns as f32 + 1.0);
        assert!(((f.t1 + f.t2) as f32 * cycle - 800.0).abs() <= f.error_ns as f32 + 1.0);
        assert_eq!(f.cycle_hz, n * 800_000);
        assert!(f.error_ns < 20, "{f:?}");
    }

    #[test]
    fn ticks_round() {
        assert_eq!(Timing::ticks(400, 80_000_000), 32);
        assert_eq!(Timing::ticks(1250, 10_000_000), 13);
        assert_eq!(Timing::WS2812B.frame_us(60 * 24), 1800 + 300);
    }
}
