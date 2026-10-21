//! Wi-Fi parsers (peripherals/lib/wifi-parse.ts).
//!
//! Pure functions over the text that iw, wpa_supplicant, udhcpc, ping, nslookup and curl
//! print, so the driver stays about orchestration and all of this is unit-testable against
//! fixtures captured on desky (peripherals/lib/wifi-fixtures/). iw's output is meant for
//! humans and changes between versions, so every parser here is section-based and tolerant:
//! unknown lines are skipped, missing ones leave fields unset.

use std::collections::HashMap;

use benchbob_core::http::{parse_float, parse_int};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::Value;

use crate::json::{O, num};

/// A compiled-once regex.
macro_rules! re {
    ($s:expr) => {{
        static R: std::sync::LazyLock<::regex::Regex> = std::sync::LazyLock::new(|| ::regex::Regex::new($s).unwrap());
        &*R
    }};
}
pub(crate) use re;

fn cap<'a>(r: &Regex, s: &'a str, i: usize) -> Option<&'a str> {
    r.captures(s).and_then(|c| c.get(i)).map(|m| m.as_str())
}

/// JavaScript's `parseFloat` of an optional string, as `num()` in the TypeScript.
fn numo(s: Option<&str>) -> Option<f64> {
    s.and_then(parse_float)
}

// ---------- Frequencies, channels, bands ----------

pub fn band_of(mhz: f64) -> Option<&'static str> {
    if (2400.0..2500.0).contains(&mhz) {
        Some("2.4")
    } else if (4900.0..5925.0).contains(&mhz) {
        Some("5")
    } else if (5925.0..7126.0).contains(&mhz) {
        Some("6")
    } else if (57000.0..72000.0).contains(&mhz) {
        Some("60")
    } else {
        None
    }
}

/// `Math.round`: half rounds toward +∞.
pub fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// IEEE channel number for a centre frequency (the band decides the formula).
pub fn freq_to_channel(mhz: f64) -> f64 {
    let f = js_round(mhz);
    if f == 2484.0 {
        14.0
    } else if (2407.0..2484.0).contains(&f) {
        (f - 2407.0) / 5.0
    } else if f == 5935.0 {
        2.0 // 6 GHz channel 2 is the odd one out
    } else if (5950.0..=7125.0).contains(&f) {
        (f - 5950.0) / 5.0
    } else if (4910.0..5925.0).contains(&f) {
        (f - 5000.0) / 5.0
    } else if (58320.0..=70200.0).contains(&f) {
        (f - 56160.0) / 2160.0
    } else {
        0.0
    }
}

pub fn channel_to_freq(ch: f64, band: &str) -> f64 {
    match band {
        "2.4" => {
            if ch == 14.0 {
                2484.0
            } else {
                2407.0 + ch * 5.0
            }
        }
        "5" => 5000.0 + ch * 5.0,
        "6" => {
            if ch == 2.0 {
                5935.0
            } else {
                5950.0 + ch * 5.0
            }
        }
        _ => 56160.0 + ch * 2160.0,
    }
}

/// U-NII sub-band for 5/6 GHz frequencies; DFS bands are where radar detection applies (US rules).
pub fn unii_of(mhz: f64) -> Option<(&'static str, bool)> {
    let r = |a: f64, b: f64| mhz >= a && mhz < b;
    if r(5150.0, 5250.0) {
        Some(("U-NII-1", false))
    } else if r(5250.0, 5350.0) {
        Some(("U-NII-2A", true))
    } else if r(5470.0, 5730.0) {
        Some(("U-NII-2C", true))
    } else if r(5730.0, 5850.0) {
        Some(("U-NII-3", false))
    } else if r(5850.0, 5925.0) {
        Some(("U-NII-4", false))
    } else if r(5925.0, 6425.0) {
        Some(("U-NII-5", false))
    } else if r(6425.0, 6525.0) {
        Some(("U-NII-6", false))
    } else if r(6525.0, 6875.0) {
        Some(("U-NII-7", false))
    } else if (6875.0..=7125.0).contains(&mhz) {
        Some(("U-NII-8", false))
    } else {
        None
    }
}

// ---------- Generic helpers ----------

fn tabs(l: &str) -> usize {
    l.bytes().take_while(|b| *b == b'\t').count()
}

/// iw prints SSIDs with unprintable bytes, leading/trailing spaces and backslashes as \xNN.
/// Returns the text (invalid UTF-8 replaced) and the raw bytes.
pub fn decode_iw_string(s: &str) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' && b[i + 2].is_ascii_hexdigit() && b[i + 3].is_ascii_hexdigit() {
            bytes.push(u8::from_str_radix(&s[i + 2..i + 4], 16).unwrap());
            i += 4;
        } else {
            bytes.push(b[i]);
            i += 1;
        }
    }
    (String::from_utf8_lossy(&bytes).into_owned(), bytes)
}

// ---------- iw dev ----------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct IwIface {
    /// "phy0"
    pub phy: String,
    /// None for wdev-only (P2P-device)
    pub name: Option<String>,
    pub ifindex: Option<f64>,
    pub wdev: Option<String>,
    pub addr: Option<String>,
    pub ssid: Option<String>,
    pub r#type: Option<String>,
    pub freq: Option<f64>,
    pub channel: Option<f64>,
    pub width: Option<String>,
    pub center1: Option<f64>,
    pub txpower: Option<f64>,
}

pub fn parse_iw_dev(out: &str) -> Vec<IwIface> {
    let mut res: Vec<IwIface> = Vec::new();
    let mut phy = String::new();
    let mut cur: Option<usize> = None;
    for line in out.split('\n') {
        if let Some(n) = cap(re!(r"^phy#(\d+)"), line, 1) {
            phy = format!("phy{n}");
            cur = None;
        } else if let Some(n) = cap(re!(r"^\tInterface (\S+)"), line, 1) {
            res.push(IwIface { phy: phy.clone(), name: Some(n.into()), ..Default::default() });
            cur = Some(res.len() - 1);
        } else if line.starts_with("\tUnnamed/non-netdev interface") {
            res.push(IwIface { phy: phy.clone(), name: None, ..Default::default() });
            cur = Some(res.len() - 1);
        } else if let Some(i) = cur
            && let Some(c) = re!(r"^\t\t(\S+) (.*)$").captures(line)
        {
            let (k, v) = (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str());
            let cur = &mut res[i];
            match k {
                "ifindex" => cur.ifindex = Some(parse_int(v).map(|n| n as f64).unwrap_or(f64::NAN)),
                "wdev" => cur.wdev = Some(v.into()),
                "addr" => cur.addr = Some(v.into()),
                "ssid" => cur.ssid = Some(decode_iw_string(v).0),
                "type" => cur.r#type = Some(v.into()),
                "txpower" => cur.txpower = parse_float(v),
                "channel" => {
                    // "44 (5220 MHz), width: 40 MHz, center1: 5230 MHz"
                    if let Some(c) = re!(r"^(\d+) \((\d+(?:\.\d+)?) MHz\)(?:, width: ([^,]+))?(?:, center1: (\d+) MHz)?").captures(v) {
                        cur.channel = c.get(1).and_then(|m| m.as_str().parse().ok());
                        cur.freq = c.get(2).and_then(|m| m.as_str().parse().ok());
                        cur.width = c.get(3).map(|m| m.as_str().trim().to_string());
                        cur.center1 = c.get(4).and_then(|m| m.as_str().parse().ok());
                    }
                }
                _ => {}
            }
        }
    }
    res
}

// ---------- iw phy ----------

