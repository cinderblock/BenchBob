//! The raw sector probe and the deep scan (server.ts "Raw Sector Probe"). A filesystem
//! that says "empty" says nothing about what is still on the medium: these read the block
//! device itself, so a quick-erased or freshly formatted drive still shows what the
//! previous image left behind.

use std::collections::BTreeSet;

use benchbob_core::http::{num, parse_int, round_to};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::devices::{is_sys_dev, size_bytes};
use crate::ops::Op;
use crate::sys::Sys;
use crate::util::{fmt_bytes, js_round};

pub const PROBE_BLOCK: usize = 4096;

struct Signature {
    name: &'static str,
    bytes: &'static [u8],
    /// Required offset within each block; otherwise matched anywhere.
    at: Option<usize>,
}

/// Ordered roughly by how much they tell. `at` ones are filesystem/container headers that
/// only mean anything at a fixed offset, which keeps false positives down.
const SIGNATURES: &[Signature] = &[
    Signature { name: "MBR partition table", bytes: &[0x55, 0xaa], at: Some(510) },
    Signature { name: "GPT header", bytes: b"EFI PART", at: None },
    Signature { name: "ext2/3/4 superblock", bytes: &[0x53, 0xef], at: Some(0x438) },
    Signature { name: "FAT32", bytes: b"FAT32", at: Some(0x52) },
    Signature { name: "FAT12/16", bytes: b"FAT1", at: Some(0x36) },
    Signature { name: "NTFS", bytes: b"NTFS    ", at: Some(3) },
    Signature { name: "exFAT", bytes: b"EXFAT   ", at: Some(3) },
    Signature { name: "LUKS volume", bytes: &[0x4c, 0x55, 0x4b, 0x53, 0xba, 0xbe], at: None },
    Signature { name: "squashfs", bytes: b"hsqs", at: None },
    Signature { name: "Linux swap", bytes: b"SWAPSPACE2", at: None },
    Signature { name: "arm64 kernel Image", bytes: b"ARMd", at: Some(0x38) },
    Signature { name: "devicetree blob", bytes: &[0xd0, 0x0d, 0xfe, 0xed], at: None },
    Signature { name: "ELF binary", bytes: &[0x7f, 0x45, 0x4c, 0x46], at: None },
    Signature { name: "SQLite database", bytes: b"SQLite format 3\0", at: None },
    Signature { name: "JPEG", bytes: &[0xff, 0xd8, 0xff], at: None },
    Signature { name: "PNG", bytes: &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], at: None },
    Signature { name: "GIF", bytes: b"GIF8", at: None },
    Signature { name: "PDF", bytes: b"%PDF-", at: None },
    Signature { name: "ZIP / Office / jar", bytes: &[0x50, 0x4b, 0x03, 0x04], at: None },
    Signature { name: "gzip", bytes: &[0x1f, 0x8b, 0x08], at: None },
    Signature { name: "xz", bytes: &[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00], at: None },
    Signature { name: "bzip2", bytes: b"BZh", at: None },
    Signature { name: "zstd", bytes: &[0x28, 0xb5, 0x2f, 0xfd], at: None },
    Signature { name: "7-zip", bytes: &[0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c], at: None },
    Signature { name: "tar archive", bytes: b"ustar", at: Some(257) },
    Signature { name: "MP4 / MOV", bytes: b"ftyp", at: Some(4) },
    Signature { name: "Matroska / WebM", bytes: &[0x1a, 0x45, 0xdf, 0xa3], at: None },
    Signature { name: "RIFF (wav/avi)", bytes: b"RIFF", at: None },
    Signature { name: "Ogg", bytes: b"OggS", at: None },
    Signature { name: "FLAC", bytes: b"fLaC", at: None },
    Signature { name: "MP3 (ID3)", bytes: b"ID3", at: None },
];

fn max_sig_len() -> usize {
    SIGNATURES.iter().map(|s| s.bytes.len()).max().unwrap_or(1)
}

fn matches_at(buf: &[u8], pos: usize, bytes: &[u8]) -> bool {
    pos + bytes.len() <= buf.len() && &buf[pos..pos + bytes.len()] == bytes
}

