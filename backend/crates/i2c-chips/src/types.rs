//! What probes and decoders produce (the TypeScript `Val`, `Readout`, `Probe`, `ChipState`
//! and `Opt`). Field order is the JSON key order the server emits.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::decode::{Bme280Cal, Bme680Cal, Bmp180Cal};
use crate::io::I2cError;

/// A reading's value: `number | string | boolean | null`.
#[derive(Debug, Clone, PartialEq)]
pub enum V {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
}

impl V {
    pub fn num(&self) -> Option<f64> {
        match self {
            V::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn str(&self) -> Option<&str> {
        match self {
            V::Str(s) => Some(s),
            _ => None,
        }
    }
}

impl From<f64> for V {
    fn from(n: f64) -> Self {
        V::Num(n)
    }
}
impl From<&str> for V {
    fn from(s: &str) -> Self {
        V::Str(s.into())
    }
}
impl From<String> for V {
    fn from(s: String) -> Self {
        V::Str(s)
    }
}
impl From<Option<f64>> for V {
    fn from(n: Option<f64>) -> Self {
        n.map_or(V::Null, V::Num)
    }
}

/// One reading: `{ k, label, v, unit?, d? }`. `k` is the stable key history is kept under.
#[derive(Debug, Clone, PartialEq)]
pub struct Val {
    pub k: String,
    pub label: String,
    pub v: V,
    pub unit: Option<&'static str>,
    /// Decimals for display.
    pub d: Option<u8>,
}

impl Val {
    /// A reading without unit or decimals.
    pub fn new(k: impl Into<String>, label: impl Into<String>, v: impl Into<V>) -> Self {
        Self { k: k.into(), label: label.into(), v: v.into(), unit: None, d: None }
    }
    /// A number with a unit and display decimals.
    pub fn unit(k: impl Into<String>, label: impl Into<String>, v: impl Into<V>, unit: &'static str, d: u8) -> Self {
        Self { k: k.into(), label: label.into(), v: v.into(), unit: Some(unit), d: Some(d) }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pin {
    pub name: String,
    pub level: u8,
    pub dir: Option<&'static str>,
    pub pull: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    pub ch: u8,
    pub duty: f64,
    pub on: u16,
    pub off: u16,
    pub full: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HexDump {
    pub start: u32,
    pub bytes: Vec<u8>,
}

/// A decoder's result. `stale`: the data isn't fresh (the chip is asleep / idle; Measure
/// would refresh it).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Readout {
    pub values: Vec<Val>,
    pub pins: Option<Vec<Pin>>,
    pub channels: Option<Vec<Channel>>,
    pub hex: Option<HexDump>,
    pub stale: Option<bool>,
    pub note: Option<String>,
}

impl Readout {
    pub fn values(values: Vec<Val>) -> Self {
        Self { values, ..Default::default() }
    }
    /// `{ values: [], stale: true, note }`
    pub fn stale(note: &str) -> Self {
        Self { stale: Some(true), note: Some(note.into()), ..Default::default() }
    }
    /// `{ values: [], note }`
    pub fn note(note: &str) -> Self {
        Self { note: Some(note.into()), ..Default::default() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// An ID register matched.
    Id,
    Likely,
    Guess,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Id => "id",
            Confidence::Likely => "likely",
            Confidence::Guess => "guess",
        }
    }
}

/// What an expander's pure reads showed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpanderKind {
    Pcf,
    Pcf16,
    Mcp,
}

/// Probe results a chip's read/measure reuse (the TypeScript `probe.data`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProbeData {
    pub expander: Option<ExpanderKind>,
    /// TI die id (INA226 0x2260…).
    pub die: Option<u16>,
    /// Chip ID register (Bosch, MPU).
    pub id: Option<u8>,
    pub periodic: Option<bool>,
    pub ds3231: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    pub confidence: Confidence,
    /// Display name override, e.g. "BMP280".
    pub variant: Option<String>,
    pub note: Option<String>,
    pub data: ProbeData,
}

impl Probe {
    pub fn new(confidence: Confidence, note: impl Into<String>) -> Self {
        Self { confidence, variant: None, note: Some(note.into()), data: ProbeData::default() }
    }
    pub fn variant(mut self, v: impl Into<String>) -> Self {
        self.variant = Some(v.into());
        self
    }
    pub fn data(mut self, d: ProbeData) -> Self {
        self.data = d;
        self
    }
}

/// Per-device cache filled by decoders (calibration, last sample).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cache {
    pub cal: Option<Bme280Cal>,
    pub cal680: Option<Bme680Cal>,
    pub cal180: Option<Bmp180Cal>,
    pub last_t: Option<f64>,
    pub periodic: Option<bool>,
    pub last: Option<Readout>,
}

/// One identified device: its probe result, the user's options and the decoder cache.
#[derive(Debug, Clone, PartialEq)]
pub struct ChipState {
    pub probe: Probe,
    pub opts: BTreeMap<String, f64>,
    pub cache: Cache,
}

impl ChipState {
    pub fn new(probe: Probe) -> Self {
        Self { probe, opts: BTreeMap::new(), cache: Cache::default() }
    }
    /// `Number(s.opts[key] ?? default)`
    pub fn opt(&self, key: &str, default: f64) -> f64 {
        self.opts.get(key).copied().unwrap_or(default)
    }
}

/// A per-device option shown in the UI (`shunt`, EEPROM `size`…).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Opt {
    pub key: &'static str,
    pub label: &'static str,
    /// "number" | "select"
    pub kind: &'static str,
    pub default: f64,
    pub choices: &'static [(f64, &'static str)],
    pub unit: Option<&'static str>,
}

/// Why a probe, read or measure failed: the bus, or the chip's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum ChipError {
    Bus(I2cError),
    Other(String),
}

impl ChipError {
    pub fn message(&self) -> String {
        match self {
            ChipError::Bus(e) => e.message(),
            ChipError::Other(s) => s.clone(),
        }
    }
    pub fn bus(&self) -> Option<&I2cError> {
        match self {
            ChipError::Bus(e) => Some(e),
            ChipError::Other(_) => None,
        }
    }
}

impl From<I2cError> for ChipError {
    fn from(e: I2cError) -> Self {
        ChipError::Bus(e)
    }
}

pub type R<T> = Result<T, ChipError>;

/// A plain-error result (`throw new Error(msg)`).
pub fn fail<T>(msg: &str) -> R<T> {
    Err(ChipError::Other(msg.into()))
}
