//! Pure helpers (`peripherals/lib/gps-parse.ts`): which USB ids are GPS receivers, and a fold
//! of gpsd's JSON watch stream (TPV/SKY/PPS/TOFF/DEVICE…) into the state the panel shows.

use benchbob_core::http::num;
use benchbob_core::util::{js_round, locale_cmp, parse_iso};
use serde_json::{Map, Value, json};

/// USB GPS receivers we treat as GPS without asking.
pub fn gps_usb(id: &str) -> Option<&'static str> {
    Some(match id {
        "1546:01a5" => "u-blox 5",
        "1546:01a6" => "u-blox 6",
        "1546:01a7" => "u-blox 7",
        "1546:01a8" => "u-blox 8",
        "1546:01a9" => "u-blox 9",
        "0e8d:3329" => "MediaTek MT3329 GPS",
        "091e:0003" => "Garmin GPS (USB)",
        // shows several ttys; one is NMEA
        "1199:9071" => "Sierra Wireless GNSS",
        _ => return None,
    })
}

/// Generic USB-serial bridges GPS pucks are often built on. Never claimed automatically: the
/// same chips sit in every other adapter.
pub fn maybe_gps_usb(id: &str) -> Option<&'static str> {
    Some(match id {
        "067b:2303" => "Prolific PL2303 (GlobalSat BU-353, SiRF pucks…)",
        "067b:23a3" => "Prolific PL2303GC",
        "10c4:ea60" => "Silicon Labs CP210x",
        "0403:6001" => "FTDI FT232",
        "0403:6015" => "FTDI FT-X",
        "1a86:7523" => "QinHeng CH340",
        _ => return None,
    })
}

/// (kind: gps | maybe | unknown, label)
pub fn classify_usb(vidpid: &str) -> (&'static str, &'static str) {
    if let Some(l) = gps_usb(vidpid) {
        return ("gps", l);
    }
    if vidpid.starts_with("1546:") {
        return ("gps", "u-blox");
    }
    if let Some(l) = maybe_gps_usb(vidpid) {
        return ("maybe", l);
    }
    ("unknown", "")
}

// ---------- satellites ----------

pub const GNSS: [&str; 8] = ["GPS", "SBAS", "Galileo", "BeiDou", "IMES", "QZSS", "GLONASS", "NavIC"];

/// gpsd's legacy PRN numbering, for receivers/versions that don't send gnssid.
pub fn constellation_of(sat: &Value) -> String {
    if let Some(g) = sat.get("gnssid").and_then(Value::as_f64) {
        return match (g >= 0.0 && g.fract() == 0.0).then(|| GNSS.get(g as usize)).flatten() {
            Some(n) => n.to_string(),
            None => format!("gnss {}", benchbob_core::http::js_string(&num(g))),
        };
    }
    let p = sat.get("PRN").and_then(Value::as_f64).unwrap_or(f64::NAN);
    match p {
        p if (1.0..=63.0).contains(&p) => "GPS",
        p if (64.0..=96.0).contains(&p) => "GLONASS",
        p if (120.0..=158.0).contains(&p) => "SBAS",
        p if (193.0..=200.0).contains(&p) => "QZSS",
        p if (201.0..=263.0).contains(&p) => "BeiDou",
        p if (301.0..=336.0).contains(&p) => "Galileo",
        _ => "other",
    }
    .into()
}

#[derive(Debug, Clone)]
pub struct Sat {
    /// gpsd's PRN, as sent (absent when it wasn't).
    pub prn: Option<Value>,
    pub svid: Value,
    pub gnss: String,
    pub el: Value,
    pub az: Value,
    pub snr: Value,
    pub used: bool,
    pub health: Value,
    /// Our clock: the last SKY that listed it.
    pub seen: u64,
}

impl Sat {
    fn ident(&self) -> String {
        let id = if self.svid.is_null() { self.prn.clone().unwrap_or(Value::Null) } else { self.svid.clone() };
        format!("{}:{}", self.gnss, benchbob_core::http::js_string(&id).replace("null", "undefined"))
    }
    fn order_num(&self) -> f64 {
        let v = if self.svid.is_null() { self.prn.as_ref().unwrap_or(&Value::Null) } else { &self.svid };
        v.as_f64().unwrap_or(f64::NAN)
    }
    /// `({ seen, ...s }) => s`
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        if let Some(p) = &self.prn {
            m.insert("prn".into(), p.clone());
        }
        m.insert("svid".into(), self.svid.clone());
        m.insert("gnss".into(), self.gnss.clone().into());
        m.insert("el".into(), self.el.clone());
        m.insert("az".into(), self.az.clone());
        m.insert("snr".into(), self.snr.clone());
        m.insert("used".into(), self.used.into());
        m.insert("health".into(), self.health.clone());
        Value::Object(m)
    }
}

