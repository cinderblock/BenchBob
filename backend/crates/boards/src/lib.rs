//! Board definitions as data: TOML files describing how to recognise a board, its pin
//! headers, how GPIO levels are read, where its boot-time bus configuration lives and
//! which health readings it has (the TypeScript `peripherals/boards/` layer, made
//! pluggable).
//!
//! Files are read from the shipped directory (`/usr/share/benchbob/boards`, or the copies
//! built into the binary) and then from `/etc/benchbob/boards.d`, where a file with the
//! same `id` replaces the shipped one. Adding a board is adding a file.
//!
//! [`detect`] is a pure function over [`DetectInput`] (unit-tested with fixtures for every
//! board); [`read_detect_input`] gathers that input from /proc and /sys.

pub mod schema;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub use schema::*;

include!(concat!(env!("OUT_DIR"), "/embedded.rs"));

/// The shipped board directory of an installed package.
pub const SHIPPED_DIR: &str = "/usr/share/benchbob/boards";
/// Where users add boards or override shipped ones.
pub const USER_DIR: &str = "/etc/benchbob/boards.d";

/// What a board is recognised from. Pure data so detection is unit-testable.
#[derive(Debug, Clone, Default)]
pub struct DetectInput {
    /// `std::env::consts::ARCH`: "aarch64", "x86_64", "arm"…
    pub arch: String,
    /// /proc/device-tree/compatible, most specific first. Empty on x86.
    pub compatible: Vec<String>,
    /// /proc/device-tree/model.
    pub model: String,
    /// /sys/class/dmi/id/*. Empty on ARM boards and in WSL.
    pub dmi: Dmi,
    /// Running under WSL (no DMI, no device tree, no hardware of its own).
    pub wsl: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Dmi {
    pub sys_vendor: String,
    pub product_name: String,
    pub product_version: String,
    pub board_vendor: String,
    pub board_name: String,
}

fn any(pats: &[Pattern], s: &str) -> bool {
    pats.iter().any(|p| p.0.is_match(s))
}

impl When {
    /// Every condition given holds (an empty `When` always does).
    pub fn matches(&self, i: &DetectInput) -> bool {
        (self.compatible.is_empty() || i.compatible.iter().any(|c| any(&self.compatible, c)))
            && (self.model.is_empty() || any(&self.model, &i.model))
            && (self.arch.is_empty() || self.arch.iter().any(|a| a == &i.arch))
            && (self.dmi.sys_vendor.is_empty() || any(&self.dmi.sys_vendor, &i.dmi.sys_vendor))
            && (self.dmi.product_name.is_empty() || any(&self.dmi.product_name, &i.dmi.product_name))
            && (self.dmi.board_vendor.is_empty() || any(&self.dmi.board_vendor, &i.dmi.board_vendor))
            && (self.dmi.board_name.is_empty() || any(&self.dmi.board_name, &i.dmi.board_name))
            && self.wsl.is_none_or(|w| w == i.wsl)
    }

    fn is_empty(&self) -> bool {
        self.compatible.is_empty()
            && self.model.is_empty()
            && self.arch.is_empty()
            && self.dmi.sys_vendor.is_empty()
            && self.dmi.product_name.is_empty()
            && self.dmi.board_vendor.is_empty()
            && self.dmi.board_name.is_empty()
            && self.wsl.is_none()
    }
}

impl BoardDef {
    /// Match score for this input: 0 = not this board; the highest score wins.
    pub fn score(&self, i: &DetectInput) -> u32 {
        let w = self.detect.when();
        let hit = if w.is_empty() { self.detect.always } else { w.matches(i) };
        if hit { self.detect.priority } else { 0 }
    }

    pub fn is_tested(&self, i: &DetectInput) -> bool {
        self.refine.iter().find(|r| r.tested.is_some() && r.when.matches(i)).and_then(|r| r.tested).unwrap_or(self.tested)
    }

    /// The display name from the refine rules, "" if none applies.
    pub fn label(&self, i: &DetectInput) -> String {
        for r in &self.refine {
            let Some(t) = &r.name else { continue };
            if !r.when.matches(i) {
                continue;
            }
            let s = render(t, i);
            if !s.is_empty() {
                return s;
            }
        }
        String::new()
    }

