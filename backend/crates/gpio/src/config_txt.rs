//! The Raspberry Pi side of the GPIO panel's pure logic (peripherals/lib/gpio-config.ts):
//! the `pinctrl get` parser and section-aware reading/editing of
//! /boot/firmware/config.txt, the extra BCM2711 controllers, pin usage and conflicts.
//!
//! config.txt rules that shape the editor: later lines win; [filter] lines start
//! conditional sections that last until the next filter, and [all] resets to
//! unconditional. Only lines in the effective [all] context are edited (the top of the
//! file and after any [all]), and new lines are appended under an [all] block, so a [cm4]
//! or [pi5] section is never touched; if a conditional section also sets the key, the
//! caller is told it can override the change.

use benchbob_core::http::parse_int;
use indexmap_lite::OrderedMap;
use regex::Regex;
use serde_json::{Value, json};

/// A tiny insertion-ordered map (the editor's objects keep JavaScript's key order).
mod indexmap_lite {
    #[derive(Debug, Clone, Default, PartialEq)]
    pub struct OrderedMap<V>(pub Vec<(String, V)>);
    impl<V> OrderedMap<V> {
        pub fn get(&self, k: &str) -> Option<&V> {
            self.0.iter().find(|(x, _)| x == k).map(|(_, v)| v)
        }
        pub fn get_mut(&mut self, k: &str) -> Option<&mut V> {
            self.0.iter_mut().find(|(x, _)| x == k).map(|(_, v)| v)
        }
        pub fn insert(&mut self, k: &str, v: V) {
            match self.get_mut(k) {
                Some(x) => *x = v,
                None => self.0.push((k.to_string(), v)),
            }
        }
        pub fn contains(&self, k: &str) -> bool {
            self.get(k).is_some()
        }
        pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
            self.0.iter().map(|(k, v)| (k, v))
        }
    }
}

// ---------- pinctrl ----------

#[derive(Debug, Clone, PartialEq)]
pub struct PinState {
    pub gpio: u32,
    /// ip | op | a0..a8 | no
    pub fsel: String,
    /// input | output | SDA1 | TXD1 | …
    pub func: String,
    pub level: Option<u8>,
    /// "up" | "down" | "none"
    pub pull: Option<&'static str>,
    /// "high" | "low" (for outputs)
    pub drive: Option<&'static str>,
    /// Board name, e.g. "ID_SDA/GPIO0"
    pub name: String,
}

/// `pinctrl get` lines:
/// ```text
///    2: a0    pu | hi // GPIO2 = SDA1
///   42: op -- pu | lo // STATUS_LED_G_CLK/GPIO42 = output
///   20: op dh pn | hi // GPIO20 = output
///  100: (null)    -- | -- // BT_ON/FWGPIO0 = (null)
/// ```
pub fn parse_pinctrl(text: &str) -> Vec<PinState> {
    let re = Regex::new(r"^\s*(\d+):\s+(\S+)\s+(?:(dh|dl|--)\s+)?(pu|pd|pn|--)\s*\|\s*(hi|lo|--)\s*//\s*(.*?)\s*=\s*(.*?)\s*$").unwrap();
    text.split('\n')
        .filter_map(|l| {
            let m = re.captures(l)?;
            Some(PinState {
                gpio: m[1].parse().ok()?,
                fsel: m[2].to_string(),
                func: m[7].to_string(),
                level: match &m[5] {
                    "hi" => Some(1),
                    "lo" => Some(0),
                    _ => None,
                },
                pull: match &m[4] {
                    "pu" => Some("up"),
                    "pd" => Some("down"),
                    "pn" => Some("none"),
                    _ => None,
                },
                drive: match m.get(3).map(|x| x.as_str()) {
                    Some("dh") => Some("high"),
                    Some("dl") => Some("low"),
                    _ => None,
                },
                name: m[6].to_string(),
            })
        })
        .collect()
}

// ---------- config.txt ----------

#[derive(Debug, Clone)]
pub struct CfgLine {
    pub i: usize,
    pub text: String,
    /// "all" or the active filter, e.g. "cm4"
    pub section: String,
    /// Not a comment / blank.
    pub active: bool,
    pub key: Option<String>,
    pub value: Option<String>,
}

