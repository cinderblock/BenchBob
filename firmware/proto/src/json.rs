//! A small zero-allocation JSON reader and writer.
//!
//! The reader validates a document once ([`parse`]) and then hands out [`Value`]s that
//! borrow slices of the input. That is enough for JSON-RPC: the request `id` can be echoed
//! byte-for-byte whatever its type, and tool arguments of any shape can be looked up by
//! key without declaring structs for them. The writer appends to a fixed buffer and
//! handles commas and string escaping.

use core::fmt;

/// Maximum nesting depth accepted by [`parse`] (also bounds the recursion).
pub const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Syntax(usize),
    TooDeep,
    Trailing(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}

/// One JSON value inside a validated document. `raw` is its exact source text.
#[derive(Debug, Clone, Copy)]
pub struct Value<'a> {
    raw: &'a str,
    kind: Kind,
}

/// Validate `s` as exactly one JSON value (whitespace around it allowed).
pub fn parse(s: &str) -> Result<Value<'_>, Error> {
    let b = s.as_bytes();
    let start = skip_ws(b, 0);
    let (kind, end) = value_end(b, start, 0)?;
    let rest = skip_ws(b, end);
    if rest != b.len() {
        return Err(Error::Trailing(rest));
    }
    Ok(Value { raw: &s[start..end], kind })
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

fn expect_lit(b: &[u8], i: usize, lit: &[u8]) -> Result<usize, Error> {
    if b.len() >= i + lit.len() && &b[i..i + lit.len()] == lit {
        Ok(i + lit.len())
    } else {
        Err(Error::Syntax(i))
    }
}

fn string_end(b: &[u8], mut i: usize) -> Result<usize, Error> {
    // b[i] == '"'
    i += 1;
    while i < b.len() {
        match b[i] {
            b'"' => return Ok(i + 1),
            b'\\' => {
                let Some(&e) = b.get(i + 1) else { return Err(Error::Syntax(i)) };
                match e {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => i += 2,
                    b'u' => {
                        if b.len() < i + 6 || !b[i + 2..i + 6].iter().all(|c| c.is_ascii_hexdigit()) {
                            return Err(Error::Syntax(i));
                        }
                        i += 6;
                    }
                    _ => return Err(Error::Syntax(i)),
                }
            }
            c if c < 0x20 => return Err(Error::Syntax(i)),
            _ => i += 1,
        }
    }
    Err(Error::Syntax(i))
}

fn number_end(b: &[u8], mut i: usize) -> Result<usize, Error> {
    let start = i;
    if b.get(i) == Some(&b'-') {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == int_start || (b[int_start] == b'0' && i - int_start > 1) {
        return Err(Error::Syntax(start));
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        let f = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == f {
            return Err(Error::Syntax(i));
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let e = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == e {
            return Err(Error::Syntax(i));
        }
    }
    Ok(i)
}

fn value_end(b: &[u8], i: usize, depth: usize) -> Result<(Kind, usize), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::TooDeep);
    }
    match b.get(i) {
        Some(b'n') => Ok((Kind::Null, expect_lit(b, i, b"null")?)),
        Some(b't') => Ok((Kind::Bool, expect_lit(b, i, b"true")?)),
        Some(b'f') => Ok((Kind::Bool, expect_lit(b, i, b"false")?)),
        Some(b'"') => Ok((Kind::String, string_end(b, i)?)),
        Some(b'-' | b'0'..=b'9') => Ok((Kind::Number, number_end(b, i)?)),
        Some(b'[') => {
            let mut j = skip_ws(b, i + 1);
            if b.get(j) == Some(&b']') {
                return Ok((Kind::Array, j + 1));
            }
            loop {
                let (_, e) = value_end(b, j, depth + 1)?;
                j = skip_ws(b, e);
                match b.get(j) {
                    Some(b',') => j = skip_ws(b, j + 1),
                    Some(b']') => return Ok((Kind::Array, j + 1)),
                    _ => return Err(Error::Syntax(j)),
                }
            }
        }
        Some(b'{') => {
            let mut j = skip_ws(b, i + 1);
            if b.get(j) == Some(&b'}') {
                return Ok((Kind::Object, j + 1));
            }
            loop {
                if b.get(j) != Some(&b'"') {
                    return Err(Error::Syntax(j));
                }
                j = skip_ws(b, string_end(b, j)?);
                if b.get(j) != Some(&b':') {
                    return Err(Error::Syntax(j));
                }
                let (_, e) = value_end(b, skip_ws(b, j + 1), depth + 1)?;
                j = skip_ws(b, e);
                match b.get(j) {
                    Some(b',') => j = skip_ws(b, j + 1),
                    Some(b'}') => return Ok((Kind::Object, j + 1)),
                    _ => return Err(Error::Syntax(j)),
                }
            }
        }
        _ => Err(Error::Syntax(i)),
    }
}

