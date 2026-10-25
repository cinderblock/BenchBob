//! Drives, flashing and the Pi boot tools: everything server.ts did itself rather than
//! through a peripheral driver.
//!
//! - the USB drives in `/api/status` (`rootDevice`, `drives`, `nbdExports`, `operations`,
//!   `filesRoot`) and the devices in USB boot mode (`bootDevices`, `rpibootInstalled`,
//!   `rpibootResults`);
//! - eject, re-scan, the raw sector probe and the deep scan;
//! - erase and flash (curl download, decompression into dd, then the dashboard's settings
//!   written for cloud-init or firstrun.sh, including "Install BenchBob");
//! - rpiboot (mass-storage gadget, bootloader EEPROM check and write) and bootloader
//!   update SD cards;
//! - the Raspberry Pi image catalog, the flash picker's favorites and Quick Pi list, the
//!   default flash settings;
//! - the read-only file browser and downloads, and OS detection on a drive.
//!
//! It isn't a dashboard panel: it has no `peripherals` row and can't be disabled
//! ([`Driver::listed`] is false), and its status fields are top-level ones the binary adds
//! through [`Drives::status_fields`]. Every effect on the system goes through [`sys::Sys`],
//! which is how the write paths are tested against what the TypeScript server did (see
//! the note on the frozen fixtures in `scenario_tests.rs`) without a disk in sight.

pub mod bootops;
pub mod catalog;
pub mod custom;
pub mod devices;
pub mod erase;
pub mod files;
pub mod firstboot;
pub mod flash;
pub mod ops;
pub mod osinfo;
pub mod probe;
pub mod regf;
pub mod rpiboot;
#[cfg(test)]
mod scenario_tests;
pub mod sys;
pub mod util;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::Method;
use benchbob_core::http::{HttpError, Reply, js_number, js_string, json_response, text_response, truthy};
use benchbob_core::{Ctx, Driver, Json, Request, Route, RouteResult, json};
use indexmap::IndexMap;
use serde_json::{Map, Value, json as j};

use crate::bootops::{CARD_PART_MIN, MIB, Results};
use crate::catalog::{Catalog, ImageList};
use crate::devices::{is_sys_dev, valid_name};
use crate::ops::{Op, Queue, new_op};
use crate::osinfo::OsInfo;
use crate::rpiboot::{EepromImage, Firmware, Rpi, card_board};
use crate::sys::{Real, Sys};

/// lsblk columns OS detection reads.
const OSINFO_COLS: &str = "NAME,TYPE,TRAN,FSTYPE,LABEL,UUID,PARTUUID,PARTTYPE,PARTLABEL,MOUNTPOINT,PTTYPE,PTUUID";

type OsCell = Arc<tokio::sync::OnceCell<Result<Value, (u16, String)>>>;

pub struct Inner {
    sys: Arc<dyn Sys>,
    fw: Arc<dyn Firmware>,
    queue: Arc<Queue>,
    catalog: Catalog,
    /// Deep-scan findings, kept per device so they outlive the operation record.
    scans: Mutex<HashMap<String, Value>>,
    rpiboot_results: Arc<Results>,
    osinfo: Mutex<HashMap<String, (String, OsCell)>>,
    files_root: String,
    lists: Vec<(&'static str, ImageList)>,
    defaults_path: std::path::PathBuf,
    ctx: OnceLock<Ctx>,
}

pub struct Drives {
    inner: Arc<Inner>,
}

impl Default for Drives {
    fn default() -> Self {
        Self::new()
    }
}

fn body_str(a: &Value, k: &str) -> Option<String> {
    a.get(k).and_then(Value::as_str).map(String::from)
}

fn invalid() -> HttpError {
    HttpError::new(400, "Invalid device")
}

/// The `device` of a request: a plain kernel disk name, never a system disk. (server.ts
/// only refused system disks; the name then went into commands, two of them through a
/// shell, so here it must also look like a disk name.)
fn device_arg(a: &Value) -> Result<String, HttpError> {
    let d = body_str(a, "device").unwrap_or_default();
    if d.is_empty() || is_sys_dev(&d) || !valid_name(&d) {
        return Err(invalid());
    }
    Ok(d)
}

impl Inner {
    /// [`device_arg`], and the device isn't one this machine depends on (its root disk,
    /// its eMMC). Every route that touches a device goes through here.
    async fn device(&self, a: &Value) -> Result<String, HttpError> {
        let d = device_arg(a)?;
        match devices::refuse(self.sys.as_ref(), &d).await {
            Some(why) => Err(HttpError::new(400, why)),
            None => Ok(d),
        }
    }
}

fn busy() -> HttpError {
    HttpError::new(409, "Device busy")
}

impl Drives {
    pub fn new() -> Self {
        let sys: Arc<dyn Sys> = Arc::new(Real);
        let fw: Arc<dyn Firmware> = Arc::new(Rpi::new(rpiboot::Paths::default(), sys.clone()));
        Self::with(sys, fw)
    }

