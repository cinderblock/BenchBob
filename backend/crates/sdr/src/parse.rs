//! Pure helpers (`peripherals/lib/sdr-parse.ts`): which USB ids are RTL2832U sticks, what
//! rtl_test and rtl_eeprom say about them, whether the DVB driver is blacklisted, the rtl_433
//! JSON stream folded into a per-sensor table, and rtl_tcp's peers from /proc/net/tcp.

use std::sync::OnceLock;

use benchbob_core::http::{js_string, num, parse_float, truthy};
use benchbob_core::util::{js_round, parse_iso, parse_json};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};

/// RTL2832U-based sticks, from librtlsdr's known_devices table.
pub const RTL_STICKS: &[(&str, &str)] = &[
    ("0bda:2832", "Generic RTL2832U"),
    ("0bda:2838", "Generic RTL2832U OEM"),
    ("0413:6680", "DigitalNow Quad DVB-T PCI-E card"),
    ("0413:6f0f", "Leadtek WinFast DTV Dongle mini D"),
    ("0458:707f", "Genius TVGo DVB-T03 USB dongle (Ver. B)"),
    ("0ccd:00a9", "Terratec Cinergy T Stick Black (rev 1)"),
    ("0ccd:00b3", "Terratec NOXON DAB/DAB+ USB dongle (rev 1)"),
    ("0ccd:00b4", "Terratec Deutschlandradio DAB Stick"),
    ("0ccd:00b5", "Terratec NOXON DAB Stick - Radio Energy"),
    ("0ccd:00b7", "Terratec Media Broadcast DAB Stick"),
    ("0ccd:00b8", "Terratec BR DAB Stick"),
    ("0ccd:00b9", "Terratec WDR DAB Stick"),
    ("0ccd:00c0", "Terratec MuellerVerlag DAB Stick"),
    ("0ccd:00c6", "Terratec Fraunhofer DAB Stick"),
    ("0ccd:00d3", "Terratec Cinergy T Stick RC (Rev.3)"),
    ("0ccd:00d7", "Terratec T Stick PLUS"),
    ("0ccd:00e0", "Terratec NOXON DAB/DAB+ USB dongle (rev 2)"),
    ("1554:5020", "PixelView PV-DT235U(RN)"),
    ("15f4:0131", "Astrometa DVB-T/DVB-T2"),
    ("15f4:0133", "HanfTek DAB+FM+DVB-T"),
    ("185b:0620", "Compro Videomate U620F"),
    ("185b:0650", "Compro Videomate U650F"),
    ("185b:0680", "Compro Videomate U680F"),
    ("1b80:d393", "GIGABYTE GT-U7300"),
    ("1b80:d394", "DIKOM USB-DVBT HD"),
    ("1b80:d395", "Peak 102569AGPK"),
    ("1b80:d397", "KWorld KW-UB450-T USB DVB-T Pico TV"),
    ("1b80:d398", "Zaapa ZT-MINDVBZP"),
    ("1b80:d39d", "SVEON STV20 DVB-T USB & FM"),
    ("1b80:d3a4", "Twintech UT-40"),
    ("1b80:d3a8", "ASUS U3100MINI_PLUS_V2"),
    ("1b80:d3af", "SVEON STV27 DVB-T USB & FM"),
    ("1b80:d3b0", "SVEON STV21 DVB-T USB & FM"),
    ("1d19:1101", "Dexatek DK DVB-T Dongle (Logilink VG0002A)"),
    ("1d19:1102", "Dexatek DK DVB-T Dongle (MSI DigiVox mini II V3.0)"),
    ("1d19:1103", "Dexatek Technology Ltd. DK 5217 DVB-T Dongle"),
    ("1d19:1104", "MSI DigiVox Micro HD"),
    ("1f4d:a803", "Sweex DVB-T USB"),
    ("1f4d:b803", "GTek T803"),
    ("1f4d:c803", "Lifeview LV5TDeluxe"),
    ("1f4d:d286", "MyGica TD312"),
    ("1f4d:d803", "PROlectrix DV107669"),
];

pub fn rtl_stick(usb_id: &str) -> Option<&'static str> {
    RTL_STICKS.iter().find(|(k, _)| *k == usb_id).map(|(_, v)| *v)
}

