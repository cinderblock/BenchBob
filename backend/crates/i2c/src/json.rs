//! Chip results as the Bun server's JSON (same keys, same order), and request argument
//! parsing with JavaScript's coercions (`parseInt`, `String(x)`).

use benchbob_core::Json;
use benchbob_core::http::{HttpError, js_string, num};
use benchbob_i2c_chips::num::js_num_str;
use benchbob_i2c_chips::{Opt, Readout, V, Val};
use serde_json::{Map, json};

pub fn v_json(v: &V) -> Json {
    match v {
        V::Num(n) => num(*n),
        V::Str(s) => Json::String(s.clone()),
        V::Bool(b) => Json::Bool(*b),
        V::Null => Json::Null,
    }
}

/// `String(v)` of a reading's value.
pub fn v_string(v: &V) -> String {
    match v {
        V::Num(n) => js_num_str(*n),
        V::Str(s) => s.clone(),
        V::Bool(b) => b.to_string(),
        V::Null => "null".into(),
    }
}

pub fn val_json(v: &Val) -> Json {
    let mut m = Map::new();
    m.insert("k".into(), json!(v.k));
    m.insert("label".into(), json!(v.label));
    m.insert("v".into(), v_json(&v.v));
    if let Some(u) = v.unit {
        m.insert("unit".into(), json!(u));
    }
    if let Some(d) = v.d {
        m.insert("d".into(), json!(d));
    }
    Json::Object(m)
}

pub fn readout_json(r: &Readout) -> Json {
    let mut m = Map::new();
    m.insert("values".into(), Json::Array(r.values.iter().map(val_json).collect()));
    if let Some(pins) = &r.pins {
        let list = pins
            .iter()
            .map(|p| {
                let mut o = Map::new();
                o.insert("name".into(), json!(p.name));
                o.insert("level".into(), json!(p.level));
                if let Some(d) = p.dir {
                    o.insert("dir".into(), json!(d));
                }
                if let Some(pull) = p.pull {
                    o.insert("pull".into(), json!(pull));
                }
                Json::Object(o)
            })
            .collect();
        m.insert("pins".into(), Json::Array(list));
    }
    if let Some(ch) = &r.channels {
        let list = ch
            .iter()
            .map(|c| {
                let mut o = Map::new();
                o.insert("ch".into(), json!(c.ch));
                o.insert("duty".into(), num(c.duty));
                o.insert("on".into(), json!(c.on));
                o.insert("off".into(), json!(c.off));
                if let Some(f) = c.full {
                    o.insert("full".into(), json!(f));
                }
                Json::Object(o)
            })
            .collect();
        m.insert("channels".into(), Json::Array(list));
    }
    if let Some(h) = &r.hex {
        m.insert("hex".into(), json!({ "start": h.start, "bytes": h.bytes }));
    }
    if let Some(s) = r.stale {
        m.insert("stale".into(), json!(s));
    }
    if let Some(n) = &r.note {
        m.insert("note".into(), json!(n));
    }
    Json::Object(m)
}

/// An option row of the device view: the definition plus its current `value`.
pub fn opt_json(o: &Opt, value: Json) -> Json {
    let mut m = Map::new();
    m.insert("key".into(), json!(o.key));
    m.insert("label".into(), json!(o.label));
    m.insert("type".into(), json!(o.kind));
    m.insert("default".into(), num(o.default));
    if !o.choices.is_empty() {
        m.insert("choices".into(), Json::Array(o.choices.iter().map(|(v, l)| json!({ "v": num(*v), "label": l })).collect()));
    }
    if let Some(u) = o.unit {
        m.insert("unit".into(), json!(u));
    }
    m.insert("value".into(), value);
    Json::Object(m)
}

// ---------- arguments ----------