    pub fn with(sys: Arc<dyn Sys>, fw: Arc<dyn Firmware>) -> Self {
        let inner = Arc::new(Inner {
            sys,
            fw,
            queue: Queue::new(Duration::from_secs(30)),
            catalog: Catalog::default(),
            scans: Default::default(),
            rpiboot_results: Default::default(),
            osinfo: Default::default(),
            files_root: files::files_root(),
            lists: catalog::image_lists(),
            defaults_path: benchbob_core::paths::state_path("defaults.json"),
            ctx: OnceLock::new(),
        });
        let weak = Arc::downgrade(&inner);
        inner.queue.set_runner(Arc::new(move |op: Op| {
            let w = weak.clone();
            Box::pin(async move {
                match w.upgrade() {
                    Some(i) => i.run_op(op).await,
                    None => Ok(()),
                }
            })
        }));
        Self { inner }
    }

    /// The top-level `/api/status` fields, in server.ts's order. `rates` are the disks'
    /// read/write byte rates; `root` the disk this machine runs from (never offered).
    pub async fn status_fields(&self, rates: &IndexMap<String, (f64, f64)>, root: Option<&str>) -> Vec<(&'static str, Value)> {
        let i = &self.inner;
        let sys = i.sys.as_ref();
        let ops = |dev: &str| i.queue.for_device(dev).iter().map(|o| o.lock().unwrap().public()).collect::<Vec<_>>();
        let lsblk = sys.run(&["lsblk", "-J", "-b", "-o", devices::LSBLK_COLS]).await.out;
        let ((root_dev, drives), nbd, boot, installed) = tokio::join!(
            devices::devices(sys, &lsblk, rates, root, &ops),
            devices::nbd_exports(sys),
            i.fw.boot_devices(),
            i.fw.installed()
        );
        vec![
            ("rootDevice", root_dev),
            ("drives", drives),
            ("nbdExports", nbd),
            ("bootDevices", Value::Array(boot.iter().map(|d| d.to_json()).collect())),
            ("rpibootInstalled", installed.into()),
            ("rpibootResults", Value::Array(i.rpiboot_results.lock().unwrap().values().cloned().collect())),
            ("filesRoot", i.files_root.clone().into()),
            ("operations", Value::Array(i.queue.all().iter().map(|o| o.lock().unwrap().public()).collect())),
        ]
    }

    fn route<F, Fut>(&self, method: Method, path: &str, f: F) -> Route
    where
        F: Fn(Arc<Inner>, Request) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = RouteResult> + Send + 'static,
    {
        let inner = self.inner.clone();
        Route::new(method, path, None, move |r| f(inner.clone(), r))
    }
}

impl Inner {
    async fn run_op(self: &Arc<Self>, op: Op) -> anyhow::Result<()> {
        let kind = op.lock().unwrap().kind.clone();
        let sys = self.sys.as_ref();
        match kind.as_str() {
            "erase" => erase::erase(sys, &op).await,
            "flash" => flash::flash(sys, &op, firstboot::archive_key).await,
            "deepscan" => {
                let r = probe::deep_scan(sys, &op).await?;
                let dev = op.lock().unwrap().device.clone();
                self.scans.lock().unwrap().insert(dev, r);
                Ok(())
            }
            "blcard" => bootops::bootloader_card(sys, self.fw.as_ref(), &op).await,
            _ => bootops::rpiboot(sys, self.fw.as_ref(), &op, &self.rpiboot_results).await,
        }
    }

    fn files(&self) -> files::Files<'_> {
        files::Files { sys: self.sys.as_ref(), root: self.files_root.clone() }
    }

