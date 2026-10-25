//! Addressable LED strings, and the DevKitC-1's status LED (a WS2812 on GPIO8).
//!
//! - Status LED: RMT channel 0. Wi-Fi state in colour (slow blue blink = not set up,
//!   fast amber blink = connecting, dim green = connected); white while someone holds the
//!   dashboard's onboard-LED button ([`crate::board::LED_HOLD`]).
//! - Strings, on any free GPIO through the GPIO matrix, two lanes:
//!   - SPI: GP-SPI2 + DMA. One-wire chips get their waveform drawn on MOSI
//!     ([`SpiFit`]); clocked chips (APA102, WS2801) use MOSI + SCLK. Up to MAX_PIXELS.
//!   - RMT: channel 1 (the C6's only other TX channel), one-wire only. Every data bit is
//!     a 32-bit pulse code in RAM, so it takes strings up to [`RMT_MAX_BITS`] / 24 pixels.
//!
//!   A one-wire string goes on RMT when it fits and RMT is free, otherwise on SPI; a
//!   clocked one needs SPI. Inversion (for an inverting level shifter) is the GPIO
//!   matrix's output inverter, so both lanes have it.
//!
//! A string's pins leave the GPIO API while it runs and come back as fresh `Flex`es when
//! it is removed (after a black frame, so it goes dark).

use crate::SharedNode;
use crate::board::{C6Node, LED_HOLD, LED_WAKE};
use benchbob_node::http::Shared;
use benchbob_node::net;
use benchbob_pixels::encode;
use benchbob_pixels::timing::{SpiFit, Timing};
use benchbob_proto::api::leds::{Job, MAX_PIXELS, MAX_STRINGS, StringConfig};
use benchbob_proto::api::{Leds, LedsInfo, WifiState};
use embassy_futures::select::{Either, select};
use embassy_time::{Instant, Timer};
use esp_hal::Async;
use esp_hal::dma::{DmaDescriptor, DmaTxBuf};
use esp_hal::gpio::interconnect::OutputSignal;
use esp_hal::gpio::{AnyPin, Flex, Level};
use esp_hal::peripherals::{DMA_CH0, SPI2};
use esp_hal::rmt::{Channel, ChannelCreator, PulseCode, Tx, TxChannelConfig, TxChannelCreator};
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config, Spi, SpiDma};
use esp_hal::time::Rate;
use log::{info, warn};
use static_cell::{ConstStaticCell, StaticCell};

pub static LEDS: LedsInfo =
    LedsInfo { max_strings: 2, max_pixels: MAX_PIXELS as u16, one_wire: true, clocked: true, check_timing, max_clock_hz: 20_000_000 };

/// RMT ticks at 80 MHz (12.5 ns), with 15-bit lengths: anything up to 409 µs a phase.
/// One-wire timings SPI can't fit still run on RMT, so this is the only limit.
fn check_timing(t: &Timing) -> Result<(), &'static str> {
    if ticks(t.period_ns) > PulseCode::MAX_LEN { Err("the bit period is too long for the RMT (max 409 µs)") } else { Ok(()) }
}

const RMT_HZ: u32 = 80_000_000;
/// Pulse codes the RMT lane has room for: 256 RGB (192 RGBW) pixels.
pub const RMT_MAX_BITS: usize = 256 * 24;
/// The SPI lane's DMA buffer: 1024 RGBW pixels at 4 SPI bits per bit, plus the reset.
const SPI_BUF: usize = 20 * 1024;
/// A DMA descriptor covers up to 4092 bytes.
const SPI_DESCRIPTORS: usize = SPI_BUF.div_ceil(4092);
/// How often animated strings get a new frame (and the gap that latches a frame).
const FRAME_MS: u64 = 20;

fn ticks(ns: u32) -> u16 {
    ((ns as u64 * RMT_HZ as u64 + 500_000_000) / 1_000_000_000).min(u16::MAX as u64) as u16
}

