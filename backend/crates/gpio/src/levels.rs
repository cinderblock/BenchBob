//! Passive level readers: GPIO levels straight from the SoC's registers, without a line
//! request (which could disturb the pin) and without spawning `pinctrl` (a few ms of CPU
//! each time, at the panel's 10 Hz). Chosen by name in the board file.
//!
//! - `bcm2711-gpiomem`: BCM2835/6/7 and BCM2711 map their GPIO block at /dev/gpiomem
//!   (group `gpio`, no root needed); levels are GPLEV0/GPLEV1 at 0x34/0x38 (BCM2711 ARM
//!   Peripherals, §5.2). Same on every SoC of the family. (lib/gpiomem.ts)
//! - `rp1`: the Pi 5's header GPIOs are on RP1, behind PCIe. Its kernel driver exposes
//!   /dev/gpiomem0, a 0x30000-byte window at RP1's IO_BANK0 (0x400d0000): IO_BANK0 at
//!   +0x00000, SYS_RIO0 at +0x10000, PADS_BANK0 at +0x20000. Bank 0 holds GPIO0-27, the
//!   whole header. A level is bit n of SYS_RIO0's SYNC_IN register (+0x08), valid only
//!   while the pad's input enable (PADS_BANK0 GPIOn at +0x04 + 4n, bit 6, IE) is set; with
//!   input disabled the level is unknown. Layout from the RP1 Peripherals datasheet
//!   (2023-11, §3.1 GPIO: IO_BANK0, SYS_RIO, PADS_BANK0 register tables) and Raspberry Pi's
//!   pinctrl (utils/pinctrl/gpiochip_rp1.c), which reads levels the same way.
//!   **Untested: nobody here has a Pi 5.** The API says so (`reader.tested: false`).

use std::fs::File;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;

use benchbob_boards::Reader;

/// A read-only shared mapping of a register window, unmapped on drop.
struct Mapping {
    base: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is read-only device memory; reads are volatile and need no locking.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    fn open(path: &str, len: usize) -> std::io::Result<Self> {
        let f = File::open(path)?;
        // SAFETY: a fresh mapping of `len` bytes from offset 0 of a character device.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, f.as_raw_fd(), 0) };
        if p == libc::MAP_FAILED || p.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // The mapping outlives the descriptor.
        Ok(Self { base: NonNull::new(p as *mut u8).unwrap(), len })
    }

    fn read32(&self, off: usize) -> u32 {
        assert!(off + 4 <= self.len && off.is_multiple_of(4));
        // SAFETY: in bounds and aligned (checked above); device registers want volatile reads.
        unsafe { std::ptr::read_volatile(self.base.as_ptr().add(off) as *const u32) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what open() mapped.
        unsafe { libc::munmap(self.base.as_ptr() as *mut libc::c_void, self.len) };
    }
}

/// Live levels from a register window.
pub struct Levels {
    pub reader: Reader,
    map: Mapping,
}

const BCM_BLOCK: usize = 4096;
const GPLEV0: usize = 0x34;
const GPLEV1: usize = 0x38;

const RP1_WINDOW: usize = 0x30000;
const RP1_SYS_RIO0: usize = 0x10000;
const RIO_SYNC_IN: usize = 0x08;
const RP1_PADS_BANK0: usize = 0x20000;
const PAD_IE: u32 = 1 << 6;
/// GPIOs in RP1's bank 0 (the header's GPIO0-27).
pub const RP1_BANK0: u32 = 28;

/// Per-GPIO levels: bit n = GPIOn; `known` bit n clear = level unknown (RP1 pad input off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub bits: u64,
    pub known: u64,
}

impl Levels {
    /// Open the reader the board names; None if it doesn't apply or can't be opened
    /// (the caller falls back to pinctrl).
    pub fn open(reader: Reader) -> Option<Self> {
        let (path, len) = match reader {
            // Only on the SoCs whose /dev/gpiomem is this block (a forced board elsewhere,
            // or a Pi 5, must not read some other window as GPLEV).
            Reader::Bcm2711Gpiomem if compatible_has(&["brcm,bcm2835", "brcm,bcm2836", "brcm,bcm2837", "brcm,bcm2711"]) => {
                ("/dev/gpiomem", BCM_BLOCK)
            }
            Reader::Bcm2711Gpiomem => return None,
            Reader::Rp1 => (rp1_device()?, RP1_WINDOW),
            Reader::Gpiochip | Reader::Pinctrl => return None,
        };
        match Mapping::open(path, len) {
            Ok(map) => Some(Self { reader, map }),
            Err(e) => {
                tracing::warn!("[gpio] {} ({path}): {e}; falling back to pinctrl", reader.as_str());
                None
            }
        }
    }

    pub fn read(&self) -> Snapshot {
        match self.reader {
            Reader::Rp1 => {
                decode_rp1(self.map.read32(RP1_SYS_RIO0 + RIO_SYNC_IN), |n| self.map.read32(RP1_PADS_BANK0 + 4 + 4 * n as usize))
            }
            _ => {
                let lo = self.map.read32(GPLEV0) as u64;
                let hi = self.map.read32(GPLEV1) as u64;
                // GPIO0-57; GPLEV1's upper bits are reserved.
                Snapshot { bits: (hi << 32) | lo, known: (1u64 << 58) - 1 }
            }
        }
    }
}

/// /dev/gpiomem0 is RP1's IO_BANK0 window on a Pi 5 (a symlink /dev/gpiomem may point at
/// it). Checked against the device tree so a BCM2711's /dev/gpiomem is never read as RP1.
fn rp1_device() -> Option<&'static str> {
    if !compatible_has(&["brcm,bcm2712"]) {
        return None;
    }
    ["/dev/gpiomem0", "/dev/gpiomem"].into_iter().find(|p| std::path::Path::new(p).exists())
}

fn compatible_has(any: &[&str]) -> bool {
    let compat = std::fs::read("/proc/device-tree/compatible").unwrap_or_default();
    String::from_utf8_lossy(&compat).split('\0').any(|c| any.contains(&c))
}

/// RP1 levels: SYNC_IN bits for GPIO0-27, known where the pad's input enable is set.
pub fn decode_rp1(sync_in: u32, pad: impl Fn(u32) -> u32) -> Snapshot {
    let mut known = 0u64;
    for n in 0..RP1_BANK0 {
        if pad(n) & PAD_IE != 0 {
            known |= 1 << n;
        }
    }
    Snapshot { bits: (sync_in as u64) & ((1 << RP1_BANK0) - 1) & known, known }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rp1_levels_need_the_pad_input_enabled() {
        // GPIO2 and GPIO3 high; GPIO3's pad has input disabled (a pure output or analogue).
        let s = decode_rp1(0b1100, |n| if n == 3 { 0 } else { PAD_IE });
        assert_eq!(s.bits & 0b1100, 0b0100);
        assert_eq!(s.known & 0b1000, 0);
        assert_eq!(s.known & 0b0100, 0b0100);
        // Bits past the bank are never reported.
        assert_eq!(decode_rp1(u32::MAX, |_| PAD_IE).bits, (1 << 28) - 1);
    }
}
