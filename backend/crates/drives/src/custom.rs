//! Flash customisation (server.ts "Flash Customization"): how the dashboard's settings
//! (hostname, user, SSH, Wi-Fi, locale, Install BenchBob) reach a freshly written image.
//!
//! It depends on the image: the catalog's `init_format` says which mechanism the OS reads
//! on first boot, and writing the wrong one is silently ignored (the Pi then boots
//! unconfigured, without SSH). Raspberry Pi OS Trixie is `cloudinit-rpi` and ignores
//! firstrun.sh; Bookworm/Legacy is `systemd`. The generators follow Raspberry Pi Imager
//! 2.x (src/customization_generator.cpp, src/downloadthread.cpp), so a BenchBob flash boots
//! the same as an Imager one. Settings are as the dashboard sends them (any JSON); every
//! read follows JavaScript's truthiness and `String()` the way server.ts does.

use benchbob_core::http::{js_number, js_string, truthy};
use regex::Regex;
use serde_json::Value;

use crate::devices::part_dev;
use crate::firstboot;
use crate::sys::Sys;
use crate::util::{js_num_str, shell_esc};

pub const BOOT_MOUNT: &str = "/tmp/benchbob-boot";

fn get<'a>(s: &'a Value, k: &str) -> Option<&'a Value> {
    s.get(k)
}
fn t(s: &Value, k: &str) -> bool {
    get(s, k).is_some_and(truthy)
}
/// `String(s[k] || "")`
fn str_or_empty(s: &Value, k: &str) -> String {
    get(s, k).filter(|v| truthy(v)).map(js_string).unwrap_or_default()
}
/// `s[k] || dflt` as a string.
fn str_or(s: &Value, k: &str, dflt: &str) -> String {
    get(s, k).filter(|v| truthy(v)).map(js_string).unwrap_or_else(|| dflt.into())
}

/// A JSON string literal is a valid YAML double-quoted scalar, escapes included.
pub fn yq(v: &str) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

fn strip_crlf(s: &str) -> String {
    s.replace(['\r', '\n'], "")
}

/// `settingsKeys`: the SSH keys, one per line or as a list, trimmed, blanks dropped.
pub fn keys(s: &Value) -> Vec<String> {
    let raw: Vec<String> = match get(s, "sshKeys") {
        Some(Value::Array(a)) => a.iter().map(js_string).collect(),
        Some(v) if truthy(v) => Regex::new(r"\r?\n").unwrap().split(&js_string(v)).map(String::from).collect(),
        _ => vec![],
    };
    raw.iter().map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect()
}

#[derive(Debug, Clone)]
pub struct Wifi {
    pub ssid: String,
    pub password: String,
    pub country: String,
    pub priority: f64,
}

/// `settingsWifi`: the networks (the old single-network fields still work).
pub fn wifi(s: &Value) -> Vec<Wifi> {
    let mut nets: Vec<Value> = get(s, "wifiNetworks").and_then(Value::as_array).cloned().unwrap_or_default();
    if nets.is_empty() && t(s, "configureWifi") && t(s, "wifiSsid") {
        nets.push(serde_json::json!({
            "ssid": get(s, "wifiSsid").cloned().unwrap_or(Value::Null),
            "password": get(s, "wifiPassword").filter(|v| truthy(v)).cloned().unwrap_or("".into()),
            "country": get(s, "wifiCountry").filter(|v| truthy(v)).cloned().unwrap_or("US".into()),
            "priority": 0,
        }));
    }
    nets.iter()
        .filter(|w| truthy(w) && t(w, "ssid"))
        .map(|w| {
            let p = get(w, "priority").map(js_number).unwrap_or(f64::NAN);
            Wifi {
                ssid: js_string(&w["ssid"]),
                password: strip_crlf(&str_or_empty(w, "password")),
                country: str_or(w, "country", "US"),
                priority: if p.is_nan() || p == 0.0 { 0.0 } else { p },
            }
        })
        .collect()
}

/// cmdline.txt is one line of kernel parameters, so a Wi-Fi country lands there only if
/// it is exactly two letters (anything else could inject parameters).
pub fn country_code(cc: &str) -> String {
    let t = cc.trim().to_uppercase();
    if t.len() == 2 && t.bytes().all(|c| c.is_ascii_uppercase()) { t } else { String::new() }
}