#[derive(Debug, Clone, PartialEq)]
pub struct PhyChannel {
    pub mhz: f64,
    pub channel: f64,
    pub band: &'static str,
    pub disabled: bool,
    pub radar: bool,
    pub no_ir: bool,
    /// dBm
    pub max_power: Option<f64>,
    /// every parenthesised note iw printed, e.g. "no 160MHz"
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhyBand {
    pub band: &'static str,
    pub ht: bool,
    pub vht: bool,
    pub he: bool,
    pub eht: bool,
    /// Widest channel this band's capabilities allow, in MHz.
    pub max_width: f64,
    pub channels: Vec<PhyChannel>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PhyInfo {
    pub phy: String,
    pub iftypes: Vec<String>,
    pub software_iftypes: Vec<String>,
    pub ciphers: Vec<String>,
    pub commands: Vec<String>,
    pub bands: Vec<PhyBand>,
    pub monitor: bool,
    pub ap: bool,
    /// set_wiphy_netns supported
    pub netns: bool,
    /// driver can do SAE (SME authenticate or offload)
    pub sae: bool,
    pub ext_features: Vec<String>,
}

pub fn parse_iw_phy(out: &str) -> PhyInfo {
    let mut info = PhyInfo { phy: cap(re!(r"Wiphy (\S+)"), out, 1).unwrap_or("").into(), ..Default::default() };
    let mut section = String::new();
    // (band, capability text)
    let mut bands: Vec<(PhyBand, String)> = Vec::new();
    let mut in_band = false;
    let mut band_sub = String::new();
    for line in out.split('\n') {
        let t = tabs(line);
        let s = line.trim();
        if t == 1 {
            in_band = false;
            section = s.to_string();
            if re!(r"^Band (\d+):").is_match(s) {
                bands.push((
                    PhyBand { band: "2.4", ht: false, vht: false, he: false, eht: false, max_width: 20.0, channels: Vec::new() },
                    String::new(),
                ));
                in_band = true;
            }
            if re!(r"(?i)supports SAE").is_match(s) {
                info.sae = true;
            }
            continue;
        }
        if in_band {
            let (band, caps) = bands.last_mut().unwrap();
            if t == 2 {
                band_sub = s.to_string();
            }
            caps.push_str(s);
            caps.push('\n');
            if let Some(f) = re!(r"^\* (\d+(?:\.\d+)?) MHz \[(\d+)\](.*)$").captures(s)
                && band_sub.starts_with("Frequencies")
            {
                let mhz: f64 = f[1].parse().unwrap_or(f64::NAN);
                let flags: Vec<String> = re!(r"\(([^)]*)\)").captures_iter(&f[3]).map(|x| x[1].to_string()).collect();
                let pw = flags.iter().find_map(|x| cap(re!(r"^([\d.]+) dBm$"), x, 1));
                band.channels.push(PhyChannel {
                    mhz,
                    channel: f[2].parse().unwrap_or(f64::NAN),
                    band: band_of(mhz).unwrap_or("2.4"),
                    disabled: flags.iter().any(|x| x == "disabled"),
                    radar: flags.iter().any(|x| re!(r"(?i)radar").is_match(x)),
                    no_ir: flags.iter().any(|x| re!(r"(?i)no IR|no-IR|passive scan").is_match(x)),
                    max_power: pw.map(|p| parse_float(p).unwrap_or(f64::NAN)),
                    flags: flags.iter().filter(|x| !x.ends_with("dBm")).cloned().collect(),
                });
            }
            continue;
        }
        let Some(item) = cap(re!(r"^\* (.*)$"), s, 1).filter(|i| !i.is_empty()) else { continue };
        if section.starts_with("Supported interface modes") {
            info.iftypes.push(item.into());
        } else if section.starts_with("software interface modes") {
            info.software_iftypes.push(item.into());
        } else if section.starts_with("Supported Ciphers") {
            info.ciphers.push(re!(r"\s*\(.*\)$").replace(item, "").into_owned());
        } else if section.starts_with("Supported commands") {
            info.commands.push(item.into());
        } else if section.starts_with("Supported extended features")
            && let Some(e) = cap(re!(r"\[ (\S+) \]"), item, 1)
        {
            info.ext_features.push(e.into());
        }
    }
    for (b, caps) in &mut bands {
        if let Some(c) = b.channels.first() {
            b.band = c.band;
        }
        b.ht = re!(r"HT20/HT40|HT20|HT Capabilities|HT TX/RX MCS").is_match(caps);
        b.vht = caps.contains("VHT Capabilities");
        b.he = re!(r"HE Iftypes|HE MAC Capabilities").is_match(caps);
        b.eht = re!(r"EHT Iftypes|EHT MAC Capabilities").is_match(caps);
        let mut w = if b.ht && caps.contains("HT20/HT40") { 40.0 } else { 20.0 };
        if b.vht || (b.he && b.band != "2.4") {
            w = 80.0;
        }
        if re!(r"160 MHz|160MHz|HE160").is_match(caps) && !caps.contains("neither 160 nor 80+80") {
            w = 160.0;
        }
        if b.eht && re!(r"320MHz|320 MHz").is_match(caps) {
            w = 320.0;
        }
        if b.band == "2.4" {
            w = f64::min(w, 40.0);
        }
        b.max_width = w;
    }
    // S1G (802.11ah, 902–928 MHz; mac80211_hwsim has one) lists frequencies without channel
    // numbers, so it parses to nothing; drop it and anything else we can't place.
    info.bands = bands.into_iter().map(|(b, _)| b).filter(|b| b.channels.first().is_some_and(|c| band_of(c.mhz).is_some())).collect();
    info.monitor = info.iftypes.iter().any(|x| x == "monitor") || info.software_iftypes.iter().any(|x| x == "monitor");
    info.ap = info.iftypes.iter().any(|x| x == "AP");
    info.netns = info.commands.iter().any(|x| x == "set_wiphy_netns");
    if info.ext_features.iter().any(|e| e.contains("SAE_OFFLOAD")) {
        info.sae = true;
    }
    info
}

// ---------- iw reg get ----------

#[derive(Debug, Clone, PartialEq)]
pub struct RegRule {
    pub start_mhz: f64,
    pub end_mhz: f64,
    pub max_bw_mhz: f64,
    /// dBm (NaN when iw printed something unparsable, as parseFloat gives)
    pub max_eirp: Option<f64>,
    pub dfs_cac_ms: Option<f64>,
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegDomain {
    /// "global" or "phy0"
    pub scope: String,
    pub self_managed: bool,
    /// "US", "00" (world), "99" (driver-defined)
    pub country: String,
    /// "DFS-FCC"
    pub dfs_region: Option<String>,
    pub rules: Vec<RegRule>,
}

impl RegRule {
    pub fn json(&self) -> Value {
        O::new()
            .n("startMhz", self.start_mhz)
            .n("endMhz", self.end_mhz)
            .n("maxBwMhz", self.max_bw_mhz)
            .on("maxEirp", self.max_eirp)
            .on("dfsCacMs", self.dfs_cac_ms)
            .k("flags", self.flags.clone())
            .v()
    }
}

impl RegDomain {
    pub fn json(&self) -> Value {
        O::new()
            .k("scope", self.scope.clone())
            .k("selfManaged", self.self_managed)
            .k("country", self.country.clone())
            .o("dfsRegion", self.dfs_region.clone())
            .k("rules", self.rules.iter().map(RegRule::json).collect::<Vec<_>>())
            .v()
    }
}

pub fn parse_iw_reg(out: &str) -> Vec<RegDomain> {
    let mut res: Vec<RegDomain> = Vec::new();
    let mut scope = "global".to_string();
    let mut self_managed = false;
    for line in out.split('\n') {
        if line.starts_with("global") {
            scope = "global".into();
            self_managed = false;
        } else if let Some(c) = re!(r"^phy#(\d+)(.*)$").captures(line) {
            scope = format!("phy{}", &c[1]);
            self_managed = c[2].contains("self-managed");
        } else if let Some(c) = re!(r"^country (\S+?):\s*(\S+)?").captures(line) {
            res.push(RegDomain {
                scope: scope.clone(),
                self_managed,
                country: c[1].into(),
                dfs_region: c.get(2).map(|m| m.as_str().into()),
                rules: Vec::new(),
            });
        } else if let Some(cur) = res.last_mut()
            && let Some(m) = re!(r"^\s+\((\d+) - (\d+) @ (\d+)\), \(([^)]*)\)(?:, \(([^)]*)\))?(.*)$").captures(line)
        {
            let eirp = m[4].split(',').nth(1).map(str::trim);
            let cac = m.get(5).and_then(|x| cap(re!(r"(\d+) ms"), x.as_str(), 1));
            cur.rules.push(RegRule {
                start_mhz: m[1].parse().unwrap_or(f64::NAN),
                end_mhz: m[2].parse().unwrap_or(f64::NAN),
                max_bw_mhz: m[3].parse().unwrap_or(f64::NAN),
                max_eirp: eirp.filter(|e| !e.is_empty() && *e != "N/A").map(|e| parse_float(e).unwrap_or(f64::NAN)),
                dfs_cac_ms: cac.and_then(|c| c.parse().ok()),
                flags: m[6].split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect(),
            });
        }
    }
    res
}

// ---------- iw scan ----------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Wps {
    pub state: Option<String>,
    pub locked: Option<bool>,
    pub device: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Width {
    Mhz(f64),
    EightyPlusEighty,
}

impl Width {
    pub fn json(&self) -> Value {
        match self {
            Width::Mhz(m) => num(*m),
            Width::EightyPlusEighty => Value::from("80+80"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bss {
    pub bssid: String,
    pub iface: String,
    pub associated: bool,
    pub ssid: String,
    /// set when the SSID isn't clean UTF-8 text
    pub ssid_hex: Option<String>,
    pub hidden: bool,
    pub freq: f64,
    pub channel: f64,
    pub band: Option<&'static str>,
    pub width: Width,
    /// MHz, centre of the whole (possibly bonded) channel
    pub center_freq: f64,
    pub center_channel: f64,
    /// dBm
    pub signal: Option<f64>,
    pub last_seen_ms: Option<f64>,
    pub beacon_interval: Option<f64>,
    pub capability: Vec<String>,
    /// human label, e.g. "WPA2/WPA3 (transition)"
    pub security: &'static str,
    pub security_kind: &'static str,
    pub akm: Vec<String>,
    pub pairwise: Vec<String>,
    pub group: Option<String>,
    pub pmf: &'static str,
    /// ["n","ac","ax"]
    pub standards: Vec<&'static str>,
    /// "Wi-Fi 6"
    pub generation: Option<&'static str>,
    pub stations: Option<f64>,
    /// 0..100 %
    pub utilization: Option<f64>,
    pub country: Option<String>,
    pub wps: Option<Wps>,
    /// locally-administered BSSID (multi-SSID virtual APs, randomised)
    pub local_mac: bool,
}

impl Bss {
    /// The parsed BSS's keys, in parseIwScan's order.
    pub fn fields(&self, o: O) -> O {
        o.k("bssid", self.bssid.clone())
            .k("iface", self.iface.clone())
            .k("associated", self.associated)
            .k("ssid", self.ssid.clone())
            .o("ssidHex", self.ssid_hex.clone())
            .k("hidden", self.hidden)
            .n("freq", self.freq)
            .n("channel", self.channel)
            .k("band", self.band.map(Value::from).unwrap_or(Value::Null))
            .k("width", self.width.json())
            .n("centerFreq", self.center_freq)
            .n("centerChannel", self.center_channel)
            .on("signal", self.signal)
            .on("lastSeenMs", self.last_seen_ms)
            .on("beaconInterval", self.beacon_interval)
            .k("capability", self.capability.clone())
            .k("security", self.security)
            .k("securityKind", self.security_kind)
            .k("akm", self.akm.clone())
            .k("pairwise", self.pairwise.clone())
            .o("group", self.group.clone())
            .k("pmf", self.pmf)
            .k("standards", self.standards.clone())
            .o("generation", self.generation)
            .on("stations", self.stations)
            .on("utilization", self.utilization)
            .o("country", self.country.clone())
            .o(
                "wps",
                self.wps.as_ref().map(|w| {
                    O::new()
                        .o("state", w.state.clone())
                        .o("locked", w.locked)
                        .o("device", w.device.clone())
                        .o("manufacturer", w.manufacturer.clone())
                        .o("model", w.model.clone())
                        .v()
                }),
            )
            .k("localMac", self.local_mac)
    }

    pub fn json(&self) -> Value {
        self.fields(O::new()).v()
    }
}

struct Section {
    key: String,
    value: String,
    /// value + sub-lines, for regexes that don't care about layout
    body: String,
}

fn sections_of(lines: &[&str]) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let t = tabs(line);
        if t == 1 {
            out.push(match re!(r"^\t([^:]+?):\s?(.*)$").captures(line) {
                Some(m) => Section { key: m[1].trim().into(), value: m[2].into(), body: m[2].into() },
                None => Section { key: line.trim().into(), value: String::new(), body: String::new() },
            });
        } else if t >= 2
            && let Some(cur) = out.last_mut()
        {
            cur.body.push('\n');
            cur.body.push_str(line.trim());
        }
    }
    out
}

fn akm_num(n: &str) -> Option<&'static str> {
    Some(match n {
        "1" => "802.1X",
        "2" => "PSK",
        "3" => "FT/802.1X",
        "4" => "FT/PSK",
        "5" => "802.1X/SHA-256",
        "6" => "PSK/SHA-256",
        "8" => "SAE",
        "9" => "FT/SAE",
        "11" => "802.1X/SUITE-B",
        "12" => "802.1X/SUITE-B-192",
        "13" => "FT/802.1X/SHA-384",
        "14" => "FILS/SHA-256",
        "15" => "FILS/SHA-384",
        "18" => "OWE",
        "24" => "SAE-EXT-KEY",
        "25" => "FT/SAE-EXT-KEY",
        _ => return None,
    })
}

/// "FT/IEEE 802.1X IEEE 802.1X/SHA-256" → ["FT/802.1X", "802.1X/SHA-256"].
pub fn parse_akms(s: &str) -> Vec<String> {
    s.replace("IEEE 802.1X", "802.1X")
        .split_whitespace()
        .map(|a| match cap(re!(r"(?i)^00-0f-ac:(\d+)$"), a, 1) {
            Some(n) => akm_num(n).map(String::from).unwrap_or_else(|| a.to_string()),
            None => a.to_string(),
        })
        .collect()
}

pub fn classify_security(privacy: bool, rsn_akm: &[String], wpa_akm: &[String], mfp_required: bool) -> (&'static str, &'static str) {
    let all: Vec<&String> = rsn_akm.iter().chain(wpa_akm.iter()).collect();
    if all.is_empty() {
        return if privacy { ("WEP", "wep") } else { ("Open", "open") };
    }
    if all.iter().any(|a| *a == "OWE") {
        return ("OWE (Enhanced Open)", "owe");
    }
    if all.iter().any(|a| re!(r"802\.1X|FILS").is_match(a)) {
        let wpa3 = rsn_akm.iter().any(|a| re!(r"SUITE-B|SHA-384").is_match(a))
            || (mfp_required && rsn_akm.iter().all(|a| re!(r"SHA-256|SHA-384|SUITE-B|FILS").is_match(a)));
        let label = if wpa3 {
            "WPA3-Enterprise"
        } else if rsn_akm.is_empty() {
            "WPA-Enterprise"
        } else {
            "WPA2-Enterprise"
        };
        return (label, "eap");
    }
    let sae = all.iter().any(|a| a.contains("SAE"));
    let psk = all.iter().any(|a| a.contains("PSK"));
    if sae && psk {
        return ("WPA2/WPA3 (transition)", "psk-sae");
    }
    if sae {
        return ("WPA3-SAE", "sae");
    }
    if !rsn_akm.is_empty() && !wpa_akm.is_empty() {
        return ("WPA/WPA2-PSK", "psk");
    }
    if !wpa_akm.is_empty() {
        return ("WPA-PSK", "psk");
    }
    ("WPA2-PSK", "psk")
}

fn mac_is_local(mac: &str) -> bool {
    u8::from_str_radix(mac.get(0..2).unwrap_or("0"), 16).map(|b| b & 0x02 != 0).unwrap_or(false)
}

/// Split `iw scan` output into one block per BSS (lines starting "BSS xx:xx:…").
fn scan_blocks(out: &str) -> Vec<&str> {
    let head = re!(r"(?i)^BSS [0-9a-f]{2}(?::[0-9a-f]{2}){5}");
    let mut starts = Vec::new();
    let mut pos = 0;
    for line in out.split('\n') {
        if head.is_match(line) {
            starts.push(pos);
        }
        pos += line.len() + 1;
    }
    let mut v = Vec::new();
    for (i, s) in starts.iter().enumerate() {
        let e = starts.get(i + 1).copied().unwrap_or(out.len());
        v.push(&out[*s..e.min(out.len())]);
    }
    v
}

pub fn parse_iw_scan(out: &str) -> Vec<Bss> {
    let mut res = Vec::new();
    for block in scan_blocks(out) {
        let lines: Vec<&str> = block.split('\n').collect();
        let Some(head) = re!(r"(?i)^BSS ([0-9a-f:]{17})\s*\(on ([^)]+)\)(?:\s*--\s*(\S+))?").captures(lines[0]) else { continue };
        let secs = sections_of(&lines[1..]);
        let get = |k: &str| secs.iter().find(|s| s.key == k);
        let has = |r: &Regex| secs.iter().any(|s| r.is_match(&s.key));
        let val = |k: &str| get(k).map(|s| s.value.as_str()).unwrap_or("");
        let body = |k: &str| get(k).map(|s| s.body.as_str()).unwrap_or("");

        let freq = numo(get("freq").map(|s| s.value.as_str())).unwrap_or(0.0);
        let band = band_of(freq);
        let ch = freq_to_channel(freq);
        let channel = if ch != 0.0 && !ch.is_nan() {
            ch
        } else {
            numo(cap(re!(r"channel (\d+)"), val("DS Parameter set"), 1)).filter(|c| *c != 0.0).unwrap_or(0.0)
        };

        // SSID: missing, empty, or all NULs means hidden.
        let ssid_sec = get("SSID");
        let (text, bytes) = decode_iw_string(ssid_sec.map(|s| s.value.as_str()).unwrap_or(""));
        let hidden = ssid_sec.is_none() || bytes.is_empty() || bytes.iter().all(|b| *b == 0);
        let clean = !hidden && !text.contains('\u{FFFD}') && !text.chars().any(|c| (c as u32) < 0x20);

        // "last seen" appears twice: once as boottime, once as "N ms ago".
        let ago = secs.iter().filter(|s| s.key == "last seen").find_map(|s| cap(re!(r"(\d+) ms ago"), &s.value, 1));

        let capability: Vec<String> =
            re!(r"(?i)\(0x[0-9a-f]+\)").replace(val("capability"), "").split_whitespace().map(String::from).collect();
        let privacy = capability.iter().any(|c| c == "Privacy");

        let rsn = body("RSN");
        let wpa = body("WPA");
        let suites = |s: &str| parse_akms(cap(re!(r"Authentication suites: (.*)"), s, 1).unwrap_or(""));
        let rsn_akm = suites(rsn);
        let wpa_akm = suites(wpa);
        let mut pairwise: Vec<String> = Vec::new();
        for s in [rsn, wpa] {
            for p in cap(re!(r"Pairwise ciphers: (.*)"), s, 1).unwrap_or("").split_whitespace() {
                if !pairwise.iter().any(|x| x == p) {
                    pairwise.push(p.into());
                }
            }
        }
        let group = cap(re!(r"Group cipher: (\S+)"), rsn, 1).or_else(|| cap(re!(r"Group cipher: (\S+)"), wpa, 1)).map(String::from);
        let mfp_required = rsn.contains("MFP-required");
        let pmf = if mfp_required {
            "required"
        } else if rsn.contains("MFP-capable") {
            "capable"
        } else {
            "none"
        };
        let (security, security_kind) = classify_security(privacy, &rsn_akm, &wpa_akm, mfp_required);

        // Standards from which capability elements are present.
        let mut standards = Vec::new();
        let ht = has(re!(r"^HT (capabilities|operation)$"));
        let vht = has(re!(r"^VHT (capabilities|operation)$"));
        let he = has(re!(r"(?i)^HE (capabilities|operation)$"));
        let eht = has(re!(r"(?i)^EHT (capabilities|operation)$"));
        if !ht && !vht && !he && !eht {
            let rates = format!("{} {}", val("Supported rates"), val("Extended supported rates"));
            let mut max_rate = 0f64;
            for m in re!(r"([\d.]+)\*?").captures_iter(&rates) {
                let v = parse_float(&m[1]).unwrap_or(f64::NAN);
                // Math.max: NaN wins
                max_rate = if v.is_nan() || max_rate.is_nan() { f64::NAN } else { max_rate.max(v) };
            }
            standards.push(if band == Some("2.4") { if max_rate > 11.0 { "g" } else { "b" } } else { "a" });
        }
        if ht {
            standards.push("n");
        }
        if vht && band != Some("2.4") {
            standards.push("ac");
        }
        if he {
            standards.push("ax");
        }
        if eht {
            standards.push("be");
        }
        let generation = if eht {
            Some("Wi-Fi 7")
        } else if he {
            Some(if band == Some("6") { "Wi-Fi 6E" } else { "Wi-Fi 6" })
        } else if vht && band != Some("2.4") {
            Some("Wi-Fi 5")
        } else if ht {
            Some("Wi-Fi 4")
        } else {
            None
        };

        // Channel width + centre. HT gives 40 MHz with a secondary above/below; VHT/HE/EHT
        // operation elements widen it from their centre segments.
        let mut width = Width::Mhz(20.0);
        let mut center_freq = freq;
        let ht_op = body("HT operation");
        if let Some(off) = cap(re!(r"secondary channel offset: (\S+)"), ht_op, 1)
            && off != "no"
            && !ht_op.contains("STA channel width: 20 MHz")
        {
            width = Width::Mhz(40.0);
            center_freq = freq + if off == "above" { 10.0 } else { -10.0 };
        }
        let vht_op = body("VHT operation");
        let vw = cap(re!(r"channel width: (\d)"), vht_op, 1);
        let seg = |r: &Regex| numo(cap(r, vht_op, 1)).filter(|x| *x != 0.0).unwrap_or(0.0);
        let s1 = seg(re!(r"center freq segment 1: (\d+)"));
        let s2 = seg(re!(r"center freq segment 2: (\d+)"));
        if band == Some("5")
            && let Some(vw) = vw
            && vw != "0"
            && s1 != 0.0
        {
            if vw == "1" && s2 != 0.0 && (s2 - s1).abs() == 8.0 {
                width = Width::Mhz(160.0);
                center_freq = channel_to_freq(s2, "5");
            } else if vw == "1" && s2 != 0.0 {
                width = Width::EightyPlusEighty;
                center_freq = channel_to_freq(s1, "5");
            } else if vw == "1" {
                width = Width::Mhz(80.0);
                center_freq = channel_to_freq(s1, "5");
            } else if vw == "2" {
                width = Width::Mhz(160.0);
                center_freq = channel_to_freq(s1, "5");
            } else if vw == "3" {
                width = Width::EightyPlusEighty;
                center_freq = channel_to_freq(s1, "5");
            }
        }
        // 6 GHz BSSes carry width in the HE operation's "6 GHz Operation Information".
        let he_op = secs.iter().find(|s| re!(r"(?i)^HE operation$").is_match(&s.key)).map(|s| s.body.as_str()).unwrap_or("");
        if band == Some("6") && !he_op.is_empty() {
            let w6 = cap(re!(r"(?i)Channel Width:\s*(\d+)"), he_op, 1);
            let c0 = numo(cap(re!(r"(?i)Center Frequency Segment 0:\s*(\d+)"), he_op, 1));
            let c1 = numo(cap(re!(r"(?i)Center Frequency Segment 1:\s*(\d+)"), he_op, 1));
            let mapped = match w6 {
                Some("0") | Some("20") => Some(20.0),
                Some("1") | Some("40") => Some(40.0),
                Some("2") | Some("80") => Some(80.0),
                Some("3") | Some("160") => Some(160.0),
                _ => None,
            };
            if let Some(w) = mapped {
                width = Width::Mhz(w);
            }
            let cc = if width == Width::Mhz(160.0) && c1.is_some_and(|c| c != 0.0) { c1 } else { c0 };
            if let Some(cc) = cc.filter(|c| *c != 0.0) {
                center_freq = channel_to_freq(cc, "6");
            }
        }
        let eht_op = secs.iter().find(|s| re!(r"(?i)^EHT operation$").is_match(&s.key)).map(|s| s.body.as_str()).unwrap_or("");
        let ew = cap(re!(r"(?i)Channel Width:\s*(\d+)"), eht_op, 1);
        let ec = re!(r"(?i)CCFS1?:\s*(\d+)|Center Frequency Segment 1:\s*(\d+)")
            .captures(eht_op)
            .and_then(|c| c.get(1).or_else(|| c.get(2)).map(|m| m.as_str()))
            .and_then(parse_float);
        if let Some(ew) = ew
            && (ew == "4" || ew == "320")
        {
            width = Width::Mhz(320.0);
            if let (Some(ec), Some(b)) = (ec.filter(|e| *e != 0.0), band) {
                center_freq = channel_to_freq(ec, b);
            }
        }

        let load = body("BSS Load");
        let util = cap(re!(r"channel utili[sz]ation: (\d+)/255"), load, 1);
        let wps_b = get("WPS").map(|s| s.body.as_str()).filter(|b| !b.is_empty());
        let bssid = head[1].to_lowercase();

        res.push(Bss {
            local_mac: mac_is_local(&bssid),
            bssid,
            iface: head[2].into(),
            associated: head.get(3).is_some_and(|m| m.as_str() == "associated"),
            ssid: if hidden { String::new() } else { text.clone() },
            ssid_hex: if hidden || clean { None } else { Some(bytes.iter().map(|b| format!("{b:02x}")).collect()) },
            hidden,
            freq,
            channel,
            band,
            width,
            center_freq,
            center_channel: freq_to_channel(center_freq),
            signal: numo(cap(re!(r"(-?[\d.]+) dBm"), val("signal"), 1)),
            last_seen_ms: ago.and_then(|a| a.parse().ok()),
            beacon_interval: numo(get("beacon interval").map(|s| s.value.as_str())),
            capability,
            security,
            security_kind,
            akm: {
                let mut v: Vec<String> = Vec::new();
                for a in rsn_akm.iter().chain(wpa_akm.iter()) {
                    if !v.contains(a) {
                        v.push(a.clone());
                    }
                }
                v
            },
            pairwise,
            group,
            pmf,
            standards,
            generation,
            stations: numo(cap(re!(r"station count: (\d+)"), load, 1)),
            utilization: util.map(|u| js_round(u.parse::<f64>().unwrap_or(f64::NAN) / 255.0 * 1000.0) / 10.0),
            country: cap(re!(r"^(\S\S)"), val("Country"), 1).map(String::from),
            wps: wps_b.map(|w| Wps {
                state: cap(re!(r"Protected Setup State: \d+ \(([^)]+)\)"), w, 1).map(String::from),
                locked: re!(r"AP setup locked: 0x0*1").is_match(w).then_some(true),
                device: cap(re!(r"Device name: (.*)"), w, 1).map(|s| s.trim().to_string()),
                manufacturer: cap(re!(r"Manufacturer: (.*)"), w, 1).map(|s| s.trim().to_string()),
                model: cap(re!(r"\* Model: (.*)"), w, 1).map(|s| s.trim().to_string()),
            }),
        });
    }
    res
}

// ---------- iw survey dump ----------

#[derive(Debug, Clone, PartialEq)]
pub struct Survey {
    pub freq: f64,
    pub in_use: bool,
    pub noise: Option<f64>,
    pub active_ms: Option<f64>,
    pub busy_ms: Option<f64>,
    pub rx_ms: Option<f64>,
    pub tx_ms: Option<f64>,
    pub busy_pct: Option<f64>,
}

impl Survey {
    pub fn json(&self) -> Value {
        O::new()
            .n("freq", self.freq)
            .k("inUse", self.in_use)
            .on("noise", self.noise)
            .on("activeMs", self.active_ms)
            .on("busyMs", self.busy_ms)
            .on("rxMs", self.rx_ms)
            .on("txMs", self.tx_ms)
            .on("busyPct", self.busy_pct)
            .v()
    }
}

pub fn parse_survey(out: &str) -> Vec<Survey> {
    let mut res = Vec::new();
    let mut parts = re!(r"(?m)^Survey data from \S+").split(out);
    parts.next();
    for block in parts {
        let Some(f) = re!(r"frequency:\s+(\d+(?:\.\d+)?) MHz( \[in use\])?").captures(block) else { continue };
        let g = |r: &Regex| numo(cap(r, block, 1));
        let mut s = Survey {
            freq: f[1].parse().unwrap_or(f64::NAN),
            in_use: f.get(2).is_some(),
            noise: g(re!(r"noise:\s+(-?\d+) dBm")),
            active_ms: g(re!(r"channel active time:\s+(\d+) ms")),
            busy_ms: g(re!(r"channel busy time:\s+(\d+) ms")),
            rx_ms: g(re!(r"channel receive time:\s+(\d+) ms")),
            tx_ms: g(re!(r"channel transmit time:\s+(\d+) ms")),
            busy_pct: None,
        };
        if let (Some(a), Some(b)) = (s.active_ms.filter(|a| *a != 0.0), s.busy_ms) {
            s.busy_pct = Some(js_round(b / a * 1000.0) / 10.0);
        }
        res.push(s);
    }
    res
}

// ---------- iw link / station dump ----------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkInfo {
    pub connected: bool,
    pub bssid: Option<String>,
    pub ssid: Option<String>,
    pub freq: Option<f64>,
    pub signal: Option<f64>,
    /// full descriptor, e.g. "200.0 MBit/s VHT-MCS 9 40MHz short GI VHT-NSS 1"
    pub rx_rate: Option<String>,
    pub tx_rate: Option<String>,
    /// Mbit/s
    pub rx_bitrate: Option<f64>,
    pub tx_bitrate: Option<f64>,
    pub tx_failed: Option<f64>,
    pub tx_retries: Option<f64>,
    pub connected_s: Option<f64>,
}

impl LinkInfo {
    pub fn json(&self) -> Value {
        O::new()
            .k("connected", self.connected)
            .o("bssid", self.bssid.clone())
            .o("ssid", self.ssid.clone())
            .on("freq", self.freq)
            .on("signal", self.signal)
            .o("rxRate", self.rx_rate.clone())
            .o("txRate", self.tx_rate.clone())
            .on("rxBitrate", self.rx_bitrate)
            .on("txBitrate", self.tx_bitrate)
            .on("txFailed", self.tx_failed)
            .on("txRetries", self.tx_retries)
            .on("connectedS", self.connected_s)
            .v()
    }
}

pub fn parse_link(link: &str, station: &str) -> LinkInfo {
    let g = |src: &str, r: &Regex| cap(r, src, 1).map(String::from);
    let mut l = LinkInfo { connected: re!(r"(?m)^Connected to").is_match(link), ..Default::default() };
    l.bssid = g(link, re!(r"(?i)Connected to ([0-9a-f:]{17})")).or_else(|| g(station, re!(r"(?i)Station ([0-9a-f:]{17})")));
    if let Some(ssid) = g(link, re!(r"(?m)^\s*SSID: (.*)$")) {
        l.ssid = Some(decode_iw_string(&ssid).0);
    }
    l.freq = numo(g(link, re!(r"freq: ([\d.]+)")).as_deref());
    l.signal = numo(g(station, re!(r"(?m)^\s*signal:\s+(-?\d+)")).or_else(|| g(link, re!(r"signal: (-?\d+) dBm"))).as_deref());
    l.rx_rate = g(station, re!(r"rx bitrate:\s+(.*)")).or_else(|| g(link, re!(r"rx bitrate: (.*)"))).map(|s| s.trim().to_string());
    l.tx_rate = g(station, re!(r"tx bitrate:\s+(.*)")).or_else(|| g(link, re!(r"tx bitrate: (.*)"))).map(|s| s.trim().to_string());
    l.rx_bitrate = numo(l.rx_rate.as_deref());
    l.tx_bitrate = numo(l.tx_rate.as_deref());
    l.tx_failed = numo(g(station, re!(r"tx failed:\s+(\d+)")).as_deref());
    l.tx_retries = numo(g(station, re!(r"tx retries:\s+(\d+)")).as_deref());
    l.connected_s = numo(g(station, re!(r"connected time:\s+(\d+)")).as_deref());
    l
}

// ---------- wpa_supplicant stdout ----------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WpaEvent {
    /// assoc-start, auth-start, assoc, handshake, connected, disconnected, wrong-key,
    /// temp-disabled, assoc-reject, auth-reject, not-found, scan, eap, config-error, error, info
    pub kind: &'static str,
    pub text: String,
    pub bssid: Option<String>,
    pub freq: Option<f64>,
    pub reason: Option<f64>,
    pub status: Option<f64>,
    pub detail: Option<String>,
}

/// IEEE 802.11 reason codes worth a word (Table 9-49).
pub fn reason(n: f64) -> Option<&'static str> {
    Some(match n as i64 {
        1 => "unspecified",
        2 => "previous authentication no longer valid",
        3 => "station leaving",
        4 => "inactivity",
        5 => "AP overloaded",
        6 => "class 2 frame from unauthenticated station",
        7 => "class 3 frame from unassociated station",
        8 => "station leaving BSS",
        9 => "not authenticated",
        14 => "MIC failure",
        15 => "4-way handshake timeout (wrong key?)",
        16 => "group key handshake timeout",
        17 => "IE mismatch in 4-way handshake",
        23 => "802.1X authentication failed",
        24 => "cipher rejected by policy",
        34 => "too many unacknowledged frames",
        _ => return None,
    })
}

/// IEEE 802.11 status codes for association/authentication rejections.
pub fn status_text(n: f64) -> Option<&'static str> {
    Some(match n as i64 {
        1 => "unspecified failure",
        10 => "capabilities not supported",
        12 => "denied for other reason",
        13 => "auth algorithm not supported",
        15 => "challenge failure",
        16 => "auth timeout",
        17 => "AP full (too many stations)",
        18 => "basic rates not supported",
        30 => "rejected temporarily (PMF)",
        31 => "robust management frame policy violation",
        37 => "request declined",
        40 => "invalid information element",
        41 => "invalid group cipher",
        42 => "invalid pairwise cipher",
        43 => "invalid AKMP",
        53 => "invalid PMKID",
        72 => "invalid RSNE capabilities",
        77 => "SAE: unsupported finite cyclic group",
        126 => "SAE: unknown password identifier",
        _ => return None,
    })
}

