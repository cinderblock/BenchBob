//! The Raspberry Pi image catalog (server.ts "RPi Image Catalog"), the flash picker's
//! saved lists ("Image Lists") and the default flash settings ("Default Settings").

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use benchbob_core::http::{HttpError, js_string, truthy};
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::sys::Sys;
use crate::util::js_pretty;

pub const CATALOG_URL: &str = "https://downloads.raspberrypi.com/os_list_imagingutility_v4.json";
const CACHE_FOR: Duration = Duration::from_secs(3600);

#[derive(Default)]
pub struct Catalog {
    cache: Mutex<Option<(Instant, Value, Value)>>,
}

/// `x || dflt`, as JSON.
fn or(v: Option<&Value>, dflt: Value) -> Value {
    v.filter(|x| truthy(x)).cloned().unwrap_or(dflt)
}

/// Raspberry Pi OS images are <date>-raspios-<release>-<arm64|armhf>[-lite|-full].img.xz
/// under a raspios[_oldstable][_lite|_full]_<arch>/ directory. "Legacy" comes from that
/// directory rather than the codename, so this keeps working when the next Debian release
/// takes over.
pub fn parse_rpi_os(url: &str) -> Option<Value> {
    if !url.starts_with("https://downloads.raspberrypi.com/") {
        return None;
    }
    let m = Regex::new(r"/[^/]*-raspios-([a-z]+)-(arm64|armhf)(?:-(lite|full))?\.img\.xz$").unwrap().captures(url)?;
    if !Regex::new(r"/raspios(_oldstable)?(_lite|_full)?_(arm64|armhf)/").unwrap().is_match(url) {
        return None;
    }
    Some(json!({
        "release": &m[1],
        "arch": if &m[2] == "arm64" { "64" } else { "32" },
        "edition": m.get(3).map(|x| x.as_str()).unwrap_or("desktop"),
        "legacy": url.contains("_oldstable_"),
    }))
}

/// `flattenImages`: every image with its category path, board tags and a stable key.
pub fn flatten(items: &[Value], path: &[String]) -> Vec<Value> {
    let mut r: Vec<Value> = Vec::new();
    for i in items {
        if let Some(sub) = i.get("subitems").filter(|v| truthy(v)) {
            let mut p = path.to_vec();
            p.push(i.get("name").map(js_string).unwrap_or_else(|| "undefined".into()));
            r.extend(flatten(sub.as_array().map(Vec::as_slice).unwrap_or(&[]), &p));
        } else if let Some(url) = i.get("url").filter(|v| truthy(v)) {
            let url_s = js_string(url);
            let rpios = parse_rpi_os(&url_s);
            let mut devices: Vec<Value> = Vec::new();
            for d in i.get("devices").and_then(Value::as_array).into_iter().flatten() {
                if !devices.contains(d) {
                    devices.push(d.clone());
                }
            }
            let name = i.get("name").cloned().unwrap_or(Value::Null);
            let key = match &rpios {
                Some(o) => format!(
                    "rpios/{}/{}/{}",
                    if o["legacy"] == true { "legacy" } else { "current" },
                    o["arch"].as_str().unwrap_or(""),
                    o["edition"].as_str().unwrap_or("")
                ),
                None => path
                    .iter()
                    .cloned()
                    .chain([if name.is_null() { String::new() } else { js_string(&name) }])
                    .collect::<Vec<_>>()
                    .join("/"),
            };
            let mut m = Map::new();
            m.insert("name".into(), name);
            m.insert("description".into(), or(i.get("description"), "".into()));
            m.insert("category".into(), path.last().cloned().unwrap_or_default().into());
            m.insert("path".into(), json!(path));
            m.insert("url".into(), url.clone());
            m.insert("extractSize".into(), or(i.get("extract_size"), 0.into()));
            m.insert("downloadSize".into(), or(i.get("image_download_size"), 0.into()));
            m.insert("releaseDate".into(), or(i.get("release_date"), "".into()));
            m.insert("icon".into(), or(i.get("icon"), "".into()));
            m.insert("initFormat".into(), or(i.get("init_format"), "none".into()));
            m.insert("devices".into(), Value::Array(devices));
            m.insert("key".into(), key.into());
            if let Some(o) = rpios {
                m.insert("rpios".into(), o);
            }
            r.push(Value::Object(m));
        }
    }
    if path.is_empty() {
        // A few names repeat with different boards (DietPi, Alpine): tell them apart so a
        // favorite points at one image. Some even share the boards (PINN, FullPageOS
        // nightlies), so those fall back to catalog order.
        let count = |r: &[Value]| {
            let mut n: HashMap<String, usize> = HashMap::new();
            for im in r {
                *n.entry(im["key"].as_str().unwrap_or("").to_string()).or_default() += 1;
            }
            n
        };
        let by_name = count(&r);
        for im in &mut r {
            if by_name[im["key"].as_str().unwrap_or("")] > 1 {
                let devs = im["devices"].as_array().map(|a| a.iter().map(js_string).collect::<Vec<_>>().join(",")).unwrap_or_default();
                im["key"] = format!("{}@{devs}", im["key"].as_str().unwrap_or("")).into();
            }
        }
        let by_boards = count(&r);
        let mut nth: HashMap<String, usize> = HashMap::new();
        for im in &mut r {
            let k = im["key"].as_str().unwrap_or("").to_string();
            if by_boards[&k] > 1 {
                let i = nth.entry(k.clone()).or_default();
                *i += 1;
                im["key"] = format!("{k}#{i}").into();
            }
        }
    }
    r
}

