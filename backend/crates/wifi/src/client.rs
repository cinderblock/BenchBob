//! The Wi-Fi client test runner (peripherals/lib/wifi-client.ts).
//!
//! Associates a radio to a network inside its own network namespace, so the test's address,
//! routes and DNS can never compete with eth0 (or wlan0's own NetworkManager connection) in
//! the main namespace. Sequence:
//!
//!   netns benchbob-wt-<phy index> → move the whole phy in (iw phy … set netns) →
//!   wpa_supplicant (nl80211, generated conf) → udhcpc / static → ping, DNS, HTTP, iperf3
//!   → cleanup: kill children, phy back to netns 1, delete the netns →
//!   make sure NetworkManager reconnects whatever it had before.
//!
//! Everything is timestamped into a timeline the dashboard streams while the test runs.
//! Secrets: the key is written only into a 0600 conf in a 0700 temp dir that's deleted in
//! cleanup, and every wpa_supplicant line is redacted before it's stored (a malformed key
//! line makes wpa_supplicant echo the key in its parse error; seen on desky).

use std::collections::HashSet;
use std::future::Future;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use benchbob_core::Ctx;
use benchbob_core::run::{Opts, Output, run};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::{mpsc, watch};

use crate::json::{O, fmt_num, fmt_opt, fmt_opt_s, now_ms};
use crate::parse::{self as p, LinkInfo, WpaEvent};

pub const NS_PREFIX: &str = "benchbob-wt-";
pub const TMP_PREFIX: &str = "benchbob-wifi-";
/// In /etc/netns/<ns>/: the pid of the BenchBob process running that test.
pub const OWNER_FILE: &str = "benchbob-owner";
/// Names from before the rename, still cleaned up after a crash.
pub const LEGACY_NS: &str = "deskypi-wt-";
pub const LEGACY_TMP: &str = "deskypi-wifi-";
pub const LEGACY_OWNER: &str = "deskypi-owner";

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// A spawned child's output streams and a receiver for its exit code.
type NsChild = (Option<tokio::process::ChildStdout>, Option<tokio::process::ChildStderr>, watch::Receiver<Option<i32>>);
/// pid, output streams, and the wait for its exit code.
type Spawned = (u32, Option<tokio::process::ChildStdout>, Option<tokio::process::ChildStderr>, BoxFut<i32>);

