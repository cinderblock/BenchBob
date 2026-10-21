//! The USB panel's pure logic (peripherals/lib/usb.ts): a sysfs reader and functions over
//! plain data, so the tree builder, uhubctl parser, kernel-log filter and power planner are
//! tested with fixtures on any machine. The driver owns processes and state.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use benchbob_core::http::{num, parse_int};
use benchbob_core::udev::Props;
use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize, Serializer};

fn ser_num<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    num(*v).serialize(s)
}

/// `parseInt(s, 16) || 0`
fn hex(s: &str) -> u32 {
    let t = s.trim_start();
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    let d: String = t.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    u32::from_str_radix(&d, 16).unwrap_or(0)
}

/// `parseInt(s) || 0`
fn int(s: &str) -> i64 {
    parse_int(s).unwrap_or(0)
}

/// `Number(s)` for a sysfs string (NaN → 0, as `Number(x) || 0`).
fn number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    t.parse::<f64>().ok().filter(|f| f.is_finite()).unwrap_or(0.0)
}

/// A JavaScript number as `String(n)` writes it, for the few labels that print one.
pub fn js_num_str(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e21 { format!("{}", f as i64) } else { format!("{f}") }
}

// ---------- sysfs reader ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RawPort {
    pub port: u32,
    /// hotplug | hardwired | not used | unknown
    pub connect_type: String,
    pub over_current: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RawInterface {
    /// "1-1.2:1.0"
    pub name: String,
    pub cls: u32,
    pub sub: u32,
    pub proto: u32,
    pub num: u32,
    pub alt: u32,
    pub driver: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RawController {
    pub devpath: String,
    pub name: String,
    pub driver: String,
    pub pci_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RawUsbDevice {
    /// sysfs name: "usb1", "1-1", "1-1.2"
    pub name: String,
    /// udev DEVPATH, "/devices/…/usb1/1-1"
    pub devpath: String,
    pub attrs: HashMap<String, String>,
    /// "usb" normally, "usbip-host" while shared
    pub driver: String,
    pub interfaces: Vec<RawInterface>,
    /// Hub ports (from <hub>:1.0/<name>-portN)
    pub ports: Vec<RawPort>,
    /// 1 available, 2 used, 3 error (only while bound to usbip-host)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usbip_status: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<RawController>,
}

impl RawUsbDevice {
    pub fn attr(&self, k: &str) -> &str {
        self.attrs.get(k).map(String::as_str).unwrap_or("")
    }
}

const DEV_ATTRS: [&str; 18] = [
    "idVendor",
    "idProduct",
    "speed",
    "version",
    "bMaxPower",
    "bConfigurationValue",
    "maxchild",
    "busnum",
    "devnum",
    "devpath",
    "manufacturer",
    "product",
    "serial",
    "bDeviceClass",
    "bDeviceSubClass",
    "bDeviceProtocol",
    "removable",
    "bNumInterfaces",
];

fn rd(p: &Path) -> String {
    benchbob_core::run::read_text(p)
}

fn link_name(p: &Path) -> String {
    std::fs::read_link(p)
        .ok()
        .map(|l| {
            let s = l.to_string_lossy().into_owned();
            s[s.rfind('/').map(|i| i + 1).unwrap_or(0)..].to_string()
        })
        .unwrap_or_default()
}

/// Snapshot `<sys>/bus/usb/devices`: every device, its interfaces with bound drivers, hub
/// ports and (for root hubs) the host controller.
pub fn read_usb_sysfs(sys: &Path) -> Vec<RawUsbDevice> {
    let base = sys.join("bus/usb/devices");
    let Ok(rd_dir) = std::fs::read_dir(&base) else { return Vec::new() };
    let mut names: Vec<String> = rd_dir.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    let sys_s = sys.to_string_lossy().trim_end_matches('/').to_string();
    let mut out = Vec::new();
    for name in names.iter().filter(|n| !n.contains(':')) {
        // Unplugged between readdir and here: skip it.
        let Ok(real) = std::fs::canonicalize(base.join(name)) else { continue };
        let real_s = real.to_string_lossy().into_owned();
        let attrs: HashMap<String, String> = DEV_ATTRS.iter().map(|a| (a.to_string(), rd(&real.join(a)))).collect();
        let driver = link_name(&real.join("driver"));
        let mut interfaces = Vec::new();
        let mut ports = Vec::new();
        // Root hub usbN's interfaces are named "N-0:1.0"; everyone else's "<name>:1.0".
        let if_prefix = if let Some(n) = name.strip_prefix("usb") { format!("{n}-0:") } else { format!("{name}:") };
        for ifn in names.iter().filter(|n| n.starts_with(&if_prefix)) {
            let ip = base.join(ifn);
            let a = |k: &str| rd(&ip.join(k));
            interfaces.push(RawInterface {
                name: ifn.clone(),
                cls: hex(&a("bInterfaceClass")),
                sub: hex(&a("bInterfaceSubClass")),
                proto: hex(&a("bInterfaceProtocol")),
                num: hex(&a("bInterfaceNumber")),
                alt: int(&a("bAlternateSetting")).max(0) as u32,
                driver: link_name(&ip.join("driver")),
            });
            // Hub ports appear as <name>-portN under the hub interface (usb1-port1, 1-1-port3).
            let Ok(entries) = std::fs::read_dir(&ip) else { continue };
            let pre = format!("{name}-port");
            for e in entries.filter_map(|e| e.ok()) {
                let e = e.file_name().to_string_lossy().into_owned();
                let Some(n) = e.strip_prefix(&pre) else { continue };
                if n.is_empty() || !n.bytes().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                ports.push(RawPort {
                    port: n.parse().unwrap_or(0),
                    connect_type: rd(&ip.join(&e).join("connect_type")),
                    over_current: int(&rd(&ip.join(&e).join("over_current_count"))),
                });
            }
        }
        interfaces.sort_by(|a, b| a.num.cmp(&b.num).then(a.alt.cmp(&b.alt)));
        ports.sort_by_key(|p| p.port);
        let devpath = real_s.strip_prefix(&sys_s).unwrap_or(&real_s).to_string();
        let mut dev = RawUsbDevice { name: name.clone(), devpath, attrs, driver, interfaces, ports, usbip_status: None, controller: None };
        if dev.driver == "usbip-host" {
            dev.usbip_status = Some(int(&rd(&real.join("usbip_status"))));
        }
        if name.starts_with("usb")
            && let Some(parent) = real.parent()
        {
            let v = rd(&parent.join("vendor"));
            let d = rd(&parent.join("device"));
            let ps = parent.to_string_lossy().into_owned();
            dev.controller = Some(RawController {
                devpath: ps.strip_prefix(&sys_s).unwrap_or(&ps).to_string(),
                name: parent.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default(),
                driver: link_name(&parent.join("driver")),
                pci_id: if !v.is_empty() && !d.is_empty() {
                    format!("{}:{}", v.strip_prefix("0x").unwrap_or(&v), d.strip_prefix("0x").unwrap_or(&d))
                } else {
                    String::new()
                },
            });
        }
        out.push(dev);
    }
    out
}

// ---------- Names ----------

fn base_class(cls: u32) -> Option<&'static str> {
    Some(match cls {
        0x00 => "Per-interface",
        0x01 => "Audio",
        0x02 => "Communications (CDC)",
        0x03 => "HID",
        0x05 => "Physical",
        0x06 => "Imaging",
        0x07 => "Printer",
        0x08 => "Mass storage",
        0x09 => "Hub",
        0x0a => "CDC data",
        0x0b => "Smart card",
        0x0d => "Content security",
        0x0e => "Video",
        0x0f => "Personal healthcare",
        0x10 => "Audio/video",
        0x11 => "Billboard",
        0x12 => "USB-C bridge",
        0x13 => "Bulk display",
        0x14 => "MCTP",
        0x3c => "I3C",
        0xdc => "Diagnostic",
        0xe0 => "Wireless",
        0xef => "Miscellaneous",
        0xfe => "Application-specific",
        0xff => "Vendor-specific",
        _ => return None,
    })
}

/// Human name for an interface (or device) class, with the subclasses people actually meet.
pub fn class_name(cls: u32, sub: u32, proto: u32) -> String {
    let s = match (cls, sub, proto) {
        (0x02, 0x02, _) => "CDC ACM (serial)",
        (0x02, 0x06, _) => "CDC Ethernet (ECM)",
        (0x02, 0x0d, _) => "CDC Ethernet (NCM)",
        (0x02, 0x0e, _) => "CDC MBIM",
        (0x01, 0x01, _) => "Audio control",
        (0x01, 0x02, _) => "Audio streaming",
        (0x01, 0x03, _) => "MIDI",
        (0x03, 0x01, 0x01) => "HID keyboard",
        (0x03, 0x01, 0x02) => "HID mouse",
        (0x08, _, 0x62) => "Mass storage (UAS)",
        (0x0e, 0x01, _) => "Video control",
        (0x0e, 0x02, _) => "Video streaming",
        (0xe0, 0x01, 0x01) => "Bluetooth",
        (0xe0, 0x01, 0x03) => "RNDIS",
        (0xef, 0x04, 0x01) => "RNDIS",
        (0xfe, 0x01, _) => "DFU",
        (0xfe, 0x03, _) => "USBTMC (instrument)",
        _ => return base_class(cls).map(str::to_string).unwrap_or_else(|| format!("Class 0x{cls:02x}")),
    };
    s.to_string()
}

/// sysfs `speed` (Mb/s) → label.
pub fn speed_label(speed: &str) -> String {
    let s = number(speed);
    if s == 0.0 {
        return String::new();
    }
    match s {
        1.5 => "1.5 Mb/s Low".into(),
        12.0 => "12 Mb/s Full".into(),
        480.0 => "480 Mb/s High".into(),
        5000.0 => "5 Gb/s SuperSpeed".into(),
        10000.0 => "10 Gb/s SuperSpeed+".into(),
        20000.0 => "20 Gb/s SuperSpeed+ 2x2".into(),
        _ if s >= 1000.0 => format!("{} Gb/s", js_num_str(s / 1000.0)),
        _ => format!("{} Mb/s", js_num_str(s)),
    }
}

// ---------- Device nodes + panel hints ----------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UsbNode {
    /// "/dev/ttyACM0", "can0", "/dev/sda", "card1"
    pub dev: String,
    /// serial | can | net | wifi | disk | camera | video | hidraw | i2c | sound | input | gpio | bluetooth | misc
    pub kind: String,
    /// Driver id (or "drives") that handles it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panel: Option<String>,
    /// Extra detail ("2 partitions").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn node(dev: &str, kind: &str, panel: Option<&str>) -> UsbNode {
    UsbNode { dev: dev.into(), kind: kind.into(), panel: panel.map(Into::into), label: None }
}

