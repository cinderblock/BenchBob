use super::*;

fn pi_leds(spidevs: &[&str]) -> (tempfile::TempDir, Leds) {
    let t = tempfile::tempdir().unwrap();
    for n in spidevs {
        std::fs::write(t.path().join(n), "").unwrap();
    }
    let boards = benchbob_boards::Boards::builtin();
    let input =
        benchbob_boards::DetectInput { compatible: vec!["raspberrypi,4-model-b".into(), "brcm,bcm2711".into()], ..Default::default() };
    let board = Arc::new(benchbob_boards::describe(boards.get("rpi").unwrap(), &input, true));
    let gpio = benchbob_gpio::Gpio::new(board);
    let l = Leds::with_dev_dir(gpio.handle(), t.path().to_path_buf());
    (t, l)
}

fn post(l: &Leds, id: Option<i64>, body: Value) -> Result<Value, HttpError> {
    l.inner.configure(id, body.to_string().as_bytes())
}

#[test]
fn one_wire_strings_need_a_spi_mosi_pin() {
    let (_t, l) = pi_leds(&["spidev0.0", "spidev0.1"]);
    let v = post(&l, None, json!({ "chip": "ws2812b", "pin": 10, "pixels": 60, "name": "desk" })).unwrap();
    assert_eq!(v["pin"], 10);
    assert_eq!(v["state"], "off"); // output control is off
    let e = post(&l, None, json!({ "chip": "ws2812b", "pin": 4, "pixels": 10 })).unwrap_err();
    assert_eq!(e.status, 400);
    assert!(e.message.contains("GPIO10 (SPI0)"), "{}", e.message);
    // SPI can't invert; the string's still as it was.
    let e = post(&l, Some(1), json!({ "invert": true })).unwrap_err();
    assert!(e.message.contains("invert"));
    assert_eq!(l.inner.view().unwrap()["strings"][0]["invert"], false);

    // Without any spidev the hint says how to get one.
    let (_t, l) = pi_leds(&[]);
    let e = post(&l, None, json!({ "chip": "ws2812b", "pin": 10, "pixels": 10 })).unwrap_err();
    assert!(e.message.contains("enable SPI0 in the GPIO panel"), "{}", e.message);
}

#[test]
fn clocked_strings_use_spi_or_any_pin() {
    let (_t, l) = pi_leds(&["spidev0.0"]);
    // On SPI0's MOSI + SCLK: spidev.
    post(&l, None, json!({ "chip": "apa102", "pin": 10, "clockPin": 11, "pixels": 30 })).unwrap();
    // Elsewhere: bit-banged, inversion allowed.
    let v = post(&l, None, json!({ "chip": "ws2801", "pin": 5, "clockPin": 6, "pixels": 30, "invert": true })).unwrap();
    assert_eq!(v["invert"], true);
    // A one-wire string can't share SPI0 with the APA102 (proto refuses the shared pin).
    assert_eq!(post(&l, None, json!({ "chip": "ws2812b", "pin": 10, "pixels": 3 })).unwrap_err().status, 409);
}

#[test]
fn capabilities_list_pins_by_kind() {
    let (_t, l) = pi_leds(&["spidev0.0", "spidev4.0"]);
    let v = l.inner.view().unwrap();
    let pins = v["capabilities"]["pinInfo"].as_array().unwrap();
    let one_wire: Vec<u64> = pins.iter().filter(|p| p["oneWire"] == true).map(|p| p["gpio"].as_u64().unwrap()).collect();
    // Header order: GPIO18 (pin 12), 10 (19), 6 (31), 12 (32), 13 (33), 19 (35).
    assert_eq!(one_wire, [18, 10, 6, 12, 13, 19]);
    let g10 = pins.iter().find(|p| p["gpio"] == 10).unwrap();
    assert_eq!(g10["header"], 19);
    assert_eq!(g10["via"], "SPI0 MOSI");
    let g18 = pins.iter().find(|p| p["gpio"] == 18).unwrap();
    assert_eq!((g18["oneWire"].clone(), g18["via"].clone()), (json!(true), json!("PWM0")));
    assert_eq!(g10["refusal"], Value::Null);
    assert!(v["capabilities"]["oneWireHint"].as_str().unwrap().contains("GPIO6 (SPI4)"));
}

