//! Block devices (server.ts "USB Speed", "Block Devices", "NBD Exports", "Device
//! Actions"): the USB drives offered on the dashboard, and getting a drive out of use
//! (unmount, NFS/NBD exports) before it is ejected, erased or written.

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Value, json};

use crate::sys::Sys;

pub const NBD_DIR: &str = "/etc/nbd-server/conf.d";
pub const DRIVES_DIR: &str = "/srv/nfs/drives";

/// Never offered for anything, whatever the machine: zram, loop devices, and network
/// block devices (a diskless machine's root comes in over NBD). An SD card is *not* on
/// this list: it is only refused when it is the disk the machine runs from, or eMMC
/// (see [`refuse`]), so a box that boots from the network can use its SD slot as a
/// drive bay.
pub fn is_sys_dev(name: &str) -> bool {
    name.starts_with("zram") || name.starts_with("loop") || name.starts_with("nbd")
}

/// Built-in eMMC (soldered down, usually the OS) rather than a card in a slot.
pub async fn is_emmc(sys: &dyn Sys, name: &str) -> bool {
    name.starts_with("mmcblk") && sys.read_text(&format!("/sys/block/{name}/device/type")).await == "MMC"
}

/// The disk the root filesystem is on (`nbd0` on a diskless box, `mmcblk0` on a Pi
/// booting from its card), or `None` when it can't be told (NFS root, say).
pub async fn root_disk(sys: &dyn Sys) -> Option<String> {
    let src = sys.run(&["findmnt", "-no", "SOURCE", "/"]).await.out;
    let dev = src.strip_prefix("/dev/")?.to_string();
    let parent = sys.run(&["lsblk", "-no", "PKNAME", &src]).await.out.trim().to_string();
    Some(if parent.is_empty() { dev } else { parent })
}

/// Why `dev` must not be ejected, erased, written or scanned, or `None` when it may.
/// Checked at every request that names a device: the drive list is only advice.
pub async fn refuse(sys: &dyn Sys, dev: &str) -> Option<&'static str> {
    if is_sys_dev(dev) {
        return Some("Cannot touch a system device");
    }
    if root_disk(sys).await.as_deref() == Some(dev) {
        return Some("That is the disk this machine runs from");
    }
    if is_emmc(sys, dev).await {
        return Some("That is the built-in eMMC");
    }
    None
}

/// A kernel disk name, nothing else (it ends up in commands, some through a shell).
pub fn valid_name(name: &str) -> bool {
    Regex::new(r"^[a-z][a-z0-9]*$").unwrap().is_match(name)
}

/// sda → /dev/sda1, but nvme0n1 / mmcblk0 / loop0 → /dev/nvme0n1p1.
pub fn part_dev(dev: &str, n: u32) -> String {
    let p = if dev.ends_with(|c: char| c.is_ascii_digit()) { "p" } else { "" };
    format!("/dev/{dev}{p}{n}")
}

/// Size in bytes from /sys/block/<dev>/size (512-byte sectors).
pub async fn size_bytes(sys: &dyn Sys, dev: &str) -> f64 {
    benchbob_core::http::parse_int(&sys.read_text(&format!("/sys/block/{dev}/size")).await).unwrap_or(0) as f64 * 512.0
}

/// The USB generation of the port a disk is on, from the nearest `speed` in sysfs.
pub async fn usb_speed(sys: &dyn Sys, dev: &str) -> Option<String> {
    let mut p = sys.run(&["readlink", "-f", &format!("/sys/block/{dev}")]).await.out;
    for _ in 0..8 {
        p = p[..p.rfind('/').unwrap_or(0)].to_string();
        if p.len() < 5 {
            break;
        }
        let speed = sys.read_text(&format!("{p}/speed")).await;
        if !speed.is_empty() {
            let mbps = benchbob_core::http::parse_int(&speed).map(|x| x as f64).unwrap_or(f64::NAN);
            // NaN compares false everywhere, as in JS: "USB 3.2".
            return Some(
                if mbps <= 12.0 {
                    "USB 1.1"
                } else if mbps <= 480.0 {
                    "USB 2.0"
                } else if mbps <= 5000.0 {
                    "USB 3.0"
                } else if mbps <= 10000.0 {
                    "USB 3.1"
                } else {
                    "USB 3.2"
                }
                .into(),
            );
        }
    }
    None
}

