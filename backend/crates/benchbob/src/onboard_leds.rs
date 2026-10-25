//! The board's own LEDs (`/sys/class/leds`: a Pi's ACT and PWR), lit while someone holds
//! the dashboard's onboard-LED button so they can see which board this is, then handed
//! back to the kernel trigger they had.
//!
//! The same API as a node's (`POST /api/onboard-leds {on, ms, id}`, `onboardLeds` in the
//! status, MCP `onboard_led`). A hold lasts `ms` (at most 10 s) unless renewed, so a
//! dashboard that goes away can't leave an LED on. Not gated by GPIO output control:
//! these LEDs aren't on the header.
//!
//! Only LEDs of a board LED device (gpio-leds / pwm-leds, whose sysfs device is called
//! `leds` or `*-leds`) count: keyboard, SD-card and Ethernet PHY LEDs don't.
//! `BENCHBOB_LEDS_ROOT` points at a fixture tree instead of `/sys/class/leds`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use benchbob_core::HttpError;
use benchbob_core::driver::Tool;
use benchbob_core::run::{Opts, read_text, run};
use serde_json::{Value, json};

pub const MAX_HOLD_MS: u64 = 10_000;

pub struct OnboardLeds {
    root: PathBuf,
    held: Mutex<BTreeMap<String, Held>>,
    /// The expiry task is running.
    ticking: Mutex<bool>,
}

