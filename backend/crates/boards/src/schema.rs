//! The board file format (documented with examples in `backend/boards/README.md`).

use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// One board file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardDef {
    pub id: String,
    /// Shown when the device tree / DMI gives no name.
    pub name: String,
    #[serde(default)]
    pub vendor: String,
    pub soc: Option<String>,
    /// Has BenchBob been run on this board by us? Untested boards are labelled.
    #[serde(default)]
    pub tested: bool,
    /// Pinout and config sources (URLs), shown in the API so users can check our data.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Shown with the board: caveats, the vendor's own overlay tool…
    #[serde(default)]
    pub notes: Vec<String>,
    pub detect: Detect,
    /// Per-model refinements of `tested`, the display name and the SoC.
    #[serde(default)]
    pub refine: Vec<Refine>,
    /// Chip keys used by header pins → identifiers matching a gpiochip's uAPI label or
    /// its sysfs device ("gpio4", "fec50000.gpio").
    #[serde(default)]
    pub chips: IndexMap<String, Vec<String>>,
    #[serde(default, rename = "header")]
    pub headers: Vec<Header>,
    pub gpio: Gpio,
    pub boot: Option<Boot>,
    pub health: Health,
}

/// A set of conditions on what the kernel says about the machine. Every field given must
/// match (and), any entry within a field may (or). Patterns are regular expressions.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct When {
    /// Against each string of /proc/device-tree/compatible.
    #[serde(default)]
    pub compatible: Vec<Pattern>,
    /// Against /proc/device-tree/model.
    #[serde(default)]
    pub model: Vec<Pattern>,
    /// The CPU architecture BenchBob was built for: "x86_64", "x86", "aarch64", "arm", "riscv64".
    #[serde(default)]
    pub arch: Vec<String>,
    #[serde(default)]
    pub dmi: DmiWhen,
    /// Running under Windows Subsystem for Linux.
    pub wsl: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DmiWhen {
    #[serde(default)]
    pub sys_vendor: Vec<Pattern>,
    #[serde(default)]
    pub product_name: Vec<Pattern>,
    #[serde(default)]
    pub board_vendor: Vec<Pattern>,
    #[serde(default)]
    pub board_name: Vec<Pattern>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Detect {
    /// The score when the conditions match: the highest-scoring board wins. Vendor boards
    /// use 50, families 100, x86 10, generic 1.
    pub priority: u32,
    /// Matches every machine (the generic fallback).
    #[serde(default)]
    pub always: bool,
    // The conditions, as in `When` (serde can't flatten with deny_unknown_fields).
    #[serde(default)]
    pub compatible: Vec<Pattern>,
    #[serde(default)]
    pub model: Vec<Pattern>,
    #[serde(default)]
    pub arch: Vec<String>,
    #[serde(default)]
    pub dmi: DmiWhen,
    pub wsl: Option<bool>,
}

impl Detect {
    pub fn when(&self) -> When {
        When {
            compatible: self.compatible.clone(),
            model: self.model.clone(),
            arch: self.arch.clone(),
            dmi: self.dmi.clone(),
            wsl: self.wsl,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refine {
    #[serde(default)]
    pub when: When,
    pub tested: Option<bool>,
    /// A name template: `{model}` (device-tree model), `{dmi}` (DMI vendor + product,
    /// placeholders dropped). A template whose placeholders are all empty yields nothing,
    /// and the next rule (then the model, then `name`) is tried.
    pub name: Option<String>,
    pub soc: Option<String>,
}

/// A regular expression from a board file.
#[derive(Debug, Clone)]
pub struct Pattern(pub Regex);

impl<'de> Deserialize<'de> for Pattern {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Regex::new(&s).map(Pattern).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PinKind {
    #[serde(rename = "3v3")]
    V3v3,
    #[serde(rename = "5v")]
    V5,
    Gnd,
    Gpio,
    Other,
    Nc,
}

/// One physical pin. Field order is the API's (as the TypeScript board files built it).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderPin {
    pub pin: u32,
    #[serde(default = "gpio_kind")]
    pub kind: PinKind,
    /// Raspberry Pi: the BCM GPIO number (also its line on the SoC gpiochip).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpio: Option<u32>,
    /// SoC pad name as the vendor prints it: "GPIO4_B3", "PH5", "GPIOX_12".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soc: Option<String>,
    /// Other boards: a key of `chips`, and the line offset on that chip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Power and special pins: the vendor's label ("VDD_ADC", "SARADC_IN4").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Alternate functions the vendor lists for the pin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub funcs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn gpio_kind() -> PinKind {
    PinKind::Gpio
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    /// "j8", "40pin"…
    pub id: String,
    /// "40-pin header"
    pub name: String,
    pub pins: Vec<HeaderPin>,
}

/// The GPIO panel the board gets, and how levels are read.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gpio {
    /// "rpi": pinctrl for function/pull of the SoC bank, config.txt (the Raspberry Pi
    /// path); "gpiochip": the generic v2 uAPI view. Reported as `capabilities.gpio`.
    pub panel: Panel,
    /// The passive level reader: a built-in by name, or a list tried in order.
    #[serde(default)]
    pub reader: ReaderSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Panel {
    Rpi,
    Gpiochip,
}

impl Panel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Panel::Rpi => "rpi",
            Panel::Gpiochip => "gpiochip",
        }
    }
}

/// Built-in level readers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Reader {
    /// Generic: levels only for watched lines, through as-is uAPI line requests.
    #[serde(rename = "gpiochip")]
    Gpiochip,
    /// /dev/gpiomem GPLEV0/1 on BCM2835/6/7 and BCM2711 (same register block).
    #[serde(rename = "bcm2711-gpiomem")]
    Bcm2711Gpiomem,
    /// /dev/gpiomem0 SYS_RIO SYNC_IN on the Pi 5's RP1 (untested: no Pi 5 here).
    #[serde(rename = "rp1")]
    Rp1,
    /// Run `pinctrl get` every tick (the Pi fallback when nothing faster works).
    #[serde(rename = "pinctrl")]
    Pinctrl,
}