static CAN_DRIVERS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(gs_usb|peak_usb|kvaser_usb|usb_8dev|ems_usb|esd_usb|mcba_usb|ucan|etas_es58x|f81604|nct6694_canfd)$").unwrap()
});
static CAN_IF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^v?can\d").unwrap());
static CARD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/card(\d+)$").unwrap());
static EVENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/event\d+$").unwrap());

/// Classify one udev entry that lives under a USB device into a user-visible node.
pub fn node_for(p: &Props) -> Option<UsbNode> {
    let g = |k: &str| p.get(k).map(String::as_str).unwrap_or("");
    let s = g("SUBSYSTEM");
    let dn = g("DEVNAME");
    match s {
        "tty" if !dn.is_empty() => Some(node(dn, "serial", Some("serial"))),
        "net" if !g("INTERFACE").is_empty() => {
            let i = g("INTERFACE");
            if CAN_IF.is_match(i) || CAN_DRIVERS.is_match(g("ID_NET_DRIVER")) {
                Some(node(i, "can", Some("can")))
            } else if g("DEVTYPE") == "wlan" {
                Some(node(i, "wifi", Some("wifi")))
            } else {
                Some(node(i, "net", Some("netdiag")))
            }
        }
        "block" if g("DEVTYPE") == "disk" && !dn.is_empty() => Some(node(dn, "disk", Some("drives"))),
        "video4linux" if !dn.is_empty() => {
            if g("ID_V4L_CAPABILITIES").contains(":capture:") {
                Some(node(dn, "camera", Some("camera")))
            } else {
                Some(node(dn, "video", None))
            }
        }
        "hidraw" if !dn.is_empty() => Some(node(dn, "hidraw", None)),
        "i2c-dev" if !dn.is_empty() => Some(node(dn, "i2c", Some("i2c"))),
        "sound" => {
            let c = CARD.captures(g("DEVPATH"))?;
            let mut n = node(&format!("card{}", &c[1]), "sound", None);
            n.label = Some(g("ID_MODEL_FROM_DATABASE").to_string()).filter(|s| !s.is_empty());
            Some(n)
        }
        "input" if EVENT.is_match(dn) => Some(node(dn, "input", None)),
        "gpio" if dn.contains("gpiochip") => Some(node(dn, "gpio", Some("gpio"))),
        "bluetooth" if g("DEVTYPE") == "host" => {
            let dp = g("DEVPATH");
            Some(node(&dp[dp.rfind('/').map(|i| i + 1).unwrap_or(0)..], "bluetooth", Some("ble")))
        }
        "usbmisc" if !dn.is_empty() => Some(node(dn, "misc", None)),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Hint {
    pub panel: String,
    pub label: String,
}

static ID_HINTS: LazyLock<Vec<(Regex, &'static str, &'static str)>> = LazyLock::new(|| {
    [
        (r"^0bda:(2832|2838)$", "sdr", "RTL-SDR"),
        (r"^1546:01a[5-9]$", "gps", "u-blox GPS"),
        (r"^067b:23a3$", "gps", "GPS (PL2303)"),
        (r"^0483:37(48|49|4b|4e|4f|52|53|54|55|57)$", "probes", "ST-LINK"),
        (r"^1366:", "probes", "SEGGER J-Link"),
        (r"^0d28:0204$", "probes", "DAPLink"),
        (r"^2e8a:000c$", "probes", "Raspberry Pi Debug Probe"),
        (r"^1a86:8010$", "probes", "WCH-Link"),
        (r"^0403:6010$", "probes", "FTDI FT2232 (JTAG capable)"),
        (r"^303a:1001$", "probes", "ESP32 USB-JTAG"),
        (r"^(051d|0764|0463|09ae|06da):", "ups", "UPS"),
    ]
    .into_iter()
    .map(|(r, p, l)| (Regex::new(r).unwrap(), p, l))
    .collect()
});

/// Device-level panel hints for things identified by VID:PID rather than by a node.
pub fn id_hint(vidpid: &str, product: &str) -> Option<Hint> {
    for (re, panel, label) in ID_HINTS.iter() {
        if re.is_match(vidpid) {
            return Some(Hint { panel: (*panel).into(), label: (*label).into() });
        }
    }
    if product.to_lowercase().contains("cmsis-dap") {
        return Some(Hint { panel: "probes".into(), label: "CMSIS-DAP".into() });
    }
    None
}

// ---------- Tree ----------

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsbInterface {
    /// "1.0" (config.interface)
    pub id: String,
    pub num: u32,
    pub cls: u32,
    pub sub: u32,
    pub proto: u32,
    pub cls_name: String,
    pub driver: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsbPort {
    pub port: u32,
    /// busid of what's plugged in
    pub child: Option<String>,
    pub connect_type: String,
    pub over_current: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsbDevice {
    /// "usb1", "1-1", "1-1.2" (also the physical port path)
    pub busid: String,
    pub devpath: String,
    pub bus: i64,
    pub devnum: i64,
    pub parent: Option<String>,
    /// Port number on the parent hub.
    pub port: Option<u32>,
    pub depth: u32,
    pub root: bool,
    pub hub: bool,
    pub nports: u32,
    pub ports: Vec<UsbPort>,
    pub vid: String,
    pub pid: String,
    pub vidpid: String,
    pub vendor: String,
    pub product: String,
    pub serial: String,
    #[serde(serialize_with = "ser_num")]
    pub speed: f64,
    pub speed_label: String,
    pub version: String,
    pub max_power_ma: i64,
    pub config: i64,
    pub cls: String,
    pub driver: String,
    pub interfaces: Vec<UsbInterface>,
    pub nodes: Vec<UsbNode>,
    pub hint: Option<Hint>,
    pub id_path: String,
    /// null | "available" | "in use" | "error"
    pub usbip: Option<&'static str>,
    /// Controller id (root hubs only).
    pub controller: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsbController {
    /// "0000:01:00.0"
    pub id: String,
    pub devpath: String,
    pub name: String,
    pub driver: String,
    pub pci_id: String,
    pub buses: Vec<i64>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Inventory {
    pub controllers: Vec<UsbController>,
    /// Depth-first tree order.
    pub devices: Vec<UsbDevice>,
}

impl Inventory {
    pub fn get(&self, busid: &str) -> Option<&UsbDevice> {
        self.devices.iter().find(|d| d.busid == busid)
    }
}

/// "usb2" → [2]; "2-1.4.3" → [2, 1, 4, 3]
fn port_key(busid: &str) -> Vec<i64> {
    if let Some(n) = busid.strip_prefix("usb") {
        return vec![n.parse().unwrap_or(0)];
    }
    let (b, rest) = busid.split_once('-').unwrap_or((busid, ""));
    std::iter::once(b.parse().unwrap_or(0)).chain(rest.split('.').map(|x| x.parse().unwrap_or(0))).collect()
}

/// The hub a device hangs off and the port on it; None for root hubs.
pub fn parent_of(busid: &str) -> Option<(String, u32)> {
    if busid.starts_with("usb") {
        return None;
    }
    let (b, rest) = busid.split_once('-')?;
    Some(match rest.rfind('.') {
        None => (format!("usb{b}"), rest.parse().unwrap_or(0)),
        Some(i) => (format!("{b}-{}", &rest[..i]), rest[i + 1..].parse().unwrap_or(0)),
    })
}

/// `a.localeCompare(b, undefined, { numeric: true })`, near enough for device names: digit
/// runs compare by value, the rest case-insensitively.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let mut n = String::new();
                while let Some(c) = x.peek().copied().filter(char::is_ascii_digit) {
                    n.push(c);
                    x.next();
                }
                let mut m = String::new();
                while let Some(d) = y.peek().copied().filter(char::is_ascii_digit) {
                    m.push(d);
                    y.next();
                }
                let (n, m) = (n.trim_start_matches('0'), m.trim_start_matches('0'));
                let o = n.len().cmp(&m.len()).then_with(|| n.cmp(m));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(c), Some(d)) => {
                let o = c.to_lowercase().cmp(d.to_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                x.next();
                y.next();
            }
        }
    }
}

const NODE_ORDER: [&str; 14] =
    ["serial", "can", "net", "wifi", "disk", "camera", "video", "sound", "i2c", "gpio", "bluetooth", "hidraw", "input", "misc"];

fn kind_rank(k: &str) -> i64 {
    NODE_ORDER.iter().position(|x| *x == k).map(|i| i as i64).unwrap_or(-1)
}

/// Udev nodes that belong to this device itself (its interfaces), not to a child device.
fn nodes_of(devpath: &str, udev: &[Props]) -> Vec<UsbNode> {
    let pre = format!("{devpath}/");
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for p in udev {
        let Some(dp) = p.get("DEVPATH") else { continue };
        let Some(rest) = dp.strip_prefix(&pre) else { continue };
        // A child device's subtree.
        if !rest.split('/').next().unwrap_or("").contains(':') {
            continue;
        }
        let Some(mut n) = node_for(p) else { continue };
        if !seen.insert(format!("{}{}", n.kind, n.dev)) {
            continue;
        }
        if n.kind == "disk" {
            let pp = format!("{dp}/");
            let parts = udev
                .iter()
                .filter(|q| {
                    q.get("SUBSYSTEM").map(String::as_str) == Some("block")
                        && q.get("DEVTYPE").map(String::as_str) == Some("partition")
                        && q.get("DEVPATH").is_some_and(|d| d.starts_with(&pp))
                })
                .count();
            if parts > 0 {
                n.label = Some(format!("{parts} partition{}", if parts > 1 { "s" } else { "" }));
            }
        }
        out.push(n);
    }
    out.sort_by(|a, b| kind_rank(&a.kind).cmp(&kind_rank(&b.kind)).then_with(|| natural_cmp(&a.dev, &b.dev)));
    out
}

fn ctrl_name(pci_id: &str) -> Option<&'static str> {
    (pci_id == "1106:3483").then_some("VIA VL805/806 xHCI (USB 3.0)")
}

pub fn build_inventory(raw: &[RawUsbDevice], udev: &[Props]) -> Inventory {
    let by_path: HashMap<&str, &Props> = udev.iter().filter_map(|p| p.get("DEVPATH").map(|d| (d.as_str(), p))).collect();
    let empty = Props::new();
    let mut devices: Vec<UsbDevice> = raw
        .iter()
        .map(|r| {
            let a = |k: &str| r.attr(k);
            let u = by_path.get(r.devpath.as_str()).copied().unwrap_or(&empty);
            let ug = |k: &str| u.get(k).map(String::as_str).unwrap_or("");
            let vid = a("idVendor").to_lowercase();
            let pid = a("idProduct").to_lowercase();
            let root = r.name.starts_with("usb");
            let hub = a("bDeviceClass") == "09" || r.interfaces.iter().any(|i| i.cls == 9);
            let par = parent_of(&r.name);
            let db_vendor = ug("ID_VENDOR_FROM_DATABASE");
            let db_model = ug("ID_MODEL_FROM_DATABASE");
            let product = if !db_model.is_empty() { db_model.to_string() } else { a("product").to_string() };
            let nports = int(a("maxchild")).max(0) as u32;
            let usbip = (r.driver == "usbip-host").then_some(match r.usbip_status {
                Some(2) => "in use",
                Some(3) => "error",
                _ => "available",
            });
            let bus = match parse_int(a("busnum")).filter(|n| *n != 0) {
                Some(n) => n,
                None => r.name.strip_prefix("usb").unwrap_or_else(|| r.name.split('-').next().unwrap_or("")).parse().unwrap_or(0),
            };
            let vidpid = format!("{vid}:{pid}");
            UsbDevice {
                busid: r.name.clone(),
                devpath: r.devpath.clone(),
                bus,
                devnum: int(a("devnum")),
                parent: par.as_ref().map(|p| p.0.clone()),
                port: par.as_ref().map(|p| p.1),
                depth: 0,
                root,
                hub,
                nports,
                ports: Vec::new(),
                vendor: if !db_vendor.is_empty() { db_vendor.to_string() } else { a("manufacturer").to_string() },
                serial: if root { String::new() } else { a("serial").to_string() },
                speed: number(a("speed")),
                speed_label: speed_label(a("speed")),
                version: a("version").trim().to_string(),
                max_power_ma: int(a("bMaxPower")),
                config: int(a("bConfigurationValue")),
                cls: class_name(hex(a("bDeviceClass")), hex(a("bDeviceSubClass")), hex(a("bDeviceProtocol"))),
                driver: r.driver.clone(),
                interfaces: r
                    .interfaces
                    .iter()
                    .map(|i| UsbInterface {
                        id: i.name[i.name.find(':').map(|x| x + 1).unwrap_or(0)..].to_string(),
                        num: i.num,
                        cls: i.cls,
                        sub: i.sub,
                        proto: i.proto,
                        cls_name: class_name(i.cls, i.sub, i.proto),
                        driver: Some(i.driver.clone()).filter(|d| !d.is_empty()),
                    })
                    .collect(),
                nodes: nodes_of(&r.devpath, udev),
                hint: if root || hub { None } else { id_hint(&vidpid, &product) },
                id_path: ug("ID_PATH").to_string(),
                usbip,
                controller: r.controller.as_ref().map(|c| c.name.clone()),
                product,
                vid,
                pid,
                vidpid,
            }
        })
        .collect();

    for d in devices.iter_mut() {
        let r = raw.iter().find(|x| x.name == d.busid).unwrap();
        let mut ports: BTreeMap<u32, UsbPort> = BTreeMap::new();
        for p in &r.ports {
            ports.insert(p.port, UsbPort { port: p.port, child: None, connect_type: p.connect_type.clone(), over_current: p.over_current });
        }
        for i in 1..=d.nports {
            ports.entry(i).or_insert(UsbPort { port: i, child: None, connect_type: String::new(), over_current: 0 });
        }
        d.ports = ports.into_values().collect();
    }
    let links: Vec<(String, u32, String)> = devices.iter().filter_map(|d| Some((d.parent.clone()?, d.port?, d.busid.clone()))).collect();
    for (parent, port, child) in links {
        if let Some(p) = devices.iter_mut().find(|x| x.busid == parent)
            && let Some(slot) = p.ports.iter_mut().find(|x| x.port == port)
        {
            slot.child = Some(child);
        }
    }
    let parents: HashMap<String, Option<String>> = devices.iter().map(|d| (d.busid.clone(), d.parent.clone())).collect();
    for d in devices.iter_mut() {
        let mut depth = 0;
        let mut p = d.parent.clone();
        while let Some(b) = p {
            let Some(next) = parents.get(&b) else { break };
            depth += 1;
            p = next.clone();
        }
        d.depth = depth;
    }
    // Depth-first order: sorting by the numeric port path is exactly that.
    devices.sort_by_key(|a| port_key(&a.busid));

    let mut ctrls: IndexMap<String, UsbController> = IndexMap::new();
    for r in raw {
        let Some(c) = &r.controller else { continue };
        let e = ctrls.entry(c.devpath.clone()).or_insert_with(|| {
            let u = by_path.get(c.devpath.as_str()).copied().unwrap_or(&empty);
            let db: Vec<&str> = ["ID_VENDOR_FROM_DATABASE", "ID_MODEL_FROM_DATABASE"]
                .iter()
                .filter_map(|k| u.get(*k).map(String::as_str))
                .filter(|s| !s.is_empty())
                .collect();
            let db = db.join(" ");
            let name = match ctrl_name(&c.pci_id) {
                Some(n) => n.to_string(),
                None if !db.is_empty() => db,
                None if c.driver == "dwc2" => "DWC2 OTG controller".into(),
                None if c.driver == "vhci_hcd" => "USB/IP virtual host controller".into(),
                None => c.name.clone(),
            };
            UsbController {
                id: c.name.clone(),
                devpath: c.devpath.clone(),
                name,
                driver: c.driver.clone(),
                pci_id: c.pci_id.clone(),
                buses: Vec::new(),
            }
        });
        let bus = parse_int(r.attr("busnum")).filter(|n| *n != 0).unwrap_or_else(|| r.name[3..].parse().unwrap_or(0));
        e.buses.push(bus);
    }
    let mut controllers: Vec<UsbController> = ctrls.into_values().collect();
    for c in controllers.iter_mut() {
        c.buses.sort();
    }
    controllers.sort_by_key(|c| c.buses.first().copied().unwrap_or(0));
    Inventory { controllers, devices }
}

/// All busids in the subtree rooted at `busid` (inclusive).
pub fn subtree(devices: &[UsbDevice], busid: &str) -> Vec<String> {
    let mut out = vec![busid.to_string()];
    let mut i = 0;
    while i < out.len() {
        for d in devices {
            if d.parent.as_deref() == Some(out[i].as_str()) {
                out.push(d.busid.clone());
            }
        }
        i += 1;
    }
    out
}

// ---------- uhubctl ----------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Attached {
    pub vidpid: String,
    pub desc: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UhubPort {
    pub port: u32,
    /// Raw hex word.
    pub status: String,
    pub power: bool,
    pub connect: bool,
    pub enable: bool,
    pub suspend: bool,
    pub over_current: bool,
    /// lowspeed | highspeed | 5gbps | … ("" = full speed / unknown)
    pub speed: String,
    /// USB3 link state: U0, Rx.Detect, SS.Disabled…
    pub link: String,
    pub flags: Vec<String>,
    pub attached: Option<Attached>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UhubHub {
    /// "1-1", "2" (root hub = bus number)
    pub loc: String,
    pub vidpid: String,
    pub desc: String,
    pub usb: String,
    pub nports: u32,
    /// ppps | ganged | nops
    pub mode: String,
    pub ports: Vec<UhubPort>,
}

static UH_HUB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:Current|New) status for hub (\S+) \[([0-9a-f]{4}:[0-9a-f]{4})\s*(.*?),\s*USB ([\d.]+),\s*(\d+) ports?,\s*(\w+)\]\s*$",
    )
    .unwrap()
});
static UH_PORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s+Port (\d+):\s+([0-9a-f]{4})\s*(.*?)\s*$").unwrap());
static UH_ATT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^([0-9a-f]{4}:[0-9a-f]{4})\s*(.*)$").unwrap());
static UH_SPEED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(lowspeed|highspeed|fullspeed|\d+gbps)$").unwrap());
static UH_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(U0|U1|U2|U3|SS\.Disabled|Rx\.Detect|SS\.Inactive|Polling|Recovery|HotReset|Compliance|Loopback|Resume|SS\.Disable)$")
        .unwrap()
});