pub fn parse_config(text: &str) -> Vec<CfgLine> {
    let sec_re = Regex::new(r"^\[([^\]]+)\]").unwrap();
    let kv_re = Regex::new(r"^([A-Za-z0-9_]+)\s*=\s*(.*?)\s*$").unwrap();
    let mut section = "all".to_string();
    text.split('\n')
        .enumerate()
        .map(|(i, raw)| {
            let t = raw.strip_suffix('\r').unwrap_or(raw).to_string();
            let s = t.trim();
            if let Some(c) = sec_re.captures(s) {
                // A new filter replaces the previous one; [all] resets.
                section = c[1].trim().to_lowercase();
                return CfgLine { i, text: t.clone(), section: section.clone(), active: false, key: None, value: None };
            }
            let kv = kv_re.captures(s);
            CfgLine {
                i,
                section: section.clone(),
                active: kv.is_some(),
                key: kv.as_ref().map(|c| c[1].to_string()),
                value: kv.as_ref().map(|c| c[2].to_string()),
                text: t,
            }
        })
        .collect()
}

/// "i2c_arm=on,i2c_arm_baudrate=400000" → [("i2c_arm", Some("on")), ("i2c_arm_baudrate", Some("400000"))]
pub fn params(v: &str) -> Vec<(String, Option<String>)> {
    v.split(',')
        .map(|p| {
            let mut it = p.split('=');
            let k = it.next().unwrap_or("").trim().to_string();
            let rest: Vec<&str> = it.collect();
            (k, if rest.is_empty() { None } else { Some(rest.join("=").trim().to_string()) })
        })
        .collect()
}

fn truthy(v: Option<&str>) -> bool {
    match v {
        None => true,
        Some(s) => matches!(s.to_lowercase().as_str(), "on" | "true" | "yes" | "1" | "okay"),
    }
}

const I2C_NAMES: [&str; 2] = ["i2c_arm", "i2c"];
const BAUD_NAMES: [&str; 2] = ["i2c_arm_baudrate", "i2c_baudrate"];

// ---------- extra controllers (BCM2711: Pi 4, Pi 400, CM4) ----------
//
// From /boot/firmware/overlays/README (checked on desky 2026-10-18): each of these "BCM2711
// only" overlays enables one more controller. The I²C ones take one of two pin pairs (the
// first listed here is the overlay's default) and a baudrate (default 100000); the UARTs
// put TX/RX on the first two pins and, with `ctsrts`, CTS/RTS on the next two. Pi 5 has
// differently named overlays (…-pi5) on other pins, so none of this is offered there.

pub struct ExtraI2c {
    pub name: &'static str,
    pub choices: [(&'static str, [u32; 2]); 2],
    pub def: &'static str,
}

pub const EXTRA_I2C: [ExtraI2c; 4] = [
    ExtraI2c { name: "i2c3", choices: [("pins_4_5", [4, 5]), ("pins_2_3", [2, 3])], def: "pins_4_5" },
    ExtraI2c { name: "i2c4", choices: [("pins_8_9", [8, 9]), ("pins_6_7", [6, 7])], def: "pins_8_9" },
    ExtraI2c { name: "i2c5", choices: [("pins_12_13", [12, 13]), ("pins_10_11", [10, 11])], def: "pins_12_13" },
    ExtraI2c { name: "i2c6", choices: [("pins_22_23", [22, 23]), ("pins_0_1", [0, 1])], def: "pins_22_23" },
];

pub struct ExtraUart {
    pub name: &'static str,
    pub tx: u32,
    pub rx: u32,
    pub cts: u32,
    pub rts: u32,
}

pub const EXTRA_UART: [ExtraUart; 4] = [
    ExtraUart { name: "uart2", tx: 0, rx: 1, cts: 2, rts: 3 },
    ExtraUart { name: "uart3", tx: 4, rx: 5, cts: 6, rts: 7 },
    ExtraUart { name: "uart4", tx: 8, rx: 9, cts: 10, rts: 11 },
    ExtraUart { name: "uart5", tx: 12, rx: 13, cts: 14, rts: 15 },
];

/// i2c-gpio (bit-banged, any pins): the overlay's default pins.
pub const I2C_GPIO_DEFAULTS: (u32, u32) = (23, 24);
/// Bus numbers a Pi 4's own controllers (and the HDMI DDC buses) use; i2c-gpio must pick another.
const RESERVED_BUSES: [i64; 10] = [1, 2, 3, 4, 5, 6, 10, 20, 21, 22];

pub fn extra_i2c(n: &str) -> Option<&'static ExtraI2c> {
    EXTRA_I2C.iter().find(|x| x.name == n)
}
pub fn extra_uart(n: &str) -> Option<&'static ExtraUart> {
    EXTRA_UART.iter().find(|x| x.name == n)
}
impl ExtraI2c {
    pub fn choice(&self, k: &str) -> Option<[u32; 2]> {
        self.choices.iter().find(|(c, _)| *c == k).map(|x| x.1)
    }
}

