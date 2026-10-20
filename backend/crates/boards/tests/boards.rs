// Ported from peripherals/boards/boards.test.ts: the same detection fixtures, now against
// the TOML board files (backend/boards/*.toml, embedded at build time).

use benchbob_boards::*;

fn arm(compatible: &[&str], model: &str) -> DetectInput {
    DetectInput {
        arch: "aarch64".into(),
        compatible: compatible.iter().map(|s| s.to_string()).collect(),
        model: model.into(),
        dmi: Dmi::default(),
        wsl: false,
    }
}

fn pc(f: impl FnOnce(&mut Dmi), wsl: bool) -> DetectInput {
    let mut dmi = Dmi::default();
    f(&mut dmi);
    DetectInput { arch: "x86_64".into(), compatible: vec![], model: String::new(), dmi, wsl }
}

/// /proc/device-tree/compatible and model for each board, from the kernel dts files the
/// board files cite (and, for desky, read off the machine).
fn fixtures() -> Vec<(&'static str, DetectInput, &'static str)> {
    vec![
        ("rpi", arm(&["raspberrypi,4-model-b", "brcm,bcm2711"], "Raspberry Pi 4 Model B Rev 1.1"), "Raspberry Pi 4 Model B Rev 1.1"), // desky
        ("rpi", arm(&["raspberrypi,5-model-b", "brcm,bcm2712"], "Raspberry Pi 5 Model B Rev 1.0"), "Raspberry Pi 5 Model B Rev 1.0"),
        ("rpi", arm(&["raspberrypi,model-zero-2-w", "brcm,bcm2837"], "Raspberry Pi Zero 2 W Rev 1.0"), "Raspberry Pi Zero 2 W Rev 1.0"),
        (
            "rpi",
            arm(&["raspberrypi,4-compute-module", "brcm,bcm2711"], "Raspberry Pi Compute Module 4 Rev 1.0"),
            "Raspberry Pi Compute Module 4 Rev 1.0",
        ),
        // Orange Pi: mainline strings, then Orange Pi's own kernel's.
        ("orangepi-5", arm(&["xunlong,orangepi-5", "rockchip,rk3588s"], "Xunlong Orange Pi 5"), "Xunlong Orange Pi 5"),
        ("orangepi-5", arm(&["rockchip,rk3588s-orangepi-5", "rockchip,rk3588"], "Orange Pi 5"), "Orange Pi 5"),
        ("orangepi-5b", arm(&["xunlong,orangepi-5b", "rockchip,rk3588s"], "Xunlong Orange Pi 5B"), "Xunlong Orange Pi 5B"),
        ("orangepi-5-plus", arm(&["xunlong,orangepi-5-plus", "rockchip,rk3588"], "Xunlong Orange Pi 5 Plus"), "Xunlong Orange Pi 5 Plus"),
        ("orangepi-5-plus", arm(&["rockchip,rk3588-orangepi-5-plus", "rockchip,rk3588"], "RK3588 OPi 5 Plus"), "RK3588 OPi 5 Plus"),
        ("orangepi-5-other", arm(&["xunlong,orangepi-5-max", "rockchip,rk3588"], "Xunlong Orange Pi 5 Max"), "Xunlong Orange Pi 5 Max"),
        ("orangepi-5-other", arm(&["rockchip,rk3588s-orangepi-5-pro", "rockchip,rk3588"], "RK3588S OPi 5 Pro"), "RK3588S OPi 5 Pro"),
        (
            "orangepi-3b",
            arm(&["xunlong,orangepi-3b-v2.1", "xunlong,orangepi-3b", "rockchip,rk3566"], "Xunlong Orange Pi 3B v2.1"),
            "Xunlong Orange Pi 3B v2.1",
        ),
        ("orangepi-3b", arm(&["rockchip,rk3566-orangepi-3b", "rockchip,rk3566"], "Rockchip RK3566 OPi 3B"), "Rockchip RK3566 OPi 3B"),
        ("orangepi-zero3", arm(&["xunlong,orangepi-zero3", "allwinner,sun50i-h618"], "OrangePi Zero3"), "OrangePi Zero3"),
        ("orangepi-zero3", arm(&["xunlong,orangepi-zero3", "allwinner,sun50i-h616"], "OrangePi Zero3"), "OrangePi Zero3"),
        ("orangepi-zero2w", arm(&["xunlong,orangepi-zero2w", "allwinner,sun50i-h618"], "OrangePi Zero 2W"), "OrangePi Zero 2W"),
        ("rock-5b", arm(&["radxa,rock-5b", "rockchip,rk3588"], "Radxa ROCK 5B"), "Radxa ROCK 5B"),
        ("rock-5b-plus", arm(&["radxa,rock-5b-plus", "rockchip,rk3588"], "Radxa ROCK 5B+"), "Radxa ROCK 5B+"),
        ("beagley-ai", arm(&["beagle,am67a-beagley-ai", "ti,j722s"], "BeagleBoard.org BeagleY-AI"), "BeagleBoard.org BeagleY-AI"),
        (
            "beaglebone-ai64",
            arm(&["beagle,j721e-beagleboneai64", "ti,j721e"], "BeagleBoard.org BeagleBone AI-64"),
            "BeagleBoard.org BeagleBone AI-64",
        ),
        (
            "le-potato",
            arm(&["libretech,aml-s905x-cc", "amlogic,s905x", "amlogic,meson-gxl"], "Libre Computer AML-S905X-CC"),
            "Libre Computer AML-S905X-CC (Le Potato)",
        ),
        (
            "le-potato",
            arm(&["libretech,aml-s905x-cc-v2", "amlogic,s905x", "amlogic,meson-gxl"], "Libre Computer AML-S905X-CC V2"),
            "Libre Computer AML-S905X-CC V2 (Sweet Potato)",
        ),
        ("roc-rk3328-cc", arm(&["firefly,roc-rk3328-cc", "rockchip,rk3328"], "Firefly ROC-RK3328-CC"), "Firefly ROC-RK3328-CC (Renegade)"),
        (
            "x86",
            pc(
                |d| {
                    d.sys_vendor = "ASUSTeK COMPUTER INC.".into();
                    d.product_name = "MINIPC PN51-E1".into();
                    d.board_vendor = "ASUSTeK COMPUTER INC.".into();
                    d.board_name = "PN51-E1".into();
                },
                false,
            ),
            "ASUSTeK COMPUTER INC. MINIPC PN51-E1",
        ),
        (
            "x86",
            pc(
                |d| {
                    d.sys_vendor = "Intel(R) Client Systems".into();
                    d.product_name = "NUC8i5BEH".into();
                },
                false,
            ),
            "Intel(R) Client Systems NUC8i5BEH",
        ),
        (
            "x86",
            pc(
                |d| {
                    d.sys_vendor = "To be filled by O.E.M.".into();
                    d.product_name = "To be filled by O.E.M.".into();
                    d.board_vendor = "AZW".into();
                    d.board_name = "SEi".into();
                },
                false,
            ),
            "AZW SEi",
        ),
        (
            "x86",
            pc(
                |d| {
                    d.sys_vendor = "Default string".into();
                    d.product_name = "Default string".into();
                },
                false,
            ),
            "x86-64 PC",
        ),
        ("x86", pc(|_| {}, true), "Windows Subsystem for Linux"), // bbtest-trixie: no DMI at all
        (
            "generic",
            arm(&["pine64,rockpro64-v2.1", "pine64,rockpro64", "rockchip,rk3399"], "Pine64 RockPro64 v2.1"),
            "Pine64 RockPro64 v2.1",
        ),
        (
            "generic",
            DetectInput {
                arch: "riscv64".into(),
                compatible: vec!["sifive,hifive-unmatched-a00".into()],
                model: "SiFive HiFive Unmatched A00".into(),
                ..Default::default()
            },
            "SiFive HiFive Unmatched A00",
        ),
    ]
}

