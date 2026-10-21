//! The bus interface every chip probe and decoder speaks (i2c-dev.ts `I2cIo`), the errors a
//! bus raises, and the I2C_FUNCS bits. The server implements it over /dev/i2c-N ioctls; a
//! firmware node can implement it over its own I²C peripheral; [`crate::fake::FakeBus`]
//! implements it for tests.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// I2C_FUNCS bits (<linux/i2c.h>).
pub mod func {
    pub const I2C: u32 = 0x1;
    pub const TEN_BIT: u32 = 0x2;
    pub const PEC: u32 = 0x8;
    pub const QUICK: u32 = 0x10000;
    pub const READ_BYTE: u32 = 0x20000;
    pub const WRITE_BYTE: u32 = 0x40000;
    pub const READ_BYTE_DATA: u32 = 0x80000;
    pub const WRITE_BYTE_DATA: u32 = 0x100000;
    pub const READ_WORD_DATA: u32 = 0x200000;
    pub const WRITE_WORD_DATA: u32 = 0x400000;
    pub const READ_BLOCK_DATA: u32 = 0x1000000;
    pub const READ_I2C_BLOCK: u32 = 0x4000000;
    pub const WRITE_I2C_BLOCK: u32 = 0x8000000;
}

/// Human summary of an I2C_FUNCS mask.
pub fn func_names(f: u32) -> Vec<&'static str> {
    let mut out = Vec::new();
    for (bit, name) in [
        (func::I2C, "I2C"),
        (func::QUICK, "quick"),
        (func::READ_BYTE_DATA, "byte"),
        (func::READ_WORD_DATA, "word"),
        (func::READ_I2C_BLOCK, "i2c-block"),
        (func::READ_BLOCK_DATA, "smbus-block"),
        (func::PEC, "PEC"),
        (func::TEN_BIT, "10-bit"),
    ] {
        if f & bit != 0 {
            out.push(name);
        }
    }
    out
}

/// Errno values the bus code cares about (Linux numbering, which firmware ports reuse).
pub mod errno {
    pub const EIO: i32 = 5;
    pub const ENXIO: i32 = 6;
    pub const EAGAIN: i32 = 11;
    pub const EBUSY: i32 = 16;
    pub const EINVAL: i32 = 22;
    pub const EOPNOTSUPP: i32 = 95;
    pub const ETIMEDOUT: i32 = 110;
    pub const EREMOTEIO: i32 = 121;
}

fn errno_name(e: i32) -> Option<&'static str> {
    Some(match e {
        1 => "EPERM",
        5 => "EIO",
        6 => "ENXIO",
        11 => "EAGAIN",
        13 => "EACCES",
        16 => "EBUSY",
        19 => "ENODEV",
        22 => "EINVAL",
        71 => "EPROTO",
        95 => "EOPNOTSUPP",
        110 => "ETIMEDOUT",
        121 => "EREMOTEIO",
        _ => return None,
    })
}

fn errno_text(e: i32) -> Option<&'static str> {
    Some(match e {
        6 => "no device answered (NACK)",
        121 => "no ACK from the device",
        110 => "bus timeout (clock held low or device stretching too long)",
        11 => "arbitration lost / bus busy",
        16 => "address is claimed by a kernel driver",
        95 => "the adapter doesn't support this transfer type",
        5 => "I/O error",
        71 => "protocol error",
        22 => "invalid argument",
        13 => "permission denied (is the user in the i2c group?)",
        _ => return None,
    })
}

/// A failed bus operation: the errno and what was being done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct I2cError {
    pub errno: i32,
    pub op: String,
}

impl I2cError {
    pub fn new(errno: i32, op: impl Into<String>) -> Self {
        Self { errno, op: op.into() }
    }
    /// "read 0x76[0xD0]: no ACK from the device (EREMOTEIO)"
    pub fn message(&self) -> String {
        let name = match errno_name(self.errno) {
            Some(n) => String::from(n),
            None => format!("errno {}", self.errno),
        };
        format!("{}: {} ({name})", self.op, errno_text(self.errno).unwrap_or("error"))
    }
    /// True for "nobody is there", as opposed to a bus or adapter fault.
    pub fn nack(&self) -> bool {
        self.errno == errno::EREMOTEIO || self.errno == errno::ENXIO
    }
}

impl core::fmt::Display for I2cError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.message())
    }
}

/// One message of a combined transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    W(Vec<u8>),
    R(usize),
}

/// One bus. Every method is blocking; errors carry the errno. `force` reaches an address a
/// kernel driver owns (I2C_SLAVE_FORCE); without it such an address fails with EBUSY.
pub trait I2cIo {
    fn funcs(&self) -> u32;
    fn quick(&mut self, addr: u8, read: bool, force: bool) -> Result<(), I2cError>;
    fn receive(&mut self, addr: u8, force: bool) -> Result<u8, I2cError>;
    fn send(&mut self, addr: u8, v: u8, force: bool) -> Result<(), I2cError>;
    fn read_byte(&mut self, addr: u8, reg: u8, force: bool) -> Result<u8, I2cError>;
    fn write_byte(&mut self, addr: u8, reg: u8, v: u8, force: bool) -> Result<(), I2cError>;
    /// SMBus word: low byte first on the wire.
    fn read_word(&mut self, addr: u8, reg: u8, force: bool) -> Result<u16, I2cError>;
    fn write_word(&mut self, addr: u8, reg: u8, v: u16, force: bool) -> Result<(), I2cError>;
    /// SMBus "I2C block": write reg, repeated start, read len (≤32) bytes.
    fn read_block(&mut self, addr: u8, reg: u8, len: usize, force: bool) -> Result<Vec<u8>, I2cError>;
    fn write_block(&mut self, addr: u8, reg: u8, data: &[u8], force: bool) -> Result<(), I2cError>;
    /// Combined plain-I²C transaction (repeated starts). Needs `func::I2C`. Returns the read buffers.
    fn transfer(&mut self, addr: u8, msgs: &[Msg], force: bool) -> Result<Vec<Vec<u8>>, I2cError>;
    /// Wait between the steps of a sequence (start a conversion, then read it).
    fn sleep_ms(&mut self, ms: u32);
    /// Wall-clock time, ms since the Unix epoch (RTC drift readings).
    fn now_ms(&self) -> i64;
}

/// "0x76": hex, upper case, at least two digits.
pub fn hex(n: u32) -> String {
    format!("0x{n:02X}")
}
