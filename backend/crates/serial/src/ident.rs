//! Device identification (serial.ts): known USB ids, which resets and flash tools fit a
//! board family, the DTR/RTS reset sequences, stable port ids.

use std::sync::OnceLock;

use benchbob_core::udev::Props;
use regex::Regex;

pub struct Known {
    pub vid: &'static str,
    pub pid: Option<&'static str>,
    pub name: &'static str,
    pub family: &'static str,
}

const fn k(vid: &'static str, pid: Option<&'static str>, name: &'static str, family: &'static str) -> Known {
    Known { vid, pid, name, family }
}

pub const KNOWN: &[Known] = &[
    k("303a", Some("1001"), "Espressif USB-JTAG/serial", "esp-usbjtag"),
    k("303a", None, "Espressif native USB", "esp"),
    k("10c4", Some("ea60"), "CP2102/CP2104", "bridge"),
    k("10c4", Some("ea70"), "CP2105 dual UART", "bridge"),
    k("10c4", Some("ea71"), "CP2108 quad UART", "bridge"),
    k("1a86", Some("7523"), "CH340", "bridge"),
    k("1a86", Some("5523"), "CH341", "bridge"),
    k("1a86", Some("55d4"), "CH9102", "bridge"),
    k("1a86", Some("55d3"), "CH343", "bridge"),
    k("1a86", Some("55d2"), "CH342 dual UART", "bridge"),
    k("0403", Some("6001"), "FTDI FT232R", "bridge"),
    k("0403", Some("6010"), "FTDI FT2232", "bridge"),
    k("0403", Some("6011"), "FTDI FT4232", "bridge"),
    k("0403", Some("6014"), "FTDI FT232H", "bridge"),
    k("0403", Some("6015"), "FTDI FT-X", "bridge"),
    k("067b", Some("2303"), "Prolific PL2303", "bridge"),
    k("067b", Some("23a3"), "Prolific PL2303GC", "bridge"),
    k("2341", Some("0043"), "Arduino Uno R3", "arduino"),
    k("2341", Some("0001"), "Arduino Uno", "arduino"),
    k("2341", Some("0010"), "Arduino Mega 2560", "arduino"),
    k("2341", Some("0042"), "Arduino Mega 2560 R3", "arduino"),
    k("2341", Some("0058"), "Arduino Nano Every", "arduino"),
    k("2341", Some("8036"), "Arduino Leonardo", "arduino-native"),
    k("2341", Some("8037"), "Arduino Micro", "arduino-native"),
    k("2341", Some("0069"), "Arduino Uno R4 Minima", "arduino-native"),
    k("2341", Some("1002"), "Arduino Uno R4 WiFi", "esp"),
    k("2341", None, "Arduino", "arduino"),
    k("2a03", None, "Arduino (arduino.org)", "arduino"),
    k("2e8a", Some("000c"), "Raspberry Pi Debug Probe UART", "cdc"),
    k("2e8a", Some("000a"), "Raspberry Pi Pico (SDK serial)", "pico"),
    k("2e8a", Some("0005"), "Raspberry Pi Pico (MicroPython)", "pico"),
    k("2e8a", None, "Raspberry Pi RP2040/RP2350", "pico"),
    k("0483", Some("5740"), "STM32 Virtual COM Port", "stm32"),
    k("0483", Some("374b"), "ST-LINK/V2-1 VCP", "stm32"),
    k("0483", Some("3752"), "ST-LINK/V2-1 VCP", "stm32"),
    k("0483", Some("374e"), "ST-LINK/V3 VCP", "stm32"),
    k("0483", Some("3753"), "ST-LINK/V3 VCP", "stm32"),
    k("1366", None, "SEGGER J-Link VCOM", "jlink"),
    k("1d50", Some("6018"), "Black Magic Probe", "cdc"),
    k("16c0", Some("0483"), "Teensy USB serial", "cdc"),
    k("0d28", Some("0204"), "DAPLink / micro:bit", "cdc"),
    k("239a", None, "Adafruit board", "arduino"),
];

