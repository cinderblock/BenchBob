// ==================== Libre Computer ====================
//
// UNTESTED: nobody here has one of these boards. The data below is the vendor's and the
// kernel's; what hasn't been checked is BenchBob running on the hardware.
//
// AML-S905X-CC "Le Potato" (Amlogic S905X) and its V2 "Sweet Potato" (identical header:
// the vendor's V2 pin map is a symlink to the V1 one), and ROC-RK3328-CC "Renegade".
//
// Sources, checked against each other pin by pin:
//  - Vendor pin maps (libretech-wiring-tool, commit 442faf9, 2026-10-05):
//    https://github.com/libre-computer-project/libretech-wiring-tool/blob/master/libre-computer/aml-s905x-cc/gpio.map
//    https://github.com/libre-computer-project/libretech-wiring-tool/blob/master/libre-computer/roc-rk3328-cc/gpio.map
//    The functions are the map's "Desc" column (SoC mux options). Its "Ref" column holds
//    schematic net names and is misleading in places (pins 27/28 say I2C_A; they're I2C_B).
//  - Kernel DT, whose gpio-line-names for Le Potato name every header pin and agree:
//    https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/amlogic/meson-gxl-s905x-libretech-cc.dts
//  - Line offsets: include/dt-bindings/gpio/meson-gxl-gpio.h (aobus: GPIOAO_0..9 = 0..9,
//    TEST_N = 10; periphs: GPIODV_0 = 49, GPIOX_0 = 79, GPIOCLK_0 = 98). Chip labels from
//    drivers/pinctrl/meson/pinctrl-meson-gxl.c ("aobus-banks", "periphs-banks"); the chip
//    numbers depend on probe order, so match on the label. Rockchip: gpio-rockchip.c
//    labels each bank "gpioN"; offset = group (A=0…D=3) × 8 + index.
//
// Boot config: Libre's images apply overlays with `ldto merge <name>`, which writes a whole
// merged DTB under /boot/efi/dtb rather than a list we could edit, and Armbian's GXL
// overlays are written for the Odroid C2 and not confirmed for these pins. So these boards
// are read-only here; the notes list the vendor's overlay names.

import { hasCompat, pins, type BoardDef } from "./types.ts";

const LEPOTATO_SOURCES = [
  "https://github.com/libre-computer-project/libretech-wiring-tool/blob/master/libre-computer/aml-s905x-cc/gpio.map",
  "https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/amlogic/meson-gxl-s905x-libretech-cc.dts",
  "https://github.com/torvalds/linux/blob/master/include/dt-bindings/gpio/meson-gxl-gpio.h",
];

