//! The file browser (server.ts "File Browser"): read-only browsing and downloads of the
//! auto-mounted partitions under FILES_ROOT.
//!
//! Everything goes through sudo: ext4 drives keep their own ownership, so the service user
//! alone can't read them all. That grants nothing new (Samba already shares this tree to
//! guests as root), but it means a path must never escape the root: every request is
//! realpath-resolved and checked, which also turns away absolute symlinks on a Linux rootfs
//! drive (they resolve against the host, not the drive).

use std::cmp::Ordering;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use benchbob_core::http::{HttpError, parse_float, parse_int};
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::sys::Sys;
use crate::util::{encode_uri_component, http_date, js_round, locale_cmp};

pub const MAX_LISTING: usize = 5000;
/// find's -printf format: type, target type, size, mtime, link target, name, NUL-separated
/// (NUL is the one byte a file name can't contain).
pub const FIND_FMT: &str = "%y\\0%Y\\0%s\\0%T@\\0%l\\0%f\\0";
const FIND_FIELDS: usize = 6;

pub fn files_root() -> String {
    benchbob_core::paths::env("FILES_ROOT").unwrap_or_else(|| "/srv/nfs/drives".into()).trim_end_matches('/').to_string()
}

#[derive(Debug, Clone)]
pub struct StatEntry {
    /// find %y: f d l b c p s
    pub kind: String,
    /// find %Y: the type after following links; N dangling, L loop
    pub target: String,
    pub size: f64,
    pub mtime: f64,
    pub link: String,
    pub name: String,
}

pub fn parse_find(raw: &str) -> Vec<StatEntry> {
    let f: Vec<&str> = raw.split('\0').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + FIND_FIELDS <= f.len() {
        let mt = parse_float(f[i + 3]).map(|x| js_round(x * 1000.0)).unwrap_or(0.0);
        out.push(StatEntry {
            kind: f[i].into(),
            target: f[i + 1].into(),
            size: parse_int(f[i + 2]).unwrap_or(0) as f64,
            mtime: if mt.is_nan() { 0.0 } else { mt },
            link: f[i + 4].into(),
            name: f[i + 5].into(),
        });
        i += FIND_FIELDS;
    }
    out
}

pub struct Files<'a> {
    pub sys: &'a dyn Sys,
    pub root: String,
}

