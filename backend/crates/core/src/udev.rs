//! The udev device database, loaded from `udevadm info --export-db` and kept live from
//! `udevadm monitor --udev --property`; every event goes to the drivers after the database
//! is updated (core.ts's hotplug monitor).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use indexmap::IndexMap;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// One device's properties: DEVPATH, SUBSYSTEM, DEVNAME, DEVTYPE, ID_*…
pub type Props = IndexMap<String, String>;

#[derive(Debug, Clone)]
pub struct UdevEvent {
    /// add, remove, change, bind, unbind, move…
    pub action: String,
    pub props: Props,
}

#[derive(Default)]
pub struct UdevDb {
    devices: RwLock<HashMap<String, Props>>,
}

/// Parse one block of `export-db` ("E: KEY=VAL", "P: /devices/…", "N: ttyUSB0") or monitor
/// ("KEY=VAL") lines.
pub fn parse_block(block: &str) -> Props {
    let mut p = Props::new();
    for l in block.lines() {
        let kv = l.strip_prefix("E: ").unwrap_or(l);
        if let Some((k, v)) = kv.split_once('=')
            && !k.is_empty()
            && k.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_' || c == b'.')
        {
            p.insert(k.to_string(), v.to_string());
            continue;
        }
        if let Some(d) = l.strip_prefix("P: ") {
            p.entry("DEVPATH".into()).or_insert_with(|| d.to_string());
        } else if let Some(n) = l.strip_prefix("N: ") {
            p.entry("DEVNAME".into()).or_insert_with(|| format!("/dev/{n}"));
        }
    }
    p
}

impl UdevDb {
    pub async fn load(&self) {
        let r = crate::run::run(&["udevadm", "info", "--export-db"], Default::default()).await;
        if r.code != 0 {
            tracing::warn!("udevadm info --export-db: {}", r.err);
        }
        let mut db = self.devices.write().unwrap();
        db.clear();
        for block in r.out.split("\n\n") {
            let p = parse_block(block);
            if let Some(d) = p.get("DEVPATH").cloned() {
                db.insert(d, p);
            }
        }
    }

    /// Every device, optionally filtered.
    pub fn devices(&self, filter: Option<&dyn Fn(&Props) -> bool>) -> Vec<Props> {
        let db = self.devices.read().unwrap();
        let mut v: Vec<Props> = db.values().filter(|p| filter.is_none_or(|f| f(p))).cloned().collect();
        v.sort_by(|a, b| a.get("DEVPATH").cmp(&b.get("DEVPATH")));
        v
    }

    /// The nearest ancestor of `devpath` with this subsystem (and devtype).
    pub fn ancestor(&self, devpath: &str, subsystem: &str, devtype: Option<&str>) -> Option<Props> {
        let db = self.devices.read().unwrap();
        let mut p = devpath;
        while let Some(i) = p.rfind('/') {
            p = &p[..i];
            if let Some(d) = db.get(p)
                && d.get("SUBSYSTEM").map(String::as_str) == Some(subsystem)
                && devtype.is_none_or(|t| d.get("DEVTYPE").map(String::as_str) == Some(t))
            {
                return Some(d.clone());
            }
        }
        None
    }

    /// Apply one monitor event (ACTION + DEVPATH + properties); tests feed fixtures this way.
    pub fn apply(&self, p: &Props) {
        let (Some(action), Some(devpath)) = (p.get("ACTION"), p.get("DEVPATH")) else { return };
        let mut db = self.devices.write().unwrap();
        if action == "remove" {
            db.remove(devpath);
        } else {
            let e = db.entry(devpath.clone()).or_default();
            for (k, v) in p {
                e.insert(k.clone(), v.clone());
            }
        }
    }

    /// Follow `udevadm monitor` until `stop` resolves, restarting it if it dies.
    pub async fn monitor(self: Arc<Self>, on_event: Arc<dyn Fn(UdevEvent) + Send + Sync>, mut stop: tokio::sync::watch::Receiver<bool>) {
        loop {
            let child = Command::new("udevadm")
                .args(["monitor", "--udev", "--property"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .spawn();
            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("udevadm monitor: {e}; hotplug events are off");
                    return;
                }
            };
            let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
            let mut block = String::new();
            loop {
                tokio::select! {
                    _ = stop.changed() => return,
                    l = lines.next_line() => match l {
                        Ok(Some(l)) if l.is_empty() => {
                            let p = parse_block(&block);
                            block.clear();
                            if p.contains_key("ACTION") && p.contains_key("DEVPATH") {
                                self.apply(&p);
                                on_event(UdevEvent { action: p["ACTION"].clone(), props: p });
                            }
                        }
                        Ok(Some(l)) => {
                            block.push_str(&l);
                            block.push('\n');
                        }
                        _ => break,
                    },
                }
            }
            // udevadm died (shouldn't happen): restart rather than silently losing hotplug.
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

/// udev's \x20-style escapes in ID_MODEL_ENC etc. (core.ts `udevDecode`), trimmed.
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

    #[test]
    fn usb_names() {
        let p: Props = [("ID_VENDOR_ENC", "Silicon\\x20Labs"), ("ID_MODEL", "CP2102_USB")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(usb_name(&p), ("Silicon Labs".to_string(), "CP2102_USB".to_string()));
    }

    #[test]
    fn export_db_and_monitor_blocks() {
        let p = parse_block("P: /devices/platform/x/tty/ttyUSB0\nN: ttyUSB0\nE: SUBSYSTEM=tty\nE: ID_MODEL=FT232R_USB_UART");
        assert_eq!(p["DEVPATH"], "/devices/platform/x/tty/ttyUSB0");
        assert_eq!(p["DEVNAME"], "/dev/ttyUSB0");
        assert_eq!(p["SUBSYSTEM"], "tty");
        let m = parse_block("UDEV  [123.4] add /devices/x (tty)\nACTION=add\nDEVPATH=/devices/x\nSUBSYSTEM=tty");
        assert_eq!(m["ACTION"], "add");
        assert!(!m.contains_key("UDEV  [123.4] add /devices/x (tty)"));
    }

    #[test]
    fn ancestors() {
        let db = UdevDb::default();
        for b in [
            "ACTION=add\nDEVPATH=/devices/usb1/1-1\nSUBSYSTEM=usb\nDEVTYPE=usb_device",
            "ACTION=add\nDEVPATH=/devices/usb1/1-1/1-1:1.0/ttyUSB0\nSUBSYSTEM=tty",
        ] {
            db.apply(&parse_block(b));
        }
        assert_eq!(db.ancestor("/devices/usb1/1-1/1-1:1.0/ttyUSB0", "usb", Some("usb_device")).unwrap()["DEVPATH"], "/devices/usb1/1-1");
        assert!(db.ancestor("/devices/usb1/1-1/1-1:1.0/ttyUSB0", "pci", None).is_none());
    }
}