const lePotato: BoardDef = {
  id: "le-potato",
  name: "Libre Computer AML-S905X-CC (Le Potato)",
  vendor: "Libre Computer",
  soc: "Amlogic S905X",
  tested: false,
  match: (i) => (hasCompat(i, /^libretech,aml-s905x-cc(-v2)?$/) ? 50 : 0),
  label: (i) => (i.model ? `${i.model} (${/v2/i.test(i.model) ? "Sweet Potato" : "Le Potato"})` : ""),
  chips: { aobus: ["aobus-banks"], periphs: ["periphs-banks"] },
  headers: [
    {
      id: "7j1",
      name: "40-pin header (7J1)",
      pins: pins([
        [1, "3v3"],
        [2, "5v"],
        [3, "GPIOAO_5", "aobus", 5, ["UART_RX_AO_B", "I2C_SDA_AO", "I2C_SLAVE_SDA_AO"]],
        [4, "5v"],
        [5, "GPIOAO_4", "aobus", 4, ["UART_TX_AO_B", "I2C_SCK_AO", "I2C_SLAVE_SCK_AO"]],
        [6, "gnd"],
        [7, "GPIOCLK_0", "periphs", 98, ["CLK24", "CLK12"]],
        [8, "GPIOX_12", "periphs", 91, ["UART_TX_A"]],
        [9, "gnd"],
        [10, "GPIOX_13", "periphs", 92, ["UART_RX_A"]],
        [11, "GPIOAO_8", "aobus", 8, ["AO_CEC", "EE_CEC", "I2SOUT_CH23", "PWM_AO_A"], "shared with HDMI CEC through switch 9J3"],
        [12, "GPIOAO_6", "aobus", 6, ["I2S_IN_01", "SPDIF_OUT", "PWM_AO_B"]],
        [13, "GPIOAO_9", "aobus", 9, ["REMOTE_OUTPUT", "SPDIF_OUT", "I2SOUT_CH45", "PWM_AO_B"]],
        [14, "gnd"],
        // The DT names line 10 "7J1 Header Pin15", but the GXL AO bank stops at GPIOAO_9, so
        // the mainline driver refuses it as a GPIO (vendor: "TEST_N**").
        [15, "GPIO_TEST_N", "aobus", 10, ["wd_gpio", "I2SOUT_CH67"], "TEST_N: not usable as a GPIO on mainline kernels"],
        [16, "GPIOX_14", "periphs", 93, ["UART_CTS_A"]],
        [17, "3v3"],
        [18, "GPIOX_15", "periphs", 94, ["UART_RTS_A"]],
        [19, "GPIOX_8", "periphs", 87, ["PCM_OUT_A", "UART_TX_C", "SPI_MOSI"]],
        [20, "gnd"],
        [21, "GPIOX_9", "periphs", 88, ["PCM_IN_A", "UART_RX_C", "SPI_MISO"]],
        [22, "GPIOX_0", "periphs", 79, ["SDIO_D0"]],
        [23, "GPIOX_11", "periphs", 90, ["PCM_CLK_A", "UART_RTS_C", "I2C_SCK_D", "SPI_SCLK"]],
        [24, "GPIOX_10", "periphs", 89, ["PCM_FS_A", "UART_CTS_C", "I2C_SDA_D", "SPI_SS0"]],
        [25, "gnd"],
        [26, "GPIOX_1", "periphs", 80, ["SDIO_D1"]],
        [27, "GPIODV_26", "periphs", 75, ["UART_CTS_B", "I2C_SDA_B"]],
        [28, "GPIODV_27", "periphs", 76, ["UART_RTS_B", "I2C_SCK_B"]],
        [29, "GPIOX_17", "periphs", 96],
        [30, "gnd"],
        [31, "GPIOX_18", "periphs", 97],
        [32, "GPIOX_16", "periphs", 95, ["PWM_E"]],
        [33, "GPIOX_6", "periphs", 85, ["PWM_A"]],
        [34, "gnd"],
        [35, "GPIOX_7", "periphs", 86, ["SDIO_IRQ", "PWM_F"]],
        [36, "GPIOX_2", "periphs", 81, ["SDIO_D2"]],
        [37, "GPIOX_5", "periphs", 84, ["SDIO_CMD"]],
        [38, "GPIOX_3", "periphs", 82, ["SDIO_D3"]],
        [39, "gnd"],
        [40, "GPIOX_4", "periphs", 83, ["SDIO_CLK"]],
      ]),
    },
  ],
  gpio: "gpiochip",
  bootConfig: null,
  health: "sysfs",
  sources: LEPOTATO_SOURCES,
  notes: [
    "Header overlays come from Libre's wiring tool: `sudo ldto merge <name>` then reboot (i2c-ao: pins 3/5; i2c-b: 27/28; spi-cc-1cs: 19/21/23/24; uart-a: 8/10; uart-ao-b: 5/3; uart-c-clk81: 19/21; pwm-e: 32; pwm-a: 33; pwm-f: 35). BenchBob doesn't edit the merged DTB it writes.",
  ],
};

