//! Embeds the web UI's MCU build (`web/dist-mcu`, or the directory in BENCHBOB_WEB_DIST)
//! as `$OUT_DIR/web.rs`: a `benchbob_proto::web::Asset` per file, gzipped and raw, and
//! `BUILD`, the firmware version plus a hash of the UI, stamped into index.html's
//! `__BUILD_VERSION__`.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=BENCHBOB_WEB_DIST");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let dist = match std::env::var_os("BENCHBOB_WEB_DIST") {
        Some(d) => PathBuf::from(d),
        None => manifest.join("../../web/dist-mcu"),
    };
    if !dist.join("index.html").is_file() {
        panic!(
            "\n\nNo web UI build at {}.\nBuild it first: cd web && bun install && bun run build:mcu\n\
             (firmware/build.sh does this), or set BENCHBOB_WEB_DIST to a dist-mcu directory.\n\n",
            dist.display()
        );
    }
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut files = Vec::new();
    walk(&dist, &dist, &mut files);
    files.sort();

    // The build id covers every file as built (index.html before stamping, so the value
    // isn't self-referential).
    let mut h = Fnv::new();
    for (path, bytes) in &files {
        h.update(path.as_bytes());
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    let build = format!("{}+{:08x}", std::env::var("CARGO_PKG_VERSION").unwrap(), h.0 as u32);

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut rs = String::new();
    writeln!(rs, "/// Firmware version + web UI hash.\npub const BUILD: &str = {build:?};").unwrap();
    writeln!(rs, "pub static ASSETS: &[benchbob_proto::web::Asset] = &[").unwrap();
    for (i, (path, bytes)) in files.iter().enumerate() {
        let raw = if path == "/index.html" { String::from_utf8_lossy(bytes).replace("__BUILD_VERSION__", &build).into_bytes() } else { bytes.clone() };
        let raw_file = out.join(format!("web-{i}"));
        let gz_file = out.join(format!("web-{i}.gz"));
        std::fs::write(&raw_file, &raw).unwrap();
        std::fs::write(&gz_file, gzip(&raw)).unwrap();
        let mut e = Fnv::new();
        e.update(&raw);
        writeln!(
            rs,
            "    benchbob_proto::web::Asset {{ path: {path:?}, mime: {:?}, gz: include_bytes!({:?}), raw: include_bytes!({:?}), etag: \"\\\"{:016x}\\\"\", immutable: {} }},",
            mime(path),
            gz_file.display().to_string(),
            raw_file.display().to_string(),
            e.0,
            path.starts_with("/assets/"),
        )
        .unwrap();
    }
    writeln!(rs, "];").unwrap();
    std::fs::write(out.join("web.rs"), rs).unwrap();
}

fn walk(root: &Path, dir: &Path, files: &mut Vec<(String, Vec<u8>)>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            walk(root, &p, files);
        } else if p.extension().is_some_and(|x| x == "gz") {
            // A precompressed copy from another tool; we compress ourselves.
        } else {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            files.push((format!("/{rel}"), std::fs::read(&p).unwrap()));
        }
    }
}

fn gzip(data: &[u8]) -> Vec<u8> {
    // GzEncoder writes mtime 0, so the image is reproducible.
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn mime(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, x)| x) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// FNV-1a, 64-bit.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn update(&mut self, b: &[u8]) {
        for &x in b {
            self.0 = (self.0 ^ x as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
}
