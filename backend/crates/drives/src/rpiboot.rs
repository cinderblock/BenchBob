//! Compute Modules (and other Pis) in USB boot mode (rpiboot.ts).
//!
//! With nRPIBOOT pulled low, a CM's boot ROM enumerates as a bare Broadcom USB device and
//! waits for a host to send it code. `rpiboot` (apt package `rpiboot`) is that host; its
//! payload directories in /usr/share/rpiboot do the two useful things:
//!
//! - `mass-storage-gadget64` boots a tiny Linux that exports the eMMC/NVMe as a USB disk,
//!   which then shows up as an ordinary drive to flash or browse;
//! - `recovery` / `recovery5`: recovery.bin, which reads OTP metadata and (given
//!   pieeprom.bin + .sig) rewrites the SPI bootloader EEPROM.
//!
//! Every rpiboot run is pinned to one USB port path (`-p`), so two CMs on one host can't
//! be confused. Also here: the files of a bootloader update SD card.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::sys::{Killer, Sys};
use crate::util::{iso_date, js_pretty, locale_cmp};

#[derive(Clone)]
pub struct Paths {
    pub usb_root: PathBuf,
    pub rpiboot_dir: PathBuf,
    pub eeprom_fw_dir: PathBuf,
    pub work_root: PathBuf,
    pub history: PathBuf,
    pub rpiboot_bin: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            usb_root: "/sys/bus/usb/devices".into(),
            rpiboot_dir: "/usr/share/rpiboot".into(),
            eeprom_fw_dir: "/lib/firmware/raspberrypi".into(),
            work_root: "/tmp/benchbob-rpiboot".into(),
            history: benchbob_core::paths::state_path("eeprom-history.json"),
            rpiboot_bin: "/usr/bin/rpiboot".into(),
        }
    }
}

