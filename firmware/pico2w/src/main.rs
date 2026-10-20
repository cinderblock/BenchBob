//! BenchBob node firmware for the Raspberry Pi Pico 2 W.
//!
//! - Wi-Fi (CYW43439 over PIO SPI) with DHCP; credentials from the config region.
//! - HTTP on port 80: the node API and MCP (two connections at a time).
//! - mDNS: `<hostname>.local`, `_benchbob._tcp`, `_http._tcp`.
//! - Improv Wi-Fi over USB CDC for provisioning.

#![no_std]
#![no_main]

mod board;
mod usb;
mod wifi;

use benchbob_node::http::{self, Shared};
use benchbob_node::improv::{self, DeviceInfo};
use benchbob_node::net;
use benchbob_node::store::ConfigStore;
use board::PicoNode;
use cyw43::aligned_bytes;
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use defmt::{info, unwrap};
use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use embassy_rp::clocks::RoscRng;
use embassy_rp::flash::{Blocking, Flash};
use embassy_rp::gpio::{Flex, Level, Output};
use embassy_rp::i2c::{self, I2c};
use embassy_rp::peripherals::{DMA_CH0, FLASH, PIO0, USB};
use embassy_rp::pio::{self, Pio};
use embassy_rp::{bind_interrupts, dma};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

/// Flash size the firmware assumes (the Pico 2 W has 4 MiB; bigger chips work too).
pub const FLASH_SIZE: usize = 4 * 1024 * 1024;
/// Offset of the config region from the start of flash: the last 8 KiB of 4 MiB
/// (0x103FE000 on the bus). Must match memory.x and firmware/tools.
pub const CONFIG_OFFSET: u32 = 0x003F_E000;

pub type NodeFlash = Flash<'static, FLASH, Blocking, FLASH_SIZE>;
type SharedNode = Mutex<CriticalSectionRawMutex, Shared<PicoNode>>;

#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"BenchBob node"),
    embassy_rp::binary_info::rp_program_description!(c"BenchBob node firmware: GPIO and I2C over HTTP and MCP, Improv Wi-Fi over USB"),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>;
    USBCTRL_IRQ => embassy_rp::usb::InterruptHandler<USB>;
});

static DEVICE: DeviceInfo = DeviceInfo { firmware: "BenchBob node", version: benchbob_proto::VERSION, chip: "RP2350" };

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task(pool_size = 2)]
async fn http_task(stack: Stack<'static>, shared: &'static SharedNode, bufs: &'static mut http::Buffers) -> ! {
    http::serve(stack, shared, 80, bufs).await
}

#[embassy_executor::task]
async fn mdns_task(stack: Stack<'static>, bufs: &'static mut benchbob_node::mdns::Buffers) -> ! {
    const TXT: &[&str] = &["board=pico2w", concat!("version=", env!("CARGO_PKG_VERSION")), "api=/api", "mcp=/mcp"];
    benchbob_node::mdns::run(stack, TXT, bufs).await
}

