//! A minimal mDNS responder (RFC 6762 / DNS-SD RFC 6763) for one host and one HTTP
//! service, so a BenchBob server can find nodes:
//!
//! - `<host>.local` A
//! - `_benchbob._tcp.local` PTR `<host>._benchbob._tcp.local`, with SRV (port 80) and TXT
//!   (`board=…`, `version=…`, `api=/api`, `mcp=/mcp`)
//! - `_http._tcp.local` PTR `<host>._http._tcp.local`, so generic browsers list it too
//! - `_services._dns-sd._udp.local` PTR both service types
//!
//! It answers queries and builds announcements. There is no probing or conflict
//! resolution: pick unique hostnames (the default includes the MAC).

pub const PORT: u16 = 5353;
pub const GROUP: [u8; 4] = [224, 0, 0, 251];

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_SRV: u16 = 33;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
const CACHE_FLUSH: u16 = 0x8000;

const TTL_HOST: u32 = 120;
const TTL_SHARED: u32 = 4500;

pub const SERVICE_TYPES: [&str; 2] = ["_benchbob._tcp", "_http._tcp"];

pub struct Service<'a> {
    /// Bare hostname (no `.local`); also used as the instance name.
    pub hostname: &'a str,
    pub ip: [u8; 4],
    pub port: u16,
    /// TXT strings for the `_benchbob._tcp` instance, `key=value`.
    pub txt: &'a [&'a str],
}

/// What a question asks about, after matching it against our names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Subject {
    Host,
    /// One of SERVICE_TYPES (index).
    ServiceType(usize),
    Instance(usize),
    ServicesMeta,
}

/// A dotted, lowercased name read from a packet.
struct Name {
    buf: [u8; 256],
    len: usize,
}

impl Name {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

fn read_name(p: &[u8], mut i: usize) -> Option<(Name, usize)> {
    let mut n = Name { buf: [0; 256], len: 0 };
    let mut end = None;
    let mut jumps = 0;
    loop {
        let l = *p.get(i)? as usize;
        if l == 0 {
            return Some((n, end.unwrap_or(i + 1)));
        }
        if l & 0xC0 == 0xC0 {
            let off = ((l & 0x3F) << 8) | *p.get(i + 1)? as usize;
            end.get_or_insert(i + 2);
            jumps += 1;
            if jumps > 16 {
                return None;
            }
            i = off;
            continue;
        }
        if l > 63 {
            return None;
        }
        let label = p.get(i + 1..i + 1 + l)?;
        if n.len + l + 1 > n.buf.len() {
            return None;
        }
        if n.len > 0 {
            n.buf[n.len] = b'.';
            n.len += 1;
        }
        for &c in label {
            n.buf[n.len] = c.to_ascii_lowercase();
            n.len += 1;
        }
        i += 1 + l;
    }
}

fn eq_name(a: &str, parts: &[&str]) -> bool {
    // a == parts joined with '.', case-insensitively.
    let mut rest = a;
    for (k, part) in parts.iter().enumerate() {
        if rest.len() < part.len() || !rest[..part.len()].eq_ignore_ascii_case(part) {
            return false;
        }
        rest = &rest[part.len()..];
        if k + 1 < parts.len() {
            match rest.strip_prefix('.') {
                Some(r) => rest = r,
                None => return false,
            }
        }
    }
    rest.is_empty()
}

fn classify(name: &str, svc: &Service<'_>) -> Option<Subject> {
    if eq_name(name, &[svc.hostname, "local"]) {
        return Some(Subject::Host);
    }
    if eq_name(name, &["_services._dns-sd._udp", "local"]) {
        return Some(Subject::ServicesMeta);
    }
    for (k, t) in SERVICE_TYPES.iter().enumerate() {
        if eq_name(name, &[t, "local"]) {
            return Some(Subject::ServiceType(k));
        }
        if eq_name(name, &[svc.hostname, t, "local"]) {
            return Some(Subject::Instance(k));
        }
    }
    None
}

/// Packet writer (no name compression: these packets are small).
struct W<'a> {
    out: &'a mut [u8],
    n: usize,
    ok: bool,
}