fn boards() -> Boards {
    let b = Boards::builtin();
    assert!(b.errors.is_empty(), "board files failed to load: {:?}", b.errors);
    b
}

#[test]
fn detection() {
    let b = boards();
    for (id, input, name) in fixtures() {
        let def = b.detect(&input);
        assert_eq!(def.id, id, "{input:?}");
        assert_eq!(describe(&def, &input, false).name, name);
    }
}

#[test]
fn only_the_pi_4_family_counts_as_tested_among_pis() {
    let b = boards();
    let f = fixtures();
    let (pi4, pi5) = (&f[0].1, &f[1].1);
    assert!(describe(&b.detect(pi4), pi4, false).tested);
    assert!(!describe(&b.detect(pi5), pi5, false).tested);
}

#[test]
fn ids_are_unique_and_generic_is_the_fallback() {
    let b = boards();
    let mut ids: Vec<&str> = b.defs.iter().map(|d| d.id.as_str()).collect();
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n);
    assert_eq!(b.detect(&arm(&[], "")).id, "generic");
    // Every TypeScript board was converted.
    assert_eq!(n, 16);
}

#[test]
fn board_info_shape() {
    let b = boards();
    let i = &fixtures()[0].1;
    let info = describe(&b.detect(i), i, false).info_with(&|_| false);
    assert_eq!(info["id"], "rpi");
    assert_eq!(info["soc"], "BCM2711");
    assert_eq!(info["tested"], true);
    assert_eq!(info["capabilities"]["gpio"], "rpi");
    assert_eq!(info["capabilities"]["bootConfig"]["kind"], "config.txt");
    assert_eq!(info["capabilities"]["health"], "vcgencmd");
    assert_eq!(info["capabilities"]["headers"][0]["id"], "j8");
    assert_eq!(info["capabilities"]["headers"][0]["pins"], 40);
    // The exact key order the Bun server sends.
    let keys: Vec<&str> = info.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["id", "name", "model", "tested", "forced", "vendor", "soc", "capabilities", "sources", "notes"]);
    let pi5 = &fixtures()[1].1;
    assert_eq!(describe(&b.detect(pi5), pi5, false).info()["soc"], "BCM2712");
}

