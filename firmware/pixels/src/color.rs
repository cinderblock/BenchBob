//! Pixels and how they become wire bytes: colour order, gamma, brightness and a power
//! budget.
//!
//! Every source (patterns, the API, sACN) produces [`Rgbw`] pixels; [`Order::write`]
//! turns one into the bytes a chip expects. On an RGB strip the white component is added
//! to red, green and blue; on an RGBW strip it drives the white LED.

/// One pixel. `w` is the white channel (see the module docs for RGB strips).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rgbw {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub w: u8,
}

impl Rgbw {
    pub const OFF: Rgbw = Rgbw::rgb(0, 0, 0);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Rgbw { r, g, b, w: 0 }
    }

    pub const fn new(r: u8, g: u8, b: u8, w: u8) -> Self {
        Rgbw { r, g, b, w }
    }

    /// `#rrggbb` or `#rrggbbww` (the `#` is optional).
    pub fn parse_hex(s: &str) -> Option<Self> {
        let s = s.strip_prefix('#').unwrap_or(s);
        let byte = |i: usize| u8::from_str_radix(s.get(i..i + 2)?, 16).ok();
        match s.len() {
            6 => Some(Rgbw::rgb(byte(0)?, byte(2)?, byte(4)?)),
            8 => Some(Rgbw::new(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
            _ => None,
        }
    }

    /// Each channel scaled by `num / 255`.
    pub fn scale(self, num: u8) -> Self {
        let s = |v: u8| ((v as u16 * num as u16 + 127) / 255) as u8;
        Rgbw::new(s(self.r), s(self.g), s(self.b), s(self.w))
    }

    /// `self` → `to` by `t / 255`.
    pub fn lerp(self, to: Rgbw, t: u8) -> Self {
        let l = |a: u8, b: u8| ((a as i32 * (255 - t as i32) + b as i32 * t as i32 + 127) / 255) as u8;
        Rgbw::new(l(self.r, to.r), l(self.g, to.g), l(self.b, to.b), l(self.w, to.w))
    }

    /// The colour wheel at `hue` (0–255), full saturation and value: red → green → blue → red.
    pub fn hue(hue: u8) -> Self {
        let h = hue as u16 * 6;
        let (sector, f) = ((h >> 8) as u8, (h & 0xff) as u8);
        let (up, down) = (f, 255 - f);
        match sector {
            0 => Rgbw::rgb(255, up, 0),
            1 => Rgbw::rgb(down, 255, 0),
            2 => Rgbw::rgb(0, 255, up),
            3 => Rgbw::rgb(0, down, 255),
            4 => Rgbw::rgb(up, 0, 255),
            _ => Rgbw::rgb(255, 0, down),
        }
    }
}

/// One colour channel of a chip's wire order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    R,
    G,
    B,
    W,
}

/// The order a chip expects its channels in: any permutation of R, G and B, with an
/// optional W anywhere (`GRB` for WS2812B, `GRBW` for SK6812 RGBW, `BGR` for APA102).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Order {
    chans: [Channel; 4],
    len: u8,
}

impl Order {
    pub const RGB: Order = Order { chans: [Channel::R, Channel::G, Channel::B, Channel::W], len: 3 };
    pub const GRB: Order = Order { chans: [Channel::G, Channel::R, Channel::B, Channel::W], len: 3 };
    pub const BGR: Order = Order { chans: [Channel::B, Channel::G, Channel::R, Channel::W], len: 3 };
    pub const GRBW: Order = Order { chans: [Channel::G, Channel::R, Channel::B, Channel::W], len: 4 };