pub const DVB_MODULE: &str = "dvb_usb_rtl28xxu";

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

// ---------- rtl_test -t ----------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RtlTestInfo {
    pub devices: Vec<(i64, String, String, String)>,
    pub using: String,
    /// "Rafael Micro R820T"
    pub tuner: String,
    pub gains: Vec<f64>,
    pub blog_v4: bool,
    /// The kernel DVB driver or another program has it.
    pub claimed: bool,
    /// librtlsdr built with DETACH_KERNEL_DRIVER kicked the DVB driver off.
    pub detached_kernel: bool,
    pub error: String,
}

impl RtlTestInfo {
    /// The JSON fields, in the TypeScript's order.
    pub fn to_map(&self) -> Map<String, Value> {
        let v = json!({
            "devices": self.devices.iter().map(|(i, v, p, s)| json!({ "index": i, "vendor": v, "product": p, "serial": s })).collect::<Vec<_>>(),
            "using": self.using,
            "tuner": self.tuner,
            "gains": self.gains.iter().map(|g| num(*g)).collect::<Vec<_>>(),
            "blogV4": self.blog_v4,
            "claimed": self.claimed,
            "detachedKernel": self.detached_kernel,
            "error": self.error,
        });
        v.as_object().cloned().unwrap_or_default()
    }
}

pub fn parse_rtl_test(out: &str) -> RtlTestInfo {
    static DEV: OnceLock<Regex> = OnceLock::new();
    static USING: OnceLock<Regex> = OnceLock::new();
    static TUNER: OnceLock<Regex> = OnceLock::new();
    static GAINS: OnceLock<Regex> = OnceLock::new();
    static BUSY: OnceLock<Regex> = OnceLock::new();
    static NODEV: OnceLock<Regex> = OnceLock::new();
    let mut info = RtlTestInfo::default();
    for raw in out.split('\n') {
        let l = raw.trim();
        if let Some(m) = re(&DEV, r"^(\d+):\s+(.*?),\s*(.*?),\s*SN:\s*(.*)$").captures(l) {
            info.devices.push((m[1].parse().unwrap_or(0), m[2].into(), m[3].into(), m[4].trim().into()));
        } else if let Some(m) = re(&USING, r"^Using device \d+:\s*(.*)$").captures(l) {
            info.using = m[1].into();
        } else if let Some(m) = re(&TUNER, r"^Found (.+?) tuner$").captures(l) {
            info.tuner = m[1].into();
        } else if let Some(m) = re(&GAINS, r"^Supported gain values \(\d+\):\s*(.*)$").captures(l) {
            info.gains = m[1].split_whitespace().filter_map(|g| g.parse::<f64>().ok()).collect();
        } else if l.to_lowercase().contains("rtl-sdr blog v4 detected") {
            info.blog_v4 = true;
        } else if l.to_lowercase().starts_with("detached kernel driver") {
            info.detached_kernel = true;
        } else if re(&BUSY, r"(?i)usb_claim_interface error|Kernel driver is active|claimed by second instance").is_match(l) {
            info.claimed = true;
            if info.error.is_empty() {
                info.error = "Device is busy: the kernel DVB driver or another program has it".into();
            }
        } else if re(&NODEV, r"(?i)No supported devices found|Failed to open rtlsdr device|No matching devices").is_match(l)
            && info.error.is_empty()
        {
            info.error = l.into();
        }
    }
    if info.blog_v4 && !info.tuner.is_empty() {
        info.tuner += " (RTL-SDR Blog V4)";
    }
    info
}

/// rtl_eeprom's dump: "Manufacturer:\t\tRealtek" etc., after "Current configuration".
pub fn parse_rtl_eeprom(out: &str) -> IndexMap<String, String> {
    static KV: OnceLock<Regex> = OnceLock::new();
    let mut r = IndexMap::new();
    let mut in_config = false;
    for l in out.split('\n') {
        if l.starts_with("Current configuration") {
            in_config = true;
        }
        if !in_config {
            continue;
        }
        if let Some(m) = re(&KV, r"^([A-Za-z][A-Za-z ]+):\s+(.*)$").captures(l.trim()) {
            r.insert(m[1].trim().to_string(), m[2].trim().to_string());
        }
    }
    r
}

// ---------- DVB driver blacklist ----------