#[test]
fn boot_config_is_whichever_file_this_image_has() {
    let b = boards();
    let opi = b.get("orangepi-5").unwrap();
    assert_eq!(
        opi.boot_config_with(&|p| p == "/boot/orangepiEnv.txt").unwrap(),
        BootFile { kind: BootKind::OrangepiEnv, path: "/boot/orangepiEnv.txt".into() }
    );
    assert_eq!(opi.boot_config_with(&|p| p.starts_with("/boot/")).unwrap().kind, BootKind::ArmbianEnv); // Armbian's first
    assert!(opi.boot_config_with(&|_| false).is_none());
    assert_eq!(b.get("rpi").unwrap().boot_config_with(&|_| false).unwrap().kind, BootKind::ConfigTxt);
    assert!(b.get("x86").unwrap().boot_config().is_none());
}

#[test]
fn overlay_catalogs_only_name_pins_that_are_gpios_on_the_first_header() {
    for d in boards().defs {
        for o in d.boot.iter().flat_map(|b| &b.overlays) {
            let h = &d.headers[0];
            for p in &o.pins {
                let kind = h.pins.iter().find(|x| x.pin == *p).map(|x| x.kind);
                assert_eq!(kind, Some(PinKind::Gpio), "{} {} pin {p}", d.id, o.name);
            }
        }
    }
}

#[test]
fn dmi_name_drops_placeholders_and_repeated_vendors() {
    let i = pc(
        |d| {
            d.sys_vendor = "LENOVO".into();
            d.product_name = "LENOVO ThinkCentre".into();
        },
        false,
    );
    assert_eq!(dmi_name(&i), "LENOVO ThinkCentre");
    let i = pc(
        |d| {
            d.sys_vendor = "System manufacturer".into();
            d.product_name = "System Product Name".into();
            d.board_vendor = "ASUSTeK COMPUTER INC.".into();
            d.board_name = "PRIME B450M-A".into();
        },
        false,
    );
    assert_eq!(dmi_name(&i), "ASUSTeK COMPUTER INC. PRIME B450M-A");
}

