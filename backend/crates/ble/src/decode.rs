//! BLE advertisement decoding (peripherals/lib/ble-decode.ts).
//!
//! Pure functions, no I/O, so everything here is unit-tested against captured and crafted
//! bytes. The pipeline is:
//!
//!   btmon -w (btsnoop, Linux monitor format) → BtsnoopReader → HCI LE advertising reports
//!   → AD structures → beacon decoders → BleTracker (per-advertiser aggregate)
//!
//! Working from raw HCI reports rather than BlueZ's D-Bus view is deliberate: D-Bus merges
//! advertisements and scan responses, hides the PDU type (so "connectable" is unknowable)
//! and only keeps the latest manufacturer data per company.
//!
//! Where the TypeScript would throw on a truncated payload (reading past the end of an
//! array), the decoders here return nothing for it instead.

use benchbob_core::http::{js_string, num};
use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::ids::{apple_audio_model, apple_type, company_name, ms_cdp_device, uuid16_name, xiaomi_product};

// ---------- small byte helpers ----------

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn hex_sep(b: &[u8], sep: &str) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(sep)
}

pub fn from_hex(s: &str) -> Vec<u8> {
    let c: Vec<u8> = s.bytes().filter(u8::is_ascii_hexdigit).collect();
    c.as_chunks::<2>().0.iter().map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap()).collect()
}

fn u16le(b: &[u8], o: usize) -> u16 {
    b[o] as u16 | (b[o + 1] as u16) << 8
}
fn u16be(b: &[u8], o: usize) -> u16 {
    (b[o] as u16) << 8 | b[o + 1] as u16
}
fn s16le(b: &[u8], o: usize) -> i16 {
    u16le(b, o) as i16
}
fn s16be(b: &[u8], o: usize) -> i16 {
    u16be(b, o) as i16
}
fn u24le(b: &[u8], o: usize) -> u32 {
    b[o] as u32 | (b[o + 1] as u32) << 8 | (b[o + 2] as u32) << 16
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u32be(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn s8(v: u8) -> i8 {
    v as i8
}

/// `Math.round(v * 10 ** d) / 10 ** d`
pub fn round(v: f64, d: i32) -> f64 {
    let m = 10f64.powi(d);
    (v * m + 0.5).floor() / m
}

/// `str.slice(a, b)` on bytes, clamped like JavaScript's.
fn sl(b: &[u8], a: usize, e: usize) -> &[u8] {
    let e = e.min(b.len());
    if a >= e { &[] } else { &b[a..e] }
}

/// `new TextDecoder().decode(v).replace(/\0+$/, "")`
fn text(v: &[u8]) -> String {
    String::from_utf8_lossy(v).trim_end_matches('\0').to_string()
}

/// Bluetooth addresses travel little-endian; display is big-endian with colons.
pub fn addr_str(b: &[u8], o: usize) -> Option<String> {
    let s = b.get(o..o + 6)?;
    Some(s.iter().rev().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(":"))
}

fn uuid128_str(b: &[u8], o: usize) -> String {
    let mut v = b[o..o + 16].to_vec();
    v.reverse();
    let h = hex(&v);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]).to_uppercase()
}

// ---------- btsnoop stream ----------

#[derive(Debug, Clone)]
pub struct BtsnoopRecord {
    /// ms since the Unix epoch
    pub ts: f64,
    /// controller index (hciN); 0xFFFF for system notes
    pub index: u16,
    /// monitor opcode: 0 new index, 1 del index, 3 event, 12 system note…
    pub opcode: u16,
    pub data: Vec<u8>,
}

pub mod mon {
    pub const NEW_INDEX: u16 = 0;
    pub const DEL_INDEX: u16 = 1;
    pub const COMMAND: u16 = 2;
    pub const EVENT: u16 = 3;
    pub const INDEX_INFO: u16 = 10;
    pub const SYSTEM_NOTE: u16 = 12;
}

/// btsnoop timestamps count microseconds from 0000-01-01; this is 1970-01-01 on that scale.
const BTSNOOP_EPOCH_US: i128 = 0x00dc_ddb3_0f2f_8000;

/// Incremental btsnoop parser: feed it pipe chunks, get whole records back.
#[derive(Default)]
pub struct BtsnoopReader {
    buf: Vec<u8>,
    header_seen: bool,
    pub datalink: u32,
}

impl BtsnoopReader {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<BtsnoopRecord>, String> {
        self.buf.extend_from_slice(chunk);
        let m = &self.buf;
        let mut o = 0;
        let mut out = Vec::new();
        if !self.header_seen {
            if m.len() < 16 {
                return Ok(out);
            }
            if &m[..7] != b"btsnoop" {
                return Err("not a btsnoop stream".into());
            }
            self.datalink = u32::from_be_bytes([m[12], m[13], m[14], m[15]]);
            self.header_seen = true;
            o = 16;
        }
        let be32 = |o: usize| u32::from_be_bytes([m[o], m[o + 1], m[o + 2], m[o + 3]]);
        while m.len() - o >= 24 {
            let incl = be32(o + 4) as usize;
            if m.len() - o < 24 + incl {
                break;
            }
            let flags = be32(o + 8);
            let ts_us = u64::from_be_bytes(m[o + 16..o + 24].try_into().unwrap());
            let data = &m[o + 24..o + 24 + incl];
            let ts = (ts_us as i128 - BTSNOOP_EPOCH_US) as f64 / 1000.0;
            if self.datalink == 2001 {
                out.push(BtsnoopRecord { ts, index: (flags >> 16) as u16, opcode: (flags & 0xffff) as u16, data: data.to_vec() });
            } else if self.datalink == 1002 && data.first() == Some(&0x04) {
                out.push(BtsnoopRecord { ts, index: 0, opcode: mon::EVENT, data: data[1..].to_vec() }); // H4 UART
            } else if self.datalink == 1001 && flags & 2 != 0 && flags & 1 != 0 {
                out.push(BtsnoopRecord { ts, index: 0, opcode: mon::EVENT, data: data.to_vec() });
            }
            o += 24 + incl;
        }
        self.buf.drain(..o);
        Ok(out)
    }
}

