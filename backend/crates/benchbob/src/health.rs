//! Host health: the generic sysfs readings (thermal zones, hwmon, power supplies) every
//! Linux box has, the Raspberry Pi firmware's throttling flags and PMIC (vcgencmd), and a
//! board script for anything else (peripherals/lib/health.ts, boards/rpi.ts).

use std::cmp::Ordering;
use std::path::Path;
use std::time::{Duration, Instant};

use benchbob_core::http::{num, round_to};
use benchbob_core::run::{Opts, run};
use regex::Regex;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sensors {
    pub temps: Vec<(String, String, f64)>,
    pub volts: Vec<(String, String, f64)>,
    pub fans: Vec<(String, String, f64)>,
    pub supplies: Vec<Supply>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Supply {
    pub name: String,
    pub kind: String,
    pub online: Option<bool>,
    pub status: Option<String>,
    pub capacity: Option<f64>,
    pub volts: Option<f64>,
}

impl Sensors {
    pub fn to_json(&self) -> Value {
        let tri = |v: &[(String, String, f64)], k: &str| -> Vec<Value> {
            v.iter()
                .map(|(s, l, x)| {
                    let mut m = Map::new();
                    m.insert("source".into(), s.clone().into());
                    m.insert("label".into(), l.clone().into());
                    m.insert(k.into(), num(*x));
                    Value::Object(m)
                })
                .collect()
        };
        json!({
            "temps": tri(&self.temps, "c"),
            "volts": tri(&self.volts, "v"),
            "fans": tri(&self.fans, "rpm"),
            "supplies": self.supplies.iter().map(|p| json!({
                "name": p.name, "type": p.kind, "online": p.online, "status": p.status,
                "capacity": p.capacity.map(num), "volts": p.volts.map(num),
            })).collect::<Vec<_>>(),
        })
    }

    /// The script contract's shape back into Sensors (missing lists are empty).
    pub fn from_json(v: &Value) -> Self {
        let tri = |k: &str, f: &str| -> Vec<(String, String, f64)> {
            v.get(k)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|e| {
                            Some((
                                e.get("source")?.as_str()?.to_string(),
                                e.get("label").and_then(Value::as_str).unwrap_or("").to_string(),
                                e.get(f)?.as_f64()?,
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let supplies = v
            .get("supplies")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        Some(Supply {
                            name: e.get("name")?.as_str()?.to_string(),
                            kind: e.get("type").and_then(Value::as_str).unwrap_or("").into(),
                            online: e.get("online").and_then(Value::as_bool),
                            status: e.get("status").and_then(Value::as_str).map(Into::into),
                            capacity: e.get("capacity").and_then(Value::as_f64),
                            volts: e.get("volts").and_then(Value::as_f64),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Sensors { temps: tri("temps", "c"), volts: tri("volts", "v"), fans: tri("fans", "rpm"), supplies }
    }
}

fn rd(p: &Path) -> String {
    std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Directory entries in natural order (`localeCompare(…, { numeric: true })`).
fn ls(p: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(p)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort_by(|a, b| natural_cmp(a, b));
    v
}

/// Digit runs compare as numbers, the rest case-insensitively (then by case).
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    fn chunks(s: &str) -> Vec<(bool, &str)> {
        let mut out = Vec::new();
        let mut start = 0;
        let b = s.as_bytes();
        for i in 1..=b.len() {
            if i == b.len() || b[i].is_ascii_digit() != b[start].is_ascii_digit() {
                out.push((b[start].is_ascii_digit(), &s[start..i]));
                start = i;
            }
        }
        out
    }
    let (ca, cb) = (chunks(a), chunks(b));
    for (x, y) in ca.iter().zip(cb.iter()) {
        let o = match (x.0, y.0) {
            (true, true) => x.1.parse::<u128>().unwrap_or(0).cmp(&y.1.parse::<u128>().unwrap_or(0)),
            _ => x.1.to_lowercase().cmp(&y.1.to_lowercase()),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    ca.len().cmp(&cb.len()).then_with(|| a.cmp(b))
}

/// `s === "" || isNaN(Number(s)) ? null : Number(s)`
fn number(s: &str) -> Option<f64> {
    if s.is_empty() { None } else { s.parse::<f64>().ok().filter(|f| f.is_finite()) }
}

fn norm(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| !matches!(c, '-' | '_') && !c.is_whitespace()).collect()
}

/// Read every sensor under `root` (normally /sys/class).
pub fn read_sensors(root: &Path) -> Sensors {
    let mut s = Sensors::default();
    let mut zone_types = std::collections::HashSet::new();
    for z in ls(&root.join("thermal")) {
        if !z.starts_with("thermal_zone") {
            continue;
        }
        let ty = rd(&root.join("thermal").join(&z).join("type"));
        let Some(t) = number(&rd(&root.join("thermal").join(&z).join("temp"))) else { continue };
        zone_types.insert(norm(&ty));
        let label = if ty.is_empty() { z.clone() } else { ty };
        s.temps.push((z, label, round_to(t / 100.0, 0) / 10.0));
    }
    let re = Regex::new(r"^(temp|in|fan)(\d+)_input$").unwrap();
    for h in ls(&root.join("hwmon")) {
        let dir = root.join("hwmon").join(&h);
        let name = Some(rd(&dir.join("name"))).filter(|s| !s.is_empty()).unwrap_or_else(|| h.clone());
        for f in ls(&dir) {
            let Some(m) = re.captures(&f) else { continue };
            let Some(v) = number(&rd(&dir.join(&f))) else { continue };
            let (kind, n) = (&m[1], &m[2]);
            let label = Some(rd(&dir.join(format!("{kind}{n}_label")))).filter(|s| !s.is_empty()).unwrap_or_else(|| format!("{kind}{n}"));
            // A thermal zone that is also registered as hwmon (cpu-thermal / cpu_thermal) is listed once.
            if kind == "temp" && zone_types.contains(&norm(&name)) {
                continue;
            }
            match kind {
                "temp" => s.temps.push((name.clone(), label, round_to(v / 100.0, 0) / 10.0)),
                "in" => s.volts.push((name.clone(), label, round_to(v, 0) / 1000.0)),
                _ => s.fans.push((name.clone(), label, v)),
            }
        }
    }
    for p in ls(&root.join("power_supply")) {
        let mut u = std::collections::HashMap::new();
        for l in rd(&root.join("power_supply").join(&p).join("uevent")).lines() {
            if let Some(rest) = l.strip_prefix("POWER_SUPPLY_")
                && let Some((k, v)) = rest.split_once('=')
                && !k.is_empty()
                && k.bytes().all(|c| c.is_ascii_uppercase() || c == b'_')
            {
                u.insert(k.to_string(), v.to_string());
            }
        }
        let uv = u.get("VOLTAGE_NOW").and_then(|v| number(v));
        s.supplies.push(Supply {
            name: p,
            kind: u.get("TYPE").cloned().unwrap_or_default(),
            online: u.get("ONLINE").map(|v| v == "1"),
            status: u.get("STATUS").filter(|v| !v.is_empty()).cloned(),
            capacity: u.get("CAPACITY").and_then(|v| number(v)),
            volts: uv.map(|v| round_to(v / 10000.0, 0) / 100.0),
        });
    }
    s
}

/// The temperature to show as "CPU", or None if the box has no sensors (WSL, most VMs):
/// package-level CPU sensors first, then the SoC's zone, then anything.
pub fn cpu_temp(s: &Sensors) -> Option<f64> {
    let hw = [r"^coretemp$", r"^k10temp$", r"^zenpower$", r"^cpu.?thermal$", r"^soc.?thermal$"];
    let label = Regex::new(r"(?i)^(package id 0|tctl|tdie|cpu)").unwrap();
    for p in hw {
        let re = Regex::new(p).unwrap();
        let ts: Vec<&(String, String, f64)> = s.temps.iter().filter(|t| re.is_match(&t.0)).collect();
        if let Some(first) = ts.first() {
            return Some(ts.iter().find(|t| label.is_match(&t.1)).unwrap_or(first).2);
        }
    }
    let zone = Regex::new(r"(?i)x86_pkg_temp|cpu|soc|package|bigcore|littlecore").unwrap();
    s.temps.iter().find(|t| t.0.starts_with("thermal_zone") && zone.is_match(&t.1)).or(s.temps.first()).map(|t| t.2)
}

// ---------- Raspberry Pi host power (vcgencmd) ----------
//
// A sagging supply on a Pi is worth shouting about: USB drives (and any Compute Module
// powered from our ports) hang off the same 5 V rail, and under-voltage shows up as flaky
// writes, dropped disks and corrupted flashes long before anything else.

const THROTTLE_FLAGS: [(u32, &str); 4] =
    [(0, "under-voltage"), (1, "ARM frequency capped"), (2, "throttled"), (3, "soft temperature limit")];

#[derive(Default)]
pub struct RpiPower {
    uv_at: Option<Instant>,
    uv_times: Vec<f64>,
    ext5v_unsupported: bool,
}

impl RpiPower {
    pub async fn read(&mut self) -> Value {
        let r = run(&["vcgencmd", "get_throttled"], Opts::default()).await;
        let re = Regex::new(r"(?i)throttled=(0x[0-9a-f]+)").unwrap();
        let Some(m) = re.captures(&r.out) else { return Value::Null };
        if r.code != 0 {
            return Value::Null;
        }
        let raw = m[1].to_string();
        let v = u32::from_str_radix(&raw[2..], 16).unwrap_or(0);
        let now: Vec<&str> = THROTTLE_FLAGS.iter().filter(|(b, _)| v & (1 << b) != 0).map(|(_, n)| *n).collect();
        let since: Vec<&str> = THROTTLE_FLAGS.iter().filter(|(b, _)| v & (1 << (b + 16)) != 0).map(|(_, n)| *n).collect();
        // get_throttled only says "it happened since boot". The firmware hwmon driver logs
        // every episode, which gives a count and how recent the last one was. Re-read at
        // most every 30 s, and only once there is something to find.
        if v & (1 << 16) != 0 && self.uv_at.is_none_or(|t| t.elapsed() > Duration::from_secs(30)) {
            self.uv_at = Some(Instant::now());
            let j =
                run(&["sudo", "journalctl", "-k", "-b", "-o", "short-unix", "--no-pager", "-g", "Undervoltage detected"], Opts::default())
                    .await;
            self.uv_times =
                j.out.lines().filter_map(|l| benchbob_core::http::parse_float(l).map(|t| t * 1000.0)).filter(|t| *t > 0.0).collect();
        }
        // Pi 5 can read its 5 V input directly; older boards can't ("Command not registered").
        let mut input = Value::Null;
        if !self.ext5v_unsupported {
            let r = run(&["vcgencmd", "pmic_read_adc", "EXT5V_V"], Opts::default()).await;
            match Regex::new(r"=([\d.]+)V").unwrap().captures(&r.out).and_then(|c| c[1].parse::<f64>().ok()) {
                Some(f) => input = num(round_to(f, 2)),
                None => self.ext5v_unsupported = true,
            }
        }
        json!({
            "raw": raw,
            "now": now,
            "sinceBoot": since,
            "undervoltageEvents": self.uv_times.len(),
            "lastUndervoltage": self.uv_times.last().map(|t| num(*t)),
            "inputVolts": input,
        })
    }
}

// ---------- the board's health source ----------

pub enum Source {
    Sysfs,
    Vcgencmd(Mutex<RpiPower>),
    /// A board script: `<script> read` prints `{ temps, volts, fans, supplies, cpuTemp?, power? }`.
    Script(String),
}

/// One reading: the sensors, a script's `power`, a script's `cpuTemp`.
pub type Reading = (Sensors, Value, Option<f64>);

/// Cached readings, refreshed at most every 5 s (the status tick is every 2 s).
pub struct Health {
    pub source: Source,
    root: std::path::PathBuf,
    cache: Mutex<Option<(Instant, Reading)>>,
}

impl Health {
    pub fn new(kind: &benchbob_boards::Health) -> Self {
        let source = match kind {
            benchbob_boards::Health::Builtin(benchbob_boards::HealthKind::Vcgencmd) => Source::Vcgencmd(Mutex::new(RpiPower::default())),
            benchbob_boards::Health::Builtin(benchbob_boards::HealthKind::Sysfs) => Source::Sysfs,
            benchbob_boards::Health::Script { script } => Source::Script(script.clone()),
        };
        Self { source, root: "/sys/class".into(), cache: Mutex::new(None) }
    }

    /// Sensors (cached 5 s) and, from a script, its own `power` and `cpuTemp`.
    pub async fn sensors(&self) -> Reading {
        let mut c = self.cache.lock().await;
        if let Some((at, r)) = c.as_ref()
            && at.elapsed() < Duration::from_secs(5)
        {
            return r.clone();
        }
        let (s, p, t) = match &self.source {
            Source::Script(path) => {
                let r = run(&[path, "read"], Opts::timeout_ms(3000)).await;
                match serde_json::from_str::<Value>(&r.out) {
                    Ok(v) if r.code == 0 => {
                        (Sensors::from_json(&v), v.get("power").cloned().unwrap_or(Value::Null), v.get("cpuTemp").and_then(Value::as_f64))
                    }
                    _ => {
                        tracing::warn!("health script {path}: exit {} {}", r.code, r.err);
                        (read_sensors(&self.root), Value::Null, None)
                    }
                }
            }
            _ => (read_sensors(&self.root), Value::Null, None),
        };
        *c = Some((Instant::now(), (s.clone(), p.clone(), t)));
        (s, p, t)
    }

    /// `status.power`: vcgencmd's flags on a Pi, a script's `power`, else null.
    pub async fn power(&self) -> Value {
        match &self.source {
            Source::Vcgencmd(p) => p.lock().await.read().await,
            Source::Script(_) => self.sensors().await.1,
            Source::Sysfs => Value::Null,
        }
    }

    /// The CPU temperature for `system.cpuTemp`. A Pi's thermal_zone0 is the SoC, as it
    /// always was here; elsewhere zone0 can be an ACPI zone that never moves, so the sensor
    /// list picks the CPU package.
    pub async fn cpu_temp(&self) -> f64 {
        match &self.source {
            Source::Vcgencmd(_) => {
                benchbob_core::http::parse_int(&rd(&self.root.join("thermal/thermal_zone0/temp"))).map(|t| t as f64 / 1000.0).unwrap_or(0.0)
            }
            _ => {
                let (s, _, t) = self.sensors().await;
                t.or_else(|| cpu_temp(&s)).unwrap_or(0.0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (p, v) in files {
            let f = d.path().join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, format!("{v}\n")).unwrap();
        }
        d
    }

    #[test]
    fn desky_thermal_zone_and_its_hwmon_twin_are_one_reading() {
        let t = tree(&[
            ("thermal/thermal_zone0/type", "cpu-thermal"),
            ("thermal/thermal_zone0/temp", "44790"),
            ("hwmon/hwmon0/name", "cpu_thermal"),
            ("hwmon/hwmon0/temp1_input", "44790"),
            ("hwmon/hwmon1/name", "rpi_volt"),
            ("hwmon/hwmon1/in0_lcrit_alarm", "0"),
        ]);
        let s = read_sensors(t.path());
        assert_eq!(s.temps, vec![("thermal_zone0".into(), "cpu-thermal".into(), 44.8)]);
        assert!(s.volts.is_empty());
        assert_eq!(cpu_temp(&s), Some(44.8));
        assert_eq!(s.to_json()["temps"].to_string(), r#"[{"source":"thermal_zone0","label":"cpu-thermal","c":44.8}]"#);
    }

    #[test]
    fn intel_pc_coretemp_package_wins_voltages_fans_and_a_ups() {
        let t = tree(&[
            ("thermal/thermal_zone0/type", "acpitz"),
            ("thermal/thermal_zone0/temp", "27800"),
            ("thermal/thermal_zone1/type", "x86_pkg_temp"),
            ("thermal/thermal_zone1/temp", "51000"),
            ("hwmon/hwmon2/name", "coretemp"),
            ("hwmon/hwmon2/temp2_input", "48000"),
            ("hwmon/hwmon2/temp2_label", "Core 0"),
            ("hwmon/hwmon2/temp1_input", "52000"),
            ("hwmon/hwmon2/temp1_label", "Package id 0"),
            ("hwmon/hwmon3/name", "nct6798"),
            ("hwmon/hwmon3/in0_input", "1032"),
            ("hwmon/hwmon3/fan2_input", "812"),
            (
                "power_supply/ups/uevent",
                "POWER_SUPPLY_NAME=ups\nPOWER_SUPPLY_TYPE=UPS\nPOWER_SUPPLY_STATUS=Discharging\nPOWER_SUPPLY_CAPACITY=87\nPOWER_SUPPLY_VOLTAGE_NOW=13410000",
            ),
            ("power_supply/AC/uevent", "POWER_SUPPLY_TYPE=Mains\nPOWER_SUPPLY_ONLINE=0"),
        ]);
        let s = read_sensors(t.path());
        assert_eq!(cpu_temp(&s), Some(52.0));
        assert_eq!(s.volts, vec![("nct6798".into(), "in0".into(), 1.032)]);
        assert_eq!(s.fans, vec![("nct6798".into(), "fan2".into(), 812.0)]);
        assert_eq!(
            s.to_json()["supplies"].to_string(),
            r#"[{"name":"AC","type":"Mains","online":false,"status":null,"capacity":null,"volts":null},{"name":"ups","type":"UPS","online":null,"status":"Discharging","capacity":87,"volts":13.41}]"#
        );
    }

    #[test]
    fn rk3588_soc_zone_by_name_and_nothing_at_all() {
        let t = tree(&[
            ("thermal/thermal_zone0/type", "soc-thermal"),
            ("thermal/thermal_zone0/temp", "40000"),
            ("thermal/thermal_zone1/type", "bigcore0-thermal"),
            ("thermal/thermal_zone1/temp", "41000"),
        ]);
        assert_eq!(cpu_temp(&read_sensors(t.path())), Some(40.0));
        let e = tree(&[]);
        assert_eq!(cpu_temp(&read_sensors(e.path())), None);
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["hwmon10", "hwmon2", "AC", "ups", "thermal_zone1", "thermal_zone0"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["AC", "hwmon2", "hwmon10", "thermal_zone0", "thermal_zone1", "ups"]);
    }
}
