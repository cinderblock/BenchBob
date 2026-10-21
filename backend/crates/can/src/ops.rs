//! What changes interfaces and traffic: `ip link` configuration, bus restart, auto-bitrate,
//! vcan, SLCAN, periodic jobs, captures and recordings (can.ts's middle sections).

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use benchbob_core::HttpError;
use benchbob_core::http::truthy;
use benchbob_core::run::{Opts, run};
use benchbob_core::timeline::now_ms;
use regex::Regex;
use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::frame::{self, CanFrame, IdFilter, fmt_bitrate};
use crate::hub::{Capture, Job, Rec};
use crate::iplink::{self, CanCfg};
use crate::json::{O, num, rec_stamp};
use crate::{AbProgress, Inner, MIN_PERIOD_MS, SLCAN_SPEED, Slcan, Task, fmt_num, ip_bin, trial_json};

fn err(status: u16, m: impl Into<String>) -> HttpError {
    HttpError::new(status, m)
}

pub(crate) async fn sudo_ip(args: &[String]) -> Result<(), HttpError> {
    let mut cmd: Vec<&str> = vec!["sudo", "-n", ip_bin()];
    cmd.extend(args.iter().map(String::as_str));
    let r = run(&cmd, Opts::timeout_ms(10_000)).await;
    if r.code != 0 {
        let why = if !r.err.is_empty() {
            r.err
        } else if !r.out.is_empty() {
            r.out
        } else {
            format!("exit {}", r.code)
        };
        return Err(err(400, format!("ip {}: {why}", args.join(" "))));
    }
    Ok(())
}

fn sv(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn link(name: &str, rest: &[&str]) -> Vec<String> {
    let mut v = sv(&["link", "set", "dev", name]);
    v.extend(sv(rest));
    v
}

fn valid_name(n: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-zA-Z][A-Za-z0-9_-]{0,14}$").unwrap()).is_match(n)
}

fn net_exists(name: &str) -> bool {
    std::path::Path::new(&format!("/sys/class/net/{name}")).exists()
}

/// For spawned tasks: configure() as a boxed future.
pub(crate) fn configure_boxed(
    i: Arc<Inner>,
    name: String,
    raw: Value,
    persist: bool,
) -> Pin<Box<dyn Future<Output = Result<Value, HttpError>> + Send>> {
    Box::pin(async move { i.configure(&name, &raw, persist).await })
}

impl Inner {
    pub(crate) async fn configure(self: &Arc<Self>, name: &str, raw: &Value, persist: bool) -> Result<Value, HttpError> {
        let (i, cmds, want_up) = {
            let mut st = self.st.lock().unwrap();
            let i = st.iface(name)?.clone();
            if let Some(b) = st.busy.get(name) {
                return Err(err(409, format!("{name} is busy: {b}")));
            }
            let c = iplink::norm_cfg(raw)?;
            if persist {
                self.save_cfg(&i.key, &c);
            }
            let cmds = iplink::ip_args(&i, &c)?;
            let want_up = c.up.unwrap_or(i.up);
            if cmds.is_empty() && want_up == i.up {
                // Only bringUpOnPlug changed.
                return Ok(self.iface_out(&st, &i));
            }
            st.busy.insert(name.into(), "configuring".into());
            if let Some(h) = st.hubs.get_mut(name) {
                h.close_sock();
            }
            (i, cmds, want_up)
        };
        let res = async {
            if i.up && (!cmds.is_empty() || !want_up) {
                sudo_ip(&link(name, &["down"])).await?;
            }
            for a in &cmds {
                if let Err(e) = sudo_ip(a).await {
                    if i.up {
                        let _ = sudo_ip(&link(name, &["up"])).await;
                    }
                    return Err(e);
                }
            }
            if want_up && let Err(e) = sudo_ip(&link(name, &["up"])).await {
                let re = Regex::new(r"(?i)bit-timing|Invalid argument").unwrap();
                return Err(if re.is_match(&e.message) { err(400, format!("{name} can't come up until a bitrate is set")) } else { e });
            }
            Ok(())
        }
        .await;
        self.st.lock().unwrap().busy.remove(name);
        self.refresh().await;
        res?;
        let st = self.st.lock().unwrap();
        let n = st.iface(name)?.clone();
        let mut text = format!("{name}: {}", if n.up { "up" } else { "down" });
        if let Some(b) = n.bitrate {
            text += &format!(" at {}", fmt_bitrate(b));
        }
        if n.fd_on
            && let Some(d) = n.dbitrate
        {
            text += &format!("/{} FD", fmt_bitrate(d));
        }
        if !n.ctrl.is_empty() {
            text += &format!(" {}", n.ctrl.join(" ").to_lowercase());
        }
        self.ctx().event(&text, None);
        Ok(self.iface_out(&st, &n))
    }