    /// `"GRB"`, `"rgbw"`, `"WRGB"`…: R, G and B exactly once each, W at most once.
    pub fn parse(s: &str) -> Option<Self> {
        let mut chans = [Channel::W; 4];
        let mut seen = [false; 4];
        let mut len = 0usize;
        for c in s.bytes() {
            let (ch, i) = match c.to_ascii_uppercase() {
                b'R' => (Channel::R, 0),
                b'G' => (Channel::G, 1),
                b'B' => (Channel::B, 2),
                b'W' => (Channel::W, 3),
                _ => return None,
            };
            if seen[i] || len == 4 {
                return None;
            }
            seen[i] = true;
            chans[len] = ch;
            len += 1;
        }
        (seen[0] && seen[1] && seen[2]).then_some(Order { chans, len: len as u8 })
    }

    /// Bytes per pixel on the wire: 3 or 4.
    pub fn channels(&self) -> usize {
        self.len as usize
    }

    pub fn has_white(&self) -> bool {
        self.len == 4
    }

    /// The order's name, e.g. `"GRBW"`.
    pub fn name(&self, out: &mut [u8; 4]) -> usize {
        for (o, c) in out.iter_mut().zip(&self.chans[..self.channels()]) {
            *o = match c {
                Channel::R => b'R',
                Channel::G => b'G',
                Channel::B => b'B',
                Channel::W => b'W',
            };
        }
        self.channels()
    }

    /// One pixel's wire bytes into `out[..self.channels()]`, after gamma (when `gamma`)
    /// and then brightness (`brightness / 255`).
    pub fn write(&self, px: Rgbw, gamma: bool, brightness: u8, out: &mut [u8]) {
        let px = if self.has_white() {
            px
        } else {
            let add = |v: u8| v.saturating_add(px.w);
            Rgbw::rgb(add(px.r), add(px.g), add(px.b))
        };
        for (o, c) in out.iter_mut().zip(&self.chans[..self.channels()]) {
            let v = match c {
                Channel::R => px.r,
                Channel::G => px.g,
                Channel::B => px.b,
                Channel::W => px.w,
            };
            let v = if gamma { GAMMA[v as usize] } else { v };
            *o = ((v as u16 * brightness as u16 + 127) / 255) as u8;
        }
    }
}

/// A current budget for a string. Chips draw roughly linearly with each channel's value,
/// so the estimate is `sum(channel / 255 × ma_per_channel) + leds × idle_ma`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PowerBudget {
    /// One channel at full (a WS2812B is about 12–20 mA).
    pub ma_per_channel: u16,
    /// Each pixel's draw when dark (about 1 mA for a WS2812B).
    pub idle_ma: u16,
    /// What the supply can give the string.
    pub max_ma: u32,
}

impl PowerBudget {
    /// The estimated draw of `bytes` (wire bytes of `leds` pixels), in mA.
    pub fn estimate_ma(&self, bytes: &[u8], leds: usize) -> u32 {
        let sum: u32 = bytes.iter().map(|&b| b as u32).sum();
        sum * self.ma_per_channel as u32 / 255 + leds as u32 * self.idle_ma as u32
    }

    /// Scale `bytes` down, if needed, to fit the budget. Returns the estimate afterwards.
    pub fn limit(&self, bytes: &mut [u8], leds: usize) -> u32 {
        let idle = leds as u32 * self.idle_ma as u32;
        let est = self.estimate_ma(bytes, leds);
        if est <= self.max_ma {
            return est;
        }
        let lit = est - idle;
        let room = self.max_ma.saturating_sub(idle);
        // Multiply by room/lit in 16.16 fixed point, rounding down so the result fits.
        let k = ((room as u64) << 16) / lit as u64;
        for b in bytes.iter_mut() {
            *b = ((*b as u64 * k) >> 16) as u8;
        }
        self.estimate_ma(bytes, leds)
    }
}