    /// `getOsInfo`: cached per disk until its set of mounts changes (a remount gets a new
    /// mount ID, so it counts as a change).
    async fn os_info(&self, device: &str) -> Result<Value, (u16, String)> {
        let out = self.sys.run(&["lsblk", "-J", "-o", OSINFO_COLS, &format!("/dev/{device}")]).await.out;
        let disk = serde_json::from_str::<Value>(&out).ok().and_then(|v| v.get("blockdevices")?.get(0).cloned());
        // A USB disk, or a card in the SD slot (the caller has refused the root disk and eMMC).
        let Some(disk) = disk.filter(|d| {
            d.get("type").and_then(Value::as_str) == Some("disk")
                && (d.get("tran").and_then(Value::as_str) == Some("usb") || device.starts_with("mmcblk"))
        }) else {
            return Err((404, "Not a USB disk or SD card".into()));
        };
        let ids = mount_ids(&self.sys.read_text("/proc/self/mountinfo").await);
        let key: Vec<Value> = disk
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|p| {
                let mp = p.get("mountpoint").cloned().unwrap_or(Value::Null);
                let id = mp.as_str().and_then(|m| ids.get(m)).cloned().map(Value::String).unwrap_or(Value::Null);
                j!([p.get("name"), p.get("fstype"), mp, id])
            })
            .collect();
        let key = serde_json::to_string(&key).unwrap_or_default();
        let cell = {
            let mut c = self.osinfo.lock().unwrap();
            match c.get(device) {
                Some((k, cell)) if *k == key => cell.clone(),
                _ => {
                    let cell: OsCell = Default::default();
                    c.insert(device.into(), (key, cell.clone()));
                    cell
                }
            }
        };
        let root = self.files_root.clone();
        let now = self.sys.now_ms();
        let r = cell
            .get_or_init(|| async move {
                tokio::task::spawn_blocking(move || OsInfo::new(root).analyse(&disk, now)).await.map_err(|e| (500, e.to_string()))
            })
            .await
            .clone();
        if r.is_err() {
            // A failure shouldn't stick: the next request tries again.
            let mut c = self.osinfo.lock().unwrap();
            if c.get(device).is_some_and(|(_, x)| Arc::ptr_eq(x, &cell)) {
                c.remove(device);
            }
        }
        r
    }

    async fn image(&self, chip: &str, id: Option<&Value>) -> Option<EepromImage> {
        self.fw.eeprom_images(chip).await.into_iter().find(|i| id.and_then(Value::as_str) == Some(i.id.as_str()))
    }

    fn changed(&self) {
        if let Some(c) = self.ctx.get() {
            c.changed();
        }
    }
}

/// Mount point -> mount ID, from /proc/self/mountinfo (points are octal-escaped).
pub fn mount_ids(text: &str) -> HashMap<String, String> {
    let esc = regex::Regex::new(r"\\(\d{3})").unwrap();
    let mut ids = HashMap::new();
    for line in text.split('\n') {
        let f: Vec<&str> = line.split(' ').collect();
        if f.len() > 4 {
            let mp = esc.replace_all(f[4], |c: &regex::Captures| {
                u32::from_str_radix(&c[1], 8).ok().and_then(char::from_u32).map(String::from).unwrap_or_default()
            });
            ids.insert(mp.into_owned(), f[0].to_string());
        }
    }
    ids
}

/// JavaScript `length`: UTF-16 code units.
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn boot_conf_ok(c: Option<&Value>) -> Option<String> {
    c.and_then(Value::as_str).filter(|s| !s.trim().is_empty() && js_len(s) <= 4000).map(String::from)
}

fn ok_json(mut extra: Map<String, Value>) -> Value {
    let mut m = Map::new();
    m.insert("ok".into(), true.into());
    m.append(&mut extra);
    Value::Object(m)
}

