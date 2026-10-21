//! The 1-Wire family table and decoders (w1-families.ts): pure functions over what the
//! kernel's w1 slave drivers expose in sysfs, so they're tested against fixture trees.
//! Which attribute each family has (the 6.18 module set on desky):
//!   w1_therm   0x10 0x22 0x28 0x3B 0x42  w1_slave (scratchpad + CRC + t=), temperature,
//!                                        resolution, ext_power, alarms; master therm_bulk_read
//!   w1_ds2438  0x26  temperature (/256 °C), vad, vdd (/100 V), page0, page1
//!   w1_ds2408  0x29  state, output, activity (1 byte each, binary)
//!   w1_ds2413  0x3A  state (binary: A pin, A latch, B pin, B latch), output
//!   w1_ds2406  0x12  state ('0'..'3' ASCII = flip-flops), output       (no autoload alias!)
//!   w1_ds2423  0x1D  w1_slave: one line per counter page, "... crc=YES c=<n>"
//!   w1_ds2431  0x2D, w1_ds2433 0x23/0x43, w1_ds28e04 0x1C, w1_ds2430 0x14: eeprom
//!   w1_smem    0x01 0x81 (ROM only)
//!   no driver  0x24 0x27 (RTCs): the generic `rw` attribute (select + raw bytes)

use std::sync::OnceLock;

use benchbob_i2c_chips::num::js_num_str;
use benchbob_i2c_chips::time::iso_seconds;
use benchbob_i2c_chips::{V, Val};

#[derive(Debug, Clone, PartialEq)]
pub struct Family {
    pub name: String,
    /// "temp" | "thermocouple" | "battery" | "io" | "counter" | "eeprom" | "id" | "rtc" | "adapter" | "other"
    pub kind: &'static str,
    pub desc: &'static str,
    pub module: Option<&'static str>,
    pub eeprom_size: Option<u32>,
}

type Row = (u8, &'static str, &'static str, &'static str, Option<&'static str>, Option<u32>);

