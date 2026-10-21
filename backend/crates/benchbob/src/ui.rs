//! Serving the dashboard: `web/dist`, the component UI's build, at `/` and the files below.
//!
//! The dashboard is a long-lived page held open for hours, so a deploy would otherwise
//! leave a stale UI talking to a newer backend. The build version is substituted into the
//! page (`__BUILD_VERSION__`) and sent in every status; a page seeing another version
//! reloads. It hashes what defines the running app: this executable and web/dist's
//! index.html (whose asset names are content hashes). A cheap mtime/size stamp picks up a
//! new web build without a restart.
//!
//! Without a build (a checkout whose `web/` hasn't been built), `/` says how to build it;
//! the API works regardless.

use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use xxhash_rust::xxh64::{Xxh64, xxh64};

#[derive(Clone)]
pub struct Build {
    stamp: String,
    pub version: String,
    /// web/dist/index.html, version stamped (or the "not built" page).
    pub html: String,
}

/// `/` when there's no web/dist/index.html.
const NOT_BUILT: &str = "<!doctype html>
<meta charset=\"utf-8\"><title>BenchBob</title>
<h1>BenchBob</h1>
<p>The dashboard isn't built. In the checkout: <code>cd web &amp;&amp; bun install &amp;&amp; bun run build</code>,
or point <code>BENCHBOB_ROOT</code> at a directory with <code>web/dist</code>.
The API is at <a href=\"/api\">/api</a>.</p>
";

pub struct Ui {
    root: PathBuf,
    exe_hash: u64,
    cache: Mutex<Option<Build>>,
    gz: Mutex<Option<(String, Bytes)>>,
}

fn stamp_of(p: &Path) -> String {
    match std::fs::metadata(p) {
        Ok(m) => {
            format!("{}:{}", m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis()).unwrap_or(0), m.len())
        }
        Err(_) => "-".into(),
    }
}

pub fn accepts_gzip(h: &HeaderMap) -> bool {
    h.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split([',', ' ', ';']).any(|t| t == "gzip"))
}

pub fn gzip(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = e.write_all(data);
    e.finish().unwrap_or_default()
}

impl Ui {
    pub fn new(root: &Path) -> Self {
        // Once: the binary can only change by being replaced, which restarts the service.
        let exe_hash = std::env::current_exe().ok().and_then(|p| std::fs::read(p).ok()).map(|b| xxh64(&b, 0)).unwrap_or(0);
        Self { root: root.to_path_buf(), exe_hash, cache: Mutex::new(None), gz: Mutex::new(None) }
    }

    pub fn web_dir(&self) -> Option<PathBuf> {
        let d = self.root.join("web/dist");
        d.join("index.html").is_file().then_some(d)
    }

    /// The current build.
    pub fn build(&self) -> Build {
        let web = self.root.join("web/dist/index.html");
        let stamp = stamp_of(&web);
        if let Some(b) = self.cache.lock().unwrap().as_ref()
            && b.stamp == stamp
        {
            return b.clone();
        }
        let web_text = std::fs::read_to_string(&web).ok();
        // Hash the page as stored, before substitution, so the value is stable rather than
        // self-referential.
        let t = web_text.as_deref().unwrap_or("");
        let mut h = Xxh64::new(0);
        h.update(&self.exe_hash.to_le_bytes());
        h.update(&(t.len() as u64).to_le_bytes());
        h.update(t.as_bytes());
        let version = format!("{:x}", h.digest());
        let html = match web_text {
            Some(t) => t.replace("__BUILD_VERSION__", &version),
            None => NOT_BUILT.to_string(),
        };
        let b = Build { stamp, version, html };
        *self.cache.lock().unwrap() = Some(b.clone());
        b
    }