/// Parse `uhubctl` output (listing, or the Current/New status blocks around an action).
/// Later blocks for the same hub win.
pub fn parse_uhubctl(out: &str) -> Vec<UhubHub> {
    let mut hubs: IndexMap<String, UhubHub> = IndexMap::new();
    let mut cur: Option<String> = None;
    for line in out.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(h) = UH_HUB.captures(line) {
            let hub = UhubHub {
                loc: h[1].to_string(),
                vidpid: h[2].to_lowercase(),
                desc: h[3].trim().to_string(),
                usb: h[4].to_string(),
                nports: h[5].parse().unwrap_or(0),
                mode: h[6].to_lowercase(),
                ports: Vec::new(),
            };
            cur = Some(hub.loc.clone());
            hubs.insert(hub.loc.clone(), hub);
            continue;
        }
        let p = UH_PORT.captures(line);
        if let (Some(p), Some(loc)) = (&p, &cur) {
            let mut rest = p[3].to_string();
            let mut attached = None;
            if let Some(b) = rest.find('[') {
                let inner = rest[b + 1..].trim_end();
                let inner = inner.strip_suffix(']').unwrap_or(inner).to_string();
                rest = rest[..b].trim().to_string();
                attached = Some(match UH_ATT.captures(&inner) {
                    Some(m) => {
                        let d = m[2].to_string();
                        let d = d.strip_prefix(',').map(|s| s.trim_start()).unwrap_or(&d);
                        Attached { vidpid: m[1].to_lowercase(), desc: d.trim().to_string() }
                    }
                    None => Attached { vidpid: String::new(), desc: inner.trim().to_string() },
                });
            }
            let flags: Vec<String> = rest.split_whitespace().map(str::to_string).collect();
            let has = |f: &str| flags.iter().any(|x| x == f);
            let port = UhubPort {
                port: p[1].parse().unwrap_or(0),
                status: p[2].to_lowercase(),
                power: has("power"),
                connect: has("connect"),
                enable: has("enable"),
                suspend: has("suspend"),
                over_current: has("oc"),
                speed: flags.iter().find(|f| UH_SPEED.is_match(f)).cloned().unwrap_or_default(),
                link: flags.iter().find(|f| UH_LINK.is_match(f)).cloned().unwrap_or_default(),
                attached,
                flags,
            };
            if let Some(h) = hubs.get_mut(loc) {
                h.ports.push(port);
            }
        } else if !line.starts_with(|c: char| c.is_whitespace()) {
            cur = None;
        }
    }
    hubs.into_values().collect()
}