    pub(crate) async fn restart_bus(self: &Arc<Self>, name: &str) -> Result<Value, HttpError> {
        let kind = self.st.lock().unwrap().iface(name)?.kind.clone();
        if kind != "can" {
            return Err(err(400, format!("{name} has no controller to restart")));
        }
        sudo_ip(&link(name, &["type", "can", "restart"])).await?;
        self.refresh().await;
        let st = self.st.lock().unwrap();
        let i = st.iface(name)?;
        Ok(self.iface_out(&st, i))
    }

    // ---------- auto-bitrate (listen-only, never ACKs) ----------

    pub(crate) async fn autobitrate(self: &Arc<Self>, name: &str, opts: &Value) -> Result<Value, HttpError> {
        let i = {
            let mut st = self.st.lock().unwrap();
            let i = st.iface(name)?.clone();
            if let Some(b) = st.busy.get(name) {
                return Err(err(409, format!("{name} is busy: {b}")));
            }
            if i.kind != "can" && i.kind != "vcan" {
                return Err(err(400, format!("Auto-bitrate needs a CAN controller ({name} is {})", i.kind)));
            }
            st.busy.insert(name.into(), "auto-bitrate".into());
            if let Some(h) = st.hubs.get_mut(name) {
                h.close_sock();
            }
            i
        };
        let rates: Vec<f64> = match opts.get("rates").and_then(Value::as_array).filter(|a| !a.is_empty()) {
            Some(a) => a.iter().map(benchbob_core::http::js_number).collect::<Vec<f64>>(),
            None => frame::STD_BITRATES.iter().map(|b| *b as f64).collect(),
        }
        .into_iter()
        .filter(|r| *r >= 1000.0 && *r <= 1_000_000.0)
        .collect();
        let lm = crate::arg_num(opts, "listenMs");
        let listen_ms = (if lm.is_nan() || lm == 0.0 { 400.0 } else { lm }).clamp(100.0, 5000.0);
        let real = i.kind == "can";
        let supports_berr = i.ctrl_supported.as_ref().is_none_or(|c| c.iter().any(|x| x == "BERR-REPORTING"));
        let prev_listen = i.listen_only();
        let prev_berr = i.ctrl.iter().any(|c| c == "BERR-REPORTING");
        self.st.lock().unwrap().ab_progress.insert(name.into(), AbProgress { step: 0, of: rates.len(), bitrate: 0.0, trials: Vec::new() });
        self.ctx().changed();
        let res = self.autobitrate_run(name, &i, &rates, listen_ms, real, supports_berr, prev_listen, prev_berr, opts).await;
        {
            let mut st = self.st.lock().unwrap();
            st.ab_progress.remove(name);
            st.busy.remove(name);
        }
        self.refresh().await;
        res
    }

