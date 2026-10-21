//! The write paths and device actions, replayed against the TypeScript server's own code:
//! fixtures/scenarios.json says how commands answer, fixtures/harness.ts ran server.ts's
//! functions on them and wrote scenarios.expected.json (every command with its stdin,
//! every stream read, every file check, each with the operation's state at that moment,
//! and the outcome). Here the Rust functions run on the same scenarios and must produce
//! the same log and outcome.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use benchbob_core::http::num;
use serde_json::{Map, Value, json};

use crate::bootops;
use crate::ops::{Op, OpData};
use crate::rpiboot::{BootDevice, CardFiles, EepromImage, Firmware, Hooks, RpibootRun};
use crate::sys::mock::{Mock, Shared};

struct MockFw {
    mock: Arc<Mock>,
    scn: Value,
    images: Value,
}

fn run_from(v: &Value) -> RpibootRun {
    RpibootRun {
        code: v.get("code").and_then(Value::as_i64).unwrap_or(0) as i32,
        log: vec![],
        metadata: v.get("metadata").and_then(Value::as_object).cloned(),
        timed_out: v.get("timedOut").and_then(Value::as_bool).unwrap_or(false),
        image_sha256: v.get("imageSha256").and_then(Value::as_str).map(String::from),
    }
}

#[async_trait]
impl Firmware for MockFw {
    async fn installed(&self) -> bool {
        true
    }
    async fn boot_devices(&self) -> Vec<BootDevice> {
        self.scn
            .get("bootDevices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|d| BootDevice {
                path: d["path"].as_str().unwrap_or("").into(),
                chip: d["chip"].as_str().unwrap_or("").into(),
                label: d["label"].as_str().unwrap_or("").into(),
                serial: d["serial"].as_str().map(String::from),
                product: d["product"].as_str().map(String::from),
            })
            .collect()
    }
    async fn eeprom_images(&self, chip: &str) -> Vec<EepromImage> {
        self.images.get(chip).and_then(Value::as_array).into_iter().flatten().map(EepromImage::from_json).collect()
    }
    async fn default_boot_conf(&self, _chip: &str) -> String {
        String::new()
    }
    async fn build_card_files(&self, chip: &str, image: &EepromImage, boot_conf: &str) -> anyhow::Result<CardFiles> {
        self.mock.record(json!({ "buildCardFiles": [chip, image.id, boot_conf] }));
        let mut files: Vec<String> = ["recovery.bin", "pieeprom.bin", "pieeprom.sig"].iter().map(|s| s.to_string()).collect();
        if chip == "2711" {
            files.extend(["vl805.bin".to_string(), "vl805.sig".to_string()]);
        }
        files.push("README.txt".into());
        Ok(CardFiles { dir: "/tmp/benchbob-rpiboot/card".into(), files, image_sha256: "sha".into() })
    }
    async fn load_mass_storage(&self, dev: &BootDevice, _: &Hooks) -> RpibootRun {
        self.mock.record(json!({ "rpi": "loadMassStorage", "path": dev.path }));
        run_from(&self.scn["rpiboot"])
    }
    async fn read_bootloader_info(&self, dev: &BootDevice, _: &Hooks) -> anyhow::Result<RpibootRun> {
        self.mock.record(json!({ "rpi": "readBootloaderInfo", "path": dev.path }));
        Ok(run_from(&self.scn["rpiboot"]))
    }
    async fn write_bootloader(&self, dev: &BootDevice, image: &EepromImage, boot_conf: &str, _: &Hooks) -> anyhow::Result<RpibootRun> {
        self.mock.record(json!({ "rpi": "writeBootloader", "path": dev.path, "image": image.id, "bootConf": boot_conf }));
        Ok(run_from(&self.scn["rpiboot"]))
    }
    async fn record_write(&self, rec: Value) -> anyhow::Result<()> {
        self.mock.record(json!({ "rpi": "recordWrite", "record": rec }));
        Ok(())
    }
    async fn identify(&self, chip: &str, _: Option<&Map<String, Value>>) -> Value {
        self.mock.record(json!({ "rpi": "identifyBootloader", "chip": chip }));
        json!({ "source": "unknown", "buildDate": null, "label": "x", "bootConf": null })
    }
}

fn op(kind: &str, device: &str, f: impl FnOnce(&mut OpData)) -> Op {
    let mut o = OpData {
        id: "op".into(),
        kind: kind.into(),
        device: device.into(),
        status: "starting".into(),
        message: "Starting...".into(),
        ..Default::default()
    };
    f(&mut o);
    Arc::new(Mutex::new(o))
}

