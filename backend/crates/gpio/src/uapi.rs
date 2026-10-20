//! The GPIO character device, v2 uAPI (peripherals/lib/gpiochip.ts, plus output requests).
//!
//! The board-independent way to see GPIO: every /dev/gpiochipN, its label and line count,
//! and per line the name, consumer, direction and flags, straight from the kernel with
//! ioctl() (no libgpiod, no gpioinfo process). This also covers USB GPIO adapters (FT232H,
//! CH341, MCP2221, CP2112): their drivers register ordinary gpiochips.
//!
//! What v2 can't do is read a level passively. A value is only readable through a line
//! *request*, and a request can disturb the pin: a direction flag reconfigures it, and the
//! request fails with EBUSY if a driver owns the line. So levels are only offered for lines
//! the user picks to watch, and those are requested "as is" (no INPUT/OUTPUT flag): the
//! kernel then leaves direction, bias and drive alone and just lets us read the value.
//!
//! Driving a line ([`LineHandle::drive`]) reconfigures the request as an output with
//! GPIO_V2_LINE_SET_CONFIG, so the line is never let go between "watched" and "driven".
//! Closing the request releases the line; what the pin does then is up to the chip's
//! driver (the uAPI promises nothing). Seen on desky (Pi 4, Linux 6.18 rpt): the pin stays
//! an output at its last value, and gpiolib keeps reporting "output" for it until the
//! next request, even after something else (pinctrl) has made it an input again.
//!
//! Struct layouts and ioctl numbers are from include/uapi/linux/gpio.h (v2 since Linux
//! 5.10); the structs were designed with explicit padding, so they are identical on
//! 32- and 64-bit ARM and x86. Pure encode/decode is separate for the unit tests.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

// ---------- constants (include/uapi/linux/gpio.h) ----------

const NAME: usize = 32; // GPIO_MAX_NAME_SIZE
const LINES_MAX: usize = 64; // GPIO_V2_LINES_MAX
const ATTRS_MAX: u32 = 10; // GPIO_V2_LINE_NUM_ATTRS_MAX

pub const CHIPINFO_SIZE: usize = NAME * 2 + 4; // name, label, lines = 68
pub const LINEINFO_SIZE: usize = 256; // name, consumer, offset, num_attrs, flags, attrs[10], padding[4]
pub const LINECONFIG_SIZE: usize = 272; // flags, num_attrs, padding[5], attrs[10] (24 each)
pub const LINEREQ_SIZE: usize = 592; // offsets[64], consumer, config, num_lines, event_buffer_size, padding[5], fd
pub const VALUES_SIZE: usize = 16; // bits, mask

/// `_IOC(dir, type, nr, size)` with the asm-generic layout ARM and x86 share.
pub const fn ioc(dir: u32, ty: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | (ty << 8) | nr
}
const R: u32 = 2;
const RW: u32 = 3;
pub const GPIO_GET_CHIPINFO_IOCTL: u32 = ioc(R, 0xb4, 0x01, CHIPINFO_SIZE);
pub const GPIO_V2_GET_LINEINFO_IOCTL: u32 = ioc(RW, 0xb4, 0x05, LINEINFO_SIZE);
pub const GPIO_V2_GET_LINE_IOCTL: u32 = ioc(RW, 0xb4, 0x07, LINEREQ_SIZE);
pub const GPIO_V2_LINE_SET_CONFIG_IOCTL: u32 = ioc(RW, 0xb4, 0x0d, LINECONFIG_SIZE);
pub const GPIO_V2_LINE_GET_VALUES_IOCTL: u32 = ioc(RW, 0xb4, 0x0e, VALUES_SIZE);
pub const GPIO_V2_LINE_SET_VALUES_IOCTL: u32 = ioc(RW, 0xb4, 0x0f, VALUES_SIZE);

pub mod flag {
    pub const USED: u64 = 1 << 0;
    pub const ACTIVE_LOW: u64 = 1 << 1;
    pub const INPUT: u64 = 1 << 2;
    pub const OUTPUT: u64 = 1 << 3;
    pub const EDGE_RISING: u64 = 1 << 4;
    pub const EDGE_FALLING: u64 = 1 << 5;
    pub const OPEN_DRAIN: u64 = 1 << 6;
    pub const OPEN_SOURCE: u64 = 1 << 7;
    pub const BIAS_PULL_UP: u64 = 1 << 8;
    pub const BIAS_PULL_DOWN: u64 = 1 << 9;
    pub const BIAS_DISABLED: u64 = 1 << 10;
}
const ATTR_ID_OUTPUT_VALUES: u32 = 2;
const ATTR_ID_DEBOUNCE: u32 = 3;