    pub fn soc_of(&self, i: &DetectInput) -> Option<String> {
        self.refine.iter().find(|r| r.soc.is_some() && r.when.matches(i)).and_then(|r| r.soc.clone()).or_else(|| self.soc.clone())
    }

    /// The boot-config file this machine actually has: the first candidate that exists
    /// (config.txt is taken as-is: the Pi path reports a missing file itself).
    pub fn boot_config_with(&self, exists: &dyn Fn(&str) -> bool) -> Option<BootFile> {
        self.boot.as_ref()?.files.iter().find(|f| f.kind == BootKind::ConfigTxt || exists(&f.path)).cloned()
    }

    pub fn boot_config(&self) -> Option<BootFile> {
        self.boot_config_with(&|p| Path::new(p).exists())
    }
}

/// Fill a name template: `{model}`, `{dmi}`. Empty if the placeholders it has are all empty.
fn render(t: &str, i: &DetectInput) -> String {
    let vars = [("{model}", i.model.trim().to_string()), ("{dmi}", dmi_name(i))];
    let used: Vec<&(&str, String)> = vars.iter().filter(|(k, _)| t.contains(k)).collect();
    if !used.is_empty() && used.iter().all(|(_, v)| v.is_empty()) {
        return String::new();
    }
    let mut s = t.to_string();
    for (k, v) in &vars {
        s = s.replace(k, v);
    }
    s
}

/// "ASUSTeK COMPUTER INC. PN51" from DMI, falling back to the board fields, "" if all are
/// the placeholders firmware vendors leave there.
pub fn dmi_name(i: &DetectInput) -> String {
    static JUNK: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let junk = JUNK.get_or_init(|| {
        regex::Regex::new(r"(?i)^(to be filled by o\.?e\.?m\.?|default string|system (product name|manufacturer|version)|not applicable|none|n/a|o\.?e\.?m\.?|x\.x|0123456789|\s*)$").unwrap()
    });
    let clean = |s: &str| if junk.is_match(s.trim()) { String::new() } else { s.trim().to_string() };
    let vendor = Some(clean(&i.dmi.sys_vendor)).filter(|s| !s.is_empty()).unwrap_or_else(|| clean(&i.dmi.board_vendor));
    let product = Some(clean(&i.dmi.product_name)).filter(|s| !s.is_empty()).unwrap_or_else(|| clean(&i.dmi.board_name));
    // Some vendors repeat themselves ("Intel Corporation" / "Intel NUC…"); don't say it twice.
    if !vendor.is_empty() && !product.is_empty() {
        let first = vendor.split(char::is_whitespace).next().unwrap_or("").to_lowercase();
        if product.to_lowercase().starts_with(&first) {
            return product;
        }
    }
    [vendor, product].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")
}

// ---------- the set of boards ----------

/// A board file that failed to load, reported in the API.
#[derive(Debug, Clone)]
pub struct LoadError {
    pub file: String,
    pub error: String,
}

/// Every board BenchBob knows, in load order, with where each came from.
#[derive(Debug, Clone, Default)]
pub struct Boards {
    pub defs: Vec<BoardDef>,
    /// The file each def was read from (same index), "builtin:<name>" for embedded ones.
    pub origin: Vec<String>,
    pub errors: Vec<LoadError>,
    /// The directories read, in order.
    pub dirs: Vec<String>,
}

/// Parse and check one board file.
pub fn parse(text: &str) -> Result<BoardDef, String> {
    let d: BoardDef = toml::from_str(text).map_err(|e| e.to_string())?;
    validate(&d)?;
    Ok(d)
}

fn validate(d: &BoardDef) -> Result<(), String> {
    if d.id.is_empty() || !d.id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
        return Err(format!("id \"{}\": letters, digits, - and _ only", d.id));
    }
    for h in &d.headers {
        for p in &h.pins {
            if p.kind == PinKind::Gpio {
                let pi = p.gpio.is_some();
                let chip = p.chip.is_some() && p.line.is_some();
                if !pi && !chip {
                    return Err(format!("{} pin {}: a gpio pin needs gpio = N or chip + line", h.name, p.pin));
                }
                if let Some(c) = &p.chip
                    && !d.chips.contains_key(c)
                {
                    return Err(format!("{} pin {}: chip \"{c}\" isn't in [chips]", h.name, p.pin));
                }
            }
        }
    }
    if let Health::Script { script } = &d.health
        && !script.starts_with('/')
    {
        return Err("health.script must be an absolute path".into());
    }
    Ok(())
}

