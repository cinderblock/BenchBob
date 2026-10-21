//! JavaScript number semantics the decoders rely on, without std: rounding, sign
//! extension, and number → string as `String(n)` prints it.

use alloc::format;
use alloc::string::String;

/// `Math.round`: half rounds up (towards +∞).
pub fn js_round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let f = libm::floor(x);
    if x - f >= 0.5 { f + 1.0 } else { f }
}

/// `Math.round(v * 10 ** n) / 10 ** n`
pub fn r2(v: f64, n: i32) -> f64 {
    let m = libm::pow(10.0, n as f64);
    js_round(v * m) / m
}

pub fn s8(v: u8) -> i32 {
    v as i8 as i32
}

pub fn s16(v: u16) -> i32 {
    v as i16 as i32
}

pub fn bcd(v: u8) -> u32 {
    (v >> 4) as u32 * 10 + (v & 0x0f) as u32
}

pub fn is_bcd(v: u8) -> bool {
    (v & 0x0f) <= 9 && v >> 4 <= 9
}

/// `String(n)` for the numbers the decoders produce: integers without a fraction, -0 as
/// "0", otherwise the shortest round-trip form (what JavaScript prints in that range).
pub fn js_num_str(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if n == 0.0 {
        return "0".into();
    }
    if n == libm::trunc(n) && n.abs() < 1e21 {
        return format!("{}", n as i128);
    }
    format!("{n}")
}

/// `n.toString(16)` for a non-negative integer (lower case, no padding).
pub fn hex_lower(n: u32) -> String {
    format!("{n:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_and_strings_match_javascript() {
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(js_round(-2.5), -2.0);
        assert_eq!(r2(25.08456, 2), 25.08);
        assert_eq!(js_num_str(-0.0), "0");
        assert_eq!(js_num_str(100.0), "100");
        assert_eq!(js_num_str(25.08), "25.08");
        assert_eq!(js_num_str(-10.5), "-10.5");
    }
}
