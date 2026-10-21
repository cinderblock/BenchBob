//! Serial port control with termios2 (lib/termios.ts): ports are plain fds opened
//! O_NONBLOCK and configured with ioctl() on a Linux `struct termios2`, which carries an
//! arbitrary baud rate (BOTHER) instead of the fixed Bnnn table.
//!
//! The struct is handled as its 44 raw bytes, as the TypeScript does, so encoding and
//! decoding are pure and testable anywhere. The constants are the asm-generic values shared
//! by every architecture BenchBob builds for (x86-64, arm64, armv7, armv6): checked against
//! the 6.18 kernel headers on desky.

use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use benchbob_core::http::{js_number, js_string};
use serde_json::{Value, json};

// ---------- Constants ----------

pub const TCGETS2: u32 = 0x802c_542a;
pub const TCSETS2: u32 = 0x402c_542b;
pub const TERMIOS2_SIZE: usize = 44; // iflag@0 oflag@4 cflag@8 lflag@12 line@16 cc[19]@17 ispeed@36 ospeed@40
const VTIME: usize = 5;
const VMIN: usize = 6;
const VSTART: usize = 8;
const VSTOP: usize = 9;

const CBAUD: u32 = 0x100f;
const BOTHER: u32 = 0x1000;
const CSIZE: u32 = 0x30;
const CSTOPB: u32 = 0x40;
const CREAD: u32 = 0x80;
const PARENB: u32 = 0x100;
const PARODD: u32 = 0x200;
const HUPCL: u32 = 0x400;
const CLOCAL: u32 = 0x800;
const CMSPAR: u32 = 0x4000_0000;
const CRTSCTS: u32 = 0x8000_0000;
const IXON: u32 = 0x400;
const IXOFF: u32 = 0x1000;
const INPCK: u32 = 0x10;

const TIOCMGET: u32 = 0x5415;
const TIOCMBIS: u32 = 0x5416;
const TIOCMBIC: u32 = 0x5417;
const TIOCSBRK: u32 = 0x5427;
const TIOCCBRK: u32 = 0x5428;
const TCFLSH: u32 = 0x540b;
const TIOCEXCL: u32 = 0x540c;
const TIOCNXCL: u32 = 0x540d;
pub const TIOCM_DTR: i32 = 0x002;
pub const TIOCM_RTS: i32 = 0x004;
pub const TIOCM_CTS: i32 = 0x020;
pub const TIOCM_CAR: i32 = 0x040;
pub const TIOCM_RNG: i32 = 0x080;
pub const TIOCM_DSR: i32 = 0x100;

/// The fixed-rate codes. Standard rates are written with these rather than BOTHER so tools
/// that read termios the old way (stty, pyserial, getty) see the right speed.
const BAUD_CODES: &[(u32, u32)] = &[
    (0, 0),
    (50, 1),
    (75, 2),
    (110, 3),
    (134, 4),
    (150, 5),
    (200, 6),
    (300, 7),
    (600, 8),
    (1200, 9),
    (1800, 10),
    (2400, 11),
    (4800, 12),
    (9600, 13),
    (19200, 14),
    (38400, 15),
    (57600, 0x1001),
    (115200, 0x1002),
    (230400, 0x1003),
    (460800, 0x1004),
    (500000, 0x1005),
    (576000, 0x1006),
    (921600, 0x1007),
    (1000000, 0x1008),
    (1152000, 0x1009),
    (1500000, 0x100a),
    (2000000, 0x100b),
    (2500000, 0x100c),
    (3000000, 0x100d),
    (3500000, 0x100e),
    (4000000, 0x100f),
];

fn baud_code(baud: u32) -> Option<u32> {
    BAUD_CODES.iter().find(|(b, _)| *b == baud).map(|(_, c)| *c)
}

fn code_baud(code: u32) -> Option<u32> {
    BAUD_CODES.iter().find(|(_, c)| *c == code).map(|(b, _)| *b)
}

// ---------- Config <-> termios2 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parity {
    None,
    Even,
    Odd,
    Mark,
    Space,
}

