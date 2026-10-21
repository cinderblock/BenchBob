//! CAN interfaces as `ip -details -statistics -json link show` reports them, and the `ip
//! link` arguments that configure one (can.ts's parseIpLink, normCfg and ipArgs).

use benchbob_core::HttpError;
use benchbob_core::http::{js_number, js_string, parse_float, truthy};
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct Usb {
    pub vendor: String,
    pub model: String,
    pub serial: String,
    pub vidpid: String,
    pub path: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub rx_packets: Value,
    pub tx_packets: Value,
    pub rx_errors: Value,
    pub tx_errors: Value,
    pub rx_dropped: Value,
    pub tx_dropped: Value,
    pub rx_bytes: Value,
    pub tx_bytes: Value,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Iface {
    pub name: String,
    /// "can" for real controllers (incl. slcan), "vcan", "vxcan"
    pub kind: String,
    pub driver: String,
    pub up: bool,
    pub operstate: String,
    pub mtu: Value,
    pub fd_capable: bool,
    pub fd_on: bool,
    /// ERROR-ACTIVE, ERROR-WARNING, ERROR-PASSIVE, BUS-OFF, STOPPED
    pub state: Option<Value>,
    pub berr: Option<(Value, Value)>,
    pub restart_ms: Option<f64>,
    pub bitrate: Option<f64>,
    pub sample_point: Option<f64>,
    pub dbitrate: Option<f64>,
    pub dsample_point: Option<f64>,
    pub ctrl: Vec<String>,
    /// None: the kernel/iproute2 didn't say.
    pub ctrl_supported: Option<Vec<String>>,
    pub clock: Option<f64>,
    pub bitrate_const: Option<Value>,
    pub xstats: Option<Value>,
    pub stats: Stats,
    // Filled from udev / our own bookkeeping:
    pub key: String,
    pub label: String,
    pub usb: Option<Usb>,
    pub slcan_tty: Option<String>,
}

impl Iface {
    pub fn state_str(&self) -> Option<String> {
        self.state.as_ref().map(js_string)
    }
    pub fn listen_only(&self) -> bool {
        self.ctrl.iter().any(|c| c == "LISTEN-ONLY")
    }
}

fn strings(v: Option<&Value>) -> Option<Vec<String>> {
    v.and_then(Value::as_array).map(|a| a.iter().map(js_string).collect())
}

/// `v == null || v === "" ? undefined : Number(v)`
fn num(v: Option<&Value>) -> Option<f64> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(v) => Some(js_number(v)),
    }
}

/// `a ?? b` over JSON (undefined or null fall through).
fn or<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    a.filter(|v| !v.is_null()).or(b)
}

/// `x || undefined` for a number.
fn nonzero(x: Option<f64>) -> Option<f64> {
    x.filter(|n| *n != 0.0 && !n.is_nan())
}