/// uhubctl location ↔ busid: root hubs are just the bus number.
pub fn hub_loc(busid: &str) -> String {
    busid.strip_prefix("usb").unwrap_or(busid).to_string()
}

pub fn loc_busid(loc: &str) -> String {
    if loc.contains('-') { loc.to_string() } else { format!("usb{loc}") }
}

// ---------- Ganged groups and internal links ----------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Group {
    pub id: String,
    /// uhubctl locations, all switched together
    pub hubs: Vec<String>,
    pub label: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PowerTopology {
    pub groups: Vec<Group>,
    /// "hubloc:port" → why it must not be switched casually (internal hub-to-hub link).
    pub internal: IndexMap<String, String>,
}

static PI5: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)Raspberry Pi 5|Compute Module 5").unwrap());

/// Work out which ports really switch together. uhubctl's own mode is the starting point,
/// but Raspberry Pi onboard hubs claim per-port switching (ppps) while the hardware is
/// ganged: see the uhubctl README, "Raspberry Pi turns power off on all ports".
pub fn power_topology(inv: &Inventory, hubs: &[UhubHub], model: &str) -> PowerTopology {
    let mut groups = Vec::new();
    let mut internal = IndexMap::new();
    let mut in_group: HashSet<String> = HashSet::new();

    // Pi 4: VL805. USB2 root hub port 1 is the internal link to the VIA hub (2109:3431); that
    // hub and the USB3 root hub are the same four sockets, ganged.
    for c in inv.controllers.iter().filter(|c| c.pci_id == "1106:3483") {
        let roots: Vec<&UsbDevice> = inv.devices.iter().filter(|d| d.root && c.buses.contains(&d.bus)).collect();
        let r2 = roots.iter().find(|d| d.speed <= 480.0);
        let r3 = roots.iter().find(|d| d.speed >= 5000.0);
        let via =
            r2.and_then(|r2| inv.devices.iter().find(|d| d.parent.as_deref() == Some(r2.busid.as_str()) && d.port == Some(1) && d.hub));
        let locs: Vec<String> = [via.map(|v| hub_loc(&v.busid)), r3.map(|r| hub_loc(&r.busid))].into_iter().flatten().collect();
        if let Some(r2) = r2 {
            internal.insert(
                format!("{}:1", hub_loc(&r2.busid)),
                "Internal link to the onboard VIA hub. Cutting it drops every USB socket and confuses the hub.".to_string(),
            );
        }
        if !locs.is_empty() {
            groups.push(Group {
                id: format!("vl805-{}", c.id),
                label: format!(
                    "Raspberry Pi 4 sockets: all 4 switch together (hub {} are the USB 2 and USB 3 halves of the same sockets)",
                    locs.join(" and bus ")
                ),
                hubs: locs.clone(),
            });
            in_group.extend(locs);
        }
    }

    // Pi 5: every onboard root hub (RP1's two xHCIs) is one ganged VBUS group.
    if PI5.is_match(model) {
        let locs: Vec<String> = inv
            .devices
            .iter()
            .filter(|d| d.root && hubs.iter().any(|h| h.loc == hub_loc(&d.busid)))
            .map(|d| hub_loc(&d.busid))
            .filter(|l| !in_group.contains(l))
            .collect();
        if !locs.is_empty() {
            groups.push(Group {
                id: "pi5".into(),
                hubs: locs.clone(),
                label: "Raspberry Pi 5 sockets: all 4 share one VBUS switch; power only drops when every port is off".into(),
            });
            in_group.extend(locs);
        }
    }

    for h in hubs {
        if h.mode == "ganged" && !in_group.contains(&h.loc) {
            groups.push(Group {
                id: format!("ganged-{}", h.loc),
                hubs: vec![h.loc.clone()],
                label: format!("Hub {} has ganged power: all {} ports switch together", h.loc, h.nports),
            });
        }
    }

    // Generic: a hard-wired port feeding another hub is an internal daisy-chain link.
    for d in &inv.devices {
        for p in &d.ports {
            let child = p.child.as_deref().and_then(|c| inv.get(c));
            let key = format!("{}:{}", hub_loc(&d.busid), p.port);
            if p.connect_type == "hardwired" && child.is_some_and(|c| c.hub) && !internal.contains_key(&key) {
                internal.insert(key, "Hard-wired link to an internal hub. Cutting it confuses the hub circuitry.".to_string());
            }
        }
    }
    PowerTopology { groups, internal }
}

