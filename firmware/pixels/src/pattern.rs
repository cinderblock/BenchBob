//! Built-in patterns: each is a pure function of the time (ms since it started) and the
//! pixel index, so every driver renders them the same way and they need no state.
//!
//! Two of them are for bring-up rather than looks:
//! - [`Pattern::OrderTest`]: pixel 0 red, 1 green, 2 blue, 3 white, the rest off. A wrong
//!   colour order shows up as the wrong colours in the first three pixels.
//! - [`Pattern::Ruler`]: every 10th pixel white, every 50th red, the last one green, so a
//!   string's length can be counted and checked at a glance.

use crate::color::Rgbw;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pattern {
    Off,
    Solid {
        color: Rgbw,
    },
    /// `a` at the first pixel to `b` at the last.
    Gradient {
        a: Rgbw,
        b: Rgbw,
    },
    /// The colour wheel, `spread` pixels per full cycle, moving once round every
    /// `period_ms` (0: still).
    Rainbow {
        spread: u16,
        period_ms: u32,
    },
    /// A `width`-pixel block of `color` on `background`, moving `speed` pixels/s.
    Chase {
        color: Rgbw,
        background: Rgbw,
        width: u16,
        speed: u16,
    },
    /// `color` fading in and out every `period_ms`.
    Breathe {
        color: Rgbw,
        period_ms: u32,
    },
    /// Random pixels lighting up and fading: about `density`/255 of them lit at a time,
    /// each flash `period_ms` long.
    Twinkle {
        color: Rgbw,
        density: u8,
        period_ms: u32,
    },
    OrderTest,
    Ruler,
}

/// Pattern names, in the order the UI lists them, with a description.
pub const NAMES: &[(&str, &str)] = &[
    ("off", "All pixels off"),
    ("solid", "One colour"),
    ("gradient", "Colour a at the first pixel to b at the last"),
    ("rainbow", "Moving colour wheel"),
    ("chase", "A block of colour running along the string"),
    ("breathe", "One colour fading in and out"),
    ("twinkle", "Random pixels flashing"),
    ("order-test", "Pixels 0–3 red, green, blue, white: checks the colour order"),
    ("ruler", "Every 10th pixel white, every 50th red, the last green: checks the length"),
];

impl Pattern {
    pub fn name(&self) -> &'static str {
        NAMES[match self {
            Pattern::Off => 0,
            Pattern::Solid { .. } => 1,
            Pattern::Gradient { .. } => 2,
            Pattern::Rainbow { .. } => 3,
            Pattern::Chase { .. } => 4,
            Pattern::Breathe { .. } => 5,
            Pattern::Twinkle { .. } => 6,
            Pattern::OrderTest => 7,
            Pattern::Ruler => 8,
        }]
        .0
    }

    /// The pattern called `name` with its default parameters (callers then override the
    /// ones they were given).
    pub fn default_for(name: &str) -> Option<Pattern> {
        const WHITE: Rgbw = Rgbw::rgb(255, 255, 255);
        Some(match name {
            "off" => Pattern::Off,
            "solid" => Pattern::Solid { color: WHITE },
            "gradient" => Pattern::Gradient { a: Rgbw::rgb(255, 0, 0), b: Rgbw::rgb(0, 0, 255) },
            "rainbow" => Pattern::Rainbow { spread: 60, period_ms: 5000 },
            "chase" => Pattern::Chase { color: WHITE, background: Rgbw::OFF, width: 3, speed: 20 },
            "breathe" => Pattern::Breathe { color: WHITE, period_ms: 4000 },
            "twinkle" => Pattern::Twinkle { color: WHITE, density: 25, period_ms: 1000 },
            "order-test" => Pattern::OrderTest,
            "ruler" => Pattern::Ruler,
            _ => return None,
        })
    }

    /// Whether the output never changes over time: a driver renders it once.
    pub fn is_static(&self) -> bool {
        match self {
            Pattern::Rainbow { period_ms, .. } => *period_ms == 0,
            Pattern::Chase { speed, .. } => *speed == 0,
            Pattern::Breathe { .. } | Pattern::Twinkle { .. } => false,
            _ => true,
        }
    }

    /// Pixel `i` of `n` at `t_ms`.
    pub fn pixel(&self, t_ms: u32, i: usize, n: usize) -> Rgbw {
        match *self {
            Pattern::Off => Rgbw::OFF,
            Pattern::Solid { color } => color,
            Pattern::Gradient { a, b } => {
                let t = if n > 1 { (i * 255 / (n - 1)) as u8 } else { 0 };
                a.lerp(b, t)
            }
            Pattern::Rainbow { spread, period_ms } => {
                let shift = if period_ms == 0 { 0 } else { ((t_ms % period_ms) as u64 * 256 / period_ms as u64) as u32 };
                let spread = spread.max(1) as u32;
                let along = (i as u32 % spread) * 256 / spread;
                Rgbw::hue((along + shift) as u8)
            }
            Pattern::Chase { color, background, width, speed } => {
                let n = n.max(1) as u64;
                let head = (t_ms as u64 * speed as u64 / 1000) % n;
                let behind = (head + n - i as u64 % n) % n;
                if behind < width as u64 { color } else { background }
            }
            Pattern::Breathe { color, period_ms } => color.scale(breathe(t_ms, period_ms)),
            Pattern::Twinkle { color, density, period_ms } => {
                let period = period_ms.max(1);
                // Each pixel gets its own phase, so flashes don't all start together.
                let offset = hash(i as u32, 0x5EED) % period;
                let t = t_ms.wrapping_add(offset);
                let slot = t / period;
                if (hash(i as u32, slot) & 0xFF) < density as u32 { color.scale(breathe(t, period)) } else { Rgbw::OFF }
            }
            Pattern::OrderTest => match i {
                0 => Rgbw::rgb(255, 0, 0),
                1 => Rgbw::rgb(0, 255, 0),
                2 => Rgbw::rgb(0, 0, 255),
                3 => Rgbw::new(0, 0, 0, 255),
                _ => Rgbw::OFF,
            },
            Pattern::Ruler => {
                if i + 1 == n {
                    Rgbw::rgb(0, 255, 0)
                } else if (i + 1).is_multiple_of(50) {
                    Rgbw::rgb(255, 0, 0)
                } else if (i + 1).is_multiple_of(10) {
                    Rgbw::rgb(255, 255, 255)
                } else {
                    Rgbw::OFF
                }
            }
        }
    }

    /// Render a whole frame.
    pub fn fill(&self, t_ms: u32, out: &mut [Rgbw]) {
        let n = out.len();
        for (i, px) in out.iter_mut().enumerate() {
            *px = self.pixel(t_ms, i, n);
        }
    }
}