/// Gamma 2.6: perceptually even steps for 8-bit values (`round((i/255)^2.6 × 255)`).
pub const GAMMA: [u8; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, //
    1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3, //
    3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 5, 6, 6, 6, 6, 7, //
    7, 7, 8, 8, 8, 9, 9, 9, 10, 10, 10, 11, 11, 11, 12, 12, //
    13, 13, 13, 14, 14, 15, 15, 16, 16, 17, 17, 18, 18, 19, 19, 20, //
    20, 21, 21, 22, 22, 23, 24, 24, 25, 25, 26, 27, 27, 28, 29, 29, //
    30, 31, 31, 32, 33, 34, 34, 35, 36, 37, 38, 38, 39, 40, 41, 42, //
    42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, //
    58, 59, 60, 61, 62, 63, 64, 65, 66, 68, 69, 70, 71, 72, 73, 75, //
    76, 77, 78, 80, 81, 82, 84, 85, 86, 88, 89, 90, 92, 93, 94, 96, //
    97, 99, 100, 102, 103, 105, 106, 108, 109, 111, 112, 114, 115, 117, 119, 120, //
    122, 124, 125, 127, 129, 130, 132, 134, 136, 137, 139, 141, 143, 145, 146, 148, //
    150, 152, 154, 156, 158, 160, 162, 164, 166, 168, 170, 172, 174, 176, 178, 180, //
    182, 184, 186, 188, 191, 193, 195, 197, 199, 202, 204, 206, 209, 211, 213, 215, //
    218, 220, 223, 225, 227, 230, 232, 235, 237, 240, 242, 245, 247, 250, 252, 255, //
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_orders() {
        assert_eq!(Order::parse("grb"), Some(Order::GRB));
        assert_eq!(Order::parse("GRBW"), Some(Order::GRBW));
        let w = Order::parse("WRGB").unwrap();
        assert_eq!((w.channels(), w.has_white()), (4, true));
        let mut name = [0; 4];
        let n = w.name(&mut name);
        assert_eq!(&name[..n], b"WRGB");
        for bad in ["", "RG", "RRGB", "RGBX", "RGBWW", "RGWW"] {
            assert_eq!(Order::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn writes_in_order_with_white_folded_into_rgb() {
        let px = Rgbw::new(10, 20, 30, 5);
        let mut out = [0; 4];
        Order::GRB.write(px, false, 255, &mut out);
        assert_eq!(out[..3], [25, 15, 35]);
        Order::GRBW.write(px, false, 255, &mut out);
        assert_eq!(out, [20, 10, 30, 5]);
        Order::RGB.write(Rgbw::new(250, 0, 0, 10), false, 255, &mut out);
        assert_eq!(out[..3], [255, 10, 10]);
    }

    #[test]
    fn gamma_then_brightness() {
        let mut out = [0; 3];
        Order::RGB.write(Rgbw::rgb(255, 128, 0), true, 255, &mut out);
        assert_eq!(out, [255, GAMMA[128], 0]);
        Order::RGB.write(Rgbw::rgb(255, 128, 0), false, 128, &mut out);
        assert_eq!(out, [128, 64, 0]);
    }

    #[test]
    fn hex_and_wheel() {
        assert_eq!(Rgbw::parse_hex("#ff8000"), Some(Rgbw::rgb(255, 128, 0)));
        assert_eq!(Rgbw::parse_hex("01020304"), Some(Rgbw::new(1, 2, 3, 4)));
        assert_eq!(Rgbw::parse_hex("#fff"), None);
        assert_eq!(Rgbw::parse_hex("#gg0000"), None);
        assert_eq!(Rgbw::hue(0), Rgbw::rgb(255, 0, 0));
        assert_eq!(Rgbw::hue(85).g, 255);
        assert_eq!(Rgbw::hue(171).b, 255);
    }

    #[test]
    fn power_limit_scales_to_budget() {
        let b = PowerBudget { ma_per_channel: 20, idle_ma: 1, max_ma: 1000 };
        // 60 pixels full white: 60 × 3 × 20 + 60 = 3660 mA.
        let mut bytes = [255u8; 180];
        assert_eq!(b.estimate_ma(&bytes, 60), 3660);
        let after = b.limit(&mut bytes, 60);
        assert!(after <= 1000 && after > 950, "{after}");
        // Under budget: untouched.
        let mut dim = [10u8; 180];
        b.limit(&mut dim, 60);
        assert_eq!(dim, [10u8; 180]);
    }
}
