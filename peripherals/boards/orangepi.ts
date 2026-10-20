// ==================== Orange Pi (Shenzhen Xunlong) ====================
//
// UNTESTED: nobody here has an Orange Pi. The pinouts are the vendor's and the kernel's;
// what hasn't been checked is BenchBob running on the hardware.
//
// Boards: Orange Pi 5 / 5B (RK3588S, 26-pin), 5 Plus (RK3588, 40-pin), 3B (RK3566,
// 40-pin), Zero 3 (H618, 26-pin + 13-pin) and Zero 2W (H618, 40-pin). The 5 Pro, 5 Max and
// 5 Ultra are recognised but have no header map: their 40-pin headers differ from the
// 5 Plus (wiringOP has a separate table for each) and we haven't sourced them.
//
// Sources:
//  - Vendor wiki pin tables (the online user manuals):
//    http://www.orangepi.org/orangepiwiki/index.php/26_Pin_Interface_Pin_Description (OPi 5)
//    http://www.orangepi.org/orangepiwiki/index.php/Orange_Pi_5_Plus
//    http://www.orangepi.org/orangepiwiki/index.php/Orange_Pi_3B
//    http://www.orangepi.org/orangepiwiki/index.php/Orange_Pi_Zero_3
//    http://www.orangepi.org/orangepiwiki/index.php/Orange_Pi_Zero_2W
//  - Orange Pi's wiringOP (physToGpio_* / physNames_*), which agrees with the OPi 5 table
//    on every pin and corrects the 5 Plus wiki, whose GPIO numbers for pins 8, 10, 16, 18
//    don't match the pin names printed beside them (it says 109/110/32/33):
//    https://github.com/orangepi-xunlong/wiringOP/blob/next/wiringPi/wiringPi.c
//  - Kernel: compatible strings from the mainline dts files (arch/arm64/boot/dts/rockchip
//    and allwinner) and Orange Pi's kernel (orangepi-xunlong/linux-orangepi), whose
//    strings differ ("rockchip,rk3588s-orangepi-5", "allwinner,sun50i-h616" on a Zero 3).
//    Chip labels: gpio-rockchip.c labels banks "gpio0".."gpio4" (offset = group A–D × 8 +
//    index); sunxi labels its controller by device name ("300b000.pinctrl" on H616/H618,
//    offset = (port − 'A') × 32 + n). Register bases from rk3588-base.dtsi,
//    rk356x-base.dtsi, sun50i-h616.dtsi.
//
// Gotcha for users comparing with the vendor's numbers: on mainline kernels Rockchip GPIO
// bases are dynamic (512+), so the vendor's "GPIO 47" isn't Linux GPIO 47 there. The
// header map is chip + line, which is the same on both kernels.
//
// Boot config: Orange Pi's images keep overlays in /boot/orangepiEnv.txt, Armbian in
// /boot/armbianEnv.txt (same `overlays=` format). The overlay *names* differ between the
// two (Zero 3: "ph-i2c3" vs "i2c3-ph"; Armbian's vendor-kernel Rockchip images want the
// full "rk3588-i2c5-m3"), so the panel offers what is installed in the overlay directory
// and uses the catalogs below only to say which pins each takes.

import { hasCompat, pins, type BoardDef, type BootConfig, type OverlayChoice } from "./types.ts";

const WIKI = "http://www.orangepi.org/orangepiwiki/index.php";
const WIRINGOP = "https://github.com/orangepi-xunlong/wiringOP/blob/next/wiringPi/wiringPi.c";

const rkChips = (bases: string[]) => Object.fromEntries(bases.map((b, n) => [`gpio${n}`, [`gpio${n}`, b]]));
const RK3588_CHIPS = rkChips(["fd8a0000.gpio", "fec20000.gpio", "fec30000.gpio", "fec40000.gpio", "fec50000.gpio"]);
const RK3566_CHIPS = rkChips(["fdd60000.gpio", "fe740000.gpio", "fe750000.gpio", "fe760000.gpio", "fe770000.gpio"]);
const H616_CHIPS = { pio: ["300b000.pinctrl"] };