fn overlay_name(v: &str) -> String {
    params(v).first().map(|p| p.0.clone()).unwrap_or_default()
}
fn is_managed_overlay(n: &str) -> bool {
    n.starts_with("w1-gpio") || extra_i2c(n).is_some() || extra_uart(n).is_some() || n == "i2c-gpio"
}

/// The catalog the dialog may offer (GET /api/gpio/interfaces `catalog`).
pub fn catalog() -> Value {
    let i2c: serde_json::Map<String, Value> = EXTRA_I2C
        .iter()
        .map(|x| (x.name.to_string(), json!({ "choices": x.choices.iter().map(|(k, p)| (k.to_string(), json!(p))).collect::<serde_json::Map<_, _>>(), "def": x.def })))
        .collect();
    let uart: serde_json::Map<String, Value> =
        EXTRA_UART.iter().map(|x| (x.name.to_string(), json!({ "tx": x.tx, "rx": x.rx, "cts": x.cts, "rts": x.rts }))).collect();
    json!({ "extraI2c": i2c, "extraUart": uart, "i2cGpio": { "sda": I2C_GPIO_DEFAULTS.0, "scl": I2C_GPIO_DEFAULTS.1 } })
}

#[derive(Debug, Clone, PartialEq)]
pub struct I2cX {
    pub enabled: bool,
    pub pins: String,
    pub baudrate: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UartX {
    pub enabled: bool,
    pub ctsrts: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct I2cGpio {
    pub enabled: bool,
    pub sda: i64,
    pub scl: i64,
    pub bus: Option<i64>,
    pub delay_us: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Interfaces {
    pub i2c: (bool, Option<i64>),
    pub spi: bool,
    pub uart: bool,
    /// enabled, pin (None = NaN in config), pullup
    pub onewire: (bool, Option<i64>, bool),
    pub extra_i2c: OrderedMap<I2cX>,
    pub extra_uart: OrderedMap<UartX>,
    pub i2c_gpio: I2cGpio,
    /// Keys also set in conditional sections (may override the [all] value).
    pub conditional: Vec<(String, String)>,
}

impl Interfaces {
    pub fn to_json(&self) -> Value {
        json!({
            "i2c": { "enabled": self.i2c.0, "baudrate": self.i2c.1 },
            "spi": { "enabled": self.spi },
            "uart": { "enabled": self.uart },
            "onewire": { "enabled": self.onewire.0, "pin": self.onewire.1, "pullup": self.onewire.2 },
            "extraI2c": self.extra_i2c.iter().map(|(k, x)| (k.clone(), json!({ "enabled": x.enabled, "pins": x.pins, "baudrate": x.baudrate }))).collect::<serde_json::Map<_, _>>(),
            "extraUart": self.extra_uart.iter().map(|(k, x)| (k.clone(), json!({ "enabled": x.enabled, "ctsrts": x.ctsrts }))).collect::<serde_json::Map<_, _>>(),
            "i2cGpio": { "enabled": self.i2c_gpio.enabled, "sda": self.i2c_gpio.sda, "scl": self.i2c_gpio.scl, "bus": self.i2c_gpio.bus, "delayUs": self.i2c_gpio.delay_us },
            "conditional": self.conditional.iter().map(|(s, l)| json!({ "section": s, "line": l })).collect::<Vec<_>>(),
        })
    }
}

pub fn read_interfaces(text: &str) -> Interfaces {
    let mut r = Interfaces {
        i2c: (false, None),
        spi: false,
        uart: false,
        onewire: (false, None, false),
        extra_i2c: OrderedMap(
            EXTRA_I2C.iter().map(|x| (x.name.to_string(), I2cX { enabled: false, pins: x.def.into(), baudrate: None })).collect(),
        ),
        extra_uart: OrderedMap(EXTRA_UART.iter().map(|x| (x.name.to_string(), UartX { enabled: false, ctsrts: false })).collect()),
        i2c_gpio: I2cGpio { enabled: false, sda: I2C_GPIO_DEFAULTS.0 as i64, scl: I2C_GPIO_DEFAULTS.1 as i64, bus: None, delay_us: None },
        conditional: Vec::new(),
    };
    for l in parse_config(text) {
        if !l.active {
            continue;
        }
        let (key, value) = (l.key.as_deref().unwrap_or(""), l.value.as_deref().unwrap_or(""));
        let relevant = (key == "dtparam"
            && params(value).iter().any(|(k, _)| I2C_NAMES.contains(&k.as_str()) || BAUD_NAMES.contains(&k.as_str()) || k == "spi"))
            || key == "enable_uart"
            || (key == "dtoverlay" && is_managed_overlay(&overlay_name(value)));
        if !relevant {
            continue;
        }
        if l.section != "all" {
            r.conditional.push((l.section.clone(), l.text.trim().to_string()));
            continue;
        }
        if key == "dtparam" {
            for (k, v) in params(value) {
                if I2C_NAMES.contains(&k.as_str()) {
                    r.i2c.0 = truthy(v.as_deref());
                }
                if BAUD_NAMES.contains(&k.as_str())
                    && let Some(v) = v.as_deref().filter(|s| !s.is_empty())
                {
                    r.i2c.1 = parse_int(v).filter(|n| *n != 0);
                }
                if k == "spi" {
                    r.spi = truthy(v.as_deref());
                }
            }
        }
        if key == "enable_uart" {
            r.uart = value == "1";
        }
        if key == "dtoverlay" {
            let ps = params(value);
            let name = ps[0].0.clone();
            let rest = &ps[1..];
            let num = |k: &str| rest.iter().find(|(x, _)| x == k).and_then(|(_, v)| v.as_deref()).and_then(parse_int);
            if let Some(x) = extra_i2c(&name) {
                let pins = rest
                    .iter()
                    .rfind(|(k, v)| x.choice(k).is_some() && truthy(v.as_deref()))
                    .map(|p| p.0.clone())
                    .unwrap_or_else(|| x.def.into());
                r.extra_i2c.insert(&name, I2cX { enabled: true, pins, baudrate: num("baudrate") });
                continue;
            }
            if extra_uart(&name).is_some() {
                let ctsrts = rest.iter().any(|(k, v)| (k == "ctsrts" || k == "rs485") && truthy(v.as_deref()));
                r.extra_uart.insert(&name, UartX { enabled: true, ctsrts });
                continue;
            }
            if name == "i2c-gpio" {
                r.i2c_gpio = I2cGpio {
                    enabled: true,
                    sda: num("i2c_gpio_sda").unwrap_or(I2C_GPIO_DEFAULTS.0 as i64),
                    scl: num("i2c_gpio_scl").unwrap_or(I2C_GPIO_DEFAULTS.1 as i64),
                    bus: num("bus"),
                    delay_us: num("i2c_gpio_delay_us"),
                };
                continue;
            }
            r.onewire.0 = true;
            r.onewire.2 = name == "w1-gpio-pullup" || rest.iter().any(|(k, v)| k == "pullup" && truthy(v.as_deref()));
            r.onewire.1 = match rest.iter().find(|(k, _)| k == "gpiopin") {
                Some((_, Some(v))) if !v.is_empty() => parse_int(v),
                _ => Some(4),
            };
        }
    }
    r
}

/// One extra I²C controller's change: name, enabled, pins, baudrate.
pub type ExtraI2cChange = (String, Option<bool>, Option<String>, Option<f64>);

/// Interface changes, as parsed from a stage request.
#[derive(Debug, Clone, Default)]
pub struct Changes {
    pub i2c: Option<(Option<bool>, Option<f64>)>,
    pub spi: Option<bool>,
    pub uart: Option<bool>,
    pub onewire: Option<(Option<bool>, Option<i64>)>,
    pub extra_i2c: Vec<ExtraI2cChange>,
    pub extra_uart: Vec<(String, Option<bool>, Option<bool>)>,
    /// enabled, sda, scl, bus (Some(None) = clear), delayUs (Some(None) = clear)
    pub i2c_gpio: Option<I2cGpioChange>,
}

#[derive(Debug, Clone, Default)]
pub struct I2cGpioChange {
    pub enabled: Option<bool>,
    pub sda: Option<f64>,
    pub scl: Option<f64>,
    pub bus: Option<Option<f64>>,
    pub delay_us: Option<Option<f64>>,
}

// ---------- pin usage ----------

#[derive(Debug, Clone, PartialEq)]
pub struct PinUse {
    pub gpio: i64,
    /// "I²C1", "UART3", "1-Wire"…
    pub owner: String,
    /// "SDA", "TX", "CTS"…
    pub role: String,
}

pub fn i2c_label(n: &str) -> String {
    match n.strip_prefix("i2c") {
        Some(d) if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => format!("I²C{d}"),
        _ => n.to_string(),
    }
}

/// Which header GPIOs each enabled interface takes.
pub fn pin_usage(r: &Interfaces) -> Vec<PinUse> {
    let mut u = Vec::new();
    let mut add = |owner: &str, pins: &[(i64, &str)]| {
        for (g, role) in pins {
            u.push(PinUse { gpio: *g, owner: owner.into(), role: (*role).into() });
        }
    };
    if r.i2c.0 {
        add("I²C1", &[(2, "SDA"), (3, "SCL")]);
    }
    if r.spi {
        add("SPI0", &[(7, "CE1"), (8, "CE0"), (9, "MISO"), (10, "MOSI"), (11, "SCLK")]);
    }
    if r.uart {
        add("UART serial0", &[(14, "TX"), (15, "RX")]);
    }
    if r.onewire.0 {
        add("1-Wire", &[(r.onewire.1.unwrap_or(4), "data")]);
    }
    for (n, x) in r.extra_i2c.iter() {
        if !x.enabled {
            continue;
        }
        let e = extra_i2c(n).unwrap();
        let [sda, scl] = e.choice(&x.pins).unwrap_or_else(|| e.choice(e.def).unwrap());
        add(&i2c_label(n), &[(sda as i64, "SDA"), (scl as i64, "SCL")]);
    }
    for (n, x) in r.extra_uart.iter() {
        if !x.enabled {
            continue;
        }
        let p = extra_uart(n).unwrap();
        let mut pins = vec![(p.tx as i64, "TX"), (p.rx as i64, "RX")];
        if x.ctsrts {
            pins.extend([(p.cts as i64, "CTS"), (p.rts as i64, "RTS")]);
        }
        add(&n.to_uppercase(), &pins);
    }
    if r.i2c_gpio.enabled {
        add("i2c-gpio", &[(r.i2c_gpio.sda, "SDA"), (r.i2c_gpio.scl, "SCL")]);
    }
    u
}

/// GPIOs claimed by more than one interface, e.g. 1-Wire and UART3 TX both on GPIO4.
pub fn pin_conflicts(r: &Interfaces) -> Vec<(i64, Vec<String>)> {
    let mut by: Vec<(i64, Vec<String>)> = Vec::new();
    for p in pin_usage(r) {
        let who = format!("{} {}", p.owner, p.role);
        match by.iter_mut().find(|(g, _)| *g == p.gpio) {
            Some((_, v)) => v.push(who),
            None => by.push((p.gpio, vec![who])),
        }
    }
    let mut c: Vec<(i64, Vec<String>)> = by.into_iter().filter(|(_, u)| u.len() > 1).collect();
    c.sort_by_key(|x| x.0);
    c
}

pub fn usage_json(u: &[PinUse]) -> Value {
    Value::Array(u.iter().map(|p| json!({ "gpio": p.gpio, "owner": p.owner, "role": p.role })).collect())
}

pub fn conflicts_json(c: &[(i64, Vec<String>)]) -> Value {
    Value::Array(c.iter().map(|(g, u)| json!({ "gpio": g, "users": u })).collect())
}

const MARK: &str = "# Added by BenchBob (GPIO panel → Interfaces)";

fn fmt_params(ps: &[(String, Option<String>)]) -> String {
    ps.iter()
        .map(|(k, v)| match v {
            None => k.clone(),
            Some(v) => format!("{k}={v}"),
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn comment_out(line: &str) -> String {
    format!("#{}", line.trim_start())
}

/// Apply interface changes to config.txt text; only [all]-context lines are edited.
pub fn edit_config(text: &str, ch: &Changes) -> Result<String, String> {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out: Vec<String> = text.replace("\r\n", "\n").split('\n').map(String::from).collect();
    let mut append: Vec<String> = Vec::new();
    let all_lines =
        |out: &Vec<String>| -> Vec<CfgLine> { parse_config(&out.join("\n")).into_iter().filter(|l| l.section == "all").collect() };

    // Last active dtparam line in [all] carrying one of `names`; edits that param in place.
    let set_param = |out: &mut Vec<String>, append: &mut Vec<String>, names: &[&str], value: &str, enable: bool| {
        let lines = all_lines(out);
        let hit = lines.iter().rfind(|l| {
            l.active
                && l.key.as_deref() == Some("dtparam")
                && params(l.value.as_deref().unwrap_or("")).iter().any(|(k, _)| names.contains(&k.as_str()))
        });
        if let Some(hit) = hit {
            let ps: Vec<(String, Option<String>)> = params(hit.value.as_deref().unwrap_or(""))
                .into_iter()
                .map(|(k, v)| if names.contains(&k.as_str()) { (k, Some(value.to_string())) } else { (k, v) })
                .collect();
            let indent: String = out[hit.i].chars().take_while(|c| c.is_whitespace()).collect();
            out[hit.i] = format!("{indent}dtparam={}", fmt_params(&ps));
            return;
        }
        if !enable {
            return; // absent = default off
        }
        // Prefer un-commenting the stock "#dtparam=spi=on" line where it sits.
        let re = Regex::new(&format!(r"^\s*#\s*dtparam=({})=", names.join("|"))).unwrap();
        match lines.iter().rfind(|l| !l.active && re.is_match(&l.text)) {
            Some(c) => out[c.i] = format!("dtparam={}={value}", names[0]),
            None => append.push(format!("dtparam={}={value}", names[0])),
        }
    };

    if let Some((Some(en), _)) = ch.i2c {
        set_param(&mut out, &mut append, &I2C_NAMES, if en { "on" } else { "off" }, en);
    }
    if let Some((_, Some(baud))) = ch.i2c {
        let has = all_lines(&out).iter().any(|l| {
            l.active
                && l.key.as_deref() == Some("dtparam")
                && params(l.value.as_deref().unwrap_or("")).iter().any(|(k, _)| BAUD_NAMES.contains(&k.as_str()))
        });
        if has || baud != 100000.0 {
            set_param(&mut out, &mut append, &BAUD_NAMES, &js_num(baud), true);
        }
    }
    if let Some(en) = ch.spi {
        set_param(&mut out, &mut append, &["spi"], if en { "on" } else { "off" }, en);
    }
    if let Some(en) = ch.uart {
        let hits: Vec<usize> =
            all_lines(&out).iter().filter(|l| l.active && l.key.as_deref() == Some("enable_uart")).map(|l| l.i).collect();
        let v = if en { "1" } else { "0" };
        if let Some(&i) = hits.last() {
            out[i] = format!("enable_uart={v}");
        } else if en {
            append.push("enable_uart=1".into());
        }
    }
    if let Some((en, pin)) = ch.onewire
        && (en.is_some() || pin.is_some())
    {
        let hits: Vec<CfgLine> = all_lines(&out)
            .into_iter()
            .filter(|l| l.active && l.key.as_deref() == Some("dtoverlay") && l.value.as_deref().unwrap_or("").starts_with("w1-gpio"))
            .collect();
        let enable = en.unwrap_or(!hits.is_empty());
        if !enable {
            for h in &hits {
                out[h.i] = comment_out(&out[h.i]);
            }
        } else {
            if let Some(p) = pin
                && !(2..=27).contains(&p)
            {
                return Err("1-Wire pin must be GPIO2–27".into());
            }
            match hits.last() {
                Some(h) => {
                    if let Some(p) = pin {
                        let ps = params(h.value.as_deref().unwrap_or(""));
                        let rest: Vec<(String, Option<String>)> = ps[1..].iter().filter(|(k, _)| k != "gpiopin").cloned().collect();
                        let mut all = vec![(ps[0].0.clone(), None), (format!("gpiopin={p}"), None)];
                        all.extend(rest);
                        out[h.i] = format!("dtoverlay={}", fmt_params(&all));
                    }
                }
                None => append.push(format!("dtoverlay=w1-gpio,gpiopin={}", pin.unwrap_or(4))),
            }
        }
    }

    // Extra controllers: one dtoverlay line each. The wanted state is the current one with
    // the change merged in; a line is only rewritten when that state differs, and params we
    // don't manage (the UARTs' rs485 options…) are kept.
    let cur = read_interfaces(text);
    let set_overlay = |out: &mut Vec<String>,
                       append: &mut Vec<String>,
                       name: &str,
                       enable: bool,
                       managed: &dyn Fn(&str) -> bool,
                       want: Vec<(String, Option<String>)>| {
        let hits: Vec<CfgLine> = all_lines(out)
            .into_iter()
            .filter(|l| l.active && l.key.as_deref() == Some("dtoverlay") && overlay_name(l.value.as_deref().unwrap_or("")) == name)
            .collect();
        if !enable {
            for h in &hits {
                out[h.i] = comment_out(&out[h.i]);
            }
            return;
        }
        let h = hits.last();
        let keep: Vec<(String, Option<String>)> = h
            .map(|h| params(h.value.as_deref().unwrap_or(""))[1..].iter().filter(|(k, _)| !managed(k)).cloned().collect())
            .unwrap_or_default();
        let mut all = vec![(name.to_string(), None)];
        all.extend(want);
        all.extend(keep);
        let line = format!("dtoverlay={}", fmt_params(&all));
        match h {
            Some(h) => out[h.i] = line,
            None => append.push(line),
        }
    };
    for (n, en, pins, baud) in &ch.extra_i2c {
        let Some(x) = extra_i2c(n) else {
            return Err(format!(
                "Unknown I²C controller {n} (expected {})",
                EXTRA_I2C.iter().map(|x| x.name).collect::<Vec<_>>().join(", ")
            ));
        };
        let was = cur.extra_i2c.get(n).unwrap().clone();
        let enabled = en.unwrap_or(was.enabled);
        let pins = pins.clone().unwrap_or(was.pins.clone());
        let baud = baud.or(was.baudrate.map(|b| b as f64));
        if x.choice(&pins).is_none() {
            return Err(format!("{n} pins must be one of {}", x.choices.iter().map(|c| c.0).collect::<Vec<_>>().join(", ")));
        }
        if let Some(b) = baud
            && !(10000.0..=3_400_000.0).contains(&b)
        {
            return Err(format!("{n} baudrate must be 10000–3400000 Hz"));
        }
        let was_baud = was.baudrate.map(|b| b as f64).unwrap_or(100000.0);
        if enabled == was.enabled && (!enabled || (pins == was.pins && baud.unwrap_or(100000.0) == was_baud)) {
            continue;
        }
        let mut want = vec![(pins.clone(), None)];
        if let Some(b) = baud.filter(|b| *b != 100000.0) {
            want.push(("baudrate".into(), Some(js_num(b))));
        }
        set_overlay(&mut out, &mut append, n, enabled, &|k| x.choice(k).is_some() || k == "baudrate", want);
    }
    for (n, en, cts) in &ch.extra_uart {
        if extra_uart(n).is_none() {
            return Err(format!("Unknown UART {n} (expected {})", EXTRA_UART.iter().map(|x| x.name).collect::<Vec<_>>().join(", ")));
        }
        let was = cur.extra_uart.get(n).unwrap().clone();
        let enabled = en.unwrap_or(was.enabled);
        let ctsrts = cts.unwrap_or(was.ctsrts);
        if enabled == was.enabled && (!enabled || ctsrts == was.ctsrts) {
            continue;
        }
        set_overlay(&mut out, &mut append, n, enabled, &|k| k == "ctsrts", if ctsrts { vec![("ctsrts".into(), None)] } else { vec![] });
    }
    if let Some(c) = &ch.i2c_gpio {
        let was = &cur.i2c_gpio;
        let enabled = c.enabled.unwrap_or(was.enabled);
        let sda = c.sda.unwrap_or(was.sda as f64);
        let scl = c.scl.unwrap_or(was.scl as f64);
        let bus = match c.bus {
            Some(b) => b,
            None => was.bus.map(|b| b as f64),
        };
        let delay = match c.delay_us {
            Some(d) => d,
            None => was.delay_us.map(|d| d as f64),
        };
        let pin_ok = |g: f64| g.fract() == 0.0 && (0.0..=27.0).contains(&g);
        if enabled {
            if !pin_ok(sda) || !pin_ok(scl) || sda == scl {
                return Err("i2c-gpio SDA and SCL must be two different GPIOs 0–27".into());
            }
            if let Some(b) = bus
                && (b.fract() != 0.0 || !(1.0..=255.0).contains(&b) || RESERVED_BUSES.contains(&(b as i64)))
            {
                return Err(format!(
                    "i2c-gpio bus number must be 1–255 and not {} (the Pi's own buses)",
                    RESERVED_BUSES.iter().map(|x| x.to_string()).collect::<Vec<_>>().join("/")
                ));
            }
            if let Some(d) = delay
                && !(d.fract() == 0.0 && (1.0..=100.0).contains(&d))
            {
                return Err("i2c-gpio clock delay must be 1–100 µs".into());
            }
        }
        let same = enabled == was.enabled
            && (!enabled
                || (sda == was.sda as f64
                    && scl == was.scl as f64
                    && bus == was.bus.map(|b| b as f64)
                    && delay == was.delay_us.map(|d| d as f64)));
        if !same {
            let mut want = vec![("i2c_gpio_sda".to_string(), Some(js_num(sda))), ("i2c_gpio_scl".to_string(), Some(js_num(scl)))];
            if let Some(b) = bus {
                want.push(("bus".into(), Some(js_num(b))));
            }
            if let Some(d) = delay {
                want.push(("i2c_gpio_delay_us".into(), Some(js_num(d))));
            }
            set_overlay(
                &mut out,
                &mut append,
                "i2c-gpio",
                enabled,
                &|k| ["i2c_gpio_sda", "i2c_gpio_scl", "bus", "i2c_gpio_delay_us"].contains(&k),
                want,
            );
        }
    }

    if !append.is_empty() {
        while out.last().is_some_and(|l| l.trim().is_empty()) {
            out.pop();
        }
        let joined = out.join("\n");
        let last_section = parse_config(&joined).into_iter().rev().find(|l| l.text.trim_start().starts_with('['));
        let in_all = last_section.is_none_or(|l| l.text.trim_start().to_lowercase().starts_with("[all]"));
        out.push(String::new());
        if !in_all {
            out.push("[all]".into());
        }
        out.push(MARK.into());
        out.extend(append);
        out.push(String::new());
    }
    Ok(out.join("\n").replace('\n', eol))
}

/// A number as JavaScript prints it in a template string.
fn js_num(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e21 { format!("{}", f as i64) } else { format!("{f}") }
}

// ---------- live device tree ----------

/// Onboard UART ttys mapped to their controller through the device-tree aliases:
/// ttyS0 → uart1, ttyAMA2 → uart3. (Reads /proc and /sys.)
pub fn onboard_uarts() -> Vec<(String, String, String, String)> {
    let mut aliases = Vec::new();
    for i in 0..=5 {
        if let Ok(v) = std::fs::read_to_string(format!("/proc/device-tree/aliases/uart{i}"))
            && !v.is_empty()
        {
            aliases.push((v.replace('\0', ""), format!("uart{i}")));
        }
    }
    let re = Regex::new(r"^tty(AMA|S)\d+$").unwrap();
    let mut out = Vec::new();
    for e in std::fs::read_dir("/sys/class/tty").into_iter().flatten().flatten() {
        let tty = e.file_name().to_string_lossy().into_owned();
        if !re.is_match(&tty) {
            continue;
        }
        let node = std::fs::canonicalize(format!("/sys/class/tty/{tty}/device/of_node"))
            .map(|p| p.to_string_lossy().trim_start_matches("/sys/firmware/devicetree/base").to_string())
            .unwrap_or_default();
        let Some(uart) = (!node.is_empty()).then(|| aliases.iter().find(|(n, _)| *n == node).map(|x| x.1.clone())).flatten() else {
            continue;
        };
        let pins = extra_uart(&uart).map(|p| format!("GPIO{}/{}", p.tx, p.rx)).unwrap_or_default();
        out.push((uart, tty, node, pins));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---------- unified diff ----------

/// Line-based unified diff (LCS) with `ctx` lines of context; "" when equal.
pub fn unified_diff(a: &str, b: &str, name: &str, ctx: usize) -> String {
    if a == b {
        return String::new();
    }
    // A final newline is a line terminator, not an empty last line.
    let lines = |s: &str| -> Vec<String> {
        let mut l: Vec<String> = s.replace("\r\n", "\n").split('\n').map(String::from).collect();
        if l.len() > 1 && l.last().is_some_and(|x| x.is_empty()) {
            l.pop();
        }
        l
    };
    let (aa, bb) = (lines(a), lines(b));
    let (n, m) = (aa.len(), bb.len());
    let mut lcs = vec![vec![0i32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if aa[i] == bb[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    struct Op<'a> {
        t: char,
        s: &'a str,
        ai: usize,
        bi: usize,
    }
    let mut ops: Vec<Op> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && aa[i] == bb[j] {
            ops.push(Op { t: ' ', s: &aa[i], ai: i, bi: j });
            i += 1;
            j += 1;
        } else if i < n && (j >= m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            ops.push(Op { t: '-', s: &aa[i], ai: i, bi: j }); // deletions first
            i += 1;
        } else {
            ops.push(Op { t: '+', s: &bb[j], ai: i, bi: j });
            j += 1;
        }
    }
    let mut out = vec![format!("--- {name}"), format!("+++ {name} (proposed)")];
    let mut k = 0;
    while k < ops.len() {
        if ops[k].t == ' ' {
            k += 1;
            continue;
        }
        let start = k.saturating_sub(ctx);
        let mut end = k;
        // Extend the hunk while changes are within 2*ctx of each other.
        while end < ops.len() {
            if ops[end].t != ' ' {
                end += 1;
                continue;
            }
            let mut run = 0;
            while end + run < ops.len() && ops[end + run].t == ' ' {
                run += 1;
            }
            if end + run >= ops.len() || run > 2 * ctx {
                end = ops.len().min(end + ctx);
                break;
            }
            end += run;
        }
        let h = &ops[start..end];
        let (a_start, b_start) = (h[0].ai + 1, h[0].bi + 1);
        let a_len = h.iter().filter(|o| o.t != '+').count();
        let b_len = h.iter().filter(|o| o.t != '-').count();
        out.push(format!(
            "@@ -{},{a_len} +{},{b_len} @@",
            if a_len > 0 { a_start } else { a_start - 1 },
            if b_len > 0 { b_start } else { b_start - 1 }
        ));
        for o in h {
            out.push(format!("{}{}", o.t, o.s));
        }
        k = end;
    }
    out.join("\n") + "\n"
}

#[cfg(test)]
mod tests;