    #[allow(clippy::too_many_arguments)]
    async fn autobitrate_run(
        self: &Arc<Self>,
        name: &str,
        i: &crate::Iface,
        rates: &[f64],
        listen_ms: f64,
        real: bool,
        supports_berr: bool,
        prev_listen: bool,
        prev_berr: bool,
        opts: &Value,
    ) -> Result<Value, HttpError> {
        let push = |me: &Arc<Self>, t: frame::BitrateTrial| {
            if let Some(p) = me.st.lock().unwrap().ab_progress.get_mut(name) {
                p.trials.push(t);
            }
        };
        for &rate in rates {
            if let Some(p) = self.st.lock().unwrap().ab_progress.get_mut(name) {
                p.step += 1;
                p.bitrate = rate;
            }
            self.ctx().changed();
            let setup = async {
                sudo_ip(&link(name, &["down"])).await?;
                if real {
                    let mut a = link(name, &["type", "can", "bitrate", &fmt_num(rate), "listen-only", "on"]);
                    if supports_berr {
                        a.extend(sv(&["berr-reporting", "on"]));
                    }
                    sudo_ip(&a).await?;
                }
                sudo_ip(&link(name, &["up"])).await
            }
            .await;
            if let Err(e) = setup {
                push(self, frame::BitrateTrial { bitrate: rate, frames: 0, errors: 0, bus_errors: 0, skipped: Some(e.message) });
                continue;
            }
            let b0 = if real { xstat(name).await } else { 0 };
            let counts = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
            let c2 = counts.clone();
            let opened = crate::sock::CanSocket::open(
                name,
                &crate::sock::Options { fd_frames: true, err_mask: frame::CAN_ERR_MASK, ..Default::default() },
                Box::new(move |fs| {
                    for f in fs {
                        if f.err { &c2.1 } else { &c2.0 }.fetch_add(1, Ordering::Relaxed);
                    }
                }),
                None,
            );
            match opened {
                Ok(s) => {
                    tokio::time::sleep(Duration::from_millis(listen_ms as u64)).await;
                    drop(s);
                }
                Err(e) => {
                    push(self, frame::BitrateTrial { bitrate: rate, frames: 0, errors: 0, bus_errors: 0, skipped: Some(e) });
                    continue;
                }
            }
            let bus_errors = if real { xstat(name).await.saturating_sub(b0) } else { 0 };
            push(
                self,
                frame::BitrateTrial {
                    bitrate: rate,
                    frames: counts.0.load(Ordering::Relaxed),
                    errors: counts.1.load(Ordering::Relaxed),
                    bus_errors,
                    skipped: None,
                },
            );
        }
        let trials: Vec<frame::BitrateTrial> = self.st.lock().unwrap().ab_progress.get(name).map(|p| p.trials.clone()).unwrap_or_default();
        let verdict = frame::pick_bitrate(&trials.iter().filter(|t| t.skipped.is_none()).cloned().collect::<Vec<_>>());
        let apply = opts.get("apply").is_some_and(truthy);
        // Put the interface back how it was, or on the winner if asked to apply it.
        let _ = sudo_ip(&link(name, &["down"])).await;
        let mut applied: Option<f64> = None;
        if real {
            let br = if apply && verdict.bitrate.is_some() { verdict.bitrate } else { i.bitrate };
            let mut a = link(name, &["type", "can"]);
            if let Some(b) = br {
                a.extend(["bitrate".into(), fmt_num(b)]);
                if let Some(sp) = i.sample_point.filter(|s| *s != 0.0 && !s.is_nan())
                    && Some(b) == i.bitrate
                {
                    a.extend(["sample-point".into(), format!("{sp:.3}")]);
                }
            }
            a.extend(sv(&["listen-only", if prev_listen { "on" } else { "off" }]));
            if supports_berr {
                a.extend(sv(&["berr-reporting", if prev_berr { "on" } else { "off" }]));
            }
            if let Err(e) = sudo_ip(&a).await {
                self.ctx().log(&format!("{name}: restore failed: {}", e.message));
            }
            if apply && let Some(b) = verdict.bitrate {
                applied = Some(b);
                self.save_cfg(&i.key, &CanCfg { bitrate: Some(b), ..Default::default() });
            }
        }
        if (i.up || applied.is_some())
            && let Err(e) = sudo_ip(&link(name, &["up"])).await
        {
            self.ctx().log(&format!("{name}: bring-up failed: {}", e.message));
        }
        let mut o = O::new()
            .k("iface", name)
            .k("result", verdict.result)
            .on("bitrate", verdict.bitrate)
            .o("confidence", verdict.confidence)
            .k("text", verdict.text.clone())
            .k("applied", applied.map(num).unwrap_or(Value::Null))
            .n("listenMs", listen_ms)
            .k("trials", trials.iter().map(trial_json).collect::<Vec<_>>());
        if !real {
            o = o.k("note", "vcan has no bit timing: every rate sees the same traffic, so this only exercises the procedure");
        }
        let out = o.v();
        let mut last = out.clone();
        last["at"] = Value::from(now_ms());
        self.st.lock().unwrap().ab_last.insert(name.into(), last);
        self.ctx().event(&format!("{name}: auto-bitrate: {}", verdict.text), None);
        Ok(out)
    }

