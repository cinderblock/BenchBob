//! The ESP32-C6's pins and I²C bus behind `benchbob_proto::api::Node`. Pin notes follow
//! the ESP32-C6-DevKitC-1; other boards expose a subset.

use benchbob_node::net::{self, NetState};
use benchbob_proto::api::{BoardInfo, I2cBusInfo, I2cFault, Mode, Node, PinInfo, PinState, Pull, Status};
use esp_hal::Blocking;
use esp_hal::gpio::{self, Flex, InputConfig, Level};
use esp_hal::i2c::master::{AcknowledgeCheckFailedReason, Error as I2cError, I2c};
use embassy_time::Instant;

pub const I2C0_SDA: u8 = 6;
pub const I2C0_SCL: u8 = 7;

const fn free(gpio: u8, note: Option<&'static str>) -> PinInfo {
    PinInfo { gpio, header: None, func: None, note }
}

const fn taken(gpio: u8, func: &'static str) -> PinInfo {
    PinInfo { gpio, header: None, func: Some(func), note: None }
}

const STRAP: Option<&str> = Some("strapping pin: keep it free at reset");

/// GPIO 24–30 drive the module's flash and GPIO 14 isn't brought out, so they aren't listed.
pub static PINS: [PinInfo; 23] = [
    free(0, None),
    free(1, None),
    free(2, None),
    free(3, None),
    free(4, STRAP),
    free(5, STRAP),
    taken(I2C0_SDA, "I2C0 SDA"),
    taken(I2C0_SCL, "I2C0 SCL"),
    free(8, Some("strapping pin; the DevKitC-1's RGB LED")),
    free(9, Some("strapping pin; the BOOT button")),
    free(10, None),
    free(11, None),
    taken(12, "USB D- (USB-Serial-JTAG)"),
    taken(13, "USB D+ (USB-Serial-JTAG)"),
    free(15, STRAP),
    taken(16, "UART0 TX (log)"),
    taken(17, "UART0 RX (log)"),
    free(18, None),
    free(19, None),
    free(20, None),
    free(21, None),
    free(22, None),
    free(23, None),
];

pub static BUSES: [I2cBusInfo; 1] = [I2cBusInfo {
    bus: 0,
    label: "I2C0 (GPIO6 SDA, GPIO7 SCL)",
    sda: I2C0_SDA,
    scl: I2C0_SCL,
    hz: 100_000,
    // esp-hal refuses zero-length writes, so scans read a byte everywhere.
    can_quick: false,
}];

pub static INFO: BoardInfo = BoardInfo {
    board: "esp32c6",
    board_name: "ESP32-C6",
    chip: "ESP32-C6",
    firmware_version: benchbob_proto::VERSION,
    build: benchbob_node::web::BUILD,
    pins: &PINS,
    i2c: &BUSES,
};

pub type Pins = [Option<Flex<'static>>; 24];

pub struct C6Node {
    pins: Pins,
    shadow: [(Mode, Pull); 24],
    i2c0: I2c<'static, Blocking>,
    net: NetState,
}

impl C6Node {
    /// Free pins start as inputs without pulls.
    pub fn new(mut pins: Pins, i2c0: I2c<'static, Blocking>) -> Self {
        for p in pins.iter_mut().flatten() {
            p.apply_input_config(&InputConfig::default().with_pull(gpio::Pull::None));
            p.set_output_enable(false);
            p.set_input_enable(true);
        }
        C6Node { pins, shadow: [(Mode::Input, Pull::None); 24], i2c0, net: net::get() }
    }
}

fn fault(e: I2cError) -> I2cFault {
    match e {
        I2cError::AcknowledgeCheckFailed(AcknowledgeCheckFailedReason::Address) => I2cFault::Nack,
        I2cError::AcknowledgeCheckFailed(_) => I2cFault::Nack,
        I2cError::Timeout => I2cFault::Timeout,
        I2cError::ArbitrationLost => I2cFault::Bus,
        _ => I2cFault::Other,
    }
}

impl Node for C6Node {
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
            heap: Some((esp_alloc::HEAP.free() as u32, esp_alloc::HEAP.used() as u32)),
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
            let hal = match pull {
                Pull::Up => gpio::Pull::Up,
                Pull::Down => gpio::Pull::Down,
                Pull::None => gpio::Pull::None,
            };
            p.apply_input_config(&InputConfig::default().with_pull(hal));
            s.1 = pull;
        }
        match mode {
            Some((Mode::Output, high)) => {
                p.set_level(if high { Level::High } else { Level::Low });
                p.set_output_enable(true);
                s.0 = Mode::Output;
            }
            Some((Mode::Input, _)) => {
                p.set_output_enable(false);
                s.0 = Mode::Input;
            }
            None => {}
        }
    }

    fn i2c_probe(&mut self, _bus: u8, addr: u8, _read: bool) -> Result<bool, I2cFault> {
        let mut b = [0u8; 1];
        match self.i2c0.read(addr, &mut b) {
            Ok(()) => Ok(true),
            Err(e) => match fault(e) {
                I2cFault::Nack => Ok(false),
                f => Err(f),
            },
        }
    }

    fn i2c_write_read(&mut self, _bus: u8, addr: u8, wr: &[u8], rd: &mut [u8]) -> Result<(), I2cFault> {
        let r = match (wr.is_empty(), rd.is_empty()) {
            (true, false) => self.i2c0.read(addr, rd),
            (false, true) => self.i2c0.write(addr, wr),
            (false, false) => self.i2c0.write_read(addr, wr, rd),
            (true, true) => return Ok(()),
        };
        r.map_err(fault)
    }
}
