//! The two Pi boot-tool operations (server.ts "USB Boot Mode (rpiboot)" and "Bootloader
//! update SD cards"): rpiboot runs on a device in USB boot mode, keyed `usb:<port path>` in
//! the op queue, and bootloader update cards, a drive operation like erase or flash.
//!
//! A card is laid out like the official image (MBR, one FAT32 partition at 1 MiB with
//! 512-byte clusters) but sized to fit the card, and capped at the official 256 MiB
//! because recovery.bin wants a FAT partition under 32 GB.

use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::devices::{part_dev, prepare, size_bytes};
use crate::ops::Op;
use crate::rpiboot::{Firmware, Hooks, RpibootRun, card_board, decode_board_rev};
use crate::sys::Sys;
use crate::util::fmt_bytes;

pub const CARD_MOUNT: &str = "/tmp/benchbob-blcard";
pub const MIB: f64 = 1024.0 * 1024.0;
pub const CARD_PART_MAX: f64 = 256.0 * MIB;
pub const CARD_PART_MIN: f64 = 8.0 * MIB;

/// server.ts `mustRun`: fails with the last line of stderr.
async fn must(sys: &dyn Sys, cmd: &[&str], stdin: Option<&str>) -> anyhow::Result<String> {
    let r = sys.run_input(cmd, stdin.map(str::as_bytes)).await;
    let name = if cmd[0] == "sudo" { cmd.get(1).copied().unwrap_or("sudo") } else { cmd[0] };
    if r.code != 0 {
        let last = r.err.trim().split('\n').next_back().unwrap_or("").to_string();
        anyhow::bail!("{name} failed: {}", if last.is_empty() { format!("exit {}", r.code) } else { last });
    }
    Ok(r.out)
}

async fn unmount_all(sys: &dyn Sys, part: &str) -> anyhow::Result<()> {
    let out = sys.run(&["lsblk", "-nro", "MOUNTPOINT", part]).await.out;
    for mp in out.split('\n').filter(|s| !s.is_empty()) {
        must(sys, &["sudo", "umount", mp], None).await?;
    }
    Ok(())
}

fn cancelled(op: &Op) -> anyhow::Result<()> {
    if op.lock().unwrap().aborted {
        anyhow::bail!("Cancelled");
    }
    Ok(())
}

