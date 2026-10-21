//! A long-running child whose output is read line by line (Bun's `ctx.spawn` with piped
//! stdout/stderr plus a `for await` reader, and `proc.exited`): the tools the camera, probes,
//! SDR, GPS and UPS panels start on demand (ustreamer, OpenOCD, rtl_tcp, gpsd, NUT).
//!
//! The child is registered with the driver's [`Ctx`], so it is killed when the driver stops.
//! [`Proc::wait`] resolves once it has exited *and* its output has been read to the end, so
//! an exit handler sees the last lines it printed.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::watch;

use crate::driver::Ctx;

/// Called with every line (stdout and stderr alike), without its line ending.
pub type LineFn = Arc<dyn Fn(&str) + Send + Sync>;

pub struct Proc {
    pid: Option<u32>,
    exit: watch::Receiver<Option<i32>>,
}

/// How lines are split.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Split {
    /// On "\n" (a trailing "\r" is kept, as `buf.indexOf("\n")` keeps it).
    Lf,
    /// On "\r" or "\n", empty lines dropped: progress bars redrawn with "\r" become lines.
    CrLf,
}

impl Proc {
    /// Spawn `cmd` with stdin closed, stdout and stderr piped, and feed their lines to
    /// `on_line`.
    pub fn spawn(ctx: &Ctx, cmd: &mut Command, split: Split, on_line: LineFn) -> std::io::Result<Arc<Proc>> {
        Self::spawn_with(ctx, cmd, split, on_line, true)
    }

    /// `spawn`, with stdout discarded unless `stdout` (stderr is always read).
    pub fn spawn_with(ctx: &Ctx, cmd: &mut Command, split: Split, on_line: LineFn, stdout: bool) -> std::io::Result<Arc<Proc>> {
        cmd.stdin(Stdio::null()).stderr(Stdio::piped());
        cmd.stdout(if stdout { Stdio::piped() } else { Stdio::null() });
        let mut child = ctx.spawn(cmd)?;
        let pid = child.child.id();
        let mut readers = Vec::new();
        if let Some(out) = child.child.stdout.take() {
            readers.push(tokio::spawn(read_lines(out, split, on_line.clone())));
        }
        if let Some(err) = child.child.stderr.take() {
            readers.push(tokio::spawn(read_lines(err, split, on_line)));
        }
        let (tx, rx) = watch::channel(None);
        tokio::spawn(async move {
            let code = match child.child.wait().await {
                Ok(s) => exit_code(s),
                Err(_) => -1,
            };
            // A grandchild may still hold the pipes open; don't wait on it for long.
            for r in readers {
                let _ = tokio::time::timeout(Duration::from_secs(1), r).await;
            }
            let _ = tx.send(Some(code));
            drop(child);
        });
        Ok(Arc::new(Proc { pid, exit: rx }))
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The exit code once it has exited (128 + signal when a signal ended it).
    pub fn exited(&self) -> Option<i32> {
        *self.exit.borrow()
    }

    /// Wait for the exit (and the end of its output).
    pub async fn wait(&self) -> i32 {
        let mut rx = self.exit.clone();
        loop {
            if let Some(c) = *rx.borrow_and_update() {
                return c;
            }
            if rx.changed().await.is_err() {
                return (*rx.borrow()).unwrap_or(-1);
            }
        }
    }

    /// `wait()` with a limit; None on timeout.
    pub async fn wait_for(&self, d: Duration) -> Option<i32> {
        tokio::time::timeout(d, self.wait()).await.ok()
    }

    /// Send a signal (SIGTERM, SIGINT, SIGKILL…) unless it has already exited.
    pub fn kill(&self, sig: i32) {
        if self.exited().is_some() {
            return;
        }
        if let Some(p) = self.pid {
            #[cfg(unix)]
            // SAFETY: kill(2) on a child we spawned and whose exit we haven't seen.
            unsafe {
                libc::kill(p as libc::pid_t, sig);
            }
            #[cfg(not(unix))]
            let _ = (p, sig);
        }
    }

    /// SIGTERM, then SIGKILL after `grace`; resolves once it has exited.
    pub async fn terminate(&self, grace: Duration) -> i32 {
        self.kill(SIGTERM);
        if let Some(c) = self.wait_for(grace).await {
            return c;
        }
        self.kill(SIGKILL);
        self.wait().await
    }
}

#[cfg(unix)]
pub const SIGINT: i32 = libc::SIGINT;
#[cfg(unix)]
pub const SIGTERM: i32 = libc::SIGTERM;
#[cfg(unix)]
pub const SIGKILL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
pub const SIGINT: i32 = 2;
#[cfg(not(unix))]
pub const SIGTERM: i32 = 15;
#[cfg(not(unix))]
pub const SIGKILL: i32 = 9;

fn exit_code(s: std::process::ExitStatus) -> i32 {
    if let Some(c) = s.code() {
        return c;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = s.signal() {
            return 128 + sig;
        }
    }
    -1
}

async fn read_lines(mut r: impl AsyncRead + Unpin, split: Split, on_line: LineFn) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 8192];
    loop {
        let n = match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        let mut start = 0;
        for i in 0..buf.len() {
            let c = buf[i];
            let end = match split {
                Split::Lf => c == b'\n',
                Split::CrLf => c == b'\n' || c == b'\r',
            };
            if end {
                emit(&buf[start..i], split, &on_line);
                start = i + 1;
            }
        }
        buf.drain(..start);
    }
    if !String::from_utf8_lossy(&buf).trim().is_empty() {
        emit(&buf, split, &on_line);
    }
}

fn emit(line: &[u8], split: Split, on_line: &LineFn) {
    let s = String::from_utf8_lossy(line);
    if split == Split::CrLf && s.trim().is_empty() {
        return;
    }
    on_line(&s);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::driver::Hub;
    use crate::settings::Settings;
    use std::sync::Mutex;

    #[tokio::test]
    async fn lines_then_exit() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new("t", Arc::new(Hub::new(Settings::load(dir.path().join("p.json")))));
        let got = Arc::new(Mutex::new(Vec::<String>::new()));
        let g = got.clone();
        let p = Proc::spawn(
            &ctx,
            Command::new("sh").args(["-c", "printf 'a\\nb\\rc\\n'; printf 'err' >&2; exit 3"]),
            Split::CrLf,
            Arc::new(move |l| g.lock().unwrap().push(l.to_string())),
        )
        .unwrap();
        assert_eq!(p.wait().await, 3);
        let mut v = got.lock().unwrap().clone();
        v.sort();
        assert_eq!(v, ["a", "b", "c", "err"]);
        let p = Proc::spawn(&ctx, Command::new("sleep").arg("10"), Split::Lf, Arc::new(|_| {})).unwrap();
        assert_eq!(p.terminate(Duration::from_secs(2)).await, 128 + SIGTERM);
    }
}
