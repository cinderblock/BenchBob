//! Network lighting protocols: parsing, source arbitration and mapping onto strings.
//!
//! | Protocol | UDP port | Addressing |
//! | --- | --- | --- |
//! | sACN (ANSI E1.31) | 5568, multicast `239.255.<hi>.<lo>` per universe (or unicast) | universes 1–63999, 512 channels each |
//! | Art-Net (ArtDmx) | 6454, broadcast or unicast | 15-bit port-address (net, sub-net, universe) |
//! | DDP | 4048 | a byte offset into one flat channel space: no universes, no 512 limit |
//!
//! Channels arrive in the controller's pixel order (R, G, B[, W]); the string's own colour
//! order is applied afterwards like any other source. Controllers usually apply their own
//! gamma, so strings fed from the network normally have gamma off.

pub const SACN_PORT: u16 = 5568;
pub const ARTNET_PORT: u16 = 6454;
pub const DDP_PORT: u16 = 4048;

/// sACN: a source that stops sending is dropped after this long (E1.31's
/// network data loss timeout).
pub const SACN_TIMEOUT_MS: u32 = 2500;

/// The multicast group carrying an sACN universe.
pub fn sacn_multicast(universe: u16) -> [u8; 4] {
    [239, 255, (universe >> 8) as u8, universe as u8]
}

/// One universe's worth of DMX data from sACN or Art-Net.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Dmx<'a> {
    pub universe: u16,
    pub sequence: u8,
    /// Slot values after the start code (up to 512).
    pub data: &'a [u8],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sacn<'a> {
    pub dmx: Dmx<'a>,
    pub cid: [u8; 16],
    pub priority: u8,
    /// The source's name, NUL padding removed (may not be valid UTF-8).
    pub source_name: &'a [u8],
    /// Visualiser-only data: not for live output.
    pub preview: bool,
    /// The source has stopped sending this universe.
    pub terminated: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParseError {
    /// Not this protocol at all (wrong identifier or opcode): ignore quietly.
    NotOurs,
    /// This protocol, but a kind of packet we don't act on (sACN sync/discovery, ArtPoll…).
    Other,
    /// Damaged or inconsistent.
    Malformed,
}

const ACN_ID: &[u8; 12] = b"ASC-E1.17\0\0\0";

fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// An E1.31 data packet (ANSI E1.31-2018 §4–7). Only DMX (start code 0) data is
/// returned; other start codes come back as `Other`.
pub fn parse_sacn(p: &[u8]) -> Result<Sacn<'_>, ParseError> {
    if p.len() < 126 || be16(p, 0) != 0x0010 || &p[4..16] != ACN_ID {
        return Err(ParseError::NotOurs);
    }
    if be32(p, 18) != 0x0000_0004 {
        return Err(ParseError::Other); // extended: sync or universe discovery
    }
    if be32(p, 40) != 0x0000_0002 {
        return Err(ParseError::Malformed);
    }
    if p[117] != 0x02 || p[118] != 0xA1 || be16(p, 119) != 0 || be16(p, 121) != 1 {
        return Err(ParseError::Malformed);
    }
    let count = be16(p, 123) as usize; // start code + slots
    if count == 0 || count > 513 || p.len() < 125 + count {
        return Err(ParseError::Malformed);
    }
    // Each layer's length (low 12 bits) runs from its own start to the end of the packet.
    let flen = |i: usize| (be16(p, i) & 0x0FFF) as usize;
    let end = 125 + count;
    if flen(16) != end - 16 || flen(38) != end - 38 || flen(115) != end - 115 {
        return Err(ParseError::Malformed);
    }
    if p[125] != 0x00 {
        return Err(ParseError::Other);
    }
    let universe = be16(p, 113);
    if universe == 0 || universe > 63999 {
        return Err(ParseError::Malformed);
    }
    let name = &p[44..108];
    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(64)];
    let mut cid = [0; 16];
    cid.copy_from_slice(&p[22..38]);
    Ok(Sacn {
        dmx: Dmx { universe, sequence: p[111], data: &p[126..end] },
        cid,
        priority: p[108].min(200),
        source_name: name,
        preview: p[112] & 0x80 != 0,
        terminated: p[112] & 0x40 != 0,
    })
}

