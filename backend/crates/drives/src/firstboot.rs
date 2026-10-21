//! "Install BenchBob on first boot" (update.ts `cloudInitWriteFiles` / `firstrunInstall`):
//! what a card we flash needs to install BenchBob itself.

use benchbob_core::paths::app_root;

// "Install BenchBob" in the flash settings: the new system gets our apt source and key,
// and installs the package on its first boot with a network. cloud-init images (Pi OS
// Trixie) do that with write_files + packages; firstrun.sh images (Bookworm) run before
// the network is up, so they get a oneshot unit that waits for it.

pub const APT_URI: &str = "https://cinderblock.github.io/BenchBob/apt";
const KEYRING: &str = "/usr/share/keyrings/benchbob-archive-keyring.asc";
// Not benchbob.sources: that's the package's conffile, and a file already there makes dpkg
// stop and ask which to keep, with nobody to answer on a first boot. postinst deletes this
// one once the package's own source is in place.
const BOOTSTRAP_SOURCES: &str = "/etc/apt/sources.list.d/benchbob-bootstrap.sources";

fn apt_sources() -> String {
    format!("Types: deb\nURIs: {APT_URI}\nSuites: stable\nComponents: main\nSigned-By: {KEYRING}\n")
}

/// Our signing key: installed by the package, or in packaging/ when run from a checkout.
pub fn archive_key() -> anyhow::Result<String> {
    for p in [std::path::PathBuf::from(KEYRING), app_root().join("packaging/benchbob-archive-keyring.asc")] {
        if let Ok(t) = std::fs::read_to_string(&p) {
            return Ok(t);
        }
    }
    anyhow::bail!("BenchBob's archive key isn't installed, so a flashed card can't be set up to install BenchBob")
}

/// cloud-config lines: write_files for the apt source and key. Add "benchbob" to packages.
pub fn cloud_init_write_files(key: &str) -> Vec<String> {
    let block = |t: &str| t.trim_end().split('\n').map(|l| format!("    {l}")).collect::<Vec<_>>();
    let mut v = vec!["write_files:".to_string(), format!("- path: {KEYRING}"), "  permissions: '0644'".into(), "  content: |".into()];
    v.extend(block(key));
    v.extend([format!("- path: {BOOTSTRAP_SOURCES}"), "  permissions: '0644'".into(), "  content: |".into()]);
    v.extend(block(&apt_sources()));
    v.push(String::new());
    v
}

/// firstrun.sh fragment: the source, the key, and a unit that installs once online.
pub fn firstrun_install(key: &str) -> String {
    let unit = "[Unit]
Description=Install BenchBob (first boot)
Wants=network-online.target
After=network-online.target
ConditionPathExists=!/usr/lib/benchbob/VERSION

[Service]
Type=oneshot
# Check-Date off: the clock may not have synced yet, and apt would call the repo \"not valid yet\".
ExecStart=/bin/sh -c 'apt-get -o Acquire::Check-Date=false update && DEBIAN_FRONTEND=noninteractive apt-get install -y -o Dpkg::Options::=--force-confold benchbob'
ExecStartPost=/bin/systemctl disable benchbob-firstboot.service
Restart=on-failure
RestartSec=60

[Install]
WantedBy=multi-user.target
";
    let file = |path: &str, text: &str, tag: &str| format!("cat > '{path}' << '{tag}'\n{}\n{tag}\n", text.trim_end());
    format!(
        "\n# BenchBob: our apt source and key, installed from it once the network is up\nmkdir -p /usr/share/keyrings /etc/apt/sources.list.d\n{}{}{}systemctl enable benchbob-firstboot.service\n",
        file(KEYRING, key, "BBKEYEOF"),
        file(BOOTSTRAP_SOURCES, &apt_sources(), "BBSRCEOF"),
        file("/etc/systemd/system/benchbob-firstboot.service", unit, "BBUNITEOF"),
    )
}

#[cfg(test)]
mod tests {
    use regex::Regex;

    use super::*;

    const KEY: &str = include_str!("../../../../packaging/benchbob-archive-keyring.asc");

    /// The `content: |` blocks of a write_files list, by path (enough YAML for this shape).
    fn write_files(lines: &[String]) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        let mut path: Option<String> = None;
        let mut content: Option<Vec<String>> = None;
        let flush = |p: &mut Option<String>, c: &mut Option<Vec<String>>, out: &mut std::collections::HashMap<String, String>| {
            if let (Some(p), Some(c)) = (p.take(), c.take()) {
                out.insert(p, c.join("\n") + "\n");
            }
        };
        for l in lines {
            if let Some(p) = l.strip_prefix("- path: ") {
                flush(&mut path, &mut content, &mut out);
                path = Some(p.into());
            } else if l == "  content: |" {
                content = Some(vec![]);
            } else if let (Some(c), Some(t)) = (content.as_mut(), l.strip_prefix("    ")) {
                c.push(t.into());
            }
        }
        flush(&mut path, &mut content, &mut out);
        out
    }

    #[test]
    fn cloud_init_write_files_carry_the_apt_source_and_the_exact_key() {
        let files = write_files(&cloud_init_write_files(KEY));
        assert_eq!(files["/usr/share/keyrings/benchbob-archive-keyring.asc"], KEY.trim_end().to_string() + "\n");
        // Not benchbob.sources, the package's conffile: dpkg would stop to ask about it.
        assert!(!files.contains_key("/etc/apt/sources.list.d/benchbob.sources"));
        let src = &files["/etc/apt/sources.list.d/benchbob-bootstrap.sources"];
        assert!(src.contains("URIs: https://cinderblock.github.io/BenchBob/apt"));
        assert!(src.contains("Signed-By: /usr/share/keyrings/benchbob-archive-keyring.asc"));
    }

    #[test]
    fn firstrun_fragment_writes_the_key_verbatim_and_enables_the_install_unit() {
        let sh = firstrun_install(KEY);
        let re = Regex::new(r"cat > '/usr/share/keyrings/benchbob-archive-keyring\.asc' << 'BBKEYEOF'\n([\s\S]*?)\nBBKEYEOF\n").unwrap();
        assert_eq!(&re.captures(&sh).unwrap()[1], KEY.trim_end());
        assert!(sh.contains("systemctl enable benchbob-firstboot.service"));
        assert!(sh.contains("ConditionPathExists=!/usr/lib/benchbob/VERSION"));
    }
}
