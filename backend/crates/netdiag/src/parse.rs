//! Pure helpers for the netdiag driver (peripherals/lib/netdiag.ts): parsers for ethtool,
//! lldpcli JSON, ping and iperf3 output, presets, iperf3 logins, and formatting.

use std::sync::LazyLock;

use benchbob_core::http::{js_number, js_string, num, truthy};
use indexmap::IndexMap;
use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

fn re(s: &str) -> Regex {
    Regex::new(s).unwrap()
}

// ---------- ethtool ----------

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EthLink {
    /// Mb/s
    pub speed: Option<i64>,
    pub duplex: String,
    pub autoneg: Option<bool>,
    pub link: Option<bool>,
    pub port: String,
    pub supported: Vec<String>,
    pub advertised: Vec<String>,
    /// Link partner advertised modes.
    pub partner: Vec<String>,
    pub partner_autoneg: Option<bool>,
    pub partner_pause: String,
    pub master_slave: String,
    pub mdix: String,
}

static ETH_KV: LazyLock<Regex> = LazyLock::new(|| re(r"^([A-Za-z][\w /-]*?):\s*(.*)$"));
static MODE: LazyLock<Regex> = LazyLock::new(|| re(r"^\d+base"));
static YES: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^(yes|on)$"));
static SPEED: LazyLock<Regex> = LazyLock::new(|| re(r"^(\d+)Mb/s"));

fn modes(s: Option<&String>) -> Vec<String> {
    s.map(|s| s.split_whitespace().filter(|x| MODE.is_match(x)).map(str::to_string).collect()).unwrap_or_default()
}

fn yn(s: Option<&String>) -> Option<bool> {
    s.map(|s| YES.is_match(s.trim()))
}

/// `ethtool IFACE` text. Multi-line mode lists continue on lines with no "Key:" prefix, so
/// we keep appending to the last key until the next "Key: value" line.
pub fn parse_ethtool(text: &str) -> EthLink {
    let mut kv: IndexMap<String, String> = IndexMap::new();
    let mut last = String::new();
    for raw in text.split('\n') {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("Settings for") || line.starts_with("netlink error") {
            continue;
        }
        match ETH_KV.captures(line) {
            Some(m) if !line.starts_with(|c: char| c.is_ascii_digit()) => {
                last = m[1].to_string();
                kv.insert(last.clone(), m[2].to_string());
            }
            _ if !last.is_empty() => {
                if let Some(v) = kv.get_mut(&last) {
                    v.push(' ');
                    v.push_str(line);
                }
            }
            _ => {}
        }
    }
    let g = |k: &str| kv.get(k);
    let duplex = g("Duplex").cloned().unwrap_or_default();
    EthLink {
        speed: SPEED.captures(g("Speed").map(String::as_str).unwrap_or("")).and_then(|m| m[1].parse().ok()),
        duplex: if duplex.to_lowercase().contains("unknown") { String::new() } else { duplex },
        autoneg: yn(g("Auto-negotiation")),
        link: yn(g("Link detected")),
        port: g("Port").cloned().unwrap_or_default(),
        supported: modes(g("Supported link modes")),
        advertised: modes(g("Advertised link modes")),
        partner: modes(g("Link partner advertised link modes")),
        partner_autoneg: yn(g("Link partner advertised auto-negotiation")),
        partner_pause: g("Link partner advertised pause frame use").cloned().unwrap_or_default(),
        master_slave: g("master-slave status").cloned().unwrap_or_default(),
        mdix: g("MDI-X").cloned().unwrap_or_default(),
    }
}

static KEY_COLON: LazyLock<Regex> = LazyLock::new(|| re(r"^([\w-]+):\s*(.*)$"));

/// `ethtool -i`: driver, version, firmware-version, bus-info…
pub fn parse_key_colon(text: &str) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for l in text.split('\n') {
        if let Some(m) = KEY_COLON.captures(l.trim()) {
            out.insert(m[1].to_string(), m[2].trim().to_string());
        }
    }
    out
}

