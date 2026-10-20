// ==================== BeagleBoard.org ====================
//
// UNTESTED: nobody here has a Beagle board. The pinout is BeagleBoard.org's and the
// kernel's; what hasn't been checked is BenchBob running on the hardware.
//
// The classic BeagleBone Black / Green (TI AM335x) can't run BenchBob at all: the AM335x
// is a 32-bit-only Cortex-A8 and Bun has no 32-bit ARM build. So this file covers the
// 64-bit boards:
//
//  - BeagleY-AI (TI AM67A / J722S): a Raspberry Pi-style 40-pin header, fully mapped.
//    Sources, cross-checked: the kernel's gpio-line-names (each line is named after the
//    Pi's BCM number, "GPIO14"…) and the pad comments in BeagleBoard.org's pinmux file;
//    balls and alternate functions from pinout.beagleboard.io. That site lists pin 8 as
//    GPIO0_14; both device trees say main_gpio1 line 14 (pad comment
//    "(F24) MCASP0_ACLKR.GPIO1_14"), which is what's used here.
//      https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/ti/k3-am67a-beagley-ai.dts
//      https://github.com/beagleboard/BeagleBoard-DeviceTrees/blob/v6.12.x-Beagle/src/arm64/ti/k3-am67a-beagley-ai-pinmux.dtsi
//      https://pinout.beagleboard.io/
//    Chips: gpio-davinci labels each controller by device name ("600000.gpio" main_gpio0,
//    "601000.gpio" main_gpio1, "4201000.gpio" mcu_gpio0); the gpiochipN numbers aren't
//    documented and depend on probe order.
//    Boot config: overlays go on the default label's `fdtoverlays` line in
//    /boot/firmware/extlinux/extlinux.conf, .dtbo files in /boot/firmware/overlays/
//    (https://docs.beagleboard.org/boards/beagley/ai/demos/using-pwm.html).
//
//  - BeagleBone AI-64 (TDA4VM): recognised, but no header map. Its device tree has no
//    line names, many P8/P9 pins connect two SoC balls each, and BeagleBoard.org's own
//    tables disagree with their worked example, so nothing here would be trustworthy
//    without the hardware (https://docs.beagleboard.org/boards/beaglebone/ai-64/04-expansion.html).

import { hasCompat, pins, type BoardDef, type OverlayChoice } from "./types.ts";

const pwm = (name: string, pin: number, what: string): OverlayChoice => ({ name, what: `${what} on pin ${pin}`, pins: [pin] });