/// NEW_INDEX payload: type, bus, bdaddr, 8-byte name ("hci0").
pub fn parse_new_index(d: &[u8]) -> (String, String) {
    let name = String::from_utf8_lossy(sl(d, 8, 16)).split('\0').next().unwrap_or("").to_string();
    (addr_str(d, 2).unwrap_or_default(), name)
}

// ---------- HCI LE advertising reports ----------

#[derive(Debug, Clone, PartialEq)]
pub struct AdvReport {
    pub ts: f64,
    pub index: u16,
    pub addr: String,
    /// HCI: 0 public, 1 random, 2 public identity, 3 random static identity
    pub addr_type: u8,
    /// ADV_IND, ADV_NONCONN_IND, SCAN_RSP, EXT…
    pub pdu: String,
    pub connectable: bool,
    pub scannable: bool,
    pub scan_rsp: bool,
    pub rssi: Option<i32>,
    /// extended reports carry it in the header
    pub tx_power: Option<i32>,
    pub data: Vec<u8>,
}

const LEGACY_PDU: [&str; 5] = ["ADV_IND", "ADV_DIRECT_IND", "ADV_SCAN_IND", "ADV_NONCONN_IND", "SCAN_RSP"];

/// Decode an HCI event packet; returns the advertising reports it carries (usually zero or one).
pub fn parse_hci_event(ev: &[u8], ts: f64, index: u16) -> Vec<AdvReport> {
    let mut out = Vec::new();
    if ev.len() < 3 || ev[0] != 0x3e {
        return out;
    }
    let sub = ev[2];
    let mut o = 4;
    let n = ev.get(3).copied().unwrap_or(0) as usize;
    if sub == 0x02 {
        // LE Advertising Report. Controllers send one report per event in practice; BlueZ parses
        // multiple sequentially, so do the same.
        let mut i = 0;
        while i < n && o + 9 <= ev.len() {
            let t = ev[o];
            let len = ev[o + 8] as usize;
            let r = ev.get(o + 9 + len).copied();
            out.push(AdvReport {
                ts,
                index,
                addr: addr_str(ev, o + 2).unwrap_or_default(),
                addr_type: ev[o + 1],
                pdu: LEGACY_PDU.get(t as usize).map(|s| s.to_string()).unwrap_or_else(|| format!("type {t}")),
                connectable: t == 0 || t == 1,
                scannable: t == 0 || t == 2,
                scan_rsp: t == 4,
                rssi: r.filter(|r| *r != 127).map(|r| s8(r) as i32),
                tx_power: None,
                data: sl(ev, o + 9, o + 9 + len).to_vec(),
            });
            o += 10 + len;
            i += 1;
        }
    } else if sub == 0x0d {
        // LE Extended Advertising Report (BT 5).
        let mut i = 0;
        while i < n && o + 24 <= ev.len() {
            let et = u16le(ev, o);
            let tx = s8(ev[o + 12]) as i32;
            let r = s8(ev[o + 13]) as i32;
            let len = ev[o + 23] as usize;
            let legacy = et & 0x10 != 0;
            let pdu = if legacy {
                (if et & 0x08 != 0 {
                    "SCAN_RSP"
                } else if et & 0x01 != 0 {
                    "ADV_IND"
                } else if et & 0x02 != 0 {
                    "ADV_SCAN_IND"
                } else {
                    "ADV_NONCONN_IND"
                })
                .to_string()
            } else {
                format!("EXT{}", if et & 0x08 != 0 { " SCAN_RSP" } else { "" })
            };
            out.push(AdvReport {
                ts,
                index,
                addr: addr_str(ev, o + 3).unwrap_or_default(),
                addr_type: ev[o + 2],
                pdu,
                connectable: et & 0x01 != 0,
                scannable: et & 0x02 != 0,
                scan_rsp: et & 0x08 != 0,
                rssi: (r != 127).then_some(r),
                tx_power: (tx != 127).then_some(tx),
                data: sl(ev, o + 24, o + 24 + len).to_vec(),
            });
            o += 24 + len;
            i += 1;
        }
    }
    out
}

/// Human address type, including the random sub-type from the top two address bits.
pub fn addr_kind(addr: &str, hci_type: u8) -> &'static str {
    match hci_type {
        0 => "public",
        2 => "public (resolved)",
        3 => "random static (resolved)",
        _ => match u8::from_str_radix(addr.get(..2).unwrap_or(""), 16).map(|b| b >> 6) {
            Ok(3) => "random static",
            Ok(1) => "random resolvable",
            Ok(0) => "random non-resolvable",
            _ => "random",
        },
    }
}

// ---------- AD structures ----------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AdInfo {
    pub flags: Option<u8>,
    pub name: Option<String>,
    pub name_complete: bool,
    pub tx_power: Option<i32>,
    pub appearance: Option<u16>,
    /// "180F" for 16-bit, full form for 128-bit
    pub uuids: Vec<String>,
    pub mfr: Vec<(u16, Vec<u8>)>,
    pub svc: Vec<(String, Vec<u8>)>,
}

pub fn parse_ad(d: &[u8]) -> AdInfo {
    let mut ad = AdInfo::default();
    let mut o = 0;
    while o < d.len() {
        let len = d[o] as usize;
        if len == 0 || o + 1 + len > d.len() {
            break;
        }
        let t = d[o + 1];
        let v = &d[o + 2..o + 1 + len];
        o += 1 + len;
        match t {
            0x01 => ad.flags = v.first().copied(),
            0x02 | 0x03 => {
                let mut i = 0;
                while i + 1 < v.len() {
                    ad.uuids.push(format!("{:04X}", u16le(v, i)));
                    i += 2;
                }
            }
            0x04 | 0x05 => {
                let mut i = 0;
                while i + 3 < v.len() {
                    ad.uuids.push(format!("{:08X}", u32le(v, i)));
                    i += 4;
                }
            }
            0x06 | 0x07 => {
                let mut i = 0;
                while i + 15 < v.len() {
                    ad.uuids.push(uuid128_str(v, i));
                    i += 16;
                }
            }
            0x08 | 0x09 => {
                let name = text(v);
                // A complete name beats a shortened one regardless of which packet came first.
                if !name.is_empty() && (t == 0x09 || !ad.name_complete) {
                    ad.name = Some(name);
                    ad.name_complete = t == 0x09;
                }
            }
            0x0a => ad.tx_power = Some(s8(v.first().copied().unwrap_or(0)) as i32),
            0x19 if v.len() >= 2 => ad.appearance = Some(u16le(v, 0)),
            0x16 if v.len() >= 2 => ad.svc.push((format!("{:04X}", u16le(v, 0)), v[2..].to_vec())),
            0x20 if v.len() >= 4 => ad.svc.push((format!("{:08X}", u32le(v, 0)), v[4..].to_vec())),
            0x21 if v.len() >= 16 => ad.svc.push((uuid128_str(v, 0), v[16..].to_vec())),
            0xff if v.len() >= 2 => ad.mfr.push((u16le(v, 0), v[2..].to_vec())),
            _ => {}
        }
    }
    ad
}

