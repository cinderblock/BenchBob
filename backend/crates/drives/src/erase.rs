//! Erasing a drive (server.ts "Erase"): quick (signatures, partition table and GPT
//! backup), full (zeros over everything, with dd's progress) or discard (TRIM).

use benchbob_core::http::parse_int;
use regex::Regex;

use crate::devices::prepare;
use crate::ops::Op;
use crate::sys::Sys;
use crate::util::{fmt_bytes, js_num_str, js_round};

/// The byte count of the last "N bytes" in dd's `status=progress` output.
pub fn dd_bytes(buf: &str) -> Option<f64> {
    Regex::new(r"(\d+) bytes").unwrap().captures_iter(buf).last().and_then(|c| c[1].parse().ok())
}

/// Keep the progress buffer small, as server.ts does.
pub fn trim_buf(buf: &mut String) {
    if buf.len() > 4096 {
        let mut at = buf.len() - 2048;
        while !buf.is_char_boundary(at) {
            at += 1;
        }
        *buf = buf[at..].to_string();
    }
}

pub async fn erase(sys: &dyn Sys, op: &Op) -> anyhow::Result<()> {
    let (dev, mode) = {
        let o = op.lock().unwrap();
        (o.device.clone(), o.mode.clone().unwrap_or_else(|| "quick".into()))
    };
    let path = format!("/dev/{dev}");
    op.lock().unwrap().set("preparing", None, "Unmounting...");
    prepare(sys, &dev).await;
    op.lock().unwrap().status = "erasing".into();
    match mode.as_str() {
        "quick" => {
            op.lock().unwrap().message = "Wiping signatures...".into();
            sys.run(&["sudo", "wipefs", "-a", &path]).await;
            op.lock().unwrap().set("", Some(30.0), "Zeroing partition table...");
            sys.run(&["sudo", "dd", "if=/dev/zero", &format!("of={path}"), "bs=1M", "count=10", "conv=fsync"]).await;
            op.lock().unwrap().progress = 60.0;
            let sz = sys.run(&["sudo", "blockdev", "--getsz", &path]).await.out;
            if let Some(sectors) = parse_int(&sz).filter(|s| *s > 20480) {
                op.lock().unwrap().message = "Zeroing GPT backup...".into();
                sys.run(&[
                    "sudo",
                    "dd",
                    "if=/dev/zero",
                    &format!("of={path}"),
                    "bs=512",
                    &format!("seek={}", sectors - 20480),
                    "count=20480",
                    "conv=fsync",
                ])
                .await;
            }
            op.lock().unwrap().set("done", Some(100.0), "Quick erase complete");
        }
        "full" => {
            let total = parse_int(&sys.run(&["sudo", "blockdev", "--getsize64", &path]).await.out).unwrap_or(0) as f64;
            op.lock().unwrap().message = format!("Writing zeros to {}...", fmt_bytes(total));
            let mut p = sys.spawn(&["sudo", "dd", "if=/dev/zero", &format!("of={path}"), "bs=4M", "conv=fsync", "status=progress"])?;
            op.lock().unwrap().killer = Some(p.killer());
            let mut buf = String::new();
            while let Some(c) = p.stderr().await {
                if op.lock().unwrap().aborted {
                    (p.killer())();
                    break;
                }
                buf.push_str(&String::from_utf8_lossy(&c));
                if let Some(n) = dd_bytes(&buf).filter(|_| total > 0.0) {
                    let mut o = op.lock().unwrap();
                    o.progress = js_round(n / total * 100.0);
                    o.message = format!("Erasing: {} / {} ({}%)", fmt_bytes(n), fmt_bytes(total), js_num_str(o.progress));
                }
                trim_buf(&mut buf);
            }
            p.wait().await;
            if op.lock().unwrap().aborted {
                op.lock().unwrap().set("error", None, "Cancelled");
                return Ok(());
            }
            op.lock().unwrap().set("done", Some(100.0), "Full erase complete");
        }
        "discard" => {
            op.lock().unwrap().message = "Discarding (TRIM)...".into();
            let r = sys.run(&["sudo", "blkdiscard", "-f", &path]).await;
            if r.code != 0 {
                let msg = if r.err.contains("not supported") || r.err.contains("not permitted") {
                    "Device does not support TRIM/discard (only SSDs do). Use Quick or Full erase instead.".to_string()
                } else {
                    format!("Discard failed: {}", r.err)
                };
                op.lock().unwrap().set("error", None, &msg);
                return Ok(());
            }
            op.lock().unwrap().set("done", Some(100.0), "Discard complete");
        }
        // An unknown mode does nothing more, and the op stays "erasing", as in server.ts.
        _ => {}
    }
    if op.lock().unwrap().status == "done" {
        sys.run(&["sudo", "partprobe", &path]).await;
    }
    Ok(())
}
