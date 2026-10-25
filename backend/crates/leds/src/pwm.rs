//! One-wire strings on a Raspberry Pi 1–4's PWM serializer, fed by DMA: the method
//! rpi_ws281x made standard (PWM0 on GPIO12/18, PWM1 on GPIO13/19, so two strings), a
//! port of its register work. The serializer shifts 32-bit words out MSB first at the
//! PWM clock, exactly like SPI MOSI, so the waveform is the same [`SpiFit`] encoding the
//! spidev path uses, packed into words and interleaved for the two channels.
//!
//! The hardware needs `/dev/mem` (PWM, clock manager, DMA and GPIO registers) and the
//! VideoCore mailbox (`/dev/vcio`, for uncached memory DMA can read), so it runs in a
//! **privileged helper**: this same binary, `benchbob led-pwm-helper`, started through
//! `sudo -n` ([`PwmClient`]). The server stays unprivileged. Protocol on the helper's
//! stdin/stdout:
//!
//! 1. One JSON line, [`HelperConfig`]. The helper sets the hardware up and answers
//!    `ok\n` or `error: <why>\n` (then exits).
//! 2. Frames: `F`, then for each of the two channels a little-endian u32 length and that
//!    many bytes (the [`encode::ws_spi`] stream; length 0 = idle). The helper waits for
//!    the previous frame (and its reset time), copies this one into the DMA buffer, starts
//!    the DMA and answers `k`, or `e<why>\n` on a DMA error.
//! 3. EOF (or SIGTERM): the helper stops the DMA and PWM, puts the pins back to inputs and
//!    frees the VideoCore memory (it isn't freed with the process).
//!
//! The Pi 5 (RP1) has a different PWM block: not this path.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use benchbob_pixels::timing::{SpiFit, Timing};
use serde_json::{Value, json};

/// DMA channel: rpi_ws281x's default. Channel 5 corrupts the SD card on a Pi 3; the
/// firmware and kernel use the low channels.
pub const DMA_CHANNEL: u32 = 10;

/// The PWM pins: (BCM GPIO, PWM channel, GPFSEL function code). ALT0 = 4, ALT5 = 2.
pub const PWM_PINS: [(u32, usize, u32); 4] = [(12, 0, 4), (18, 0, 2), (13, 1, 4), (19, 1, 2)];

pub fn pwm_channel(gpio: u32) -> Option<usize> {
    PWM_PINS.iter().find(|p| p.0 == gpio).map(|p| p.1)
}

/// Which Pis have this PWM: BCM2835/6/7, BCM2710, BCM2711 (not the Pi 5's BCM2712).
pub fn soc_has_pwm_dma(soc: Option<&str>) -> bool {
    matches!(soc, Some("BCM2835" | "BCM2836" | "BCM2837" | "BCM2710" | "BCM2711"))
}

/// The PWM clock's source: the crystal, 54 MHz on a Pi 4, 19.2 MHz before it.
pub fn osc_hz(bcm2711: bool) -> u32 {
    if bcm2711 { 54_000_000 } else { 19_200_000 }
}

/// The clock manager's integer divider for `hz` (rounded up so the clock never exceeds
/// the request, as the SPI paths do), and the clock it gives.
pub fn clock_div(osc: u32, hz: u32) -> Option<(u32, u32)> {
    if hz == 0 {
        return None;
    }
    let d = osc.div_ceil(hz).max(2);
    (d <= 4095).then(|| (d, osc / d))
}

/// The waveform fit for `t` at what this PWM clock can do.
pub fn fit(t: &Timing, osc: u32) -> Option<SpiFit> {
    SpiFit::find(t, |hz| clock_div(osc, hz).map_or(0, |(_, a)| a))
}