#[embassy_executor::task]
async fn improv_task(serial: usb::Serial, mut store: ConfigStore<NodeFlash>) -> ! {
    let usb::Serial { mut tx, mut rx } = serial;
    loop {
        rx.wait_connection().await;
        info!("usb: serial connected");
        improv::run(&mut rx, &mut tx, &mut store, &DEVICE).await;
        info!("usb: serial disconnected");
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    info!("BenchBob node {} (Pico 2 W)", benchbob_proto::VERSION);

    let store = ConfigStore::new(Flash::new_blocking(p.FLASH), CONFIG_OFFSET);
    let config = store.config().copied().unwrap_or_default();

    // Wi-Fi chip.
    let fw = aligned_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = aligned_bytes!("../cyw43-firmware/43439A0_clm.bin");
    let nvram = aligned_bytes!("../cyw43-firmware/nvram_rp2040.bin");
    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(&mut pio.common, pio.sm0, RM2_CLOCK_DIVIDER, pio.irq0, cs, p.PIN_24, p.PIN_29, dma::Channel::new(p.DMA_CH0, Irqs));
    static CYW43: StaticCell<cyw43::State> = StaticCell::new();
    let (net_device, mut control, runner) = cyw43::new(CYW43.init(cyw43::State::new()), pwr, spi, fw, nvram).await;
    spawner.spawn(unwrap!(cyw43_task(runner)));
    control.init(clm).await;
    // Power saving delays and drops multicast (mDNS); a bench node is on USB power.
    control.set_power_management(cyw43::PowerManagementMode::None).await;
    if control.add_multicast_address(benchbob_node::mdns::MDNS_MAC).await.is_err() {
        defmt::warn!("wifi: couldn't add the mDNS multicast address");
    }
    let mac = control.address().await;

    let hostname = if config.hostname.is_empty() { net::default_hostname(mac) } else { config.hostname };
    info!("hostname {}", hostname.as_str());
    net::update(|n| {
        n.mac = mac;
        n.hostname = hostname;
        n.ssid = config.ssid;
    });

    // Network stack. DHCP sends the hostname (embassy-net allows 32 characters).
    let mut dhcp = embassy_net::DhcpConfig::default();
    let h = hostname.as_str();
    dhcp.hostname = heapless::String::try_from(&h[..h.len().min(32)]).ok();
    static RESOURCES: StaticCell<StackResources<6>> = StaticCell::new();
    let seed = RoscRng.next_u64();
    let (stack, net_runner) = embassy_net::new(net_device, embassy_net::Config::dhcpv4(dhcp), RESOURCES.init(StackResources::new()), seed);
    spawner.spawn(unwrap!(net_task(net_runner)));
    spawner.spawn(unwrap!(wifi::task(control, stack, improv::credentials(&config))));

    // Board I/O.
    let mut i2c_cfg = i2c::Config::default();
    i2c_cfg.frequency = board::BUSES[0].hz;
    let i2c0 = I2c::new_blocking(p.I2C0, p.PIN_5, p.PIN_4, i2c_cfg);
    let mut pins: board::Pins = core::array::from_fn(|_| None);
    macro_rules! free {
        ($($n:literal => $pin:ident),* $(,)?) => { $( pins[$n] = Some(Flex::new(p.$pin)); )* };
    }
    free!(0 => PIN_0, 1 => PIN_1, 2 => PIN_2, 3 => PIN_3, 6 => PIN_6, 7 => PIN_7, 8 => PIN_8, 9 => PIN_9,
        10 => PIN_10, 11 => PIN_11, 12 => PIN_12, 13 => PIN_13, 14 => PIN_14, 15 => PIN_15, 16 => PIN_16,
        17 => PIN_17, 18 => PIN_18, 19 => PIN_19, 20 => PIN_20, 21 => PIN_21, 22 => PIN_22, 26 => PIN_26,
        27 => PIN_27, 28 => PIN_28);
    static SHARED: StaticCell<SharedNode> = StaticCell::new();
    let shared: &'static SharedNode = SHARED.init(Mutex::new(Shared::new(PicoNode::new(pins, i2c0))));

    static HTTP_BUFS: StaticCell<[http::Buffers; 2]> = StaticCell::new();
    let [b0, b1] = HTTP_BUFS.init([const { http::Buffers::new() }; 2]);
    spawner.spawn(unwrap!(http_task(stack, shared, b0)));
    spawner.spawn(unwrap!(http_task(stack, shared, b1)));
    static MDNS_BUFS: StaticCell<benchbob_node::mdns::Buffers> = StaticCell::new();
    spawner.spawn(unwrap!(mdns_task(stack, MDNS_BUFS.init(benchbob_node::mdns::Buffers::new()))));

    // USB: CDC ACM for Improv.
    let (device, serial) = usb::init(p.USB, Irqs);
    spawner.spawn(unwrap!(usb::device_task(device)));
    spawner.spawn(unwrap!(improv_task(serial, store)));
}
