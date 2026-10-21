//! Everything the drive operations do to the system goes through [`Sys`]: commands,
//! streamed processes, downloads, a few file reads, sleeps and the clock. The server uses
//! [`Real`]; the tests use a scripted mock (`mock`) that records every command, which is
//! how the write paths (erase, flash, bootloader cards, settings) are checked against the
//! TypeScript server's without touching a disk.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use benchbob_core::run::{Opts, Output};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// Kills a running process (SIGTERM, as Bun's `proc.kill()`), from anywhere.
pub type Killer = Arc<dyn Fn() + Send + Sync>;

/// A process whose output is read as it comes.
#[async_trait]
pub trait Proc: Send {
    /// The next chunk of stdout; None at EOF.
    async fn stdout(&mut self) -> Option<Vec<u8>>;
    /// The next chunk of stderr; None at EOF.
    async fn stderr(&mut self) -> Option<Vec<u8>>;
    /// The next chunk from either stream (1 = stdout, 2 = stderr); None once both end.
    async fn any(&mut self) -> Option<(u8, Vec<u8>)> {
        if let Some(c) = self.stdout().await {
            return Some((1, c));
        }
        self.stderr().await.map(|c| (2, c))
    }
    fn killer(&self) -> Killer;
    /// Exit code (-1 when killed by a signal).
    async fn wait(&mut self) -> i32;
}

/// Download progress: (bytes so far, Content-Length or 0). Return false to stop.
pub type Progress<'a> = &'a mut (dyn FnMut(u64, u64) -> bool + Send);

#[async_trait]
pub trait Sys: Send + Sync + 'static {
    /// server.ts `run()`: stdout and stderr trimmed, 127 when not installed.
    async fn run(&self, cmd: &[&str]) -> Output;
    /// A command with optional stdin, stdout NOT trimmed (mustRun, `sudo tee`, openssl,
    /// rpi-eeprom-config).
    async fn run_input(&self, cmd: &[&str], stdin: Option<&[u8]>) -> Output;
    /// stdout as bytes.
    async fn run_bytes(&self, cmd: &[&str]) -> Vec<u8>;
    fn spawn(&self, cmd: &[&str]) -> std::io::Result<Box<dyn Proc>>;
    /// Fetch `url` into `path`. Errors read "Download failed: <status>" for an HTTP error.
    async fn download(&self, url: &str, path: &str, progress: Progress<'_>) -> anyhow::Result<()>;
    /// server.ts `readSysFile`: trimmed, "" when unreadable.
    async fn read_text(&self, path: &str) -> String;
    /// A whole file (the work files of a bootloader card, for verifying).
    async fn read_bytes(&self, path: &str) -> Option<Vec<u8>>;
    async fn exists(&self, path: &str) -> bool;
    /// Whether this process can read the file itself (no sudo needed).
    async fn readable(&self, path: &str) -> bool;
    /// `rm -rf`, errors ignored.
    async fn remove_dir(&self, path: &str);
    async fn sleep(&self, ms: u64);
    fn now_ms(&self) -> i64;
}

/// The real system.
pub struct Real;

struct RealProc {
    child: tokio::process::Child,
    out: Option<tokio::process::ChildStdout>,
    err: Option<tokio::process::ChildStderr>,
    pid: Option<u32>,
}

async fn chunk<R: AsyncRead + Unpin>(r: &mut Option<R>) -> Option<Vec<u8>> {
    let s = r.as_mut()?;
    let mut buf = vec![0u8; 64 * 1024];
    match s.read(&mut buf).await {
        Ok(0) | Err(_) => {
            *r = None;
            None
        }
        Ok(n) => {
            buf.truncate(n);
            Some(buf)
        }
    }
}

pub fn kill_pid(pid: Option<u32>) -> Killer {
    Arc::new(move || {
        if let Some(p) = pid {
            // SAFETY: kill(2) on a pid we spawned.
            unsafe {
                libc::kill(p as libc::pid_t, libc::SIGTERM);
            }
        }
    })
}