const envBoot = (vendorDir: string, overlays: OverlayChoice[]): BootConfig => ({
  files: [
    { kind: "armbianEnv", path: "/boot/armbianEnv.txt" },
    { kind: "orangepiEnv", path: "/boot/orangepiEnv.txt" },
  ],
  overlayDirs: [`/boot/dtb/${vendorDir}/overlay`],
  overlays,
});

const o = (name: string, what: string, p: number[], aliases?: string[]): OverlayChoice => ({ name, what, pins: p, ...(aliases ? { aliases } : {}) });

// ---------- Orange Pi 5 / 5B (RK3588S, 26-pin) ----------

const OPI5_PINS = pins([
  [1, "3v3"],
  [2, "5v"],
  [3, "GPIO1_B7", "gpio1", 15, ["I2C5_SDA_M3", "UART1_RX_M1", "PWM13_M2"]],
  [4, "5v"],
  [5, "GPIO1_B6", "gpio1", 14, ["I2C5_SCL_M3", "UART1_TX_M1"]],
  [6, "gnd"],
  [7, "GPIO1_C6", "gpio1", 22, ["PWM15_IR_M2"]],
  [8, "GPIO4_A3", "gpio4", 3, ["UART0_TX_M2"]],
  [9, "gnd"],
  [10, "GPIO4_A4", "gpio4", 4, ["UART0_RX_M2"]],
  [11, "GPIO4_B2", "gpio4", 10, ["CAN1_RX_M1", "PWM14_M1"]],
  [12, "GPIO0_D5", "gpio0", 29, ["CAN2_TX_M1", "I2C1_SDA_M2"]],
  [13, "GPIO4_B3", "gpio4", 11, ["CAN1_TX_M1"]],
  [14, "gnd"],
  [15, "GPIO0_D4", "gpio0", 28, ["CAN2_RX_M1", "I2C1_SCL_M2", "PWM3_IR_M0"]],
  [16, "GPIO1_D3", "gpio1", 27, ["UART4_RX_M0", "I2C1_SDA_M4", "PWM1_M1"]],
  [17, "3v3"],
  [18, "GPIO1_D2", "gpio1", 26, ["UART4_TX_M0", "I2C1_SCL_M4", "PWM0_M1"]],
  [19, "GPIO1_C1", "gpio1", 17, ["SPI4_MOSI_M0", "UART3_TX_M0", "I2C3_SCL_M0"]],
  [20, "gnd"],
  [21, "GPIO1_C0", "gpio1", 16, ["SPI4_MISO_M0", "UART3_RX_M0", "I2C3_SDA_M0"]],
  [22, "GPIO2_D4", "gpio2", 28],
  [23, "GPIO1_C2", "gpio1", 18, ["SPI4_CLK_M0", "PWM3_IR_M2"]],
  [24, "GPIO1_C4", "gpio1", 20, ["SPI4_CS1_M0"]],
  [25, "gnd"],
  [26, "GPIO1_A3", "gpio1", 3, ["PWM1_M2"]],
]);

// Orange Pi 5 user manual v2.4 §3.18 (names as in orangepiEnv.txt; I²C1/PWM1/PWM3 have two
// pin choices each, pick one).
const OPI5_OVERLAYS = [
  o("i2c1-m2", "I²C1 (SDA 12, SCL 15)", [12, 15]),
  o("i2c1-m4", "I²C1 (SDA 16, SCL 18)", [16, 18]),
  o("i2c3-m0", "I²C3 (SDA 21, SCL 19)", [21, 19]),
  o("i2c5-m3", "I²C5 (SDA 3, SCL 5)", [3, 5]),
  o("uart0-m2", "UART0 (TX 8, RX 10)", [8, 10]),
  o("uart1-m1", "UART1 (TX 5, RX 3)", [5, 3]),
  o("uart3-m0", "UART3 (TX 19, RX 21)", [19, 21]),
  o("uart4-m0", "UART4 (TX 18, RX 16)", [18, 16]),
  o("spi4-m0-cs1-spidev", "SPI4 → /dev/spidev4.1 (MOSI 19, MISO 21, CLK 23, CS1 24)", [19, 21, 23, 24]),
  o("pwm0-m1", "PWM0 on 18", [18]),
  o("pwm1-m1", "PWM1 on 16", [16]),
  o("pwm1-m2", "PWM1 on 26", [26]),
  o("pwm3-m0", "PWM3 on 15", [15]),
  o("pwm3-m2", "PWM3 on 23", [23]),
  o("pwm13-m2", "PWM13 on 3", [3]),
  o("pwm14-m1", "PWM14 on 11", [11]),
  o("pwm15-m2", "PWM15 on 7", [7]),
  o("can1-m1", "CAN1 (RX 11, TX 13); not on Linux 6.1 images (manual)", [11, 13]),
  o("can2-m1", "CAN2 (RX 15, TX 12); not on Linux 6.1 images (manual)", [15, 12]),
];

