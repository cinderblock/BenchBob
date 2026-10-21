//! Flash jobs (serial.ts "Flashing"): fetch the firmware, release the port, run esptool or
//! avrdude, stream its progress, then reopen the port.

use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use benchbob_core::HttpError;
use benchbob_core::http::{js_number, js_string, num, parse_int, truthy};
use benchbob_core::run::{Opts, SYSTEM_PATH, run};
use benchbob_core::timeline::now_ms;
use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt;

use crate::Inner;
use crate::flash::{FlashFile, FlashOpts, FlashProgress, flash_command, hex};
use crate::port::{Port, PortState, ensure_present, ensure_writable, exists, http};
use crate::util::{Utf8Stream, base36, base64_decode, js_round, multipart};

pub const FLASH_DIR: &str = "/var/tmp/benchbob-serial";

pub struct FlashJob {
    pub id: String,
    pub tool: String,
    pub action: String,
    pub label: String,
    /// preparing | running | done | error | cancelled
    pub state: &'static str,
    pub phase: String,
    pub progress: f64,
    pub info: Map<String, Value>,
    pub message: String,
    pub log: Vec<String>,
    pub started: u64,
    pub ended: Option<u64>,
    pub cmd: Vec<String>,
    pub pid: Option<u32>,
}

/// A firmware file from a request: bytes, or a URL to fetch.
#[derive(Debug)]
pub struct Source {
    pub offset: u64,
    pub name: String,
    pub data: Option<Vec<u8>>,
    pub url: Option<String>,
}

#[derive(Debug)]
pub struct FlashRequest {
    pub opts: FlashOpts,
    pub sources: Vec<Source>,
}

/// The job as the API shows it: no process, the last 12 log lines.
pub fn job_view(j: Option<&FlashJob>) -> Value {
    let Some(j) = j else { return Value::Null };
    let mut v = json!({
        "id": j.id,
        "tool": j.tool,
        "action": j.action,
        "label": j.label,
        "state": j.state,
        "phase": j.phase,
        "progress": num(js_round(j.progress)),
        "info": j.info,
        "message": j.message,
        "started": j.started,
        "cmd": j.cmd,
    });
    if let Some(e) = j.ended {
        v["ended"] = json!(e);
    }
    v["logTail"] = json!(j.log[j.log.len().saturating_sub(12)..]);
    v
}

pub fn parse_offset(v: Option<&Value>, dflt: u64) -> Result<u64, HttpError> {
    let Some(v) = v.filter(|v| !v.is_null() && v.as_str() != Some("")) else { return Ok(dflt) };
    let n = match v {
        Value::Number(_) => js_number(v),
        _ => {
            let s = js_string(v);
            let t = s.trim_start();
            if t.len() >= 2 && t[..2].eq_ignore_ascii_case("0x") {
                let h: String = t[2..].chars().take_while(|c| c.is_ascii_hexdigit()).collect();
                u64::from_str_radix(&h, 16).map(|n| n as f64).unwrap_or(f64::NAN)
            } else {
                parse_int(&s).map(|n| n as f64).unwrap_or(f64::NAN)
            }
        }
    };
    if !(n.fract() == 0.0 && n >= 0.0) {
        return Err(HttpError::bad_request(format!("Bad offset \"{}\"", js_string(v))));
    }
    Ok(n as u64)
}

fn url_name(url: &str) -> String {
    url.rsplit('/').next().unwrap_or("").split('?').next().unwrap_or("").to_string()
}

/// `{ offset, url | base64, name }` entries (JSON body or the MCP tool).
pub fn json_sources(files: Option<&Value>) -> Result<Vec<Source>, HttpError> {
    let mut out = Vec::new();
    for f in files.and_then(Value::as_array).into_iter().flatten() {
        let url = f.get("url").filter(|v| truthy(v)).map(js_string);
        let b64 = f.get("base64").filter(|v| truthy(v)).map(js_string);
        if url.is_none() && b64.is_none() {
            return Err(HttpError::bad_request("Each file needs url or base64"));
        }
        let name = match f.get("name").filter(|v| truthy(v)) {
            Some(n) => js_string(n),
            None => url.as_deref().map(url_name).unwrap_or_else(|| "firmware.bin".into()),
        };
        out.push(Source { offset: parse_offset(f.get("offset"), 0)?, name, data: b64.map(|b| base64_decode(&b)), url });
    }
    Ok(out)
}

