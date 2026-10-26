//! BootBob servers on this network, found over mDNS (`_bootbob._tcp`). The dashboard
//! offers boot media for them: a USB boot stick the server builds for itself
//! (`<url>/files/usb/bootbob-usb.img`, flashed like any image) and a Raspberry Pi
//! bootloader card that makes a Pi netboot from the server (`BOOT_ORDER` and `TFTP_IP`).
//!
//! The browse starts the first time the dashboard asks, and the list then follows the
//! network: servers appear when they announce themselves and go when they say goodbye.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde_json::{Value, json};

pub const SERVICE: &str = "_bootbob._tcp.local.";

/// One BootBob server, as its announcement describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    /// The instance name, e.g. "BootBob on steamboat".
    pub name: String,
    /// Where machines and tools reach it, e.g. "http://10.255.0.5:8067".
    pub url: String,
    /// Its IPv4 address: what a Pi's `TFTP_IP` should be.
    pub ip: Option<String>,
    pub version: String,
    pub caps: Vec<String>,
}

impl Server {
    pub fn to_json(&self) -> Value {
        json!({ "name": self.name, "url": self.url, "ip": self.ip, "version": self.version, "caps": self.caps })
    }
}

/// The instance part of a full mDNS name: "BootBob on steamboat._bootbob._tcp.local." →
/// "BootBob on steamboat".
fn instance(fullname: &str) -> String {
    fullname.strip_suffix(SERVICE).map(|s| s.trim_end_matches('.')).unwrap_or(fullname).to_string()
}

/// The host in a URL, when it's an IPv4 address: "http://10.255.0.5:8067" → 10.255.0.5.
fn url_ipv4(url: &str) -> Option<String> {
    let host = url.split("://").nth(1)?.split(['/', ':']).next()?;
    host.parse::<std::net::Ipv4Addr>().ok().map(|a| a.to_string())
}

/// A server from its TXT records and addresses. `None` without a usable URL.
pub fn server(fullname: &str, txt: &dyn Fn(&str) -> Option<String>, ipv4: &[String]) -> Option<Server> {
    let url = txt("url").filter(|u| u.starts_with("http://") || u.starts_with("https://"))?;
    let ip = url_ipv4(&url).or_else(|| ipv4.first().cloned());
    Some(Server {
        name: instance(fullname),
        version: txt("version").unwrap_or_default(),
        caps: txt("caps").map(|c| c.split(',').filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default(),
        url,
        ip,
    })
}

/// The live list.
pub struct Servers {
    found: Arc<Mutex<BTreeMap<String, Server>>>,
    /// Kept so the browse keeps running.
    _daemon: Option<ServiceDaemon>,
}

impl Servers {
    /// Starts browsing. If mDNS can't start (no multicast here), the list just stays empty.
    pub fn start() -> Servers {
        let found = Arc::new(Mutex::new(BTreeMap::new()));
        let daemon = match ServiceDaemon::new().and_then(|d| d.browse(SERVICE).map(|rx| (d, rx))) {
            Ok((d, rx)) => {
                let found = found.clone();
                std::thread::Builder::new()
                    .name("bootbob-browse".into())
                    .spawn(move || {
                        while let Ok(ev) = rx.recv() {
                            match ev {
                                ServiceEvent::ServiceResolved(s) => {
                                    let txt = |k: &str| s.txt_properties.get_property_val_str(k).map(String::from);
                                    let mut v4: Vec<String> = s.get_addresses_v4().iter().map(|a| a.to_string()).collect();
                                    v4.sort();
                                    if let Some(srv) = server(&s.fullname, &txt, &v4) {
                                        found.lock().unwrap().insert(s.fullname.clone(), srv);
                                    }
                                }
                                ServiceEvent::ServiceRemoved(_, fullname) => {
                                    found.lock().unwrap().remove(&fullname);
                                }
                                _ => {}
                            }
                        }
                    })
                    .ok();
                Some(d)
            }
            Err(e) => {
                tracing::warn!("mDNS browse for BootBob servers didn't start: {e}");
                None
            }
        };
        Servers { found, _daemon: daemon }
    }

    pub fn list(&self) -> Vec<Server> {
        self.found.lock().unwrap().values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txt<'a>(m: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| m.iter().find(|(key, _)| *key == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn reads_an_announcement() {
        let t = [("url", "http://10.255.0.5:8067"), ("version", "0.1.0"), ("caps", "ipxe,pi,usb,nbd")];
        let s = server("BootBob on steamboat._bootbob._tcp.local.", &txt(&t), &["10.255.0.9".into()]).unwrap();
        assert_eq!(s.name, "BootBob on steamboat");
        assert_eq!(s.ip.as_deref(), Some("10.255.0.5"), "the URL's address wins over the A record");
        assert_eq!(s.caps, vec!["ipxe", "pi", "usb", "nbd"]);
        assert_eq!(s.to_json()["url"], "http://10.255.0.5:8067");

        // A name URL: the announced address is the TFTP_IP.
        let t = [("url", "http://pxe.tsl")];
        let s = server("x._bootbob._tcp.local.", &txt(&t), &["10.255.0.5".into()]).unwrap();
        assert_eq!(s.ip.as_deref(), Some("10.255.0.5"));
        assert!(s.caps.is_empty());

        // No URL, or not an HTTP one: not usable.
        assert!(server("x._bootbob._tcp.local.", &txt(&[]), &[]).is_none());
        assert!(server("x._bootbob._tcp.local.", &txt(&[("url", "file:///etc")]), &[]).is_none());
    }
}
