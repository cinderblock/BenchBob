//! Boot-time device-tree overlays outside the Raspberry Pi (peripherals/lib/boot-env.ts,
//! plus BeagleBone uEnv.txt). Pure reading and editing of the files U-Boot reads:
//!
//! - `env`: Armbian's /boot/armbianEnv.txt and Orange Pi's /boot/orangepiEnv.txt, key=value
//!   lines. `overlays=` is a space-separated list of short names; the boot script loads
//!   <overlay dir>/<overlay_prefix>-<name>.dtbo for each. `user_overlays=` names .dtbo files
//!   the user compiled (left alone). U-Boot's `env import -t` lets a later line override an
//!   earlier one, so the last `overlays=` counts and is the one edited.
//! - `extlinux`: extlinux.conf (BeagleBoard.org's K3 images), where the default label's
//!   `fdtoverlays` line lists .dtbo paths. Only that label is edited.
//! - `uenv`: BeagleBoard.org's AM335x /boot/uEnv.txt, eight slots
//!   `uboot_overlay_addr0..7=<path>.dtbo` (BeagleBone Black/Green). New overlays take a free
//!   slot, custom-cape slots 4-7 first (0-3 are meant to override the cape EEPROMs), and
//!   un-comment that slot's `#uboot_overlay_addrN=` placeholder where there is one. A
//!   disabled overlay's line is commented out, as the stock file does.
//!
//! Same contract as the Pi's config.txt editor: the caller shows a unified diff, backs the
//! file up and writes only after a confirm; line endings and every line we don't manage are
//! kept byte for byte.

use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvKind {
    Env,
    Extlinux,
    UEnv,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Overlays {
    /// Enabled overlays as written: short names (env) or .dtbo paths (extlinux, uEnv).
    pub enabled: Vec<String>,
    /// env: overlay_prefix (e.g. "rockchip-rk3588", "sun50i-h616").
    pub prefix: Option<String>,
    /// env: user_overlays=, shown but never edited.
    pub user: Vec<String>,
    /// extlinux: the label whose fdtoverlays line is read and edited.
    pub label: Option<String>,
}

fn split(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n").split('\n').map(String::from).collect()
}
fn join(lines: &[String], text: &str) -> String {
    lines.join("\n").replace('\n', if text.contains("\r\n") { "\r\n" } else { "\n" })
}
fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

// ---------- env ----------

fn env_lines(lines: &[String], key: &str) -> Vec<usize> {
    let re = Regex::new(&format!(r"^\s*{key}\s*=")).unwrap();
    lines.iter().enumerate().filter(|(_, l)| re.is_match(l)).map(|(i, _)| i).collect()
}
fn env_value(l: &str) -> &str {
    &l[l.find('=').map(|i| i + 1).unwrap_or(l.len())..]
}

fn read_env(text: &str) -> Overlays {
    let lines = split(text);
    let last = |k: &str| env_lines(&lines, k).last().map(|&i| env_value(&lines[i]).to_string());
    Overlays {
        enabled: words(&last("overlays").unwrap_or_default()),
        prefix: last("overlay_prefix").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        user: words(&last("user_overlays").unwrap_or_default()),
        label: None,
    }
}

fn edit_env(text: &str, enabled: &[String]) -> String {
    let mut lines = split(text);
    let hits = env_lines(&lines, "overlays");
    let line = format!("overlays={}", enabled.join(" "));
    if let Some(&i) = hits.last() {
        if enabled.is_empty() {
            lines.remove(i);
        } else {
            lines[i] = line;
        }
    } else if !enabled.is_empty() {
        // Before the file's trailing blank line(s), so the file still ends the way it did.
        let mut at = lines.len();
        while at > 0 && lines[at - 1].trim().is_empty() {
            at -= 1;
        }
        lines.insert(at, line);
    }
    join(&lines, text)
}

// ---------- extlinux.conf ----------

struct Block {
    label: String,
    start: usize,
    end: usize,
}

fn blocks(lines: &[String]) -> (Vec<Block>, Option<String>) {
    let re = Regex::new(r"^\s*(\w+)\s*(.*?)\s*$").unwrap();
    let mut out: Vec<Block> = Vec::new();
    let mut def = None;
    for (i, l) in lines.iter().enumerate() {
        let Some(m) = re.captures(l) else { continue };
        let k = m[1].to_lowercase();
        if k == "default" && out.is_empty() {
            def = Some(m[2].to_string());
        }
        if k == "label" {
            if let Some(b) = out.last_mut() {
                b.end = i;
            }
            out.push(Block { label: m[2].to_string(), start: i, end: lines.len() });
        }
    }
    (out, def)
}

/// The label U-Boot boots without input: `default`'s, else the first.
fn boot_block(lines: &[String]) -> Option<Block> {
    let (b, def) = blocks(lines);
    let i = b.iter().position(|x| Some(&x.label) == def.as_ref()).unwrap_or(0);
    b.into_iter().nth(i)
}

fn key_line(lines: &[String], b: &Block, key: &str) -> Option<usize> {
    let re = Regex::new(&format!(r"(?i)^\s*{key}\b")).unwrap();
    (b.start + 1..b.end).find(|&i| re.is_match(&lines[i]))
}

fn read_extlinux(text: &str) -> Overlays {
    let lines = split(text);
    let Some(b) = boot_block(&lines) else { return Overlays::default() };
    let enabled = key_line(&lines, &b, "fdtoverlays")
        .map(|i| words(&Regex::new(r"(?i)^\s*fdtoverlays\b").unwrap().replace(&lines[i], "")))
        .unwrap_or_default();
    Overlays { enabled, prefix: None, user: vec![], label: Some(b.label) }
}

fn indent_of(l: &str) -> String {
    l.chars().take_while(|c| c.is_whitespace()).collect()
}

fn edit_extlinux(text: &str, enabled: &[String]) -> Result<String, String> {
    let mut lines = split(text);
    let b = boot_block(&lines).ok_or("extlinux.conf has no label to add overlays to")?;
    if let Some(i) = key_line(&lines, &b, "fdtoverlays") {
        if enabled.is_empty() {
            lines.remove(i);
        } else {
            lines[i] = format!("{}fdtoverlays {}", indent_of(&lines[i]), enabled.join(" "));
        }
        return Ok(join(&lines, text));
    }
    if enabled.is_empty() {
        return Ok(text.to_string());
    }
    // After the fdt/fdtdir line, indented like it (the docs' example layout).
    let after = ["fdt", "fdtdir", "linux", "kernel"].iter().filter_map(|k| key_line(&lines, &b, k)).max();
    let (at, indent) = match after {
        Some(a) => (a, indent_of(&lines[a])),
        None => (b.start, "    ".into()),
    };
    lines.insert(at + 1, format!("{indent}fdtoverlays {}", enabled.join(" ")));
    Ok(join(&lines, text))
}

// ---------- uEnv.txt (AM335x BeagleBones) ----------

fn uenv_slots(lines: &[String]) -> Vec<(usize, u32, bool, String)> {
    // (line, slot, active, value)
    let re = Regex::new(r"^\s*(#\s*)?uboot_overlay_addr([0-7])\s*=\s*(.*?)\s*$").unwrap();
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            let m = re.captures(l)?;
            Some((i, m[2].parse().ok()?, m.get(1).is_none(), m[3].to_string()))
        })
        .collect()
}

