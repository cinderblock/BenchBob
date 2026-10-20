//! The Raspberry Pi's boot-time buses: what is live in the device tree now, parsing a
//! stage request into [`Changes`], and the one-line summaries of a change
//! (peripherals/drivers/gpio.ts "interfaces").

use std::path::Path;

use benchbob_core::http::{HttpError, js_number, js_string};
use benchbob_core::run::read_text;
use serde_json::{Map, Value, json};

use crate::config_txt::{self as G, Changes, I2cGpioChange, Interfaces};

fn dt_text(p: &str) -> String {
    read_text(p).replace('\0', "")
}

/// A device-tree alias's node and whether its status is okay.
fn dt_status(alias: &str) -> Option<(String, bool)> {
    let path = dt_text(&format!("/proc/device-tree/aliases/{alias}"));
    if path.is_empty() {
        return None;
    }
    let st = dt_text(&format!("/proc/device-tree{path}/status"));
    Some((path, st.is_empty() || st == "okay"))
}

fn dt_clock(path: &str) -> Option<u32> {
    let b = std::fs::read(format!("/proc/device-tree{path}/clock-frequency")).ok()?;
    (b.len() >= 4).then(|| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn sorted_dir(p: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(p)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Pi 4 / 400 / CM4: the only boards the extra-controller overlays (i2c3–6, uart2–5) are offered on.
pub fn is_bcm2711() -> bool {
    match std::env::var("BENCHBOB_BOARD_BCM2711").ok().filter(|s| !s.is_empty()) {
        Some(v) => v == "1",
        None => dt_text("/proc/device-tree/compatible").contains("brcm,bcm2711"),
    }
}

pub fn live(cmdline_txt: &Path) -> Value {
    let bcm2711 = is_bcm2711();
    let i2c = dt_status("i2c1");
    let hz = i2c.as_ref().and_then(|(p, _)| dt_clock(p));
    let spi = dt_status("spi0");
    let spidev: Vec<String> = sorted_dir("/dev").into_iter().filter(|n| n.starts_with("spidev")).map(|n| format!("/dev/{n}")).collect();
    let serial0 = std::fs::read_link("/dev/serial0").ok().map(|p| p.to_string_lossy().into_owned());
    let cmdline = read_text("/proc/cmdline");
    let console_re = regex::Regex::new(r"console=([^,\s]+)").unwrap();
    let serial_console = console_re
        .captures_iter(&cmdline)
        .map(|c| c[1].to_string())
        .find(|c| c.starts_with("ttyS") || c.starts_with("ttyAMA") || c.starts_with("serial"));
    let boot_cmdline = read_text(cmdline_txt);
    let mut w1 = Vec::new();
    for n in sorted_dir("/proc/device-tree") {
        if !n.starts_with("onewire") {
            continue;
        }
        if let Ok(g) = std::fs::read(format!("/proc/device-tree/{n}/gpios"))
            && g.len() >= 8
        {
            w1.push(json!({ "node": n, "gpio": u32::from_be_bytes([g[4], g[5], g[6], g[7]]) }));
        }
    }
    let masters = sorted_dir("/sys/bus/w1/devices").into_iter().filter(|n| n.starts_with("w1_bus_master")).count();
    // Extra controllers: device-tree status (the overlay sets it "okay") and the node it made.
    let mut extra_i2c = Map::new();
    let mut extra_uart = Map::new();
    if bcm2711 {
        for x in &G::EXTRA_I2C {
            let s = dt_status(x.name);
            let dev = format!("/dev/i2c-{}", &x.name[3..]);
            let okay = s.as_ref().is_some_and(|s| s.1);
            extra_i2c.insert(
                x.name.into(),
                json!({ "enabled": okay, "dev": Path::new(&dev).exists().then_some(dev), "hz": if okay { s.as_ref().and_then(|s| dt_clock(&s.0)) } else { None } }),
            );
        }
        let ttys = G::onboard_uarts();
        for x in &G::EXTRA_UART {
            let s = dt_status(x.name);
            extra_uart.insert(
                x.name.into(),
                json!({ "enabled": s.is_some_and(|s| s.1), "tty": ttys.iter().find(|t| t.0 == x.name).map(|t| t.1.clone()) }),
            );
        }
    }
    let gpio_re = regex::Regex::new(r"^i2c@?\d*$").unwrap();
    let mut gpio_buses = Vec::new();
    for n in sorted_dir("/proc/device-tree") {
        if !gpio_re.is_match(&n) && !n.starts_with("i2c-gpio") {
            continue;
        }
        if read_text(format!("/proc/device-tree/{n}/compatible")).replace('\0', " ").contains("i2c-gpio") {
            gpio_buses.push(n);
        }
    }
    let note = match serial0.as_deref() {
        Some("ttyS0") => "serial0 is the mini-UART (ttyS0); the PL011 drives Bluetooth",
        Some("ttyAMA0") => "serial0 is the PL011 (ttyAMA0)",
        _ => "",
    };
    json!({
        "board": { "model": dt_text("/proc/device-tree/model"), "bcm2711": bcm2711 },
        "extraI2c": extra_i2c,
        "extraUart": extra_uart,
        "i2cGpio": { "nodes": gpio_buses },
        "i2c": { "enabled": i2c.as_ref().is_some_and(|s| s.1), "dev": Path::new("/dev/i2c-1").exists().then_some("/dev/i2c-1"), "hz": hz },
        "spi": { "enabled": spi.as_ref().is_some_and(|s| s.1), "devices": spidev },
        "uart": {
            "serial0": serial0.as_ref().map(|s| format!("/dev/{s}")),
            "consoleOnSerial": serial_console,
            "bootConsole": boot_cmdline.contains("console=serial0").then_some("serial0"),
            "note": note,
        },
        "onewire": { "nodes": w1, "masters": masters },
    })
}

fn bool_field(v: &Value, what: &str) -> Result<bool, HttpError> {
    v.as_bool().ok_or_else(|| HttpError::bad_request(format!("{what} must be true or false")))
}

fn obj<'a>(v: &'a Value, what: &str) -> Result<&'a Map<String, Value>, HttpError> {
    v.as_object().ok_or_else(|| HttpError::bad_request(format!("{what} must be an object")))
}

/// `null` stays null; anything else must be a finite number.
fn opt_num(v: &Value, what: &str) -> Result<Option<f64>, HttpError> {
    if v.is_null() {
        return Ok(None);
    }
    let n = js_number(v);
    if !n.is_finite() {
        return Err(HttpError::bad_request(format!("{what} must be a number")));
    }
    Ok(Some(n))
}

fn present(a: &Value, k: &str) -> Option<Value> {
    a.get(k).filter(|v| !v.is_null()).cloned()
}

/// A stage request body into config changes (gpio.ts parseChanges).
pub fn parse_changes(a: &Value) -> Result<Changes, HttpError> {
    let mut ch = Changes::default();
    if let Some(i) = a.get("i2c").filter(|v| benchbob_core::http::truthy(v)) {
        let mut en = None;
        let mut baud = None;
        if let Some(e) = present(i, "enabled") {
            en = Some(bool_field(&e, "i2c.enabled")?);
        }
        if let Some(b) = present(i, "baudrate") {
            let b = js_number(&b);
            if !(10000.0..=3_400_000.0).contains(&b) {
                return Err(HttpError::bad_request("i2c.baudrate must be 10000–3400000 Hz"));
            }
            baud = Some(b);
        }
        ch.i2c = Some((en, baud));
    }
    if let Some(e) = a.get("spi").and_then(|s| present(s, "enabled")) {
        ch.spi = Some(bool_field(&e, "spi.enabled")?);
    }
    if let Some(e) = a.get("uart").and_then(|s| present(s, "enabled")) {
        ch.uart = Some(bool_field(&e, "uart.enabled")?);
    }
    if let Some(o) = a.get("onewire").filter(|v| benchbob_core::http::truthy(v)) {
        let mut en = None;
        let mut pin = None;
        if let Some(e) = present(o, "enabled") {
            en = Some(bool_field(&e, "onewire.enabled")?);
        }
        if let Some(p) = present(o, "pin") {
            let p = js_number(&p);
            if !(p.fract() == 0.0 && (2.0..=27.0).contains(&p)) {
                return Err(HttpError::bad_request("onewire.pin must be GPIO2–27"));
            }
            pin = Some(p as i64);
        }
        ch.onewire = Some((en, pin));
    }
    if let Some(x) = a.get("extraI2c").filter(|v| benchbob_core::http::truthy(v)) {
        for (n, c) in obj(x, "extraI2c")? {
            obj(c, &format!("extraI2c.{n}"))?;
            let en = present(c, "enabled").map(|e| bool_field(&e, &format!("extraI2c.{n}.enabled"))).transpose()?;
            let pins = present(c, "pins").map(|p| js_string(&p));
            let baud = match present(c, "baudrate") {
                Some(b) => opt_num(&b, &format!("extraI2c.{n}.baudrate"))?,
                None => None,
            };
            ch.extra_i2c.push((n.clone(), en, pins, baud));
        }
    }
    if let Some(x) = a.get("extraUart").filter(|v| benchbob_core::http::truthy(v)) {
        for (n, c) in obj(x, "extraUart")? {
            obj(c, &format!("extraUart.{n}"))?;
            let en = present(c, "enabled").map(|e| bool_field(&e, &format!("extraUart.{n}.enabled"))).transpose()?;
            let cts = present(c, "ctsrts").map(|e| bool_field(&e, &format!("extraUart.{n}.ctsrts"))).transpose()?;
            ch.extra_uart.push((n.clone(), en, cts));
        }
    }
    if let Some(x) = a.get("i2cGpio").filter(|v| benchbob_core::http::truthy(v)) {
        let c = obj(x, "i2cGpio")?;
        let cv = Value::Object(c.clone());
        let mut g = I2cGpioChange::default();
        if let Some(e) = present(&cv, "enabled") {
            g.enabled = Some(bool_field(&e, "i2cGpio.enabled")?);
        }
        for k in ["sda", "scl"] {
            if let Some(v) = present(&cv, k) {
                let n = opt_num(&v, &format!("i2cGpio.{k}"))?;
                if k == "sda" {
                    g.sda = n;
                } else {
                    g.scl = n;
                }
            }
        }
        // bus / delayUs: present (even as null) means "set to this".
        for k in ["bus", "delayUs"] {
            if let Some(v) = c.get(k) {
                let n = opt_num(v, &format!("i2cGpio.{k}"))?;
                if k == "bus" {
                    g.bus = Some(n);
                } else {
                    g.delay_us = Some(n);
                }
            }
        }
        ch.i2c_gpio = Some(g);
    }
    Ok(ch)
}

fn onoff(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

fn khz(b: Option<i64>) -> String {
    let v = b.unwrap_or(100000) as f64 / 1000.0;
    if v.fract() == 0.0 { format!("{}", v as i64) } else { format!("{v}") }
}

/// One line per change, for the confirm dialog and the timeline (gpio.ts describe).
pub fn describe(before: &Interfaces, after: &Interfaces) -> Vec<String> {
    let mut s = Vec::new();
    if before.i2c.0 != after.i2c.0 {
        s.push(format!("I²C (GPIO2/3) {}", onoff(after.i2c.0)));
    }
    if before.i2c.1 != after.i2c.1 {
        s.push(format!("I²C speed {} kHz", khz(after.i2c.1)));
    }
    if before.spi != after.spi {
        s.push(format!("SPI0 {}", onoff(after.spi)));
    }
    if before.uart != after.uart {
        s.push(format!("UART (GPIO14/15) {}", onoff(after.uart)));
    }
    if before.onewire.0 != after.onewire.0 {
        s.push(format!("1-Wire {}", onoff(after.onewire.0)));
    }
    if after.onewire.0 && before.onewire.1 != after.onewire.1 {
        s.push(format!("1-Wire on GPIO{}", after.onewire.1.map(|p| p.to_string()).unwrap_or_else(|| "null".into())));
    }
    for (n, a) in after.extra_i2c.iter() {
        let b = before.extra_i2c.get(n).unwrap();
        let x = G::extra_i2c(n).unwrap();
        let pins = x.choice(&a.pins).map(|p| format!("GPIO{}/{}", p[0], p[1])).unwrap_or_default();
        if a.enabled != b.enabled {
            s.push(format!("{} ({pins}) {}", G::i2c_label(n), onoff(a.enabled)));
        } else if a.enabled && a.pins != b.pins {
            s.push(format!("{} moved to {pins}", G::i2c_label(n)));
        }
        if a.enabled && a.baudrate.unwrap_or(100000) != b.baudrate.unwrap_or(100000) {
            s.push(format!("{} speed {} kHz", G::i2c_label(n), khz(a.baudrate)));
        }
    }
    for (n, a) in after.extra_uart.iter() {
        let b = before.extra_uart.get(n).unwrap();
        let p = G::extra_uart(n).unwrap();
        if a.enabled != b.enabled {
            s.push(format!("{} (GPIO{}/{}) {}", n.to_uppercase(), p.tx, p.rx, onoff(a.enabled)));
        }
        if a.enabled && a.ctsrts != b.ctsrts {
            s.push(format!("{} CTS/RTS (GPIO{}/{}) {}", n.to_uppercase(), p.cts, p.rts, onoff(a.ctsrts)));
        }
    }
    let (ga, gb) = (&after.i2c_gpio, &before.i2c_gpio);
    if ga.enabled != gb.enabled {
        s.push(format!("software I²C (i2c-gpio) {}", onoff(ga.enabled)));
    }
    if ga.enabled && (ga.sda != gb.sda || ga.scl != gb.scl || ga.bus != gb.bus || ga.delay_us != gb.delay_us) {
        s.push(format!("i2c-gpio on SDA GPIO{} / SCL GPIO{}{}", ga.sda, ga.scl, ga.bus.map(|b| format!(", bus {b}")).unwrap_or_default()));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_txt::{edit_config, read_interfaces};

    #[test]
    fn stage_bodies_parse_and_validate_like_the_ts_driver() {
        let c = parse_changes(
            &json!({ "i2c": { "enabled": true, "baudrate": 400000 }, "spi": { "enabled": false }, "onewire": { "pin": "17" } }),
        )
        .unwrap();
        assert_eq!(c.i2c, Some((Some(true), Some(400000.0))));
        assert_eq!(c.spi, Some(false));
        assert_eq!(c.onewire, Some((None, Some(17))));
        assert!(parse_changes(&json!({ "i2c": { "baudrate": 5 } })).unwrap_err().message.contains("10000"));
        assert!(parse_changes(&json!({ "spi": { "enabled": "yes" } })).unwrap_err().message.contains("true or false"));
        assert!(parse_changes(&json!({ "onewire": { "pin": 30 } })).is_err());
        assert!(parse_changes(&json!({ "extraI2c": [1] })).unwrap_err().message.contains("object"));
        let g = parse_changes(&json!({ "i2cGpio": { "enabled": true, "bus": null } })).unwrap().i2c_gpio.unwrap();
        assert_eq!((g.enabled, g.bus), (Some(true), Some(None)));
    }

    #[test]
    fn summaries() {
        let base = "dtparam=i2c_arm=on\n";
        let before = read_interfaces(base);
        let next = edit_config(
            base,
            &parse_changes(&json!({ "i2c": { "baudrate": 400000 }, "extraUart": { "uart3": { "enabled": true } } })).unwrap(),
        )
        .unwrap();
        assert_eq!(describe(&before, &read_interfaces(&next)), ["I²C speed 400 kHz", "UART3 (GPIO4/5) on"]);
    }
}