static STAT: LazyLock<Regex> = LazyLock::new(|| re(r"^\s+([\w.\-\[\]]+):\s*(-?\d+)\s*$"));

/// `ethtool -S`: counters. Some drivers repeat a name (bcmgenet has rx_bytes twice); the
/// first wins.
pub fn parse_ethtool_stats(text: &str) -> IndexMap<String, i64> {
    let mut out = IndexMap::new();
    for l in text.split('\n') {
        if let Some(m) = STAT.captures(l)
            && !out.contains_key(&m[1])
        {
            let v = m[2].parse::<i64>().unwrap_or_else(|_| m[2].parse::<f64>().map(|f| f as i64).unwrap_or(0));
            out.insert(m[1].to_string(), v);
        }
    }
    out
}

static QUEUE_TRAFFIC: LazyLock<Regex> = LazyLock::new(|| re(r"^(rx|tx)q\d+_(bytes|packets|multicast|broadcast)$"));
static ERR_COUNTER: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)err|drop|crc|fcs|miss|overrun|over_|oversize|undersize|runt|jabber|align|frag|collision|_col$|late_col|excess|defer|discard|fail|ovflow|carrier|abort|timeout|symbol|code$|outrange|lost|no_buf|nobuf|fifo",
    )
});

/// Counters that mean something went wrong (as opposed to traffic, size histograms,
/// per-queue totals).
pub fn is_error_counter(name: &str) -> bool {
    if QUEUE_TRAFFIC.is_match(name) {
        return false;
    }
    // bcmgenet's MAC-destination-filter count: frames addressed to other hosts that the
    // switch flooded to us. It climbs all day on a busy LAN and means nothing is wrong.
    if name == "mdf_err_cnt" {
        return false;
    }
    ERR_COUNTER.is_match(name)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Eee {
    pub supported: bool,
    pub enabled: bool,
    pub active: bool,
    pub tx_lpi_us: Option<i64>,
    pub advertised: Vec<String>,
    pub partner: Vec<String>,
}

static EEE_STATUS: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)EEE status:\s*(.*)"));
static LPI: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)Tx LPI:\s*(\d+)"));
static EEE_ADV: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)Advertised EEE link modes:\s*([^\n]*(?:\n\s+\d+base[^\n]*)*)"));
static EEE_LP: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)Link partner advertised EEE link modes:\s*([^\n]*(?:\n\s+\d+base[^\n]*)*)"));

pub fn parse_eee(text: &str) -> Option<Eee> {
    let st = EEE_STATUS.captures(text)?[1].to_string();
    let block = |r: &Regex| -> Vec<String> {
        r.captures(text).map(|m| m[1].split_whitespace().filter(|x| MODE.is_match(x)).map(str::to_string).collect()).unwrap_or_default()
    };
    let lower = st.to_lowercase();
    Some(Eee {
        supported: !lower.contains("not supported"),
        enabled: lower.starts_with("enabled"),
        active: lower.contains("- active"),
        tx_lpi_us: LPI.captures(text).and_then(|m| m[1].parse().ok()),
        advertised: block(&EEE_ADV),
        partner: block(&EEE_LP),
    })
}

// ---------- LLDP ----------

fn arr(x: Option<&Value>) -> Vec<Value> {
    match x {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(v) => vec![v.clone()],
    }
}

/// Entries of a lldpcli "keyed object": `{ "eth0": {...} }` or `[{ "eth0": {...} }, …]`.
fn entries(x: Option<&Value>) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for o in arr(x) {
        if let Value::Object(m) = o {
            for (k, v) in m {
                out.push((k, v));
            }
        }
    }
    out
}

/// `x || ""` for a JSON value shown as text.
fn or_str(v: Option<&Value>, def: &str) -> String {
    match v {
        Some(v) if truthy(v) => js_string(v),
        _ => def.to_string(),
    }
}