#[test]
fn settings_round_trip_with_ids_and_patterns() {
    let (t, l) = pi_leds(&["spidev0.0"]);
    post(&l, None, json!({ "chip": "ws2812b", "pin": 10, "pixels": 60, "name": "desk", "order": "GRBW", "brightness": 40 })).unwrap();
    post(&l, None, json!({ "chip": "custom", "pin": 5, "clockPin": null, "pixels": 8 })).unwrap_err(); // one-wire off SPI
    post(&l, None, json!({ "chip": "apa102", "pin": 20, "clockPin": 21, "pixels": 12, "name": "shelf" })).unwrap();
    l.inner.remove(1).unwrap();
    post(
        &l,
        None,
        json!({ "chip": "custom", "pin": 10, "pixels": 5, "timing": { "t0hNs": 350, "t1hNs": 700, "periodNs": 1250, "resetUs": 80 } }),
    )
    .unwrap();
    l.inner.with_body(br##"{"name":"chase","color":"#ff0000"}"##, |m, a, now| m.set_pattern(3, a, now).map(|_| 3)).unwrap();
    let saved = l.inner.saved();
    assert_eq!(saved.as_array().unwrap().len(), 2);
    assert!(saved[0].get("state").is_none());

    let l2 = Leds::with_dev_dir(l.inner.gpio.clone(), t.path().to_path_buf());
    l2.inner.load(&saved);
    let a = l.inner.view().unwrap()["strings"].clone();
    let b = l2.inner.view().unwrap()["strings"].clone();
    assert_eq!(a, b);
    assert_eq!(b[0]["id"], 3);
    assert_eq!(b[0]["timing"]["resetUs"], 80);
    assert_eq!(b[0]["pattern"]["name"], "chase");
    // New strings don't reuse a restored id.
    let v = post(&l2, None, json!({ "chip": "apa102", "pin": 5, "clockPin": 6, "pixels": 1 })).unwrap();
    assert_eq!(v["id"], 4);
}

#[test]
fn frames_encode_per_chip() {
    let mut out = vec![];
    let (_t, l) = pi_leds(&["spidev0.0"]);
    post(&l, None, json!({ "chip": "apa102", "pin": 10, "clockPin": 11, "pixels": 2 })).unwrap();
    let c = l.inner.model.lock().unwrap().job(0).unwrap().config;
    encode_frame(Encoding::Apa102, &c, &[1, 2, 3, 4, 5, 6], &mut out);
    assert_eq!(out.len(), encode::apa102_len(2));
    assert_eq!(&out[4..12], &[0xFF, 1, 2, 3, 0xFF, 4, 5, 6]);
    encode_frame(Encoding::Raw, &c, &[9, 9], &mut out);
    assert_eq!(out, [9, 9]);
}

#[test]
fn one_wire_strings_on_the_pwm_pins() {
    let (_t, l) = pi_leds(&[]);
    // No SPI at all: PWM still takes one-wire strings, inverted too.
    let v = post(&l, None, json!({ "chip": "ws2812b", "pin": 18, "pixels": 60, "invert": true })).unwrap();
    assert_eq!(v["invert"], true);
    post(&l, None, json!({ "chip": "ws2812b", "pin": 13, "pixels": 10 })).unwrap();
    // GPIO12 is PWM0 too: taken.
    let e = post(&l, None, json!({ "chip": "ws2812b", "pin": 12, "pixels": 10 })).unwrap_err();
    assert!(e.message.contains("PWM0 already drives LED string"), "{}", e.message);
    // Moving the first string within PWM0 is fine.
    post(&l, Some(1), json!({ "pin": 12 })).unwrap();
}

#[test]
fn the_pwm_helper_config_covers_both_channels() {
    let osc = pwm::osc_hz(true);
    let f = pwm::fit(&Timing::WS2812B, osc).unwrap();
    let mut hub = PwmHub::default();
    assert!(hub.config(osc).is_none());
    hub.chans[1] = Some(PwmChan { pin: 19, invert: true, fit: f, max_bytes: 700, reset_us: 300 });
    hub.chans[0] = Some(PwmChan { pin: 18, invert: false, fit: f, max_bytes: 1200, reset_us: 80 });
    let c = hub.config(osc).unwrap();
    assert_eq!(c.pins, [Some(18), Some(19)]);
    assert_eq!(c.invert, [false, true]);
    assert_eq!(c.clock_div, 23);
    assert_eq!((c.max_bytes, c.reset_us), (1200, 300));
}