impl Files<'_> {
    fn inside(&self, p: &str) -> bool {
        p == self.root || p.starts_with(&format!("{}/", self.root))
    }

    /// A root-relative path to a real absolute path inside the root, or an error.
    pub async fn resolve(&self, rel: &str) -> Result<String, HttpError> {
        if rel.contains('\0') {
            return Err(HttpError::new(400, "Invalid path"));
        }
        let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
        if parts.contains(&"..") {
            return Err(HttpError::new(400, "Invalid path"));
        }
        let abs = std::iter::once(self.root.as_str()).chain(parts).collect::<Vec<_>>().join("/");
        // -z: NUL-terminated, so the trim can't eat trailing spaces in a directory name.
        let r = self.sys.run(&["sudo", "realpath", "-ez", "--", &abs]).await;
        let out = r.out.strip_suffix('\0').unwrap_or(&r.out).to_string();
        if r.code != 0 || out.is_empty() {
            return Err(HttpError::new(404, "Not found"));
        }
        if !self.inside(&out) {
            return Err(HttpError::new(403, "Path leads outside the drives"));
        }
        Ok(out)
    }

    fn rel_of(&self, real: &str) -> String {
        if real == self.root { String::new() } else { real[self.root.len() + 1..].to_string() }
    }

    pub async fn stat(&self, real: &str) -> Result<StatEntry, HttpError> {
        let r = self.sys.run(&["sudo", "find", real, "-maxdepth", "0", "-printf", FIND_FMT]).await;
        match parse_find(&r.out).into_iter().next() {
            Some(e) if r.code == 0 => Ok(e),
            _ => Err(HttpError::new(404, "Not found")),
        }
    }

    /// Two batched calls however many links a directory has: realpath them all, then stat
    /// only the targets inside the root, so a link to a host file never reveals so much as
    /// its size. Per name: (outside, stat of the target).
    async fn resolve_links(&self, dir: &str, names: &[String]) -> Vec<(bool, Option<StatEntry>)> {
        if names.is_empty() {
            return vec![];
        }
        let paths: Vec<String> = names.iter().map(|n| format!("{dir}/{n}")).collect();
        let mut cmd: Vec<&str> = vec!["sudo", "realpath", "-mz", "--"];
        cmd.extend(paths.iter().map(String::as_str));
        let mut r = self.sys.run(&cmd).await;
        if r.code != 0 {
            let mut cmd: Vec<&str> = vec!["sudo", "sh", "-c", "for p; do realpath -mz -- \"$p\" 2>/dev/null || printf \"\\0\"; done", "sh"];
            cmd.extend(paths.iter().map(String::as_str));
            r = self.sys.run(&cmd).await;
        }
        let reals: Vec<&str> = r.out.split('\0').collect();
        let mut result: Vec<(bool, Option<StatEntry>)> = Vec::new();
        let mut inside: Vec<String> = Vec::new();
        for i in 0..names.len() {
            let real = reals.get(i).copied().unwrap_or("");
            let outside = !real.is_empty() && !self.inside(real);
            result.push((outside, None));
            if !real.is_empty() && !outside {
                inside.push(real.to_string());
            }
        }
        if !inside.is_empty() {
            let fmt = format!("%p\\0{FIND_FMT}");
            let mut cmd: Vec<&str> = vec!["sudo", "find"];
            cmd.extend(inside.iter().map(String::as_str));
            cmd.extend(["-maxdepth", "0", "-printf", &fmt]);
            let out = self.sys.run(&cmd).await.out;
            let f: Vec<&str> = out.split('\0').collect();
            let mut by_path: Vec<(String, StatEntry)> = Vec::new();
            let mut i = 0;
            while i + FIND_FIELDS < f.len() {
                if let Some(e) = parse_find(&(f[i + 1..i + 1 + FIND_FIELDS].join("\0") + "\0")).into_iter().next() {
                    by_path.retain(|(p, _)| p != f[i]);
                    by_path.push((f[i].to_string(), e));
                }
                i += FIND_FIELDS + 1;
            }
            for (k, _) in names.iter().enumerate() {
                let real = reals.get(k).copied().unwrap_or("");
                if let Some((_, s)) = by_path.iter().find(|(p, _)| p == real) {
                    // Still a link after resolving means a loop: as unusable as a dangling one.
                    if s.kind != "l" {
                        result[k].1 = Some(s.clone());
                    }
                }
            }
        }
        result
    }

    /// `listDir`.
    pub async fn list(&self, rel: &str) -> Result<Value, HttpError> {
        let real = self.resolve(rel).await?;
        if self.stat(&real).await?.kind != "d" {
            return Err(HttpError::new(400, "Not a directory"));
        }
        let r = self.sys.run(&["sudo", "find", &real, "-mindepth", "1", "-maxdepth", "1", "-printf", FIND_FMT]).await;
        if r.code != 0 && r.out.is_empty() {
            return Err(HttpError::new(500, if r.err.is_empty() { "Could not read directory".to_string() } else { r.err.clone() }));
        }
        let raw = parse_find(&r.out);
        let links: Vec<String> = raw.iter().filter(|e| e.kind == "l").map(|e| e.name.clone()).collect();
        let targets = self.resolve_links(&real, &links).await;
        let target_of = |name: &str| links.iter().position(|n| n == name).map(|i| &targets[i]);
        let mut entries: Vec<(String, Map<String, Value>)> = raw
            .iter()
            .map(|e| {
                // A link is shown as what it points at; `link` keeps it recognisable.
                let t = if e.kind == "l" { target_of(&e.name) } else { None };
                let st = t.and_then(|t| t.1.as_ref()).unwrap_or(e);
                let unresolved = e.kind == "l" && t.and_then(|t| t.1.as_ref()).is_none();
                let ty = if unresolved {
                    if t.is_some_and(|t| t.0) { "outside" } else { "broken" }
                } else if st.kind == "d" {
                    "dir"
                } else if st.kind == "f" {
                    "file"
                } else {
                    "other"
                };
                let mut m = Map::new();
                m.insert("name".into(), e.name.clone().into());
                m.insert("type".into(), ty.into());
                m.insert("size".into(), if ty == "file" || ty == "other" { benchbob_core::http::num(st.size) } else { Value::Null });
                m.insert("mtime".into(), if unresolved { Value::Null } else { benchbob_core::http::num(st.mtime) });
                if e.kind == "l" {
                    m.insert("link".into(), e.link.clone().into());
                }
                (ty.to_string(), m)
            })
            .collect();
        entries.sort_by(|a, b| {
            let (da, db) = (if a.0 == "dir" { 0 } else { 1 }, if b.0 == "dir" { 0 } else { 1 });
            da.cmp(&db).then_with(|| locale_cmp(a.1["name"].as_str().unwrap_or(""), b.1["name"].as_str().unwrap_or(""), true, true))
        });
        let total = entries.len();
        let mut out = Map::new();
        out.insert("ok".into(), true.into());
        out.insert("path".into(), self.rel_of(&real).into());
        // Readable only in part (permission errors on some entries) still lists what it can.
        if r.code != 0 {
            out.insert("warning".into(), r.err.split('\n').next().unwrap_or("").into());
        }
        out.insert("truncated".into(), (total > MAX_LISTING).into());
        out.insert("total".into(), total.into());
        out.insert("entries".into(), entries.into_iter().take(MAX_LISTING).map(|e| Value::Object(e.1)).collect());
        Ok(Value::Object(out))
    }

    /// `downloadFile`: Range support (so `curl -C -` resumes), sendfile-like reads where the
    /// service user can read the file, `sudo dd` where it can't.
    pub async fn download(&self, rel: &str, method: &axum::http::Method, range: Option<&str>) -> Result<Response, HttpError> {
        let real = self.resolve(rel).await?;
        let st = self.stat(&real).await?;
        if st.kind != "f" {
            return Err(HttpError::new(400, "Not a regular file"));
        }
        let size = st.size as u64;
        let range = match parse_range(range, size) {
            Range::Bad => {
                let mut r = Response::new(Body::empty());
                *r.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                r.headers_mut().insert(header::CONTENT_RANGE, hv(&format!("bytes */{size}")));
                return Ok(r);
            }
            Range::All => None,
            Range::Some(s, e) => Some((s, e)),
        };
        let start = range.map_or(0, |r| r.0);
        let length = range.map_or(size, |(s, e)| e - s + 1);
        let mut res = Response::new(Body::empty());
        *res.status_mut() = if range.is_some() { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };
        let h = res.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
        h.insert(header::CONTENT_DISPOSITION, hv(&content_disposition(&st.name)));
        h.insert(header::CONTENT_LENGTH, hv(&length.to_string()));
        h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        h.insert(header::LAST_MODIFIED, hv(&http_date(st.mtime as i64)));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        if let Some((s, e)) = range {
            h.insert(header::CONTENT_RANGE, hv(&format!("bytes {s}-{e}/{size}")));
        }
        if method == axum::http::Method::HEAD || length == 0 {
            return Ok(res);
        }
        *res.body_mut() = self.body(&real, start, length).await;
        Ok(res)
    }

    async fn body(&self, real: &str, start: u64, length: u64) -> Body {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        if self.sys.readable(real).await
            && let Ok(mut f) = tokio::fs::File::open(real).await
            && f.seek(std::io::SeekFrom::Start(start)).await.is_ok()
        {
            let stream = futures_util::stream::unfold((f.take(length), vec![0u8; 256 * 1024]), |(mut r, mut buf)| async move {
                match r.read(&mut buf).await {
                    Ok(0) => None,
                    Ok(n) => Some((Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&buf[..n])), (r, buf))),
                    Err(e) => Some((Err(e), (r, buf))),
                }
            });
            return Body::from_stream(stream);
        }
        // Not readable as the service user: dd as root. Dropping the body (a cancelled
        // download) drops the process, which is then killed.
        let cmd = [
            "sudo".to_string(),
            "dd".into(),
            format!("if={real}"),
            "bs=1M".into(),
            "iflag=skip_bytes,count_bytes".into(),
            format!("skip={start}"),
            format!("count={length}"),
            "status=none".into(),
        ];
        let argv: Vec<&str> = cmd.iter().map(String::as_str).collect();
        match self.sys.spawn(&argv) {
            Ok(p) => Body::from_stream(futures_util::stream::unfold(p, |mut p| async move {
                p.stdout().await.map(|c| (Ok::<_, std::io::Error>(bytes::Bytes::from(c)), p))
            })),
            Err(_) => Body::empty(),
        }
    }
}