/// Broadcom vendor 0a5c; the product is the SoC's boot ROM.
fn boot_rom(product: &str) -> Option<(&'static str, &'static str)> {
    match product {
        "2763" => Some(("2835", "BCM2835/2837 (CM1/CM3)")),
        "2764" => Some(("2835", "BCM2837 (CM3/CM3+)")),
        "2711" => Some(("2711", "BCM2711 (CM4 / Pi 4 family)")),
        "2712" => Some(("2712", "BCM2712 (CM5 / Pi 5 family)")),
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub struct BootDevice {
    /// USB port path, e.g. "1-1.3": what `rpiboot -p` wants.
    pub path: String,
    pub chip: String,
    pub label: String,
    pub serial: Option<String>,
    /// BCM2711 re-enumerates once bootcode4.bin runs, with another serial string.
    pub product: Option<String>,
}

impl BootDevice {
    pub fn to_json(&self) -> Value {
        json!({ "path": self.path, "chip": self.chip, "label": self.label, "serial": self.serial, "product": self.product })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EepromImage {
    pub id: String,
    pub label: String,
    pub path: String,
    pub recovery_bin: String,
    pub build_date: String,
    /// The boot.conf embedded in the image as shipped.
    pub config: String,
}

impl EepromImage {
    pub fn to_json(&self) -> Value {
        json!({ "id": self.id, "label": self.label, "path": self.path, "recoveryBin": self.recovery_bin, "buildDate": self.build_date, "config": self.config })
    }
    pub fn from_json(v: &Value) -> Self {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        Self {
            id: s("id"),
            label: s("label"),
            path: s("path"),
            recovery_bin: s("recoveryBin"),
            build_date: s("buildDate"),
            config: s("config"),
        }
    }
}

pub struct CardFiles {
    /// Work dir in /tmp; the caller removes it.
    pub dir: String,
    pub files: Vec<String>,
    pub image_sha256: String,
}

#[derive(Default, Debug, Clone)]
pub struct RpibootRun {
    pub code: i32,
    pub log: Vec<String>,
    pub metadata: Option<Map<String, Value>>,
    pub timed_out: bool,
    /// Of the pieeprom.bin sent, for bootloader writes.
    pub image_sha256: Option<String>,
}

pub struct Hooks {
    pub on_line: Box<dyn Fn(&str) + Send + Sync>,
    pub on_spawn: Box<dyn Fn(Killer) + Send + Sync>,
}

pub const CARD_BOARDS: [(&str, &str); 2] = [("2711", "Raspberry Pi 4 / 400"), ("2712", "Raspberry Pi 5 / 500")];

pub fn card_board(chip: &str) -> &'static str {
    CARD_BOARDS.iter().find(|(c, _)| *c == chip).map(|x| x.1).unwrap_or("")
}

/// What the operations need from rpiboot and rpi-eeprom (mocked in tests).
#[async_trait]
pub trait Firmware: Send + Sync {
    async fn installed(&self) -> bool;
    async fn boot_devices(&self) -> Vec<BootDevice>;
    async fn eeprom_images(&self, chip: &str) -> Vec<EepromImage>;
    async fn default_boot_conf(&self, chip: &str) -> String;
    async fn build_card_files(&self, chip: &str, image: &EepromImage, boot_conf: &str) -> anyhow::Result<CardFiles>;
    async fn load_mass_storage(&self, dev: &BootDevice, hooks: &Hooks) -> RpibootRun;
    async fn read_bootloader_info(&self, dev: &BootDevice, hooks: &Hooks) -> anyhow::Result<RpibootRun>;
    async fn write_bootloader(&self, dev: &BootDevice, image: &EepromImage, boot_conf: &str, hooks: &Hooks) -> anyhow::Result<RpibootRun>;
    async fn record_write(&self, rec: Value) -> anyhow::Result<()>;
    async fn identify(&self, chip: &str, md: Option<&Map<String, Value>>) -> Value;
}

// ==================== Board revision ====================

fn board_type(t: u32) -> Option<&'static str> {
    Some(match t {
        0x00 => "Pi A",
        0x01 => "Pi B",
        0x02 => "Pi A+",
        0x03 => "Pi B+",
        0x04 => "Pi 2B",
        0x08 => "Pi 3B",
        0x09 => "Pi Zero",
        0x0c => "Pi Zero W",
        0x0d => "Pi 3B+",
        0x0e => "Pi 3A+",
        0x12 => "Pi Zero 2 W",
        0x06 => "CM1",
        0x0a => "CM3",
        0x10 => "CM3+",
        0x11 => "Pi 4B",
        0x13 => "Pi 400",
        0x14 => "CM4",
        0x15 => "CM4S",
        0x17 => "Pi 5",
        0x18 => "CM5",
        0x19 => "Pi 500",
        0x1a => "CM5 Lite",
        0x1b => "CM0",
        _ => return None,
    })
}
const MEMORY: [&str; 7] = ["256 MB", "512 MB", "1 GB", "2 GB", "4 GB", "8 GB", "16 GB"];

/// `parseInt(hex, 16)`: leading hex digits (an optional 0x), None for NaN.
fn parse_hex(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    let d: String = t.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    if d.is_empty() {
        return None;
    }
    let v = i64::from_str_radix(&d[..d.len().min(15)], 16).ok()?;
    Some(if neg { -v } else { v })
}

/// A new-style revision code (OTP USER_BOARDREV): model, memory, PCB revision.
/// https://www.raspberrypi.com/documentation/computers/raspberry-pi.html#new-style-revision-codes
pub fn decode_board_rev(hex: Option<&str>) -> Value {
    let Some(v) = hex.filter(|h| !h.is_empty()).and_then(parse_hex) else { return Value::Null };
    // JavaScript's bitwise operators work on 32-bit integers.
    let v = v as i32 as u32;
    if v & (1 << 23) == 0 {
        return Value::Null;
    }
    let t = (v >> 4) & 0xff;
    json!({
        "model": board_type(t).map(String::from).unwrap_or_else(|| format!("type 0x{t:x}")),
        "memory": MEMORY.get(((v >> 20) & 7) as usize).copied().unwrap_or("?"),
        "revision": format!("1.{}", v & 0xf),
    })
}

// ==================== The real thing ====================

pub struct Rpi {
    pub paths: Paths,
    sys: Arc<dyn Sys>,
    info: Mutex<HashMap<String, (std::time::SystemTime, String, String)>>,
    digest_soc: Mutex<Option<bool>>,
}

fn read_attr(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn build_date(path: &str) -> String {
    Regex::new(r"pieeprom-(\d{4}-\d{2}-\d{2})\.bin$").unwrap().captures(path).map(|c| c[1].to_string()).unwrap_or_default()
}

fn channel_images(dir: &Path) -> Vec<String> {
    let re = Regex::new(r"^pieeprom-\d{4}-\d{2}-\d{2}\.bin$").unwrap();
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|f| re.is_match(f)).collect())
        .unwrap_or_default();
    v.sort();
    v
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The README a bootloader card carries (written with CRLF line ends).
pub fn card_readme(chip: &str, image: &EepromImage, boot_conf: &str, now_ms: i64) -> String {
    format!(
        "{} bootloader update card
Made by BenchBob on {}

Bootloader release: {}

Settings written into the EEPROM (boot.conf):

{}

To update the bootloader:

1. Power off the Raspberry Pi
2. Insert this card
3. Power on and wait at least 10 seconds

Success: the green LED blinks rapidly forever, and an HDMI display (if
attached) turns green. A different blink pattern, or red on HDMI, means the
update failed and the old bootloader is still in place.

Then power off and remove the card. If the card now holds RECOVERY.000
instead of recovery.bin, rename it back (or write the card again) before
using it on another Pi.
",
        card_board(chip),
        iso_date(now_ms),
        image.label,
        boot_conf.replace("\r\n", "\n").trim_end()
    )
}

/// boot.conf as written next to the image: LF line ends, one trailing newline.
pub fn normalise_boot_conf(c: &str) -> String {
    c.replace("\r\n", "\n").trim_end().to_string() + "\n"
}

impl Rpi {
    pub fn new(paths: Paths, sys: Arc<dyn Sys>) -> Self {
        Self { paths, sys, info: Default::default(), digest_soc: Mutex::new(None) }
    }

    async fn stock_image(&self, chip: &str) -> Option<(String, String)> {
        let path = self.paths.rpiboot_dir.join(format!("firmware/{chip}/pieeprom.bin"));
        let data = std::fs::read(&path).ok()?;
        let text: String = data.iter().map(|b| *b as char).collect();
        let ts = Regex::new(r"BUILD_TIMESTAMP=(\d+)").unwrap().captures(&text).and_then(|c| c[1].parse::<i64>().ok()).unwrap_or(0);
        Some((s(&path), if ts != 0 { iso_date(ts * 1000) } else { String::new() }))
    }

    /// sha256 and embedded config of an image file, cached until the file changes.
    async fn image_info(&self, path: &str) -> Option<(String, String)> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
        if let Some((m, h, c)) = self.info.lock().unwrap().get(path)
            && *m == mtime
        {
            return Some((h.clone(), c.clone()));
        }
        let cfg = self.sys.run_input(&["rpi-eeprom-config", path], None).await;
        let sha = sha256_hex(&std::fs::read(path).ok()?);
        let config = if cfg.code == 0 { cfg.out } else { String::new() };
        self.info.lock().unwrap().insert(path.into(), (mtime, sha.clone(), config.clone()));
        Some((sha, config))
    }

    fn read_history(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.paths.history)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    }

    /// rpiboot polls forever when nothing answers, so every run has a deadline.
    async fn rpiboot(&self, args: &[&str], hooks: &Hooks, timeout: Duration) -> RpibootRun {
        let mut cmd = vec!["sudo", "rpiboot"];
        cmd.extend_from_slice(args);
        let mut p = match self.sys.spawn(&cmd) {
            Ok(p) => p,
            Err(e) => return RpibootRun { code: 127, log: vec![e.to_string()], ..Default::default() },
        };
        let kill = p.killer();
        (hooks.on_spawn)(kill.clone());
        let mut log: Vec<String> = Vec::new();
        let push = |line: &str, log: &mut Vec<String>| {
            log.push(line.to_string());
            if log.len() > 400 {
                log.drain(..log.len() - 400);
            }
            (hooks.on_line)(line);
        };
        let deadline = tokio::time::Instant::now() + timeout;
        let mut timed_out = false;
        let (mut bo, mut be) = (String::new(), String::new());
        let (mut out_done, mut err_done) = (false, false);
        while !(out_done && err_done) {
            let next = tokio::select! {
                c = p.any() => c,
                _ = tokio::time::sleep_until(deadline), if !timed_out => {
                    timed_out = true;
                    kill();
                    continue;
                }
            };
            match next {
                None => {
                    out_done = true;
                    err_done = true;
                }
                Some((fd, c)) => {
                    let buf = if fd == 1 { &mut bo } else { &mut be };
                    buf.push_str(&String::from_utf8_lossy(&c));
                    while let Some(i) = buf.find(['\r', '\n']) {
                        let line = buf[..i].trim().to_string();
                        buf.drain(..=i);
                        if !line.is_empty() {
                            push(&line, &mut log);
                        }
                    }
                }
            }
        }
        for b in [bo, be] {
            if !b.trim().is_empty() {
                push(b.trim(), &mut log);
            }
        }
        let code = p.wait().await;
        RpibootRun { code, log, timed_out, ..Default::default() }
    }

    fn read_metadata(dir: &Path) -> Option<Map<String, Value>> {
        let f = std::fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|n| n.ends_with(".json"))?;
        serde_json::from_str::<Value>(&std::fs::read_to_string(dir.join(f)).ok()?).ok()?.as_object().cloned()
    }

    /// recovery.bin in a work dir of our own: /usr/share/rpiboot is never modified.
    fn recovery_work_dir(&self, chip: &str, recovery_bin: &str) -> anyhow::Result<PathBuf> {
        let dir = self.paths.work_root.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(dir.join("metadata"))?;
        std::fs::copy(recovery_bin, dir.join(if chip == "2712" { "bootcode5.bin" } else { "bootcode4.bin" }))?;
        let stock = if chip == "2712" { "recovery5" } else { "recovery" };
        std::fs::copy(self.paths.rpiboot_dir.join(format!("{stock}/config.txt")), dir.join("config.txt"))?;
        Ok(dir)
    }

    /// A tool that must succeed; rpi-eeprom-config fails with a Python traceback, whose
    /// last line says what.
    async fn tool(&self, cmd: &[&str]) -> anyhow::Result<()> {
        let r = self.sys.run_input(cmd, None).await;
        if r.code == 0 {
            return Ok(());
        }
        let last = r.err.trim().split('\n').next_back().unwrap_or("").to_string();
        anyhow::bail!("{}: {}", cmd[0], if last.is_empty() { format!("exit {}", r.code) } else { last })
    }

    /// rpi-eeprom-digest from 2026-09 can tag the .sig with "target-soc", which newer
    /// bootloaders check, so a Pi 4 image can't land on a Pi 5. Older ones lack the option.
    async fn digest_soc_args(&self, chip: &str) -> Vec<String> {
        let known = *self.digest_soc.lock().unwrap();
        let has = match known {
            Some(h) => h,
            None => {
                let h = Regex::new(r"(?i)target SoC").unwrap().is_match(&self.sys.run_input(&["rpi-eeprom-digest", "-h"], None).await.out);
                *self.digest_soc.lock().unwrap() = Some(h);
                h
            }
        };
        if has { vec!["-c".into(), chip.into()] } else { vec![] }
    }

    /// Embed boot.conf into a copy of the image as dir/pieeprom.bin, plus the .sig
    /// recovery.bin checks it against. Returns the built image's sha256.
    async fn build_pieeprom(&self, chip: &str, image: &EepromImage, boot_conf: &str, dir: &Path) -> anyhow::Result<String> {
        std::fs::write(dir.join("boot.conf"), normalise_boot_conf(boot_conf))?;
        let (conf, bin, sig) = (s(&dir.join("boot.conf")), s(&dir.join("pieeprom.bin")), s(&dir.join("pieeprom.sig")));
        self.tool(&["rpi-eeprom-config", "--config", &conf, "--out", &bin, &image.path]).await?;
        let soc = self.digest_soc_args(chip).await;
        let mut cmd: Vec<&str> = vec!["rpi-eeprom-digest"];
        cmd.extend(soc.iter().map(String::as_str));
        cmd.extend(["-i", &bin, "-o", &sig]);
        self.tool(&cmd).await?;
        Ok(sha256_hex(&std::fs::read(&bin)?))
    }

    /// The USB 3 controller firmware the official Pi 4 card carries.
    fn vl805_image(&self, image: &EepromImage) -> Option<String> {
        let re = Regex::new(r"^vl805-[0-9a-f]+\.bin$").unwrap();
        let own = image.path[..image.path.rfind('/').unwrap_or(0)].to_string();
        for dir in [own, s(&self.paths.eeprom_fw_dir.join("bootloader-2711/default"))] {
            let mut v: Vec<String> = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|f| re.is_match(f)).collect()
                })
                .unwrap_or_default();
            v.sort();
            if let Some(f) = v.last() {
                return Some(format!("{dir}/{f}"));
            }
        }
        None
    }
}

