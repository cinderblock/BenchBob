// Ported from peripherals/lib/gpio-config.test.ts, with the same desky captures.

use super::*;

const DESKY: &str = include_str!("../../fixtures/config-desky.txt"); // /boot/firmware/config.txt on desky, 2026-10-17
const PINCTRL: &str = include_str!("../../fixtures/pinctrl-desky.txt");

fn ch() -> Changes {
    Changes::default()
}

fn edit(text: &str, c: &Changes) -> String {
    edit_config(text, c).unwrap()
}

fn diff(a: &str, b: &str) -> String {
    unified_diff(a, b, "config.txt", 3)
}

#[test]
fn pinctrl_get_desky_capture() {
    let p = parse_pinctrl(PINCTRL);
    assert_eq!(p.len(), 58);
    assert_eq!((p[2].gpio, p[2].fsel.as_str(), p[2].func.as_str(), p[2].pull, p[2].level), (2, "a0", "SDA1", Some("up"), Some(1)));
    assert_eq!((p[4].gpio, p[4].fsel.as_str(), p[4].func.as_str(), p[4].pull), (4, "ip", "input", Some("none")));
    assert_eq!((p[14].fsel.as_str(), p[14].func.as_str()), ("a5", "TXD1"));
    assert_eq!((p[42].fsel.as_str(), p[42].func.as_str(), p[42].drive, p[42].level), ("op", "output", None, Some(0)));
    let x = &parse_pinctrl("20: op dh pn | hi // GPIO20 = output")[0];
    assert_eq!((x.drive, x.pull, x.level), (Some("high"), Some("none"), Some(1)));
    let x = &parse_pinctrl("100: (null)    -- | -- // BT_ON/FWGPIO0 = (null)")[0];
    assert_eq!((x.gpio, x.level, x.pull), (100, None, None));
}

#[test]
fn sections_cm4_cm5_are_conditional_later_all_resets() {
    let l = parse_config(DESKY);
    assert_eq!(l.iter().find(|x| x.key.as_deref() == Some("otg_mode")).unwrap().section, "cm4");
    assert_eq!(l.iter().find(|x| x.text.starts_with("dtoverlay=dwc2")).unwrap().section, "cm5");
    assert_eq!(l.iter().find(|x| x.key.as_deref() == Some("enable_uart")).unwrap().section, "all");
}

#[test]
fn reads_deskys_interfaces() {
    let r = read_interfaces(DESKY);
    assert_eq!(r.i2c, (true, None));
    assert!(!r.spi);
    assert!(r.uart);
    assert_eq!((r.onewire.0, r.onewire.1), (true, Some(4)));
    assert!(r.conditional.is_empty());
}

#[test]
fn enable_spi_uncomments_the_stock_line_in_place() {
    let n = edit(DESKY, &Changes { spi: Some(true), ..ch() });
    assert!(n.contains("\ndtparam=spi=on\n"));
    assert!(!n.contains("#dtparam=spi=on"));
    assert_eq!(n.split('\n').count(), DESKY.split('\n').count());
    assert!(read_interfaces(&n).spi);
    let d = diff(DESKY, &n);
    assert!(d.contains("-#dtparam=spi=on\n+dtparam=spi=on"));
    assert_eq!(d.lines().filter(|l| l.starts_with("@@")).count(), 1);
}

#[test]
fn i2c_off_and_400k_edits_the_existing_line_and_appends_in_the_trailing_all_block() {
    let n = edit(DESKY, &Changes { i2c: Some((Some(false), Some(400000.0))), ..ch() });
    assert_eq!(read_interfaces(&n).i2c, (false, Some(400000)));
    assert!(n.contains("dtparam=i2c_arm=off"));
    assert!(n.trim_end().ends_with("# Added by BenchBob (GPIO panel → Interfaces)\ndtparam=i2c_arm_baudrate=400000"));
    // The file already ends in an [all] context, so no extra [all] header.
    assert_eq!(n.lines().filter(|l| l.starts_with("[all]")).count(), 2);
}

#[test]
fn onewire_move_pin_then_disable() {
    let moved = edit(DESKY, &Changes { onewire: Some((None, Some(17))), ..ch() });
    assert!(moved.contains("dtoverlay=w1-gpio,gpiopin=17"));
    assert_eq!(read_interfaces(&moved).onewire.1, Some(17));
    let off = edit(DESKY, &Changes { onewire: Some((Some(false), None)), ..ch() });
    assert!(off.contains("#dtoverlay=w1-gpio,gpiopin=4"));
    assert!(!read_interfaces(&off).onewire.0);
    assert!(edit_config(DESKY, &Changes { onewire: Some((Some(true), Some(40))), ..ch() }).is_err());
}

