//! Serving the dashboard (server.ts "Build Versioning" and the page route).
//!
//! `web/dist` (the component UI) is served at `/` when it's there, with the old page at
//! `/legacy`; without it, `/` is the old page: `index.html` with every `ui/*.js` inlined, one
//! `<script>` each, exactly as server.ts builds it.
//!
//! The dashboard is a long-lived page held open for hours, so a deploy would otherwise
//! leave a stale UI talking to a newer backend. The build version is substituted into the
//! page (`__BUILD_VERSION__`) and sent in every status; a page seeing another version
//! reloads. It hashes everything that defines the running app: this executable, the page
//! sources, web/dist's index.html (whose asset names are content hashes), and in proxy mode
//! the Bun server's own build version. A cheap mtime/size stamp picks up file changes
//! (a git pull) without a restart.

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
    /// The old page, scripts inlined, version stamped (None without index.html).
    pub legacy_html: Option<String>,
    /// web/dist/index.html, version stamped.
    pub web_html: Option<String>,
}

pub struct Ui {
    root: PathBuf,
    exe_hash: u64,
    cache: Mutex<Option<Build>>,
    gz: Mutex<Vec<(String, Bytes)>>,
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
        Self { root: root.to_path_buf(), exe_hash, cache: Mutex::new(None), gz: Mutex::new(Vec::new()) }
    }

    fn ui_files(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(self.root.join("ui"))
            .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "js")).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    pub fn web_dir(&self) -> Option<PathBuf> {
        let d = self.root.join("web/dist");
        d.join("index.html").is_file().then_some(d)
    }

    /// The current build. `extra` is folded into the version (the legacy server's build).
    pub fn build(&self, extra: &str) -> Build {
        let index = self.root.join("index.html");
        let ui = self.ui_files();
        let web = self.root.join("web/dist/index.html");
        let mut stamp = format!("{extra}|{}|{}", stamp_of(&index), stamp_of(&web));
        for f in &ui {
            stamp.push('|');
            stamp.push_str(&stamp_of(f));
        }
        if let Some(b) = self.cache.lock().unwrap().as_ref()
            && b.stamp == stamp
        {
            return b.clone();
        }
        let read = |p: &Path| std::fs::read_to_string(p).ok();
        let index_text = read(&index);
        let web_text = read(&web);
        let ui_texts: Vec<String> = ui.iter().map(|p| read(p).unwrap_or_default()).collect();
        // Hash the sources as stored, before substitution, so the value is stable rather
        // than self-referential.
        let mut h = Xxh64::new(0);
        h.update(&self.exe_hash.to_le_bytes());
        h.update(extra.as_bytes());
        for t in
            [index_text.as_deref().unwrap_or(""), web_text.as_deref().unwrap_or("")].into_iter().chain(ui_texts.iter().map(String::as_str))
        {
            h.update(&(t.len() as u64).to_le_bytes());
            h.update(t.as_bytes());
        }
        let version = format!("{:x}", h.digest());
        // One <script> per driver UI, so a mistake in one can't take down the rest of the page.
        let scripts: Vec<String> = ui
            .iter()
            .zip(&ui_texts)
            .map(|(p, t)| format!("<script>/* {} */\n{t}\n</script>", p.file_name().unwrap_or_default().to_string_lossy()))
            .collect();
        let scripts = scripts.join("\n");
        let legacy_html = index_text.map(|t| t.replacen("<!--PERIPH_UI-->", &scripts, 1).replace("__BUILD_VERSION__", &version));
        let web_html = web_text.map(|t| t.replace("__BUILD_VERSION__", &version));
        let b = Build { stamp, version, legacy_html, web_html };
        *self.cache.lock().unwrap() = Some(b.clone());
        b
    }

    /// A page with the version as ETag: no-cache (revalidate every load, so a deploy is
    /// seen without a hard refresh), 304 on a match, gzip once per build.
    pub fn page(&self, html: &str, version: &str, which: &str, headers: &HeaderMap) -> Response {
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
        let key = format!("{which}:{version}");
        let mut gz = self.gz.lock().unwrap();
        let body = match gz.iter().find(|(k, _)| *k == key) {
            Some((_, b)) => b.clone(),
            None => {
                let b = Bytes::from(gzip(html.as_bytes()));
                gz.retain(|(k, _)| !k.starts_with(&format!("{which}:")));
                gz.push((key, b.clone()));
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
    fn inlines_ui_scripts_and_stamps_the_version() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("index.html"), "<p>__BUILD_VERSION__</p>\n<!--PERIPH_UI-->\n<i>__BUILD_VERSION__</i>").unwrap();
        std::fs::create_dir(d.path().join("ui")).unwrap();
        std::fs::write(d.path().join("ui/b.js"), "b()").unwrap();
        std::fs::write(d.path().join("ui/a.js"), "a()").unwrap();
        let ui = Ui::new(d.path());
        let b = ui.build("");
        let html = b.legacy_html.unwrap();
        assert_eq!(
            html,
            format!("<p>{0}</p>\n<script>/* a.js */\na()\n</script>\n<script>/* b.js */\nb()\n</script>\n<i>{0}</i>", b.version)
        );
        assert!(b.web_html.is_none());
        // A legacy server's build and a changed file each change the version.
        assert_ne!(ui.build("x").version, b.version);
        std::fs::write(d.path().join("ui/a.js"), "a(1)").unwrap();
        assert_ne!(ui.build("").version, b.version);
    }

    #[test]
    fn web_dist_files_are_served_safely() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("web/dist/assets")).unwrap();
        std::fs::write(d.path().join("web/dist/index.html"), "<meta content=\"__BUILD_VERSION__\">").unwrap();
        std::fs::write(d.path().join("web/dist/assets/x.js"), "x".repeat(2000)).unwrap();
        let ui = Ui::new(d.path());
        let b = ui.build("");
        assert_eq!(b.web_html.unwrap(), format!("<meta content=\"{}\">", b.version));
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
