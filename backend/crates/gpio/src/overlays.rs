//! Boot-time configuration routes: config.txt on the Pi, device-tree overlays elsewhere
//! (gpio.ts "interfaces" and "boot overlays").
//!
//! Overlays: the list U-Boot applies at boot, in whichever file this image keeps it (the
//! board file lists the candidates), or the board's script. Only overlays installed on this
//! image can be enabled (a missing .dtbo makes U-Boot skip it or, on Armbian, run its fixup
//! script), and a change that puts two of the board's documented overlays on one pin is
//! refused, as on the Pi.

use std::sync::Arc;

use benchbob_boards::{Boot, BootKind, OverlayChoice};
use benchbob_core::http::{HttpError, js_string};
use benchbob_core::timeline::now_ms;
use serde_json::{Value, json};

use crate::boot_env::{self as BE, EnvKind};
use crate::config_txt as G;
use crate::script::Script;
use crate::{Inner, Staged, backup_and_write};

fn http(status: u16, m: impl Into<String>) -> HttpError {
    HttpError::new(status, m)
}

pub struct Target {
    pub kind: BootKind,
    pub path: String,
    /// None for a script.
    pub env: Option<EnvKind>,
    pub boot: Boot,
    pub test_copy: bool,
}

/// Where this machine keeps its boot overlays, if BenchBob can edit them.
pub fn target(i: &Inner) -> Option<Target> {
    let def = &i.board.def;
    let boot = def.boot.as_ref()?;
    let found = def.boot_config();
    let over = i.env.boot_env.clone();
    let kind = match &over {
        Some(_) => found.as_ref().map(|f| f.kind).or_else(|| boot.files.iter().find(|f| f.kind != BootKind::ConfigTxt).map(|f| f.kind)),
        None => found.as_ref().map(|f| f.kind),
    }?;
    if kind == BootKind::ConfigTxt {
        return None;
    }
    let env = match kind {
        BootKind::Extlinux => Some(EnvKind::Extlinux),
        BootKind::UEnv => Some(EnvKind::UEnv),
        BootKind::Script => None,
        _ => Some(EnvKind::Env),
    };
    // A script is its own test copy; BENCHBOB_BOOT_ENV redirects the built-in editors.
    let (path, test_copy) = match (&over, kind) {
        (Some(o), k) if k != BootKind::Script => (o.clone(), true),
        _ => (found?.path, false),
    };
    Some(Target { kind, path, env, boot: boot.clone(), test_copy })
}

pub fn no_overlays(i: &Inner) -> String {
    let b = &i.board;
    if i.is_pi() {
        return "This is a Raspberry Pi: its buses are in config.txt (GET /api/gpio/interfaces)".into();
    }
    let files: Vec<String> = b.def.boot.as_ref().map(|x| x.files.iter().map(|f| f.path.clone()).collect()).unwrap_or_default();
    if files.is_empty() {
        return format!("BenchBob doesn't know where {} keeps its boot configuration", b.name);
    }
    let notes = if b.def.notes.is_empty() { String::new() } else { format!(" ({})", b.def.notes.join(" ")) };
    format!("None of {} exists on this image, so BenchBob can't change boot overlays here{notes}", files.join(", "))
}

/// config.txt routes on a board that has none: say where to look instead.
pub fn pi_only(i: &Inner) -> Result<(), HttpError> {
    if i.is_pi() {
        return Ok(());
    }
    let hint = if target(i).is_some() { "Boot overlays: GET /api/gpio/overlays".to_string() } else { no_overlays(i) };
    Err(http(409, format!("{} isn't a Raspberry Pi: there is no config.txt. {hint}", i.board.name)))
}

/// The board's documented overlay that this name (an installed file's, or one in the list) is.
fn known<'a>(t: &'a Target, name: &str) -> Option<&'a OverlayChoice> {
    let short = name.rsplit('/').next().unwrap_or(name);
    let short = short.strip_suffix(".dtbo").unwrap_or(short);
    t.boot.overlays.iter().find(|o| std::iter::once(&o.name).chain(&o.aliases).any(|n| short == n || short.ends_with(&format!("-{n}"))))
}

