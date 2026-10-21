//! A minimal read-only reader for Windows registry hive files ("regf", regf.ts): enough to
//! pull values out of BCD stores and the SOFTWARE / SYSTEM hives on a mounted Windows
//! drive.
//!
//! Hives are read by random access through a small page cache rather than loaded whole:
//! SOFTWARE is often 100 MB+, and a lookup touches a few dozen cells. Every offset comes
//! from an untrusted file, so reads are bounds-checked and list walks capped; a damaged
//! hive is an error, never a loop.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Result, bail};

const PAGE: u64 = 4096;
const MAX_CACHED_PAGES: usize = 4096; // 16 MB
const HBIN_BASE: u64 = 0x1000;
const MAX_LIST: usize = 100_000;

pub const REG_SZ: u32 = 1;
pub const REG_EXPAND_SZ: u32 = 2;
pub const REG_DWORD: u32 = 4;
pub const REG_MULTI_SZ: u32 = 7;
pub const REG_QWORD: u32 = 11;

#[derive(Debug, Clone)]
pub struct Key {
    pub name: String,
    subkey_count: u32,
    subkey_list: u32,
    value_count: u32,
    value_list: u32,
}

#[derive(Debug, Clone)]
struct RegValue {
    name: String,
    kind: u32,
    size: u32,
    inline: bool,
    offset: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Data {
    Str(String),
    Multi(Vec<String>),
    Num(f64),
    Bytes(Vec<u8>),
}

pub struct Hive {
    f: File,
    size: u64,
    pages: HashMap<u64, Vec<u8>>,
}

fn u16le(b: &[u8], at: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(b.get(at..at + 2).ok_or_else(|| anyhow::anyhow!("regf: read out of range"))?.try_into()?))
}
fn u32le(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(b.get(at..at + 4).ok_or_else(|| anyhow::anyhow!("regf: read out of range"))?.try_into()?))
}
fn latin1(b: &[u8]) -> String {
    b.iter().map(|c| *c as char).collect()
}
/// TextDecoder("utf-16le"): lone surrogates become U+FFFD; an odd last byte too.
pub fn utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    let mut s: String = char::decode_utf16(units).map(|r| r.unwrap_or('\u{fffd}')).collect();
    if b.len() % 2 == 1 {
        s.push('\u{fffd}');
    }
    s
}

impl Hive {
    pub fn open(path: &Path) -> Option<Hive> {
        let f = File::open(path).ok()?;
        let size = f.metadata().ok()?.len();
        let mut h = Hive { f, size, pages: HashMap::new() };
        match h.bytes(0, 4) {
            Ok(b) if b == b"regf" => Some(h),
            _ => None,
        }
    }

    fn bytes(&mut self, off: u64, len: u64) -> Result<Vec<u8>> {
        if off + len > self.size {
            bail!("regf: read out of range");
        }
        let mut out = Vec::with_capacity(len as usize);
        let mut done = 0u64;
        while done < len {
            let p = (off + done) / PAGE;
            if !self.pages.contains_key(&p) {
                let mut buf = vec![0u8; PAGE as usize];
                self.f.seek(SeekFrom::Start(p * PAGE))?;
                let mut n = 0;
                while n < buf.len() {
                    let r = self.f.read(&mut buf[n..])?;
                    if r == 0 {
                        break;
                    }
                    n += r;
                }
                buf.truncate(n);
                if self.pages.len() >= MAX_CACHED_PAGES {
                    self.pages.clear();
                }
                self.pages.insert(p, buf);
            }
            let page = &self.pages[&p];
            let at = (off + done - p * PAGE) as usize;
            let n = ((len - done) as usize).min(page.len().saturating_sub(at));
            if n == 0 {
                bail!("regf: short read");
            }
            out.extend_from_slice(&page[at..at + n]);
            done += n as u64;
        }
        Ok(out)
    }

    /// Cell offsets are relative to the first hive bin; a cell starts with its size
    /// (negative while allocated).
    fn cell(&mut self, off: u32) -> Result<Vec<u8>> {
        let raw = self.bytes(HBIN_BASE + u64::from(off), 4)?;
        let size = i32::from_le_bytes(raw[..4].try_into()?).unsigned_abs();
        if !(8..=16 * 1024 * 1024).contains(&size) {
            bail!("regf: bad cell size");
        }
        self.bytes(HBIN_BASE + u64::from(off) + 4, u64::from(size) - 4)
    }

    pub fn root(&mut self) -> Result<Key> {
        let b = self.bytes(0x24, 4)?;
        let off = u32le(&b, 0)?;
        self.key(off)
    }