pub const LSBLK_COLS: &str = "NAME,PATH,SIZE,TYPE,MOUNTPOINT,MODEL,SERIAL,TRAN,FSTYPE,LABEL,UUID,RM,RO,REV";

fn flag(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Bool(true))) || matches!(v, Some(Value::String(s)) if s == "1") || v.and_then(Value::as_f64) == Some(1.0)
}

/// `enrich`: rates, rm/ro as booleans, children likewise (keys stay where lsblk put them).
fn enrich(d: &mut Value, rates: &IndexMap<String, (f64, f64)>) {
    let name = d.get("name").map(benchbob_core::http::js_string).unwrap_or_default();
    let r = rates.get(&name);
    d["readRate"] = benchbob_core::http::num(r.map_or(0.0, |x| x.0));
    d["writeRate"] = benchbob_core::http::num(r.map_or(0.0, |x| x.1));
    let rm = flag(d.get("rm"));
    d["rm"] = rm.into();
    let ro = flag(d.get("ro"));
    d["ro"] = ro.into();
    if let Some(Value::Array(c)) = d.get_mut("children") {
        for x in c {
            enrich(x, rates);
        }
    }
}

/// `getDevices` from lsblk's JSON: the root disk (enriched) and the drives: USB disks,
/// and a card in the SD slot when the machine doesn't run from it (eMMC never). Each
/// comes with its USB speed (a card: `bus: "sd"` instead), whether it has partitions,
/// and its operations (`ops`).
pub async fn devices(
    sys: &dyn Sys,
    lsblk: &str,
    rates: &IndexMap<String, (f64, f64)>,
    root: Option<&str>,
    ops: &(dyn Fn(&str) -> Vec<Value> + Sync),
) -> (Value, Value) {
    let mut all: Vec<Value> = serde_json::from_str::<Value>(lsblk)
        .ok()
        .and_then(|v| v.get("blockdevices").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    for d in &mut all {
        enrich(d, rates);
    }
    let is_root = |d: &Value| root.is_some_and(|r| d.get("name").and_then(Value::as_str) == Some(r));
    let root_dev = all.iter().find(|d| is_root(d)).cloned();
    let mut drives = Vec::new();
    for mut d in all.into_iter().filter(|d| {
        let name = d.get("name").and_then(Value::as_str).unwrap_or("");
        (d.get("tran").and_then(Value::as_str) == Some("usb") || name.starts_with("mmcblk"))
            && d.get("type").and_then(Value::as_str) == Some("disk")
            && d.get("size").map(benchbob_core::http::js_number).is_some_and(|s| s > 0.0)
    }) {
        if is_root(&d) {
            continue;
        }
        let name = d["name"].as_str().unwrap_or("").to_string();
        let sd = name.starts_with("mmcblk");
        if sd && is_emmc(sys, &name).await {
            continue;
        }
        if sd {
            d["bus"] = "sd".into();
        }
        d["usbVersion"] = if sd { None } else { usb_speed(sys, &name).await }.map(Value::String).unwrap_or(Value::Null);
        d["hasPartitions"] = d.get("children").and_then(Value::as_array).is_some_and(|c| !c.is_empty()).into();
        d["operations"] = Value::Array(ops(&name));
        drives.push(d);
    }
    (root_dev.unwrap_or(Value::Null), Value::Array(drives))
}

/// NBD exports: the .conf files the drive udev handler writes, without the extension.
pub async fn nbd_exports(sys: &dyn Sys) -> Value {
    let out = sys.run(&["ls", &format!("{NBD_DIR}/")]).await.out;
    Value::Array(out.split('\n').filter(|f| f.ends_with(".conf")).map(|f| Value::String(f.replacen(".conf", "", 1))).collect())
}

/// `prepareDevice`: unmount everything on the disk, drop stale mount dirs and the disk's
/// NBD exports, and refresh the NFS/NBD servers.
pub async fn prepare(sys: &dyn Sys, dev: &str) {
    let out = sys.run(&["lsblk", "-J", "-o", "NAME,MOUNTPOINT", &format!("/dev/{dev}")]).await.out;
    let mut mps = Vec::new();
    fn collect(d: &Value, mps: &mut Vec<String>) {
        if let Some(m) = d.get("mountpoint").filter(|m| benchbob_core::http::truthy(m)) {
            mps.push(benchbob_core::http::js_string(m));
        }
        for c in d.get("children").and_then(Value::as_array).into_iter().flatten() {
            collect(c, mps);
        }
    }
    if let Some(d) = serde_json::from_str::<Value>(&out).ok().and_then(|v| v.get("blockdevices")?.get(0).cloned()) {
        collect(&d, &mut mps);
    }
    for mp in &mps {
        sys.run(&["sudo", "umount", "-l", mp]).await;
    }
    let dirs = sys.run(&["ls", &format!("{DRIVES_DIR}/")]).await.out;
    for d in dirs.split('\n').filter(|s| !s.is_empty()) {
        let p = format!("{DRIVES_DIR}/{d}");
        if sys.run(&["mountpoint", "-q", &p]).await.code != 0 {
            sys.run(&["sudo", "rmdir", &p]).await;
        }
    }
    let nbd = sys.run(&["ls", &format!("{NBD_DIR}/")]).await.out;
    for f in nbd.split('\n').filter(|s| !s.is_empty()) {
        if f.starts_with(dev) {
            sys.run(&["sudo", "rm", "-f", &format!("{NBD_DIR}/{f}")]).await;
        }
    }
    sys.run(&["sudo", "systemctl", "restart", "nbd-server"]).await;
    sys.run(&["sudo", "exportfs", "-ra"]).await;
}

/// `ejectDevice` (the caller has checked it isn't busy, and [`refuse`]d what must stay).
/// A card in the SD slot has no power to cut: it is unmounted and can then be pulled.
pub async fn eject(sys: &dyn Sys, dev: &str) -> Value {
    if is_sys_dev(dev) {
        return json!({ "ok": false, "message": "Cannot eject system device" });
    }
    prepare(sys, dev).await;
    if dev.starts_with("mmcblk") {
        return json!({ "ok": true, "message": format!("{dev} unmounted; the card can be removed") });
    }
    let r = sys.run(&["sudo", "udisksctl", "power-off", "-b", &format!("/dev/{dev}"), "--no-user-interaction"]).await;
    if r.code != 0 {
        let sp = sys.run(&["readlink", "-f", &format!("/sys/block/{dev}/device")]).await.out;
        if !sp.is_empty() {
            sys.run(&["sudo", "bash", "-c", &format!("echo 1 > {sp}/delete")]).await;
        }
    }
    json!({ "ok": true, "message": format!("{dev} ejected") })
}

/// `scanDevice`: re-read the partition table and say what's there.
pub async fn scan(sys: &dyn Sys, dev: &str) -> Value {
    if is_sys_dev(dev) {
        return json!({ "ok": false, "message": "Cannot scan system device" });
    }
    let path = format!("/dev/{dev}");
    sys.run(&["sudo", "partprobe", &path]).await;
    sys.run(&["sudo", "udevadm", "settle", "--timeout=5"]).await;
    let out = sys.run(&["lsblk", "-J", "-b", "-o", "NAME,SIZE,TYPE,FSTYPE,LABEL", &path]).await.out;
    let Ok(v) = serde_json::from_str::<Value>(&out) else { return json!({ "ok": false, "message": "Scan failed" }) };
    let parts =
        v.get("blockdevices").and_then(|b| b.get(0)).and_then(|d| d.get("children")).and_then(Value::as_array).cloned().unwrap_or_default();
    if !parts.is_empty() {
        return json!({ "ok": true, "message": format!("Found {} partition(s)", parts.len()), "partitions": parts });
    }
    let blk = sys.run(&["sudo", "blkid", &path]).await.out;
    if !blk.is_empty() {
        return json!({ "ok": true, "message": format!("Direct filesystem: {blk}") });
    }
    let f = sys.run(&["sudo", "file", "-sL", &path]).await.out;
    json!({ "ok": true, "message": if f.is_empty() { "No partitions or filesystems found".to_string() } else { f } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::mock::{Mock, Shared};

    #[test]
    fn names() {
        assert_eq!(part_dev("sda", 1), "/dev/sda1");
        assert_eq!(part_dev("nvme0n1", 2), "/dev/nvme0n1p2");
        assert!(is_sys_dev("nbd0") && is_sys_dev("loop3") && !is_sys_dev("sda") && !is_sys_dev("mmcblk0"));
        assert!(valid_name("nvme0n1") && !valid_name("sda; rm -rf /") && !valid_name("../sda") && !valid_name(""));
    }

    /// desky's lsblk with a USB stick added: key order as lsblk gives it, rates and flags
    /// appended/replaced as server.ts did (its harness recorded the expected output; frozen).
    #[tokio::test]
    async fn devices_from_lsblk_match_server_ts() {
        let scn: Value = serde_json::from_str(include_str!("../fixtures/devices-scenario.json")).unwrap();
        let sys = Shared(Mock::new(&scn));
        let rates: IndexMap<String, (f64, f64)> =
            scn["rates"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), (v[0].as_f64().unwrap(), v[1].as_f64().unwrap()))).collect();
        let mut op = crate::ops::new_op("erase", "sda");
        (op.id, op.status, op.progress, op.message) = ("op-1".into(), "erasing".into(), 30.0, "Zeroing partition table...".into());
        let ops = |d: &str| if d == "sda" { vec![op.public()] } else { vec![] };
        let lsblk = include_str!("../fixtures/lsblk-desky-usb.json");
        let (root, drives) = devices(&sys, lsblk, &rates, Some("mmcblk0"), &ops).await;
        let want: Value = serde_json::from_str(include_str!("../fixtures/devices-desky-usb.expected.json")).unwrap();
        assert_eq!(json!({ "rootDevice": root, "drives": drives }).to_string(), want.to_string());
    }

    /// The same box booted from the network: its SD card is a drive now, the NBD root
    /// isn't, and eMMC never is.
    #[tokio::test]
    async fn an_sd_card_is_a_drive_when_the_machine_runs_from_the_network() {
        let lsblk = include_str!("../fixtures/lsblk-desky-usb.json");
        let no_ops = |_: &str| vec![];
        let rates = IndexMap::new();

        let sd = Shared(Mock::new(&json!({ "files": { "/sys/block/mmcblk0/device/type": "SD" } })));
        let (root, drives) = devices(&sd, lsblk, &rates, Some("nbd0"), &no_ops).await;
        assert!(root.is_null(), "nbd0 isn't in this lsblk");
        let names: Vec<&str> = drives.as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["mmcblk0", "sda"], "sdb is empty, as before");
        let card = &drives[0];
        assert_eq!(card["bus"], "sd");
        assert!(card["usbVersion"].is_null());
        assert!(drives[1].get("bus").is_none(), "USB drives are shaped as server.ts shaped them");

        let emmc = Shared(Mock::new(&json!({ "files": { "/sys/block/mmcblk0/device/type": "MMC" } })));
        let (_, drives) = devices(&emmc, lsblk, &rates, Some("nbd0"), &no_ops).await;
        let names: Vec<&str> = drives.as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["sda"]);

        // Refusals: the root disk, eMMC and NBD; a card in the slot is fine.
        let scn = json!({
            "files": { "/sys/block/mmcblk0/device/type": "SD", "/sys/block/mmcblk1/device/type": "MMC" },
            "answers": [
                { "match": "findmnt -no SOURCE /", "out": "/dev/nbd0p2" },
                { "match": "lsblk -no PKNAME /dev/nbd0p2", "out": "nbd0" }
            ]
        });
        let sys = Shared(Mock::new(&scn));
        assert_eq!(root_disk(&sys).await.as_deref(), Some("nbd0"));
        assert!(refuse(&sys, "nbd0").await.is_some());
        assert!(refuse(&sys, "mmcblk1").await.unwrap().contains("eMMC"));
        assert!(refuse(&sys, "mmcblk0").await.is_none());
        assert!(refuse(&sys, "sda").await.is_none());
        let scn = json!({ "answers": [
            { "match": "findmnt -no SOURCE /", "out": "/dev/mmcblk0p2" },
            { "match": "lsblk -no PKNAME /dev/mmcblk0p2", "out": "mmcblk0" }
        ] });
        let sys = Shared(Mock::new(&scn));
        assert!(refuse(&sys, "mmcblk0").await.unwrap().contains("runs from"));

        // Ejecting a card only unmounts it.
        let r = eject(&sd, "mmcblk0").await;
        assert_eq!(r["ok"], true);
        assert!(r["message"].as_str().unwrap().contains("can be removed"));
    }
}