    // ---------- vcan ----------

    pub(crate) async fn create_vcan(self: &Arc<Self>, name: Option<String>, fd: bool) -> Result<Value, HttpError> {
        if let Some(n) = &name
            && !valid_name(n)
        {
            return Err(err(400, "Interface names are 1–15 letters, digits, - or _"));
        }
        let name = name.unwrap_or_else(|| (0..).map(|n| format!("vcan{n}")).find(|n| !net_exists(n)).unwrap());
        if net_exists(&name) {
            return Err(err(409, format!("{name} already exists")));
        }
        run(&["sudo", "-n", "modprobe", "vcan"], Opts::default()).await;
        sudo_ip(&sv(&["link", "add", "dev", &name, "type", "vcan"])).await?;
        if fd {
            sudo_ip(&link(&name, &["mtu", "72"])).await?;
        }
        sudo_ip(&link(&name, &["up"])).await?;
        self.refresh().await;
        self.ctx().event(&format!("Created virtual CAN {name}{}", if fd { " (FD)" } else { "" }), None);
        let st = self.st.lock().unwrap();
        let i = st.ifaces.get(&name).ok_or_else(|| err(500, format!("{name} didn't show up in ip link")))?;
        Ok(self.iface_out(&st, i))
    }

    pub(crate) async fn remove_vcan(self: &Arc<Self>, name: &str) -> Result<Value, HttpError> {
        let kind = self.st.lock().unwrap().iface(name)?.kind.clone();
        if kind != "vcan" {
            return Err(err(400, format!("{name} is not a virtual CAN interface")));
        }
        sudo_ip(&sv(&["link", "del", "dev", name])).await?;
        self.refresh().await;
        Ok(O::new().k("ok", true).k("removed", name).v())
    }

    // ---------- SLCAN (serial adapters, attached with slcand) ----------

    fn tty_key(&self, tty: &str) -> String {
        let p = self
            .ctx()
            .devices(Some(&|d: &benchbob_core::udev::Props| d.get("DEVNAME").map(String::as_str) == Some(tty)))
            .into_iter()
            .next();
        match p {
            Some(p) if p.get("ID_SERIAL_SHORT").is_some_and(|s| !s.is_empty()) => format!("{}:{}", crate::vidpid(&p), p["ID_SERIAL_SHORT"]),
            Some(p) if p.get("ID_PATH").is_some_and(|s| !s.is_empty()) => p["ID_PATH"].clone(),
            _ => tty.to_string(),
        }
    }