// ---------- Power planning ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerAction {
    On,
    Off,
    Cycle,
}

impl PowerAction {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            "cycle" => Some(Self::Cycle),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Cycle => "cycle",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PowerPlan {
    pub action: PowerAction,
    /// uhubctl location
    pub hub: String,
    /// None = whole hub
    pub port: Option<i64>,
    /// uhubctl argument lists (without sudo/binary); cycle = off-set then on-set
    pub commands: Vec<Vec<String>>,
    pub group: Option<Group>,
    /// "hub:port" switched
    pub ports: Vec<String>,
    /// busids that lose power
    pub devices: Vec<String>,
    pub warnings: Vec<String>,
    /// none | ganged | unsafe
    pub needs: &'static str,
}

/// Busids of a device and every hub above it (cutting any of those ports kills it).
pub fn with_upstream(devices: &[UsbDevice], busid: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut b = Some(busid.to_string());
    while let Some(x) = b {
        let Some(d) = devices.iter().find(|d| d.busid == x) else { break };
        out.push(x);
        b = d.parent.clone();
    }
    out
}

pub struct PowerReq {
    pub hub: String,
    pub port: Option<i64>,
    pub action: PowerAction,
}

/// What switching a hub/port would actually do, and how much confirmation it needs.
/// `protect` maps busid → reasons (rootfs, served drive, management NIC).
pub fn plan_power(
    inv: &Inventory,
    hubs: &[UhubHub],
    topo: &PowerTopology,
    protect: &IndexMap<String, Vec<String>>,
    req: &PowerReq,
) -> Result<PowerPlan, String> {
    let hub =
        hubs.iter().find(|h| h.loc == req.hub).ok_or_else(|| format!("Hub {} isn't power-switchable (not listed by uhubctl)", req.hub))?;
    if hub.mode == "nops" {
        return Err(format!("Hub {} has no power switching", req.hub));
    }
    let port = req.port;
    if let Some(p) = port
        && (p < 1 || p > hub.nports as i64)
    {
        return Err(format!("Hub {} has ports 1–{}", req.hub, hub.nports));
    }
    let group = topo.groups.iter().find(|g| g.hubs.contains(&hub.loc)).cloned();
    let force = |loc: &str| -> Vec<String> {
        if hubs.iter().find(|h| h.loc == loc).is_some_and(|h| h.mode == "ppps") { vec![] } else { vec!["-f".into()] }
    };

    // Ganged: the request widens to the whole group whatever port was asked for.
    let targets: Vec<(String, Option<i64>)> = match &group {
        Some(g) => g.hubs.iter().map(|l| (l.clone(), None)).collect(),
        None => vec![(hub.loc.clone(), port)],
    };
    let mut ports = Vec::new();
    for (loc, tp) in &targets {
        let n = hubs.iter().find(|x| &x.loc == loc).map(|h| h.nports).unwrap_or(0);
        for i in 1..=n as i64 {
            if tp.is_none() || *tp == Some(i) {
                ports.push(format!("{loc}:{i}"));
            }
        }
    }

    let mut devices: IndexMap<String, ()> = IndexMap::new();
    for pk in &ports {
        let (loc, pn) = pk.split_once(':').unwrap();
        let pn: u32 = pn.parse().unwrap_or(0);
        let child = inv.get(&loc_busid(loc)).and_then(|d| d.ports.iter().find(|p| p.port == pn)).and_then(|p| p.child.clone());
        if let Some(c) = child {
            for b in subtree(&inv.devices, &c) {
                devices.insert(b, ());
            }
        }
    }

    let mut warnings = Vec::new();
    let mut needs = "none";
    if req.action != PowerAction::On {
        if let Some(g) = &group {
            needs = "ganged";
            let what = match port {
                Some(p) => format!("port {p}"),
                None => "this hub".into(),
            };
            warnings.push(format!("{}. Switching {what} switches all {} ports listed.", g.label, ports.len()));
        }
        for pk in &ports {
            if let Some(why) = topo.internal.get(pk) {
                needs = "unsafe";
                warnings.push(format!("Port {pk}: {why}"));
            }
        }
        for b in devices.keys() {
            for why in protect.get(b).map(Vec::as_slice).unwrap_or(&[]) {
                needs = "unsafe";
                warnings.push(format!("{b} would lose power: it {why}."));
            }
        }
    }

    let cmd = |a: &str| -> Vec<Vec<String>> {
        targets
            .iter()
            .map(|(loc, p)| {
                let mut c = vec!["-l".to_string(), loc.clone()];
                if let Some(p) = p {
                    c.extend(["-p".to_string(), p.to_string()]);
                }
                c.extend(["-a".to_string(), a.to_string()]);
                c.extend(force(loc));
                c
            })
            .collect()
    };
    let commands = match req.action {
        PowerAction::Cycle => [cmd("off"), cmd("on")].concat(),
        a => cmd(a.as_str()),
    };
    Ok(PowerPlan {
        action: req.action,
        hub: hub.loc.clone(),
        port,
        commands,
        group,
        ports,
        devices: devices.into_keys().collect(),
        warnings,
        needs,
    })
}

// ---------- Kernel log filter ----------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct KernelLine {
    /// ISO timestamp from dmesg, "" if none
    pub ts: String,
    /// error | warn | info
    pub level: &'static str,
    pub text: String,
}

