//! CAN frame helpers (the pure half of `peripherals/lib/cansock.ts`): frame encode/decode,
//! the candump log and cansend formats, error-frame decoding, bus-load bit counting, the
//! socketcand ASCII protocol, filters and the auto-bitrate decision. No I/O, so it is tested
//! on any OS.
//!
//! Struct layouts (checked against the kernel headers on desky, see plans/peripherals-can.md):
//! can_frame is 16 bytes (id@0 len@4 data@8), canfd_frame is 72 (flags@5).

use std::sync::OnceLock;

use benchbob_core::http::{js_number, js_string};
use regex::Regex;
use serde_json::Value;

pub const CAN_EFF_FLAG: u32 = 0x8000_0000;
pub const CAN_RTR_FLAG: u32 = 0x4000_0000;
pub const CAN_ERR_FLAG: u32 = 0x2000_0000;
pub const CAN_SFF_MASK: u32 = 0x7ff;
pub const CAN_EFF_MASK: u32 = 0x1fff_ffff;
pub const CAN_ERR_MASK: u32 = 0x1fff_ffff;
pub const CANFD_BRS: u8 = 0x01;
pub const CANFD_ESI: u8 = 0x02;
pub const CANFD_FDF: u8 = 0x04;

pub const CAN_MTU: usize = 16;
pub const CANFD_MTU: usize = 72;

/// A decoded frame. `ts` is Unix epoch milliseconds (fractional), taken when the read landed.
#[derive(Clone, Debug, PartialEq)]
pub struct CanFrame {
    pub id: u32,
    pub ext: bool,
    pub rtr: bool,
    /// Error frame: `id` holds the CAN_ERR_* class bits.
    pub err: bool,
    pub fd: bool,
    pub brs: bool,
    pub esi: bool,
    pub len: u8,
    pub data: Vec<u8>,
    pub ts: f64,
    /// Sent by this socket, echoed once the controller confirmed it (MSG_CONFIRM).
    pub own: bool,
    /// Sent by another socket on this host (MSG_DONTROUTE).
    pub local: bool,
}

/// What a frame's payload was given as.
#[derive(Clone, Debug, Default)]
pub enum Payload {
    #[default]
    None,
    Hex(String),
    Bytes(Vec<u8>),
}

/// What callers hand to [`make_frame`]: any missing flag is false.
#[derive(Clone, Debug, Default)]
pub struct FrameSpec {
    pub id: f64,
    pub ext: bool,
    pub rtr: bool,
    pub fd: bool,
    pub brs: bool,
    pub esi: bool,
    /// RTR request length; defaults to the data length.
    pub len: Option<f64>,
    pub data: Payload,
}

/// Unix epoch milliseconds, fractional (`performance.timeOrigin + performance.now()`).
pub fn now_ms_f64() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

// ---------- Small JavaScript conversions ----------

/// `ToUint32`: what JavaScript's bitwise operators do to a number.
pub fn to_uint32(f: f64) -> u32 {
    if !f.is_finite() {
        return 0;
    }
    let t = f.trunc();
    (t.rem_euclid(4_294_967_296.0)) as u32
}

/// `parseInt(s, 16)`: leading whitespace, a sign, an optional 0x, then the longest run of hex
/// digits. None for NaN.
pub fn parse_int_hex(s: &str) -> Option<f64> {
    let t = s.trim_start();
    let (neg, t) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    let digits: Vec<u32> = t.chars().map_while(|c| c.to_digit(16)).collect();
    if digits.is_empty() {
        return None;
    }
    let v = digits.iter().fold(0f64, |a, d| a * 16.0 + *d as f64);
    Some(if neg { -v } else { v })
}

/// `x.toFixed(d)` without trailing zeros (`+x.toFixed(d)` printed).
pub fn fixed_trim(x: f64, d: usize) -> String {
    let s = format!("{x:.d$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

// ---------- Bytes and hex ----------

const HEXD: &[u8; 16] = b"0123456789ABCDEF";

pub fn hex2(b: u8) -> [char; 2] {
    [HEXD[(b >> 4) as usize] as char, HEXD[(b & 15) as usize] as char]
}

pub fn to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.extend(hex2(x));
    }
    s
}

