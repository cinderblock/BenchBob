//! Release updates (update.ts): notice a newer BenchBob on GitHub, and install it through
//! apt. Also what a flashed card needs to install BenchBob itself.
//!
//! The check asks GitHub's API for the latest release every 6 hours (and on demand); a
//! checkout ("dev") never checks. It shells out to curl (a Depends of the package), which
//! keeps a TLS stack out of the binary. Installing hands off to benchbob-update, a root
//! script started with systemd-run as its own unit: the upgrade's postinst restarts this
//! service, which would kill an apt run that lived in our cgroup halfway through dpkg.

use std::sync::Mutex;
use std::time::Duration;

use benchbob_core::http::{HttpError, json_response};
use benchbob_core::paths::{app_root, state_path, version};
use benchbob_core::run::{Opts, run};
use benchbob_core::timeline::now_ms;
use regex::Regex;
use serde_json::{Value, json};

const REPO: &str = "cinderblock/BenchBob";
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const LOG: &str = "/var/log/benchbob-update.log";
const UNIT: &str = "benchbob-update";

/// Compare Debian-style versions as this project makes them: x.y.z, optionally
/// `~git<n>.<sha>` (before x.y.z) or `+git<n>.<sha>` (after it). Negative if a < b; 0 when
/// either doesn't parse.
pub fn compare_versions(a: &str, b: &str) -> i64 {
    let re = Regex::new(r"^v?(\d+)\.(\d+)\.(\d+)(?:([~+])git(\d+))?").unwrap();
    let parse = |v: &str| -> Option<[i64; 5]> {
        let m = re.captures(v.trim())?;
        let n = |i: usize| m.get(i).and_then(|x| x.as_str().parse::<i64>().ok()).unwrap_or(0);
        // ~ sorts before the release, + after it; within either, by commit count.
        let rank = match m.get(4).map(|x| x.as_str()) {
            Some("~") => -1,
            Some("+") => 1,
            _ => 0,
        };
        Some([n(1), n(2), n(3), rank, n(5)])
    };
    let (Some(pa), Some(pb)) = (parse(a), parse(b)) else { return 0 };
    pa.iter().zip(pb.iter()).find(|(x, y)| x != y).map(|(x, y)| x - y).unwrap_or(0)
}

#[derive(Clone)]
struct State {
    check: bool,
    latest: Option<String>,
    url: Option<String>,
    notes: String,
    checked_at: u64,
    error: String,
}

pub struct Update {
    state: Mutex<State>,
}

impl Default for Update {
    fn default() -> Self {
        Self {
            state: Mutex::new(State { check: true, latest: None, url: None, notes: String::new(), checked_at: 0, error: String::new() }),
        }
    }
}

const DEV_MSG: &str = "Running from a checkout: update with git, not the package";

