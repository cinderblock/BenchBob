//! `benchbob`: the server binary. Everything but argument parsing is in the library.

use std::process::ExitCode;

use benchbob_core::paths::version;

const HELP: &str = "benchbob: the BenchBob server (dashboard, REST API, SSE, WebSockets, MCP)

Usage: benchbob [--version | --help]

Environment:
  PORT                    listen port (default 80), IPv6 + IPv4
  BENCHBOB_STATE_DIR      settings directory (else $STATE_DIRECTORY, else the app root)
  BENCHBOB_ROOT           where web/dist (the dashboard) and VERSION are (default: the
                          nearest directory above the executable with a web/ directory)
  BENCHBOB_BOARD          force a board id instead of detecting it
  BENCHBOB_BOARDS_DIR     shipped board files (default /usr/share/benchbob/boards, else
                          the copies built in)
  BENCHBOB_BOARDS_D       user board files (default /etc/benchbob/boards.d)
  BENCHBOB_PORT_OFFSET    shift every peripheral listener port
  RUST_LOG                log filter (default info)
";

fn main() -> ExitCode {
    // SAFETY: first thing in main, before any thread exists.
    unsafe {
        benchbob_core::paths::alias_legacy_env();
        // The tools behind the panels (iw, ip, pinctrl, vcgencmd…) live in the sbin
        // directories too, which a login shell's PATH (a side instance) may not have.
        let path = std::env::var("PATH").unwrap_or_default();
        let mut dirs: Vec<&str> = path.split(':').filter(|d| !d.is_empty()).collect();
        for d in benchbob_core::run::SYSTEM_PATH.split(':') {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
        std::env::set_var("PATH", dirs.join(":"));
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("benchbob {} (app {})", env!("CARGO_PKG_VERSION"), version());
        return ExitCode::SUCCESS;
    }
    if !args.is_empty() {
        print!("{HELP}");
        return if args.iter().any(|a| a == "--help" || a == "-h") { ExitCode::SUCCESS } else { ExitCode::from(2) };
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .without_time() // journald adds its own
        .init();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    match rt.block_on(benchbob::serve()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
