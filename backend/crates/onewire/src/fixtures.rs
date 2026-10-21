//! A fake /sys/bus/w1/devices tree with one slave of every family the driver decodes, laid
//! out the way the 6.18 kernel modules expose them (w1-fixtures.ts). Used by the tests and
//! by side instances: build one with `BENCHBOB_W1_FIXTURE_OUT=/tmp/w1fx cargo test -p
//! benchbob-onewire write_fixture_tree`, then run the server with BENCHBOB_W1_ROOT pointing
//! at it. Binary attributes are generated, so CRCs are computed, not hand-typed.

use std::path::Path;

use crate::families::crc8w1;

/// ROM id bytes for "ff-xxxxxxxxxxxx" (serial printed big-endian, stored little-endian).
pub fn rom_bytes(id: &str) -> Vec<u8> {
    let fam = u8::from_str_radix(&id[..2], 16).unwrap_or(0);
    let mut b = vec![fam];
    let ser: Vec<u8> = (0..6).map(|i| u8::from_str_radix(&id[3 + i * 2..5 + i * 2], 16).unwrap_or(0)).collect();
    b.extend(ser.iter().rev());
    let c = crc8w1(&b);
    b.push(c);
    b
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

/// w1_therm w1_slave text for a scratchpad (CRC computed unless `bad_crc`).
pub fn therm_text(s8: &[u8], t_milli: i64, bad_crc: bool) -> String {
    let mut s = s8.to_vec();
    s.push(crc8w1(s8) ^ if bad_crc { 0x55 } else { 0 });
    let line = format!("{} : crc={:02x} {}", hex(&s), s[8], if bad_crc { "NO" } else { "YES" });
    format!("{line}\n{} t={t_milli}\n", hex(&s))
}

pub struct Fix {
    pub ds18b20: &'static str,
    pub ds18b20por: &'static str,
    pub ds18s20: &'static str,
    pub ds1822: &'static str,
    pub max31850: &'static str,
    pub ds28ea00: &'static str,
    pub ds2438: &'static str,
    pub ds2408: &'static str,
    pub ds2413: &'static str,
    pub ds2406: &'static str,
    pub ds2423: &'static str,
    pub ds2431: &'static str,
    pub ds2433: &'static str,
    pub ds28ec20: &'static str,
    pub ibutton: &'static str,
    pub ds1904: &'static str,
    pub ds1420: &'static str,
    pub ds2450: &'static str,
    pub unknown: &'static str,
    pub ghost: &'static str,
}

pub const FIX: Fix = Fix {
    ds18b20: "28-0000075b1a2c",
    ds18b20por: "28-0000075b9911",
    ds18s20: "10-000802b4c1d2",
    ds1822: "22-000000a1b2c3",
    max31850: "3b-0000001a2b3c",
    ds28ea00: "42-00000011aa22",
    ds2438: "26-000001f2e3d4",
    ds2408: "29-00000012ab34",
    ds2413: "3a-00000045cd67",
    ds2406: "12-000000123456",
    ds2423: "1d-000000fedcba",
    ds2431: "2d-0000004a5b6c",
    ds2433: "23-000000112233",
    ds28ec20: "43-000000445566",
    ibutton: "01-000017c2a3b4",
    ds1904: "24-0000000a0b0c",
    ds1420: "81-00000034a1b2",
    ds2450: "20-000000778899",
    unknown: "7e-000000010203",
    ghost: "00-280000000000",
};

impl Fix {
    /// Every slave, in the order the fixture's w1_master_slaves lists them.
    pub fn all(&self) -> [&'static str; 20] {
        [
            self.ds18b20,
            self.ds18b20por,
            self.ds18s20,
            self.ds1822,
            self.max31850,
            self.ds28ea00,
            self.ds2438,
            self.ds2408,
            self.ds2413,
            self.ds2406,
            self.ds2423,
            self.ds2431,
            self.ds2433,
            self.ds28ec20,
            self.ibutton,
            self.ds1904,
            self.ds1420,
            self.ds2450,
            self.unknown,
            self.ghost,
        ]
    }
}

/// Write the fixture tree to `root` (replacing it).
pub fn build_w1_fixture(root: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_dir_all(root);
    let master = root.join("w1_bus_master1");
    std::fs::create_dir_all(&master)?;
    let f = |dir: &Path, name: &str, data: &[u8]| std::fs::write(dir.join(name), data);
    let slaves = FIX.all();
    f(&master, "w1_master_name", b"w1_bus_master1\n")?;
    f(&master, "w1_master_slaves", format!("{}\n", slaves.join("\n")).as_bytes())?;
    f(&master, "w1_master_slave_count", format!("{}\n", slaves.len()).as_bytes())?;
    f(&master, "w1_master_search", b"-1\n")?;
    f(&master, "w1_master_pullup", b"1\n")?;
    f(&master, "w1_master_timeout", b"10\n")?;
    f(&master, "w1_master_timeout_us", b"0\n")?;
    f(&master, "w1_master_attempts", b"42\n")?;
    f(&master, "w1_master_max_slave_count", b"64\n")?;
    f(&master, "therm_bulk_read", b"0\n")?;
    f(&master, "w1_master_add", b"")?;
    f(&master, "w1_master_remove", b"")?;

    let slave = |id: &str, attrs: &[(&str, Vec<u8>)]| -> std::io::Result<()> {
        let d = root.join(id);
        std::fs::create_dir_all(&d)?;
        f(&d, "id", &rom_bytes(id))?;
        f(&d, "name", format!("{id}\n").as_bytes())?;
        for (k, v) in attrs {
            f(&d, k, v)?;
        }
        Ok(())
    };
    let s = |x: &str| x.as_bytes().to_vec();
    let therm = |id: &str, sp: &[u8], t: i64, res: &str| {
        slave(
            id,
            &[
                ("w1_slave", s(&therm_text(sp, t, false))),
                ("temperature", s(&format!("{t}\n"))),
                ("resolution", s(res)),
                ("ext_power", s("1\n")),
            ],
        )
    };

    // DS18B20 23.125 °C, 12-bit (config 0x7F)
    therm(FIX.ds18b20, &[0x72, 0x01, 0x4b, 0x46, 0x7f, 0xff, 0x0e, 0x10], 23125, "12\n")?;
    // DS18B20 that never converted: power-on 85 °C (0x0550)
    therm(FIX.ds18b20por, &[0x50, 0x05, 0x4b, 0x46, 0x7f, 0xff, 0x0c, 0x10], 85000, "12\n")?;
    // DS18S20 25.0 °C via COUNT_REMAIN 12, COUNT_PER_C 16: 25 − 0.25 + 4/16
    therm(FIX.ds18s20, &[0x32, 0x00, 0x4b, 0x46, 0xff, 0xff, 0x0c, 0x10], 25000, "9\n")?;
    // DS1822 −10.5 °C, 10-bit (config 0x3F)
    therm(FIX.ds1822, &[0x58, 0xff, 0x4b, 0x46, 0x3f, 0xff, 0x08, 0x10], -10500, "10\n")?;
    // MAX31850, address pins 3, open thermocouple: TC 100.25 °C + fault, CJ 24.0625 °C + OC
    therm(FIX.max31850, &[0x45, 0x06, 0x11, 0x18, 0xf3, 0xff, 0xff, 0xff], 100250, "12\n")?;
    // DS28EA00 21.5 °C
    therm(FIX.ds28ea00, &[0x58, 0x01, 0x4b, 0x46, 0x7f, 0xff, 0x08, 0x10], 21500, "12\n")?;
    // DS2438: 23.5 °C, VAD 3.12 V, VDD 4.98 V, current register 205 (≈50 mV)
    slave(
        FIX.ds2438,
        &[
            ("temperature", s("6016\n")),
            ("vad", s("312\n")),
            ("vdd", s("498\n")),
            ("page0", vec![0x0f, 0x00, 0x2f, 0xf2, 0x01, 0xcd, 0x00, 0x00]),
            ("page1", vec![0; 8]),
            ("iad", vec![]),
            ("offset", vec![]),
        ],
    )?;
    // DS2408: inputs 0xF5, all latches off, no activity
    slave(
        FIX.ds2408,
        &[
            ("state", vec![0xf5]),
            ("output", vec![0xff]),
            ("activity", vec![0x00]),
            ("cond_search_mask", vec![0]),
            ("cond_search_polarity", vec![0]),
            ("status_control", vec![0x88]),
        ],
    )?;
    // DS2413: PIO-A pin 1 latch 1 (off), PIO-B pin 0 latch 0 (on) → 0b0011 + complement 0xC
    slave(FIX.ds2413, &[("state", vec![0xc3]), ("output", vec![0xff])])?;
    // DS2406: flip-flops A=1 (off), B=0 (on)
    slave(FIX.ds2406, &[("state", vec![0x31]), ("output", vec![0])])?;
    // DS2423 counters on pages 12..15
    let page = |c: u32| format!("{} crc=YES c={c}\n", hex(&[0; 42]));
    slave(FIX.ds2423, &[("w1_slave", s(&(page(1) + &page(2) + &page(12345) + &page(678))))])?;
    let eeprom = |n: usize, tag: &str| -> Vec<u8> { (0..n).map(|i| tag.as_bytes().get(i).copied().unwrap_or(i as u8)).collect() };
    slave(FIX.ds2431, &[("eeprom", eeprom(128, "DS2431 fixture"))])?;
    slave(FIX.ds2433, &[("eeprom", eeprom(512, "DS2433 fixture"))])?;
    slave(FIX.ds28ec20, &[("eeprom", eeprom(2560, "DS28EC20 fixture"))])?;
    slave(FIX.ibutton, &[("rw", vec![])])?;
    // DS1904: control 0x0C (oscillator on), seconds = 2026-10-17 12:00:00 UTC
    let secs = (benchbob_i2c_chips::time::utc_ms(2026, 9, 17, 12, 0, 0) / 1000) as u32;
    let mut rw = vec![0x0c];
    rw.extend(secs.to_le_bytes());
    slave(FIX.ds1904, &[("rw", rw)])?;
    slave(FIX.ds1420, &[])?;
    slave(FIX.ds2450, &[("rw", vec![])])?;
    slave(FIX.unknown, &[("rw", vec![])])?;
    slave(FIX.ghost, &[("rw", vec![])])?;
    Ok(())
}
