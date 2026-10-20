//! The system half of `/api/status` (server.ts "Monitoring", "System Info", "Network",
//! "Root Storage" and the history ring buffer): CPU, memory, temperature, interfaces and
//! their rates, the disk BenchBob runs from. Everything is sampled when a status is built,
//! as server.ts does, so the rates are over the time since the previous status.

use std::time::Instant;

use benchbob_core::http::{num, parse_int, round_to};
use benchbob_core::run::{Opts, read_text, run};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};

const HIST_N: usize = 30;

#[derive(Default)]
pub struct Sampler {
    prev_cpu: Option<(f64, f64)>,
    pub cpu_usage: f64,
    prev_disk: IndexMap<String, (f64, f64, Instant)>,
    pub disk_rates: IndexMap<String, (f64, f64)>,
    prev_net: IndexMap<String, (f64, f64, Instant)>,
    pub net_rates: IndexMap<String, (f64, f64)>,
    pub hist: IndexMap<String, Vec<Value>>,
    ram_mb: Option<u64>,
    root_disk: Option<RootDisk>,
}

#[derive(Clone)]
struct RootDisk {
    name: Option<String>,
    kind: String,
    size: u64,
    source: String,
    fstype: String,
}

fn ls_sorted(p: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(p)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

impl Sampler {
    pub fn push_hist(&mut self, key: &str, val: Value) {
        let h = self.hist.entry(key.to_string()).or_default();
        h.push(val);
        if h.len() > HIST_N {
            h.remove(0);
        }
    }

    pub fn history(&self) -> Value {
        Value::Object(self.hist.iter().map(|(k, v)| (k.clone(), Value::Array(v.clone()))).collect())
    }

    /// CPU usage since the last sample, disk and network byte rates.
    pub fn sample_io(&mut self) {
        let now = Instant::now();
        let stat = read_text("/proc/stat");
        if let Some(line) = stat.lines().next() {
            let f: Vec<f64> = line.split_whitespace().skip(1).map(|x| x.parse().unwrap_or(f64::NAN)).collect();
            let total: f64 = f.iter().sum();
            let idle = f.get(3).copied().unwrap_or(f64::NAN) + f.get(4).copied().filter(|x| *x != 0.0 && !x.is_nan()).unwrap_or(0.0);
            if let Some((pt, pi)) = self.prev_cpu {
                let (dt, di) = (total - pt, idle - pi);
                self.cpu_usage = if dt > 0.0 { (dt - di) / dt * 100.0 } else { 0.0 };
            }
            self.prev_cpu = Some((total, idle));
        }
        for name in ls_sorted("/sys/block") {
            if name.starts_with("loop") || name.starts_with("zram") || name.starts_with("ram") {
                continue;
            }
            let stat = read_text(format!("/sys/block/{name}/stat"));
            if stat.is_empty() {
                continue;
            }
            let f: Vec<&str> = stat.split_whitespace().collect();
            let rs = f.get(2).and_then(|x| parse_int(x)).unwrap_or(0) as f64;
            let ws = f.get(6).and_then(|x| parse_int(x)).unwrap_or(0) as f64;
            if let Some((pr, pw, pt)) = self.prev_disk.get(&name) {
                let dt = now.duration_since(*pt).as_secs_f64();
                if dt > 0.0 {
                    self.disk_rates.insert(name.clone(), (((rs - pr) * 512.0 / dt).max(0.0), ((ws - pw) * 512.0 / dt).max(0.0)));
                }
            }
            self.prev_disk.insert(name, (rs, ws, now));
        }
        for name in ls_sorted("/sys/class/net") {
            if name == "lo" {
                continue;
            }
            let rx = parse_int(&read_text(format!("/sys/class/net/{name}/statistics/rx_bytes"))).unwrap_or(0) as f64;
            let tx = parse_int(&read_text(format!("/sys/class/net/{name}/statistics/tx_bytes"))).unwrap_or(0) as f64;
            if let Some((pr, pt0, pt)) = self.prev_net.get(&name) {
                let dt = now.duration_since(*pt).as_secs_f64();
                if dt > 0.0 {
                    self.net_rates.insert(name.clone(), (((rx - pr) / dt).max(0.0), ((tx - pt0) / dt).max(0.0)));
                }
            }
            self.prev_net.insert(name, (rx, tx, now));
        }
        let cpu = round_to(self.cpu_usage, 1);
        self.push_hist("cpu", num(cpu));
        for (n, (r, w)) in self.disk_rates.clone() {
            self.push_hist(&format!("drv:{n}:r"), num(r));
            self.push_hist(&format!("drv:{n}:w"), num(w));
        }
        for (n, (r, t)) in self.net_rates.clone() {
            self.push_hist(&format!("net:{n}:rx"), num(r));
            self.push_hist(&format!("net:{n}:tx"), num(t));
        }
    }

    /// Installed RAM from the board revision code (bits 20-22 of a new-style code), since
    /// MemTotal is short by whatever the GPU reserved. Old-style codes: round MemTotal up.
    fn ram_mb(&mut self) -> u64 {
        if let Some(r) = self.ram_mb {
            return r;
        }
        let cpuinfo = read_text("/proc/cpuinfo");
        let rev = Regex::new(r"(?im)^Revision\s*:\s*([0-9a-f]+)")
            .unwrap()
            .captures(&cpuinfo)
            .and_then(|c| u64::from_str_radix(&c[1], 16).ok())
            .unwrap_or(0);
        let mb = if rev & 0x80_0000 != 0 {
            256u64 << ((rev >> 20) & 7)
        } else {
            let kb = Regex::new(r"(?m)^MemTotal:\s+(\d+)")
                .unwrap()
                .captures(&read_text("/proc/meminfo"))
                .and_then(|c| c[1].parse::<f64>().ok())
                .unwrap_or(0.0);
            if kb > 0.0 { 2f64.powf((kb / 1024.0).log2().ceil()) as u64 } else { 0 }
        };
        self.ram_mb = Some(mb);
        mb
    }

    pub fn system(&mut self, model: &str, temp: f64) -> Value {
        let mut mem: IndexMap<String, f64> = IndexMap::new();
        for line in read_text("/proc/meminfo").lines() {
            let mut it = line.splitn(2, ':');
            if let (Some(k), Some(v)) = (it.next(), it.next())
                && let Some(n) = v.split_whitespace().next().and_then(|x| x.parse::<f64>().ok())
                && k.chars().all(|c| c.is_alphanumeric() || c == '_')
            {
                mem.insert(k.to_string(), n);
            }
        }
        let cores = read_text("/proc/cpuinfo").matches("processor").count();
        let total = mem.get("MemTotal").copied().unwrap_or(0.0);
        let avail = mem.get("MemAvailable").copied().unwrap_or(0.0);
        let pct = if total > 0.0 { round_to((total - avail) / total * 100.0, 1) } else { 0.0 };
        json!({
            "model": model,
            "ramMB": self.ram_mb(),
            "cores": if cores == 0 { 4 } else { cores },
            "cpuUsage": num(round_to(self.cpu_usage, 1)),
            "cpuTemp": num(round_to(temp, 1)),
            "memTotal": num(total),
            "memAvailable": num(avail),
            "memPercent": num(pct),
        })
    }

    pub async fn network(&self) -> Value {
        let (addr, wifi, route4, route6) = tokio::join!(
            run(&["ip", "-j", "addr", "show"], Opts::default()),
            wifi_info(),
            run(&["ip", "-j", "route", "show", "default"], Opts::default()),
            run(&["ip", "-j", "-6", "route", "show", "default"], Opts::default()),
        );
        let mut gw: IndexMap<String, (String, Value)> = IndexMap::new();
        if let Ok(Value::Array(rs)) = serde_json::from_str::<Value>(&route4.out) {
            for r in rs {
                if let Some(dev) = r.get("dev").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    let g = r.get("gateway").and_then(Value::as_str).unwrap_or("").to_string();
                    gw.insert(dev.into(), (g, r.get("metric").cloned().unwrap_or(Value::Null)));
                }
            }
        }
        let mut gw6: IndexMap<String, String> = IndexMap::new();
        if let Ok(Value::Array(rs)) = serde_json::from_str::<Value>(&route6.out) {
            for r in rs {
                if let (Some(d), Some(g)) = (r.get("dev").and_then(Value::as_str), r.get("gateway").and_then(Value::as_str))
                    && !d.is_empty()
                    && !g.is_empty()
                {
                    gw6.insert(d.into(), g.into());
                }
            }
        }
        let Ok(Value::Array(ifs)) = serde_json::from_str::<Value>(&addr.out) else { return json!([]) };
        let mut out: Vec<(f64, Value)> = Vec::new();
        for i in ifs {
            let name = i.get("ifname").and_then(Value::as_str).unwrap_or("");
            if name == "lo" {
                continue;
            }
            let infos = i.get("addr_info").and_then(Value::as_array).cloned().unwrap_or_default();
            let inet = infos.iter().find(|a| a.get("family").and_then(Value::as_str) == Some("inet"));
            let v6: Vec<Value> = infos
                .iter()
                .filter(|a| a.get("family").and_then(Value::as_str) == Some("inet6"))
                .map(|a| Value::String(format!("{}/{}", js(a.get("local")), js(a.get("prefixlen")))))
                .collect();
            let r = self.net_rates.get(name);
            let g = gw.get(name);
            let mut m = Map::new();
            m.insert("name".into(), i.get("ifname").cloned().unwrap_or(Value::Null));
            m.insert("ip".into(), or_null(inet.and_then(|a| a.get("local"))));
            m.insert("prefix".into(), or_null(inet.and_then(|a| a.get("prefixlen"))));
            if let Some(mac) = i.get("address") {
                m.insert("mac".into(), mac.clone());
            }
            m.insert("mtu".into(), or_null(i.get("mtu")));
            if let Some(st) = i.get("operstate") {
                m.insert("state".into(), st.clone());
            }
            let metric = g.map(|x| x.1.clone()).unwrap_or(Value::Null);
            m.insert("metric".into(), metric.clone());
            m.insert("gateway".into(), g.map(|x| x.0.clone()).filter(|s| !s.is_empty()).map(Value::String).unwrap_or(Value::Null));
            m.insert("gateway6".into(), gw6.get(name).cloned().map(Value::String).unwrap_or(Value::Null));
            m.insert("ipv6".into(), Value::Array(v6));
            m.insert("rxRate".into(), num(r.map(|x| x.0).unwrap_or(0.0)));
            m.insert("txRate".into(), num(r.map(|x| x.1).unwrap_or(0.0)));
            if name == "wlan0" {
                m.insert("wifi".into(), wifi.clone());
            }
            out.push((metric.as_f64().unwrap_or(9999.0), Value::Object(m)));
        }
        // Stable, as Array.prototype.sort is.
        out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        Value::Array(out.into_iter().map(|x| x.1).collect())
    }

    /// What this machine runs from. Found from the / mount rather than by guessing the SD
    /// card, so NVMe, USB and netboot hosts are described (and protected) correctly.
    pub async fn root_storage(&mut self) -> Value {
        let r = run(&["findmnt", "-n", "-J", "-b", "-o", "SOURCE,FSTYPE,SIZE,USE%", "/"], Opts::default()).await;
        let Some(fs) = serde_json::from_str::<Value>(&r.out).ok().and_then(|v| v.get("filesystems")?.get(0).cloned()) else {
            return Value::Null;
        };
        let source = fs.get("source").and_then(Value::as_str).unwrap_or("").to_string();
        let fstype = fs.get("fstype").and_then(Value::as_str).unwrap_or("").to_string();
        if self.root_disk.as_ref().is_none_or(|d| d.source != source) {
            let mut d = RootDisk { name: None, kind: fstype.clone(), size: 0, source: source.clone(), fstype: fstype.clone() };
            if fstype.starts_with("nfs") {
                d.kind = "Netboot".into();
            } else if let Some(dev) = source.strip_prefix("/dev/") {
                let parent = run(&["lsblk", "-no", "PKNAME", &source], Opts::default()).await.out.trim().to_string();
                let name = if parent.is_empty() { dev.to_string() } else { parent };
                let info = run(&["lsblk", "-dn", "-J", "-b", "-o", "NAME,TRAN,SIZE", &format!("/dev/{name}")], Opts::default()).await;
                let bd =
                    serde_json::from_str::<Value>(&info.out).ok().and_then(|v| v.get("blockdevices")?.get(0).cloned()).unwrap_or(json!({}));
                d.size = bd.get("size").and_then(|s| s.as_u64().or_else(|| s.as_str().and_then(|x| x.parse().ok()))).unwrap_or(0);
                let tran = bd.get("tran").and_then(Value::as_str).unwrap_or("");
                if name.starts_with("nvme") || tran == "nvme" {
                    d.kind = "NVMe".into();
                } else if name.starts_with("mmcblk") {
                    d.kind = if read_text(format!("/sys/block/{name}/device/type")) == "MMC" { "eMMC" } else { "microSD" }.into();
                } else if tran == "usb" {
                    d.kind = "USB".into();
                } else if !tran.is_empty() {
                    d.kind = tran.to_uppercase();
                }
                d.name = Some(name);
            }
            self.root_disk = Some(d);
        }
        let d = self.root_disk.clone().unwrap();
        let use_pct = fs.get("use%").map(benchbob_core::http::js_string).and_then(|s| parse_int(&s)).unwrap_or(0);
        json!({ "name": d.name, "kind": d.kind, "size": d.size, "source": d.source, "fstype": d.fstype, "usePercent": use_pct })
    }

    /// The root disk's kernel name, once known (drives must never offer it).
    pub fn root_disk_name(&self) -> Option<String> {
        self.root_disk.as_ref().and_then(|d| d.name.clone())
    }
}

fn js(v: Option<&Value>) -> String {
    v.map(benchbob_core::http::js_string).unwrap_or_else(|| "undefined".into())
}

/// `x || null`
fn or_null(v: Option<&Value>) -> Value {
    match v {
        Some(x) if benchbob_core::http::truthy(x) => x.clone(),
        _ => Value::Null,
    }
}

async fn wifi_info() -> Value {
    let r = run(&["iw", "dev", "wlan0", "link"], Opts::default()).await;
    if r.code != 0 || r.out.contains("Not connected") {
        return Value::Null;
    }
    let cap = |re: &str| Regex::new(re).unwrap().captures(&r.out).map(|c| c[1].to_string());
    json!({
        "ssid": cap(r"SSID:\s*(.+)").unwrap_or_default(),
        "signal": cap(r"signal:\s*(-?\d+)").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
        "bitrate": cap(r"tx bitrate:\s*(.+)").unwrap_or_default(),
    })
}

pub fn hostname() -> String {
    read_text("/etc/hostname")
}

pub fn uptime() -> Value {
    num(benchbob_core::http::parse_float(read_text("/proc/uptime").split(' ').next().unwrap_or("")).unwrap_or(0.0))
}
