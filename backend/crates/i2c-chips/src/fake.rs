//! A byte-level bus simulator for tests (i2c-dev.ts `FakeBus`, `RegChip`, `WordChip`):
//! identification and decoding run against it with no hardware.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;

use crate::io::{I2cError, I2cIo, Msg, errno, func, hex};

/// `Any` access for the chips a [`FakeBus`] owns, so tests can look at them afterwards.
pub trait AsAny: Any {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Any> AsAny for T {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// A chip on the fake bus, modelled at the byte level the way real silicon sees it.
pub trait FakeChip: AsAny + Send {
    /// Bytes written in one transaction (after the address byte). Err to NACK.
    fn write(&mut self, _bytes: &[u8]) -> Result<(), I2cError> {
        Err(I2cError::new(errno::EREMOTEIO, "fake"))
    }
    /// Bytes read in one transaction.
    fn read(&mut self, _n: usize) -> Result<Vec<u8>, I2cError> {
        Err(I2cError::new(errno::EREMOTEIO, "fake"))
    }
}

/// Register-file chip: the first written byte sets the pointer, further bytes are stored;
/// reads auto-increment.
pub struct RegChip {
    pub ptr: usize,
    pub regs: Vec<u8>,
    pub writes: Vec<Vec<u8>>,
    pub auto_inc: bool,
    pub read_only: bool,
}

impl RegChip {
    pub fn new(init: impl IntoIterator<Item = (u8, u8)>) -> Self {
        let mut regs = vec![0; 256];
        for (k, v) in init {
            regs[k as usize] = v;
        }
        Self { ptr: 0, regs, writes: Vec::new(), auto_inc: true, read_only: false }
    }
    /// Registers from a list (index = register).
    pub fn from_list(list: &[u8]) -> Self {
        Self::new(list.iter().enumerate().map(|(i, v)| (i as u8, *v)))
    }
}

impl FakeChip for RegChip {
    fn write(&mut self, bytes: &[u8]) -> Result<(), I2cError> {
        self.writes.push(bytes.to_vec());
        let Some((&p, rest)) = bytes.split_first() else { return Ok(()) };
        self.ptr = p as usize % self.regs.len();
        if self.read_only {
            return Ok(());
        }
        for &b in rest {
            self.regs[self.ptr] = b;
            if self.auto_inc {
                self.ptr = (self.ptr + 1) % self.regs.len();
            }
        }
        Ok(())
    }
    fn read(&mut self, n: usize) -> Result<Vec<u8>, I2cError> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.regs[self.ptr]);
            if self.auto_inc {
                self.ptr = (self.ptr + 1) % self.regs.len();
            }
        }
        Ok(out)
    }
}

/// Word-register chip (TI INA/TMP/ADS style): 1-byte pointer, 16-bit big-endian registers.
pub struct WordChip {
    pub ptr: u8,
    pub regs: BTreeMap<u8, u16>,
    pub writes: Vec<Vec<u8>>,
    pub nack_unknown: bool,
}

impl WordChip {
    pub fn new(regs: impl IntoIterator<Item = (u8, u16)>) -> Self {
        Self { ptr: 0, regs: regs.into_iter().collect(), writes: Vec::new(), nack_unknown: false }
    }
}

impl FakeChip for WordChip {
    fn write(&mut self, bytes: &[u8]) -> Result<(), I2cError> {
        self.writes.push(bytes.to_vec());
        let Some(&p) = bytes.first() else { return Ok(()) };
        if self.nack_unknown && !self.regs.contains_key(&p) {
            return Err(I2cError::new(errno::EREMOTEIO, "fake"));
        }
        self.ptr = p;
        if bytes.len() >= 3 {
            self.regs.insert(p, (bytes[1] as u16) << 8 | bytes[2] as u16);
        }
        Ok(())
    }
    fn read(&mut self, n: usize) -> Result<Vec<u8>, I2cError> {
        let v = self.regs.get(&self.ptr).copied().unwrap_or(0xffff);
        Ok((0..n).map(|i| if i % 2 == 0 { (v >> 8) as u8 } else { v as u8 }).collect())
    }
}

type ReadFn = Box<dyn FnMut(usize) -> Vec<u8> + Send>;

/// A chip made of closures: `read` answers reads (None NACKs them); writes are logged and
/// accepted when `accept_writes`, else NACKed.
pub struct FnChip {
    pub read: Option<ReadFn>,
    pub accept_writes: bool,
    pub writes: Vec<Vec<u8>>,
}

impl FnChip {
    pub fn reads(f: impl FnMut(usize) -> Vec<u8> + Send + 'static) -> Self {
        Self { read: Some(Box::new(f)), accept_writes: true, writes: Vec::new() }
    }
    /// Reads only: writes NACK.
    pub fn read_only(f: impl FnMut(usize) -> Vec<u8> + Send + 'static) -> Self {
        Self { read: Some(Box::new(f)), accept_writes: false, writes: Vec::new() }
    }
}

impl FakeChip for FnChip {
    fn write(&mut self, bytes: &[u8]) -> Result<(), I2cError> {
        self.writes.push(bytes.to_vec());
        if self.accept_writes { Ok(()) } else { Err(I2cError::new(errno::EREMOTEIO, "fake")) }
    }
    fn read(&mut self, n: usize) -> Result<Vec<u8>, I2cError> {
        match &mut self.read {
            Some(f) => Ok(f(n)),
            None => Err(I2cError::new(errno::EREMOTEIO, "fake")),
        }
    }
}

/// The default capabilities: a plain-I²C adapter with every SMBus transfer.
pub const ALL_FUNCS: u32 = func::I2C
    | func::QUICK
    | func::READ_BYTE
    | func::WRITE_BYTE
    | func::READ_BYTE_DATA
    | func::WRITE_BYTE_DATA
    | func::READ_WORD_DATA
    | func::WRITE_WORD_DATA
    | func::READ_I2C_BLOCK
    | func::WRITE_I2C_BLOCK;