const FAMILIES: &[Row] = &[
    (0x01, "DS2401 / DS1990A", "id", "Silicon serial number / iButton", Some("w1_smem"), None),
    (0x02, "DS1991", "other", "MultiKey iButton", None, None),
    (0x04, "DS2404 / DS1994", "rtc", "EconoRAM time chip", None, None),
    (0x05, "DS2405", "io", "Addressable switch", None, None),
    (0x06, "DS1993", "eeprom", "4 Kbit memory iButton", None, None),
    (0x08, "DS1992", "eeprom", "1 Kbit memory iButton", None, None),
    (0x09, "DS2502 / DS1982", "eeprom", "1 Kbit add-only memory", None, None),
    (0x0a, "DS1995", "eeprom", "16 Kbit memory iButton", None, None),
    (0x0b, "DS2505 / DS1985", "eeprom", "16 Kbit add-only memory", None, None),
    (0x0c, "DS1996", "eeprom", "64 Kbit memory iButton", None, None),
    (0x0f, "DS2506 / DS1986", "eeprom", "64 Kbit add-only memory", None, None),
    (0x10, "DS18S20", "temp", "Temperature (9-bit + extended)", Some("w1_therm"), None),
    (0x12, "DS2406 / DS2407", "io", "Dual addressable switch + 1 Kbit memory", Some("w1_ds2406"), None),
    (0x14, "DS2430A", "eeprom", "256-bit EEPROM", Some("w1_ds2430"), Some(32)),
    (0x18, "DS1963S", "other", "SHA iButton", None, None),
    (0x19, "DS28E17", "adapter", "1-Wire → I²C bridge", Some("w1_ds28e17"), None),
    (0x1a, "DS1963L", "counter", "Monetary iButton", None, None),
    (0x1c, "DS28E04", "eeprom", "4 Kbit EEPROM + PIO", Some("w1_ds28e04"), Some(512)),
    (0x1d, "DS2423", "counter", "4 Kbit RAM + counters", Some("w1_ds2423"), None),
    (0x1f, "DS2409", "adapter", "MicroLAN coupler", None, None),
    (0x20, "DS2450", "other", "Quad A/D converter (no kernel driver)", None, None),
    (0x21, "DS1921", "temp", "Thermochron logger iButton (no kernel driver)", None, None),
    (0x22, "DS1822", "temp", "Econo temperature", Some("w1_therm"), None),
    (0x23, "DS2433", "eeprom", "4 Kbit EEPROM", Some("w1_ds2433"), Some(512)),
    (0x24, "DS2415 / DS1904", "rtc", "Real-time clock", None, None),
    (0x26, "DS2438", "battery", "Battery monitor: temperature, VAD, VDD, current", Some("w1_ds2438"), None),
    (0x27, "DS2417", "rtc", "Real-time clock with interrupt", None, None),
    (0x28, "DS18B20", "temp", "Temperature (9–12 bit)", Some("w1_therm"), None),
    (0x29, "DS2408", "io", "8-channel addressable switch", Some("w1_ds2408"), None),
    (0x2c, "DS2890", "other", "Digital potentiometer", None, None),
    (0x2d, "DS2431", "eeprom", "1 Kbit EEPROM", Some("w1_ds2431"), Some(128)),
    (0x30, "DS2760", "battery", "Li-ion battery monitor", None, None),
    (0x32, "DS2780", "battery", "Fuel gauge", Some("w1_ds2780"), None),
    (0x33, "DS1961S / DS2432", "eeprom", "SHA-1 EEPROM", None, None),
    (0x37, "DS1977", "eeprom", "Password-protected 32 KB iButton", None, None),
    (0x3a, "DS2413", "io", "Dual-channel addressable switch", Some("w1_ds2413"), None),
    (0x3b, "MAX31850 / DS1825", "thermocouple", "Thermocouple converter / temperature", Some("w1_therm"), None),
    (0x3d, "DS2781", "battery", "Fuel gauge", Some("w1_ds2781"), None),
    (0x41, "DS1922 / DS1923", "temp", "Hygrochron / Thermochron logger (no kernel driver)", None, None),
    (0x42, "DS28EA00", "temp", "Temperature with sequence detect + PIO", Some("w1_therm"), None),
    (0x43, "DS28EC20", "eeprom", "20 Kbit EEPROM", Some("w1_ds2433"), Some(2560)),
    (0x51, "DS2751", "battery", "Battery fuel gauge", None, None),
    (0x7e, "EDS00xx", "other", "Embedded Data Systems environmental sensor", None, None),
    (0x81, "DS1420", "adapter", "ID chip inside a DS9490R USB adapter", Some("w1_smem"), None),
];

/// Whether the table knows this family code.
pub fn known(code: u8) -> bool {
    FAMILIES.iter().any(|r| r.0 == code)
}

pub fn family(code: u8) -> Family {
    match FAMILIES.iter().find(|r| r.0 == code) {
        Some(&(_, name, kind, desc, module, eeprom_size)) => Family { name: name.into(), kind, desc, module, eeprom_size },
        None => Family { name: format!("Family 0x{code:02X}"), kind: "other", desc: "Unknown family", module: None, eeprom_size: None },
    }
}

/// Dallas/Maxim CRC-8 (x^8 + x^5 + x^4 + 1, reflected 0x8C), for ROM IDs and scratchpads.
pub fn crc8w1(bytes: &[u8]) -> u8 {
    let mut c = 0u8;
    for &b in bytes {
        let mut b = b;
        for _ in 0..8 {
            let mix = (c ^ b) & 1;
            c >>= 1;
            if mix != 0 {
                c ^= 0x8c;
            }
            b >>= 1;
        }
    }
    c
}

/// "28-0000075b1a2c" → family 0x28 (`parseInt(id.slice(0, 2), 16)`; None for NaN).
pub fn family_of(slave_id: &str) -> Option<u8> {
    let h: String = slave_id.chars().take(2).take_while(|c| c.is_ascii_hexdigit()).collect();
    u8::from_str_radix(&h, 16).ok()
}

/// Check the 8-byte `id` attribute (family, 48-bit serial LE, CRC).
pub fn rom_ok(id: &[u8]) -> bool {
    id.len() == 8 && crc8w1(&id[..7]) == id[7]
}