#[async_trait]
impl Proc for RealProc {
    async fn stdout(&mut self) -> Option<Vec<u8>> {
        chunk(&mut self.out).await
    }
    async fn stderr(&mut self) -> Option<Vec<u8>> {
        chunk(&mut self.err).await
    }
    async fn any(&mut self) -> Option<(u8, Vec<u8>)> {
        loop {
            if self.out.is_none() && self.err.is_none() {
                return None;
            }
            let (has_out, has_err) = (self.out.is_some(), self.err.is_some());
            tokio::select! {
                c = chunk(&mut self.out), if has_out => if let Some(c) = c { return Some((1, c)) },
                c = chunk(&mut self.err), if has_err => if let Some(c) = c { return Some((2, c)) },
            }
        }
    }
    fn killer(&self) -> Killer {
        kill_pid(self.pid)
    }
    async fn wait(&mut self) -> i32 {
        self.out = None;
        self.err = None;
        self.child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1)
    }
}

/// The last HTTP status in a `curl --dump-header` file (redirects come first).
pub fn last_status(headers: &str) -> Option<u16> {
    headers.lines().rfind(|l| l.starts_with("HTTP/")).and_then(|l| l.split_whitespace().nth(1)).and_then(|s| s.parse().ok())
}

/// The Content-Length of the last response in a header dump.
pub fn last_length(headers: &str) -> u64 {
    let block = headers.rsplit("HTTP/").next().unwrap_or("");
    block
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0)
}

/// The curl command for a download to stdout, headers to `headers` (no TLS stack here).
pub fn curl_args(url: &str, headers: &str) -> Vec<String> {
    ["curl", "--silent", "--show-error", "--location", "--dump-header", headers, "--", url].iter().map(|s| s.to_string()).collect()
}

#[async_trait]
impl Sys for Real {
    async fn run(&self, cmd: &[&str]) -> Output {
        benchbob_core::run::run(cmd, Opts::default()).await
    }

    async fn run_input(&self, cmd: &[&str], stdin: Option<&[u8]>) -> Output {
        let Some((prog, args)) = cmd.split_first() else { return Output { code: 127, ..Default::default() } };
        let mut c = tokio::process::Command::new(prog);
        c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        let mut child = match c.spawn() {
            Ok(ch) => ch,
            Err(e) => {
                let why = if e.kind() == std::io::ErrorKind::NotFound { "not installed".to_string() } else { e.to_string() };
                return Output { out: String::new(), err: format!("{prog}: {why}"), code: 127 };
            }
        };
        if let (Some(data), Some(mut si)) = (stdin.map(|d| d.to_vec()), child.stdin.take()) {
            tokio::spawn(async move {
                let _ = si.write_all(&data).await;
            });
        }
        match child.wait_with_output().await {
            Ok(o) => Output {
                out: String::from_utf8_lossy(&o.stdout).into_owned(),
                err: String::from_utf8_lossy(&o.stderr).into_owned(),
                code: o.status.code().unwrap_or(-1),
            },
            Err(e) => Output { out: String::new(), err: e.to_string(), code: -1 },
        }
    }

    async fn run_bytes(&self, cmd: &[&str]) -> Vec<u8> {
        let Some((prog, args)) = cmd.split_first() else { return Vec::new() };
        tokio::process::Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map(|o| o.stdout)
            .unwrap_or_default()
    }

    fn spawn(&self, cmd: &[&str]) -> std::io::Result<Box<dyn Proc>> {
        let (prog, args) = cmd.split_first().ok_or_else(|| std::io::Error::other("empty command"))?;
        let mut child = tokio::process::Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id();
        Ok(Box::new(RealProc { out: child.stdout.take(), err: child.stderr.take(), child, pid }))
    }