/// "DE AD be:ef", "deadbeef", "0xDE,0xAD" → bytes. Fails on odd digits or junk.
pub fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    // .replace(/0x/gi, "").replace(/[\s,:.\-_]/g, "")
    let mut t = String::with_capacity(s.len());
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '0' && i + 1 < b.len() && (b[i + 1] == 'x' || b[i + 1] == 'X') {
            i += 2;
            continue;
        }
        t.push(b[i]);
        i += 1;
    }
    let t: String = t.chars().filter(|c| !(js_space(*c) || matches!(c, ',' | ':' | '.' | '-' | '_'))).collect();
    if !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("Not hex: \"{s}\""));
    }
    if !t.len().is_multiple_of(2) {
        return Err(format!("Odd number of hex digits: \"{s}\""));
    }
    Ok((0..t.len() / 2).map(|i| u8::from_str_radix(&t[i * 2..i * 2 + 2], 16).unwrap()).collect())
}

/// JavaScript's `\s`.
pub fn js_space(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r' | '\u{a0}' | '\u{feff}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// `Uint8Array.from(v)` for a JSON value: an array's elements as bytes, anything else empty.
fn bytes_from_json(v: &Value) -> Vec<u8> {
    match v {
        Value::Array(a) => a.iter().map(|x| to_uint32(js_number(x)) as u8).collect(),
        _ => Vec::new(),
    }
}

/// CAN FD payload sizes. A length in between is padded up to the next one.
pub const FD_LENS: [usize; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32, 48, 64];

pub fn fd_len(n: usize) -> Option<usize> {
    FD_LENS.iter().copied().find(|&l| l >= n)
}

// ---------- Frame encode / decode ----------

/// Validate a spec and turn it into a frame (ts = now), with a readable message on bad input.
pub fn make_frame(spec: &FrameSpec) -> Result<CanFrame, String> {
    let id = spec.id;
    if !(id.is_finite() && id.fract() == 0.0) || id < 0.0 {
        return Err("id must be a non-negative integer".into());
    }
    let ext = spec.ext || id > CAN_SFF_MASK as f64;
    if id > (if ext { CAN_EFF_MASK } else { CAN_SFF_MASK }) as f64 {
        return Err(format!("id 0x{:x} is out of range", id as u64));
    }
    let id = id as u32;
    let (fd, rtr) = (spec.fd, spec.rtr);
    if fd && rtr {
        return Err("CAN FD has no remote frames".into());
    }
    let mut data = match &spec.data {
        Payload::None => Vec::new(),
        Payload::Hex(s) => parse_hex(s)?,
        Payload::Bytes(b) => b.clone(),
    };
    let mut len = data.len();
    if rtr {
        let l = spec.len.unwrap_or(data.len() as f64);
        if l < 0.0 || l > 8.0 {
            return Err("RTR length must be 0–8".into());
        }
        len = if l.is_nan() { 0 } else { l as usize };
        data = Vec::new();
    } else if fd {
        let Some(l) = fd_len(len) else { return Err("CAN FD payload is at most 64 bytes".into()) };
        data.resize(l, 0);
        len = l;
    } else if len > 8 {
        return Err("Classic CAN payload is at most 8 bytes (use fd for more)".into());
    }
    Ok(CanFrame {
        id,
        ext,
        rtr,
        err: false,
        fd,
        brs: fd && spec.brs,
        esi: fd && spec.esi,
        len: len as u8,
        data,
        ts: now_ms_f64(),
        own: false,
        local: false,
    })
}

pub fn encode_frame(f: &CanFrame) -> Vec<u8> {
    let mut buf = vec![0u8; if f.fd { CANFD_MTU } else { CAN_MTU }];
    let mut raw = f.id & if f.ext { CAN_EFF_MASK } else { CAN_SFF_MASK };
    if f.ext {
        raw |= CAN_EFF_FLAG;
    }
    if f.rtr {
        raw |= CAN_RTR_FLAG;
    }
    buf[0..4].copy_from_slice(&raw.to_le_bytes());
    buf[4] = f.len;
    if f.fd {
        buf[5] = (if f.brs { CANFD_BRS } else { 0 }) | (if f.esi { CANFD_ESI } else { 0 }) | CANFD_FDF;
    }
    if !f.rtr {
        let n = (f.len as usize).min(f.data.len());
        buf[8..8 + n].copy_from_slice(&f.data[..n]);
    }
    buf
}

fn decode_at(b: &[u8], off: usize, size: usize, ts: f64) -> CanFrame {
    let raw = u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]);
    let fd = size == CANFD_MTU;
    let err = raw & CAN_ERR_FLAG != 0;
    let ext = !err && raw & CAN_EFF_FLAG != 0;
    let rtr = !err && !fd && raw & CAN_RTR_FLAG != 0;
    let len = b[off + 4].min(if fd { 64 } else { 8 });
    let flags = if fd { b[off + 5] } else { 0 };
    CanFrame {
        id: if err { raw & CAN_ERR_MASK } else { raw & if ext { CAN_EFF_MASK } else { CAN_SFF_MASK } },
        ext,
        rtr,
        err,
        fd,
        brs: flags & CANFD_BRS != 0,
        esi: flags & CANFD_ESI != 0,
        len,
        data: if rtr { Vec::new() } else { b[off + 8..off + 8 + len as usize].to_vec() },
        ts,
        own: false,
        local: false,
    }
}

