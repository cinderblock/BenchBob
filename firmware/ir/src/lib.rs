//! Consumer infrared, as timings: decode a captured frame into protocol + address +
//! command, and encode one for sending.
//!
//! A frame is a list of durations in µs, alternating mark (carrier on) and space, starting
//! with a mark; a capture usually ends on the last mark (the trailing space is the gap
//! before the next frame). Decoding allows ±30 % (or ±150 µs for short pulses): remotes and
//! receivers are sloppy, and a demodulating receiver lengthens marks and shortens spaces.
//!
//! | Protocol | Carrier | Address | Command | Notes |
//! | --- | --- | --- | --- | --- |
//! | NEC | 38 kHz | 8 bits (16 = extended NEC) | 8 bits | plus its 9 ms / 2.25 ms repeat code |
//! | Samsung32 | 38 kHz | 8 bits | 8 bits | NEC-like, 4.5 ms leader |
//! | Sony SIRC | 40 kHz | 5, 8 or 13 bits | 7 bits | 12, 15 or 20 bits; sent three times |
//! | RC5 | 36 kHz | 5 bits | 7 bits (RC5X) | toggle bit |
//! | RC6 mode 0 | 36 kHz | 8 bits | 8 bits | toggle bit |
//! | JVC | 38 kHz | 8 bits | 8 bits | repeats without the leader |
//!
//! Anything else stays raw; [`pronto`] converts raw timings to and from Pronto hex, the
//! common interchange format.

#![cfg_attr(not(test), no_std)]

pub mod pronto;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    Nec,
    /// NEC with a 16-bit address (no inverted address byte).
    NecExt,
    /// NEC's "key still held" frame: no address or command.
    NecRepeat,
    Samsung,
    Sony12,
    Sony15,
    Sony20,
    Rc5,
    Rc6,
    Jvc,
}

pub const PROTOCOLS: [Protocol; 10] = [
    Protocol::Nec,
    Protocol::NecExt,
    Protocol::NecRepeat,
    Protocol::Samsung,
    Protocol::Sony12,
    Protocol::Sony15,
    Protocol::Sony20,
    Protocol::Rc5,
    Protocol::Rc6,
    Protocol::Jvc,
];

impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Nec => "nec",
            Protocol::NecExt => "necx",
            Protocol::NecRepeat => "nec-repeat",
            Protocol::Samsung => "samsung",
            Protocol::Sony12 => "sony12",
            Protocol::Sony15 => "sony15",
            Protocol::Sony20 => "sony20",
            Protocol::Rc5 => "rc5",
            Protocol::Rc6 => "rc6",
            Protocol::Jvc => "jvc",
        }
    }

    pub fn parse(s: &str) -> Option<Protocol> {
        PROTOCOLS.into_iter().find(|p| p.name().eq_ignore_ascii_case(s))
    }

    pub fn carrier_hz(self) -> u32 {
        match self {
            Protocol::Sony12 | Protocol::Sony15 | Protocol::Sony20 => 40_000,
            Protocol::Rc5 | Protocol::Rc6 => 36_000,
            _ => 38_000,
        }
    }

    /// The largest address and command this protocol carries.
    pub fn limits(self) -> (u32, u32) {
        match self {
            Protocol::Nec | Protocol::Samsung | Protocol::Jvc | Protocol::Rc6 => (0xFF, 0xFF),
            Protocol::NecExt => (0xFFFF, 0xFF),
            Protocol::NecRepeat => (0, 0),
            Protocol::Sony12 => (0x1F, 0x7F),
            Protocol::Sony15 => (0xFF, 0x7F),
            Protocol::Sony20 => (0x1FFF, 0x7F),
            Protocol::Rc5 => (0x1F, 0x7F),
        }
    }

    /// Times a sender should send the frame for one press (Sony needs three), and the
    /// gap from the start of one to the next (µs).
    pub fn repeats(self) -> (u8, u32) {
        match self {
            Protocol::Sony12 | Protocol::Sony15 | Protocol::Sony20 => (3, 45_000),
            Protocol::Rc5 | Protocol::Rc6 => (1, 114_000),
            _ => (1, 108_000),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Code {
    pub protocol: Protocol,
    pub address: u32,
    pub command: u32,
    /// RC5 / RC6: flips on each new press (so a receiver can tell a repeat from a second press).
    pub toggle: bool,
}

/// Longest frame any of these makes: RC6's 20 bits as up to 2 halves each, plus leaders.
pub const MAX_FRAME: usize = 72;

fn near(v: u32, want: u32) -> bool {
    let tol = (want * 3 / 10).max(150);
    v.abs_diff(want) <= tol
}

/// A pulse-distance frame: leader mark/space, then per bit a mark and a short (0) or long
/// (1) space, LSB first, and a final mark.
fn decode_distance(t: &[u32], lead: (u32, u32), mark: u32, zero: u32, one: u32, bits: usize) -> Option<u64> {
    // Exactly this many timings (a trailing gap allowed): JVC's 16 bits mustn't match the
    // first half of an NEC frame.
    let len = 2 + 2 * bits + 1;
    if !(len..=len + 1).contains(&t.len()) || !near(t[0], lead.0) || !near(t[1], lead.1) {
        return None;
    }
    let mut v = 0u64;
    for i in 0..bits {
        let (m, s) = (t[2 + 2 * i], t[3 + 2 * i]);
        if !near(m, mark) {
            return None;
        }
        if near(s, one) {
            v |= 1 << i;
        } else if !near(s, zero) {
            return None;
        }
    }
    near(t[2 + 2 * bits], mark).then_some(v)
}

fn encode_distance(lead: (u32, u32), mark: u32, zero: u32, one: u32, v: u64, bits: usize, out: &mut [u32]) -> usize {
    let mut n = 0;
    let mut push = |d: u32| {
        out[n] = d;
        n += 1;
    };
    push(lead.0);
    push(lead.1);
    for i in 0..bits {
        push(mark);
        push(if v >> i & 1 != 0 { one } else { zero });
    }
    push(mark);
    n
}

const NEC_LEAD: (u32, u32) = (9000, 4500);
const SAMSUNG_LEAD: (u32, u32) = (4500, 4500);
const JVC_LEAD: (u32, u32) = (8400, 4200);
const RC5_T: u32 = 889;
const RC6_T: u32 = 444;

fn decode_nec(t: &[u32]) -> Option<Code> {
    if t.len() <= 4 && t.len() >= 3 && near(t[0], 9000) && near(t[1], 2250) && near(t[2], 562) {
        return Some(Code { protocol: Protocol::NecRepeat, address: 0, command: 0, toggle: false });
    }
    let v = decode_distance(t, NEC_LEAD, 562, 562, 1687, 32)?;
    let (a0, a1, c, ci) = ((v & 0xFF) as u32, (v >> 8 & 0xFF) as u32, (v >> 16 & 0xFF) as u32, (v >> 24 & 0xFF) as u32);
    if c ^ ci != 0xFF {
        return None;
    }
    if a0 ^ a1 == 0xFF {
        Some(Code { protocol: Protocol::Nec, address: a0, command: c, toggle: false })
    } else {
        Some(Code { protocol: Protocol::NecExt, address: a0 | a1 << 8, command: c, toggle: false })
    }
}

fn decode_samsung(t: &[u32]) -> Option<Code> {
    let v = decode_distance(t, SAMSUNG_LEAD, 560, 560, 1690, 32)?;
    let (a0, a1, c, ci) = ((v & 0xFF) as u32, (v >> 8 & 0xFF) as u32, (v >> 16 & 0xFF) as u32, (v >> 24 & 0xFF) as u32);
    (a0 == a1 && c ^ ci == 0xFF).then_some(Code { protocol: Protocol::Samsung, address: a0, command: c, toggle: false })
}

fn decode_jvc(t: &[u32]) -> Option<Code> {
    let v = decode_distance(t, JVC_LEAD, 526, 526, 1574, 16)?;
    Some(Code { protocol: Protocol::Jvc, address: (v & 0xFF) as u32, command: (v >> 8 & 0xFF) as u32, toggle: false })
}

/// Sony: a 2.4 ms leader, then per bit a 1.2 ms (1) or 0.6 ms (0) mark and a 0.6 ms
/// space, LSB first: 7 command bits, then 5, 8 or 13 address bits.
fn decode_sony(t: &[u32]) -> Option<Code> {
    if t.len() < 2 || !near(t[0], 2400) || !near(t[1], 600) {
        return None;
    }
    // Marks at 2, 4, …; the last bit has no space after it in a capture.
    let marks = (t.len() - 1) / 2;
    let protocol = match marks {
        12 => Protocol::Sony12,
        15 => Protocol::Sony15,
        20 => Protocol::Sony20,
        _ => return None,
    };
    let mut v = 0u32;
    for i in 0..marks {
        let m = t[2 + 2 * i];
        if near(m, 1200) {
            v |= 1 << i;
        } else if !near(m, 600) {
            return None;
        }
        if let Some(&s) = t.get(3 + 2 * i)
            && i + 1 < marks
            && !near(s, 600)
        {
            return None;
        }
    }
    Some(Code { protocol, address: v >> 7, command: v & 0x7F, toggle: false })
}

/// Durations as a run of half-bit (unit) levels: true = mark. None if a duration isn't
/// 1–`max` units.
fn units(t: &[u32], unit: u32, max: u32, out: &mut [bool; 2 * MAX_FRAME]) -> Option<usize> {
    let mut n = 0;
    for (i, &d) in t.iter().enumerate() {
        let k = (d + unit / 2) / unit;
        if k == 0 || k > max || !near(d, k * unit) {
            return None;
        }
        for _ in 0..k {
            *out.get_mut(n)? = i % 2 == 0;
            n += 1;
        }
    }
    Some(n)
}

/// RC5: Manchester, 889 µs halves; 1 = space then mark. 14 bits: start 1, field (inverted
/// command bit 6), toggle, 5 address bits, 6 command bits, MSB first. The first half of
/// the first bit is a space, so it doesn't show in a capture.
fn decode_rc5(t: &[u32]) -> Option<Code> {
    let mut lv = [false; 2 * MAX_FRAME];
    let mut n = units(t, RC5_T, 2, &mut lv)?;
    // The hidden leading space half, and a trailing space half the capture may lack.
    lv.copy_within(0..n, 1);
    lv[0] = false;
    n += 1;
    if n == 27 {
        lv[27] = false;
        n = 28;
    }
    if n != 28 {
        return None;
    }
    let mut v = 0u32;
    for b in 0..14 {
        let bit = match (lv[2 * b], lv[2 * b + 1]) {
            (false, true) => 1,
            (true, false) => 0,
            _ => return None,
        };
        v = v << 1 | bit;
    }
    if v >> 13 != 1 {
        return None;
    }
    let field = v >> 12 & 1;
    let command = (v & 0x3F) | if field == 0 { 0x40 } else { 0 };
    Some(Code { protocol: Protocol::Rc5, address: v >> 6 & 0x1F, command, toggle: v >> 11 & 1 != 0 })
}

/// RC6 mode 0: a 6-unit mark and 2-unit space leader (unit 444 µs), then Manchester bits
/// with 1 = mark then space: start (1), mode (3 bits, 0), toggle (double-length halves),
/// 8 address and 8 command bits, MSB first.
fn decode_rc6(t: &[u32]) -> Option<Code> {
    if t.len() < 3 || !near(t[0], 6 * RC6_T) || !near(t[1], 2 * RC6_T) {
        return None;
    }
    let mut lv = [false; 2 * MAX_FRAME];
    let mut n = units(&t[2..], RC6_T, 3, &mut lv)?;
    // 1 + 3 bits (2 units each), toggle (4 units), 16 bits (2 each): 44 units.
    while n < 44 {
        lv[n] = false; // a trailing space the capture didn't see
        n += 1;
    }
    if n != 44 {
        return None;
    }
    let bit = |at: usize, w: usize| -> Option<u32> {
        let (a, b) = (lv[at], lv[at + w]);
        for k in 1..w {
            if lv[at + k] != a || lv[at + w + k] != b {
                return None;
            }
        }
        match (a, b) {
            (true, false) => Some(1),
            (false, true) => Some(0),
            _ => None,
        }
    };
    if bit(0, 1)? != 1 {
        return None;
    }
    let mode = bit(2, 1)? << 2 | bit(4, 1)? << 1 | bit(6, 1)?;
    if mode != 0 {
        return None;
    }
    let toggle = bit(8, 2)? != 0;
    let mut v = 0u32;
    for k in 0..16 {
        v = v << 1 | bit(12 + 2 * k, 1)?;
    }
    Some(Code { protocol: Protocol::Rc6, address: v >> 8, command: v & 0xFF, toggle })
}

/// Recognise a captured frame. None: not one of these protocols (keep it raw).
pub fn decode(t: &[u32]) -> Option<Code> {
    decode_nec(t)
        .or_else(|| decode_samsung(t))
        .or_else(|| decode_jvc(t))
        .or_else(|| decode_sony(t))
        .or_else(|| decode_rc6(t))
        .or_else(|| decode_rc5(t))
}

/// Manchester levels (one entry per unit, true = mark) into durations.
fn from_units(lv: &[bool], unit: u32, out: &mut [u32]) -> usize {
    let mut n = 0;
    let mut i = 0;
    // Skip a leading space: a frame starts on a mark.
    while i < lv.len() && !lv[i] {
        i += 1;
    }
    while i < lv.len() {
        let level = lv[i];
        let mut k = 0;
        while i < lv.len() && lv[i] == level {
            k += 1;
            i += 1;
        }
        // A trailing space is the gap: leave it off.
        if !level && i == lv.len() {
            break;
        }
        out[n] = k * unit;
        n += 1;
    }
    n
}

/// The timings for `c` (one frame) into `out` (at least [`MAX_FRAME`]). None when the
/// address or command doesn't fit the protocol.
pub fn encode(c: &Code, out: &mut [u32]) -> Option<usize> {
    let (amax, cmax) = c.protocol.limits();
    if c.address > amax || c.command > cmax || out.len() < MAX_FRAME {
        return None;
    }
    let (a, cmd) = (c.address as u64, c.command as u64);
    Some(match c.protocol {
        Protocol::Nec => encode_distance(NEC_LEAD, 562, 562, 1687, a | (!a & 0xFF) << 8 | cmd << 16 | (!cmd & 0xFF) << 24, 32, out),
        Protocol::NecExt => encode_distance(NEC_LEAD, 562, 562, 1687, a | cmd << 16 | (!cmd & 0xFF) << 24, 32, out),
        Protocol::NecRepeat => {
            out[..3].copy_from_slice(&[9000, 2250, 562]);
            3
        }
        Protocol::Samsung => encode_distance(SAMSUNG_LEAD, 560, 560, 1690, a | a << 8 | cmd << 16 | (!cmd & 0xFF) << 24, 32, out),
        Protocol::Jvc => encode_distance(JVC_LEAD, 526, 526, 1574, a | cmd << 8, 16, out),
        Protocol::Sony12 | Protocol::Sony15 | Protocol::Sony20 => {
            let bits = match c.protocol {
                Protocol::Sony12 => 12,
                Protocol::Sony15 => 15,
                _ => 20,
            };
            let v = cmd | a << 7;
            let mut n = 0;
            out[0] = 2400;
            out[1] = 600;
            n += 2;
            for i in 0..bits {
                out[n] = if v >> i & 1 != 0 { 1200 } else { 600 };
                n += 1;
                if i + 1 < bits {
                    out[n] = 600;
                    n += 1;
                }
            }
            n
        }
        Protocol::Rc5 => {
            let field = if cmd & 0x40 != 0 { 0 } else { 1 };
            let v: u32 = 1 << 13 | field << 12 | (c.toggle as u32) << 11 | (a as u32) << 6 | (cmd as u32 & 0x3F);
            let mut lv = [false; 28];
            for b in 0..14 {
                let one = v >> (13 - b) & 1 != 0;
                lv[2 * b] = !one;
                lv[2 * b + 1] = one;
            }
            from_units(&lv, RC5_T, out)
        }
        Protocol::Rc6 => {
            let mut lv = [false; 52];
            let mut n = 0;
            let mut put = |one: bool, w: usize| {
                for _ in 0..w {
                    lv[n] = one;
                    n += 1;
                }
                for _ in 0..w {
                    lv[n] = !one;
                    n += 1;
                }
            };
            // Leader: 6 units mark, 2 space.
            let mut lead = [true; 8];
            lead[6] = false;
            lead[7] = false;
            put(true, 1); // start
            for _ in 0..3 {
                put(false, 1); // mode 0
            }
            put(c.toggle, 2);
            let v = (a as u32) << 8 | cmd as u32;
            for k in (0..16).rev() {
                put(v >> k & 1 != 0, 1);
            }
            let mut all = [false; 60];
            all[..8].copy_from_slice(&lead);
            all[8..8 + n].copy_from_slice(&lv[..n]);
            from_units(&all[..8 + n], RC6_T, out)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(c: Code) {
        let mut t = [0u32; MAX_FRAME];
        let n = encode(&c, &mut t).unwrap();
        assert_eq!(n % 2, 1, "{c:?}: a frame ends on a mark");
        assert_eq!(decode(&t[..n]), Some(c), "{c:?} → {:?}", &t[..n]);
        // A demodulating receiver stretches marks ~10 % and shortens spaces as much.
        let skew: std::vec::Vec<u32> = t[..n].iter().enumerate().map(|(i, &d)| if i % 2 == 0 { d * 11 / 10 } else { d * 9 / 10 }).collect();
        assert_eq!(decode(&skew), Some(c), "{c:?} skewed");
    }

    #[test]
    fn every_protocol_round_trips() {
        let c = |protocol, address, command, toggle| Code { protocol, address, command, toggle };
        for t in [false, true] {
            round_trip(c(Protocol::Rc5, 0x05, 0x35, t));
            round_trip(c(Protocol::Rc5, 0x1F, 0x7F, t)); // RC5X: command bit 6
            round_trip(c(Protocol::Rc6, 0x04, 0x0C, t));
            round_trip(c(Protocol::Rc6, 0xFF, 0x00, t));
        }
        round_trip(c(Protocol::Nec, 0x04, 0x08, false));
        // (An "extended" address whose bytes are complements is plain NEC on the wire.)
        round_trip(c(Protocol::NecExt, 0x1234, 0x15, false));
        round_trip(c(Protocol::NecRepeat, 0, 0, false));
        round_trip(c(Protocol::Samsung, 0x07, 0x02, false));
        round_trip(c(Protocol::Jvc, 0x03, 0x17, false));
        round_trip(c(Protocol::Sony12, 0x01, 0x15, false));
        round_trip(c(Protocol::Sony15, 0x97, 0x2A, false));
        round_trip(c(Protocol::Sony20, 0x1A3A, 0x40, false));
    }

    #[test]
    fn known_frames() {
        // NEC, address 0x00 command 0x45 (the "CH-" of the common 21-key remote): bytes
        // 00 FF 45 BA, LSB first.
        let mut t = [0u32; MAX_FRAME];
        let n = encode(&Code { protocol: Protocol::Nec, address: 0, command: 0x45, toggle: false }, &mut t).unwrap();
        assert_eq!(n, 67);
        assert_eq!(&t[..4], &[9000, 4500, 562, 562]);
        assert_eq!(t[2 * 16 + 3], 1687, "bit 16 (command bit 0) is 1");
        // Sony 12: 13 marks' worth of timings minus the last space: 2 + 12 + 11 = 25.
        let n = encode(&Code { protocol: Protocol::Sony12, address: 1, command: 0x15, toggle: false }, &mut t).unwrap();
        assert_eq!(n, 25);
        assert_eq!(&t[..4], &[2400, 600, 1200, 600]);
    }

    #[test]
    fn rejects_noise_and_out_of_range() {
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[9000, 4500, 562]), None);
        assert_eq!(decode(&[1000, 1000, 1000, 1000, 1000]), None);
        let mut t = [0u32; MAX_FRAME];
        let n = encode(&Code { protocol: Protocol::Nec, address: 1, command: 2, toggle: false }, &mut t).unwrap();
        t[40] = 3000; // one space far out of spec
        assert_eq!(decode(&t[..n]), None);
        assert!(encode(&Code { protocol: Protocol::Nec, address: 0x100, command: 0, toggle: false }, &mut t).is_none());
        assert!(encode(&Code { protocol: Protocol::Sony12, address: 0x20, command: 0, toggle: false }, &mut t).is_none());
        assert_eq!(Protocol::parse("NECX"), Some(Protocol::NecExt));
        assert_eq!(Protocol::parse("x"), None);
    }
}