#[test]
fn header_maps() {
    for b in boards().defs {
        for h in &b.headers {
            assert!([13, 26, 40, 46].contains(&h.pins.len()), "{} {}", b.id, h.name);
            // Physical pins 1..N in order, each once.
            assert_eq!(h.pins.iter().map(|p| p.pin).collect::<Vec<_>>(), (1..=h.pins.len() as u32).collect::<Vec<_>>());
            let gpio: Vec<&HeaderPin> = h.pins.iter().filter(|p| p.kind == PinKind::Gpio).collect();
            assert!(!gpio.is_empty());
            assert!(h.pins.iter().any(|p| p.kind == PinKind::Gnd));
            if b.gpio.panel == Panel::Rpi {
                let mut nums: Vec<u32> = gpio.iter().map(|p| p.gpio.unwrap()).collect();
                nums.sort();
                nums.dedup();
                assert_eq!(nums.len(), gpio.len());
                continue;
            }
            // Every GPIO pin names a known chip and a line, and no line appears twice.
            let mut keys = Vec::new();
            for p in &gpio {
                assert!(b.chips.contains_key(p.chip.as_ref().unwrap()));
                assert!(p.line.unwrap() < 512);
                assert!(p.soc.as_ref().is_some_and(|s| !s.is_empty()));
                keys.push(format!("{}:{}", p.chip.as_ref().unwrap(), p.line.unwrap()));
            }
            let n = keys.len();
            keys.sort();
            keys.dedup();
            assert_eq!(keys.len(), n, "{} {}", b.id, h.name);
        }
    }
}

#[test]
fn every_non_pi_board_cites_its_pinout_source_and_untested_boards_are_marked() {
    for b in boards().defs {
        if !b.headers.is_empty() {
            assert!(!b.sources.is_empty(), "{}", b.id);
        }
        if !["rpi", "x86", "generic"].contains(&b.id.as_str()) {
            assert!(!b.tested, "{}", b.id);
        }
    }
}