/// Absolute offsets of every signature hit in `buf`, which starts at device offset `base`.
pub fn find_signatures(buf: &[u8], base: u64) -> Vec<(&'static str, u64)> {
    let mut hits = Vec::new();
    for sig in SIGNATURES {
        if let Some(at) = sig.at {
            // Anchored: alignment is relative to the device, not the buffer (in the deep
            // scan `base` is offset by the carry-over and isn't block-aligned).
            let blk = PROBE_BLOCK as u64;
            let mut abs = base.div_ceil(blk) * blk;
            while (abs - base) as usize + at + sig.bytes.len() <= buf.len() {
                if matches_at(buf, (abs - base) as usize + at, sig.bytes) {
                    hits.push((sig.name, abs));
                }
                abs += blk;
            }
        } else {
            let first = sig.bytes[0];
            for i in 0..buf.len() {
                if buf[i] == first && matches_at(buf, i, sig.bytes) {
                    hits.push((sig.name, base + i as u64));
                }
            }
        }
    }
    hits
}

pub struct Reading {
    pub offset: u64,
    pub kind: &'static str,
    pub entropy: f64,
    pub signatures: Vec<&'static str>,
}

impl Reading {
    pub fn to_json(&self) -> Value {
        json!({ "offset": self.offset, "kind": self.kind, "entropy": num(self.entropy), "signatures": self.signatures })
    }
}

/// zero / ones / text / compressed / structured, with Shannon entropy in bits per byte.
pub fn classify(buf: &[u8], offset: u64) -> Reading {
    let mut freq = [0u32; 256];
    let (mut zeros, mut ones, mut printable) = (0usize, 0usize, 0usize);
    for &b in buf {
        freq[b as usize] += 1;
        if b == 0 {
            zeros += 1;
        } else if b == 0xff {
            ones += 1;
        }
        if (0x20..0x7f).contains(&b) || b == 0x09 || b == 0x0a || b == 0x0d {
            printable += 1;
        }
    }
    let n = if buf.is_empty() { 1 } else { buf.len() };
    let mut entropy = 0.0f64;
    for f in freq {
        if f == 0 {
            continue;
        }
        let p = f as f64 / n as f64;
        entropy -= p * p.log2();
    }
    let kind = if zeros == n {
        "zero"
    } else if ones == n {
        "ones"
    } else if printable as f64 / n as f64 > 0.85 {
        "text"
    } else if entropy >= 7.5 {
        "compressed"
    } else {
        "structured"
    };
    let mut signatures: Vec<&'static str> = Vec::new();
    for (name, _) in find_signatures(buf, 0) {
        if !signatures.contains(&name) {
            signatures.push(name);
        }
    }
    Reading { offset, kind, entropy: round_to(entropy, 2), signatures }
}