    async fn download(&self, url: &str, path: &str, progress: Progress<'_>) -> anyhow::Result<()> {
        let headers = format!("{path}.headers");
        let _ = std::fs::remove_file(&headers);
        let args = curl_args(url, &headers);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut p = self.spawn(&argv)?;
        let mut file: Option<tokio::fs::File> = None;
        let mut total: Option<u64> = None;
        let mut done = 0u64;
        let check = |headers: &str| -> anyhow::Result<u64> {
            let h = std::fs::read_to_string(headers).unwrap_or_default();
            match last_status(&h) {
                Some(s) if !(200..300).contains(&s) => anyhow::bail!("Download failed: {s}"),
                _ => Ok(last_length(&h)),
            }
        };
        let res: anyhow::Result<()> = async {
            while let Some(c) = p.stdout().await {
                let t = match total {
                    Some(t) => t,
                    None => *total.insert(check(&headers)?),
                };
                if file.is_none() {
                    file = Some(tokio::fs::File::create(path).await?);
                }
                file.as_mut().unwrap().write_all(&c).await?;
                done += c.len() as u64;
                if !progress(done, t) {
                    anyhow::bail!("Cancelled");
                }
            }
            Ok(())
        }
        .await;
        if res.is_err() {
            (p.killer())();
        }
        let mut err = String::new();
        while let Some(c) = p.stderr().await {
            err.push_str(&String::from_utf8_lossy(&c));
        }
        let code = p.wait().await;
        // An empty body still has to have been a success.
        let empty = if res.is_ok() && total.is_none() { check(&headers).map(|_| ()) } else { Ok(()) };
        let _ = std::fs::remove_file(&headers);
        res?;
        empty?;
        match file.as_mut() {
            Some(f) => f.flush().await?,
            None if code == 0 => drop(tokio::fs::File::create(path).await?),
            None => {}
        }
        if code != 0 {
            anyhow::bail!("Download failed: {}", err.trim().lines().next_back().unwrap_or(&format!("curl exit {code}")));
        }
        Ok(())
    }

    async fn read_text(&self, path: &str) -> String {
        benchbob_core::run::read_text(path)
    }

    async fn read_bytes(&self, path: &str) -> Option<Vec<u8>> {
        tokio::fs::read(path).await.ok()
    }

    async fn exists(&self, path: &str) -> bool {
        std::path::Path::new(path).is_file()
    }

    async fn readable(&self, path: &str) -> bool {
        tokio::fs::File::open(path).await.is_ok()
    }

    async fn remove_dir(&self, path: &str) {
        let _ = tokio::fs::remove_dir_all(path).await;
    }

    async fn sleep(&self, ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    fn now_ms(&self) -> i64 {
        benchbob_core::timeline::now_ms() as i64
    }
}

#[cfg(test)]
pub mod mock {
    //! A scripted system. Scenarios are JSON shared with the TypeScript harness
    //! (fixtures/harness.ts), so both servers answer the same commands the same way, and
    //! both logs are compared entry by entry.

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use serde_json::{Value, json};

    use super::*;

    /// A canned answer. `match` is the space-joined command, exactly or as a prefix.
    /// `stdout`/`stderr` (chunk lists) make a stream whose reads are recorded.
    #[derive(Clone, Debug, Default)]
    pub struct Answer {
        pub matches: String,
        pub out: String,
        pub out_b64: Option<String>,
        pub err: String,
        pub code: i32,
        pub stdout: Option<Vec<Vec<u8>>>,
        pub stderr: Option<Vec<Vec<u8>>>,
    }

    pub type Snap = Box<dyn Fn() -> Value + Send + Sync>;
    /// What a command leaves behind (files a tool writes), for tests on a real tree.
    pub type Effect = Box<dyn Fn(&[&str]) + Send + Sync>;

    pub struct Mock {
        answers: Vec<Answer>,
        pub files: Value,
        pub log: Mutex<Vec<Value>>,
        /// Snapshot of the operation being run, added to every log entry.
        pub snap: Mutex<Option<Snap>>,
        pub effect: Mutex<Option<Effect>>,
        now: i64,
        fetch: Vec<String>,
        fetch_status: u16,
    }

    fn s(v: &Value, k: &str) -> String {
        v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
    }
    fn list(v: &Value, k: &str) -> Option<Vec<String>> {
        v.get(k).and_then(Value::as_array).map(|a| a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect())
    }