/// The password hash, from `openssl passwd -6` (fed on stdin, so it never shows in `ps`).
/// Pasted passwords can carry a newline PAM never sends: CR/LF are stripped, as Imager does.
pub async fn hash_password(sys: &dyn Sys, pw: &str) -> anyhow::Result<String> {
    let input = strip_crlf(pw) + "\n";
    let r = sys.run_input(&["openssl", "passwd", "-6", "-stdin"], Some(input.as_bytes())).await;
    let hash = r.out.trim().to_string();
    if r.code != 0 || !hash.starts_with('$') {
        anyhow::bail!("could not hash the password");
    }
    Ok(hash)
}

/// The archive key for "Install BenchBob"; tests inject theirs.
pub type KeyFn = fn() -> anyhow::Result<String>;

/// cloud-init `user-data` (after the `#cloud-config` line).
pub async fn user_data(sys: &dyn Sys, s: &Value, key: KeyFn) -> anyhow::Result<String> {
    let mut l: Vec<String> = vec!["manage_resolv_conf: false".into(), String::new()];
    let hostname = str_or_empty(s, "hostname").trim().to_string();
    if !hostname.is_empty() {
        l.push(format!("hostname: {}", yq(&hostname)));
        l.push("manage_etc_hosts: true".into());
    }
    // Parity with Imager: avahi for <hostname>.local. And for any packages, don't let a
    // first-boot clock that hasn't synced yet make apt reject the repo signatures.
    let mut packages: Vec<&str> = Vec::new();
    if !hostname.is_empty() {
        packages.push("avahi-daemon");
    }
    if t(s, "installBenchBob") {
        packages.push("benchbob");
        l.extend(firstboot::cloud_init_write_files(&key()?));
    }
    if !packages.is_empty() {
        l.push("packages:".into());
        l.extend(packages.iter().map(|p| format!("- {p}")));
        for x in ["", "apt:", "  preserve_sources_list: true", "  conf: |", "    Acquire {", "      Check-Date \"false\";", "    };", ""] {
            l.push(x.into());
        }
    }
    if t(s, "timezone") {
        l.push(format!("timezone: {}", yq(js_string(&s["timezone"]).trim())));
    }
    if t(s, "keyboardLayout") {
        l.push("keyboard:".into());
        l.push("  model: pc105".into());
        l.push(format!("  layout: {}", yq(js_string(&s["keyboardLayout"]).trim())));
    }
    let username = {
        let u = str_or_empty(s, "username").trim().to_string();
        if u.is_empty() { "pi".to_string() } else { u }
    };
    let ks = if t(s, "enableSsh") { keys(s) } else { vec![] };
    let passwordless = t(s, "passwordlessSudo");
    // With neither a password nor a key the account would be unusable: leave the image's
    // default user alone.
    if t(s, "password") || !ks.is_empty() {
        // Singular `user:` renames/extends the distro's default user, keeping its groups
        // (sudo, gpio, video…); the plural `users:` list would make a group-less account.
        l.extend(["".into(), "user:".into(), format!("  name: {}", yq(&username)), "  shell: /bin/bash".into()]);
        if t(s, "password") {
            l.push("  lock_passwd: false".into());
            l.push(format!("  passwd: {}", yq(&hash_password(sys, &js_string(&s["password"])).await?)));
        } else {
            l.push("  lock_passwd: true".into());
        }
        if !ks.is_empty() {
            l.push("  ssh_authorized_keys:".into());
            for k in &ks {
                l.push(format!("    - {}", yq(k)));
            }
        }
        // Must be explicit: the default user in /etc/cloud/cloud.cfg carries NOPASSWD
        // sudo, and `user:` is merged over it. `null` drops that rule but keeps the group.
        l.push(if passwordless { "  sudo: ALL=(ALL) NOPASSWD:ALL" } else { "  sudo: null" }.into());
        l.push(String::new());
    }
    let mut runcmd: Vec<String> = Vec::new();
    if t(s, "enableSsh") {
        l.push(format!("ssh_pwauth: {}", if t(s, "password") { "true" } else { "false" }));
        // cc_ssh writes host keys and authorized_keys but never enables sshd, and
        // Raspberry Pi OS ships it disabled.
        runcmd.push("  - [ systemctl, enable, --now, ssh ]".into());
    }
    if passwordless {
        // `sudo:` above isn't honoured by every cloud-init build; Imager writes the file too.
        let file = format!("/etc/sudoers.d/010_{username}-nopasswd");
        let sh = format!(
            "echo '{}' >'{}' && chmod 0440 '{}'",
            shell_esc(&format!("{username} ALL=(ALL) NOPASSWD:ALL")),
            shell_esc(&file),
            shell_esc(&file)
        );
        runcmd.push(format!("  - [ sh, -c, {} ]", yq(&sh)));
    }
    if !runcmd.is_empty() {
        l.push(String::new());
        l.push("runcmd:".into());
        l.extend(runcmd);
    }
    Ok(l.join("\n") + "\n")
}