/// `doBootloaderCard`.
pub async fn bootloader_card(sys: &dyn Sys, fw: &dyn Firmware, op: &Op) -> anyhow::Result<()> {
    let (chip, image_id, boot_conf, device) = {
        let o = op.lock().unwrap();
        (o.chip.clone().unwrap_or_default(), o.eeprom_image_id.clone(), o.boot_conf.clone().unwrap_or_default(), o.device.clone())
    };
    let Some(image) = fw.eeprom_images(&chip).await.into_iter().find(|i| image_id.as_ref().and_then(Value::as_str) == Some(i.id.as_str()))
    else {
        anyhow::bail!("Unknown bootloader image");
    };
    let dev = format!("/dev/{device}");
    let part = part_dev(&device, 1);
    op.lock().unwrap().set("preparing", None, "Building bootloader files...");
    let card = fw.build_card_files(&chip, &image, &boot_conf).await?;
    let res: anyhow::Result<()> = async {
        op.lock().unwrap().set("", Some(10.0), "Unmounting...");
        prepare(sys, &device).await;
        cancelled(op)?;

        op.lock().unwrap().set("writing", Some(20.0), "Partitioning...");
        let total = size_bytes(sys, &device).await;
        let size = CARD_PART_MAX.min(((total - MIB) / MIB).floor() * MIB);
        if size < CARD_PART_MIN {
            anyhow::bail!("Card too small ({})", fmt_bytes(total));
        }
        must(sys, &["sudo", "wipefs", "-a", &dev], None).await?;
        let table = format!("label: dos\nstart=2048, size={}, type=c\n", crate::util::js_num_str(size / 512.0));
        must(sys, &["sudo", "sfdisk", "--wipe", "always", "--wipe-partitions", "always", &dev], Some(&table)).await?;
        sys.run(&["sudo", "partprobe", &dev]).await;
        sys.run(&["sudo", "udevadm", "settle"]).await;
        let mut i = 0;
        while i < 50 && sys.run(&["test", "-b", &part]).await.code != 0 {
            sys.sleep(100).await;
            i += 1;
        }
        cancelled(op)?;

        op.lock().unwrap().set("", Some(35.0), "Formatting...");
        // FAT32 needs 65525 clusters, about 33 MiB of 512-byte clusters; FAT16 below that.
        let fat: &[&str] = if size >= 64.0 * MIB { &["-F", "32", "-s", "1"] } else { &["-F", "16"] };
        let mut mkfs = vec!["sudo", "mkfs.vfat"];
        mkfs.extend_from_slice(fat);
        mkfs.extend(["-n", "BOOTLOADER", &part]);
        must(sys, &mkfs, None).await?;
        cancelled(op)?;

        op.lock().unwrap().set("", Some(50.0), "Copying files...");
        sys.run(&["sudo", "mkdir", "-p", CARD_MOUNT]).await;
        unmount_all(sys, &part).await?; // udev may have mounted the new filesystem already
        must(sys, &["sudo", "mount", "-t", "vfat", &part, CARD_MOUNT], None).await?;
        let copy: anyhow::Result<()> = async {
            for f in &card.files {
                must(sys, &["sudo", "cp", &format!("{}/{f}", card.dir), &format!("{CARD_MOUNT}/{f}")], None).await?;
            }
            Ok(())
        }
        .await;
        sys.run(&["sudo", "umount", CARD_MOUNT]).await;
        copy?;
        cancelled(op)?;

        // Read back from the card itself, not the page cache.
        op.lock().unwrap().set("", Some(75.0), "Verifying...");
        unmount_all(sys, &part).await?;
        must(sys, &["sudo", "blockdev", "--flushbufs", &part], None).await?;
        must(sys, &["sudo", "mount", "-o", "ro", "-t", "vfat", &part, CARD_MOUNT], None).await?;
        let verify: anyhow::Result<()> = async {
            let paths: Vec<String> = card.files.iter().map(|f| format!("{CARD_MOUNT}/{f}")).collect();
            let mut cmd = vec!["sudo", "sha256sum"];
            cmd.extend(paths.iter().map(String::as_str));
            let out = must(sys, &cmd, None).await?;
            let mut on_card: IndexMap<String, String> = IndexMap::new();
            for l in out.trim().split('\n') {
                let name = l[l.rfind('/').map_or(0, |i| i + 1)..].to_string();
                on_card.insert(name, l.split_whitespace().next().unwrap_or("").to_string());
            }
            for f in &card.files {
                let data = sys.read_bytes(&format!("{}/{f}", card.dir)).await.unwrap_or_default();
                let want: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
                if on_card.get(f) != Some(&want) {
                    anyhow::bail!("Verify failed: {f} on the card doesn't match what was written");
                }
            }
            Ok(())
        }
        .await;
        sys.run(&["sudo", "umount", CARD_MOUNT]).await;
        verify?;

        sys.run(&["sudo", "partprobe", &dev]).await; // lets udev mount it, so the files show up
        op.lock().unwrap().set(
            "done",
            Some(100.0),
            &format!(
                "Bootloader card ready and verified: {} for the {}. Insert it, power on, and wait for the green LED to blink rapidly (or a green HDMI screen).",
                image.label,
                card_board(&chip)
            ),
        );
        Ok(())
    }
    .await;
    sys.remove_dir(&card.dir).await;
    res
}

/// What a check or write found, kept after the op is gone (the device leaves boot mode
/// when recovery.bin is done, and the board details are the point of a check).
pub type Results = Mutex<IndexMap<String, Value>>;