/// The SPI clock GP-SPI2 gives for a requested one: esp-hal's divider search
/// (`Config::recalculate`), from the 80 MHz PLL clock.
fn spi_clock(want: u32) -> u32 {
    const SRC: i32 = 80_000_000;
    if want == 0 {
        return 0;
    }
    if want > SRC as u32 / 4 * 3 {
        return SRC as u32;
    }
    let want = want as i32;
    let mut best = (i32::MAX, 1, 1);
    for n in 2..=64 {
        let pre = ((SRC / n + want / 2) / want).clamp(1, 16);
        let err = (SRC / (pre * n) - want).abs();
        if err <= best.0 {
            best = (err, n, pre);
        }
    }
    (SRC / (best.1 * best.2)) as u32
}

/// A GPIO as a peripheral output, inverted or not.
fn out(gpio: u8, invert: bool) -> OutputSignal<'static> {
    // SAFETY: the board gave this pin up (take_pin); only this lane drives it until it's
    // returned.
    Flex::new(unsafe { AnyPin::steal(gpio) }).into_peripheral_output().with_output_inverter(invert)
}

/// The SPI lane's DMA buffer, in internal RAM. Call once.
pub fn dma_buffer() -> DmaTxBuf {
    static DESC: StaticCell<[DmaDescriptor; SPI_DESCRIPTORS]> = StaticCell::new();
    // Const-initialised, so the 20 KiB never passes through the stack.
    static MEM: ConstStaticCell<[u8; SPI_BUF]> = ConstStaticCell::new([0; SPI_BUF]);
    DmaTxBuf::new(DESC.init([DmaDescriptor::EMPTY; SPI_DESCRIPTORS]), MEM.take()).expect("enough descriptors")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LaneId {
    Spi,
    Rmt,
}

#[derive(Clone, Copy)]
enum SpiKind {
    OneWire(SpiFit),
    Apa102,
    Ws2801,
}

struct SpiLane {
    spi: Option<SpiDma<'static, Async>>,
    /// Out only while a transfer has it.
    buf: Option<DmaTxBuf>,
    kind: SpiKind,
    pins: [Option<u8>; 2],
    len: usize,
}

impl SpiLane {
    fn release(&mut self, node: &mut C6Node) {
        // Dropping the driver disconnects its pins from the SPI signals.
        self.spi = None;
        for g in self.pins.iter_mut().filter_map(Option::take) {
            node.return_pin(g);
        }
        self.len = 0;
    }

    fn claim(&mut self, node: &mut C6Node, c: &StringConfig) -> Result<(), &'static str> {
        let (hz, kind, len) = if c.chip.clocked() {
            let len = if c.chip.is_apa102() { encode::apa102_len(c.pixels as usize) } else { c.wire_len() };
            (c.clock_hz, if c.chip.is_apa102() { SpiKind::Apa102 } else { SpiKind::Ws2801 }, len)
        } else {
            let f = SpiFit::find(&c.timing, spi_clock).ok_or("SPI can't make this timing within 150 ns")?;
            (f.clock_hz, SpiKind::OneWire(f), encode::ws_spi_len(&f, c.wire_len(), c.timing.reset_us))
        };
        if len > SPI_BUF {
            return Err("too long for the SPI lane's buffer at this timing");
        }
        if !node.take_pin(c.pin) {
            return Err("the data pin isn't free");
        }
        self.pins[0] = Some(c.pin);
        let clock = c.clock_pin.filter(|_| c.chip.clocked());
        if let Some(g) = clock {
            if !node.take_pin(g) {
                return Err("the clock pin isn't free");
            }
            self.pins[1] = Some(g);
        }
        let cfg = Config::default().with_frequency(Rate::from_hz(hz)).with_mode(Mode::_0);
        // SAFETY: the task owns SPI2 and DMA_CH0 (passed in at start); this lane is their
        // only user, and the previous driver was dropped in release().
        let spi = Spi::new(unsafe { SPI2::steal() }, cfg).map_err(|_| "the SPI clock is out of range")?.with_mosi(out(c.pin, c.invert));
        let spi = match clock {
            Some(g) => spi.with_sck(out(g, false)),
            None => spi,
        };
        self.spi = Some(spi.with_dma(unsafe { DMA_CH0::steal() }).into_async());
        self.kind = kind;
        Ok(())
    }

    fn render(&mut self, leds: &mut Leds, i: usize, c: &StringConfig, now: u64, scratch: &mut [u8]) {
        let Some(buf) = self.buf.as_mut() else { return };
        let out = buf.as_mut_slice();
        self.len = match self.kind {
            SpiKind::OneWire(f) => {
                let n = leds.render(i, now, scratch);
                encode::ws_spi(&f, &scratch[..n], c.timing.reset_us, out).unwrap_or(0)
            }
            SpiKind::Apa102 => {
                let n = leds.render(i, now, scratch);
                encode::apa102(&scratch[..n], 31, out).unwrap_or(0)
            }
            SpiKind::Ws2801 => leds.render(i, now, out),
        };
    }

    fn black(&mut self, c: &StringConfig, scratch: &mut [u8]) {
        let Some(buf) = self.buf.as_mut() else { return };
        let out = buf.as_mut_slice();
        let zeros = &mut scratch[..c.wire_len().min(MAX_PIXELS * 4)];
        zeros.fill(0);
        self.len = match self.kind {
            SpiKind::OneWire(f) => encode::ws_spi(&f, zeros, c.timing.reset_us, out).unwrap_or(0),
            SpiKind::Apa102 => encode::apa102(zeros, 0, out).unwrap_or(0),
            SpiKind::Ws2801 => {
                out[..zeros.len()].fill(0);
                zeros.len()
            }
        };
    }

    async fn push(&mut self) {
        if self.len == 0 {
            return;
        }
        let (Some(spi), Some(mut buf)) = (self.spi.take(), self.buf.take()) else { return };
        buf.set_length(self.len);
        match spi.write(self.len, buf) {
            Ok(mut t) => {
                t.wait_for_done().await;
                let (spi, buf) = t.wait();
                self.spi = Some(spi);
                self.buf = Some(buf);
            }
            Err((e, spi, buf)) => {
                warn!("leds: SPI write failed: {:?}", e);
                self.spi = Some(spi);
                self.buf = Some(buf);
            }
        }
    }
}