// ---------- pure encode/decode ----------

fn cstr(b: &[u8]) -> String {
    let z = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..z]).into_owned()
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn put_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChipInfo {
    /// "gpiochip0"
    pub name: String,
    /// "pinctrl-bcm2711", "gpio4", "ftdi-cbus"…
    pub label: String,
    pub lines: u32,
}

pub fn decode_chip_info(b: &[u8]) -> ChipInfo {
    ChipInfo { name: cstr(&b[..NAME]), label: cstr(&b[NAME..NAME * 2]), lines: u32_at(b, NAME * 2) }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineInfo {
    pub offset: u32,
    pub name: String,
    /// Who holds the line: a driver ("onewire@4", "ACT"), a process, or None.
    pub consumer: Option<String>,
    /// Requested by someone, or (on pinctrl-strict SoCs) muxed to a peripheral.
    pub used: bool,
    /// "input" | "output"
    pub direction: &'static str,
    pub active_low: bool,
    /// "push-pull" | "open-drain" | "open-source"
    pub drive: &'static str,
    /// "pull-up" | "pull-down" | "disabled"
    pub bias: Option<&'static str>,
    /// "rising" | "falling" | "both"
    pub edge: Option<&'static str>,
    pub debounce_us: Option<u32>,
    /// gpioinfo-style words, for display: ["used", "output", "open-drain"].
    pub flags: Vec<String>,
}

/// An empty gpio_v2_line_info with the offset filled in, as GPIO_V2_GET_LINEINFO_IOCTL wants it.
pub fn encode_line_info_req(offset: u32) -> [u8; LINEINFO_SIZE] {
    let mut b = [0u8; LINEINFO_SIZE];
    put_u32(&mut b, 64, offset);
    b
}

pub fn decode_line_info(b: &[u8]) -> LineInfo {
    let offset = u32_at(b, 64);
    let num_attrs = u32_at(b, 68);
    // Flags are a u64, but every defined bit fits in the low word.
    let f = u32_at(b, 72) as u64;
    let mut debounce_us = None;
    for i in 0..num_attrs.min(ATTRS_MAX) as usize {
        let a = 80 + i * 16;
        if u32_at(b, a) == ATTR_ID_DEBOUNCE {
            debounce_us = Some(u32_at(b, a + 8));
        }
    }
    let consumer = cstr(&b[NAME..NAME * 2]);
    let used = f & flag::USED != 0;
    let direction = if f & flag::OUTPUT != 0 { "output" } else { "input" };
    let drive = if f & flag::OPEN_DRAIN != 0 {
        "open-drain"
    } else if f & flag::OPEN_SOURCE != 0 {
        "open-source"
    } else {
        "push-pull"
    };
    let bias = if f & flag::BIAS_PULL_UP != 0 {
        Some("pull-up")
    } else if f & flag::BIAS_PULL_DOWN != 0 {
        Some("pull-down")
    } else if f & flag::BIAS_DISABLED != 0 {
        Some("disabled")
    } else {
        None
    };
    let (rising, falling) = (f & flag::EDGE_RISING != 0, f & flag::EDGE_FALLING != 0);
    let edge = match (rising, falling) {
        (true, true) => Some("both"),
        (true, false) => Some("rising"),
        (false, true) => Some("falling"),
        _ => None,
    };
    let mut flags: Vec<String> = Vec::new();
    if used {
        flags.push("used".into());
    }
    flags.push(direction.into());
    if f & flag::ACTIVE_LOW != 0 {
        flags.push("active-low".into());
    }
    if drive != "push-pull" {
        flags.push(drive.into());
    }
    if let Some(b) = bias {
        flags.push(if b == "disabled" { "bias-disabled".into() } else { b.into() });
    }
    if let Some(e) = edge {
        flags.push(format!("edges={e}"));
    }
    if let Some(d) = debounce_us.filter(|d| *d != 0) {
        flags.push(format!("debounce={d}us"));
    }
    // A line muxed to a peripheral on a pinctrl-strict SoC is "used" with no consumer;
    // libgpiod shows those as "kernel", and so do we, so "free" always means free.
    let consumer = if !consumer.is_empty() {
        Some(consumer)
    } else if used {
        Some("kernel".into())
    } else {
        None
    };
    LineInfo {
        offset,
        name: cstr(&b[..NAME]),
        consumer,
        used,
        direction,
        active_low: f & flag::ACTIVE_LOW != 0,
        drive,
        bias,
        edge,
        debounce_us,
        flags,
    }
}

/// gpio_v2_line_config: `flags` (0 = as-is: direction, bias and drive left exactly as
/// they are), and for outputs the initial values of the requested lines (bit i = i-th line).
pub fn encode_line_config(b: &mut [u8], flags: u64, output: Option<(u64, u64)>) {
    b[..LINECONFIG_SIZE].fill(0);
    put_u64(b, 0, flags);
    if let Some((values, mask)) = output {
        put_u32(b, 8, 1); // num_attrs
        // attrs[0] @32: attr { id, padding, values } then mask
        put_u32(b, 32, ATTR_ID_OUTPUT_VALUES);
        put_u64(b, 40, values);
        put_u64(b, 48, mask);
    }
}

/// gpio_v2_line_request for `offsets` with the given config.
pub fn encode_line_request(offsets: &[u32], consumer: &str, flags: u64, output: Option<(u64, u64)>) -> anyhow::Result<[u8; LINEREQ_SIZE]> {
    anyhow::ensure!(!offsets.is_empty() && offsets.len() <= LINES_MAX, "1–{LINES_MAX} lines per request");
    let mut b = [0u8; LINEREQ_SIZE];
    for (i, o) in offsets.iter().enumerate() {
        put_u32(&mut b, i * 4, *o);
    }
    let c = consumer.as_bytes();
    let n = c.len().min(NAME - 1);
    b[256..256 + n].copy_from_slice(&c[..n]);
    encode_line_config(&mut b[288..288 + LINECONFIG_SIZE], flags, output);
    put_u32(&mut b, 560, offsets.len() as u32); // num_lines
    b[588..592].copy_from_slice(&(-1i32).to_le_bytes()); // fd, filled in by the kernel
    Ok(b)
}

pub fn request_fd(b: &[u8]) -> i32 {
    i32::from_le_bytes(b[588..592].try_into().unwrap())
}

/// gpio_v2_line_values asking for (or setting) the lines in `mask`.
pub fn encode_values(bits: u64, mask: u64) -> [u8; VALUES_SIZE] {
    let mut b = [0u8; VALUES_SIZE];
    put_u64(&mut b, 0, bits);
    put_u64(&mut b, 8, mask);
    b
}

pub fn decode_value_bits(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

// ---------- ioctl ----------

#[derive(Debug, Clone)]
pub struct GpioError {
    pub what: String,
    pub errno: i32,
}

impl std::fmt::Display for GpioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.errno {
            1 => "EPERM".to_string(),
            2 => "ENOENT".into(),
            13 => "EACCES".into(),
            16 => "EBUSY".into(),
            19 => "ENODEV".into(),
            22 => "EINVAL".into(),
            25 => "ENOTTY".into(),
            n => format!("errno {n}"),
        };
        let hint = match self.errno {
            16 => " (the line is in use)",
            1 | 13 => " (no permission: is BenchBob in the gpio group?)",
            _ => "",
        };
        write!(f, "{}: {name}{hint}", self.what)
    }
}