/// Classify one wpa_supplicant line. `secrets` are blanked out first: a malformed key line
/// makes wpa_supplicant echo the key itself in its parse error.
pub fn parse_wpa_line(raw: &str, secrets: &[String]) -> Option<WpaEvent> {
    let mut text = raw.trim().to_string();
    for s in secrets {
        if !s.is_empty() {
            text = text.replace(s.as_str(), "•••");
        }
    }
    text = re!(r"(?i)((?:psk|password|sae_password)\s*[=:]\s*)\S+").replace_all(&text, "${1}•••").into_owned();
    if text.is_empty() {
        return None;
    }
    let body = re!(r"^[\w.-]+: ").replace(&text, "").into_owned();
    let ev = |kind: &'static str| WpaEvent { kind, text: body.clone(), ..Default::default() };
    let f = |s: &str| s.parse::<f64>().ok();
    if let Some(m) = re!(r"SME: Trying to authenticate with ([0-9a-f:]{17}) \(SSID='(.*)' freq=(\d+) MHz\)").captures(&body) {
        return Some(WpaEvent { bssid: Some(m[1].into()), freq: f(&m[3]), ..ev("auth-start") });
    }
    if let Some(m) = re!(r"Trying to associate with (?:SSID '.*'|([0-9a-f:]{17}) \(SSID='(.*)' freq=(\d+) MHz\))").captures(&body) {
        return Some(WpaEvent {
            bssid: m.get(1).map(|x| x.as_str().into()),
            freq: m.get(3).and_then(|x| f(x.as_str())),
            ..ev("assoc-start")
        });
    }
    if let Some(m) = re!(r"^Associated with ([0-9a-f:]{17})").captures(&body) {
        return Some(WpaEvent { bssid: Some(m[1].into()), ..ev("assoc") });
    }
    if let Some(m) = re!(r"WPA: Key negotiation completed with ([0-9a-f:]{17}) \[(.*)\]").captures(&body) {
        return Some(WpaEvent { bssid: Some(m[1].into()), detail: Some(m[2].into()), ..ev("handshake") });
    }
    if body.contains("CTRL-EVENT-CONNECTED") {
        return Some(WpaEvent { bssid: cap(re!(r"Connection to ([0-9a-f:]{17})"), &body, 1).map(String::from), ..ev("connected") });
    }
    if re!(r"4-Way Handshake failed|pre-shared key may be incorrect").is_match(&body) {
        return Some(ev("wrong-key"));
    }
    if let Some(m) = re!(r"CTRL-EVENT-SSID-TEMP-DISABLED .*reason=(\S+)").captures(&body) {
        return Some(WpaEvent { detail: Some(m[1].into()), ..ev("temp-disabled") });
    }
    if let Some(m) = re!(r"CTRL-EVENT-DISCONNECTED bssid=([0-9a-f:]{17}) reason=(\d+)").captures(&body) {
        let r = f(&m[2]);
        return Some(WpaEvent { bssid: Some(m[1].into()), reason: r, detail: r.and_then(reason).map(String::from), ..ev("disconnected") });
    }
    if let Some(m) = re!(r"CTRL-EVENT-ASSOC-REJECT (?:bssid=)?([0-9a-f:]{17}).*status_code=(\d+)").captures(&body) {
        let s = f(&m[2]);
        return Some(WpaEvent {
            bssid: Some(m[1].into()),
            status: s,
            detail: s.and_then(status_text).map(String::from),
            ..ev("assoc-reject")
        });
    }
    if let Some(m) = re!(r"CTRL-EVENT-AUTH-REJECT ([0-9a-f:]{17}).*status_code=(\d+)").captures(&body) {
        let s = f(&m[2]);
        return Some(WpaEvent {
            bssid: Some(m[1].into()),
            status: s,
            detail: s.and_then(status_text).map(String::from),
            ..ev("auth-reject")
        });
    }
    if body.contains("CTRL-EVENT-NETWORK-NOT-FOUND") {
        return Some(ev("not-found"));
    }
    if body.contains("CTRL-EVENT-SCAN-RESULTS") {
        return Some(ev("scan"));
    }
    if re!(r"CTRL-EVENT-EAP-|EAP-").is_match(&body) {
        return Some(ev("eap"));
    }
    if re!(r"^Line \d+:|Failed to read or parse configuration|failed to parse").is_match(&body) {
        return Some(ev("config-error"));
    }
    if re!(r"(?i)Could not|Failed to initialize|nl80211: (?:Failed|Could)|ioctl.*failed").is_match(&body) && !body.contains("P2P") {
        return Some(ev("error"));
    }
    Some(ev("info"))
}