impl Boards {
    /// Load the shipped boards (`shipped` if it exists, else the embedded copies), then
    /// every `*.toml` in `user`; a later file with the same id replaces the earlier one.
    pub fn load(shipped: Option<&Path>, user: Option<&Path>) -> Self {
        let mut b = Boards::default();
        match shipped.filter(|p| p.is_dir()) {
            Some(dir) => b.read_dir(dir),
            None => {
                b.dirs.push("builtin".into());
                for (name, text) in EMBEDDED {
                    b.add(&format!("builtin:{name}"), text);
                }
            }
        }
        if let Some(dir) = user.filter(|p| p.is_dir()) {
            b.read_dir(dir);
        }
        b
    }

    /// The standard locations: `BENCHBOB_BOARDS_DIR` / /usr/share/benchbob/boards (else
    /// the embedded copies), then `BENCHBOB_BOARDS_D` / /etc/benchbob/boards.d.
    pub fn load_default() -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty()).map(PathBuf::from);
        let shipped = env("BENCHBOB_BOARDS_DIR").unwrap_or_else(|| PathBuf::from(SHIPPED_DIR));
        let user = env("BENCHBOB_BOARDS_D").unwrap_or_else(|| PathBuf::from(USER_DIR));
        Self::load(Some(&shipped), Some(&user))
    }

    /// Only the embedded copies (tests, and machines without the shipped directory).
    pub fn builtin() -> Self {
        Self::load(None, None)
    }

    fn read_dir(&mut self, dir: &Path) {
        self.dirs.push(dir.display().to_string());
        let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect(),
            Err(e) => {
                self.errors.push(LoadError { file: dir.display().to_string(), error: e.to_string() });
                return;
            }
        };
        files.sort();
        for f in files {
            match std::fs::read_to_string(&f) {
                Ok(t) => self.add(&f.display().to_string(), &t),
                Err(e) => self.errors.push(LoadError { file: f.display().to_string(), error: e.to_string() }),
            }
        }
    }

    fn add(&mut self, origin: &str, text: &str) {
        match parse(text) {
            Ok(d) => {
                if let Some(i) = self.defs.iter().position(|x| x.id == d.id) {
                    self.defs[i] = d;
                    self.origin[i] = origin.into();
                } else {
                    self.defs.push(d);
                    self.origin.push(origin.into());
                }
            }
            Err(e) => {
                tracing::warn!("[boards] {origin}: {e}");
                self.errors.push(LoadError { file: origin.into(), error: e });
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&BoardDef> {
        self.defs.iter().find(|d| d.id == id)
    }

    /// The best-matching board for this input (generic if nothing else claims it).
    pub fn detect(&self, i: &DetectInput) -> BoardDef {
        let mut best: Option<&BoardDef> = None;
        let mut score = 0;
        for d in &self.defs {
            let s = d.score(i);
            if s > score {
                best = Some(d);
                score = s;
            }
        }
        best.cloned().unwrap_or_else(fallback)
    }
}

/// The generic board, for when no file (not even generic.toml) matched.
fn fallback() -> BoardDef {
    parse(
        r#"id = "generic"
name = "Linux computer"
tested = true
health = "sysfs"
[detect]
priority = 1
always = true
[gpio]
panel = "gpiochip"
"#,
    )
    .expect("built-in generic board")
}

// ---------- the board BenchBob runs on ----------

/// A definition plus what the hardware called itself.
#[derive(Debug, Clone)]
pub struct Board {
    pub def: BoardDef,
    pub id: String,
    /// Best human name: the refine rules', the device-tree model, or the definition's name.
    pub name: String,
    /// The raw model string (device tree or DMI), "" if none.
    pub model: String,
    pub tested: bool,
    /// Forced with BENCHBOB_BOARD rather than detected.
    pub forced: bool,
    pub input: DetectInput,
}

pub fn describe(def: &BoardDef, i: &DetectInput, forced: bool) -> Board {
    let model = if !i.model.is_empty() {
        i.model.clone()
    } else {
        [i.dmi.sys_vendor.trim(), i.dmi.product_name.trim()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")
    };
    let name = if forced {
        def.name.clone()
    } else {
        Some(def.label(i))
            .filter(|s| !s.is_empty())
            .or_else(|| Some(i.model.clone()).filter(|s| !s.is_empty()))
            .unwrap_or_else(|| def.name.clone())
    };
    Board { def: def.clone(), id: def.id.clone(), name, model, tested: def.is_tested(i), forced, input: i.clone() }
}

fn rd(p: &str) -> String {
    std::fs::read(p).map(|b| b.iter().map(|&c| c as char).collect()).unwrap_or_default()
}

/// Read what the kernel says about this machine.
pub fn read_detect_input() -> DetectInput {
    let dmi = |f: &str| rd(&format!("/sys/class/dmi/id/{f}")).trim().to_string();
    DetectInput {
        arch: std::env::consts::ARCH.into(),
        compatible: rd("/proc/device-tree/compatible").split('\0').filter(|s| !s.is_empty()).map(String::from).collect(),
        model: rd("/proc/device-tree/model").replace('\0', "").trim().to_string(),
        dmi: Dmi {
            sys_vendor: dmi("sys_vendor"),
            product_name: dmi("product_name"),
            product_version: dmi("product_version"),
            board_vendor: dmi("board_vendor"),
            board_name: dmi("board_name"),
        },
        wsl: rd("/proc/sys/kernel/osrelease").to_lowercase().contains("microsoft"),
    }
}

/// Pick the board: `BENCHBOB_BOARD=<id>` forces one (to try a board's UI on a machine that
/// isn't one), otherwise detection.
pub fn pick(boards: &Boards, i: &DetectInput, want: Option<&str>) -> Board {
    if let Some(w) = want {
        if let Some(d) = boards.get(w) {
            return describe(d, i, true);
        }
        let ids: Vec<&str> = boards.defs.iter().map(|d| d.id.as_str()).collect();
        tracing::warn!("[boards] BENCHBOB_BOARD={w} isn't a known board ({}); detecting instead", ids.join(", "));
    }
    describe(&boards.detect(i), i, false)
}

impl Board {
    /// What the API and dashboards get: identity, plus what the board can do.
    pub fn info(&self) -> Value {
        self.info_with(&|p| Path::new(p).exists())
    }

    pub fn info_with(&self, exists: &dyn Fn(&str) -> bool) -> Value {
        let d = &self.def;
        let boot = d.boot_config_with(exists).map(|f| json!({ "kind": f.kind.as_str(), "path": f.path }));
        json!({
            "id": self.id,
            "name": self.name,
            "model": self.model,
            "tested": self.tested,
            "forced": self.forced,
            "vendor": d.vendor,
            "soc": d.soc_of(&self.input),
            "capabilities": {
                "headers": d.headers.iter().map(|h| json!({ "id": h.id, "name": h.name, "pins": h.pins.len() })).collect::<Vec<_>>(),
                "gpio": d.gpio.panel.as_str(),
                "bootConfig": boot,
                "health": d.health.as_str(),
            },
            "sources": d.sources,
            "notes": d.notes,
        })
    }

    /// The passive level reader for this machine.
    pub fn reader(&self) -> Option<Reader> {
        match &self.def.gpio.reader {
            ReaderSpec::One(r) => Some(*r),
            ReaderSpec::List(l) => l.iter().find(|c| c.when.matches(&self.input)).map(|c| c.reader),
        }
    }
}

/// Board chip keys → the gpiochip (by uAPI label or sysfs device name, case-insensitively)
/// that is that controller here.
pub fn resolve_chips<'a, C>(
    def: &BoardDef,
    chips: &'a [C],
    label: impl Fn(&C) -> &str,
    device: impl Fn(&C) -> &str,
) -> indexmap::IndexMap<String, &'a C> {
    let mut out = indexmap::IndexMap::new();
    for (key, ids) in &def.chips {
        let want: Vec<String> = ids.iter().map(|s| s.to_lowercase()).collect();
        if let Some(hit) = chips.iter().find(|c| want.contains(&label(c).to_lowercase()) || want.contains(&device(c).to_lowercase())) {
            out.insert(key.clone(), hit);
        }
    }
    out
}