fn hv(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).unwrap_or_else(|_| HeaderValue::from_static(""))
}

#[derive(Debug, PartialEq)]
pub enum Range {
    All,
    Some(u64, u64),
    Bad,
}

/// A single-range `Range` header; multi-range or malformed means the whole file.
pub fn parse_range(h: Option<&str>, size: u64) -> Range {
    let Some(h) = h.filter(|h| !h.is_empty()) else { return Range::All };
    let Some(m) = Regex::new(r"^bytes=(\d*)-(\d*)$").unwrap().captures(h) else { return Range::All };
    let (a, b) = (&m[1], &m[2]);
    if a.is_empty() && b.is_empty() {
        return Range::All;
    }
    let num = |s: &str| s.parse::<u128>().unwrap_or(u128::MAX);
    let size = size as u128;
    let (start, end) = if a.is_empty() {
        (size.saturating_sub(num(b)), size as i128 - 1)
    } else {
        (num(a), if b.is_empty() { size as i128 - 1 } else { (num(b) as i128).min(size as i128 - 1) })
    };
    if start >= size || (start as i128) > end {
        return Range::Bad;
    }
    Range::Some(start as u64, end as u64)
}

/// An ASCII fallback name plus the exact UTF-8 one.
pub fn content_disposition(name: &str) -> String {
    let ascii: String = name.chars().map(|c| if (' '..='~').contains(&c) && c != '"' && c != '\\' { c } else { '_' }).collect();
    // JavaScript replaces per UTF-16 unit: a character outside the BMP becomes two "_".
    let ascii: String = ascii
        .chars()
        .zip(name.chars())
        .flat_map(|(a, c)| std::iter::repeat_n(a, if a == '_' && c.len_utf16() == 2 { 2 } else { 1 }))
        .collect();
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{}", encode_uri_component(name))
}