/// Is another live process using this pid? (EPERM: alive but not ours to signal.)
pub fn pid_alive(pid: i64) -> bool {
    if pid <= 0 || pid > i32::MAX as i64 || pid == std::process::id() as i64 {
        return false;
    }
    // SAFETY: kill(2) with signal 0 only checks for existence and permission.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The tools, resolved once against an explicit PATH: the service's PATH (and a non-login
/// SSH one) lacks /usr/sbin, where iw, wpa_supplicant and udhcpc live.
#[derive(Debug, Clone)]
pub struct Bins {
    /// ["sudo", "-n"], or [] when already root
    pub sudo: Vec<String>,
    pub ip: String,
    pub iw: String,
    pub wpa: String,
    pub udhcpc: String,
    pub nmcli: String,
    pub ping: String,
    pub curl: String,
    pub busybox: String,
    pub iperf3: String,
    pub stdbuf: String,
    pub kill: String,
    pub rm: String,
    pub mkdir: String,
    pub tee: String,
}

impl Bins {
    pub fn resolve() -> Self {
        let w = |n: &str| {
            benchbob_core::run::which_in(n, benchbob_core::run::SYSTEM_PATH).map(|p| p.display().to_string()).unwrap_or_else(|| n.into())
        };
        // SAFETY: getuid never fails.
        let root = unsafe { libc::getuid() } == 0;
        Self {
            sudo: if root { vec![] } else { vec!["sudo".into(), "-n".into()] },
            ip: w("ip"),
            iw: w("iw"),
            wpa: w("wpa_supplicant"),
            udhcpc: w("udhcpc"),
            nmcli: w("nmcli"),
            ping: w("ping"),
            curl: w("curl"),
            busybox: w("busybox"),
            iperf3: w("iperf3"),
            stdbuf: w("stdbuf"),
            kill: w("kill"),
            rm: w("rm"),
            mkdir: w("mkdir"),
            tee: w("tee"),
        }
    }

    /// `[...sudo, ...cmd]`
    pub fn sudo_cmd(&self, cmd: &[&str]) -> Vec<String> {
        self.sudo.iter().cloned().chain(cmd.iter().map(|s| s.to_string())).collect()
    }
}

/// `run()` over owned strings.
pub async fn run_v(cmd: &[String], opts: Opts) -> Output {
    let v: Vec<&str> = cmd.iter().map(String::as_str).collect();
    run(&v, opts).await
}

pub async fn sudo(b: &Bins, cmd: &[&str], timeout_ms: u64) -> Output {
    run_v(&b.sudo_cmd(cmd), Opts::timeout_ms(timeout_ms)).await
}

// ---------- Spec ----------

#[derive(Debug, Clone, PartialEq)]
pub struct Eap {
    /// PEAP or TTLS
    pub method: &'static str,
    pub identity: String,
    pub anonymous_identity: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Static {
    pub address: String,
    pub gateway: Option<String>,
    pub dns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Iperf {
    pub server: String,
    pub port: f64,
    pub seconds: f64,
    pub reverse: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TestSpec {
    pub phy: String,
    pub ssid: String,
    /// open, owe, psk, sae, psk-sae, eap
    pub security: String,
    pub bssid: Option<String>,
    pub eap: Option<Eap>,
    /// dhcp, static, none
    pub ip: String,
    pub r#static: Option<Static>,
    pub ping_gateway: bool,
    pub dns: Vec<String>,
    pub http: Vec<String>,
    pub ping: Vec<String>,
    pub iperf: Option<Iperf>,
    pub count: f64,
    pub hold_s: f64,
    pub assoc_timeout_s: f64,
    /// given, profile, networkmanager, none
    pub key_source: Option<&'static str>,
    pub profile: Option<Value>,
}

impl TestSpec {
    /// A spec with the defaults the tests use.
    pub fn basic(ssid: &str, security: &str) -> Self {
        Self {
            phy: "phy0".into(),
            ssid: ssid.into(),
            security: security.into(),
            bssid: None,
            eap: None,
            ip: "dhcp".into(),
            r#static: None,
            ping_gateway: true,
            dns: vec![],
            http: vec![],
            ping: vec![],
            iperf: None,
            count: 3.0,
            hold_s: 0.0,
            assoc_timeout_s: 30.0,
            key_source: None,
            profile: None,
        }
    }

    pub fn eap_json(&self) -> Option<Value> {
        self.eap.as_ref().map(|e| {
            O::new().k("method", e.method).k("identity", e.identity.clone()).o("anonymousIdentity", e.anonymous_identity.clone()).v()
        })
    }
    pub fn static_json(&self) -> Option<Value> {
        self.r#static.as_ref().map(|s| O::new().k("address", s.address.clone()).o("gateway", s.gateway.clone()).k("dns", s.dns.clone()).v())
    }
    pub fn iperf_json(&self) -> Value {
        self.iperf
            .as_ref()
            .map(|i| O::new().k("server", i.server.clone()).n("port", i.port).n("seconds", i.seconds).k("reverse", i.reverse).v())
            .unwrap_or(Value::Null)
    }

    pub fn json(&self) -> Value {
        O::new()
            .k("phy", self.phy.clone())
            .k("ssid", self.ssid.clone())
            .k("security", self.security.clone())
            .o("bssid", self.bssid.clone())
            .o("eap", self.eap_json())
            .k("ip", self.ip.clone())
            .o("static", self.static_json())
            .k("pingGateway", self.ping_gateway)
            .k("dns", self.dns.clone())
            .k("http", self.http.clone())
            .k("ping", self.ping.clone())
            .k("iperf", self.iperf_json())
            .n("count", self.count)
            .n("holdS", self.hold_s)
            .n("assocTimeoutS", self.assoc_timeout_s)
            .o("keySource", self.key_source)
            .o("profile", self.profile.clone())
            .v()
    }
}

/// What the runner must put back afterwards, captured by the driver before borrowing.
#[derive(Debug, Clone, PartialEq)]
pub struct RadioCtx {
    pub phy: String,
    /// netdevs on this phy in the main namespace
    pub ifaces: Vec<String>,
    /// NM connections to see restored: (iface, connection)
    pub nm: Vec<(String, String)>,
    /// ifaces BenchBob keeps NM away from (USB dongles)
    pub unmanage: Vec<String>,
}

impl RadioCtx {
    pub fn json(&self) -> Value {
        O::new()
            .k("phy", self.phy.clone())
            .k("ifaces", self.ifaces.clone())
            .k("nm", self.nm.iter().map(|(i, c)| O::new().k("iface", i.clone()).k("connection", c.clone()).v()).collect::<Vec<_>>())
            .k("unmanage", self.unmanage.clone())
            .v()
    }
    /// From the `borrowed` setting, as an earlier process stored it.
    pub fn from_json(v: &Value) -> Self {
        let strs = |k: &str| -> Vec<String> {
            v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
        };
        let nm = v
            .get("nm")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|x| {
                        let s = |k: &str| x.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                        (s("iface"), s("connection"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { phy: v.get("phy").and_then(Value::as_str).unwrap_or("").into(), ifaces: strs("ifaces"), nm, unmanage: strs("unmanage") }
    }
}

// ---------- Validation ----------

fn is_pmk(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn valid_key(security: &str, key: Option<&str>) -> Option<String> {
    if security == "open" || security == "owe" {
        return None;
    }
    let Some(key) = key.filter(|k| !k.is_empty()) else { return Some(format!("A key (passphrase) is required for {security}")) };
    if security == "eap" {
        return None; // EAP password: any text, written as hex
    }
    let pmk = is_pmk(key);
    if security == "sae" && pmk {
        return Some("WPA3-SAE needs the passphrase itself, not a 64-hex PSK".into());
    }
    if pmk {
        return None;
    }
    let len = key.encode_utf16().count();
    if !(8..=63).contains(&len) {
        return Some("WPA passphrases are 8–63 characters".into());
    }
    if !key.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Some("WPA passphrases must be printable ASCII".into());
    }
    None
}

/// Hosts and URLs end up as argv entries; refuse anything that could be read as an option.
pub fn valid_host(h: &str) -> bool {
    p::re!(r"^[A-Za-z0-9][A-Za-z0-9.:_-]*$").is_match(h) && h.encode_utf16().count() < 254
}

/// `new URL(u)` accepts it with an http(s) scheme, and it isn't an option. A WHATWG-URL
/// approximation covering what matters here: the scheme, and a non-empty host.
pub fn valid_url(u: &str) -> bool {
    if u.starts_with('-') {
        return false;
    }
    let t: String = u.trim_matches(|c: char| c <= ' ').chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
    let lower = t.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("https:").or_else(|| lower.strip_prefix("http:")) else { return false };
    let rest = rest.trim_start_matches(['/', '\\']);
    let auth = &rest[..rest.find(['/', '\\', '?', '#']).unwrap_or(rest.len())];
    let host_port = auth.rsplit('@').next().unwrap_or("");
    let (host, port) = if let Some(h) = host_port.strip_prefix('[') {
        match h.split_once(']') {
            Some((h, p)) => (h, p.strip_prefix(':').unwrap_or(p)),
            None => return false,
        }
    } else {
        host_port.split_once(':').unwrap_or((host_port, ""))
    };
    !host.is_empty()
        && !host.chars().any(|c| c == ' ' || "<>^|%\"`{}".contains(c) || (c as u32) < 0x20)
        && port.bytes().all(|b| b.is_ascii_digit())
        && (port.is_empty() || port.parse::<u32>().is_ok_and(|p| p <= 65535))
}

/// May a radio whose interfaces are `mine` be borrowed? Only if it carries no default route,
/// or a lower-metric default route exists via another interface that is up. Returns the
/// refusal reason, or None. `other_up` reports the best other route's link state.
pub fn borrow_refusal(routes: &[(String, f64)], mine: &HashSet<String>, other_up: impl Fn(&str) -> bool) -> Option<String> {
    let own: Vec<&(String, f64)> = routes.iter().filter(|x| mine.contains(&x.0)).collect();
    let name = &own.first()?.0;
    let mut others: Vec<&(String, f64)> = routes.iter().filter(|x| !mine.contains(&x.0)).collect();
    others.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let Some(best) = others.first() else { return Some(format!("{name} is the uplink (the only default route); refusing to borrow it")) };
    if own.iter().map(|x| x.1).fold(f64::INFINITY, f64::min) <= best.1 {
        return Some(format!("{name} is the uplink (lowest-metric default route); refusing to borrow it"));
    }
    if !other_up(&best.0) {
        return Some(format!("{} is down, so {name} is effectively the uplink; refusing to borrow it", best.0));
    }
    None
}

pub fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

/// wpa_supplicant network block. String fields go in as hex so no quoting can break out.
pub fn wpa_conf(spec: &TestSpec, key: Option<&str>) -> String {
    let mut n: Vec<String> = vec![format!("ssid={}", hex(&spec.ssid)), "scan_ssid=1".into()];
    if let Some(b) = &spec.bssid {
        n.push(format!("bssid={b}"));
    }
    let k = key.unwrap_or("");
    let pmk = key.is_some_and(is_pmk);
    let psk = || if pmk { format!("psk={}", k.to_lowercase()) } else { format!("psk=\"{k}\"") };
    match spec.security.as_str() {
        "open" => n.push("key_mgmt=NONE".into()),
        "owe" => n.extend(["key_mgmt=OWE".into(), "ieee80211w=2".into()]),
        "psk" => n.extend(["key_mgmt=WPA-PSK".into(), "ieee80211w=1".into(), psk()]),
        "sae" => n.extend(["key_mgmt=SAE".into(), "ieee80211w=2".into(), format!("sae_password={}", hex(k))]),
        "psk-sae" => {
            // A hex PMK can't do SAE, so with one this is plain WPA2-PSK.
            if pmk {
                n.extend(["key_mgmt=WPA-PSK".into(), "ieee80211w=1".into(), psk()]);
            } else {
                n.extend(["key_mgmt=WPA-PSK SAE".into(), "ieee80211w=1".into(), psk(), format!("sae_password={}", hex(k))]);
            }
        }
        "eap" => {
            let e = spec.eap.as_ref();
            n.extend([
                "key_mgmt=WPA-EAP WPA-EAP-SHA256".into(),
                "ieee80211w=1".into(),
                format!("eap={}", e.map(|e| e.method).unwrap_or("PEAP")),
                format!("identity={}", hex(e.map(|e| e.identity.as_str()).unwrap_or(""))),
                format!("password={}", hex(k)),
                "phase2=\"auth=MSCHAPV2\"".into(),
            ]);
            if let Some(a) = e.and_then(|e| e.anonymous_identity.as_ref()).filter(|a| !a.is_empty()) {
                n.push(format!("anonymous_identity={}", hex(a)));
            }
        }
        _ => {}
    }
    let mut out = vec!["p2p_disabled=1".to_string(), "sae_pwe=2".into(), "network={".into()];
    out.extend(n.iter().map(|l| format!("\t{l}")));
    out.push("}".into());
    out.push(String::new());
    out.join("\n")
}

/// udhcpc script: applies the lease inside the netns and echoes it for the timeline.
pub fn dhcp_script(ns: &str, ip: &str) -> String {
    format!(
        r#"#!/bin/sh
# Generated by BenchBob for a Wi-Fi client test in netns {ns}; deleted afterwards.
echo "BENCHBOB-DHCP $1|ip=$ip|mask=$mask|router=$router|dns=$dns|lease=$lease|serverid=$serverid|domain=$domain|broadcast=$broadcast|ntpsrv=$ntpsrv"
case "$1" in
  deconfig) {ip} addr flush dev "$interface" ;;
  bound|renew)
    {ip} addr flush dev "$interface"
    {ip} addr add "$ip/$mask" ${{broadcast:+broadcast $broadcast}} dev "$interface"
    for r in $router; do {ip} route replace default via "$r" dev "$interface"; break; done
    : > /etc/netns/{ns}/resolv.conf
    [ -n "$domain" ] && echo "search $domain" >> /etc/netns/{ns}/resolv.conf
    for d in $dns; do echo "nameserver $d" >> /etc/netns/{ns}/resolv.conf; done ;;
esac
exit 0
"#
    )
}

// ---------- Result ----------

#[derive(Debug, Clone)]
pub struct Step {
    pub t: u64,
    pub dt: u64,
    pub kind: String,
    pub ok: Option<bool>,
    pub text: String,
    pub data: Option<Value>,
}

impl Step {
    pub fn json(&self) -> Value {
        O::new()
            .k("t", self.t)
            .k("dt", self.dt)
            .k("kind", self.kind.clone())
            .o("ok", self.ok)
            .k("text", self.text.clone())
            .o("data", self.data.clone())
            .v()
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    /// ping-gateway, ping, dns, http, iperf
    pub kind: &'static str,
    pub target: String,
    pub ok: bool,
    pub ms: u64,
    pub summary: String,
    pub data: Option<Value>,
}

impl Outcome {
    pub fn json(&self) -> Value {
        O::new()
            .k("kind", self.kind)
            .k("target", self.target.clone())
            .k("ok", self.ok)
            .k("ms", self.ms)
            .k("summary", self.summary.clone())
            .o("data", self.data.clone())
            .v()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Sample {
    pub t: u64,
    pub signal: Option<f64>,
    pub tx: Option<f64>,
    pub rx: Option<f64>,
}

impl Sample {
    pub fn json(&self) -> Value {
        O::new().k("t", self.t).on("signal", self.signal).on("tx", self.tx).on("rx", self.rx).v()
    }
}

/// A test's result. The keys the TypeScript adds as the run goes on come out in the order
/// they were first set, as a JavaScript object's do.
#[derive(Debug, Clone)]
pub struct TestResult {
    pub id: String,
    pub phy: String,
    pub ns: String,
    pub ssid: String,
    pub security: String,
    /// never contains a key
    pub spec: Value,
    pub started_at: u64,
    pub stage: &'static str,
    pub ok: Option<bool>,
    pub timeline: Vec<Step>,
    pub tests: Vec<Outcome>,
    pub samples: Vec<Sample>,
    pub wpa_log: Vec<String>,
    // set later
    pub iface: Option<String>,
    pub ended_at: Option<u64>,
    pub error: Option<String>,
    pub cancelled: Option<bool>,
    pub link: Option<LinkInfo>,
    pub lease: Option<IndexMap<String, String>>,
    pub address: Option<String>,
    pub restore: Option<(bool, String)>,
    pub uplink_before: Option<String>,
    pub uplink_after: Option<String>,
    late: Vec<&'static str>,
}

impl TestResult {
    fn touch(&mut self, k: &'static str) {
        if !self.late.contains(&k) {
            self.late.push(k);
        }
    }

    /// The result as JSON; `drop_samples` leaves out samples and wpaLog (for callers that
    /// re-add their own versions at the end).
    pub fn json_with(&self, drop_samples: bool) -> Map<String, Value> {
        let mut o = O::new()
            .k("id", self.id.clone())
            .k("phy", self.phy.clone())
            .k("ns", self.ns.clone())
            .k("ssid", self.ssid.clone())
            .k("security", self.security.clone())
            .k("spec", self.spec.clone())
            .k("startedAt", self.started_at)
            .k("stage", self.stage)
            .k("ok", self.ok.map(Value::Bool).unwrap_or(Value::Null))
            .k("timeline", self.timeline.iter().map(Step::json).collect::<Vec<_>>())
            .k("tests", self.tests.iter().map(Outcome::json).collect::<Vec<_>>());
        if !drop_samples {
            o = o.k("samples", self.samples.iter().map(Sample::json).collect::<Vec<_>>()).k("wpaLog", self.wpa_log.clone());
        }
        for k in &self.late {
            o = match *k {
                "uplinkBefore" => o.o(k, self.uplink_before.clone()),
                "iface" => o.o(k, self.iface.clone()),
                "link" => o.o(k, self.link.as_ref().map(LinkInfo::json)),
                "lease" => {
                    o.o(k, self.lease.as_ref().map(|l| Value::Object(l.iter().map(|(a, b)| (a.clone(), Value::from(b.clone()))).collect())))
                }
                "address" => o.o(k, self.address.clone()),
                "error" => o.o(k, self.error.clone()),
                "cancelled" => o.o(k, self.cancelled),
                "restore" => o.o(k, self.restore.as_ref().map(|(ok, t)| O::new().k("ok", *ok).k("text", t.clone()).v())),
                "uplinkAfter" => o.o(k, self.uplink_after.clone()),
                "endedAt" => o.o(k, self.ended_at),
                _ => o,
            };
        }
        o.0
    }

    pub fn json(&self) -> Value {
        Value::Object(self.json_with(false))
    }
}

// ---------- Runner ----------

/// What the runner needs from the driver.
#[derive(Clone)]
pub struct Deps {
    pub bins: Arc<Bins>,
    /// For spawning children the driver kills on stop, and status pushes.
    pub ctx: Option<Ctx>,
    /// Lowest-metric default route's device in the main namespace.
    pub uplink: Arc<dyn Fn() -> BoxFut<Option<String>> + Send + Sync>,
}

struct RunState {
    r: TestResult,
    key: Option<String>,
    tmp: String,
    created_ns: bool,
    connected: bool,
    wpa_exited: bool,
    wpa_events: Vec<WpaEvent>,
    /// pids of the children we spawned (sudo … ip netns exec …)
    pids: HashSet<u32>,
    wpa_tx: Option<mpsc::UnboundedSender<Option<WpaEvent>>>,
}

pub struct ClientTest {
    pub spec: TestSpec,
    radio: RadioCtx,
    d: Deps,
    st: Mutex<RunState>,
    cancelled: AtomicBool,
    /// Skip the NetworkManager wait (driver shutting down): NM reconnects by itself anyway.
    pub fast: AtomicBool,
    cancel_tx: watch::Sender<bool>,
    done_tx: watch::Sender<bool>,
}

type Fail = String;

async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Feed every line of a child's stream to `cb`.
async fn read_lines<R: AsyncRead + Unpin>(r: Option<R>, mut cb: impl FnMut(String)) {
    let Some(r) = r else { return };
    let mut br = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match br.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                }
                cb(String::from_utf8_lossy(&buf).into_owned());
            }
        }
    }
}

impl ClientTest {
    pub fn new(id: String, spec: TestSpec, key: Option<String>, radio: RadioCtx, d: Deps) -> Arc<Self> {
        let ns = format!("{NS_PREFIX}{}", spec.phy.strip_prefix("phy").unwrap_or(&spec.phy));
        let r = TestResult {
            id,
            phy: spec.phy.clone(),
            ns,
            ssid: spec.ssid.clone(),
            security: spec.security.clone(),
            spec: spec.json(),
            started_at: now_ms(),
            stage: "prepare",
            ok: None,
            timeline: vec![],
            tests: vec![],
            samples: vec![],
            wpa_log: vec![],
            iface: None,
            ended_at: None,
            error: None,
            cancelled: None,
            link: None,
            lease: None,
            address: None,
            restore: None,
            uplink_before: None,
            uplink_after: None,
            late: vec![],
        };
        Arc::new(Self {
            spec,
            radio,
            d,
            st: Mutex::new(RunState {
                r,
                key,
                tmp: String::new(),
                created_ns: false,
                connected: false,
                wpa_exited: false,
                wpa_events: vec![],
                pids: HashSet::new(),
                wpa_tx: None,
            }),
            cancelled: AtomicBool::new(false),
            fast: AtomicBool::new(false),
            cancel_tx: watch::channel(false).0,
            done_tx: watch::channel(false).0,
        })
    }

    pub fn ns(&self) -> String {
        self.st.lock().unwrap().r.ns.clone()
    }

    /// Read the live result.
    pub fn with_r<T>(&self, f: impl FnOnce(&TestResult) -> T) -> T {
        f(&self.st.lock().unwrap().r)
    }

    pub fn stage(&self) -> &'static str {
        self.st.lock().unwrap().r.stage
    }

    /// Resolves once the test has finished (stage "done").
    pub async fn wait_done(&self) {
        let mut rx = self.done_tx.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    fn changed(&self) {
        if let Some(c) = &self.d.ctx {
            c.changed();
        }
    }

    fn step(&self, kind: &str, text: &str, ok: Option<bool>, data: Option<Value>) {
        {
            let mut st = self.st.lock().unwrap();
            let t = now_ms();
            let dt = t.saturating_sub(st.r.started_at);
            st.r.timeline.push(Step { t, dt, kind: kind.into(), ok, text: text.into(), data });
        }
        self.changed();
    }

    fn set_stage(&self, s: &'static str) {
        self.st.lock().unwrap().r.stage = s;
        self.changed();
    }

    fn check(&self) -> Result<(), Fail> {
        if self.cancelled.load(Ordering::SeqCst) { Err("Cancelled".into()) } else { Ok(()) }
    }

    fn kill_procs(&self) {
        let pids: Vec<u32> = self.st.lock().unwrap().pids.iter().copied().collect();
        for p in pids {
            // SAFETY: kill(2) on a child we spawned (sudo relays SIGTERM).
            unsafe { libc::kill(p as libc::pid_t, libc::SIGTERM) };
        }
    }

    pub fn cancel(&self) {
        if self.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        self.step("cancel", "Cancel requested", None, None);
        self.kill_procs();
        let _ = self.cancel_tx.send(true);
    }

    async fn priv_run(&self, cmd: &[&str], timeout_ms: u64) -> Output {
        sudo(&self.d.bins, cmd, timeout_ms).await
    }

    async fn in_ns(&self, cmd: &[&str], timeout_ms: u64) -> Output {
        let ns = self.ns();
        let b = &self.d.bins;
        let mut full: Vec<&str> = vec![b.ip.as_str(), "netns", "exec", ns.as_str()];
        full.extend_from_slice(cmd);
        self.priv_run(&full, timeout_ms).await
    }

    /// Spawn a command inside the namespace with piped stdout/stderr. Returns the child's
    /// output streams and a receiver for its exit code.
    fn spawn_ns(self: &Arc<Self>, cmd: &[&str]) -> Result<NsChild, Fail> {
        let b = &self.d.bins;
        let ns = self.ns();
        let mut argv: Vec<String> = b.sudo.clone();
        argv.extend([b.ip.clone(), "netns".into(), "exec".into(), ns]);
        argv.extend(cmd.iter().map(|s| s.to_string()));
        let mut c = tokio::process::Command::new(&argv[0]);
        c.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let (tx, rx) = watch::channel(None);
        let me = self.clone();
        let spawned: Result<Spawned, String> = match &self.d.ctx {
            Some(ctx) => match ctx.spawn(&mut c) {
                Ok(mut ch) => {
                    let pid = ch.child.id().unwrap_or(0);
                    let (o, e) = (ch.child.stdout.take(), ch.child.stderr.take());
                    let fut: BoxFut<i32> = Box::pin(async move { ch.child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1) });
                    Ok((pid, o, e, fut))
                }
                Err(e) => Err(e.to_string()),
            },
            None => match c.kill_on_drop(true).spawn() {
                Ok(mut ch) => {
                    let pid = ch.id().unwrap_or(0);
                    let (o, e) = (ch.stdout.take(), ch.stderr.take());
                    let fut: BoxFut<i32> = Box::pin(async move { ch.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1) });
                    Ok((pid, o, e, fut))
                }
                Err(e) => Err(e.to_string()),
            },
        };
        let (pid, out, err, fut) = spawned.map_err(|e| format!("{}: {e}", argv[0]))?;
        me.st.lock().unwrap().pids.insert(pid);
        tokio::spawn(async move {
            let code = fut.await;
            me.st.lock().unwrap().pids.remove(&pid);
            let _ = tx.send(Some(code));
        });
        Ok((out, err, rx))
    }

    pub async fn run(self: Arc<Self>) -> TestResult {
        let res = self.run_stages().await;
        let me = &self;
        match res {
            Ok(()) => {
                let mut st = me.st.lock().unwrap();
                let ok = st.r.tests.iter().all(|t| t.ok);
                st.r.ok = Some(ok);
                if !ok {
                    let failed = st.r.tests.iter().filter(|t| !t.ok).count();
                    st.r.error = Some(format!("{failed} of {} tests failed", st.r.tests.len()));
                    st.r.touch("error");
                }
            }
            Err(e) => {
                {
                    let mut st = me.st.lock().unwrap();
                    st.r.ok = Some(false);
                    st.r.cancelled = me.cancelled.load(Ordering::SeqCst).then_some(true);
                    st.r.touch("cancelled");
                    st.r.error = Some(e.clone());
                    st.r.touch("error");
                }
                me.step("error", &e, Some(false), None);
            }
        }
        me.st.lock().unwrap().wpa_tx = None;
        me.set_stage("cleanup");
        if let Err(e) = me.cleanup().await {
            me.step("cleanup", &format!("Cleanup error: {e}"), Some(false), None);
        }
        me.st.lock().unwrap().key = None;
        me.set_stage("restore");
        me.restore().await;
        let after = (me.d.uplink)().await;
        let before = {
            let mut st = me.st.lock().unwrap();
            st.r.uplink_after = after.clone();
            st.r.touch("uplinkAfter");
            st.r.uplink_before.clone()
        };
        me.step(
            "uplink",
            &format!("Main namespace default route via {}", after.clone().unwrap_or_else(|| "nothing".into())),
            Some(after == before),
            None,
        );
        {
            let mut st = me.st.lock().unwrap();
            st.r.ended_at = Some(now_ms());
            st.r.touch("endedAt");
        }
        me.set_stage("done");
        let (ok, err) = me.with_r(|r| (r.ok == Some(true), r.error.clone()));
        me.step("done", &if ok { "Passed".to_string() } else { format!("Failed: {}", fmt_opt_s(&err)) }, Some(ok), None);
        let _ = me.done_tx.send(true);
        me.with_r(|r| r.clone())
    }

    async fn run_stages(self: &Arc<Self>) -> Result<(), Fail> {
        let before = (self.d.uplink)().await;
        {
            let mut st = self.st.lock().unwrap();
            st.r.uplink_before = before.clone();
            st.r.touch("uplinkBefore");
        }
        self.step(
            "start",
            &format!(
                "Testing {} ({}) on {}; main uplink {}",
                self.spec.ssid,
                self.spec.security,
                self.spec.phy,
                before.unwrap_or_else(|| "none".into())
            ),
            None,
            None,
        );
        self.prepare().await?;
        self.check()?;
        self.borrow().await?;
        self.check()?;
        self.associate().await?;
        self.check()?;
        let sampler = self.start_sampler();
        let r = async {
            self.address().await?;
            self.check()?;
            self.run_tests().await?;
            self.check()?;
            if self.spec.hold_s > 0.0 {
                self.set_stage("hold");
                self.step("hold", &format!("Holding the connection for {} s", fmt_num(self.spec.hold_s)), None, None);
                let until = Instant::now() + Duration::from_secs_f64(self.spec.hold_s);
                while Instant::now() < until && self.st.lock().unwrap().connected && !self.cancelled.load(Ordering::SeqCst) {
                    sleep_ms(500).await;
                }
                self.check()?;
                if !self.st.lock().unwrap().connected {
                    return Err("Connection dropped during the hold period".to_string());
                }
                self.check()?;
                self.step("hold", "Hold complete", Some(true), None);
            }
            Ok(())
        }
        .await;
        sampler.abort();
        r
    }

    // ---- stages ----

    async fn prepare(&self) -> Result<(), Fail> {
        let b = self.d.bins.clone();
        let key = self.st.lock().unwrap().key.clone();
        if let Some(e) = valid_key(&self.spec.security, key.as_deref()) {
            return Err(e);
        }
        let ns = self.ns();
        // Creating the netns is the claim on this phy: it fails if another BenchBob process (a
        // second instance, say) is mid-test on it, and then cleanup must leave it alone.
        let r = self.priv_run(&[&b.ip, "netns", "add", &ns], 15000).await;
        if r.code != 0 {
            return Err(if r.err.to_lowercase().contains("exists") {
                format!("{ns} already exists: another BenchBob process is testing {}", self.spec.phy)
            } else {
                format!("ip netns add {ns}: {}", r.err)
            });
        }
        self.st.lock().unwrap().created_ns = true;
        // The pid lets crash recovery in another process tell a live test from a leftover.
        let tmp = mkdtemp(&format!("{}/{TMP_PREFIX}{}-", tmpdir(), std::process::id())).map_err(|e| format!("mkdtemp: {e}"))?;
        self.st.lock().unwrap().tmp = tmp.clone();
        write_mode(&format!("{tmp}/wpa.conf"), wpa_conf(&self.spec, key.as_deref()).as_bytes(), 0o600)
            .map_err(|e| format!("wpa.conf: {e}"))?;
        write_mode(&format!("{tmp}/dhcp.sh"), dhcp_script(&ns, &b.ip).as_bytes(), 0o700).map_err(|e| format!("dhcp.sh: {e}"))?;
        let _ = std::fs::set_permissions(format!("{tmp}/dhcp.sh"), std::fs::Permissions::from_mode(0o700));
        // `ip netns exec` bind-mounts /etc/netns/<ns>/resolv.conf over /etc/resolv.conf, but
        // only if it exists when the command starts, so create it (empty) up front.
        let r = self.priv_run(&[&b.mkdir, "-p", &format!("/etc/netns/{ns}")], 15000).await;
        if r.code != 0 {
            return Err(format!("mkdir /etc/netns/{ns}: {}", r.err));
        }
        tee(&b, &format!("/etc/netns/{ns}/resolv.conf"), "").await;
        tee(&b, &format!("/etc/netns/{ns}/{OWNER_FILE}"), &std::process::id().to_string()).await;
        self.step("netns", &format!("Created network namespace {ns}"), Some(true), None);
        Ok(())
    }

    async fn borrow(&self) -> Result<(), Fail> {
        let b = self.d.bins.clone();
        let ns = self.ns();
        self.set_stage("borrow");
        let r = self.priv_run(&[&b.iw, "phy", &self.spec.phy, "set", "netns", "name", &ns], 15000).await;
        if r.code != 0 {
            return Err(format!("Couldn't move {} into {ns}: {}", self.spec.phy, if r.err.is_empty() { &r.out } else { &r.err }));
        }
        let dev = p::parse_iw_dev(&self.in_ns(&[&b.iw, "dev"], 15000).await.out);
        let iface = dev
            .iter()
            .find(|x| x.name.is_some() && x.r#type.as_deref() == Some("managed"))
            .or_else(|| dev.iter().find(|x| x.name.is_some()))
            .and_then(|x| x.name.clone())
            .ok_or_else(|| format!("{} has no network interface inside {ns}", self.spec.phy))?;
        {
            let mut st = self.st.lock().unwrap();
            st.r.iface = Some(iface.clone());
            st.r.touch("iface");
        }
        self.in_ns(&[&b.ip, "link", "set", "lo", "up"], 15000).await;
        let up = self.in_ns(&[&b.ip, "link", "set", &iface, "up"], 15000).await;
        if up.code != 0 {
            return Err(format!("ip link set {iface} up: {}", up.err));
        }
        let names = if self.radio.ifaces.is_empty() { iface.clone() } else { self.radio.ifaces.join(", ") };
        self.step("borrow", &format!("Moved {} ({names}) into {ns}; {iface} is up", self.spec.phy), Some(true), None);
        Ok(())
    }

    fn on_wpa_line(&self, line: &str) {
        let secrets: Vec<String> = self.st.lock().unwrap().key.iter().flat_map(|k| [k.clone(), hex(k)]).collect();
        let Some(ev) = p::parse_wpa_line(line, &secrets) else { return };
        let (stage, connected_before, tx) = {
            let mut st = self.st.lock().unwrap();
            st.r.wpa_log.push(ev.text.clone());
            if st.r.wpa_log.len() > 80 {
                st.r.wpa_log.remove(0);
            }
            st.wpa_events.push(ev.clone());
            (st.r.stage, st.connected, st.wpa_tx.clone())
        };
        let bss = ev.bssid.as_ref().map(|b| format!(" {b}")).unwrap_or_default();
        let on_freq = ev.freq.map(|f| format!(" on {} MHz", fmt_num(f))).unwrap_or_default();
        let because = |n: Option<f64>| format!("{}{}", fmt_opt(n), ev.detail.as_ref().map(|d| format!(" ({d})")).unwrap_or_default());
        match ev.kind {
            "auth-start" => self.step("auth", &format!("Authenticating with{bss}{on_freq}"), None, None),
            "assoc-start" => self.step(
                "assoc-start",
                &format!("Authenticating + associating with{bss}{on_freq}"),
                None,
                Some(O::new().o("bssid", ev.bssid.clone()).on("freq", ev.freq).v()),
            ),
            "assoc" => self.step("assoc", &format!("Associated with{bss}"), Some(true), None),
            "handshake" => self.step("handshake", &format!("4-way handshake complete [{}]", fmt_opt_s(&ev.detail)), Some(true), None),
            "connected" => {
                self.st.lock().unwrap().connected = true;
                if self.spec.security == "open" || self.spec.security == "owe" {
                    self.step("connected", "Connected", Some(true), None);
                } else {
                    self.step("connected", "Connected (keys installed)", Some(true), None);
                }
            }
            "disconnected" => {
                if self.cancelled.load(Ordering::SeqCst) || matches!(stage, "cleanup" | "restore" | "done") {
                    self.step("disconnected", "Disconnected (test teardown)", None, None);
                } else if connected_before {
                    self.step("disconnected", &format!("Disconnected: reason {}", because(ev.reason)), Some(false), None);
                } else {
                    self.step("disconnected", &format!("Attempt failed: reason {}", because(ev.reason)), Some(false), None);
                }
                self.st.lock().unwrap().connected = false;
            }
            "wrong-key" => self.step("wrong-key", "4-way handshake failed: wrong key", Some(false), None),
            "temp-disabled" => self.step(
                "temp-disabled",
                &format!("wpa_supplicant gave up on this network for now: {}", fmt_opt_s(&ev.detail)),
                Some(false),
                None,
            ),
            "assoc-reject" | "auth-reject" => self.step(
                ev.kind,
                &format!(
                    "{} rejected by{bss}: status {}",
                    if ev.kind == "assoc-reject" { "Association" } else { "Authentication" },
                    because(ev.status)
                ),
                Some(false),
                None,
            ),
            "not-found" => self.step("not-found", "Scan found no matching network", Some(false), None),
            "eap" => self.step("eap", &ev.text, None, None),
            "config-error" | "error" => self.step("wpa-error", &ev.text, Some(false), None),
            _ => {}
        }
        if let Some(tx) = tx {
            let _ = tx.send(Some(ev));
        }
    }

    async fn associate(self: &Arc<Self>) -> Result<(), Fail> {
        let b = self.d.bins.clone();
        self.set_stage("associate");
        let (tx, mut rx) = mpsc::unbounded_channel::<Option<WpaEvent>>();
        self.st.lock().unwrap().wpa_tx = Some(tx.clone());
        let iface = self.with_r(|r| r.iface.clone()).unwrap_or_default();
        let tmp = self.st.lock().unwrap().tmp.clone();
        let conf = format!("{tmp}/wpa.conf");
        // stdbuf: wpa_supplicant's stdout is block-buffered on a pipe, which would hold every
        // event until exit.
        let (out, err, mut exited) = self.spawn_ns(&[&b.stdbuf, "-oL", "-eL", &b.wpa, "-i", &iface, "-D", "nl80211", "-c", &conf])?;
        let (m1, m2, m3) = (self.clone(), self.clone(), self.clone());
        tokio::spawn(async move { read_lines(out, |l| m1.on_wpa_line(&l)).await });
        tokio::spawn(async move { read_lines(err, |l| m2.on_wpa_line(&l)).await });
        tokio::spawn(async move {
            let code = loop {
                if let Some(c) = *exited.borrow_and_update() {
                    break c;
                }
                if exited.changed().await.is_err() {
                    break -1;
                }
            };
            let stage = {
                let mut st = m3.st.lock().unwrap();
                st.wpa_exited = true;
                st.connected = false;
                st.r.stage
            };
            if !m3.cancelled.load(Ordering::SeqCst) && !matches!(stage, "cleanup" | "restore" | "done") {
                m3.step("wpa-exit", &format!("wpa_supplicant exited (code {code})"), Some(false), None);
            }
            let tx = m3.st.lock().unwrap().wpa_tx.clone();
            if let Some(tx) = tx {
                let _ = tx.send(None);
            }
        });
        self.step(
            "wpa-start",
            &format!(
                "wpa_supplicant started on {iface} ({}{})",
                self.spec.security,
                self.spec.bssid.as_ref().map(|b| format!(", pinned to {b}")).unwrap_or_default()
            ),
            None,
            None,
        );
        drop(tx);

        let total = Duration::from_secs_f64(self.spec.assoc_timeout_s);
        let start = Instant::now();
        let deadline = start + total;
        let (mut not_found, mut rejects, mut failed_attempts) = (0, 0, 0);
        let mut cancel = self.cancel_tx.subscribe();
        let seen_start = |me: &Self| me.st.lock().unwrap().wpa_events.iter().any(|e| e.kind == "assoc-start" || e.kind == "auth-start");
        while !self.st.lock().unwrap().connected {
            self.check()?;
            if self.st.lock().unwrap().wpa_exited {
                let cfg = self.st.lock().unwrap().wpa_events.iter().find(|e| e.kind == "config-error").map(|e| e.text.clone());
                return Err(match cfg {
                    Some(t) => format!("wpa_supplicant rejected the configuration: {t}"),
                    None => "wpa_supplicant exited before connecting".into(),
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(if seen_start(self) {
                    format!("Timed out after {} s without completing the connection", fmt_num(self.spec.assoc_timeout_s))
                } else {
                    format!("Network \"{}\" not found within {} s", self.spec.ssid, fmt_num(self.spec.assoc_timeout_s))
                });
            }
            let wait = (deadline - now).min(Duration::from_millis(3000));
            let ev = tokio::select! {
                e = rx.recv() => e.flatten(),
                _ = tokio::time::sleep(wait) => None,
                _ = cancel.changed() => None,
            };
            let Some(ev) = ev else {
                // CTRL-EVENT-NETWORK-NOT-FOUND only goes to the control socket, not stdout, so a
                // missing network would otherwise just time out. The kernel's scan cache inside
                // the netns holds what wpa_supplicant's scans (with probe requests for this SSID,
                // so hidden networks answer too) have found.
                let elapsed = start.elapsed();
                if !seen_start(self) && elapsed > Duration::from_millis(12000) && !self.st.lock().unwrap().connected {
                    let dump = self.in_ns(&[&b.iw, "dev", &iface, "scan", "dump"], 5000).await;
                    let list = p::parse_iw_scan(&dump.out);
                    if !list.is_empty() && !list.iter().any(|x| x.ssid == self.spec.ssid) {
                        not_found += 1;
                        if not_found >= 2 {
                            return Err(format!(
                                "Network \"{}\" not found: not among the {} BSSes this radio can see",
                                self.spec.ssid,
                                list.len()
                            ));
                        }
                    } else {
                        not_found = 0;
                    }
                }
                continue;
            };
            let detail = ev.detail.clone().unwrap_or_default();
            let because = |n: Option<f64>| format!("{}{}", fmt_opt(n), ev.detail.as_ref().map(|d| format!(" ({d})")).unwrap_or_default());
            if ev.kind == "wrong-key" || (ev.kind == "temp-disabled" && detail.contains("WRONG_KEY")) {
                return Err("Wrong key: the 4-way handshake failed".into());
            }
            if ev.kind == "temp-disabled" && detail.contains("AUTH_FAILED") {
                return Err("Authentication failed (wrong password, or SAE/EAP rejected)".into());
            }
            // SAE (auth_type=3) rejected at the confirm (transaction 2): the AP's and our
            // password-derived confirms didn't match. Seen on desky with a wrong password.
            if ev.kind == "auth-reject" && ev.text.contains("auth_type=3 auth_transaction=2") {
                return Err(format!("Wrong password: WPA3-SAE confirm rejected by the AP (status {})", fmt_opt(ev.status)));
            }
            if ev.kind == "not-found" {
                not_found += 1;
                if not_found >= 4 {
                    return Err(format!("Network \"{}\" not found ({not_found} scans)", self.spec.ssid));
                }
            }
            if ev.kind == "assoc-reject" || ev.kind == "auth-reject" {
                rejects += 1;
                if rejects >= 3 {
                    return Err(format!("Rejected by the AP {rejects} times: status {}", because(ev.status)));
                }
            }
            if ev.kind == "disconnected" {
                failed_attempts += 1;
                if failed_attempts >= 3 {
                    return Err(if ev.reason == Some(15.0) {
                        "Wrong key: 4-way handshake timed out (reason 15)".into()
                    } else {
                        format!("Connection attempts keep failing: reason {}", because(ev.reason))
                    });
                }
            }
            if ev.kind == "disconnected" && ev.reason == Some(15.0) && self.spec.security != "open" {
                return Err("Wrong key: 4-way handshake timed out (reason 15)".into());
            }
        }
        self.sample_link(true).await;
        Ok(())
    }

    async fn sample_link(&self, record: bool) {
        let b = self.d.bins.clone();
        let Some(iface) = self.with_r(|r| r.iface.clone()) else { return };
        let (c1, c2) = ([b.iw.as_str(), "dev", &iface, "link"], [b.iw.as_str(), "dev", &iface, "station", "dump"]);
        let (link, sta) = tokio::join!(self.in_ns(&c1, 5000), self.in_ns(&c2, 5000));
        let l = p::parse_link(&link.out, &sta.out);
        if !l.connected {
            return;
        }
        let rec = {
            let mut st = self.st.lock().unwrap();
            st.r.samples.push(Sample { t: now_ms(), signal: l.signal, tx: l.tx_bitrate, rx: l.rx_bitrate });
            if st.r.samples.len() > 300 {
                st.r.samples.remove(0);
            }
            if record {
                st.r.link = Some(l.clone());
                st.r.touch("link");
                true
            } else {
                if let Some(cur) = st.r.link.as_mut() {
                    cur.signal = l.signal;
                    cur.tx_rate = l.tx_rate.clone();
                    cur.rx_rate = l.rx_rate.clone();
                    cur.tx_bitrate = l.tx_bitrate;
                    cur.rx_bitrate = l.rx_bitrate;
                }
                false
            }
        };
        if rec {
            self.step(
                "link",
                &format!(
                    "{} on {} MHz (ch {}), {} dBm, tx {}, rx {}",
                    fmt_opt_s(&l.bssid),
                    fmt_opt(l.freq),
                    fmt_num(p::freq_to_channel(l.freq.filter(|f| *f != 0.0).unwrap_or(0.0))),
                    fmt_opt(l.signal),
                    l.tx_rate.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "?".into()),
                    l.rx_rate.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "?".into())
                ),
                Some(true),
                Some(l.json()),
            );
        }
        self.changed();
    }

    fn start_sampler(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_millis(2000));
            iv.tick().await;
            loop {
                iv.tick().await;
                if me.st.lock().unwrap().connected {
                    me.sample_link(false).await;
                }
            }
        })
    }

    async fn address(self: &Arc<Self>) -> Result<(), Fail> {
        let b = self.d.bins.clone();
        let iface = self.with_r(|r| r.iface.clone()).unwrap_or_default();
        let ns = self.ns();
        if self.spec.ip == "none" {
            self.step("ip", "No IP configuration requested (link-layer test only)", None, None);
            return Ok(());
        }
        if self.spec.ip == "static" {
            self.set_stage("static");
            let s = self.spec.r#static.clone().unwrap_or(Static { address: String::new(), gateway: None, dns: vec![] });
            let r = self.in_ns(&[&b.ip, "addr", "add", &s.address, "dev", &iface], 15000).await;
            if r.code != 0 {
                return Err(format!("ip addr add {}: {}", s.address, r.err));
            }
            if let Some(gw) = &s.gateway {
                let r = self.in_ns(&[&b.ip, "route", "replace", "default", "via", gw, "dev", &iface], 15000).await;
                if r.code != 0 {
                    return Err(format!("default route via {gw}: {}", r.err));
                }
            }
            if !s.dns.is_empty() {
                tee(&b, &format!("/etc/netns/{ns}/resolv.conf"), &s.dns.iter().map(|d| format!("nameserver {d}\n")).collect::<String>())
                    .await;
            }
            {
                let mut st = self.st.lock().unwrap();
                st.r.address = Some(s.address.clone());
                st.r.touch("address");
                let mut lease = IndexMap::new();
                lease.insert("router".to_string(), s.gateway.clone().unwrap_or_default());
                lease.insert("dns".to_string(), s.dns.join(" "));
                st.r.lease = Some(lease);
                st.r.touch("lease");
            }
            self.step(
                "static",
                &format!(
                    "Static {}{}{}",
                    s.address,
                    s.gateway.as_ref().map(|g| format!(" via {g}")).unwrap_or_default(),
                    if s.dns.is_empty() { String::new() } else { format!(", DNS {}", s.dns.join(" ")) }
                ),
                Some(true),
                None,
            );
            return Ok(());
        }
        self.set_stage("dhcp");
        let tmp = self.st.lock().unwrap().tmp.clone();
        let script = format!("{tmp}/dhcp.sh");
        let (out, err, mut exited) =
            self.spawn_ns(&[&b.udhcpc, "-i", &iface, "-f", "-q", "-n", "-t", "4", "-T", "3", "-A", "1", "-s", &script])?;
        let lease: Arc<Mutex<Option<IndexMap<String, String>>>> = Arc::default();
        let discovers = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let on_line = {
            let (me, lease, discovers) = (self.clone(), lease.clone(), discovers.clone());
            move |l: String| {
                let Some(e) = p::parse_udhcpc_line(&l) else { return };
                match e.kind {
                    "discover" => {
                        let n = discovers.fetch_add(1, Ordering::SeqCst) + 1;
                        me.step(
                            "dhcp-discover",
                            &format!("DHCP discover{}", if n > 1 { format!(" (#{n})") } else { String::new() }),
                            None,
                            None,
                        )
                    }
                    "offer" => me.step(
                        "dhcp-offer",
                        &format!("Offer of {} from {}; requesting it", fmt_opt_s(&e.ip), fmt_opt_s(&e.server)),
                        Some(true),
                        Some(O::new().o("ip", e.ip.clone()).o("server", e.server.clone()).v()),
                    ),
                    "ack" => me.step(
                        "dhcp-ack",
                        &format!("ACK: {} from {}, lease {} s", fmt_opt_s(&e.ip), fmt_opt_s(&e.server), fmt_opt(e.lease_s)),
                        Some(true),
                        Some(O::new().o("ip", e.ip.clone()).o("server", e.server.clone()).on("leaseS", e.lease_s).v()),
                    ),
                    "nak" => me.step("dhcp-nak", "DHCP NAK", Some(false), None),
                    "fail" => me.step("dhcp-fail", "No DHCP lease", Some(false), None),
                    "lease" => {
                        if let Some(l) = e.lease
                            && matches!(l.get("event").map(String::as_str), Some("bound") | Some("renew"))
                        {
                            *lease.lock().unwrap() = Some(l);
                        }
                    }
                    _ => {}
                }
            }
        };
        let on2 = on_line.clone();
        let me = self.clone();
        let kill = async move {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(30000)) => me.kill_procs(),
                _ = async { while exited.borrow_and_update().is_none() { if exited.changed().await.is_err() { break; } } } => {}
            }
        };
        tokio::join!(read_lines(out, on_line), read_lines(err, on2), kill);
        let Some(l) = lease.lock().unwrap().take() else {
            return Err(if self.cancelled.load(Ordering::SeqCst) { "Cancelled".into() } else { "DHCP failed: no lease".into() });
        };
        let mut rest = l.clone();
        rest.shift_remove("event");
        let address = format!(
            "{}/{}",
            l.get("ip").cloned().unwrap_or_else(|| "undefined".into()),
            fmt_num(p::mask_to_prefix(l.get("mask").map(String::as_str).unwrap_or("32")))
        );
        {
            let mut st = self.st.lock().unwrap();
            st.r.lease = Some(rest.clone());
            st.r.touch("lease");
            st.r.address = Some(address.clone());
            st.r.touch("address");
        }
        let g = |k: &str, d: &str| l.get(k).filter(|v| !v.is_empty()).cloned().unwrap_or_else(|| d.into());
        self.step(
            "lease",
            &format!(
                "Address {address}, router {}, DNS {}{}",
                g("router", "none"),
                g("dns", "none"),
                l.get("domain").filter(|d| !d.is_empty()).map(|d| format!(", domain {d}")).unwrap_or_default()
            ),
            Some(true),
            Some(Value::Object(rest.iter().map(|(k, v)| (k.clone(), Value::from(v.clone()))).collect())),
        );
        Ok(())
    }

    fn record(&self, o: Outcome) {
        let text = format!("{} {}: {}", o.kind, o.target, o.summary);
        let (ok, data) = (o.ok, o.json());
        self.st.lock().unwrap().r.tests.push(o);
        self.step("test", &text, Some(ok), Some(data));
    }

    async fn ping(&self, kind: &'static str, host: &str) {
        let b = self.d.bins.clone();
        let n = self.spec.count;
        let t0 = Instant::now();
        let r = self
            .in_ns(
                &[&b.ping, "-c", &fmt_num(n), "-i", "0.3", "-W", "2", "-w", &fmt_num((n * 0.3 + 5.0).ceil()), host],
                ((n * 0.3 + 10.0) * 1000.0) as u64,
            )
            .await;
        let ms = t0.elapsed().as_millis() as u64;
        let pr = p::parse_ping(&format!("{}\n{}", r.out, r.err));
        let summary = match &pr.error {
            Some(e) if pr.received == 0.0 => e.clone(),
            _ => format!(
                "{}/{} replies, {}% loss{}",
                fmt_num(pr.received),
                fmt_num(pr.sent),
                fmt_num(pr.loss_pct),
                pr.avg.map(|a| format!(", avg {} ms (min {}, max {})", fmt_num(a), fmt_opt(pr.min), fmt_opt(pr.max))).unwrap_or_default()
            ),
        };
        self.record(Outcome { kind, target: host.into(), ok: pr.received > 0.0, ms, summary, data: Some(pr.json()) });
    }

    async fn run_tests(&self) -> Result<(), Fail> {
        let b = self.d.bins.clone();
        let s = &self.spec;
        if s.ip == "none" {
            return Ok(());
        }
        self.set_stage("tests");
        let gw = self.with_r(|r| r.lease.as_ref().and_then(|l| l.get("router")).and_then(|r| r.split(' ').next().map(String::from)));
        if s.ping_gateway {
            match gw.filter(|g| !g.is_empty()) {
                Some(gw) => self.ping("ping-gateway", &gw).await,
                None => self.record(Outcome {
                    kind: "ping-gateway",
                    target: "-".into(),
                    ok: false,
                    ms: 0,
                    summary: "No gateway to ping".into(),
                    data: None,
                }),
            }
        }
        for name in &s.dns {
            self.check()?;
            let t0 = Instant::now();
            let r = self.in_ns(&[&b.busybox, "nslookup", name], 12000).await;
            let ms = t0.elapsed().as_millis() as u64;
            let n = p::parse_nslookup(&format!("{}\n{}", r.out, r.err));
            let summary = if !n.addresses.is_empty() {
                let more = if n.addresses.len() > 4 { format!(" +{}", n.addresses.len() - 4) } else { String::new() };
                format!(
                    "{}{more} via {} in {ms} ms",
                    n.addresses.iter().take(4).cloned().collect::<Vec<_>>().join(", "),
                    n.server.clone().unwrap_or_else(|| "?".into())
                )
            } else {
                n.error.clone().unwrap_or_else(|| "no answer".into())
            };
            self.record(Outcome { kind: "dns", target: name.clone(), ok: !n.addresses.is_empty(), ms, summary, data: Some(n.json()) });
        }
        for url in &s.http {
            self.check()?;
            let t0 = Instant::now();
            let r = self
                .in_ns(&[&b.curl, "-sS", "-L", "--max-redirs", "5", "-o", "/dev/null", "--max-time", "20", "-w", "%{json}", url], 25000)
                .await;
            let ms = t0.elapsed().as_millis() as u64;
            let h = p::parse_curl_json(&r.out);
            let dash = |x: Option<f64>| x.map(fmt_num).unwrap_or_else(|| "-".into());
            let summary = if h.ok {
                format!(
                    "HTTP {} from {} in {} ms (DNS {}, connect {}, TLS {}, first byte {})",
                    fmt_num(h.code),
                    fmt_opt_s(&h.remote_ip),
                    fmt_opt(h.total_ms),
                    dash(h.dns_ms),
                    dash(h.connect_ms),
                    dash(h.tls_ms),
                    dash(h.first_byte_ms)
                )
            } else {
                h.error.clone().filter(|e| !e.is_empty()).unwrap_or_else(|| format!("HTTP {}", fmt_num(h.code)))
            };
            self.record(Outcome { kind: "http", target: url.clone(), ok: h.ok, ms, summary, data: Some(h.json()) });
        }
        for host in &s.ping {
            self.check()?;
            self.ping("ping", host).await;
        }
        if let Some(ip) = s.iperf.as_ref().filter(|i| !i.server.is_empty()) {
            self.check()?;
            let secs = if ip.seconds != 0.0 { ip.seconds } else { 5.0 };
            let port = if ip.port != 0.0 { ip.port } else { 5201.0 };
            let (port_s, secs_s) = (fmt_num(port), fmt_num(secs));
            let mut args: Vec<&str> = vec![&b.iperf3, "-c", &ip.server, "-p", &port_s, "-t", &secs_s, "-J", "--connect-timeout", "5000"];
            if ip.reverse {
                args.push("-R");
            }
            let t0 = Instant::now();
            let r = self.in_ns(&args, ((secs + 20.0) * 1000.0) as u64).await;
            let ms = t0.elapsed().as_millis() as u64;
            let res = p::parse_iperf(&r.out);
            let mbps = |x: &Option<Value>| match x.as_ref().and_then(Value::as_f64) {
                Some(v) => format!("{:.1} Mbit/s", v / 1e6),
                None => "?".into(),
            };
            let summary = match &res.error {
                Some(e) => e.clone(),
                None => format!(
                    "sent {}, received {}{}",
                    mbps(&res.sent_bps),
                    mbps(&res.received_bps),
                    res.retransmits.as_ref().map(|r| format!(", {} retransmits", crate::json::js_string(r))).unwrap_or_default()
                ),
            };
            self.record(Outcome {
                kind: "iperf",
                target: format!("{}:{port_s}{}", ip.server, if ip.reverse { " (download)" } else { " (upload)" }),
                ok: res.error.is_none(),
                ms,
                summary,
                data: Some(res.json()),
            });
        }
        Ok(())
    }

    // ---- cleanup + restore ----

    async fn cleanup(&self) -> Result<(), Fail> {
        let b = self.d.bins.clone();
        self.kill_procs();
        let (created, tmp) = {
            let st = self.st.lock().unwrap();
            (st.created_ns, st.tmp.clone())
        };
        let ns = self.ns();
        if !created {
            // We never claimed the namespace (it was someone else's, or creating it failed):
            // nothing of ours is in it and the phy never moved.
            if !tmp.is_empty() {
                let _ = std::fs::remove_dir_all(&tmp);
            }
            return Ok(());
        }
        sleep_ms(300).await;
        // Belt and braces: anything still running in the namespace (sudo relays SIGTERM, but a
        // child may have outlived its sudo).
        kill_ns_pids(&ns, &b).await;
        let mut back = false;
        for _ in 0..3 {
            if back {
                break;
            }
            let r = self.priv_run(&[&b.ip, "netns", "exec", &ns, &b.iw, "phy", &self.spec.phy, "set", "netns", "1"], 15000).await;
            // A phy that never left (borrow failed) reports "No such file"; check it's home.
            let home = run(&[&b.iw, "phy", &self.spec.phy, "info"], Opts::default()).await;
            back = home.code == 0;
            if !back {
                self.step(
                    "cleanup",
                    &format!("Couldn't return {} yet: {}", self.spec.phy, if r.err.is_empty() { &r.out } else { &r.err }),
                    Some(false),
                    None,
                );
                sleep_ms(1000).await;
            }
        }
        if back {
            self.step("cleanup", &format!("{} is back in the main namespace", self.spec.phy), Some(true), None);
            let d = self.priv_run(&[&b.ip, "netns", "del", &ns], 15000).await;
            self.step(
                "cleanup",
                &if d.code != 0 { format!("ip netns del {ns}: {}", d.err) } else { format!("Deleted {ns}") },
                Some(d.code == 0),
                None,
            );
            self.priv_run(&[&b.rm, "-rf", &format!("/etc/netns/{ns}")], 15000).await;
        } else {
            self.step("cleanup", &format!("{} is STILL in {ns}; the driver retries on its next start", self.spec.phy), Some(false), None);
        }
        if !tmp.is_empty() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        Ok(())
    }

    fn set_restore(&self, ok: bool, text: String) {
        let mut st = self.st.lock().unwrap();
        st.r.restore = Some((ok, text));
        st.r.touch("restore");
    }

    async fn restore(&self) {
        let b = self.d.bins.clone();
        // NetworkManager sees the returning interface as a new device; dongles BenchBob keeps
        // to itself need telling again before NM's autoconnect grabs them.
        for iface in &self.radio.unmanage {
            for _ in 0..10 {
                if run(&[&b.ip, "link", "show", iface], Opts::default()).await.code == 0 {
                    break;
                }
                sleep_ms(500).await;
            }
            let r = self.priv_run(&[&b.nmcli, "dev", "set", iface, "managed", "no"], 15000).await;
            self.step(
                "restore",
                &if r.code != 0 {
                    format!("nmcli dev set {iface} managed no: {}", r.err)
                } else {
                    format!("Told NetworkManager to leave {iface} alone again")
                },
                Some(r.code == 0),
                None,
            );
        }
        if self.radio.nm.is_empty() {
            self.set_restore(true, "Nothing to restore (NetworkManager had no connection on this radio)".into());
            return;
        }
        let t0 = Instant::now();
        let fast = self.fast.load(Ordering::SeqCst);
        for (iface, connection) in &self.radio.nm {
            let state = || async {
                let r = run(&[&b.nmcli, "-t", "-f", "GENERAL.STATE,GENERAL.CONNECTION", "dev", "show", iface], Opts::default()).await;
                let connected = r.out.contains("GENERAL.STATE:100");
                let conn = p::re!(r"GENERAL.CONNECTION:(.*)").captures(&r.out).map(|c| c[1].to_string()).unwrap_or_default();
                (connected, conn)
            };
            let mut s = state().await;
            let limit = if fast { Duration::ZERO } else { Duration::from_millis(20000) };
            while !(s.0 && &s.1 == connection) && t0.elapsed() < limit {
                sleep_ms(1000).await;
                s = state().await;
            }
            if s.0 && &s.1 == connection {
                let t = format!("NetworkManager reconnected {iface} to {connection} after {:.1} s", t0.elapsed().as_secs_f64());
                self.set_restore(true, t.clone());
                self.step("restore", &t, Some(true), None);
                continue;
            }
            if fast {
                let t = format!("Shutting down; left {iface} for NetworkManager to reconnect");
                self.set_restore(false, t.clone());
                self.step("restore", &t, None, None);
                continue;
            }
            self.step(
                "restore",
                &format!(
                    "{iface} not back on {connection} after 20 s (state: {}); running nmcli con up",
                    if s.0 { format!("connected to {}", s.1) } else { "not connected".into() }
                ),
                Some(false),
                None,
            );
            let up = self.priv_run(&[&b.nmcli, "con", "up", "id", connection, "ifname", iface], 60000).await;
            s = state().await;
            let ok = s.0 && &s.1 == connection;
            let t = if up.code != 0 {
                format!("nmcli con up {connection} failed: {}", if up.err.is_empty() { &up.out } else { &up.err })
            } else {
                format!("nmcli con up {connection}: {}", if s.0 { "connected" } else { "still not connected" })
            };
            self.set_restore(ok, t.clone());
            self.step("restore", &t, Some(ok), None);
        }
    }
}