const SAT_TTL_MS: u64 = 10_000;

pub const FIX_MODE: [&str; 4] = ["unknown", "no fix", "2D", "3D"];

#[derive(Debug, Clone)]
pub struct Offset {
    pub offset_ns: f64,
    pub at: u64,
}

#[derive(Debug, Clone, Default)]
pub struct GpsdState {
    pub version: String,
    pub devices: Vec<Value>,
    pub tpv: Option<Map<String, Value>>,
    pub tpv_at: u64,
    pub sky: Option<Map<String, Value>>,
    pub sky_at: u64,
    /// Merged across partial SKY reports, newest data per satellite.
    pub sats: Vec<Sat>,
    pub pps: Option<Offset>,
    pub toff: Option<Offset>,
    pub errors: Vec<String>,
    pub messages: u64,
}

/// The number as JavaScript arithmetic sees a field: undefined/non-numbers are NaN.
fn f(o: &Value, k: &str) -> f64 {
    match o.get(k) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(v) => benchbob_core::http::js_number(v),
        None => f64::NAN,
    }
}

/// Offset in ns from a PPS/TOFF report: the receiver's idea of the second minus our clock.
fn real_minus_clock(o: &Value) -> f64 {
    (f(o, "real_sec") - f(o, "clock_sec")) * 1e9 + (f(o, "real_nsec") - f(o, "clock_nsec"))
}

/// `num()`: a finite number, else null.
fn fnum(v: Option<&Value>) -> Value {
    match v {
        Some(Value::Number(n)) if n.as_f64().is_some_and(f64::is_finite) => Value::Number(n.clone()),
        _ => Value::Null,
    }
}

/// `x ?? null`
fn or_null(v: Option<&Value>) -> Value {
    v.cloned().unwrap_or(Value::Null)
}

/// `dev(o)`: the DEVICE fields the panel shows, in this order.
fn dev(o: &Value) -> Value {
    let mut m = Map::new();
    for k in ["path", "driver", "subtype", "subtype1", "bps", "cycle", "activated", "flags"] {
        if let Some(v) = o.get(k) {
            m.insert(k.into(), v.clone());
        }
    }
    Value::Object(m)
}

pub fn ingest(s: &mut GpsdState, o: &Value, now: u64) {
    let Some(obj) = o.as_object() else { return };
    s.messages += 1;
    match obj.get("class").and_then(Value::as_str).unwrap_or("") {
        "VERSION" => s.version = obj.get("release").and_then(Value::as_str).unwrap_or("").into(),
        "DEVICES" => s.devices = obj.get("devices").and_then(Value::as_array).map(|a| a.iter().map(dev).collect()).unwrap_or_default(),
        "DEVICE" => {
            // `{ ...old, ...dev(o) }`: dev(o) carries every key (undefined ones too), so it
            // replaces the entry.
            let d = dev(o);
            match s.devices.iter_mut().find(|x| x.get("path") == d.get("path")) {
                Some(x) => *x = d,
                None => s.devices.push(d),
            }
        }
        "TPV" => {
            // gpsd sends a mode-only TPV while it has nothing; keep the last position for
            // display but let the mode drop so the panel says "no fix".
            let mode = f(o, "mode");
            s.tpv = match s.tpv.take() {
                // `o.mode >= 2` is false for NaN (no mode) too.
                Some(mut old) if mode.partial_cmp(&2.0).is_none_or(|o| o.is_lt()) => {
                    match obj.get("mode") {
                        Some(m) => {
                            old.insert("mode".into(), m.clone());
                        }
                        None => {
                            old.shift_remove("mode");
                        }
                    }
                    if let Some(t) = obj.get("time").filter(|t| !t.is_null()) {
                        old.insert("time".into(), t.clone());
                    }
                    Some(old)
                }
                _ => Some(obj.clone()),
            };
            s.tpv_at = now;
        }
        "SKY" => {
            // gpsd 3.2x splits SKY: DOPs from GSA, satellites from GSV. Merge, don't replace.
            let sky = s.sky.get_or_insert_with(Map::new);
            for (k, v) in obj {
                sky.insert(k.clone(), v.clone());
            }
            s.sky_at = now;
            // Satellites arrive one constellation's GSV group at a time, so merge by identity
            // and age out the ones no longer reported instead of replacing the list.
            if let Some(list) = obj.get("satellites").and_then(Value::as_array) {
                let mut by_id: Vec<(String, Sat)> = s.sats.drain(..).map(|x| (x.ident(), x)).collect();
                for x in list {
                    let sat = Sat {
                        prn: x.get("PRN").cloned(),
                        svid: or_null(x.get("svid")),
                        gnss: constellation_of(x),
                        el: fnum(x.get("el")),
                        az: fnum(x.get("az")),
                        snr: fnum(x.get("ss")),
                        used: x.get("used").is_some_and(benchbob_core::http::truthy),
                        health: or_null(x.get("health")),
                        seen: now,
                    };
                    let id = sat.ident();
                    match by_id.iter_mut().find(|(k, _)| *k == id) {
                        Some(e) => e.1 = sat,
                        None => by_id.push((id, sat)),
                    }
                }
                let mut sats: Vec<Sat> = by_id.into_iter().map(|(_, s)| s).filter(|x| now.saturating_sub(x.seen) <= SAT_TTL_MS).collect();
                sats.sort_by(|a, b| {
                    locale_cmp(&a.gnss, &b.gnss)
                        .then_with(|| a.order_num().partial_cmp(&b.order_num()).unwrap_or(std::cmp::Ordering::Equal))
                });
                s.sats = sats;
            }
        }
        "PPS" => s.pps = Some(Offset { offset_ns: real_minus_clock(o), at: now }),
        "TOFF" => s.toff = Some(Offset { offset_ns: real_minus_clock(o), at: now }),
        "ERROR" => {
            let m = obj.get("message").and_then(Value::as_str).filter(|m| !m.is_empty()).map(String::from).unwrap_or_else(|| o.to_string());
            s.errors.push(m);
            if s.errors.len() > 20 {
                s.errors.remove(0);
            }
        }
        _ => {}
    }
}