/// cloud-init `network-config` (netplan v2); "" without Wi-Fi.
pub fn network_config(s: &Value) -> String {
    let mut nets = wifi(s);
    if nets.is_empty() {
        return String::new();
    }
    // Highest priority first (stable): netplan has no per-AP priority, but order is a hint.
    nets.sort_by(|a, b| b.priority.partial_cmp(&a.priority).unwrap_or(std::cmp::Ordering::Equal));
    let cc = country_code(&nets[0].country);
    // eth0 too: once a network-config is supplied it replaces the distro default, and
    // wired Ethernet would otherwise stop coming up.
    let mut l: Vec<String> = [
        "network:",
        "  version: 2",
        "  ethernets:",
        "    eth0:",
        "      dhcp4: true",
        "      dhcp6: true",
        "      optional: true",
        "  wifis:",
        "    wlan0:",
        "      dhcp4: true",
    ]
    .iter()
    .map(|x| x.to_string())
    .collect();
    if !cc.is_empty() {
        l.push(format!("      regulatory-domain: {}", yq(&cc)));
    }
    l.push("      access-points:".into());
    for w in &nets {
        l.push(format!("        {}:", yq(&w.ssid)));
        if !w.password.is_empty() {
            l.push(format!("          password: {}", yq(&w.password)));
        } else {
            l.push("          auth:".into());
            l.push("            key-management: none".into());
        }
    }
    l.push("      optional: true".into());
    l.join("\n") + "\n"
}

const CUSTOM: &str = "/usr/lib/raspberrypi-sys-mods/imager_custom";

/// firstrun.sh for `systemd` (Bookworm-era) images.
pub fn firstrun(s: &Value, key: KeyFn) -> anyhow::Result<String> {
    let hn = shell_esc(&str_or(s, "hostname", "raspberrypi"));
    let mut sc = String::from("#!/bin/bash\nset +e\nCURRENT_HOSTNAME=$(cat /etc/hostname | tr -d \" \\t\\n\\r\")\n");
    sc += &format!(
        "\nif [ -f {CUSTOM} ]; then\n  {CUSTOM} set_hostname '{hn}'\nelse\n  echo '{hn}' >/etc/hostname\n  sed -i \"s/127.0.1.1.*$CURRENT_HOSTNAME/127.0.1.1\\t{hn}/g\" /etc/hosts\nfi\n"
    );
    if t(s, "enableSsh") {
        sc += &format!("\nif [ -f {CUSTOM} ]; then\n  {CUSTOM} enable_ssh\nelse\n  systemctl enable ssh\nfi\n");
        if !t(s, "password") {
            sc += "\nsed -i 's/^#*PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config\n";
        }
    }
    let ks = keys(s);
    let user = shell_esc(&str_or(s, "username", "pi"));
    if !ks.is_empty() {
        sc += &format!(
            "\nFIRSTUSERHOME=$(getent passwd '{user}' | cut -d: -f6)\nif [ -z \"$FIRSTUSERHOME\" ]; then FIRSTUSERHOME=\"/home/{user}\"; fi\nmkdir -p \"$FIRSTUSERHOME/.ssh\"\nchmod 700 \"$FIRSTUSERHOME/.ssh\"\ncat >> \"$FIRSTUSERHOME/.ssh/authorized_keys\" <<'SSHEOF'\n"
        );
        for k in &ks {
            sc += &format!("{k}\n");
        }
        sc += &format!("SSHEOF\nchmod 600 \"$FIRSTUSERHOME/.ssh/authorized_keys\"\nchown -R '{user}:{user}' \"$FIRSTUSERHOME/.ssh\"\n");
    }
    if t(s, "passwordlessSudo") {
        sc += &format!(
            "\necho '{user} ALL=(ALL) NOPASSWD:ALL' >'/etc/sudoers.d/010_{user}-nopasswd'\nchmod 0440 '/etc/sudoers.d/010_{user}-nopasswd'\n"
        );
    }
    for (i, w) in wifi(s).iter().enumerate() {
        let (ssid, psk, cc) = (shell_esc(&w.ssid), shell_esc(&w.password), shell_esc(&w.country));
        let conn = format!("wifi-{i}");
        sc += &format!(
            "\nif [ -f {CUSTOM} ]; then\n  {CUSTOM} set_wlan '{ssid}' '{psk}' '{cc}'\nelse\n  cat > '/etc/NetworkManager/system-connections/{conn}.nmconnection' <<'NMEOF'\n[connection]\nid={}\ntype=wifi\nautoconnect-priority={}\n[wifi]\nmode=infrastructure\nssid={}\n[wifi-security]\nauth-alg=open\nkey-mgmt=wpa-psk\npsk={}\n[ipv4]\nmethod=auto\n[ipv6]\nmethod=auto\nNMEOF\n  chmod 600 '/etc/NetworkManager/system-connections/{conn}.nmconnection'\nfi\n",
            w.ssid,
            js_num_str(w.priority),
            w.ssid,
            w.password
        );
    }
    if t(s, "timezone") {
        let tz = shell_esc(&js_string(&s["timezone"]));
        sc += &format!(
            "\nif [ -f {CUSTOM} ]; then\n  {CUSTOM} set_timezone '{tz}'\nelse\n  rm -f /etc/localtime\n  echo '{tz}' >/etc/timezone\n  dpkg-reconfigure -f noninteractive tzdata\nfi\n"
        );
    }
    if t(s, "keyboardLayout") {
        let kb = shell_esc(&js_string(&s["keyboardLayout"]));
        sc += &format!("\nif [ -f {CUSTOM} ]; then\n  {CUSTOM} set_keymap '{kb}' '{kb}'\nfi\n");
    }
    if t(s, "installBenchBob") {
        sc += &firstboot::firstrun_install(&key()?);
    }
    sc += "\nrm -f /boot/firmware/firstrun.sh\nsed -i 's| systemd.run.*||g' /boot/firmware/cmdline.txt\nexit 0\n";
    Ok(sc)
}

