//! USB: one CDC ACM serial port, used for Improv Wi-Fi provisioning.

use embassy_rp::Peri;
use embassy_rp::interrupt::typelevel::Binding;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_usb::class::cdc_acm::{BufferedReceiver, CdcAcmClass, CdcAcmError, Sender, State};
use embassy_usb::{Builder, UsbDevice};
use static_cell::StaticCell;

pub type UsbDriver = Driver<'static, USB>;

const MAX_PACKET: u16 = 64;

/// The serial port's two halves.
pub struct Serial {
    pub tx: Tx,
    pub rx: BufferedReceiver<'static, UsbDriver>,
}

/// The CDC sender, plus the zero-length packet a transfer needs when it ends exactly on a
/// packet boundary (otherwise the host waits for more).
pub struct Tx {
    inner: Sender<'static, UsbDriver>,
    needs_zlp: bool,
}

impl embedded_io_async::ErrorType for Tx {
    type Error = CdcAcmError;
}

impl embedded_io_async::Write for Tx {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let n = self.inner.write(buf).await?;
        self.needs_zlp = n == MAX_PACKET as usize;
        Ok(n)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        if self.needs_zlp {
            self.needs_zlp = false;
            self.inner.write_packet(&[]).await.map_err(|_| CdcAcmError::NotConnected)?;
        }
        Ok(())
    }
}

pub fn init(usb: Peri<'static, USB>, irqs: impl Binding<embassy_rp::interrupt::typelevel::USBCTRL_IRQ, InterruptHandler<USB>>) -> (UsbDevice<'static, UsbDriver>, Serial) {
    let driver = Driver::new(usb, irqs);
    // Raspberry Pi's VID with the Pico SDK's CDC PID, which serial tools already know.
    // TODO: request a BenchBob PID (github.com/raspberrypi/usb-pid) before a 1.0.
    let mut config = embassy_usb::Config::new(0x2E8A, 0x000A);
    config.manufacturer = Some("BenchBob");
    config.product = Some("BenchBob node (Pico 2 W)");
    config.serial_number = None;
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
    static STATE: StaticCell<State> = StaticCell::new();
    static RX_BUF: StaticCell<[u8; MAX_PACKET as usize]> = StaticCell::new();

    let mut builder = Builder::new(driver, config, CONFIG_DESC.init([0; 256]), BOS_DESC.init([0; 256]), &mut [], CONTROL_BUF.init([0; 64]));
    let class = CdcAcmClass::new(&mut builder, STATE.init(State::new()), MAX_PACKET);
    let device = builder.build();
    let (tx, rx) = class.split();
    let rx = rx.into_buffered(RX_BUF.init([0; MAX_PACKET as usize]));
    (device, Serial { tx: Tx { inner: tx, needs_zlp: false }, rx })
}

#[embassy_executor::task]
pub async fn device_task(mut device: UsbDevice<'static, UsbDriver>) -> ! {
    device.run().await
}