/// An Art-Net ArtDmx packet. The universe is the 15-bit port-address
/// (`net << 8 | sub-net << 4 | universe`).
pub fn parse_artnet(p: &[u8]) -> Result<Dmx<'_>, ParseError> {
    if p.len() < 10 || &p[..8] != b"Art-Net\0" {
        return Err(ParseError::NotOurs);
    }
    if u16::from_le_bytes([p[8], p[9]]) != 0x5000 {
        return Err(ParseError::Other); // ArtPoll, ArtSync…
    }
    if p.len() < 18 {
        return Err(ParseError::Malformed);
    }
    let len = be16(p, 16) as usize;
    if !(2..=512).contains(&len) || p.len() < 18 + len {
        return Err(ParseError::Malformed);
    }
    let universe = ((p[15] as u16 & 0x7F) << 8) | p[14] as u16;
    Ok(Dmx { universe, sequence: p[12], data: &p[18..18 + len] })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ddp<'a> {
    /// Byte offset of `data` in the destination's channel space.
    pub offset: u32,
    pub data: &'a [u8],
    /// Show the frame now (the last packet of a frame sets this).
    pub push: bool,
    pub sequence: u8,
    /// Destination: 1 is the default output; 255 all outputs.
    pub id: u8,
}

/// A DDP data packet (v1). Queries, replies and the JSON control/status IDs are `Other`.
pub fn parse_ddp(p: &[u8]) -> Result<Ddp<'_>, ParseError> {
    if p.len() < 10 || p[0] >> 6 != 1 {
        return Err(ParseError::NotOurs);
    }
    let flags = p[0];
    if flags & 0x06 != 0 {
        return Err(ParseError::Other); // reply or query
    }
    let id = p[3];
    if !matches!(id, 1..=245 | 255) {
        return Err(ParseError::Other);
    }
    let header = if flags & 0x10 != 0 { 14 } else { 10 };
    let len = be16(p, 8) as usize;
    if p.len() < header + len {
        return Err(ParseError::Malformed);
    }
    Ok(Ddp { offset: be32(p, 4), data: &p[header..header + len], push: flags & 0x01 != 0, sequence: p[1] & 0x0F, id })
}

/// Picks one sACN source per universe: the highest priority wins; between equal
/// priorities the one already being followed keeps it (no merging). A source is dropped
/// when it says it has terminated or after [`SACN_TIMEOUT_MS`] of silence. Packets out of
/// sequence (E1.31 §6.7.2: a repeat, or up to 19 behind the last) are discarded.
#[derive(Clone, Copy, Debug, Default)]
pub struct SacnArbiter {
    current: Option<Followed>,
}

#[derive(Clone, Copy, Debug)]
struct Followed {
    cid: [u8; 16],
    priority: u8,
    sequence: u8,
    last_ms: u32,
}

impl SacnArbiter {
    /// Whether `p`, arriving at `now_ms`, should drive the output.
    pub fn accept(&mut self, p: &Sacn<'_>, now_ms: u32) -> bool {
        if p.preview {
            return false;
        }
        if let Some(c) = self.current
            && now_ms.wrapping_sub(c.last_ms) > SACN_TIMEOUT_MS
        {
            self.current = None;
        }
        let ours = self.current.is_some_and(|c| c.cid == p.cid);
        if p.terminated {
            if ours {
                self.current = None;
            }
            return false;
        }
        match self.current {
            Some(c) if ours => {
                // E1.31 §6.7.2: discard when new − last is in (−20, 0].
                let ahead = p.dmx.sequence.wrapping_sub(c.sequence) as i8;
                if ahead <= 0 && ahead > -20 {
                    return false;
                }
            }
            Some(c) if p.priority <= c.priority => return false,
            _ => {}
        }
        self.current = Some(Followed { cid: p.cid, priority: p.priority, sequence: p.dmx.sequence, last_ms: now_ms });
        true
    }

    /// The followed source's CID, if there is one and it hasn't timed out.
    pub fn source(&self, now_ms: u32) -> Option<[u8; 16]> {
        self.current.filter(|c| now_ms.wrapping_sub(c.last_ms) <= SACN_TIMEOUT_MS).map(|c| c.cid)
    }
}