/// Kind of a value already known to be valid, from its first byte.
fn kind_of(raw: &str) -> Kind {
    match raw.as_bytes().first() {
        Some(b'n') => Kind::Null,
        Some(b't' | b'f') => Kind::Bool,
        Some(b'"') => Kind::String,
        Some(b'[') => Kind::Array,
        Some(b'{') => Kind::Object,
        _ => Kind::Number,
    }
}

/// Decodes the characters of a validated JSON string body (without the quotes).
struct Chars<'a> {
    b: &'a [u8],
    i: usize,
}

impl Iterator for Chars<'_> {
    type Item = char;
    fn next(&mut self) -> Option<char> {
        let b = self.b;
        if self.i >= b.len() {
            return None;
        }
        if b[self.i] != b'\\' {
            // Valid UTF-8 (the input is &str): decode one scalar.
            let s = unsafe { core::str::from_utf8_unchecked(&b[self.i..]) };
            let c = s.chars().next()?;
            self.i += c.len_utf8();
            return Some(c);
        }
        let e = b[self.i + 1];
        self.i += 2;
        Some(match e {
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let hi = hex4(&b[self.i..self.i + 4]);
                self.i += 4;
                if (0xD800..0xDC00).contains(&hi) && b.get(self.i) == Some(&b'\\') && b.get(self.i + 1) == Some(&b'u') {
                    let lo = hex4(&b[self.i + 2..self.i + 6]);
                    if (0xDC00..0xE000).contains(&lo) {
                        self.i += 6;
                        let c = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                        return Some(char::from_u32(c).unwrap_or('\u{FFFD}'));
                    }
                }
                char::from_u32(hi).unwrap_or('\u{FFFD}')
            }
            other => other as char, // " \ /
        })
    }
}

fn hex4(h: &[u8]) -> u32 {
    h.iter().fold(0, |acc, &c| acc * 16 + (c as char).to_digit(16).unwrap_or(0))
}

impl<'a> Value<'a> {
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The exact source text of this value.
    pub fn raw(&self) -> &'a str {
        self.raw
    }

    pub fn is_null(&self) -> bool {
        self.kind == Kind::Null
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self.raw {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }

    /// An integer, accepting a fraction only when it is zero (`3.0`).
    pub fn as_i64(&self) -> Option<i64> {
        if self.kind != Kind::Number || self.raw.contains(['e', 'E']) {
            return None;
        }
        let (int, frac) = match self.raw.split_once('.') {
            Some((i, f)) => (i, f),
            None => (self.raw, ""),
        };
        if !frac.bytes().all(|c| c == b'0') {
            return None;
        }
        int.parse().ok()
    }

    fn string_body(&self) -> Option<&'a str> {
        (self.kind == Kind::String).then(|| &self.raw[1..self.raw.len() - 1])
    }

    /// The string, if it has no escapes (borrowed straight from the input).
    pub fn as_plain_str(&self) -> Option<&'a str> {
        self.string_body().filter(|s| !s.contains('\\'))
    }

    /// The decoded string, written into `buf`. None when not a string or `buf` is too small.
    pub fn as_str_in<'b>(&self, buf: &'b mut [u8]) -> Option<&'b str> {
        let body = self.string_body()?;
        let mut n = 0;
        for c in (Chars { b: body.as_bytes(), i: 0 }) {
            let l = c.len_utf8();
            if n + l > buf.len() {
                return None;
            }
            c.encode_utf8(&mut buf[n..n + l]);
            n += l;
        }
        core::str::from_utf8(&buf[..n]).ok()
    }

    /// Does this string value decode to exactly `s`?
    pub fn str_eq(&self, s: &str) -> bool {
        match self.string_body() {
            Some(body) => (Chars { b: body.as_bytes(), i: 0 }).eq(s.chars()),
            None => false,
        }
    }

    /// Elements of an array (empty for anything else).
    pub fn items(&self) -> Items<'a> {
        let inner = if self.kind == Kind::Array { &self.raw[1..self.raw.len() - 1] } else { "" };
        Items { s: inner, i: 0 }
    }

    /// Members of an object as (key, value); the key is a String value.
    pub fn members(&self) -> Members<'a> {
        let inner = if self.kind == Kind::Object { &self.raw[1..self.raw.len() - 1] } else { "" };
        Members { s: inner, i: 0 }
    }

    /// Look up a member of an object by (decoded) key. The first match wins.
    pub fn get(&self, key: &str) -> Option<Value<'a>> {
        self.members().find(|(k, _)| k.str_eq(key)).map(|(_, v)| v)
    }
}

