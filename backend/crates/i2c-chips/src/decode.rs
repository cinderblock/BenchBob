//! Pure decoders and CRCs (unit-tested against the datasheets' worked examples).

use alloc::string::String;
use alloc::vec::Vec;

use crate::num::{bcd, js_round, s8, s16};
use crate::time::utc_ms;

// ---------- CRCs ----------

/// Sensirion / Aosong CRC-8: poly 0x31, init 0xFF.
pub fn crc8sens(bytes: &[u8], init: u8) -> u8 {
    let mut c = init;
    for &b in bytes {
        c ^= b;
        for _ in 0..8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0x31 } else { c << 1 };
        }
    }
    c
}

/// Sensirion-style words: [hi, lo, crc]×n → words, or None if any CRC fails.
pub fn sens_words(b: &[u8], init: u8) -> Option<Vec<u16>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < b.len() {
        if crc8sens(&b[i..i + 2], init) != b[i + 2] {
            return None;
        }
        out.push((b[i] as u16) << 8 | b[i + 1] as u16);
        i += 3;
    }
    Some(out)
}

// ---------- Bosch BME280 / BMP280 ----------

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Bme280Cal {
    pub t1: f64,
    pub t2: f64,
    pub t3: f64,
    pub p: [f64; 9],
    /// H1..H6, BME280 only.
    pub h: Option<[f64; 6]>,
}

/// Calibration from 0x88..0xA1 (26 bytes) and, for BME280, 0xE1..0xE7 (7 bytes).
pub fn bme280_cal(a: &[u8], h: Option<&[u8]>) -> Bme280Cal {
    let u = |i: usize| (a[i] as u16 | (a[i + 1] as u16) << 8) as f64;
    let s = |i: usize| s16(a[i] as u16 | (a[i + 1] as u16) << 8) as f64;
    let mut p = [0.0; 9];
    p[0] = u(6);
    for (k, item) in p.iter_mut().enumerate().skip(1) {
        *item = s(6 + k * 2);
    }
    Bme280Cal {
        t1: u(0),
        t2: s(2),
        t3: s(4),
        p,
        h: h.map(|h| {
            [
                a[25] as f64,
                s16(h[0] as u16 | (h[1] as u16) << 8) as f64,
                h[2] as f64,
                ((s8(h[3]) << 4) | (h[4] & 0x0f) as i32) as f64,
                ((s8(h[5]) << 4) | (h[4] >> 4) as i32) as f64,
                s8(h[6]) as f64,
            ]
        }),
    }
}

/// Bosch BME280/BMP280 floating-point compensation (datasheet §8.1): °C, Pa, %RH.
pub fn bme280_comp(c: &Bme280Cal, adc_t: f64, adc_p: f64, adc_h: Option<f64>) -> (f64, Option<f64>, Option<f64>) {
    let p = &c.p;
    let mut v1 = (adc_t / 16384.0 - c.t1 / 1024.0) * c.t2;
    let mut v2 = libm::pow(adc_t / 131072.0 - c.t1 / 8192.0, 2.0) * c.t3;
    let tf = v1 + v2;
    let t = tf / 5120.0;
    v1 = tf / 2.0 - 64000.0;
    v2 = (v1 * v1 * p[5]) / 32768.0;
    v2 += v1 * p[4] * 2.0;
    v2 = v2 / 4.0 + p[3] * 65536.0;
    v1 = ((p[2] * v1 * v1) / 524288.0 + p[1] * v1) / 524288.0;
    v1 = (1.0 + v1 / 32768.0) * p[0];
    let mut pres = None;
    if v1 != 0.0 && adc_p != 524288.0 {
        let mut q = 1048576.0 - adc_p;
        q = ((q - v2 / 4096.0) * 6250.0) / v1;
        let a = (p[8] * q * q) / 2147483648.0;
        let b = (q * p[7]) / 32768.0;
        pres = Some(q + (a + b + p[6]) / 16.0);
    }
    let mut hum = None;
    if let (Some(adc_h), Some(h)) = (adc_h, c.h)
        && adc_h != 32768.0
    {
        let mut x = tf - 76800.0;
        x = (adc_h - (h[3] * 64.0 + (h[4] / 16384.0) * x))
            * ((h[1] / 65536.0) * (1.0 + (h[5] / 67108864.0) * x * (1.0 + (h[2] / 67108864.0) * x)));
        x *= 1.0 - (h[0] * x) / 524288.0;
        hum = Some(x.clamp(0.0, 100.0));
    }
    (t, pres, hum)
}