/// 0 → 255 → 0 over `period_ms`, eased (a squared triangle, so it lingers when dim, as
/// the eye expects).
fn breathe(t_ms: u32, period_ms: u32) -> u8 {
    let p = period_ms.max(2);
    let half = p / 2;
    let t = t_ms % p;
    let tri = if t < half { t * 255 / half } else { (p - t) * 255 / (p - half) };
    (tri * tri / 255) as u8
}

/// A small integer hash (lowbias32), for repeatable "random" twinkles.
fn hash(a: u32, b: u32) -> u32 {
    let mut x = a.wrapping_mul(0x9E37_79B9) ^ b;
    x ^= x >> 16;
    x = x.wrapping_mul(0x7FEB_352D);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846C_A68B);
    x ^ (x >> 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_has_a_default() {
        for (name, _) in NAMES {
            let p = Pattern::default_for(name).unwrap();
            assert_eq!(p.name(), *name);
        }
        assert_eq!(Pattern::default_for("disco"), None);
    }

    #[test]
    fn order_test_and_ruler() {
        let mut px = [Rgbw::rgb(9, 9, 9); 60];
        Pattern::OrderTest.fill(0, &mut px);
        assert_eq!(px[0], Rgbw::rgb(255, 0, 0));
        assert_eq!(px[3].w, 255);
        assert_eq!(px[4], Rgbw::OFF);
        Pattern::Ruler.fill(0, &mut px);
        assert_eq!(px[9], Rgbw::rgb(255, 255, 255));
        assert_eq!(px[49], Rgbw::rgb(255, 0, 0));
        assert_eq!(px[59], Rgbw::rgb(0, 255, 0));
        assert_eq!(px[10], Rgbw::OFF);
    }

    #[test]
    fn gradient_ends() {
        let p = Pattern::Gradient { a: Rgbw::rgb(255, 0, 0), b: Rgbw::rgb(0, 0, 255) };
        assert_eq!(p.pixel(0, 0, 10), Rgbw::rgb(255, 0, 0));
        assert_eq!(p.pixel(0, 9, 10), Rgbw::rgb(0, 0, 255));
        assert_eq!(p.pixel(0, 0, 1), Rgbw::rgb(255, 0, 0));
    }

    #[test]
    fn chase_moves_and_wraps() {
        let p = Pattern::Chase { color: Rgbw::rgb(1, 1, 1), background: Rgbw::OFF, width: 2, speed: 10 };
        // At 0 ms the head is pixel 0, and the tail wraps to the end.
        assert_eq!(p.pixel(0, 0, 20), Rgbw::rgb(1, 1, 1));
        assert_eq!(p.pixel(0, 19, 20), Rgbw::rgb(1, 1, 1));
        assert_eq!(p.pixel(0, 1, 20), Rgbw::OFF);
        // 10 px/s: after 500 ms the head is pixel 5.
        assert_eq!(p.pixel(500, 5, 20), Rgbw::rgb(1, 1, 1));
        assert_eq!(p.pixel(500, 4, 20), Rgbw::rgb(1, 1, 1));
        assert_eq!(p.pixel(500, 3, 20), Rgbw::OFF);
        assert!(!p.is_static());
    }

    #[test]
    fn rainbow_and_breathe() {
        let p = Pattern::Rainbow { spread: 6, period_ms: 0 };
        assert_eq!(p.pixel(0, 0, 6), Rgbw::rgb(255, 0, 0));
        assert_eq!(p.pixel(0, 6, 12), Rgbw::rgb(255, 0, 0));
        assert!(p.is_static());
        assert_eq!(breathe(0, 1000), 0);
        assert_eq!(breathe(500, 1000), 255);
        assert!(breathe(250, 1000) < 128);
    }

    #[test]
    fn twinkle_is_repeatable_and_roughly_dense() {
        let p = Pattern::Twinkle { color: Rgbw::rgb(255, 255, 255), density: 64, period_ms: 1000 };
        let lit = (0..1000).filter(|&i| p.pixel(1234, i, 1000) != Rgbw::OFF).count();
        assert_eq!(lit, (0..1000).filter(|&i| p.pixel(1234, i, 1000) != Rgbw::OFF).count());
        // 64/255 ≈ 25% selected, of which most are past the dark ends of their fade.
        assert!((100..300).contains(&lit), "{lit}");
    }
}