/// "180F" → "Battery"; 128-bit UUIDs built on the SIG base are shortened first.
pub fn uuid_name(uuid: &str) -> &'static str {
    let u = uuid.to_uppercase();
    let short = if u.len() == 36 && u.starts_with("0000") && u.ends_with("-0000-1000-8000-00805F9B34FB") {
        Some(&u[4..8])
    } else if uuid.len() == 4 {
        Some(&u[..])
    } else {
        None
    };
    short.and_then(|s| u32::from_str_radix(s, 16).ok()).map(uuid16_name).unwrap_or("")
}

// ---------- Beacon decoders ----------

#[derive(Debug, Clone, PartialEq)]
pub struct Beacon {
    /// stable id: "ibeacon", "eddystone-tlm", "bthome", "apple", …
    pub kind: &'static str,
    /// one-line human summary
    pub label: String,
    /// decoded values (units in the key names)
    pub fields: Option<Map<String, Value>>,
}

impl Beacon {
    pub fn json(&self) -> Value {
        let mut m = Map::new();
        m.insert("kind".into(), self.kind.into());
        m.insert("label".into(), self.label.clone().into());
        if let Some(f) = &self.fields {
            m.insert("fields".into(), Value::Object(f.clone()));
        }
        Value::Object(m)
    }
}

/// A fields object under construction (insertion order kept, as a JavaScript object's).
#[derive(Default)]
struct F(Map<String, Value>);

impl F {
    fn n(&mut self, k: &str, v: f64) -> &mut Self {
        self.0.insert(k.into(), num(v));
        self
    }
    fn s(&mut self, k: &str, v: impl Into<String>) -> &mut Self {
        self.0.insert(k.into(), Value::String(v.into()));
        self
    }
    fn v(&mut self, k: &str, v: Value) -> &mut Self {
        self.0.insert(k.into(), v);
        self
    }
}

/// `${x}` of a field value.
fn fs(v: &Value) -> String {
    js_string(v)
}

fn sensor_label(f: &Map<String, Value>) -> String {
    let has = |k: &str| f.get(k).filter(|v| !v.is_null());
    let mut p = Vec::new();
    if let Some(t) = has("temperature_C") {
        p.push(format!("{} °C", fs(t)));
    }
    if let Some(h) = has("humidity") {
        p.push(format!("{} %RH", fs(h)));
    }
    if let Some(x) = has("pressure_hPa") {
        p.push(format!("{} hPa", fs(x)));
    }
    if let Some(b) = has("battery") {
        p.push(format!("batt {} %", fs(b)));
    } else if let Some(b) = has("battery_mV") {
        p.push(format!("batt {} mV", fs(b)));
    }
    p.join(" · ")
}

fn fmt_dur(s: f64) -> String {
    let r = |x: f64| js_string(&num((x + 0.5).floor()));
    if s < 120.0 {
        format!("{} s", r(s))
    } else if s < 7200.0 {
        format!("{} min", r(s / 60.0))
    } else if s < 172800.0 {
        format!("{} h", r(s / 3600.0))
    } else {
        format!("{} d", r(s / 86400.0))
    }
}

pub fn decode_ibeacon(company: u16, d: &[u8]) -> Option<Beacon> {
    if company != 0x004c || d.len() < 23 || d[0] != 0x02 || d[1] != 0x15 {
        return None;
    }
    let h = hex(&d[2..18]).to_uppercase();
    let uuid = format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]);
    let (major, minor, mp) = (u16be(d, 18), u16be(d, 20), s8(d[22]));
    let mut f = F::default();
    f.s("uuid", uuid.clone()).n("major", major as f64).n("minor", minor as f64).n("measuredPower_dBm", mp as f64);
    Some(Beacon { kind: "ibeacon", label: format!("iBeacon {uuid} major {major} minor {minor} @1m {mp} dBm"), fields: Some(f.0) })
}

/// Apple Continuity: a sequence of type/length/value messages. Labels only — the payloads
/// are mostly encrypted.
pub fn decode_apple(company: u16, d: &[u8]) -> Option<Beacon> {
    if company != 0x004c || d.len() < 2 {
        return None;
    }
    let mut msgs: Vec<String> = Vec::new();
    let mut fields = F::default();
    let mut o = 0;
    while o + 2 <= d.len() {
        let t = d[o];
        let len = d[o + 1] as usize;
        let v = sl(d, o + 2, o + 2 + len);
        o += 2 + len;
        if t == 0x02 {
            continue; // iBeacon has its own decoder
        }
        let mut label = apple_type(t).map(String::from).unwrap_or_else(|| format!("type 0x{t:02x}"));
        if t == 0x07 && v.len() >= 3 {
            if let Some(model) = apple_audio_model(u16be(v, 1)) {
                label += &format!(": {model}");
                fields.s("audioModel", model);
            }
        } else if t == 0x12 {
            // Find My (offline finding). A 25-byte payload carries the rotating public key and is
            // only broadcast while the accessory is separated from its owner; 2 bytes = near owner.
            let separated = len >= 25;
            let battery = v.first().map(|b| ["full", "medium", "low", "critical"][(b >> 6) as usize]);
            label += if separated { " (separated from owner)" } else { " (near owner)" };
            fields.v("findMySeparated", Value::Bool(separated));
            if let Some(b) = battery
                && separated
            {
                fields.s("findMyBattery", b);
            }
        } else if t == 0x10
            && let Some(b) = v.first()
        {
            fields.n("nearbyInfoStatus", *b as f64);
        }
        msgs.push(label);
    }
    if msgs.is_empty() {
        return None;
    }
    let mut all = F::default();
    all.v("messages", Value::from(msgs.clone()));
    for (k, v) in fields.0 {
        all.0.insert(k, v);
    }
    Some(Beacon { kind: "apple", label: format!("Apple: {}", msgs.join(", ")), fields: Some(all.0) })
}