/// `x != null ? +x : null`
fn opt_num(v: Option<&Value>) -> Value {
    match v {
        None | Some(Value::Null) => Value::Null,
        Some(v) => num(js_number(v)),
    }
}

/// `lldpcli -f json show neighbors details` (lldpd 1.0.x JSON), as the API's neighbour
/// objects.
pub fn parse_lldp_neighbors(j: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for (iface, n) in entries(j.get("lldp").and_then(|l| l.get("interface"))) {
        if !n.is_object() {
            continue;
        }
        // The chassis is keyed by system name when there is one, else it's the bare object.
        let mut name = String::new();
        let mut ch = n.get("chassis").filter(|c| truthy(c)).cloned().unwrap_or(json!({}));
        if ch.get("id").is_none_or(|i| !truthy(i))
            && let Some((k, v)) = entries(Some(&ch)).into_iter().next()
        {
            name = k;
            ch = v;
        }
        let port = n.get("port").filter(|p| truthy(p)).cloned().unwrap_or(json!({}));
        let ttl = port.get("ttl").filter(|v| !v.is_null()).or_else(|| n.get("ttl").and_then(|t| t.get("ttl")));
        let power = port.get("power").filter(|p| truthy(p));
        let med_poe = n.get("lldp-med").and_then(|m| m.get("poe")).filter(|p| truthy(p));
        let poe = if power.is_some() || med_poe.is_some() {
            let mut m = Map::new();
            for src in [med_poe, power].into_iter().flatten() {
                if let Value::Object(o) = src {
                    for (k, v) in o {
                        if !v.is_null() && !v.is_object() && !v.is_array() {
                            m.insert(k.clone(), Value::String(js_string(v)));
                        } else if m.contains_key(k) {
                            // `{...a, ...b}` keeps b's null/object too, which the filter then drops.
                            m.shift_remove(k);
                        }
                    }
                }
            }
            Value::Object(m)
        } else {
            Value::Null
        };
        let id = ch.get("id");
        let pid = port.get("id");
        let capabilities: Vec<Value> = arr(ch.get("capability"))
            .iter()
            .map(|c| {
                let mut m = Map::new();
                if let Some(t) = c.get("type") {
                    m.insert("type".into(), t.clone());
                }
                m.insert("enabled".into(), json!(c.get("enabled").is_some_and(truthy)));
                Value::Object(m)
            })
            .collect();
        let vlans: Vec<Value> = arr(n.get("vlan"))
            .iter()
            .filter_map(|v| {
                let id = v.get("vlan-id").map(js_number).unwrap_or(f64::NAN);
                id.is_finite()
                    .then(|| json!({ "id": num(id), "pvid": v.get("pvid").is_some_and(truthy), "name": or_str(v.get("value"), "") }))
            })
            .collect();
        let mut o = Map::new();
        o.insert("iface".into(), json!(iface));
        o.insert("protocol".into(), json!(or_str(n.get("via"), "LLDP")));
        o.insert("age".into(), json!(or_str(n.get("age"), "")));
        o.insert("name".into(), json!(name));
        o.insert(
            "description".into(),
            json!(
                arr(ch.get("descr")).iter().map(|d| if d.is_null() { String::new() } else { js_string(d) }).collect::<Vec<_>>().join(" ")
            ),
        );
        o.insert("chassisId".into(), json!(or_str(id.and_then(|i| i.get("value")), "")));
        o.insert("chassisIdType".into(), json!(or_str(id.and_then(|i| i.get("type")), "")));
        o.insert("mgmtIps".into(), json!(arr(ch.get("mgmt-ip")).iter().map(js_string).collect::<Vec<_>>()));
        o.insert("capabilities".into(), Value::Array(capabilities));
        o.insert("portId".into(), json!(or_str(pid.and_then(|i| i.get("value")), "")));
        o.insert("portIdType".into(), json!(or_str(pid.and_then(|i| i.get("type")), "")));
        o.insert("portDescription".into(), json!(or_str(port.get("descr"), "")));
        o.insert("ttl".into(), opt_num(ttl));
        o.insert("vlans".into(), Value::Array(vlans));
        o.insert("mtu".into(), opt_num(port.get("mfs")));
        o.insert("poe".into(), poe);
        o.insert("medDeviceType".into(), json!(or_str(n.get("lldp-med").and_then(|m| m.get("device-type")), "")));
        out.push(Value::Object(o));
    }
    out
}