// ---------- decoded readings ----------

#[derive(Debug, Clone, PartialEq)]
pub struct OwPin {
    pub name: &'static str,
    pub level: u8,
    pub latch: Option<u8>,
    pub out: Option<bool>,
}

/// A slave's decoded state. `warn` is `Some` where the TypeScript decoder sets the key.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Decoded {
    pub values: Vec<Val>,
    pub pins: Option<Vec<OwPin>>,
    pub warn: Option<Vec<String>>,
    pub eeprom: Option<Vec<u8>>,
}

impl Decoded {
    pub fn values(values: Vec<Val>) -> Self {
        Self { values, ..Default::default() }
    }
    pub fn warnings(&self) -> &[String] {
        self.warn.as_deref().unwrap_or(&[])
    }
    pub fn add_warning(&mut self, w: &str) {
        self.warn.get_or_insert_with(Vec::new).push(w.into());
    }
}

fn s16le(lo: u8, hi: u8) -> i32 {
    i16::from_le_bytes([lo, hi]) as i32
}

// ---------- w1_therm ----------

#[derive(Debug, Clone, PartialEq)]
pub struct ThermSlave {
    pub crc_ok: bool,
    pub scratch: Vec<u8>,
    pub t_milli: Option<i64>,
}

/// w1_therm's w1_slave:
///   72 01 4b 46 7f ff 0e 10 57 : crc=57 YES
///   72 01 4b 46 7f ff 0e 10 57 t=23125
pub fn parse_therm_slave(text: &str) -> Option<ThermSlave> {
    static RE: OnceLock<(regex::Regex, regex::Regex)> = OnceLock::new();
    let (head, t) = RE.get_or_init(|| {
        (regex::Regex::new(r"(?i)^((?:[0-9a-f]{2} ){9}): crc=([0-9a-f]{2}) (YES|NO)").unwrap(), regex::Regex::new(r"t=(-?\d+)").unwrap())
    });
    let mut lines = text.trim().split('\n');
    let first = lines.next().filter(|l| !l.is_empty())?;
    let m = head.captures(first)?;
    let scratch = m[1].split_whitespace().filter_map(|x| u8::from_str_radix(x, 16).ok()).collect();
    let t_milli = lines.next().and_then(|l| t.captures(l)).and_then(|c| c[1].parse().ok());
    Some(ThermSlave { crc_ok: &m[3] == "YES", scratch, t_milli })
}

fn temp(k: &str, label: &str, v: Option<f64>, d: u8) -> Val {
    Val::unit(k, label, v, "°C", d)
}