impl Update {
    fn load_settings(&self) {
        let p = state_path("update.json");
        if let Some(c) =
            std::fs::read(&p).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| v.get("check")?.as_bool())
        {
            self.state.lock().unwrap().check = c;
        }
    }

    pub async fn check(&self) {
        if version() == "dev" {
            return;
        }
        let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
        let ua = format!("User-Agent: BenchBob/{}", version());
        let r = run(
            &["curl", "-sS", "--max-time", "15", "-H", "Accept: application/vnd.github+json", "-H", &ua, "-w", "\n%{http_code}", &url],
            Opts::timeout_ms(20_000),
        )
        .await;
        let result: Result<Value, String> = (|| {
            if r.code != 0 {
                return Err(if r.err.is_empty() { format!("curl exit {}", r.code) } else { r.err.clone() });
            }
            let (body, code) = r.out.rsplit_once('\n').unwrap_or(("", r.out.as_str()));
            if !code.trim().starts_with('2') {
                return Err(format!("GitHub answered {}", code.trim()));
            }
            serde_json::from_str(body).map_err(|e| e.to_string())
        })();
        let mut s = self.state.lock().unwrap();
        match result {
            Ok(j) => {
                // String(j.tag_name || "").replace(/^v/, "") || null
                let tag =
                    j.get("tag_name").filter(|t| benchbob_core::http::truthy(t)).map(benchbob_core::http::js_string).unwrap_or_default();
                s.latest = Some(tag.strip_prefix('v').unwrap_or(&tag).to_string()).filter(|t| !t.is_empty());
                s.url = j.get("html_url").and_then(Value::as_str).filter(|u| !u.is_empty()).map(String::from);
                let notes = j.get("body").and_then(Value::as_str).unwrap_or("");
                // String(j.body || "").slice(0, 4000): UTF-16 units, close enough in chars.
                s.notes = notes.chars().take(4000).collect();
                s.error.clear();
            }
            Err(e) => s.error = e,
        }
        s.checked_at = now_ms();
    }

    pub fn start(self: &std::sync::Arc<Self>) {
        self.load_settings();
        if version() == "dev" {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            // Not at startup itself: a boot loop shouldn't hammer the API.
            tokio::time::sleep(Duration::from_secs(60)).await;
            loop {
                if me.state.lock().unwrap().check {
                    me.check().await;
                }
                tokio::time::sleep(CHECK_EVERY).await;
            }
        });
    }

    /// For /api/status: small enough to send every 2 s.
    pub fn summary(&self) -> Value {
        let s = self.state.lock().unwrap().clone();
        let v = version();
        let available = s.latest.as_ref().is_some_and(|l| v != "dev" && compare_versions(l, v) > 0);
        json!({ "current": v, "latest": s.latest, "available": available, "url": s.url, "checkedAt": s.checked_at, "error": s.error, "check": s.check })
    }

    async fn installing() -> bool {
        run(&["systemctl", "is-active", "--quiet", &format!("{UNIT}.service")], Opts::default()).await.code == 0
    }

    fn log_tail() -> String {
        std::fs::read_to_string(LOG)
            .map(|t| t.split('\n').rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    }

    /// Routes under /api/update; None when the path isn't ours.
    pub async fn route(&self, method: &axum::http::Method, path: &str, body: &[u8]) -> Option<axum::response::Response> {
        use axum::http::Method;
        let ok = |v: Value| Some(json_response(200, &v));
        let err = |status: u16, m: &str| Some(json_response(status, &json!({ "ok": false, "message": m })));
        let parsed = || serde_json::from_slice::<Value>(body).unwrap_or(json!({}));
        match (method, path) {
            (&Method::GET, "/api/update") => {
                let mut v = self.summary();
                v["notes"] = self.state.lock().unwrap().notes.clone().into();
                v["installing"] = Self::installing().await.into();
                v["log"] = Self::log_tail().into();
                ok(v)
            }
            (&Method::POST, "/api/update/check") => {
                if version() == "dev" {
                    return err(409, DEV_MSG);
                }
                self.check().await;
                let mut v = json!({ "ok": true });
                for (k, x) in self.summary().as_object().unwrap() {
                    v[k] = x.clone();
                }
                ok(v)
            }
            (&Method::PUT, "/api/update/settings") => {
                let Some(c) = parsed().get("check").and_then(Value::as_bool) else { return err(400, "body: { check: true|false }") };
                self.state.lock().unwrap().check = c;
                let text = serde_json::to_string_pretty(&json!({ "check": c })).unwrap();
                if let Err(e) = std::fs::write(state_path("update.json"), text) {
                    return Some(HttpError::new(500, e.to_string()).into_response());
                }
                ok(json!({ "ok": true, "check": c }))
            }
            (&Method::POST, "/api/update/install") => {
                if parsed().get("confirm") != Some(&Value::Bool(true)) {
                    return err(400, "Installing restarts BenchBob: send { confirm: true }");
                }
                if version() == "dev" {
                    return err(409, DEV_MSG);
                }
                if Self::installing().await {
                    return err(409, "An update is already running");
                }
                let unit = format!("--unit={UNIT}");
                let r = run(
                    &["sudo", "-n", "systemd-run", &unit, "--collect", "--quiet", "/usr/lib/benchbob/system/benchbob-update"],
                    Opts::default(),
                )
                .await;
                if r.code != 0 {
                    let why = if r.err.is_empty() { format!("exit {}", r.code) } else { r.err };
                    return err(500, &format!("Couldn't start the update: {why}"));
                }
                ok(json!({ "ok": true, "message": "Updating: BenchBob restarts when it's done" }))
            }
            _ => None,
        }
    }
}

pub fn docs() -> Value {
    json!({
        "GET /api/update": "Installed and latest release, whether an update is available, release notes, and the log of the last update run",
        "POST /api/update/check": "Ask GitHub for the latest release now (also happens every 6 hours unless turned off)",
        "PUT /api/update/settings": { "body": { "check": true }, "description": "Turn the periodic release check on or off" },
        "POST /api/update/install": { "body": { "confirm": true }, "description": "Install the latest release with apt (falls back to the GitHub download). BenchBob restarts when it's done" },
    })
}

// ---------- BenchBob on a card we flash ----------
//
// "Install BenchBob" in the flash settings: the new system gets our apt source and key,
// and installs the package on its first boot with a network. cloud-init images (Pi OS
// Trixie) do that with write_files + packages; firstrun.sh images (Bookworm) run before
// the network is up, so they get a oneshot unit that waits for it. Used by the flash panel.

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

    #[test]
    fn compare_versions_orders_releases_pre_and_post_release_builds() {
        let ordered = [
            "0.0.0~git85.83b68de",
            "0.0.0~git86.a1b2c3d",
            "0.1.0~git90.ffffff0",
            "0.1.0",
            "0.1.0+git4.abc1234",
            "0.1.0+git12.0000000",
            "0.1.1",
            "0.2.0",
            "1.0.0",
        ];
        for (i, a) in ordered.iter().enumerate() {
            for (j, b) in ordered.iter().enumerate() {
                assert_eq!(compare_versions(a, b).signum(), (i as i64 - j as i64).signum(), "{a} vs {b}");
            }
        }
    }

    #[test]
    fn compare_versions_accepts_a_v_prefix_and_treats_unparseable_as_equal() {
        assert!(compare_versions("v0.2.0", "0.1.0") > 0);
        assert_eq!(compare_versions("dev", "0.1.0"), 0);
    }
}