/// One `ip -d -s -j link show` entry → Iface (minus udev enrichment). None if not CAN.
pub fn parse_ip_link(l: &Value) -> Option<Iface> {
    if l.get("link_type").and_then(Value::as_str) != Some("can") {
        return None;
    }
    let empty = Value::Object(Map::new());
    let li = l.get("linkinfo").filter(|v| truthy(v)).unwrap_or(&empty);
    let d = li.get("info_data").filter(|v| truthy(v)).unwrap_or(&empty);
    let s = l.get("stats64").filter(|v| truthy(v)).unwrap_or(&empty);
    let kind = li.get("info_kind").filter(|v| truthy(v)).map(js_string).unwrap_or_else(|| "can".into());
    let ctrl = strings(d.get("ctrlmode")).unwrap_or_default();
    let ctrl_supported = strings(d.get("ctrlmode_supported")).map(|mut v| {
        v.extend(ctrl.iter().cloned());
        v
    });
    let has = |v: &[String], x: &str| v.iter().any(|c| c == x);
    let fd_capable = kind == "vcan"
        || kind == "vxcan"
        || d.get("data_bittiming_const").is_some_and(truthy)
        || d.get("data_bitrate_const").is_some_and(truthy)
        || ctrl_supported.as_ref().is_some_and(|c| has(c, "FD"))
        || has(&ctrl, "FD");
    let mtu = l.get("mtu").filter(|v| truthy(v)).cloned().unwrap_or(Value::from(16));
    let st = |dir: &str, k: &str| s.get(dir).and_then(|x| x.get(k)).filter(|v| !v.is_null()).cloned().unwrap_or(Value::from(0));
    let bt = d.get("bittiming");
    let dbt = d.get("data_bittiming");
    Some(Iface {
        name: l.get("ifname").map(js_string).unwrap_or_default(),
        driver: if kind == "can" { String::new() } else { kind.clone() },
        up: l.get("flags").and_then(Value::as_array).is_some_and(|f| f.iter().any(|x| x == "UP")),
        operstate: l.get("operstate").filter(|v| truthy(v)).map(js_string).unwrap_or_default(),
        fd_on: if kind == "can" { has(&ctrl, "FD") } else { l.get("mtu").and_then(Value::as_f64) == Some(72.0) },
        mtu,
        fd_capable,
        state: d.get("state").filter(|v| !v.is_null()).cloned(),
        berr: d.get("berr_counter").filter(|v| truthy(v)).map(|b| {
            let g = |k: &str| b.get(k).filter(|v| !v.is_null()).cloned().unwrap_or(Value::from(0));
            (g("tx"), g("rx"))
        }),
        restart_ms: num(d.get("restart_ms")),
        bitrate: nonzero(num(or(bt.and_then(|b| b.get("bitrate")), d.get("bittiming_bitrate")))),
        sample_point: num(bt.and_then(|b| b.get("sample_point"))),
        dbitrate: nonzero(num(or(dbt.and_then(|b| b.get("bitrate")), d.get("data_bittiming_bitrate")))),
        dsample_point: num(dbt.and_then(|b| b.get("sample_point"))),
        ctrl,
        ctrl_supported,
        clock: num(d.get("clock")),
        bitrate_const: d.get("bitrate_const").cloned(),
        xstats: li.get("info_xstats").cloned(),
        stats: Stats {
            rx_packets: st("rx", "packets"),
            tx_packets: st("tx", "packets"),
            rx_errors: st("rx", "errors"),
            tx_errors: st("tx", "errors"),
            rx_dropped: st("rx", "dropped"),
            tx_dropped: st("tx", "dropped"),
            rx_bytes: st("rx", "bytes"),
            tx_bytes: st("tx", "bytes"),
        },
        kind,
        key: String::new(),
        label: String::new(),
        usb: None,
        slcan_tty: None,
    })
}

/// A normalised config request (CanCfg); every field optional.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CanCfg {
    pub bitrate: Option<f64>,
    pub dbitrate: Option<f64>,
    pub restart_ms: Option<f64>,
    pub sample_point: Option<f64>,
    pub dsample_point: Option<f64>,
    pub fd: Option<bool>,
    pub listen_only: Option<bool>,
    pub loopback: Option<bool>,
    pub triple_sampling: Option<bool>,
    pub one_shot: Option<bool>,
    pub berr_reporting: Option<bool>,
    pub bring_up_on_plug: Option<bool>,
    pub up: Option<bool>,
}

const FLAGS: [&str; 8] = ["fd", "listenOnly", "loopback", "tripleSampling", "oneShot", "berrReporting", "bringUpOnPlug", "up"];

impl CanCfg {
    fn flag_mut(&mut self, k: &str) -> &mut Option<bool> {
        match k {
            "fd" => &mut self.fd,
            "listenOnly" => &mut self.listen_only,
            "loopback" => &mut self.loopback,
            "tripleSampling" => &mut self.triple_sampling,
            "oneShot" => &mut self.one_shot,
            "berrReporting" => &mut self.berr_reporting,
            "bringUpOnPlug" => &mut self.bring_up_on_plug,
            _ => &mut self.up,
        }
    }
    fn flag(&self, k: &str) -> Option<bool> {
        self.clone().flag_mut(k).to_owned()
    }

    /// As JSON, in normCfg's key order, without the unset fields.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        let n = |m: &mut Map<String, Value>, k: &str, v: Option<f64>| {
            if let Some(v) = v {
                m.insert(k.into(), benchbob_core::http::num(v));
            }
        };
        n(&mut m, "bitrate", self.bitrate);
        n(&mut m, "dbitrate", self.dbitrate);
        n(&mut m, "restartMs", self.restart_ms);
        n(&mut m, "samplePoint", self.sample_point);
        n(&mut m, "dsamplePoint", self.dsample_point);
        for k in FLAGS {
            if let Some(b) = self.flag(k) {
                m.insert(k.into(), Value::Bool(b));
            }
        }
        Value::Object(m)
    }
}

