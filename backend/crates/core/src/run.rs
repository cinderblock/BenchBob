//! Child processes, as `run()`/`must()` in core.ts: stdout and stderr collected and trimmed,
//! a missing program reported as exit 127 ("not installed") rather than an error.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Where tools are looked for when the service's own PATH is thin (systemd units).
pub const SYSTEM_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Debug, Clone, Default)]
pub struct Output {
    pub out: String,
    pub err: String,
    pub code: i32,
}

#[derive(Debug, Clone, Default)]
pub struct Opts {
    pub stdin: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
}

impl Opts {
    pub fn timeout_ms(ms: u64) -> Self {
        Self { timeout: Some(Duration::from_millis(ms)), ..Default::default() }
    }
}

/// Run a command to completion. Never fails: a program that isn't installed is exit 127,
/// a timeout kills it and reports what it printed with exit -1.
pub async fn run(cmd: &[&str], opts: Opts) -> Output {
    let Some((prog, args)) = cmd.split_first() else {
        return Output { code: 127, err: "empty command".into(), ..Default::default() };
    };
    let mut c = Command::new(prog);
    c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    c.stdin(if opts.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    for (k, v) in &opts.env {
        c.env(k, v);
    }
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            let why = if e.kind() == std::io::ErrorKind::NotFound { "not installed".to_string() } else { e.to_string() };
            return Output { out: String::new(), err: format!("{prog}: {why}"), code: 127 };
        }
    };
    if let (Some(data), Some(mut stdin)) = (opts.stdin, child.stdin.take()) {
        tokio::spawn(async move {
            let _ = stdin.write_all(&data).await;
        });
    }
    let fut = child.wait_with_output();
    let res = match opts.timeout {
        Some(t) => match tokio::time::timeout(t, fut).await {
            Ok(r) => r,
            // Dropping the future drops the child, which kill_on_drop kills.
            Err(_) => return Output { out: String::new(), err: format!("{prog}: timed out"), code: -1 },
        },
        None => fut.await,
    };
    match res {
        Ok(o) => Output {
            out: String::from_utf8_lossy(&o.stdout).trim().to_string(),
            err: String::from_utf8_lossy(&o.stderr).trim().to_string(),
            code: o.status.code().unwrap_or(-1),
        },
        Err(e) => Output { out: String::new(), err: format!("{prog}: {e}"), code: -1 },
    }
}

/// `run()` that fails with stderr (or stdout, or the exit code) on a non-zero exit.
pub async fn must(cmd: &[&str], opts: Opts) -> anyhow::Result<String> {
    let r = run(cmd, opts).await;
    if r.code != 0 {
        let why = if !r.err.is_empty() {
            r.err
        } else if !r.out.is_empty() {
            r.out
        } else {
            format!("exit {}", r.code)
        };
        anyhow::bail!("{}: {why}", cmd.join(" "));
    }
    Ok(r.out)
}

/// `Bun.which(name, { PATH })`: the first executable `name` in `path` (":"-separated).
pub fn which_in(name: &str, path: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return is_exec(Path::new(name)).then(|| PathBuf::from(name));
    }
    path.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join(name)).find(|p| is_exec(p))
}

/// `which` over this process's PATH plus the system directories.
pub fn which(name: &str) -> Option<PathBuf> {
    let p = std::env::var("PATH").unwrap_or_default();
    which_in(name, &format!("{p}:{SYSTEM_PATH}"))
}

fn is_exec(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// A small file's contents, trimmed; "" if it can't be read (core.ts `readText`).
pub fn read_text(path: impl AsRef<Path>) -> String {
    std::fs::read(path).map(|b| String::from_utf8_lossy(&b).trim().to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_program_is_127() {
        let r = run(&["benchbob-no-such-program"], Opts::default()).await;
        assert_eq!(r.code, 127);
        assert!(r.err.ends_with("not installed"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_is_trimmed_and_stdin_passed() {
        let r = run(&["cat"], Opts { stdin: Some(b"  hi\n".to_vec()), ..Default::default() }).await;
        assert_eq!((r.code, r.out.as_str()), (0, "hi"));
        let r = run(&["sleep", "5"], Opts::timeout_ms(100)).await;
        assert_eq!(r.code, -1);
    }
}