const beagleyAi: BoardDef = {
  id: "beagley-ai",
  name: "BeagleY-AI",
  vendor: "BeagleBoard.org",
  soc: "TI AM67A",
  tested: false,
  match: (i) => (hasCompat(i, /^beagle,am67a-beagley-ai$/) ? 50 : 0),
  chips: { main0: ["600000.gpio"], main1: ["601000.gpio"], mcu0: ["4201000.gpio"] },
  headers: [
    {
      id: "40pin",
      name: "40-pin header",
      pins: pins([
        [1, "3v3"],
        [2, "5v"],
        [3, "GPIO2 (E11)", "mcu0", 18, ["MCU_I2C0_SDA"], "/dev/i2c-1, on by default at 100 kHz; fixed 2.2 kΩ pull-up"],
        [4, "5v"],
        [5, "GPIO3 (B13)", "mcu0", 17, ["MCU_I2C0_SCL"], "/dev/i2c-1; fixed 2.2 kΩ pull-up"],
        [6, "gnd"],
        [7, "GPIO4 (W26)", "main0", 38, ["GPMC0_WAIT1", "VOUT0_EXTPCLKIN", "GPMC0_A21", "UART6_RXD", "AUDIO_EXT_REFCLK2", "EQEP2_I"]],
        [8, "GPIO14 (F24)", "main1", 14, ["MCASP0_ACLKR", "SPI2_CLK", "UART1_TXD", "EHRPWM0_B", "EQEP1_I"], "UART TX (main_uart1)"],
        [9, "gnd"],
        [10, "GPIO15 (C27)", "main1", 13, ["MCASP0_AFSR", "SPI2_CS0", "UART1_RXD", "EHRPWM0_A", "EQEP1_S"], "UART RX (main_uart1)"],
        [11, "GPIO17 (A26)", "main1", 8, ["MCASP0_AXR2", "SPI2_D1", "UART1_RTSn", "UART6_TXD", "ECAP2_IN_APWM_OUT", "EQEP0_B"]],
        [12, "GPIO18 (D25)", "main1", 11, ["MCASP0_ACLKX", "SPI2_CS1", "ECAP2_IN_APWM_OUT", "EQEP1_A"]],
        [13, "GPIO27 (N22)", "main0", 33, ["GPMC0_OEn_REn", "MCASP1_AXR1"]],
        [14, "gnd"],
        [15, "GPIO22 (R27)", "main0", 41, ["GPMC0_CSn0", "I2C4_SCL", "MCASP2_AXR14"]],
        [16, "GPIO23 (B5)", "mcu0", 7, ["MCU_UART0_CTSn", "MCU_TIMER_IO0", "MCU_SPI1_D0"]],
        [17, "3v3"],
        [18, "GPIO24 (C8)", "mcu0", 10, ["WKUP_UART0_TXD", "MCU_SPI1_CS2"]],
        [19, "GPIO10 (B12)", "mcu0", 3, ["MCU_SPI0_D0"], "SPI0 MOSI"],
        [20, "gnd"],
        [21, "GPIO9 (C11)", "mcu0", 4, ["MCU_SPI0_D1"], "SPI0 MISO"],
        [22, "GPIO25 (P21)", "main0", 42, ["GPMC0_CSn1", "I2C4_SDA", "MCASP2_AXR15"]],
        [23, "GPIO11 (A9)", "mcu0", 2, ["MCU_SPI0_CLK"], "SPI0 SCLK"],
        [24, "GPIO8 (C12)", "mcu0", 0, ["MCU_SPI0_CS0", "WKUP_TIMER_IO1"], "SPI0 CE0"],
        [25, "gnd"],
        [26, "GPIO7 (B3)", "mcu0", 9, ["WKUP_UART0_RXD", "MCU_SPI0_CS2"], "SPI0 CE1"],
        [27, "GPIO0 (D11)", "mcu0", 20, ["WKUP_I2C0_SDA"], "HAT EEPROM bus, shared with the PMIC, core regulator, board EEPROM and RTC: leave unused"],
        [28, "GPIO1 (B9)", "mcu0", 19, ["WKUP_I2C0_SCL"], "HAT EEPROM bus, shared with the PMIC: leave unused"],
        [29, "GPIO5 (B20)", "main1", 15, ["SPI0_CS0", "EHRPWM0_A"]],
        [30, "gnd"],
        [31, "GPIO6 (D20)", "main1", 17, ["SPI0_CLK", "CP_GEMAC_CPTS0_TS_SYNC", "EHRPWM1_A"]],
        [32, "GPIO12 (C20)", "main1", 16, ["SPI0_CS1", "CP_GEMAC_CPTS0_TS_COMP", "EHRPWM0_B", "ECAP0_IN_APWM_OUT", "MAIN_ERRORn", "EHRPWM_TZn_IN5"], "PWM0"],
        [33, "GPIO13 (E19)", "main1", 18, ["SPI0_D0", "CP_GEMAC_CPTS0_HW1TSPUSH", "EHRPWM1_B"], "PWM1"],
        [34, "gnd"],
        [35, "GPIO19 (C26)", "main1", 12, ["MCASP0_AFSX", "SPI2_CS3", "AUDIO_EXT_REFCLK1", "EQEP1_B"]],
        [36, "GPIO16 (A25)", "main1", 7, ["MCASP0_AXR3", "SPI2_D0", "UART1_CTSn", "UART6_RXD", "ECAP1_IN_APWM_OUT", "EQEP0_A"]],
        [37, "GPIO26 (P26)", "main0", 36, ["GPMC0_BE1n", "MCASP2_AXR12"]],
        [38, "GPIO20 (F23)", "main1", 10, ["MCASP0_AXR0", "AUDIO_EXT_REFCLK0", "EHRPWM1_B", "EQEP0_I"]],
        [39, "gnd"],
        [40, "GPIO21 (B25)", "main1", 9, ["MCASP0_AXR1", "SPI2_CS2", "ECAP1_IN_APWM_OUT", "MAIN_ERRORn", "EHRPWM1_A", "EQEP0_S"]],
      ]),
    },
  ],
  gpio: "gpiochip",
  bootConfig: {
    files: [{ kind: "extlinux", path: "/boot/firmware/extlinux/extlinux.conf" }],
    overlayDirs: ["/boot/firmware/overlays"],
    overlayRef: "/overlays/",
    // Overlays BeagleBoard.org ships for the header (BeagleBoard-DeviceTrees src/arm64/overlays).
    overlays: [
      { name: "i2c1-400000", what: "I²C1 at 400 kHz (pins 3/5 are I²C at 100 kHz without it)", pins: [3, 5] },
      { name: "uart-ttyama0", what: "UART on pins 8/10 as /dev/ttyAMA0", pins: [8, 10] },
      { name: "spidev0", what: "Bit-banged SPI (spi-gpio) on 19/21/23, chip selects 24 and 26", pins: [19, 21, 23, 24, 26] },
      pwm("pwm-epwm0-gpio14", 8, "EPWM0"),
      pwm("pwm-epwm0-gpio15", 10, "EPWM0"),
      pwm("pwm-epwm0-gpio5", 29, "EPWM0"),
      pwm("pwm-epwm0-gpio12", 32, "EPWM0"),
      pwm("pwm-epwm1-gpio6", 31, "EPWM1"),
      pwm("pwm-epwm1-gpio13", 33, "EPWM1"),
      pwm("pwm-epwm1-gpio20", 38, "EPWM1"),
      pwm("pwm-epwm1-gpio21", 40, "EPWM1"),
      pwm("pwm-ecap0-gpio12", 32, "ECAP0"),
      pwm("pwm-ecap1-gpio16", 36, "ECAP1"),
      pwm("pwm-ecap1-gpio21", 40, "ECAP1"),
      pwm("pwm-ecap2-gpio17", 11, "ECAP2"),
      pwm("pwm-ecap2-gpio18", 12, "ECAP2"),
      pwm("pps-gpio18", 12, "PPS input"),
    ],
  },
  health: "sysfs",
  sources: [
    "https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/ti/k3-am67a-beagley-ai.dts",
    "https://github.com/beagleboard/BeagleBoard-DeviceTrees/blob/v6.12.x-Beagle/src/arm64/ti/k3-am67a-beagley-ai-pinmux.dtsi",
    "https://pinout.beagleboard.io/",
    "https://docs.beagleboard.org/boards/beagley/ai/demos/using-pwm.html",
  ],
  notes: ["UART6 (pins 7/11/36) may already be the Bluetooth UART on BeagleBoard.org's images."],
};

const beagleboneAi64: BoardDef = {
  id: "beaglebone-ai64",
  name: "BeagleBone AI-64",
  vendor: "BeagleBoard.org",
  soc: "TI TDA4VM",
  tested: false,
  match: (i) => (hasCompat(i, /^beagle,j721e-beagleboneai64$/) ? 50 : 0),
  headers: [],
  gpio: "gpiochip",
  bootConfig: null,
  health: "sysfs",
  sources: ["https://docs.beagleboard.org/boards/beaglebone/ai-64/04-expansion.html"],
  notes: ["No P8/P9 map: many header pins connect two SoC balls and the published tables disagree, so the GPIO panel lists the SoC's lines instead."],
};

export default [beagleyAi, beagleboneAi64];