const renegade: BoardDef = {
  id: "roc-rk3328-cc",
  name: "Libre Computer ROC-RK3328-CC (Renegade)",
  vendor: "Libre Computer",
  soc: "Rockchip RK3328",
  tested: false,
  match: (i) => (hasCompat(i, /^firefly,roc-rk3328-cc$/) ? 50 : 0),
  label: (i) => (i.model ? `${i.model} (Renegade)` : ""),
  chips: { gpio0: ["gpio0"], gpio1: ["gpio1"], gpio2: ["gpio2"], gpio3: ["gpio3"] },
  headers: [
    {
      id: "j1",
      name: "40-pin header (J1)",
      pins: pins([
        [1, "3v3"],
        [2, "5v"],
        [3, "GPIO2_D1", "gpio2", 25, ["I2C0_SDA"]],
        [4, "5v"],
        [5, "GPIO2_D0", "gpio2", 24, ["I2C0_SCL"]],
        [6, "gnd"],
        [7, "GPIO1_D4", "gpio1", 28, ["CLK32K_OUT_M1"]],
        [8, "GPIO3_A4", "gpio3", 4, ["UART1_TX"]],
        [9, "gnd"],
        [10, "GPIO3_A6", "gpio3", 6, ["UART1_RX"]],
        [11, "GPIO2_C4", "gpio2", 20, ["I2S1_SDIO1"]],
        [12, "GPIO2_A6", "gpio2", 6, ["PWM2"]],
        [13, "GPIO2_C5", "gpio2", 21, ["I2S1_SDIO2"]],
        [14, "gnd"],
        [15, "GPIO2_C6", "gpio2", 22, ["I2S1_SDIO3"]],
        [16, "GPIO3_A7", "gpio3", 7, ["UART1_CTSN"]],
        [17, "3v3"],
        [18, "GPIO3_A5", "gpio3", 5, ["UART1_RTSN"]],
        [19, "GPIO3_A1", "gpio3", 1, ["SPI_TXD_M2"]],
        [20, "gnd"],
        [21, "GPIO3_A2", "gpio3", 2, ["SPI_RXD_M2"]],
        [22, "GPIO0_A2", "gpio0", 2, ["CLKOUT_GMAC_M0", "SPDIF_TX_M2"]],
        [23, "GPIO3_A0", "gpio3", 0, ["SPI_CLK_M2"]],
        [24, "GPIO3_B0", "gpio3", 8, ["SPI_CSN0_M2"]],
        [25, "gnd"],
        [26, "GPIO2_B4", "gpio2", 12, ["SPI_CSN1_M0"]],
        [27, "GPIO2_A4", "gpio2", 4, ["PWM0", "I2C1_SDA"], "I²C1 is shared with the PMIC"],
        [28, "GPIO2_A5", "gpio2", 5, ["PWM1", "I2C1_SCL"], "I²C1 is shared with the PMIC"],
        [29, "GPIO2_C3", "gpio2", 19, ["I2S1_SDI"]],
        [30, "gnd"],
        [31, "GPIO2_C7", "gpio2", 23, ["I2S1_SDO"]],
        [32, "GPIO0_A0", "gpio0", 0, ["CLKOUT_WIFI_M0"]],
        // Wired to two SoC lines through resistors; the vendor's first row is authoritative
        // and both must never be driven at once.
        [33, "GPIO2_C0", "gpio2", 16, ["I2S1_LRCK_RX"], "also wired to GPIO2_C1 (gpio2 line 17): never drive both"],
        [34, "gnd"],
        [35, "GPIO2_C2", "gpio2", 18, ["I2S1_SCLK"]],
        [36, "GPIO2_A0", "gpio2", 0, ["UART2_TX_M1"]],
        [37, "GPIO2_B7", "gpio2", 15, ["I2S1_MCLK"]],
        [38, "GPIO2_A1", "gpio2", 1, ["UART2_RX_M1"]],
        [39, "gnd"],
        [40, "GPIO0_D3", "gpio0", 27, ["SPDIF_TX_M0"]],
      ]),
    },
  ],
  gpio: "gpiochip",
  bootConfig: null,
  health: "sysfs",
  sources: [
    "https://github.com/libre-computer-project/libretech-wiring-tool/blob/master/libre-computer/roc-rk3328-cc/gpio.map",
    "https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/rockchip/rk3328-roc-cc.dts",
  ],
  notes: ["Header overlays come from Libre's wiring tool: `sudo ldto merge <name>` (i2c-0, i2c-1, spi-0-1cs, spi-0-2cs, uart-1, pwm-2). BenchBob doesn't edit the merged DTB it writes."],
};

export default [lePotato, renegade];
