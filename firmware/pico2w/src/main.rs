//! BenchBob node firmware for the Raspberry Pi Pico 2 W.
//!
//! - Wi-Fi (CYW43439 over PIO SPI) with DHCP; credentials from the config region.
//! - HTTP on port 80: the web UI (from flash), the node API and MCP (four connections
//!   at a time: a browser loads the page's files in parallel, and a connection arriving
//!   while every task is busy is refused).
//! - mDNS: `<hostname>.local`, `_benchbob._tcp`, `_http._tcp`.
//! - Improv Wi-Fi over USB CDC for provisioning.
//! - Addressable LED strings (WS2812 family, APA102, WS2801) on PIO1, any free GPIO.

#![no_std]
#![no_main]

mod board;
mod leds;
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
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, DMA_CH2, DMA_CH3, DMA_CH4, FLASH, PIO0, PIO1, USB};
use embassy_rp::pio::{self, Pio};
use embassy_rp::{bind_interrupts, dma};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use static_cell::{ConstStaticCell, StaticCell};
use {defmt_rtt as _, panic_probe as _};

/// Flash size the firmware assumes (the Pico 2 W has 4 MiB; bigger chips work too).
pub const FLASH_SIZE: usize = 4 * 1024 * 1024;
/// Offset of the config region from the start of flash: the last 8 KiB of 4 MiB
/// (0x103FE000 on the bus). Must match memory.x and firmware/tools.
pub const CONFIG_OFFSET: u32 = 0x003F_E000;

pub type NodeFlash = Flash<'static, FLASH, Blocking, FLASH_SIZE>;
/// The config record, shared by Improv and the LED settings' saver.
type SharedStore = Mutex<CriticalSectionRawMutex, ConfigStore<NodeFlash>>;
/// HTTP server tasks (about 19 KiB of buffers each). The stack gets four more sockets
/// (DHCP, mDNS and two spare, as before).
const HTTP_TASKS: usize = 4;
pub type SharedNode = Mutex<CriticalSectionRawMutex, Shared<PicoNode>>;

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
    PIO1_IRQ_0 => pio::InterruptHandler<PIO1>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>, dma::InterruptHandler<DMA_CH2>, dma::InterruptHandler<DMA_CH3>, dma::InterruptHandler<DMA_CH4>;
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

#[embassy_executor::task(pool_size = HTTP_TASKS)]
async fn http_task(stack: Stack<'static>, shared: &'static SharedNode, bufs: &'static mut http::Buffers) -> ! {
    http::serve(stack, shared, 80, bufs).await
}

#[embassy_executor::task]
async fn persist_task(shared: &'static SharedNode, store: &'static SharedStore) -> ! {
    benchbob_node::store::persist_leds(shared, store).await
}

#[embassy_executor::task]
async fn lights_task(stack: Stack<'static>, shared: &'static SharedNode, bufs: &'static mut benchbob_node::lights::Buffers) -> ! {
    benchbob_node::lights::run(stack, shared, bufs).await
}

#[embassy_executor::task]
async fn mdns_task(stack: Stack<'static>, bufs: &'static mut benchbob_node::mdns::Buffers) -> ! {
    const TXT: &[&str] = &["board=pico2w", concat!("version=", env!("CARGO_PKG_VERSION")), "api=/api", "mcp=/mcp"];
    benchbob_node::mdns::run(stack, TXT, bufs).await
}

#[embassy_executor::task]
async fn improv_task(serial: usb::Serial, store: &'static SharedStore) -> ! {
    let usb::Serial { mut tx, mut rx } = serial;
    loop {
        rx.wait_connection().await;
        info!("usb: serial connected");
        improv::run(&mut rx, &mut tx, store, &DEVICE).await;
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
    let spi =
        PioSpi::new(&mut pio.common, pio.sm0, RM2_CLOCK_DIVIDER, pio.irq0, cs, p.PIN_24, p.PIN_29, dma::Channel::new(p.DMA_CH0, Irqs));
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
    // + DHCP, mDNS, two spare, and the three LED lighting receivers (sACN, Art-Net, DDP).
    static RESOURCES: StaticCell<StackResources<{ HTTP_TASKS + 7 }>> = StaticCell::new();
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
    let mut node = Shared::new(PicoNode::new(pins, i2c0));
    // LED strings come back as they were saved (dark until output control is turned on).
    store.restore_leds(&mut node.api.leds, &board::INFO);
    let shared: &'static SharedNode = SHARED.init(Mutex::new(node));
    static STORE: StaticCell<SharedStore> = StaticCell::new();
    let store: &'static SharedStore = STORE.init(Mutex::new(store));
    spawner.spawn(unwrap!(persist_task(shared, store)));

    static HTTP_BUFS: StaticCell<[http::Buffers; HTTP_TASKS]> = StaticCell::new();
    for b in HTTP_BUFS.init([const { http::Buffers::new() }; HTTP_TASKS]) {
        spawner.spawn(unwrap!(http_task(stack, shared, b)));
    }
    // LED strings: PIO1, one DMA channel per state machine.
    let dmas = [
        dma::Channel::new(p.DMA_CH1, Irqs),
        dma::Channel::new(p.DMA_CH2, Irqs),
        dma::Channel::new(p.DMA_CH3, Irqs),
        dma::Channel::new(p.DMA_CH4, Irqs),
    ];
    // Const-initialised: ~21 KiB that shouldn't pass through the stack.
    static LED_BUFS: ConstStaticCell<leds::Buffers> = ConstStaticCell::new(leds::Buffers::new());
    spawner.spawn(unwrap!(leds::task(shared, Pio::new(p.PIO1, Irqs), dmas, LED_BUFS.take())));

    static LIGHTS_BUFS: ConstStaticCell<benchbob_node::lights::Buffers> = ConstStaticCell::new(benchbob_node::lights::Buffers::new());
    spawner.spawn(unwrap!(lights_task(stack, shared, LIGHTS_BUFS.take())));

    static MDNS_BUFS: StaticCell<benchbob_node::mdns::Buffers> = StaticCell::new();
    spawner.spawn(unwrap!(mdns_task(stack, MDNS_BUFS.init(benchbob_node::mdns::Buffers::new()))));

    // USB: CDC ACM for Improv.
    let (device, serial) = usb::init(p.USB, Irqs);
    spawner.spawn(unwrap!(usb::device_task(device)));
    spawner.spawn(unwrap!(improv_task(serial, store)));
}