/// `doRpiboot`: msd, eeprom-check or eeprom-write on a device in USB boot mode.
pub async fn rpiboot(sys: &dyn Sys, fw: &dyn Firmware, op: &Op, results: &Results) -> anyhow::Result<()> {
    let (usb_path, kind, image_id, boot_conf) = {
        let o = op.lock().unwrap();
        (o.usb_path.clone().unwrap_or_default(), o.kind.clone(), o.eeprom_image_id.clone(), o.boot_conf.clone().unwrap_or_default())
    };
    let Some(dev) = fw.boot_devices().await.into_iter().find(|d| d.path == usb_path) else {
        anyhow::bail!("Device is no longer in USB boot mode");
    };
    let log: Arc<Mutex<Vec<String>>> = Default::default();
    let hooks = {
        let (op1, op2, log) = (op.clone(), op.clone(), log.clone());
        Hooks {
            on_line: Box::new(move |l: &str| {
                log.lock().unwrap().push(l.to_string());
                op1.lock().unwrap().message =
                    if l.chars().count() > 160 { l.chars().take(160).collect::<String>() + "…" } else { l.to_string() };
            }),
            on_spawn: Box::new(move |k| op2.lock().unwrap().killer = Some(k)),
        }
    };
    {
        let mut o = op.lock().unwrap();
        o.status = "running".into();
        o.progress = 10.0;
    }
    let tail = || {
        let l = log.lock().unwrap();
        let t = l[l.len().saturating_sub(3)..].join(" | ");
        if t.is_empty() { "no output".to_string() } else { t }
    };

    if kind == "msd" {
        op.lock().unwrap().message = "Loading mass-storage gadget...".into();
        let r = fw.load_mass_storage(&dev, &hooks).await;
        cancelled(op)?;
        // The gadget is up once the boot ROM device has gone; rpiboot's exit code alone
        // doesn't say that.
        sys.sleep(1500).await;
        let still = fw.boot_devices().await.iter().any(|d| d.path == dev.path);
        if r.timed_out || (r.code != 0 && still) {
            anyhow::bail!(
                "rpiboot {}: {}",
                if r.timed_out { "timed out".to_string() } else { format!("failed (exit {})", r.code) },
                tail()
            );
        }
        op.lock().unwrap().set(
            "done",
            Some(100.0),
            "Mass-storage gadget loaded. The module's storage should appear under Connected Drives within a few seconds.",
        );
        return Ok(());
    }

    let mut image = None;
    let r: RpibootRun = if kind == "eeprom-check" {
        op.lock().unwrap().message = "Reading bootloader metadata...".into();
        fw.read_bootloader_info(&dev, &hooks).await?
    } else {
        let Some(img) =
            fw.eeprom_images(&dev.chip).await.into_iter().find(|i| image_id.as_ref().and_then(Value::as_str) == Some(i.id.as_str()))
        else {
            anyhow::bail!("Unknown bootloader image");
        };
        op.lock().unwrap().message = format!("Writing bootloader {}...", img.label);
        let r = fw.write_bootloader(&dev, &img, &boot_conf, &hooks).await?;
        image = Some(img);
        r
    };
    cancelled(op)?;

    let md = r.metadata.as_ref();
    let mdv = |k: &str| md.and_then(|m| m.get(k)).cloned();
    // Remember what was sent unless the module said it failed: a hash match later is proof
    // on its own, so recording a write whose outcome is unknown can't mislead.
    if let (Some(img), Some(sha)) = (&image, &r.image_sha256)
        && (md.is_none() || mdv("EEPROM_UPDATE").and_then(|v| v.as_str().map(String::from)).as_deref() == Some("success"))
    {
        let rec = json!({
            "at": sys.now_ms(),
            "chip": dev.chip,
            "mac": mdv("MAC_ADDR").filter(benchbob_core::http::truthy).unwrap_or(Value::Null),
            "imageLabel": img.label,
            "buildDate": img.build_date,
            "bootConf": boot_conf,
            "hashes": [sha, mdv("EEPROM_HASH").filter(benchbob_core::http::truthy).unwrap_or(Value::String(String::new()))],
        });
        if let Err(e) = fw.record_write(rec).await {
            tracing::error!("could not record bootloader write: {e:#}");
        }
    }
    let board = decode_board_rev(mdv("USER_BOARDREV").as_ref().and_then(Value::as_str));
    let why = if r.timed_out { "timed out".to_string() } else { format!("rpiboot exit {}", r.code) };
    let (ok, message) = if kind == "eeprom-check" {
        match md {
            Some(_) => (
                true,
                format!(
                    "Read metadata{}. Nothing was written. Power-cycle the module to leave recovery.",
                    if board.is_null() {
                        String::new()
                    } else {
                        format!(" from a {} ({})", board["model"].as_str().unwrap_or(""), board["memory"].as_str().unwrap_or(""))
                    }
                ),
            ),
            None => (false, format!("No metadata came back ({why}): {}", tail())),
        }
    } else {
        let upd = mdv("EEPROM_UPDATE").filter(benchbob_core::http::truthy);
        let ok = upd.as_ref().and_then(Value::as_str) == Some("success");
        let message = if ok {
            "Bootloader EEPROM written and verified. Power-cycle the module (remove the nRPIBOOT jumper to boot normally).".to_string()
        } else if let Some(u) = &upd {
            format!(
                "Bootloader update reported \"{}\". The old bootloader is most likely still in place.",
                benchbob_core::http::js_string(u)
            )
        } else {
            format!(
                "Outcome unknown, no metadata came back ({why}). A rapidly flashing green LED on the module means success. Last output: {}",
                tail()
            )
        };
        (ok, message)
    };
    let installed = match md {
        Some(m) => fw.identify(&dev.chip, Some(m)).await,
        None => Value::Null,
    };
    results.lock().unwrap().insert(
        dev.path.clone(),
        json!({
            "path": dev.path, "chip": dev.chip, "at": sys.now_ms(), "kind": kind, "ok": ok, "message": message,
            "metadata": md.cloned().map(Value::Object).unwrap_or(Value::Null), "board": board, "installed": installed,
        }),
    );
    op.lock().unwrap().set(if ok { "done" } else { "error" }, Some(100.0), &message);
    Ok(())
}
