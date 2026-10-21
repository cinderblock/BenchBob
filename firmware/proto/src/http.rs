//! Just enough HTTP/1.1 for the node API and UI: parse a request head, frame a response.
//! Every response closes the connection, so there is no keep-alive or chunking.
//!
//! Every response carries permissive CORS ([`CORS`]; no credentials, there's no login),
//! as the Linux server's do, so a page or app served from elsewhere (a desktop app
//! holding several bobs) can use the API. [`preflight`] answers `OPTIONS`.

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
    pub accept_encoding: Option<&'a str>,
    pub if_none_match: Option<&'a str>,
    /// `Access-Control-Request-Headers` (a CORS preflight).
    pub request_headers: Option<&'a str>,
    /// `Access-Control-Request-Private-Network: true` (Chrome's Private Network Access).
    pub private_network: bool,
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
    let mut head = Head {
        method,
        path,
        query,
        host: None,
        accept_encoding: None,
        if_none_match: None,
        request_headers: None,
        private_network: false,
        content_length: 0,
        len: end.1,
    };
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
        } else if name.eq_ignore_ascii_case("accept-encoding") {
            head.accept_encoding = Some(value);
        } else if name.eq_ignore_ascii_case("if-none-match") {
            head.if_none_match = Some(value);
        } else if name.eq_ignore_ascii_case("access-control-request-headers") {
            head.request_headers = Some(value);
        } else if name.eq_ignore_ascii_case("access-control-request-private-network") {
            head.private_network = value.eq_ignore_ascii_case("true");
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
        304 => "Not Modified",
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

/// Sent with every response.
pub const CORS: &str = "Access-Control-Allow-Origin: *\r\nAccess-Control-Expose-Headers: *\r\n";

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
    out.push(CORS.as_bytes());
    out.push(b"\r\n");
}

/// The whole response to a CORS preflight (`OPTIONS`, any path): every method, the
/// headers asked for, cached for a day.
pub fn preflight(h: &Head<'_>, out: &mut Buf<'_>) {
    out.push(b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Methods: GET, HEAD, POST, OPTIONS\r\n");
    let _ = write!(out, "Access-Control-Allow-Headers: {}\r\nAccess-Control-Max-Age: 86400\r\n", h.request_headers.unwrap_or("*"));
    if h.private_network {
        out.push(b"Access-Control-Allow-Private-Network: true\r\n");
    }
    out.push(b"Vary: Origin, Access-Control-Request-Method, Access-Control-Request-Headers\r\nContent-Length: 0\r\nConnection: close\r\n");
    out.push(CORS.as_bytes());
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
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 5\r\nConnection: close\r\nMCP-Protocol-Version: 2025-06-18\r\n\
             Access-Control-Allow-Origin: *\r\nAccess-Control-Expose-Headers: *\r\n\r\n"
        );
    }

    #[test]
    fn caching_and_cors_headers() {
        let req = b"GET / HTTP/1.1\r\nAccept-Encoding: gzip, br\r\nIf-None-Match: \"ab\"\r\n\r\n";
        let h = parse_head(req).unwrap().unwrap();
        assert_eq!((h.accept_encoding, h.if_none_match), (Some("gzip, br"), Some("\"ab\"")));

        let req = b"OPTIONS /api/gpio/5 HTTP/1.1\r\nOrigin: http://x\r\nAccess-Control-Request-Method: POST\r\n\
                    Access-Control-Request-Headers: content-type\r\nAccess-Control-Request-Private-Network: true\r\n\r\n";
        let h = parse_head(req).unwrap().unwrap();
        let mut mem = [0u8; 512];
        let mut b = Buf::new(&mut mem);
        preflight(&h, &mut b);
        let t = b.as_str();
        assert!(t.starts_with("HTTP/1.1 204 No Content\r\n"), "{t}");
        assert!(t.contains("Access-Control-Allow-Headers: content-type\r\n"), "{t}");
        assert!(t.contains("Access-Control-Allow-Private-Network: true\r\n"), "{t}");
        assert!(t.contains("Access-Control-Allow-Origin: *\r\n") && t.ends_with("\r\n\r\n"), "{t}");
    }
}
