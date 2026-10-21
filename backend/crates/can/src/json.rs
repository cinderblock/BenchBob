//! Building JSON objects in the TypeScript's key order, leaving out what it leaves
//! `undefined`, and the small date helpers.

use serde_json::{Map, Value};

/// An object under construction: `.k()` always sets a key, `.o()` only when Some (an
/// `undefined` property is left out of JSON.stringify's output).
pub struct O(pub Map<String, Value>);

impl O {
    pub fn new() -> Self {
        O(Map::new())
    }
    pub fn k(mut self, k: &str, v: impl Into<Value>) -> Self {
        self.0.insert(k.into(), v.into());
        self
    }
    pub fn o<V: Into<Value>>(mut self, k: &str, v: Option<V>) -> Self {
        if let Some(v) = v {
            self.0.insert(k.into(), v.into());
        }
        self
    }
    pub fn n(self, k: &str, v: f64) -> Self {
        self.k(k, num(v))
    }
    pub fn on(self, k: &str, v: Option<f64>) -> Self {
        self.o(k, v.map(num))
    }
    pub fn v(self) -> Value {
        Value::Object(self.0)
    }
}

impl Default for O {
    fn default() -> Self {
        Self::new()
    }
}

pub use benchbob_core::http::num;

/// `{ ...a, ...b }` for objects.
pub fn spread(a: Value, b: Value) -> Value {
    let mut m = match a {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    if let Value::Object(b) = b {
        for (k, v) in b {
            m.insert(k, v);
        }
    }
    Value::Object(m)
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

/// `new Date(ms).toISOString()`
pub fn iso(ms: i64) -> String {
    let (y, mo, d, h, mi, s, ms) = civil(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

/// `new Date(ms).toISOString().replace(/[:]/g, "").replace(/\..*/, "")`: 2026-10-20T123456
pub fn rec_stamp(ms: i64) -> String {
    let (y, mo, d, h, mi, s, _) = civil(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}{mi:02}{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(iso(1792225996360), "2026-10-17T08:33:16.360Z");
        assert_eq!(rec_stamp(1792225996360), "2026-10-17T083316");
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn spread_keeps_first_positions() {
        let v = spread(serde_json::json!({ "a": 1, "b": 2 }), serde_json::json!({ "c": 3, "a": 4 }));
        assert_eq!(serde_json::to_string(&v).unwrap(), r#"{"a":4,"b":2,"c":3}"#);
    }
}
