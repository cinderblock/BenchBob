//! What a Linux string runs on:
//!
//! - **spidev**: a WS2812 waveform drawn on MOSI ([`SpiFit`]), or a clocked chip on MOSI +
//!   SCLK, in one `SPI_IOC_MESSAGE` per frame. On a Raspberry Pi the buses' pins are
//!   known (SPI0 GPIO10/11, SPI1 GPIO20/21, and on a Pi 4 SPI3–6); other boards have no
//!   SPI map yet.
//! - **bit-banged** clocked strings (APA102, WS2801) through the GPIO uAPI on any free
//!   header GPIO: the clock is ours, so scheduling jitter does no harm. Slow (a few µs a
//!   bit), so long animated strings update less often.
//!
//! One-wire strings never bit-bang: their timing can't be met from userspace.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use benchbob_gpio::uapi::LineHandle;

/// A spidev bus (its CS0 device) and the pins it drives, in the GPIO panel's numbering.
#[derive(Debug, Clone, PartialEq)]
pub struct SpiBus {
    pub bus: u32,
    pub dev: PathBuf,
    pub mosi: u32,
    pub sclk: u32,
}

/// A Raspberry Pi SPI controller's MOSI and SCLK (BCM GPIO numbers).
pub fn pi_spi_pins(bus: u32, bcm2711: bool) -> Option<(u32, u32)> {
    match (bus, bcm2711) {
        (0, _) => Some((10, 11)),
        (1, _) => Some((20, 21)),
        (3, true) => Some((2, 3)),
        (4, true) => Some((6, 7)),
        (5, true) => Some((14, 15)),
        (6, true) => Some((20, 21)),
        _ => None,
    }
}

/// The spidev buses in `dev_dir` with a known pin map (`spidevB.0` of each bus B).
pub fn spi_buses(dev_dir: &Path, pins: impl Fn(u32) -> Option<(u32, u32)>) -> Vec<SpiBus> {
    let Ok(rd) = std::fs::read_dir(dev_dir) else { return vec![] };
    let mut v: Vec<SpiBus> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let bus: u32 = name.strip_prefix("spidev")?.strip_suffix(".0")?.parse().ok()?;
            let (mosi, sclk) = pins(bus)?;
            Some(SpiBus { bus, dev: e.path(), mosi, sclk })
        })
        .collect();
    v.sort_by_key(|b| b.bus);
    v
}

/// spidev's per-transfer limit (`spidev.bufsiz` on the kernel command line).
pub fn spidev_bufsiz() -> usize {
    std::fs::read_to_string("/sys/module/spidev/parameters/bufsiz").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(4096)
}

/// How a controller turns a requested SPI clock into the one it sends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpiClock {
    /// BCM283x / BCM2711 (spi-bcm2835): the core clock over an even divider, rounded up
    /// so the clock never exceeds the request.
    Bcm2835 { core_hz: u32 },
    /// The Pi 5's RP1 (DesignWare SSI): 200 MHz over an even divider.
    Rp1,
    /// Unknown controller: assume it gives what was asked for.
    Exact,
}

impl SpiClock {
    pub fn actual(&self, hz: u32) -> u32 {
        let even = |src: u32| -> u32 {
            if hz == 0 {
                return 0;
            }
            let mut d = src.div_ceil(hz).max(2);
            d += d % 2;
            if d > 65534 { 0 } else { src / d }
        };
        match self {
            SpiClock::Bcm2835 { core_hz } => even(*core_hz),
            SpiClock::Rp1 => even(200_000_000),
            SpiClock::Exact => hz,
        }
    }
}

// linux/spi/spidev.h
#[repr(C)]
#[derive(Default)]
struct SpiIocTransfer {
    tx_buf: u64,
    rx_buf: u64,
    len: u32,
    speed_hz: u32,
    delay_usecs: u16,
    bits_per_word: u8,
    cs_change: u8,
    tx_nbits: u8,
    rx_nbits: u8,
    word_delay_usecs: u8,
    pad: u8,
}

const _: () = assert!(std::mem::size_of::<SpiIocTransfer>() == 32);

/// _IOW('k', 0, struct spi_ioc_transfer[1]) and _IOW('k', 1, __u8).
const SPI_IOC_MESSAGE_1: u32 = 0x4020_6b00;
const SPI_IOC_WR_MODE: u32 = 0x4001_6b01;

pub struct SpiOut {
    file: File,
    pub speed_hz: u32,
}

