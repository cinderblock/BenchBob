//! Small helpers that keep the API's behaviour identical to the TypeScript's: payloads,
//! hex and base64 as Node reads them, ANSI stripping, JavaScript regexes, number formatting.

use std::sync::OnceLock;

use benchbob_core::HttpError;
use benchbob_core::http::js_string;
use regex::Regex;
use serde_json::Value;

/// Strip ANSI escape sequences for agents (colours, cursor moves, OSC titles).
pub fn strip_ansi(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]").unwrap())
        .replace_all(s, "")
        .into_owned()
}

/// `new TextDecoder().decode(b)`: UTF-8 with replacement characters, a leading BOM dropped.
pub fn decode_utf8(b: &[u8]) -> String {
    let b = b.strip_prefix(&[0xef, 0xbb, 0xbf][..]).unwrap_or(b);
    String::from_utf8_lossy(b).into_owned()
}

pub fn parse_hex(s: &str) -> Result<Vec<u8>, HttpError> {
    static OX: OnceLock<Regex> = OnceLock::new();
    let s = OX.get_or_init(|| Regex::new("(?i)0x").unwrap()).replace_all(s, "");
    let clean: String = s.chars().filter(|c| !(c.is_whitespace() || *c == ',' || *c == ':')).collect();
    if !clean.len().is_multiple_of(2) || !clean.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(HttpError::bad_request("hex must be pairs of hex digits (spaces, commas, colons and 0x prefixes are ignored)"));
    }
    Ok((0..clean.len()).step_by(2).map(|i| u8::from_str_radix(&clean[i..i + 2], 16).unwrap()).collect())
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// `Buffer.from(s, "base64")`: lenient like Node's: both alphabets, anything else (spaces,
/// newlines) skipped, decoding stops at the first '='.
pub fn base64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// `Buffer.toString("hex")` with a space between bytes.
pub fn hex_spaced(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

fn eol(v: Option<&Value>) -> Result<&'static str, HttpError> {
    let Some(v) = v.filter(|v| !v.is_null()) else { return Ok("") };
    Ok(match js_string(v).to_lowercase().as_str() {
        "none" => "",
        "cr" => "\r",
        "lf" => "\n",
        "crlf" => "\r\n",
        _ => return Err(HttpError::bad_request("eol must be none, cr, lf or crlf")),
    })
}

/// Payload from `{text|hex|base64, eol}`.
pub fn payload(a: &Value) -> Result<Vec<u8>, HttpError> {
    let e = eol(a.get("eol"))?;
    let given = |k: &str| a.get(k).filter(|v| !v.is_null());
    let mut out = Vec::new();
    let mut any = false;
    if let Some(t) = given("text") {
        out.extend_from_slice(js_string(t).as_bytes());
        any = true;
    }
    if let Some(h) = given("hex") {
        out.extend(parse_hex(&js_string(h))?);
        any = true;
    }
    if let Some(b) = given("base64") {
        out.extend(base64_decode(&js_string(b)));
        any = true;
    }
    if !any {
        return Err(HttpError::bad_request("Give text, hex or base64"));
    }
    out.extend_from_slice(e.as_bytes());
    Ok(out)
}

/// `new RegExp(source, flags)` with the regex crate: i, m and s map to its flags (m with
/// CRLF mode, so `$` also ends a line before "\r" as JavaScript's does); g, u, v, y and d
/// change nothing for a single search. Lookaround and backreferences aren't supported.
pub fn js_regex(source: &str, flags: &str) -> Result<Regex, String> {
    let mut pre = String::from("R");
    for c in flags.chars() {
        match c {
            'i' | 'm' | 's' if !pre.contains(c) => pre.push(c),
            'g' | 'u' | 'v' | 'y' | 'd' => {}
            _ => return Err(format!("Invalid flags supplied to RegExp constructor '{flags}'")),
        }
    }
    if flags.contains('u') && flags.contains('v') {
        return Err(format!("Invalid flags supplied to RegExp constructor '{flags}'"));
    }
    Regex::new(&format!("(?{pre}){source}")).map_err(|e| {
        let msg = e.to_string();
        // The regex crate's message is several lines (pattern, caret, error); keep the error.
        let last = msg.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or(&msg).trim().to_string();
        format!("Invalid regular expression: /{source}/{flags}: {}", last.trim_start_matches("error: "))
    })
}

