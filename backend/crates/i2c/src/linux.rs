//! /dev/i2c-N through the kernel's i2c-dev ioctls (i2c-dev.ts `LinuxI2cBus`): no i2c-tools
//! processes, a precise errno for every NACK/timeout, and fast enough (~0.2 ms per probe) to
//! poll sensors. Struct layouts are from <linux/i2c-dev.h> and <linux/i2c.h>, declared
//! `repr(C)` so they're right on 32-bit ARM as well as 64-bit.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;

use benchbob_i2c_chips::io::{errno, func};
use benchbob_i2c_chips::{I2cError, I2cIo, Msg, hex};

const I2C_SLAVE: u32 = 0x0703;
const I2C_SLAVE_FORCE: u32 = 0x0706;
const I2C_FUNCS: u32 = 0x0705;
const I2C_RDWR: u32 = 0x0707;
const I2C_SMBUS: u32 = 0x0720;
const I2C_M_RD: u16 = 0x0001;

const SMBUS_READ: u8 = 1;
const SMBUS_WRITE: u8 = 0;
const SZ_QUICK: u32 = 0;
const SZ_BYTE: u32 = 1;
const SZ_BYTE_DATA: u32 = 2;
const SZ_WORD_DATA: u32 = 3;
const SZ_I2C_BLOCK_DATA: u32 = 8;

/// struct i2c_smbus_ioctl_data
#[repr(C)]
struct SmbusIoctl {
    read_write: u8,
    command: u8,
    size: u32,
    data: *mut u8,
}

/// struct i2c_msg
#[repr(C)]
struct I2cMsg {
    addr: u16,
    flags: u16,
    len: u16,
    buf: *mut u8,
}

/// struct i2c_rdwr_ioctl_data
#[repr(C)]
struct RdwrIoctl {
    msgs: *mut I2cMsg,
    nmsgs: u32,
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(errno::EIO)
}

/// Node's wording for a failed open: "EACCES: permission denied, open '/dev/i2c-1'".
pub fn open_error(e: &std::io::Error, path: &str) -> String {
    let code = match e.raw_os_error() {
        Some(2) => "ENOENT",
        Some(13) => "EACCES",
        Some(1) => "EPERM",
        Some(16) => "EBUSY",
        Some(19) => "ENODEV",
        Some(6) => "ENXIO",
        _ => "EIO",
    };
    let text = e.to_string();
    let text = text.split(" (os error").next().unwrap_or(&text).to_lowercase();
    format!("{code}: {text}, open '{path}'")
}

pub struct LinuxI2cBus {
    file: File,
    cur: i32,
    cur_force: bool,
    funcs: u32,
}

impl LinuxI2cBus {
    pub fn open(bus: u32) -> Result<Self, String> {
        let path = format!("/dev/i2c-{bus}");
        let file = OpenOptions::new().read(true).write(true).open(&path).map_err(|e| open_error(&e, &path))?;
        let mut f: libc::c_ulong = 0;
        // SAFETY: I2C_FUNCS writes one unsigned long through the pointer.
        if unsafe { libc::ioctl(file.as_raw_fd(), I2C_FUNCS as _, &mut f as *mut libc::c_ulong) } < 0 {
            return Err(I2cError::new(last_errno(), "I2C_FUNCS").message());
        }
        Ok(Self { file, cur: -1, cur_force: false, funcs: (f as u64 & 0xffff_ffff) as u32 })
    }

    fn target(&mut self, addr: u8, force: bool) -> Result<(), I2cError> {
        if self.cur == addr as i32 && self.cur_force == force {
            return Ok(());
        }
        let req = if force { I2C_SLAVE_FORCE } else { I2C_SLAVE };
        // SAFETY: I2C_SLAVE takes the address as an integer argument.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), req as _, addr as libc::c_ulong) } < 0 {
            self.cur = -1;
            return Err(I2cError::new(last_errno(), format!("set address {}", hex(addr as u32))));
        }
        self.cur = addr as i32;
        self.cur_force = force;
        Ok(())
    }

    fn smbus(&mut self, rw: u8, cmd: u8, size: u32, data: Option<&mut [u8; 34]>, op: impl FnOnce() -> String) -> Result<(), I2cError> {
        let mut a = SmbusIoctl { read_write: rw, command: cmd, size, data: data.map_or(std::ptr::null_mut(), |d| d.as_mut_ptr()) };
        // SAFETY: `a` and the 34-byte union buffer outlive the call; the kernel reads/writes
        // at most sizeof(union i2c_smbus_data) = 34 bytes.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), I2C_SMBUS as _, &mut a as *mut SmbusIoctl) } < 0 {
            return Err(I2cError::new(last_errno(), op()));
        }
        Ok(())
    }
}