/// The board list for the picker. "exclusive" boards (Pi 5) only match images that list
/// them; others also take images with no board list, as Imager does.
pub fn devices(data: &Value) -> Value {
    Value::Array(
        data.pointer("/imager/devices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|d| {
                json!({
                    "name": d.get("name").cloned().unwrap_or(Value::Null),
                    "tags": or(d.get("tags"), json!([])),
                    "description": or(d.get("description"), "".into()),
                    "exclusive": d.get("matching_type").and_then(Value::as_str) == Some("exclusive"),
                })
            })
            .collect(),
    )
}

impl Catalog {
    /// `getRpiImages`: cached for an hour; a failed fetch keeps what we had.
    pub async fn images(&self, sys: &dyn Sys) -> Value {
        if let Some((at, imgs, _)) = self.cache.lock().unwrap().as_ref()
            && at.elapsed() < CACHE_FOR
        {
            return imgs.clone();
        }
        let r = sys.run(&["curl", "-fsSL", "--max-time", "60", CATALOG_URL]).await;
        let data = (r.code == 0).then(|| serde_json::from_str::<Value>(&r.out).ok()).flatten();
        match data {
            Some(d) => {
                let imgs = Value::Array(flatten(d.get("os_list").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]), &[]));
                *self.cache.lock().unwrap() = Some((Instant::now(), imgs.clone(), devices(&d)));
                imgs
            }
            None => {
                if r.code != 0 {
                    tracing::warn!("image catalog: {}", r.err);
                }
                self.cache.lock().unwrap().as_ref().map(|c| c.1.clone()).unwrap_or(json!([]))
            }
        }
    }

    /// Images plus the board list.
    pub async fn catalog(&self, sys: &dyn Sys) -> Value {
        let images = self.images(sys).await;
        let devices = self.cache.lock().unwrap().as_ref().map(|c| c.2.clone()).unwrap_or(json!([]));
        json!({ "images": images, "devices": devices })
    }

    /// The catalog's init_format for an image URL (never the client's say-so: an old
    /// dashboard tab doesn't send it, and guessing wrong drops every setting).
    pub async fn init_format(&self, sys: &dyn Sys, url: &str) -> Option<String> {
        self.images(sys)
            .await
            .as_array()?
            .iter()
            .find(|i| i.get("url").and_then(Value::as_str) == Some(url))?
            .get("initFormat")?
            .as_str()
            .map(String::from)
    }
}

// ---------- image lists ----------

pub struct ImageList {
    pub path: PathBuf,
    pub initial: Value,
}