/// `x.toFixed(d)` as a number (`+x.toFixed(d)`): ties round up, as JavaScript does.
pub fn to_fixed(x: f64, d: usize) -> f64 {
    let s = format!("{x:.d$}");
    let exact = format!("{x:.60}");
    // JavaScript picks the larger n on an exact tie; Rust's formatting rounds half to even.
    if let Some(dot) = exact.find('.') {
        let tail = &exact[dot + 1 + d..];
        if tail.starts_with('5') && tail[1..].bytes().all(|c| c == b'0') {
            let up = (x * 10f64.powi(d as i32)).floor() + 1.0;
            return up / 10f64.powi(d as i32);
        }
    }
    s.parse().unwrap_or(x)
}

/// `Math.round(x)`.
pub fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// UTF-8 decoding of a stream (`TextDecoder.decode(chunk, { stream: true })`): an
/// incomplete sequence at the end of a chunk waits for the next one.
#[derive(Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub fn decode(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        let keep = incomplete_tail(&self.pending);
        let cut = self.pending.len() - keep;
        let s = String::from_utf8_lossy(&self.pending[..cut]).into_owned();
        self.pending.drain(..cut);
        s
    }
}

/// How many bytes at the end start a UTF-8 sequence that isn't finished yet.
fn incomplete_tail(b: &[u8]) -> usize {
    for back in 1..=3.min(b.len()) {
        let c = b[b.len() - back];
        if c & 0xc0 == 0x80 {
            continue;
        }
        let need = if c >= 0xf0 {
            4
        } else if c >= 0xe0 {
            3
        } else if c >= 0xc0 {
            2
        } else {
            1
        };
        return if need > back { back } else { 0 };
    }
    0
}

/// `new Date(ms).toISOString()`.
pub fn iso_time(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, rem % 3600 / 60, rem % 60, ms % 1000)
}

/// `n.toString(36)`.
pub fn base36(mut n: u64) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// The machine's hostname (`os.hostname()`).
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is writable for its length.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "localhost".into();
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// One part of a multipart/form-data body.
pub struct Part {
    pub name: String,
    /// Set for file fields (even when empty, as a browser sends it).
    pub filename: Option<String>,
    pub data: Vec<u8>,
}

