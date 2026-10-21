//! The wired-network panel (peripherals/drivers/netdiag.ts).
//!
//! The bench questions about a cable: what did it negotiate, is it taking errors, which
//! switch port is it plugged into, and how fast is it really. For eth0 and any USB Ethernet
//! adapter that appears:
//!  - link: ethtool speed/duplex/autoneg/partner modes/EEE, error counters that are
//!    non-zero or moving, driver/firmware, carrier changes, addresses, gateway, DNS
//!  - neighbour: lldpd as a managed child in receive-only mode (never advertises BenchBob),
//!    also listening for CDP/FDP/EDP/SONMP, queried over a private control socket
//!  - iperf3: an on-demand server (auto-off after an hour) and a client runner
//!  - quick checks: gateway ping, DNS timing per nameserver, HTTP(S) reachability, path MTU
//!
//! lldpd/iperf3 system units stay masked; everything here dies with BenchBob.

pub mod dns;
pub mod parse;
mod routes;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use benchbob_core::http::{HttpError, js_number, js_string, num, truthy};
use benchbob_core::run::{Opts, read_text, run, which};
use benchbob_core::timeline::now_ms;
use benchbob_core::udev::{Props, UdevEvent, usb_name};
use benchbob_core::{Ctx, Driver, Json, Route, StopReason, Tool};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::watch;

use crate::parse::{Eee, EthLink, Preset};

/// A spawned task, aborted when this handle is dropped.
struct Task(tokio::task::JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// SIGTERM (sudo passes it on to the program it runs; SIGKILL would orphan it).
fn term(pid: Option<u32>) {
    if let Some(p) = pid {
        // SAFETY: kill(2) on a child we spawned.
        unsafe {
            libc::kill(p as libc::pid_t, libc::SIGTERM);
        }
    }
}

/// `Bun.which(name, { PATH: …:/usr/sbin:/sbin }) || name`
fn bin(name: &str) -> String {
    which(name).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| name.into())
}

/// The listen port this instance serves on: the lldpd socket and iperf3 key directories are
/// per instance, so a side instance never touches the live service's.
fn instance() -> String {
    benchbob_core::paths::env("PORT").unwrap_or_else(|| "80".into())
}

pub(crate) fn lldp_dir() -> String {
    format!("/tmp/benchbob-lldpd-{}", instance())
}

fn iperf_key_dir() -> String {
    format!("/tmp/benchbob-iperf3-{}", instance())
}

/// Append each non-blank line of `text`, trimmed, keeping the last `max`.
fn tail(buf: &mut Vec<String>, text: &str, max: usize) {
    for l in text.split('\n') {
        if !l.trim().is_empty() {
            buf.push(l.trim().to_string());
        }
    }
    if buf.len() > max {
        let n = buf.len() - max;
        buf.drain(..n);
    }
}

async fn lines<R: AsyncRead + Unpin>(r: Option<R>, mut cb: impl FnMut(String)) {
    let Some(r) = r else { return };
    let mut l = BufReader::new(r).lines();
    while let Ok(Some(line)) = l.next_line().await {
        cb(line);
    }
}

fn exit_code(s: std::io::Result<std::process::ExitStatus>) -> String {
    match s.ok().and_then(|s| s.code()) {
        Some(c) => c.to_string(),
        None => "null".into(),
    }
}

// ---------- State ----------

#[derive(Clone, Default)]
pub(crate) struct Iface {
    name: String,
    usb: bool,
    /// "Realtek RTL8153 (0bda:8153)"
    usb_desc: String,
    net_driver: String,
    mac: String,
    operstate: String,
    carrier: bool,
    carrier_changes: i64,
    mtu: i64,
    link: Option<EthLink>,
    info: IndexMap<String, String>,
    eee: Option<Eee>,
    stats: IndexMap<String, i64>,
    /// Error counters worth showing: non-zero now, or moved since we started watching.
    errors: Vec<(String, i64, i64)>,
    baseline: HashMap<String, i64>,
    addrs: Vec<Value>,
    dns: Vec<String>,
}

#[derive(Default)]
struct Lldp {
    /// Non-zero while our lldpd runs (its spawn sequence number).
    seq: u64,
    pid: Option<u32>,
    watcher: Option<u32>,
    exited: Option<watch::Receiver<bool>>,
    since: u64,
    neighbors: Vec<Value>,
    log: Vec<String>,
    error: String,
    seen: HashSet<String>,
    last_heal: u64,
}

#[derive(Default)]
struct Server {
    seq: u64,
    pid: Option<u32>,
    port: i64,
    started_at: u64,
    off_at: u64,
    current: Option<Map<String, Value>>,
    tests: Vec<Value>,
    log: Vec<String>,
    off_timer: Option<Task>,
}

pub(crate) struct ClientRun {
    id: String,
    host: String,
    port: i64,
    duration: f64,
    reverse: bool,
    udp: bool,
    bitrate: String,
    parallel: i64,
    auth: bool,
    started: u64,
    status: &'static str,
    points: Vec<Value>,
    result: Option<Value>,
    error: String,
    pid: Option<u32>,
    done: watch::Receiver<bool>,
}

impl ClientRun {
    fn public(&self) -> Value {
        let bps_text = self.result.as_ref().map(|r| parse::fmt_bps(r["bps"].as_f64().unwrap_or(0.0))).unwrap_or_default();
        json!({
            "id": self.id, "host": self.host, "port": self.port, "duration": num(self.duration), "reverse": self.reverse, "udp": self.udp,
            "bitrate": self.bitrate, "parallel": self.parallel, "auth": self.auth, "started": self.started, "status": self.status,
            "points": self.points, "result": self.result, "error": self.error, "bpsText": bps_text,
        })
    }
}

struct Gateway {
    dev: String,
    gateway: String,
    metric: Value,
}

pub(crate) struct State {
    ifaces: IndexMap<String, Iface>,
    gateways: Vec<Gateway>,
    nameservers: Vec<String>,
    search: Vec<String>,
    lldp: Lldp,
    server: Server,
    runs: Vec<ClientRun>,
    /// `{ pings: [] }`, then ping/dns/http/mtu in the order they first ran (as the
    /// JavaScript object's keys).
    checks: IndexMap<String, Value>,
}

impl Default for State {
    fn default() -> Self {
        let mut checks = IndexMap::new();
        checks.insert("pings".into(), json!([]));
        Self {
            ifaces: IndexMap::new(),
            gateways: Vec::new(),
            nameservers: Vec::new(),
            search: Vec::new(),
            lldp: Lldp::default(),
            server: Server::default(),
            runs: Vec::new(),
            checks,
        }
    }
}

pub(crate) struct Inner {
    ctx: OnceLock<Ctx>,
    st: Mutex<State>,
    seq: AtomicU64,
    /// Bumped on stop, so stale loops exit.
    epoch: AtomicU64,
    loops: Mutex<Vec<Task>>,
}

/// The wired-network driver.
pub struct Netdiag {
    inner: Arc<Inner>,
}

impl Default for Netdiag {
    fn default() -> Self {
        Self::new()
    }
}

impl Netdiag {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                ctx: OnceLock::new(),
                st: Mutex::new(State::default()),
                seq: AtomicU64::new(1),
                epoch: AtomicU64::new(0),
                loops: Mutex::new(Vec::new()),
            }),
        }
    }
}

fn is_wired(p: &Props) -> bool {
    let g = |k: &str| p.get(k).map(String::as_str).unwrap_or("");
    let i = g("INTERFACE");
    g("SUBSYSTEM") == "net"
        && !i.is_empty()
        && !g("DEVPATH").contains("/virtual/")
        && g("DEVTYPE") != "wlan"
        && !(i.starts_with("wl") || i == "lo")
        && !matches!(g("DEVTYPE"), "bridge" | "bond" | "vlan" | "wwan")
}