impl W<'_> {
    fn bytes(&mut self, b: &[u8]) {
        if self.n + b.len() > self.out.len() {
            self.ok = false;
            return;
        }
        self.out[self.n..self.n + b.len()].copy_from_slice(b);
        self.n += b.len();
    }
    fn u16(&mut self, v: u16) {
        self.bytes(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_be_bytes());
    }
    /// Labels from dotted parts, e.g. ["host", "_benchbob._tcp", "local"].
    fn name(&mut self, parts: &[&str]) {
        for part in parts {
            for label in part.split('.') {
                let l = label.len().min(63);
                self.bytes(&[l as u8]);
                self.bytes(&label.as_bytes()[..l]);
            }
        }
        self.bytes(&[0]);
    }
    /// A record header; returns where to patch the data length.
    fn rr(&mut self, name: &[&str], ty: u16, flush: bool, ttl: u32) -> usize {
        self.name(name);
        self.u16(ty);
        self.u16(CLASS_IN | if flush { CACHE_FLUSH } else { 0 });
        self.u32(ttl);
        let at = self.n;
        self.u16(0);
        at
    }
    fn patch_len(&mut self, at: usize) {
        if self.ok {
            let len = (self.n - at - 2) as u16;
            self.out[at..at + 2].copy_from_slice(&len.to_be_bytes());
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rec {
    A,
    Ptr(usize),
    Srv(usize),
    Txt(usize),
    Meta(usize),
}

fn write_rec(w: &mut W<'_>, r: Rec, svc: &Service<'_>) {
    let h = svc.hostname;
    match r {
        Rec::A => {
            let at = w.rr(&[h, "local"], TYPE_A, true, TTL_HOST);
            w.bytes(&svc.ip);
            w.patch_len(at);
        }
        Rec::Ptr(k) => {
            let at = w.rr(&[SERVICE_TYPES[k], "local"], TYPE_PTR, false, TTL_SHARED);
            w.name(&[h, SERVICE_TYPES[k], "local"]);
            w.patch_len(at);
        }
        Rec::Meta(k) => {
            let at = w.rr(&["_services._dns-sd._udp", "local"], TYPE_PTR, false, TTL_SHARED);
            w.name(&[SERVICE_TYPES[k], "local"]);
            w.patch_len(at);
        }
        Rec::Srv(k) => {
            let at = w.rr(&[h, SERVICE_TYPES[k], "local"], TYPE_SRV, true, TTL_HOST);
            w.u16(0);
            w.u16(0);
            w.u16(svc.port);
            w.name(&[h, "local"]);
            w.patch_len(at);
        }
        Rec::Txt(k) => {
            let at = w.rr(&[h, SERVICE_TYPES[k], "local"], TYPE_TXT, true, TTL_SHARED);
            let txt: &[&str] = if k == 0 { svc.txt } else { &["path=/api"] };
            for t in txt {
                let l = t.len().min(255);
                w.bytes(&[l as u8]);
                w.bytes(&t.as_bytes()[..l]);
            }
            if txt.is_empty() {
                w.bytes(&[0]);
            }
            w.patch_len(at);
        }
    }
}

/// A small set of records, deduplicated.
struct Set {
    r: [Option<Rec>; 12],
}

impl Set {
    fn new() -> Self {
        Set { r: [None; 12] }
    }
    fn add(&mut self, rec: Rec) {
        if self.r.contains(&Some(rec)) {
            return;
        }
        if let Some(slot) = self.r.iter_mut().find(|x| x.is_none()) {
            *slot = Some(rec);
        }
    }
    fn has(&self, rec: Rec) -> bool {
        self.r.contains(&Some(rec))
    }
    fn count(&self) -> u16 {
        self.r.iter().filter(|x| x.is_some()).count() as u16
    }
    fn iter(&self) -> impl Iterator<Item = Rec> + '_ {
        self.r.iter().flatten().copied()
    }
}

fn header(w: &mut W<'_>, id: u16, qd: u16, an: u16, ar: u16) {
    w.u16(id);
    w.u16(0x8400); // response, authoritative
    w.u16(qd);
    w.u16(an);
    w.u16(0);
    w.u16(ar);
}

/// Answer a query packet. `legacy`: it came from a port other than 5353 (a one-shot
/// resolver), so the reply goes back unicast with the query id and question echoed.
/// Returns the reply length, or None when nothing in it is for us.
pub fn respond(q: &[u8], legacy: bool, svc: &Service<'_>, out: &mut [u8]) -> Option<usize> {
    if q.len() < 12 || q[2] & 0x80 != 0 {
        return None; // too short, or a response
    }
    let id = u16::from_be_bytes([q[0], q[1]]);
    let qd = u16::from_be_bytes([q[4], q[5]]);
    let mut ans = Set::new();
    let mut i = 12;
    let mut first_q: Option<(usize, usize)> = None;
    for _ in 0..qd {
        let (name, next) = read_name(q, i)?;
        let ty = u16::from_be_bytes([*q.get(next)?, *q.get(next + 1)?]);
        let class = u16::from_be_bytes([*q.get(next + 2)?, *q.get(next + 3)?]) & 0x7FFF;
        first_q.get_or_insert((i, next + 4));
        i = next + 4;
        if class != CLASS_IN && class != 255 {
            continue;
        }
        let any = ty == TYPE_ANY;
        match classify(name.as_str(), svc) {
            Some(Subject::Host) if any || ty == TYPE_A => ans.add(Rec::A),
            Some(Subject::ServiceType(k)) if any || ty == TYPE_PTR => ans.add(Rec::Ptr(k)),
            Some(Subject::ServicesMeta) if any || ty == TYPE_PTR => {
                ans.add(Rec::Meta(0));
                ans.add(Rec::Meta(1));
            }
            Some(Subject::Instance(k)) => {
                if any || ty == TYPE_SRV {
                    ans.add(Rec::Srv(k));
                }
                if any || ty == TYPE_TXT {
                    ans.add(Rec::Txt(k));
                }
            }
            _ => {}
        }
    }
    if ans.count() == 0 {
        return None;
    }
    // Additional records: what a browser needs next to connect.
    let mut add = Set::new();
    for r in ans.iter() {
        match r {
            Rec::Ptr(k) => {
                add.add(Rec::Srv(k));
                add.add(Rec::Txt(k));
                add.add(Rec::A);
            }
            Rec::Srv(_) => add.add(Rec::A),
            _ => {}
        }
    }
    let mut extra = Set::new();
    for r in add.iter().filter(|r| !ans.has(*r)) {
        extra.add(r);
    }
    let mut w = W { out, n: 0, ok: true };
    let echo = if legacy { first_q } else { None };
    header(&mut w, if legacy { id } else { 0 }, echo.is_some() as u16, ans.count(), extra.count());
    if let Some((a, b)) = echo {
        w.bytes(&q[a..b]);
    }
    for r in ans.iter().chain(extra.iter()) {
        write_rec(&mut w, r, svc);
    }
    w.ok.then_some(w.n)
}

/// An unsolicited announcement of every record (send it a couple of times after the
/// address comes up, RFC 6762 §8.3).
pub fn announce(svc: &Service<'_>, out: &mut [u8]) -> Option<usize> {
    let recs = [Rec::A, Rec::Ptr(0), Rec::Srv(0), Rec::Txt(0), Rec::Ptr(1), Rec::Srv(1), Rec::Txt(1)];
    let mut w = W { out, n: 0, ok: true };
    header(&mut w, 0, 0, recs.len() as u16, 0);
    for r in recs {
        write_rec(&mut w, r, svc);
    }
    w.ok.then_some(w.n)
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    const SVC: Service = Service { hostname: "benchbob-0a0b0c", ip: [192, 168, 1, 50], port: 80, txt: &["board=pico2w", "version=0.1.0"] };

    fn query(names: &[(&str, u16)]) -> Vec<u8> {
        let mut q = std::vec![0x12, 0x34, 0, 0, 0, names.len() as u8, 0, 0, 0, 0, 0, 0];
        for (n, t) in names {
            for l in n.split('.') {
                q.push(l.len() as u8);
                q.extend_from_slice(l.as_bytes());
            }
            q.push(0);
            q.extend_from_slice(&t.to_be_bytes());
            q.extend_from_slice(&1u16.to_be_bytes());
        }
        q
    }

    /// Parse a response into (name, type, rdata) for every record.
    fn records(p: &[u8]) -> Vec<(std::string::String, u16, Vec<u8>)> {
        let qd = u16::from_be_bytes([p[4], p[5]]) as usize;
        let n = (u16::from_be_bytes([p[6], p[7]]) + u16::from_be_bytes([p[10], p[11]])) as usize;
        let mut i = 12;
        for _ in 0..qd {
            i = read_name(p, i).unwrap().1 + 4;
        }
        let mut out = Vec::new();
        for _ in 0..n {
            let (name, next) = read_name(p, i).unwrap();
            let ty = u16::from_be_bytes([p[next], p[next + 1]]);
            let len = u16::from_be_bytes([p[next + 8], p[next + 9]]) as usize;
            out.push((name.as_str().into(), ty, p[next + 10..next + 10 + len].to_vec()));
            i = next + 10 + len;
        }
        assert_eq!(i, p.len());
        out
    }

    #[test]
    fn answers_hostname() {
        let mut out = [0u8; 512];
        let n = respond(&query(&[("BenchBob-0a0b0c.local", TYPE_A)]), false, &SVC, &mut out).unwrap();
        assert_eq!(&out[..4], &[0, 0, 0x84, 0]);
        let r = records(&out[..n]);
        assert_eq!(r, [("benchbob-0a0b0c.local".into(), TYPE_A, std::vec![192, 168, 1, 50])]);
    }

    #[test]
    fn answers_service_browse_with_additionals() {
        let mut out = [0u8; 512];
        let n = respond(&query(&[("_benchbob._tcp.local", TYPE_PTR)]), false, &SVC, &mut out).unwrap();
        let r = records(&out[..n]);
        let types: Vec<u16> = r.iter().map(|x| x.1).collect();
        assert_eq!(types, [TYPE_PTR, TYPE_SRV, TYPE_TXT, TYPE_A]);
        // SRV: priority, weight, port 80, target.
        assert_eq!(&r[1].2[4..6], &80u16.to_be_bytes());
        assert_eq!(r[2].2, b"\x0cboard=pico2w\x0dversion=0.1.0");
    }

    #[test]
    fn ignores_others_and_responses() {
        let mut out = [0u8; 512];
        assert!(respond(&query(&[("other.local", TYPE_A)]), false, &SVC, &mut out).is_none());
        let mut resp = query(&[("benchbob-0a0b0c.local", TYPE_A)]);
        resp[2] = 0x84;
        assert!(respond(&resp, false, &SVC, &mut out).is_none());
        assert!(respond(&[0; 5], false, &SVC, &mut out).is_none());
    }

    #[test]
    fn legacy_unicast_echoes_id_and_question() {
        let mut out = [0u8; 512];
        let q = query(&[("benchbob-0a0b0c.local", TYPE_A)]);
        let n = respond(&q, true, &SVC, &mut out).unwrap();
        assert_eq!(&out[..2], &[0x12, 0x34]);
        assert_eq!(u16::from_be_bytes([out[4], out[5]]), 1);
        assert_eq!(records(&out[..n]).len(), 1);
    }

    #[test]
    fn follows_compression_pointers() {
        // Two questions; the second's name is "_http._tcp" + pointer to "local" in the first.
        let mut q = query(&[("benchbob-0a0b0c.local", TYPE_A)]);
        q[5] = 2;
        let local_at = 12 + 1 + 15; // after the first label
        q.extend_from_slice(&[5, b'_', b'h', b't', b't', b'p', 4, b'_', b't', b'c', b'p', 0xC0, local_at as u8]);
        q.extend_from_slice(&TYPE_PTR.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        let mut out = [0u8; 1024];
        let n = respond(&q, false, &SVC, &mut out).unwrap();
        let r = records(&out[..n]);
        assert_eq!(r[0].1, TYPE_A);
        assert_eq!((r[1].0.as_str(), r[1].1), ("_http._tcp.local", TYPE_PTR));
    }

    #[test]
    fn announcement_parses() {
        let mut out = [0u8; 1024];
        let n = announce(&SVC, &mut out).unwrap();
        assert_eq!(records(&out[..n]).len(), 7);
        assert!(announce(&SVC, &mut [0u8; 40]).is_none());
    }
}