/// Parse a multipart/form-data body, in order. None when it isn't well formed.
pub fn multipart(content_type: &str, body: &[u8]) -> Option<Vec<Part>> {
    let boundary = content_type.split(';').map(str::trim).find_map(|p| p.strip_prefix("boundary="))?.trim_matches('"');
    let delim = format!("--{boundary}").into_bytes();
    let find = |hay: &[u8], needle: &[u8], from: usize| -> Option<usize> {
        if hay.len() < needle.len() {
            return None;
        }
        (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
    };
    let mut parts = Vec::new();
    let mut pos = find(body, &delim, 0)? + delim.len();
    loop {
        if body[pos..].starts_with(b"--") {
            return Some(parts);
        }
        pos += if body[pos..].starts_with(b"\r\n") { 2 } else { return None };
        let head_end = find(body, b"\r\n\r\n", pos)?;
        let head = String::from_utf8_lossy(&body[pos..head_end]).into_owned();
        let start = head_end + 4;
        let mut d = b"\r\n".to_vec();
        d.extend_from_slice(&delim);
        let end = find(body, &d, start)?;
        let mut name = String::new();
        let mut filename = None;
        for line in head.split("\r\n") {
            let (k, v) = line.split_once(':')?;
            if !k.trim().eq_ignore_ascii_case("content-disposition") {
                continue;
            }
            for p in v.split(';').map(str::trim) {
                if let Some(n) = p.strip_prefix("name=") {
                    name = n.trim_matches('"').to_string();
                } else if let Some(f) = p.strip_prefix("filename=") {
                    filename = Some(f.trim_matches('"').to_string());
                }
            }
        }
        parts.push(Part { name, filename, data: body[start..end].to_vec() });
        pos = end + d.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn payloads() {
        assert_eq!(payload(&json!({ "text": "AT", "eol": "crlf" })).unwrap(), vec![65, 84, 13, 10]);
        assert_eq!(payload(&json!({ "hex": "0xde ad:BE,ef" })).unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(payload(&json!({ "base64": "AQI=" })).unwrap(), vec![1, 2]);
        assert!(parse_hex("abc").unwrap_err().message.contains("pairs"));
        assert!(payload(&json!({})).unwrap_err().message.contains("text, hex or base64"));
        assert!(payload(&json!({ "text": "x", "eol": "nl" })).unwrap_err().message.contains("eol"));
        assert_eq!(strip_ansi("\x1b[1;32mI (29) boot\x1b[0m\x1b]0;title\x07 ok"), "I (29) boot ok");
        assert_eq!(payload(&json!({ "text": 5 })).unwrap(), b"5");
    }

    #[test]
    fn base64_both_ways() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_decode("Zm9v\nYmFy"), b"foobar");
        assert_eq!(base64_decode("Zg"), b"f");
        assert_eq!(base64_decode("_-8="), vec![0xff, 0xef]);
        assert_eq!(hex_spaced(&[0xde, 0xad, 1]), "de ad 01");
    }

    #[test]
    fn javascript_regexes() {
        let r = js_regex(r"\n> ?$", "m").unwrap();
        assert!(r.is_match("help\r\n> \r\nmore"));
        assert!(js_regex("ok", "i").unwrap().is_match("OK"));
        assert!(js_regex("a", "x").unwrap_err().starts_with("Invalid flags"));
        assert!(js_regex("(", "m").unwrap_err().starts_with("Invalid regular expression: /(/m: "));
        // Without m, $ is the end of the input.
        assert!(!js_regex("a$", "").unwrap().is_match("a\nb"));
    }

    #[test]
    fn numbers_and_dates() {
        assert_eq!(to_fixed(0.0625, 3), 0.063);
        assert_eq!(to_fixed(0.91234, 3), 0.912);
        assert_eq!(to_fixed(1.0, 2), 1.0);
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(iso_time(1_792_532_090_542), "2026-10-20T21:34:50.542Z");
        assert_eq!(base36(1_792_532_090_542), "mvh7bp2m");
    }

    #[test]
    fn utf8_split_across_chunks() {
        let mut d = Utf8Stream::default();
        let s = "héllo ✓".as_bytes();
        let mut out = String::new();
        for b in s {
            out += &d.decode(&[*b]);
        }
        assert_eq!(out, "héllo ✓");
    }

    #[test]
    fn multipart_bodies() {
        let body = b"--XX\r\nContent-Disposition: form-data; name=\"tool\"\r\n\r\nesptool\r\n--XX\r\nContent-Disposition: form-data; name=\"file\"; filename=\"app.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n\x01\x02\r\n\x03\r\n--XX\r\nContent-Disposition: form-data; name=\"offset\"\r\n\r\n0x10000\r\n--XX--\r\n";
        let p = multipart("multipart/form-data; boundary=XX", body).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!((p[0].name.as_str(), p[0].data.as_slice(), p[0].filename.is_none()), ("tool", &b"esptool"[..], true));
        assert_eq!(p[1].filename.as_deref(), Some("app.bin"));
        assert_eq!(p[1].data, b"\x01\x02\r\n\x03");
        assert_eq!(p[2].data, b"0x10000");
    }
}
