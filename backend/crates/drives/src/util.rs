//! Small helpers that make this crate's output identical to server.ts's: JavaScript's
//! number formatting (`toFixed`), `fmtBytes`, shell quoting, `encodeURIComponent`,
//! `localeCompare`, `JSON.stringify(v, null, 2)` and HTTP dates.

use std::cmp::Ordering;

use serde_json::Value;

/// `Number.prototype.toFixed(f)`: round half up on the exact decimal value of the double
/// (Rust's `{:.f}` rounds an exact tie to even, so 1.25 would print as "1.2").
pub fn to_fixed(x: f64, f: usize) -> String {
    if !x.is_finite() {
        return if x.is_nan() {
            "NaN".into()
        } else if x > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if x.abs() >= 1e21 {
        return format!("{x}");
    }
    let neg = x < 0.0;
    // A double's exact decimal expansion has at most 1074 fractional digits; values this
    // function sees are ≥ 1e-3 or so, whose expansions end well within 80.
    let exact = format!("{:.80}", x.abs());
    let (int, frac) = exact.split_once('.').unwrap_or((&exact, ""));
    let mut digits: Vec<u8> = int.bytes().chain(frac.bytes().take(f)).map(|c| c - b'0').collect();
    let rest = &frac.as_bytes()[f.min(frac.len())..];
    if rest.first().is_some_and(|c| *c >= b'5') {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, 1);
                break;
            }
            i -= 1;
            if digits[i] == 9 {
                digits[i] = 0;
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let n = digits.len() - f;
    let mut s: String = digits[..n].iter().map(|d| (b'0' + d) as char).collect();
    if s.is_empty() {
        s.push('0');
    }
    if f > 0 {
        s.push('.');
        s.extend(digits[n..].iter().map(|d| (b'0' + d) as char));
    }
    let zero = s.bytes().all(|c| c == b'0' || c == b'.');
    if neg && !zero { format!("-{s}") } else { s }
}

/// server.ts `fmtBytes`.
pub fn fmt_bytes(bytes: f64) -> String {
    if bytes == 0.0 {
        return "0 B".into();
    }
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let i = ((bytes.ln() / 1024f64.ln()).floor() as i64).clamp(i64::MIN, UNITS.len() as i64 - 1);
    // Math.floor of a negative log (bytes < 1) indexes units[-1]: undefined, as in JS.
    if i < 0 {
        return format!("{} undefined", js_num_str(bytes / 1024f64.powi(i as i32)));
    }
    let i = i as usize;
    format!("{} {}", to_fixed(bytes / 1024f64.powi(i as i32), if i > 0 { 1 } else { 0 }), UNITS[i])
}

/// `String(n)` for a number.
pub fn js_num_str(n: f64) -> String {
    benchbob_core::http::js_string(&benchbob_core::http::num(n))
}

/// server.ts `shellEsc`: for use inside single quotes.
pub fn shell_esc(s: &str) -> String {
    s.replace('\'', "'\\''")
}

/// `encodeURIComponent`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `Math.round(x)`.
pub fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// `JSON.stringify(v, null, 2)`, with integral numbers written as integers.
pub fn js_pretty(v: &Value) -> String {
    serde_json::to_string_pretty(&js_numbers(v)).unwrap_or_default()
}

/// Integral floats as integers, recursively, as JavaScript would print them.
pub fn js_numbers(v: &Value) -> Value {
    match v {
        Value::Number(n) if n.is_f64() => benchbob_core::http::num(n.as_f64().unwrap_or(f64::NAN)),
        Value::Array(a) => Value::Array(a.iter().map(js_numbers).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), js_numbers(x))).collect()),
        x => x.clone(),
    }
}

