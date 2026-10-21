//! How every child process this server starts is ended when nobody waits for it any more.
//!
//! tokio's `kill_on_drop` sends SIGKILL. For a `sudo -n prog` child that kills sudo only:
//! sudo passes SIGTERM (and SIGINT, SIGHUP…) on to the program it runs, but nothing can pass
//! on SIGKILL, so `prog` would keep running as root, orphaned. That happened whenever a
//! handle was dropped early: an aborted task holding it, or [`run`](crate::run::run) hitting
//! its timeout (a hung `sudo uhubctl`, say).
//!
//! A [`Guarded`] child, dropped while it still runs, gets SIGTERM instead, then SIGKILL if it
//! is still there after a grace period ([`GRACE`]). sudo relays the SIGTERM and exits once
//! its program has; a program that ignores SIGTERM ends with the SIGKILL (only possible
//! without sudo in between). The wait happens in the background, so dropping never blocks.
//!
//! Spawn with [`CommandExt::spawn_guarded`] (or [`Ctx::spawn`](crate::Ctx::spawn), which
//! uses it) rather than `kill_on_drop(true).spawn()`.

use std::io;
use std::ops::{Deref, DerefMut};
use std::process::{ExitStatus, Output};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

/// How long a dropped child gets between SIGTERM and SIGKILL.
pub const GRACE: Duration = Duration::from_secs(3);

/// A child process that is ended gracefully when dropped (see the module docs). Derefs to
/// tokio's [`Child`] for everything else.
#[derive(Debug)]
pub struct Guarded {
    child: Option<Child>,
    grace: Duration,
}

/// `spawn_guarded()` on tokio's [`Command`].
pub trait CommandExt {
    /// Spawn the command as a [`Guarded`] child. `kill_on_drop` is switched off: the guard
    /// does the ending.
    fn spawn_guarded(&mut self) -> io::Result<Guarded>;
}

impl CommandExt for Command {
    fn spawn_guarded(&mut self) -> io::Result<Guarded> {
        let child = self.kill_on_drop(false).spawn()?;
        Ok(Guarded { child: Some(child), grace: GRACE })
    }
}

impl Guarded {
    /// Another grace period between SIGTERM and SIGKILL when dropped.
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    fn inner(&mut self) -> &mut Child {
        self.child.as_mut().expect("child taken")
    }

    /// Send `sig` unless it has already exited (and been waited for).
    pub fn signal(&mut self, sig: i32) {
        let c = self.inner();
        if matches!(c.try_wait(), Ok(Some(_))) {
            return;
        }
        if let Some(pid) = c.id() {
            send(pid, sig);
        }
    }

    /// SIGTERM, then SIGKILL after `grace`; resolves with the exit status.
    pub async fn terminate(&mut self, grace: Duration) -> io::Result<ExitStatus> {
        self.signal(libc::SIGTERM);
        let c = self.inner();
        match tokio::time::timeout(grace, c.wait()).await {
            Ok(r) => r,
            Err(_) => {
                let _ = c.start_kill();
                c.wait().await
            }
        }
    }

    /// Wait for the exit, collecting whatever is left of stdout and stderr (tokio's
    /// `wait_with_output`, but by reference: when this future is dropped, by a timeout say,
    /// the guard still owns the child and ends it).
    pub async fn wait_with_output(&mut self) -> io::Result<Output> {
        let c = self.inner();
        let (out, err) = (c.stdout.take(), c.stderr.take());
        let (status, stdout, stderr) = tokio::try_join!(c.wait(), read_all(out), read_all(err))?;
        Ok(Output { status, stdout, stderr })
    }
}

async fn read_all(r: Option<impl AsyncRead + Unpin>) -> io::Result<Vec<u8>> {
    let mut v = Vec::new();
    if let Some(mut r) = r {
        r.read_to_end(&mut v).await?;
    }
    Ok(v)
}

impl Deref for Guarded {
    type Target = Child;
    fn deref(&self) -> &Child {
        self.child.as_ref().expect("child taken")
    }
}

impl DerefMut for Guarded {
    fn deref_mut(&mut self) -> &mut Child {
        self.inner()
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        if let Some(c) = self.child.take() {
            end(c, self.grace);
        }
    }
}