/// Words for the DMA buffer: each channel's byte stream as big-endian words (the
/// serializer sends a word's MSB first), interleaved ch0, ch1, ch0… (the FIFO alternates
/// between the channels when both use it), padded with zero words to `words_per_channel`.
pub fn interleave(frames: [&[u8]; 2], words_per_channel: usize, out: &mut Vec<u32>) {
    out.clear();
    out.resize(words_per_channel * 2, 0);
    for (c, f) in frames.iter().enumerate() {
        for (w, chunk) in f.chunks(4).enumerate().take(words_per_channel) {
            let mut b = [0u8; 4];
            b[..chunk.len()].copy_from_slice(chunk);
            out[w * 2 + c] = u32::from_be_bytes(b);
        }
    }
}

/// The ARM physical address of the peripherals, from `/proc/device-tree/soc/ranges`
/// (what bcm_host_get_peripheral_address reads): the second cell, or the third when the
/// parent address takes two cells (Pi 4).
pub fn periph_base(ranges: &[u8]) -> Option<u32> {
    let cell = |i: usize| ranges.get(i * 4..i * 4 + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    match cell(1)? {
        0 => cell(2),
        b => Some(b),
    }
}

/// `_IOWR(100, 0, char *)`: the pointer size is in the request number.
pub const IOCTL_MBOX_PROPERTY: u32 = 0xC000_6400 | ((std::mem::size_of::<usize>() as u32) << 16);

/// A DMA control block (32 bytes, 32-byte aligned).
pub fn control_block(src_bus: u32, len: u32) -> [u32; 8] {
    const TI_NO_WIDE_BURSTS: u32 = 1 << 26;
    const TI_WAIT_RESP: u32 = 1 << 3;
    const TI_DEST_DREQ: u32 = 1 << 6;
    const TI_SRC_INC: u32 = 1 << 8;
    const PERMAP_PWM: u32 = 5 << 16;
    // The PWM FIFO (FIF1), as the DMA engine (a bus master) addresses it.
    const PWM_FIF1_BUS: u32 = 0x7E20_C018;
    [TI_NO_WIDE_BURSTS | TI_WAIT_RESP | TI_DEST_DREQ | PERMAP_PWM | TI_SRC_INC, src_bus, PWM_FIF1_BUS, len, 0, 0, 0, 0]
}

/// What the helper is asked to set up.
#[derive(Debug, Clone, PartialEq)]
pub struct HelperConfig {
    /// The pin of each channel (None: unused).
    pub pins: [Option<u32>; 2],
    pub invert: [bool; 2],
    pub clock_div: u32,
    /// The longest frame either channel will send, in bytes.
    pub max_bytes: usize,
    /// How long after a frame's last bit the next may start (the latch).
    pub reset_us: u32,
}

impl HelperConfig {
    pub fn to_json(&self) -> Value {
        json!({ "pins": self.pins, "invert": self.invert, "clockDiv": self.clock_div, "maxBytes": self.max_bytes, "resetUs": self.reset_us })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        let pin = |i: usize| v["pins"].get(i).and_then(Value::as_u64).map(|p| p as u32);
        let inv = |i: usize| v["invert"].get(i).and_then(Value::as_bool).unwrap_or(false);
        Some(HelperConfig {
            pins: [pin(0), pin(1)],
            invert: [inv(0), inv(1)],
            clock_div: v["clockDiv"].as_u64()? as u32,
            max_bytes: v["maxBytes"].as_u64()? as usize,
            reset_us: v["resetUs"].as_u64().unwrap_or(300) as u32,
        })
    }

    /// Words per channel: the longest frame, plus one zero word so the line idles low.
    pub fn words_per_channel(&self) -> usize {
        self.max_bytes.div_ceil(4) + 1
    }
}

// ---------- the server's side ----------

/// The running helper.
pub struct PwmClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    pub config: HelperConfig,
}