    /// A page with the version as ETag: no-cache (revalidate every load, so a deploy is
    /// seen without a hard refresh), 304 on a match, gzip once per build.
    pub fn page(&self, html: &str, version: &str, headers: &HeaderMap) -> Response {
        let etag = format!("\"{version}\"");
        let mut r = Response::new(Body::empty());
        let h = r.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        h.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
        h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
            *r.status_mut() = StatusCode::NOT_MODIFIED;
            return r;
        }
        if !accepts_gzip(headers) {
            *r.body_mut() = Body::from(html.to_string());
            return r;
        }
        let mut gz = self.gz.lock().unwrap();
        let body = match gz.as_ref().filter(|(v, _)| v == version) {
            Some((_, b)) => b.clone(),
            None => {
                let b = Bytes::from(gzip(html.as_bytes()));
                *gz = Some((version.to_string(), b.clone()));
                b
            }
        };
        r.headers_mut().insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        *r.body_mut() = Body::from(body);
        r
    }

    /// A file under web/dist, for GET/HEAD. None if there's no such file.
    pub fn static_file(&self, method: &Method, path: &str, headers: &HeaderMap) -> Option<Response> {
        if method != Method::GET && method != Method::HEAD {
            return None;
        }
        let dir = self.web_dir()?;
        let rel = Path::new(path.trim_start_matches('/'));
        if rel.as_os_str().is_empty() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return None;
        }
        let file = dir.join(rel);
        if !file.is_file() {
            return None;
        }
        let data = std::fs::read(&file).ok()?;
        let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
        let ctype = match ext {
            "js" | "mjs" => "text/javascript; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "html" => "text/html; charset=utf-8",
            "json" | "map" => "application/json",
            "svg" => "image/svg+xml",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "ico" => "image/x-icon",
            "webp" => "image/webp",
            "woff2" => "font/woff2",
            "woff" => "font/woff",
            "txt" => "text/plain; charset=utf-8",
            "webmanifest" => "application/manifest+json",
            _ => "application/octet-stream",
        };
        let mut r = Response::new(Body::empty());
        let h = r.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
        // Vite names assets by content hash: they never change under the same name.
        let immutable = path.starts_with("/assets/");
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(if immutable { "public, max-age=31536000, immutable" } else { "no-cache" }),
        );
        let text = ctype.starts_with("text/") || ctype.contains("json") || ctype.contains("svg");
        let body = if text && data.len() >= 1024 && accepts_gzip(headers) {
            h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
            gzip(&data)
        } else {
            data
        };
        if method != Method::HEAD {
            *r.body_mut() = Body::from(body);
        }
        Some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_the_version_and_follows_a_new_build() {
        let d = tempfile::tempdir().unwrap();
        let ui = Ui::new(d.path());
        let none = ui.build();
        assert!(none.html.contains("isn't built"));
        assert!(ui.web_dir().is_none());
        std::fs::create_dir_all(d.path().join("web/dist")).unwrap();
        std::fs::write(d.path().join("web/dist/index.html"), "<p>__BUILD_VERSION__</p>\n<i>__BUILD_VERSION__</i>").unwrap();
        let b = ui.build();
        assert_eq!(b.html, format!("<p>{0}</p>\n<i>{0}</i>", b.version));
        assert_ne!(b.version, none.version);
        std::fs::write(d.path().join("web/dist/index.html"), "<p>__BUILD_VERSION__</p> rebuilt").unwrap();
        assert_ne!(ui.build().version, b.version);
    }

    #[test]
    fn web_dist_files_are_served_safely() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("web/dist/assets")).unwrap();
        std::fs::write(d.path().join("web/dist/index.html"), "<meta content=\"__BUILD_VERSION__\">").unwrap();
        std::fs::write(d.path().join("web/dist/assets/x.js"), "x".repeat(2000)).unwrap();
        let ui = Ui::new(d.path());
        let b = ui.build();
        assert_eq!(b.html, format!("<meta content=\"{}\">", b.version));
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip, deflate"));
        let r = ui.static_file(&Method::GET, "/assets/x.js", &h).unwrap();
        assert_eq!(r.headers()[header::CONTENT_ENCODING], "gzip");
        assert!(r.headers()[header::CACHE_CONTROL].to_str().unwrap().contains("immutable"));
        assert!(ui.static_file(&Method::GET, "/../index.html", &h).is_none());
        assert!(ui.static_file(&Method::GET, "/assets/nope.js", &h).is_none());
        assert!(ui.static_file(&Method::POST, "/assets/x.js", &h).is_none());
    }
}
