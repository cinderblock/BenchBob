//! Addressable LED strings on PIO1: one state machine per string (up to four), on any
//! free header GPIO, each fed by its own DMA channel. PIO0 is the CYW43439's; PIO2 is
//! left for other features.
//!
//! - One-wire (WS2812 family): the Pico SDK's ws2812 program, with its three phase
//!   lengths and the state machine clock fitted to the string's timing
//!   ([`benchbob_pixels::timing::PioFit`]). Each string loads its own copy (4
//!   instructions), since the phase lengths are encoded in the instructions.
//! - Clocked (APA102, WS2801): `out pins, 1 side 0; nop side 1`, two cycles per bit,
//!   data on the out pin and the clock on side-set.
//!
//! The FIFO is fed bytes by DMA: an 8-bit write to a TX FIFO is replicated across the
//! word, and the state machine shifts left with an 8-bit autopull threshold, so it takes
//! the top byte. A string's pins leave the GPIO API (the board forgets their `Flex`, so
//! dropping it can't reset the pin) and come back as fresh `Flex`es when it is removed.

use crate::SharedNode;
use crate::board::PicoNode;
use benchbob_node::http::Shared;
use benchbob_pixels::encode;
use benchbob_pixels::timing::{PioFit, Timing};
use benchbob_proto::api::leds::{Job, MAX_PIXELS, MAX_STRINGS, StringConfig};
use benchbob_proto::api::{Leds, LedsInfo};
use defmt::{info, warn};
use embassy_futures::join::join4;
use embassy_rp::clocks::clk_sys_freq;
use embassy_rp::dma;
use embassy_rp::peripherals::PIO1;
use embassy_rp::pio::program::{Assembler, JmpCondition, OutDestination, SideSet};
use embassy_rp::pio::{Common, Config, Direction, FifoJoin, InstanceMemory, Pin, Pio, ShiftConfig, ShiftDirection, StateMachine};
use embassy_time::{Instant, Timer};
use fixed::types::U24F8;

pub static LEDS: LedsInfo =
    LedsInfo { max_strings: 4, max_pixels: MAX_PIXELS as u16, one_wire: true, clocked: true, check_timing, max_clock_hz: 20_000_000 };

fn check_timing(t: &Timing) -> Result<(), &'static str> {
    PioFit::find(t).map(|_| ()).ok_or("the PIO program can't get within 150 ns of it (each phase is 1-16 cycles)")
}

/// The largest frame: an APA102 frame of MAX_PIXELS (one-wire RGBW needs 4 per pixel too).
const FRAME_MAX: usize = encode::apa102_len(MAX_PIXELS);
/// How often animated strings get a new frame.
const FRAME_MS: u64 = 20;

pub struct Buffers {
    frames: [[u8; FRAME_MAX]; MAX_STRINGS],
    scratch: [u8; MAX_PIXELS * 4],
}

impl Buffers {
    pub const fn new() -> Self {
        Buffers { frames: [[0; FRAME_MAX]; MAX_STRINGS], scratch: [0; MAX_PIXELS * 4] }
    }
}

/// One state machine and what it's driving.
struct Lane<const SM: usize> {
    sm: StateMachine<'static, PIO1, SM>,
    dma: dma::Channel<'static>,
    program: Option<InstanceMemory<'static, PIO1>>,
    /// The config generation this lane last set up (None: idle).
    config_gen: Option<u32>,
    content_gen: u32,
    /// What is set up, when it worked.
    active: Option<StringConfig>,
    pins: [Option<u8>; 2],
    len: usize,
}

impl<const SM: usize> Lane<SM> {
    fn new(sm: StateMachine<'static, PIO1, SM>, dma: dma::Channel<'static>) -> Self {
        Lane { sm, dma, program: None, config_gen: None, content_gen: 0, active: None, pins: [None; 2], len: 0 }
    }

    fn release(&mut self, common: &mut Common<'static, PIO1>, node: &mut PicoNode) {
        self.sm.set_enable(false);
        self.sm.clear_fifos();
        if let Some(mem) = self.program.take() {
            // SAFETY: this lane's state machine is stopped and nothing else runs the program.
            unsafe { common.free_instr(mem) };
        }
        for g in self.pins.iter_mut().filter_map(Option::take) {
            node.return_pin(g);
        }
        self.active = None;
        self.len = 0;
    }