/// An SMBus-only adapter (like i2c-stub): no plain I²C.
pub const SMBUS_ONLY: u32 = ALL_FUNCS & !func::I2C;

pub struct FakeBus {
    pub funcs: u32,
    pub chips: BTreeMap<u8, Box<dyn FakeChip>>,
    pub claimed: BTreeSet<u8>,
    pub log: Vec<String>,
    pub slept_ms: u64,
    pub now: i64,
}

impl Default for FakeBus {
    fn default() -> Self {
        Self::new(ALL_FUNCS)
    }
}

impl FakeBus {
    pub fn new(funcs: u32) -> Self {
        Self { funcs, chips: BTreeMap::new(), claimed: BTreeSet::new(), log: Vec::new(), slept_ms: 0, now: 0 }
    }
    pub fn add(mut self, addr: u8, chip: impl FakeChip) -> Self {
        self.chips.insert(addr, Box::new(chip));
        self
    }
    /// The chip at `addr`, as its concrete type.
    pub fn get<T: FakeChip>(&self, addr: u8) -> &T {
        (*self.chips[&addr]).as_any().downcast_ref().expect("chip type")
    }
    fn chip(&mut self, addr: u8, op: &str, force: bool) -> Result<&mut Box<dyn FakeChip>, I2cError> {
        if self.claimed.contains(&addr) && !force {
            return Err(I2cError::new(errno::EBUSY, op));
        }
        self.chips.get_mut(&addr).ok_or_else(|| I2cError::new(errno::EREMOTEIO, op))
    }
    fn w(&mut self, addr: u8, bytes: &[u8], op: &str, force: bool) -> Result<(), I2cError> {
        let l = bytes.iter().map(|b| hex(*b as u32)).collect::<Vec<_>>().join(" ");
        self.log.push(format!("{} W {l}", hex(addr as u32)));
        self.chip(addr, op, force)?.write(bytes).map_err(|e| I2cError::new(e.errno, op))
    }
    fn r(&mut self, addr: u8, n: usize, op: &str, force: bool) -> Result<Vec<u8>, I2cError> {
        self.log.push(format!("{} R {n}", hex(addr as u32)));
        self.chip(addr, op, force)?.read(n).map_err(|e| I2cError::new(e.errno, op))
    }
    fn need(&self, f: u32, op: &str) -> Result<(), I2cError> {
        if self.funcs & f == 0 { Err(I2cError::new(errno::EOPNOTSUPP, op)) } else { Ok(()) }
    }
}

impl I2cIo for FakeBus {
    fn funcs(&self) -> u32 {
        self.funcs
    }
    fn quick(&mut self, addr: u8, read: bool, force: bool) -> Result<(), I2cError> {
        self.need(func::QUICK, "quick")?;
        self.log.push(format!("{} Q{}", hex(addr as u32), if read { "r" } else { "w" }));
        self.chip(addr, "quick", force).map(|_| ())
    }
    fn receive(&mut self, addr: u8, force: bool) -> Result<u8, I2cError> {
        Ok(self.r(addr, 1, "receive", force)?.first().copied().unwrap_or(0))
    }
    fn send(&mut self, addr: u8, v: u8, force: bool) -> Result<(), I2cError> {
        self.w(addr, &[v], "send", force)
    }
    fn read_byte(&mut self, addr: u8, reg: u8, force: bool) -> Result<u8, I2cError> {
        self.w(addr, &[reg], "read", force)?;
        Ok(self.r(addr, 1, "read", force)?.first().copied().unwrap_or(0))
    }
    fn write_byte(&mut self, addr: u8, reg: u8, v: u8, force: bool) -> Result<(), I2cError> {
        self.w(addr, &[reg, v], "write", force)
    }
    fn read_word(&mut self, addr: u8, reg: u8, force: bool) -> Result<u16, I2cError> {
        self.w(addr, &[reg], "read word", force)?;
        let b = self.r(addr, 2, "read word", force)?;
        Ok(b.first().copied().unwrap_or(0) as u16 | (b.get(1).copied().unwrap_or(0) as u16) << 8)
    }
    fn write_word(&mut self, addr: u8, reg: u8, v: u16, force: bool) -> Result<(), I2cError> {
        self.w(addr, &[reg, v as u8, (v >> 8) as u8], "write word", force)
    }
    fn read_block(&mut self, addr: u8, reg: u8, len: usize, force: bool) -> Result<Vec<u8>, I2cError> {
        self.need(func::READ_I2C_BLOCK, "block read")?;
        self.w(addr, &[reg], "block read", force)?;
        self.r(addr, len.min(32), "block read", force)
    }
    fn write_block(&mut self, addr: u8, reg: u8, data: &[u8], force: bool) -> Result<(), I2cError> {
        let mut b = vec![reg];
        b.extend(data.iter().take(32));
        self.w(addr, &b, "block write", force)
    }
    fn transfer(&mut self, addr: u8, msgs: &[Msg], force: bool) -> Result<Vec<Vec<u8>>, I2cError> {
        self.need(func::I2C, "transfer")?;
        let mut out = Vec::new();
        for m in msgs {
            match m {
                Msg::W(b) => self.w(addr, b, "transfer", force)?,
                Msg::R(n) => out.push(self.r(addr, *n, "transfer", force)?),
            }
        }
        Ok(out)
    }
    fn sleep_ms(&mut self, ms: u32) {
        self.slept_ms += ms as u64;
        self.now += ms as i64;
    }
    fn now_ms(&self) -> i64 {
        self.now
    }
}