impl Inner {
    fn ctx(&self) -> &Ctx {
        self.ctx.get().expect("netdiag driver used before init")
    }

    fn event(&self, text: &str, detail: Option<Value>) {
        if let Some(c) = self.ctx.get() {
            c.event(text, detail);
        }
    }

    fn changed(&self) {
        if let Some(c) = self.ctx.get() {
            c.changed();
        }
    }

    fn setting(&self, k: &str) -> Option<Value> {
        self.ctx.get().and_then(|c| c.setting(k))
    }

    pub(crate) fn lldp_enabled(&self) -> Value {
        self.setting("lldp").unwrap_or(json!(true))
    }

    pub(crate) fn iperf_port(&self) -> Value {
        self.setting("iperfPort").unwrap_or_else(|| json!(5201 + benchbob_core::paths::port_offset() as i64))
    }

    fn iperf_auto_off_min(&self) -> Value {
        self.setting("iperfAutoOffMin").unwrap_or(json!(60))
    }

    // Quick-pick hosts for the panel's iperf3 client and ping buttons; "gateway" = the
    // default gateway.
    pub(crate) fn iperf_presets(&self) -> Value {
        self.setting("iperfPresets").unwrap_or_else(|| Value::Array(parse::default_iperf_presets().iter().map(Preset::json).collect()))
    }

    pub(crate) fn ping_targets(&self) -> Value {
        self.setting("pingTargets").unwrap_or_else(|| Value::Array(parse::default_ping_targets().iter().map(Preset::json).collect()))
    }

    // Logins for iperf3 servers that require one, by host. Like Wi-Fi PSKs, the password
    // never leaves the driver: the API shows hasPassword and the users-file line (a hash)
    // instead. pkcs1: iperf3 ≤3.16 servers (Ubuntu 24.04's) only decrypt PKCS#1 v1.5; 3.17+
    // clients send OAEP unless told --use-pkcs1-padding.
    pub(crate) fn iperf_auths(&self) -> Map<String, Value> {
        match self.setting("iperfAuth") {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        }
    }

    // ---------- Interfaces ----------

    fn rescan_ifaces(self: &Arc<Self>) {
        let found = self.ctx().devices(Some(&is_wired));
        let names: HashSet<String> = found.iter().map(|p| p["INTERFACE"].clone()).collect();
        let mut added = Vec::new();
        for p in &found {
            let name = p["INTERFACE"].clone();
            if self.st.lock().unwrap().ifaces.contains_key(&name) {
                continue;
            }
            let usb_dev = self.ctx().ancestor(p.get("DEVPATH").map(String::as_str).unwrap_or(""), "usb", Some("usb_device"));
            let usb_desc = usb_dev
                .as_ref()
                .map(|u| {
                    let (vendor, model) = usb_name(u);
                    let g = |k: &str| u.get(k).cloned().unwrap_or_else(|| "undefined".into());
                    format!("{vendor} {model} ({}:{})", g("ID_VENDOR_ID"), g("ID_MODEL_ID")).trim().to_string()
                })
                .unwrap_or_default();
            let i = Iface {
                name: name.clone(),
                usb: usb_dev.is_some(),
                usb_desc,
                net_driver: p.get("ID_NET_DRIVER").cloned().unwrap_or_default(),
                ..Default::default()
            };
            if i.usb {
                self.event(
                    &format!(
                        "USB Ethernet adapter: {} — {}{}",
                        i.name,
                        i.usb_desc,
                        if i.net_driver.is_empty() { String::new() } else { format!(", driver {}", i.net_driver) }
                    ),
                    None,
                );
            }
            self.st.lock().unwrap().ifaces.insert(name.clone(), i);
            added.push(name);
        }
        let gone: Vec<Iface> = {
            let mut st = self.st.lock().unwrap();
            let g: Vec<String> = st.ifaces.keys().filter(|k| !names.contains(*k)).cloned().collect();
            g.iter().filter_map(|k| st.ifaces.shift_remove(k)).collect()
        };
        for i in gone {
            if i.usb {
                self.event(&format!("USB Ethernet adapter removed: {}", i.name), None);
            }
        }
        for name in added {
            let me = self.clone();
            tokio::spawn(async move { me.poll_full(&name).await });
        }
        self.changed();
    }

    /// Every 5 s, sysfs only (no processes): carrier and its change counter, operstate, MTU.
    async fn poll_sysfs(self: &Arc<Self>) {
        let names: Vec<String> = self.st.lock().unwrap().ifaces.keys().cloned().collect();
        for name in names {
            let d = format!("/sys/class/net/{name}");
            let r = |f: &str| read_text(format!("{d}/{f}"));
            let (carrier, changes, oper, mtu, mac) = (r("carrier"), r("carrier_changes"), r("operstate"), r("mtu"), r("address"));
            let (link_change, notify) = {
                let mut st = self.st.lock().unwrap();
                let Some(i) = st.ifaces.get_mut(&name) else { continue };
                let was = i.carrier;
                let prev_oper = std::mem::take(&mut i.operstate);
                i.carrier = carrier == "1";
                i.carrier_changes = changes.parse().unwrap_or(0);
                i.operstate = oper.clone();
                i.mtu = mtu.parse().unwrap_or(0);
                i.mac = mac;
                ((was != i.carrier && i.link.is_some()).then_some(i.carrier), was != i.carrier || oper != prev_oper)
            };
            if let Some(up) = link_change {
                self.event(&format!("{name}: link {}", if up { "up" } else { "down" }), None);
                let me = self.clone();
                let n = name.clone();
                tokio::spawn(async move { me.poll_full(&n).await });
            }
            if notify {
                self.changed();
            }
        }
    }

    /// Every minute (and on link change): ethtool's view.
    async fn poll_full(&self, name: &str) {
        let e = bin("ethtool");
        let need_info = self.st.lock().unwrap().ifaces.get(name).is_some_and(|i| i.info.is_empty());
        let (a1, a2, a3, a4) = ([e.as_str(), name], [e.as_str(), "-S", name], [e.as_str(), "--show-eee", name], [e.as_str(), "-i", name]);
        let (l, s, ee, inf) =
            tokio::join!(run(&a1, Opts::timeout_ms(5000)), run(&a2, Opts::timeout_ms(5000)), run(&a3, Opts::timeout_ms(5000)), async {
                if need_info { Some(run(&a4, Opts::timeout_ms(5000)).await) } else { None }
            },);
        let mut st = self.st.lock().unwrap();
        let Some(i) = st.ifaces.get_mut(name) else { return };
        i.link = Some(parse::parse_ethtool(&l.out));
        i.eee = parse::parse_eee(&ee.out);
        if let Some(inf) = inf {
            i.info = parse::parse_key_colon(&inf.out);
        }
        i.stats = parse::parse_ethtool_stats(&s.out);
        i.errors.clear();
        for (k, v) in &i.stats {
            if !parse::is_error_counter(k) {
                continue;
            }
            let base = *i.baseline.entry(k.clone()).or_insert(*v);
            let delta = v - base;
            if *v != 0 || delta != 0 {
                i.errors.push((k.clone(), *v, delta));
            }
        }
    }