/// Does any modprobe.d file blacklist (or neuter with `install … /bin/false`) the DVB driver?
pub fn find_blacklist(files: &[(String, String)], module: &str) -> (bool, Vec<String>) {
    let name = module.replace('-', "_");
    let bl = Regex::new(&format!(r"^blacklist\s+{}$", regex::escape(&name))).unwrap();
    let inst = Regex::new(&format!(r"^install\s+{}\s+/bin/(false|true)", regex::escape(&name))).unwrap();
    let mut where_ = Vec::new();
    for (path, text) in files {
        for raw in text.split('\n') {
            let l = raw.split('#').next().unwrap_or("").trim().replace('-', "_");
            if bl.is_match(&l) || inst.is_match(&l) {
                where_.push(format!("{path}: {}", raw.trim()));
            }
        }
    }
    (!where_.is_empty(), where_)
}

// ---------- rtl_433 ----------

/// Keys that describe the reception rather than the sensor.
const META: [&str; 13] = ["time", "model", "id", "channel", "mic", "mod", "freq", "freq1", "freq2", "rssi", "snr", "noise", "protocol"];
/// Numeric fields worth a sparkline.
pub const HIST_FIELDS: [&str; 13] = [
    "temperature_C",
    "temperature_F",
    "humidity",
    "moisture",
    "pressure_hPa",
    "pressure_kPa",
    "pressure_PSI",
    "wind_avg_km_h",
    "wind_avg_m_s",
    "rain_mm",
    "light_lux",
    "uv",
    "power_W",
];
const HIST_N: usize = 120;

#[derive(Debug, Clone)]
pub struct Rtl433Device {
    pub key: String,
    pub model: String,
    pub id: String,
    pub channel: String,
    pub protocol: Value,
    pub count: u64,
    pub first_seen: f64,
    pub last_seen: f64,
    pub rssi: Value,
    pub snr: Value,
    pub freq: Value,
    /// Latest sensor values.
    pub fields: Map<String, Value>,
    /// field → [t ms, value]
    pub hist: IndexMap<String, Vec<(f64, Value)>>,
}

impl Rtl433Device {
    pub fn to_json(&self, with_hist: bool) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("key".into(), self.key.clone().into());
        m.insert("model".into(), self.model.clone().into());
        m.insert("id".into(), self.id.clone().into());
        m.insert("channel".into(), self.channel.clone().into());
        m.insert("protocol".into(), self.protocol.clone());
        m.insert("count".into(), self.count.into());
        m.insert("firstSeen".into(), num(self.first_seen));
        m.insert("lastSeen".into(), num(self.last_seen));
        m.insert("rssi".into(), self.rssi.clone());
        m.insert("snr".into(), self.snr.clone());
        m.insert("freq".into(), self.freq.clone());
        m.insert("fields".into(), Value::Object(self.fields.clone()));
        if with_hist {
            let h: Map<String, Value> =
                self.hist.iter().map(|(k, v)| (k.clone(), Value::Array(v.iter().map(|(t, x)| json!([num(*t), x])).collect()))).collect();
            m.insert("hist".into(), Value::Object(h));
        }
        m
    }
}

#[derive(Debug, Clone)]
pub struct Rtl433Message {
    pub seq: u64,
    pub t: f64,
    pub key: String,
    pub msg: Value,
}

impl Rtl433Message {
    pub fn to_json(&self) -> Value {
        json!({ "seq": self.seq, "t": num(self.t), "key": self.key, "msg": self.msg })
    }
}

/// rtl_433 -M time:unix:usec gives "1729123456.123456"; plain -M time a local
/// "YYYY-MM-DD HH:MM:SS" (read as UTC here).
pub fn rtl433_time(v: Option<&Value>, fallback: f64) -> f64 {
    match v {
        Some(Value::Number(n)) => {
            let f = n.as_f64().unwrap_or(f64::NAN);
            if f > 1e12 { f } else { f * 1000.0 }
        }
        Some(Value::String(s)) => {
            static DIGITS: OnceLock<Regex> = OnceLock::new();
            if re(&DIGITS, r"^\d+(\.\d+)?$").is_match(s) {
                return js_round(parse_float(s).unwrap_or(f64::NAN) * 1000.0);
            }
            parse_iso(&s.replacen(' ', "T", 1)).map(|ms| ms as f64).unwrap_or(fallback)
        }
        _ => fallback,
    }
}

