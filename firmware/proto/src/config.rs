//! The node's config record: Wi-Fi credentials and hostname, kept in a reserved flash
//! region outside the firmware image so reflashing the firmware leaves it alone.
//!
//! The region is two 4 KiB slots. Each slot holds at most one record:
//!
//! | Offset | Size | Field |
//! | --- | --- | --- |
//! | 0 | 4 | magic `BBNC` |
//! | 4 | 1 | format, 1 |
//! | 5 | 1 | flags, 0 (reserved) |
//! | 6 | 2 | payload length, little-endian |
//! | 8 | 4 | sequence number, little-endian (higher wins; 0xFFFFFFFF is invalid) |
//! | 12 | 4 | CRC-32 (IEEE) of bytes 0..12 followed by the payload, little-endian |
//! | 16 | n | payload: TLV fields, each tag (1 byte), length (1 byte), value |
//!
//! Tags: 1 = SSID (1–32 bytes), 2 = password (0–64 bytes, empty for an open network),
//! 3 = hostname (RFC 1123 label, 1–63 chars), 4 = a chunk of one LED string's settings
//! (`[string, part, bytes…]`: the parts of each string in order make its JSON, as
//! `Leds::persist_json` writes it). Tags this version doesn't know are kept as they are
//! ([`Config::rest`]) and written back, so saving the Wi-Fi settings never drops a later
//! version's fields; older firmware skips them. The firmware writes the slot that does not hold the current
//! record (erase, then program), with the sequence number one higher, so a power cut
//! mid-write leaves the old record valid. A `config.uf2` writes the whole region: slot 0
//! with sequence 1 and slot 1 erased (all 0xFF), so it always replaces what was there.
//!
//! `firmware/tools/config-uf2.ts` and `config.html` implement the same format; the
//! round-trip tests here pin it.

pub const SLOT_SIZE: usize = 4096;
pub const REGION_SIZE: usize = 2 * SLOT_SIZE;
pub const MAGIC: [u8; 4] = *b"BBNC";
pub const FORMAT: u8 = 1;
pub const HEADER_LEN: usize = 16;

pub const TAG_SSID: u8 = 1;
pub const TAG_PASSWORD: u8 = 2;
pub const TAG_HOSTNAME: u8 = 3;
pub const TAG_LED_STRING: u8 = 4;

pub const MAX_SSID: usize = 32;
pub const MAX_PASSWORD: usize = 64;
pub const MAX_HOSTNAME: usize = 63;
/// Room for the TLVs after the Wi-Fi ones (LED strings and anything newer): what a slot
/// has left.
pub const REST_MAX: usize = SLOT_SIZE - HEADER_LEN - (2 + MAX_SSID) - (2 + MAX_PASSWORD) - (2 + MAX_HOSTNAME);
/// Data bytes per LED string chunk (a TLV value is at most 255: two go to the indices).
const CHUNK: usize = 253;

/// TLVs this version keeps but doesn't interpret itself (raw tag, length, value runs).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rest {
    buf: [u8; REST_MAX],
    len: u16,
}

impl Rest {
    pub const fn new() -> Self {
        Rest { buf: [0; REST_MAX], len: 0 }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    fn push(&mut self, tag: u8, v: &[u8]) -> bool {
        let n = self.len as usize;
        if v.len() > 255 || n + 2 + v.len() > REST_MAX {
            return false;
        }
        self.buf[n] = tag;
        self.buf[n + 1] = v.len() as u8;
        self.buf[n + 2..n + 2 + v.len()].copy_from_slice(v);
        self.len = (n + 2 + v.len()) as u16;
        true
    }

    /// Each TLV as (tag, value).
    pub fn tlvs(&self) -> impl Iterator<Item = (u8, &[u8])> {
        let b = self.as_bytes();
        let mut i = 0;
        core::iter::from_fn(move || {
            let (&tag, &l) = (b.get(i)?, b.get(i + 1)?);
            let v = b.get(i + 2..i + 2 + l as usize)?;
            i += 2 + l as usize;
            Some((tag, v))
        })
    }
}

impl Default for Rest {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for Rest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Rest({} bytes)", self.len)
    }
}

/// CRC-32/ISO-HDLC (the zlib / PNG / Ethernet CRC), bitwise to stay small.
pub fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for part in parts {
        for &b in *part {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
    }
    !crc
}

/// A UTF-8 string of at most N bytes, stored inline.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FixedStr<const N: usize> {
    buf: [u8; N],
    len: u8,
}

