// Embed the shipped board files (backend/boards/*.toml), so a lone binary still knows
// every board. An installed /usr/share/benchbob/boards takes precedence at run time.

use std::fmt::Write;
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../boards");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect())
        .unwrap_or_default();
    files.sort();
    let mut out = String::from("pub static EMBEDDED: &[(&str, &str)] = &[\n");
    for f in &files {
        println!("cargo:rerun-if-changed={}", f.display());
        let name = f.file_name().unwrap().to_string_lossy();
        let abs = f.canonicalize().unwrap();
        writeln!(out, "    ({name:?}, include_str!({:?})),", abs.display().to_string()).unwrap();
    }
    out.push_str("];\n");
    std::fs::write(PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("embedded.rs"), out).unwrap();
}