    /// Standard base64, for binary answers.
    pub fn b64(s: &str) -> Vec<u8> {
        let val = |c: u8| match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        };
        let v: Vec<u8> = s.bytes().filter_map(val).collect();
        let mut out = Vec::with_capacity(v.len() * 3 / 4);
        for c in v.chunks(4) {
            let n = c.iter().enumerate().fold(0u32, |a, (i, x)| a | (u32::from(*x) << (18 - 6 * i)));
            out.push((n >> 16) as u8);
            if c.len() > 2 {
                out.push((n >> 8) as u8);
            }
            if c.len() > 3 {
                out.push(n as u8);
            }
        }
        out
    }

    impl Mock {
        /// From a scenario: `{ answers: [{match, out, out_b64, err, code, stdout,
        /// stdout_b64, stderr}], files: {path: text}, now, fetch: [chunks], fetchStatus }`.
        pub fn new(scn: &Value) -> Arc<Self> {
            let answers = scn
                .get("answers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|a| Answer {
                    matches: s(a, "match"),
                    out: s(a, "out"),
                    out_b64: a.get("out_b64").and_then(Value::as_str).map(String::from),
                    err: s(a, "err"),
                    code: a.get("code").and_then(Value::as_i64).unwrap_or(0) as i32,
                    stdout: list(a, "stdout")
                        .map(|l| l.into_iter().map(String::into_bytes).collect())
                        .or_else(|| list(a, "stdout_b64").map(|l| l.iter().map(|c| b64(c)).collect())),
                    stderr: list(a, "stderr").map(|l| l.into_iter().map(String::into_bytes).collect()),
                })
                .collect();
            Arc::new(Self {
                answers,
                files: scn.get("files").cloned().unwrap_or(json!({})),
                log: Mutex::new(Vec::new()),
                snap: Mutex::new(None),
                effect: Mutex::new(None),
                now: scn.get("now").and_then(Value::as_i64).unwrap_or(1_792_490_400_000),
                fetch: list(scn, "fetch").unwrap_or_default(),
                fetch_status: scn.get("fetchStatus").and_then(Value::as_u64).unwrap_or(200) as u16,
            })
        }

        fn answer(&self, cmd: &[&str]) -> Answer {
            if let Some(f) = self.effect.lock().unwrap().as_ref() {
                f(cmd);
            }
            let line = cmd.join(" ");
            self.answers
                .iter()
                .find(|a| a.matches == line)
                .or_else(|| self.answers.iter().find(|a| !a.matches.is_empty() && line.starts_with(&a.matches)))
                .cloned()
                .unwrap_or_default()
        }

        pub fn record(&self, mut e: Value) {
            if let Some(f) = self.snap.lock().unwrap().as_ref() {
                e["op"] = f();
            }
            self.log.lock().unwrap().push(e);
        }