impl std::error::Error for GpioError {}

fn ioctl(fd: RawFd, req: u32, buf: &mut [u8], what: impl FnOnce() -> String) -> Result<(), GpioError> {
    // SAFETY: `buf` is the size the request number encodes, and lives across the call.
    let r = unsafe { libc::ioctl(fd, req as _, buf.as_mut_ptr()) };
    if r < 0 {
        return Err(GpioError { what: what(), errno: std::io::Error::last_os_error().raw_os_error().unwrap_or(0) });
    }
    Ok(())
}

// ---------- chips and lines ----------

#[derive(Debug, Clone)]
pub struct Chip {
    pub info: ChipInfo,
    /// /dev/gpiochip0
    pub path: String,
    /// The parent device in sysfs: "fe200000.gpio", "fec50000.gpio", "1-1.2:1.0" for USB.
    pub device: String,
    pub usb: bool,
    pub error: Option<String>,
}

/// The kernel's own name for a chip's parent device, so board files can match a chip by
/// its register base.
fn sys_device(name: &str) -> (String, bool) {
    match std::fs::canonicalize(format!("/sys/bus/gpio/devices/{name}")) {
        Ok(p) => {
            let s = p.to_string_lossy().into_owned();
            let parts: Vec<&str> = s.split('/').collect();
            (parts.get(parts.len().wrapping_sub(2)).copied().unwrap_or("").to_string(), s.contains("/usb"))
        }
        Err(_) => (String::new(), false),
    }
}