/// Splits off the next value of a validated, comma-separated list.
fn next_in<'a>(s: &'a str, i: &mut usize) -> Option<Value<'a>> {
    let b = s.as_bytes();
    let mut j = skip_ws(b, *i);
    if j < b.len() && b[j] == b',' {
        j = skip_ws(b, j + 1);
    }
    if j >= b.len() {
        return None;
    }
    let (kind, end) = value_end(b, j, 0).ok()?;
    *i = end;
    Some(Value { raw: &s[j..end], kind })
}

pub struct Items<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Iterator for Items<'a> {
    type Item = Value<'a>;
    fn next(&mut self) -> Option<Value<'a>> {
        next_in(self.s, &mut self.i)
    }
}

pub struct Members<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Iterator for Members<'a> {
    type Item = (Value<'a>, Value<'a>);
    fn next(&mut self) -> Option<Self::Item> {
        let key = next_in(self.s, &mut self.i)?;
        let b = self.s.as_bytes();
        let colon = skip_ws(b, self.i);
        self.i = colon + 1;
        let j = skip_ws(b, self.i);
        let (_, end) = value_end(b, j, 0).ok()?;
        self.i = end;
        let raw = &self.s[j..end];
        Some((key, Value { raw, kind: kind_of(raw) }))
    }
}

// ---------------------------------------------------------------------------------------
// Writer

/// A fixed-capacity byte buffer. Writes past the end set `overflow` and are dropped.
pub struct Buf<'a> {
    data: &'a mut [u8],
    len: usize,
    overflow: bool,
}

impl<'a> Buf<'a> {
    pub fn new(data: &'a mut [u8]) -> Self {
        Buf { data, len: 0, overflow: false }
    }
    pub fn push(&mut self, b: &[u8]) {
        if self.overflow || self.len + b.len() > self.data.len() {
            self.overflow = true;
            return;
        }
        self.data[self.len..self.len + b.len()].copy_from_slice(b);
        self.len += b.len();
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn overflowed(&self) -> bool {
        self.overflow
    }
    pub fn clear(&mut self) {
        self.len = 0;
        self.overflow = false;
    }
    /// Shrink back to `len` bytes (used to roll back a partly written document).
    pub fn truncate(&mut self, len: usize) {
        if len < self.len {
            self.len = len;
        }
        self.overflow = false;
    }
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
    }
}

impl fmt::Write for Buf<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push(s.as_bytes());
        if self.overflow { Err(fmt::Error) } else { Ok(()) }
    }
}

/// Escapes everything written through it as JSON string content.
struct Escape<'b, 'a>(&'b mut Buf<'a>);

impl fmt::Write for Escape<'_, '_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        escape_into(self.0, s);
        if self.0.overflow { Err(fmt::Error) } else { Ok(()) }
    }
}