const opi5: BoardDef = {
  id: "orangepi-5",
  name: "Orange Pi 5",
  vendor: "Orange Pi",
  soc: "Rockchip RK3588S",
  tested: false,
  // Mainline and Orange Pi's kernel; the vendor kernel gives the 5B this same string.
  match: (i) => (hasCompat(i, /^xunlong,orangepi-5$/, /^rockchip,rk3588s-orangepi-5$/) ? 50 : 0),
  chips: RK3588_CHIPS,
  headers: [{ id: "26pin", name: "26-pin header", pins: OPI5_PINS }],
  gpio: "gpiochip",
  bootConfig: envBoot("rockchip", OPI5_OVERLAYS),
  health: "sysfs",
  sources: [`${WIKI}/26_Pin_Interface_Pin_Description`, WIRINGOP, "https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/rockchip/rk3588s-orangepi-5.dts"],
  notes: ["An Orange Pi 5B on Orange Pi's own kernel reports itself as an Orange Pi 5; its pin 22 is the power key, not GPIO2_D4 (wiringOP)."],
};

const opi5b: BoardDef = {
  ...opi5,
  id: "orangepi-5b",
  name: "Orange Pi 5B",
  match: (i) => (hasCompat(i, /^xunlong,orangepi-5b$/) ? 51 : 0),
  // Same header as the 5 except pin 22, which wiringOP (physToGpio_5B: -1, physNames_5B:
  // "PowerKey") says is the power key. No vendor pin table for the 5B was found.
  headers: [{ id: "26pin", name: "26-pin header", pins: OPI5_PINS.map((p) => (p.pin === 22 ? { pin: 22, kind: "other" as const, label: "PowerKey", note: "per wiringOP" } : p)) }],
  sources: [...opi5.sources, "https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/rockchip/rk3588s-orangepi-5b.dts"],
  notes: [],
};

// ---------- Orange Pi 5 Plus (RK3588, 40-pin) ----------

