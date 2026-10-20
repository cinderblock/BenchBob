//! BenchBob node firmware for the ESP32-C6.
//!
//! - Wi-Fi station (esp-radio) with DHCP; credentials from the `bbconfig` partition.
//! - HTTP on port 80: the node API and MCP (two connections at a time).
//! - mDNS: `<hostname>.local`, `_benchbob._tcp`, `_http._tcp`.
//! - Improv Wi-Fi on the native USB port (USB-Serial-JTAG). Logs go to UART0 (GPIO16/17,
//!   the DevKitC-1's "UART" USB port) so they don't interleave with Improv packets.

#![no_std]
#![no_main]

extern crate alloc;

mod board;
mod wifi;

use benchbob_node::http::{self, Shared};
use benchbob_node::improv::{self, DeviceInfo};
use benchbob_node::net;
use benchbob_node::store::ConfigStore;
use board::C6Node;
use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::Timer;
use esp_backtrace as _;
use esp_bootloader_esp_idf::partitions;
use esp_hal::Async;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::Flex;
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::rng::Rng;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx};
use esp_radio::wifi::{ControllerConfig, Interface};
use esp_storage::FlashStorage;
use log::{info, warn};
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

/// Where partitions.csv puts `bbconfig`; used only if the table can't be read.
const CONFIG_OFFSET_DEFAULT: u32 = 0x3F_E000;

type SharedNode = Mutex<CriticalSectionRawMutex, Shared<C6Node>>;

static DEVICE: DeviceInfo = DeviceInfo { firmware: "BenchBob node", version: benchbob_proto::VERSION, chip: "ESP32-C6" };

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task(pool_size = 2)]
async fn http_task(stack: Stack<'static>, shared: &'static SharedNode, bufs: &'static mut http::Buffers) -> ! {
    http::serve(stack, shared, 80, bufs).await
}

#[embassy_executor::task]
async fn mdns_task(stack: Stack<'static>, bufs: &'static mut benchbob_node::mdns::Buffers) -> ! {
    const TXT: &[&str] = &["board=esp32c6", concat!("version=", env!("CARGO_PKG_VERSION")), "api=/api", "mcp=/mcp"];
    benchbob_node::mdns::run(stack, TXT, bufs).await
}

#[embassy_executor::task]
async fn improv_task(mut rx: UsbSerialJtagRx<'static, Async>, mut tx: UsbSerialJtagTx<'static, Async>, mut store: ConfigStore<FlashStorage<'static>>) -> ! {
    loop {
        improv::run(&mut rx, &mut tx, &mut store, &DEVICE).await;
        Timer::after_millis(100).await;
    }
}

/// Find the `bbconfig` data partition.
fn config_offset(flash: &mut FlashStorage<'static>) -> u32 {
    let mut mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    match partitions::read_partition_table(flash, &mut mem) {
        Ok(pt) => match pt.iter().find(|p| p.label_as_str() == "bbconfig") {
            Some(p) if p.len() >= 0x2000 => return p.offset(),
            _ => warn!("no bbconfig partition (flash with firmware/esp32c6/partitions.csv); using 0x{:x}", CONFIG_OFFSET_DEFAULT),
        },
        Err(e) => warn!("partition table unreadable ({:?}); using 0x{:x}", e, CONFIG_OFFSET_DEFAULT),
    }
    CONFIG_OFFSET_DEFAULT
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger_from_env();
    let p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    // esp-radio allocates; the reclaimed bootloader RAM goes first.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 48 * 1024);
    info!("BenchBob node {} (ESP32-C6)", benchbob_proto::VERSION);

    let timg0 = TimerGroup::new(p.TIMG0);
    let sw_int = SoftwareInterruptControl::new(p.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    let mut flash = FlashStorage::new(p.FLASH);
    let offset = config_offset(&mut flash);
    let store = ConfigStore::new(flash, offset);
    let config = store.config().copied().unwrap_or_default();

    // Wi-Fi.
    let (controller, interfaces) = esp_radio::wifi::new(p.WIFI, ControllerConfig::default()).expect("wifi init");
    let mac = interfaces.station.mac_address();
    let hostname = if config.hostname.is_empty() { net::default_hostname(mac) } else { config.hostname };
    info!("hostname {}", hostname.as_str());
    net::update(|n| {
        n.mac = mac;
        n.hostname = hostname;
        n.ssid = config.ssid;
    });

    let mut dhcp = embassy_net::DhcpConfig::default();
    let h = hostname.as_str();
    dhcp.hostname = heapless::String::try_from(&h[..h.len().min(32)]).ok();
    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;
    static RESOURCES: StaticCell<StackResources<6>> = StaticCell::new();
    let (stack, net_runner) = embassy_net::new(interfaces.station, embassy_net::Config::dhcpv4(dhcp), RESOURCES.init(StackResources::new()), seed);
    spawner.spawn(net_task(net_runner).expect("net task"));
    spawner.spawn(wifi::task(controller, stack, improv::credentials(&config)).expect("wifi task"));

    // Board I/O.
    let i2c0 = I2c::new(p.I2C0, I2cConfig::default().with_frequency(Rate::from_hz(board::BUSES[0].hz)))
        .expect("i2c config")
        .with_sda(p.GPIO6)
        .with_scl(p.GPIO7);
    let mut pins: board::Pins = core::array::from_fn(|_| None);
    macro_rules! free {
        ($($n:literal => $pin:ident),* $(,)?) => { $( pins[$n] = Some(Flex::new(p.$pin)); )* };
    }
    free!(0 => GPIO0, 1 => GPIO1, 2 => GPIO2, 3 => GPIO3, 4 => GPIO4, 5 => GPIO5, 8 => GPIO8, 9 => GPIO9,
        10 => GPIO10, 11 => GPIO11, 15 => GPIO15, 18 => GPIO18, 19 => GPIO19, 20 => GPIO20, 21 => GPIO21,
        22 => GPIO22, 23 => GPIO23);
    static SHARED: StaticCell<SharedNode> = StaticCell::new();
    let shared: &'static SharedNode = SHARED.init(Mutex::new(Shared::new(C6Node::new(pins, i2c0))));

    static HTTP_BUFS: StaticCell<[http::Buffers; 2]> = StaticCell::new();
    let [b0, b1] = HTTP_BUFS.init([const { http::Buffers::new() }; 2]);
    spawner.spawn(http_task(stack, shared, b0).expect("http task"));
    spawner.spawn(http_task(stack, shared, b1).expect("http task"));
    static MDNS_BUFS: StaticCell<benchbob_node::mdns::Buffers> = StaticCell::new();
    spawner.spawn(mdns_task(stack, MDNS_BUFS.init(benchbob_node::mdns::Buffers::new())).expect("mdns task"));

    // Improv on the native USB port.
    let (rx, tx) = UsbSerialJtag::new(p.USB_DEVICE).into_async().split();
    spawner.spawn(improv_task(rx, tx, store).expect("improv task"));
}