    async fn poll_net(&self) {
        let (a, r) =
            tokio::join!(run(&["ip", "-j", "addr"], Opts::default()), run(&["ip", "-j", "route", "show", "default"], Opts::default()));
        let rc = read_text("/etc/resolv.conf");
        let names: Vec<String> = {
            let mut st = self.st.lock().unwrap();
            if let Ok(Value::Array(list)) = serde_json::from_str::<Value>(if a.out.is_empty() { "[]" } else { &a.out }) {
                for x in list {
                    let Some(i) = x.get("ifname").and_then(Value::as_str).and_then(|n| st.ifaces.get_mut(n)) else { continue };
                    i.addrs = x
                        .get("addr_info")
                        .and_then(Value::as_array)
                        .map(|ai| {
                            ai.iter()
                                .map(|ai| {
                                    let mut m = Map::new();
                                    for (k, src) in [("family", "family"), ("addr", "local"), ("prefix", "prefixlen")] {
                                        if let Some(v) = ai.get(src) {
                                            m.insert(k.into(), v.clone());
                                        }
                                    }
                                    Value::Object(m)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                }
            }
            if let Ok(Value::Array(list)) = serde_json::from_str::<Value>(if r.out.is_empty() { "[]" } else { &r.out }) {
                let mut g: Vec<Gateway> = list
                    .iter()
                    .filter(|g| g.get("gateway").is_some_and(truthy))
                    .map(|g| Gateway {
                        dev: g.get("dev").map(js_string).unwrap_or_default(),
                        gateway: g.get("gateway").map(js_string).unwrap_or_default(),
                        metric: g.get("metric").filter(|m| !m.is_null()).cloned().unwrap_or(json!(0)),
                    })
                    .collect();
                g.sort_by(|x, y| js_number(&x.metric).partial_cmp(&js_number(&y.metric)).unwrap_or(std::cmp::Ordering::Equal));
                st.gateways = g;
            }
            let (ns, search) = parse::parse_resolv_conf(&rc);
            st.nameservers = ns;
            st.search = search;
            st.ifaces.keys().cloned().collect()
        };
        // Per-link DNS as NetworkManager has it (resolv.conf is the merged result).
        let nm = bin("nmcli");
        for name in names {
            let n = run(&[&nm, "-t", "-f", "IP4.DNS,IP6.DNS", "dev", "show", &name], Opts::timeout_ms(3000)).await;
            let dns: Vec<String> = if n.code != 0 {
                Vec::new()
            } else {
                n.out.split('\n').map(|l| l.split(':').skip(1).collect::<Vec<_>>().join(":")).filter(|s| !s.is_empty()).collect()
            };
            if let Some(i) = self.st.lock().unwrap().ifaces.get_mut(&name) {
                i.dns = dns;
            }
        }
    }

    pub(crate) async fn poll_all(self: &Arc<Self>) {
        let names: Vec<String> = self.st.lock().unwrap().ifaces.keys().cloned().collect();
        futures_util::future::join_all(names.iter().map(|n| self.poll_full(n))).await;
        self.poll_net().await;
        self.changed();
    }

    // ---------- LLDP ----------

    async fn lldp_kill_stale(&self) {
        // A previous BenchBob that died hard leaves lldpd holding our socket path: find it by
        // that path.
        let r = run(&["pgrep", "-f", "--", &format!("-u {}/ctl.sock", lldp_dir())], Opts::default()).await;
        let pids: Vec<&str> = r.out.split('\n').filter(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())).collect();
        if !pids.is_empty() {
            self.ctx().log(&format!("killing stale lldpd {}", pids.join(" ")));
            let mut argv = vec!["sudo", "-n", "kill"];
            argv.extend(pids.iter());
            run(&argv, Opts::default()).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub(crate) async fn lldp_start(self: &Arc<Self>) {
        if self.st.lock().unwrap().lldp.seq != 0 {
            return;
        }
        self.lldp_kill_stale().await;
        let dir = lldp_dir();
        // lldpd drops to _lldpd after creating the socket; lldpcli then needs to traverse the dir.
        run(&["mkdir", "-p", &dir], Opts::default()).await;
        run(&["chmod", "755", &dir], Opts::default()).await;
        // -r receive-only: we listen but never transmit, so the switch learns nothing about
        // us. -c -e -f -s: also decode CDP, EDP, FDP, SONMP. -O /dev/null: ignore /etc/lldpd.d.
        let sock = format!("{dir}/ctl.sock");
        let mut cmd = tokio::process::Command::new("sudo");
        cmd.args(["-n", &bin("lldpd"), "-d", "-r", "-c", "-e", "-f", "-s", "-u", &sock, "-O", "/dev/null", "-I", "eth*,en*,usb*"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = match self.ctx().spawn(&mut cmd) {
            Ok(c) => c,
            Err(e) => {
                self.st.lock().unwrap().lldp.error = format!("lldpd: {e}");
                return;
            }
        };
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = watch::channel(false);
        {
            let mut st = self.st.lock().unwrap();
            if st.lldp.seq != 0 {
                // Raced with another start.
                term(child.child.id());
                return;
            }
            let l = &mut st.lldp;
            l.seq = seq;
            l.pid = child.child.id();
            l.exited = Some(rx);
            l.log.clear();
            l.error.clear();
            l.neighbors.clear();
            l.since = now_ms();
        }
        let me = self.clone();
        tokio::spawn(async move {
            let err = child.child.stderr.take();
            let m2 = me.clone();
            lines(err, |l| tail(&mut m2.st.lock().unwrap().lldp.log, &l, 20)).await;
            let code = exit_code(child.child.wait().await);
            let _ = tx.send(true);
            let watcher = {
                let mut st = me.st.lock().unwrap();
                let w = st.lldp.watcher.take();
                if st.lldp.seq == seq {
                    let l = &mut st.lldp;
                    l.seq = 0;
                    l.pid = None;
                    let n = l.log.len();
                    l.error = format!("lldpd exited ({code}): {}", l.log[n.saturating_sub(2)..].join(" / "));
                }
                w
            };
            term(watcher);
            me.changed();
        });
        self.changed();
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            me.lldp_query().await;
            me.lldp_watch(seq);
        });
    }

    /// `lldpcli watch` prints a block whenever a neighbour is added, updated or expires, so
    /// one long-lived child replaces polling (each sudo writes journal lines). Every block
    /// triggers one full query for the detailed view.
    fn lldp_watch(self: &Arc<Self>, seq: u64) {
        if self.st.lock().unwrap().lldp.seq != seq {
            return;
        }
        let mut cmd = tokio::process::Command::new("sudo");
        cmd.args(["-n", &bin("lldpcli"), "-u", &format!("{}/ctl.sock", lldp_dir()), "watch"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let Ok(mut child) = self.ctx().spawn(&mut cmd) else { return };
        self.st.lock().unwrap().lldp.watcher = child.child.id();
        let me = self.clone();
        tokio::spawn(async move {
            let out = child.child.stdout.take();
            let mut debounce: Option<Task> = None;
            let m2 = me.clone();
            lines(out, |_| {
                let m3 = m2.clone();
                debounce = Some(Task(tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    tokio::spawn(async move { m3.lldp_query().await });
                })));
            })
            .await;
            let _ = child.child.wait().await;
            drop(debounce);
            // The watcher only dies on its own if lldpd's socket went away; retry while
            // lldpd lives.
            tokio::time::sleep(Duration::from_secs(5)).await;
            me.lldp_watch(seq);
        });
    }

    pub(crate) async fn lldp_stop(&self) {
        let (pid, exited) = {
            let mut st = self.st.lock().unwrap();
            let l = &mut st.lldp;
            l.seq = 0;
            l.neighbors.clear();
            l.since = 0;
            (l.pid.take(), l.exited.take())
        };
        if pid.is_some() {
            term(pid);
            if let Some(mut rx) = exited {
                let _ = tokio::time::timeout(Duration::from_secs(3), rx.wait_for(|x| *x)).await;
            }
        }
        self.changed();
    }

    pub(crate) async fn lldp_query(&self) {
        if self.st.lock().unwrap().lldp.seq == 0 {
            return;
        }
        let sock = format!("{}/ctl.sock", lldp_dir());
        let r =
            run(&["sudo", "-n", &bin("lldpcli"), "-u", &sock, "-f", "json", "show", "neighbors", "details"], Opts::timeout_ms(5000)).await;
        if r.code != 0 {
            return;
        }
        let Ok(j) = serde_json::from_str::<Value>(&r.out) else { return };
        let neighbors = parse::parse_lldp_neighbors(&j);
        let mut events = Vec::new();
        {
            let mut st = self.st.lock().unwrap();
            for n in &neighbors {
                let s = |k: &str| n[k].as_str().unwrap_or("").to_string();
                let k = format!("{}|{}|{}", s("iface"), s("chassisId"), s("portId"));
                if !st.lldp.seen.insert(k) {
                    continue;
                }
                let who = if s("name").is_empty() { s("chassisId") } else { s("name") };
                let port = if s("portDescription").is_empty() { s("portId") } else { s("portDescription") };
                events.push((
                    format!("{} is plugged into {who} {port} ({})", s("iface"), s("protocol")),
                    json!({ "chassis": s("chassisId"), "port": s("portId") }),
                ));
            }
            st.lldp.neighbors = neighbors;
        }
        for (t, d) in events {
            self.event(&t, Some(d));
        }
        self.changed();
    }

    /// lldpd joins the LLDP multicast group with SIOCADDMULTI, which isn't counted per
    /// socket: when a second lldpd on the same NIC (another BenchBob instance) exits, its
    /// SIOCDELMULTI drops the group for ours too and we go deaf (ISSUES.md). Once lldpd has
    /// been up for two minutes, a linked port it watches without the group means that
    /// happened: restart it, at most every ten minutes.
    async fn lldp_heal(self: &Arc<Self>) {
        let deaf: Vec<String> = {
            let st = self.st.lock().unwrap();
            if st.lldp.seq == 0 || now_ms().saturating_sub(st.lldp.since) < 120_000 || now_ms().saturating_sub(st.lldp.last_heal) < 600_000
            {
                return;
            }
            let mc = read_text("/proc/net/dev_mcast");
            if mc.is_empty() {
                return;
            }
            st.ifaces
                .values()
                .filter(|i| i.carrier && (i.name.starts_with("eth") || i.name.starts_with("en") || i.name.starts_with("usb")))
                .filter(|i| !parse::has_lldp_multicast(&mc, &i.name))
                .map(|i| i.name.clone())
                .collect()
        };
        if deaf.is_empty() {
            return;
        }
        self.st.lock().unwrap().lldp.last_heal = now_ms();
        self.event(&format!("lldpd lost the LLDP multicast group on {}; restarting it", deaf.join(", ")), None);
        self.lldp_stop().await;
        self.lldp_start().await;
    }

    // ---------- iperf3 server ----------

    /// An iperf3 server left by a BenchBob that died hard would hold the port; ours are
    /// recognisable by their exact command line.
    async fn server_kill_stale(&self) {
        let pat = format!("^\\S*iperf3 -s -p {} --json-stream --forceflush$", js_string(&self.iperf_port()));
        let r = run(&["pgrep", "-f", "--", &pat], Opts::default()).await;
        let pids: Vec<&str> = r.out.split('\n').filter(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())).collect();
        if !pids.is_empty() {
            self.ctx().log(&format!("killing stale iperf3 server {}", pids.join(" ")));
            let mut argv = vec!["kill"];
            argv.extend(pids.iter());
            run(&argv, Opts::default()).await;
        }
    }

    fn push_test(st: &mut State, t: Value) {
        st.server.current = None;
        st.server.tests.insert(0, t);
        st.server.tests.truncate(20);
    }

    pub(crate) fn server_start(self: &Arc<Self>) {
        if self.st.lock().unwrap().server.seq != 0 {
            return;
        }
        let port = js_number(&self.iperf_port()) as i64;
        let mut cmd = tokio::process::Command::new(bin("iperf3"));
        cmd.args(["-s", "-p", &port.to_string(), "--json-stream", "--forceflush"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        {
            let mut st = self.st.lock().unwrap();
            st.server.port = port;
            st.server.log.clear();
            st.server.started_at = now_ms();
        }
        let mut child = match self.ctx().spawn(&mut cmd) {
            Ok(c) => c,
            Err(e) => {
                tail(&mut self.st.lock().unwrap().server.log, &format!("iperf3: {e}"), 20);
                return;
            }
        };
        {
            let mut st = self.st.lock().unwrap();
            st.server.seq = seq;
            st.server.pid = child.child.id();
        }
        let me = self.clone();
        tokio::spawn(async move {
            let (out, err) = (child.child.stdout.take(), child.child.stderr.take());
            let m1 = me.clone();
            let m2 = me.clone();
            let mut start = Value::Null;
            let mut peer = String::new();
            tokio::join!(
                lines(out, |line| {
                    let line = line.trim();
                    if line.is_empty() {
                        return;
                    }
                    let mut st = m1.st.lock().unwrap();
                    let Ok(ev) = serde_json::from_str::<Value>(line) else {
                        tail(&mut st.server.log, line, 20);
                        return;
                    };
                    let d = ev.get("data").cloned().unwrap_or(Value::Null);
                    match ev.get("event").and_then(Value::as_str) {
                        Some("start") => {
                            peer = match d.get("connected").and_then(|c| c.get(0)) {
                                Some(cn) => format!("{}:{}", js_string(&cn["remote_host"]), js_string(&cn["remote_port"])),
                                None => "?".into(),
                            };
                            let ts = d.get("test_start");
                            let mut cur = Map::new();
                            cur.insert("peer".into(), json!(peer));
                            cur.insert("started".into(), json!(now_ms()));
                            cur.insert("bps".into(), json!(0));
                            cur.insert(
                                "protocol".into(),
                                json!(
                                    ts.and_then(|t| t.get("protocol")).filter(|p| truthy(p)).map(js_string).unwrap_or_else(|| "TCP".into())
                                ),
                            );
                            cur.insert("reverse".into(), json!(ts.and_then(|t| t.get("reverse")).is_some_and(truthy)));
                            st.server.current = Some(cur);
                            start = d;
                        }
                        Some("interval") => {
                            if let Some(c) = st.server.current.as_mut() {
                                let b =
                                    d.get("sum").and_then(|s| s.get("bits_per_second")).filter(|b| truthy(b)).cloned().unwrap_or(json!(0));
                                c.insert("bps".into(), b);
                            }
                        }
                        Some("end") => {
                            let s = parse::iperf_summary(&d, &start, true);
                            Inner::push_test(&mut st, json!({ "t": now_ms(), "peer": peer, "summary": s, "error": "" }));
                        }
                        Some("error") => {
                            // A client that vanishes mid-test; the server carries on listening.
                            let msg = js_string(&d);
                            if st.server.current.is_some() {
                                Inner::push_test(&mut st, json!({ "t": now_ms(), "peer": peer, "summary": null, "error": msg }));
                            } else {
                                tail(&mut st.server.log, &msg, 20);
                            }
                        }
                        _ => {}
                    }
                    drop(st);
                    m1.changed();
                }),
                lines(err, |l| tail(&mut m2.st.lock().unwrap().server.log, &l, 20)),
            );
            let code = exit_code(child.child.wait().await);
            {
                let mut st = me.st.lock().unwrap();
                if st.server.seq != seq {
                    return;
                }
                st.server.seq = 0;
                st.server.pid = None;
                st.server.current = None;
                tail(&mut st.server.log, &format!("iperf3 server exited ({code})"), 20);
            }
            me.changed();
        });
        // "Temporary": an iperf server left open forever is a surprise for whoever finds it.
        let mins = js_number(&self.iperf_auto_off_min());
        let mut st = self.st.lock().unwrap();
        st.server.off_timer = None;
        st.server.off_at = if mins > 0.0 { now_ms() + (mins * 60_000.0) as u64 } else { 0 };
        if mins > 0.0 {
            let me = self.clone();
            st.server.off_timer = Some(Task(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs_f64(mins * 60.0)).await;
                me.event(&format!("iperf3 server stopped automatically after {} min", benchbob_core::http::js_string(&num(mins))), None);
                // Detached: server_stop drops this very timer.
                tokio::spawn(async move { me.server_stop() });
            })));
        }
        drop(st);
        self.event(&format!("iperf3 server listening on port {port}"), None);
    }

    pub(crate) fn server_stop(&self) {
        let pid = {
            let mut st = self.st.lock().unwrap();
            let s = &mut st.server;
            s.off_timer = None;
            s.off_at = 0;
            s.seq = 0;
            s.current = None;
            s.pid.take()
        };
        term(pid);
        self.changed();
    }

    // ---------- iperf3 client ----------

    pub(crate) fn start_run(self: &Arc<Self>, a: &Value) -> Result<String, HttpError> {
        if let Some(r) = self.st.lock().unwrap().runs.first().filter(|r| r.status == "running") {
            return Err(HttpError::new(409, format!("An iperf3 run to {} is already going", r.host)));
        }
        let host = parse::valid_host_value(a.get("host")).map_err(HttpError::bad_request)?;
        let given = |k: &str| a.get(k).filter(|v| !v.is_null()).map(js_number);
        let port = given("port").unwrap_or(5201.0);
        let duration = given("duration").unwrap_or(5.0);
        let parallel = given("parallel").unwrap_or(1.0);
        let int = |x: f64| x.is_finite() && x.fract() == 0.0;
        if !int(port) || !(1.0..=65535.0).contains(&port) {
            return Err(HttpError::bad_request("port must be 1–65535"));
        }
        if !(1.0..=60.0).contains(&duration) {
            return Err(HttpError::bad_request("duration must be 1–60 s"));
        }
        if !int(parallel) || !(1.0..=16.0).contains(&parallel) {
            return Err(HttpError::bad_request("parallel must be 1–16"));
        }
        let bitrate = a.get("bitrate").filter(|b| truthy(b)).map(js_string).unwrap_or_default();
        if !bitrate.is_empty() && !regex::Regex::new(r"(?i)^\d+(\.\d+)?[KMG]?$").unwrap().is_match(&bitrate) {
            return Err(HttpError::bad_request("bitrate looks like \"100M\", \"1G\" or \"500K\""));
        }
        let udp = a.get("udp").is_some_and(truthy);
        let reverse = a.get("reverse").is_some_and(truthy);
        let login = self.iperf_auths().get(&host).cloned();
        let lf = |k: &str| login.as_ref().and_then(|l| l.get(k)).filter(|v| truthy(v)).map(js_string);
        let auth = lf("username").is_some() && lf("password").is_some() && lf("publicKey").is_some();
        let bitrate = if !bitrate.is_empty() {
            bitrate
        } else if udp {
            "1M".into()
        } else {
            String::new()
        };
        let (port, parallel) = (port as i64, parallel as i64);
        let mut argv: Vec<String> = vec![bin("iperf3"), "-c".into(), host.clone(), "-p".into(), port.to_string(), "-t".into()];
        argv.extend([
            benchbob_core::http::js_string(&num(duration)),
            "-P".into(),
            parallel.to_string(),
            "--json-stream".into(),
            "--forceflush".into(),
            "--connect-timeout".into(),
            "3000".into(),
        ]);
        if reverse {
            argv.push("-R".into());
        }
        if udp {
            argv.push("-u".into());
        }
        if !bitrate.is_empty() {
            argv.extend(["-b".into(), bitrate.clone()]);
        }
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if auth {
            // iperf3 reads the key from a file and the password from the environment, never argv.
            let dir = iperf_key_dir();
            let key_path = format!("{dir}/{}.pem", &parse::sha256_hex(&host)[..16]);
            write_private(&dir, &key_path, lf("publicKey").unwrap_or_default().as_bytes())
                .map_err(|e| HttpError::new(500, format!("iperf3 key file: {e}")))?;
            cmd.args(["--username", &lf("username").unwrap_or_default(), "--rsa-public-key-path", &key_path]);
            if login.as_ref().and_then(|l| l.get("pkcs1")).is_some_and(truthy) {
                cmd.arg("--use-pkcs1-padding");
            }
            cmd.env("IPERF3_PASSWORD", lf("password").unwrap_or_default());
        }
        let mut child = self.ctx().spawn(&mut cmd).map_err(|e| HttpError::new(500, format!("iperf3: {e}")))?;
        let id = radix36(now_ms());
        let (tx, rx) = watch::channel(false);
        let pid = child.child.id();
        {
            let mut st = self.st.lock().unwrap();
            st.runs.insert(
                0,
                ClientRun {
                    id: id.clone(),
                    host: host.clone(),
                    port,
                    duration,
                    reverse,
                    udp,
                    bitrate,
                    parallel,
                    auth,
                    started: now_ms(),
                    status: "running",
                    points: Vec::new(),
                    result: None,
                    error: String::new(),
                    pid,
                    done: rx,
                },
            );
            st.runs.truncate(20);
        }
        let me = self.clone();
        let rid = id.clone();
        tokio::spawn(async move {
            let killer = Task(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs_f64(duration + 20.0)).await;
                term(pid);
            }));
            let (out, err) = (child.child.stdout.take(), child.child.stderr.take());
            let m1 = me.clone();
            let mut start = Value::Null;
            let mut stderr = String::new();
            tokio::join!(
                lines(out, |line| {
                    let Ok(ev) = serde_json::from_str::<Value>(line.trim()) else { return };
                    let d = ev.get("data").cloned().unwrap_or(Value::Null);
                    let mut st = m1.st.lock().unwrap();
                    let Some(r) = st.runs.iter_mut().find(|r| r.id == rid) else { return };
                    match ev.get("event").and_then(Value::as_str) {
                        Some("start") => start = d,
                        Some("interval") => {
                            let sum = d.get("sum");
                            let mut p = Map::new();
                            p.insert(
                                "t".into(),
                                sum.and_then(|s| s.get("end")).filter(|v| !v.is_null()).cloned().unwrap_or(json!(r.points.len() + 1)),
                            );
                            p.insert(
                                "bps".into(),
                                sum.and_then(|s| s.get("bits_per_second")).filter(|v| truthy(v)).cloned().unwrap_or(json!(0)),
                            );
                            if let Some(rt) = sum.and_then(|s| s.get("retransmits")) {
                                p.insert("retransmits".into(), rt.clone());
                            }
                            r.points.push(Value::Object(p));
                        }
                        Some("end") => r.result = Some(parse::iperf_summary(&d, &start, false)),
                        Some("error") => r.error = js_string(&d),
                        _ => {}
                    }
                    drop(st);
                    m1.changed();
                }),
                lines(err, |l| {
                    stderr.push_str(&l);
                    stderr.push('\n');
                }),
            );
            let _ = child.child.wait().await;
            drop(killer);
            let msg = {
                let mut st = me.st.lock().unwrap();
                let Some(r) = st.runs.iter_mut().find(|r| r.id == rid) else { return };
                r.status = if r.result.is_some() && r.error.is_empty() { "done" } else { "error" };
                // iperf3 still emits an (all-zero) "end" after a connect failure; don't present
                // it as a result.
                if r.status == "error" {
                    r.result = None;
                    let raw = if r.error.is_empty() { stderr.trim().to_string() } else { r.error.clone() };
                    r.error = parse::iperf_error(&raw, &r.host, r.port);
                }
                r.pid = None;
                let what = match &r.result {
                    Some(res) => {
                        format!("{}{}", parse::fmt_bps(res["bps"].as_f64().unwrap_or(0.0)), if r.reverse { " (reverse)" } else { "" })
                    }
                    None => r.error.clone(),
                };
                format!("iperf3 to {}: {what}", r.host)
            };
            me.event(&msg, None);
            let _ = tx.send(true);
            me.changed();
        });
        self.changed();
        Ok(id)
    }

