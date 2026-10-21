//! i2c-chips.test.ts "identification", against the byte-level FakeBus.

use std::collections::HashMap;

use super::*;
use crate::fake::{FakeBus, FakeChip, FnChip, RegChip, SMBUS_ONLY, WordChip};
use crate::fixtures::{bme280_regs, eeprom_regs, lm75_words, mpu6050_regs};
use crate::io::errno;

fn ctx() -> ProbeCtx {
    ProbeCtx::default()
}

fn ident(bus: &mut FakeBus, addr: u8, c: &ProbeCtx) -> Ident {
    identify(bus, addr, c).unwrap().expect("identified")
}

fn state(id: &Ident) -> ChipState {
    ChipState::new(id.probe.clone())
}

fn with_opts(id: &Ident, opts: &[(&str, f64)]) -> ChipState {
    let mut s = state(id);
    for (k, v) in opts {
        s.opts.insert((*k).into(), *v);
    }
    s
}

fn read(bus: &mut FakeBus, addr: u8, id: &Ident, s: &mut ChipState) -> Readout {
    (id.chip.read.unwrap())(&mut Dev::new(bus, addr), s).unwrap()
}

fn measure(bus: &mut FakeBus, addr: u8, id: &Ident, s: &mut ChipState) -> Readout {
    (id.chip.measure.unwrap())(&mut Dev::new(bus, addr), s).unwrap()
}

fn val(r: &Readout, k: &str) -> V {
    r.values.iter().find(|v| v.k == k).map(|v| v.v.clone()).unwrap_or(V::Null)
}

fn num(r: &Readout, k: &str) -> f64 {
    val(r, k).num().unwrap_or_else(|| panic!("{k} is not a number in {r:?}"))
}

fn close(a: f64, b: f64, digits: i32) -> bool {
    (a - b).abs() < 10f64.powi(-digits) / 2.0
}

/// A command-style chip that NACKs unknown commands and records every write.
struct CmdChip {
    handlers: HashMap<&'static str, Vec<u8>>,
    writes: Vec<Vec<u8>>,
    next: Vec<u8>,
}