struct RmtLane {
    ch: Option<Channel<'static, Async, Tx>>,
    codes: &'static mut [PulseCode; RMT_MAX_BITS + 1],
    /// (high, low) ticks for a 0 bit and a 1 bit.
    zero: (u16, u16),
    one: (u16, u16),
    pin: Option<u8>,
    len: usize,
}

impl RmtLane {
    fn release(&mut self, node: &mut C6Node) {
        self.ch = None;
        if let Some(g) = self.pin.take() {
            node.return_pin(g);
        }
        self.len = 0;
    }

    fn claim(&mut self, node: &mut C6Node, c: &StringConfig) -> Result<(), &'static str> {
        if c.chip.clocked() {
            return Err("clocked strings need the SPI lane");
        }
        if c.wire_len() * 8 > RMT_MAX_BITS {
            return Err("too long for the RMT lane");
        }
        let t = &c.timing;
        self.zero = (ticks(t.t0h_ns), ticks(t.period_ns - t.t0h_ns));
        self.one = (ticks(t.t1h_ns), ticks(t.period_ns - t.t1h_ns));
        if !node.take_pin(c.pin) {
            return Err("the data pin isn't free");
        }
        self.pin = Some(c.pin);
        // Three of the four 48-code memory blocks (the RX channels' are unused), so Wi-Fi
        // interrupts have longer before a refill is late.
        let cfg = TxChannelConfig::default().with_clk_divider(1).with_idle_output(true).with_idle_output_level(Level::Low).with_memsize(3);
        // SAFETY: the task owns channel 1's creator (passed in at start); this lane is its
        // only user, and the previous channel was dropped in release().
        let creator = unsafe { ChannelCreator::<'static, Async, 1>::steal() };
        let ch = creator.configure_tx(&cfg).map_err(|_| "the RMT channel's memory is taken")?;
        self.ch = Some(ch.with_pin(out(c.pin, c.invert)));
        Ok(())
    }

