//! What a driver's route handler gets and gives back, plus the JSON helpers that keep the
//! API's output identical to the Bun server's.

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;

use crate::Json;

/// An error with an HTTP status, answered as `{ ok: false, message }` (core.ts `HttpError`).
#[derive(Debug, Clone)]
pub struct HttpError {
    pub status: u16,
    pub message: String,
}

impl HttpError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }
    pub fn into_response(self) -> Response {
        if self.status == 500 {
            tracing::error!("{}", self.message);
        }
        json_response(self.status, &serde_json::json!({ "ok": false, "message": self.message }))
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HttpError {}

impl From<anyhow::Error> for HttpError {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast::<HttpError>() {
            Ok(h) => h,
            Err(e) => Self::new(500, format!("{e:#}")),
        }
    }
}

/// A request as a driver route sees it.
pub struct Request {
    pub method: Method,
    pub path: String,
    /// `:name` segments of the route's path.
    pub params: HashMap<String, String>,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Request {
    /// The request body as JSON; `{}` for an empty body (core.ts `body()`).
    pub fn json(&self) -> Result<Json, HttpError> {
        if self.body.is_empty() {
            return Ok(Json::Object(Default::default()));
        }
        serde_json::from_slice(&self.body).map_err(|_| HttpError::bad_request("Body is not valid JSON"))
    }
    pub fn query(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    pub fn param(&self, name: &str) -> &str {
        self.params.get(name).map(String::as_str).unwrap_or("")
    }
}

/// What a route handler returns: JSON (the common case) or a full response.
pub enum Reply {
    Json(Json),
    Response(Response),
}

impl From<Json> for Reply {
    fn from(v: Json) -> Self {
        Reply::Json(v)
    }
}

pub type RouteResult = Result<Reply, HttpError>;

/// `Ok(Reply::Json(v))`, for handlers.
pub fn json(v: Json) -> RouteResult {
    Ok(Reply::Json(v))
}

/// Bun's `Response.json()`: compact JSON, `application/json;charset=utf-8`.
pub fn json_response(status: u16, v: &Json) -> Response {
    let body = serde_json::to_vec(v).unwrap_or_else(|_| b"null".to_vec());
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json;charset=utf-8"));
    r
}

/// A plain-text response (`new Response("Not Found", { status: 404 })`).
pub fn text_response(status: u16, text: impl Into<String>) -> Response {
    let mut r = Response::new(Body::from(text.into()));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain;charset=utf-8"));
    r
}

/// A JavaScript number as JSON: integral values as integers, so 51.0 is written `51` the
/// way `JSON.stringify` writes it (serde_json would write `51.0`).
pub fn num(f: f64) -> Json {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
        Json::from(f as i64)
    } else if f.is_finite() {
        Json::from(f)
    } else {
        Json::Null
    }
}

/// `Math.round(x * 10^d) / 10^d` as JavaScript computes it (round half up), as JSON.
pub fn round_to(x: f64, decimals: i32) -> f64 {
    let m = 10f64.powi(decimals);
    (x * m + 0.5).floor() / m
}

/// Match a route pattern ("/api/serial/:id/config") against a path. `:name` segments
/// become params, percent-decoded.
pub fn match_path(pattern: &str, path: &str) -> Option<HashMap<String, String>> {
    let a: Vec<&str> = pattern.split('/').collect();
    let b: Vec<&str> = path.split('/').collect();
    if a.len() != b.len() {
        return None;
    }
    let mut params = HashMap::new();
    for (p, s) in a.iter().zip(b.iter()) {
        if let Some(name) = p.strip_prefix(':') {
            params.insert(name.to_string(), percent_decode(s));
        } else if p != s {
            return None;
        }
    }
    Some(params)
}

/// `decodeURIComponent`, leniently: invalid escapes are kept as they are.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Query string pairs, `+` as space, percent-decoded (URLSearchParams).
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(&k.replace('+', " ")), percent_decode(&v.replace('+', " ")))
        })
        .collect()
}