// ---------- udhcpc ----------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DhcpEvent {
    /// discover, offer, ack, nak, fail, lease, info
    pub kind: &'static str,
    pub text: String,
    pub ip: Option<String>,
    pub server: Option<String>,
    pub lease_s: Option<f64>,
    pub lease: Option<IndexMap<String, String>>,
}

/// Our udhcpc script echoes "BENCHBOB-DHCP <event>|key=value|…" with the lease details.
pub fn parse_udhcpc_line(raw: &str) -> Option<DhcpEvent> {
    let text = raw.trim().to_string();
    if text.is_empty() {
        return None;
    }
    let ev = |kind: &'static str| DhcpEvent { kind, text: text.clone(), ..Default::default() };
    if let Some(m) = re!(r"^BENCHBOB-DHCP (\w+)\|(.*)$").captures(&text) {
        let mut lease = IndexMap::new();
        lease.insert("event".to_string(), m[1].to_string());
        for kv in m[2].split('|') {
            if let Some(i) = kv.find('=')
                && i > 0
                && !kv[i + 1..].is_empty()
            {
                lease.insert(kv[..i].to_string(), kv[i + 1..].trim().to_string());
            }
        }
        let ip = lease.get("ip").cloned();
        return Some(DhcpEvent { lease: Some(lease), ip, ..ev("lease") });
    }
    if re!(r"(?:broadcasting|sending) discover").is_match(&text) {
        return Some(ev("discover"));
    }
    if let Some(m) = re!(r"(?:broadcasting|sending) select for (\S+), server (\S+)").captures(&text) {
        return Some(DhcpEvent { ip: Some(m[1].into()), server: Some(m[2].into()), ..ev("offer") });
    }
    if let Some(m) = re!(r"lease of (\S+) obtained from (\S+), lease time (\d+)").captures(&text) {
        return Some(DhcpEvent { ip: Some(m[1].into()), server: Some(m[2].into()), lease_s: m[3].parse().ok(), ..ev("ack") });
    }
    if text.contains("NAK") {
        return Some(ev("nak"));
    }
    if text.contains("no lease, failing") {
        return Some(ev("fail"));
    }
    Some(ev("info"))
}