    fn encode(&mut self, bytes: &[u8]) {
        let mut k = 0;
        for &b in bytes {
            for i in (0..8).rev() {
                let (h, l) = if b >> i & 1 != 0 { self.one } else { self.zero };
                self.codes[k] = PulseCode::new(Level::High, h, Level::Low, l);
                k += 1;
            }
        }
        self.codes[k] = PulseCode::end_marker();
        self.len = k + 1;
    }

    async fn push(&mut self) {
        if let (Some(ch), true) = (self.ch.as_mut(), self.len > 1)
            && let Err(e) = ch.transmit(&self.codes[..self.len]).await
        {
            warn!("leds: RMT transmit failed: {:?}", e);
        }
    }
}

#[derive(Clone, Copy, Default)]
struct SlotState {
    config_gen: Option<u32>,
    content_gen: u32,
    lane: Option<LaneId>,
    active: Option<StringConfig>,
}

pub struct Buffers {
    codes: [PulseCode; RMT_MAX_BITS + 1],
    scratch: [u8; MAX_PIXELS * 4],
}

impl Buffers {
    pub const fn new() -> Self {
        Buffers { codes: [PulseCode::end_marker(); RMT_MAX_BITS + 1], scratch: [0; MAX_PIXELS * 4] }
    }
}

/// The lane for a new string: RMT for one-wire strings that fit, SPI otherwise.
fn choose(c: &StringConfig, used: [bool; 2]) -> Result<LaneId, &'static str> {
    let fits_rmt = !c.chip.clocked() && c.wire_len() * 8 <= RMT_MAX_BITS;
    match (fits_rmt, used) {
        (true, [_, false]) => Ok(LaneId::Rmt),
        (_, [false, _]) => Ok(LaneId::Spi),
        (false, _) if c.chip.clocked() => Err("the SPI lane is in use (clocked strings need it)"),
        (false, _) => Err("the SPI lane is in use, and the string is too long for the RMT lane"),
        (true, _) => Err("both lanes are in use"),
    }
}