// ---------- Bosch BME680 ----------

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Bme680Cal {
    pub t1: f64,
    pub t2: f64,
    pub t3: f64,
    pub p1: f64,
    pub p2: f64,
    pub p3: f64,
    pub p4: f64,
    pub p5: f64,
    pub p6: f64,
    pub p7: f64,
    pub p8: f64,
    pub p9: f64,
    pub p10: f64,
    pub h1: f64,
    pub h2: f64,
    pub h3: f64,
    pub h4: f64,
    pub h5: f64,
    pub h6: f64,
    pub h7: f64,
    pub g1: f64,
    pub g2: f64,
    pub g3: f64,
    pub heat_range: f64,
    pub heat_val: f64,
    pub sw_err: f64,
}

/// `reg(addr)` → byte for 0x00..0x04, 0x8A..0xA1, 0xE1..0xEE.
pub fn bme680_cal(reg: impl Fn(u8) -> u8) -> Bme680Cal {
    let u16_ = |lo: u8, hi: u8| reg(lo) as u16 | (reg(hi) as u16) << 8;
    let s8f = |r: u8| s8(reg(r)) as f64;
    let s16f = |lo: u8, hi: u8| s16(u16_(lo, hi)) as f64;
    Bme680Cal {
        t1: u16_(0xe9, 0xea) as f64,
        t2: s16f(0x8a, 0x8b),
        t3: s8f(0x8c),
        p1: u16_(0x8e, 0x8f) as f64,
        p2: s16f(0x90, 0x91),
        p3: s8f(0x92),
        p4: s16f(0x94, 0x95),
        p5: s16f(0x96, 0x97),
        p6: s8f(0x99),
        p7: s8f(0x98),
        p8: s16f(0x9c, 0x9d),
        p9: s16f(0x9e, 0x9f),
        p10: reg(0xa0) as f64,
        h1: ((reg(0xe3) as u32) << 4 | (reg(0xe2) & 0x0f) as u32) as f64,
        h2: ((reg(0xe1) as u32) << 4 | (reg(0xe2) >> 4) as u32) as f64,
        h3: s8f(0xe4),
        h4: s8f(0xe5),
        h5: s8f(0xe6),
        h6: reg(0xe7) as f64,
        h7: s8f(0xe8),
        g1: s8f(0xed),
        g2: s16f(0xeb, 0xec),
        g3: s8f(0xee),
        heat_range: ((reg(0x02) & 0x30) >> 4) as f64,
        heat_val: s8f(0x00),
        sw_err: (s8(reg(0x04) & 0xf0) >> 4) as f64,
    }
}

const K1: [f64; 16] = [0.0, 0.0, 0.0, 0.0, 0.0, -1.0, 0.0, -0.8, 0.0, 0.0, -0.2, -0.5, 0.0, -1.0, 0.0, 0.0];
const K2: [f64; 16] = [0.0, 0.0, 0.0, 0.0, 0.1, 0.7, 0.0, -0.8, -0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];