/// The legacy sysfs class (…/fe200000.gpio/gpio/gpiochip512/label, if CONFIG_GPIO_SYSFS
/// is on) still names a chip whose device node we can't open, so a board's chips match.
fn sys_label(name: &str) -> String {
    let Ok(dir) = std::fs::canonicalize(format!("/sys/bus/gpio/devices/{name}/..")) else { return String::new() };
    let dir = dir.join("gpio");
    std::fs::read_dir(&dir)
        .ok()
        .and_then(|rd| {
            rd.filter_map(|e| e.ok()).find(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with("gpiochip") && n[8..].bytes().all(|c| c.is_ascii_digit())
            })
        })
        .map(|e| std::fs::read_to_string(e.path().join("label")).unwrap_or_default().trim().to_string())
        .unwrap_or_default()
}

/// Every gpiochip with its uAPI info. Symlinks (Pi OS adds gpiochip4 → gpiochip0) are skipped.
pub fn list_chips() -> Vec<Chip> {
    list_chips_in(Path::new("/dev"))
}

pub fn list_chips_in(dev: &Path) -> Vec<Chip> {
    let mut names: Vec<String> = match std::fs::read_dir(dev) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.len() > 8 && n.starts_with("gpiochip") && n[8..].bytes().all(|c| c.is_ascii_digit()))
            .collect(),
        Err(_) => return Vec::new(),
    };
    names.sort_by_key(|n| n[8..].parse::<u32>().unwrap_or(0));
    let mut out = Vec::new();
    for n in names {
        let path = dev.join(&n);
        // A symlink to another chip node is a duplicate.
        if std::fs::symlink_metadata(&path).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            continue;
        }
        let path_s = path.to_string_lossy().into_owned();
        let (device, usb) = sys_device(&n);
        let res = File::open(&path).map_err(|e| e.to_string()).and_then(|f| {
            let mut b = [0u8; CHIPINFO_SIZE];
            ioctl(f.as_raw_fd(), GPIO_GET_CHIPINFO_IOCTL, &mut b, || format!("{n} chip info")).map_err(|e| e.to_string())?;
            Ok(decode_chip_info(&b))
        });
        match res {
            Ok(info) => out.push(Chip { info, path: path_s, device, usb, error: None }),
            Err(e) => {
                // Typically EACCES: root-only /dev/gpiochip* on a stock Debian without a gpio group rule.
                let msg =
                    if e.contains("ermission denied") { format!("{path_s}: permission denied (BenchBob needs the gpio group)") } else { e };
                out.push(Chip {
                    info: ChipInfo { name: n.clone(), label: sys_label(&n), lines: 0 },
                    path: path_s,
                    device,
                    usb,
                    error: Some(msg),
                });
            }
        }
    }
    out
}

/// Info for every line of one chip.
pub fn read_lines(chip: &Chip) -> Result<Vec<LineInfo>, String> {
    let f = File::open(&chip.path).map_err(|e| format!("{}: {e}", chip.path))?;
    let mut out = Vec::with_capacity(chip.info.lines as usize);
    for i in 0..chip.info.lines {
        let mut b = encode_line_info_req(i);
        ioctl(f.as_raw_fd(), GPIO_V2_GET_LINEINFO_IOCTL, &mut b, || format!("{} line {i}", chip.path)).map_err(|e| e.to_string())?;
        out.push(decode_line_info(&b));
    }
    Ok(out)
}

// ---------- line requests ----------

/// How a held line is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Requested as-is, to read the level (a watch).
    AsIs,
    /// Driven as an output.
    Output(u8),
    /// Reconfigured as an input, with this bias.
    Input,
}