impl PwmClient {
    /// Start the helper through `sudo -n` and set the hardware up.
    pub fn start(config: HelperConfig) -> Result<Self, String> {
        let exe = std::env::current_exe().map_err(|e| format!("can't find benchbob's own binary: {e}"))?;
        let mut child = Command::new("sudo")
            .arg("-n")
            .arg(exe)
            .arg("led-pwm-helper")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("couldn't start the PWM helper through sudo: {e}"))?;
        let mut stdin = child.stdin.take().ok_or("no helper stdin")?;
        let mut stdout = BufReader::new(child.stdout.take().ok_or("no helper stdout")?);
        writeln!(stdin, "{}", config.to_json()).map_err(|e| format!("PWM helper: {e}"))?;
        let mut line = String::new();
        stdout.read_line(&mut line).map_err(|e| format!("PWM helper: {e}"))?;
        let line = line.trim();
        if line != "ok" {
            let _ = child.wait();
            return Err(match line.strip_prefix("error: ") {
                Some(e) => e.to_string(),
                None if line.is_empty() => "the PWM helper exited during setup (sudo -n refused, or it crashed; see the journal)".into(),
                None => format!("PWM helper: {line}"),
            });
        }
        Ok(PwmClient { child, stdin: Some(stdin), stdout, config })
    }

    /// Send one frame for both channels; returns once the DMA has started it.
    pub fn frame(&mut self, frames: [&[u8]; 2]) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("PWM helper closed")?;
        let mut msg = Vec::with_capacity(9 + frames[0].len() + frames[1].len());
        msg.push(b'F');
        for f in frames {
            msg.extend_from_slice(&(f.len() as u32).to_le_bytes());
            msg.extend_from_slice(f);
        }
        stdin.write_all(&msg).map_err(|e| format!("PWM helper: {e}"))?;
        let mut b = [0u8; 1];
        self.stdout.read_exact(&mut b).map_err(|e| format!("PWM helper: {e}"))?;
        match b[0] {
            b'k' => Ok(()),
            _ => {
                let mut line = String::new();
                let _ = self.stdout.read_line(&mut line);
                Err(format!("PWM DMA: {}", line.trim()))
            }
        }
    }
}