impl Reader {
    pub fn as_str(&self) -> &'static str {
        match self {
            Reader::Gpiochip => "gpiochip",
            Reader::Bcm2711Gpiomem => "bcm2711-gpiomem",
            Reader::Rp1 => "rp1",
            Reader::Pinctrl => "pinctrl",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ReaderSpec {
    One(Reader),
    List(Vec<ReaderChoice>),
}

impl Default for ReaderSpec {
    fn default() -> Self {
        ReaderSpec::One(Reader::Gpiochip)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderChoice {
    #[serde(default)]
    pub when: When,
    #[serde(rename = "use")]
    pub reader: Reader,
}

/// Where the board's boot-time bus configuration lives.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Boot {
    /// Tried in order; the first that exists is used (the file depends on the OS as much
    /// as on the board). config.txt is taken as-is.
    #[serde(default)]
    pub files: Vec<BootFile>,
    /// Where installed .dtbo files are, to offer only overlays this image has.
    #[serde(default)]
    pub overlay_dirs: Vec<String>,
    /// extlinux / uEnv: how a .dtbo in overlay_dirs is written in the file ("/overlays/").
    pub overlay_ref: Option<String>,
    /// Header overlays the vendor documents, with the pins each takes.
    #[serde(default)]
    pub overlays: Vec<OverlayChoice>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BootFile {
    pub kind: BootKind,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum BootKind {
    /// Raspberry Pi firmware config (dtparam/dtoverlay).
    #[serde(rename = "config.txt")]
    ConfigTxt,
    /// `overlays=` in Armbian's /boot/armbianEnv.txt.
    #[serde(rename = "armbianEnv")]
    ArmbianEnv,
    /// The same format in Orange Pi's /boot/orangepiEnv.txt.
    #[serde(rename = "orangepiEnv")]
    OrangepiEnv,
    /// `uboot_overlay_addrN=` in BeagleBoard.org's AM335x /boot/uEnv.txt.
    #[serde(rename = "uEnv")]
    UEnv,
    /// `fdtoverlays` in extlinux.conf's default label.
    #[serde(rename = "extlinux")]
    Extlinux,
    /// An executable implementing the board-script contract (boards/README.md).
    #[serde(rename = "script")]
    Script,
}

impl BootKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            BootKind::ConfigTxt => "config.txt",
            BootKind::ArmbianEnv => "armbianEnv",
            BootKind::OrangepiEnv => "orangepiEnv",
            BootKind::UEnv => "uEnv",
            BootKind::Extlinux => "extlinux",
            BootKind::Script => "script",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayChoice {
    /// The vendor's short name ("i2c5-m3"); matched against the installed .dtbo files.
    pub name: String,
    /// The same overlay under another image's name (Armbian's "i2c3-ph" for "ph-i2c3").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// "I²C5 (SDA 3, SCL 5)"
    pub what: String,
    /// Physical pins on the board's first header.
    pub pins: Vec<u32>,
}

/// Health readings: built-in or a script.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Health {
    Builtin(HealthKind),
    Script { script: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthKind {
    /// The Pi firmware's throttling flags and PMIC, plus sysfs.
    Vcgencmd,
    /// thermal / hwmon / power_supply only.
    Sysfs,
}

impl Health {
    pub fn as_str(&self) -> &'static str {
        match self {
            Health::Builtin(HealthKind::Vcgencmd) => "vcgencmd",
            Health::Builtin(HealthKind::Sysfs) => "sysfs",
            Health::Script { .. } => "script",
        }
    }
}