/// Pull (bias) for inputs and outputs: None leaves it as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pull {
    Up,
    Down,
    Off,
}

impl Pull {
    fn flag(p: Option<Pull>) -> u64 {
        match p {
            Some(Pull::Up) => flag::BIAS_PULL_UP,
            Some(Pull::Down) => flag::BIAS_PULL_DOWN,
            Some(Pull::Off) => flag::BIAS_DISABLED,
            None => 0,
        }
    }
    pub fn as_str(p: Option<Pull>) -> Option<&'static str> {
        p.map(|p| match p {
            Pull::Up => "up",
            Pull::Down => "down",
            Pull::Off => "none",
        })
    }
}

/// One requested line, held until dropped. While held it shows our consumer name to
/// everyone else, and nobody else can claim it.
pub struct LineHandle {
    pub chip: String,
    pub offset: u32,
    fd: OwnedFd,
    pub mode: Mode,
    pub pull: Option<Pull>,
}

impl LineHandle {
    /// Request one line "as is" so its level can be read. Fails with EBUSY if anything holds it.
    pub fn request_as_is(chip_path: &str, offset: u32, consumer: &str) -> Result<Self, GpioError> {
        Self::request(chip_path, offset, consumer, 0, None, Mode::AsIs, None)
    }

    /// Request one line as an output at `value`.
    pub fn request_output(chip_path: &str, offset: u32, consumer: &str, value: u8, pull: Option<Pull>) -> Result<Self, GpioError> {
        Self::request(
            chip_path,
            offset,
            consumer,
            flag::OUTPUT | Pull::flag(pull),
            Some(((value & 1) as u64, 1)),
            Mode::Output(value & 1),
            pull,
        )
    }