/// Microsoft: Swift Pair (beacon id 0x03) and CDP (0x01, Windows/Xbox "nearby devices").
pub fn decode_microsoft(company: u16, d: &[u8]) -> Option<Beacon> {
    if company != 0x0006 || d.len() < 2 {
        return None;
    }
    if d[0] == 0x03 {
        // Swift Pair: 0x03, sub-scenario, reserved RSSI byte, then [BR/EDR addr + CoD] + display name.
        let sub = d[1];
        let name_at = if sub == 0x01 { 3 + 6 + 3 } else { 3 };
        let name = if d.len() > name_at { text(&d[name_at..]) } else { String::new() };
        let mode = match sub {
            0x00 => "LE".to_string(),
            0x01 => "BR/EDR via LE".into(),
            0x02 => "LE & BR/EDR".into(),
            s => format!("scenario {s}"),
        };
        let mut f = F::default();
        f.s("scenario", mode.clone());
        if !name.is_empty() {
            f.s("displayName", name.clone());
        }
        let label = format!("Microsoft Swift Pair ({mode}){}", if name.is_empty() { String::new() } else { format!(": {name}") });
        return Some(Beacon { kind: "swiftpair", label, fields: Some(f.0) });
    }
    if d[0] == 0x01 {
        let t = d[1] & 0x1f;
        let dev = ms_cdp_device(t).map(String::from).unwrap_or_else(|| format!("device type {t}"));
        let mut f = F::default();
        f.s("deviceType", dev.clone());
        return Some(Beacon { kind: "ms-cdp", label: format!("Microsoft CDP beacon ({dev})"), fields: Some(f.0) });
    }
    None
}

/// Ruuvi RAWv2 (data format 5).
pub fn decode_ruuvi(company: u16, d: &[u8]) -> Option<Beacon> {
    if company != 0x0499 || d.len() < 24 || d[0] != 0x05 {
        return None;
    }
    let mut f = F::default();
    let t = s16be(d, 1);
    if t != i16::MIN {
        f.n("temperature_C", round(t as f64 * 0.005, 3));
    }
    let h = u16be(d, 3);
    if h != 0xffff {
        f.n("humidity", round(h as f64 * 0.0025, 2));
    }
    let p = u16be(d, 5);
    if p != 0xffff {
        f.n("pressure_hPa", round((p as f64 + 50000.0) / 100.0, 2));
    }
    let (ax, ay, az) = (s16be(d, 7), s16be(d, 9), s16be(d, 11));
    if ax != i16::MIN {
        f.n("accelX_mg", ax as f64).n("accelY_mg", ay as f64).n("accelZ_mg", az as f64);
    }
    let pw = u16be(d, 13);
    if pw >> 5 != 0x7ff {
        f.n("battery_mV", ((pw >> 5) + 1600) as f64);
    }
    if pw & 0x1f != 0x1f {
        f.n("txPower_dBm", (pw & 0x1f) as f64 * 2.0 - 40.0);
    }
    if d[15] != 0xff {
        f.n("movements", d[15] as f64);
    }
    if u16be(d, 16) != 0xffff {
        f.n("sequence", u16be(d, 16) as f64);
    }
    f.s("mac", hex_sep(&d[18..24], ":").to_uppercase());
    Some(Beacon { kind: "ruuvi", label: format!("RuuviTag {}", sensor_label(&f.0)), fields: Some(f.0) })
}

/// Govee H5072/H5075 (and the many models sharing the 3-byte packed format).
pub fn decode_govee(company: u16, d: &[u8], name: &str) -> Option<Beacon> {
    if company != 0xec88 || d.len() < 5 {
        return None;
    }
    // Packed as one 24-bit big-endian number: temp*10000 + humidity*10, bit 23 = negative.
    let mut packed = (d[1] as u32) << 16 | (d[2] as u32) << 8 | d[3] as u32;
    let neg = packed & 0x80_0000 != 0;
    packed &= 0x7f_ffff;
    let mut f = F::default();
    f.n("temperature_C", round((if neg { -1.0 } else { 1.0 }) * (packed / 1000) as f64 / 10.0, 1))
        .n("humidity", round((packed % 1000) as f64 / 10.0, 1))
        .n("battery", (d[4] & 0x7f) as f64);
    let model =
        crate::re!(r"(?i)GVH5\d{3}|Govee_H5\d{3}").find(name).map(|m| m.as_str().to_string()).unwrap_or_else(|| "H5075/H5072".into());
    Some(Beacon { kind: "govee", label: format!("Govee {model} {}", sensor_label(&f.0)), fields: Some(f.0) })
}