const opi5plus: BoardDef = {
  id: "orangepi-5-plus",
  name: "Orange Pi 5 Plus",
  vendor: "Orange Pi",
  soc: "Rockchip RK3588",
  tested: false,
  match: (i) => (hasCompat(i, /^xunlong,orangepi-5-plus$/, /^rockchip,rk3588-orangepi-5-plus$/) ? 50 : 0),
  chips: RK3588_CHIPS,
  headers: [
    {
      id: "40pin",
      name: "40-pin header",
      pins: pins([
        [1, "3v3"],
        [2, "5v"],
        [3, "GPIO0_C0", "gpio0", 16, ["I2C2_SDA_M0", "PWM1_M0", "CAN0_RX_M0"]],
        [4, "5v"],
        [5, "GPIO0_B7", "gpio0", 15, ["I2C2_SCL_M0", "PWM0_M0", "CAN0_TX_M0"]],
        [6, "gnd"],
        [7, "GPIO1_D6", "gpio1", 30, ["PWM14_M2", "I2C8_SCL_M2", "UART1_RTSN_M1"]],
        [8, "GPIO1_A1", "gpio1", 1, ["UART6_TX_M1", "I2C2_SCL_M4", "SPI4_MOSI_M2"]],
        [9, "gnd"],
        [10, "GPIO1_A0", "gpio1", 0, ["UART6_RX_M1", "I2C2_SDA_M4", "SPI4_MISO_M2"]],
        [11, "GPIO1_A4", "gpio1", 4],
        [12, "GPIO3_A1", "gpio3", 1, ["SPI4_MOSI_M1", "PWM11_IR_M0"]],
        [13, "GPIO1_A7", "gpio1", 7],
        [14, "gnd"],
        [15, "GPIO1_B0", "gpio1", 8],
        [16, "GPIO3_B5", "gpio3", 13, ["UART3_TX_M1", "CAN1_RX_M0", "PWM12_M0"]],
        [17, "3v3"],
        [18, "GPIO3_B6", "gpio3", 14, ["UART3_RX_M1", "CAN1_TX_M0", "PWM13_M0"]],
        [19, "GPIO1_B2", "gpio1", 10, ["SPI0_MOSI_M2", "UART4_RX_M2"]],
        [20, "gnd"],
        [21, "GPIO1_B1", "gpio1", 9, ["SPI0_MISO_M2"]],
        [22, "GPIO1_A2", "gpio1", 2, ["UART6_RTSN_M1", "I2C4_SDA_M3", "SPI4_CLK_M2", "PWM0_M2"]],
        [23, "GPIO1_B3", "gpio1", 11, ["SPI0_CLK_M2", "UART4_TX_M2"]],
        [24, "GPIO1_B4", "gpio1", 12, ["SPI0_CS0_M2", "UART7_RX_M2"]],
        [25, "gnd"],
        [26, "GPIO1_B5", "gpio1", 13, ["SPI0_CS1_M2", "UART7_TX_M2"]],
        [27, "GPIO1_B7", "gpio1", 15, ["I2C5_SDA_M3", "UART1_RX_M1", "PWM13_M2"]],
        [28, "GPIO1_B6", "gpio1", 14, ["I2C5_SCL_M3", "UART1_TX_M1"]],
        [29, "GPIO1_D7", "gpio1", 31, ["I2C8_SDA_M2", "UART1_CTSN_M1"]],
        [30, "gnd"],
        [31, "GPIO3_A0", "gpio3", 0, ["SPI4_MISO_M1", "PWM10_M0"]],
        [32, "GPIO1_A3", "gpio1", 3, ["UART6_CTSN_M1", "I2C4_SCL_M3", "SPI4_CS0_M2", "PWM1_M2"]],
        [33, "GPIO3_C2", "gpio3", 18, ["PWM14_M0"]],
        [34, "gnd"],
        [35, "GPIO3_A2", "gpio3", 2, ["UART8_TX_M1", "SPI4_CLK_M1"]],
        [36, "GPIO3_A5", "gpio3", 5, ["UART8_CTSN_M1"]],
        [37, "GPIO3_C1", "gpio3", 17],
        [38, "GPIO3_A4", "gpio3", 4, ["UART8_RTSN_M1", "SPI4_CS1_M1"]],
        [39, "gnd"],
        [40, "GPIO3_A3", "gpio3", 3, ["UART8_RX_M1", "SPI4_CS0_M1"]],
      ]),
    },
  ],
  gpio: "gpiochip",
  // Orange Pi 5 Plus manual §3.18. I²C2 is one controller on two pin choices: pick one.
  bootConfig: envBoot("rockchip", [
    o("i2c2-m0", "I²C2 (SDA 3, SCL 5)", [3, 5]),
    o("i2c2-m4", "I²C2 (SDA 10, SCL 8)", [10, 8]),
    o("i2c4-m3", "I²C4 (SDA 22, SCL 32)", [22, 32]),
    o("i2c5-m3", "I²C5 (SDA 27, SCL 28)", [27, 28]),
    o("i2c8-m2", "I²C8 (SDA 29, SCL 7)", [29, 7]),
    o("uart1-m1", "UART1 (TX 28, RX 27)", [28, 27]),
    o("uart3-m1", "UART3 (TX 16, RX 18)", [16, 18]),
    o("uart4-m2", "UART4 (TX 23, RX 19)", [23, 19]),
    o("uart6-m1", "UART6 (TX 8, RX 10)", [8, 10]),
    o("uart7-m2", "UART7 (TX 26, RX 24)", [26, 24]),
    o("uart8-m1", "UART8 (TX 35, RX 40)", [35, 40]),
    o("spi0-m2-cs0-spidev", "SPI0 → spidev0.0 (MOSI 19, MISO 21, CLK 23, CS0 24)", [19, 21, 23, 24]),
    o("spi0-m2-cs1-spidev", "SPI0 → spidev0.1 (MOSI 19, MISO 21, CLK 23, CS1 26)", [19, 21, 23, 26]),
    o("spi0-m2-cs0-cs1-spidev", "SPI0, both chip selects (19, 21, 23, CS0 24, CS1 26)", [19, 21, 23, 24, 26]),
    o("spi4-m1-cs0-spidev", "SPI4 → spidev4.0 (MOSI 12, MISO 31, CLK 35, CS0 40)", [12, 31, 35, 40]),
    o("spi4-m1-cs1-spidev", "SPI4 → spidev4.1 (MOSI 12, MISO 31, CLK 35, CS1 38)", [12, 31, 35, 38]),
    o("spi4-m1-cs0-cs1-spidev", "SPI4, both chip selects (12, 31, 35, CS0 40, CS1 38)", [12, 31, 35, 40, 38]),
    o("spi4-m2-cs0-spidev", "SPI4 → spidev4.0 (MOSI 8, MISO 10, CLK 22, CS0 32)", [8, 10, 22, 32]),
    o("pwm0-m0", "PWM0 on 5", [5]),
    o("pwm0-m2", "PWM0 on 22", [22]),
    o("pwm1-m0", "PWM1 on 3", [3]),
    o("pwm1-m2", "PWM1 on 32", [32]),
    o("pwm11-m0", "PWM11 on 12", [12]),
    o("pwm12-m0", "PWM12 on 16", [16]),
    o("pwm13-m0", "PWM13 on 18", [18]),
    o("pwm14-m0", "PWM14 on 33", [33]),
    o("pwm14-m2", "PWM14 on 7", [7]),
    o("can0-m0", "CAN0 (RX 3, TX 5)", [3, 5]),
    o("can1-m0", "CAN1 (RX 16, TX 18)", [16, 18]),
  ]),
  health: "sysfs",
  sources: [`${WIKI}/Orange_Pi_5_Plus`, WIRINGOP, "https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/rockchip/rk3588-orangepi-5-plus.dts"],
};