#[async_trait]
impl Driver for Drives {
    fn id(&self) -> &'static str {
        "drives"
    }
    fn title(&self) -> &'static str {
        "Drives"
    }
    fn description(&self) -> &'static str {
        "USB drives, flashing, rpiboot and bootloader cards, files, OS detection"
    }
    fn listed(&self) -> bool {
        false
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let _ = self.inner.ctx.set(ctx);
        Ok(())
    }

    fn routes(&self) -> Vec<Route> {
        let mut r = vec![
            self.route(Method::POST, "/api/eject", |i, r| async move {
                let a = r.json()?;
                let d = i.device(&a).await?;
                if i.queue.busy(&d) {
                    return Err(busy());
                }
                let res = devices::eject(i.sys.as_ref(), &d).await;
                i.changed();
                json(res)
            }),
            self.route(Method::POST, "/api/erase", |i, r| async move {
                let a = r.json()?;
                let d = i.device(&a).await?;
                if i.queue.busy(&d) {
                    return Err(busy());
                }
                let mut op = new_op("erase", &d);
                op.mode = Some(a.get("mode").filter(|m| truthy(m)).map(js_string).unwrap_or_else(|| "quick".into()));
                i.queue.enqueue(op);
                i.changed();
                json(j!({ "ok": true, "message": "Erase started" }))
            }),
            self.route(Method::POST, "/api/flash", |i, r| async move {
                let a = r.json()?;
                let d = i.device(&a).await?;
                let url = a.get("imageUrl").filter(|u| truthy(u)).map(js_string);
                let Some(url) = url else { return Err(HttpError::bad_request("No image URL")) };
                // Queueing after an erase is fine; not when something else runs or waits.
                let ops = i.queue.for_device(&d);
                let blocked = ops.iter().any(|o| {
                    let o = o.lock().unwrap();
                    (!matches!(o.status.as_str(), "done" | "error" | "queued") && o.kind != "erase") || o.status == "queued"
                });
                if blocked {
                    return Err(busy());
                }
                if flash::file_name(&url).is_err() {
                    return Err(HttpError::bad_request("Bad image URL"));
                }
                let mut op = new_op("flash", &d);
                op.init_format = i.catalog.init_format(i.sys.as_ref(), &url).await;
                op.image_url = url;
                op.image_name = a.get("imageName").cloned();
                let ex = a.get("extractSize").map(js_number).unwrap_or(0.0);
                op.extract_size = if ex.is_nan() { 0.0 } else { ex };
                op.settings = a.get("settings").filter(|s| truthy(s)).cloned();
                i.queue.enqueue(op);
                i.changed();
                let msg = if i.queue.has_queued_flash(&d) { "Flash queued after erase" } else { "Flash started" };
                json(j!({ "ok": true, "message": msg }))
            }),
            self.route(Method::POST, "/api/scan", |i, r| async move {
                let d = i.device(&r.json()?).await?;
                json(devices::scan(i.sys.as_ref(), &d).await)
            }),
            self.route(Method::POST, "/api/probe", |i, r| async move {
                let d = i.device(&r.json()?).await?;
                json(probe::probe(i.sys.as_ref(), &d).await)
            }),
            self.route(Method::POST, "/api/deepscan", |i, r| async move {
                let d = i.device(&r.json()?).await?;
                if i.queue.busy(&d) {
                    return Err(busy());
                }
                let op = i.queue.enqueue(new_op("deepscan", &d));
                i.changed();
                let id = op.lock().unwrap().id.clone();
                json(j!({ "ok": true, "message": "Deep scan started", "operationId": id }))
            }),
            self.route(Method::GET, "/api/scanresult", |i, r| async move {
                let d = r.query("device").unwrap_or("").to_string();
                match i.scans.lock().unwrap().get(&d) {
                    Some(v) => json(ok_json(v.as_object().cloned().unwrap_or_default())),
                    None => json(j!({ "ok": false, "message": "No deep scan result for this device" })),
                }
            }),
            self.route(Method::POST, "/api/cancel", |i, r| async move {
                let a = r.json()?;
                if let Some(id) = a.get("operationId").and_then(Value::as_str) {
                    i.queue.cancel(id);
                }
                i.changed();
                json(j!({ "ok": true }))
            }),
            self.route(Method::GET, "/api/blcard/options", |i, r| async move {
                let chip = r.query("chip").unwrap_or("").to_string();
                if chip != "2711" && chip != "2712" {
                    return Err(HttpError::bad_request("chip must be 2711 or 2712"));
                }
                let images: Vec<Value> = i.fw.eeprom_images(&chip).await.iter().map(EepromImage::to_json).collect();
                json(j!({ "ok": true, "chip": chip, "board": card_board(&chip), "images": images }))
            }),
            self.route(Method::POST, "/api/blcard", |i, r| async move {
                let a = r.json()?;
                let sys = i.sys.as_ref();
                // Goes into sfdisk/mkfs arguments: a plain disk name that exists, nothing else.
                let d = i.device(&a).await?;
                if devices::size_bytes(sys, &d).await == 0.0 {
                    return Err(invalid());
                }
                let chip = body_str(&a, "chip").unwrap_or_default();
                if chip != "2711" && chip != "2712" {
                    return Err(HttpError::bad_request("chip must be 2711 or 2712"));
                }
                let Some(image) = i.image(&chip, a.get("imageId")).await else {
                    return Err(HttpError::bad_request("Unknown bootloader image"));
                };
                let Some(conf) = boot_conf_ok(a.get("bootConf")) else {
                    return Err(HttpError::bad_request("boot.conf is missing or too long"));
                };
                if devices::size_bytes(sys, &d).await < CARD_PART_MIN + MIB {
                    return Err(HttpError::bad_request("Card too small"));
                }
                if i.queue.busy(&d) || i.queue.has_queued(&d) {
                    return Err(busy());
                }
                let mut op = new_op("blcard", &d);
                op.chip = Some(chip.clone());
                op.eeprom_image_id = a.get("imageId").cloned();
                op.boot_conf = Some(conf);
                op.image_name = Some(format!("{} · {}", card_board(&chip), image.build_date).into());
                let op = i.queue.enqueue(op);
                i.changed();
                let id = op.lock().unwrap().id.clone();
                json(j!({ "ok": true, "message": "Writing bootloader card", "operationId": id }))
            }),
            self.route(Method::GET, "/api/osinfo", |i, r| async move {
                let d = i.device(&j!({ "device": r.query("device").unwrap_or("") })).await?;
                match i.os_info(&d).await {
                    Ok(v) => json(ok_json(v.as_object().cloned().unwrap_or_default())),
                    Err((code, m)) => Err(HttpError::new(code, m)),
                }
            }),
            self.route(Method::GET, "/api/files", |i, r| async move {
                let rel = r.query("path").unwrap_or("").to_string();
                json(i.files().list(&rel).await?)
            }),
            self.route(Method::GET, "/api/images", |i, _| async move { json(i.catalog.images(i.sys.as_ref()).await) }),
            self.route(Method::GET, "/api/catalog", |i, _| async move { json(i.catalog.catalog(i.sys.as_ref()).await) }),
            self.route(Method::GET, "/api/defaults", |i, _| async move {
                match std::fs::read(&i.defaults_path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
                    Some(v) => json(v),
                    None => json(catalog::default_settings(i.sys.as_ref()).await),
                }
            }),
            self.route(Method::POST, "/api/defaults", |i, r| async move {
                let a = r.json()?;
                std::fs::write(&i.defaults_path, util::js_pretty(&a)).map_err(|e| HttpError::new(500, e.to_string()))?;
                json(j!({ "ok": true }))
            }),
            // ---------- rpiboot ----------
            self.route(Method::GET, "/api/rpiboot/eeprom/options", |i, r| async move {
                let path = r.query("path").map(String::from);
                let Some(dev) = i.fw.boot_devices().await.into_iter().find(|d| Some(&d.path) == path.as_ref()) else {
                    return Err(HttpError::new(404, "No such device in USB boot mode"));
                };
                // A check (or write) on this port since it was last power-cycled is the only
                // source of what's installed.
                let last = i.rpiboot_results.lock().unwrap().get(&dev.path).cloned().filter(|l| l["chip"] == dev.chip.as_str());
                let images: Vec<Value> = i.fw.eeprom_images(&dev.chip).await.iter().map(EepromImage::to_json).collect();
                json(j!({
                    "ok": true, "chip": dev.chip, "images": images, "bootConf": i.fw.default_boot_conf(&dev.chip).await,
                    "installed": last.as_ref().map(|l| l["installed"].clone()).unwrap_or(Value::Null),
                    "checkedAt": last.as_ref().map(|l| l["at"].clone()).unwrap_or(Value::Null),
                }))
            }),
            self.route(Method::POST, "/api/rpiboot/results/clear", |i, r| async move {
                let a = r.json()?;
                let p = a.get("path").map(js_string).unwrap_or_default();
                i.rpiboot_results.lock().unwrap().shift_remove(&p);
                i.changed();
                json(j!({ "ok": true }))
            }),
        ];
        for (path, kind) in
            [("/api/rpiboot/msd", "msd"), ("/api/rpiboot/eeprom/check", "eeprom-check"), ("/api/rpiboot/eeprom/write", "eeprom-write")]
        {
            r.push(self.route(Method::POST, path, move |i, r| async move {
                if !i.fw.installed().await {
                    return Err(HttpError::new(501, "rpiboot is not installed (sudo apt install rpiboot)"));
                }
                let a = r.json()?;
                let path = a.get("path").and_then(Value::as_str).map(String::from);
                let Some(dev) = i.fw.boot_devices().await.into_iter().find(|d| Some(&d.path) == path.as_ref()) else {
                    return Err(HttpError::new(404, "No such device in USB boot mode"));
                };
                if kind != "msd" && dev.chip == "2835" {
                    return Err(HttpError::bad_request("CM1/CM3 have no bootloader EEPROM"));
                }
                let mut conf = a.get("bootConf").and_then(Value::as_str).map(String::from);
                if kind == "eeprom-write" {
                    if i.image(&dev.chip, a.get("imageId")).await.is_none() {
                        return Err(HttpError::bad_request("Unknown bootloader image"));
                    }
                    conf = Some(boot_conf_ok(a.get("bootConf")).ok_or_else(|| HttpError::bad_request("boot.conf is missing or too long"))?);
                }
                let key = format!("usb:{}", dev.path);
                if i.queue.busy(&key) || i.queue.has_queued(&key) {
                    return Err(busy());
                }
                let mut op = new_op(kind, &key);
                op.usb_path = Some(dev.path.clone());
                op.eeprom_image_id = a.get("imageId").cloned();
                op.boot_conf = conf;
                let op = i.queue.enqueue(op);
                i.changed();
                let id = op.lock().unwrap().id.clone();
                json(j!({ "ok": true, "message": "Started", "operationId": id }))
            }));
        }
        // Downloads: GET and HEAD; anything else is 405, as server.ts answers.
        for m in [Method::GET, Method::HEAD, Method::POST, Method::PUT, Method::DELETE] {
            r.push(self.route(m, "/api/download", |i, r| async move {
                if r.method != Method::GET && r.method != Method::HEAD {
                    return Ok(Reply::Response(text_response(405, "Method Not Allowed")));
                }
                let rel = r.query("path").unwrap_or("").to_string();
                let range = r.headers.get(axum::http::header::RANGE).and_then(|v| v.to_str().ok()).map(String::from);
                Ok(Reply::Response(i.files().download(&rel, &r.method, range.as_deref()).await?))
            }));
        }
        for (idx, (path, _)) in self.inner.lists.iter().enumerate() {
            r.push(self.route(Method::GET, path, move |i, _| async move { json(i.lists[idx].1.get()) }));
            r.push(self.route(Method::POST, path, move |i, r| async move { json(i.lists[idx].1.set(&r.json()?)?) }));
            r.push(self.route(Method::PUT, path, |_, _| async move { Ok(Reply::Response(text_response(405, "Method Not Allowed"))) }));
        }
        r
    }
}