// ---------- helpers ----------

/// `os.tmpdir()`
pub fn tmpdir() -> String {
    std::env::var("TMPDIR")
        .ok()
        .filter(|t| !t.is_empty())
        .map(|t| t.trim_end_matches('/').to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "/tmp".into())
}

/// `mkdtempSync(prefix)`: prefix + 6 random characters, mode 0700.
pub fn mkdtemp(prefix: &str) -> std::io::Result<String> {
    let t = std::ffi::CString::new(format!("{prefix}XXXXXX")).map_err(std::io::Error::other)?;
    let mut buf = t.into_bytes_with_nul();
    // SAFETY: buf is a NUL-terminated template that mkdtemp rewrites in place.
    let r = unsafe { libc::mkdtemp(buf.as_mut_ptr().cast()) };
    if r.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    buf.pop();
    String::from_utf8(buf).map_err(std::io::Error::other)
}

fn write_mode(path: &str, data: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(path)?;
    f.write_all(data)
}

/// `run([...sudo, tee, path], { stdin })`
async fn tee(b: &Bins, path: &str, stdin: &str) -> Output {
    run_v(&b.sudo_cmd(&[&b.tee, path]), Opts { stdin: Some(stdin.as_bytes().to_vec()), ..Default::default() }).await
}

/// Kill every process inside a namespace: TERM, then KILL for stragglers.
pub async fn kill_ns_pids(ns: &str, b: &Bins) {
    for sig in ["-TERM", "-KILL"] {
        let out = run_v(&b.sudo_cmd(&[&b.ip, "netns", "pids", ns]), Opts::default()).await.out;
        let pids: Vec<&str> = out.split_whitespace().filter(|x| x.bytes().all(|c| c.is_ascii_digit())).collect();
        if pids.is_empty() {
            return;
        }
        let mut cmd: Vec<&str> = vec![&b.kill, sig];
        cmd.extend(pids);
        run_v(&b.sudo_cmd(&cmd), Opts::default()).await;
        sleep_ms(500).await;
    }
}