pub fn usb_driver_name(d: &str) -> &str {
    match d {
        "ftdi_sio" => "FTDI",
        "cp210x" => "CP210x",
        "ch341" => "CH34x",
        "pl2303" => "PL2303",
        "cdc_acm" => "CDC-ACM",
        "ch343" => "CH343",
        other => other,
    }
}

pub fn identify(vid: &str, pid: &str) -> Option<&'static Known> {
    let (vid, pid) = (vid.to_lowercase(), pid.to_lowercase());
    KNOWN.iter().find(|k| k.vid == vid && k.pid == Some(pid.as_str())).or_else(|| KNOWN.iter().find(|k| k.vid == vid && k.pid.is_none()))
}

/// Reset helpers worth offering for a family, most useful first.
pub fn resets_for(f: &str) -> &'static [&'static str] {
    match f {
        "esp-usbjtag" | "esp" | "bridge" | "other" => &["esp", "esp-boot", "dtr"],
        "arduino" => &["dtr", "touch1200"],
        "arduino-native" | "pico" => &["touch1200"],
        "stm32" | "jlink" | "cdc" => &["dtr"],
        _ => &[],
    }
}

pub fn flash_tools_for(f: &str) -> &'static [&'static str] {
    match f {
        "arduino" => &["avrdude"],
        "esp-usbjtag" | "esp" => &["esptool"],
        "bridge" | "other" | "onboard" => &["esptool", "avrdude"],
        _ => &[],
    }
}

/// One step of a reset: set lines in order, or wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Dtr(bool),
    Rts(bool),
    Ms(u64),
}

use Step::{Dtr, Ms, Rts};

// Line sequences, in esptool's own order (a step like { rts: true, dtr: false } passes
// through RTS=DTR=1 the way esptool does). "Asserted" (true) pulls the adapter pin low. On
// the classic ESP dev board circuit DTR drives IO0 and RTS drives EN; with both asserted
// neither is pulled.
pub fn reset_seq(name: &str) -> &'static [Step] {
    match name {
        // esptool ClassicReset
        "esp-boot" => &[Dtr(false), Rts(true), Ms(100), Dtr(true), Rts(false), Ms(50), Dtr(false)],
        // esptool HardReset
        "esp" => &[Dtr(false), Rts(true), Ms(100), Rts(false)],
        // esptool USBJTAGSerialReset: the built-in USB-Serial/JTAG peripheral decodes line states, not pins.
        "esp-boot-usbjtag" => &[
            Rts(false),
            Dtr(false),
            Ms(100),
            Dtr(true),
            Rts(false),
            Ms(100),
            Rts(true),
            Dtr(false),
            Rts(true),
            Ms(100),
            Dtr(false),
            Rts(false),
        ],
        "esp-usbjtag" => &[Dtr(false), Rts(true), Ms(200), Rts(false), Ms(200)],
        // avrdude's arduino programmer: drop DTR/RTS, then raise them; the board's capacitor
        // turns the edge into a reset pulse.
        "dtr" => &[Dtr(false), Rts(false), Ms(250), Dtr(true), Rts(true), Ms(50)],
        _ => &[],
    }
}

pub fn reset_sequence(mode: &str, family: &str) -> &'static [Step] {
    if family == "esp-usbjtag" && mode == "esp-boot" {
        return reset_seq("esp-boot-usbjtag");
    }
    if family == "esp-usbjtag" && mode == "esp" {
        return reset_seq("esp-usbjtag");
    }
    reset_seq(mode)
}

fn non_alnum(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[^A-Za-z0-9.]+").unwrap()).replace_all(s, "-").into_owned()
}

/// A URL-safe id from udev's ID_PATH: "platform-fd500000.pcie-pci-0000:01:00.0-usb-0:1.3:1.0" -> "usb-0-1.3-1.0".
pub fn stable_id(p: &Props) -> String {
    static USB: OnceLock<Regex> = OnceLock::new();
    let get = |k: &str| p.get(k).map(String::as_str).unwrap_or("");
    let path = get("ID_PATH");
    if let Some(m) = USB.get_or_init(|| Regex::new(r"-usb-(\S+)$").unwrap()).captures(path) {
        return format!("usb-{}", m[1].replace(':', "-"));
    }
    if !path.is_empty() {
        return non_alnum(path.strip_prefix("platform-").unwrap_or(path));
    }
    if !get("ID_SERIAL").is_empty() {
        let iface = get("ID_USB_INTERFACE_NUM");
        return format!("sn-{}{}", non_alnum(get("ID_SERIAL")), if iface.is_empty() { String::new() } else { format!("-{iface}") });
    }
    let dev = if get("DEVNAME").is_empty() { "tty" } else { get("DEVNAME") };
    non_alnum(dev.strip_prefix("/dev/").unwrap_or(dev))
}