static K_TS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{4}-\d\d-\d\dT[\d:,.]+[+-]\d\d:?\d\d)\s+((?s).*)$").unwrap());
static K_SKIP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"New USB device strings|: Product: |: Manufacturer: |: SerialNumber: |New USB device found|registered new (interface |device )?driver|USB hub found|\d+ ports? detected").unwrap()
});
static K_ERR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)over-?current|cannot enumerate|unable to enumerate|device descriptor read|device not accepting address|can't set config|cannot reset|Cannot enable|not enough bandwidth|HC died|host controller halted|hub_port_status failed|cannot get .*descriptor|config .* has an invalid|unable to read config|-EPROTO|error -(71|110|32|19)").unwrap()
});
static K_WARN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\breset (?:low|full|high|super)[\w +-]*USB device number|disabled by hub \(EMI\?\)|over-?current condition cleared|Cannot use|usbip.*(detached|error)").unwrap()
});
static K_INFO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)new (?:low|full|high|super|wireless)[\w +-]*USB device number|USB disconnect|now attached to tty|now disconnected from tty|: tty\w+: USB ACM device|USB Mass Storage device detected|scsi host\d+: (usb-storage|uas)|Attached SCSI removable disk|^(cdc_acm|ftdi_sio|cp210x|ch341|pl2303|usbserial|option|gs_usb|peak_usb|usb-storage|uas|uvcvideo|cdc_ether|cdc_ncm|r8152|ax88179_178a|asix|qmi_wwan|rndis_host|btusb|snd-usb-audio|dvb_usb_rtl28xxu|rtl2832|usbtmc|xpad)\b|usbip-host \S+: usbip-host: register new device|hid-generic .*: input|: USB HID v|new USB bus registered|USB bus \d+ deregistered|vhci_hcd \S+: (Device attached|devid)").unwrap()
});
static K_USBISH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\busb\b|usb[-_]|xhci|ehci|ohci|dwc2|\bhub\b|tty(USB|ACM)|usbip|vhci").unwrap());