fn overlay_dirs(i: &Inner, t: &Target) -> Vec<String> {
    match &i.env.overlay_dir {
        Some(d) => vec![d.clone()],
        None => t.boot.overlay_dirs.clone(),
    }
}

/// .dtbo files this image has, as they'd be written in the boot file.
fn installed(i: &Inner, t: &Target, prefix: Option<&str>) -> Vec<Value> {
    let mut out = Vec::new();
    for dir in overlay_dirs(i, t) {
        let mut files: Vec<String> = match std::fs::read_dir(&dir) {
            Ok(rd) => {
                rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|f| f.ends_with(".dtbo")).collect()
            }
            Err(_) => continue,
        };
        files.sort();
        for f in files {
            let base = &f[..f.len() - 5];
            // env: Armbian/Orange Pi load <prefix>-<name>.dtbo, so the short name is written;
            // a file without the prefix is written whole (the boot scripts try that too).
            // extlinux / uEnv: the path as the boot loader sees it.
            let name = match t.env {
                Some(EnvKind::Extlinux) | Some(EnvKind::UEnv) => {
                    format!("{}{f}", t.boot.overlay_ref.clone().unwrap_or_else(|| format!("{dir}/")))
                }
                _ => match prefix.and_then(|p| base.strip_prefix(&format!("{p}-"))) {
                    Some(short) => short.to_string(),
                    None => base.to_string(),
                },
            };
            let k = known(t, &name);
            out.push(json!({ "name": name, "file": format!("{dir}/{f}"), "what": k.map(|k| k.what.clone()), "pins": k.map(|k| k.pins.clone()).unwrap_or_default() }));
        }
    }
    out
}

fn conflicts(t: &Target, enabled: &[String]) -> Vec<(u32, Vec<String>)> {
    let mut by: Vec<(u32, Vec<String>)> = Vec::new();
    for n in enabled {
        for p in known(t, n).map(|k| k.pins.clone()).unwrap_or_default() {
            match by.iter_mut().find(|x| x.0 == p) {
                Some(x) => x.1.push(n.clone()),
                None => by.push((p, vec![n.clone()])),
            }
        }
    }
    let mut c: Vec<(u32, Vec<String>)> = by.into_iter().filter(|x| x.1.len() > 1).collect();
    c.sort_by_key(|x| x.0);
    c
}

fn conflicts_json(c: &[(u32, Vec<String>)]) -> Value {
    Value::Array(c.iter().map(|(p, u)| json!({ "pin": p, "users": u })).collect())
}

fn read_raw(path: &str) -> Result<String, HttpError> {
    // Raw text: the diff and the write keep the file's own line endings.
    std::fs::read_to_string(path).map_err(|_| http(500, format!("Can't read {path}")))
}

fn script<'a>(i: &'a Inner, t: &'a Target) -> Script<'a> {
    Script { path: &t.path, board: &i.board.id, overlay_dirs: &t.boot.overlay_dirs }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
}