pub fn device_key(m: &Value) -> String {
    let st = m.get("subtype").filter(|v| !v.is_null()).or_else(|| m.get("type"));
    [m.get("model"), st, m.get("id"), m.get("channel")]
        .into_iter()
        .flatten()
        .filter(|v| !v.is_null() && v.as_str() != Some(""))
        .map(js_string)
        .collect::<Vec<_>>()
        .join("/")
}

pub struct Rtl433Aggregator {
    pub devices: IndexMap<String, Rtl433Device>,
    pub messages: Vec<Rtl433Message>,
    pub log: Vec<(f64, String)>,
    pub seq: u64,
    max_messages: usize,
    max_log: usize,
}

impl Default for Rtl433Aggregator {
    fn default() -> Self {
        Self::new(300, 200)
    }
}

impl Rtl433Aggregator {
    pub fn new(max_messages: usize, max_log: usize) -> Self {
        Self { devices: IndexMap::new(), messages: Vec::new(), log: Vec::new(), seq: 0, max_messages, max_log }
    }

    /// Feed one output line: JSON becomes a message, anything else a log line.
    pub fn line(&mut self, raw: &str, now: f64) -> Option<String> {
        let line = raw.trim();
        if line.is_empty() {
            return None;
        }
        if !line.starts_with('{') {
            self.log.push((now, line.to_string()));
            if self.log.len() > self.max_log {
                self.log.remove(0);
            }
            return None;
        }
        let Some(m) = parse_json(line).filter(Value::is_object) else {
            self.log.push((now, line.to_string()));
            return None;
        };
        if !m.get("model").is_some_and(truthy) {
            return None;
        }
        Some(self.ingest(m, now))
    }

    pub fn ingest(&mut self, m: Value, now: f64) -> String {
        let t = rtl433_time(m.get("time"), now);
        let key = device_key(&m);
        let d = self.devices.entry(key.clone()).or_insert_with(|| Rtl433Device {
            key: key.clone(),
            model: js_string(&m["model"]),
            id: m.get("id").filter(|v| !v.is_null()).map(js_string).unwrap_or_default(),
            channel: m.get("channel").filter(|v| !v.is_null()).map(js_string).unwrap_or_default(),
            protocol: Value::Null,
            count: 0,
            first_seen: t,
            last_seen: t,
            rssi: Value::Null,
            snr: Value::Null,
            freq: Value::Null,
            fields: Map::new(),
            hist: IndexMap::new(),
        });
        d.count += 1;
        d.last_seen = t;
        for (k, slot) in [("protocol", &mut d.protocol), ("rssi", &mut d.rssi), ("snr", &mut d.snr), ("freq", &mut d.freq)] {
            if let Some(v @ Value::Number(_)) = m.get(k) {
                *slot = v.clone();
            }
        }
        if let Some(o) = m.as_object() {
            for (k, v) in o {
                if !META.contains(&k.as_str()) {
                    d.fields.insert(k.clone(), v.clone());
                }
            }
        }
        for f in HIST_FIELDS {
            let Some(v @ Value::Number(_)) = m.get(f) else { continue };
            let h = d.hist.entry(f.to_string()).or_default();
            h.push((t, v.clone()));
            if h.len() > HIST_N {
                h.remove(0);
            }
        }
        self.seq += 1;
        self.messages.push(Rtl433Message { seq: self.seq, t, key: key.clone(), msg: m });
        if self.messages.len() > self.max_messages {
            self.messages.remove(0);
        }
        key
    }

    /// Newest first.
    pub fn list(&self) -> Vec<&Rtl433Device> {
        let mut v: Vec<&Rtl433Device> = self.devices.values().collect();
        v.sort_by(|a, b| b.last_seen.partial_cmp(&a.last_seen).unwrap_or(std::cmp::Ordering::Equal));
        v
    }

    pub fn clear(&mut self) {
        self.devices.clear();
        self.messages.clear();
        self.log.clear();
    }
}