impl I2cIo for LinuxI2cBus {
    fn funcs(&self) -> u32 {
        self.funcs
    }
    fn quick(&mut self, addr: u8, read: bool, force: bool) -> Result<(), I2cError> {
        self.target(addr, force)?;
        self.smbus(if read { SMBUS_READ } else { SMBUS_WRITE }, 0, SZ_QUICK, None, || format!("quick {}", hex(addr as u32)))
    }
    fn receive(&mut self, addr: u8, force: bool) -> Result<u8, I2cError> {
        self.target(addr, force)?;
        let mut d = [0u8; 34];
        self.smbus(SMBUS_READ, 0, SZ_BYTE, Some(&mut d), || format!("receive {}", hex(addr as u32)))?;
        Ok(d[0])
    }
    fn send(&mut self, addr: u8, v: u8, force: bool) -> Result<(), I2cError> {
        self.target(addr, force)?;
        self.smbus(SMBUS_WRITE, v, SZ_BYTE, None, || format!("send {}", hex(addr as u32)))
    }
    fn read_byte(&mut self, addr: u8, reg: u8, force: bool) -> Result<u8, I2cError> {
        self.target(addr, force)?;
        let mut d = [0u8; 34];
        self.smbus(SMBUS_READ, reg, SZ_BYTE_DATA, Some(&mut d), || format!("read {}[{}]", hex(addr as u32), hex(reg as u32)))?;
        Ok(d[0])
    }
    fn write_byte(&mut self, addr: u8, reg: u8, v: u8, force: bool) -> Result<(), I2cError> {
        self.target(addr, force)?;
        let mut d = [0u8; 34];
        d[0] = v;
        self.smbus(SMBUS_WRITE, reg, SZ_BYTE_DATA, Some(&mut d), || format!("write {}[{}]", hex(addr as u32), hex(reg as u32)))
    }
    fn read_word(&mut self, addr: u8, reg: u8, force: bool) -> Result<u16, I2cError> {
        self.target(addr, force)?;
        let mut d = [0u8; 34];
        self.smbus(SMBUS_READ, reg, SZ_WORD_DATA, Some(&mut d), || format!("read word {}[{}]", hex(addr as u32), hex(reg as u32)))?;
        Ok(u16::from_le_bytes([d[0], d[1]]))
    }
    fn write_word(&mut self, addr: u8, reg: u8, v: u16, force: bool) -> Result<(), I2cError> {
        self.target(addr, force)?;
        let mut d = [0u8; 34];
        d[..2].copy_from_slice(&v.to_le_bytes());
        self.smbus(SMBUS_WRITE, reg, SZ_WORD_DATA, Some(&mut d), || format!("write word {}[{}]", hex(addr as u32), hex(reg as u32)))
    }
    fn read_block(&mut self, addr: u8, reg: u8, len: usize, force: bool) -> Result<Vec<u8>, I2cError> {
        if self.funcs & func::READ_I2C_BLOCK == 0 {
            return (0..len).map(|i| self.read_byte(addr, reg.wrapping_add(i as u8), force)).collect();
        }
        self.target(addr, force)?;
        let mut d = [0u8; 34];
        d[0] = len.min(32) as u8;
        self.smbus(SMBUS_READ, reg, SZ_I2C_BLOCK_DATA, Some(&mut d), || format!("block read {}[{}]", hex(addr as u32), hex(reg as u32)))?;
        let n = (d[0] as usize).min(32);
        Ok(d[1..1 + n].to_vec())
    }
    fn write_block(&mut self, addr: u8, reg: u8, data: &[u8], force: bool) -> Result<(), I2cError> {
        self.target(addr, force)?;
        let mut d = [0u8; 34];
        let n = data.len().min(32);
        d[0] = n as u8;
        d[1..1 + n].copy_from_slice(&data[..n]);
        self.smbus(SMBUS_WRITE, reg, SZ_I2C_BLOCK_DATA, Some(&mut d), || format!("block write {}[{}]", hex(addr as u32), hex(reg as u32)))
    }
    fn transfer(&mut self, addr: u8, msgs: &[Msg], force: bool) -> Result<Vec<Vec<u8>>, I2cError> {
        let op = || format!("transfer {}", hex(addr as u32));
        if self.funcs & func::I2C == 0 {
            return Err(I2cError::new(errno::EOPNOTSUPP, op()));
        }
        if msgs.is_empty() || msgs.len() > 42 {
            return Err(I2cError::new(errno::EINVAL, op()));
        }
        // I2C_RDWR addresses each message itself, but a bound kernel driver must still win:
        // check it the same way i2ctransfer does (it refuses without -f).
        self.target(addr, force)?;
        let mut bufs: Vec<Vec<u8>> = msgs
            .iter()
            .map(|m| match m {
                Msg::W(b) => b.clone(),
                Msg::R(n) => vec![0; *n],
            })
            .collect();
        let mut raw: Vec<I2cMsg> = msgs
            .iter()
            .zip(bufs.iter_mut())
            .map(|(m, b)| I2cMsg {
                addr: addr as u16,
                flags: if matches!(m, Msg::R(_)) { I2C_M_RD } else { 0 },
                len: b.len() as u16,
                buf: if b.is_empty() { std::ptr::null_mut() } else { b.as_mut_ptr() },
            })
            .collect();
        let mut io = RdwrIoctl { msgs: raw.as_mut_ptr(), nmsgs: raw.len() as u32 };
        // SAFETY: every message buffer and the message array outlive the call; lengths match.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), I2C_RDWR as _, &mut io as *mut RdwrIoctl) } < 0 {
            return Err(I2cError::new(last_errno(), op()));
        }
        drop(raw);
        Ok(msgs.iter().zip(bufs).filter(|(m, _)| matches!(m, Msg::R(_))).map(|(_, b)| b).collect())
    }
    fn sleep_ms(&mut self, ms: u32) {
        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
    }
    fn now_ms(&self) -> i64 {
        benchbob_core::timeline::now_ms() as i64
    }
}