#[async_trait]
impl Firmware for Rpi {
    async fn installed(&self) -> bool {
        self.sys.exists(&s(&self.paths.rpiboot_bin)).await
    }

    async fn boot_devices(&self) -> Vec<BootDevice> {
        let Ok(rd) = std::fs::read_dir(&self.paths.usb_root) else { return vec![] };
        let mut out = Vec::new();
        for e in rd.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.contains(':') {
                continue; // interfaces, not devices
            }
            let dir = self.paths.usb_root.join(&name);
            if read_attr(&dir, "idVendor").as_deref() != Some("0a5c") {
                continue;
            }
            let Some((chip, label)) = boot_rom(&read_attr(&dir, "idProduct").unwrap_or_default()) else { continue };
            out.push(BootDevice {
                path: name,
                chip: chip.into(),
                label: label.into(),
                serial: read_attr(&dir, "serial"),
                product: read_attr(&dir, "product"),
            });
        }
        out.sort_by(|a, b| locale_cmp(&a.path, &b.path, false, false));
        out
    }

    /// The newest image in each rpi-eeprom release channel, plus rpiboot's own recovery image.
    async fn eeprom_images(&self, chip: &str) -> Vec<EepromImage> {
        if chip == "2835" {
            return vec![];
        }
        let mut imgs: Vec<EepromImage> = Vec::new();
        let base = self.paths.eeprom_fw_dir.join(format!("bootloader-{chip}"));
        for channel in ["default", "latest"] {
            let Some(newest) = channel_images(&base.join(channel)).pop() else { continue };
            let path = s(&base.join(format!("{channel}/{newest}")));
            let date = build_date(&path);
            if imgs.iter().any(|i| i.build_date == date) {
                continue;
            }
            let config = self.image_info(&path).await.map(|x| x.1).unwrap_or_default();
            imgs.push(EepromImage {
                id: channel.into(),
                label: format!("{date} (rpi-eeprom \"{channel}\" channel)"),
                recovery_bin: s(&base.join(format!("{channel}/recovery.bin"))),
                path,
                build_date: date,
                config,
            });
        }
        if let Some((path, date)) = self.stock_image(chip).await {
            let config = self.image_info(&path).await.map(|x| x.1).unwrap_or_default();
            imgs.push(EepromImage {
                id: "rpiboot".into(),
                label: format!("{} (rpiboot's stock recovery image)", if date.is_empty() { "?" } else { &date }),
                path,
                recovery_bin: s(&self.paths.rpiboot_dir.join(format!("firmware/{chip}/recovery.bin"))),
                build_date: date,
                config,
            });
        }
        imgs
    }

    /// The boot.conf rpiboot's recovery directory ships for this chip.
    async fn default_boot_conf(&self, chip: &str) -> String {
        let dir = if chip == "2712" { "recovery5" } else { "recovery" };
        std::fs::read_to_string(self.paths.rpiboot_dir.join(format!("{dir}/boot.conf"))).unwrap_or_else(|_| "[all]\nBOOT_UART=1\n".into())
    }

    async fn build_card_files(&self, chip: &str, image: &EepromImage, boot_conf: &str) -> anyhow::Result<CardFiles> {
        let dir = self.paths.work_root.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir)?;
        let res: anyhow::Result<CardFiles> = async {
            std::fs::copy(&image.recovery_bin, dir.join("recovery.bin"))?;
            let sha = self.build_pieeprom(chip, image, boot_conf, &dir).await?;
            let mut files: Vec<String> = ["recovery.bin", "pieeprom.bin", "pieeprom.sig"].iter().map(|x| x.to_string()).collect();
            if chip == "2711" {
                let Some(vl) = self.vl805_image(image) else { anyhow::bail!("No VL805 firmware found in rpi-eeprom's files") };
                std::fs::copy(&vl, dir.join("vl805.bin"))?;
                self.tool(&["rpi-eeprom-digest", "-i", &s(&dir.join("vl805.bin")), "-o", &s(&dir.join("vl805.sig"))]).await?;
                files.push("vl805.bin".into());
                files.push("vl805.sig".into());
            }
            std::fs::write(dir.join("README.txt"), card_readme(chip, image, boot_conf, self.sys.now_ms()).replace('\n', "\r\n"))?;
            files.push("README.txt".into());
            Ok(CardFiles { dir: s(&dir), files, image_sha256: sha })
        }
        .await;
        if res.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        res
    }

    /// Load the mass-storage gadget: the device then leaves boot mode and its storage
    /// enumerates as a USB disk.
    async fn load_mass_storage(&self, dev: &BootDevice, hooks: &Hooks) -> RpibootRun {
        // CM1/CM3 use rpiboot's built-in msd; 2711/2712 the Linux mass-storage-gadget.
        let gadget = s(&self.paths.rpiboot_dir.join("mass-storage-gadget64"));
        let args: Vec<&str> = if dev.chip == "2835" { vec!["-p", &dev.path] } else { vec!["-d", &gadget, "-p", &dev.path] };
        self.rpiboot(&args, hooks, Duration::from_secs(120)).await
    }

    /// Read-only: recovery.bin with no pieeprom.* files writes nothing and still sends the
    /// OTP metadata.
    async fn read_bootloader_info(&self, dev: &BootDevice, hooks: &Hooks) -> anyhow::Result<RpibootRun> {
        if dev.chip == "2835" {
            anyhow::bail!("CM1/CM3 have no bootloader EEPROM");
        }
        let stock = s(&self.paths.rpiboot_dir.join(format!("firmware/{}/recovery.bin", dev.chip)));
        let dir = self.recovery_work_dir(&dev.chip, &stock)?;
        let (d, m) = (s(&dir), s(&dir.join("metadata")));
        let mut r = self.rpiboot(&["-d", &d, "-j", &m, "-p", &dev.path], hooks, Duration::from_secs(60)).await;
        r.metadata = Self::read_metadata(&dir.join("metadata"));
        let _ = std::fs::remove_dir_all(&dir);
        Ok(r)
    }

    async fn write_bootloader(&self, dev: &BootDevice, image: &EepromImage, boot_conf: &str, hooks: &Hooks) -> anyhow::Result<RpibootRun> {
        if dev.chip == "2835" {
            anyhow::bail!("CM1/CM3 have no bootloader EEPROM");
        }
        let dir = self.recovery_work_dir(&dev.chip, &image.recovery_bin)?;
        let res = async {
            let sha = self.build_pieeprom(&dev.chip, image, boot_conf, &dir).await?;
            (hooks.on_line)(&format!("Built pieeprom.bin from {}", image.label));
            let (d, m) = (s(&dir), s(&dir.join("metadata")));
            let mut r = self.rpiboot(&["-d", &d, "-j", &m, "-p", &dev.path], hooks, Duration::from_secs(180)).await;
            r.metadata = Self::read_metadata(&dir.join("metadata"));
            r.image_sha256 = Some(sha);
            Ok(r)
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        res
    }

    async fn record_write(&self, rec: Value) -> anyhow::Result<()> {
        let mut rec = rec.as_object().cloned().unwrap_or_default();
        let mut hashes: Vec<String> = Vec::new();
        for h in rec.get("hashes").and_then(Value::as_array).into_iter().flatten() {
            let h = h.as_str().unwrap_or("").to_lowercase();
            if !h.is_empty() && !hashes.contains(&h) {
                hashes.push(h);
            }
        }
        let mac = rec
            .get("mac")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .map(|m| Value::String(m.to_lowercase()))
            .unwrap_or(Value::Null);
        rec.insert("mac".into(), mac);
        rec.insert("hashes".into(), json!(hashes));
        let mut h = self.read_history();
        h.push(Value::Object(rec));
        let keep = h.len().saturating_sub(200);
        let h: Vec<Value> = h.into_iter().skip(keep).collect();
        let tmp = format!("{}.tmp", s(&self.paths.history));
        std::fs::write(&tmp, js_pretty(&Value::Array(h)) + "\n")?;
        std::fs::rename(&tmp, &self.paths.history)?;
        Ok(())
    }

    /// Which bootloader is installed, recognised by the EEPROM_HASH recovery.bin reports:
    /// against every stock release image and every image BenchBob wrote. No match means
    /// "unknown", never a guess.
    async fn identify(&self, chip: &str, md: Option<&Map<String, Value>>) -> Value {
        let field = |k: &str| md.and_then(|m| m.get(k)).and_then(Value::as_str).filter(|x| !x.is_empty()).map(str::to_lowercase);
        let (hash, mac) = (field("EEPROM_HASH"), field("MAC_ADDR"));
        let history: Vec<Value> = self.read_history().into_iter().filter(|r| r.get("chip").and_then(Value::as_str) == Some(chip)).collect();
        let last_write = mac.as_ref().and_then(|m| history.iter().rfind(|r| r.get("mac").and_then(Value::as_str) == Some(m)).cloned());
        let with_last = |mut v: Value| {
            if let Some(l) = &last_write {
                v["lastWrite"] = l.clone();
            }
            v
        };
        let Some(hash) = hash else {
            return with_last(
                json!({ "source": "unknown", "buildDate": null, "label": "Unknown: the module didn't report an EEPROM hash", "bootConf": null }),
            );
        };
        let ours = history
            .iter()
            .rfind(|r| r.get("hashes").and_then(Value::as_array).is_some_and(|h| h.iter().any(|x| x.as_str() == Some(&hash))));
        if let Some(o) = ours {
            let date = o.get("buildDate").cloned().unwrap_or(Value::Null);
            return json!({
                "source": "benchbob",
                "buildDate": date,
                "label": format!("{}, written by BenchBob", benchbob_core::http::js_string(&date)),
                "bootConf": o.get("bootConf").cloned().unwrap_or(Value::Null),
                "writtenAt": o.get("at").cloned().unwrap_or(Value::Null),
            });
        }
        let base = self.paths.eeprom_fw_dir.join(format!("bootloader-{chip}"));
        let mut paths: Vec<String> = Vec::new();
        for channel in ["default", "latest", "stable", "beta", "critical"] {
            for f in channel_images(&base.join(channel)) {
                let p = s(&base.join(format!("{channel}/{f}")));
                if !paths.contains(&p) {
                    paths.push(p);
                }
            }
        }
        let stock = self.stock_image(chip).await;
        if let Some((p, _)) = &stock
            && !paths.contains(p)
        {
            paths.push(p.clone());
        }
        for p in &paths {
            let Some((sha, config)) = self.image_info(p).await else { continue };
            if sha != hash {
                continue;
            }
            let date = match &stock {
                Some((sp, sd)) if sp == p => sd.clone(),
                _ => build_date(p),
            };
            return with_last(
                json!({ "source": "stock", "buildDate": date, "label": format!("{date}, a stock release with its default settings"), "bootConf": config }),
            );
        }
        with_last(
            json!({ "source": "unknown", "buildDate": null, "label": "Unrecognised: not a stock release with default settings, and not written by BenchBob", "bootConf": null }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_revisions() {
        assert_eq!(decode_board_rev(Some("b03141")), json!({ "model": "CM4", "memory": "2 GB", "revision": "1.1" }));
        assert_eq!(decode_board_rev(Some("0xd04170")), json!({ "model": "Pi 5", "memory": "8 GB", "revision": "1.0" }));
        assert_eq!(decode_board_rev(Some("a1f990")), json!({ "model": "type 0x99", "memory": "1 GB", "revision": "1.0" }));
        assert_eq!(decode_board_rev(Some("f21ff0")), json!({ "model": "type 0xff", "memory": "?", "revision": "1.0" }));
        assert_eq!(decode_board_rev(Some("0002")), Value::Null);
        assert_eq!(decode_board_rev(Some("zz")), Value::Null);
        assert_eq!(decode_board_rev(None), Value::Null);
    }
}
