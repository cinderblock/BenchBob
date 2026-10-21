//! Small helpers several panels need to answer exactly as the TypeScript did: udev's string
//! escapes and USB names (core.ts `udevDecode`/`usbName`), JavaScript's rounding, number
//! printing of parsed JSON, dates, base64 and the host name.

use serde_json::Value;

use crate::udev::Props;

/// udev's \x20-style escapes in ID_MODEL_ENC etc. (core.ts `udevDecode`), trimmed.
pub fn udev_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' && b[i + 2].is_ascii_hexdigit() && b[i + 3].is_ascii_hexdigit() {
            // String.fromCharCode: one code unit, i.e. Latin-1 for one byte.
            let v = u8::from_str_radix(std::str::from_utf8(&b[i + 2..i + 4]).unwrap_or("00"), 16).unwrap_or(0);
            out.push(v as char);
            i += 4;
            continue;
        }
        // Copy one UTF-8 character.
        let len = utf8_len(b[i]);
        out.push_str(&String::from_utf8_lossy(&b[i..(i + len).min(b.len())]));
        i += len;
    }
    out.trim().to_string()
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xf0..=0xff => 4,
        0xe0..=0xef => 3,
        0xc0..=0xdf => 2,
        _ => 1,
    }
}

/// Vendor and product for display (core.ts `usbName`): the database names, else the
/// descriptor strings.
pub fn usb_name(p: &Props) -> (String, String) {
    let pick = |db: &str, enc: &str, raw: &str| {
        let get = |k: &str| p.get(k).cloned().unwrap_or_default();
        let d = get(db);
        if !d.is_empty() {
            return d;
        }
        let e = udev_decode(&get(enc));
        if !e.is_empty() {
            return e;
        }
        get(raw)
    };
    (pick("ID_VENDOR_FROM_DATABASE", "ID_VENDOR_ENC", "ID_VENDOR"), pick("ID_MODEL_FROM_DATABASE", "ID_MODEL_ENC", "ID_MODEL"))
}

/// A udev property, "" when missing.
pub fn prop<'a>(p: &'a Props, k: &str) -> &'a str {
    p.get(k).map(String::as_str).unwrap_or("")
}

/// `Math.round(x)`: half up.
pub fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// Make parsed JSON print as `JSON.stringify(JSON.parse(text))` would: integral floats
/// (`121.0`, `1e3`) become integers.
pub fn js_normalize(v: &mut Value) {
    match v {
        Value::Number(n) if !n.is_i64() && !n.is_u64() => {
            if let Some(f) = n.as_f64() {
                *v = crate::http::num(f);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(js_normalize),
        Value::Object(o) => o.values_mut().for_each(js_normalize),
        _ => {}
    }
}

/// `JSON.parse` with numbers as JavaScript prints them.
pub fn parse_json(text: &str) -> Option<Value> {
    let mut v: Value = serde_json::from_str(text).ok()?;
    js_normalize(&mut v);
    Some(v)
}

/// (year, month, day, h, m, s, ms) in UTC for epoch milliseconds.
fn civil(ms: i64) -> (i64, i64, i64, i64, i64, i64, i64) {
    let secs = ms.div_euclid(1000);
    let msr = ms.rem_euclid(1000);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60, msr)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `new Date(ms).toISOString()`
pub fn iso(ms: i64) -> String {
    let (y, mo, d, h, mi, s, ms) = civil(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

/// `Date.parse` for the ISO 8601 forms tools print: `2026-10-21T12:34:56.789Z`, with an
/// offset (`+02:00`), a space instead of the T, or no zone (read as UTC; JavaScript would
/// use local time, which on a bench server is UTC more often than not). None for NaN.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.trim().as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = b.get(r)?;
        if t.iter().all(u8::is_ascii_digit) { std::str::from_utf8(t).ok()?.parse().ok() } else { None }
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut ms = days_from_civil(y, mo, d) * 86_400_000;
    if b.len() == 10 {
        return Some(ms);
    }
    if !matches!(b.get(10), Some(b'T' | b' ')) || b.get(13) != Some(&b':') {
        return None;
    }
    let (h, mi) = (num(11..13)?, num(14..16)?);
    let mut i = 16;
    let mut sec = 0;
    if b.get(16) == Some(&b':') {
        sec = num(17..19)?;
        i = 19;
    }
    if h > 24 || mi > 59 || sec > 59 {
        return None;
    }
    ms += (h * 3600 + mi * 60 + sec) * 1000;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        let mut j = start;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == start {
            return None;
        }
        let frac = std::str::from_utf8(&b[start..j.min(start + 3)]).ok()?;
        ms += frac.parse::<i64>().ok()? * 10i64.pow(3 - frac.len() as u32);
        i = j;
    }
    match b.get(i) {
        None => Some(ms),
        Some(b'Z') if i + 1 == b.len() => Some(ms),
        Some(&c @ (b'+' | b'-')) => {
            let oh = num(i + 1..i + 3)?;
            let om = if b.get(i + 3) == Some(&b':') { num(i + 4..i + 6)? } else { num(i + 3..i + 5).unwrap_or(0) };
            let off = (oh * 60 + om) * 60_000;
            Some(if c == b'+' { ms - off } else { ms + off })
        }
        _ => None,
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `Buffer.toString("base64")`
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

/// `Buffer.from(s, "base64")`: lenient like Node's (both alphabets, other characters
/// skipped, stops at the first '=').
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

/// `os.hostname()`
pub fn hostname() -> String {
    let h = crate::run::read_text("/proc/sys/kernel/hostname");
    if h.is_empty() { "localhost".into() } else { h }
}

/// `a.localeCompare(b, undefined, { numeric: true })` for the ASCII names it is used on:
/// case-insensitive first, digit runs compared as numbers.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let si = i;
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            let sj = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let na = std::str::from_utf8(&a[si..i]).unwrap().trim_start_matches('0');
            let nb = std::str::from_utf8(&b[sj..j]).unwrap().trim_start_matches('0');
            let o = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
            if o != Ordering::Equal {
                return o;
            }
            continue;
        }
        let o = a[i].to_ascii_lowercase().cmp(&b[j].to_ascii_lowercase());
        if o != Ordering::Equal {
            return o;
        }
        i += 1;
        j += 1;
    }
    (a.len() - i).cmp(&(b.len() - j)).then_with(|| a.cmp(b).reverse())
}

/// `a.localeCompare(b)` for ASCII words: case-insensitive, lower case first on a tie.
pub fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()).then_with(|| b.cmp(a))
}