/// Eddystone frames (service data 0xFEAA).
pub fn decode_eddystone(uuid: &str, d: &[u8]) -> Option<Beacon> {
    if uuid != "FEAA" || d.is_empty() {
        return None;
    }
    let t = d[0];
    if t == 0x00 && d.len() >= 18 {
        let ns = hex(&d[2..12]).to_uppercase();
        let inst = hex(&d[12..18]).to_uppercase();
        let mut f = F::default();
        f.n("txPower0m_dBm", s8(d[1]) as f64).s("namespace", ns.clone()).s("instance", inst.clone());
        return Some(Beacon { kind: "eddystone-uid", label: format!("Eddystone-UID {ns} / {inst}"), fields: Some(f.0) });
    }
    if t == 0x10 && d.len() >= 3 {
        const SCHEMES: [&str; 4] = ["http://www.", "https://www.", "http://", "https://"];
        const EXP: [&str; 14] =
            [".com/", ".org/", ".edu/", ".net/", ".info/", ".biz/", ".gov/", ".com", ".org", ".edu", ".net", ".info", ".biz", ".gov"];
        let mut url = SCHEMES.get(d[2] as usize).copied().unwrap_or("").to_string();
        for &c in &d[3..] {
            if (c as usize) < EXP.len() {
                url += EXP[c as usize];
            } else if c > 0x20 && c < 0x7f {
                url.push(c as char);
            }
        }
        let mut f = F::default();
        f.n("txPower0m_dBm", s8(d[1]) as f64).s("url", url.clone());
        return Some(Beacon { kind: "eddystone-url", label: format!("Eddystone-URL {url}"), fields: Some(f.0) });
    }
    if t == 0x20 && d.len() >= 14 && d[1] == 0x00 {
        let mut f = F::default();
        let mv = u16be(d, 2);
        if mv != 0 {
            f.n("battery_mV", mv as f64);
        }
        if u16be(d, 4) != 0x8000 {
            f.n("temperature_C", round(s16be(d, 4) as f64 / 256.0, 2));
        }
        let up = round(u32be(d, 10) as f64 / 10.0, 1);
        f.n("advCount", u32be(d, 6) as f64).n("uptime_s", up);
        return Some(Beacon {
            kind: "eddystone-tlm",
            label: format!("Eddystone-TLM {} up {}", sensor_label(&f.0), fmt_dur(up)),
            fields: Some(f.0),
        });
    }
    if t == 0x20 && d.get(1) == Some(&0x01) {
        return Some(Beacon { kind: "eddystone-tlm", label: "Eddystone-TLM (encrypted)".into(), fields: None });
    }
    if t == 0x30 {
        return Some(Beacon { kind: "eddystone-eid", label: format!("Eddystone-EID {}", hex(sl(d, 2, 10))), fields: None });
    }
    None
}

/// BTHome v2 object table: id → (name, byte length, signed, factor). Text/raw (0x53/0x54)
/// are length-prefixed and handled inline. An unknown id ends parsing because its size isn't
/// known.
fn bthome(id: u8) -> Option<(&'static str, usize, bool, f64)> {
    Some(match id {
        0x00 => ("packet_id", 1, false, 1.0),
        0x01 => ("battery", 1, false, 1.0),
        0x02 => ("temperature_C", 2, true, 0.01),
        0x03 => ("humidity", 2, false, 0.01),
        0x04 => ("pressure_hPa", 3, false, 0.01),
        0x05 => ("illuminance_lux", 3, false, 0.01),
        0x06 => ("mass_kg", 2, false, 0.01),
        0x07 => ("mass_lb", 2, false, 0.01),
        0x08 => ("dewpoint_C", 2, true, 0.01),
        0x09 => ("count", 1, false, 1.0),
        0x0a => ("energy_kWh", 3, false, 0.001),
        0x0b => ("power_W", 3, false, 0.01),
        0x0c => ("voltage_V", 2, false, 0.001),
        0x0d => ("pm2_5_ugm3", 2, false, 1.0),
        0x0e => ("pm10_ugm3", 2, false, 1.0),
        0x0f => ("generic", 1, false, 1.0),
        0x10 => ("power_on", 1, false, 1.0),
        0x11 => ("opening", 1, false, 1.0),
        0x12 => ("co2_ppm", 2, false, 1.0),
        0x13 => ("tvoc_ugm3", 2, false, 1.0),
        0x14 => ("moisture", 2, false, 0.01),
        0x15 => ("battery_low", 1, false, 1.0),
        0x16 => ("battery_charging", 1, false, 1.0),
        0x17 => ("carbon_monoxide", 1, false, 1.0),
        0x18 => ("cold", 1, false, 1.0),
        0x19 => ("connectivity", 1, false, 1.0),
        0x1a => ("door", 1, false, 1.0),
        0x1b => ("garage_door", 1, false, 1.0),
        0x1c => ("gas", 1, false, 1.0),
        0x1d => ("heat", 1, false, 1.0),
        0x1e => ("light", 1, false, 1.0),
        0x1f => ("lock", 1, false, 1.0),
        0x20 => ("moisture_detected", 1, false, 1.0),
        0x21 => ("motion", 1, false, 1.0),
        0x22 => ("moving", 1, false, 1.0),
        0x23 => ("occupancy", 1, false, 1.0),
        0x24 => ("plug", 1, false, 1.0),
        0x25 => ("presence", 1, false, 1.0),
        0x26 => ("problem", 1, false, 1.0),
        0x27 => ("running", 1, false, 1.0),
        0x28 => ("safety", 1, false, 1.0),
        0x29 => ("smoke", 1, false, 1.0),
        0x2a => ("sound", 1, false, 1.0),
        0x2b => ("tamper", 1, false, 1.0),
        0x2c => ("vibration", 1, false, 1.0),
        0x2d => ("window", 1, false, 1.0),
        0x2e => ("humidity", 1, false, 1.0),
        0x2f => ("moisture", 1, false, 1.0),
        0x3a => ("button", 1, false, 1.0),
        0x3c => ("dimmer", 2, false, 1.0),
        0x3d => ("count", 2, false, 1.0),
        0x3e => ("count", 4, false, 1.0),
        0x3f => ("rotation_deg", 2, true, 0.1),
        0x40 => ("distance_mm", 2, false, 1.0),
        0x41 => ("distance_m", 2, false, 0.1),
        0x42 => ("duration_s", 3, false, 0.001),
        0x43 => ("current_A", 2, false, 0.001),
        0x44 => ("speed_ms", 2, false, 0.01),
        0x45 => ("temperature_C", 2, true, 0.1),
        0x46 => ("uv_index", 1, false, 0.1),
        0x47 => ("volume_L", 2, false, 0.1),
        0x48 => ("volume_mL", 2, false, 1.0),
        0x49 => ("flow_m3h", 2, false, 0.001),
        0x4a => ("voltage_V", 2, false, 0.1),
        0x4b => ("gas_m3", 3, false, 0.001),
        0x4c => ("gas_m3", 4, false, 0.001),
        0x4d => ("energy_kWh", 4, false, 0.001),
        0x4e => ("volume_L", 4, false, 0.001),
        0x4f => ("water_L", 4, false, 0.001),
        0x50 => ("timestamp", 4, false, 1.0),
        0x51 => ("acceleration_ms2", 2, false, 0.001),
        0x52 => ("gyroscope_dps", 2, false, 0.001),
        0x55 => ("volume_storage_L", 4, false, 0.001),
        0x56 => ("conductivity_uScm", 2, false, 1.0),
        0x57 => ("temperature_C", 1, true, 1.0),
        0x58 => ("temperature_C", 1, true, 0.35),
        0x59 => ("count", 1, true, 1.0),
        0x5a => ("count", 2, true, 1.0),
        0x5b => ("count", 4, true, 1.0),
        0x5c => ("power_W", 4, true, 0.01),
        0x5d => ("current_A", 2, true, 0.001),
        0x5e => ("direction_deg", 2, false, 0.01),
        0x5f => ("precipitation_mm", 2, false, 0.1),
        0x60 => ("channel", 1, false, 1.0),
        0x61 => ("rotational_speed_rpm", 2, false, 1.0),
        0xf0 => ("device_type_id", 2, false, 1.0),
        0xf1 => ("firmware_version", 4, false, 1.0),
        0xf2 => ("firmware_version", 3, false, 1.0),
        _ => return None,
    })
}