/// Bosch BME68x API floating-point compensation (BME680 gas formula): °C, Pa, %RH, Ω.
pub fn bme680_comp(c: &Bme680Cal, adc_t: f64, adc_p: f64, adc_h: f64, gas: Option<(f64, usize)>) -> (f64, f64, f64, Option<f64>) {
    let mut v1 = (adc_t / 16384.0 - c.t1 / 1024.0) * c.t2;
    let mut v2 = libm::pow(adc_t / 131072.0 - c.t1 / 8192.0, 2.0) * (c.t3 * 16.0);
    let tf = v1 + v2;
    let t = tf / 5120.0;
    v1 = tf / 2.0 - 64000.0;
    v2 = v1 * v1 * (c.p6 / 131072.0);
    v2 += v1 * c.p5 * 2.0;
    v2 = v2 / 4.0 + c.p4 * 65536.0;
    v1 = ((c.p3 * v1 * v1) / 16384.0 + c.p2 * v1) / 524288.0;
    v1 = (1.0 + v1 / 32768.0) * c.p1;
    let mut p = 1048576.0 - adc_p;
    if v1 != 0.0 {
        p = ((p - v2 / 4096.0) * 6250.0) / v1;
        let a = (c.p9 * p * p) / 2147483648.0;
        let b = p * (c.p8 / 32768.0);
        let d = libm::pow(p / 256.0, 3.0) * (c.p10 / 131072.0);
        p += (a + b + d + c.p7 * 128.0) / 16.0;
    }
    let tc = t;
    let h1 = adc_h - (c.h1 * 16.0 + (c.h3 / 2.0) * tc);
    let h2 = h1 * ((c.h2 / 262144.0) * (1.0 + (c.h4 / 16384.0) * tc + (c.h5 / 1048576.0) * tc * tc));
    let h3 = c.h6 / 16384.0;
    let h4 = c.h7 / 2097152.0;
    let h = (h2 + (h3 + h4 * tc) * h2 * h2).clamp(0.0, 100.0);
    let g = gas.map(|(adc, range)| {
        let a = 1340.0 + 5.0 * c.sw_err;
        let b = a * (1.0 + K1[range & 15] / 100.0);
        let d = 1.0 + K2[range & 15] / 100.0;
        1.0 / (d * 0.000000125 * (1u32 << (range & 15)) as f64 * ((adc - 512.0) / b + 1.0))
    });
    (t, p, h, g)
}

/// BME680 heater resistance code for a target °C.
pub fn bme680_heat_res(c: &Bme680Cal, target: f64, ambient: f64) -> u8 {
    let a = c.g1 / 16.0 + 49.0;
    let b = (c.g2 / 32768.0) * 0.0005 + 0.00235;
    let d = c.g3 / 1024.0;
    let e = a * (1.0 + b * target) + d * ambient;
    js_round(3.4 * (e * (4.0 / (4.0 + c.heat_range)) * (1.0 / (1.0 + c.heat_val * 0.002)) - 25.0)).clamp(0.0, 255.0) as u8
}

// ---------- Bosch BMP180 ----------

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Bmp180Cal {
    pub ac1: i64,
    pub ac2: i64,
    pub ac3: i64,
    pub ac4: i64,
    pub ac5: i64,
    pub ac6: i64,
    pub b1: i64,
    pub b2: i64,
    pub mb: i64,
    pub mc: i64,
    pub md: i64,
}

pub fn bmp180_cal(b: &[u8]) -> Bmp180Cal {
    let u = |i: usize| ((b[i] as u16) << 8 | b[i + 1] as u16) as i64;
    let s = |i: usize| s16((b[i] as u16) << 8 | b[i + 1] as u16) as i64;
    Bmp180Cal { ac1: s(0), ac2: s(2), ac3: s(4), ac4: u(6), ac5: u(8), ac6: u(10), b1: s(12), b2: s(14), mb: s(16), mc: s(18), md: s(20) }
}

/// BMP180 datasheet integer algorithm. Power-of-two divisions are arithmetic shifts in the
/// reference code (floor, not truncation), which matters for the negative terms. °C and Pa.
pub fn bmp180_comp(c: &Bmp180Cal, ut: i64, up: i64, oss: u32) -> (f64, i64) {
    let div = |a: i64, b: i64| a.checked_div(b).unwrap_or(0);
    let mut x1 = ((ut - c.ac6) * c.ac5) >> 15;
    let mut x2 = div(c.mc * 2048, x1 + c.md);
    let b5 = x1 + x2;
    let temp = ((b5 + 8) >> 4) as f64 / 10.0;
    let b6 = b5 - 4000;
    x1 = (c.b2 * ((b6 * b6) >> 12)) >> 11;
    x2 = (c.ac2 * b6) >> 11;
    let mut x3 = x1 + x2;
    let b3 = div((c.ac1 * 4 + x3) * (1 << oss) + 2, 4);
    x1 = (c.ac3 * b6) >> 13;
    x2 = (c.b1 * ((b6 * b6) >> 12)) >> 16;
    x3 = (x1 + x2 + 2) >> 2;
    let b4 = (c.ac4 * (x3 + 32768)) >> 15;
    let b7 = (up - b3) * (50000 >> oss);
    let mut p = if b7 < 0x8000_0000 { div(b7 * 2, b4) } else { div(b7, b4) * 2 };
    x1 = (p >> 8) * (p >> 8);
    x1 = (x1 * 3038) >> 16;
    x2 = (-7357 * p) >> 16;
    p += (x1 + x2 + 3791) >> 4;
    (temp, p)
}