// ---------- ping ----------

#[derive(Debug, Clone, PartialEq)]
pub struct PingResult {
    pub host: String,
    pub sent: i64,
    pub received: i64,
    pub loss_pct: f64,
    pub min: Option<f64>,
    pub avg: Option<f64>,
    pub max: Option<f64>,
    pub mdev: Option<f64>,
    pub errors: i64,
    pub times: Vec<f64>,
    /// "Message too long": the local interface MTU already refuses it.
    pub too_big: bool,
}

impl PingResult {
    pub fn to_json(&self) -> Map<String, Value> {
        let o = |v: Option<f64>| v.map(num).unwrap_or(Value::Null);
        let mut m = Map::new();
        m.insert("host".into(), json!(self.host));
        m.insert("sent".into(), json!(self.sent));
        m.insert("received".into(), json!(self.received));
        m.insert("lossPct".into(), num(self.loss_pct));
        m.insert("min".into(), o(self.min));
        m.insert("avg".into(), o(self.avg));
        m.insert("max".into(), o(self.max));
        m.insert("mdev".into(), o(self.mdev));
        m.insert("errors".into(), json!(self.errors));
        m.insert("times".into(), Value::Array(self.times.iter().map(|t| num(*t)).collect()));
        m.insert("tooBig".into(), json!(self.too_big));
        m
    }
}

static PING_ST: LazyLock<Regex> =
    LazyLock::new(|| re(r"(\d+) packets transmitted, (\d+) received(?:, \+(\d+) errors)?(?:, [^,]*)?, ([\d.]+)% packet loss"));
static PING_RTT: LazyLock<Regex> = LazyLock::new(|| re(r"= ([\d.]+)/([\d.]+)/([\d.]+)/([\d.]+) ms"));
static PING_TIME: LazyLock<Regex> = LazyLock::new(|| re(r"time=([\d.]+) ms"));
static TOO_BIG: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)message too long|frag needed"));

/// `+s` for a matched decimal.
fn f(s: &str) -> f64 {
    s.parse().unwrap_or(f64::NAN)
}

pub fn parse_ping(text: &str, host: &str) -> PingResult {
    let st = PING_ST.captures(text);
    let rtt = PING_RTT.captures(text);
    PingResult {
        host: host.into(),
        sent: st.as_ref().map(|m| m[1].parse().unwrap_or(0)).unwrap_or(0),
        received: st.as_ref().map(|m| m[2].parse().unwrap_or(0)).unwrap_or(0),
        loss_pct: st.as_ref().map(|m| f(&m[4])).unwrap_or(100.0),
        min: rtt.as_ref().map(|m| f(&m[1])),
        avg: rtt.as_ref().map(|m| f(&m[2])),
        max: rtt.as_ref().map(|m| f(&m[3])),
        mdev: rtt.as_ref().map(|m| f(&m[4])),
        errors: st.as_ref().and_then(|m| m.get(3)).map(|e| e.as_str().parse().unwrap_or(0)).unwrap_or(0),
        times: PING_TIME.captures_iter(text).map(|m| f(&m[1])).collect(),
        too_big: TOO_BIG.is_match(text),
    }
}