/// The current overlays: (enabled, prefix, label, user, path shown, installed list).
async fn current(i: &Inner, t: &Target) -> Result<(BE::Overlays, String, Vec<Value>), HttpError> {
    match t.env {
        Some(env) => {
            let cur = BE::read_overlays(env, &read_raw(&t.path)?);
            let inst = installed(i, t, cur.prefix.as_deref());
            Ok((cur, t.path.clone(), inst))
        }
        None => {
            let s = script(i, t);
            let r = s.call(&["read"], 30_000).await.map_err(|e| http(502, e))?;
            let l = s.call(&["list"], 30_000).await.map_err(|e| http(502, e))?;
            let cur = BE::Overlays {
                enabled: strings(&r["enabled"]),
                prefix: r.get("prefix").and_then(Value::as_str).map(String::from),
                user: strings(&r["user"]),
                label: r.get("label").and_then(Value::as_str).map(String::from),
            };
            let inst = l
                .get("installed")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|o| {
                            let name = o.get("name")?.as_str()?.to_string();
                            let k = known(t, &name);
                            Some(json!({
                                "name": name,
                                "file": o.get("file").cloned().unwrap_or(Value::Null),
                                "what": o.get("what").cloned().filter(|v| !v.is_null()).unwrap_or_else(|| json!(k.map(|k| k.what.clone()))),
                                "pins": o.get("pins").cloned().filter(|v| v.is_array()).unwrap_or_else(|| json!(k.map(|k| k.pins.clone()).unwrap_or_default())),
                            }))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let shown = r.get("path").and_then(Value::as_str).unwrap_or(&t.path).to_string();
            Ok((cur, shown, inst))
        }
    }
}

pub async fn info(i: &Arc<Inner>) -> Result<Value, HttpError> {
    let t = target(i).ok_or_else(|| http(409, no_overlays(i)))?;
    let (cur, path, inst) = current(i, &t).await?;
    let is_installed = |n: &str| inst.iter().any(|x| x["name"] == n);
    let (reboot, staged) = {
        let st = i.st.lock().unwrap();
        (st.reboot_required, st.staged_ov.as_ref().map(|s| json!({ "token": s.token, "summary": s.summary, "at": s.at })))
    };
    Ok(json!({
        "kind": t.kind.as_str(),
        "path": path,
        "testCopy": t.test_copy,
        "prefix": cur.prefix,
        "label": cur.label,
        "enabled": cur.enabled.iter().map(|n| json!({ "name": n, "what": known(&t, n).map(|k| k.what.clone()), "pins": known(&t, n).map(|k| k.pins.clone()).unwrap_or_default(), "installed": is_installed(n) })).collect::<Vec<_>>(),
        "user": cur.user,
        "installed": inst,
        "overlayDirs": overlay_dirs(i, &t),
        "conflicts": conflicts_json(&conflicts(&t, &cur.enabled)),
        "rebootRequired": reboot,
        "staged": staged,
    }))
}

fn list(v: Option<&Value>, what: &str) -> Result<Vec<String>, HttpError> {
    match v {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => Ok(strings(&Value::Array(a.clone()))),
        _ => Err(http(400, format!("{what} must be a list of overlay names"))),
    }
}

pub async fn stage(i: &Arc<Inner>, a: &Value) -> Result<Value, HttpError> {
    let t = target(i).ok_or_else(|| http(409, no_overlays(i)))?;
    let enable = list(a.get("enable"), "enable")?;
    let disable = list(a.get("disable"), "disable")?;
    let (cur, path, inst) = current(i, &t).await?;
    let missing: Vec<&String> =
        enable.iter().filter(|n| !cur.enabled.contains(n) && !inst.iter().any(|x| x["name"] == n.as_str())).collect();
    if !missing.is_empty() {
        let dirs = overlay_dirs(i, &t);
        return Err(http(
            400,
            format!(
                "Not installed on this image: {} (looked in {})",
                missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
                if dirs.is_empty() { "no overlay directory".to_string() } else { dirs.join(", ") }
            ),
        ));
    }
    let change = json!({ "enable": enable, "disable": disable });
    // The built-in editors compute the new text; a script says what the list becomes.
    let (raw, next, after, script_diff, mut warnings, script_summary) = match t.env {
        Some(env) => {
            let raw = read_raw(&t.path)?;
            let next = BE::edit_overlays(env, &raw, &enable, &disable).map_err(|e| http(400, e))?;
            let after = BE::read_overlays(env, &next).enabled;
            (raw, next, after, None, vec![], None)
        }
        None => {
            let r = script(i, &t).call(&["stage", &change.to_string()], 30_000).await.map_err(|e| http(400, e))?;
            let after = strings(&r["enabled"]);
            (
                String::new(),
                String::new(),
                after,
                Some(r.get("diff").and_then(Value::as_str).unwrap_or("").to_string()),
                strings(&r["warnings"]),
                Some(strings(&r["summary"])),
            )
        }
    };
    let key = |c: &(u32, Vec<String>)| format!("{}:{}", c.0, c.1.join("|"));
    let had: Vec<String> = conflicts(&t, &cur.enabled).iter().map(key).collect();
    let fresh: Vec<(u32, Vec<String>)> = conflicts(&t, &after).into_iter().filter(|c| !had.contains(&key(c))).collect();
    if !fresh.is_empty() {
        let what: Vec<String> = fresh.iter().map(|c| format!("pin {} would be {}", c.0, c.1.join(" and "))).collect();
        return Err(http(400, format!("Pin conflict: {}. Turn one off first.", what.join("; "))));
    }
    for c in conflicts(&t, &after) {
        warnings.push(format!("Pin {} is already claimed by {}; only one of them can work.", c.0, c.1.join(" and ")));
    }
    for n in &after {
        if known(&t, n).is_none() && !cur.enabled.contains(n) {
            warnings.push(format!(
                "{n} isn't one of the overlays documented for {}, so its pins can't be checked for conflicts.",
                i.board.name
            ));
        }
    }
    if t.env == Some(EnvKind::UEnv) && !after.is_empty() && !BE::uenv_overlays_on(&next) {
        warnings.push("uEnv.txt doesn't set enable_uboot_overlays=1, so U-Boot won't load these overlays until it does.".into());
    }
    let diff = match &script_diff {
        Some(d) => d.clone(),
        None => G::unified_diff(&raw, &next, &t.path, 3),
    };
    let changed = if script_diff.is_some() { after != cur.enabled } else { !diff.is_empty() };
    if !changed {
        i.st.lock().unwrap().staged_ov = None;
        return Ok(json!({ "diff": "", "summary": [], "warnings": warnings, "message": format!("No change: {path} already says that.") }));
    }
    let mut summary: Vec<String> = after.iter().filter(|n| !cur.enabled.contains(n)).map(|n| format!("{n} on")).collect();
    summary.extend(cur.enabled.iter().filter(|n| !after.contains(n)).map(|n| format!("{n} off")));
    if let Some(s) = script_summary.filter(|s| !s.is_empty()) {
        summary = s;
    }
    let token = uuid::Uuid::new_v4().to_string();
    let base = if script_diff.is_some() { cur.enabled.join(" ") } else { raw };
    i.st.lock().unwrap().staged_ov = Some(Staged {
        also: None,
        token: token.clone(),
        text: next,
        base,
        path: t.path.clone(),
        at: now_ms(),
        summary: summary.clone(),
        change: script_diff.is_some().then_some(change),
    });
    Ok(json!({ "token": token, "diff": diff, "summary": summary, "warnings": warnings, "configPath": path, "testCopy": t.test_copy }))
}

pub async fn apply(i: &Arc<Inner>, a: &Value) -> Result<Value, HttpError> {
    if a.get("confirm") != Some(&Value::Bool(true)) {
        return Err(http(400, "Applying changes the boot configuration; repeat with \"confirm\": true"));
    }
    let token = a.get("token").map(js_string);
    let staged = {
        let mut st = i.st.lock().unwrap();
        match &st.staged_ov {
            Some(s) if Some(&s.token) == token.as_ref() => st.staged_ov.take().unwrap(),
            _ => return Err(http(409, "Nothing staged with that token (stage again)")),
        }
    };
    let t = target(i).ok_or_else(|| http(409, no_overlays(i)))?;
    let (backup, message) = match &staged.change {
        // A script applies its own change (and keeps its own backup), once the list is
        // still what it was when staged.
        Some(change) => {
            let s = script(i, &t);
            let now = strings(&s.call(&["read"], 30_000).await.map_err(|e| http(502, e))?["enabled"]).join(" ");
            if now != staged.base {
                return Err(http(409, format!("{} changed since the diff was made; stage again", staged.path)));
            }
            let r = s.call(&["apply", &change.to_string()], 120_000).await.map_err(|e| http(502, e))?;
            (r.get("backup").cloned().unwrap_or(Value::Null), r.get("message").and_then(Value::as_str).map(String::from))
        }
        None => {
            if read_raw(&staged.path)? != staged.base {
                return Err(http(409, format!("{} changed since the diff was made; stage again", staged.path)));
            }
            (Value::String(backup_and_write(&staged.path, &staged.text).await?), None)
        }
    };
    i.st.lock().unwrap().reboot_required = true;
    let b = backup.as_str().map(|b| format!("; backup {b}")).unwrap_or_default();
    i.ctx().event(&format!("Boot overlays changed in {} ({}){b}; reboot required", staged.path, staged.summary.join(", ")), None);
    i.ctx().changed();
    Ok(
        json!({ "ok": true, "backup": backup, "summary": staged.summary, "rebootRequired": true, "message": message.unwrap_or_else(|| "Saved. The change takes effect after a reboot.".into()) }),
    )
}

// ---------- the Pi's config.txt ----------

pub async fn interfaces(i: &Arc<Inner>) -> Result<Value, HttpError> {
    let cfg = i.env.config.display().to_string();
    let text = benchbob_core::run::read_text(&i.env.config);
    if text.is_empty() {
        return Err(http(500, format!("Can't read {cfg}")));
    }
    let configured = G::read_interfaces(&text);
    let live = crate::interfaces::live(&i.env.cmdline);
    let bcm2711 = crate::interfaces::is_bcm2711();
    let (reboot, staged) = {
        let st = i.st.lock().unwrap();
        (st.reboot_required, st.staged.as_ref().map(|s| json!({ "token": s.token, "summary": s.summary, "at": s.at })))
    };
    Ok(json!({
        "configPath": cfg,
        "testCopy": i.env.config_overridden,
        "configured": configured.to_json(),
        "live": live,
        // What the dialog may offer here, and the pin tables behind it (so the UI doesn't keep its own copy).
        "catalog": if bcm2711 { G::catalog() } else { Value::Null },
        "pins": G::usage_json(&G::pin_usage(&configured)),
        "conflicts": G::conflicts_json(&G::pin_conflicts(&configured)),
        "rebootRequired": reboot,
        "staged": staged,
    }))
}

pub async fn stage_config(i: &Arc<Inner>, a: &Value) -> Result<Value, HttpError> {
    let cfg = i.env.config.display().to_string();
    // Raw text (not trimmed): the diff and the write keep the file's own endings.
    let raw = read_raw(&cfg)?;
    let ch = crate::interfaces::parse_changes(a)?;
    let enables_extra = ch.extra_i2c.iter().any(|c| c.1 == Some(true)) || ch.extra_uart.iter().any(|c| c.1 == Some(true));
    if enables_extra && !crate::interfaces::is_bcm2711() {
        return Err(http(
            400,
            "Extra I²C buses and UARTs (i2c3–6, uart2–5) are BCM2711 overlays (Pi 4, Pi 400, CM4); this board isn't one",
        ));
    }
    let next = G::edit_config(&raw, &ch).map_err(|e| http(400, e))?;
    let diff = G::unified_diff(&raw, &next, &cfg, 3);
    let (before, after) = (G::read_interfaces(&raw), G::read_interfaces(&next));
    let mut warnings: Vec<String> =
        after.conditional.iter().map(|(s, l)| format!("[{s}] also has \"{l}\"; on a board matching [{s}] that line wins.")).collect();
    if before.uart && !after.uart && benchbob_core::run::read_text(&i.env.cmdline).contains("console=serial0") {
        warnings.push(
            "cmdline.txt puts a login console on serial0; with the UART off that console disappears (cmdline.txt is not changed here)."
                .into(),
        );
    }
    // Two interfaces on one pin can't both work: refuse any overlap this change creates,
    // and keep reminding about ones config.txt already had.
    let key = |c: &(i64, Vec<String>)| format!("{}:{}", c.0, c.1.join("|"));
    let had: Vec<String> = G::pin_conflicts(&before).iter().map(key).collect();
    let fresh: Vec<(i64, Vec<String>)> = G::pin_conflicts(&after).into_iter().filter(|c| !had.contains(&key(c))).collect();
    if !fresh.is_empty() {
        let what: Vec<String> = fresh.iter().map(|c| format!("GPIO{} would be {}", c.0, c.1.join(" and "))).collect();
        return Err(http(400, format!("Pin conflict: {}. Turn one off or move it first.", what.join("; "))));
    }
    for c in G::pin_conflicts(&after) {
        warnings.push(format!("GPIO{} is already claimed twice in config.txt ({}); only one of them can work.", c.0, c.1.join(", ")));
    }
    let hat = |r: &G::Interfaces| G::pin_usage(r).iter().any(|u| u.gpio <= 1);
    if hat(&after) && !hat(&before) {
        warnings.push("GPIO0/1 are the HAT ID EEPROM bus: the firmware probes them at boot. Fine with no HAT attached; with a HAT that has an ID EEPROM, don't use them.".into());
    }
    if ch.i2c_gpio.as_ref().is_some_and(|g| g.enabled == Some(true)) && after.i2c_gpio.bus.is_none() {
        warnings.push("i2c-gpio without a bus number gets the next free /dev/i2c-N (probably 3, or higher if I²C3 is on); set a bus number for a stable name.".into());
    }
    // spidev.bufsiz lives on the kernel command line: the same change also edits cmdline.txt.
    let mut diff = diff;
    let mut also = None;
    let mut cmd_summary = Vec::new();
    if let Some(n) = ch.spidev_bufsiz {
        let path = i.env.cmdline.display().to_string();
        let raw_cmd = read_raw(&path)?;
        let next_cmd = G::edit_cmdline(&raw_cmd, n).map_err(|e| http(400, e))?;
        if next_cmd != raw_cmd {
            diff.push_str(&G::unified_diff(&raw_cmd, &next_cmd, &path, 0));
            cmd_summary.push(match n {
                Some(n) => format!("spidev transfers up to {n} bytes (cmdline.txt)"),
                None => "spidev transfer limit back to 4096 bytes (cmdline.txt)".into(),
            });
            also = Some((path, raw_cmd, next_cmd));
        }
    }
    if diff.is_empty() {
        i.st.lock().unwrap().staged = None;
        return Ok(json!({ "diff": "", "summary": [], "warnings": warnings, "message": "No change: config.txt already says that." }));
    }
    let token = uuid::Uuid::new_v4().to_string();
    let mut summary = crate::interfaces::describe(&before, &after);
    summary.extend(cmd_summary);
    i.st.lock().unwrap().staged = Some(Staged {
        token: token.clone(),
        text: next,
        base: raw,
        path: cfg.clone(),
        at: now_ms(),
        summary: summary.clone(),
        change: None,
        also,
    });
    Ok(
        json!({ "token": token, "diff": diff, "summary": summary, "warnings": warnings, "configPath": cfg, "testCopy": i.env.config_overridden }),
    )
}

pub async fn apply_config(i: &Arc<Inner>, a: &Value) -> Result<Value, HttpError> {
    if a.get("confirm") != Some(&Value::Bool(true)) {
        return Err(http(400, "Applying changes config.txt; repeat with \"confirm\": true"));
    }
    let token = a.get("token").map(js_string);
    let staged = {
        let mut st = i.st.lock().unwrap();
        match &st.staged {
            Some(s) if Some(&s.token) == token.as_ref() => st.staged.take().unwrap(),
            _ => return Err(http(409, "Nothing staged with that token (stage again)")),
        }
    };
    if std::fs::read_to_string(&staged.path).unwrap_or_default() != staged.base {
        return Err(http(409, format!("{} changed since the diff was made; stage again", staged.path)));
    }
    if let Some((p, base, _)) = &staged.also
        && std::fs::read_to_string(p).unwrap_or_default() != *base
    {
        return Err(http(409, format!("{p} changed since the diff was made; stage again")));
    }
    // config.txt may not change at all (a cmdline.txt-only change).
    let mut backup = String::new();
    if staged.text != staged.base {
        backup = backup_and_write(&staged.path, &staged.text).await?;
    }
    if let Some((p, _, text)) = &staged.also {
        let b = backup_and_write(p, text).await?;
        backup = if backup.is_empty() { b } else { format!("{backup}, {b}") };
    }
    i.st.lock().unwrap().reboot_required = true;
    i.ctx().event(&format!("config.txt changed ({}); backup {backup}; reboot required", staged.summary.join(", ")), None);
    i.ctx().changed();
    Ok(
        json!({ "ok": true, "backup": backup, "summary": staged.summary, "rebootRequired": true, "message": "Saved. The change takes effect after a reboot." }),
    )
}