// ---------- light, humidity, ADC ----------

/// TSL2561 T/FN/CL lux from raw channels, integration code (0..2) and gain bit. None when saturated.
pub fn tsl2561_lux(ch0: u16, ch1: u16, integ: u8, gain16: bool) -> Option<f64> {
    if ch0 == 0 {
        return Some(0.0);
    }
    let sat = match integ {
        0 => 5047,
        1 => 37177,
        _ => 65535,
    };
    if ch0 >= sat || ch1 >= sat {
        return None;
    }
    let scale = (match integ {
        0 => 322.0 / 11.0,
        1 => 322.0 / 81.0,
        _ => 1.0,
    }) * if gain16 { 1.0 } else { 16.0 };
    let c0 = ch0 as f64 * scale;
    let c1 = ch1 as f64 * scale;
    let r = c1 / c0;
    let lux = if r <= 0.5 {
        0.0304 * c0 - 0.062 * c0 * libm::pow(r, 1.4)
    } else if r <= 0.61 {
        0.0224 * c0 - 0.031 * c1
    } else if r <= 0.8 {
        0.0128 * c0 - 0.0153 * c1
    } else if r <= 1.3 {
        0.00146 * c0 - 0.00112 * c1
    } else {
        0.0
    };
    Some(lux.max(0.0))
}

/// AHT10/AHT20 7-byte frame (status, 5 data bytes, CRC) → (°C, %RH, CRC ok if present).
pub fn aht_decode(b: &[u8]) -> (f64, f64, Option<bool>) {
    let hr = (b[1] as u32) << 12 | (b[2] as u32) << 4 | (b[3] >> 4) as u32;
    let tr = ((b[3] & 0x0f) as u32) << 16 | (b[4] as u32) << 8 | b[5] as u32;
    let crc = (b.len() >= 7).then(|| crc8sens(&b[..6], 0xff) == b[6]);
    ((tr as f64 / 1048576.0) * 200.0 - 50.0, (hr as f64 / 1048576.0) * 100.0, crc)
}

pub fn sht3x_t(raw: u16) -> f64 {
    -45.0 + (175.0 * raw as f64) / 65535.0
}
pub fn sht3x_h(raw: u16) -> f64 {
    (100.0 * raw as f64) / 65535.0
}
pub fn sht4x_h(raw: u16) -> f64 {
    (-6.0 + (125.0 * raw as f64) / 65535.0).clamp(0.0, 100.0)
}

/// VEML7700 integration time (ms) for a config word (IT bits 9:6).
pub fn veml7700_it(cfg: u16) -> f64 {
    match (cfg >> 6) & 0xf {
        0xc => 25.0,
        0x8 => 50.0,
        0x0 => 100.0,
        0x1 => 200.0,
        0x2 => 400.0,
        0x3 => 800.0,
        _ => 100.0,
    }
}
/// Lux per count (gain bits 12:11).
pub fn veml7700_res(cfg: u16) -> f64 {
    let gain = [1.0, 2.0, 1.0 / 8.0, 1.0 / 4.0][((cfg >> 11) & 3) as usize];
    0.0036 * (800.0 / veml7700_it(cfg)) * (2.0 / gain)
}
pub fn veml7700_lux(raw: u16, cfg: u16) -> f64 {
    let mut l = raw as f64 * veml7700_res(cfg);
    // Vishay's high-lux non-linearity correction (app note), valid above ~1000 lx.
    if l > 1000.0 {
        l = 6.0135e-13 * libm::pow(l, 4.0) - 9.3924e-9 * libm::pow(l, 3.0) + 8.1488e-5 * libm::pow(l, 2.0) + 1.0023 * l;
    }
    l
}

pub const ADS_FSR: [f64; 8] = [6.144, 4.096, 2.048, 1.024, 0.512, 0.256, 0.256, 0.256];
pub const ADS_MUX: [&str; 8] = ["AIN0−AIN1", "AIN0−AIN3", "AIN1−AIN3", "AIN2−AIN3", "AIN0", "AIN1", "AIN2", "AIN3"];