/// The compact view used by status, REST and MCP.
pub fn summarize(s: &GpsdState, now: u64) -> Value {
    let empty = Map::new();
    let t = s.tpv.as_ref().unwrap_or(&empty);
    let g = |k: &str| t.get(k);
    let mode = g("mode").and_then(Value::as_f64).unwrap_or(0.0);
    let fix = if mode >= 0.0 && mode.fract() == 0.0 { FIX_MODE.get(mode as usize).copied().unwrap_or("unknown") } else { "unknown" };
    let used = s.sats.iter().filter(|x| x.used).count();
    let mut by_gnss = Map::new();
    for x in &s.sats {
        let e = by_gnss.entry(x.gnss.clone()).or_insert_with(|| json!({ "visible": 0, "used": 0 }));
        e["visible"] = (e["visible"].as_u64().unwrap_or(0) + 1).into();
        if x.used {
            e["used"] = (e["used"].as_u64().unwrap_or(0) + 1).into();
        }
    }
    // TPV time vs our clock at receipt: includes the receiver's output latency and serial
    // transfer time (typically 50–500 ms), so a sanity check, not a time source.
    let tpv_time = g("time").and_then(Value::as_str).and_then(parse_iso);
    let nmea_offset = match tpv_time {
        Some(tt) if s.tpv_at != 0 => num(js_round((tt - s.tpv_at as i64) as f64)),
        _ => Value::Null,
    };
    let sky = |k: &str| fnum(s.sky.as_ref().and_then(|m| m.get(k)));
    let time = g("time").filter(|v| benchbob_core::http::truthy(v)).cloned().unwrap_or(Value::Null);
    let alt_hae = match g("altHAE") {
        Some(v) if !v.is_null() => fnum(Some(v)),
        _ => fnum(g("alt")),
    };
    json!({
        "fix": fix,
        "mode": num(mode),
        "status": or_null(g("status")),
        "time": time,
        "lat": fnum(g("lat")),
        "lon": fnum(g("lon")),
        "altHAE_m": alt_hae,
        "altMSL_m": fnum(g("altMSL")),
        "speed_ms": fnum(g("speed")),
        "track_deg": fnum(g("track")),
        "climb_ms": fnum(g("climb")),
        "eph_m": fnum(g("eph")),
        "epv_m": fnum(g("epv")),
        "leapseconds": or_null(g("leapseconds")),
        "hdop": sky("hdop"),
        "vdop": sky("vdop"),
        "pdop": sky("pdop"),
        "gdop": sky("gdop"),
        // From the merged list: SKY's own uSat/nSat only count that report's constellations.
        "satsUsed": used,
        "satsVisible": s.sats.len(),
        "constellations": by_gnss,
        "nmeaOffset_ms": nmea_offset,
        "toffOffset_ms": s.toff.as_ref().map(|o| num(js_round(o.offset_ns / 1e4) / 100.0)).unwrap_or(Value::Null),
        "ppsOffset_us": s.pps.as_ref().map(|o| num(js_round(o.offset_ns / 10.0) / 100.0)).unwrap_or(Value::Null),
        "ppsAge_s": s.pps.as_ref().map(|o| num(js_round((now as f64 - o.at as f64) / 1000.0))).unwrap_or(Value::Null),
        "lastUpdate_s": if s.tpv_at != 0 { num(js_round((now as f64 - s.tpv_at as f64) / 1000.0)) } else { Value::Null },
    })
}