impl<const N: usize> FixedStr<N> {
    pub const fn new() -> Self {
        FixedStr { buf: [0; N], len: 0 }
    }
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() > N || core::str::from_utf8(b).is_err() {
            return None;
        }
        let mut s = Self::new();
        s.buf[..b.len()].copy_from_slice(b);
        s.len = b.len() as u8;
        Some(s)
    }
    pub fn from_text(s: &str) -> Option<Self> {
        Self::from_bytes(s.as_bytes())
    }
    pub fn as_str(&self) -> &str {
        // Only ever filled from validated UTF-8.
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> Default for FixedStr<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> core::fmt::Debug for FixedStr<N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self.as_str(), f)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Config {
    pub ssid: FixedStr<MAX_SSID>,
    pub password: FixedStr<MAX_PASSWORD>,
    /// Empty means "use the default" (`benchbob-<last 3 MAC bytes>`).
    pub hostname: FixedStr<MAX_HOSTNAME>,
    /// LED strings and tags from later versions, kept as they are.
    pub rest: Rest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    SsidLength,
    PasswordLength,
    BadHostname,
}

/// RFC 1123 label: letters, digits and hyphens, not starting or ending with a hyphen.
pub fn valid_hostname(h: &str) -> bool {
    let b = h.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_HOSTNAME
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

impl Config {
    pub fn new(ssid: &str, password: &str, hostname: &str) -> Result<Self, ConfigError> {
        if ssid.is_empty() {
            return Err(ConfigError::SsidLength);
        }
        if !hostname.is_empty() && !valid_hostname(hostname) {
            return Err(ConfigError::BadHostname);
        }
        Ok(Config {
            ssid: FixedStr::from_text(ssid).ok_or(ConfigError::SsidLength)?,
            password: FixedStr::from_text(password).ok_or(ConfigError::PasswordLength)?,
            hostname: FixedStr::from_text(hostname).ok_or(ConfigError::BadHostname)?,
            rest: Rest::new(),
        })
    }

    /// Replace the stored LED strings with these (each one's JSON). False when they don't
    /// all fit in the slot (the ones that fit are kept).
    pub fn set_led_strings<'a>(&mut self, strings: impl IntoIterator<Item = &'a [u8]>) -> bool {
        let mut next = Rest::new();
        for (tag, v) in self.rest.tlvs() {
            if tag != TAG_LED_STRING {
                next.push(tag, v);
            }
        }
        let mut ok = true;
        for (k, json) in strings.into_iter().enumerate() {
            for (part, c) in json.chunks(CHUNK).enumerate() {
                let mut v = [0u8; 255];
                v[0] = k as u8;
                v[1] = part as u8;
                v[2..2 + c.len()].copy_from_slice(c);
                ok &= next.push(TAG_LED_STRING, &v[..2 + c.len()]);
            }
        }
        self.rest = next;
        ok
    }

    /// The stored LED strings, each reassembled into `buf` and handed to `f`.
    pub fn led_strings(&self, buf: &mut [u8], mut f: impl FnMut(&[u8])) {
        let mut k = 0u8;
        loop {
            let mut n = 0;
            let mut found = false;
            for (tag, v) in self.rest.tlvs() {
                if tag == TAG_LED_STRING && v.len() >= 2 && v[0] == k {
                    found = true;
                    let d = &v[2..];
                    if n + d.len() <= buf.len() {
                        buf[n..n + d.len()].copy_from_slice(d);
                        n += d.len();
                    }
                }
            }
            if !found {
                return;
            }
            f(&buf[..n]);
            k = k.wrapping_add(1);
        }
    }

    /// Has Wi-Fi credentials (a record with an empty SSID is treated as unprovisioned).
    pub fn provisioned(&self) -> bool {
        !self.ssid.is_empty()
    }

    /// Encode a record into `out` (a slot, or at least `HEADER_LEN + 3*2 + 159` bytes plus
    /// the rest); returns its length.
    pub fn encode(&self, seq: u32, out: &mut [u8]) -> usize {
        let mut n = HEADER_LEN;
        for (tag, v) in [(TAG_SSID, self.ssid.as_bytes()), (TAG_PASSWORD, self.password.as_bytes()), (TAG_HOSTNAME, self.hostname.as_bytes())] {
            if tag != TAG_SSID && v.is_empty() {
                continue;
            }
            out[n] = tag;
            out[n + 1] = v.len() as u8;
            out[n + 2..n + 2 + v.len()].copy_from_slice(v);
            n += 2 + v.len();
        }
        let rest = self.rest.as_bytes();
        out[n..n + rest.len()].copy_from_slice(rest);
        n += rest.len();
        let len = (n - HEADER_LEN) as u16;
        out[0..4].copy_from_slice(&MAGIC);
        out[4] = FORMAT;
        out[5] = 0;
        out[6..8].copy_from_slice(&len.to_le_bytes());
        out[8..12].copy_from_slice(&seq.to_le_bytes());
        let crc = crc32(&[&out[0..12], &out[HEADER_LEN..n]]);
        out[12..16].copy_from_slice(&crc.to_le_bytes());
        n
    }

    /// Decode the record at the start of `slot`; returns (sequence, config).
    pub fn decode(slot: &[u8]) -> Option<(u32, Config)> {
        if slot.len() < HEADER_LEN || slot[0..4] != MAGIC || slot[4] != FORMAT {
            return None;
        }
        let len = u16::from_le_bytes([slot[6], slot[7]]) as usize;
        let seq = u32::from_le_bytes([slot[8], slot[9], slot[10], slot[11]]);
        let crc = u32::from_le_bytes([slot[12], slot[13], slot[14], slot[15]]);
        if seq == u32::MAX || HEADER_LEN + len > slot.len().min(SLOT_SIZE) {
            return None;
        }
        let payload = &slot[HEADER_LEN..HEADER_LEN + len];
        if crc32(&[&slot[0..12], payload]) != crc {
            return None;
        }
        let mut c = Config::default();
        let mut i = 0;
        while i + 2 <= payload.len() {
            let (tag, l) = (payload[i], payload[i + 1] as usize);
            let v = payload.get(i + 2..i + 2 + l)?;
            match tag {
                TAG_SSID => c.ssid = FixedStr::from_bytes(v)?,
                TAG_PASSWORD => c.password = FixedStr::from_bytes(v)?,
                TAG_HOSTNAME => {
                    let h = FixedStr::from_bytes(v)?;
                    if valid_hostname(h.as_str()) {
                        c.hostname = h;
                    }
                }
                _ => {
                    c.rest.push(tag, v);
                }
            }
            i += 2 + l;
        }
        Some((seq, c))
    }
}