fn escape_into(buf: &mut Buf<'_>, s: &str) {
    let b = s.as_bytes();
    let mut start = 0;
    for (i, &c) in b.iter().enumerate() {
        let esc: Option<&[u8]> = match c {
            b'"' => Some(b"\\\""),
            b'\\' => Some(b"\\\\"),
            b'\n' => Some(b"\\n"),
            b'\r' => Some(b"\\r"),
            b'\t' => Some(b"\\t"),
            c if c < 0x20 => None,
            _ => continue,
        };
        buf.push(&b[start..i]);
        match esc {
            Some(e) => buf.push(e),
            None => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                buf.push(&[b'\\', b'u', b'0', b'0', HEX[(c >> 4) as usize], HEX[(c & 15) as usize]]);
            }
        }
        start = i + 1;
    }
    buf.push(&b[start..]);
}

/// Streaming JSON writer. Call `key` before each value inside an object.
pub struct Json<'b, 'a> {
    pub buf: &'b mut Buf<'a>,
    /// Bit n set: the container at depth n already has an element (so the next needs a comma).
    has: u64,
    depth: u32,
    after_key: bool,
}

impl<'b, 'a> Json<'b, 'a> {
    pub fn new(buf: &'b mut Buf<'a>) -> Self {
        Json { buf, has: 0, depth: 0, after_key: false }
    }

    fn sep(&mut self) {
        if self.after_key {
            self.after_key = false;
            return;
        }
        if self.depth > 0 {
            let bit = 1u64 << (self.depth - 1);
            if self.has & bit != 0 {
                self.buf.push(b",");
            }
            self.has |= bit;
        }
    }

    fn open(&mut self, c: u8) -> &mut Self {
        self.sep();
        self.buf.push(&[c]);
        self.depth += 1;
        self.has &= !(1u64 << (self.depth - 1));
        self
    }

    fn close(&mut self, c: u8) -> &mut Self {
        self.buf.push(&[c]);
        self.depth = self.depth.saturating_sub(1);
        self
    }

    pub fn obj(&mut self) -> &mut Self {
        self.open(b'{')
    }
    pub fn end_obj(&mut self) -> &mut Self {
        self.close(b'}')
    }
    pub fn arr(&mut self) -> &mut Self {
        self.open(b'[')
    }
    pub fn end_arr(&mut self) -> &mut Self {
        self.close(b']')
    }

    pub fn key(&mut self, k: &str) -> &mut Self {
        self.sep();
        self.buf.push(b"\"");
        escape_into(self.buf, k);
        self.buf.push(b"\":");
        self.after_key = true;
        self
    }

    pub fn str(&mut self, s: &str) -> &mut Self {
        self.sep();
        self.buf.push(b"\"");
        escape_into(self.buf, s);
        self.buf.push(b"\"");
        self
    }

    /// A string built with `format_args!`, escaped as it is written.
    pub fn str_fmt(&mut self, args: fmt::Arguments<'_>) -> &mut Self {
        self.sep();
        self.buf.push(b"\"");
        let _ = fmt::write(&mut Escape(self.buf), args);
        self.buf.push(b"\"");
        self
    }

    pub fn opt_str(&mut self, s: Option<&str>) -> &mut Self {
        match s {
            Some(s) => self.str(s),
            None => self.null(),
        }
    }

    pub fn u64(&mut self, n: u64) -> &mut Self {
        self.sep();
        let _ = fmt::write(self.buf, format_args!("{n}"));
        self
    }

    pub fn i64(&mut self, n: i64) -> &mut Self {
        self.sep();
        let _ = fmt::write(self.buf, format_args!("{n}"));
        self
    }

    /// A non-negative fixed-point number: `units / 10^decimals`.
    pub fn fixed(&mut self, units: u64, decimals: u32) -> &mut Self {
        self.sep();
        let p = 10u64.pow(decimals);
        let _ = fmt::write(self.buf, format_args!("{}.{:0w$}", units / p, units % p, w = decimals as usize));
        self
    }

    pub fn bool(&mut self, b: bool) -> &mut Self {
        self.sep();
        self.buf.push(if b { b"true" } else { b"false" });
        self
    }