/** RK3588 Orange Pis whose headers we haven't sourced: identity and boot config, lines listed but no physical pins. */
const opi5other: BoardDef = {
  id: "orangepi-5-other",
  name: "Orange Pi 5 Pro / Max / Ultra",
  vendor: "Orange Pi",
  soc: "Rockchip RK3588",
  tested: false,
  match: (i) => (hasCompat(i, /^xunlong,orangepi-5-(pro|max|ultra)$/, /^rockchip,rk3588s?-orangepi-5-(pro|max|ultra)$/) ? 50 : 0),
  headers: [],
  gpio: "gpiochip",
  bootConfig: envBoot("rockchip", []),
  health: "sysfs",
  sources: [WIRINGOP],
  notes: ["The 40-pin header on these boards differs from the 5 Plus and isn't mapped yet; the GPIO panel lists the SoC's lines instead."],
};

// ---------- Orange Pi 3B (RK3566, 40-pin) ----------

const opi3b: BoardDef = {
  id: "orangepi-3b",
  name: "Orange Pi 3B",
  vendor: "Orange Pi",
  soc: "Rockchip RK3566",
  tested: false,
  match: (i) => (hasCompat(i, /^xunlong,orangepi-3b/, /^rockchip,rk3566-orangepi-3b$/) ? 50 : 0),
  chips: RK3566_CHIPS,
  headers: [
    {
      id: "40pin",
      name: "40-pin header",
      // The wiki publishes this table as images only (Pi3b-img255-1.png); it doesn't say
      // whether v1.1 and v2.1 differ.
      pins: pins([
        [1, "3v3"],
        [2, "5v"],
        [3, "GPIO4_B4", "gpio4", 12, ["I2C2_SDA_M1"]],
        [4, "5v"],
        [5, "GPIO4_B5", "gpio4", 13, ["I2C2_SCL_M1"]],
        [6, "gnd"],
        [7, "GPIO4_C3", "gpio4", 19, ["PWM15_IR_M1"]],
        [8, "GPIO0_D1", "gpio0", 25, ["UART2_TX_M0"], "debug console"],
        [9, "gnd"],
        [10, "GPIO0_D0", "gpio0", 24, ["UART2_RX_M0"], "debug console"],
        [11, "GPIO3_C6", "gpio3", 22],
        [12, "GPIO3_C7", "gpio3", 23],
        [13, "GPIO4_A0", "gpio4", 0],
        [14, "gnd"],
        [15, "GPIO4_A2", "gpio4", 2, ["UART7_TX_M2"]],
        [16, "GPIO4_A3", "gpio4", 3, ["UART7_RX_M2"]],
        [17, "3v3"],
        [18, "GPIO4_A1", "gpio4", 1],
        [19, "GPIO4_B2", "gpio4", 10, ["SPI3_MOSI_M0", "I2C4_SDA_M0"]],
        [20, "gnd"],
        [21, "GPIO4_B0", "gpio4", 8, ["SPI3_MISO_M0"]],
        [22, "GPIO4_A4", "gpio4", 4, ["UART9_TX_M2"]],
        [23, "GPIO4_B3", "gpio4", 11, ["SPI3_CLK_M0", "I2C4_SCL_M0"]],
        [24, "GPIO4_A6", "gpio4", 6, ["SPI3_CS0_M0"]],
        [25, "gnd"],
        [26, "GPIO4_A7", "gpio4", 7, ["SPI3_CS1_M0"]],
        [27, "GPIO1_A0", "gpio1", 0, ["I2C3_SDA_M0", "UART3_RX_M0"]],
        [28, "GPIO1_A1", "gpio1", 1, ["I2C3_SCL_M0", "UART3_TX_M0"]],
        [29, "GPIO4_A5", "gpio4", 5, ["UART9_RX_M2"]],
        [30, "gnd"],
        [31, "GPIO3_D4", "gpio3", 28],
        [32, "GPIO4_C0", "gpio4", 16, ["PWM11_IR_M1"]],
        [33, "GPIO3_D7", "gpio3", 31],
        [34, "gnd"],
        [35, "GPIO3_D0", "gpio3", 24],
        [36, "GPIO3_D5", "gpio3", 29],
        [37, "GPIO3_D3", "gpio3", 27],
        [38, "GPIO3_D2", "gpio3", 26],
        [39, "gnd"],
        [40, "GPIO3_D1", "gpio3", 25],
      ]),
    },
  ],
  gpio: "gpiochip",
  // Orange Pi 3B manual (overlay_prefix=rk356x); Armbian mainline uses the same short names.
  bootConfig: envBoot("rockchip", [
    o("i2c2-m1", "I²C2 (SDA 3, SCL 5)", [3, 5]),
    o("i2c3-m0", "I²C3 (SDA 27, SCL 28)", [27, 28]),
    o("i2c4-m0", "I²C4 (SDA 19, SCL 23)", [19, 23]),
    o("uart3-m0", "UART3 (TX 28, RX 27)", [28, 27]),
    o("uart7-m2", "UART7 (TX 15, RX 16)", [15, 16]),
    o("uart9-m2", "UART9 (TX 22, RX 29)", [22, 29]),
    o("pwm11-m1", "PWM11 on 32", [32]),
    o("pwm15-m1", "PWM15 on 7", [7]),
    o("spi3-m0-cs0-spidev", "SPI3 → spidev3.0 (MOSI 19, MISO 21, CLK 23, CS0 24)", [19, 21, 23, 24]),
  ]),
  health: "sysfs",
  sources: [`${WIKI}/Orange_Pi_3B`, "https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/rockchip/rk3566-orangepi-3b.dtsi"],
};

