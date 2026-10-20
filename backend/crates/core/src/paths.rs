//! Where BenchBob keeps what it writes (settings, saved lists), where its app files are, and
//! which release it is. Same files and variables as `paths.ts`, so swapping the Bun server
//! for this one keeps every setting.
//!
//! A packaged install keeps state in /var/lib/benchbob: systemd's `StateDirectory=` creates
//! it and passes it as `$STATE_DIRECTORY`. `BENCHBOB_STATE_DIR` overrides that (a side
//! instance, tests). Run from a checkout, the default is the app root, where these files
//! have always lived.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// BenchBob was DeskyPi: copy every `DESKYPI_*` variable to `BENCHBOB_*` where that isn't
/// set. Call once at the very start of `main`, before any thread exists.
///
/// # Safety
/// Mutates the process environment; no other thread may be reading it.
pub unsafe fn alias_legacy_env() {
    let old: Vec<(String, String)> = std::env::vars().filter(|(k, _)| k.starts_with("DESKYPI_")).collect();
    for (k, v) in old {
        let new = format!("BENCHBOB_{}", &k[8..]);
        if std::env::var_os(&new).is_none() {
            // SAFETY: the caller guarantees we're single-threaded.
            unsafe { std::env::set_var(new, v) };
        }
    }
}

/// An environment variable, `None` when unset or empty (as `process.env.X ||` treats it).
pub fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The app's files: `index.html`, `ui/`, `VERSION`, `web/dist`. `BENCHBOB_ROOT` if set;
/// otherwise the nearest directory at or above the executable that has an `index.html`
/// (`/usr/lib/benchbob` for `/usr/lib/benchbob/bin/benchbob`, the checkout for a cargo
/// build); otherwise the working directory.
pub fn app_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        if let Some(r) = env("BENCHBOB_ROOT") {
            return PathBuf::from(r);
        }
        if let Ok(exe) = std::env::current_exe() {
            let exe = exe.canonicalize().unwrap_or(exe);
            for dir in exe.ancestors().skip(1).take(5) {
                if dir.join("index.html").is_file() {
                    return dir.to_path_buf();
                }
            }
        }
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    })
}

/// The state directory, created on first use.
pub fn state_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = env("BENCHBOB_STATE_DIR")
            .or_else(|| env("STATE_DIRECTORY").map(|s| s.split(':').next().unwrap_or("").to_string()))
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| app_root().to_path_buf());
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("can't create state dir {}: {e}", dir.display());
        }
        dir
    })
}

/// A file in the state directory: `peripherals.json`, `defaults.json`, `update.json`…
pub fn state_path(name: &str) -> PathBuf {
    state_dir().join(name)
}

/// The release version: packaging writes a VERSION file next to the code. A checkout says "dev".
pub fn version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        std::fs::read_to_string(app_root().join("VERSION"))
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "dev".into())
    })
}

/// The port offset every peripheral listener (serial bridges, usbredir…) is shifted by.
pub fn port_offset() -> u16 {
    env("BENCHBOB_PORT_OFFSET").and_then(|v| v.parse().ok()).unwrap_or(0)
}