/// A JSON value's truthiness as JavaScript sees it (`!!v`).
pub fn truthy(v: &Json) -> bool {
    match v {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Json::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// JavaScript's `Number(v)` for a JSON value (NaN when it isn't numeric).
pub fn js_number(v: &Json) -> f64 {
    match v {
        Json::Null => 0.0,
        Json::Bool(b) => *b as i32 as f64,
        Json::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Json::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                i64::from_str_radix(h, 16).map(|n| n as f64).unwrap_or(f64::NAN)
            } else if t.bytes().all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-')) {
                t.parse::<f64>().unwrap_or(f64::NAN)
            } else {
                f64::NAN
            }
        }
        Json::Array(a) if a.is_empty() => 0.0,
        Json::Array(a) if a.len() == 1 => js_number(&a[0]),
        _ => f64::NAN,
    }
}

/// JavaScript's `String(v)` for a JSON value.
pub fn js_string(v: &Json) -> String {
    match v {
        Json::Null => "null".into(),
        Json::String(s) => s.clone(),
        Json::Bool(b) => b.to_string(),
        Json::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e21 => format!("{}", f as i64),
            _ => n.to_string(),
        },
        Json::Array(a) => a.iter().map(|x| if x.is_null() { String::new() } else { js_string(x) }).collect::<Vec<_>>().join(","),
        Json::Object(_) => "[object Object]".into(),
    }
}

/// `parseInt(s, 10)`: leading whitespace, an optional sign, then digits; None for NaN.
pub fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// `parseFloat(s)`: the longest numeric prefix; None for NaN.
pub fn parse_float(s: &str) -> Option<f64> {
    let t = s.trim_start();
    let mut end = 0;
    let b = t.as_bytes();
    let mut seen_digit = false;
    let mut seen_dot = false;
    let mut seen_e = false;
    while end < b.len() {
        let c = b[end];
        if c.is_ascii_digit() {
            seen_digit = true;
        } else if c == b'.' && !seen_dot && !seen_e {
            seen_dot = true;
        } else if (c == b'e' || c == b'E') && seen_digit && !seen_e {
            // Only an exponent if digits follow.
            let mut k = end + 1;
            if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
                k += 1;
            }
            if k < b.len() && b[k].is_ascii_digit() {
                seen_e = true;
                end = k;
                continue;
            }
            break;
        } else if (c == b'-' || c == b'+') && end == 0 {
        } else {
            break;
        }
        end += 1;
    }
    if !seen_digit {
        return None;
    }
    t[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_match_with_params() {
        let p = match_path("/api/serial/:id/config", "/api/serial/ttyUSB0/config").unwrap();
        assert_eq!(p["id"], "ttyUSB0");
        assert!(match_path("/api/gpio/:gpio", "/api/gpio/reboot/x").is_none());
        assert_eq!(match_path("/api/x/:a", "/api/x/a%20b").unwrap()["a"], "a b");
    }

    #[test]
    fn numbers_print_like_javascript() {
        assert_eq!(serde_json::to_string(&num(51.0)).unwrap(), "51");
        assert_eq!(serde_json::to_string(&num(51.6)).unwrap(), "51.6");
        assert_eq!(round_to(44.79, 1), 44.8);
        assert_eq!(parse_int("400000abc"), Some(400000));
        assert_eq!(parse_int(" -7"), Some(-7));
        assert_eq!(parse_int("x"), None);
        assert_eq!(parse_float("12.5 0.3"), Some(12.5));
        assert_eq!(parse_float("1e3x"), Some(1000.0));
        assert_eq!(js_number(&Json::from("17")), 17.0);
        assert!(js_number(&Json::from("abc")).is_nan());
        assert_eq!(js_string(&Json::from(17)), "17");
    }

    #[test]
    fn query_strings() {
        assert_eq!(parse_query("path=a%2Fb&x=1+2"), vec![("path".into(), "a/b".into()), ("x".into(), "1 2".into())]);
    }
}