/// Classify one `dmesg --time-format iso` line; None if it isn't USB-worthy.
pub fn filter_kernel_line(line: &str) -> Option<KernelLine> {
    let t = line.trim();
    let (ts, text) = match K_TS.captures(t) {
        Some(m) => (m[1].to_string(), m[2].trim().to_string()),
        None => (String::new(), t.to_string()),
    };
    if text.is_empty() || K_SKIP.is_match(&text) {
        return None;
    }
    if !(K_USBISH.is_match(&text) || K_INFO.is_match(&text)) {
        return None;
    }
    let level = if K_ERR.is_match(&text) {
        "error"
    } else if K_WARN.is_match(&text) {
        "warn"
    } else if K_INFO.is_match(&text) {
        "info"
    } else {
        return None;
    };
    Some(KernelLine { ts, level, text })
}

// ---------- ss ----------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Peer {
    pub local: u16,
    pub peer: String,
}

static V4MAPPED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[::ffff:([\d.]+)\]").unwrap());

/// Peers of established TCP connections on the given local ports, from
/// `ss -tnH state established`.
pub fn parse_ss_peers(out: &str, local_ports: &[u16]) -> Vec<Peer> {
    let mut res = Vec::new();
    for line in out.split('\n') {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 4 {
            continue;
        }
        // With a state filter, columns are: Recv-Q Send-Q Local:Port Peer:Port [process]
        let local = f[2];
        let lp = parse_int(&local[local.rfind(':').map(|i| i + 1).unwrap_or(0)..]);
        if let Some(lp) = lp.and_then(|p| u16::try_from(p).ok())
            && local_ports.contains(&lp)
        {
            let peer = V4MAPPED.replace(f[3], "$1");
            let peer = peer.strip_prefix("::ffff:").unwrap_or(&peer).to_string();
            res.push(Peer { local: lp, peer });
        }
    }
    res
}
