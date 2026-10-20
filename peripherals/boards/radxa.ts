// ==================== Radxa ====================
//
// UNTESTED: nobody here has a ROCK 5B. The pinout is Radxa's; what hasn't been checked is
// BenchBob running on the hardware.
//
// ROCK 5B and 5B+ (RK3588, 40-pin). Sources:
//  - Pin table: https://docs.radxa.com/en/rock5/rock5b/hardware-design/hardware-interface
//    ("GPIO Pinout", ROCK 5B and ROCK 5B+ tabs; the 5B+ differs at pins 29 and 31). All
//    GPIOs are 3.3 V; SARADC_IN4 (pin 22) is a 1.8 V ADC input. Pin 37's GPIO number is
//    blank in Radxa's table; the line follows from its name (GPIO0_A0 = bank 0, line 0).
//    I²S / SPDIF functions are left out of the lists below.
//  - Kernel: compatible "radxa,rock-5b" / "radxa,rock-5b-plus" (mainline rk3588-rock-5b.dts,
//    and the same in radxa/kernel); banks labelled "gpio0".."gpio4" by gpio-rockchip.c at
//    the rk3588-base.dtsi addresses.
//
// Boot config: Radxa OS manages overlays with rsetup, which renames .dtbo files and runs
// u-boot-update to regenerate /boot/extlinux/extlinux.conf, so editing that file by hand
// (or from here) would be overwritten. Only Armbian's armbianEnv.txt is offered.

import { hasCompat, pins, type BoardDef, type HeaderPin } from "./types.ts";

const ROCK5B_PINS: HeaderPin[] = pins([
  [1, "3v3"],
  [2, "5v"],
  [3, "GPIO4_B3", "gpio4", 11, ["PWM15_IR_M1", "UART8_CTSN_M0", "I2C7_SDA_M3"]],
  [4, "5v"],
  [5, "GPIO4_B2", "gpio4", 10, ["PWM14_M1", "UART8_RTSN_M0", "I2C7_SCL_M3"]],
  [6, "gnd"],
  [7, "GPIO3_C3", "gpio3", 19, ["PWM15_IR_M0", "UART7_CTSN_M1", "I2C8_SDA_M4", "SPI1_CS1_M1"]],
  [8, "GPIO0_B5", "gpio0", 13, ["UART2_TX_M0", "I2C1_SCL_M0"], "debug console"],
  [9, "gnd"],
  [10, "GPIO0_B6", "gpio0", 14, ["UART2_RX_M0", "I2C1_SDA_M0"], "debug console"],
  [11, "GPIO3_C1", "gpio3", 17, ["UART7_RX_M1", "SPI1_CLK_M1"]],
  [12, "GPIO3_B5", "gpio3", 13, ["PWM12_M0", "UART3_TX_M1"]],
  [13, "GPIO3_B7", "gpio3", 15, ["I2C3_SCL_M1", "SPI1_MOSI_M1"]],
  [14, "gnd"],
  [15, "GPIO3_C0", "gpio3", 16, ["UART7_TX_M1", "I2C3_SDA_M1", "SPI1_MISO_M1"]],
  [16, "GPIO3_A4", "gpio3", 4],
  [17, "3v3"],
  [18, "GPIO4_C4", "gpio4", 20, ["PWM5_M2", "SPI3_MISO_M0"]],
  [19, "GPIO1_B2", "gpio1", 10, ["UART4_RX_M2", "SPI0_MOSI_M2"]],
  [20, "gnd"],
  [21, "GPIO1_B1", "gpio1", 9, ["SPI0_MISO_M2"]],
  [22, "other", "SARADC_IN4"],
  [23, "GPIO1_B3", "gpio1", 11, ["UART4_TX_M2", "SPI0_CLK_M2"]],
  [24, "GPIO1_B4", "gpio1", 12, ["UART7_RX_M2", "SPI0_CS0_M2"]],
  [25, "gnd"],
  [26, "GPIO1_B5", "gpio1", 13, ["UART7_TX_M2", "SPI0_CS1_M2"]],
  [27, "GPIO4_C6", "gpio4", 22, ["PWM7_IR_M3", "I2C0_SDA_M1", "SPI3_CLK_M0"]],
  [28, "GPIO4_C5", "gpio4", 21, ["PWM6_M2", "I2C0_SCL_M1", "SPI3_MOSI_M0"]],
  [29, "GPIO1_D7", "gpio1", 31, ["PWM15_IR_M3", "UART1_CTSN_M1"]],
  [30, "gnd"],
  [31, "GPIO1_B7", "gpio1", 15, ["PWM13_M2", "UART1_RX_M1"]],
  [32, "GPIO3_C2", "gpio3", 18, ["PWM14_M0", "UART7_RTSN_M1", "I2C8_SCL_M4", "SPI1_CS0_M1"]],
  [33, "GPIO3_A7", "gpio3", 7, ["PWM8_M0"]],
  [34, "gnd"],
  [35, "GPIO3_B6", "gpio3", 14, ["PWM13_M0", "UART3_RX_M1"]],
  [36, "GPIO3_B1", "gpio3", 9, ["PWM2_M1", "UART2_TX_M2"]],
  [37, "GPIO0_A0", "gpio0", 0],
  [38, "GPIO3_B2", "gpio3", 10, ["PWM3_IR_M1", "UART2_RX_M2"]],
  [39, "gnd"],
  [40, "GPIO3_B3", "gpio3", 11, ["UART2_RTSN"]],
]);

const SOURCES = ["https://docs.radxa.com/en/rock5/rock5b/hardware-design/hardware-interface", "https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/rockchip/rk3588-rock-5b.dts"];
const CHIPS = Object.fromEntries(["fd8a0000.gpio", "fec20000.gpio", "fec30000.gpio", "fec40000.gpio", "fec50000.gpio"].map((b, n) => [`gpio${n}`, [`gpio${n}`, b]]));

const rock5b: BoardDef = {
  id: "rock-5b",
  name: "Radxa ROCK 5B",
  vendor: "Radxa",
  soc: "Rockchip RK3588",
  tested: false,
  match: (i) => (hasCompat(i, /^radxa,rock-5b$/) ? 50 : 0),
  chips: CHIPS,
  headers: [{ id: "40pin", name: "40-pin header", pins: ROCK5B_PINS }],
  gpio: "gpiochip",
  bootConfig: { files: [{ kind: "armbianEnv", path: "/boot/armbianEnv.txt" }], overlayDirs: ["/boot/dtb/rockchip/overlay"] },
  health: "sysfs",
  sources: SOURCES,
  notes: ["On Radxa OS, change overlays with `sudo rsetup` (Overlays): it regenerates extlinux.conf, so BenchBob leaves it alone. On Armbian the GPIO panel edits armbianEnv.txt."],
};

const rock5bPlus: BoardDef = {
  ...rock5b,
  id: "rock-5b-plus",
  name: "Radxa ROCK 5B+",
  match: (i) => (hasCompat(i, /^radxa,rock-5b-plus$/) ? 50 : 0),
  headers: [
    {
      id: "40pin",
      name: "40-pin header",
      pins: ROCK5B_PINS.map((p) =>
        p.pin === 29 ? pins([[29, "GPIO1_A3", "gpio1", 3, ["I2C4_SCL_M3", "PWM1_M2"]]])[0] : p.pin === 31 ? pins([[31, "GPIO1_A2", "gpio1", 2, ["I2C4_SDA_M3", "PWM0_M2"]]])[0] : p,
      ),
    },
  ],
  sources: [...SOURCES, "https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/rockchip/rk3588-rock-5b-plus.dts"],
};

export default [rock5b, rock5bPlus];
