//! `peripherals.json`: which drivers are disabled, and each driver's own settings
//! (`{ disabled: { id: true }, drivers: { id: { key: value } } }`, as core.ts writes it).
//!
//! Saving merges into what is on disk instead of overwriting it: only the entries of the
//! drivers this process runs are replaced. Entries for drivers it doesn't know are kept,
//! so in proxy mode the Bun server can own its drivers' entries in the same file, and
//! switching back to Bun loses nothing. Writes are debounced (200 ms) like core.ts's.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value};

#[derive(Clone)]
pub struct Settings {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    data: Mutex<Data>,
    /// The drivers whose entries this process owns.
    owned: Mutex<HashSet<String>>,
    pending: Mutex<bool>,
}

#[derive(Default, Clone)]
struct Data {
    disabled: Map<String, Value>,
    drivers: Map<String, Value>,
}

fn read(path: &PathBuf) -> Data {
    let v: Value = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    let obj = |k: &str| v.get(k).and_then(Value::as_object).cloned().unwrap_or_default();
    Data { disabled: obj("disabled"), drivers: obj("drivers") }
}

impl Settings {
    pub fn load(path: PathBuf) -> Self {
        let data = read(&path);
        Self { inner: Arc::new(Inner { path, data: Mutex::new(data), owned: Mutex::new(HashSet::new()), pending: Mutex::new(false) }) }
    }

    /// Mark a driver's entries as this process's to write.
    pub fn own(&self, id: &str) {
        self.inner.owned.lock().unwrap().insert(id.to_string());
    }

    pub fn is_disabled(&self, id: &str) -> bool {
        self.inner.data.lock().unwrap().disabled.get(id).is_some_and(crate::http::truthy)
    }

    pub fn set_disabled(&self, id: &str, disabled: bool) {
        {
            let mut d = self.inner.data.lock().unwrap();
            if disabled {
                d.disabled.insert(id.to_string(), Value::Bool(true));
            } else {
                d.disabled.remove(id);
            }
        }
        self.save();
    }

    pub fn get(&self, id: &str, key: &str) -> Option<Value> {
        self.inner.data.lock().unwrap().drivers.get(id).and_then(|m| m.get(key)).cloned()
    }

    pub fn set(&self, id: &str, key: &str, value: Value) {
        {
            let mut d = self.inner.data.lock().unwrap();
            let e = d.drivers.entry(id.to_string()).or_insert_with(|| Value::Object(Map::new()));
            if !e.is_object() {
                *e = Value::Object(Map::new());
            }
            e.as_object_mut().unwrap().insert(key.to_string(), value);
        }
        self.save();
    }

    /// Schedule a write (debounced). Needs a tokio runtime; without one it writes now.
    pub fn save(&self) {
        {
            let mut p = self.inner.pending.lock().unwrap();
            if *p {
                return;
            }
            *p = true;
        }
        let me = self.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    me.flush();
                });
            }
            Err(_) => me.flush(),
        }
    }

    /// Write now: re-read the file, replace this process's drivers' entries, write it back.
    pub fn flush(&self) {
        *self.inner.pending.lock().unwrap() = false;
        let owned = self.inner.owned.lock().unwrap().clone();
        let mine = self.inner.data.lock().unwrap().clone();
        let mut disk = read(&self.inner.path);
        let ids: HashSet<String> =
            owned.iter().cloned().chain(mine.drivers.keys().filter(|k| !disk.drivers.contains_key(*k)).cloned()).collect();
        for id in &ids {
            match mine.drivers.get(id) {
                Some(v) => {
                    disk.drivers.insert(id.clone(), v.clone());
                }
                None => {
                    disk.drivers.remove(id);
                }
            }
        }
        for id in &owned {
            match mine.disabled.get(id) {
                Some(v) => {
                    disk.disabled.insert(id.clone(), v.clone());
                }
                None => {
                    disk.disabled.remove(id);
                }
            }
        }
        let out = serde_json::json!({ "disabled": disk.disabled, "drivers": disk.drivers });
        // JSON.stringify(…, null, 2)
        let text = serde_json::to_string_pretty(&out).unwrap_or_default();
        if let Err(e) = write_atomic(&self.inner.path, text.as_bytes()) {
            tracing::warn!("saving {}: {e}", self.inner.path.display());
        }
        // Pick up what the other process wrote meanwhile, for drivers we don't own.
        let mut d = self.inner.data.lock().unwrap();
        for (k, v) in disk.drivers {
            if !owned.contains(&k) {
                d.drivers.insert(k, v);
            }
        }
    }
}

/// Write via a temp file and rename, so a reader never sees half a file.
pub fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_on_write_keeps_other_processes_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peripherals.json");
        std::fs::write(&path, r#"{"disabled":{"serial":true},"drivers":{"gpio":{"outputEnabled":true},"serial":{"baud":9600}}}"#).unwrap();
        let s = Settings::load(path.clone());
        s.own("gpio");
        assert_eq!(s.get("gpio", "outputEnabled"), Some(Value::Bool(true)));
        // Another process changes serial's settings after we loaded the file.
        std::fs::write(&path, r#"{"disabled":{},"drivers":{"gpio":{"outputEnabled":true},"serial":{"baud":115200}}}"#).unwrap();
        s.set("gpio", "outputEnabled", Value::Bool(false));
        s.flush();
        let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["drivers"]["gpio"]["outputEnabled"], Value::Bool(false));
        assert_eq!(v["drivers"]["serial"]["baud"], 115200);
        assert!(v["disabled"].as_object().unwrap().is_empty());
    }
}