pub fn order(a: &str, b: &str) -> Ordering {
    locale_cmp(a, b, true, true)
}

/// For tests: a listing request's outcome, as the TS harness writes it.
pub fn error_json(e: &HttpError) -> Value {
    json!({ "error": e.message, "status": e.status })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 100), Range::All);
        assert_eq!(parse_range(Some("bytes=0-9"), 100), Range::Some(0, 9));
        assert_eq!(parse_range(Some("bytes=90-"), 100), Range::Some(90, 99));
        assert_eq!(parse_range(Some("bytes=-10"), 100), Range::Some(90, 99));
        assert_eq!(parse_range(Some("bytes=-1000"), 100), Range::Some(0, 99));
        assert_eq!(parse_range(Some("bytes=50-1000"), 100), Range::Some(50, 99));
        assert_eq!(parse_range(Some("bytes=100-"), 100), Range::Bad);
        assert_eq!(parse_range(Some("bytes=9-3"), 100), Range::Bad);
        assert_eq!(parse_range(Some("bytes=0-1,5-6"), 100), Range::All);
        assert_eq!(parse_range(Some("bytes=-"), 100), Range::All);
        assert_eq!(parse_range(Some("bytes=-0"), 100), Range::Bad);
    }

    #[test]
    fn dispositions() {
        assert_eq!(content_disposition("a \"b\".txt"), "attachment; filename=\"a _b_.txt\"; filename*=UTF-8''a%20%22b%22.txt");
        assert_eq!(content_disposition("über😀.bin"), "attachment; filename=\"_ber__.bin\"; filename*=UTF-8''%C3%BCber%F0%9F%98%80.bin");
    }
}