pub fn image_lists() -> Vec<(&'static str, ImageList)> {
    use benchbob_core::paths::state_path;
    vec![
        ("/api/favorites", ImageList { path: state_path("favorites.json"), initial: json!([]) }),
        (
            "/api/quicklist",
            ImageList {
                path: state_path("quicklist.json"),
                initial: json!([{ "key": "rpios/current/64/lite", "label": "Pi OS Lite 64-bit" }]),
            },
        ),
    ]
}

impl ImageList {
    pub fn get(&self) -> Value {
        std::fs::read(&self.path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| self.initial.clone())
    }

    pub fn set(&self, body: &Value) -> Result<Value, HttpError> {
        let Some(a) = body.as_array().filter(|a| a.iter().all(|f| f.get("key").is_some_and(Value::is_string))) else {
            return Err(HttpError::bad_request("Expected [{ key, label }]"));
        };
        let out: Vec<Value> = a
            .iter()
            .map(|f| {
                let label = match f.get("label") {
                    None | Some(Value::Null) => f["key"].clone(),
                    Some(l) => Value::String(js_string(l)),
                };
                json!({ "key": f["key"], "label": label })
            })
            .collect();
        std::fs::write(&self.path, js_pretty(&Value::Array(out))).map_err(|e| HttpError::new(500, e.to_string()))?;
        Ok(json!({ "ok": true }))
    }
}

// ---------- default flash settings ----------

/// When nothing has been saved: this machine's timezone and the Wi-Fi it's on.
pub async fn default_settings(sys: &dyn Sys) -> Value {
    let tz = {
        let t = sys.read_text("/etc/timezone").await;
        if t.is_empty() { "UTC".to_string() } else { t }
    };
    let out = sys.run(&["nmcli", "-t", "-f", "active,ssid", "dev", "wifi"]).await.out;
    let ssid =
        out.split('\n').find(|l| l.starts_with("yes:")).map(|l| l.split(':').skip(1).collect::<Vec<_>>().join(":")).unwrap_or_default();
    json!({
        "hostname": "raspberrypi", "username": "pi", "password": "", "enableSsh": true,
        "sshKeys": [],
        "wifiNetworks": if ssid.is_empty() { json!([]) } else { json!([{ "ssid": ssid, "password": "", "country": "US", "priority": 0 }]) },
        "timezone": tz, "keyboardLayout": "us",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_flattens_like_server_ts() {
        let os: Value = serde_json::from_str(include_str!("../fixtures/os_list_v4.json")).unwrap();
        let want: Value = serde_json::from_str(include_str!("../fixtures/catalog.expected.json")).unwrap();
        let images = Value::Array(flatten(os["os_list"].as_array().unwrap(), &[]));
        assert_eq!(serde_json::to_string(&images).unwrap(), serde_json::to_string(&want["images"]).unwrap());
        assert_eq!(serde_json::to_string(&devices(&os)).unwrap(), serde_json::to_string(&want["devices"]).unwrap());
    }

    #[test]
    fn rpi_os_names() {
        let u = "https://downloads.raspberrypi.com/raspios_oldstable_lite_armhf/images/x/2025-05-13-raspios-bookworm-armhf-lite.img.xz";
        assert_eq!(parse_rpi_os(u).unwrap(), json!({ "release": "bookworm", "arch": "32", "edition": "lite", "legacy": true }));
        assert!(parse_rpi_os("https://example.com/raspios_arm64/x-raspios-trixie-arm64.img.xz").is_none());
    }

    #[test]
    fn image_lists_validate_and_label() {
        let dir = tempfile::tempdir().unwrap();
        let l = ImageList { path: dir.path().join("q.json"), initial: json!([{ "key": "a", "label": "A" }]) };
        assert_eq!(l.get(), json!([{ "key": "a", "label": "A" }]));
        assert!(l.set(&json!([{ "label": "x" }])).is_err());
        l.set(&json!([{ "key": "k1" }, { "key": "k2", "label": 5, "x": 1 }])).unwrap();
        assert_eq!(
            std::fs::read_to_string(&l.path).unwrap(),
            "[\n  {\n    \"key\": \"k1\",\n    \"label\": \"k1\"\n  },\n  {\n    \"key\": \"k2\",\n    \"label\": \"5\"\n  }\n]"
        );
    }
}