const BTHOME_BUTTON: [&str; 8] =
    ["none", "press", "double_press", "triple_press", "long_press", "long_double_press", "long_triple_press", "hold_press"];

pub fn decode_bthome(uuid: &str, d: &[u8]) -> Option<Beacon> {
    if uuid != "FCD2" || d.is_empty() {
        return None;
    }
    let info = d[0];
    let version = info >> 5;
    if version != 2 {
        return Some(Beacon { kind: "bthome", label: format!("BTHome v{version} (unsupported version)"), fields: None });
    }
    if info & 1 != 0 {
        let mut f = F::default();
        f.v("encrypted", Value::Bool(true));
        return Some(Beacon { kind: "bthome", label: "BTHome v2 (encrypted)".into(), fields: Some(f.0) });
    }
    let mut f: Map<String, Value> = Map::new();
    // Repeated objects (two temperatures, several buttons) get _2, _3… suffixes.
    let put = |f: &mut Map<String, Value>, k: &str, v: Value| {
        let mut key = k.to_string();
        let mut i = 2;
        while f.contains_key(&key) {
            key = format!("{k}_{i}");
            i += 1;
        }
        f.insert(key, v);
    };
    let mut o = 1;
    while o < d.len() {
        let id = d[o];
        o += 1;
        if id == 0x53 || id == 0x54 {
            let len = d.get(o).copied().unwrap_or(0) as usize;
            o += 1;
            let v = sl(d, o, o + len);
            o += len;
            if id == 0x53 {
                put(&mut f, "text", Value::from(String::from_utf8_lossy(v).into_owned()));
            } else {
                put(&mut f, "raw", Value::from(hex(v)));
            }
            continue;
        }
        let Some((name, len, signed, factor)) = bthome(id) else {
            f.insert("unparsed".into(), Value::from(hex(&d[o - 1..])));
            break;
        };
        if o + len > d.len() {
            break;
        }
        let mut raw = 0f64;
        for i in (0..len).rev() {
            raw = raw * 256.0 + d[o + i] as f64;
        }
        if signed && raw >= 2f64.powi(len as i32 * 8 - 1) {
            raw -= 2f64.powi(len as i32 * 8);
        }
        o += len;
        if id == 0x3a {
            put(&mut f, name, BTHOME_BUTTON.get(raw as usize).map(|s| Value::from(*s)).unwrap_or_else(|| num(raw)));
        } else if id == 0x3c {
            let r = raw as u32;
            let mut m = Map::new();
            m.insert("event".into(), Value::from(r & 0xff));
            m.insert("steps".into(), Value::from(r >> 8));
            put(&mut f, name, Value::Object(m));
        } else {
            put(&mut f, name, if factor == 1.0 { num(raw) } else { num(round(raw * factor, 4)) });
        }
    }
    if info & 4 != 0 {
        f.insert("trigger_based".into(), Value::Bool(true));
    }
    let sl = sensor_label(&f);
    let label = format!("BTHome {}", if sl.is_empty() { f.keys().cloned().collect::<Vec<_>>().join(", ") } else { sl });
    Some(Beacon { kind: "bthome", label, fields: Some(f) })
}

/// Xiaomi MiBeacon (service data 0xFE95), unencrypted objects only.
pub fn decode_mibeacon(uuid: &str, d: &[u8]) -> Option<Beacon> {
    if uuid != "FE95" || d.len() < 5 {
        return None;
    }
    let fc = u16le(d, 0);
    let pid = u16le(d, 2);
    let product = xiaomi_product(pid).map(String::from).unwrap_or_else(|| format!("product 0x{pid:04x}"));
    let mut f = F::default();
    f.s("product", product.clone()).n("frameCounter", d[4] as f64);
    if fc & 0x0008 != 0 {
        f.v("encrypted", Value::Bool(true));
        return Some(Beacon { kind: "mibeacon", label: format!("Xiaomi {product} (encrypted, needs bind key)"), fields: Some(f.0) });
    }
    let mut o = 5;
    if fc & 0x0010 != 0 {
        f.s("mac", addr_str(d, o)?);
        o += 6;
    }
    if fc & 0x0020 != 0 {
        let cap = d.get(o).copied().unwrap_or(0);
        o += 1;
        if cap & 0x18 == 0x18 {
            o += 2; // capability carries an extra I/O capability word
        }
    }
    if fc & 0x0040 != 0 {
        while o + 3 <= d.len() {
            let id = u16le(d, o);
            let len = d[o + 2] as usize;
            let v = sl(d, o + 3, o + 3 + len);
            o += 3 + len;
            if v.len() < len {
                break;
            }
            match id {
                0x1004 if len >= 2 => {
                    f.n("temperature_C", round(s16le(v, 0) as f64 / 10.0, 1));
                }
                0x1006 if len >= 2 => {
                    f.n("humidity", round(u16le(v, 0) as f64 / 10.0, 1));
                }
                0x100a if len >= 1 => {
                    f.n("battery", v[0] as f64);
                }
                0x100d if len >= 4 => {
                    f.n("temperature_C", round(s16le(v, 0) as f64 / 10.0, 1)).n("humidity", round(u16le(v, 2) as f64 / 10.0, 1));
                }
                0x1007 if len >= 3 => {
                    f.n("illuminance_lux", u24le(v, 0) as f64);
                }
                0x1008 if len >= 1 => {
                    f.n("moisture", v[0] as f64);
                }
                0x1009 if len >= 2 => {
                    f.n("conductivity_uScm", u16le(v, 0) as f64);
                }
                0x4c01 if len >= 4 => {
                    f.n("temperature_C", round(f32::from_le_bytes([v[0], v[1], v[2], v[3]]) as f64, 2));
                }
                0x4c02 if len >= 1 => {
                    f.n("humidity", v[0] as f64);
                }
                0x4803 if len >= 1 => {
                    f.n("battery", v[0] as f64);
                }
                _ => {
                    f.s(&format!("obj_{id:04x}"), hex(v));
                }
            }
        }
    }
    let label = format!("Xiaomi {product} {}", sensor_label(&f.0)).trim().to_string();
    Some(Beacon { kind: "mibeacon", label, fields: Some(f.0) })
}