/// Firmware sources from a request: multipart, raw body + query, or JSON {files:[…]}.
pub fn flash_request(content_type: &str, query: &[(String, String)], body: &[u8]) -> Result<FlashRequest, HttpError> {
    let mut o = Map::new();
    for (k, v) in query {
        o.insert(k.clone(), Value::String(v.clone()));
    }
    let mut sources = Vec::new();
    if content_type.starts_with("multipart/form-data") {
        let parts = multipart(content_type, body).ok_or_else(|| HttpError::bad_request("Body is not valid multipart/form-data"))?;
        for p in parts.iter().filter(|p| p.filename.is_none() && p.name != "offset") {
            o.insert(p.name.clone(), Value::String(String::from_utf8_lossy(&p.data).into_owned()));
        }
        let offsets: Vec<String> = parts
            .iter()
            .filter(|p| p.filename.is_none() && p.name == "offset")
            .map(|p| String::from_utf8_lossy(&p.data).into_owned())
            .collect();
        let mut i = 0;
        for p in parts.iter().filter(|p| p.filename.is_some()) {
            // A file field named like an offset ("0x10000") carries its own; otherwise
            // offsets[] pair up in order.
            let named = {
                let k = &p.name;
                (k.len() > 2 && k[..2].eq_ignore_ascii_case("0x") && k[2..].bytes().all(|c| c.is_ascii_hexdigit()))
                    || (!k.is_empty() && k.bytes().all(|c| c.is_ascii_digit()))
            };
            let off = if named {
                parse_offset(Some(&Value::String(p.name.clone())), 0)?
            } else {
                let v = offsets.get(i).map(|s| Value::String(s.clone()));
                i += 1;
                parse_offset(v.as_ref(), 0)?
            };
            let name = p.filename.clone().filter(|f| !f.is_empty()).unwrap_or_else(|| p.name.clone());
            sources.push(Source { offset: off, name, data: Some(p.data.clone()), url: None });
        }
    } else if content_type.contains("application/json") {
        let b: Value = if body.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(body).map_err(|_| HttpError::bad_request("Body is not valid JSON"))?
        };
        if let Value::Object(m) = &b {
            for (k, v) in m {
                o.insert(k.clone(), v.clone());
            }
        }
        sources = json_sources(o.get("files"))?;
    } else if !body.is_empty() {
        let q = |k: &str| query.iter().rev().find(|(x, _)| x == k).map(|(_, v)| v.clone());
        let name = q("name").filter(|n| !n.is_empty()).unwrap_or_else(|| "firmware.bin".into());
        sources.push(Source {
            offset: parse_offset(q("offset").map(Value::String).as_ref(), 0)?,
            name,
            data: Some(body.to_vec()),
            url: None,
        });
    }
    let s = |k: &str| o.get(k).filter(|v| truthy(v)).map(js_string);
    let opts = FlashOpts {
        tool: s("tool").unwrap_or_else(|| "esptool".into()),
        action: s("action").unwrap_or_else(|| if sources.is_empty() { "info".into() } else { "write".into() }),
        files: vec![],
        baud: o.get("baud").filter(|v| truthy(v)).map(js_number),
        chip: o.get("chip").filter(|v| !v.is_null()).map(js_string),
        preset: o.get("preset").filter(|v| !v.is_null()).map(js_string),
    };
    Ok(FlashRequest { opts, sources })
}

