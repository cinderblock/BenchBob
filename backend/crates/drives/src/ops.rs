//! The operation queue (server.ts "Operation Queue"): erase, flash, deep scan, bootloader
//! cards and rpiboot runs, one at a time per device, the rest queued behind. A finished
//! operation stays visible for 30 s, then is reaped.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use benchbob_core::driver::BoxFut;
use benchbob_core::http::num;
use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::sys::Killer;

#[derive(Default)]
pub struct OpData {
    pub id: String,
    /// erase | flash | deepscan | blcard | msd | eeprom-check | eeprom-write
    pub kind: String,
    pub device: String,
    pub status: String,
    pub progress: f64,
    pub message: String,
    pub mode: Option<String>,
    pub image_url: String,
    /// As the client sent it (any JSON); omitted from the status when absent.
    pub image_name: Option<Value>,
    pub extract_size: f64,
    pub init_format: Option<String>,
    pub settings: Option<Value>,
    pub scan_result: Option<Value>,
    pub usb_path: Option<String>,
    pub eeprom_image_id: Option<Value>,
    pub boot_conf: Option<String>,
    pub chip: Option<String>,
    pub aborted: bool,
    pub killer: Option<Killer>,
}

pub type Op = Arc<Mutex<OpData>>;

pub fn new_op(kind: &str, device: &str) -> OpData {
    OpData {
        id: uuid::Uuid::new_v4().to_string(),
        kind: kind.into(),
        device: device.into(),
        status: "starting".into(),
        message: "Starting...".into(),
        ..Default::default()
    }
}

impl OpData {
    /// `opPublic`: undefined fields (imageName, scanResult) are left out, as JSON.stringify does.
    pub fn public(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), self.id.clone().into());
        m.insert("type".into(), self.kind.clone().into());
        m.insert("device".into(), self.device.clone().into());
        m.insert("status".into(), self.status.clone().into());
        m.insert("progress".into(), num(self.progress));
        m.insert("message".into(), self.message.clone().into());
        if let Some(n) = &self.image_name {
            m.insert("imageName".into(), n.clone());
        }
        if let Some(s) = &self.scan_result {
            m.insert("scanResult".into(), s.clone());
        }
        Value::Object(m)
    }

    pub fn set(&mut self, status: &str, progress: Option<f64>, message: &str) {
        if !status.is_empty() {
            self.status = status.into();
        }
        if let Some(p) = progress {
            self.progress = p;
        }
        self.message = message.into();
    }

    fn active(&self) -> bool {
        !matches!(self.status.as_str(), "done" | "error" | "queued")
    }
}

/// Runs one operation to completion; an error becomes the op's message.
pub type Runner = Arc<dyn Fn(Op) -> BoxFut<anyhow::Result<()>> + Send + Sync>;

pub struct Queue {
    devices: Mutex<IndexMap<String, Vec<Op>>>,
    runner: Mutex<Option<Runner>>,
    reap: Duration,
}

impl Queue {
    pub fn new(reap: Duration) -> Arc<Self> {
        Arc::new(Self { devices: Default::default(), runner: Mutex::new(None), reap })
    }

    pub fn set_runner(&self, r: Runner) {
        *self.runner.lock().unwrap() = Some(r);
    }

    pub fn for_device(&self, device: &str) -> Vec<Op> {
        self.devices.lock().unwrap().get(device).cloned().unwrap_or_default()
    }

    pub fn all(&self) -> Vec<Op> {
        self.devices.lock().unwrap().values().flatten().cloned().collect()
    }

    pub fn find(&self, id: &str) -> Option<Op> {
        self.all().into_iter().find(|o| o.lock().unwrap().id == id)
    }

    /// `isDeviceBusy`: something other than done/error/queued.
    pub fn busy(&self, device: &str) -> bool {
        self.for_device(device).iter().any(|o| o.lock().unwrap().active())
    }

    pub fn has_queued(&self, device: &str) -> bool {
        self.for_device(device).iter().any(|o| o.lock().unwrap().status == "queued")
    }

    pub fn has_queued_flash(&self, device: &str) -> bool {
        self.for_device(device).iter().any(|o| {
            let o = o.lock().unwrap();
            o.kind == "flash" && o.status == "queued"
        })
    }

    /// Run now, or queue behind the device's running operation.
    pub fn enqueue(self: &Arc<Self>, op: OpData) -> Op {
        let device = op.device.clone();
        let op = Arc::new(Mutex::new(op));
        let start = {
            let mut d = self.devices.lock().unwrap();
            let ops = d.entry(device).or_default();
            let running = ops.iter().find(|o| o.lock().unwrap().active()).map(|o| o.lock().unwrap().kind.clone());
            ops.push(op.clone());
            match running {
                None => true,
                Some(k) => {
                    let mut o = op.lock().unwrap();
                    o.status = "queued".into();
                    o.message = format!("Queued after {k}");
                    false
                }
            }
        };
        if start {
            self.exec(op.clone());
        }
        op
    }

    fn exec(self: &Arc<Self>, op: Op) {
        let me = self.clone();
        let Some(runner) = self.runner.lock().unwrap().clone() else { return };
        tokio::spawn(async move {
            if let Err(e) = runner(op.clone()).await {
                let mut o = op.lock().unwrap();
                if o.status != "error" {
                    o.status = "error".into();
                    o.message = e.to_string();
                }
            }
            let device = op.lock().unwrap().device.clone();
            {
                let (q, op, device) = (me.clone(), op.clone(), device.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(q.reap).await;
                    let mut d = q.devices.lock().unwrap();
                    if let Some(ops) = d.get_mut(&device) {
                        ops.retain(|o| !Arc::ptr_eq(o, &op));
                        if ops.is_empty() {
                            d.shift_remove(&device);
                        }
                    }
                });
            }
            let next = me.for_device(&device).into_iter().find(|o| o.lock().unwrap().status == "queued");
            if let Some(n) = next {
                me.exec(n);
            }
        });
    }

    /// `/api/cancel`: flag it, kill its process, mark it cancelled.
    pub fn cancel(&self, id: &str) {
        if let Some(op) = self.find(id) {
            let k = {
                let mut o = op.lock().unwrap();
                o.aborted = true;
                o.status = "error".into();
                o.message = "Cancelled".into();
                o.killer.clone()
            };
            if let Some(k) = k {
                k();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queues_behind_the_running_op_and_reaps() {
        let q = Queue::new(Duration::from_millis(50));
        let (tx, rx) = tokio::sync::watch::channel(false);
        q.set_runner(Arc::new(move |op: Op| {
            let mut rx = rx.clone();
            Box::pin(async move {
                op.lock().unwrap().status = "erasing".into();
                let _ = rx.wait_for(|v| *v).await;
                let kind = op.lock().unwrap().kind.clone();
                if kind == "flash" {
                    anyhow::bail!("Flash failed");
                }
                op.lock().unwrap().status = "done".into();
                Ok(())
            })
        }));
        q.enqueue(new_op("erase", "sdb"));
        let f = q.enqueue(new_op("flash", "sdb"));
        tokio::task::yield_now().await;
        assert!(q.busy("sdb"));
        assert!(q.has_queued_flash("sdb"));
        assert_eq!(f.lock().unwrap().message, "Queued after erase");
        tx.send(true).unwrap();
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            if f.lock().unwrap().status == "error" {
                break;
            }
        }
        assert_eq!(f.lock().unwrap().message, "Flash failed");
        assert_eq!(f.lock().unwrap().public()["type"], "flash");
        assert!(f.lock().unwrap().public().get("imageName").is_none());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(q.all().is_empty());
    }
}
