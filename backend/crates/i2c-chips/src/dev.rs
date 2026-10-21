//! One address on one bus: thin wrappers plus SMBus fallbacks (i2c-chips.ts `Dev`).

use alloc::vec::Vec;

use crate::io::{I2cError, I2cIo, Msg, errno, func};
use crate::num::s16;

pub struct Dev<'a> {
    pub io: &'a mut dyn I2cIo,
    pub addr: u8,
}

impl<'a> Dev<'a> {
    pub fn new(io: &'a mut dyn I2cIo, addr: u8) -> Self {
        Self { io, addr }
    }
    /// A plain-I²C adapter (combined transactions, arbitrary lengths).
    pub fn plain(&self) -> bool {
        self.io.funcs() & func::I2C != 0
    }
    pub fn u8(&mut self, reg: u8) -> Result<u8, I2cError> {
        self.io.read_byte(self.addr, reg, false)
    }
    /// 16-bit big-endian register (TI/most sensors) in one transaction.
    pub fn u16be(&mut self, reg: u8) -> Result<u16, I2cError> {
        Ok(self.io.read_word(self.addr, reg, false)?.swap_bytes())
    }
    pub fn i16be(&mut self, reg: u8) -> Result<i32, I2cError> {
        Ok(s16(self.u16be(reg)?))
    }
    /// 16-bit little-endian register (SMBus native).
    pub fn u16le(&mut self, reg: u8) -> Result<u16, I2cError> {
        self.io.read_word(self.addr, reg, false)
    }
    /// Consecutive registers, chunked to the 32-byte SMBus I2C-block limit.
    pub fn block(&mut self, reg: u8, n: usize) -> Result<Vec<u8>, I2cError> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            let want = (n - out.len()).min(32);
            let got = self.io.read_block(self.addr, reg.wrapping_add(out.len() as u8), want, false)?;
            if got.is_empty() {
                return Err(I2cError::new(errno::EIO, "short block read"));
            }
            out.extend(got);
        }
        out.truncate(n);
        Ok(out)
    }
    pub fn recv(&mut self) -> Result<u8, I2cError> {
        self.io.receive(self.addr, false)
    }
    /// Pure read (no pointer write). SMBus-only adapters fall back to repeated receive-byte.
    pub fn read(&mut self, n: usize) -> Result<Vec<u8>, I2cError> {
        if self.plain() {
            return Ok(self.io.transfer(self.addr, &[Msg::R(n)], false)?.pop().unwrap_or_default());
        }
        (0..n).map(|_| self.io.receive(self.addr, false)).collect()
    }
    /// Write raw bytes (one transaction).
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), I2cError> {
        if self.plain() {
            return self.io.transfer(self.addr, &[Msg::W(bytes.to_vec())], false).map(|_| ());
        }
        match bytes {
            [a] => self.io.send(self.addr, *a, false),
            [r, v] => self.io.write_byte(self.addr, *r, *v, false),
            [r, lo, hi] => self.io.write_word(self.addr, *r, *lo as u16 | (*hi as u16) << 8, false),
            [r, rest @ ..] => self.io.write_block(self.addr, *r, rest, false),
            [] => self.io.transfer(self.addr, &[Msg::W(Vec::new())], false).map(|_| ()),
        }
    }
    /// Write then read with a repeated start.
    pub fn write_read(&mut self, bytes: &[u8], n: usize) -> Result<Vec<u8>, I2cError> {
        if self.plain() {
            return Ok(self.io.transfer(self.addr, &[Msg::W(bytes.to_vec()), Msg::R(n)], false)?.pop().unwrap_or_default());
        }
        if let [r] = bytes {
            return self.block(*r, n);
        }
        Err(I2cError::new(errno::EOPNOTSUPP, "write-then-read needs a plain-I²C adapter"))
    }
    pub fn wb(&mut self, reg: u8, v: u8) -> Result<(), I2cError> {
        self.io.write_byte(self.addr, reg, v, false)
    }
    pub fn write_word(&mut self, reg: u8, v: u16) -> Result<(), I2cError> {
        self.io.write_word(self.addr, reg, v, false)
    }
    pub fn sleep(&mut self, ms: f64) {
        if ms > 0.0 {
            self.io.sleep_ms(libm::ceil(ms) as u32);
        }
    }
}