    pub(crate) async fn attach_slcan(self: &Arc<Self>, b: &Value) -> Result<Value, HttpError> {
        let tty = b.get("tty").and_then(Value::as_str).unwrap_or("").to_string();
        let re = Regex::new(r"^/dev/[A-Za-z0-9_/.+-]+$").unwrap();
        if !re.is_match(&tty) || !std::path::Path::new(&tty).exists() {
            return Err(err(400, format!("No such serial device {}", crate::arg_str(b, "tty"))));
        }
        if let Some(s) = self.st.lock().unwrap().slcans.get(&tty) {
            return Err(err(409, format!("{tty} is already attached as {}", s.name)));
        }
        let br = crate::arg_num(b, "bitrate");
        let bitrate = if br.is_nan() || br == 0.0 { 500000.0 } else { br };
        let Some(code) = SLCAN_SPEED.iter().find(|(b, _)| *b as f64 == bitrate).map(|(_, c)| *c) else {
            return Err(err(
                400,
                format!("SLCAN supports {}", SLCAN_SPEED.iter().map(|(b, _)| fmt_bitrate(*b as f64)).collect::<Vec<_>>().join(", ")),
            ));
        };
        let name = b.get("name").filter(|v| truthy(v)).map(benchbob_core::http::js_string);
        if let Some(n) = &name
            && !valid_name(n)
        {
            return Err(err(400, "Interface names are 1–15 letters, digits, - or _"));
        }
        let name = name.unwrap_or_else(|| (0..).map(|n| format!("slcan{n}")).find(|n| !net_exists(n)).unwrap());
        if net_exists(&name) {
            return Err(err(409, format!("{name} already exists")));
        }
        let sb = crate::arg_num(b, "serialBaud");
        let baud = if sb.is_nan() || sb == 0.0 { 3_000_000.0 } else { sb };
        // -F keeps slcand in the foreground so it's our managed child: killing it (via sudo,
        // which relays SIGTERM) closes the channel (-c) and removes the netdev.
        let mut cmd = tokio::process::Command::new("sudo");
        cmd.args(["-n", "slcand", "-o", "-c", "-F", &format!("-s{code}"), "-t", "hw", "-S", &fmt_num(baud), &tty, &name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = self.ctx().spawn(&mut cmd).map_err(|e| err(500, format!("slcand: {e}")))?;
        for _ in 0..40 {
            if net_exists(&name) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            if child.child.try_wait().ok().flatten().is_some() {
                break;
            }
        }
        if !net_exists(&name) {
            let _ = child.child.start_kill();
            let _ = child.child.wait().await;
            let mut e = String::new();
            if let Some(mut s) = child.child.stderr.take() {
                let _ = s.read_to_string(&mut e).await;
            }
            let e = e.trim();
            return Err(err(500, format!("slcand didn't create {name}{}", if e.is_empty() { String::new() } else { format!(": {e}") })));
        }
        let label = {
            let st = self.st.lock().unwrap();
            let c = self.slcan_candidates(&st).into_iter().find(|c| c["tty"] == tty.as_str());
            match c {
                Some(c) => format!("{} on {tty}", c["kind"].as_str().unwrap_or("")),
                None => format!("SLCAN on {tty}"),
            }
        };
        let (tx, rx) = tokio::sync::watch::channel(None);
        let generation = self.slcan_seq.fetch_add(1, Ordering::SeqCst);
        let pid = child.child.id();
        if let Some(mut e) = child.child.stderr.take() {
            // Keep the pipe drained.
            tokio::spawn(async move {
                let mut sink = Vec::new();
                let _ = e.read_to_end(&mut sink).await;
            });
        }
        self.st.lock().unwrap().slcans.insert(
            tty.clone(),
            Slcan {
                name: name.clone(),
                tty: tty.clone(),
                tty_key: self.tty_key(&tty),
                label,
                bitrate,
                pid,
                since: now_ms(),
                exited: rx,
                generation,
            },
        );
        {
            let me = self.clone();
            let (tty, name) = (tty.clone(), name.clone());
            tokio::spawn(async move {
                let code = child.child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
                let _ = tx.send(Some(code));
                {
                    let mut st = me.st.lock().unwrap();
                    if st.slcans.get(&tty).is_some_and(|s| s.generation == generation) {
                        st.slcans.shift_remove(&tty);
                    }
                }
                if me.ctx.get().is_some() {
                    me.ctx().event(
                        &format!(
                            "SLCAN {name} on {tty} detached{}",
                            if code != 0 { format!(" (slcand exit {code})") } else { String::new() }
                        ),
                        None,
                    );
                    me.refresh().await;
                }
            });
        }
        if let Err(e) = sudo_ip(&link(&name, &["up"])).await {
            self.ctx().log(&e.message);
        }
        self.refresh().await;
        self.ctx().event(&format!("SLCAN {name} attached on {tty} at {}", fmt_bitrate(bitrate)), None);
        Ok(self.iface_out_named(&name))
    }

    pub(crate) async fn detach_slcan(self: &Arc<Self>, name_or_tty: &str) -> Result<Value, HttpError> {
        let found = {
            let st = self.st.lock().unwrap();
            st.slcans
                .get(name_or_tty)
                .or_else(|| st.slcans.values().find(|s| s.name == name_or_tty))
                .map(|s| (s.tty.clone(), s.name.clone(), s.pid, s.exited.clone()))
        };
        let Some((tty, name, pid, mut exited)) = found else {
            return Err(err(404, format!("{name_or_tty} isn't an attached SLCAN adapter")));
        };
        let kill = |sig| {
            if let Some(p) = pid {
                // SAFETY: kill(2) on the sudo/slcand child we spawned.
                unsafe { libc::kill(p as libc::pid_t, sig) };
            }
        };
        kill(libc::SIGTERM);
        let _ = tokio::time::timeout(Duration::from_secs(3), exited.wait_for(|c| c.is_some())).await;
        if exited.borrow().is_none() {
            kill(libc::SIGKILL);
        }
        self.st.lock().unwrap().slcans.shift_remove(&tty);
        self.refresh().await;
        Ok(O::new().k("ok", true).k("detached", name).k("tty", tty).v())
    }

    // ---------- periodic transmit ----------

    pub(crate) fn start_job(self: &Arc<Self>, name: &str, frame: CanFrame, period_ms: f64) -> Result<Value, HttpError> {
        if !(MIN_PERIOD_MS..=3_600_000.0).contains(&period_ms) {
            return Err(err(400, format!("periodMs must be {}–3600000", fmt_num(MIN_PERIOD_MS))));
        }
        let mut st = self.st.lock().unwrap();
        let st = &mut *st;
        let i = st.ifaces.get(name);
        let hub = st.hubs.get_mut(name).ok_or_else(|| err(404, format!("No CAN interface \"{name}\"")))?;
        if hub.jobs.len() >= 64 {
            return Err(err(409, "Too many periodic jobs on this interface"));
        }
        hub.send(i, &frame)?; // validates (up, FD mode, listen-only) before we commit to a timer
        let id = self.job_seq.fetch_add(1, Ordering::SeqCst);
        let me = Arc::downgrade(self);
        let (n, f2) = (name.to_string(), frame.clone());
        let period = Duration::from_micros((period_ms * 1000.0) as u64);
        let task = tokio::spawn(async move {
            let mut t = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                t.tick().await;
                let Some(me) = me.upgrade() else { return };
                let mut st = me.st.lock().unwrap();
                let st = &mut *st;
                let i = st.ifaces.get(&n);
                let Some(hub) = st.hubs.get_mut(&n) else { return };
                let r = hub.send(i, &f2);
                let Some(job) = hub.jobs.get_mut(&id) else { return };
                match r {
                    Ok(()) => {
                        job.sent += 1;
                        job.last_err = None;
                    }
                    Err(e) => {
                        job.errors += 1;
                        job.last_err = Some(e.message.clone());
                        // A bus with nothing ACKing fills the TX queue; give up rather than spin forever.
                        if job.errors >= 50 && job.errors > job.sent {
                            let j = hub.jobs.shift_remove(&id);
                            me.ctx().changed();
                            me.ctx().event(&format!("{n}: periodic {} stopped: {}", frame::candump_body(&f2), e.message), None);
                            drop(j); // aborts this task last
                            return;
                        }
                    }
                }
            }
        });
        let job = Job { id, frame, period_ms, sent: 1, errors: 0, last_err: None, since: now_ms(), _task: Task(task) };
        let out = job.json();
        hub.jobs.insert(id, job);
        self.ctx().changed();
        Ok(out)
    }

    pub(crate) fn stop_job(&self, name: &str, id: f64) -> Result<bool, HttpError> {
        let mut st = self.st.lock().unwrap();
        let hub = st.hub_for(name)?;
        let ok = id.fract() == 0.0 && id >= 0.0 && hub.jobs.shift_remove(&(id as u64)).is_some();
        if ok {
            self.ctx().changed();
        }
        Ok(ok)
    }

    // ---------- capture ----------

    pub(crate) async fn capture(
        self: &Arc<Self>,
        name: &str,
        ms: f64,
        filters: Option<Vec<IdFilter>>,
        max: f64,
    ) -> Result<Value, HttpError> {
        let ms = (if ms.is_nan() || ms == 0.0 { 1000.0 } else { ms }).clamp(10.0, 60000.0);
        let max = (if max.is_nan() || max == 0.0 { 1000.0 } else { max }).clamp(1.0, 20000.0);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = {
            let mut st = self.st.lock().unwrap();
            st.capture_seq += 1;
            let id = st.capture_seq;
            let hub = st.hub_for(name)?;
            if hub.sock.is_none() {
                return Err(err(409, format!("{name} is down")));
            }
            hub.captures.insert(id, Capture { filters, frames: Vec::new(), max: max as usize, done: Some(tx) });
            id
        };
        let _ = tokio::time::timeout(Duration::from_secs_f64(ms / 1000.0), rx).await;
        let c = self.st.lock().unwrap().hubs.get_mut(name).and_then(|h| h.captures.remove(&id));
        let frames = c.map(|c| c.frames).unwrap_or_default();
        Ok(O::new()
            .k("iface", name)
            .n("ms", ms)
            .k("count", frames.len())
            .k("truncated", frames.len() as f64 >= max)
            .k("frames", frames.iter().map(|f| f.json()).collect::<Vec<_>>())
            .v())
    }

    // ---------- recording (candump -L format) ----------

    pub(crate) fn start_recording(&self, name: &str) -> Result<String, HttpError> {
        let file = {
            let mut st = self.st.lock().unwrap();
            let hub = st.hub_for(name)?;
            if let Some(r) = &hub.rec {
                return Err(err(409, format!("{name} is already recording to {}", r.file)));
            }
            std::fs::create_dir_all(&self.rec_dir).map_err(|e| err(500, format!("{}: {e}", self.rec_dir.display())))?;
            let file = format!("{name}-{}.log", rec_stamp(now_ms() as i64));
            let path = self.rec_dir.join(&file);
            let f = std::fs::File::create(&path).map_err(|e| err(500, format!("{}: {e}", path.display())))?;
            hub.rec = Some(Rec {
                file: file.clone(),
                path,
                writer: std::io::BufWriter::with_capacity(64 * 1024, f),
                frames: 0,
                bytes: 0,
                since: now_ms(),
            });
            file
        };
        self.ctx().event(&format!("{name}: recording to {file}"), None);
        self.refresh_recordings();
        Ok(file)
    }

    pub(crate) fn stop_recording(&self, name: &str) -> Result<Option<Value>, HttpError> {
        let r = self.st.lock().unwrap().hub_for(name)?.stop_recording();
        Ok(r.map(|r| {
            self.rec_stopped(name, &r, "");
            O::new().k("file", r.file.clone()).k("frames", r.frames).k("bytes", r.bytes).v()
        }))
    }

    pub(crate) fn rec_path(&self, file: &str) -> Result<std::path::PathBuf, HttpError> {
        let re = Regex::new(r"^[A-Za-z0-9_.-]+\.log$").unwrap();
        if !re.is_match(file) || file.starts_with('.') {
            return Err(err(400, "Bad recording name"));
        }
        let p = self.rec_dir.join(file);
        if !p.exists() {
            return Err(err(404, format!("No recording {file}")));
        }
        Ok(p)
    }
}

async fn xstat(name: &str) -> u64 {
    let r = run(&[ip_bin(), "-details", "-statistics", "-json", "link", "show", "dev", name], Opts::default()).await;
    serde_json::from_str::<Value>(&r.out)
        .ok()
        .and_then(|v| {
            v.get(0)
                .and_then(|x| x.get("linkinfo"))
                .and_then(|x| x.get("info_xstats"))
                .and_then(|x| x.get("bus_error"))
                .map(benchbob_core::http::js_number)
        })
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n as u64)
        .unwrap_or(0)
}