/// Path MTU by binary search on the ICMP payload size (MTU = payload + 28 bytes of IPv4 and
/// ICMP headers). Tries a full-size packet first, since that's the common answer, and an
/// empty one second so a host that ignores ping isn't reported as MTU 28.
pub async fn mtu_search<F, Fut>(mut probe: F, if_mtu: i64) -> (Option<i64>, Vec<(i64, bool)>)
where
    F: FnMut(i64) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut steps = Vec::new();
    let mut lo = 0;
    let mut hi = if_mtu - 28;
    let ok = probe(hi).await;
    steps.push((hi + 28, ok));
    if ok {
        return (Some(if_mtu), steps);
    }
    let ok = probe(0).await;
    steps.push((28, ok));
    if !ok {
        return (None, steps);
    }
    while hi - lo > 1 {
        let mid = (lo + hi) >> 1;
        let ok = probe(mid).await;
        steps.push((mid + 28, ok));
        if ok {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (Some(lo + 28), steps)
}

// ---------- iperf3 ----------

/// `x || 0`
fn or0(v: Option<&Value>) -> Value {
    match v {
        Some(v) if truthy(v) => v.clone(),
        _ => json!(0),
    }
}

/// `x ?? null`
fn or_null(v: Option<&Value>) -> Value {
    v.cloned().unwrap_or(Value::Null)
}

/// Throughput from an iperf3 "end" event, as measured by the receiver.
/// Client: sum_received is always the receiving side's count (iperf3 exchanges results).
/// Server: it only knows its own side, so it's sum_received normally and sum_sent under -R.
/// UDP has a single "sum" with jitter and loss.
pub fn iperf_summary(end: &Value, start: &Value, server: bool) -> Value {
    let ts = start.get("test_start").filter(|t| truthy(t)).cloned().unwrap_or(json!({}));
    let protocol = match ts.get("protocol").filter(|p| truthy(p)) {
        Some(p) => js_string(p),
        None if end.get("sum").and_then(|s| s.get("jitter_ms")).is_some_and(|j| !j.is_null()) => "UDP".into(),
        None => "TCP".into(),
    };
    let reverse = ts.get("reverse").is_some_and(truthy);
    if protocol == "UDP" {
        let s = end.get("sum").filter(|s| truthy(s)).cloned().unwrap_or(json!({}));
        return json!({
            "protocol": protocol, "reverse": reverse, "seconds": or0(s.get("seconds")), "bytes": or0(s.get("bytes")),
            "bps": or0(s.get("bits_per_second")), "retransmits": null, "jitterMs": or_null(s.get("jitter_ms")), "lostPct": or_null(s.get("lost_percent")),
        });
    }
    let rcv = if server && reverse { end.get("sum_sent") } else { end.get("sum_received") };
    let snd = end.get("sum_sent").filter(|s| truthy(s)).cloned().unwrap_or(json!({}));
    json!({
        "protocol": protocol, "reverse": reverse, "seconds": or0(rcv.and_then(|r| r.get("seconds"))), "bytes": or0(rcv.and_then(|r| r.get("bytes"))),
        "bps": or0(rcv.and_then(|r| r.get("bits_per_second"))), "retransmits": or_null(snd.get("retransmits")), "jitterMs": null, "lostPct": null,
    })
}

static CONNECT_PRE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^unable to connect to server[^:]*:\s*"));
static REFUSED: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)connection refused"));
static TIMED_OUT: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)timed out"));
static NO_ROUTE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)no route|unreachable"));
static BAD_NAME: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)name or service|not known|getaddrinfo|resolve|invalid argument"));
static BUSY: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)busy"));
static AUTH: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)authoriz|authenticat"));