impl Drop for PwmClient {
    fn drop(&mut self) {
        // EOF is the clean way out: the helper blanks nothing (the server already sent a
        // black frame), stops the hardware and frees its memory.
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------- the helper (root) ----------

#[cfg(target_os = "linux")]
mod helper {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    static STOP: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_signal(_: libc::c_int) {
        STOP.store(true, Ordering::SeqCst);
    }

    const PAGE: usize = 4096;

    // Offsets from the peripheral base.
    const DMA_BASE: u32 = 0x0000_7000;
    const PWM_OFFSET: u32 = 0x0020_C000;
    const CM_OFFSET: u32 = 0x0010_1000; // CM_PWMCTL at +0xA0, CM_PWMDIV at +0xA4
    const GPIO_OFFSET: u32 = 0x0020_0000;

    // Register indices (u32 words).
    const PWM_CTL: usize = 0;
    const PWM_DMAC: usize = 2;
    const PWM_RNG1: usize = 4;
    const PWM_RNG2: usize = 8;
    const CM_PWMCTL: usize = 0xA0 / 4;
    const CM_PWMDIV: usize = 0xA4 / 4;
    const DMA_CS: usize = 0;
    const DMA_CONBLK_AD: usize = 1;
    const DMA_TXFR_LEN: usize = 5;
    const DMA_DEBUG: usize = 8;

    const CM_PASSWD: u32 = 0x5a << 24;
    const CM_BUSY: u32 = 1 << 7;
    const CM_KILL: u32 = 1 << 5;
    const CM_ENAB: u32 = 1 << 4;
    const CM_SRC_OSC: u32 = 1;

    const CTL_CLRF1: u32 = 1 << 6;
    const CTL_USEF1: u32 = 1 << 5;
    const CTL_POLA1: u32 = 1 << 4;
    const CTL_MODE1: u32 = 1 << 1;
    const CTL_PWEN1: u32 = 1 << 0;
    const CTL_USEF2: u32 = 1 << 13;
    const CTL_POLA2: u32 = 1 << 12;
    const CTL_MODE2: u32 = 1 << 9;
    const CTL_PWEN2: u32 = 1 << 8;
    const DMAC_ENAB: u32 = 1 << 31;

    const CS_RESET: u32 = 1 << 31;
    const CS_WAIT_OUTSTANDING_WRITES: u32 = 1 << 28;
    const CS_ERROR: u32 = 1 << 8;
    const CS_INT: u32 = 1 << 2;
    const CS_END: u32 = 1 << 1;
    const CS_ACTIVE: u32 = 1 << 0;

    /// A mapping of /dev/mem.
    struct Map {
        ptr: *mut u32,
        base: *mut libc::c_void,
        len: usize,
    }

    impl Map {
        fn new(fd: i32, phys: u32, len: usize) -> Result<Self, String> {
            let page = phys as usize & !(PAGE - 1);
            let off = phys as usize - page;
            let maplen = (off + len).div_ceil(PAGE) * PAGE;
            // SAFETY: mapping /dev/mem; the address range is a peripheral or memory the
            // mailbox gave us.
            let base = unsafe {
                libc::mmap(std::ptr::null_mut(), maplen, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, page as libc::off_t)
            };
            if base == libc::MAP_FAILED {
                return Err(format!("mmap 0x{phys:08x}: {}", std::io::Error::last_os_error()));
            }
            // SAFETY: `off` is inside the mapping.
            let ptr = unsafe { (base as *mut u8).add(off) as *mut u32 };
            Ok(Map { ptr, base, len: maplen })
        }
        fn rd(&self, i: usize) -> u32 {
            // SAFETY: i is a register index inside the mapping.
            unsafe { std::ptr::read_volatile(self.ptr.add(i)) }
        }
        fn wr(&self, i: usize, v: u32) {
            // SAFETY: as rd.
            unsafe { std::ptr::write_volatile(self.ptr.add(i), v) }
        }
    }

    impl Drop for Map {
        fn drop(&mut self) {
            // SAFETY: unmapping what new() mapped.
            unsafe { libc::munmap(self.base, self.len) };
        }
    }

    fn sleep_us(us: u64) {
        std::thread::sleep(Duration::from_micros(us));
    }

    /// Wait for `cond`, at most `ms`.
    fn wait(ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        let t = Instant::now();
        while !cond() {
            if t.elapsed() > Duration::from_millis(ms) {
                return false;
            }
            sleep_us(10);
        }
        true
    }

    struct Mbox {
        fd: i32,
        handle: u32,
        bus: u32,
    }

    fn mbox_call(fd: i32, tag: u32, args: &[u32]) -> Result<u32, String> {
        let mut p = [0u32; 32];
        let mut i = 0;
        let mut push = |v: u32| {
            p[i] = v;
            i += 1;
        };
        push(0);
        push(0); // process request
        push(tag);
        push(args.len() as u32 * 4);
        push(args.len() as u32 * 4);
        for &a in args {
            push(a);
        }
        push(0); // end tag
        p[0] = (i * 4) as u32;
        // SAFETY: the property buffer is the size it says.
        let r = unsafe { libc::ioctl(fd, IOCTL_MBOX_PROPERTY as _, p.as_mut_ptr()) };
        if r < 0 {
            return Err(format!("mailbox tag 0x{tag:x}: {}", std::io::Error::last_os_error()));
        }
        Ok(p[5])
    }

    impl Mbox {
        fn alloc(size: u32, flags: u32) -> Result<Self, String> {
            let path = std::ffi::CString::new("/dev/vcio").unwrap();
            // SAFETY: opening a device node.
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
            if fd < 0 {
                return Err(format!("/dev/vcio: {}", std::io::Error::last_os_error()));
            }
            let handle = mbox_call(fd, 0x3000c, &[size, PAGE as u32, flags])?;
            if handle == 0 {
                // SAFETY: closing what we opened.
                unsafe { libc::close(fd) };
                return Err("the VideoCore wouldn't allocate DMA memory".into());
            }
            let bus = mbox_call(fd, 0x3000d, &[handle])?;
            if bus == 0 || bus == u32::MAX {
                let _ = mbox_call(fd, 0x3000f, &[handle]);
                // SAFETY: as above.
                unsafe { libc::close(fd) };
                return Err("the VideoCore wouldn't lock the DMA memory".into());
            }
            Ok(Mbox { fd, handle, bus })
        }
    }

    impl Drop for Mbox {
        fn drop(&mut self) {
            let _ = mbox_call(self.fd, 0x3000e, &[self.handle]);
            let _ = mbox_call(self.fd, 0x3000f, &[self.handle]);
            // SAFETY: closing what we opened.
            unsafe { libc::close(self.fd) };
        }
    }

    struct Hw {
        cfg: HelperConfig,
        pwm: Map,
        cm: Map,
        dma: Map,
        gpio: Map,
        mem: Map,
        // Dropped after `mem` is unmapped.
        _mbox: Mbox,
        last_end: Option<Instant>,
        last_len_us: u64,
        words: Vec<u32>,
    }

    fn read_bytes(p: &str) -> Vec<u8> {
        std::fs::read(p).unwrap_or_default()
    }

    impl Hw {
        fn new(cfg: HelperConfig) -> Result<Self, String> {
            let compat = String::from_utf8_lossy(&read_bytes("/proc/device-tree/compatible")).replace('\0', " ");
            let soc = ["bcm2711", "bcm2710", "bcm2837", "bcm2836", "bcm2835"].into_iter().find(|s| compat.contains(&format!("brcm,{s}")));
            let Some(soc) = soc else { return Err("this isn't a Raspberry Pi 1–4 (no BCM2835-family SoC in the device tree)".into()) };
            let cards = String::from_utf8_lossy(&read_bytes("/proc/asound/cards")).to_string();
            if cards.contains("Headphones") || cards.contains("bcm2835 ALSA") {
                return Err("analog audio uses the PWM: turn it off (dtparam=audio=off; GPIO panel → SPI settings) and reboot; HDMI audio is unaffected".into());
            }
            let base = periph_base(&read_bytes("/proc/device-tree/soc/ranges"))
                .ok_or("couldn't read the peripheral base (/proc/device-tree/soc/ranges)")?;
            // Uncached for DMA: Pi 1's VideoCore aliases memory at 0x4… (L1-nonallocating),
            // later Pis at 0xC… (direct).
            let flags = if soc == "bcm2835" { 0xC } else { 0x4 };
            let words = cfg.words_per_channel() * 2;
            let size = (32 + words * 4).div_ceil(PAGE) * PAGE;
            let mbox = Mbox::alloc(size as u32, flags)?;

            let path = std::ffi::CString::new("/dev/mem").unwrap();
            // SAFETY: opening a device node.
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_SYNC) };
            if fd < 0 {
                return Err(format!("/dev/mem: {}", std::io::Error::last_os_error()));
            }
            let maps = (|| -> Result<_, String> {
                let dma_off = if DMA_CHANNEL == 15 { 0x00E0_5000 } else { DMA_BASE + DMA_CHANNEL * 0x100 };
                Ok((
                    Map::new(fd, base + PWM_OFFSET, 0x28)?,
                    Map::new(fd, base + CM_OFFSET, 0xA8)?,
                    Map::new(fd, base + dma_off, 0x24)?,
                    Map::new(fd, base + GPIO_OFFSET, 0xB4)?,
                    Map::new(fd, mbox.bus & !0xC000_0000, size)?,
                ))
            })();
            // SAFETY: the mappings stay valid after the fd is closed.
            unsafe { libc::close(fd) };
            let (pwm, cm, dma, gpio, mem) = maps?;
            let hw = Hw { cfg, pwm, cm, dma, gpio, mem, _mbox: mbox, last_end: None, last_len_us: 0, words: Vec::new() };
            hw.setup()?;
            Ok(hw)
        }

        fn fsel(&self, pin: u32, func: u32) {
            let (reg, shift) = ((pin / 10) as usize, (pin % 10) * 3);
            let v = self.gpio.rd(reg) & !(7 << shift);
            self.gpio.wr(reg, v | (func << shift));
        }

        fn stop_pwm(&self) -> Result<(), String> {
            self.pwm.wr(PWM_CTL, 0);
            sleep_us(10);
            self.cm.wr(CM_PWMCTL, CM_PASSWD | CM_KILL);
            sleep_us(10);
            if !wait(50, || self.cm.rd(CM_PWMCTL) & CM_BUSY == 0) {
                return Err("the PWM clock didn't stop".into());
            }
            Ok(())
        }

        fn setup(&self) -> Result<(), String> {
            self.stop_pwm()?;
            self.cm.wr(CM_PWMDIV, CM_PASSWD | ((self.cfg.clock_div & 0xfff) << 12));
            self.cm.wr(CM_PWMCTL, CM_PASSWD | CM_SRC_OSC);
            self.cm.wr(CM_PWMCTL, CM_PASSWD | CM_SRC_OSC | CM_ENAB);
            sleep_us(10);
            if !wait(50, || self.cm.rd(CM_PWMCTL) & CM_BUSY != 0) {
                return Err("the PWM clock didn't start".into());
            }
            // Delays between PWM writes: the block is said to lock up without them.
            self.pwm.wr(PWM_RNG1, 32);
            self.pwm.wr(PWM_RNG2, 32);
            sleep_us(10);
            self.pwm.wr(PWM_CTL, CTL_CLRF1);
            sleep_us(10);
            self.pwm.wr(PWM_DMAC, DMAC_ENAB | (7 << 8) | 3); // PANIC 7, DREQ 3
            sleep_us(10);
            let mut ctl = CTL_USEF1 | CTL_MODE1 | CTL_USEF2 | CTL_MODE2;
            if self.cfg.invert[0] {
                ctl |= CTL_POLA1;
            }
            if self.cfg.invert[1] {
                ctl |= CTL_POLA2;
            }
            self.pwm.wr(PWM_CTL, ctl);
            sleep_us(10);
            self.pwm.wr(PWM_CTL, ctl | CTL_PWEN1 | CTL_PWEN2);
            // An all-zero buffer, then the pins (so they come up idle).
            for i in 0..self.mem.len / 4 {
                self.mem.wr(i, 0);
            }
            for (c, pin) in self.cfg.pins.iter().enumerate() {
                if let Some(p) = pin {
                    let f = PWM_PINS.iter().find(|x| x.0 == *p && x.1 == c).ok_or_else(|| format!("GPIO{p} isn't a PWM{c} pin"))?.2;
                    self.fsel(*p, f);
                }
            }
            self.dma.wr(DMA_CS, 0);
            self.dma.wr(DMA_TXFR_LEN, 0);
            Ok(())
        }

        /// Wait out the previous frame and its latch, then start this one.
        fn frame(&mut self, frames: [&[u8]; 2]) -> Result<(), String> {
            if !wait(1000, || self.dma.rd(DMA_CS) & (CS_ACTIVE | CS_ERROR) != CS_ACTIVE) {
                return Err("the previous DMA transfer never finished".into());
            }
            if self.dma.rd(DMA_CS) & CS_ERROR != 0 {
                return Err(format!("DMA error, debug 0x{:08x}", self.dma.rd(DMA_DEBUG)));
            }
            if let Some(t) = self.last_end {
                let need = Duration::from_micros(self.last_len_us + self.cfg.reset_us as u64);
                if let Some(rest) = need.checked_sub(t.elapsed()) {
                    std::thread::sleep(rest);
                }
            }
            let wpc = self.cfg.words_per_channel();
            interleave(frames, wpc, &mut self.words);
            // The control block first, then the data right after it.
            let len = (self.words.len() * 4) as u32;
            let cb = control_block(self._mbox.bus + 32, len);
            for (i, w) in cb.iter().enumerate() {
                self.mem.wr(i, *w);
            }
            for (i, w) in self.words.iter().enumerate() {
                self.mem.wr(8 + i, *w);
            }
            self.dma.wr(DMA_CS, CS_RESET);
            sleep_us(10);
            self.dma.wr(DMA_CS, CS_INT | CS_END);
            sleep_us(10);
            self.dma.wr(DMA_CONBLK_AD, self._mbox.bus);
            self.dma.wr(DMA_DEBUG, 7);
            self.dma.wr(DMA_CS, CS_WAIT_OUTSTANDING_WRITES | (15 << 20) | (15 << 16) | CS_ACTIVE);
            // Both channels run in parallel: the longer one sets the frame time.
            let bits = frames.iter().map(|f| f.len()).max().unwrap_or(0) as u64 * 8;
            let clock = osc_from_div(self.cfg.clock_div);
            self.last_len_us = bits * 1_000_000 / clock.max(1) as u64;
            self.last_end = Some(Instant::now());
            Ok(())
        }
    }