/// End a child nobody waits for any more: SIGTERM now, SIGKILL after `grace` if it is still
/// running, and reap it either way. Returns at once.
pub fn end(mut c: Child, grace: Duration) {
    if matches!(c.try_wait(), Ok(Some(_))) {
        return;
    }
    let Some(pid) = c.id() else { return };
    send(pid, libc::SIGTERM);
    match tokio::runtime::Handle::try_current() {
        Ok(h) => {
            h.spawn(async move {
                if tokio::time::timeout(grace, c.wait()).await.is_err() {
                    let _ = c.start_kill();
                    let _ = c.wait().await;
                }
            });
        }
        // No runtime (shutting down, or a plain thread): poll from a thread of our own.
        Err(_) => {
            std::thread::spawn(move || {
                let t0 = std::time::Instant::now();
                while t0.elapsed() < grace {
                    if !matches!(c.try_wait(), Ok(None)) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = c.start_kill();
            });
        }
    }
}

/// [`end`] for a blocking `std::process::Child`, waiting for it here: SIGTERM, up to
/// `grace` for it to exit, then SIGKILL.
pub fn end_blocking(c: &mut std::process::Child, grace: Duration) {
    if matches!(c.try_wait(), Ok(Some(_))) {
        return;
    }
    send(c.id(), libc::SIGTERM);
    let t0 = std::time::Instant::now();
    while t0.elapsed() < grace {
        if matches!(c.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = c.kill();
    let _ = c.wait();
}

fn send(pid: u32, sig: i32) {
    // SAFETY: kill(2) on a child we spawned and have not reaped (try_wait said it runs, or
    // it is a zombie we still hold), so the pid can't belong to anyone else yet.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::io::AsyncBufReadExt;

    /// The pid a `sh -c 'echo $$; exec …'` child printed first.
    async fn first_pid(g: &mut Guarded) -> i32 {
        let out = g.stdout.take().unwrap();
        let mut l = tokio::io::BufReader::new(out).lines();
        let line = tokio::time::timeout(Duration::from_secs(10), l.next_line()).await.unwrap().unwrap().unwrap();
        // Keep the pipe open so the program doesn't die of SIGPIPE: leak the reader.
        std::mem::forget(l);
        line.trim().parse().unwrap()
    }

    /// Whether a process exists (whoever owns it: EPERM means a root process is there).
    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks for existence.
        let r = unsafe { libc::kill(pid, 0) };
        if r == 0 {
            // A zombie isn't running any more.
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            return !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z');
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    async fn gone_within(pid: i32, d: Duration) -> bool {
        let t0 = std::time::Instant::now();
        while t0.elapsed() < d {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        !alive(pid)
    }

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        c
    }

    #[tokio::test]
    async fn dropped_child_gets_sigterm() {
        let mut g = sh("echo $$; exec sleep 600").spawn_guarded().unwrap();
        let pid = first_pid(&mut g).await;
        assert!(alive(pid));
        drop(g);
        assert!(gone_within(pid, Duration::from_secs(1)).await, "SIGTERM should end sleep at once");
    }

    #[tokio::test]
    async fn sigterm_ignored_then_sigkill() {
        let mut g = sh("trap '' TERM; echo $$; exec sleep 600").spawn_guarded().unwrap().with_grace(Duration::from_millis(500));
        let pid = first_pid(&mut g).await;
        drop(g);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(alive(pid), "still in its grace period");
        assert!(gone_within(pid, Duration::from_secs(3)).await, "SIGKILL after the grace period");
    }

    #[tokio::test]
    async fn exited_child_is_left_alone() {
        let mut g = sh("exit 3").spawn_guarded().unwrap();
        assert_eq!(g.wait().await.unwrap().code(), Some(3));
        drop(g);
        let mut g = sh("echo hi; echo err >&2").stderr(Stdio::piped()).spawn_guarded().unwrap();
        let o = g.wait_with_output().await.unwrap();
        assert_eq!((o.stdout.as_slice(), o.stderr.as_slice(), o.status.code()), (&b"hi\n"[..], &b"err\n"[..], Some(0)));
    }

    #[tokio::test]
    async fn timed_out_run_ends_the_program() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("pid");
        let script = format!("echo $$ > {}; exec sleep 600", f.display());
        let r = crate::run::run(&["sh", "-c", &script], crate::run::Opts::timeout_ms(500)).await;
        assert_eq!(r.code, -1);
        let pid: i32 = std::fs::read_to_string(&f).unwrap().trim().parse().unwrap();
        assert!(gone_within(pid, Duration::from_secs(2)).await);
    }

    /// The bug this module exists for: a `sudo` child whose handle is dropped must not leave
    /// its program running as root. Needs passwordless sudo (CI runners have it); skipped
    /// without it. sudo runs in this process's group, as under the service. (Under systemd
    /// there's no terminal, so sudo runs the program directly; from a terminal with
    /// `use_pty` it runs it on a pty of its own. Both relay SIGTERM.)
    #[tokio::test]
    async fn sudo_child_dies_with_its_handle() {
        let ok = std::process::Command::new("sudo").args(["-n", "true"]).stderr(Stdio::null()).status();
        if !ok.map(|s| s.success()).unwrap_or(false) {
            eprintln!("skipped: no passwordless sudo");
            return;
        }
        for (what, via_run) in [("handle dropped", false), ("run() timeout", true)] {
            let dir = tempfile::tempdir().unwrap();
            let f = dir.path().join("pid");
            // The root program records its pid, then sleeps; sudo stays its parent.
            let script = format!("echo $$ > {}; chmod 0644 {0}; exec sleep 600", f.display());
            if via_run {
                let r = crate::run::run(&["sudo", "-n", "sh", "-c", &script], crate::run::Opts::timeout_ms(1500)).await;
                assert_eq!(r.code, -1, "{what}");
            } else {
                let mut c = Command::new("sudo");
                c.args(["-n", "sh", "-c", &script]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
                let g = c.spawn_guarded().unwrap();
                let t0 = std::time::Instant::now();
                while !f.exists() && t0.elapsed() < Duration::from_secs(10) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                drop(g);
            }
            let pid: i32 = std::fs::read_to_string(&f).unwrap().trim().parse().unwrap();
            let root = std::fs::metadata(format!("/proc/{pid}")).map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap_or(0);
            assert_eq!(root, 0, "{what}: the program runs as root");
            let gone = gone_within(pid, GRACE + Duration::from_secs(2)).await;
            if !gone {
                // Don't leave it behind for the next test run.
                let _ = std::process::Command::new("sudo").args(["-n", "kill", "-9", &pid.to_string()]).status();
            }
            assert!(gone, "{what}: the root program (pid {pid}) outlived its sudo handle");
        }
    }
}