/// iperf3's client error, said plainly. Its connect failures all read "unable to connect
/// to server - server may have stopped running or use a different port, firewall issue,
/// etc.: <errno text>" (3.18); the errno text is the useful part.
pub fn iperf_error(raw: &str, host: &str, port: i64) -> String {
    let s = raw.trim();
    let why = CONNECT_PRE.replace(s, "").to_string();
    let at = format!("{host}:{port}");
    if REFUSED.is_match(&why) {
        return format!("connection refused: no iperf3 server listening on {at}");
    }
    if TIMED_OUT.is_match(&why) {
        return format!("no answer from {at} (timed out): host down, or a firewall drops the port");
    }
    if NO_ROUTE.is_match(&why) {
        return format!("can't reach {host} ({why})");
    }
    if BAD_NAME.is_match(&why) && why != s {
        return format!("couldn't connect to {at} ({why}): check the host name");
    }
    if BUSY.is_match(s) {
        return format!("the iperf3 server on {at} is busy with another test; try again shortly");
    }
    if AUTH.is_match(s) {
        return format!(
            "{at} refused the login: check the username, password and the server's public key (Logins…); a server on iperf3 3.16 or older also needs \"older server\" ticked"
        );
    }
    if !why.is_empty() {
        return why;
    }
    if !s.is_empty() { s.to_string() } else { "iperf3 gave no result".into() }
}

// ---------- iperf3 logins ----------
// iperf3 ≥3.10 servers started with --rsa-private-key-path + --authorized-users-path only
// run tests for listed users. The client encrypts "username + password" with the server's
// RSA public key; the server's users file holds `username,sha256("{username}password")`.

static USER: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z0-9_.-]{1,32}$"));

pub fn valid_iperf_user(u: &str) -> bool {
    USER.is_match(u)
}

pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The line for the server's --authorized-users-path file. Safe to publish: only the hash.
pub fn iperf_auth_line(username: &str, password: &str) -> String {
    format!("{username},{}", sha256_hex(&format!("{{{username}}}{password}")))
}

static PEM: LazyLock<Regex> = LazyLock::new(|| re(r"^-----BEGIN PUBLIC KEY-----\n[A-Za-z0-9+/=\n]+\n-----END PUBLIC KEY-----$"));

/// An RSA public key as `openssl rsa -pubout` writes it, normalised to LF + final newline.
pub fn iperf_public_key(pem: &str) -> Result<String, String> {
    let t = pem.replace("\r\n", "\n");
    let t = t.trim();
    if !PEM.is_match(t) {
        return Err("publicKey must be a PEM public key (-----BEGIN PUBLIC KEY----- … -----END PUBLIC KEY-----)".into());
    }
    Ok(format!("{t}\n"))
}

/// URL-safe base64 without padding (`Buffer.toString("base64url")`).
pub fn base64url(b: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        let k = c.len() + 1;
        for i in 0..k {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

// ---------- presets ----------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Preset {
    /// "gateway" (ping only) = the default gateway
    pub host: String,
    pub label: String,
}

impl Preset {
    pub fn json(&self) -> Value {
        json!({ "host": self.host, "label": self.label })
    }
}

/// No iperf3 hosts out of the box: the useful ones are your own (`iperf3 -s` on 5201),
/// added with "Edit hosts…". Public iperf3 servers are rate-limited and come and go.
pub fn default_iperf_presets() -> Vec<Preset> {
    Vec::new()
}

pub fn default_ping_targets() -> Vec<Preset> {
    [("gateway", "Gateway"), ("1.1.1.1", "1.1.1.1"), ("8.8.8.8", "8.8.8.8")]
        .iter()
        .map(|(h, l)| Preset { host: (*h).into(), label: (*l).into() })
        .collect()
}

static COMMENT: LazyLock<Regex> = LazyLock::new(|| re(r"#.*$"));

/// Presets from text, one per line: "host label…" (label optional, # comments). Max 16.
pub fn parse_presets(text: &str, allow_gateway: bool) -> Result<Vec<Preset>, String> {
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let l = COMMENT.replace(raw, "");
        let l = l.trim();
        if l.is_empty() {
            continue;
        }
        let mut parts = l.split_whitespace();
        let host = parts.next().unwrap_or("").to_string();
        if !(allow_gateway && host == "gateway") {
            valid_host(&host)?;
        }
        let rest: Vec<&str> = parts.collect();
        let label = if rest.is_empty() { host.clone() } else { rest.join(" ") };
        out.push(Preset { host, label });
    }
    if out.len() > 16 {
        return Err("At most 16 presets".into());
    }
    Ok(out)
}