/// Decode a w1_therm scratchpad (9 bytes) for the given family.
pub fn decode_therm(fam: u8, ts: &ThermSlave) -> Decoded {
    let s = &ts.scratch;
    let b = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut warn = Vec::new();
    if !ts.crc_ok {
        warn.push("Scratchpad CRC mismatch: wiring/pull-up problem or a collision; value discarded.".to_string());
    }
    let mut values = Vec::new();
    if fam == 0x3b && b(4) >> 4 == 0x0f {
        // MAX31850: bytes 0-1 thermocouple (14-bit, 0.25 °C) + fault bit, 2-3 cold junction
        // (12-bit, 0.0625 °C) + OC/SCG/SCV.
        let tc = (s16le(b(0), b(1)) >> 2) as f64 * 0.25;
        let cj = (s16le(b(2), b(3)) >> 4) as f64 * 0.0625;
        let fault = b(0) & 1 != 0;
        let faults: Vec<&str> = [(1, "open circuit"), (2, "short to GND"), (4, "short to VDD")]
            .iter()
            .filter(|(m, _)| b(2) & m != 0)
            .map(|(_, n)| *n)
            .collect();
        values.push(temp("t", "Thermocouple", (ts.crc_ok && !fault).then_some(tc), 2));
        values.push(temp("cj", "Cold junction", ts.crc_ok.then_some(cj), 2));
        values.push(Val::new("addr", "Address pins", (b(4) & 0x0f) as f64));
        if fault {
            warn.push(format!("Thermocouple fault: {}", if faults.is_empty() { "unspecified".into() } else { faults.join(", ") }));
        }
        return Decoded { values, warn: Some(warn), ..Default::default() };
    }
    if fam == 0x10 {
        // DS18S20: 0.5 °C + COUNT_REMAIN/COUNT_PER_C extension.
        let raw = s16le(b(0), b(1));
        let t = if b(7) != 0 { (raw >> 1) as f64 - 0.25 + (b(7) as f64 - b(6) as f64) / b(7) as f64 } else { raw as f64 / 2.0 };
        values.push(temp("t", "Temperature", ts.crc_ok.then(|| benchbob_i2c_chips::num::js_round(t * 1000.0) / 1000.0), 3));
        values.push(Val::new("res", "Resolution", "9-bit + extended"));
    } else {
        let raw = s16le(b(0), b(1));
        let bits = 9 + ((b(4) >> 5) & 3) as usize;
        let t = raw as f64 / 16.0;
        values.push(temp(
            "t",
            "Temperature",
            ts.crc_ok.then_some(t),
            match bits {
                9 => 1,
                10 => 2,
                _ => 3,
            },
        ));
        let step = [0.5, 0.25, 0.125, 0.0625][bits - 9];
        let ms = [94, 188, 375, 750][bits - 9];
        values.push(Val::new("res", "Resolution", format!("{bits}-bit ({} °C, {ms} ms)", js_num_str(step))));
        if fam == 0x3b {
            values.push(Val::new("addr", "Address pins", (b(4) & 0x0f) as f64));
        }
    }
    // 85.0 °C is the power-on reset value: a sensor that lost power mid-conversion, or a
    // parasite-powered sensor without enough strong pull-up, reads exactly this.
    if ts.crc_ok && b(0) == 0x50 && b(1) == 0x05 && fam != 0x10 {
        warn.push("85 °C is the power-on value: the conversion didn't run (power glitch or parasite power without strong pull-up).".into());
    }
    if ts.crc_ok && fam == 0x10 && b(0) == 0xaa && b(1) == 0x00 {
        warn.push("85 °C is the power-on value: the conversion didn't run.".into());
    }
    if ts.crc_ok && s.iter().all(|x| *x == 0xff) {
        warn.push("All-ones scratchpad: the sensor isn't responding.".into());
    }
    if ts.crc_ok && s.iter().take(8).all(|x| *x == 0) {
        warn.push("All-zero scratchpad: the data line is stuck low.".into());
    }
    values.push(Val::new("alarm", "Alarm TH/TL", format!("{} / {} °C", b(2) as i8, b(3) as i8)));
    Decoded { values, warn: Some(warn), ..Default::default() }
}

// ---------- other families ----------

/// DS2413 state byte: bit0 PIOA pin, bit1 PIOA latch, bit2 PIOB pin, bit3 PIOB latch; high nibble = complement.
pub fn decode_ds2413(state: u8) -> Decoded {
    let ok = (!state >> 4) & 0x0f == state & 0x0f;
    let pin = |name, level: u8, latch: u8| OwPin { name, level, latch: Some(latch), out: Some(latch == 0) };
    Decoded {
        values: vec![Val::new("state", "State", format!("0x{state:02x}"))],
        pins: Some(vec![pin("PIO-A", state & 1, (state >> 1) & 1), pin("PIO-B", (state >> 2) & 1, (state >> 3) & 1)]),
        warn: Some(if ok { vec![] } else { vec!["State byte failed its complement check.".into()] }),
        eeprom: None,
    }
}

const P8: [&str; 8] = ["P0", "P1", "P2", "P3", "P4", "P5", "P6", "P7"];

/// DS2408 state/output/activity bytes. A PIO with output latch 0 has its transistor on (pulled low).
pub fn decode_ds2408(state: u8, output: u8, activity: Option<u8>) -> Decoded {
    let pins = (0..8)
        .map(|i| OwPin { name: P8[i], level: (state >> i) & 1, latch: Some((output >> i) & 1), out: Some((output >> i) & 1 == 0) })
        .collect();
    let mut values =
        vec![Val::new("state", "Pin state", format!("0x{state:02x}")), Val::new("output", "Output latch", format!("0x{output:02x}"))];
    if let Some(a) = activity {
        values.push(Val::new("activity", "Activity latch", format!("0x{a:02x}")));
    }
    Decoded { values, pins: Some(pins), ..Default::default() }
}