/// Where a string's pixels sit in universe space (sACN, Art-Net) and in DDP's flat space.
///
/// Universe layout follows the common controller convention (WLED, xLights, Falcon):
/// pixels are never split across universes. The first universe starts at
/// `start_channel` (1-based); each later one starts at channel 1; each holds at most
/// `pixels_per_universe` (170 RGB / 128 RGBW by default: all that fit in 512).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Patch {
    pub pixels: u16,
    /// 3 (RGB) or 4 (RGBW): what the controller sends per pixel.
    pub channels_per_pixel: u8,
    pub start_universe: u16,
    pub start_channel: u16,
    pub pixels_per_universe: u16,
    /// Byte offset of pixel 0 in DDP's channel space.
    pub ddp_offset: u32,
}

impl Patch {
    pub fn new(pixels: u16, channels_per_pixel: u8, start_universe: u16) -> Self {
        Patch {
            pixels,
            channels_per_pixel,
            start_universe,
            start_channel: 1,
            pixels_per_universe: 512 / channels_per_pixel as u16,
            ddp_offset: 0,
        }
    }

    fn cpp(&self) -> usize {
        self.channels_per_pixel.max(1) as usize
    }

    fn in_universe(&self, k: u16) -> usize {
        let room = if k == 0 { (512 - (self.start_channel.clamp(1, 512) as usize - 1)) / self.cpp() } else { 512 / self.cpp() };
        room.min(self.pixels_per_universe.max(1) as usize)
    }

    /// The universes this string spans, first..=last.
    pub fn universes(&self) -> (u16, u16) {
        let (mut left, mut k) = (self.pixels as usize, 0u16);
        while left > self.in_universe(k) {
            left -= self.in_universe(k);
            k += 1;
        }
        (self.start_universe, self.start_universe.saturating_add(k))
    }

    /// Hand each pixel in `dmx` for this string to `f(pixel_index, channels)`.
    pub fn apply_dmx(&self, dmx: &Dmx<'_>, mut f: impl FnMut(usize, &[u8])) {
        let (first, last) = self.universes();
        if dmx.universe < first || dmx.universe > last {
            return;
        }
        let k = dmx.universe - first;
        let base: usize = (0..k).map(|j| self.in_universe(j)).sum();
        let start = if k == 0 { self.start_channel.clamp(1, 512) as usize - 1 } else { 0 };
        let count = self.in_universe(k).min(self.pixels as usize - base);
        let cpp = self.cpp();
        for j in 0..count {
            match dmx.data.get(start + j * cpp..start + (j + 1) * cpp) {
                Some(ch) => f(base + j, ch),
                None => break,
            }
        }
    }