/// ADS1x15 conversion register → volts, given the config word that produced it.
pub fn ads_volts(raw: u16, cfg: u16, ads1015: bool) -> f64 {
    let fsr = ADS_FSR[((cfg >> 9) & 7) as usize];
    if ads1015 { (s16(raw) >> 4) as f64 * (fsr / 2048.0) } else { s16(raw) as f64 * (fsr / 32768.0) }
}

// ---------- RTC, EEPROM ----------

/// DS1307/DS3231 time registers 0x00..0x06 → (UTC ms or None if invalid, halted, 12 h mode).
pub fn rtc_decode(r: &[u8]) -> (Option<i64>, bool, bool) {
    let halted = r[0] & 0x80 != 0; // DS1307 CH bit
    let sec = bcd(r[0] & 0x7f);
    let min = bcd(r[1] & 0x7f);
    let h12 = r[2] & 0x40 != 0;
    let hr = if h12 { bcd(r[2] & 0x1f) % 12 + if r[2] & 0x20 != 0 { 12 } else { 0 } } else { bcd(r[2] & 0x3f) };
    let day = bcd(r[4] & 0x3f);
    let mon = bcd(r[5] & 0x1f);
    let yr = 2000 + bcd(r[6]) + if r[5] & 0x80 != 0 { 100 } else { 0 };
    if sec > 59 || min > 59 || hr > 23 || !(1..=31).contains(&day) || !(1..=12).contains(&mon) {
        return (None, halted, h12);
    }
    (Some(utc_ms(yr as i64, mon as i64 - 1, day as i64, hr as i64, min as i64, sec as i64)), halted, h12)
}

/// A Raspberry Pi HAT EEPROM header ("R-Pi" signature, first atom).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HatEeprom {
    pub version: u8,
    pub atoms: u16,
    pub length: u32,
    pub vendor: Option<String>,
    pub product: Option<String>,
    pub uuid: Option<String>,
    pub pid: Option<u16>,
    pub pver: Option<u16>,
}