    pub(crate) fn public_run(&self, id: &str) -> Option<Value> {
        self.st.lock().unwrap().runs.iter().find(|r| r.id == id).map(ClientRun::public)
    }

    /// Wait for a run to finish (or `limit` to pass).
    pub(crate) async fn wait_run(&self, id: &str, limit: Option<Duration>) {
        let rx = self.st.lock().unwrap().runs.iter().find(|r| r.id == id).map(|r| r.done.clone());
        let Some(mut rx) = rx else { return };
        let w = rx.wait_for(|d| *d);
        match limit {
            Some(l) => {
                let _ = tokio::time::timeout(l, w).await;
            }
            None => {
                let _ = w.await;
            }
        }
    }

    pub(crate) fn run_status(&self, id: &str) -> &'static str {
        self.st.lock().unwrap().runs.iter().find(|r| r.id == id).map(|r| r.status).unwrap_or("error")
    }

    pub(crate) fn stop_run(&self, id: &str) -> bool {
        let st = self.st.lock().unwrap();
        let Some(r) = st.runs.iter().find(|r| r.id == id) else { return false };
        term(r.pid);
        true
    }

    // ---------- Quick checks ----------

    fn default_gateway(st: &State) -> String {
        st.gateways.iter().find(|g| st.ifaces.contains_key(&g.dev)).or(st.gateways.first()).map(|g| g.gateway.clone()).unwrap_or_default()
    }

    pub(crate) async fn ping(&self, host_arg: Option<&Value>, count: f64) -> Result<Value, HttpError> {
        let target = match host_arg.filter(|h| truthy(h)) {
            None => "gateway".to_string(),
            Some(h) if js_string(h) == "gateway" => "gateway".into(),
            Some(h) => parse::valid_host(&js_string(h)).map_err(HttpError::bad_request)?,
        };
        let host = if target == "gateway" { Self::default_gateway(&self.st.lock().unwrap()) } else { target.clone() };
        if host.is_empty() {
            return Err(HttpError::new(409, "No default gateway to ping; pass a host"));
        }
        if !(1.0..=50.0).contains(&count) {
            return Err(HttpError::bad_request("count must be 1–50"));
        }
        let c = benchbob_core::http::js_string(&num(count));
        let r = run(&["ping", "-c", &c, "-i", "0.2", "-W", "1", &host], Opts::timeout_ms((count * 1200.0) as u64 + 3000)).await;
        let res = parse::parse_ping(&format!("{}\n{}", r.out, r.err), &host);
        if res.sent == 0 {
            let first = if r.err.is_empty() { &r.out } else { &r.err };
            let first = first.split('\n').next().unwrap_or("");
            return Err(HttpError::bad_request(if first.is_empty() { "ping failed" } else { first }));
        }
        let mut m = res.to_json();
        m.insert("t".into(), json!(now_ms()));
        m.insert("target".into(), json!(target));
        let v = Value::Object(m);
        {
            let mut st = self.st.lock().unwrap();
            let mut pings = vec![v.clone()];
            if let Some(Value::Array(old)) = st.checks.get("pings") {
                pings.extend(old.iter().filter(|p| p["target"] != json!(target)).cloned());
            }
            pings.truncate(12);
            st.checks.insert("ping".into(), v.clone());
            st.checks.insert("pings".into(), Value::Array(pings));
        }
        self.changed();
        Ok(v)
    }

    pub(crate) async fn dns_check(&self, names_arg: Option<&Vec<Value>>) -> Result<Value, HttpError> {
        let names: Vec<String> = match names_arg.filter(|n| !n.is_empty()) {
            Some(n) => n.iter().take(10).map(|x| parse::valid_host(&js_string(x))).collect::<Result<_, _>>(),
            None => ["google.com", "cloudflare.com", "debian.org"].iter().map(|s| parse::valid_host(s)).collect(),
        }
        .map_err(HttpError::bad_request)?;
        let servers = {
            let st = self.st.lock().unwrap();
            if st.nameservers.is_empty() { vec!["127.0.0.53".to_string()] } else { st.nameservers.clone() }
        };
        // Query each nameserver directly, so the timing is the server's, not a cache's.
        let jobs = servers.iter().take(4).flat_map(|server| {
            names.iter().map(move |name| async move {
                let t0 = Instant::now();
                match dns::resolve4(server, name, Duration::from_millis(2000)).await {
                    Ok(addresses) => {
                        let ms = benchbob_core::http::round_to(t0.elapsed().as_secs_f64() * 1000.0, 1);
                        (server.clone(), name.clone(), num(ms), addresses, String::new())
                    }
                    Err(e) => (server.clone(), name.clone(), Value::Null, Vec::new(), e),
                }
            })
        });
        let mut results = futures_util::future::join_all(jobs).await;
        let idx = |n: &str| names.iter().position(|x| x == n).unwrap_or(0);
        results.sort_by(|a, b| parse_natural(&a.0, &b.0).then(idx(&a.1).cmp(&idx(&b.1))));
        let results: Vec<Value> = results
            .into_iter()
            .map(|(server, name, ms, addresses, error)| json!({ "server": server, "name": name, "ms": ms, "addresses": addresses, "error": error }))
            .collect();
        let v = json!({ "t": now_ms(), "results": results });
        self.st.lock().unwrap().checks.insert("dns".into(), v.clone());
        self.changed();
        Ok(v)
    }

    pub(crate) async fn http_check(&self, urls_arg: Option<&Vec<Value>>) -> Value {
        let urls: Vec<Value> = match urls_arg.filter(|u| !u.is_empty()) {
            Some(u) => u.iter().take(6).cloned().collect(),
            None => [
                "http://connectivitycheck.gstatic.com/generate_204",
                "https://www.cloudflare.com/cdn-cgi/trace",
                "https://deb.debian.org/debian/",
            ]
            .iter()
            .map(|s| json!(s))
            .collect(),
        };
        let results = futures_util::future::join_all(urls.into_iter().map(|url| async move {
            let u = js_string(&url);
            if !(u.starts_with("http://") || u.starts_with("https://")) {
                return json!({ "url": url, "status": null, "ms": null, "error": "not an http(s) URL" });
            }
            match http_probe(&u, Duration::from_secs(6)).await {
                Ok((status, ms)) => json!({ "url": url, "status": status, "ms": ms, "error": "" }),
                Err(e) => json!({ "url": url, "status": null, "ms": null, "error": e }),
            }
        }))
        .await;
        let v = json!({ "t": now_ms(), "results": results });
        self.st.lock().unwrap().checks.insert("http".into(), v.clone());
        self.changed();
        v
    }

    /// Largest packet that gets through unfragmented: binary search on `ping -M do -s`.
    pub(crate) async fn mtu_probe(&self, host_arg: Option<&Value>) -> Result<Value, HttpError> {
        let host = match host_arg.filter(|h| truthy(h)) {
            Some(h) => parse::valid_host(&js_string(h)).map_err(HttpError::bad_request)?,
            None => "1.1.1.1".into(),
        };
        let (dev, if_mtu) = {
            let st = self.st.lock().unwrap();
            let dev = st.gateways.iter().find(|g| st.ifaces.contains_key(&g.dev)).map(|g| g.dev.clone());
            let m = dev.as_ref().and_then(|d| st.ifaces.get(d)).map(|i| i.mtu).filter(|m| *m != 0).unwrap_or(1500);
            (dev, m)
        };
        let h = host.clone();
        let (mtu, steps) = parse::mtu_search(
            |payload| {
                let h = h.clone();
                async move {
                    let p = payload.to_string();
                    let r = run(&["ping", "-M", "do", "-s", &p, "-c", "1", "-W", "1", &h], Opts::timeout_ms(3000)).await;
                    parse::parse_ping(&format!("{}\n{}", r.out, r.err), "").received > 0
                }
            },
            if_mtu,
        )
        .await;
        let note = match mtu {
            None => format!("{host} doesn't answer ping"),
            Some(m) if m < if_mtu => {
                format!("Something on the path to {host} has a smaller MTU than {}", dev.as_deref().unwrap_or("undefined"))
            }
            Some(_) => "Full-size packets get through".into(),
        };
        let v = json!({
            "t": now_ms(), "host": host, "interfaceMtu": if_mtu, "pathMtu": mtu,
            "steps": steps.iter().map(|(size, ok)| json!({ "size": size, "ok": ok })).collect::<Vec<_>>(), "note": note,
        });
        self.st.lock().unwrap().checks.insert("mtu".into(), v.clone());
        self.changed();
        Ok(v)
    }

    // ---------- Presentation ----------

    fn public_iface(st: &State, i: &Iface) -> Value {
        let neighbor = st.lldp.neighbors.iter().find(|n| n["iface"] == json!(i.name)).cloned().unwrap_or(Value::Null);
        let info = |k: &str| i.info.get(k).filter(|s| !s.is_empty()).cloned();
        json!({
            "name": i.name, "usb": i.usb, "usbDesc": i.usb_desc, "mac": i.mac, "operstate": i.operstate, "carrier": i.carrier,
            "carrierChanges": i.carrier_changes, "mtu": i.mtu,
            "driver": info("driver").unwrap_or_else(|| i.net_driver.clone()), "driverVersion": info("version").unwrap_or_default(),
            "firmware": info("firmware-version").unwrap_or_default(), "bus": info("bus-info").unwrap_or_default(),
            "link": i.link, "eee": i.eee,
            "errors": i.errors.iter().map(|(n, v, d)| json!({ "name": n, "value": v, "delta": d })).collect::<Vec<_>>(),
            "addrs": i.addrs, "dns": i.dns,
            "gateway": st.gateways.iter().find(|g| g.dev == i.name).map(|g| g.gateway.clone()).unwrap_or_default(),
            "neighbor": neighbor,
        })
    }

    fn speed_text(i: &Iface) -> String {
        if !i.carrier {
            return format!("{} no link", i.name);
        }
        let sp = match i.link.as_ref().and_then(|l| l.speed) {
            None => String::new(),
            Some(s) if s >= 1000 => format!("{} Gb/s", js_string(&num(s as f64 / 1000.0))),
            Some(s) => format!("{s} Mb/s"),
        };
        format!("{} {sp} {}", i.name, i.link.as_ref().map(|l| l.duplex.to_lowercase()).unwrap_or_default()).trim().to_string()
    }

    fn gateways_json(st: &State) -> Value {
        Value::Array(st.gateways.iter().map(|g| json!({ "dev": g.dev, "gateway": g.gateway, "metric": g.metric })).collect())
    }

    fn dns_json(st: &State) -> Value {
        json!({ "nameservers": st.nameservers, "search": st.search })
    }

    /// `{ ifaces, gateways, dns }`, optionally just one interface.
    pub(crate) fn link_view(&self, only: Option<&str>) -> Result<Value, HttpError> {
        let st = self.st.lock().unwrap();
        let list: Vec<Value> =
            st.ifaces.values().filter(|i| only.is_none_or(|o| o == i.name)).map(|i| Self::public_iface(&st, i)).collect();
        if let Some(o) = only
            && list.is_empty()
        {
            let have: Vec<&str> = st.ifaces.keys().map(String::as_str).collect();
            return Err(HttpError::new(500, format!("No wired interface \"{o}\" (have: {})", have.join(", "))));
        }
        Ok(json!({ "ifaces": list, "gateways": Self::gateways_json(&st), "dns": Self::dns_json(&st) }))
    }

    pub(crate) fn iface_stats(&self, name: &str) -> Result<Value, HttpError> {
        let st = self.st.lock().unwrap();
        let i = st.ifaces.get(name).ok_or_else(|| HttpError::new(404, format!("No wired interface \"{name}\"")))?;
        Ok(json!({ "iface": i.name, "stats": i.stats }))
    }

    pub(crate) fn neighbors_view(&self) -> Value {
        let st = self.st.lock().unwrap();
        json!({ "enabled": self.lldp_enabled(), "listening": st.lldp.seq != 0, "since": st.lldp.since, "neighbors": st.lldp.neighbors, "error": st.lldp.error })
    }

    pub(crate) fn neighbors_tool(&self) -> Value {
        let st = self.st.lock().unwrap();
        let since = st.lldp.since;
        let mut m = Map::new();
        m.insert("listening".into(), json!(st.lldp.seq != 0));
        m.insert(
            "listeningForSeconds".into(),
            json!(if since > 0 { ((now_ms().saturating_sub(since)) as f64 / 1000.0).round() as i64 } else { 0 }),
        );
        m.insert("neighbors".into(), json!(st.lldp.neighbors));
        if !truthy(&self.lldp_enabled()) {
            m.insert("note".into(), json!("LLDP listening is turned off in BenchBob's settings"));
        }
        Value::Object(m)
    }

    pub(crate) fn auth_view(host: &str, a: &Map<String, Value>) -> Map<String, Value> {
        let s = |k: &str| a.get(k).filter(|v| truthy(v)).map(js_string);
        let mut m = Map::new();
        m.insert("host".into(), json!(host));
        m.insert("username".into(), a.get("username").cloned().unwrap_or(Value::Null));
        m.insert("pkcs1".into(), json!(a.get("pkcs1").is_some_and(truthy)));
        m.insert("hasPassword".into(), json!(s("password").is_some()));
        m.insert("hasKey".into(), json!(s("publicKey").is_some()));
        m.insert("ready".into(), json!(s("username").is_some() && s("password").is_some() && s("publicKey").is_some()));
        m.insert(
            "usersLine".into(),
            json!(match s("password") {
                Some(p) => parse::iperf_auth_line(&a.get("username").map(js_string).unwrap_or_default(), &p),
                None => String::new(),
            }),
        );
        m
    }

    pub(crate) fn status(&self) -> Value {
        let st = self.st.lock().unwrap();
        let list: Vec<&Iface> = st.ifaces.values().collect();
        let running = st.runs.first().is_some_and(|r| r.status == "running");
        let main = list.iter().find(|i| st.gateways.iter().any(|g| g.dev == i.name)).or(list.first());
        let n = main.and_then(|m| st.lldp.neighbors.iter().find(|x| x["iface"] == json!(m.name)));
        let mut parts = vec![main.map(|m| Self::speed_text(m)).unwrap_or_else(|| "no wired interface".into())];
        if let Some(n) = n {
            let s = |k: &str| n[k].as_str().unwrap_or("").to_string();
            let who = if s("name").is_empty() { s("chassisId") } else { s("name") };
            let port = if s("portDescription").is_empty() { s("portId") } else { s("portDescription") };
            parts.push(format!("{who} {port}").trim().to_string());
        }
        if running {
            parts.push(format!("iperf3 → {}", st.runs[0].host));
        }
        if let Some(c) = &st.server.current {
            parts.push(format!("iperf3 test from {}", c.get("peer").map(js_string).unwrap_or_default()));
        }
        let srv = &st.server;
        let log: Vec<String> = if srv.seq != 0 { Vec::new() } else { srv.log[srv.log.len().saturating_sub(3)..].to_vec() };
        let auths: Vec<Value> =
            self.iperf_auths().iter().map(|(h, a)| Value::Object(Self::auth_view(h, a.as_object().unwrap_or(&Map::new())))).collect();
        json!({
            "active": !st.lldp.neighbors.is_empty() || running || srv.current.is_some() || list.iter().any(|i| i.usb),
            "summary": parts.join(" · "),
            "ifaces": list.iter().map(|i| Self::public_iface(&st, i)).collect::<Vec<_>>(),
            "gateways": Self::gateways_json(&st),
            "dns": Self::dns_json(&st),
            "lldp": { "enabled": self.lldp_enabled(), "running": st.lldp.seq != 0, "since": st.lldp.since, "neighbors": st.lldp.neighbors, "error": st.lldp.error },
            "iperf": {
                "server": {
                    "running": srv.seq != 0, "port": if srv.seq != 0 { json!(srv.port) } else { self.iperf_port() }, "startedAt": srv.started_at,
                    "offAt": srv.off_at, "current": srv.current, "tests": srv.tests, "log": log,
                },
                "runs": st.runs.iter().take(10).map(ClientRun::public).collect::<Vec<_>>(),
            },
            "checks": st.checks,
            "presets": { "iperf": self.iperf_presets(), "ping": self.ping_targets(), "auth": auths },
        })
    }
}