    fn key(&mut self, off: u32) -> Result<Key> {
        let c = self.cell(off)?;
        if c.len() < 2 || c[0] != b'n' || c[1] != b'k' {
            bail!("regf: expected nk");
        }
        let flags = u16le(&c, 2)?;
        let name_len = u16le(&c, 72)? as usize;
        let raw = c.get(76..(76 + name_len).min(c.len())).unwrap_or(&[]);
        Ok(Key {
            name: if flags & 0x20 != 0 { latin1(raw) } else { utf16(raw) },
            subkey_count: u32le(&c, 20)?,
            subkey_list: u32le(&c, 28)?,
            value_count: u32le(&c, 36)?,
            value_list: u32le(&c, 40)?,
        })
    }

    /// Offsets of every key in a subkey list; "ri" is a list of lists, one level deep.
    fn list_offsets(&mut self, off: u32, depth: u8) -> Result<Vec<u32>> {
        let c = self.cell(off)?;
        let sig = latin1(c.get(0..2).unwrap_or(&[]));
        let n = (u16le(&c, 2)? as usize).min(MAX_LIST);
        let mut out = Vec::new();
        match sig.as_str() {
            "lf" | "lh" => {
                for i in 0..n {
                    out.push(u32le(&c, 4 + i * 8)?);
                }
            }
            "li" => {
                for i in 0..n {
                    out.push(u32le(&c, 4 + i * 4)?);
                }
            }
            "ri" if depth == 0 => {
                for i in 0..n {
                    let o = u32le(&c, 4 + i * 4)?;
                    out.extend(self.list_offsets(o, 1)?);
                }
            }
            s => bail!("regf: unexpected subkey list {s}"),
        }
        Ok(out)
    }

    pub fn subkeys(&mut self, k: &Key) -> Result<Vec<Key>> {
        if k.subkey_count == 0 || k.subkey_list == 0xffff_ffff {
            return Ok(vec![]);
        }
        let offs = self.list_offsets(k.subkey_list, 0)?;
        offs.into_iter().map(|o| self.key(o)).collect()
    }

    /// A subkey by name, case-insensitively, as Windows matches them.
    pub fn child(&mut self, k: Option<&Key>, name: &str) -> Result<Option<Key>> {
        let Some(k) = k else { return Ok(None) };
        let want = name.to_lowercase();
        Ok(self.subkeys(k)?.into_iter().find(|s| s.name.to_lowercase() == want))
    }

    pub fn path(&mut self, k: &Key, parts: &[&str]) -> Result<Option<Key>> {
        let mut cur = Some(k.clone());
        for p in parts {
            cur = self.child(cur.as_ref(), p)?;
            if cur.is_none() {
                return Ok(None);
            }
        }
        Ok(cur)
    }

    fn values(&mut self, k: &Key) -> Result<Vec<RegValue>> {
        if k.value_count == 0 || k.value_list == 0xffff_ffff {
            return Ok(vec![]);
        }
        let list = self.cell(k.value_list)?;
        let mut out = Vec::new();
        for i in 0..(k.value_count as usize).min(MAX_LIST) {
            let c = self.cell(u32le(&list, i * 4)?)?;
            if c.len() < 2 || c[0] != b'v' || c[1] != b'k' {
                continue;
            }
            let name_len = u16le(&c, 2)? as usize;
            let raw_size = u32le(&c, 4)?;
            let raw = c.get(20..(20 + name_len).min(c.len())).unwrap_or(&[]);
            out.push(RegValue {
                name: if u16le(&c, 16)? & 1 != 0 { latin1(raw) } else { utf16(raw) },
                kind: u32le(&c, 12)?,
                size: raw_size & 0x7fff_ffff,
                inline: raw_size & 0x8000_0000 != 0,
                offset: u32le(&c, 8)?,
            });
        }
        Ok(out)
    }

    fn data(&mut self, v: &RegValue) -> Result<Vec<u8>> {
        if v.inline {
            return Ok(v.offset.to_le_bytes()[..(v.size as usize).min(4)].to_vec());
        }
        let c = self.cell(v.offset)?;
        // Values over ~16 KB are split into "db" big-data segments.
        if v.size as usize > c.len() && c.len() >= 2 && c[0] == b'd' && c[1] == b'b' {
            let segs = self.cell(u32le(&c, 4)?)?;
            let mut out = vec![0u8; v.size as usize];
            let mut at = 0usize;
            let count = u16le(&c, 2)? as usize;
            let mut i = 0;
            while i < count && at < v.size as usize {
                let s = self.cell(u32le(&segs, i * 4)?)?;
                let n = s.len().min(v.size as usize - at).min(16344);
                out[at..at + n].copy_from_slice(&s[..n]);
                at += n;
                i += 1;
            }
            return Ok(out);
        }
        Ok(c[..(v.size as usize).min(c.len())].to_vec())
    }