    fn request(
        chip_path: &str,
        offset: u32,
        consumer: &str,
        flags: u64,
        out: Option<(u64, u64)>,
        mode: Mode,
        pull: Option<Pull>,
    ) -> Result<Self, GpioError> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(chip_path)
            .map_err(|e| GpioError { what: chip_path.into(), errno: e.raw_os_error().unwrap_or(0) })?;
        let mut req = encode_line_request(&[offset], consumer, flags, out).map_err(|_| GpioError { what: "request".into(), errno: 22 })?;
        ioctl(f.as_raw_fd(), GPIO_V2_GET_LINE_IOCTL, &mut req, || format!("{chip_path} line {offset}"))?;
        // The line request outlives the chip descriptor (dropped here).
        // SAFETY: the kernel just returned this fd to us and nobody else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(request_fd(&req)) };
        Ok(Self { chip: chip_path.into(), offset, fd, mode, pull })
    }

    /// The line's level (works for inputs, as-is lines and outputs).
    pub fn read(&self) -> Result<u8, GpioError> {
        let mut b = encode_values(0, 1);
        ioctl(self.fd.as_raw_fd(), GPIO_V2_LINE_GET_VALUES_IOCTL, &mut b, || format!("{} line {} value", self.chip, self.offset))?;
        Ok((decode_value_bits(&b) & 1) as u8)
    }

    /// Drive the line: reconfigure as an output at `value` (or just set the value if it
    /// already is one with the same pull). The line is never released in between.
    pub fn drive(&mut self, value: u8, pull: Option<Pull>) -> Result<(), GpioError> {
        let v = value & 1;
        if matches!(self.mode, Mode::Output(_)) && pull == self.pull {
            let mut b = encode_values(v as u64, 1);
            ioctl(self.fd.as_raw_fd(), GPIO_V2_LINE_SET_VALUES_IOCTL, &mut b, || format!("{} line {} set", self.chip, self.offset))?;
        } else {
            let mut c = [0u8; LINECONFIG_SIZE];
            encode_line_config(&mut c, flag::OUTPUT | Pull::flag(pull), Some((v as u64, 1)));
            ioctl(self.fd.as_raw_fd(), GPIO_V2_LINE_SET_CONFIG_IOCTL, &mut c, || format!("{} line {} config", self.chip, self.offset))?;
            self.pull = pull;
        }
        self.mode = Mode::Output(v);
        Ok(())
    }

    /// Reconfigure as an input with this pull (None: leave the bias alone).
    pub fn input(&mut self, pull: Option<Pull>) -> Result<(), GpioError> {
        let mut c = [0u8; LINECONFIG_SIZE];
        encode_line_config(&mut c, flag::INPUT | Pull::flag(pull), None);
        ioctl(self.fd.as_raw_fd(), GPIO_V2_LINE_SET_CONFIG_IOCTL, &mut c, || format!("{} line {} config", self.chip, self.offset))?;
        self.mode = Mode::Input;
        self.pull = pull;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // Raw gpio_v2_line_info buffers from GPIO_V2_GET_LINEINFO_IOCTL on desky (Pi 4, kernel
    // 6.18, 2026-10-19), trailing zeros trimmed. gpioinfo agreed with every one.
    fn desky(k: &str) -> [u8; LINEINFO_SIZE] {
        let h = match k {
            "0:2" => "4750494f3200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000050000".to_string(),
            "0:4" => "4750494f34000000000000000000000000000000000000000000000000000000".to_string()
                + "6f6e657769726540340000000000000000000000000000000000000000000000"
                + "040000000000000049",
            "0:17" => "4750494f3137000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001100000000000000040000".to_string(),
            "0:42" => "5354415455535f4c45445f475f434c4b00000000000000000000000000000000".to_string()
                + "4143540000000000000000000000000000000000000000000000000000000000"
                + "2a0000000000000009",
            _ => unreachable!(),
        };
        let mut b = [0u8; LINEINFO_SIZE];
        let v = hex(&h);
        b[..v.len()].copy_from_slice(&v);
        b
    }

    #[test]
    fn ioctl_numbers_match_the_kernels() {
        assert_eq!(GPIO_GET_CHIPINFO_IOCTL, 0x8044b401);
        assert_eq!(GPIO_V2_GET_LINEINFO_IOCTL, 0xc100b405);
        assert_eq!(GPIO_V2_GET_LINE_IOCTL, 0xc250b407);
        assert_eq!(GPIO_V2_LINE_SET_CONFIG_IOCTL, 0xc110b40d);
        assert_eq!(GPIO_V2_LINE_GET_VALUES_IOCTL, 0xc010b40e);
        assert_eq!(GPIO_V2_LINE_SET_VALUES_IOCTL, 0xc010b40f);
        assert_eq!([CHIPINFO_SIZE, LINEINFO_SIZE, LINECONFIG_SIZE, LINEREQ_SIZE], [68, 256, 272, 592]);
    }

    #[test]
    fn chip_info() {
        let mut b = [0u8; CHIPINFO_SIZE];
        b[..9].copy_from_slice(b"gpiochip0");
        b[32..47].copy_from_slice(b"pinctrl-bcm2711");
        b[64..68].copy_from_slice(&58u32.to_le_bytes());
        assert_eq!(decode_chip_info(&b), ChipInfo { name: "gpiochip0".into(), label: "pinctrl-bcm2711".into(), lines: 58 });
    }

    #[test]
    fn line_info_decodes_deskys_lines_like_gpioinfo() {
        let l = decode_line_info(&desky("0:17"));
        assert_eq!((l.offset, l.name.as_str(), l.consumer.clone(), l.used, l.direction), (17, "GPIO17", None, false, "input"));
        assert_eq!(l.flags, ["input"]);
        // Muxed to I²C1 with no consumer: "used", and shown as "kernel" like libgpiod does.
        let l = decode_line_info(&desky("0:2"));
        assert_eq!((l.offset, l.consumer.as_deref(), l.used), (2, Some("kernel"), true));
        assert_eq!(l.flags, ["used", "input"]);
        let l = decode_line_info(&desky("0:4"));
        assert_eq!((l.offset, l.consumer.as_deref(), l.direction, l.drive), (4, Some("onewire@4"), "output", "open-drain"));
        assert_eq!(l.flags, ["used", "output", "open-drain"]);
        let l = decode_line_info(&desky("0:42"));
        assert_eq!((l.offset, l.name.as_str(), l.consumer.as_deref(), l.direction), (42, "STATUS_LED_G_CLK", Some("ACT"), "output"));
    }

    #[test]
    fn bias_active_low_edges_and_debounce() {
        let mut b = [0u8; LINEINFO_SIZE];
        put_u32(&mut b, 64, 7);
        put_u32(&mut b, 68, 1); // one attribute
        put_u32(&mut b, 72, (flag::INPUT | flag::ACTIVE_LOW | flag::BIAS_PULL_UP | flag::EDGE_RISING | flag::EDGE_FALLING) as u32);
        put_u32(&mut b, 80, 3); // GPIO_V2_LINE_ATTR_ID_DEBOUNCE
        put_u32(&mut b, 88, 5000);
        let l = decode_line_info(&b);
        assert_eq!((l.offset, l.bias, l.active_low, l.edge, l.debounce_us), (7, Some("pull-up"), true, Some("both"), Some(5000)));
        assert_eq!(l.flags, ["input", "active-low", "pull-up", "edges=both", "debounce=5000us"]);
    }

    #[test]
    fn line_request_layout() {
        let r = encode_line_request(&[17, 27], "benchbob", 0, None).unwrap();
        assert_eq!(r.len(), 592);
        assert_eq!((u32_at(&r, 0), u32_at(&r, 4)), (17, 27));
        assert_eq!(&r[256..264], b"benchbob");
        assert_eq!(u64::from_le_bytes(r[288..296].try_into().unwrap()), 0); // as-is: no direction flag
        assert_eq!(u32_at(&r, 560), 2);
        assert_eq!(request_fd(&r), -1);
        assert!(encode_line_request(&[], "x", 0, None).is_err());
        // A consumer longer than the field is cut, keeping the NUL.
        assert_eq!(encode_line_request(&[1], &"x".repeat(40), 0, None).unwrap()[256 + 31], 0);
    }

    #[test]
    fn output_request_carries_direction_bias_and_the_initial_value() {
        let r = encode_line_request(&[26], "benchbob", flag::OUTPUT | flag::BIAS_DISABLED, Some((1, 1))).unwrap();
        assert_eq!(u64::from_le_bytes(r[288..296].try_into().unwrap()), flag::OUTPUT | flag::BIAS_DISABLED);
        assert_eq!(u32_at(&r, 296), 1); // config.num_attrs
        assert_eq!(u32_at(&r, 320), 2); // attrs[0].attr.id = OUTPUT_VALUES
        assert_eq!(u64::from_le_bytes(r[328..336].try_into().unwrap()), 1); // values
        assert_eq!(u64::from_le_bytes(r[336..344].try_into().unwrap()), 1); // mask
    }

    #[test]
    fn values() {
        let b = encode_values(0, 1);
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 1);
        let mut b = b;
        b[0] = 1;
        assert_eq!(decode_value_bits(&b), 1);
        assert_eq!(encode_line_info_req(42)[64], 42);
    }

    #[test]
    fn errors_name_the_errno() {
        assert_eq!(GpioError { what: "gpiochip0 line 4".into(), errno: 16 }.to_string(), "gpiochip0 line 4: EBUSY (the line is in use)");
        assert!(GpioError { what: "x".into(), errno: 13 }.to_string().contains("gpio group"));
    }

    /// The TS server took consumers from `gpioinfo`; this one decodes the uAPI itself. Both
    /// captures are from desky on the same day: the four lines with raw buffers must agree
    /// with what libgpiod printed for them.
    #[test]
    fn decoded_consumers_match_gpioinfo_on_desky() {
        let text = include_str!("../fixtures/gpioinfo-v2-desky.txt");
        let re = regex::Regex::new(r#"^\s*line\s+(\d+):\s+"([^"]*)"\s+(input|output)(.*)$"#).unwrap();
        let cons = regex::Regex::new(r#"consumer="([^"]*)""#).unwrap();
        let mut chip0 = std::collections::HashMap::new();
        let mut in_chip0 = false;
        for l in text.lines() {
            if l.starts_with("gpiochip") {
                in_chip0 = l.starts_with("gpiochip0 ");
                continue;
            }
            if let (true, Some(m)) = (in_chip0, re.captures(l)) {
                chip0.insert(
                    m[1].parse::<u32>().unwrap(),
                    (m[2].to_string(), m[3].to_string(), cons.captures(&m[4]).map(|c| c[1].to_string())),
                );
            }
        }
        assert_eq!(chip0.len(), 58);
        for k in ["0:2", "0:4", "0:17", "0:42"] {
            let l = decode_line_info(&desky(k));
            let (name, dir, consumer) = &chip0[&l.offset];
            assert_eq!((&l.name, l.direction, &l.consumer), (name, dir.as_str(), consumer), "line {}", l.offset);
        }
    }
}