impl Parity {
    pub fn as_str(self) -> &'static str {
        match self {
            Parity::None => "none",
            Parity::Even => "even",
            Parity::Odd => "odd",
            Parity::Mark => "mark",
            Parity::Space => "space",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    None,
    RtsCts,
    XonXoff,
}

impl Flow {
    pub fn as_str(self) -> &'static str {
        match self {
            Flow::None => "none",
            Flow::RtsCts => "rtscts",
            Flow::XonXoff => "xonxoff",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialConfig {
    pub baud: u32,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: u8,
    pub flow: Flow,
}

pub const DEFAULT_CONFIG: SerialConfig =
    SerialConfig { baud: 115200, data_bits: 8, parity: Parity::None, stop_bits: 1, flow: Flow::None };

impl Default for SerialConfig {
    fn default() -> Self {
        DEFAULT_CONFIG
    }
}

impl SerialConfig {
    /// `{ baud, dataBits, parity, stopBits, flow }`, in the TypeScript's key order.
    pub fn to_json(&self) -> Value {
        json!({ "baud": self.baud, "dataBits": self.data_bits, "parity": self.parity.as_str(), "stopBits": self.stop_bits, "flow": self.flow.as_str() })
    }
}

fn present(c: &Value, k: &str) -> Option<Value> {
    c.get(k).filter(|v| !v.is_null()).cloned()
}

/// Validate and fill a partial config from the API (`normalizeConfig`). The error is the
/// readable message the API answers 400 with.
pub fn normalize_config(c: &Value, base: &SerialConfig) -> Result<SerialConfig, String> {
    let mut out = *base;
    if let Some(v) = present(c, "baud") {
        let b = js_number(&v);
        if !(b.fract() == 0.0 && (50.0..=20_000_000.0).contains(&b)) {
            return Err(format!("Invalid baud rate {}", js_string(&v)));
        }
        out.baud = b as u32;
    }
    if let Some(v) = present(c, "dataBits") {
        let d = js_number(&v);
        if ![5.0, 6.0, 7.0, 8.0].contains(&d) {
            return Err(format!("Data bits must be 5-8, not {}", js_string(&v)));
        }
        out.data_bits = d as u8;
    }
    if let Some(v) = present(c, "parity") {
        let p = js_string(&v).to_lowercase();
        out.parity = match p.as_str() {
            "n" | "none" => Parity::None,
            "e" | "even" => Parity::Even,
            "o" | "odd" => Parity::Odd,
            "m" | "mark" => Parity::Mark,
            "s" | "space" => Parity::Space,
            _ => return Err(format!("Parity must be none/even/odd/mark/space, not {}", js_string(&v))),
        };
    }
    if let Some(v) = present(c, "stopBits") {
        let s = js_number(&v);
        if s != 1.0 && s != 2.0 {
            return Err(format!("Stop bits must be 1 or 2, not {}", js_string(&v)));
        }
        out.stop_bits = s as u8;
    }
    if let Some(v) = present(c, "flow") {
        let f: String = js_string(&v).to_lowercase().chars().filter(|c| c.is_ascii_lowercase()).collect();
        out.flow = match f.as_str() {
            "none" | "off" => Flow::None,
            "rtscts" | "hw" | "hardware" => Flow::RtsCts,
            "xonxoff" | "sw" | "software" => Flow::XonXoff,
            _ => return Err(format!("Flow control must be none/rtscts/xonxoff, not {}", js_string(&v))),
        };
    }
    Ok(out)
}

/// "115200 8N1", with flow control appended when on.
pub fn config_label(c: &SerialConfig) -> String {
    let flow = match c.flow {
        Flow::None => "",
        Flow::RtsCts => " RTS/CTS",
        Flow::XonXoff => " XON/XOFF",
    };
    format!("{} {}{}{}{flow}", c.baud, c.data_bits, c.parity.as_str()[..1].to_uppercase(), c.stop_bits)
}

fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn set_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Build a raw-mode termios2 for `cfg`. `base` (from TCGETS2) keeps c_line and HUPCL as the
/// driver left them.
pub fn encode_termios(cfg: &SerialConfig, base: Option<&[u8]>) -> [u8; TERMIOS2_SIZE] {
    let mut buf = [0u8; TERMIOS2_SIZE];
    if let Some(b) = base {
        let n = b.len().min(TERMIOS2_SIZE);
        buf[..n].copy_from_slice(&b[..n]);
    }
    let old = get_u32(&buf, 8);

    // Raw: no line editing, no CR/LF translation, no signals. Bytes in, bytes out.
    let mut iflag = 0;
    if cfg.flow == Flow::XonXoff {
        iflag |= IXON | IXOFF;
    }
    // With INPCK (and neither IGNPAR nor PARMRK) a byte with a parity error reads as NUL,
    // which is what auto-baud and a human both expect to see on a mismatched link.
    if cfg.parity != Parity::None {
        iflag |= INPCK;
    }

    let mut cflag = (old & HUPCL) | CREAD | CLOCAL;
    cflag |= match cfg.data_bits {
        5 => 0x00,
        6 => 0x10,
        7 => 0x20,
        _ => 0x30,
    };
    if cfg.stop_bits == 2 {
        cflag |= CSTOPB;
    }
    if cfg.parity != Parity::None {
        cflag |= PARENB;
    }
    if matches!(cfg.parity, Parity::Odd | Parity::Mark) {
        cflag |= PARODD;
    }
    if matches!(cfg.parity, Parity::Mark | Parity::Space) {
        cflag |= CMSPAR;
    }
    if cfg.flow == Flow::RtsCts {
        cflag |= CRTSCTS;
    }
    cflag |= baud_code(cfg.baud).unwrap_or(BOTHER); // CIBAUD left 0: input speed follows output

    set_u32(&mut buf, 0, iflag);
    set_u32(&mut buf, 4, 0);
    set_u32(&mut buf, 8, cflag);
    set_u32(&mut buf, 12, 0);
    buf[17 + VMIN] = 1;
    buf[17 + VTIME] = 0;
    buf[17 + VSTART] = 0x11;
    buf[17 + VSTOP] = 0x13;
    set_u32(&mut buf, 36, cfg.baud);
    set_u32(&mut buf, 40, cfg.baud);
    buf
}

pub fn decode_termios(buf: &[u8]) -> SerialConfig {
    let iflag = get_u32(buf, 0);
    let cflag = get_u32(buf, 8);
    let code = cflag & CBAUD;
    let baud = if code == BOTHER { get_u32(buf, 40) } else { code_baud(code).unwrap_or_else(|| get_u32(buf, 40)) };
    let data_bits = match cflag & CSIZE {
        0x00 => 5,
        0x10 => 6,
        0x20 => 7,
        _ => 8,
    };
    let parity = if cflag & PARENB != 0 {
        let odd = cflag & PARODD != 0;
        if cflag & CMSPAR != 0 {
            if odd { Parity::Mark } else { Parity::Space }
        } else if odd {
            Parity::Odd
        } else {
            Parity::Even
        }
    } else {
        Parity::None
    };
    let flow = if cflag & CRTSCTS != 0 {
        Flow::RtsCts
    } else if iflag & IXON != 0 {
        Flow::XonXoff
    } else {
        Flow::None
    };
    SerialConfig { baud, data_bits, parity, stop_bits: if cflag & CSTOPB != 0 { 2 } else { 1 }, flow }
}

// ---------- errors ----------

/// The errno names the TypeScript's TtyError knows; others read "errno N".
fn tty_errno_name(e: i32) -> Option<&'static str> {
    Some(match e {
        1 => "EPERM",
        5 => "EIO",
        9 => "EBADF",
        11 => "EAGAIN",
        13 => "EACCES",
        16 => "EBUSY",
        19 => "ENODEV",
        22 => "EINVAL",
        25 => "ENOTTY",
        _ => return None,
    })
}

/// An errno's symbolic name as Node reports it in `e.code` (open, listen, write).
pub fn errno_name(e: i32) -> String {
    let s = match e {
        1 => "EPERM",
        2 => "ENOENT",
        4 => "EINTR",
        5 => "EIO",
        6 => "ENXIO",
        9 => "EBADF",
        11 => "EAGAIN",
        12 => "ENOMEM",
        13 => "EACCES",
        16 => "EBUSY",
        17 => "EEXIST",
        19 => "ENODEV",
        20 => "ENOTDIR",
        21 => "EISDIR",
        22 => "EINVAL",
        24 => "EMFILE",
        25 => "ENOTTY",
        28 => "ENOSPC",
        30 => "EROFS",
        32 => "EPIPE",
        98 => "EADDRINUSE",
        99 => "EADDRNOTAVAIL",
        104 => "ECONNRESET",
        110 => "ETIMEDOUT",
        111 => "ECONNREFUSED",
        _ => return format!("E{e}"),
    };
    s.to_string()
}

/// An io::Error as Node's `e.code || e.message`.
pub fn io_code(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(n) => errno_name(n),
        None => e.to_string(),
    }
}

#[derive(Debug, Clone)]
pub struct TtyError {
    pub op: &'static str,
    pub errno: i32,
}

impl TtyError {
    pub fn code(&self) -> String {
        tty_errno_name(self.errno).map(String::from).unwrap_or_else(|| format!("E{}", self.errno))
    }
}

impl std::fmt::Display for TtyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match tty_errno_name(self.errno) {
            Some(n) => write!(f, "{} failed: {n}", self.op),
            None => write!(f, "{} failed: errno {}", self.op, self.errno),
        }
    }
}