/// Up to `count` block indexes spread evenly over [start, end).
pub fn spread_blocks(start: u64, end: u64, count: usize) -> Vec<i64> {
    let blk = PROBE_BLOCK as f64;
    let first = (start as f64 / blk).floor() as i64;
    let last = ((end as f64 - blk) / blk).floor() as i64;
    if last < first {
        return Vec::new();
    }
    let span = last - first;
    let n = (count as i64).min(span + 1);
    if n <= 1 {
        return vec![first];
    }
    let mut out: Vec<i64> = Vec::new();
    for i in 0..n {
        let v = first + js_round((span * i) as f64 / (n - 1) as f64) as i64;
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// Byte ranges no partition covers: where leftovers hide.
pub async fn unallocated(sys: &dyn Sys, dev: &str, total: f64) -> Vec<(f64, f64)> {
    let out = sys.run(&["sudo", "sfdisk", "-J", &format!("/dev/{dev}")]).await.out;
    let Ok(v) = serde_json::from_str::<Value>(&out) else {
        return if total > 0.0 { vec![(0.0, total)] } else { vec![] };
    };
    let pt = v.get("partitiontable");
    let ss = pt.and_then(|p| p.get("sectorsize")).and_then(Value::as_f64).filter(|x| *x != 0.0).unwrap_or(512.0);
    let mut used: Vec<(f64, f64)> = pt
        .and_then(|p| p.get("partitions"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|p| {
            let (s, z) =
                (p.get("start").and_then(Value::as_f64).unwrap_or(f64::NAN), p.get("size").and_then(Value::as_f64).unwrap_or(f64::NAN));
            (s * ss, (s + z) * ss)
        })
        .collect();
    used.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut gaps = Vec::new();
    let mut cursor = 0.0f64;
    for (s, e) in used {
        if s > cursor {
            gaps.push((cursor, s));
        }
        cursor = cursor.max(e);
    }
    if cursor < total {
        gaps.push((cursor, total));
    }
    gaps.retain(|(s, e)| e - s >= PROBE_BLOCK as f64);
    gaps
}

/// One sudo, many seeks.
pub async fn read_blocks(sys: &dyn Sys, dev: &str, idx: &[i64]) -> Vec<Vec<u8>> {
    if idx.is_empty() {
        return Vec::new();
    }
    let script = idx
        .iter()
        .map(|i| format!("dd if=/dev/{dev} bs={PROBE_BLOCK} count=1 skip={i} status=none 2>/dev/null"))
        .collect::<Vec<_>>()
        .join("; ");
    let all = sys.run_bytes(&["sudo", "sh", "-c", &script]).await;
    let mut out = Vec::new();
    for i in 0..idx.len() {
        let s = i * PROBE_BLOCK;
        if s >= all.len() {
            break;
        }
        out.push(all[s..(s + PROBE_BLOCK).min(all.len())].to_vec());
    }
    out
}

/// `probeDevice`: a fast sampled read, seconds rather than minutes.
pub async fn probe(sys: &dyn Sys, dev: &str) -> Value {
    if is_sys_dev(dev) {
        return json!({ "ok": false, "message": "Cannot probe system device" });
    }
    let total = size_bytes(sys, dev).await;
    if total == 0.0 {
        return json!({ "ok": false, "message": "Could not determine device size" });
    }
    let gaps = unallocated(sys, dev, total).await;
    let t = total as u64;
    let mut plan: Vec<i64> = Vec::new();
    let mut add = |v: Vec<i64>| {
        for b in v {
            if !plan.contains(&b) {
                plan.push(b);
            }
        }
    };
    add(spread_blocks(0, t, 192));
    for (s, e) in &gaps {
        add(spread_blocks(*s as u64, *e as u64, 48));
    }
    add(spread_blocks(0, t.min(1 << 20), 16));
    add(spread_blocks(t.saturating_sub(1 << 20), t, 16));
    let idx: Vec<i64> = plan.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
    let blocks = read_blocks(sys, dev, &idx).await;
    let readings: Vec<Reading> = blocks.iter().enumerate().map(|(i, b)| classify(b, idx[i] as u64 * PROBE_BLOCK as u64)).collect();

    let mut counts: IndexMap<&str, u64> = ["zero", "ones", "text", "compressed", "structured"].into_iter().map(|k| (k, 0)).collect();
    for r in &readings {
        *counts.get_mut(r.kind).unwrap() += 1;
    }
    let mut sigs: IndexMap<&str, Vec<u64>> = IndexMap::new();
    for r in &readings {
        for n in &r.signatures {
            let l = sigs.entry(n).or_default();
            if l.len() < 8 {
                l.push(r.offset);
            }
        }
    }
    let sampled = if readings.is_empty() { 1 } else { readings.len() };
    let empty = counts["zero"] + counts["ones"];
    let empty_pct = if empty as usize == sampled { 100.0 } else { 99f64.min(js_round(empty as f64 / sampled as f64 * 100.0)) };
    let in_gaps: Vec<&Reading> =
        readings.iter().filter(|r| gaps.iter().any(|(s, e)| r.offset as f64 >= *s && (r.offset as f64) < *e)).collect();
    let with_data = in_gaps.iter().filter(|r| r.kind != "zero" && r.kind != "ones").count();
    let bytes = sampled * PROBE_BLOCK;
    let mut msg = format!("Sampled {sampled} blocks ({}) — {}% empty", fmt_bytes(bytes as f64), crate::util::js_num_str(empty_pct));
    if with_data > 0 {
        msg += &format!(", {with_data} block(s) with data outside any partition");
    }
    if !sigs.is_empty() {
        msg += &format!(", {} signature type(s) found", sigs.len());
    }
    json!({
        "ok": true,
        "device": dev,
        "sizeBytes": num(total),
        "sampledBlocks": sampled,
        "blockSize": PROBE_BLOCK,
        "bytesRead": bytes,
        "counts": counts,
        "emptyPct": num(empty_pct),
        "unallocated": {
            "ranges": gaps.iter().map(|(s, e)| json!({ "start": num(*s), "end": num(*e), "bytes": num(e - s) })).collect::<Vec<_>>(),
            "sampled": in_gaps.len(),
            "withData": with_data,
        },
        "signatures": sigs.iter().map(|(n, o)| json!({ "name": n, "offsets": o })).collect::<Vec<_>>(),
        "map": readings.iter().map(|r| json!({ "o": r.offset, "k": r.kind, "e": num(r.entropy) })).collect::<Vec<_>>(),
        "message": msg,
    })
}

/// `doDeepScan`: every byte, as a tracked, cancellable operation. Returns the result to
/// keep for /api/scanresult.
pub async fn deep_scan(sys: &dyn Sys, op: &Op) -> anyhow::Result<Value> {
    let dev = op.lock().unwrap().device.clone();
    let total = size_bytes(sys, &dev).await;
    op.lock().unwrap().set("scanning", Some(0.0), "Scanning raw device...");
    let mut p = sys.spawn(&["sudo", "dd", &format!("if=/dev/{dev}"), "bs=4M", "status=none"])?;
    op.lock().unwrap().killer = Some(p.killer());
    let mut hits: IndexMap<&'static str, (u64, Vec<u64>)> = IndexMap::new();
    let mut pos: u64 = 0;
    let mut carry: Vec<u8> = Vec::new();
    let max = max_sig_len();
    while let Some(chunk) = p.stdout().await {
        if op.lock().unwrap().aborted {
            (p.killer())();
            break;
        }
        let mut buf = std::mem::take(&mut carry);
        buf.extend_from_slice(&chunk);
        let base = pos - (buf.len() - chunk.len()) as u64;
        // A hit starting in the trailing overlap is left for the next chunk, so one that
        // straddles a boundary is counted exactly once.
        let carry_len = buf.len().min(max - 1);
        let boundary = base + (buf.len() - carry_len) as u64;
        for (name, off) in find_signatures(&buf, base) {
            if off >= boundary {
                continue;
            }
            let e = hits.entry(name).or_insert((0, Vec::new()));
            e.0 += 1;
            if e.1.len() < 10 {
                e.1.push(off);
            }
        }
        pos += chunk.len() as u64;
        carry = buf[buf.len() - carry_len..].to_vec();
        let mut o = op.lock().unwrap();
        o.progress = if total > 0.0 { 99f64.min(js_round(pos as f64 / total * 100.0)) } else { 0.0 };
        o.message = format!(
            "Scanned {}{} — {} signature type(s)",
            fmt_bytes(pos as f64),
            if total > 0.0 { format!(" / {}", fmt_bytes(total)) } else { String::new() },
            hits.len()
        );
    }
    p.wait().await;
    if op.lock().unwrap().aborted {
        anyhow::bail!("Cancelled");
    }
    let mut list: Vec<(&str, u64, Vec<u64>)> = hits.iter().map(|(n, (c, f))| (*n, *c, f.clone())).collect();
    // Stable, by count descending, as Array.prototype.sort is.
    list.sort_by_key(|x| std::cmp::Reverse(x.1));
    let result = json!({
        "bytesScanned": pos,
        "signatures": list.iter().map(|(n, c, f)| json!({ "name": n, "count": c, "first": f })).collect::<Vec<_>>(),
    });
    let mut o = op.lock().unwrap();
    o.scan_result = Some(result.clone());
    o.progress = 100.0;
    o.status = "done".into();
    o.message = if hits.is_empty() {
        format!("No known signatures in {} — device looks genuinely blank", fmt_bytes(pos as f64))
    } else {
        format!("Found {} signature type(s) in {}", hits.len(), fmt_bytes(pos as f64))
    };
    let mut kept: Map<String, Value> = result.as_object().cloned().unwrap_or_default();
    kept.insert("device".into(), dev.into());
    kept.insert("at".into(), sys.now_ms().into());
    Ok(Value::Object(kept))
}

/// `parseInt(x) || 0` of a JSON number or string.
pub fn int_or_zero(s: &str) -> i64 {
    parse_int(s).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::mock::b64;

    fn expected() -> Value {
        serde_json::from_str(include_str!("../fixtures/blocks.expected.json")).unwrap()
    }

    #[test]
    fn blocks_classify_like_server_ts() {
        let e = expected();
        for b in e["blocks"].as_array().unwrap() {
            let buf = b64(b["b64"].as_str().unwrap());
            let off = b["classify"]["offset"].as_u64().unwrap();
            assert_eq!(classify(&buf, off).to_json(), b["classify"], "{}", b["kind"]);
            let sigs: Vec<Value> = find_signatures(&buf, off).iter().map(|(n, o)| json!({ "name": n, "offset": o })).collect();
            assert_eq!(Value::Array(sigs), b["sigs"], "{}", b["kind"]);
        }
        let o = &e["offset"];
        let sigs: Vec<Value> = find_signatures(&b64(o["b64"].as_str().unwrap()), o["base"].as_u64().unwrap())
            .iter()
            .map(|(n, x)| json!({ "name": n, "offset": x }))
            .collect();
        assert_eq!(Value::Array(sigs), o["sigs"]);
        for s in e["spread"].as_array().unwrap() {
            let a: Vec<u64> = s["args"].as_array().unwrap().iter().map(|x| x.as_u64().unwrap()).collect();
            assert_eq!(json!(spread_blocks(a[0], a[1], a[2] as usize)), s["blocks"], "{a:?}");
        }
    }
}
