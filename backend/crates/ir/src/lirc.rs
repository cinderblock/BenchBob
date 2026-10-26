//! The kernel's LIRC character devices (`/dev/lirc*`), which `gpio-ir` (receive),
//! `gpio-ir-tx` and `pwm-ir-tx` (transmit) create. No lircd: raw mode2 samples in, pulse
//! arrays out, by read/write and a few ioctls (linux/lirc.h).

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

// _IOR('i', 0x00, __u32) and _IOW('i', …, __u32).
const LIRC_GET_FEATURES: u32 = 0x8004_6900;
const LIRC_SET_REC_MODE: u32 = 0x4004_6912;
const LIRC_SET_SEND_CARRIER: u32 = 0x4004_6913;
const LIRC_SET_SEND_DUTY_CYCLE: u32 = 0x4004_6915;

pub const CAN_SEND_PULSE: u32 = 0x0000_0002;
pub const CAN_SET_SEND_CARRIER: u32 = 0x0000_0100;
pub const CAN_SET_SEND_DUTY_CYCLE: u32 = 0x0000_0200;
pub const CAN_REC_MODE2: u32 = 0x0004_0000;
const MODE_MODE2: u32 = 0x4;

/// One mode2 sample: a mark or space of `us`, or the end of a burst.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sample {
    Pulse(u32),
    Space(u32),
    /// The receiver saw nothing for its timeout: the frame is over.
    Timeout,
    Other,
}

pub fn sample(raw: u32) -> Sample {
    let v = raw & 0x00FF_FFFF;
    match raw >> 24 {
        0x00 => Sample::Space(v),
        0x01 => Sample::Pulse(v),
        0x03 => Sample::Timeout,
        _ => Sample::Other,
    }
}

/// Splits a sample stream into frames: a space longer than `gap_us` (or a timeout) ends
/// one. Frames start on a pulse and end on a pulse (the trailing space is the gap).
#[derive(Default)]
pub struct Framer {
    cur: Vec<u32>,
    pub gap_us: u32,
}

impl Framer {
    pub fn new(gap_us: u32) -> Self {
        Framer { cur: Vec::new(), gap_us }
    }

    /// Feed one sample; a finished frame comes back.
    pub fn push(&mut self, s: Sample) -> Option<Vec<u32>> {
        match s {
            Sample::Pulse(us) => {
                if self.cur.len() % 2 == 1 {
                    // Two pulses in a row (a dropped space): merge.
                    *self.cur.last_mut().unwrap() += us;
                } else {
                    self.cur.push(us);
                }
                None
            }
            Sample::Space(us) if self.cur.is_empty() => {
                let _ = us; // leading space: idle line
                None
            }
            Sample::Space(us) if us >= self.gap_us => self.take(),
            Sample::Space(us) => {
                if self.cur.len().is_multiple_of(2) {
                    *self.cur.last_mut().unwrap() += us;
                } else {
                    self.cur.push(us);
                }
                None
            }
            Sample::Timeout => self.take(),
            Sample::Other => None,
        }
    }