/// Call one of a driver's routes the way the server would (the most literal match wins),
/// for tests. `body` null sends no body.
pub async fn call_route(
    routes: &[crate::Route],
    method: axum::http::Method,
    path: &str,
    query: &[(&str, &str)],
    body: Value,
) -> crate::RouteResult {
    let (r, params) = routes
        .iter()
        .filter(|r| r.method == method)
        .filter_map(|r| crate::http::match_path(&r.path, path).map(|p| (r, p)))
        .min_by_key(|(_, p)| p.len())
        .ok_or_else(|| crate::HttpError::new(404, format!("no route {method} {path}")))?;
    let req = crate::Request {
        method,
        path: path.into(),
        params,
        query: query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        headers: Default::default(),
        body: match body {
            Value::Null => Default::default(),
            Value::String(s) => s.into(),
            v => v.to_string().into(),
        },
    };
    (r.handler)(req).await
}

/// Call a driver's MCP tool, for tests: its JSON result (content blocks as `{content,
/// structured}`).
pub async fn call_tool(tools: &[crate::Tool], name: &str, args: Value) -> Result<Value, crate::HttpError> {
    let t = tools.iter().find(|t| t.name == name).ok_or_else(|| crate::HttpError::new(404, format!("no tool {name}")))?;
    Ok(match (t.handler)(args).await? {
        crate::ToolResult::Value(v) => v,
        crate::ToolResult::Content { content, structured } => serde_json::json!({ "content": content, "structured": structured }),
    })
}

/// A driver Ctx over a settings file in `dir`, for tests.
pub fn test_ctx(id: &'static str, dir: &std::path::Path) -> crate::Ctx {
    let hub = std::sync::Arc::new(crate::driver::Hub::new(crate::settings::Settings::load(dir.join("peripherals.json"))));
    hub.settings.own(id);
    crate::Ctx::new(id, hub)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udev_strings() {
        assert_eq!(udev_decode("Debugprobe\\x20on\\x20Pico\\x20\\x28CMSIS-DAP\\x29"), "Debugprobe on Pico (CMSIS-DAP)");
        assert_eq!(udev_decode(" caf\u{e9}\\x20 "), "caf\u{e9}");
        let p: Props = [("ID_MODEL_ENC", "A\\x20B"), ("ID_MODEL", "A_B"), ("ID_VENDOR", "V")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(usb_name(&p), ("V".into(), "A B".into()));
    }

    #[test]
    fn numbers_and_dates() {
        let mut v = serde_json::json!({ "a": 121.0, "b": [1.5, 2.0], "c": 7 });
        js_normalize(&mut v);
        assert_eq!(v.to_string(), r#"{"a":121,"b":[1.5,2],"c":7}"#);
        assert_eq!(js_round(-2.5), -2.0);
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(iso(1792225996360), "2026-10-17T08:33:16.360Z");
        assert_eq!(parse_iso("2026-10-17T08:33:16.360Z"), Some(1792225996360));
        assert_eq!(parse_iso("2026-10-17T08:33:16Z"), Some(1792225996000));
        assert_eq!(parse_iso("2026-10-17T10:33:16.36+02:00"), Some(1792225996360));
        assert_eq!(parse_iso("2026-10-17 08:33:16"), Some(1792225996000));
        assert_eq!(parse_iso("nope"), None);
        assert_eq!(parse_iso("1970-01-01"), Some(0));
    }

    #[test]
    fn base64_round_trip() {
        for s in [&b""[..], b"f", b"fo", b"foo", b"foob", b"\xff\x00\x10"] {
            assert_eq!(base64_decode(&base64_encode(s)), s);
        }
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn sorting() {
        let mut v = vec!["ttyUSB10", "ttyACM1", "ttyUSB2", "ttyACM0"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["ttyACM0", "ttyACM1", "ttyUSB2", "ttyUSB10"]);
        let mut g = vec!["SBAS", "GPS", "Galileo", "GLONASS", "BeiDou", "QZSS"];
        g.sort_by(|a, b| locale_cmp(a, b));
        assert_eq!(g, ["BeiDou", "Galileo", "GLONASS", "GPS", "QZSS", "SBAS"]);
    }
}
