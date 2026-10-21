//! A serialized client for one OpenOCD TCL port (`TclClient` in lib/probes.ts): one command
//! at a time, each terminated by 0x1a both ways.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::logic::{TCL_EOM, tcl_balanced, tcl_unwrap, tcl_wrap};

#[derive(Debug, Clone, PartialEq)]
pub enum TclErr {
    /// OpenOCD ran the command and it failed (TclError).
    Tcl(String),
    /// The connection: refused, closed, timed out.
    Io(String),
}

impl TclErr {
    pub fn message(&self) -> &str {
        match self {
            TclErr::Tcl(m) | TclErr::Io(m) => m,
        }
    }
}

#[derive(Default)]
struct Conn {
    sock: Option<TcpStream>,
    buf: Vec<u8>,
}

pub struct TclClient {
    port: u32,
    conn: Mutex<Conn>,
}

impl TclClient {
    pub fn new(port: u32) -> Self {
        Self { port, conn: Mutex::new(Conn::default()) }
    }

    /// Send a raw Tcl string; the raw reply.
    pub async fn raw(&self, cmd: &str, timeout: Duration) -> Result<String, TclErr> {
        let mut c = self.conn.lock().await;
        if c.sock.is_none() {
            let s = TcpStream::connect(("127.0.0.1", self.port as u16)).await.map_err(|e| TclErr::Io(connect_error(&e, self.port)))?;
            c.sock = Some(s);
            c.buf.clear();
        }
        let mut msg = cmd.as_bytes().to_vec();
        msg.push(TCL_EOM as u8);
        let r = tokio::time::timeout(timeout, async {
            let c = &mut *c;
            let s = c.sock.as_mut().unwrap();
            s.write_all(&msg).await.map_err(|e| TclErr::Io(e.to_string()))?;
            loop {
                if let Some(i) = c.buf.iter().position(|&b| b == TCL_EOM as u8) {
                    let reply = String::from_utf8_lossy(&c.buf[..i]).into_owned();
                    c.buf.drain(..=i);
                    return Ok(reply);
                }
                let mut chunk = [0u8; 4096];
                match s.read(&mut chunk).await {
                    Ok(0) | Err(_) => return Err(TclErr::Io("OpenOCD closed the TCL connection".into())),
                    Ok(n) => c.buf.extend_from_slice(&chunk[..n]),
                }
            }
        })
        .await;
        match r {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(e)) => {
                c.sock = None;
                Err(e)
            }
            Err(_) => {
                // The reply may still arrive later and would be misread as the next answer.
                c.sock = None;
                Err(TclErr::Io(format!("OpenOCD didn't answer within {} s", (timeout.as_millis() as f64 / 1000.0 + 0.5).floor())))
            }
        }
    }

    /// Run an OpenOCD command: its output, or TclErr::Tcl when it failed.
    pub async fn cmd(&self, cmd: &str, capture: bool, timeout: Duration) -> Result<String, TclErr> {
        if !tcl_balanced(cmd) {
            return Err(TclErr::Tcl("Unbalanced braces in command".into()));
        }
        tcl_unwrap(&self.raw(&tcl_wrap(cmd, capture), timeout).await?).map_err(TclErr::Tcl)
    }

    pub async fn close(&self) {
        let mut c = self.conn.lock().await;
        if let Some(mut s) = c.sock.take() {
            let _ = s.shutdown().await;
        }
    }

    /// Close without waiting for a command in flight (it then fails on its own).
    pub fn close_now(&self) {
        if let Ok(mut c) = self.conn.try_lock() {
            c.sock = None;
        }
    }
}

/// Bun.connect's error text for a refused connection.
fn connect_error(e: &std::io::Error, port: u32) -> String {
    if e.kind() == std::io::ErrorKind::ConnectionRefused {
        format!("Failed to connect to 127.0.0.1:{port}: connection refused")
    } else {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::parse_read_memory;
    use tokio::net::TcpListener;

    /// Speaks the 0x1a-terminated protocol: evaluates our wrapper by recognising the inner command.
    #[tokio::test]
    async fn against_a_fake_openocd_tcl_server() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port() as u32;
        let srv = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let re = regex::Regex::new(r"catch \{(?:capture \{)?(.*?)\}?\} __r").unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = match s.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&chunk[..n]);
                while let Some(i) = buf.iter().position(|&b| b == 0x1a) {
                    let msg = String::from_utf8_lossy(&buf[..i]).into_owned();
                    buf.drain(..=i);
                    let inner = re.captures(&msg).map(|m| m[1].to_string()).unwrap_or_default();
                    let reply = if inner == "version" {
                        "@@OK@@Open On-Chip Debugger 0.12.0".to_string()
                    } else if inner.starts_with("read_memory") {
                        "@@OK@@0x41 0x42 0x0".to_string()
                    } else {
                        format!("@@ERR@@invalid command name \"{inner}\"")
                    };
                    s.write_all(format!("{reply}\x1a").as_bytes()).await.unwrap();
                }
            }
        });
        let c = std::sync::Arc::new(TclClient::new(port));
        let t = Duration::from_secs(5);
        let (v, m) = tokio::join!(c.cmd("version", true, t), c.cmd("read_memory 0x0 8 3", false, t));
        assert_eq!(v.unwrap(), "Open On-Chip Debugger 0.12.0");
        assert_eq!(parse_read_memory(&m.unwrap()), [0x41, 0x42, 0]);
        assert_eq!(c.cmd("bogus", true, t).await, Err(TclErr::Tcl("invalid command name \"bogus\"".into())));
        assert_eq!(c.cmd("echo {", true, t).await, Err(TclErr::Tcl("Unbalanced braces in command".into())));
        c.close().await;
        srv.abort();
        // Nothing listening any more.
        let e = TclClient::new(port).cmd("version", true, t).await.unwrap_err();
        assert!(matches!(e, TclErr::Io(_)));
    }
}