/// Split a read chunk into frames. A raw-socket read returns one datagram, but a byte-stream
/// reader hands over concatenations (seen on desky: 32 = 2×16 and 88 = 16+72 byte chunks),
/// so this copes. A canfd_frame is recognised by the CANFD_FDF flag the kernel sets on every
/// received FD frame (or a length over 8); a can_frame's byte 5 is padding and always 0.
pub fn split_frames(b: &[u8], ts: f64, out: &mut Vec<CanFrame>) {
    let mut off = 0;
    while b.len() - off >= CAN_MTU {
        let is_fd = b.len() - off >= CANFD_MTU && (b[off + 5] & CANFD_FDF != 0 || b[off + 4] > 8);
        let size = if is_fd { CANFD_MTU } else { CAN_MTU };
        out.push(decode_at(b, off, size, ts));
        off += size;
    }
}

pub fn frame_key(id: u32, ext: bool) -> String {
    format!("{}{id:x}", if ext { "x" } else { "s" })
}

pub fn fmt_id(id: u32, ext: bool) -> String {
    if ext { format!("{id:08X}") } else { format!("{id:03X}") }
}

// ---------- candump -L log lines and cansend syntax ----------

/// The part after the interface in `candump -L`: 123#DEADBEEF, 1234ABCD#R, 123##5DEADBEEF, 20000080#…
pub fn candump_body(f: &CanFrame) -> String {
    if f.err {
        return format!("{}#{}", fmt_id(f.id | CAN_ERR_FLAG, true), to_hex(&f.data));
    }
    let id = fmt_id(f.id, f.ext);
    if f.rtr {
        return format!("{id}#R{}", if f.len != 0 { format!("{:X}", f.len) } else { String::new() });
    }
    // candump prints the whole flags nibble, FDF included, so a BRS frame is "##5".
    if f.fd {
        let flags = (if f.brs { CANFD_BRS } else { 0 }) | (if f.esi { CANFD_ESI } else { 0 }) | CANFD_FDF;
        return format!("{id}##{flags:X}{}", to_hex(&f.data));
    }
    format!("{id}#{}", to_hex(&f.data))
}

/// Epoch ms → "SECS.USECS", rounded to the microsecond (ms floats like 1700000000000.001 sit
/// just below).
pub fn fmt_secs(ts: f64) -> String {
    let mut s = (ts / 1000.0).floor();
    let mut us = ((ts - s * 1000.0) * 1000.0 + 0.5).floor();
    if us >= 1_000_000.0 {
        s += 1.0;
        us -= 1_000_000.0;
    }
    format!("{}.{:06}", s as i64, us as i64)
}

pub fn candump_line(f: &CanFrame, iface: &str) -> String {
    format!("({}) {iface} {}", fmt_secs(f.ts), candump_body(f))
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

/// cansend syntax → spec. "123#DEADBEEF", "123#DE.AD", "1F334455#R", "123#R4", "123##1DEADBEEF".
pub fn parse_cansend(s: &str) -> Result<FrameSpec, String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    // /^\s*([0-9a-fA-F]{1,8})#(#?)(.*?)\s*$/ (JavaScript's `.` stops at line breaks)
    let trimmed = s.trim_matches(js_space);
    let caps = re(&RE, r"^([0-9a-fA-F]{1,8})#(#?)([^\n\r  ]*)$").captures(trimmed);
    let Some(c) = caps else {
        return Err(format!("Expected cansend syntax like 123#DEADBEEF, got \"{s}\""));
    };
    let id_s = &c[1];
    let rest = &c[3];
    let id = u32::from_str_radix(id_s, 16).unwrap() as f64;
    let ext = id_s.len() > 3;
    if !c[2].is_empty() {
        let Some(first) = rest.chars().next() else { return Err("CAN FD frame needs a flags nibble after ##".into()) };
        let flags = first.to_digit(16).unwrap_or(0) as u8;
        return Ok(FrameSpec {
            id,
            ext,
            fd: true,
            brs: flags & CANFD_BRS != 0,
            esi: flags & CANFD_ESI != 0,
            data: Payload::Bytes(parse_hex(&rest[first.len_utf8()..])?),
            ..Default::default()
        });
    }
    if rest.starts_with(['R', 'r']) {
        let len = if rest.len() > 1 { parse_int_hex(&rest[1..]).unwrap_or(f64::NAN) } else { 0.0 };
        return Ok(FrameSpec { id, ext, rtr: true, len: Some(len), ..Default::default() });
    }
    Ok(FrameSpec { id, ext, data: Payload::Bytes(parse_hex(rest)?), ..Default::default() })
}