#[embassy_executor::task]
pub async fn task(
    shared: &'static SharedNode,
    // Owned so nothing else can make another SPI2/DMA_CH0/RMT channel 1 driver; the lanes
    // make theirs from stolen handles each time a string is set up.
    _spi: SPI2<'static>,
    _dma: DMA_CH0<'static>,
    _rmt1: ChannelCreator<'static, Async, 1>,
    buf: DmaTxBuf,
    bufs: &'static mut Buffers,
) -> ! {
    let Buffers { codes, scratch } = bufs;
    let mut spi = SpiLane { spi: None, buf: Some(buf), kind: SpiKind::Ws2801, pins: [None; 2], len: 0 };
    let mut rmt = RmtLane { ch: None, codes, zero: (0, 0), one: (0, 0), pin: None, len: 0 };
    let mut slots = [SlotState::default(); MAX_STRINGS];
    info!("leds: SPI2 and RMT channel 1 ready for {} strings", LEDS.max_strings);
    loop {
        let mut go = [false; 2];
        {
            let mut g = shared.lock().await;
            let Shared { api, node } = &mut *g;
            let now = Instant::now().as_millis();
            for i in 0..MAX_STRINGS {
                let job: Option<Job> = api.leds.job(i);
                let st = &mut slots[i];
                let want = job.map(|j| j.config_gen);
                let reconfigured = want != st.config_gen;
                if reconfigured {
                    // Blank the old string before letting go of its pins.
                    if let (Some(lane), Some(old)) = (st.lane.take(), st.active.take()) {
                        match lane {
                            LaneId::Spi => {
                                spi.black(&old, scratch);
                                spi.push().await;
                                spi.release(node);
                            }
                            LaneId::Rmt => {
                                scratch[..old.wire_len()].fill(0);
                                rmt.encode(&scratch[..old.wire_len()]);
                                rmt.push().await;
                                rmt.release(node);
                            }
                        }
                    }
                    st.config_gen = want;
                }
                if let Some(j) = job.filter(|_| reconfigured) {
                    let used = [LaneId::Spi, LaneId::Rmt].map(|l| slots.iter().any(|s| s.lane == Some(l)));
                    let st = &mut slots[i];
                    let r = choose(&j.config, used).and_then(|l| {
                        let r = match l {
                            LaneId::Spi => spi.claim(node, &j.config),
                            LaneId::Rmt => rmt.claim(node, &j.config),
                        };
                        match r {
                            Ok(()) => {
                                st.lane = Some(l);
                                st.active = Some(j.config);
                                Ok(())
                            }
                            Err(e) => {
                                match l {
                                    LaneId::Spi => spi.release(node),
                                    LaneId::Rmt => rmt.release(node),
                                }
                                Err(e)
                            }
                        }
                    });
                    if let Err(e) = r {
                        warn!("leds: string {} not started: {}", j.id, e);
                    }
                    api.leds.report(i, j.config_gen, r);
                }
                let st = &mut slots[i];
                if let (Some(j), Some(c), Some(lane)) = (job, st.active, st.lane)
                    && (reconfigured || j.content_gen != st.content_gen || j.animated)
                {
                    match lane {
                        LaneId::Spi => spi.render(&mut api.leds, i, &c, now, scratch),
                        LaneId::Rmt => {
                            let n = api.leds.render(i, now, scratch);
                            rmt.encode(&scratch[..n]);
                        }
                    }
                    st.content_gen = j.content_gen;
                    go[lane as usize] = true;
                }
            }
        }
        match go {
            [true, true] => {
                embassy_futures::join::join(spi.push(), rmt.push()).await;
            }
            [true, false] => spi.push().await,
            [false, true] => rmt.push().await,
            [false, false] => {}
        }
        // Covers every latch/reset time (≤ 10 ms) and caps animation at 50 fps.
        Timer::after_millis(FRAME_MS).await;
    }
}

/// The status LED: the Wi-Fi state, or white while held. Sent on every change, and
/// again every second in case a glitch on the line changed it.
#[embassy_executor::task]
pub async fn status_task(mut ch: Channel<'static, Async, Tx>) -> ! {
    // WS2812B timing, as RMT ticks.
    let (zero, one) = ((ticks(400), ticks(850)), (ticks(800), ticks(450)));
    let mut codes = [PulseCode::end_marker(); 25];
    let mut last = None;
    let mut tick: u32 = 0;
    loop {
        let now = Instant::now().as_millis();
        // GRB, kept dim: the LED sits next to the board's other parts and is very bright.
        let grb: [u8; 3] = if LED_HOLD.held(now) {
            [96, 96, 96]
        } else {
            match net::get().state {
                WifiState::Connected => [12, 0, 0],
                WifiState::Connecting if tick.is_multiple_of(2) => [10, 24, 0],
                WifiState::Unprovisioned if tick % 8 < 2 => [0, 0, 24],
                _ => [0, 0, 0],
            }
        };
        if last != Some(grb) || tick.is_multiple_of(8) {
            for (k, bit) in (0..24).map(|b| grb[b / 8] >> (7 - b % 8) & 1 != 0).enumerate() {
                let (h, l) = if bit { one } else { zero };
                codes[k] = PulseCode::new(Level::High, h, Level::Low, l);
            }
            if let Err(e) = ch.transmit(&codes).await {
                warn!("status LED: RMT transmit failed: {:?}", e);
            }
            last = Some(grb);
        }
        if let Either::First(()) = select(Timer::after_millis(125), LED_WAKE.wait()).await {
            tick = tick.wrapping_add(1);
        }
    }
}