    /// The PWM clock: the server picked the divider for this SoC's crystal.
    fn osc_from_div(div: u32) -> u32 {
        let pi4 = String::from_utf8_lossy(&read_bytes("/proc/device-tree/compatible")).contains("brcm,bcm2711");
        osc_hz(pi4) / div.max(1)
    }

    impl Drop for Hw {
        fn drop(&mut self) {
            let _ = wait(1000, || self.dma.rd(DMA_CS) & CS_ACTIVE == 0);
            self.dma.wr(DMA_CS, CS_RESET);
            sleep_us(10);
            let _ = self.stop_pwm();
            for p in self.cfg.pins.iter().flatten() {
                self.fsel(*p, 0); // back to an input
            }
        }
    }

    /// Read exactly `buf.len()` bytes from stdin; false on EOF or a stop signal.
    fn read_full(buf: &mut [u8]) -> bool {
        let mut got = 0;
        while got < buf.len() {
            // SAFETY: reading into our buffer.
            let n = unsafe { libc::read(0, buf[got..].as_mut_ptr() as *mut libc::c_void, buf.len() - got) };
            if STOP.load(Ordering::SeqCst) {
                return false;
            }
            match n {
                n if n > 0 => got += n as usize,
                0 => return false,
                _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
                _ => return false,
            }
        }
        true
    }