/// One parsed `candump -L` line (for replay and tests).
pub struct CandumpLine {
    pub ts: f64,
    pub iface: String,
    pub frame: CanFrame,
}

pub fn parse_candump_line(line: &str) -> Result<Option<CandumpLine>, String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let Some(m) = re(&RE, r"^\((\d+)\.(\d{6})\)\s+(\S+)\s+(\S+)").captures(line) else { return Ok(None) };
    let ts = m[1].parse::<f64>().unwrap_or(f64::NAN) * 1000.0 + m[2].parse::<f64>().unwrap_or(0.0) / 1000.0;
    let body = &m[4];
    let hash = body.find('#');
    let id_s = match hash {
        Some(i) => &body[..i],
        None => &body[..body.len().saturating_sub(1)],
    };
    if id_s.len() == 8 && to_uint32(parse_int_hex(id_s).unwrap_or(0.0)) & CAN_ERR_FLAG != 0 {
        let data = parse_hex(&body[hash.map(|i| i + 1).unwrap_or(0)..])?;
        let id = to_uint32(parse_int_hex(id_s).unwrap_or(0.0)) & CAN_ERR_MASK;
        let frame = CanFrame {
            id,
            ext: false,
            rtr: false,
            err: true,
            fd: false,
            brs: false,
            esi: false,
            len: data.len() as u8,
            data,
            ts,
            own: false,
            local: false,
        };
        return Ok(Some(CandumpLine { ts, iface: m[3].to_string(), frame }));
    }
    let mut f = make_frame(&parse_cansend(body)?)?;
    f.ts = ts;
    Ok(Some(CandumpLine { ts, iface: m[3].to_string(), frame: f }))
}

// ---------- Error frames (linux/can/error.h) ----------

const ERR_CLASS: [(u32, &str); 10] = [
    (0x001, "TX timeout"),
    (0x002, "lost arbitration"),
    (0x004, "controller problem"),
    (0x008, "protocol violation"),
    (0x010, "transceiver"),
    (0x020, "no ACK"),
    (0x040, "bus off"),
    (0x080, "bus error"),
    (0x100, "restarted"),
    (0x200, "error counters"),
];
const ERR_CTRL: [(u8, &str); 7] = [
    (0x01, "RX overflow"),
    (0x02, "TX overflow"),
    (0x04, "RX warning"),
    (0x08, "TX warning"),
    (0x10, "RX passive"),
    (0x20, "TX passive"),
    (0x40, "back to error-active"),
];
const ERR_PROT: [(u8, &str); 8] = [
    (0x01, "bit"),
    (0x02, "form"),
    (0x04, "stuff"),
    (0x08, "dominant bit"),
    (0x10, "recessive bit"),
    (0x20, "overload"),
    (0x40, "active error flag"),
    (0x80, "during TX"),
];

fn err_loc(b: u8) -> Option<&'static str> {
    Some(match b {
        0x03 => "SOF",
        0x02 => "ID28–21",
        0x06 => "ID20–18",
        0x04 => "SRTR",
        0x05 => "IDE",
        0x07 => "ID17–13",
        0x0f => "ID12–5",
        0x0e => "ID4–0",
        0x0c => "RTR",
        0x0d => "res1",
        0x09 => "res0",
        0x0b => "DLC",
        0x0a => "data",
        0x08 => "CRC",
        0x18 => "CRC delim",
        0x19 => "ACK slot",
        0x1b => "ACK delim",
        0x1a => "EOF",
        0x12 => "intermission",
        _ => return None,
    })
}