#[test]
fn appends_under_a_new_all_when_the_file_ends_inside_a_conditional_section() {
    let txt = "dtparam=audio=on\n[pi4]\narm_boost=1\n";
    let n = edit(txt, &Changes { uart: Some(true), onewire: Some((Some(true), None)), ..ch() });
    assert_eq!(
        n,
        "dtparam=audio=on\n[pi4]\narm_boost=1\n\n[all]\n# Added by BenchBob (GPIO panel → Interfaces)\nenable_uart=1\ndtoverlay=w1-gpio,gpiopin=4\n"
    );
    let r = read_interfaces(&n);
    assert!(r.uart);
    assert_eq!((r.onewire.0, r.onewire.1), (true, Some(4)));
}

#[test]
fn never_edits_a_key_inside_a_conditional_section_and_reports_it() {
    let txt = "dtparam=spi=on\n[cm4]\ndtparam=spi=off\n[all]\n";
    let r = read_interfaces(txt);
    assert!(r.spi);
    assert_eq!(r.conditional, vec![("cm4".to_string(), "dtparam=spi=off".to_string())]);
    assert_eq!(edit(txt, &Changes { spi: Some(false), ..ch() }), "dtparam=spi=off\n[cm4]\ndtparam=spi=off\n[all]\n");
}

#[test]
fn multi_param_dtparam_lines_are_edited_param_wise_and_crlf_is_kept() {
    let txt = "dtparam=i2c_arm=on,i2c_arm_baudrate=100000,spi=on\r\n";
    assert_eq!(
        edit(txt, &Changes { i2c: Some((None, Some(1000000.0))), ..ch() }),
        "dtparam=i2c_arm=on,i2c_arm_baudrate=1000000,spi=on\r\n"
    );
}

#[test]
fn no_op_changes_produce_no_diff() {
    assert_eq!(diff(DESKY, &edit(DESKY, &Changes { i2c: Some((Some(true), None)), uart: Some(true), ..ch() })), "");
    let c = Changes {
        extra_i2c: vec![("i2c3".into(), Some(false), None, None)],
        extra_uart: vec![("uart2".into(), Some(false), None)],
        i2c_gpio: Some(I2cGpioChange { enabled: Some(false), ..Default::default() }),
        ..ch()
    };
    assert_eq!(diff(DESKY, &edit(DESKY, &c)), "");
}

#[test]
fn desky_has_no_extra_buses_and_defaults_come_from_the_overlay_readme() {
    let r = read_interfaces(DESKY);
    assert_eq!(r.extra_i2c.get("i2c3").unwrap(), &I2cX { enabled: false, pins: "pins_4_5".into(), baudrate: None });
    assert_eq!(r.extra_i2c.get("i2c6").unwrap().pins, "pins_22_23");
    assert_eq!(r.extra_uart.get("uart5").unwrap(), &UartX { enabled: false, ctsrts: false });
    assert_eq!(r.i2c_gpio, I2cGpio { enabled: false, sda: 23, scl: 24, bus: None, delay_us: None });
}

#[test]
fn reads_overlay_lines_params_and_conditional_copies() {
    let r = read_interfaces(
        "dtoverlay=i2c4,pins_6_7,baudrate=400000\ndtoverlay=uart3,ctsrts\ndtoverlay=uart4,rs485\ndtoverlay=i2c-gpio,i2c_gpio_sda=17,i2c_gpio_scl=27,bus=11\n[cm4]\ndtoverlay=i2c5\n",
    );
    assert_eq!(r.extra_i2c.get("i2c4").unwrap(), &I2cX { enabled: true, pins: "pins_6_7".into(), baudrate: Some(400000) });
    assert!(r.extra_uart.get("uart3").unwrap().ctsrts);
    assert!(r.extra_uart.get("uart4").unwrap().ctsrts); // rs485 implies ctsrts
    assert!(!r.extra_i2c.get("i2c5").unwrap().enabled);
    assert_eq!(r.i2c_gpio, I2cGpio { enabled: true, sda: 17, scl: 27, bus: Some(11), delay_us: None });
    assert_eq!(r.conditional, vec![("cm4".to_string(), "dtoverlay=i2c5".to_string())]);
}

#[test]
fn enable_appends_under_the_benchbob_block_and_disable_comments_out() {
    let c = Changes {
        extra_i2c: vec![("i2c6".into(), Some(true), None, Some(400000.0))],
        extra_uart: vec![("uart5".into(), Some(true), None)],
        ..ch()
    };
    let n = edit(DESKY, &c);
    assert!(n.trim_end().ends_with("dtoverlay=i2c6,pins_22_23,baudrate=400000\ndtoverlay=uart5"));
    let r = read_interfaces(&n);
    assert_eq!(r.extra_i2c.get("i2c6").unwrap(), &I2cX { enabled: true, pins: "pins_22_23".into(), baudrate: Some(400000) });
    assert!(r.extra_uart.get("uart5").unwrap().enabled);
    let off = edit(&n, &Changes { extra_uart: vec![("uart5".into(), Some(false), None)], ..ch() });
    assert!(off.contains("\n#dtoverlay=uart5"));
    assert!(!read_interfaces(&off).extra_uart.get("uart5").unwrap().enabled);
}