/// A manually added path's id.
pub fn manual_id(path: &str) -> String {
    format!("path{}", non_alnum(path))
}

/// udev's \x20-style escapes in ID_MODEL_ENC etc. (core.ts `udevDecode`).
pub fn udev_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1] == b'x'
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 2..i + 4]).unwrap_or("zz"), 16)
        {
            // String.fromCharCode: a code unit, i.e. Latin-1 for one byte.
            out.extend((v as char).to_string().as_bytes());
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).trim().to_string()
}

/// Vendor and product for display (core.ts `usbName`): the database names, else the
/// descriptor strings.
pub fn usb_name(p: &Props) -> (String, String) {
    let pick = |db: &str, enc: &str, raw: &str| {
        let get = |k: &str| p.get(k).cloned().unwrap_or_default();
        let d = get(db);
        if !d.is_empty() {
            return d;
        }
        let e = udev_decode(&get(enc));
        if !e.is_empty() {
            return e;
        }
        get(raw)
    };
    (pick("ID_VENDOR_FROM_DATABASE", "ID_VENDOR_ENC", "ID_VENDOR"), pick("ID_MODEL_FROM_DATABASE", "ID_MODEL_ENC", "ID_MODEL"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(kv: &[(&str, &str)]) -> Props {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn known_devices() {
        assert_eq!(identify("303A", "1001").unwrap().family, "esp-usbjtag");
        assert_eq!(identify("10c4", "ea60").unwrap().name, "CP2102/CP2104");
        assert_eq!(identify("1a86", "55d4").unwrap().name, "CH9102");
        assert_eq!(identify("2341", "0043").unwrap().name, "Arduino Uno R3");
        assert_eq!(identify("2341", "beef").unwrap().name, "Arduino");
        assert_eq!(identify("2e8a", "000a").unwrap().family, "pico");
        assert_eq!(identify("0483", "5740").unwrap().family, "stm32");
        assert_eq!(identify("1366", "0105").unwrap().name, "SEGGER J-Link VCOM");
        assert!(identify("dead", "beef").is_none());
        assert_eq!(resets_for("pico"), &["touch1200"]);
        assert_eq!(reset_sequence("esp-boot", "esp-usbjtag")[..2], [Rts(false), Dtr(false)]);
        assert_eq!(reset_sequence("esp-boot", "bridge"), &[Dtr(false), Rts(true), Ms(100), Dtr(true), Rts(false), Ms(50), Dtr(false)]);
        assert!(reset_sequence("nope", "bridge").is_empty());
    }

    #[test]
    fn stable_ids() {
        assert_eq!(stable_id(&props(&[("ID_PATH", "platform-fd500000.pcie-pci-0000:01:00.0-usb-0:1.3:1.0")])), "usb-0-1.3-1.0");
        assert_eq!(
            stable_id(&props(&[("ID_SERIAL", "Silicon_Labs_CP2102_0001"), ("ID_USB_INTERFACE_NUM", "00")])),
            "sn-Silicon-Labs-CP2102-0001-00"
        );
        assert_eq!(stable_id(&props(&[("DEVNAME", "/dev/ttyACM0")])), "ttyACM0");
        assert_eq!(stable_id(&props(&[("ID_PATH", "platform-fe201000.serial")])), "fe201000.serial");
        assert_eq!(manual_id("/dev/pts/3"), "path-dev-pts-3");
    }

    #[test]
    fn usb_names() {
        let p = props(&[("ID_VENDOR_ENC", "Silicon\\x20Labs"), ("ID_MODEL", "CP2102_USB")]);
        assert_eq!(usb_name(&p), ("Silicon Labs".to_string(), "CP2102_USB".to_string()));
    }
}