fn info_summary(j: &FlashJob) -> String {
    let g = |k: &str| j.info.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    let parts: Vec<String> = [
        g("chip").or_else(|| g("part")),
        g("mac").map(|m| format!("MAC {m}")),
        g("flashSize").map(|f| format!("{f} flash")),
        g("signature").map(|s| format!("signature {s}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() { "Done".into() } else { parts.join(" · ") }
}

impl Inner {
    fn flash_msg(&self, st: &PortState, line: Option<&str>) {
        let mut m = json!({ "type": "flash", "job": job_view(st.flash.as_ref()) });
        if let Some(l) = line {
            m["line"] = json!(l);
        }
        let m = m.to_string();
        for w in st.ws.iter().filter(|w| !w.quiet) {
            w.client.send(m.clone());
        }
    }

    pub(crate) fn start_flash(self: &Arc<Self>, p: &Arc<Port>, req: FlashRequest) -> Result<Value, HttpError> {
        let mut st = p.lock();
        ensure_present(&st)?;
        ensure_writable(&st)?;
        if st.flash.as_ref().is_some_and(|j| j.state == "running" || j.state == "preparing") {
            return Err(http(409, "Already flashing"));
        }
        let o = req.opts;
        if !["write", "erase", "info"].contains(&o.action.as_str()) {
            return Err(HttpError::bad_request("action must be write, erase or info"));
        }
        let srcs = if o.action == "write" { req.sources } else { vec![] };
        if o.action == "write" && srcs.is_empty() {
            return Err(HttpError::bad_request("No firmware: upload a file or give files[] with url or base64"));
        }
        let id = format!("{}-{}", base36(now_ms()), self.job_seq.fetch_add(1, Ordering::SeqCst) + 1);
        let dir = format!("{FLASH_DIR}/{id}");
        let files: Vec<FlashFile> = srcs
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let base = if s.name.is_empty() { "fw.bin".to_string() } else { s.name.clone() };
                let safe: String = base.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
                FlashFile {
                    offset: s.offset,
                    name: if s.name.is_empty() { format!("fw{i}.bin") } else { s.name.clone() },
                    path: format!("{dir}/{i}-{safe}"),
                }
            })
            .collect();
        // Validate the command now so a bad preset/chip is a 400, not a failed job.
        let cmd = flash_command(&st.info.path, &FlashOpts { files: files.clone(), ..o.clone() })?;
        let label = if o.action == "write" {
            files.iter().map(|f| format!("{}@{}", f.name, hex(f.offset))).collect::<Vec<_>>().join(", ")
        } else {
            o.action.clone()
        };
        st.flash = Some(FlashJob {
            id,
            tool: o.tool.clone(),
            action: o.action.clone(),
            label,
            state: "preparing",
            phase: "preparing".into(),
            progress: 0.0,
            info: Map::new(),
            message: String::new(),
            log: vec![],
            started: now_ms(),
            ended: None,
            cmd,
            pid: None,
        });
        self.notify(p, &st);
        let view = job_view(st.flash.as_ref());
        drop(st);
        let (me, pp) = (self.clone(), p.clone());
        tokio::spawn(async move {
            let r = me.run_flash(&pp, srcs, files, &dir).await;
            let _ = std::fs::remove_dir_all(&dir);
            let mut st = pp.lock();
            if st.state == "flashing" {
                st.state = "closed";
            }
            if let Err(e) = r {
                me.finish_job(&pp, &mut st, "error", &e);
            }
        });
        Ok(view)
    }

    fn finish_job(&self, p: &Port, st: &mut PortState, state: &'static str, message: &str) {
        let Some(job) = st.flash.as_mut() else { return };
        if job.ended.is_some() {
            return;
        }
        job.state = state;
        job.message = message.into();
        job.ended = Some(now_ms());
        if state == "done" {
            job.progress = 100.0;
        }
        job.pid = None;
        let text = format!("Serial: {} {} on {}: {}", job.tool, job.action, st.info.name, if state == "done" { "done" } else { message });
        self.event(&text, Some(json!({ "id": p.id })));
        self.flash_msg(st, None);
        self.notify(p, st);
    }

    async fn run_flash(self: &Arc<Self>, p: &Arc<Port>, srcs: Vec<Source>, files: Vec<FlashFile>, dir: &str) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
        for (s, f) in srcs.iter().zip(&files) {
            let data = match (&s.data, &s.url) {
                (Some(d), _) => d.clone(),
                (None, Some(url)) => {
                    {
                        let mut st = p.lock();
                        if let Some(j) = st.flash.as_mut() {
                            j.phase = format!("downloading {}", f.name);
                        }
                        self.flash_msg(&st, None);
                    }
                    let tmp = format!("{}.download", f.path);
                    let r = run(&["curl", "-sSL", "-o", &tmp, "-w", "%{http_code}", "--", url], Opts::default()).await;
                    if r.code != 0 {
                        return Err(format!("Download {url}: {}", if r.err.is_empty() { format!("curl exit {}", r.code) } else { r.err }));
                    }
                    if !r.out.starts_with('2') {
                        return Err(format!("Download {url}: HTTP {}", r.out));
                    }
                    std::fs::read(&tmp).map_err(|e| e.to_string())?
                }
                (None, None) => vec![],
            };
            if data.is_empty() {
                return Err(format!("{} is empty", f.name));
            }
            std::fs::write(&f.path, &data).map_err(|e| format!("{}: {e}", f.path))?;
        }
        let (cmd, tool, action, reopen, close) = {
            let mut st = p.lock();
            let job = st.flash.as_ref().ok_or("job gone")?;
            if job.state == "cancelled" {
                self.finish_job(p, &mut st, "cancelled", "Cancelled");
                return Ok(());
            }
            let (cmd, tool, action) = (job.cmd.clone(), job.tool.clone(), job.action.clone());
            // Release the port so the tool can open it; bridges stay up but go quiet.
            let reopen = st.open() || st.want || st.leases > 0;
            st.state = "flashing"; // set first so nothing reopens it in the gap
            let close = self.close_fd_locked(p, &mut st, "closed");
            (cmd, tool, action, reopen, close)
        };
        if let Some(h) = close {
            let _ = h.await;
        }
        {
            let mut st = p.lock();
            if let Some(j) = st.flash.as_mut() {
                j.state = "running";
                j.phase = "starting".into();
            }
            self.notify(p, &st);
        }
        let parser = Arc::new(Mutex::new(FlashProgress::new(&tool, files.len().max(1), &action)));
        let mut c = tokio::process::Command::new(&cmd[0]);
        let path = format!("{SYSTEM_PATH}:{}", std::env::var("PATH").unwrap_or_default());
        c.args(&cmd[1..]).env("PATH", path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match self.ctx().spawn(&mut c) {
            Ok(c) => c,
            Err(e) => {
                // The tool isn't there: say so, and give the port back (the TypeScript left it closed).
                let mut st = p.lock();
                self.finish_job(p, &mut st, "error", &format!("{}: {e}", cmd[0]));
                st.state = "closed";
                self.notify(p, &st);
                drop(st);
                if reopen {
                    self.reopen_after_flash(p.clone(), 8);
                }
                return Ok(());
            }
        };
        {
            let mut st = p.lock();
            if let Some(j) = st.flash.as_mut() {
                j.pid = child.child.id();
            }
        }
        let last_push = Arc::new(Mutex::new(0u64));
        let consume = |mut s: Box<dyn tokio::io::AsyncRead + Unpin + Send>| {
            let (me, p, parser, last_push) = (self.clone(), p.clone(), parser.clone(), last_push.clone());
            async move {
                let mut dec = Utf8Stream::default();
                let mut buf = vec![0u8; 8192];
                loop {
                    let n = match s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let text = dec.decode(&buf[..n]);
                    let mut st = p.lock();
                    let mut pr = parser.lock().unwrap();
                    let lines = pr.feed(&text);
                    let Some(job) = st.flash.as_mut() else { continue };
                    for l in &lines {
                        job.log.push(l.clone());
                    }
                    if job.log.len() > 400 {
                        let n = job.log.len() - 400;
                        job.log.drain(..n);
                    }
                    job.progress = pr.progress;
                    job.phase = pr.phase.clone();
                    job.info = pr.info.clone();
                    drop(pr);
                    // Stream every line, but throttle bare progress ticks.
                    let now = now_ms();
                    let mut lp = last_push.lock().unwrap();
                    if !lines.is_empty() || now - *lp > 200 {
                        *lp = now;
                        if lines.is_empty() {
                            me.flash_msg(&st, None);
                        } else {
                            for l in &lines {
                                me.flash_msg(&st, Some(l));
                            }
                        }
                    }
                }
            }
        };
        // One parser for both streams: esptool talks on stdout, avrdude on stderr.
        let out = child.child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>);
        let err = child.child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>);
        let a = out.map(|s| tokio::spawn(consume(s)));
        let b = err.map(|s| tokio::spawn(consume(s)));
        if let Some(a) = a {
            let _ = a.await;
        }
        if let Some(b) = b {
            let _ = b.await;
        }
        let code = child.child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
        {
            let mut st = p.lock();
            let mut pr = parser.lock().unwrap();
            let rest = pr.end();
            if let Some(j) = st.flash.as_mut() {
                j.log.extend(rest);
                j.info = pr.info.clone();
            }
            let perr = pr.error.clone();
            drop(pr);
            let Some(j) = st.flash.as_ref() else { return Ok(()) };
            if j.state == "cancelled" {
                self.finish_job(p, &mut st, "cancelled", "Cancelled");
            } else if code == 0 {
                let m = if j.action == "info" { info_summary(j) } else { "Done".into() };
                self.finish_job(p, &mut st, "done", &m);
            } else {
                let m = if !perr.is_empty() {
                    perr
                } else {
                    j.log.iter().rev().find(|l| !l.starts_with(char::is_whitespace)).cloned().unwrap_or_else(|| format!("exit {code}"))
                };
                self.finish_job(p, &mut st, "error", &m);
            }
            st.state = "closed";
            self.notify(p, &st);
        }
        if reopen {
            self.reopen_after_flash(p.clone(), 8);
        }
        Ok(())
    }

    /// ESP USB-JTAG and native-USB boards re-enumerate after a reset, so the node can vanish briefly.
    fn reopen_after_flash(self: &Arc<Self>, p: Arc<Port>, tries: u32) {
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            let (open, flashing, path) = {
                let st = p.lock();
                (st.open(), st.state == "flashing", st.info.path.clone())
            };
            if open || flashing {
                return;
            }
            if exists(&path) {
                if me.ensure_open(&p).is_err() && tries > 1 {
                    me.reopen_after_flash(p, tries - 1);
                }
            } else if tries > 1 {
                me.reopen_after_flash(p, tries - 1);
            }
        });
    }

    pub(crate) fn cancel_flash(&self, p: &Port) -> Result<(), HttpError> {
        let mut st = p.lock();
        let Some(j) = st.flash.as_mut().filter(|j| j.state == "running" || j.state == "preparing") else {
            return Err(http(409, "Nothing is flashing"));
        };
        // While still downloading there's no process yet; run_flash checks the state before spawning.
        j.state = "cancelled";
        if let Some(pid) = j.pid {
            // SAFETY: kill(2) on the tool we spawned (still unreaped: we hold its Child).
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets() {
        assert_eq!(parse_offset(None, 7).unwrap(), 7);
        assert_eq!(parse_offset(Some(&json!("0x10000")), 0).unwrap(), 0x10000);
        assert_eq!(parse_offset(Some(&json!(4096)), 0).unwrap(), 4096);
        assert_eq!(parse_offset(Some(&json!("")), 3).unwrap(), 3);
        assert_eq!(parse_offset(Some(&json!("x")), 0).unwrap_err().message, "Bad offset \"x\"");
        assert!(parse_offset(Some(&json!(-1)), 0).is_err());
    }

    #[test]
    fn requests_of_every_shape() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"tool\"\r\n\r\navrdude\r\n--b\r\nContent-Disposition: form-data; name=\"0x1000\"; filename=\"boot.bin\"\r\n\r\nAB\r\n--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"app.bin\"\r\n\r\nCD\r\n--b\r\nContent-Disposition: form-data; name=\"offset\"\r\n\r\n0x10000\r\n--b--\r\n";
        let r = flash_request("multipart/form-data; boundary=b", &[], body).unwrap();
        assert_eq!((r.opts.tool.as_str(), r.opts.action.as_str()), ("avrdude", "write"));
        assert_eq!(r.sources.iter().map(|s| (s.offset, s.name.as_str())).collect::<Vec<_>>(), [(0x1000, "boot.bin"), (0x10000, "app.bin")]);
        let r = flash_request("application/octet-stream", &[("offset".into(), "0x8000".into()), ("chip".into(), "esp32".into())], b"xyz")
            .unwrap();
        assert_eq!((r.sources[0].offset, r.sources[0].name.as_str(), r.opts.chip.as_deref()), (0x8000, "firmware.bin", Some("esp32")));
        let r = flash_request(
            "application/json",
            &[],
            br#"{"action":"write","files":[{"offset":"0x0","url":"https://x/y/fw.bin?raw=1"}],"baud":"921600"}"#,
        )
        .unwrap();
        assert_eq!((r.sources[0].name.as_str(), r.opts.baud), ("fw.bin", Some(921600.0)));
        assert!(flash_request("application/json", &[], br#"{"files":[{"offset":0}]}"#).unwrap_err().message.contains("url or base64"));
        let r = flash_request("", &[], b"").unwrap();
        assert_eq!((r.opts.tool.as_str(), r.opts.action.as_str()), ("esptool", "info"));
    }

    #[test]
    fn job_views_keep_the_key_order() {
        let mut j = FlashJob {
            id: "x-1".into(),
            tool: "esptool".into(),
            action: "info".into(),
            label: "info".into(),
            state: "done",
            phase: "connected".into(),
            progress: 49.6,
            info: Map::new(),
            message: "Done".into(),
            log: (0..20).map(|i| i.to_string()).collect(),
            started: 1,
            ended: None,
            cmd: vec!["/usr/bin/esptool".into()],
            pid: None,
        };
        let v = job_view(Some(&j));
        assert_eq!(v["progress"], 50);
        assert_eq!(v["logTail"].as_array().unwrap().len(), 12);
        assert!(v.get("ended").is_none());
        j.ended = Some(2);
        let keys: Vec<String> = job_view(Some(&j)).as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            ["id", "tool", "action", "label", "state", "phase", "progress", "info", "message", "started", "cmd", "ended", "logTail"]
        );
        j.info.insert("chip".into(), json!("ESP32-C3"));
        j.info.insert("mac".into(), json!("aa"));
        assert_eq!(info_summary(&j), "ESP32-C3 · MAC aa");
    }
}