fn read_uenv(text: &str) -> Overlays {
    let lines = split(text);
    Overlays { enabled: uenv_slots(&lines).into_iter().filter(|s| s.2 && !s.3.is_empty()).map(|s| s.3).collect(), ..Default::default() }
}

fn edit_uenv(text: &str, enabled: &[String]) -> Result<String, String> {
    let mut lines = split(text);
    let slots = uenv_slots(&lines);
    let active: Vec<&(usize, u32, bool, String)> = slots.iter().filter(|s| s.2 && !s.3.is_empty()).collect();
    for s in &active {
        if !enabled.contains(&s.3) {
            lines[s.0] = format!("#{}", lines[s.0].trim_start());
        }
    }
    let mut used: Vec<u32> = active.iter().filter(|s| enabled.contains(&s.3)).map(|s| s.1).collect();
    let mut last_line = slots.iter().map(|s| s.0).max();
    for name in enabled {
        if active.iter().any(|s| &s.3 == name) {
            continue;
        }
        let slot = [4, 5, 6, 7, 0, 1, 2, 3]
            .into_iter()
            .find(|n| !used.contains(n))
            .ok_or("uEnv.txt has 8 overlay slots and they are all taken")?;
        used.push(slot);
        let line = format!("uboot_overlay_addr{slot}={name}");
        match slots.iter().find(|s| s.1 == slot && !s.2) {
            Some(p) => lines[p.0] = line,
            None => {
                let at = match last_line {
                    Some(l) => l + 1,
                    None => {
                        let mut at = lines.len();
                        while at > 0 && lines[at - 1].trim().is_empty() {
                            at -= 1;
                        }
                        at
                    }
                };
                lines.insert(at, line);
                last_line = Some(at);
            }
        }
    }
    Ok(join(&lines, text))
}

/// Whether a uEnv.txt has U-Boot overlays switched on (`enable_uboot_overlays=1`).
pub fn uenv_overlays_on(text: &str) -> bool {
    let re = Regex::new(r"^\s*enable_uboot_overlays\s*=\s*1\s*$").unwrap();
    split(text).iter().any(|l| re.is_match(l))
}

// ---------- shared ----------