    /// A value: string for SZ, list for MULTI_SZ, number for DWORD/QWORD, bytes otherwise.
    /// Names match case-insensitively, like Windows.
    pub fn get(&mut self, k: Option<&Key>, name: &str) -> Result<Option<Data>> {
        let Some(k) = k else { return Ok(None) };
        let want = name.to_lowercase();
        let Some(v) = self.values(k)?.into_iter().find(|x| x.name.to_lowercase() == want) else { return Ok(None) };
        let d = self.data(&v)?;
        Ok(match v.kind {
            REG_SZ | REG_EXPAND_SZ => Some(Data::Str(utf16(&d).trim_end_matches('\0').to_string())),
            REG_MULTI_SZ => Some(Data::Multi(utf16(&d).split('\0').filter(|s| !s.is_empty()).map(String::from).collect())),
            REG_DWORD => (d.len() >= 4).then(|| Data::Num(f64::from(u32::from_le_bytes(d[..4].try_into().unwrap())))),
            REG_QWORD => (d.len() >= 8).then(|| Data::Num(u64::from_le_bytes(d[..8].try_into().unwrap()) as f64)),
            _ => Some(Data::Bytes(d)),
        })
    }

    pub fn str(&mut self, k: Option<&Key>, name: &str) -> Result<Option<String>> {
        Ok(match self.get(k, name)? {
            Some(Data::Str(s)) => Some(s),
            _ => None,
        })
    }

    pub fn num(&mut self, k: Option<&Key>, name: &str) -> Result<Option<f64>> {
        Ok(match self.get(k, name)? {
            Some(Data::Num(n)) => Some(n),
            _ => None,
        })
    }
}