/// `a.localeCompare(b)` for nameserver addresses: plain ordering is close enough (they're
/// IP literals).
fn parse_natural(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

/// `Date.now().toString(36)`
fn radix36(mut n: u64) -> String {
    const D: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(D[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// Write a file only we can read, in a directory only we can enter.
fn write_private(dir: &str, path: &str, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    std::io::Write::write_all(&mut f, data)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// HTTP(S) reachability with curl (no TLS stack in this binary): the status code and the
/// time until the response headers arrived, redirects not followed. curl is stopped once
/// the headers are in, so a large body doesn't count (fetch() resolved at the headers too).
pub(crate) async fn http_probe(url: &str, timeout: Duration) -> Result<(u16, i64), String> {
    let t0 = Instant::now();
    let mut cmd = tokio::process::Command::new("curl");
    cmd.args(["-sS", "-o", "/dev/null", "-D", "-", "--max-time", &timeout.as_secs().to_string(), "--url", url])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = benchbob_core::child::CommandExt::spawn_guarded(&mut cmd)
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { "curl isn't installed".to_string() } else { e.to_string() })?;
    let out = child.stdout.take();
    let err = child.stderr.take();
    let headers = async {
        let mut status: Option<u16> = None;
        let out = out?;
        let mut l = BufReader::new(out).lines();
        while let Ok(Some(line)) = l.next_line().await {
            let line = line.trim_end_matches('\r');
            if line.starts_with("HTTP/") {
                status = line.split_whitespace().nth(1).and_then(|s| s.parse().ok());
            } else if line.is_empty() && status.is_some() {
                return status;
            }
        }
        None
    };
    let got = tokio::time::timeout(timeout + Duration::from_secs(1), headers).await.ok().flatten();
    if let Some(s) = got {
        let ms = t0.elapsed().as_millis() as i64;
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Ok((s, ms));
    }
    let _ = child.start_kill();
    let status = child.wait().await.ok();
    let mut text = String::new();
    if let Some(mut e) = err {
        let _ = tokio::io::AsyncReadExt::read_to_string(&mut e, &mut text).await;
    }
    if status.and_then(|s| s.code()) == Some(28) || t0.elapsed() >= timeout {
        return Err(format!("timed out after {} s", timeout.as_secs()));
    }
    let line = text.lines().next().unwrap_or("").trim();
    let line = regex::Regex::new(r"^curl: \(\d+\) ").unwrap().replace(line, "").to_string();
    Err(if line.is_empty() { "no response".into() } else { line })
}

#[async_trait]
impl Driver for Netdiag {
    fn id(&self) -> &'static str {
        "netdiag"
    }
    fn title(&self) -> &'static str {
        "Wired network"
    }
    fn order(&self) -> i32 {
        80
    }
    fn description(&self) -> &'static str {
        "Ethernet diagnostics: link/ethtool details and errors, LLDP/CDP switch neighbour (receive-only), iperf3 server and client, ping/DNS/HTTP/MTU checks"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let i = &self.inner;
        let _ = i.ctx.set(ctx);
        let epoch = i.epoch.load(Ordering::SeqCst);
        i.rescan_ifaces();
        i.poll_sysfs().await;
        let me = i.clone();
        tokio::spawn(async move { me.poll_all().await });
        // Carrier is checked often (a link change triggers a full poll at once); the ethtool
        // view is otherwise near-static, so it refreshes once a minute.
        let every = |secs: u64, f: fn(Arc<Inner>) -> futures_util::future::BoxFuture<'static, ()>| {
            let me = i.clone();
            Task(tokio::spawn(async move {
                let mut iv = tokio::time::interval(Duration::from_secs(secs));
                iv.tick().await;
                loop {
                    iv.tick().await;
                    if me.epoch.load(Ordering::SeqCst) != epoch {
                        return;
                    }
                    f(me.clone()).await;
                }
            }))
        };
        let loops = vec![
            every(5, |me| Box::pin(async move { me.poll_sysfs().await })),
            every(60, |me| {
                Box::pin(async move {
                    me.poll_all().await;
                    me.lldp_heal().await;
                })
            }),
            // Safety net only: lldp_watch() picks up changes as they happen.
            every(300, |me| Box::pin(async move { me.lldp_query().await })),
        ];
        i.loops.lock().unwrap().extend(loops);
        i.server_kill_stale().await;
        if truthy(&i.lldp_enabled()) {
            let me = i.clone();
            tokio::spawn(async move { me.lldp_start().await });
        }
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) {
        let i = &self.inner;
        i.epoch.fetch_add(1, Ordering::SeqCst);
        i.loops.lock().unwrap().clear();
        i.server_stop();
        let pids: Vec<Option<u32>> = i.st.lock().unwrap().runs.iter().map(|r| r.pid).collect();
        for p in pids {
            term(p);
        }
        i.lldp_stop().await;
        i.st.lock().unwrap().ifaces.clear();
    }

    fn udev(&self, ev: &UdevEvent) {
        if ev.props.get("SUBSYSTEM").map(String::as_str) == Some("net") && matches!(ev.action.as_str(), "add" | "remove" | "move") {
            self.inner.rescan_ifaces();
        }
    }

    fn status(&self) -> Option<Json> {
        Some(self.inner.status())
    }

    fn routes(&self) -> Vec<Route> {
        routes::routes(&self.inner)
    }

    fn tools(&self) -> Vec<Tool> {
        routes::tools(&self.inner)
    }
}