#[test]
fn edits_keep_unmanaged_params_and_unchanged_state_is_not_rewritten() {
    let txt = "dtoverlay=uart2,rs485_invert_rts\ndtoverlay=i2c3\n";
    assert_eq!(
        edit(txt, &Changes { extra_uart: vec![("uart2".into(), None, Some(true))], ..ch() }),
        "dtoverlay=uart2,ctsrts,rs485_invert_rts\ndtoverlay=i2c3\n"
    );
    assert_eq!(edit(txt, &Changes { extra_i2c: vec![("i2c3".into(), Some(true), Some("pins_4_5".into()), Some(100000.0))], ..ch() }), txt);
    assert_eq!(
        edit(txt, &Changes { extra_i2c: vec![("i2c3".into(), None, Some("pins_2_3".into()), None)], ..ch() }),
        "dtoverlay=uart2,rs485_invert_rts\ndtoverlay=i2c3,pins_2_3\n"
    );
}

#[test]
fn i2c_gpio_pins_bus_number_validation() {
    let g = |c: I2cGpioChange| Changes { i2c_gpio: Some(c), ..ch() };
    let n =
        edit("", &g(I2cGpioChange { enabled: Some(true), sda: Some(17.0), scl: Some(27.0), bus: Some(Some(11.0)), ..Default::default() }));
    assert!(n.contains("dtoverlay=i2c-gpio,i2c_gpio_sda=17,i2c_gpio_scl=27,bus=11"));
    assert!(
        edit_config("", &g(I2cGpioChange { enabled: Some(true), bus: Some(Some(1.0)), ..Default::default() }))
            .unwrap_err()
            .contains("bus number")
    );
    assert!(
        edit_config("", &g(I2cGpioChange { enabled: Some(true), sda: Some(5.0), scl: Some(5.0), ..Default::default() }))
            .unwrap_err()
            .contains("different")
    );
    assert!(
        edit_config("", &Changes { extra_i2c: vec![("i2c3".into(), Some(true), Some("pins_8_9".into()), None)], ..ch() })
            .unwrap_err()
            .contains("pins_4_5")
    );
    assert!(
        edit_config("", &Changes { extra_uart: vec![("uart9".into(), Some(true), None)], ..ch() }).unwrap_err().contains("Unknown UART")
    );
}

#[test]
fn pin_usage_and_conflicts() {
    let r = read_interfaces(DESKY);
    assert_eq!(pin_usage(&r).iter().map(|u| u.gpio).collect::<Vec<_>>(), [2, 3, 14, 15, 4]);
    assert!(pin_conflicts(&r).is_empty());
    // desky's 1-Wire is on GPIO4, which UART3 TX and I²C3's default pins also use.
    let c = pin_conflicts(&read_interfaces(&edit(
        DESKY,
        &Changes { extra_uart: vec![("uart3".into(), Some(true), None)], extra_i2c: vec![("i2c3".into(), Some(true), None, None)], ..ch() },
    )));
    assert_eq!(
        c,
        vec![
            (4, vec!["1-Wire data".to_string(), "I²C3 SDA".into(), "UART3 TX".into()]),
            (5, vec!["I²C3 SCL".to_string(), "UART3 RX".into()]),
        ]
    );
    // UART5 CTS/RTS land on the primary UART's pins.
    let c = pin_conflicts(&read_interfaces(&edit(DESKY, &Changes { extra_uart: vec![("uart5".into(), Some(true), Some(true))], ..ch() })));
    assert_eq!(c.iter().map(|x| x.0).collect::<Vec<_>>(), [14, 15]);
}

#[test]
fn json_shapes_match_the_typescript_objects() {
    let r = read_interfaces(DESKY);
    let j = r.to_json();
    let keys: Vec<&String> = j.as_object().unwrap().keys().collect();
    // "audio" is new here (the LED strips' PWM needs analog audio off).
    assert_eq!(keys, ["i2c", "spi", "audio", "uart", "onewire", "extraI2c", "extraUart", "i2cGpio", "conditional"]);
    assert_eq!(j["extraI2c"]["i2c3"].to_string(), r#"{"enabled":false,"pins":"pins_4_5","baudrate":null}"#);
    assert_eq!(catalog()["extraI2c"]["i2c3"].to_string(), r#"{"choices":{"pins_4_5":[4,5],"pins_2_3":[2,3]},"def":"pins_4_5"}"#);
}