    /// The same for a DDP packet. Partial pixels at the edges of a packet are skipped
    /// (senders split frames on pixel boundaries).
    pub fn apply_ddp(&self, p: &Ddp<'_>, mut f: impl FnMut(usize, &[u8])) {
        let cpp = self.cpp() as u64;
        let (lo, hi) = (self.ddp_offset as u64, self.ddp_offset as u64 + self.pixels as u64 * cpp);
        let (plo, phi) = (p.offset as u64, p.offset as u64 + p.data.len() as u64);
        let (from, to) = (lo.max(plo), hi.min(phi));
        if from >= to {
            return;
        }
        let first = (from - lo).div_ceil(cpp);
        let mut px = first;
        while lo + (px + 1) * cpp <= to {
            let at = (lo + px * cpp - plo) as usize;
            f(px as usize, &p.data[at..at + cpp as usize]);
            px += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    fn sacn_packet(universe: u16, seq: u8, priority: u8, options: u8, cid: u8, data: &[u8]) -> Vec<u8> {
        let end = 126 + data.len();
        let mut p = std::vec![0u8; end];
        p[0..2].copy_from_slice(&0x0010u16.to_be_bytes());
        p[4..16].copy_from_slice(ACN_ID);
        p[16..18].copy_from_slice(&(0x7000 | (end - 16) as u16).to_be_bytes());
        p[18..22].copy_from_slice(&4u32.to_be_bytes());
        p[22..38].fill(cid);
        p[38..40].copy_from_slice(&(0x7000 | (end - 38) as u16).to_be_bytes());
        p[40..44].copy_from_slice(&2u32.to_be_bytes());
        p[44..50].copy_from_slice(b"xLight");
        p[108] = priority;
        p[111] = seq;
        p[112] = options;
        p[113..115].copy_from_slice(&universe.to_be_bytes());
        p[115..117].copy_from_slice(&(0x7000 | (end - 115) as u16).to_be_bytes());
        p[117] = 0x02;
        p[118] = 0xA1;
        p[121..123].copy_from_slice(&1u16.to_be_bytes());
        p[123..125].copy_from_slice(&(1 + data.len() as u16).to_be_bytes());
        p[126..].copy_from_slice(data);
        p
    }

    #[test]
    fn parses_sacn() {
        let pkt = sacn_packet(7, 42, 100, 0, 0xAB, &[1, 2, 3, 4, 5, 6]);
        let s = parse_sacn(&pkt).unwrap();
        assert_eq!(s.dmx, Dmx { universe: 7, sequence: 42, data: &[1, 2, 3, 4, 5, 6] });
        assert_eq!((s.priority, s.preview, s.terminated, s.cid[0]), (100, false, false, 0xAB));
        assert_eq!(s.source_name, b"xLight");
        assert_eq!(sacn_multicast(7), [239, 255, 0, 7]);
        assert_eq!(sacn_multicast(0x1234), [239, 255, 0x12, 0x34]);
    }

    #[test]
    fn rejects_bad_sacn() {
        let good = sacn_packet(1, 0, 100, 0, 1, &[0; 4]);
        assert_eq!(parse_sacn(b"hello"), Err(ParseError::NotOurs));
        let mut bad = good.clone();
        bad[124] = 200; // count past the end
        assert_eq!(parse_sacn(&bad), Err(ParseError::Malformed));
        let mut bad = good.clone();
        bad[17] ^= 1; // root layer length
        assert_eq!(parse_sacn(&bad), Err(ParseError::Malformed));
        let mut sync = good.clone();
        sync[21] = 8;
        assert_eq!(parse_sacn(&sync), Err(ParseError::Other));
        let mut alt = good.clone();
        alt[125] = 0xDD; // per-channel priority start code
        assert_eq!(parse_sacn(&alt), Err(ParseError::Other));
        assert_eq!(parse_sacn(&sacn_packet(0, 0, 100, 0, 1, &[0; 4])), Err(ParseError::Malformed));
    }

    #[test]
    fn arbitration() {
        let mut a = SacnArbiter::default();
        let lo = sacn_packet(1, 10, 100, 0, 1, &[0; 3]);
        let hi = sacn_packet(1, 50, 150, 0, 2, &[0; 3]);
        assert!(a.accept(&parse_sacn(&lo).unwrap(), 0));
        assert!(a.accept(&parse_sacn(&hi).unwrap(), 10), "higher priority takes over");
        assert!(!a.accept(&parse_sacn(&sacn_packet(1, 11, 100, 0, 1, &[0; 3])).unwrap(), 20));
        // Out of order from the followed source.
        assert!(!a.accept(&parse_sacn(&sacn_packet(1, 45, 150, 0, 2, &[0; 3])).unwrap(), 30));
        assert!(a.accept(&parse_sacn(&sacn_packet(1, 51, 150, 0, 2, &[0; 3])).unwrap(), 40));
        // Far behind (> 20) is a restarted source: accepted.
        assert!(a.accept(&parse_sacn(&sacn_packet(1, 0, 150, 0, 2, &[0; 3])).unwrap(), 50));
        // Silence: the lower-priority source gets it back.
        assert!(a.accept(&parse_sacn(&sacn_packet(1, 12, 100, 0, 1, &[0; 3])).unwrap(), 50 + SACN_TIMEOUT_MS + 1));
        // Terminated releases; preview never drives.
        assert!(!a.accept(&parse_sacn(&sacn_packet(1, 13, 100, 0x40, 1, &[0; 3])).unwrap(), 6000));
        assert_eq!(a.source(6000), None);
        assert!(!a.accept(&parse_sacn(&sacn_packet(1, 14, 100, 0x80, 1, &[0; 3])).unwrap(), 6001));
    }

    #[test]
    fn parses_artnet() {
        let mut p = b"Art-Net\0".to_vec();
        p.extend([0x00, 0x50, 0, 14, 9, 0, 0x23, 0x01, 0, 4, 10, 20, 30, 40]);
        let d = parse_artnet(&p).unwrap();
        assert_eq!(d, Dmx { universe: 0x123, sequence: 9, data: &[10, 20, 30, 40] });
        let mut poll = b"Art-Net\0".to_vec();
        poll.extend([0x00, 0x20, 0, 14]);
        assert_eq!(parse_artnet(&poll), Err(ParseError::Other));
        p[17] = 100;
        assert_eq!(parse_artnet(&p), Err(ParseError::Malformed));
    }

    #[test]
    fn parses_ddp() {
        let p = [0x41, 0x03, 0x0B, 0x01, 0, 0, 0, 6, 0, 3, 1, 2, 3];
        let d = parse_ddp(&p).unwrap();
        assert_eq!(d, Ddp { offset: 6, data: &[1, 2, 3], push: true, sequence: 3, id: 1 });
        let tc = [0x50, 0, 0, 1, 0, 0, 0, 0, 0, 1, 9, 9, 9, 9, 7];
        assert_eq!(parse_ddp(&tc).unwrap().data, &[7]);
        assert_eq!(parse_ddp(&[0x43, 0, 0, 1, 0, 0, 0, 0, 0, 0]), Err(ParseError::Other));
        assert_eq!(parse_ddp(&[0x41, 0, 0, 246, 0, 0, 0, 0, 0, 0]), Err(ParseError::Other));
        assert_eq!(parse_ddp(&[0x81, 0, 0, 1, 0, 0, 0, 0, 0, 0]), Err(ParseError::NotOurs));
    }

    fn collect_dmx(patch: &Patch, universe: u16, data: &[u8]) -> Vec<(usize, Vec<u8>)> {
        let mut v = Vec::new();
        patch.apply_dmx(&Dmx { universe, sequence: 0, data }, |i, ch| v.push((i, ch.to_vec())));
        v
    }

    #[test]
    fn patch_spans_universes_without_splitting_pixels() {
        let p = Patch::new(200, 3, 5);
        assert_eq!(p.universes(), (5, 6));
        let full: Vec<u8> = (0..=255u8).cycle().take(512).collect();
        let u5 = collect_dmx(&p, 5, &full);
        assert_eq!(u5.len(), 170);
        assert_eq!(u5[0], (0, std::vec![0, 1, 2]));
        assert_eq!(u5[169].0, 169);
        let u6 = collect_dmx(&p, 6, &full);
        assert_eq!(u6.len(), 30);
        assert_eq!(u6[0], (170, std::vec![0, 1, 2]));
        assert!(collect_dmx(&p, 7, &full).is_empty());
        // A short packet only fills what it carries.
        assert_eq!(collect_dmx(&p, 5, &[1, 2, 3, 4, 5]).len(), 1);
    }

    #[test]
    fn patch_with_start_channel() {
        let mut p = Patch::new(10, 3, 1);
        p.start_channel = 508; // room for one pixel (508–510) in the first universe
        assert_eq!(p.universes(), (1, 2));
        let data = [0u8; 512];
        assert_eq!(collect_dmx(&p, 1, &data).len(), 1);
        let u2 = collect_dmx(&p, 2, &data);
        assert_eq!((u2.len(), u2[0].0), (9, 1));
    }

    #[test]
    fn patch_ddp() {
        let mut p = Patch::new(4, 3, 1);
        p.ddp_offset = 6; // pixel 0 at channels 6..9
        let data: Vec<u8> = (0..30).collect();
        let pkt = Ddp { offset: 0, data: &data, push: true, sequence: 0, id: 1 };
        let mut got = Vec::new();
        p.apply_ddp(&pkt, |i, ch| got.push((i, ch.to_vec())));
        assert_eq!(got.len(), 4);
        assert_eq!(got[0], (0, std::vec![6, 7, 8]));
        assert_eq!(got[3], (3, std::vec![15, 16, 17]));
        // A packet starting mid-pixel skips the partial one.
        let pkt = Ddp { offset: 10, data: &data[..8], push: false, sequence: 0, id: 1 };
        let mut got = Vec::new();
        p.apply_ddp(&pkt, |i, ch| got.push((i, ch.to_vec())));
        assert_eq!(got, [(2, std::vec![2, 3, 4]), (3, std::vec![5, 6, 7])]);
    }
}