/// udhcpc gives the mask as a dotted quad or (newer BusyBox) a prefix length.
pub fn mask_to_prefix(mask: &str) -> f64 {
    if !mask.is_empty() && mask.bytes().all(|b| b.is_ascii_digit()) {
        return mask.parse().unwrap_or(f64::NAN);
    }
    mask.split('.').map(|o| crate::json::js_number(&Value::from(o)) as i64 as u32).map(|o| o.count_ones() as f64).sum()
}

// ---------- ping / nslookup / curl / iperf3 ----------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PingResult {
    pub sent: f64,
    pub received: f64,
    pub loss_pct: f64,
    pub min: Option<f64>,
    pub avg: Option<f64>,
    pub max: Option<f64>,
    pub mdev: Option<f64>,
    pub error: Option<String>,
}

impl PingResult {
    pub fn json(&self) -> Value {
        O::new()
            .n("sent", self.sent)
            .n("received", self.received)
            .n("lossPct", self.loss_pct)
            .on("min", self.min)
            .on("avg", self.avg)
            .on("max", self.max)
            .on("mdev", self.mdev)
            .o("error", self.error.clone())
            .v()
    }
}

pub fn parse_ping(out: &str) -> PingResult {
    let s = re!(r"(\d+) packets transmitted, (\d+) (?:packets )?received.*?(\d+(?:\.\d+)?)% packet loss").captures(out);
    let r = re!(r"= ([\d.]+)/([\d.]+)/([\d.]+)/([\d.]+) ms").captures(out);
    let rv = |i: usize| r.as_ref().and_then(|r| r.get(i)).and_then(|m| parse_float(m.as_str()));
    PingResult {
        sent: s.as_ref().map(|s| s[1].parse().unwrap_or(0.0)).unwrap_or(0.0),
        received: s.as_ref().map(|s| s[2].parse().unwrap_or(0.0)).unwrap_or(0.0),
        loss_pct: s.as_ref().map(|s| s[3].parse().unwrap_or(100.0)).unwrap_or(100.0),
        min: rv(1),
        avg: rv(2),
        max: rv(3),
        mdev: rv(4),
        error: cap(re!(r"(?m)^ping: (.*)$"), out, 1).map(String::from),
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Nslookup {
    pub server: Option<String>,
    pub addresses: Vec<String>,
    pub error: Option<String>,
}

impl Nslookup {
    pub fn json(&self) -> Value {
        O::new().o("server", self.server.clone()).k("addresses", self.addresses.clone()).o("error", self.error.clone()).v()
    }
}

pub fn parse_nslookup(out: &str) -> Nslookup {
    let server = cap(re!(r"(?m)^Server:\s+(\S+)"), out, 1).map(String::from);
    // Addresses before the first "Name:" belong to the server block.
    let mut addresses: Vec<String> = Vec::new();
    if let Some(m) = re!(r"(?m)^Name:").find(out) {
        for c in re!(r"(?m)^Address(?: \d+)?:\s+(\S+)").captures_iter(&out[m.start()..]) {
            if !addresses.iter().any(|a| a == &c[1]) {
                addresses.push(c[1].into());
            }
        }
    }
    let error = cap(re!(r"\*\* (.*)"), out, 1)
        .filter(|e| !e.is_empty())
        .or_else(|| cap(re!(r"(connection timed out.*|can't resolve.*|no servers could be reached)"), out, 1))
        .map(String::from);
    let error = if addresses.is_empty() { Some(error.unwrap_or_else(|| "no answer".into())) } else { None };
    Nslookup { server, addresses, error }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HttpResult {
    pub code: f64,
    pub ok: bool,
    pub remote_ip: Option<String>,
    pub http_version: Option<String>,
    pub dns_ms: Option<f64>,
    pub connect_ms: Option<f64>,
    pub tls_ms: Option<f64>,
    pub first_byte_ms: Option<f64>,
    pub total_ms: Option<f64>,
    pub bytes: Option<Value>,
    pub speed_bps: Option<Value>,
    pub error: Option<String>,
}

impl HttpResult {
    pub fn json(&self) -> Value {
        O::new()
            .n("code", self.code)
            .k("ok", self.ok)
            .o("remoteIp", self.remote_ip.clone())
            .o("httpVersion", self.http_version.clone())
            .on("dnsMs", self.dns_ms)
            .on("connectMs", self.connect_ms)
            .on("tlsMs", self.tls_ms)
            .on("firstByteMs", self.first_byte_ms)
            .on("totalMs", self.total_ms)
            .o("bytes", self.bytes.clone())
            .o("speedBps", self.speed_bps.clone())
            .o("error", self.error.clone())
            .v()
    }
}

/// The last line of `s` (trimmed), or `dflt` when that is empty.
fn last_line(s: &str, dflt: &str) -> String {
    let l = s.trim().split('\n').next_back().unwrap_or("");
    if l.is_empty() { dflt.into() } else { l.into() }
}

/// `x || undefined` for a JSON value that should be a string.
fn str_val(v: Option<&Value>) -> Option<String> {
    v.filter(|v| crate::json::truthy(v)).map(crate::json::js_string)
}

pub fn parse_curl_json(out: &str) -> HttpResult {
    let from = out.find('{').unwrap_or(out.len().saturating_sub(1));
    let j: Value = match serde_json::from_str(out.get(from..).unwrap_or("")) {
        Ok(j) => j,
        Err(_) => return HttpResult { error: Some(last_line(out, "curl produced no output")), ..Default::default() },
    };
    let ms = |k: &str| j.get(k).and_then(Value::as_f64).filter(|s| *s > 0.0).map(|s| js_round(s * 10000.0) / 10.0);
    let code_v = [j.get("http_code"), j.get("response_code")].into_iter().flatten().find(|v| crate::json::truthy(v));
    let code = code_v.map(crate::json::js_number).unwrap_or(0.0);
    let errmsg = str_val(j.get("errormsg"));
    HttpResult {
        code,
        ok: errmsg.is_none() && code > 0.0 && code < 400.0,
        remote_ip: str_val(j.get("remote_ip")),
        http_version: str_val(j.get("http_version")),
        dns_ms: ms("time_namelookup"),
        connect_ms: ms("time_connect"),
        tls_ms: ms("time_appconnect"),
        first_byte_ms: ms("time_starttransfer"),
        total_ms: ms("time_total"),
        bytes: j.get("size_download").map(crate::json::js_value),
        speed_bps: j.get("speed_download").map(crate::json::js_value),
        error: errmsg.or_else(|| (code >= 400.0).then(|| format!("HTTP {}", crate::json::fmt_num(code)))),
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct IperfResult {
    pub sent_bps: Option<Value>,
    pub received_bps: Option<Value>,
    pub retransmits: Option<Value>,
    pub error: Option<String>,
}

impl IperfResult {
    pub fn json(&self) -> Value {
        O::new()
            .o("sentBps", self.sent_bps.clone())
            .o("receivedBps", self.received_bps.clone())
            .o("retransmits", self.retransmits.clone())
            .o("error", self.error.clone())
            .v()
    }
}

pub fn parse_iperf(out: &str) -> IperfResult {
    let Ok(j) = serde_json::from_str::<Value>(out) else {
        return IperfResult { error: Some(last_line(out, "iperf3 produced no output")), ..Default::default() };
    };
    if let Some(e) = j.get("error").filter(|e| crate::json::truthy(e)) {
        return IperfResult { error: Some(crate::json::js_string(e)), ..Default::default() };
    }
    let e = j.get("end");
    let g = |a: &str, b: &str| e.and_then(|e| e.get(a)).and_then(|x| x.get(b)).map(crate::json::js_value);
    IperfResult {
        sent_bps: g("sum_sent", "bits_per_second"),
        received_bps: g("sum_received", "bits_per_second"),
        retransmits: g("sum_sent", "retransmits"),
        error: None,
    }
}

// ---------- OUI vendor lookup ----------

/// Parse whichever OUI list is installed: IEEE's oui.txt ("AA-BB-CC   (hex)\t\tVendor"),
/// nmap's ("AABBCC Vendor") or Wireshark's manuf ("AA:BB:CC\tShort\tLong").
///
/// The patterns use `[^\r\n]` where the TypeScript has `.`: JavaScript's dot doesn't match
/// a carriage return either, so a CRLF line matches (or doesn't) exactly as there.
pub fn parse_oui(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.split('\n') {
        if line.contains("(base 16)") {
            continue; // oui.txt repeats each entry in this form
        }
        if let Some(m) = re!(r"(?i)^([0-9A-F]{2})-([0-9A-F]{2})-([0-9A-F]{2})\s+\(hex\)\s+([^\r\n  ]+)$").captures(line) {
            map.insert(format!("{}{}{}", &m[1], &m[2], &m[3]).to_lowercase(), m[4].trim().to_string());
            continue;
        }
        if let Some(m) = re!(r"(?i)^([0-9A-F]{2}):([0-9A-F]{2}):([0-9A-F]{2})\s+(\S+)(?:\s+([^\r\n  ]+))?$").captures(line) {
            let v = m.get(5).map(|x| x.as_str()).filter(|x| !x.is_empty()).unwrap_or(&m[4]);
            map.insert(format!("{}{}{}", &m[1], &m[2], &m[3]).to_lowercase(), v.trim().to_string());
            continue;
        }
        if let Some(m) = re!(r"(?i)^([0-9A-F]{6})\s+([^\r\n  ]+)$").captures(line) {
            map.insert(m[1].to_lowercase(), m[2].trim().to_string());
        }
    }
    map
}

// ---------- Scan scheduling + history ----------

/// Keep samples newer than max_age_ms, and at most `max` of them (newest win).
pub fn trim_history(h: &[(f64, f64)], now: f64, max_age_ms: f64, max: usize) -> Vec<(f64, f64)> {
    let mut i = 0;
    while i < h.len() && now - h[i].0 > max_age_ms {
        i += 1;
    }
    h[i.max(h.len().saturating_sub(max))..].to_vec()
}

pub struct ScanDue {
    pub now: f64,
    pub last_scan: f64,
    pub busy: bool,
    pub auto_enabled: bool,
    pub auto_interval_s: f64,
    pub watching: bool,
    pub watch_interval_s: f64,
}

/// Is a scan due? Background auto-scan and "someone has the panel open" both ask for scans;
/// the shorter applicable interval wins, measured from the last scan of either kind, so two
/// viewers (or a viewer plus auto-scan) never double the rate.
pub fn scan_due(o: &ScanDue) -> bool {
    if o.busy {
        return false;
    }
    let mut ivs = Vec::new();
    // Math.max(15, NaN) is NaN, and a NaN interval never comes due.
    let floor15 = |x: f64| if x.is_nan() { f64::NAN } else { x.max(15.0) };
    if o.auto_enabled {
        ivs.push(floor15(o.auto_interval_s));
    }
    if o.watching {
        ivs.push(floor15(o.watch_interval_s));
    }
    if ivs.is_empty() {
        return false;
    }
    let min = ivs.iter().copied().fold(f64::INFINITY, |a, b| if a.is_nan() || b.is_nan() { f64::NAN } else { a.min(b) });
    o.now - o.last_scan >= min * 1000.0
}

#[cfg(test)]
pub(crate) mod tests;