/// `new Date(ms).toUTCString()`: "Tue, 20 Oct 2026 10:00:00 GMT".
pub fn http_date(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil(days);
    const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MO: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        WD[days.rem_euclid(7) as usize],
        d,
        MO[(m - 1) as usize],
        y,
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// `new Date(ms).toISOString().slice(0, 10)`.
pub fn iso_date(ms: i64) -> String {
    let (y, m, d) = civil(ms.div_euclid(86_400_000));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to (year, month, day) (Howard Hinnant's civil_from_days).
fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

// ---------- localeCompare ----------

/// Where ICU's root collation puts ASCII punctuation and symbols (all before digits,
/// which come before letters).
const PUNCT_ORDER: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

/// Primary weight of one character: (class, key). Letters fold case and common Latin
/// accents, as `sensitivity: "base"` does.
fn primary(c: char) -> (u8, u32) {
    if c.is_whitespace() {
        return (0, c as u32);
    }
    if let Some(i) = PUNCT_ORDER.find(c) {
        return (1, i as u32);
    }
    if c.is_ascii_digit() {
        return (2, c as u32);
    }
    let b = fold(c);
    if b.is_alphabetic() {
        return (3, b.to_lowercase().next().unwrap_or(b) as u32);
    }
    if !c.is_alphanumeric() {
        return (1, 1000 + c as u32);
    }
    (4, c as u32)
}

/// Strip the accent from a Latin-1 / Latin Extended-A letter.
fn fold(c: char) -> char {
    const FROM: &str = "ÀÁÂÃÄÅàáâãäåÇçÈÉÊËèéêëÌÍÎÏìíîïÑñÒÓÔÕÖØòóôõöøÙÚÛÜùúûüÝýÿŠšŽžČčĆćĐđŁłŃńŚśŹźŻżĘęĄą";
    const TO: &str = "AAAAAAaaaaaaCcEEEEeeeeIIIIiiiiNnOOOOOOooooooUUUUuuuuYyySsZzCcCcDdLlNnSsZzZzEeAa";
    match FROM.chars().position(|x| x == c) {
        Some(i) => TO.chars().nth(i).unwrap_or(c),
        None => c,
    }
}

/// An approximation of `a.localeCompare(b, undefined, { numeric, sensitivity })` with
/// ICU's root collation, good for file names: whitespace < punctuation < digits <
/// letters, case- and accent-insensitive first; with `numeric`, digit runs compare by
/// value. `base` false (the default sensitivity) then breaks ties: accents, then
/// lowercase before uppercase.
pub fn locale_cmp(a: &str, b: &str, numeric: bool, base: bool) -> Ordering {
    let (x, y): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (mut i, mut j) = (0, 0);
    while i < x.len() && j < y.len() {
        if numeric && x[i].is_ascii_digit() && y[j].is_ascii_digit() {
            let si = i;
            while i < x.len() && x[i].is_ascii_digit() {
                i += 1;
            }
            let sj = j;
            while j < y.len() && y[j].is_ascii_digit() {
                j += 1;
            }
            let na: String = x[si..i].iter().collect::<String>().trim_start_matches('0').to_string();
            let nb: String = y[sj..j].iter().collect::<String>().trim_start_matches('0').to_string();
            let o = na.len().cmp(&nb.len()).then_with(|| na.cmp(&nb));
            if o != Ordering::Equal {
                return o;
            }
            continue;
        }
        let o = primary(x[i]).cmp(&primary(y[j]));
        if o != Ordering::Equal {
            return o;
        }
        i += 1;
        j += 1;
    }
    let o = (x.len() - i).cmp(&(y.len() - j));
    if o != Ordering::Equal || base {
        return o;
    }
    // Secondary (accents), then tertiary (case: lower first).
    for (p, q) in x.iter().zip(y.iter()) {
        let (fp, fq) = (fold(*p) != *p, fold(*q) != *q);
        if fp != fq {
            return fp.cmp(&fq);
        }
    }
    for (p, q) in x.iter().zip(y.iter()) {
        let (up, uq) = (p.is_uppercase(), q.is_uppercase());
        if up != uq {
            return up.cmp(&uq);
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_fixed_rounds_half_up_like_javascript() {
        assert_eq!(to_fixed(1.25, 1), "1.3");
        assert_eq!(to_fixed(1.05, 1), "1.1"); // 1.05 is 1.0500000000000000444…
        assert_eq!(to_fixed(1.45, 1), "1.4"); // 1.4499999999999999556…
        assert_eq!(to_fixed(9.96, 1), "10.0");
        assert_eq!(to_fixed(512.0, 0), "512");
        assert_eq!(to_fixed(0.5, 0), "1");
    }

    #[test]
    fn fmt_bytes_matches_server_ts() {
        assert_eq!(fmt_bytes(0.0), "0 B");
        assert_eq!(fmt_bytes(512.0), "512 B");
        assert_eq!(fmt_bytes(1280.0), "1.3 KB");
        assert_eq!(fmt_bytes(4096.0 * 250.0), "1000.0 KB");
        assert_eq!(fmt_bytes(15978201088.0), "14.9 GB");
        assert_eq!(fmt_bytes(5e15), "4547.5 TB");
    }

    #[test]
    fn fmt_bytes_matches_the_ts_fixture() {
        let v: Value = serde_json::from_str(include_str!("../fixtures/fmtbytes.expected.json")).unwrap();
        for e in v.as_array().unwrap() {
            assert_eq!(fmt_bytes(e[0].as_f64().unwrap()), e[1].as_str().unwrap(), "{}", e[0]);
        }
    }

    #[test]
    fn dates() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_792_490_400_000), "Tue, 20 Oct 2026 10:00:00 GMT");
        assert_eq!(iso_date(1_792_490_400_000), "2026-10-20");
    }

    #[test]
    fn uri_component() {
        assert_eq!(encode_uri_component("a b/ü(1).txt"), "a%20b%2F%C3%BC(1).txt");
    }

    #[test]
    fn locale_order() {
        let mut v = vec!["file10", "File2", "file1", "_x", "a", "B", "émile", "zed", "10", "9"];
        v.sort_by(|a, b| locale_cmp(a, b, true, true));
        assert_eq!(v, ["_x", "9", "10", "a", "B", "émile", "file1", "File2", "file10", "zed"]);
        assert_eq!(locale_cmp("a", "A", false, false), Ordering::Less);
        assert_eq!(locale_cmp("a", "A", false, true), Ordering::Equal);
        assert_eq!(locale_cmp("Microsoft", "boot", false, false), Ordering::Greater);
    }
}
