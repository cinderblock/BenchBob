//! Building JSON in the TypeScript's key order, leaving out what it leaves `undefined`, and
//! JavaScript's number formatting.

use serde_json::{Map, Value};

pub use benchbob_core::http::{js_number, js_string, num, truthy};

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

/// A number as a JavaScript template string prints it (`${x}`).
pub fn fmt_num(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    js_string(&num(f))
}

/// `${x}` for an optional number: "undefined" when missing.
pub fn fmt_opt(f: Option<f64>) -> String {
    f.map(fmt_num).unwrap_or_else(|| "undefined".into())
}

/// `${x}` for an optional string: "undefined" when missing.
pub fn fmt_opt_s(s: &Option<String>) -> String {
    s.clone().unwrap_or_else(|| "undefined".into())
}

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

/// A JSON value with every integral float written as an integer, as JSON.stringify does
/// (serde_json keeps `5e7` as the float `50000000.0`).
pub fn js_value(v: &Value) -> Value {
    match v {
        Value::Number(n) if n.is_f64() => num(n.as_f64().unwrap_or(f64::NAN)),
        Value::Array(a) => Value::Array(a.iter().map(js_value).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), js_value(v))).collect()),
        _ => v.clone(),
    }
}

/// `Date.now()`
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `String(v || "")`: "" for a falsy value, else JavaScript's string conversion.
pub fn str_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(v) if truthy(v) => js_string(v),
        _ => String::new(),
    }
}

/// `s.slice(0, n)` counted in UTF-16 units, as JavaScript does (never splitting a char).
pub fn js_slice(s: &str, n: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in s.chars() {
        units += c.len_utf16();
        if units > n {
            break;
        }
        out.push(c);
    }
    out
}

/// `a.slice(0, n)` for a JavaScript number `n` (negative counts from the end).
pub fn slice_n<T: Clone>(a: &[T], n: f64) -> Vec<T> {
    let len = a.len() as f64;
    let end = if n.is_nan() {
        0.0
    } else if n < 0.0 {
        (len + n.trunc()).max(0.0)
    } else {
        n.trunc().min(len)
    };
    a[..end as usize].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_helpers() {
        assert_eq!(fmt_num(-0.0), "0");
        assert_eq!(fmt_num(22.5), "22.5");
        assert_eq!(js_slice("héllo", 2), "hé");
        assert_eq!(slice_n(&[1, 2, 3], -1.0), vec![1, 2]);
        assert_eq!(slice_n(&[1, 2, 3], 2.7), vec![1, 2]);
    }
}