impl SpiOut {
    pub fn open(dev: &Path, speed_hz: u32) -> Result<Self, String> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(dev).map_err(|e| format!("{}: {e}", dev.display()))?;
        let mode: u8 = 0; // SPI mode 0: clock idles low, which is what WS2801/APA102 latch on
        // SAFETY: SPI_IOC_WR_MODE reads one byte from the pointer.
        let r = unsafe { libc::ioctl(file.as_raw_fd(), SPI_IOC_WR_MODE as _, &mode as *const u8) };
        if r < 0 {
            return Err(format!("{}: set mode: {}", dev.display(), std::io::Error::last_os_error()));
        }
        Ok(SpiOut { file, speed_hz })
    }

    /// One transfer of `data` (MOSI only).
    pub fn write(&self, data: &[u8]) -> Result<(), String> {
        let x = SpiIocTransfer {
            tx_buf: data.as_ptr() as u64,
            len: data.len() as u32,
            speed_hz: self.speed_hz,
            bits_per_word: 8,
            ..Default::default()
        };
        // SAFETY: the transfer points at `data`, which outlives the call; rx_buf is null.
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), SPI_IOC_MESSAGE_1 as _, &x as *const SpiIocTransfer) };
        if r < 0 { Err(format!("spidev write: {}", std::io::Error::last_os_error())) } else { Ok(()) }
    }
}

/// A clocked string on two GPIO lines.
pub struct BitBang {
    data: LineHandle,
    clock: LineHandle,
    invert: bool,
}

pub const CONSUMER: &str = "benchbob-leds";

impl BitBang {
    pub fn open(data: (&str, u32), clock: (&str, u32), invert: bool) -> Result<Self, String> {
        let d = LineHandle::request_output(data.0, data.1, CONSUMER, invert as u8, None).map_err(|e| e.to_string())?;
        let c = LineHandle::request_output(clock.0, clock.1, CONSUMER, 0, None).map_err(|e| e.to_string())?;
        Ok(BitBang { data: d, clock: c, invert })
    }

    /// MSB first; data changes while the clock is low and is latched on the rising edge.
    pub fn write(&mut self, frame: &[u8]) -> Result<(), String> {
        let mut last: Option<u8> = None;
        for &byte in frame {
            for i in (0..8).rev() {
                let bit = (byte >> i & 1) ^ self.invert as u8;
                if last != Some(bit) {
                    self.data.drive(bit, None).map_err(|e| e.to_string())?;
                    last = Some(bit);
                }
                self.clock.drive(1, None).map_err(|e| e.to_string())?;
                self.clock.drive(0, None).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

/// The Pi core clock, if config.txt pins it (core_freq_min ≥ core_freq). Without that the
/// SPI clock (and so a WS2812 waveform) moves whenever the firmware scales the core.
pub fn pi_fixed_core_hz() -> Result<u32, String> {
    let get = |k: &str| -> Option<u32> {
        let out = std::process::Command::new("vcgencmd").args(["get_config", k]).output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        s.trim().split_once('=')?.1.trim().parse().ok()
    };
    match (get("core_freq"), get("core_freq_min")) {
        (Some(f), Some(min)) if f > 0 && min >= f => Ok(f * 1_000_000),
        (Some(_), Some(_)) => Err("the Pi's core clock (and with it SPI's) changes with load: fix it with core_freq and core_freq_min (500 on a Pi 4, 250 on a Pi 3; GPIO panel → SPI settings) and reboot".into()),
        _ => Err("couldn't read the core clock (vcgencmd get_config core_freq)".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_spi_clock_rounds_down_to_even_dividers() {
        let c = SpiClock::Bcm2835 { core_hz: 500_000_000 };
        assert_eq!(c.actual(2_400_000), 500_000_000 / 210);
        assert_eq!(c.actual(250_000_000), 250_000_000);
        assert_eq!(c.actual(1_000_000_000), 250_000_000);
        assert_eq!(SpiClock::Rp1.actual(3_000_000), 200_000_000 / 68);
        assert_eq!(SpiClock::Exact.actual(1234), 1234);
    }

    #[test]
    fn finds_spidev_buses_with_known_pins() {
        let t = tempfile::tempdir().unwrap();
        for n in ["spidev0.0", "spidev0.1", "spidev4.0", "spidev2.0", "ttyS0"] {
            std::fs::write(t.path().join(n), "").unwrap();
        }
        let b = spi_buses(t.path(), |n| pi_spi_pins(n, true));
        assert_eq!(b.iter().map(|b| (b.bus, b.mosi, b.sclk)).collect::<Vec<_>>(), [(0, 10, 11), (4, 6, 7)]);
        let b = spi_buses(t.path(), |n| pi_spi_pins(n, false));
        assert_eq!(b.len(), 1);
    }
}