/// ATC1441 and pvvx custom firmware for LYWSD03MMC (service data 0x181A), told apart by length.
pub fn decode_atc(uuid: &str, d: &[u8]) -> Option<Beacon> {
    if uuid != "181A" {
        return None;
    }
    if d.len() == 13 {
        // ATC1441: big-endian, MAC in display order.
        let mut f = F::default();
        f.s("mac", hex_sep(&d[..6], ":").to_uppercase())
            .n("temperature_C", round(s16be(d, 6) as f64 / 10.0, 1))
            .n("humidity", d[8] as f64)
            .n("battery", d[9] as f64)
            .n("battery_mV", u16be(d, 10) as f64)
            .n("frameCounter", d[12] as f64);
        return Some(Beacon { kind: "atc", label: format!("ATC thermometer {}", sensor_label(&f.0)), fields: Some(f.0) });
    }
    if d.len() == 15 || d.len() == 16 {
        // pvvx "custom": little-endian, MAC reversed.
        let mut f = F::default();
        f.s("mac", addr_str(d, 0)?)
            .n("temperature_C", round(s16le(d, 6) as f64 / 100.0, 2))
            .n("humidity", round(u16le(d, 8) as f64 / 100.0, 2))
            .n("battery_mV", u16le(d, 10) as f64)
            .n("battery", d[12] as f64)
            .n("frameCounter", d[13] as f64)
            .n("flags", d[14] as f64);
        return Some(Beacon { kind: "pvvx", label: format!("pvvx thermometer {}", sensor_label(&f.0)), fields: Some(f.0) });
    }
    None
}

/// Run every decoder over a parsed advertisement.
pub fn decode_beacons(ad: &AdInfo) -> Vec<Beacon> {
    let mut out = Vec::new();
    for (company, data) in &ad.mfr {
        for f in [decode_ibeacon, decode_apple, decode_microsoft, decode_ruuvi] {
            out.extend(f(*company, data));
        }
        out.extend(decode_govee(*company, data, ad.name.as_deref().unwrap_or("")));
    }
    for (uuid, data) in &ad.svc {
        for f in [decode_eddystone, decode_bthome, decode_mibeacon, decode_atc] {
            out.extend(f(uuid, data));
        }
    }
    out
}

// ---------- Per-advertiser aggregation ----------