struct Held {
    until: Instant,
    /// What to put back: the trigger (as written to `trigger`) and the brightness.
    trigger: String,
    brightness: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Led {
    pub id: String,
    pub color: &'static str,
    /// The kernel trigger it normally follows ("mmc0", "default-on", "none").
    pub trigger: String,
}

impl Default for OnboardLeds {
    fn default() -> Self {
        let root = benchbob_core::paths::env("BENCHBOB_LEDS_ROOT").unwrap_or_else(|| "/sys/class/leds".into());
        Self::new(root)
    }
}

/// The bracketed entry of a `trigger` file: "none [mmc0] timer" → "mmc0".
pub fn current_trigger(s: &str) -> String {
    s.split_whitespace().find_map(|w| w.strip_prefix('[')?.strip_suffix(']')).unwrap_or("none").to_string()
}

/// A board LED device's LED: `/sys/devices/platform/leds/leds/ACT` (the LED's own
/// directory under a `leds` class dir, under a device named `leds` or `*-leds`).
pub fn is_board_led(canonical: &Path) -> bool {
    let mut up = canonical.ancestors().skip(1);
    let (Some(class), Some(dev)) = (up.next(), up.next()) else { return false };
    let name = |p: &Path| p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    let dev = name(dev);
    name(class) == "leds" && (dev == "leds" || dev.ends_with("-leds") || dev.ends_with("_leds"))
}

/// The colour from a `device:color:function` name, or a Pi's well-known names.
pub fn color_of(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    let part = lower.split(':').nth(1).unwrap_or("");
    for c in ["green", "red", "blue", "white", "yellow", "amber", "orange", "rgb"] {
        if part == c {
            return c;
        }
    }
    match lower.as_str() {
        "act" | "led0" => "green",
        "pwr" | "led1" => "red",
        _ => "",
    }
}

impl OnboardLeds {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), held: Mutex::default(), ticking: Mutex::new(false) }
    }

    pub fn list(&self) -> Vec<Led> {
        let Ok(rd) = std::fs::read_dir(&self.root) else { return vec![] };
        let held = self.held.lock().unwrap();
        let mut v: Vec<Led> = rd
            .flatten()
            .filter(|e| std::fs::canonicalize(e.path()).is_ok_and(|p| is_board_led(&p)))
            .filter_map(|e| {
                let id = e.file_name().to_str()?.to_string();
                let trigger = match held.get(&id) {
                    Some(h) => h.trigger.clone(),
                    None => current_trigger(&read_text(e.path().join("trigger"))),
                };
                Some(Led { color: color_of(&id), id, trigger })
            })
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    pub fn view(&self) -> Value {
        let now = Instant::now();
        let held = self.held.lock().unwrap().iter().filter(|(_, h)| h.until > now).map(|(k, _)| k.clone()).collect::<Vec<_>>();
        let leds = self.list();
        Value::Array(
            leds.into_iter()
                .map(|l| {
                    let normally = match l.trigger.as_str() {
                        "none" => "off unless set".to_string(),
                        "default-on" => "on".to_string(),
                        t => format!("kernel trigger: {t}"),
                    };
                    json!({ "id": l.id, "name": l.id, "color": l.color, "normally": normally, "held": held.contains(&l.id) })
                })
                .collect(),
        )
    }

    /// Light `id` (or every LED) for `ms`, or release it (`on: false`).
    pub async fn hold(self: &Arc<Self>, args: &Value) -> Result<Value, HttpError> {
        let leds = self.list();
        if leds.is_empty() {
            return Err(HttpError::new(404, "This board has no onboard LEDs BenchBob can drive (none in /sys/class/leds)"));
        }
        let id = match args.get("id") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => n.as_u64().and_then(|i| leds.get(i as usize)).map(|l| l.id.clone()).or(Some(n.to_string())),
            Some(v) => return Err(HttpError::bad_request(format!("id must be an LED name like ACT (got {v})"))),
        };
        if let Some(id) = &id
            && !leds.iter().any(|l| &l.id == id)
        {
            let names: Vec<_> = leds.iter().map(|l| l.id.as_str()).collect();
            return Err(HttpError::bad_request(format!("No onboard LED {id}; this board has {}", names.join(", "))));
        }
        let on = args.get("on").and_then(Value::as_bool).unwrap_or(true);
        let ms = match args.get("ms") {
            None | Some(Value::Null) => 1000,
            Some(v) => match v.as_u64() {
                Some(n @ 1..=MAX_HOLD_MS) => n,
                _ => return Err(HttpError::bad_request(format!("ms must be 1-{MAX_HOLD_MS} (got {v})"))),
            },
        };
        let targets: Vec<&Led> = leds.iter().filter(|l| id.as_ref().is_none_or(|id| &l.id == id)).collect();
        for l in targets {
            if on {
                self.light(&l.id, Instant::now() + Duration::from_millis(ms)).await?;
            } else {
                self.restore(&l.id).await;
            }
        }
        if on {
            self.tick();
        }
        Ok(json!({ "onboardLeds": self.view() }))
    }

    async fn light(&self, id: &str, until: Instant) -> Result<(), HttpError> {
        let dir = self.root.join(id);
        let first = {
            let mut held = self.held.lock().unwrap();
            match held.get_mut(id) {
                Some(h) => {
                    h.until = until;
                    false
                }
                None => {
                    let trigger = current_trigger(&read_text(dir.join("trigger")));
                    let brightness = read_text(dir.join("brightness"));
                    held.insert(id.to_string(), Held { until, trigger, brightness });
                    true
                }
            }
        };
        if first {
            let max = read_text(dir.join("max_brightness"));
            let max = if max.is_empty() { "1".to_string() } else { max };
            // Writing a brightness stops a blinking trigger on its own, but "none" also
            // stops triggers like mmc0 turning it back off.
            let r = async {
                write(&dir.join("trigger"), "none").await?;
                write(&dir.join("brightness"), &max).await
            }
            .await;
            if let Err(e) = r {
                self.restore(id).await;
                return Err(e);
            }
        }
        Ok(())
    }

    async fn restore(&self, id: &str) {
        let Some(h) = self.held.lock().unwrap().remove(id) else { return };
        let dir = self.root.join(id);
        if h.trigger == "none" {
            let _ = write(&dir.join("trigger"), "none").await;
            let _ = write(&dir.join("brightness"), if h.brightness.is_empty() { "0" } else { &h.brightness }).await;
        } else if let Err(e) = write(&dir.join("trigger"), &h.trigger).await {
            tracing::warn!("onboard LED {id}: couldn't restore trigger {}: {}", h.trigger, e.message);
        }
    }

    /// Put LEDs back as their holds run out, while any are held.
    fn tick(self: &Arc<Self>) {
        {
            let mut t = self.ticking.lock().unwrap();
            if *t {
                return;
            }
            *t = true;
        }
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let now = Instant::now();
                let expired: Vec<String> = me.held.lock().unwrap().iter().filter(|(_, h)| h.until <= now).map(|(k, _)| k.clone()).collect();
                for id in expired {
                    me.restore(&id).await;
                }
                let mut t = me.ticking.lock().unwrap();
                if me.held.lock().unwrap().is_empty() {
                    *t = false;
                    return;
                }
            }
        });
    }

    /// Put everything back now (shutdown).
    pub async fn release_all(&self) {
        let ids: Vec<String> = self.held.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.restore(&id).await;
        }
    }

    pub fn tool(self: &Arc<Self>) -> Tool {
        let me = self.clone();
        Tool::new(
            "onboard_led",
            "Light this BenchBob's own onboard LEDs (a Pi's ACT and PWR) for ms milliseconds (default 1000, max 10000) so a person can see which board this is; afterwards they go back to their normal kernel trigger. on: false releases early. Doesn't need GPIO output control.",
            json!({ "type": "object", "properties": {
                "id": { "type": ["string", "integer"], "description": "One LED by name (e.g. ACT; see onboardLeds in GET /api/status) or index; default all" },
                "on": { "type": "boolean" },
                "ms": { "type": "integer", "minimum": 1, "maximum": MAX_HOLD_MS },
            } }),
            move |args| {
                let me = me.clone();
                async move { me.hold(&args).await }
            },
        )
    }
}