/// `arr.slice(-n)`: the last n (all of it for 0, everything after the first |n| for n < 0).
pub fn js_slice_last<T>(v: &[T], n: f64) -> &[T] {
    if n.is_nan() || n == f64::INFINITY {
        return v;
    }
    if n == f64::NEG_INFINITY {
        return &v[v.len()..];
    }
    let k = n.trunc();
    if k > 0.0 { &v[v.len().saturating_sub(k as usize)..] } else { &v[((-k) as usize).min(v.len())..] }
}

// ---------- /proc/net/tcp ----------

/// Established TCP peers connected to a local port, from /proc/net/tcp{,6} text.
pub fn tcp_peers(proc_net_tcp: &str, port: u16) -> Vec<String> {
    let mut out = Vec::new();
    for l in proc_net_tcp.split('\n').skip(1) {
        let f: Vec<&str> = l.split_whitespace().collect();
        // 01 = ESTABLISHED
        if f.len() < 4 || f[3] != "01" {
            continue;
        }
        let lp = f[1].split(':').nth(1).unwrap_or("");
        if u32::from_str_radix(lp, 16).ok() != Some(port as u32) {
            continue;
        }
        let mut r = f[2].split(':');
        let (ra, rp) = (r.next().unwrap_or(""), r.next().unwrap_or(""));
        out.push(format!("{}:{}", hex_ip(ra), u32::from_str_radix(rp, 16).map(|p| p.to_string()).unwrap_or_else(|_| "NaN".into())));
    }
    out
}

