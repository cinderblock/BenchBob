//! Serving the web UI from flash: the MCU build of `web/` (`web/dist-mcu`), embedded by
//! the node crate's build script as a table of [`Asset`]s.
//!
//! Each file is stored gzipped and raw, so a client that doesn't take gzip still gets the
//! page. `index.html` is revalidated on every load (`no-cache` plus an ETag, so a reflash
//! is seen without a hard refresh); everything under `/assets/` has a content hash in its
//! name and is cached for good.

use crate::http;
use crate::json::Buf;
use core::fmt::Write;

pub struct Asset {
    /// Request path: `/index.html`, `/assets/index-<hash>.js`.
    pub path: &'static str,
    pub mime: &'static str,
    pub gz: &'static [u8],
    pub raw: &'static [u8],
    /// Quoted, as sent: `"<hash>"`.
    pub etag: &'static str,
    /// Content-hashed name: cache for a year without revalidating.
    pub immutable: bool,
}

/// What to send for a UI request.
#[derive(Clone, Copy)]
pub struct Static<'a> {
    pub asset: &'a Asset,
    /// 200, or 304 when the client's copy is current.
    pub status: u16,
    pub gzip: bool,
    /// HEAD: the headers of a GET, without the body.
    pub head_only: bool,
}

impl<'a> Static<'a> {
    /// The bytes after the head (empty for HEAD and 304).
    pub fn body(&self) -> &'a [u8] {
        if self.head_only || self.status == 304 {
            &[]
        } else if self.gzip {
            self.asset.gz
        } else {
            self.asset.raw
        }
    }

    /// Status line and headers.
    pub fn write_head(&self, out: &mut Buf<'_>) {
        let a = self.asset;
        let _ = write!(out, "HTTP/1.1 {} {}\r\n", self.status, http::reason(self.status));
        if self.status != 304 {
            let len = if self.gzip { a.gz.len() } else { a.raw.len() };
            let _ = write!(out, "Content-Type: {}\r\nContent-Length: {}\r\n", a.mime, len);
            if self.gzip {
                out.push(b"Content-Encoding: gzip\r\n");
            }
        }
        let cache = if a.immutable { "public, max-age=31536000, immutable" } else { "no-cache" };
        let _ = write!(out, "ETag: {}\r\nCache-Control: {}\r\nVary: Accept-Encoding\r\nConnection: close\r\n", a.etag, cache);
        out.push(http::CORS.as_bytes());
        out.push(b"\r\n");
    }
}

/// The UI file for a request, or None when the path isn't one (the API handles it).
/// `/` is `/index.html`.
pub fn lookup<'a>(assets: &'a [Asset], method: &str, path: &str, accept_encoding: Option<&str>, if_none_match: Option<&str>) -> Option<Static<'a>> {
    let head_only = match method {
        "GET" => false,
        "HEAD" => true,
        _ => return None,
    };
    let path = if path == "/" { "/index.html" } else { path };
    let asset = assets.iter().find(|a| a.path == path)?;
    let fresh = if_none_match.is_some_and(|v| v.split(',').map(str::trim).any(|t| t == asset.etag || t == "*"));
    Some(Static { asset, status: if fresh { 304 } else { 200 }, gzip: accepts_gzip(accept_encoding), head_only })
}

/// `Accept-Encoding` names gzip (q-values aren't weighed; browsers always send gzip).
pub fn accepts_gzip(accept_encoding: Option<&str>) -> bool {
    accept_encoding.is_some_and(|v| v.split([',', ' ', ';']).any(|t| t.eq_ignore_ascii_case("gzip")))
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::String;

    static ASSETS: [Asset; 2] = [
        Asset { path: "/index.html", mime: "text/html; charset=utf-8", gz: b"GZ", raw: b"<p>raw</p>", etag: "\"aa\"", immutable: false },
        Asset { path: "/assets/app-1.js", mime: "text/javascript; charset=utf-8", gz: b"gzjs", raw: b"js", etag: "\"bb\"", immutable: true },
    ];

    fn head(s: &Static<'_>) -> String {
        let mut mem = [0u8; 512];
        let mut b = Buf::new(&mut mem);
        s.write_head(&mut b);
        b.as_str().into()
    }

    #[test]
    fn root_is_the_page_gzipped_and_revalidated() {
        let s = lookup(&ASSETS, "GET", "/", Some("gzip, deflate, br"), None).unwrap();
        assert_eq!((s.status, s.gzip, s.body()), (200, true, &b"GZ"[..]));
        let h = head(&s);
        assert!(h.starts_with("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: 2\r\nContent-Encoding: gzip\r\n"), "{h}");
        assert!(h.contains("ETag: \"aa\"\r\nCache-Control: no-cache\r\nVary: Accept-Encoding\r\n"), "{h}");
        assert!(h.contains("Access-Control-Allow-Origin: *\r\n") && h.ends_with("\r\n\r\n"), "{h}");
        assert!(lookup(&ASSETS, "GET", "/index.html", None, None).is_some());
    }

    #[test]
    fn raw_without_gzip() {
        let s = lookup(&ASSETS, "GET", "/", None, None).unwrap();
        assert_eq!((s.gzip, s.body()), (false, &b"<p>raw</p>"[..]));
        let h = head(&s);
        assert!(h.contains("Content-Length: 10\r\n") && !h.contains("Content-Encoding"), "{h}");
        assert!(!lookup(&ASSETS, "GET", "/", Some("identity"), None).unwrap().gzip);
    }

    #[test]
    fn hashed_assets_are_immutable() {
        let s = lookup(&ASSETS, "GET", "/assets/app-1.js", Some("gzip"), None).unwrap();
        assert!(head(&s).contains("Cache-Control: public, max-age=31536000, immutable\r\n"));
    }

    #[test]
    fn not_modified_and_head() {
        let s = lookup(&ASSETS, "GET", "/", Some("gzip"), Some("\"zz\", \"aa\"")).unwrap();
        assert_eq!((s.status, s.body().len()), (304, 0));
        let h = head(&s);
        assert!(h.starts_with("HTTP/1.1 304 Not Modified\r\nETag: \"aa\"\r\n") && !h.contains("Content-Length"), "{h}");
        assert_eq!(lookup(&ASSETS, "GET", "/", None, Some("\"old\"")).unwrap().status, 200);

        let s = lookup(&ASSETS, "HEAD", "/", Some("gzip"), None).unwrap();
        assert_eq!((s.status, s.body().len()), (200, 0));
        assert!(head(&s).contains("Content-Length: 2\r\n"));
    }

    #[test]
    fn other_paths_and_methods_fall_through() {
        assert!(lookup(&ASSETS, "GET", "/api", None, None).is_none());
        assert!(lookup(&ASSETS, "GET", "/assets/missing.js", None, None).is_none());
        assert!(lookup(&ASSETS, "POST", "/", None, None).is_none());
    }
}
