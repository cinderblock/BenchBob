//! Just enough HTTP/1.1 for the node API: parse a request head, frame a response.
//! Every response closes the connection, so there is no keep-alive or chunking.

use crate::json::Buf;
use core::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadError {
    /// Not a request line / header block we understand.
    BadRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub host: Option<&'a str>,
    pub content_length: usize,
    /// Bytes up to and including the blank line.
    pub len: usize,
}

/// Parse the request head at the start of `buf`. Ok(None): not complete yet.
pub fn parse_head(buf: &[u8]) -> Result<Option<Head<'_>>, HeadError> {
    let Some(end) = find_blank_line(buf) else { return Ok(None) };
    let text = core::str::from_utf8(&buf[..end.0]).map_err(|_| HeadError::BadRequest)?;
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let mut rl = lines.next().ok_or(HeadError::BadRequest)?.split(' ');
    let (Some(method), Some(target), Some(version)) = (rl.next(), rl.next(), rl.next()) else {
        return Err(HeadError::BadRequest);
    };
    if !version.starts_with("HTTP/1.") || method.is_empty() || !target.starts_with('/') {
        return Err(HeadError::BadRequest);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut head = Head { method, path, query, host: None, content_length: 0, len: end.1 };
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':').ok_or(HeadError::BadRequest)?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            head.content_length = value.parse().map_err(|_| HeadError::BadRequest)?;
        } else if name.eq_ignore_ascii_case("host") {
            head.host = Some(value);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            // Chunked uploads aren't supported; clients send Content-Length for JSON.
            return Err(HeadError::BadRequest);
        }
    }
    Ok(Some(head))
}

/// (end of the header text, end of the blank line) for CRLFCRLF or LFLF.
fn find_blank_line(b: &[u8]) -> Option<(usize, usize)> {
    for i in 0..b.len() {
        if b[i..].starts_with(b"\r\n\r\n") {
            return Some((i, i + 4));
        }
        if b[i..].starts_with(b"\n\n") {
            return Some((i, i + 2));
        }
    }
    None
}

/// Look up `key` in a query string (no percent-decoding: values here are plain).
pub fn query_get<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == key).then_some(v)
    })
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

/// Write the status line and headers for a body of `body_len` bytes.
pub fn response_head(status: u16, content_type: &str, body_len: usize, extra: &[(&str, &str)], out: &mut Buf<'_>) {
    let _ = write!(out, "HTTP/1.1 {} {}\r\n", status, reason(status));
    if body_len > 0 || status != 202 {
        let _ = write!(out, "Content-Type: {}\r\n", content_type);
    }
    let _ = write!(out, "Content-Length: {}\r\nConnection: close\r\n", body_len);
    for (k, v) in extra {
        let _ = write!(out, "{}: {}\r\n", k, v);
    }
    out.push(b"\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_post() {
        let req = b"POST /api/gpio/5?x=1 HTTP/1.1\r\nHost: node.local\r\ncontent-length: 12\r\n\r\n{\"mode\":1}..";
        let h = parse_head(req).unwrap().unwrap();
        assert_eq!((h.method, h.path, h.query, h.host, h.content_length), ("POST", "/api/gpio/5", "x=1", Some("node.local"), 12));
        assert_eq!(&req[h.len..h.len + 2], b"{\"");
        assert_eq!(query_get(h.query, "x"), Some("1"));
        assert_eq!(query_get(h.query, "y"), None);
    }

    #[test]
    fn incomplete_and_bad() {
        assert_eq!(parse_head(b"GET / HTTP/1.1\r\nHost: x\r\n"), Ok(None));
        assert_eq!(parse_head(b"GET /\r\n\r\n"), Err(HeadError::BadRequest));
        assert_eq!(parse_head(b"GET / HTTP/1.1\r\nContent-Length: x\r\n\r\n"), Err(HeadError::BadRequest));
        assert_eq!(parse_head(b"GET / HTTP/1.1\nHost: y\n\n").unwrap().unwrap().host, Some("y"));
    }

    #[test]
    fn head_format() {
        let mut mem = [0u8; 256];
        let mut b = Buf::new(&mut mem);
        response_head(404, "application/json", 5, &[("MCP-Protocol-Version", "2025-06-18")], &mut b);
        assert_eq!(
            b.as_str(),
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 5\r\nConnection: close\r\nMCP-Protocol-Version: 2025-06-18\r\n\r\n"
        );
    }
}