/// Which slot holds the current record, as read from the region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loaded {
    pub slot: usize,
    pub seq: u32,
    pub config: Config,
}

/// Pick the valid record with the highest sequence number from the two slots.
pub fn load(slot0: &[u8], slot1: &[u8]) -> Option<Loaded> {
    let a = Config::decode(slot0).map(|(seq, config)| Loaded { slot: 0, seq, config });
    let b = Config::decode(slot1).map(|(seq, config)| Loaded { slot: 1, seq, config });
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.seq > a.seq { b } else { a }),
        (a, b) => a.or(b),
    }
}

/// Where the next write goes: the other slot, one sequence number higher.
pub fn next_write(current: Option<&Loaded>) -> (usize, u32) {
    match current {
        Some(l) => (1 - l.slot, l.seq.wrapping_add(1).min(u32::MAX - 1)),
        None => (0, 1),
    }
}

/// The full region as a `config.uf2` writes it: slot 0 holds the record with sequence 1,
/// everything else is 0xFF (erased).
pub fn region_image(c: &Config, out: &mut [u8; REGION_SIZE]) {
    out.fill(0xFF);
    c.encode(1, &mut out[..SLOT_SIZE]);
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    #[test]
    fn crc_matches_reference() {
        // The standard check value for CRC-32/ISO-HDLC.
        assert_eq!(crc32(&[b"123456789"]), 0xCBF4_3926);
        assert_eq!(crc32(&[b"1234", b"56789"]), 0xCBF4_3926);
    }

    #[test]
    fn record_round_trip() {
        let c = Config::new("Bench Lab", "correct horse", "bench-node-1").unwrap();
        let mut slot = [0xFFu8; SLOT_SIZE];
        let n = c.encode(42, &mut slot);
        assert_eq!(n, 16 + 2 + 9 + 2 + 13 + 2 + 12);
        assert_eq!(Config::decode(&slot), Some((42, c)));
        // Open network, default hostname: only the SSID TLV.
        let o = Config::new("open", "", "").unwrap();
        let n = o.encode(1, &mut slot);
        assert_eq!(n, 16 + 2 + 4);
        assert_eq!(Config::decode(&slot), Some((1, o)));
    }

    #[test]
    fn corrupt_records_are_rejected() {
        let c = Config::new("a", "b", "").unwrap();
        let mut slot = [0xFFu8; SLOT_SIZE];
        c.encode(5, &mut slot);
        let mut bad = slot;
        bad[HEADER_LEN + 2] ^= 1;
        assert!(Config::decode(&bad).is_none());
        let mut bad = slot;
        bad[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Config::decode(&bad).is_none());
        assert!(Config::decode(&[0xFF; SLOT_SIZE]).is_none());
        assert!(Config::decode(&[0x00; SLOT_SIZE]).is_none());
    }

    #[test]
    fn led_strings_round_trip_in_chunks_and_survive_wifi_saves() {
        let mut c = Config::new("lab", "pw", "node").unwrap();
        let long: std::vec::Vec<u8> = (0..600).map(|i| b'a' + (i % 26) as u8).collect();
        assert!(c.set_led_strings([b"{\"id\":1}".as_slice(), long.as_slice()]));
        let mut slot = [0xFFu8; SLOT_SIZE];
        c.encode(7, &mut slot);
        let (_, mut d) = Config::decode(&slot).unwrap();
        assert_eq!(d, c);
        let mut got = std::vec::Vec::new();
        let mut buf = [0u8; 2048];
        d.led_strings(&mut buf, |s| got.push(s.to_vec()));
        assert_eq!(got, [b"{\"id\":1}".to_vec(), long.clone()]);
        // A Wi-Fi change (what Improv does) keeps them; replacing them drops the old ones.
        d.ssid = FixedStr::from_text("other").unwrap();
        d.encode(8, &mut slot);
        let (_, mut e) = Config::decode(&slot).unwrap();
        got.clear();
        e.led_strings(&mut buf, |s| got.push(s.to_vec()));
        assert_eq!(got.len(), 2);
        assert!(e.set_led_strings([b"x".as_slice()]));
        got.clear();
        e.led_strings(&mut buf, |s| got.push(s.to_vec()));
        assert_eq!(got, [b"x".to_vec()]);
        // Too much to fit: what fits is kept, and it says so.
        let huge = std::vec![b'z'; REST_MAX];
        assert!(!e.set_led_strings([huge.as_slice()]));
    }

    #[test]
    fn unknown_tags_are_skipped() {
        // Hand-built record: SSID "x", unknown tag 9 with 2 bytes.
        let mut slot = [0xFFu8; SLOT_SIZE];
        let payload = [1, 1, b'x', 9, 2, 0xAA, 0xBB];
        slot[0..4].copy_from_slice(&MAGIC);
        slot[4] = 1;
        slot[5] = 0;
        slot[6..8].copy_from_slice(&(payload.len() as u16).to_le_bytes());
        slot[8..12].copy_from_slice(&3u32.to_le_bytes());
        slot[16..16 + payload.len()].copy_from_slice(&payload);
        let crc = crc32(&[&slot[0..12], &payload]);
        slot[12..16].copy_from_slice(&crc.to_le_bytes());
        let (seq, c) = Config::decode(&slot).unwrap();
        assert_eq!(seq, 3);
        assert_eq!(c.ssid.as_str(), "x");
        // Kept, and written back as they were.
        assert_eq!(c.rest.as_bytes(), &[9, 2, 0xAA, 0xBB]);
        let mut again = [0xFFu8; SLOT_SIZE];
        let n = c.encode(3, &mut again);
        assert_eq!(&again[..n], &slot[..n]);
    }

    #[test]
    fn slots_alternate_and_newest_wins() {
        let c1 = Config::new("one", "", "").unwrap();
        let c2 = Config::new("two", "", "").unwrap();
        let mut s0 = [0xFFu8; SLOT_SIZE];
        let mut s1 = [0xFFu8; SLOT_SIZE];
        assert_eq!(load(&s0, &s1), None);
        assert_eq!(next_write(None), (0, 1));
        c1.encode(1, &mut s0);
        let l = load(&s0, &s1).unwrap();
        assert_eq!((l.slot, l.seq), (0, 1));
        let (slot, seq) = next_write(Some(&l));
        assert_eq!((slot, seq), (1, 2));
        c2.encode(seq, &mut s1);
        assert_eq!(load(&s0, &s1).unwrap().config, c2);
        // A torn write of slot 1 falls back to slot 0.
        s1[20] ^= 0xFF;
        assert_eq!(load(&s0, &s1).unwrap().config, c1);
    }

    #[test]
    fn hostnames() {
        assert!(valid_hostname("benchbob-0a1b2c"));
        for h in ["", "-a", "a-", "a.b", "a_b", "ü"] {
            assert!(!valid_hostname(h), "{h:?}");
        }
        assert_eq!(Config::new("x", "", "bad_name"), Err(ConfigError::BadHostname));
        assert_eq!(Config::new("", "", ""), Err(ConfigError::SsidLength));
        assert_eq!(Config::new(&"s".repeat(33), "", ""), Err(ConfigError::SsidLength));
    }

    #[test]
    fn region_image_layout() {
        let c = Config::new("lab", "pw", "").unwrap();
        let mut img = [0u8; REGION_SIZE];
        region_image(&c, &mut img);
        let l = load(&img[..SLOT_SIZE], &img[SLOT_SIZE..]).unwrap();
        assert_eq!((l.slot, l.seq, l.config), (0, 1, c));
        assert!(img[SLOT_SIZE..].iter().all(|&b| b == 0xFF));
    }
}