// ---------- NMEA helpers (fixtures and tests) ----------

pub fn nmea_checksum(body: &str) -> String {
    format!("{:02X}", body.bytes().fold(0u8, |c, b| c ^ b))
}

pub fn nmea(body: &str) -> String {
    format!("${body}*{}", nmea_checksum(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use benchbob_core::util::{iso, parse_json};

    const WATCH: &str = include_str!("../fixtures/gpsd-watch.jsonl");
    const NMEA: &str = include_str!("../fixtures/gps-multi.nmea");

    fn close(v: &Value, want: f64, digits: i32) {
        let x = v.as_f64().unwrap();
        assert!((x - want).abs() < 10f64.powi(-digits) / 2.0, "{x} vs {want}");
    }

    #[test]
    fn tpv_sky_device_fold_into_a_3d_fix() {
        let mut s = GpsdState::default();
        let mut t = 1_000_000;
        for l in WATCH.trim().split('\n') {
            t += 100;
            ingest(&mut s, &parse_json(l).unwrap(), t);
        }
        assert!(s.version.starts_with("3.25"));
        let d = &s.devices[0];
        assert_eq!((d["path"].clone(), d["driver"].clone(), d["bps"].clone()), (json!("/dev/pts/2"), json!("NMEA0183"), json!(4800)));
        // "cycle":1.00 prints as JavaScript prints it.
        assert_eq!(d["cycle"].to_string(), "1");
        let v = summarize(&s, t);
        assert_eq!(v["fix"], "3D");
        close(&v["lat"], 37.3861, 3);
        close(&v["lon"], -122.0839, 3);
        assert_eq!(v["altMSL_m"], 32.5);
        assert_eq!(v["track_deg"], 87.5);
        assert_eq!(v["hdop"], 0.9);
        assert_eq!(v["pdop"], 1.6);
        // The fixture has 9 GPS + 4 GLONASS + 3 Galileo + 3 BeiDou, 14 used.
        assert_eq!(v["satsVisible"], 19);
        assert_eq!(v["satsUsed"], 14);
        assert_eq!(
            v["constellations"],
            json!({ "BeiDou": { "visible": 3, "used": 2 }, "Galileo": { "visible": 3, "used": 2 }, "GLONASS": { "visible": 4, "used": 3 }, "GPS": { "visible": 9, "used": 7 } })
        );
        // localeCompare order: BeiDou, Galileo, GLONASS, GPS.
        let order: Vec<&String> = v["constellations"].as_object().unwrap().keys().collect();
        assert_eq!(order, ["BeiDou", "Galileo", "GLONASS", "GPS"]);
        let s12 = s.sats.iter().find(|x| x.gnss == "GPS" && x.svid == 12).unwrap();
        assert_eq!((s12.el.clone(), s12.az.clone(), s12.snr.clone(), s12.used), (json!(62), json!(45), json!(42), true));
        assert!(v["time"].as_str().unwrap().starts_with("2026-10-17T08:30"));
        let sj = s12.to_json();
        let keys: Vec<&String> = sj.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["prn", "svid", "gnss", "el", "az", "snr", "used", "health"]);
    }

    #[test]
    fn partial_sky_reports_merge_and_stale_satellites_age_out() {
        let mut s = GpsdState::default();
        ingest(
            &mut s,
            &json!({ "class": "SKY", "satellites": [{ "PRN": 2, "gnssid": 0, "svid": 2, "el": 10, "az": 20, "ss": 30, "used": true }] }),
            0,
        );
        ingest(
            &mut s,
            &json!({ "class": "SKY", "satellites": [{ "PRN": 65, "gnssid": 6, "svid": 1, "el": 10, "az": 20, "ss": 30, "used": false }] }),
            500,
        );
        assert_eq!(s.sats.len(), 2);
        // A DOP-only report keeps the satellites.
        ingest(&mut s, &json!({ "class": "SKY", "hdop": 1.1 }), 600);
        assert_eq!(s.sats.len(), 2);
        assert_eq!(summarize(&s, 600)["hdop"], 1.1);
        ingest(
            &mut s,
            &json!({ "class": "SKY", "satellites": [{ "PRN": 65, "gnssid": 6, "svid": 1, "el": 11, "az": 20, "ss": 31, "used": true }] }),
            10_400,
        );
        assert_eq!(s.sats.iter().map(|x| x.gnss.as_str()).collect::<Vec<_>>(), ["GLONASS"]);
    }

    #[test]
    fn mode_only_tpv_drops_the_fix_but_keeps_the_last_position() {
        let mut s = GpsdState::default();
        ingest(&mut s, &json!({ "class": "TPV", "mode": 3, "lat": 1, "lon": 2, "time": "2026-10-17T00:00:00.000Z" }), 1000);
        ingest(&mut s, &json!({ "class": "TPV", "mode": 1 }), 2000);
        let v = summarize(&s, 2000);
        assert_eq!(v["fix"], "no fix");
        assert_eq!(v["lat"], 1);
        assert_eq!(v["time"], "2026-10-17T00:00:00.000Z");
    }

    #[test]
    fn pps_toff_and_nmea_offsets() {
        let mut s = GpsdState::default();
        ingest(
            &mut s,
            &json!({ "class": "PPS", "device": "/dev/pps0", "real_sec": 100, "real_nsec": 0, "clock_sec": 99, "clock_nsec": 999_990_000, "precision": -20 }),
            5000,
        );
        ingest(
            &mut s,
            &json!({ "class": "TOFF", "device": "/dev/ttyACM0", "real_sec": 100, "real_nsec": 0, "clock_sec": 100, "clock_nsec": 120_000_000 }),
            5000,
        );
        ingest(&mut s, &json!({ "class": "TPV", "mode": 3, "time": iso(4800) }), 5000);
        let v = summarize(&s, 6000);
        assert_eq!(v["ppsOffset_us"], 10);
        assert_eq!(v["toffOffset_ms"], -120);
        assert_eq!(v["nmeaOffset_ms"], -200);
        assert_eq!(v["ppsAge_s"], 1);
        // Missing fields are NaN, which JSON writes as null.
        ingest(&mut s, &json!({ "class": "PPS" }), 6000);
        assert_eq!(summarize(&s, 6000)["ppsOffset_us"], Value::Null);
    }

    #[test]
    fn empty_state_summary() {
        let v = summarize(&GpsdState::default(), 0);
        assert_eq!(v["fix"], "unknown");
        assert_eq!(v["mode"], 0);
        assert_eq!(v["lastUpdate_s"], Value::Null);
        assert_eq!(v["constellations"], json!({}));
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 26);
        assert_eq!((keys[0].as_str(), keys[25].as_str()), ("fix", "lastUpdate_s"));
    }

    #[test]
    fn constellation_from_gnssid_or_legacy_prn_ranges() {
        assert_eq!(constellation_of(&json!({ "gnssid": 2, "PRN": 3 })), "Galileo");
        assert_eq!(constellation_of(&json!({ "PRN": 70 })), "GLONASS");
        assert_eq!(constellation_of(&json!({ "PRN": 305 })), "Galileo");
        assert_eq!(constellation_of(&json!({ "PRN": 133 })), "SBAS");
        assert_eq!(constellation_of(&json!({ "gnssid": 9 })), "gnss 9");
        assert_eq!(constellation_of(&json!({})), "other");
    }

    #[test]
    fn usb_classification() {
        assert_eq!(classify_usb("1546:01a8").0, "gps");
        assert_eq!(classify_usb("1546:0502").0, "gps");
        assert_eq!(classify_usb("067b:2303").0, "maybe");
        assert_eq!(classify_usb("2109:3431").0, "unknown");
    }

    #[test]
    fn nmea_checksums_match_the_fixture() {
        assert_eq!(nmea_checksum("GPGSV,3,3,09,29,35,240,33"), "4B");
        for l in NMEA.trim().split('\n').map(|l| l.trim_end_matches('\r')) {
            let body = l.strip_prefix('$').unwrap().rsplit_once('*').unwrap().0;
            assert_eq!(nmea(body), l);
        }
    }
}