/// After a crash: give back every phy found in leftover benchbob-wt-* (and deskypi-wt-*)
/// namespaces and delete them. Returns a line per action for the timeline.
pub async fn recover_namespaces(b: &Bins) -> Vec<String> {
    let mut out = Vec::new();
    let list = run_v(&b.sudo_cmd(&[&b.ip, "netns", "list"]), Opts::default()).await.out;
    let names: Vec<String> = list
        .split('\n')
        .map(|l| l.split(' ').next().unwrap_or("").to_string())
        .filter(|n| n.starts_with(NS_PREFIX) || n.starts_with(LEGACY_NS))
        .collect();
    for ns in names {
        // A second BenchBob instance (a side instance next to the service) may be mid-test.
        let owner_file = if ns.starts_with(NS_PREFIX) { OWNER_FILE } else { LEGACY_OWNER };
        let owner = benchbob_core::http::parse_int(
            &run_v(&b.sudo_cmd(&["cat", &format!("/etc/netns/{ns}/{owner_file}")]), Opts::default()).await.out,
        )
        .unwrap_or(0);
        if pid_alive(owner) {
            out.push(format!("Left {ns} alone: BenchBob process {owner} is still using it"));
            continue;
        }
        kill_ns_pids(&ns, b).await;
        let mut phys: Vec<String> = Vec::new();
        for d in p::parse_iw_dev(&run_v(&b.sudo_cmd(&[&b.ip, "netns", "exec", &ns, &b.iw, "dev"]), Opts::default()).await.out) {
            if !phys.contains(&d.phy) {
                phys.push(d.phy);
            }
        }
        // A phy with no interfaces doesn't show in `iw dev`; `iw phy` lists those too.
        let all = run_v(&b.sudo_cmd(&[&b.ip, "netns", "exec", &ns, &b.iw, "phy"]), Opts::default()).await.out;
        for c in p::re!(r"(?m)^Wiphy (\S+)").captures_iter(&all) {
            if !phys.iter().any(|x| x == &c[1]) {
                phys.push(c[1].into());
            }
        }
        for phy in &phys {
            let r = run_v(&b.sudo_cmd(&[&b.ip, "netns", "exec", &ns, &b.iw, "phy", phy, "set", "netns", "1"]), Opts::default()).await;
            out.push(if r.code != 0 {
                format!("Couldn't return {phy} from {ns}: {}", r.err)
            } else {
                format!("Returned {phy} from leftover {ns}")
            });
        }
        let d = run_v(&b.sudo_cmd(&[&b.ip, "netns", "del", &ns]), Opts::default()).await;
        run_v(&b.sudo_cmd(&[&b.rm, "-rf", &format!("/etc/netns/{ns}")]), Opts::default()).await;
        out.push(if d.code != 0 { format!("Couldn't delete {ns}: {}", d.err) } else { format!("Deleted leftover {ns}") });
    }
    // Temp dirs from a crashed run hold a key; remove them.
    out.extend(remove_stale_tmp(&tmpdir()));
    out
}

/// The temp-dir half of crash recovery: `<dir>/benchbob-wifi-<pid>-*` (and the legacy
/// prefix) whose pid isn't running any more.
pub fn remove_stale_tmp(dir: &str) -> Vec<String> {
    let mut out = Vec::new();
    for prefix in [TMP_PREFIX, LEGACY_TMP] {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        let mut names: Vec<String> =
            rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|f| f.starts_with(prefix)).collect();
        names.sort();
        for f in names {
            if pid_alive(benchbob_core::http::parse_int(&f[prefix.len()..]).unwrap_or(0)) {
                continue;
            }
            let path = format!("{dir}/{f}");
            let _ = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
            out.push(format!("Removed stale {path}"));
        }
    }
    out
}

#[cfg(test)]
mod tests;