    fn read_line() -> Option<String> {
        let mut v = Vec::new();
        let mut b = [0u8; 1];
        while read_full(&mut b) {
            if b[0] == b'\n' {
                return String::from_utf8(v).ok();
            }
            v.push(b[0]);
            if v.len() > 4096 {
                return None;
            }
        }
        None
    }

    pub fn main() -> i32 {
        // SAFETY: installing a handler that only sets an atomic; no SA_RESTART, so a
        // blocked read returns and the hardware gets put back.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
            libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
            libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut());
        }
        let out = |s: &[u8]| {
            let mut o = std::io::stdout().lock();
            let _ = o.write_all(s);
            let _ = o.flush();
        };
        let Some(line) = read_line() else { return 1 };
        let Some(cfg) = serde_json::from_str::<Value>(&line).ok().as_ref().and_then(HelperConfig::from_json) else {
            out(b"error: bad config\n");
            return 1;
        };
        if cfg.max_bytes > 1 << 20 {
            out(b"error: frame too large\n");
            return 1;
        }
        let mut hw = match Hw::new(cfg) {
            Ok(h) => h,
            Err(e) => {
                out(format!("error: {e}\n").as_bytes());
                return 1;
            }
        };
        out(b"ok\n");
        let max = hw.cfg.max_bytes;
        let mut bufs = [vec![0u8; max], vec![0u8; max]];
        loop {
            let mut tag = [0u8; 1];
            if !read_full(&mut tag) || tag[0] != b'F' {
                break;
            }
            let mut lens = [0usize; 2];
            let mut ok = true;
            for c in 0..2 {
                let mut l = [0u8; 4];
                if !read_full(&mut l) {
                    ok = false;
                    break;
                }
                let n = u32::from_le_bytes(l) as usize;
                if n > max || !read_full(&mut bufs[c][..n]) {
                    ok = false;
                    break;
                }
                lens[c] = n;
            }
            if !ok {
                break;
            }
            match hw.frame([&bufs[0][..lens[0]], &bufs[1][..lens[1]]]) {
                Ok(()) => out(b"k"),
                Err(e) => out(format!("e{e}\n").as_bytes()),
            }
        }
        drop(hw);
        0
    }
}