fn err_trx(b: u8) -> Option<&'static str> {
    Some(match b {
        0x04 => "CANH no wire",
        0x05 => "CANH short to BAT",
        0x06 => "CANH short to VCC",
        0x07 => "CANH short to GND",
        0x40 => "CANL no wire",
        0x50 => "CANL short to BAT",
        0x60 => "CANL short to VCC",
        0x70 => "CANL short to GND",
        0x80 => "CANL short to CANH",
        _ => return None,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecodedError {
    pub ts: f64,
    pub classes: Vec<&'static str>,
    pub text: String,
    pub bus_off: bool,
    pub tec: Option<u8>,
    pub rec: Option<u8>,
}

pub fn decode_error(f: &CanFrame) -> DecodedError {
    let d = |i: usize| f.data.get(i).copied().unwrap_or(0);
    let classes: Vec<&'static str> = ERR_CLASS.iter().filter(|(b, _)| f.id & b != 0).map(|(_, n)| *n).collect();
    let mut parts: Vec<String> = Vec::new();
    if f.id & 0x002 != 0 && d(0) != 0 {
        parts.push(format!("arbitration lost at bit {}", d(0)));
    }
    if f.id & 0x004 != 0 && d(1) != 0 {
        parts.push(format!(
            "controller: {}",
            ERR_CTRL.iter().filter(|(b, _)| d(1) & b != 0).map(|(_, n)| *n).collect::<Vec<_>>().join(", ")
        ));
    }
    if f.id & 0x008 != 0 {
        let types: Vec<&str> = ERR_PROT.iter().filter(|(b, _)| d(2) & b != 0).map(|(_, n)| *n).collect();
        let loc = f.data.get(3).and_then(|b| err_loc(*b));
        parts.push(format!(
            "protocol: {}{}",
            if types.is_empty() { "unspecified".to_string() } else { format!("{} error", types.join("/")) },
            loc.map(|l| format!(" at {l}")).unwrap_or_default()
        ));
    }
    if f.id & 0x010 != 0 && d(4) != 0 {
        parts.push(format!("transceiver: {}", err_trx(d(4)).map(String::from).unwrap_or_else(|| format!("0x{}", to_hex(&[d(4)])))));
    }
    let mut text: Vec<String> = classes
        .iter()
        .filter(|c| !["controller problem", "protocol violation", "transceiver", "error counters"].contains(c))
        .map(|c| c.to_string())
        .collect();
    text.extend(parts);
    let text = if text.is_empty() { "error frame".to_string() } else { text.join("; ") };
    let (tec, rec) = if f.id & 0x200 != 0 || d(6) != 0 || d(7) != 0 {
        (Some(f.data.get(6).copied()), Some(f.data.get(7).copied()))
    } else {
        (None, None)
    };
    DecodedError { ts: f.ts, classes, text, bus_off: f.id & 0x040 != 0, tec: tec.flatten(), rec: rec.flatten() }
}

// ---------- Bus load ----------

/// Bits on the wire for one frame, worst-case bit stuffing, including the 3-bit
/// intermission. Classic: the usual 47+8n (std) / 67+8n (ext) plus ⌊(34+8n−1)/4⌋ or
/// ⌊(54+8n−1)/4⌋ stuff bits. CAN FD is split into the part sent at the nominal rate and, with
/// BRS, the part sent at the data rate (ESI…CRC delimiter), with the FD CRC's fixed stuff bits.
/// Returns (nominal-rate bits, data-rate bits).
pub fn frame_bits(ext: bool, rtr: bool, fd: bool, brs: bool, len: u8) -> (u32, u32) {
    let n = if rtr { 0 } else { len as u32 };
    if !fd {
        let base = if ext { 67 } else { 47 };
        let stuffable = (if ext { 54 } else { 34 }) + 8 * n;
        return (base + 8 * n + (stuffable - 1) / 4, 0);
    }
    let arb = if ext { 36 } else { 17 }; // SOF … BRS
    let tail = 1 + 1 + 7 + 3; // ACK slot, ACK delim, EOF, intermission
    let crc = if n > 16 { 21 } else { 17 };
    let dynb = 1 + 4 + 8 * n; // ESI, DLC, data: dynamically stuffed
    let data_bits = dynb + (dynb - 1) / 4 + 4 + crc + (4 + crc).div_ceil(4) + 1; // + stuff count, CRC (+fixed stuff), CRC delim
    let nom = arb + (arb - 1) / 4 + tail;
    if brs { (nom, data_bits) } else { (nom + data_bits, 0) }
}

/// Seconds of bus time one frame takes.
pub fn frame_time(f: &CanFrame, bitrate: f64, dbitrate: Option<f64>) -> f64 {
    let (nom, data) = frame_bits(f.ext, f.rtr, f.fd, f.brs, f.len);
    let db = dbitrate.filter(|d| *d != 0.0 && !d.is_nan()).unwrap_or(bitrate);
    nom as f64 / bitrate + if data != 0 { data as f64 / db } else { 0.0 }
}

// ---------- Filters ----------

#[derive(Clone, Debug, PartialEq)]
pub struct IdFilter {
    pub id: u32,
    /// Default: exact match.
    pub mask: Option<u32>,
    /// None: either.
    pub ext: Option<bool>,
}

/// Filters from JSON (`{id, mask?, ext?}` or a list of them); None for "no filter".
pub fn parse_filters(x: &Value) -> Result<Option<Vec<IdFilter>>, String> {
    match x {
        Value::Null => return Ok(None),
        Value::String(s) if s.is_empty() => return Ok(None),
        Value::Array(a) if a.is_empty() => return Ok(None),
        _ => {}
    }
    let arr: Vec<&Value> = match x {
        Value::Array(a) => a.iter().collect(),
        v => vec![v],
    };
    let num = |v: &Value| match v {
        Value::String(s) => parse_int_hex(s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s)).unwrap_or(f64::NAN),
        other => js_number(other),
    };
    let missing = Value::Null;
    arr.into_iter()
        .map(|e| {
            let get = |k: &str| match e {
                Value::Object(o) => o.get(k).unwrap_or(&missing),
                _ => &missing,
            };
            // A missing id is Number(undefined): NaN.
            let id = if get("id").is_null() && !matches!(e, Value::Object(o) if o.get("id").is_some()) { f64::NAN } else { num(get("id")) };
            if !id.is_finite() {
                return Err("filter id must be a number or hex string".to_string());
            }
            let mask = if get("mask").is_null() { None } else { Some(to_uint32(num(get("mask")))) };
            let ext = if get("ext").is_null() { None } else { Some(benchbob_core::http::truthy(get("ext"))) };
            Ok(IdFilter { id: to_uint32(id), mask, ext })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Compact filter syntax for query strings and the UI: comma-separated hex ids, each with an
/// optional ":mask"; an "x" prefix forces extended. "123,7E0:7F8,x18DAF100:1FFFFF00".
pub fn parse_filter_string(s: Option<&str>) -> Result<Option<Vec<IdFilter>>, String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let Some(s) = s.filter(|s| !s.trim_matches(js_space).is_empty()) else { return Ok(None) };
    let r = re(&RE, r"(?i)^(x)?(?:0x)?([0-9a-f]{1,8})(?::(?:0x)?([0-9a-f]{1,8}))?$");
    s.split(|c: char| c == ',' || js_space(c))
        .filter(|t| !t.is_empty())
        .map(|tok| {
            let m = r.captures(tok).ok_or_else(|| format!("Bad filter \"{tok}\": use hex ID or ID:MASK, x prefix for extended"))?;
            let id = u32::from_str_radix(&m[2], 16).unwrap();
            let ext = if m.get(1).is_some() || m[2].len() > 3 || id > CAN_SFF_MASK { Some(true) } else { None };
            Ok(IdFilter { id, mask: m.get(3).map(|x| u32::from_str_radix(x.as_str(), 16).unwrap()), ext })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// A frame spec from JSON (REST, MCP, WebSocket). `id` as a number, or a hex string ("0x123"
/// or "123"); `data` as hex or a byte array; or `cansend: "123#DEADBEEF"` instead of both.
pub fn spec_from_json(o: &Value) -> Result<FrameSpec, String> {
    let Value::Object(m) = o else { return Err("frame must be an object".into()) };
    if let Some(Value::String(c)) = m.get("cansend") {
        return parse_cansend(c);
    }
    let idv = m.get("id").unwrap_or(&Value::Null);
    if idv.is_null() || idv.as_str() == Some("") {
        return Err("frame needs an id".into());
    }
    let id = match idv {
        Value::String(s) => parse_int_hex(s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s)).unwrap_or(f64::NAN),
        v => js_number(v),
    };
    if !id.is_finite() {
        return Err(format!("bad id {}", serde_json::to_string(idv).unwrap_or_default()));
    }
    let t = |k: &str| m.get(k).is_some_and(benchbob_core::http::truthy);
    let data = match m.get("data") {
        None | Some(Value::Null) => Payload::Hex(String::new()),
        Some(Value::String(s)) => Payload::Hex(s.clone()),
        Some(v) => Payload::Bytes(bytes_from_json(v)),
    };
    let len = match m.get("len") {
        None | Some(Value::Null) => None,
        Some(v) => Some(js_number(v)),
    };
    Ok(FrameSpec { id, ext: t("ext"), rtr: t("rtr"), fd: t("fd"), brs: t("brs"), esi: t("esi"), len, data })
}

pub fn match_filters(id: u32, ext: bool, err: bool, filters: Option<&[IdFilter]>) -> bool {
    let Some(filters) = filters else { return true };
    if err {
        return false;
    }
    filters.iter().any(|flt| {
        if flt.ext.is_some_and(|e| e != ext) {
            return false;
        }
        let mask = flt.mask.unwrap_or(if ext { CAN_EFF_MASK } else { CAN_SFF_MASK });
        (id ^ flt.id) & mask == 0
    })
}

// ---------- Auto-bitrate decision ----------

pub const STD_BITRATES: [u32; 11] = [1_000_000, 800_000, 500_000, 250_000, 125_000, 100_000, 83_333, 50_000, 33_333, 20_000, 10_000];

#[derive(Clone, Debug, PartialEq)]
pub struct BitrateTrial {
    pub bitrate: f64,
    /// Valid frames received.
    pub frames: u64,
    /// Error frames received.
    pub errors: u64,
    /// The controller's bus_error counter delta.
    pub bus_errors: u64,
    pub skipped: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BitrateVerdict {
    /// "found" | "silent" | "none"
    pub result: &'static str,
    pub bitrate: Option<f64>,
    /// "high" | "low"
    pub confidence: Option<&'static str>,
    pub text: String,
}

/// Pick the bitrate. At the right rate a listen-only controller decodes frames cleanly; at a
/// wrong one it sees mostly stuff/form/CRC errors, with the odd fluke frame. So the winner is
/// the rate with the most valid frames whose error share stays under half; confidence is high
/// when it saw a few frames and errors were rare.
pub fn pick_bitrate(trials: &[BitrateTrial]) -> BitrateVerdict {
    let bad = |t: &BitrateTrial| (t.errors + t.bus_errors) as f64;
    let ratio = |t: &BitrateTrial| bad(t) / (t.frames as f64 + bad(t)).max(1.0);
    let mut ok: Vec<&BitrateTrial> = trials.iter().filter(|t| t.frames > 0 && ratio(t) < 0.5).collect();
    if !ok.is_empty() {
        ok.sort_by(|a, b| {
            b.frames
                .cmp(&a.frames)
                .then(ratio(a).partial_cmp(&ratio(b)).unwrap_or(std::cmp::Ordering::Equal))
                .then(b.bitrate.partial_cmp(&a.bitrate).unwrap_or(std::cmp::Ordering::Equal))
        });
        let w = ok[0];
        let high = w.frames >= 3 && ratio(w) < 0.05;
        return BitrateVerdict {
            result: "found",
            bitrate: Some(w.bitrate),
            confidence: Some(if high { "high" } else { "low" }),
            text: format!(
                "{}: {} frames, {} errors{}",
                fmt_bitrate(w.bitrate),
                w.frames,
                bad(w),
                if high { "" } else { " (low confidence: few frames or many errors)" }
            ),
        };
    }
    if trials.iter().all(|t| t.frames == 0 && bad(t) == 0.0) {
        return BitrateVerdict {
            result: "silent",
            bitrate: None,
            confidence: None,
            text: "Silent bus: no frames and no errors at any rate. Is anything transmitting, and is the bus wired and terminated?".into(),
        };
    }
    BitrateVerdict {
        result: "none",
        bitrate: None,
        confidence: None,
        text: "Errors at every rate and no clean frames: check CAN-H/CAN-L wiring and termination, or the bus uses a non-standard bitrate."
            .into(),
    }
}

pub fn fmt_bitrate(b: f64) -> String {
    if b == 0.0 || b.is_nan() {
        return "?".into();
    }
    if b >= 1_000_000.0 {
        return format!("{}M", fixed_trim(b / 1_000_000.0, 3));
    }
    if b >= 1000.0 {
        return format!("{}k", fixed_trim(b / 1000.0, 3));
    }
    js_string(&benchbob_core::http::num(b))
}

// ---------- socketcand ASCII protocol ----------

/// Pull complete `< … >` elements out of a receive buffer, socketcand style: junk before '<'
/// is dropped. Returns (commands, the rest to keep).
pub fn sc_take(buf: &str) -> (Vec<String>, String) {
    let mut cmds = Vec::new();
    let mut i = 0;
    loop {
        let Some(s) = buf[i..].find('<').map(|x| x + i) else { return (cmds, String::new()) };
        let Some(e) = buf[s + 1..].find('>').map(|x| x + s + 1) else {
            let rest = &buf[s..];
            // A command never gets this long; don't let a peer grow the buffer forever.
            return (cmds, if rest.chars().count() > 4096 { String::new() } else { rest.to_string() });
        };
        let mut c = String::new();
        let mut in_space = false;
        for ch in buf[s..=e].chars() {
            if js_space(ch) {
                if !in_space {
                    c.push(' ');
                }
                in_space = true;
            } else {
                c.push(ch);
                in_space = false;
            }
        }
        cmds.push(c);
        i = e + 1;
    }
}

/// The words inside "< a b c >".
pub fn sc_words(cmd: &str) -> Vec<String> {
    let t = cmd.strip_prefix('<').map(|x| x.trim_start_matches(js_space)).unwrap_or(cmd);
    let t = t.strip_suffix('>').map(|x| x.trim_end_matches(js_space)).unwrap_or(t);
    t.split(' ').filter(|w| !w.is_empty()).map(String::from).collect()
}

/// Raw-mode RX line: `< frame 123 1700000000.123456 DEADBEEF >` (ext ids are 8 digits;
/// python-can keys on that).
pub fn sc_frame_raw(f: &CanFrame) -> String {
    format!("< frame {} {} {} >", fmt_id(f.id, f.ext), fmt_secs(f.ts), to_hex(&f.data))
}

/// BCM-mode RX line: data bytes space separated, as socketcand's state_bcm prints them.
pub fn sc_frame_bcm(f: &CanFrame) -> String {
    let mut s = format!("< frame {} {} ", fmt_id(f.id, f.ext), fmt_secs(f.ts));
    for &b in &f.data {
        s.extend(hex2(b));
        s.push(' ');
    }
    s + " >"
}

pub fn sc_error(f: &CanFrame) -> String {
    format!("< error {:03X} {} >", f.id, fmt_secs(f.ts))
}

/// Parse the id/len/data part of `< send ID LEN B0 … >` (also `add`/`update` after their
/// leading words). socketcand marks the id extended when it is 8 characters; SavvyCAN also
/// sets bit 31 on extended ids, so mask that off. Bytes are hex, `a` or `0A`.
pub fn sc_parse_id_len_data(words: &[String]) -> Result<CanFrame, String> {
    if words.len() < 2 {
        return Err("expected ID LEN [bytes…]".into());
    }
    let (id_s, len_s, bytes) = (&words[0], &words[1], &words[2..]);
    if !(1..=8).contains(&id_s.len()) || !id_s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("bad id".into());
    }
    let raw = u32::from_str_radix(id_s, 16).unwrap();
    let len = benchbob_core::http::parse_int(len_s);
    let Some(len) = len.filter(|l| (0..=8).contains(l)) else { return Err("bad length".into()) };
    if bytes.len() != len as usize || bytes.iter().any(|b| !(1..=2).contains(&b.len()) || !b.bytes().all(|c| c.is_ascii_hexdigit())) {
        return Err("byte count doesn't match length".into());
    }
    let id = raw & CAN_EFF_MASK;
    make_frame(&FrameSpec {
        id: id as f64,
        ext: id_s.len() == 8 || id > CAN_SFF_MASK,
        data: Payload::Bytes(bytes.iter().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()),
        ..Default::default()
    })
}

pub fn beacon_xml(host: &str, ip: &str, port: u16, buses: &[String]) -> String {
    let x = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let mut s =
        format!("<CANBeacon name=\"{}\" type=\"SocketCAN\" description=\"BenchBob socketcand\">\n<URL>can://{ip}:{port}</URL>", x(host));
    for b in buses {
        s += &format!("<Bus name=\"{}\"/>", x(b));
    }
    s + "</CANBeacon>"
}

#[cfg(test)]
mod tests;