impl CmdChip {
    fn new(h: &[(&'static str, Vec<u8>)]) -> Self {
        Self { handlers: h.iter().cloned().collect(), writes: Vec::new(), next: Vec::new() }
    }
}

impl FakeChip for CmdChip {
    fn write(&mut self, b: &[u8]) -> Result<(), I2cError> {
        self.writes.push(b.to_vec());
        let key: String = b.iter().map(|x| format!("{x:02x}")).collect();
        match self.handlers.get(key.as_str()) {
            Some(r) => {
                self.next = r.clone();
                Ok(())
            }
            None => Err(I2cError::new(errno::EREMOTEIO, "nack")),
        }
    }
    fn read(&mut self, n: usize) -> Result<Vec<u8>, I2cError> {
        if self.next.is_empty() {
            return Err(I2cError::new(errno::EREMOTEIO, "nack"));
        }
        let out = self.next.iter().take(n).copied().collect();
        self.next.clear();
        Ok(out)
    }
}

fn sens(words: &[u16]) -> Vec<u8> {
    words.iter().flat_map(|w| [(w >> 8) as u8, *w as u8, crc8sens(&[(w >> 8) as u8, *w as u8], 0xff)]).collect()
}

#[test]
fn bme280_id_and_normal_mode_auto_read_decodes_the_worked_example() {
    let mut bus = FakeBus::default().add(0x76, RegChip::new(bme280_regs()));
    let id = ident(&mut bus, 0x76, &ctx());
    assert_eq!(id.chip.id, "bosch");
    assert_eq!(id.probe.variant.as_deref(), Some("BME280"));
    let r = read(&mut bus, 0x76, &id, &mut state(&id));
    assert!(close(num(&r, "t"), 25.08, 2));
    assert!(close(num(&r, "p"), 1006.53, 1));
    assert_eq!(r.stale, None);
}

#[test]
fn bme280_on_an_smbus_only_adapter_still_identifies_and_decodes() {
    let mut bus = FakeBus::new(SMBUS_ONLY).add(0x76, RegChip::new(bme280_regs()));
    let id = ident(&mut bus, 0x76, &ctx());
    assert_eq!(id.probe.variant.as_deref(), Some("BME280"));
    assert!(close(num(&read(&mut bus, 0x76, &id, &mut state(&id)), "t"), 25.08, 2));
}

#[test]
fn bme280_in_sleep_mode_reads_stale_and_measure_triggers_forced_mode() {
    let mut regs = bme280_regs();
    regs.insert(0xf4, 0x24);
    let mut bus = FakeBus::default().add(0x77, RegChip::new(regs));
    let id = ident(&mut bus, 0x77, &ctx());
    let mut st = state(&id);
    assert_eq!(read(&mut bus, 0x77, &id, &mut st).stale, Some(true));
    // Identification + read wrote no data.
    assert_eq!(bus.get::<RegChip>(0x77).writes.iter().filter(|w| w.len() > 1).count(), 0);
    let m = measure(&mut bus, 0x77, &id, &mut st);
    assert!(close(num(&m, "t"), 25.08, 2));
    assert_eq!(bus.get::<RegChip>(0x77).regs[0xf4] & 3, 1); // forced
}

#[test]
fn mpu6050_asleep_reads_stale_and_measure_wakes_then_restores() {
    let mut regs = mpu6050_regs();
    regs.insert(0x6b, 0x40);
    let mut bus = FakeBus::default().add(0x68, RegChip::new(regs));
    let id = ident(&mut bus, 0x68, &ctx());
    assert_eq!(id.probe.variant.as_deref(), Some("MPU-6050"));
    let mut st = state(&id);
    assert_eq!(read(&mut bus, 0x68, &id, &mut st).stale, Some(true));
    let m = measure(&mut bus, 0x68, &id, &mut st);
    assert!(close(num(&m, "az"), 1.0, 3));
    assert!(close(num(&m, "t"), 25.0, 0));
    assert_eq!(bus.get::<RegChip>(0x68).regs[0x6b], 0x40);
}

#[test]
fn pcf8574_is_classified_by_pure_reads_and_never_written() {
    let mut bus = FakeBus::default().add(0x27, FnChip::reads(|n| vec![0xf7; n]));
    let id = ident(&mut bus, 0x27, &ctx());
    assert_eq!(id.probe.variant.as_deref(), Some("PCF8574"));
    let r = read(&mut bus, 0x27, &id, &mut state(&id));
    let levels: String = r.pins.unwrap().iter().map(|p| p.level.to_string()).collect();
    assert_eq!(levels, "11101111");
    assert!(bus.get::<FnChip>(0x27).writes.is_empty());
}

#[test]
fn pcf8574_on_an_smbus_only_adapter_is_classified_without_writes() {
    let mut bus = FakeBus::new(SMBUS_ONLY).add(0x20, FnChip::reads(|n| vec![0xff; n]));
    assert_eq!(ident(&mut bus, 0x20, &ctx()).probe.variant.as_deref(), Some("PCF8574"));
    assert!(bus.get::<FnChip>(0x20).writes.is_empty());
}

#[test]
fn mcp23017_register_file_gives_pins_with_direction() {
    let mut bus = FakeBus::default().add(0x20, RegChip::new([(0x00, 0xff), (0x01, 0x0f), (0x0d, 0x01), (0x12, 0x05), (0x13, 0x80)]));
    let id = ident(&mut bus, 0x20, &ctx());
    assert_eq!(id.probe.variant.as_deref(), Some("MCP23017 / MCP23008"));
    let pins = read(&mut bus, 0x20, &id, &mut state(&id)).pins.unwrap();
    assert_eq!((pins[0].name.as_str(), pins[0].level, pins[0].dir), ("GPA0", 1, Some("in")));
    assert_eq!((pins[15].name.as_str(), pins[15].level, pins[15].dir), ("GPB7", 1, Some("out")));
    assert_eq!(pins[8].pull, Some(true));
}

#[test]
fn htu21d_at_0x40_is_recognised_before_any_ti_id_probe() {
    let mut bus = FakeBus::default().add(0x40, CmdChip::new(&[("e7", vec![0x02]), ("fcc9", vec![0x32, 0, 0, 0, 0, 0])]));
    let id = ident(&mut bus, 0x40, &ctx());
    assert_eq!(id.chip.id, "htu21d");
    assert_eq!(id.probe.variant.as_deref(), Some("HTU21D"));
    // 0xFE is soft reset.
    assert!(!bus.get::<CmdChip>(0x40).writes.iter().any(|w| w[0] == 0xfe));
}

#[test]
fn ina219_has_no_id_register_so_it_is_identified_last_and_uses_the_shunt() {
    let mut bus = FakeBus::default().add(0x40, WordChip::new([(0, 0x399f), (1, 1000), (2, (1250 << 3) | 2), (3, 0), (4, 0), (5, 0)]));
    let id = ident(&mut bus, 0x40, &ctx());
    assert_eq!(id.chip.id, "ina219");
    let r = read(&mut bus, 0x40, &id, &mut with_opts(&id, &[("shunt", 0.1)]));
    assert!(close(num(&r, "v"), 5.0, 3));
    assert!(close(num(&r, "i"), 100.0, 2));
    assert!(close(num(&r, "p"), 500.0, 1));
}

#[test]
fn ina226_and_ina260_by_ti_manufacturer_and_die_id() {
    let mut bus = FakeBus::default()
        .add(0x41, WordChip::new([(0, 0x4127), (1, 4000), (2, 9600), (0xfe, 0x5449), (0xff, 0x2260)]))
        .add(0x42, WordChip::new([(0, 0x6127), (1, 800), (2, 9600), (3, 120), (0xfe, 0x5449), (0xff, 0x2270)]));
    let a = ident(&mut bus, 0x41, &ctx());
    assert_eq!(a.probe.variant.as_deref(), Some("INA226"));
    let ra = read(&mut bus, 0x41, &a, &mut with_opts(&a, &[("shunt", 0.1)]));
    assert!(close(num(&ra, "v"), 12.0, 3));
    assert!(close(num(&ra, "i"), 100.0, 2));
    let b = ident(&mut bus, 0x42, &ctx());
    assert_eq!(b.probe.variant.as_deref(), Some("INA260"));
    assert!(close(num(&read(&mut bus, 0x42, &b, &mut state(&b)), "i"), 1000.0, 1));
}

#[test]
fn at_0x48_lm75_vs_ads1115_vs_tmp102() {
    let mut bus = FakeBus::default()
        .add(0x48, WordChip::new(lm75_words()))
        .add(0x49, WordChip::new([(0, 0x4000), (1, 0x8583), (2, 0x8000), (3, 0x7fff)]))
        .add(0x4a, WordChip::new([(0, 0x1900), (1, 0x60a0), (2, 0x4b00), (3, 0x5000)]));
    let lm = ident(&mut bus, 0x48, &ctx());
    assert_eq!(lm.chip.id, "lm75");
    assert_eq!(num(&read(&mut bus, 0x48, &lm, &mut state(&lm)), "t"), 25.5);
    let ads = ident(&mut bus, 0x49, &ctx());
    assert_eq!(ads.probe.variant.as_deref(), Some("ADS1115"));
    assert_eq!(read(&mut bus, 0x49, &ads, &mut state(&ads)).stale, Some(true)); // single-shot POR
    let tmp = ident(&mut bus, 0x4a, &ctx());
    assert_eq!(tmp.chip.id, "tmp102");
    assert_eq!(num(&read(&mut bus, 0x4a, &tmp, &mut state(&tmp)), "t"), 25.0);
}

#[test]
fn sht3x_and_sht4x_by_crc_checked_read_commands_and_measure_decodes() {
    let sht3 = CmdChip::new(&[("f32d", sens(&[0x8010])), ("2400", sens(&[0x6666, 0x8000]))]);
    let sht4 = CmdChip::new(&[("89", sens(&[0x1234, 0x5678])), ("fd", sens(&[0x6666, 0x8000]))]);
    let mut bus = FakeBus::default().add(0x44, sht4).add(0x45, sht3);
    assert_eq!(ident(&mut bus, 0x44, &ctx()).chip.id, "sht4x");
    let b = ident(&mut bus, 0x45, &ctx());
    assert_eq!(b.chip.id, "sht3x");
    let m = measure(&mut bus, 0x45, &b, &mut state(&b));
    assert!(close(num(&m, "t"), 25.0, 0));
    assert!(close(num(&m, "rh"), 50.0, 0));
}

#[test]
fn an_mcp4725_at_0x62_answers_the_bare_read_and_is_never_sent_scd4x_commands() {
    let mut bus = FakeBus::default().add(0x62, FnChip::reads(|n| [0xc0, 0x80, 0x00, 0x08, 0x00].into_iter().take(n).collect()));
    let id = ident(&mut bus, 0x62, &ctx());
    assert_eq!(id.chip.id, "mcp4725");
    assert!(bus.get::<FnChip>(0x62).writes.is_empty());
    let r = read(&mut bus, 0x62, &id, &mut with_opts(&id, &[("vdd", 3.3)]));
    assert_eq!(num(&r, "code"), 2048.0);
    assert!(close(num(&r, "vout"), 1.65, 3));
}

#[test]
fn scd4x_at_0x62_nacks_bare_reads_and_is_found_by_its_serial_number_command() {
    let mut bus = FakeBus::default().add(0x62, CmdChip::new(&[("3682", sens(&[0xabcd, 0x1234, 0x5678])), ("e4b8", sens(&[0x0000]))]));
    let id = ident(&mut bus, 0x62, &ctx());
    assert_eq!(id.chip.id, "scd4x");
    assert_eq!(id.probe.data.periodic, Some(false));
    assert_eq!(id.probe.note.as_deref(), Some("Serial 0xabcd12345678"));
}

#[test]
fn tca9548a_at_0x70_is_receive_byte_only() {
    let mut bus = FakeBus::default().add(0x70, FnChip::reads(|_| vec![0x05]));
    let id = ident(&mut bus, 0x70, &ctx());
    assert_eq!(id.chip.id, "tca9548");
    assert_eq!(val(&read(&mut bus, 0x70, &id, &mut state(&id)), "mask"), V::Str("0, 2".into()));
    assert!(bus.get::<FnChip>(0x70).writes.is_empty());
}

#[test]
fn pca9685_at_0x40_and_its_all_call_address() {
    let mut regs: Vec<(u8, u8)> = vec![(0x00, 0x21), (0x01, 0x04), (0x02, 0xe2), (0x03, 0xe4), (0x04, 0xe8), (0x05, 0xe0), (0xfe, 121)];
    // channel 0: on 0, off 2048 (50 %); channel 1 full on
    regs.extend([(0x06, 0), (0x07, 0), (0x08, 0x00), (0x09, 0x08), (0x0b, 0x10)]);
    let mut bus = FakeBus::default().add(0x40, RegChip::new(regs)).add(0x70, FnChip::reads(|_| vec![0]));
    let mut c = ctx();
    let id = ident(&mut bus, 0x40, &c);
    assert_eq!(id.chip.id, "pca9685");
    c.found.insert(0x40, "pca9685");
    assert_eq!(ident(&mut bus, 0x70, &c).chip.id, "pca9685-allcall");
    let r = read(&mut bus, 0x40, &id, &mut state(&id));
    assert!(close(num(&r, "freq"), 50.0, 0));
    let ch = r.channels.unwrap();
    assert_eq!(ch[0].duty, 50.0);
    assert_eq!(ch[1].full, Some("on"));
}

#[test]
fn ds3231_time_and_temperature_and_an_eeprom_with_a_hat_header() {
    let rtc = [0x30, 0x45, 0x13, 0x05, 0x17, 0x10, 0x26, 0, 0, 0, 0, 0, 0, 0, 0x1c, 0x00, 0, 0x19, 0x40];
    let mut bus = FakeBus::default().add(0x68, RegChip::from_list(&rtc)).add(0x50, RegChip::new(eeprom_regs()));
    let id = ident(&mut bus, 0x68, &ctx());
    assert_eq!(id.probe.variant.as_deref(), Some("DS3231"));
    let r = read(&mut bus, 0x68, &id, &mut state(&id));
    assert_eq!(val(&r, "time"), V::Str("2026-10-17 13:45:30".into()));
    assert_eq!(num(&r, "t"), 25.25);
    let e = ident(&mut bus, 0x50, &ctx());
    assert_eq!(e.chip.id, "at24");
    let r = read(&mut bus, 0x50, &e, &mut with_opts(&e, &[("size", 256.0)]));
    assert_eq!(val(&r, "hat"), V::Str("BenchBob · Bench HAT".into()));
}

#[test]
fn claimed_addresses_fail_with_ebusy_rather_than_being_probed() {
    let mut bus = FakeBus::default().add(0x76, RegChip::new(bme280_regs()));
    bus.claimed.insert(0x76);
    let e = bus.quick(0x76, false, false).unwrap_err();
    assert_eq!(e.errno, errno::EBUSY);
    assert!(e.message().contains("claimed"));
}

#[test]
fn every_chip_id_resolves() {
    for id in ["bosch", "mpu", "rtc", "at24", "lm75", "ina219", "sht3x", "scd4x", "expander", "ds2482"] {
        assert!(chip_by_id(id).is_some(), "{id}");
    }
    // The mux's high-address entry resolves to the first definition.
    assert!(chip_by_id("tca9548").unwrap().has_addr(0x70));
}

// ---------- beyond the TypeScript tests ----------

#[test]
fn identification_never_writes_data_bytes_to_register_chips() {
    // Every register-file probe sends at most a pointer byte (writes of length 1).
    let mut bus = FakeBus::default().add(0x76, RegChip::new(bme280_regs())).add(0x68, RegChip::new(mpu6050_regs()));
    ident(&mut bus, 0x76, &ctx());
    ident(&mut bus, 0x68, &ctx());
    assert!(bus.get::<RegChip>(0x76).writes.iter().all(|w| w.len() <= 1));
    assert!(bus.get::<RegChip>(0x68).writes.iter().all(|w| w.len() <= 1));
}

#[test]
fn an_unknown_address_identifies_as_nothing_and_has_hints() {
    let mut bus = FakeBus::default().add(0x27, FnChip { read: None, accept_writes: false, writes: vec![] });
    assert!(identify(&mut bus, 0x27, &ctx()).unwrap().is_none());
    assert_eq!(address_hints(0x27), vec!["GPIO expander", "HD44780 LCD backpack (PCF8574)"]);
    assert_eq!(address_hints(0x76), vec!["PCA9685", "BME280", "BMP388", "DPS310", "TCA9548A"]);
    assert!(!address_hints(0x70).contains(&"PCA9685 All Call"));
}

#[test]
fn cross_hints_explain_pairs() {
    let found = BTreeMap::from([(0x57, "at24"), (0x68, "rtc")]);
    assert!(cross_hints(&found)[&0x57].contains("AT24C32"));
    assert!(cross_hints(&BTreeMap::from([(0x0d, "ak8963"), (0x68, "mpu")])).contains_key(&0x0c));
}

#[test]
fn deep_identify_lets_pointer_probes_run_at_0x39() {
    // A TSL2561 at 0x39 answers pure reads with the same byte too.
    let mut regs = vec![(0x8a, 0x50)];
    regs.extend((0..=255u8).filter(|r| *r != 0x8a).map(|r| (r, 0)));
    let mut chip = RegChip::new(regs);
    chip.auto_inc = false;
    let mut bus = FakeBus::default().add(0x39, chip);
    assert_eq!(ident(&mut bus, 0x39, &ctx()).probe.variant.as_deref(), Some("PCF8574A"));
    let deep = ProbeCtx { deep: true, ..ctx() };
    assert_eq!(ident(&mut bus, 0x39, &deep).probe.variant.as_deref(), Some("TSL2561"));
}

#[test]
fn stub_commands_load_every_image() {
    let c = crate::fixtures::stub_commands(22);
    assert!(c.contains(&"i2cset -y 22 0x76 0xd0 0x60 b".to_string()));
    assert!(c.contains(&"i2cset -y 22 0x48 0x0 0x8019 w".to_string()));
}
