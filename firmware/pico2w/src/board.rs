//! The Pico 2 W's pins and I²C bus behind `benchbob_proto::api::Node`.

use benchbob_node::net::{self, NetState};
use benchbob_proto::api::{BoardInfo, I2cBusInfo, I2cFault, Mode, Node, PinInfo, PinState, Pull, Status};
use embassy_rp::gpio::{self, Flex};
use embassy_rp::i2c::{self, AbortReason, Blocking, I2c};
use embassy_rp::peripherals::I2C0;
use embassy_time::Instant;

pub const I2C0_SDA: u8 = 4;
pub const I2C0_SCL: u8 = 5;

const fn pin(gpio: u8, header: u8) -> PinInfo {
    PinInfo { gpio, header: Some(header), func: None, note: None }
}

/// The GPIOs on the Pico 2 W's header (GP23–25 and GP29 belong to the CYW43439).
pub static PINS: [PinInfo; 26] = [
    pin(0, 1),
    pin(1, 2),
    pin(2, 4),
    pin(3, 5),
    PinInfo { gpio: I2C0_SDA, header: Some(6), func: Some("I2C0 SDA"), note: None },
    PinInfo { gpio: I2C0_SCL, header: Some(7), func: Some("I2C0 SCL"), note: None },
    pin(6, 9),
    pin(7, 10),
    pin(8, 11),
    pin(9, 12),
    pin(10, 14),
    pin(11, 15),
    pin(12, 16),
    pin(13, 17),
    pin(14, 19),
    pin(15, 20),
    pin(16, 21),
    pin(17, 22),
    pin(18, 24),
    pin(19, 25),
    pin(20, 26),
    pin(21, 27),
    pin(22, 29),
    PinInfo { gpio: 26, header: Some(31), func: None, note: Some("ADC0") },
    PinInfo { gpio: 27, header: Some(32), func: None, note: Some("ADC1") },
    PinInfo { gpio: 28, header: Some(34), func: None, note: Some("ADC2") },
];

pub static BUSES: [I2cBusInfo; 1] = [I2cBusInfo {
    bus: 0,
    label: "I2C0 (GP4 SDA, GP5 SCL; header pins 6/7)",
    sda: I2C0_SDA,
    scl: I2C0_SCL,
    hz: 100_000,
    // The DesignWare controller can't send an address-only transaction.
    can_quick: false,
}];

pub static INFO: BoardInfo = BoardInfo {
    board: "pico2w",
    board_name: "Raspberry Pi Pico 2 W",
    chip: "RP2350",
    firmware_version: benchbob_proto::VERSION,
    pins: &PINS,
    i2c: &BUSES,
};

/// Flex drivers for every free header GPIO, indexed by GPIO number.
pub type Pins = [Option<Flex<'static>>; 30];

pub struct PicoNode {
    pins: Pins,
    /// What we last set (Flex can't read direction and pulls back).
    shadow: [(Mode, Pull); 30],
    i2c0: I2c<'static, I2C0, Blocking>,
    net: NetState,
}

impl PicoNode {
    /// All free pins start as inputs without pulls. (RP2350 pads reset with a pull-down,
    /// and erratum RP2350-E9 makes that pull-down latch on some inputs.)
    pub fn new(mut pins: Pins, i2c0: I2c<'static, I2C0, Blocking>) -> Self {
        for p in pins.iter_mut().flatten() {
            p.set_pull(gpio::Pull::None);
            p.set_as_input();
        }
        PicoNode { pins, shadow: [(Mode::Input, Pull::None); 30], i2c0, net: net::get() }
    }
}

fn fault(e: i2c::Error) -> I2cFault {
    match e {
        i2c::Error::Abort(AbortReason::NoAcknowledge) => I2cFault::Nack,
        i2c::Error::Abort(AbortReason::ArbitrationLoss) => I2cFault::Bus,
        _ => I2cFault::Other,
    }
}

impl Node for PicoNode {
    fn info(&self) -> &'static BoardInfo {
        &INFO
    }

    fn status(&mut self) -> Status<'_> {
        self.net = net::get();
        Status {
            hostname: self.net.hostname.as_str(),
            uptime_ms: Instant::now().as_millis(),
            mac: self.net.mac,
            wifi_state: self.net.state,
            ssid: self.net.ssid.as_str(),
            rssi: self.net.rssi,
            ip: self.net.ip,
            heap: None, // no heap: everything is static
        }
    }

    fn gpio_read(&mut self, g: u8) -> PinState {
        let (mode, pull) = self.shadow[g as usize];
        let level = self.pins[g as usize].as_ref().is_some_and(|p| p.is_high());
        PinState { mode, level, pull }
    }

    fn gpio_write(&mut self, g: u8, mode: Option<(Mode, bool)>, pull: Option<Pull>) {
        let Some(p) = self.pins[g as usize].as_mut() else { return };
        let s = &mut self.shadow[g as usize];
        if let Some(pull) = pull {
            p.set_pull(match pull {
                Pull::Up => gpio::Pull::Up,
                Pull::Down => gpio::Pull::Down,
                Pull::None => gpio::Pull::None,
            });
            s.1 = pull;
        }
        match mode {
            Some((Mode::Output, high)) => {
                // Level first, so the pin doesn't glitch to the old level.
                p.set_level(if high { gpio::Level::High } else { gpio::Level::Low });
                p.set_as_output();
                s.0 = Mode::Output;
            }
            Some((Mode::Input, _)) => {
                p.set_as_input();
                s.0 = Mode::Input;
            }
            None => {}
        }
    }

    fn i2c_probe(&mut self, _bus: u8, addr: u8, _read: bool) -> Result<bool, I2cFault> {
        // Always a one-byte read (can_quick is false).
        let mut b = [0u8; 1];
        match self.i2c0.blocking_read(addr, &mut b) {
            Ok(()) => Ok(true),
            Err(e) => match fault(e) {
                I2cFault::Nack => Ok(false),
                f => Err(f),
            },
        }
    }

    fn i2c_write_read(&mut self, _bus: u8, addr: u8, wr: &[u8], rd: &mut [u8]) -> Result<(), I2cFault> {
        let r = match (wr.is_empty(), rd.is_empty()) {
            (true, false) => self.i2c0.blocking_read(addr, rd),
            (false, true) => self.i2c0.blocking_write(addr, wr),
            (false, false) => self.i2c0.blocking_write_read(addr, wr, rd),
            (true, true) => return Ok(()),
        };
        r.map_err(fault)
    }
}