// ---------- Orange Pi Zero 3 (H618, 26-pin + 13-pin) ----------

const opiZero3: BoardDef = {
  id: "orangepi-zero3",
  name: "Orange Pi Zero 3",
  vendor: "Orange Pi",
  soc: "Allwinner H618",
  tested: false,
  match: (i) => (hasCompat(i, /^xunlong,orangepi-zero3$/) ? 50 : 0),
  chips: H616_CHIPS,
  headers: [
    {
      id: "26pin",
      name: "26-pin header",
      pins: pins([
        [1, "3v3"],
        [2, "5v"],
        [3, "PH5", "pio", 229, ["TWI3-SDA", "SPI1_CS0", "UART2_TX"]],
        [4, "5v"],
        [5, "PH4", "pio", 228, ["TWI3-SCK"]],
        [6, "gnd"],
        [7, "PC9", "pio", 73],
        [8, "PH2", "pio", 226, ["UART5_TX", "PWM2", "TWI2"]],
        [9, "gnd"],
        [10, "PH3", "pio", 227, ["UART5_RX", "PWM1", "TWI2"]],
        [11, "PC6", "pio", 70],
        [12, "PC11", "pio", 75],
        [13, "PC5", "pio", 69],
        [14, "gnd"],
        [15, "PC8", "pio", 72],
        [16, "PC15", "pio", 79],
        [17, "3v3"],
        [18, "PC14", "pio", 78],
        [19, "PH7", "pio", 231, ["SPI1_MOSI", "TWI4"]],
        [20, "gnd"],
        [21, "PH8", "pio", 232, ["SPI1_MISO"]],
        [22, "PC7", "pio", 71],
        [23, "PH6", "pio", 230, ["SPI1_CLK", "TWI4"]],
        [24, "PH9", "pio", 233, ["SPI1_CS1"]],
        [25, "gnd"],
        [26, "PC10", "pio", 74],
      ]),
    },
    {
      id: "13pin",
      name: "13-pin header",
      pins: pins([
        [1, "5v"],
        [2, "gnd"],
        [3, "other", "USB2-DM"],
        [4, "other", "USB2-DP"],
        [5, "other", "USB3-DM"],
        [6, "other", "USB3-DP"],
        [7, "other", "LINEOUTR"],
        [8, "other", "LINEOUTL"],
        [9, "other", "TV-OUT"],
        [10, "PC1", "pio", 65, [], "not usable with the expansion board fitted"],
        [11, "PI16", "pio", 272, [], "not usable with the expansion board fitted"],
        [12, "PI6", "pio", 262, [], "not usable with the expansion board fitted"],
        [13, "PH10", "pio", 234, ["IR-RX"]],
      ]),
    },
  ],
  gpio: "gpiochip",
  // Orange Pi names first, Armbian's as aliases (Armbian: i2c3-ph, uart5, pwm1-ph3…).
  bootConfig: envBoot("allwinner", [
    o("ph-i2c3", "I²C3 → /dev/i2c-3 (SDA 3, SCL 5)", [3, 5], ["i2c3-ph"]),
    o("ph-uart5", "UART5 (TX 8, RX 10)", [8, 10], ["uart5"]),
    o("ph-pwm12", "PWM1 on 10, PWM2 on 8", [10, 8]),
    o("pwm1-ph3", "PWM1 on 10", [10]),
    o("pwm2-ph2", "PWM2 on 8", [8]),
    o("spi1-cs0-spidev", "SPI1 → spidev1.0 (CLK 23, MOSI 19, MISO 21, CS0 3)", [23, 19, 21, 3]),
    o("spi1-cs1-spidev", "SPI1 → spidev1.1 (CLK 23, MOSI 19, MISO 21, CS1 24)", [23, 19, 21, 24]),
    o("spi1-cs0-cs1-spidev", "SPI1, both chip selects (23, 19, 21, CS0 3, CS1 24)", [23, 19, 21, 3, 24]),
  ]),
  health: "sysfs",
  sources: [`${WIKI}/Orange_Pi_Zero_3`, "https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/allwinner/sun50i-h618-orangepi-zero3.dts", "https://github.com/torvalds/linux/blob/master/drivers/pinctrl/sunxi/pinctrl-sun50i-h616.c"],
  notes: ["Armbian's spidev1_* overlays don't set SPI1's pin mux; whether they work on this board is unconfirmed."],
};