/// `parseInt(s, radix)` for radix 10 or 16 (a "0x" prefix is allowed with 16); None for NaN.
pub fn parse_int_radix(s: &str, radix: u32) -> Option<i64> {
    let t = s.trim_start();
    let (neg, mut rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if radix == 16
        && let Some(r) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X"))
    {
        rest = r;
    }
    let digits: String = rest.chars().take_while(|c| c.is_digit(radix)).collect();
    if digits.is_empty() {
        return None;
    }
    let n = i64::from_str_radix(&digits, radix).ok()?;
    Some(if neg { -n } else { n })
}

/// `String(x)`, with `undefined` for a missing value.
pub fn arg_string(v: Option<&Json>) -> String {
    v.map(js_string).unwrap_or_else(|| "undefined".into())
}

fn starts_0x(s: &str) -> bool {
    s.len() >= 2 && s[..2].eq_ignore_ascii_case("0x")
}

/// A number as given, or `parseInt(String(v), /^0x/i ? 16 : 10)`. NaN when it isn't one.
pub fn js_int(v: Option<&Json>) -> f64 {
    match v {
        Some(Json::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        other => {
            let s = arg_string(other);
            parse_int_radix(&s, if starts_0x(&s) { 16 } else { 10 }).map_or(f64::NAN, |n| n as f64)
        }
    }
}

fn is_int(n: f64) -> bool {
    n.is_finite() && n.fract() == 0.0
}

/// i2c.ts `intArg`.
pub fn int_arg(v: Option<&Json>, what: &str, lo: i64, hi: i64) -> Result<i64, HttpError> {
    let n = js_int(v);
    if !is_int(n) || n < lo as f64 || n > hi as f64 {
        return Err(HttpError::bad_request(format!("{what} must be {lo}–{hi}")));
    }
    Ok(n as i64)
}

/// i2c.ts `parseAddr`: 7-bit, 0x03–0x77.
pub fn parse_addr(v: Option<&Json>) -> Result<u8, HttpError> {
    let n = js_int(v);
    if !is_int(n) || !(3.0..=119.0).contains(&n) {
        return Err(HttpError::bad_request(format!("Address must be 0x03–0x77 (got {})", arg_string(v))));
    }
    Ok(n as u8)
}

/// i2c.ts `byteArr`: an array of bytes or a string like "0x00 0x1f".
pub fn byte_arr(v: Option<&Json>, what: &str) -> Result<Vec<u8>, HttpError> {
    let items: Vec<Json> = match v {
        Some(Json::Array(a)) => a.clone(),
        Some(Json::String(s)) => {
            s.split(|c: char| c.is_whitespace() || c == ',').filter(|x| !x.is_empty()).map(|x| Json::String(x.into())).collect()
        }
        _ => return Err(HttpError::bad_request(format!("{what} must be an array of bytes or a string like \"0x00 0x1f\""))),
    };
    items
        .iter()
        .map(|x| {
            let n = js_int(Some(x));
            if !is_int(n) || !(0.0..=255.0).contains(&n) {
                return Err(HttpError::bad_request(format!("{what}: {} is not a byte", js_string(x))));
            }
            Ok(n as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_coerce_like_javascript() {
        assert_eq!(parse_addr(Some(&json!("0x76"))).unwrap(), 0x76);
        assert_eq!(parse_addr(Some(&json!(118))).unwrap(), 0x76);
        assert_eq!(parse_addr(Some(&json!("118abc"))).unwrap(), 118);
        assert_eq!(parse_addr(Some(&json!("0x78"))).unwrap_err().message, "Address must be 0x03–0x77 (got 0x78)");
        assert_eq!(parse_addr(None).unwrap_err().message, "Address must be 0x03–0x77 (got undefined)");
        assert_eq!(int_arg(Some(&json!("0xD0")), "register", 0, 255).unwrap(), 0xd0);
        assert_eq!(int_arg(Some(&json!(1.5)), "register", 0, 255).unwrap_err().message, "register must be 0–255");
        assert_eq!(byte_arr(Some(&json!("0x00, 0x1f 7")), "bytes").unwrap(), vec![0, 0x1f, 7]);
        assert_eq!(byte_arr(Some(&json!([1, "0x100"])), "bytes").unwrap_err().message, "bytes: 0x100 is not a byte");
        assert!(byte_arr(Some(&json!(5)), "w").unwrap_err().message.starts_with("w must be an array"));
    }

    #[test]
    fn readouts_serialize_in_the_typescript_key_order() {
        let r = Readout {
            values: vec![Val::unit("t", "Temperature", 25.0, "°C", 2), Val { d: Some(1), ..Val::new("lux", "Illuminance", "saturated") }],
            stale: Some(false),
            note: None,
            ..Default::default()
        };
        assert_eq!(
            readout_json(&r).to_string(),
            r#"{"values":[{"k":"t","label":"Temperature","v":25,"unit":"°C","d":2},{"k":"lux","label":"Illuminance","v":"saturated","d":1}],"stale":false}"#
        );
    }
}
