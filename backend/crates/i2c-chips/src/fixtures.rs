//! Register images of real chips (i2c-fixtures.ts), shared by the unit tests and
//! [`stub_commands`], which prints the i2cset commands that load them into i2c-stub to test
//! the driver end to end. Calibration values are the worked examples from the Bosch
//! datasheets, so the decoded numbers are known: BME280/BMP280 adc_T 519888 → 25.08 °C,
//! adc_P 415148 → 100653.27 Pa.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

fn le(v: u16) -> [u8; 2] {
    v.to_le_bytes()
}

/// BME280 bytes by register (0x88..0xA1 calibration, 0xD0 id, 0xE1..0xE7 humidity cal, data).
pub fn bme280_regs() -> BTreeMap<u8, u8> {
    let cal: [i32; 12] = [27504, 26435, -1000, 36477, -10685, 3024, 2855, 140, -7, 15500, -14600, 6000];
    let mut r = BTreeMap::new();
    for (i, b) in cal.iter().flat_map(|v| le(*v as u16)).enumerate() {
        r.insert(0x88 + i as u8, b);
    }
    r.insert(0xa1, 75); // H1
    // H2=362, H3=0, H4=313, H5=50, H6=30 (typical BME280 values)
    for (i, b) in [0x6a, 0x01, 0x00, 0x13, 0x29, 0x03, 0x1e].into_iter().enumerate() {
        r.insert(0xe1 + i as u8, b);
    }
    r.insert(0xd0, 0x60);
    r.insert(0xf2, 0x01); // ctrl_hum osrs_h ×1
    r.insert(0xf4, 0x27); // normal mode, osrs_t/p ×1
    // adc_P 415148, adc_T 519888, adc_H 27000
    for (i, b) in [0x65, 0x5a, 0xc0, 0x7e, 0xed, 0x00, 0x69, 0x78].into_iter().enumerate() {
        r.insert(0xf7 + i as u8, b);
    }
    r
}

/// MPU-6050, awake, ±2 g / ±250 °/s, lying flat (Z = +1 g), die 25 °C-ish.
pub fn mpu6050_regs() -> BTreeMap<u8, u8> {
    let mut r = BTreeMap::from([(0x75, 0x68), (0x6b, 0x00), (0x1b, 0x00), (0x1c, 0x00)]);
    let temp = crate::num::js_round((25.0 - 36.53) * 340.0) as i32;
    let words = [164, -328, 16384, temp, 131, 0, -262];
    for (i, b) in words.iter().flat_map(|v| (*v as u16).to_be_bytes()).enumerate() {
        r.insert(0x3b + i as u8, b);
    }
    r
}

/// 24C02 with a Raspberry Pi HAT header and a vendor atom.
pub fn eeprom_regs() -> BTreeMap<u8, u8> {
    let vendor = "BenchBob";
    let product = "Bench HAT";
    let mut atom: Vec<u8> = (1..=16).collect();
    atom.extend(le(0x1234));
    atom.extend(le(2));
    atom.push(vendor.len() as u8);
    atom.push(product.len() as u8);
    atom.extend(vendor.bytes().chain(product.bytes()));
    let mut bytes: Vec<u8> = b"R-Pi".to_vec();
    bytes.extend([1, 0]);
    bytes.extend(le(1));
    bytes.extend(le(12 + 8 + atom.len() as u16));
    bytes.extend([0, 0]);
    bytes.extend(le(1));
    bytes.extend(le(0));
    bytes.extend(le(atom.len() as u16));
    bytes.extend([0, 0]);
    bytes.extend(atom);
    bytes.into_iter().enumerate().map(|(i, b)| (i as u8, b)).collect()
}

/// LM75 words (big-endian values as the chip holds them): 25.5 °C, default thresholds.
pub fn lm75_words() -> BTreeMap<u8, u16> {
    BTreeMap::from([(0x00, 0x1980), (0x01, 0x0000), (0x02, 0x4b00), (0x03, 0x5000)])
}

/// i2cset commands that load these images into i2c-stub (chip_addr=0x76,0x48,0x68,0x50).
pub fn stub_commands(bus: u32) -> Vec<String> {
    let mut cmds = Vec::new();
    let mut bytes = |addr: u8, regs: BTreeMap<u8, u8>| {
        for (k, v) in regs {
            cmds.push(format!("i2cset -y {bus} 0x{addr:x} 0x{k:x} 0x{v:x} b"));
        }
    };
    bytes(0x76, bme280_regs());
    bytes(0x68, mpu6050_regs());
    bytes(0x50, eeprom_regs());
    // i2c-stub keeps one 16-bit word per register; SMBus words are low byte first, so store
    // the byte-swapped value for big-endian chips.
    for (k, v) in lm75_words() {
        cmds.push(format!("i2cset -y {bus} 0x48 0x{k:x} 0x{:x} w", v.swap_bytes()));
    }
    cmds
}