/// DS2406 `state`: '0'..'3' (ASCII) = PIO-A / PIO-B flip-flops; 1 = transistor off.
pub fn decode_ds2406(raw: u8) -> Decoded {
    let v = if raw >= 0x30 { raw - 0x30 } else { raw };
    let pin = |name, b: u8| OwPin { name, level: b, latch: Some(b), out: Some(b == 0) };
    Decoded {
        values: vec![Val::new("ff", "Flip-flops", v as f64)],
        pins: Some(vec![pin("PIO-A", v & 1), pin("PIO-B", (v >> 1) & 1)]),
        ..Default::default()
    }
}

/// DS2423 w1_slave: one line per counter page, "<hex bytes> crc=YES c=<n>". Counters A/B are on pages 14/15.
pub fn parse_ds2423(text: &str) -> Decoded {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"crc=(YES|NO)(?: c=(\d+))?").unwrap());
    let lines: Vec<&str> = text.trim().split('\n').filter(|l| !l.is_empty()).collect();
    let first_page = 16 - lines.len() as i64;
    let mut values = Vec::new();
    let mut warn = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(m) = re.captures(l) else { continue };
        let Some(c) = m.get(2).and_then(|c| c.as_str().parse::<f64>().ok()) else { continue };
        let ok = &m[1] == "YES";
        let page = first_page + i as i64;
        let label = match page {
            14 => "Counter A".to_string(),
            15 => "Counter B".to_string(),
            p => format!("Page {p} counter"),
        };
        values.push(Val::new(format!("c{page}"), label.clone(), if ok { V::Num(c) } else { V::Null }));
        if !ok {
            warn.push(format!("{label}: CRC mismatch"));
        }
    }
    Decoded { values, warn: Some(warn), ..Default::default() }
}

fn int_text(s: &str) -> Option<f64> {
    let t = s.trim();
    let digits = t.strip_prefix('-').unwrap_or(t);
    (!digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit())).then(|| t.parse().ok()).flatten()
}

/// DS2438 text attributes (°C×256, V×100) + the page0 current register.
pub fn decode_ds2438(temp_s: &str, vad: &str, vdd: &str, page0: Option<&[u8]>, rsens: Option<f64>) -> Decoded {
    let r = benchbob_i2c_chips::num::js_round;
    let mut values = vec![
        temp("t", "Temperature", int_text(temp_s).map(|t| r((t / 256.0) * 100.0) / 100.0), 2),
        Val::unit("vad", "VAD", int_text(vad).map(|a| a / 100.0), "V", 2),
        Val::unit("vdd", "VDD", int_text(vdd).map(|d| d / 100.0), "V", 2),
    ];
    if let Some(p) = page0.filter(|p| p.len() >= 7) {
        let vsens = s16le(p[5], p[6]) as f64 * 0.2441; // mV
        values.push(Val::unit("vsens", "Current sense", r(vsens * 100.0) / 100.0, "mV", 2));
        if let Some(rs) = rsens.filter(|x| *x != 0.0 && !x.is_nan()) {
            values.push(Val::unit("i", "Current", r((vsens / rs) * 10.0) / 10.0, "mA", 1));
        }
    }
    Decoded::values(values)
}