        pub fn entries(&self) -> Vec<Value> {
            self.log.lock().unwrap().clone()
        }
    }

    struct MockProc {
        out: VecDeque<Vec<u8>>,
        err: VecDeque<Vec<u8>>,
        /// Whether reads of each stream are recorded (it was scripted chunk by chunk).
        rec: (bool, bool),
        n: (usize, usize),
        code: i32,
        mock: Arc<Mock>,
        killed: Arc<Mutex<bool>>,
    }

    #[async_trait]
    impl Proc for MockProc {
        async fn stdout(&mut self) -> Option<Vec<u8>> {
            self.next(true)
        }
        async fn stderr(&mut self) -> Option<Vec<u8>> {
            self.next(false)
        }
        fn killer(&self) -> Killer {
            let (k, m) = (self.killed.clone(), self.mock.clone());
            Arc::new(move || {
                *k.lock().unwrap() = true;
                m.record(json!({ "kill": true }));
            })
        }
        async fn wait(&mut self) -> i32 {
            if *self.killed.lock().unwrap() { -1 } else { self.code }
        }
    }

    impl MockProc {
        fn next(&mut self, out: bool) -> Option<Vec<u8>> {
            if *self.killed.lock().unwrap() {
                return None;
            }
            let (q, rec, n, fd) =
                if out { (&mut self.out, self.rec.0, &mut self.n.0, 1) } else { (&mut self.err, self.rec.1, &mut self.n.1, 2) };
            if rec {
                // The state after the previous chunk, as the TypeScript harness records it.
                self.mock.record(json!({ "read": *n, "fd": fd }));
            }
            *n += 1;
            q.pop_front()
        }
    }

    pub struct Shared(pub Arc<Mock>);

    #[async_trait]
    impl Sys for Shared {
        async fn run(&self, cmd: &[&str]) -> Output {
            self.0.record(json!({ "cmd": cmd }));
            let a = self.0.answer(cmd);
            Output { out: a.out.trim().to_string(), err: a.err.trim().to_string(), code: a.code }
        }
        async fn run_input(&self, cmd: &[&str], stdin: Option<&[u8]>) -> Output {
            let mut e = json!({ "cmd": cmd });
            if let Some(i) = stdin {
                e["stdin"] = String::from_utf8_lossy(i).into();
            }
            self.0.record(e);
            let a = self.0.answer(cmd);
            Output { out: a.out, err: a.err, code: a.code }
        }
        async fn run_bytes(&self, cmd: &[&str]) -> Vec<u8> {
            self.0.record(json!({ "cmd": cmd }));
            let a = self.0.answer(cmd);
            a.out_b64.map(|x| b64(&x)).unwrap_or_else(|| a.out.into_bytes())
        }
        fn spawn(&self, cmd: &[&str]) -> std::io::Result<Box<dyn Proc>> {
            self.0.record(json!({ "cmd": cmd }));
            let a = self.0.answer(cmd);
            let plain = |s: String| if s.is_empty() { vec![] } else { vec![s.into_bytes()] };
            let rec = (a.stdout.is_some(), a.stderr.is_some());
            let out = a.stdout.unwrap_or_else(|| a.out_b64.as_deref().map(|x| vec![b64(x)]).unwrap_or_else(|| plain(a.out)));
            Ok(Box::new(MockProc {
                out: out.into(),
                err: a.stderr.unwrap_or_else(|| plain(a.err)).into(),
                rec,
                n: (0, 0),
                code: a.code,
                mock: self.0.clone(),
                killed: Default::default(),
            }))
        }
        async fn download(&self, url: &str, path: &str, progress: Progress<'_>) -> anyhow::Result<()> {
            // server.ts only opens the file once the server said yes.
            if self.0.fetch_status != 200 {
                self.0.record(json!({ "fetch": url }));
                anyhow::bail!("Download failed: {}", self.0.fetch_status);
            }
            self.0.record(json!({ "fetch": url, "to": path }));
            let total: u64 = self.0.fetch.iter().map(|c| c.len() as u64).sum();
            let mut done = 0;
            for c in &self.0.fetch {
                done += c.len() as u64;
                if !progress(done, total) {
                    anyhow::bail!("Cancelled");
                }
            }
            Ok(())
        }
        async fn read_text(&self, path: &str) -> String {
            match self.0.files.get(path) {
                Some(Value::String(s)) => s.trim().to_string(),
                Some(v) => benchbob_core::http::js_string(v).trim().to_string(),
                None => String::new(),
            }
        }
        async fn read_bytes(&self, path: &str) -> Option<Vec<u8>> {
            Some(self.0.files.get(path).and_then(Value::as_str).unwrap_or("").as_bytes().to_vec())
        }
        async fn exists(&self, path: &str) -> bool {
            self.0.record(json!({ "exists": path }));
            self.0.files.get(path).is_some()
        }
        async fn readable(&self, path: &str) -> bool {
            self.0.record(json!({ "access": path }));
            false
        }
        async fn remove_dir(&self, path: &str) {
            self.0.record(json!({ "rm": path }));
        }
        async fn sleep(&self, ms: u64) {
            self.0.record(json!({ "sleep": ms }));
        }
        fn now_ms(&self) -> i64 {
            self.0.now
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_dumps() {
        let h = "HTTP/1.1 302 Found\r\nLocation: x\r\nContent-Length: 5\r\n\r\nHTTP/2 200\r\ncontent-length: 1234\r\n\r\n";
        assert_eq!(last_status(h), Some(200));
        assert_eq!(last_length(h), 1234);
        assert_eq!(last_status("HTTP/2 404 \r\n"), Some(404));
    }
}
