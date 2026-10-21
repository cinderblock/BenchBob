//! Flashing an image (server.ts "Flash"): download it (curl, no TLS stack here),
//! decompress it into dd, then apply the dashboard's settings to the new boot partition.

use serde_json::Value;

use crate::custom::{self, KeyFn};
use crate::devices::prepare;
use crate::erase::{dd_bytes, trim_buf};
use crate::ops::Op;
use crate::sys::Sys;
use crate::util::{fmt_bytes, js_round};

pub const FLASH_DIR: &str = "/var/tmp/benchbob-flash";

/// The shell pipeline that writes the downloaded file to the device. Run with
/// `bash -o pipefail`: without it a decompressor dying mid-stream (a truncated download)
/// leaves dd exiting 0 on a partial image, and the flash reports success.
pub fn write_cmd(filename: &str, dl: &str, dev: &str) -> String {
    let dd = format!("sudo dd of=/dev/{dev} bs=4M conv=fsync status=progress");
    if filename.ends_with(".xz") {
        format!("xzcat \"{dl}\" | {dd}")
    } else if filename.ends_with(".gz") {
        format!("gzip -dc \"{dl}\" | {dd}")
    } else if filename.ends_with(".zip") {
        format!("unzip -p \"{dl}\" \"*.img\" | {dd}")
    } else {
        format!("sudo dd if=\"{dl}\" of=/dev/{dev} bs=4M conv=fsync status=progress")
    }
}

/// The download's file name: the URL's last path segment, as server.ts takes it. Anything
/// that would leave FLASH_DIR or break the shell quoting is refused.
pub fn file_name(url: &str) -> anyhow::Result<String> {
    let f = url.rsplit('/').next().unwrap_or("").to_string();
    if f.is_empty() || f == "." || f == ".." || f.contains(['"', '\\', '$', '`', '\0', '\n']) {
        anyhow::bail!("Bad image URL");
    }
    Ok(f)
}

pub async fn flash(sys: &dyn Sys, op: &Op, key: KeyFn) -> anyhow::Result<()> {
    let (url, dev, ex_size, settings, init_format) = {
        let o = op.lock().unwrap();
        (o.image_url.clone(), o.device.clone(), o.extract_size, o.settings.clone(), o.init_format.clone())
    };
    let filename = file_name(&url)?;
    let dl = format!("{FLASH_DIR}/{filename}");
    sys.run(&["mkdir", "-p", FLASH_DIR]).await;

    op.lock().unwrap().set("downloading", None, "Starting download...");
    {
        let op = op.clone();
        let mut progress = move |done: u64, total: u64| {
            let mut o = op.lock().unwrap();
            if o.aborted {
                return false;
            }
            o.progress = if total > 0 { js_round(done as f64 / total as f64 * 40.0) } else { 0.0 };
            o.message = format!(
                "Downloading: {}{}",
                fmt_bytes(done as f64),
                if total > 0 { format!(" / {}", fmt_bytes(total as f64)) } else { String::new() }
            );
            true
        };
        sys.download(&url, &dl, &mut progress).await?;
    }

    op.lock().unwrap().set("preparing", Some(40.0), "Unmounting...");
    prepare(sys, &dev).await;

    op.lock().unwrap().set("flashing", None, "Writing image...");
    let cmd = write_cmd(&filename, &dl, &dev);
    let mut p = sys.spawn(&["bash", "-o", "pipefail", "-c", &cmd])?;
    op.lock().unwrap().killer = Some(p.killer());
    let mut buf = String::new();
    while let Some(c) = p.stderr().await {
        if op.lock().unwrap().aborted {
            (p.killer())();
            break;
        }
        buf.push_str(&String::from_utf8_lossy(&c));
        if let Some(n) = dd_bytes(&buf).filter(|_| ex_size > 0.0) {
            let mut o = op.lock().unwrap();
            o.progress = 40.0 + js_round(n / ex_size * 45.0);
            o.message = format!("Flashing: {} / {}", fmt_bytes(n), fmt_bytes(ex_size));
        }
        trim_buf(&mut buf);
    }
    let exit = p.wait().await;
    if op.lock().unwrap().aborted {
        anyhow::bail!("Cancelled");
    }
    if exit != 0 {
        anyhow::bail!("Flash failed");
    }

    {
        let mut o = op.lock().unwrap();
        o.status = "configuring".into();
        o.progress = 85.0;
    }
    sys.run(&["sudo", "partprobe", &format!("/dev/{dev}")]).await;
    sys.sleep(2000).await;
    let mut applied = String::new();
    if let Some(s) = settings.filter(|s| matches!(s, Value::Object(m) if !m.is_empty()) || matches!(s, Value::Array(a) if !a.is_empty())) {
        op.lock().unwrap().message = "Applying settings...".into();
        match custom::apply(sys, &dev, &s, init_format.as_deref(), key).await {
            Ok(a) => applied = a,
            Err(e) => {
                // The image itself is good; say so, so nobody re-flashes to fix a settings problem.
                op.lock().unwrap().set("error", None, &format!("Image written, but settings were NOT applied: {e}"));
                sys.run(&["rm", "-f", &dl]).await;
                return Ok(());
            }
        }
    }
    op.lock().unwrap().set(
        "done",
        Some(100.0),
        &if applied.is_empty() { "Flash complete!".to_string() } else { format!("Flash complete! {applied}") },
    );
    sys.run(&["rm", "-f", &dl]).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_commands() {
        assert_eq!(
            write_cmd("a.img.xz", "/d/a.img.xz", "sda"),
            "xzcat \"/d/a.img.xz\" | sudo dd of=/dev/sda bs=4M conv=fsync status=progress"
        );
        assert_eq!(
            write_cmd("a.zip", "/d/a.zip", "sda"),
            "unzip -p \"/d/a.zip\" \"*.img\" | sudo dd of=/dev/sda bs=4M conv=fsync status=progress"
        );
        assert_eq!(write_cmd("a.img", "/d/a.img", "sdb"), "sudo dd if=\"/d/a.img\" of=/dev/sdb bs=4M conv=fsync status=progress");
        assert!(file_name("https://x/\"$(reboot)\".img").is_err());
        assert!(file_name("https://x/a/").is_err());
        assert_eq!(file_name("https://x/a/b.img.xz").unwrap(), "b.img.xz");
    }
}