fn missing(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null)) || v.and_then(Value::as_str) == Some("")
}

/// Accept 0.875, 87.5 or "87.5%".
fn norm_sp(v: Option<&Value>) -> Result<Option<f64>, HttpError> {
    if missing(v) {
        return Ok(None);
    }
    let v = v.unwrap();
    let mut n = parse_float(&js_string(v)).unwrap_or(f64::NAN);
    if n > 1.0 {
        n /= 100.0;
    }
    if !(n > 0.0 && n < 1.0) {
        return Err(HttpError::bad_request(format!("sample point {} must be between 0 and 1 (or 0–100 %)", js_string(v))));
    }
    Ok(Some((n * 1000.0 + 0.5).floor() / 1000.0))
}

pub fn norm_cfg(x: &Value) -> Result<CanCfg, HttpError> {
    let get = |k: &str| x.as_object().and_then(|o| o.get(k));
    let int = |k: &str, min: f64, max: f64| -> Result<Option<f64>, HttpError> {
        if missing(get(k)) {
            return Ok(None);
        }
        let n = (js_number(get(k).unwrap()) + 0.5).floor();
        if !(n >= min && n <= max) {
            return Err(HttpError::bad_request(format!("{k} must be {min}–{max}")));
        }
        Ok(Some(n))
    };
    let mut c = CanCfg {
        bitrate: int("bitrate", 1000.0, 1_000_000.0)?,
        dbitrate: int("dbitrate", 1000.0, 15_000_000.0)?,
        restart_ms: int("restartMs", 0.0, 3_600_000.0)?,
        sample_point: norm_sp(get("samplePoint"))?,
        dsample_point: norm_sp(get("dsamplePoint"))?,
        ..Default::default()
    };
    for k in FLAGS {
        if let Some(v) = get(k).filter(|v| !v.is_null()) {
            *c.flag_mut(k) = Some(truthy(v));
        }
    }
    Ok(c)
}

const FLAG_ARGS: [(&str, &str, &str); 5] = [
    ("listenOnly", "listen-only", "LISTEN-ONLY"),
    ("loopback", "loopback", "LOOPBACK"),
    ("tripleSampling", "triple-sampling", "TRIPLE-SAMPLING"),
    ("oneShot", "one-shot", "ONE-SHOT"),
    ("berrReporting", "berr-reporting", "BERR-REPORTING"),
];

fn int_str(f: f64) -> String {
    js_string(&benchbob_core::http::num(f))
}