#[test]
fn header_pins_serialize_like_the_typescript_tables() {
    let b = boards();
    let pi = &b.get("rpi").unwrap().headers[0].pins;
    assert_eq!(serde_json::to_string(&pi[0]).unwrap(), r#"{"pin":1,"kind":"3v3"}"#);
    assert_eq!(serde_json::to_string(&pi[2]).unwrap(), r#"{"pin":3,"kind":"gpio","gpio":2}"#);
    let opi = &b.get("orangepi-5").unwrap().headers[0].pins;
    assert_eq!(
        serde_json::to_string(&opi[2]).unwrap(),
        r#"{"pin":3,"kind":"gpio","soc":"GPIO1_B7","chip":"gpio1","line":15,"funcs":["I2C5_SDA_M3","UART1_RX_M1","PWM13_M2"]}"#
    );
    let b5 = &b.get("orangepi-5b").unwrap().headers[0].pins;
    assert_eq!(serde_json::to_string(&b5[21]).unwrap(), r#"{"pin":22,"kind":"other","label":"PowerKey","note":"per wiringOP"}"#);
}

#[test]
fn resolve_chips_by_label_or_device_case_insensitively() {
    let b = boards();
    let def = b.get("le-potato").unwrap();
    let chips = [
        ("gpiochip0", "periphs-banks", "c8834000.bus:pinctrl@4b0"),
        ("gpiochip1", "aobus-banks", "c8100000.bus:pinctrl@14"),
        ("gpiochip2", "ftdi-cbus", "1-1:1.0"),
    ];
    let m = resolve_chips(def, &chips, |c| c.1, |c| c.2);
    assert_eq!(m["aobus"].0, "gpiochip1");
    assert_eq!(m["periphs"].0, "gpiochip0");
    let mut d2 = def.clone();
    d2.chips = [("x".to_string(), vec!["C8834000.BUS:PINCTRL@4B0".to_string()])].into_iter().collect();
    assert_eq!(resolve_chips(&d2, &chips, |c| c.1, |c| c.2)["x"].0, "gpiochip0");
}

#[test]
fn readers_follow_the_soc() {
    let b = boards();
    let f = fixtures();
    let r = |i: &DetectInput| describe(&b.detect(i), i, false).reader();
    assert_eq!(r(&f[0].1), Some(Reader::Bcm2711Gpiomem));
    assert_eq!(r(&f[1].1), Some(Reader::Rp1));
    assert_eq!(r(&f[2].1), Some(Reader::Bcm2711Gpiomem)); // Zero 2 W: BCM2837
    assert_eq!(r(&f[4].1), Some(Reader::Gpiochip));
}

// ---------- pluggable boards: no recompile ----------

#[test]
fn a_user_file_adds_a_board_and_overrides_a_shipped_one() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("my-board.toml"),
        r#"
id = "my-board"
name = "My Board"
vendor = "Me"
health = { script = "/usr/local/lib/my-board-health" }
[detect]
priority = 60
compatible = ['^me,my-board$']
[gpio]
panel = "gpiochip"
[chips]
main = ["gpio0"]
[[header]]
id = "j1"
name = "10-pin header"
pins = [{ pin = 1, kind = "3v3" }, { pin = 2, soc = "PA0", chip = "main", line = 0 }]
[boot]
files = [{ kind = "script", path = "/usr/local/lib/my-board-boot" }]
"#,
    )
    .unwrap();
    // Same id as a shipped board: replaces it.
    std::fs::write(dir.path().join("rock-5b.toml"), b"id = \"rock-5b\"\nname = \"Local ROCK\"\nhealth = \"sysfs\"\n[detect]\npriority = 50\ncompatible = ['^radxa,rock-5b$']\n[gpio]\npanel = \"gpiochip\"\n").unwrap();
    std::fs::write(dir.path().join("broken.toml"), b"id = \"broken\"\n[detect\n").unwrap();
    let b = Boards::load(None, Some(dir.path()));
    assert_eq!(b.errors.len(), 1);
    assert!(b.errors[0].file.ends_with("broken.toml"));
    let i = arm(&["me,my-board"], "My Board v1");
    let board = describe(&b.detect(&i), &i, false);
    assert_eq!(board.id, "my-board");
    let info = board.info_with(&|p| p == "/usr/local/lib/my-board-boot");
    assert_eq!(info["capabilities"]["health"], "script");
    assert_eq!(info["capabilities"]["bootConfig"]["kind"], "script");
    assert_eq!(b.get("rock-5b").unwrap().name, "Local ROCK");
    assert!(b.get("rock-5b").unwrap().headers.is_empty());
    assert_eq!(b.defs.len(), 17);
}

#[test]
fn bad_files_are_refused_with_a_reason() {
    assert!(parse("id = \"x\"\nname = \"x\"\nhealth = \"sysfs\"\n[detect]\npriority = 1\n[gpio]\npanel = \"gpiochip\"\n[[header]]\nid = \"h\"\nname = \"h\"\npins = [{ pin = 1, chip = \"nope\", line = 0, soc = \"A\" }]\n").unwrap_err().contains("isn't in [chips]"));
    assert!(
        parse("id = \"x\"\nname = \"x\"\nhealth = \"sysfs\"\nbogus = 1\n[detect]\npriority = 1\n[gpio]\npanel = \"gpiochip\"\n").is_err()
    );
    assert!(
        parse("id = \"x\"\nname = \"x\"\nhealth = \"sysfs\"\n[detect]\npriority = 1\ncompatible = ['(']\n[gpio]\npanel = \"gpiochip\"\n")
            .is_err()
    );
}