/// DS1904/DS2415/DS2417 "read clock" (0x66) reply: control byte + 32-bit seconds (LE).
pub fn decode_rtc(b: &[u8], now_ms: i64) -> Decoded {
    if b.len() < 5 {
        return Decoded { warn: Some(vec!["Short clock read".into()]), ..Default::default() };
    }
    let secs = u32::from_le_bytes([b[1], b[2], b[3], b[4]]) as i64;
    let running = b[0] & 0x0c == 0x0c; // OSC bits
    Decoded::values(vec![
        Val::new("time", "Clock (UTC)", iso_seconds(secs * 1000)),
        Val::unit("drift", "vs system clock", benchbob_i2c_chips::num::js_round(secs as f64 - now_ms as f64 / 1000.0), "s", 0),
        Val::new("osc", "Oscillator", if running { "running" } else { "stopped" }),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{FIX, rom_bytes, therm_text};

    fn v<'a>(d: &'a Decoded, k: &str) -> &'a V {
        &d.values.iter().find(|x| x.k == k).unwrap().v
    }

    #[test]
    fn maxim_crc8_of_the_classic_ds18b20_scratchpad() {
        // Kernel docs example: 72 01 4b 46 7f ff 0e 10 : crc=57
        assert_eq!(crc8w1(&[0x72, 0x01, 0x4b, 0x46, 0x7f, 0xff, 0x0e, 0x10]), 0x57);
    }

    #[test]
    fn rom_id_bytes_and_crc_including_the_ghost_id_seen_on_desky() {
        assert!(rom_ok(&rom_bytes(FIX.ds18b20)));
        // desky: /sys/bus/w1/devices/00-280000000000/id = 00 00 00 00 00 00 28 e1
        assert_eq!(rom_bytes("00-280000000000"), vec![0, 0, 0, 0, 0, 0, 0x28, 0xe1]);
        assert!(!rom_ok(&[0, 0, 0, 0, 0, 0, 0x28, 0xe0]));
    }

    #[test]
    fn ds18b20_23_125_at_12_bit() {
        let ts = parse_therm_slave(&therm_text(&[0x72, 0x01, 0x4b, 0x46, 0x7f, 0xff, 0x0e, 0x10], 23125, false)).unwrap();
        assert!(ts.crc_ok);
        assert_eq!(ts.t_milli, Some(23125));
        let d = decode_therm(0x28, &ts);
        assert_eq!(v(&d, "t"), &V::Num(23.125));
        assert!(v(&d, "res").str().unwrap().starts_with("12-bit"));
        assert_eq!(v(&d, "res").str().unwrap(), "12-bit (0.0625 °C, 750 ms)");
        assert_eq!(d.warn, Some(vec![]));
    }

    #[test]
    fn crc_no_withholds_the_value_and_warns() {
        let d =
            decode_therm(0x28, &parse_therm_slave(&therm_text(&[0x72, 0x01, 0x4b, 0x46, 0x7f, 0xff, 0x0e, 0x10], 23125, true)).unwrap());
        assert_eq!(v(&d, "t"), &V::Null);
        assert!(d.warnings()[0].contains("CRC"));
    }

    #[test]
    fn the_85_degree_power_on_value_is_flagged() {
        let d =
            decode_therm(0x28, &parse_therm_slave(&therm_text(&[0x50, 0x05, 0x4b, 0x46, 0x7f, 0xff, 0x0c, 0x10], 85000, false)).unwrap());
        assert!(d.warnings().join(",").contains("power-on"));
    }

    #[test]
    fn ds18s20_extended_resolution() {
        let d =
            decode_therm(0x10, &parse_therm_slave(&therm_text(&[0x32, 0x00, 0x4b, 0x46, 0xff, 0xff, 0x0c, 0x10], 25000, false)).unwrap());
        assert_eq!(v(&d, "t"), &V::Num(25.0));
        let d2 = decode_therm(0x10, &parse_therm_slave(&therm_text(&[0x31, 0x00, 0x4b, 0x46, 0xff, 0xff, 0x05, 0x10], 0, false)).unwrap());
        assert!((v(&d2, "t").num().unwrap() - (24.0 - 0.25 + 11.0 / 16.0)).abs() < 5e-4);
    }

    #[test]
    fn ds1822_negative_10_bit() {
        let d =
            decode_therm(0x22, &parse_therm_slave(&therm_text(&[0x58, 0xff, 0x4b, 0x46, 0x3f, 0xff, 0x08, 0x10], -10500, false)).unwrap());
        assert_eq!(v(&d, "t"), &V::Num(-10.5));
        assert!(v(&d, "res").str().unwrap().starts_with("10-bit"));
        assert_eq!(v(&d, "alarm").str().unwrap(), "75 / 70 °C");
    }

    #[test]
    fn max31850_fault_bits_and_cold_junction() {
        let d =
            decode_therm(0x3b, &parse_therm_slave(&therm_text(&[0x45, 0x06, 0x11, 0x18, 0xf3, 0xff, 0xff, 0xff], 100250, false)).unwrap());
        assert_eq!(v(&d, "t"), &V::Null); // faulted
        assert_eq!(v(&d, "cj"), &V::Num(24.0625));
        assert_eq!(v(&d, "addr"), &V::Num(3.0));
        assert!(d.warnings().join(",").contains("open circuit"));
        let ok =
            decode_therm(0x3b, &parse_therm_slave(&therm_text(&[0x44, 0x06, 0x10, 0x18, 0xf3, 0xff, 0xff, 0xff], 100250, false)).unwrap());
        assert_eq!(v(&ok, "t"), &V::Num(100.25));
    }

    #[test]
    fn ds2413_state_byte() {
        let d = decode_ds2413(0xc3);
        assert_eq!(
            d.pins.clone().unwrap(),
            vec![
                OwPin { name: "PIO-A", level: 1, latch: Some(1), out: Some(false) },
                OwPin { name: "PIO-B", level: 0, latch: Some(0), out: Some(true) },
            ]
        );
        assert!(d.warnings().is_empty());
        assert!(decode_ds2413(0x03).warnings()[0].contains("complement"));
    }

    #[test]
    fn ds2408_pins() {
        let d = decode_ds2408(0xf5, 0xfe, Some(0));
        let p = d.pins.unwrap();
        assert_eq!(p[0], OwPin { name: "P0", level: 1, latch: Some(0), out: Some(true) });
        assert_eq!(p[1].level, 0);
    }

    #[test]
    fn ds2406_ascii_state() {
        assert_eq!(decode_ds2406(0x31).pins.unwrap().iter().map(|p| p.out.unwrap()).collect::<Vec<_>>(), vec![false, true]);
    }

    #[test]
    fn ds2423_counters_a_b_on_pages_14_15() {
        let line = |c: u32, ok: &str| format!("{}crc={ok} c={c}", "00 ".repeat(42));
        let d = parse_ds2423(&[line(1, "YES"), line(2, "YES"), line(12345, "YES"), line(678, "NO")].join("\n"));
        assert_eq!(d.values.iter().find(|x| x.label == "Counter A").unwrap().v, V::Num(12345.0));
        assert_eq!(d.values.iter().find(|x| x.label == "Counter B").unwrap().v, V::Null);
        assert!(d.warnings()[0].contains("Counter B"));
    }

    #[test]
    fn ds2438_scaling() {
        let d = decode_ds2438("6016", "312", "498", Some(&[0x0f, 0, 0, 0, 0, 0xcd, 0x00, 0]), Some(0.05));
        assert_eq!(d.values.iter().map(|x| x.v.num().unwrap()).collect::<Vec<_>>(), vec![23.5, 3.12, 4.98, 50.04, 1000.8]);
    }

    #[test]
    fn ds1904_clock() {
        let secs = (benchbob_i2c_chips::time::utc_ms(2026, 9, 17, 12, 0, 0) / 1000) as u32;
        let mut b = vec![0x0c];
        b.extend(secs.to_le_bytes());
        let d = decode_rtc(&b, secs as i64 * 1000);
        assert_eq!(d.values[0].v, V::Str("2026-10-17 12:00:00".into()));
        assert_eq!(d.values[1].v, V::Num(0.0));
        assert_eq!(d.values[2].v, V::Str("running".into()));
    }

    #[test]
    fn family_table_covers_the_task_list() {
        for f in [0x28, 0x10, 0x22, 0x42, 0x3b, 0x26, 0x29, 0x3a, 0x12, 0x1d, 0x2d, 0x23, 0x43, 0x01, 0x24, 0x27, 0x81] {
            assert!(known(f), "{f:#x}");
        }
        assert_eq!(family(0x7f).name, "Family 0x7F");
        assert_eq!(family_of("28-0000075b1a2c"), Some(0x28));
    }
}