/// Presets from the API: text, or a list of {host, label}.
pub fn presets_from(v: &Value, allow_gateway: bool) -> Result<Vec<Preset>, String> {
    match v {
        Value::String(s) => parse_presets(s, allow_gateway),
        Value::Array(a) => {
            let lines: Vec<String> = a
                .iter()
                .map(|p| {
                    let host = p.get("host").filter(|h| !h.is_null()).map(js_string).unwrap_or_default();
                    let label = p.get("label").filter(|h| !h.is_null()).map(js_string).unwrap_or_default().replace(['\n', '#'], " ");
                    format!("{host} {label}")
                })
                .collect();
            parse_presets(&lines.join("\n"), allow_gateway)
        }
        _ => Err("presets must be text or a list of {host, label}".into()),
    }
}

pub fn format_presets(list: &[Preset]) -> String {
    list.iter()
        .map(|p| if !p.label.is_empty() && p.label != p.host { format!("{} {}", p.host, p.label) } else { p.host.clone() })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `(x).toFixed(d)`
fn to_fixed(x: f64, d: usize) -> String {
    format!("{x:.d$}")
}

pub fn fmt_bps(bps: f64) -> String {
    if bps == 0.0 || bps.is_nan() {
        return "0 bit/s".into();
    }
    let u = ["bit/s", "kbit/s", "Mbit/s", "Gbit/s"];
    let i = ((bps.log10() / 3.0).floor() as i64).clamp(0, 3) as usize;
    let v = bps / 10f64.powi(i as i32 * 3);
    let d = if i == 0 {
        0
    } else if v < 10.0 {
        2
    } else {
        1
    };
    format!("{} {}", to_fixed(v, d), u[i])
}

// ---------- misc ----------

static RESOLV: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*(nameserver|search|domain)\s+(.+)$"));

/// resolv.conf's nameservers and search domains.
pub fn parse_resolv_conf(text: &str) -> (Vec<String>, Vec<String>) {
    let mut ns = Vec::new();
    let mut search = Vec::new();
    for l in text.split('\n') {
        let l = l.strip_suffix('\r').unwrap_or(l);
        let Some(m) = RESOLV.captures(l) else { continue };
        if &m[1] == "nameserver" {
            ns.push(m[2].trim().to_string());
        } else {
            search.extend(m[2].split_whitespace().map(str::to_string));
        }
    }
    (ns, search)
}

static HOST: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z0-9][A-Za-z0-9.:\-_%]{0,252}$"));

/// Hostnames/IPs only: these end up as command arguments (never via a shell, but no option
/// injection either).
pub fn valid_host(h: &str) -> Result<String, String> {
    let s = h.trim();
    if !HOST.is_match(s) {
        return Err(format!("\"{s}\" isn't a hostname or IP address"));
    }
    Ok(s.to_string())
}

/// `validHost(String(h ?? ""))` for a JSON argument.
pub fn valid_host_value(h: Option<&Value>) -> Result<String, String> {
    valid_host(&h.filter(|v| !v.is_null()).map(js_string).unwrap_or_default())
}

/// Whether an interface's link-layer multicast list (/proc/net/dev_mcast) has the LLDP
/// nearest-bridge group, 01:80:c2:00:00:0e.
pub fn has_lldp_multicast(dev_mcast: &str, iface: &str) -> bool {
    dev_mcast.lines().any(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        f.len() >= 5 && f[1] == iface && f[4].eq_ignore_ascii_case("0180c200000e")
    })
}