#[derive(Debug, Clone, PartialEq)]
pub struct Mfr {
    pub company: u16,
    pub company_name: &'static str,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Svc {
    pub uuid: String,
    pub name: &'static str,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BleDevice {
    pub addr: String,
    pub addr_type: &'static str,
    pub controller: String,
    pub name: String,
    pub rssi: Option<i32>,
    /// one sample per RSSI_STEP_MS, newest last
    pub rssi_hist: Vec<i32>,
    pub tx_power: Option<i32>,
    pub first_seen: f64,
    pub last_seen: f64,
    pub count: u64,
    pub connectable: bool,
    pub scannable: bool,
    pub flags: Option<u8>,
    pub appearance: Option<u16>,
    pub uuids: Vec<(String, &'static str)>,
    pub mfr: Vec<Mfr>,
    pub svc: Vec<Svc>,
    pub beacons: Vec<Beacon>,
    pub last_pdu: String,
    /// latest raw advertising data (hex)
    pub adv: String,
    /// latest raw scan response (hex)
    pub scan_rsp: String,
}

fn opt<T: Into<Value>>(v: Option<T>) -> Value {
    v.map(Into::into).unwrap_or(Value::Null)
}

impl BleDevice {
    /// The device as JSON, in the TypeScript object's key order.
    pub fn json(&self) -> Map<String, Value> {
        let mut m = Map::new();
        let mut k = |key: &str, v: Value| {
            m.insert(key.into(), v);
        };
        k("addr", self.addr.clone().into());
        k("addrType", self.addr_type.into());
        k("controller", self.controller.clone().into());
        k("name", self.name.clone().into());
        k("rssi", opt(self.rssi));
        k("rssiHist", self.rssi_hist.clone().into());
        k("txPower", opt(self.tx_power));
        k("firstSeen", num(self.first_seen));
        k("lastSeen", num(self.last_seen));
        k("count", self.count.into());
        k("connectable", self.connectable.into());
        k("scannable", self.scannable.into());
        k("flags", opt(self.flags));
        k("appearance", opt(self.appearance));
        k(
            "uuids",
            Value::Array(
                self.uuids
                    .iter()
                    .map(|(u, n)| {
                        let mut o = Map::new();
                        o.insert("uuid".into(), u.clone().into());
                        o.insert("name".into(), (*n).into());
                        Value::Object(o)
                    })
                    .collect(),
            ),
        );
        k(
            "mfr",
            Value::Array(
                self.mfr
                    .iter()
                    .map(|x| {
                        let mut o = Map::new();
                        o.insert("company".into(), x.company.into());
                        o.insert("companyName".into(), x.company_name.into());
                        o.insert("data".into(), x.data.clone().into());
                        Value::Object(o)
                    })
                    .collect(),
            ),
        );
        k(
            "svc",
            Value::Array(
                self.svc
                    .iter()
                    .map(|x| {
                        let mut o = Map::new();
                        o.insert("uuid".into(), x.uuid.clone().into());
                        o.insert("name".into(), x.name.into());
                        o.insert("data".into(), x.data.clone().into());
                        Value::Object(o)
                    })
                    .collect(),
            ),
        );
        k("beacons", Value::Array(self.beacons.iter().map(Beacon::json).collect()));
        k("lastPdu", self.last_pdu.clone().into());
        k("adv", self.adv.clone().into());
        k("scanRsp", self.scan_rsp.clone().into());
        m
    }
}

const RSSI_STEP_MS: f64 = 1000.0;
const RSSI_HIST: usize = 60;

#[derive(Default)]
pub struct BleTracker {
    pub devices: IndexMap<String, BleDevice>,
    pub packets: u64,
}

impl BleTracker {
    pub fn ingest(&mut self, r: &AdvReport, controller: &str) -> &BleDevice {
        self.packets += 1;
        let d = self.devices.entry(r.addr.clone()).or_insert_with(|| BleDevice {
            addr: r.addr.clone(),
            addr_type: addr_kind(&r.addr, r.addr_type),
            controller: controller.into(),
            name: String::new(),
            rssi: None,
            rssi_hist: vec![],
            tx_power: None,
            first_seen: r.ts,
            last_seen: r.ts,
            count: 0,
            connectable: false,
            scannable: false,
            flags: None,
            appearance: None,
            uuids: vec![],
            mfr: vec![],
            svc: vec![],
            beacons: vec![],
            last_pdu: String::new(),
            adv: String::new(),
            scan_rsp: String::new(),
        });
        d.count += 1;
        d.controller = controller.into();
        d.last_pdu = r.pdu.clone();
        if r.connectable {
            d.connectable = true;
        }
        if r.scannable {
            d.scannable = true;
        }
        if let Some(rssi) = r.rssi {
            // One history slot per second: a phone can send 30 adverts/s, and a sparkline of raw
            // packets would only show the last second. Keep the strongest reading in each slot.
            let slot_new = d.rssi_hist.is_empty()
                || r.ts - d.last_seen >= RSSI_STEP_MS
                || (r.ts / RSSI_STEP_MS).floor() != (d.last_seen / RSSI_STEP_MS).floor();
            if slot_new {
                d.rssi_hist.push(rssi);
                if d.rssi_hist.len() > RSSI_HIST {
                    d.rssi_hist.remove(0);
                }
            } else {
                let last = d.rssi_hist.last_mut().unwrap();
                *last = (*last).max(rssi);
            }
            d.rssi = Some(rssi);
        }
        d.last_seen = r.ts;
        if r.scan_rsp {
            d.scan_rsp = hex(&r.data);
        } else {
            d.adv = hex(&r.data);
        }

        let ad = parse_ad(&r.data);
        if let Some(n) = &ad.name
            && (d.name.is_empty() || ad.name_complete || d.name.encode_utf16().count() < n.encode_utf16().count())
        {
            d.name = n.clone();
        }
        if let Some(t) = ad.tx_power {
            d.tx_power = Some(t);
        } else if let Some(t) = r.tx_power {
            d.tx_power = Some(t);
        }
        if ad.flags.is_some() {
            d.flags = ad.flags;
        }
        if ad.appearance.is_some() {
            d.appearance = ad.appearance;
        }
        for u in &ad.uuids {
            if !d.uuids.iter().any(|x| &x.0 == u) {
                d.uuids.push((u.clone(), uuid_name(u)));
            }
        }
        for (company, data) in &ad.mfr {
            let e = Mfr { company: *company, company_name: company_name(*company), data: hex(data) };
            match d.mfr.iter_mut().find(|x| x.company == *company) {
                Some(x) => *x = e,
                None => d.mfr.push(e),
            }
        }
        for (uuid, data) in &ad.svc {
            let e = Svc { uuid: uuid.clone(), name: uuid_name(uuid), data: hex(data) };
            match d.svc.iter_mut().find(|x| &x.uuid == uuid) {
                Some(x) => *x = e,
                None => d.svc.push(e),
            }
        }
        // Latest decode per beacon kind; an Apple device alternating Nearby Info and Find My
        // keeps both.
        for b in decode_beacons(&ad) {
            match d.beacons.iter_mut().find(|x| x.kind == b.kind) {
                Some(x) => *x = b,
                None => d.beacons.push(b),
            }
        }
        d
    }

    /// Forget advertisers not heard from in `max_age_ms`.
    pub fn prune(&mut self, now: f64, max_age_ms: f64) {
        self.devices.retain(|_, d| now - d.last_seen <= max_age_ms);
    }

    /// Strongest first; `since` (truthy) and `min_rssi` filter.
    pub fn list(&self, since: Option<f64>, min_rssi: Option<f64>, limit: Option<usize>) -> Vec<&BleDevice> {
        let mut l: Vec<&BleDevice> = self.devices.values().collect();
        if let Some(s) = since.filter(|s| *s != 0.0 && !s.is_nan()) {
            l.retain(|d| d.last_seen >= s);
        }
        if let Some(m) = min_rssi {
            l.retain(|d| d.rssi.is_some_and(|r| r as f64 >= m));
        }
        l.sort_by_key(|a| std::cmp::Reverse(a.rssi.unwrap_or(-999)));
        if let Some(n) = limit.filter(|n| *n > 0) {
            l.truncate(n);
        }
        l
    }
}

/// One-line description of what a device is, for lists.
pub fn device_kind(d: &BleDevice) -> String {
    if !d.beacons.is_empty() {
        return d.beacons.iter().map(|b| b.label.clone()).collect::<Vec<_>>().join(" | ");
    }
    let mut parts: Vec<String> = Vec::new();
    for m in &d.mfr {
        parts.push(if m.company_name.is_empty() { format!("company 0x{:04x}", m.company) } else { m.company_name.into() });
    }
    for s in &d.svc {
        parts.push(if s.name.is_empty() { s.uuid.clone() } else { s.name.into() });
    }
    for (_, n) in &d.uuids {
        if !n.is_empty() && !parts.iter().any(|p| p == n) {
            parts.push((*n).into());
        }
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests;