/// Write a root-owned file without the content touching a world-readable temp file
/// (these carry password hashes and Wi-Fi keys).
pub async fn sudo_write(sys: &dyn Sys, path: &str, content: &str) -> anyhow::Result<()> {
    let r = sys.run_input(&["sudo", "tee", path], Some(content.as_bytes())).await;
    if r.code != 0 {
        let e = r.err.trim();
        anyhow::bail!(
            "writing {}: {}",
            path.rsplit('/').next().unwrap_or(path),
            if e.is_empty() { format!("exit {}", r.code) } else { e.to_string() }
        );
    }
    Ok(())
}

async fn append_cmdline(sys: &dyn Sys, params: &str) -> anyhow::Result<()> {
    let r = sys.run(&["sudo", "cat", &format!("{BOOT_MOUNT}/cmdline.txt")]).await;
    if r.code != 0 || r.out.is_empty() {
        anyhow::bail!("could not read cmdline.txt");
    }
    sudo_write(sys, &format!("{BOOT_MOUNT}/cmdline.txt"), &format!("{}{params}\n", r.out.trim())).await
}

/// Only for images not in the catalog: cloud-init images ship NoCloud seed files on
/// bootfs; anything else with a cmdline.txt is taken for a firstrun.sh-era Pi OS.
async fn detect_format(sys: &dyn Sys) -> &'static str {
    if sys.exists(&format!("{BOOT_MOUNT}/meta-data")).await || sys.exists(&format!("{BOOT_MOUNT}/user-data")).await {
        return "cloudinit";
    }
    if sys.exists(&format!("{BOOT_MOUNT}/cmdline.txt")).await {
        return "systemd";
    }
    "none"
}

async fn apply_cloud_init(sys: &dyn Sys, s: &Value, key: KeyFn) -> anyhow::Result<String> {
    let user = user_data(sys, s, key).await?;
    let net = network_config(s);
    // A fresh instance-id per write makes cloud-init treat this as a new machine and run
    // first-boot setup; the stub meta-data in the image has a fixed one.
    let id = format!("benchbob-{}", sys.now_ms());
    sudo_write(sys, &format!("{BOOT_MOUNT}/meta-data"), &format!("instance-id: {id}\n")).await?;
    sudo_write(sys, &format!("{BOOT_MOUNT}/user-data"), &format!("#cloud-config\n{user}")).await?;
    let mut written = vec!["user-data", "meta-data"];
    if !net.is_empty() {
        sudo_write(sys, &format!("{BOOT_MOUNT}/network-config"), &net).await?;
        written.push("network-config");
    }
    // ds= lets cloud-init validate its cache from the cmdline instead of rediscovering the
    // datasource on every boot.
    let mut params = format!(" ds=nocloud;i={id}");
    let cc = country_code(wifi(s).first().map(|w| w.country.as_str()).unwrap_or(""));
    if !cc.is_empty() {
        params += &format!(" cfg80211.ieee80211_regdom={cc}");
    }
    append_cmdline(sys, &params).await?;
    written.push("cmdline.txt");
    Ok(format!("Settings applied via cloud-init ({}).", written.join(", ")))
}