// ---------- Orange Pi Zero 2W (H618, 40-pin) ----------

const opiZero2w: BoardDef = {
  id: "orangepi-zero2w",
  name: "Orange Pi Zero 2W",
  vendor: "Orange Pi",
  soc: "Allwinner H618",
  tested: false,
  match: (i) => (hasCompat(i, /^xunlong,orangepi-zero2w$/) ? 50 : 0),
  chips: H616_CHIPS,
  headers: [
    {
      id: "40pin",
      name: "40-pin header",
      pins: pins([
        [1, "3v3"],
        [2, "5v"],
        [3, "PI8", "pio", 264, ["TWI1-SDA"]],
        [4, "5v"],
        [5, "PI7", "pio", 263, ["TWI1-SCL"]],
        [6, "gnd"],
        [7, "PI13", "pio", 269, ["PWM3", "UART4_TX"]],
        [8, "PH0", "pio", 224, ["UART0_TX"], "debug console"],
        [9, "gnd"],
        [10, "PH1", "pio", 225, ["UART0_RX"], "debug console"],
        [11, "PH2", "pio", 226, ["UART5_TX"]],
        [12, "PI1", "pio", 257],
        [13, "PH3", "pio", 227, ["UART5_RX"]],
        [14, "gnd"],
        [15, "PI5", "pio", 261, ["TWI0_SCL", "UART2_TX"]],
        [16, "PI14", "pio", 270, ["PWM4", "UART4_RX"]],
        [17, "3v3"],
        [18, "PH4", "pio", 228],
        [19, "PH7", "pio", 231, ["SPI1_MOSI"]],
        [20, "gnd"],
        [21, "PH8", "pio", 232, ["SPI1_MISO"]],
        [22, "PI6", "pio", 262, ["TWI0_SDA", "UART2_RX"]],
        [23, "PH6", "pio", 230, ["SPI1_CLK"]],
        [24, "PH5", "pio", 229, ["SPI1_CS0"]],
        [25, "gnd"],
        [26, "PH9", "pio", 233, ["SPI1_CS1"]],
        [27, "PI10", "pio", 266, ["TWI2-SDA", "UART3_RX"]],
        [28, "PI9", "pio", 265, ["TWI2-SCL", "UART3_TX"]],
        [29, "PI0", "pio", 256],
        [30, "gnd"],
        [31, "PI15", "pio", 271],
        [32, "PI11", "pio", 267, ["PWM1"]],
        [33, "PI12", "pio", 268, ["PWM2"]],
        [34, "gnd"],
        [35, "PI2", "pio", 258],
        [36, "PC12", "pio", 76],
        [37, "PI16", "pio", 272],
        [38, "PI4", "pio", 260],
        [39, "gnd"],
        [40, "PI3", "pio", 259],
      ]),
    },
  ],
  gpio: "gpiochip",
  bootConfig: envBoot("allwinner", [
    o("pi-i2c0", "I²C0 (SDA 22, SCL 15)", [22, 15], ["i2c0-pi"]),
    o("pi-i2c1", "I²C1 (SDA 3, SCL 5)", [3, 5], ["i2c1-pi"]),
    o("pi-i2c2", "I²C2 (SDA 27, SCL 28)", [27, 28], ["i2c2-pi"]),
    o("pi-uart2", "UART2 (TX 15, RX 22)", [15, 22], ["uart2-pi"]),
    o("pi-uart3", "UART3 (TX 28, RX 27)", [28, 27], ["uart3-pi"]),
    o("pi-uart4", "UART4 (TX 7, RX 16)", [7, 16], ["uart4-pi"]),
    o("ph-uart5", "UART5 (TX 11, RX 13)", [11, 13], ["uart5"]),
    o("pi-pwm1", "PWM1 on 32", [32], ["pwm1-pi11"]),
    o("pi-pwm2", "PWM2 on 33", [33], ["pwm2-pi12"]),
    o("pi-pwm3", "PWM3 on 7", [7], ["pwm3-pi13"]),
    o("pi-pwm4", "PWM4 on 16", [16], ["pwm4-pi14"]),
    o("spi1-cs0-spidev", "SPI1 → spidev1.0 (MOSI 19, MISO 21, CLK 23, CS0 24)", [19, 21, 23, 24]),
    o("spi1-cs1-spidev", "SPI1 → spidev1.1 (MOSI 19, MISO 21, CLK 23, CS1 26)", [19, 21, 23, 26]),
    o("spi1-cs0-cs1-spidev", "SPI1, both chip selects (19, 21, 23, CS0 24, CS1 26)", [19, 21, 23, 24, 26]),
  ]),
  health: "sysfs",
  sources: [`${WIKI}/Orange_Pi_Zero_2W`, "https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/allwinner/sun50i-h618-orangepi-zero2w.dts"],
  notes: ["Armbian's uart3-pi / uart4-pi overlays look broken in its sources (wrong UART node, a pin function the driver doesn't have); Orange Pi's pi-uart3 / pi-uart4 are the vendor's."],
};

export default [opi5, opi5b, opi5plus, opi5other, opi3b, opiZero3, opiZero2w];