    fn take(&mut self) -> Option<Vec<u32>> {
        let mut f = std::mem::take(&mut self.cur);
        if f.len().is_multiple_of(2) {
            f.pop(); // ends on a space: drop it
        }
        // A blip isn't a frame.
        (f.len() >= 3).then_some(f)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Dev {
    pub path: PathBuf,
    pub features: u32,
    /// The kernel driver behind it ("gpio_ir_recv", "gpio-ir-tx", "pwm-ir-tx"), if found.
    pub driver: Option<String>,
}

impl Dev {
    pub fn can_receive(&self) -> bool {
        self.features & CAN_REC_MODE2 != 0
    }
    pub fn can_send(&self) -> bool {
        self.features & CAN_SEND_PULSE != 0
    }
}

fn ioctl_u32(fd: i32, req: u32, v: &mut u32) -> std::io::Result<()> {
    // SAFETY: each of these LIRC ioctls reads or writes one u32 through the pointer.
    let r = unsafe { libc::ioctl(fd, req as _, v as *mut u32) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// The driver name of /sys/class/lirc/<name>: its device, or (receivers) the rc device's.
fn driver_of(sys: &Path, name: &str) -> Option<String> {
    let base = sys.join(name).join("device");
    for p in [base.join("driver"), base.join("device").join("driver")] {
        if let Ok(t) = std::fs::read_link(&p)
            && let Some(n) = t.file_name()
        {
            return Some(n.to_string_lossy().into_owned());
        }
    }
    None
}

/// Every LIRC device in `dev_dir` (normally /dev) with its features.
pub fn list(dev_dir: &Path, sys: &Path) -> Vec<Dev> {
    let Ok(rd) = std::fs::read_dir(dev_dir) else { return vec![] };
    let mut v: Vec<Dev> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            name.strip_prefix("lirc")?.parse::<u32>().ok()?;
            let path = e.path();
            let features = File::open(&path).ok().and_then(|f| {
                let mut v = 0u32;
                ioctl_u32(f.as_raw_fd(), LIRC_GET_FEATURES, &mut v).ok().map(|_| v)
            })?;
            Some(Dev { path, features, driver: driver_of(sys, &name) })
        })
        .collect();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    v
}

/// A receiver in mode2, read in whole samples.
pub struct Rx {
    f: File,
}

impl Rx {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let f = File::open(path)?;
        let mut m = MODE_MODE2;
        ioctl_u32(f.as_raw_fd(), LIRC_SET_REC_MODE, &mut m)?;
        Ok(Rx { f })
    }

    /// Wait up to `ms` for samples; None on a timeout.
    pub fn read(&mut self, ms: i32, out: &mut Vec<Sample>) -> std::io::Result<bool> {
        let mut pfd = libc::pollfd { fd: self.f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: polling our own fd.
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n <= 0 {
            return Ok(false);
        }
        let mut buf = [0u8; 4 * 64];
        let got = self.f.read(&mut buf)?;
        for c in buf[..got - got % 4].as_chunks::<4>().0 {
            out.push(sample(u32::from_ne_bytes([c[0], c[1], c[2], c[3]])));
        }
        Ok(true)
    }
}

/// Send one frame (µs, mark first, odd length) with this carrier and duty cycle. The
/// write returns once the kernel has sent it.
pub fn send(dev: &Dev, carrier_hz: u32, duty: u32, frame: &[u32]) -> Result<(), String> {
    if !dev.can_send() {
        return Err(format!("{} can't send", dev.path.display()));
    }
    let f = std::fs::OpenOptions::new().write(true).open(&dev.path).map_err(|e| format!("{}: {e}", dev.path.display()))?;
    let fd = f.as_raw_fd();
    if dev.features & CAN_SET_SEND_CARRIER != 0 {
        let mut c = carrier_hz;
        ioctl_u32(fd, LIRC_SET_SEND_CARRIER, &mut c).map_err(|e| format!("set carrier {carrier_hz} Hz: {e}"))?;
    }
    if dev.features & CAN_SET_SEND_DUTY_CYCLE != 0 {
        let mut d = duty;
        ioctl_u32(fd, LIRC_SET_SEND_DUTY_CYCLE, &mut d).map_err(|e| format!("set duty {duty} %: {e}"))?;
    }
    let mut frame = frame.to_vec();
    if frame.len().is_multiple_of(2) {
        frame.pop(); // LIRC wants it to end on a pulse
    }
    let bytes: Vec<u8> = frame.iter().flat_map(|d| d.to_ne_bytes()).collect();
    // SAFETY: writing our buffer to our fd.
    let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if n < 0 {
        return Err(format!("{}: {}", dev.path.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_and_frames() {
        assert_eq!(sample(0x0100_0233), Sample::Pulse(0x233));
        assert_eq!(sample(0x0000_1000), Sample::Space(0x1000));
        assert_eq!(sample(0x0300_0000), Sample::Timeout);
        let mut f = Framer::new(15_000);
        let feed = [
            Sample::Space(500_000), // idle
            Sample::Pulse(9000),
            Sample::Space(4500),
            Sample::Pulse(560),
            Sample::Pulse(10), // a dropped space: merged into the mark
            Sample::Space(560),
            Sample::Pulse(560),
            Sample::Space(40_000), // the gap ends it
        ];
        let mut got = None;
        for s in feed {
            if let Some(fr) = f.push(s) {
                got = Some(fr);
            }
        }
        assert_eq!(got.unwrap(), vec![9000, 4500, 570, 560, 560]);
        // A blip isn't a frame; a timeout ends one too.
        assert_eq!(f.push(Sample::Pulse(300)), None);
        assert_eq!(f.push(Sample::Timeout), None);
        for s in [Sample::Pulse(2400), Sample::Space(600), Sample::Pulse(600), Sample::Space(600)] {
            f.push(s);
        }
        assert_eq!(f.push(Sample::Timeout).unwrap(), vec![2400, 600, 600]);
    }
}