/// The drives' entries in `GET /api`, as server.ts lists them (keys with their example
/// query strings).
pub fn docs() -> Map<String, Value> {
    let v = j!({
        "POST /api/eject": { "body": { "device": "sdb" }, "description": "Safely eject USB device (unmount, remove NFS/NBD, power off)" },
        "POST /api/erase": { "body": { "device": "sdb", "mode": "quick|full|discard" }, "description": "Erase device. Returns immediately; track via SSE. Queueable before flash." },
        "POST /api/flash": { "body": { "imageUrl": "https://...", "extractSize": 0, "device": "sdb", "settings": {} }, "description": "Flash RPi image. Can queue after erase." },
        "POST /api/scan": { "body": { "device": "sdb" }, "description": "Re-read partition table and identify contents" },
        "POST /api/probe": { "body": { "device": "sdb" }, "description": "Fast sampled raw-sector probe. Reads ~1MB spread across the device (denser in unallocated gaps), classifies each block as zero/ones/text/compressed/structured and reports file signatures. Seconds." },
        "POST /api/deepscan": { "body": { "device": "sdb" }, "description": "Exhaustive raw scan for file signatures across the whole device. Tracked like erase/flash: progress via SSE, cancellable." },
        "GET /api/scanresult?device=sdb": "Findings from the last deep scan of a device",
        "POST /api/cancel": { "body": { "operationId": "uuid" }, "description": "Cancel active operation" },
        "GET /api/files?path=LABEL/dir": "List a directory on a mounted drive (read-only). path is relative to /srv/nfs/drives; empty lists the mounted drives. Symlinks that lead outside the drives are refused.",
        "GET /api/download?path=LABEL/dir/file": "Download one file. Supports Range requests, so interrupted downloads can resume (curl -C -).",
        "GET /api/osinfo?device=sda": "What's installed on a USB disk and how it boots: per-partition role, EFI loaders (with Windows BCD entries, GRUB targets, systemd-boot entries, fallback-loader identity), Windows edition/build/PC name, Linux os-release, Raspberry Pi bootfs, macOS version. Only mounted partitions are inspected.",
        "GET /api/images": "Official RPi OS image catalog (cached 1hr)",
        "GET /api/catalog": "Image catalog with picker metadata (full category path, board tags, stable favorite key, parsed Raspberry Pi OS release/arch/edition) plus the board list",
        "GET /api/favorites": "Starred images: [{ key, label }], keys as in /api/catalog",
        "POST /api/favorites": { "body": [{ "key": "rpios/current/64/lite", "label": "Raspberry Pi OS Lite (64-bit)" }], "description": "Replace the starred images" },
        "GET /api/quicklist": "Images Quick Pi offers: [{ key, label }]. Defaults to the current Pi OS Lite 64-bit until first saved.",
        "POST /api/quicklist": { "body": [{ "key": "rpios/current/64/lite", "label": "Pi OS Lite 64-bit" }], "description": "Replace the Quick Pi list" },
        "GET /api/defaults": "Saved default flash settings",
        "POST /api/rpiboot/msd": { "body": { "path": "1-1.3" }, "description": "Load the mass-storage gadget onto a device in USB boot mode (path from status.bootDevices). Its eMMC/NVMe then appears as a normal drive." },
        "POST /api/rpiboot/eeprom/check": { "body": { "path": "1-1.3" }, "description": "Read OTP metadata (board, MAC, secure-boot state, EEPROM hash) through recovery.bin without writing anything. Result in status.rpibootResults." },
        "GET /api/rpiboot/eeprom/options?path=1-1.3": "Bootloader images available for that device's SoC, plus rpiboot's default boot.conf",
        "POST /api/rpiboot/eeprom/write": { "body": { "path": "1-1.3", "imageId": "default|latest|rpiboot", "bootConf": "[all]\nBOOT_ORDER=0xf25641" }, "description": "Rewrite the SPI bootloader EEPROM with the chosen image and config." },
        "POST /api/rpiboot/results/clear": { "body": { "path": "1-1.3" }, "description": "Dismiss a stored bootloader check/write result" },
        "GET /api/blcard/options?chip=2711|2712": "Bootloader images for a Pi 4 (2711) or Pi 5 (2712) bootloader update card, each with its built-in boot.conf",
        "POST /api/blcard": { "body": { "device": "sdb", "chip": "2711|2712", "imageId": "default|latest|rpiboot", "bootConf": "[all]\nBOOT_ORDER=0xf41" }, "description": "Erase a drive and write a bootloader update SD card (recovery.bin + pieeprom.bin with that config), sized to fit any card. Tracked like erase/flash." },
        "POST /api/defaults": "Save default flash settings",
    });
    v.as_object().cloned().unwrap_or_default()
}

/// server.ts's curl examples for `GET /api`.
pub fn examples() -> Json {
    j!({
        "eject": "curl -X POST http://benchbob.local/api/eject -H \"Content-Type: application/json\" -d '{\"device\":\"sdb\"}'",
        "status": "curl -s http://benchbob.local/api/status | jq '.drives'",
        "list_exports": "curl -s http://benchbob.local/api/status | jq '.nbdExports'",
        "list_files": "curl -s 'http://benchbob.local/api/files?path=bootfs' | jq '.entries[].name'",
        "download": "curl -OJ 'http://benchbob.local/api/download?path=bootfs/config.txt'",
    })
}

/// A JSON error answer with a status (for callers outside the route table).
pub fn error_response(code: u16, msg: &str) -> axum::response::Response {
    json_response(code, &j!({ "ok": false, "message": msg }))
}