pub fn read_overlays(kind: EnvKind, text: &str) -> Overlays {
    match kind {
        EnvKind::Env => read_env(text),
        EnvKind::Extlinux => read_extlinux(text),
        EnvKind::UEnv => read_uenv(text),
    }
}

/// Turn overlays on/off; anything not mentioned keeps its place in the list.
pub fn edit_overlays(kind: EnvKind, text: &str, enable: &[String], disable: &[String]) -> Result<String, String> {
    let ok = Regex::new(r"^[\w.,/+-]+$").unwrap();
    if let Some(bad) = enable.iter().chain(disable).find(|n| !ok.is_match(n)) {
        return Err(format!("Not an overlay name: \"{bad}\""));
    }
    let cur = read_overlays(kind, text).enabled;
    let mut next: Vec<String> = cur.iter().filter(|n| !disable.contains(n)).cloned().collect();
    for n in enable {
        if !next.contains(n) {
            next.push(n.clone());
        }
    }
    if next.join(" ") == cur.join(" ") {
        return Ok(text.to_string());
    }
    match kind {
        EnvKind::Env => Ok(edit_env(text, &next)),
        EnvKind::Extlinux => edit_extlinux(text, &next),
        EnvKind::UEnv => edit_uenv(text, &next),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_txt::unified_diff;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // A stock Armbian armbianEnv.txt for a Rockchip board (the layout Armbian's build
    // writes: verbosity, bootlogo, console, overlay_prefix, fdtfile, rootdev, rootfstype,
    // usbstoragequirks).
    const ARMBIAN: &str = "verbosity=1
bootlogo=false
console=both
overlay_prefix=rockchip-rk3588
fdtfile=rockchip/rk3588-orangepi-5-plus.dtb
rootdev=UUID=6b3f1e2a-0c1d-4a55-9a41-3c2f8a1e7d10
rootfstype=ext4
usbstoragequirks=0x2537:0x1066:u,0x2537:0x1068:u
";

    // BeagleY-AI's extlinux.conf as shipped (docs.beagleboard.org), with a second label.
    const EXTLINUX: &str = "label microSD (default)
    kernel /Image
    append console=ttyS2,115200n8 root=/dev/mmcblk1p3 ro rootfstype=ext4 rootwait net.ifnames=0
    fdtdir /
    fdt /ti/k3-am67a-beagley-ai.dtb
    initrd /initrd.img

label recovery
    kernel /Image
    fdt /ti/k3-am67a-beagley-ai.dtb
    fdtoverlays /overlays/old.dtbo
";

    #[test]
    fn env_read_a_stock_file() {
        assert_eq!(
            read_overlays(EnvKind::Env, ARMBIAN),
            Overlays { enabled: vec![], prefix: Some("rockchip-rk3588".into()), user: vec![], label: None }
        );
    }

    #[test]
    fn env_enable_appends_one_line_and_round_trips() {
        let on = edit_overlays(EnvKind::Env, ARMBIAN, &s(&["i2c5-m3", "uart1-m1"]), &[]).unwrap();
        assert_eq!(on, ARMBIAN.replace("0x1068:u\n", "0x1068:u\noverlays=i2c5-m3 uart1-m1\n"));
        assert_eq!(read_overlays(EnvKind::Env, &on).enabled, s(&["i2c5-m3", "uart1-m1"]));
        let d = unified_diff(ARMBIAN, &on, "/boot/armbianEnv.txt", 3);
        let changed: Vec<&str> =
            d.lines().filter(|l| (l.starts_with('+') || l.starts_with('-')) && !l.starts_with("+++") && !l.starts_with("---")).collect();
        assert_eq!(changed, ["+overlays=i2c5-m3 uart1-m1"]);
        let off =
            edit_overlays(EnvKind::Env, &edit_overlays(EnvKind::Env, &on, &[], &s(&["uart1-m1"])).unwrap(), &[], &s(&["i2c5-m3"])).unwrap();
        assert_eq!(off, ARMBIAN);
    }

    #[test]
    fn env_last_overlays_wins_order_and_crlf_kept() {
        let t = "overlays=old\r\nfoo=1\r\noverlays=spi4-m0-cs1-spidev  pwm14-m0\r\nuser_overlays=my-sensor\r\n";
        let r = read_overlays(EnvKind::Env, t);
        assert_eq!((r.enabled, r.user), (s(&["spi4-m0-cs1-spidev", "pwm14-m0"]), s(&["my-sensor"])));
        let n = edit_overlays(EnvKind::Env, t, &s(&["i2c5-m3"]), &s(&["spi4-m0-cs1-spidev"])).unwrap();
        assert_eq!(n, "overlays=old\r\nfoo=1\r\noverlays=pwm14-m0 i2c5-m3\r\nuser_overlays=my-sensor\r\n");
    }

    #[test]
    fn env_no_change_returns_the_same_text_and_bad_names_are_refused() {
        assert_eq!(edit_overlays(EnvKind::Env, ARMBIAN, &[], &s(&["nothing"])).unwrap(), ARMBIAN);
        assert!(edit_overlays(EnvKind::Env, ARMBIAN, &s(&["a b"]), &[]).is_err());
        assert!(edit_overlays(EnvKind::Env, ARMBIAN, &s(&["x\nrootdev=/dev/sda1"]), &[]).is_err());
    }

    #[test]
    fn extlinux_reads_the_default_first_label_only() {
        assert_eq!(
            read_overlays(EnvKind::Extlinux, EXTLINUX),
            Overlays { enabled: vec![], prefix: None, user: vec![], label: Some("microSD (default)".into()) }
        );
    }

    #[test]
    fn extlinux_adds_fdtoverlays_after_fdt_and_removing_restores() {
        let p = "/overlays/k3-am67a-beagley-ai-spi0-2cs.dtbo".to_string();
        let on = edit_overlays(EnvKind::Extlinux, EXTLINUX, std::slice::from_ref(&p), &[]).unwrap();
        assert!(on.contains(&format!("    fdt /ti/k3-am67a-beagley-ai.dtb\n    fdtoverlays {p}\n    initrd")));
        assert_eq!(read_overlays(EnvKind::Extlinux, &on).enabled, vec![p.clone()]);
        // The recovery label's own fdtoverlays is untouched.
        assert!(on.contains("fdtoverlays /overlays/old.dtbo"));
        assert_eq!(edit_overlays(EnvKind::Extlinux, &on, &[], &[p]).unwrap(), EXTLINUX);
    }

    #[test]
    fn extlinux_a_default_line_picks_the_label() {
        let t = format!("default recovery\n{EXTLINUX}");
        let r = read_overlays(EnvKind::Extlinux, &t);
        assert_eq!((r.label.as_deref(), r.enabled), (Some("recovery"), s(&["/overlays/old.dtbo"])));
        assert!(
            edit_overlays(EnvKind::Extlinux, &t, &s(&["/overlays/b.dtbo"]), &[])
                .unwrap()
                .contains("fdtoverlays /overlays/old.dtbo /overlays/b.dtbo")
        );
    }

    // The overlay section of BeagleBoard.org's AM335x Debian uEnv.txt.
    const UENV: &str = "uname_r=5.10.168-ti-r71
#uuid=
#dtb=

###U-Boot Overlays###
###Documentation: http://elinux.org/Beagleboard:BeagleBoneBlack_Debian#U-Boot_Overlays
###Master Enable
enable_uboot_overlays=1
###
###Overide capes with eeprom
#uboot_overlay_addr0=<file0>.dtbo
#uboot_overlay_addr1=<file1>.dtbo
#uboot_overlay_addr2=<file2>.dtbo
#uboot_overlay_addr3=<file3>.dtbo
###
###Additional custom capes
#uboot_overlay_addr4=<file4>.dtbo
#uboot_overlay_addr5=<file5>.dtbo
#uboot_overlay_addr6=<file6>.dtbo
#uboot_overlay_addr7=<file7>.dtbo
###
cmdline=coherent_pool=1M net.ifnames=0 lpj=1990656 rng_core.default_quality=100 quiet
";

    #[test]
    fn uenv_takes_custom_cape_slots_first_and_comments_out_on_disable() {
        assert!(read_overlays(EnvKind::UEnv, UENV).enabled.is_empty());
        assert!(uenv_overlays_on(UENV));
        let a = "/lib/firmware/BB-UART1-00A0.dtbo".to_string();
        let b = "/lib/firmware/BB-I2C2-00A0.dtbo".to_string();
        let on = edit_overlays(EnvKind::UEnv, UENV, &[a.clone(), b.clone()], &[]).unwrap();
        assert!(on.contains(&format!("\nuboot_overlay_addr4={a}\nuboot_overlay_addr5={b}\n#uboot_overlay_addr6=")));
        assert_eq!(read_overlays(EnvKind::UEnv, &on).enabled, vec![a.clone(), b.clone()]);
        let off = edit_overlays(EnvKind::UEnv, &on, &[], std::slice::from_ref(&a)).unwrap();
        assert!(off.contains(&format!("\n#uboot_overlay_addr4={a}\n")));
        assert_eq!(read_overlays(EnvKind::UEnv, &off).enabled, vec![b.clone()]);
        // Nine overlays don't fit in eight slots.
        let many: Vec<String> = (0..9).map(|i| format!("/lib/firmware/x{i}.dtbo")).collect();
        assert!(edit_overlays(EnvKind::UEnv, UENV, &many, &[]).is_err());
    }
}