/// Open, run, close. None when the file isn't a hive.
pub fn with_hive<T>(path: &Path, f: impl FnOnce(&mut Hive) -> Result<T>) -> Result<Option<T>> {
    match Hive::open(path) {
        Some(mut h) => f(&mut h).map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
pub mod build {
    //! A tiny hive writer, for tests: keys with values, laid out as Windows does (nk / vk
    //! cells, lf subkey lists, big values in db segments).

    pub enum V {
        Sz(&'static str),
        Dword(u32),
        Qword(u64),
        Multi(Vec<&'static str>),
        Bin(Vec<u8>),
    }

    pub struct K {
        pub name: &'static str,
        pub values: Vec<(&'static str, V)>,
        pub keys: Vec<K>,
    }

    pub fn k(name: &'static str, values: Vec<(&'static str, V)>, keys: Vec<K>) -> K {
        K { name, values, keys }
    }

    struct W {
        bin: Vec<u8>,
    }

    impl W {
        /// A cell (negative size = allocated), 8-aligned; returns its offset.
        fn cell(&mut self, data: &[u8]) -> u32 {
            let off = self.bin.len() as u32;
            let size = (data.len() + 4).div_ceil(8) * 8;
            self.bin.extend_from_slice(&(-(size as i32)).to_le_bytes());
            self.bin.extend_from_slice(data);
            self.bin.resize(off as usize + size, 0);
            off
        }
        fn utf16(s: &str) -> Vec<u8> {
            s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
        }
        fn value(&mut self, name: &str, v: &V) -> u32 {
            let (kind, data): (u32, Vec<u8>) = match v {
                V::Sz(s) => (1, Self::utf16(&format!("{s}\0"))),
                V::Dword(n) => (4, n.to_le_bytes().to_vec()),
                V::Qword(n) => (11, n.to_le_bytes().to_vec()),
                V::Multi(l) => (7, Self::utf16(&(l.join("\0") + "\0\0"))),
                V::Bin(b) => (3, b.clone()),
            };
            let (size, offset) = if data.len() <= 4 {
                let mut b = [0u8; 4];
                b[..data.len()].copy_from_slice(&data);
                (data.len() as u32 | 0x8000_0000, u32::from_le_bytes(b))
            } else if data.len() > 16344 {
                let segs: Vec<u32> = data.chunks(16344).map(|c| self.cell(c)).collect();
                let list: Vec<u8> = segs.iter().flat_map(|o| o.to_le_bytes()).collect();
                let l = self.cell(&list);
                let mut db = b"db".to_vec();
                db.extend_from_slice(&(segs.len() as u16).to_le_bytes());
                db.extend_from_slice(&l.to_le_bytes());
                (data.len() as u32, self.cell(&db))
            } else {
                (data.len() as u32, self.cell(&data))
            };
            let mut vk = b"vk".to_vec();
            vk.extend_from_slice(&(name.len() as u16).to_le_bytes());
            vk.extend_from_slice(&size.to_le_bytes());
            vk.extend_from_slice(&offset.to_le_bytes());
            vk.extend_from_slice(&kind.to_le_bytes());
            vk.extend_from_slice(&1u16.to_le_bytes()); // ASCII name
            vk.extend_from_slice(&[0, 0]);
            vk.extend_from_slice(name.as_bytes());
            self.cell(&vk)
        }
        fn key(&mut self, k: &K) -> u32 {
            let subs: Vec<u32> = k.keys.iter().map(|s| self.key(s)).collect();
            let sub_list = if subs.is_empty() {
                0xffff_ffff
            } else {
                let mut lf = b"lf".to_vec();
                lf.extend_from_slice(&(subs.len() as u16).to_le_bytes());
                for o in &subs {
                    lf.extend_from_slice(&o.to_le_bytes());
                    lf.extend_from_slice(&[0; 4]);
                }
                self.cell(&lf)
            };
            let vals: Vec<u32> = k.values.iter().map(|(n, v)| self.value(n, v)).collect();
            let val_list =
                if vals.is_empty() { 0xffff_ffff } else { self.cell(&vals.iter().flat_map(|o| o.to_le_bytes()).collect::<Vec<_>>()) };
            let mut nk = vec![0u8; 76];
            nk[0..2].copy_from_slice(b"nk");
            nk[2..4].copy_from_slice(&0x20u16.to_le_bytes());
            nk[20..24].copy_from_slice(&(subs.len() as u32).to_le_bytes());
            nk[28..32].copy_from_slice(&sub_list.to_le_bytes());
            nk[36..40].copy_from_slice(&(vals.len() as u32).to_le_bytes());
            nk[40..44].copy_from_slice(&val_list.to_le_bytes());
            nk[72..74].copy_from_slice(&(k.name.len() as u16).to_le_bytes());
            nk.extend_from_slice(k.name.as_bytes());
            self.cell(&nk)
        }
    }

    pub fn hive(root: &K) -> Vec<u8> {
        let mut w = W { bin: Vec::new() };
        let r = w.key(root);
        let mut out = vec![0u8; 0x1000];
        out[0..4].copy_from_slice(b"regf");
        out[0x24..0x28].copy_from_slice(&r.to_le_bytes());
        out.extend_from_slice(&w.bin);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::build::*;
    use super::*;

    #[test]
    fn reads_what_was_written() {
        let big: Vec<u8> = (0..40000u32).map(|i| (i % 251) as u8).collect();
        let h = hive(&k(
            "ROOT",
            vec![],
            vec![k(
                "Microsoft",
                vec![
                    ("ProductName", V::Sz("Windows 10 Pro")),
                    ("UBR", V::Dword(4317)),
                    ("Big", V::Bin(big.clone())),
                    ("Q", V::Qword(1 << 40)),
                    ("M", V::Multi(vec!["{a}", "{b}"])),
                    ("Tiny", V::Bin(vec![1, 2])),
                ],
                vec![k("Sub", vec![], vec![])],
            )],
        ));
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("SOFTWARE");
        std::fs::write(&p, h).unwrap();
        let r = with_hive(&p, |h| {
            let root = h.root()?;
            let ms = h.path(&root, &["microsoft"])?.unwrap();
            assert!(h.child(Some(&ms), "SUB")?.is_some());
            assert_eq!(h.str(Some(&ms), "productname")?.as_deref(), Some("Windows 10 Pro"));
            assert_eq!(h.num(Some(&ms), "UBR")?, Some(4317.0));
            assert_eq!(h.num(Some(&ms), "Q")?, Some((1u64 << 40) as f64));
            assert_eq!(h.get(Some(&ms), "M")?, Some(Data::Multi(vec!["{a}".into(), "{b}".into()])));
            assert_eq!(h.get(Some(&ms), "Big")?, Some(Data::Bytes(big.clone())));
            assert_eq!(h.get(Some(&ms), "Tiny")?, Some(Data::Bytes(vec![1, 2])));
            assert_eq!(h.get(Some(&ms), "nope")?, None);
            Ok(())
        })
        .unwrap();
        assert!(r.is_some());
        std::fs::write(&p, b"not a hive").unwrap();
        assert!(with_hive(&p, |_| Ok(())).unwrap().is_none());
    }
}