/// `benchbob led-pwm-helper` (as root): see the module docs.
pub fn helper_main() -> i32 {
    #[cfg(target_os = "linux")]
    return helper::main();
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("led-pwm-helper only runs on a Raspberry Pi");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_and_socs() {
        assert_eq!(pwm_channel(18), Some(0));
        assert_eq!(pwm_channel(13), Some(1));
        assert_eq!(pwm_channel(10), None);
        assert!(soc_has_pwm_dma(Some("BCM2711")));
        assert!(!soc_has_pwm_dma(Some("BCM2712")));
        assert!(!soc_has_pwm_dma(None));
    }

    #[test]
    fn clock_and_fit() {
        // Pi 4: 54 MHz / 23 = 2.348 MHz for WS2812B's 3 bits per bit.
        assert_eq!(clock_div(54_000_000, 2_400_000), Some((23, 2_347_826)));
        assert_eq!(clock_div(19_200_000, 2_400_000), Some((8, 2_400_000)));
        assert_eq!(clock_div(19_200_000, 1000), None);
        let f = fit(&Timing::WS2812B, 19_200_000).unwrap();
        assert_eq!((f.clock_hz, f.bits, f.ones0, f.ones1), (2_400_000, 3, 1, 2));
        assert!(fit(&Timing::WS2812B, 54_000_000).is_some());
    }

    #[test]
    fn words_interleave_big_endian() {
        let mut w = Vec::new();
        interleave([&[0x11, 0x22, 0x33, 0x44, 0x55], &[0xAA]], 3, &mut w);
        assert_eq!(w, [0x1122_3344, 0xAA00_0000, 0x5500_0000, 0, 0, 0]);
    }

    #[test]
    fn device_tree_and_ioctls() {
        // Pi 4: <0x7e000000 0x0 0xfe000000 0x1800000>
        let pi4 = [0x7e, 0, 0, 0, 0, 0, 0, 0, 0xfe, 0, 0, 0, 0x01, 0x80, 0, 0];
        assert_eq!(periph_base(&pi4), Some(0xFE00_0000));
        // Pi 3: <0x7e000000 0x3f000000 0x1000000>
        let pi3 = [0x7e, 0, 0, 0, 0x3f, 0, 0, 0, 0x01, 0, 0, 0];
        assert_eq!(periph_base(&pi3), Some(0x3F00_0000));
        assert_eq!(periph_base(&[]), None);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(IOCTL_MBOX_PROPERTY, 0xC008_6400);
        let cb = control_block(0xC000_1020, 4096);
        assert_eq!(cb[0], (1 << 26) | (1 << 3) | (1 << 6) | (5 << 16) | (1 << 8));
        assert_eq!(&cb[1..4], &[0xC000_1020, 0x7E20_C018, 4096]);
    }

    #[test]
    fn config_round_trips() {
        let c = HelperConfig { pins: [Some(18), None], invert: [false, true], clock_div: 23, max_bytes: 1000, reset_us: 300 };
        assert_eq!(HelperConfig::from_json(&c.to_json()), Some(c.clone()));
        assert_eq!(c.words_per_channel(), 251);
    }
}