/// The `ip` argument lists (after `sudo ip`) that apply cfg to an interface. Up/down not
/// included.
pub fn ip_args(i: &Iface, c: &CanCfg) -> Result<Vec<Vec<String>>, HttpError> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    if i.kind != "can" {
        // vcan/vxcan have no bit timing; CAN FD is just the 72-byte MTU.
        return Ok(match c.fd {
            Some(fd) => vec![s(&["link", "set", "dev", &i.name, "mtu", if fd { "72" } else { "16" }])],
            None => vec![],
        });
    }
    let mut a: Vec<String> = Vec::new();
    let br = nonzero(c.bitrate);
    if let Some(b) = br {
        a.extend(["bitrate".into(), int_str(b)]);
    }
    if let (Some(_), Some(sp)) = (br, nonzero(c.sample_point)) {
        a.extend(["sample-point".into(), format!("{sp:.3}")]);
    }
    if c.fd == Some(true) && i.fd_capable {
        let db = nonzero(c.dbitrate).or(i.dbitrate).ok_or_else(|| HttpError::bad_request("CAN FD needs a data bitrate (dbitrate)"))?;
        a.extend(["dbitrate".into(), int_str(db)]);
        if let Some(dsp) = nonzero(c.dsample_point) {
            a.extend(["dsample-point".into(), format!("{dsp:.3}")]);
        }
        a.extend(["fd".into(), "on".into()]);
    } else if c.fd == Some(false) && i.fd_capable {
        a.extend(["fd".into(), "off".into()]);
    } else if c.fd == Some(true) && !i.fd_capable {
        return Err(HttpError::bad_request(format!("{} doesn't support CAN FD", i.name)));
    }
    for (k, arg, flag) in FLAG_ARGS {
        let Some(v) = c.flag(k) else { continue };
        // Only send flags the controller supports; "off" for an unsupported mode is a no-op anyway.
        if let Some(sup) = &i.ctrl_supported
            && !sup.iter().any(|x| x == flag)
        {
            if v {
                return Err(HttpError::bad_request(format!("{} doesn't support {arg}", i.name)));
            }
            continue;
        }
        a.extend([arg.to_string(), if v { "on" } else { "off" }.to_string()]);
    }
    if let Some(r) = c.restart_ms {
        a.extend(["restart-ms".into(), int_str(r)]);
    }
    if a.is_empty() {
        return Ok(vec![]);
    }
    let mut cmd = s(&["link", "set", "dev", &i.name, "type", "can"]);
    cmd.extend(a);
    Ok(vec![cmd])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Shaped like `ip -details -statistics -json link show` for a gs_usb FD adapter (key
    // names from iproute2 6.15 ip/iplink_can.c) and the real vcan0 output captured on desky.
    fn gs_usb() -> Value {
        json!({
            "ifname": "can0",
            "flags": ["NOARP", "UP", "LOWER_UP", "ECHO"],
            "mtu": 72,
            "operstate": "UP",
            "link_type": "can",
            "linkinfo": {
                "info_kind": "can",
                "info_data": {
                    "ctrlmode": ["FD", "BERR-REPORTING"],
                    "ctrlmode_supported": ["LOOPBACK", "LISTEN-ONLY", "TRIPLE-SAMPLING", "ONE-SHOT", "BERR-REPORTING", "FD", "CC-LEN8-DLC"],
                    "state": "ERROR-PASSIVE",
                    "berr_counter": { "tx": 128, "rx": 3 },
                    "restart_ms": 100,
                    "bittiming": { "bitrate": 500000, "sample_point": "0.875", "tq": 25, "prop_seg": 34, "phase_seg1": 35, "phase_seg2": 10, "sjw": 1, "brp": 2 },
                    "bittiming_const": { "name": "gs_usb", "tseg1": { "min": 1, "max": 256 }, "tseg2": { "min": 1, "max": 128 }, "sjw": { "min": 1, "max": 128 }, "brp": { "min": 1, "max": 512 }, "brp_inc": 1 },
                    "data_bittiming": { "bitrate": 2000000, "sample_point": "0.750" },
                    "data_bittiming_const": { "name": "gs_usb" },
                    "clock": 80000000,
                },
                "info_xstats": { "restarts": 1, "bus_error": 7, "arbitration_lost": 0, "error_warning": 2, "error_passive": 1, "bus_off": 0 },
            },
            "stats64": { "rx": { "bytes": 800, "packets": 100, "errors": 7, "dropped": 0 }, "tx": { "bytes": 80, "packets": 10, "errors": 0, "dropped": 0 } },
        })
    }

    fn vcan() -> Value {
        json!({ "ifindex": 4, "ifname": "vcan0", "flags": ["NOARP", "UP", "LOWER_UP"], "mtu": 72, "operstate": "UNKNOWN", "link_type": "can", "linkinfo": { "info_kind": "vcan" }, "stats64": { "rx": { "bytes": 5330, "packets": 923, "errors": 0, "dropped": 0 }, "tx": { "bytes": 5330, "packets": 923, "errors": 0, "dropped": 0 } } })
    }

    #[test]
    fn gs_usb_fd_adapter() {
        let i = parse_ip_link(&gs_usb()).unwrap();
        assert_eq!((i.name.as_str(), i.kind.as_str(), i.up, i.fd_capable, i.fd_on), ("can0", "can", true, true, true));
        assert_eq!(i.state, Some(json!("ERROR-PASSIVE")));
        assert_eq!(i.berr, Some((json!(128), json!(3))));
        assert_eq!((i.bitrate, i.sample_point, i.dbitrate, i.dsample_point), (Some(500000.0), Some(0.875), Some(2000000.0), Some(0.75)));
        assert_eq!((i.clock, i.restart_ms), (Some(80000000.0), Some(100.0)));
        assert_eq!(i.xstats.as_ref().unwrap()["bus_error"], 7);
        assert_eq!(i.stats.rx_packets, 100);
        // ctrlmode_supported is concatenated with ctrlmode, as the TypeScript does.
        assert_eq!(i.ctrl_supported.as_ref().unwrap().len(), 9);
    }

    #[test]
    fn vcan_link() {
        let i = parse_ip_link(&vcan()).unwrap();
        assert_eq!((i.name.as_str(), i.kind.as_str(), i.up, i.fd_on, i.fd_capable, i.bitrate), ("vcan0", "vcan", true, true, true, None));
        assert_eq!(i.driver, "vcan");
        // A fresh vcan on Linux 6.x has the CAN XL MTU: not FD as far as the panel goes.
        let mut v = vcan();
        v["mtu"] = json!(2060);
        assert!(!parse_ip_link(&v).unwrap().fd_on);
    }

    #[test]
    fn non_can_links_are_skipped() {
        assert!(parse_ip_link(&json!({ "ifname": "eth0", "link_type": "ether" })).is_none());
    }

    #[test]
    fn bitrate_fd_and_flags() {
        let can0 = parse_ip_link(&gs_usb()).unwrap();
        let c = norm_cfg(
            &json!({ "bitrate": 250000, "samplePoint": 87.5, "fd": true, "dbitrate": 4000000, "listenOnly": true, "restartMs": 0 }),
        )
        .unwrap();
        assert_eq!(
            ip_args(&can0, &c).unwrap(),
            vec![
                [
                    "link",
                    "set",
                    "dev",
                    "can0",
                    "type",
                    "can",
                    "bitrate",
                    "250000",
                    "sample-point",
                    "0.875",
                    "dbitrate",
                    "4000000",
                    "fd",
                    "on",
                    "listen-only",
                    "on",
                    "restart-ms",
                    "0"
                ]
                .map(String::from)
                .to_vec()
            ]
        );
    }

    #[test]
    fn fd_reuses_the_current_data_bitrate() {
        let can0 = parse_ip_link(&gs_usb()).unwrap();
        assert!(ip_args(&can0, &CanCfg { fd: Some(true), ..Default::default() }).unwrap()[0].contains(&"2000000".to_string()));
    }

    #[test]
    fn unsupported_flag_is_refused_when_on_skipped_when_off() {
        let mut c = parse_ip_link(&gs_usb()).unwrap();
        c.ctrl_supported = Some(vec!["LISTEN-ONLY".into()]);
        let e = ip_args(&c, &CanCfg { loopback: Some(true), ..Default::default() }).unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (400, "can0 doesn't support loopback"));
        assert!(ip_args(&c, &CanCfg { loopback: Some(false), ..Default::default() }).unwrap().is_empty());
    }

    #[test]
    fn vcan_only_takes_the_fd_mtu() {
        let v = parse_ip_link(&vcan()).unwrap();
        assert!(ip_args(&v, &CanCfg { bitrate: Some(500000.0), ..Default::default() }).unwrap().is_empty());
        assert_eq!(
            ip_args(&v, &CanCfg { fd: Some(false), ..Default::default() }).unwrap(),
            vec![["link", "set", "dev", "vcan0", "mtu", "16"].map(String::from).to_vec()]
        );
    }

    #[test]
    fn norm_cfg_validation() {
        assert_eq!(norm_cfg(&json!({ "bitrate": 5 })).unwrap_err().message, "bitrate must be 1000–1000000");
        assert_eq!(norm_cfg(&json!({ "samplePoint": 120 })).unwrap_err().message, "sample point 120 must be between 0 and 1 (or 0–100 %)");
        assert_eq!(
            norm_cfg(&json!({ "samplePoint": "0.8", "up": 1, "junk": 1 })).unwrap().to_json(),
            json!({ "samplePoint": 0.8, "up": true })
        );
        assert_eq!(norm_cfg(&json!({ "bitrate": "", "fd": null })).unwrap(), CanCfg::default());
        assert_eq!(norm_cfg(&json!({ "samplePoint": "87.5%" })).unwrap().sample_point, Some(0.875));
        // Key order is normCfg's, whatever the request's.
        let c = norm_cfg(&json!({ "up": false, "bitrate": 500000.4 })).unwrap().to_json();
        assert_eq!(serde_json::to_string(&c).unwrap(), r#"{"bitrate":500000,"up":false}"#);
    }
}