async fn apply_firstrun(sys: &dyn Sys, s: &Value, key: KeyFn) -> anyhow::Result<String> {
    if t(s, "enableSsh") {
        sudo_write(sys, &format!("{BOOT_MOUNT}/ssh"), "").await?;
    }
    if t(s, "username") {
        // No password: a locked password (SSH key only).
        let hash = if t(s, "password") { hash_password(sys, &js_string(&s["password"])).await? } else { "*".into() };
        sudo_write(sys, &format!("{BOOT_MOUNT}/userconf.txt"), &format!("{}:{hash}\n", js_string(&s["username"]))).await?;
    }
    sudo_write(sys, &format!("{BOOT_MOUNT}/firstrun.sh"), &firstrun(s, key)?).await?;
    let cmdline = sys.run(&["sudo", "cat", &format!("{BOOT_MOUNT}/cmdline.txt")]).await.out;
    if !cmdline.contains("firstrun.sh") {
        // Must stay last on the line: firstrun.sh deletes from " systemd.run" to the end.
        append_cmdline(
            sys,
            " systemd.run=/boot/firmware/firstrun.sh systemd.run_success_action=reboot systemd.unit=kernel-command-line.target",
        )
        .await?;
    }
    Ok("Settings applied via firstrun.sh.".into())
}

/// `applySettings`: mount the boot partition, write what the image's first boot reads.
pub async fn apply(sys: &dyn Sys, dev: &str, s: &Value, init_format: Option<&str>, key: KeyFn) -> anyhow::Result<String> {
    sys.run(&["sudo", "mkdir", "-p", BOOT_MOUNT]).await;
    let boot = part_dev(dev, 1);
    let m = sys.run(&["sudo", "mount", &boot, BOOT_MOUNT]).await;
    if m.code != 0 {
        anyhow::bail!("could not mount boot partition {boot}: {}", if m.err.is_empty() { format!("exit {}", m.code) } else { m.err });
    }
    let res = async {
        let fmt = match init_format {
            Some(f) => f,
            None => detect_format(sys).await,
        };
        match fmt {
            "cloudinit" | "cloudinit-rpi" => apply_cloud_init(sys, s, key).await,
            "systemd" => apply_firstrun(sys, s, key).await,
            "rpi-preseed" => anyhow::bail!("this image expects rpi-preseed customization, which BenchBob does not support yet"),
            _ => Ok("This image does not support customization; settings were not applied.".into()),
        }
    }
    .await;
    sys.run(&["sudo", "umount", BOOT_MOUNT]).await;
    res
}

#[cfg(test)]
pub mod tests {
    use serde_json::json;

    use super::*;
    use crate::sys::mock::{Mock, Shared};

    pub fn key() -> anyhow::Result<String> {
        Ok(include_str!("../../../../packaging/benchbob-archive-keyring.asc").replace("\r\n", "\n"))
    }

    /// Every generator, byte for byte against server.ts's on the same settings (the
    /// password hash comes from the same scripted openssl).
    #[tokio::test]
    async fn generators_match_server_ts() {
        let settings: Value = serde_json::from_str(include_str!("../fixtures/settings.json")).unwrap();
        let want: Value = serde_json::from_str(include_str!("../fixtures/settings.expected.json")).unwrap();
        for (name, s) in settings.as_object().unwrap() {
            let m = Mock::new(&json!({ "answers": [{ "match": "openssl passwd -6 -stdin", "out": "$6$fixture$0123456789abcdef\n" }] }));
            let sys = Shared(m.clone());
            let w = &want[name];
            assert_eq!(user_data(&sys, s, key).await.unwrap(), w["userData"].as_str().unwrap(), "user-data for {name}");
            assert_eq!(network_config(s), w["networkConfig"].as_str().unwrap(), "network-config for {name}");
            assert_eq!(firstrun(s, key).unwrap(), w["firstrun"].as_str().unwrap(), "firstrun.sh for {name}");
            assert_eq!(Value::Array(m.entries()), w["log"], "commands for {name}");
        }
    }

    #[test]
    fn country_codes() {
        assert_eq!(country_code(" us "), "US");
        assert_eq!(country_code("GBR"), "");
        assert_eq!(country_code("U1"), "");
    }
}