pub fn parse_hat_eeprom(b: &[u8]) -> Option<HatEeprom> {
    if b.len() < 12 || &b[..4] != b"R-Pi" {
        return None;
    }
    let u16_ = |i: usize| b[i] as u16 | (b[i + 1] as u16) << 8;
    let u32_ = |i: usize| u16_(i) as u32 | (u16_(i + 2) as u32) << 16;
    let mut out = HatEeprom { version: b[4], atoms: u16_(6), length: u32_(8), ..Default::default() };
    // First atom at 12: type u16, count u16, dlen u32, data. Type 1 = vendor info.
    if b.len() >= 20 && u16_(12) == 1 {
        let d = 20;
        if b.len() >= d + 22 {
            let uuid: String = b[d..d + 16].iter().rev().map(|x| alloc::format!("{x:02x}")).collect();
            out.uuid = Some(alloc::format!("{}-{}-{}-{}-{}", &uuid[..8], &uuid[8..12], &uuid[12..16], &uuid[16..20], &uuid[20..]));
            out.pid = Some(u16_(d + 16));
            out.pver = Some(u16_(d + 18));
            let vl = b[d + 20] as usize;
            let pl = b[d + 21] as usize;
            let s = |st: usize, n: usize| -> String {
                let t: String = b[st..(st + n).min(b.len())].iter().map(|&c| c as char).collect();
                t.trim_end_matches('\0').into()
            };
            if b.len() >= d + 22 + vl {
                out.vendor = Some(s(d + 22, vl));
            }
            if b.len() >= d + 22 + vl + pl {
                out.product = Some(s(d + 22 + vl, pl));
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{bme280_regs, eeprom_regs};

    fn close(a: f64, b: f64, digits: i32) -> bool {
        // bun's toBeCloseTo: |a - b| < 10^-digits / 2
        (a - b).abs() < libm::pow(10.0, -digits as f64) / 2.0
    }

    #[test]
    fn sensirion_crc_matches_the_datasheet_example() {
        assert_eq!(crc8sens(&[0xbe, 0xef], 0xff), 0x92);
        assert_eq!(sens_words(&[0xbe, 0xef, 0x92], 0xff), Some(alloc::vec![0xbeef]));
        assert_eq!(sens_words(&[0xbe, 0xef, 0x93], 0xff), None);
    }

    #[test]
    fn bme280_compensation_reproduces_the_bosch_worked_example() {
        let r = bme280_regs();
        let a: Vec<u8> = (0..26).map(|i| r.get(&(0x88 + i)).copied().unwrap_or(0)).collect();
        let h: Vec<u8> = (0..7).map(|i| r[&(0xe1 + i)]).collect();
        let cal = bme280_cal(&a, Some(&h));
        assert_eq!((cal.t1, cal.t2, cal.t3, cal.p[0], cal.p[8]), (27504.0, 26435.0, -1000.0, 36477.0, 6000.0));
        assert_eq!(cal.h.unwrap(), [75.0, 362.0, 0.0, 313.0, 50.0, 30.0]);
        let (t, p, h) = bme280_comp(&cal, 519888.0, 415148.0, Some(27000.0));
        assert!(close(t, 25.08, 2));
        assert!(close(p.unwrap() / 100.0, 1006.53, 1));
        assert!(h.unwrap() > 0.0 && h.unwrap() < 100.0);
    }

    #[test]
    fn bmp180_integer_algorithm_reproduces_the_datasheet_example() {
        let c = Bmp180Cal {
            ac1: 408,
            ac2: -72,
            ac3: -14383,
            ac4: 32741,
            ac5: 32757,
            ac6: 23153,
            b1: 6190,
            b2: 4,
            mb: -32768,
            mc: -8711,
            md: 2868,
        };
        assert_eq!(bmp180_comp(&c, 27898, 23843, 0), (15.0, 69964));
    }

    #[test]
    fn aht20_frame_decode_and_crc() {
        let b = [0x1c, 0x80, 0x00, 0x06, 0x00, 0x00];
        let mut f = b.to_vec();
        f.push(crc8sens(&b, 0xff));
        let (t, h, ok) = aht_decode(&f);
        assert!(close(h, 50.0, 3) && close(t, 25.0, 3));
        assert_eq!(ok, Some(true));
        f[6] = 0;
        assert_eq!(aht_decode(&f).2, Some(false));
    }

    #[test]
    fn tsl2561_lux_dark_typical_saturated() {
        assert_eq!(tsl2561_lux(0, 0, 2, false), Some(0.0));
        let l = tsl2561_lux(1000, 200, 2, true).unwrap();
        assert!(close(l, 0.0304 * 1000.0 - 0.062 * 1000.0 * libm::pow(0.2, 1.4), 3));
        assert_eq!(tsl2561_lux(5047, 10, 0, true), None);
    }

    #[test]
    fn veml7700_resolution() {
        assert!(close(veml7700_lux(1000, 0x0000), 57.6, 3));
    }

    #[test]
    fn ads1x15_volts() {
        assert!(close(ads_volts(0x4000, 0x8583, false), 1.024, 4));
        assert!(close(ads_volts(0x4000, 0x8583, true), 1.024, 4));
        assert!(close(ads_volts(0xc000, 0x8383, false), -2.048, 4));
    }

    #[test]
    fn rtc_bcd_decode() {
        let (d, _, _) = rtc_decode(&[0x30, 0x45, 0x13, 0x05, 0x17, 0x10, 0x26]);
        assert_eq!(crate::time::iso(d.unwrap()), "2026-10-17T13:45:30.000Z");
        let (d, _, h12) = rtc_decode(&[0x00, 0x00, 0x61, 0x05, 0x17, 0x10, 0x26]);
        assert!(h12);
        assert_eq!(d.unwrap() / 3_600_000 % 24, 13); // 12 h mode, PM, 1
        assert_eq!(rtc_decode(&[0x00, 0, 0, 0, 1, 0x13, 0]).0, None); // month 13
    }

    #[test]
    fn pi_hat_eeprom_header() {
        let r = eeprom_regs();
        let b: Vec<u8> = (0..64).map(|i| r.get(&i).copied().unwrap_or(0xff)).collect();
        let h = parse_hat_eeprom(&b).unwrap();
        assert_eq!((h.version, h.atoms, h.pid, h.pver), (1, 1, Some(0x1234), Some(2)));
        assert_eq!(h.vendor.as_deref(), Some("BenchBob"));
        assert_eq!(h.product.as_deref(), Some("Bench HAT"));
        assert_eq!(parse_hat_eeprom(&[0xff; 64]), None);
    }
}