fn hex_ip(h: &str) -> String {
    let byte = |i: usize| h.get(i..i + 2).and_then(|s| u8::from_str_radix(s, 16).ok());
    if h.len() == 8 {
        return [3, 2, 1, 0]
            .iter()
            .map(|i| byte(i * 2).map(|b| b.to_string()).unwrap_or_else(|| "NaN".into()))
            .collect::<Vec<_>>()
            .join(".");
    }
    // IPv6: four little-endian 32-bit words. Show v4-mapped addresses as plain IPv4.
    let mut full = String::new();
    for w in 0..4 {
        let word = h.get(w * 8..w * 8 + 8).unwrap_or("");
        let pairs: Vec<&str> = (0..word.len() / 2).map(|i| &word[i * 2..i * 2 + 2]).collect();
        full.extend(pairs.into_iter().rev());
    }
    let full = full.to_lowercase();
    if full.starts_with("00000000000000000000ffff") {
        return hex_ip(h.get(24..32).unwrap_or(""));
    }
    let groups: Vec<String> = (0..full.len() / 4)
        .map(|i| {
            let g = &full[i * 4..i * 4 + 4];
            let t = g.trim_start_matches('0');
            if t.is_empty() { "0".to_string() } else { t.to_string() }
        })
        .collect();
    static ZEROS: OnceLock<Regex> = OnceLock::new();
    re(&ZEROS, r"(^|:)0(:0)+(:|$)").replace(&groups.join(":"), "::").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RTL433: &str = include_str!("../fixtures/rtl433.jsonl");

    #[test]
    fn rtl_test_r820t() {
        let i = parse_rtl_test(
            "Found 1 device(s):
  0:  Realtek, RTL2838UHIDIR, SN: 00000001

Using device 0: Generic RTL2832U OEM
Detached kernel driver
Found Rafael Micro R820T tuner
Supported gain values (29): 0.0 0.9 1.4 2.7 3.7 7.7 8.7 12.5 14.4 15.7 16.6 19.7 20.7 22.9 25.4 28.0 29.7 32.8 33.8 36.4 37.2 38.6 40.2 42.1 43.4 43.9 44.5 48.0 49.6
[R82XX] PLL not locked!
Sampling at 2048000 S/s.
No E4000 tuner found, aborting.
Reattached kernel driver",
        );
        assert_eq!(i.devices, vec![(0, "Realtek".into(), "RTL2838UHIDIR".into(), "00000001".into())]);
        assert_eq!(i.tuner, "Rafael Micro R820T");
        assert_eq!(i.gains.len(), 29);
        assert_eq!(*i.gains.last().unwrap(), 49.6);
        assert!(i.detached_kernel);
        assert_eq!(i.error, "");
        assert_eq!(serde_json::to_string(&i.to_map()["gains"][0]).unwrap(), "0");
    }

    #[test]
    fn rtl_test_blog_v4_e4000_fc0013() {
        let i = parse_rtl_test(
            "Found 1 device(s):
  0:  RTLSDRBlog, Blog V4, SN: 00000001

Using device 0: Generic RTL2832U OEM
Found Rafael Micro R828D tuner
RTL-SDR Blog V4 Detected
Supported gain values (29): 0.0 0.9 1.4",
        );
        assert_eq!(i.tuner, "Rafael Micro R828D (RTL-SDR Blog V4)");
        assert!(i.blog_v4);
        assert_eq!(parse_rtl_test("Found Elonics E4000 tuner").tuner, "Elonics E4000");
        assert_eq!(parse_rtl_test("Found Fitipower FC0013 tuner").tuner, "Fitipower FC0013");
    }

    #[test]
    fn rtl_test_claimed_and_missing() {
        let i = parse_rtl_test(
            "Found 1 device(s):
  0:  Realtek, RTL2838UHIDIR, SN: 00000001

Using device 0: Generic RTL2832U OEM
usb_claim_interface error -6
Failed to open rtlsdr device #0.",
        );
        assert!(i.claimed);
        assert!(i.error.contains("busy"));
        // Real desky output.
        let i = parse_rtl_test("No supported devices found.");
        assert!(i.devices.is_empty());
        assert_eq!(i.error, "No supported devices found.");
    }

    #[test]
    fn rtl_eeprom_dump() {
        let e = parse_rtl_eeprom(
            "Found 1 device(s):
  0:  Generic RTL2832U OEM

Using device 0: Generic RTL2832U OEM
Found Rafael Micro R820T tuner

Current configuration:
__________________________________________
Vendor ID:\t\t0x0bda
Product ID:\t\t0x2838
Manufacturer:\t\tRealtek
Product:\t\tRTL2838UHIDIR
Serial number:\t\t00000001
Serial number enabled:\tyes
IR endpoint enabled:\tyes
Remote wakeup enabled:\tno
__________________________________________",
        );
        assert_eq!(e["Vendor ID"], "0x0bda");
        assert_eq!(e["Manufacturer"], "Realtek");
        assert_eq!(e["Product"], "RTL2838UHIDIR");
        assert_eq!(e["Serial number"], "00000001");
        assert!(!e.contains_key("Found Rafael Micro R820T tuner"));
    }

    #[test]
    fn stick_table() {
        assert!(rtl_stick("0bda:2838").is_some());
        assert!(rtl_stick("0bda:2832").is_some());
        assert_eq!(RTL_STICKS.len(), 42);
    }

    #[test]
    fn dvb_blacklist_detection() {
        let f = |p: &str, t: &str| vec![(p.to_string(), t.to_string())];
        assert!(!find_blacklist(&f("/etc/modprobe.d/blacklist-8192cu.conf", "blacklist 8192cu\n"), DVB_MODULE).0);
        let r = find_blacklist(&f("/etc/modprobe.d/rtl.conf", "# rtl-sdr\nblacklist dvb_usb_rtl28xxu\nblacklist rtl2832\n"), DVB_MODULE);
        assert_eq!(r, (true, vec!["/etc/modprobe.d/rtl.conf: blacklist dvb_usb_rtl28xxu".to_string()]));
        assert!(find_blacklist(&f("x", "install dvb-usb-rtl28xxu /bin/false"), DVB_MODULE).0);
        assert!(!find_blacklist(&f("x", "#blacklist dvb_usb_rtl28xxu"), DVB_MODULE).0);
    }

    #[test]
    fn rtl433_fixture_stream() {
        let mut a = Rtl433Aggregator::default();
        for l in RTL433.split('\n') {
            a.line(l, 1.0);
        }
        assert!(a.log.iter().any(|(_, l)| l == "Tuned to 433.920MHz."));
        let mut keys: Vec<String> = a.list().iter().map(|d| d.key.clone()).collect();
        let newest = keys[0].clone();
        keys.sort();
        assert_eq!(
            keys,
            ["Acurite-Tower/2483/A", "Acurite-Tower/9120/C", "Fineoffset-WH51/0d0b1c", "LaCrosse-TX141THBv2/192/0", "Toyota/TPMS/f1e2d3c4"]
        );
        let tower = &a.devices["Acurite-Tower/2483/A"];
        assert_eq!(tower.count, 3);
        assert_eq!(Value::Object(tower.fields.clone()), json!({ "battery_ok": 0, "temperature_C": 22.5, "humidity": 46 }));
        let t: Vec<&Value> = tower.hist["temperature_C"].iter().map(|(_, v)| v).collect();
        assert_eq!(t, [&json!(22.3), &json!(22.4), &json!(22.5)]);
        assert_eq!(tower.hist["temperature_C"][0].0, 1792226000123.0);
        assert_eq!(tower.protocol, 40);
        assert_eq!(tower.rssi, -0.5);
        assert_eq!(tower.snr, 22);
        let wh51 = &a.devices["Fineoffset-WH51/0d0b1c"];
        assert_eq!(wh51.fields["moisture"], 35);
        // "battery_ok" : 1.000 prints as JavaScript prints it.
        assert_eq!(wh51.fields["battery_ok"].to_string(), "1");
        assert_eq!(wh51.hist["moisture"].len(), 1);
        assert_eq!(a.devices["Toyota/TPMS/f1e2d3c4"].fields["pressure_PSI"], 32.5);
        assert_eq!(a.messages.len(), 7);
        assert_eq!(a.messages.last().unwrap().seq, 7);
        assert_eq!(newest, "Acurite-Tower/9120/C");
        let j = tower.to_json(true);
        assert_eq!(
            j.keys().cloned().collect::<Vec<_>>(),
            ["key", "model", "id", "channel", "protocol", "count", "firstSeen", "lastSeen", "rssi", "snr", "freq", "fields", "hist"]
        );
        assert_eq!(j["hist"]["temperature_C"][0], json!([1792226000123i64, 22.3]));
    }

    #[test]
    fn ring_buffers_are_bounded() {
        let mut a = Rtl433Aggregator::new(5, 3);
        for i in 0..20 {
            a.line(&json!({ "time": (1e9 as i64 + i).to_string(), "model": "X", "id": 1, "temperature_C": i }).to_string(), 0.0);
        }
        for i in 0..10 {
            a.line(&format!("log {i}"), 0.0);
        }
        assert_eq!(a.messages.len(), 5);
        assert_eq!(a.log.len(), 3);
        assert_eq!(a.devices["X/1"].count, 20);
    }

    #[test]
    fn time_formats_and_keys() {
        assert_eq!(rtl433_time(Some(&json!("1792226000.5")), 0.0), 1792226000500.0);
        assert_eq!(rtl433_time(None, 42.0), 42.0);
        assert_eq!(rtl433_time(Some(&json!("2026-10-17 08:33:20")), 0.0), 1792226000000.0);
        assert_eq!(device_key(&json!({ "model": "Nexus-TH", "id": 129, "channel": 1 })), "Nexus-TH/129/1");
        assert_eq!(device_key(&json!({ "model": "Toyota", "type": "TPMS", "id": "f1" })), "Toyota/TPMS/f1");
    }

    #[test]
    fn slices() {
        let v = [1, 2, 3, 4];
        assert_eq!(js_slice_last(&v, 2.0), [3, 4]);
        assert_eq!(js_slice_last(&v, 0.0), v);
        assert_eq!(js_slice_last(&v, -1.0), [2, 3, 4]);
        assert_eq!(js_slice_last(&v, 9.0), v);
    }

    #[test]
    fn proc_net_tcp_peers_for_rtl_tcp_clients() {
        let v4 = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:04D2 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1 0000000000000000 100 0 0 10 0
   1: 2D07FF0A:04D2 0507FF0A:D431 01 00000000:00000000 00:00000000 00000000  1000        0 2 1 0000000000000000 20 4 30 10 -1";
        assert_eq!(tcp_peers(v4, 1234), ["10.255.7.5:54321"]);
        let v6 = "  sl  local_address                         remote_address                        st
   0: 0000000000000000FFFF00002D07FF0A:04D2 0000000000000000FFFF00000507FF0A:D431 01 0";
        assert_eq!(tcp_peers(v6, 1234), ["10.255.7.5:54321"]);
        // A real IPv6 peer (fe80::1 as the kernel writes it).
        assert_eq!(hex_ip("000080FE000000000000000001000000"), "fe80::1");
    }
}