impl std::error::Error for TtyError {}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn check(op: &'static str, rc: libc::c_int) -> Result<(), TtyError> {
    if rc < 0 { Err(TtyError { op, errno: errno() }) } else { Ok(()) }
}

// The request argument is c_ulong on glibc and c_int on musl: `as _` fits either.
fn ioctl_ptr(fd: RawFd, req: u32, arg: *mut libc::c_void) -> libc::c_int {
    // SAFETY: every caller passes a buffer of the size the request expects.
    unsafe { libc::ioctl(fd, req as _, arg) }
}

fn ioctl_int(fd: RawFd, req: u32, arg: libc::c_int) -> libc::c_int {
    // SAFETY: requests that take an integer argument (or none).
    unsafe { libc::ioctl(fd, req as _, arg as libc::c_ulong) }
}

// ---------- Port operations ----------

/// Open a tty O_NONBLOCK|O_NOCTTY. `read_only` for ports we must not drive (a console).
pub fn open_port(path: &str, read_only: bool) -> std::io::Result<OwnedFd> {
    let c = CString::new(path).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let flags = if read_only { libc::O_RDONLY } else { libc::O_RDWR } | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: a valid C string; the fd is owned by the returned OwnedFd.
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh fd nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn get_termios(fd: RawFd) -> Result<[u8; TERMIOS2_SIZE], TtyError> {
    let mut buf = [0u8; TERMIOS2_SIZE];
    check("TCGETS2", ioctl_ptr(fd, TCGETS2, buf.as_mut_ptr().cast()))?;
    Ok(buf)
}

/// Apply `cfg` in raw mode; returns what the driver actually accepted (read back).
pub fn configure(fd: RawFd, cfg: &SerialConfig) -> Result<SerialConfig, TtyError> {
    let base = get_termios(fd)?;
    let mut t = encode_termios(cfg, Some(&base));
    check("TCSETS2", ioctl_ptr(fd, TCSETS2, t.as_mut_ptr().cast()))?;
    Ok(decode_termios(&get_termios(fd)?))
}

/// Refuse further opens of this tty by other (non-root) processes while we own it.
pub fn set_exclusive(fd: RawFd) -> Result<(), TtyError> {
    check("TIOCEXCL", ioctl_int(fd, TIOCEXCL, 0))
}

/// Undo set_exclusive. The flag lives on the tty, not the fd, and only resets when the last
/// fd closes; if anything else holds the tty (a pty's other end, a stray `cat`), it would
/// outlive our close and lock out the next opener, esptool or ourselves included.
pub fn clear_exclusive(fd: RawFd) -> Result<(), TtyError> {
    check("TIOCNXCL", ioctl_int(fd, TIOCNXCL, 0))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModemLines {
    pub dtr: bool,
    pub rts: bool,
    pub cts: bool,
    pub dsr: bool,
    pub dcd: bool,
    pub ri: bool,
}

impl ModemLines {
    pub fn from_bits(m: i32) -> Self {
        Self {
            dtr: m & TIOCM_DTR != 0,
            rts: m & TIOCM_RTS != 0,
            cts: m & TIOCM_CTS != 0,
            dsr: m & TIOCM_DSR != 0,
            dcd: m & TIOCM_CAR != 0,
            ri: m & TIOCM_RNG != 0,
        }
    }
    pub fn to_json(&self) -> Value {
        json!({ "dtr": self.dtr, "rts": self.rts, "cts": self.cts, "dsr": self.dsr, "dcd": self.dcd, "ri": self.ri })
    }
}

/// TIOCMGET. Fails (ENOTTY/EINVAL) on ttys without modem lines, e.g. ptys.
pub fn get_modem(fd: RawFd) -> Result<ModemLines, TtyError> {
    let mut v: libc::c_int = 0;
    check("TIOCMGET", ioctl_ptr(fd, TIOCMGET, (&mut v as *mut libc::c_int).cast()))?;
    Ok(ModemLines::from_bits(v))
}

/// Assert (true) or drop (false) DTR and/or RTS; None leaves a line alone.
pub fn set_modem(fd: RawFd, dtr: Option<bool>, rts: Option<bool>) -> Result<(), TtyError> {
    let (mut on, mut off) = (0, 0);
    match dtr {
        Some(true) => on |= TIOCM_DTR,
        Some(false) => off |= TIOCM_DTR,
        None => {}
    }
    match rts {
        Some(true) => on |= TIOCM_RTS,
        Some(false) => off |= TIOCM_RTS,
        None => {}
    }
    if on != 0 {
        let mut v = on;
        check("TIOCMBIS", ioctl_ptr(fd, TIOCMBIS, (&mut v as *mut libc::c_int).cast()))?;
    }
    if off != 0 {
        let mut v = off;
        check("TIOCMBIC", ioctl_ptr(fd, TIOCMBIC, (&mut v as *mut libc::c_int).cast()))?;
    }
    Ok(())
}

pub fn set_break(fd: RawFd, on: bool) -> Result<(), TtyError> {
    if on { check("TIOCSBRK", ioctl_int(fd, TIOCSBRK, 0)) } else { check("TIOCCBRK", ioctl_int(fd, TIOCCBRK, 0)) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Queue {
    Rx,
    Tx,
    Both,
}

/// Discard queued data: received-unread, written-unsent, or both.
pub fn flush(fd: RawFd, which: Queue) -> Result<(), TtyError> {
    check(
        "TCFLSH",
        ioctl_int(
            fd,
            TCFLSH,
            match which {
                Queue::Rx => 0,
                Queue::Tx => 1,
                Queue::Both => 2,
            },
        ),
    )
}

/// Non-blocking write: bytes written (0 when the kernel buffer is full).
pub fn write_some(fd: RawFd, data: &[u8]) -> std::io::Result<usize> {
    // SAFETY: data is a valid buffer of data.len() bytes.
    let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(0);
        }
        return Err(e);
    }
    Ok(n as usize)
}

/// Non-blocking read: Ok(None) when nothing is waiting, Ok(Some(0)) at end of file.
pub fn read_some(fd: RawFd, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
    // SAFETY: buf is a valid, writable buffer of buf.len() bytes.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(e);
    }
    Ok(Some(n as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32at(b: &[u8], off: usize) -> u32 {
        get_u32(b, off)
    }

    #[test]
    fn standard_rate_uses_the_bnnn_code_not_bother() {
        let b = encode_termios(&DEFAULT_CONFIG, None);
        assert_eq!(b.len(), 44);
        let cflag = u32at(&b, 8);
        assert_eq!(cflag & 0x100f, 0x1002); // B115200
        assert_eq!(cflag & 0x30, 0x30); // CS8
        assert_eq!(cflag & 0x80, 0x80); // CREAD
        assert_eq!(cflag & 0x800, 0x800); // CLOCAL
        assert_eq!(u32at(&b, 0), 0); // raw iflag
        assert_eq!(u32at(&b, 12), 0); // raw lflag
        assert_eq!(b[17 + 6], 1); // VMIN
        assert_eq!(b[17 + 5], 0); // VTIME
        assert_eq!(u32at(&b, 36), 115200);
    }

    #[test]
    fn odd_rate_uses_bother_with_the_speed_fields() {
        let b = encode_termios(&SerialConfig { baud: 74880, ..DEFAULT_CONFIG }, None);
        assert_eq!(u32at(&b, 8) & 0x100f, 0x1000);
        assert_eq!(u32at(&b, 40), 74880);
        assert_eq!(decode_termios(&b).baud, 74880);
    }

    #[test]
    fn round_trip_of_framing_and_flow() {
        let cases = [
            SerialConfig { baud: 9600, data_bits: 7, parity: Parity::Even, stop_bits: 2, flow: Flow::None },
            SerialConfig { baud: 250000, data_bits: 8, parity: Parity::Odd, stop_bits: 1, flow: Flow::RtsCts },
            SerialConfig { baud: 1200, data_bits: 5, parity: Parity::Mark, stop_bits: 1, flow: Flow::XonXoff },
            SerialConfig { baud: 3000000, data_bits: 6, parity: Parity::Space, stop_bits: 2, flow: Flow::None },
        ];
        for c in cases {
            assert_eq!(decode_termios(&encode_termios(&c, None)), c);
        }
    }

    #[test]
    fn mark_space_set_cmspar_rtscts_sets_the_top_bit() {
        let m = u32at(&encode_termios(&SerialConfig { parity: Parity::Mark, ..DEFAULT_CONFIG }, None), 8);
        assert_eq!(m & 0x4000_0000, 0x4000_0000);
        assert_eq!(m & 0x300, 0x300); // PARENB|PARODD
        let r = u32at(&encode_termios(&SerialConfig { flow: Flow::RtsCts, ..DEFAULT_CONFIG }, None), 8);
        assert_eq!(r >> 31, 1);
    }

    #[test]
    fn base_termios_keeps_hupcl_and_c_line() {
        let mut base = [0u8; 44];
        set_u32(&mut base, 8, 0x400 | 0x0f); // HUPCL + B38400
        base[16] = 0; // N_TTY
        let b = encode_termios(&DEFAULT_CONFIG, Some(&base));
        assert_eq!(u32at(&b, 8) & 0x400, 0x400);
    }

    #[test]
    fn normalize_config_validates_and_accepts_shorthands() {
        let c = normalize_config(&json!({ "baud": "57600", "parity": "E", "flow": "RTS/CTS" }), &DEFAULT_CONFIG).unwrap();
        assert_eq!(c, SerialConfig { baud: 57600, data_bits: 8, parity: Parity::Even, stop_bits: 1, flow: Flow::RtsCts });
        assert!(normalize_config(&json!({ "baud": 12.5 }), &DEFAULT_CONFIG).unwrap_err().contains("baud"));
        assert!(normalize_config(&json!({ "dataBits": 9 }), &DEFAULT_CONFIG).unwrap_err().contains("Data bits"));
        assert!(normalize_config(&json!({ "stopBits": 1.5 }), &DEFAULT_CONFIG).unwrap_err().contains("Stop bits"));
        assert!(normalize_config(&json!({ "parity": "x" }), &DEFAULT_CONFIG).unwrap_err().contains("Parity"));
        assert_eq!(
            config_label(&SerialConfig { baud: 9600, data_bits: 7, parity: Parity::Even, stop_bits: 1, flow: Flow::XonXoff }),
            "9600 7E1 XON/XOFF"
        );
        // The messages print the value as JavaScript would.
        assert_eq!(normalize_config(&json!({ "baud": 12.5 }), &DEFAULT_CONFIG).unwrap_err(), "Invalid baud rate 12.5");
        assert_eq!(normalize_config(&json!({ "dataBits": "9" }), &DEFAULT_CONFIG).unwrap_err(), "Data bits must be 5-8, not 9");
        // null means "not given".
        assert_eq!(normalize_config(&json!({ "baud": null }), &DEFAULT_CONFIG).unwrap(), DEFAULT_CONFIG);
    }

    #[test]
    fn config_json_keeps_the_key_order() {
        assert_eq!(
            serde_json::to_string(&DEFAULT_CONFIG.to_json()).unwrap(),
            r#"{"baud":115200,"dataBits":8,"parity":"none","stopBits":1,"flow":"none"}"#
        );
    }

    #[test]
    fn tty_errors_read_like_the_typescript() {
        assert_eq!(TtyError { op: "TIOCMGET", errno: 25 }.to_string(), "TIOCMGET failed: ENOTTY");
        assert_eq!(TtyError { op: "TCSETS2", errno: 71 }.to_string(), "TCSETS2 failed: errno 71");
        assert_eq!(TtyError { op: "TCSETS2", errno: 71 }.code(), "E71");
        assert_eq!(errno_name(98), "EADDRINUSE");
    }
}