/// Run an operation as the harness does: snapshot it on every log entry, turn an error
/// into its message.
async fn run_op<F: std::future::Future<Output = anyhow::Result<()>>>(m: &Arc<Mock>, op: &Op, fut: impl FnOnce(Op) -> F) -> Value {
    let o = op.clone();
    *m.snap.lock().unwrap() = Some(Box::new(move || {
        let o = o.lock().unwrap();
        json!({ "status": o.status, "progress": num(o.progress), "message": o.message })
    }));
    if let Err(e) = fut(op.clone()).await {
        let mut o = op.lock().unwrap();
        if o.status != "error" {
            o.status = "error".into();
            o.message = e.to_string();
        }
    }
    *m.snap.lock().unwrap() = None;
    op.lock().unwrap().public()
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

#[tokio::test]
async fn scenarios_match_server_ts() {
    let all: Vec<Value> = serde_json::from_str(include_str!("../fixtures/scenarios.json")).unwrap();
    let want: Value = serde_json::from_str(include_str!("../fixtures/scenarios.expected.json")).unwrap();
    let images: Value = serde_json::from_str(include_str!("../fixtures/eeprom-images.json")).unwrap();
    let mut failed = Vec::new();
    for scn in &all {
        let name = s(scn, "name");
        let m = Mock::new(scn);
        let sys = Shared(m.clone());
        let fw = MockFw { mock: m.clone(), scn: scn.clone(), images: images.clone() };
        let a = &scn["args"];
        let dev = s(a, "device");
        let result = match s(scn, "fn").as_str() {
            "erase" => {
                let o = op("erase", &dev, |o| o.mode = a.get("mode").and_then(Value::as_str).map(String::from));
                run_op(&m, &o, |o| async move { crate::erase::erase(&sys, &o).await }).await
            }
            "flash" => {
                let o = op("flash", &dev, |o| {
                    o.image_url = s(a, "imageUrl");
                    o.image_name = a.get("imageName").cloned();
                    o.extract_size = a.get("extractSize").and_then(Value::as_f64).unwrap_or(0.0);
                    o.init_format = a.get("initFormat").and_then(Value::as_str).map(String::from);
                    o.settings = a.get("settings").cloned();
                });
                run_op(&m, &o, |o| async move { crate::flash::flash(&sys, &o, crate::custom::tests::key).await }).await
            }
            "blcard" => {
                let o = op("blcard", &dev, |o| {
                    o.chip = Some(s(a, "chip"));
                    o.eeprom_image_id = a.get("imageId").cloned();
                    o.boot_conf = Some(s(a, "bootConf"));
                });
                run_op(&m, &o, |o| async move { bootops::bootloader_card(&sys, &fw, &o).await }).await
            }
            "deepscan" => {
                let o = op("deepscan", &dev, |_| {});
                run_op(&m, &o, |o| async move { crate::probe::deep_scan(&sys, &o).await.map(|_| ()) }).await
            }
            "rpiboot" => {
                let o = op(&s(a, "type"), &format!("usb:{}", s(a, "usbPath")), |o| {
                    o.usb_path = Some(s(a, "usbPath"));
                    o.eeprom_image_id = a.get("imageId").cloned();
                    o.boot_conf = a.get("bootConf").and_then(Value::as_str).map(String::from);
                });
                let results = bootops::Results::default();
                let r = run_op(&m, &o, |o| {
                    let (sys, fw, results) = (&sys, &fw, &results);
                    async move { bootops::rpiboot(sys, fw, &o, results).await }
                })
                .await;
                let res: Vec<Value> = results
                    .lock()
                    .unwrap()
                    .values()
                    .map(|v| {
                        let mut v = v.clone();
                        v["at"] = "<at>".into();
                        v
                    })
                    .collect();
                json!({ "op": r, "results": res })
            }
            "eject" => crate::devices::eject(&sys, &dev).await,
            "scan" => crate::devices::scan(&sys, &dev).await,
            "probe" => crate::probe::probe(&sys, &dev).await,
            "listDir" => {
                let f = crate::files::Files { sys: &sys, root: "/srv/nfs/drives".into() };
                match f.list(&s(a, "path")).await {
                    Ok(v) => v,
                    Err(e) => crate::files::error_json(&e),
                }
            }
            "download" => {
                let f = crate::files::Files { sys: &sys, root: "/srv/nfs/drives".into() };
                let method = if s(a, "method") == "HEAD" { axum::http::Method::HEAD } else { axum::http::Method::GET };
                let range = a.get("range").and_then(Value::as_str);
                match f.download(&s(a, "path"), &method, range).await {
                    Ok(r) => {
                        let headers: Map<String, Value> = r
                            .headers()
                            .iter()
                            .map(|(k, v)| (k.as_str().to_string(), Value::String(v.to_str().unwrap_or("").into())))
                            .collect();
                        json!({ "status": r.status().as_u16(), "headers": headers })
                    }
                    Err(e) => crate::files::error_json(&e),
                }
            }
            other => panic!("unknown scenario fn {other}"),
        };
        let w = &want[&name];
        let log = Value::Array(m.entries());
        if log != w["log"] {
            let (got, exp) = (log.as_array().unwrap(), w["log"].as_array().unwrap());
            let at = got.iter().zip(exp).position(|(a, b)| a != b).unwrap_or(got.len().min(exp.len()));
            failed.push(format!(
                "{name}: log differs at entry {at} (rust {} entries, ts {}):\n  rust {}\n  ts   {}",
                got.len(),
                exp.len(),
                got.get(at).map(|v| v.to_string()).unwrap_or_default(),
                exp.get(at).map(|v| v.to_string()).unwrap_or_default()
            ));
        }
        // Key order counts, except for headers (the harness lists them sorted).
        let same = if s(scn, "fn") == "download" {
            result == w["result"]
        } else {
            serde_json::to_string(&result).unwrap() == serde_json::to_string(&w["result"]).unwrap()
        };
        if !same {
            failed.push(format!("{name}: result differs:\n  rust {result}\n  ts   {}", w["result"]));
        }
    }
    assert!(failed.is_empty(), "{} of {} scenarios differ:\n{}", failed.len(), all.len(), failed.join("\n"));
}