/// Write a sysfs attribute: directly when the udev rule made it group-writable, else
/// through `sudo -n tee`.
async fn write(path: &Path, data: &str) -> Result<(), HttpError> {
    let p = path.display().to_string();
    match std::fs::write(path, data) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(e) => return Err(HttpError::new(502, format!("{p}: {e}"))),
    }
    let r = run(&["sudo", "-n", "tee", &p], Opts { stdin: Some(data.as_bytes().to_vec()), ..Opts::timeout_ms(5000) }).await;
    if r.code != 0 {
        return Err(HttpError::new(502, format!("{p}: {}", if r.err.is_empty() { "write failed" } else { &r.err })));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Arc<OnboardLeds>) {
        let t = tempfile::tempdir().unwrap();
        let class = t.path().join("devices/platform/leds/leds");
        for (name, trig, b) in [("ACT", "none [mmc0] timer", "0"), ("PWR", "none [default-on] input", "1")] {
            let d = class.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("trigger"), trig).unwrap();
            std::fs::write(d.join("brightness"), b).unwrap();
            std::fs::write(d.join("max_brightness"), "255").unwrap();
        }
        // Not a board LED: an SD card's.
        let mmc = t.path().join("devices/platform/fe340000.mmc/leds/mmc0::");
        std::fs::create_dir_all(&mmc).unwrap();
        (t, Arc::new(OnboardLeds::new(class)))
    }

    fn rd(o: &OnboardLeds, id: &str, f: &str) -> String {
        read_text(o.root.join(id).join(f))
    }

    #[test]
    fn filters_and_names() {
        assert!(is_board_led(Path::new("/sys/devices/platform/leds/leds/ACT")));
        assert!(is_board_led(Path::new("/sys/devices/platform/gpio-leds/leds/orangepi:green:status")));
        assert!(!is_board_led(Path::new("/sys/devices/platform/emmc2bus/fe340000.mmc/leds/mmc0::")));
        assert!(!is_board_led(Path::new(
            "/sys/devices/platform/soc/fe980000.usb/usb1/1-1/1-1:1.0/0003:046D:C52B.0001/input/input3/leds/input3::capslock"
        )));
        assert_eq!(color_of("ACT"), "green");
        assert_eq!(color_of("PWR"), "red");
        assert_eq!(color_of("orangepi:green:status"), "green");
        assert_eq!(color_of("foo"), "");
        assert_eq!(current_trigger("none [mmc0] timer"), "mmc0");
        assert_eq!(current_trigger(""), "none");
    }

    #[tokio::test]
    async fn hold_lights_then_restores() {
        let (_t, o) = fixture();
        let l = o.list();
        assert_eq!(l.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), ["ACT", "PWR"]);
        assert_eq!(l[0].trigger, "mmc0");

        let v = o.hold(&json!({ "id": "ACT", "ms": 150 })).await.unwrap();
        assert_eq!(v["onboardLeds"][0]["held"], true);
        assert_eq!(v["onboardLeds"][0]["normally"], "kernel trigger: mmc0");
        assert_eq!(v["onboardLeds"][1]["held"], false);
        assert_eq!(rd(&o, "ACT", "trigger"), "none");
        assert_eq!(rd(&o, "ACT", "brightness"), "255");
        assert_eq!(rd(&o, "PWR", "trigger"), "none [default-on] input"); // untouched

        // Renewing doesn't touch sysfs again; running out puts the trigger back.
        o.hold(&json!({ "id": "ACT", "ms": 150 })).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(rd(&o, "ACT", "trigger"), "mmc0");
        assert_eq!(o.view()[0]["held"], false);
    }

    #[tokio::test]
    async fn release_and_errors() {
        let (_t, o) = fixture();
        o.hold(&json!({ "ms": 5000 })).await.unwrap();
        assert_eq!(rd(&o, "PWR", "brightness"), "255");
        o.hold(&json!({ "on": false })).await.unwrap();
        assert_eq!(rd(&o, "ACT", "trigger"), "mmc0");
        assert_eq!(rd(&o, "PWR", "trigger"), "default-on");
        assert_eq!(o.hold(&json!({ "id": "nope" })).await.unwrap_err().status, 400);
        assert_eq!(o.hold(&json!({ "ms": 60000 })).await.unwrap_err().status, 400);
        // An index works too.
        o.hold(&json!({ "id": 1, "ms": 5000 })).await.unwrap();
        assert_eq!(o.view()[1]["held"], true);
        o.release_all().await;
        assert_eq!(o.view()[1]["held"], false);

        let empty = Arc::new(OnboardLeds::new(_t.path().join("nothing")));
        assert_eq!(empty.hold(&json!({})).await.unwrap_err().status, 404);
    }
}
