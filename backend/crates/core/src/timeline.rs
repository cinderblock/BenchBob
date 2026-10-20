//! The hardware event timeline (`GET /api/periph/timeline`): USB plug/unplug, driver
//! events. The last 300 entries, oldest first.

use std::sync::Mutex;

use serde::Serialize;
use serde_json::Value;

const MAX: usize = 300;

#[derive(Clone, Serialize)]
pub struct Entry {
    pub t: u64,
    pub source: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

#[derive(Default)]
pub struct Timeline {
    entries: Mutex<Vec<Entry>>,
}

/// Milliseconds since the Unix epoch (`Date.now()`).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Timeline {
    pub fn add(&self, source: &str, text: &str, detail: Option<Value>) {
        let mut e = self.entries.lock().unwrap();
        e.push(Entry { t: now_ms(), source: source.into(), text: text.into(), detail });
        if e.len() > MAX {
            let n = e.len() - MAX;
            e.drain(..n);
        }
    }
    pub fn list(&self) -> Vec<Entry> {
        self.entries.lock().unwrap().clone()
    }
}