    pub fn null(&mut self) -> &mut Self {
        self.sep();
        self.buf.push(b"null");
        self
    }

    /// Already-encoded JSON, inserted as one value.
    pub fn raw(&mut self, json: &str) -> &mut Self {
        self.sep();
        self.buf.push(json.as_bytes());
        self
    }

    // Shorthands for object members.
    pub fn kv_str(&mut self, k: &str, v: &str) -> &mut Self {
        self.key(k).str(v)
    }
    pub fn kv_u64(&mut self, k: &str, v: u64) -> &mut Self {
        self.key(k).u64(v)
    }
    pub fn kv_bool(&mut self, k: &str, v: bool) -> &mut Self {
        self.key(k).bool(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::string::String;

    #[test]
    fn parses_and_looks_up() {
        let v = parse(r#" {"jsonrpc":"2.0","id":7,"params":{"name":"i2c_scan","arguments":{"bus":"i2c-0","deep":false,"list":[1, 2 ,3]}}} "#).unwrap();
        assert_eq!(v.kind(), Kind::Object);
        assert_eq!(v.get("id").unwrap().raw(), "7");
        let args = v.get("params").unwrap().get("arguments").unwrap();
        assert!(args.get("bus").unwrap().str_eq("i2c-0"));
        assert_eq!(args.get("deep").unwrap().as_bool(), Some(false));
        let l: std::vec::Vec<i64> = args.get("list").unwrap().items().map(|x| x.as_i64().unwrap()).collect();
        assert_eq!(l, [1, 2, 3]);
        assert!(args.get("missing").is_none());
    }

    #[test]
    fn rejects_bad_json() {
        for s in ["", "{", "[1,]", "{\"a\"}", "01", "1.", "\"\\x\"", "{} x", "tru", "\"a\nb\""] {
            assert!(parse(s).is_err(), "{s:?} should fail");
        }
        let deep = "[".repeat(MAX_DEPTH + 2) + &"]".repeat(MAX_DEPTH + 2);
        assert_eq!(parse(&deep).unwrap_err(), Error::TooDeep);
    }

    #[test]
    fn strings_unescape() {
        let v = parse(r#""a\"b\\c\n\u00e9\ud83d\ude00/""#).unwrap();
        let mut buf = [0u8; 32];
        assert_eq!(v.as_str_in(&mut buf).unwrap(), "a\"b\\c\né😀/");
        assert!(v.as_plain_str().is_none());
        assert!(parse(r#""k\u0065y""#).unwrap().str_eq("key"));
        let o = parse(r#"{"k\u0065y": 1}"#).unwrap();
        assert_eq!(o.get("key").unwrap().as_i64(), Some(1));
    }

    #[test]
    fn integers() {
        assert_eq!(parse("-12").unwrap().as_i64(), Some(-12));
        assert_eq!(parse("3.0").unwrap().as_i64(), Some(3));
        assert_eq!(parse("3.5").unwrap().as_i64(), None);
        assert_eq!(parse("1e3").unwrap().as_i64(), None);
        assert_eq!(parse("\"3\"").unwrap().as_i64(), None);
    }

    #[test]
    fn writer_commas_and_escapes() {
        let mut mem = [0u8; 256];
        let mut b = Buf::new(&mut mem);
        let mut j = Json::new(&mut b);
        j.obj().kv_str("a", "x\"y\n\u{1}").key("n").arr().u64(1).i64(-2).fixed(12345, 3).obj().end_obj().arr().end_arr().end_arr();
        j.key("b").bool(true).key("c").null().key("d").str_fmt(format_args!("0x{:02x}", 0x76)).end_obj();
        let s = String::from(b.as_str());
        assert_eq!(s, r#"{"a":"x\"y\n\u0001","n":[1,-2,12.345,{},[]],"b":true,"c":null,"d":"0x76"}"#);
        assert!(parse(&s).is_ok());
    }

    #[test]
    fn writer_overflow_is_flagged() {
        let mut mem = [0u8; 8];
        let mut b = Buf::new(&mut mem);
        Json::new(&mut b).obj().kv_str("long", "value").end_obj();
        assert!(b.overflowed());
    }
}