    /// Set the lane up for `c`. On an error the caller releases it (which returns any
    /// pins already taken).
    fn claim(&mut self, common: &mut Common<'static, PIO1>, node: &mut PicoNode, c: &StringConfig) -> Result<(), &'static str> {
        let sys = clk_sys_freq() as f32;
        let fit = if c.chip.clocked() { None } else { Some(PioFit::find(&c.timing).ok_or("the PIO program can't make this timing")?) };
        let divider = match fit {
            Some(f) => sys / f.cycle_hz as f32,
            None => sys / (2.0 * c.clock_hz as f32),
        };
        if divider < 1.0 {
            return Err("too fast for the system clock");
        }
        if divider >= 65536.0 {
            return Err("too slow for the PIO clock divider");
        }

        if !node.take_pin(c.pin) {
            return Err("the data pin isn't free");
        }
        self.pins[0] = Some(c.pin);
        let mut data = pio_pin(common, c.pin).ok_or("the data pin can't be used by PIO")?;
        data.set_output_inversion(c.invert);
        let clock = match c.clock_pin.filter(|_| c.chip.clocked()) {
            Some(g) => {
                if !node.take_pin(g) {
                    return Err("the clock pin isn't free");
                }
                self.pins[1] = Some(g);
                Some(pio_pin(common, g).ok_or("the clock pin can't be used by PIO")?)
            }
            None => None,
        };

        let mut a: Assembler<32> = Assembler::new_with_side_set(SideSet::new(false, 1, false));
        let mut top = a.label();
        let mut end = a.label();
        a.bind(&mut top);
        match fit {
            None => {
                // Clock low while shifting the bit out (and while stalled), high to latch it.
                a.out_with_delay_and_side_set(OutDestination::PINS, 1, 0, 0);
                a.nop_with_delay_and_side_set(0, 1);
            }
            Some(f) => {
                // The SDK's ws2812 program: low for t3 (stalls here when idle), high for t1,
                // then high (1) or low (0) for t2.
                let mut zero = a.label();
                a.out_with_delay_and_side_set(OutDestination::X, 1, f.t3 - 1, 0);
                a.jmp_with_delay_and_side_set(JmpCondition::XIsZero, &mut zero, f.t1 - 1, 1);
                a.jmp_with_delay_and_side_set(JmpCondition::Always, &mut top, f.t2 - 1, 1);
                a.bind(&mut zero);
                a.nop_with_delay_and_side_set(f.t2 - 1, 0);
            }
        }
        a.bind(&mut end);
        // At most 4 instructions per string, 4 strings, 32 slots: always fits.
        let loaded = common.load_program(&a.assemble_with_wrap(end, top));

        let mut cfg = Config::default();
        match &clock {
            Some(clk) => {
                cfg.use_program(&loaded, &[clk]);
                cfg.set_out_pins(&[&data]);
                self.sm.set_pin_dirs(Direction::Out, &[&data, clk]);
            }
            None => {
                cfg.use_program(&loaded, &[&data]);
                self.sm.set_pin_dirs(Direction::Out, &[&data]);
            }
        }
        cfg.clock_divider = U24F8::from_num(divider);
        cfg.fifo_join = FifoJoin::TxOnly;
        cfg.shift_out = ShiftConfig { auto_fill: true, threshold: 8, direction: ShiftDirection::Left };
        self.sm.set_config(&cfg);
        self.sm.set_enable(true);
        self.program = Some(loaded.used_memory);
        self.active = Some(*c);
        Ok(())
    }

    async fn push(&mut self, frame: &[u8], go: bool) {
        if go && !frame.is_empty() {
            self.sm.tx().dma_push(&mut self.dma, frame, false).await;
        }
    }
}

/// Makes an embassy PIO pin from a GPIO number (every free header pin).
fn pio_pin(common: &mut Common<'static, PIO1>, gpio: u8) -> Option<Pin<'static, PIO1>> {
    use embassy_rp::peripherals::*;
    macro_rules! pins {
        ($($n:literal => $p:ident),* $(,)?) => {
            match gpio {
                // SAFETY: the board has forgotten this pin's Flex (take_pin), so this is
                // the only handle to it until return_pin.
                $( $n => Some(common.make_pio_pin(unsafe { $p::steal() })), )*
                _ => None,
            }
        };
    }
    pins!(0 => PIN_0, 1 => PIN_1, 2 => PIN_2, 3 => PIN_3, 6 => PIN_6, 7 => PIN_7, 8 => PIN_8, 9 => PIN_9,
        10 => PIN_10, 11 => PIN_11, 12 => PIN_12, 13 => PIN_13, 14 => PIN_14, 15 => PIN_15, 16 => PIN_16,
        17 => PIN_17, 18 => PIN_18, 19 => PIN_19, 20 => PIN_20, 21 => PIN_21, 22 => PIN_22, 26 => PIN_26,
        27 => PIN_27, 28 => PIN_28)
}

/// Render slot `i` into `frame` in the wire format its chip needs. Returns the length.
fn render(leds: &mut Leds, i: usize, c: &StringConfig, now: u64, frame: &mut [u8], scratch: &mut [u8]) -> usize {
    if c.chip.is_apa102() {
        let n = leds.render(i, now, scratch);
        encode::apa102(&scratch[..n], 31, frame).unwrap_or(0)
    } else {
        leds.render(i, now, frame)
    }
}

/// A frame of black for `c` (sent when a string is removed, before its pins go back).
fn black(c: &StringConfig, frame: &mut [u8]) -> usize {
    let n = c.wire_len().min(MAX_PIXELS * 4);
    if c.chip.is_apa102() {
        let leds = n / 3;
        let len = encode::apa102_len(leds);
        frame[..len].fill(0);
        for k in 0..leds {
            frame[4 + k * 4] = 0xE0;
        }
        len
    } else {
        frame[..n].fill(0);
        n
    }
}

#[embassy_executor::task]
pub async fn task(shared: &'static SharedNode, pio: Pio<'static, PIO1>, dmas: [dma::Channel<'static>; 4], bufs: &'static mut Buffers) -> ! {
    let Pio { mut common, sm0, sm1, sm2, sm3, .. } = pio;
    let [d0, d1, d2, d3] = dmas;
    let mut l0 = Lane::new(sm0, d0);
    let mut l1 = Lane::new(sm1, d1);
    let mut l2 = Lane::new(sm2, d2);
    let mut l3 = Lane::new(sm3, d3);
    let Buffers { frames, scratch } = bufs;
    info!("leds: PIO1 ready for {} strings", MAX_STRINGS);
    loop {
        let mut go = [false; MAX_STRINGS];
        {
            let mut g = shared.lock().await;
            let Shared { api, node } = &mut *g;
            let now = Instant::now().as_millis();
            // The same steps for each lane; the lanes are different types (one per SM).
            macro_rules! step {
                ($lane:ident, $i:literal) => {{
                    let lane = &mut $lane;
                    let job: Option<Job> = api.leds.job($i);
                    let want = job.map(|j| j.config_gen);
                    let reconfigured = want != lane.config_gen;
                    if reconfigured {
                        if let Some(old) = lane.active {
                            // Blank it before letting go, so a removed string goes dark.
                            let n = black(&old, &mut frames[$i]);
                            lane.push(&frames[$i][..n], true).await;
                            Timer::after_millis(1).await;
                        }
                        lane.release(&mut common, node);
                        lane.config_gen = want;
                        if let Some(j) = job {
                            let r = lane.claim(&mut common, node, &j.config);
                            if let Err(e) = r {
                                warn!("leds: string {} not started: {}", j.id, e);
                                lane.release(&mut common, node);
                            }
                            api.leds.report($i, j.config_gen, r);
                        }
                    }
                    if let (Some(j), Some(c)) = (job, lane.active)
                        && (reconfigured || j.content_gen != lane.content_gen || j.animated)
                    {
                        lane.len = render(&mut api.leds, $i, &c, now, &mut frames[$i], scratch);
                        lane.content_gen = j.content_gen;
                        go[$i] = true;
                    }
                }};
            }
            step!(l0, 0);
            step!(l1, 1);
            step!(l2, 2);
            step!(l3, 3);
        }
        let [f0, f1, f2, f3] = &frames;
        join4(l0.push(&f0[..l0.len], go[0]), l1.push(&f1[..l1.len], go[1]), l2.push(&f2[..l2.len], go[2]), l3.push(&f3[..l3.len], go[3]))
            .await;
        // Covers every latch/reset time (≤ 10 ms) and caps animation at 50 fps.
        Timer::after_millis(FRAME_MS).await;
    }
}
