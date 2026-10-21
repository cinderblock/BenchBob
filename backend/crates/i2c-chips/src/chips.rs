//! The chip table (ORDER MATTERS, see the crate docs), identification and bus-level hints.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::decode::*;
use crate::dev::Dev;
use crate::io::{I2cError, I2cIo};
use crate::num::{hex_lower, is_bcd, js_num_str, js_round, r2, s8, s16};
use crate::time::iso_seconds;
use crate::types::*;

pub type ProbeFn = fn(&mut Dev, &ProbeCtx) -> R<Option<Probe>>;
pub type ReadFn = fn(&mut Dev, &mut ChipState) -> R<Readout>;
pub type HookFn = fn(&mut Dev, &mut ChipState) -> R<()>;

/// What a probe may look at besides its own address.
#[derive(Debug, Clone, Default)]
pub struct ProbeCtx {
    /// Chips already identified on this bus (ascending address order), by address.
    pub found: BTreeMap<u8, &'static str>,
    /// The user asked for a deep identify: allows pointer probes at ambiguous addresses.
    pub deep: bool,
}

pub struct ChipDef {
    pub id: &'static str,
    pub name: &'static str,
    /// One-line kind, "Temperature / humidity / pressure".
    pub what: &'static str,
    /// Inclusive address ranges.
    pub addrs: &'static [(u8, u8)],
    pub probe: ProbeFn,
    /// Read-only decode (safe to auto-poll).
    pub read: Option<ReadFn>,
    /// False for chips whose read is a one-off (EEPROM, display status).
    pub poll: bool,
    /// Takes a measurement; may write.
    pub measure: Option<ReadFn>,
    /// What Measure writes, shown next to the button.
    pub measure_note: &'static str,
    /// Called when auto-measure is switched on/off (e.g. SCD4x periodic mode).
    pub auto_start: Option<HookFn>,
    pub auto_stop: Option<HookFn>,
    pub opts: &'static [Opt],
}

impl ChipDef {
    pub fn has_addr(&self, a: u8) -> bool {
        self.addrs.iter().any(|(lo, hi)| (*lo..=*hi).contains(&a))
    }
    /// The decoder exists and auto-polling it is allowed.
    pub fn pollable(&self) -> bool {
        self.read.is_some() && self.poll
    }
}

const BASE: ChipDef = ChipDef {
    id: "",
    name: "",
    what: "",
    addrs: &[],
    probe: |_, _| Ok(None),
    read: None,
    poll: true,
    measure: None,
    measure_note: "",
    auto_start: None,
    auto_stop: None,
    opts: &[],
};

fn deg(k: &str, label: &str, v: Option<f64>, d: u8) -> Val {
    Val::unit(k, label, v.map(|v| r2(v, d as i32)), "°C", d)
}

fn rh(v: f64) -> Val {
    Val::unit("rh", "Humidity", r2(v, 1), "%RH", 1)
}

fn hx2(b: u8) -> String {
    format!("0x{b:02x}")
}

fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

/// `try { f() } finally { g() }`: g's error wins, as a throwing `finally` does.
fn finally<T>(r: R<T>, g: Result<(), I2cError>) -> R<T> {
    g?;
    r
}

fn xyz(prefix: &str, label: &str, f: impl Fn(usize) -> f64, d: u8, unit: &'static str) -> Vec<Val> {
    ["x", "y", "z"]
        .iter()
        .enumerate()
        .map(|(i, n)| Val::unit(format!("{prefix}{n}"), format!("{label} {}", n.to_uppercase()), r2(f(i), d as i32), unit, d))
        .collect()
}

// ---------- pure-read probes for command-style chips ----------

const SSD1306: ChipDef = ChipDef {
    id: "ssd1306",
    name: "SSD1306 / SH1106 OLED",
    what: "Monochrome OLED display controller",
    addrs: &[(0x3c, 0x3d)],
    // Pure read only: every write to a display controller is a control byte.
    probe: |d, _| {
        let _ = d.recv();
        Ok(Some(Probe::new(Confidence::Likely, "No ID register; recognised by address (0x3C/0x3D is also a PCF8574A option).")))
    },
    read: Some(|d, _| {
        let Ok(st) = d.recv() else { return Ok(Readout::note("This module doesn't support reads (write-only I²C).")) };
        Ok(Readout::values(vec![
            Val::new("on", "Display", if st & 0x40 != 0 { "off" } else { "on" }),
            Val::new("status", "Status byte", hx2(st)),
        ]))
    }),
    poll: false,
    ..BASE
};

const AHT2X: ChipDef = ChipDef {
    id: "aht2x",
    name: "AHT20",
    what: "Temperature / humidity",
    addrs: &[(0x38, 0x38)],
    // Pure status read: AHT parts answer a bare read with their status byte (bit3 = calibrated).
    probe: |d, _| {
        let Ok(st) = d.recv() else { return Ok(None) };
        if st & 0x68 != 0x08 {
            return Ok(None);
        }
        Ok(Some(Probe::new(Confidence::Likely, format!("Status 0x{} (calibrated, idle). No ID register.", hex_lower(st as u32)))))
    },
    measure_note: "sends 0xAC 0x33 0x00 (trigger measurement)",
    measure: Some(|d, _| {
        let st = d.recv()?;
        if st & 0x08 == 0 {
            d.write(&[0xbe, 0x08, 0x00])?; // initialise / load calibration
            d.sleep(10.0);
        }
        d.write(&[0xac, 0x33, 0x00])?;
        d.sleep(80.0);
        let mut b = d.read(7)?;
        for _ in 0..5 {
            if at(&b, 0) & 0x80 == 0 {
                break;
            }
            d.sleep(20.0);
            b = d.read(7)?;
        }
        if at(&b, 0) & 0x80 != 0 {
            return fail("AHT20 stayed busy");
        }
        if b.len() < 6 {
            return fail("AHT20 short read");
        }
        let (t, h, crc) = aht_decode(&b);
        if crc == Some(false) {
            return fail("AHT20 CRC mismatch");
        }
        Ok(Readout::values(vec![deg("t", "Temperature", Some(t), 2), rh(h)]))
    }),
    ..BASE
};

/// PCF8574/PCF8574A/PCF8575 (quasi-bidirectional, no registers) vs MCP23008/MCP23017 (register files).
const EXPANDER: ChipDef = ChipDef {
    id: "expander",
    name: "GPIO expander",
    what: "8/16-bit I/O expander",
    addrs: &[(0x20, 0x27), (0x38, 0x3f)],
    // A pointer write to a PCF8574 *is* an output write, so classify with pure reads only:
    // a PCF857x returns the port state over and over; an MCP230xx auto-increments through
    // its (varied) registers.
    probe: |d, c| {
        let Ok(b) = d.read(8) else { return Ok(None) };
        if b.is_empty() {
            return Ok(None);
        }
        let hi = d.addr >= 0x38;
        let same = b.iter().all(|x| *x == b[0]);
        let alt = !same && b.iter().enumerate().all(|(i, x)| Some(x) == b.get(i % 2));
        let data = |k| ProbeData { expander: Some(k), ..Default::default() };
        if same {
            if d.addr == 0x39 && c.deep {
                return Ok(None); // let the TSL2561/APDS-9960 pointer probes run
            }
            let (conf, note) = if d.addr == 0x39 {
                (
                    Confidence::Guess,
                    "Could also be a TSL2561 or APDS-9960. Deep identify writes a register pointer to tell them apart (a PCF8574A would see it as an output write).",
                )
            } else {
                (Confidence::Likely, "Reads return the same port byte every time.")
            };
            return Ok(Some(Probe::new(conf, note).variant(if hi { "PCF8574A" } else { "PCF8574" }).data(data(ExpanderKind::Pcf))));
        }
        if alt && !hi {
            return Ok(Some(
                Probe::new(Confidence::Likely, "Reads alternate between two port bytes.")
                    .variant("PCF8575")
                    .data(data(ExpanderKind::Pcf16)),
            ));
        }
        if !hi {
            return Ok(Some(
                Probe::new(Confidence::Likely, "Register file (reads auto-increment).")
                    .variant("MCP23017 / MCP23008")
                    .data(data(ExpanderKind::Mcp)),
            ));
        }
        Ok(None)
    },
    read: Some(|d, s| {
        let kind = s.probe.data.expander;
        if let Some(k @ (ExpanderKind::Pcf | ExpanderKind::Pcf16)) = kind {
            let b = d.read(if k == ExpanderKind::Pcf { 1 } else { 2 })?;
            let mut pins = Vec::new();
            for (p, byte) in b.iter().enumerate() {
                for i in 0..8 {
                    let name = if k == ExpanderKind::Pcf { format!("P{i}") } else { format!("P{p}{i}") };
                    pins.push(Pin { name, level: (byte >> i) & 1, dir: None, pull: None });
                }
            }
            return Ok(Readout {
                values: vec![Val::new("port", "Port", b.iter().map(|x| hx2(*x)).collect::<Vec<_>>().join(" "))],
                pins: Some(pins),
                note: Some("Quasi-bidirectional: a 1 is a weak pull-up (input or output high); a 0 is driven or pulled low.".into()),
                ..Default::default()
            });
        }
        // MCP230xx, BANK=0 map (IOCON.BANK=1 moves the registers; detect it via IOCON).
        let iocon = d.u8(0x0a)?;
        let regs: [(&str, u8, u8, u8); 2] = if iocon & 0x80 != 0 {
            [("A", 0x00, 0x06, 0x09), ("B", 0x10, 0x16, 0x19)]
        } else {
            [("A", 0x00, 0x0c, 0x12), ("B", 0x01, 0x0d, 0x13)]
        };
        let mut pins = Vec::new();
        for (port, dir, pu, gp) in regs {
            let (io, pull, lv) = (d.u8(dir)?, d.u8(pu)?, d.u8(gp)?);
            for i in 0..8 {
                pins.push(Pin {
                    name: format!("GP{port}{i}"),
                    level: (lv >> i) & 1,
                    dir: Some(if (io >> i) & 1 != 0 { "in" } else { "out" }),
                    pull: Some((pull >> i) & 1 != 0),
                });
            }
        }
        Ok(Readout {
            values: vec![Val::new("iocon", "IOCON", hx2(iocon))],
            pins: Some(pins),
            note: Some("MCP23008 has only port A; port B values are meaningless on one.".into()),
            ..Default::default()
        })
    }),
    ..BASE
};

const MCP4725: ChipDef = ChipDef {
    id: "mcp4725",
    name: "MCP4725",
    what: "12-bit DAC",
    addrs: &[(0x60, 0x67)],
    // Any written byte with the top bits 00 is a "fast write" of the DAC output, so this
    // address range only ever gets pure reads unless MCP4725 is ruled out by a NACK.
    probe: |d, _| {
        let n = if d.plain() { 5 } else { 1 };
        let Ok(b) = d.read(n) else { return Ok(None) };
        let ok = b.len() < 5 || (b[2] & 0x0f == 0 && b[3] & 0x90 == 0);
        Ok(Some(if ok {
            Probe::new(Confidence::Likely, "Read-back frame matches the MCP4725 layout.")
        } else {
            Probe::new(Confidence::Guess, "Answers reads but the frame doesn't look like an MCP4725.")
        }))
    },
    read: Some(|d, s| {
        if !d.plain() {
            return Ok(Readout::note("Needs a plain-I²C adapter to read the 5-byte frame."));
        }
        let b = d.read(5)?;
        let dac = (at(&b, 1) as u32) << 4 | (at(&b, 2) >> 4) as u32;
        let ee = ((at(&b, 3) & 0x0f) as u32) << 8 | at(&b, 4) as u32;
        let vdd = s.opt("vdd", 3.3);
        let pd = (at(&b, 0) >> 1) & 3;
        Ok(Readout::values(vec![
            Val::new("code", "DAC code", dac as f64),
            Val::unit("vout", "Output", if pd != 0 { 0.0 } else { r2((dac as f64 / 4096.0) * vdd, 3) }, "V", 3),
            Val::new("pd", "Power-down", ["normal", "1 kΩ", "100 kΩ", "500 kΩ"][pd as usize]),
            Val::new("eeprom", "EEPROM (power-on) code", ee as f64),
            Val::new("busy", "EEPROM write", if at(&b, 0) & 0x80 != 0 { "idle" } else { "busy" }),
        ]))
    }),
    opts: &[Opt { key: "vdd", label: "Vdd", kind: "number", default: 3.3, choices: &[], unit: Some("V") }],
    ..BASE
};

const BH1750: ChipDef = ChipDef {
    id: "bh1750",
    name: "BH1750",
    what: "Ambient light (lux)",
    addrs: &[(0x23, 0x23), (0x5c, 0x5c)],
    // BH1750 has no registers: every written byte is an opcode. A 2-byte pure read is safe.
    probe: |d, _| {
        if !d.plain() {
            return Ok(None);
        }
        Ok(d.read(2).ok().map(|_| Probe::new(Confidence::Likely, "No ID register; recognised by address and a 2-byte read.")))
    },
    measure_note: "sends opcode 0x20 (one-time high-resolution measurement; the chip powers down after)",
    measure: Some(|d, _| {
        d.write(&[0x20])?;
        d.sleep(180.0);
        let b = d.read(2)?;
        let raw = (at(&b, 0) as u32) << 8 | at(&b, 1) as u32;
        Ok(Readout::values(vec![Val::unit("lux", "Illuminance", r2(raw as f64 / 1.2, 1), "lx", 1)]))
    }),
    ..BASE
};

const PCA9685_ALLCALL: ChipDef = ChipDef {
    id: "pca9685-allcall",
    name: "PCA9685 All Call",
    what: "Broadcast address of the PCA9685(s) on this bus",
    addrs: &[(0x70, 0x70)],
    probe: |_, c| {
        Ok(c.found
            .values()
            .any(|v| *v == "pca9685")
            .then(|| Probe::new(Confidence::Likely, "PCA9685 answers 0x70 when MODE1.ALLCALL is set (the power-on default).")))
    },
    ..BASE
};

const TCA9548: ChipDef = ChipDef {
    id: "tca9548",
    name: "TCA9548A",
    what: "8-channel I²C multiplexer",
    addrs: &[(0x70, 0x75)],
    // The mux has a single control register and any written byte selects channels, so only
    // a bare receive-byte. At 0x76/0x77 this runs after the Bosch probe (see order below).
    probe: |d, _| {
        Ok(d.recv()
            .ok()
            .map(|_| Probe::new(Confidence::Guess, "No ID register; recognised by address (HT16K33 LED drivers also live at 0x70–0x77).")))
    },
    read: Some(|d, _| {
        let m = d.recv()?;
        let on: Vec<String> = (0..8).filter(|i| (m >> i) & 1 != 0).map(|i| format!("{i}")).collect();
        Ok(Readout::values(vec![Val::new("mask", "Enabled channels", if on.is_empty() { "none".into() } else { on.join(", ") })]))
    }),
    poll: false,
    ..BASE
};

// ---------- register-based chips with ID registers ----------

const MCP9808: ChipDef = ChipDef {
    id: "mcp9808",
    name: "MCP9808",
    what: "Precision temperature",
    addrs: &[(0x18, 0x1f)],
    probe: |d, _| {
        if d.u16be(0x06).ok() != Some(0x0054) {
            return Ok(None);
        }
        match d.u16be(0x07) {
            Ok(dev) if dev >> 8 == 0x04 => {
                Ok(Some(Probe::new(Confidence::Id, format!("Manufacturer 0x0054, device 0x{}", hex_lower(dev as u32)))))
            }
            _ => Ok(None),
        }
    },
    read: Some(|d, _| {
        let v = d.u16be(0x05)?;
        let mut t = (v & 0x0fff) as f64 / 16.0;
        if v & 0x1000 != 0 {
            t -= 256.0;
        }
        Ok(Readout::values(vec![deg("t", "Temperature", Some(t), 3)]))
    }),
    ..BASE
};

const LIS3DH: ChipDef = ChipDef {
    id: "lis3dh",
    name: "LIS3DH",
    what: "3-axis accelerometer",
    addrs: &[(0x18, 0x19)],
    probe: |d, _| Ok((d.u8(0x0f).ok() == Some(0x33)).then(|| Probe::new(Confidence::Id, "WHO_AM_I 0x33"))),
    ..BASE
};

const DS2482: ChipDef = ChipDef {
    id: "ds2482",
    name: "DS2482",
    what: "I²C → 1-Wire bridge",
    addrs: &[(0x18, 0x1b)],
    // Runs after MCP9808/LIS3DH: those pointer bytes (0x06/0x07/0x0F) aren't DS2482 command
    // codes, so they can't change its state. The bridge itself is only read here.
    probe: |d, _| match d.recv() {
        // DIR/TSB/SBR/1WB clear when idle
        Ok(st) if st & 0xe1 == 0 => Ok(Some(Probe::new(
            Confidence::Likely,
            format!("Status 0x{}. Bind the kernel ds2482 driver from the 1-Wire panel to use it.", hex_lower(st as u32)),
        ))),
        _ => Ok(None),
    },
    read: Some(|d, _| {
        let st = d.recv()?;
        let names = ["1WB busy", "PPD presence", "SD short", "LL line high", "RST reset", "SBR", "TSB", "DIR"];
        let bits: Vec<&str> = names.iter().enumerate().filter(|(i, _)| (st >> i) & 1 != 0).map(|(_, n)| *n).collect();
        Ok(Readout::values(vec![Val::new("status", "Status", format!("{} {}", hx2(st), bits.join(", ")))]))
    }),
    poll: false,
    ..BASE
};

const HMC5883: ChipDef = ChipDef {
    id: "hmc5883",
    name: "HMC5883L",
    what: "3-axis magnetometer",
    addrs: &[(0x1e, 0x1e)],
    probe: |d, _| {
        let b = (|| Ok::<_, I2cError>([d.u8(0x0a)?, d.u8(0x0b)?, d.u8(0x0c)?]))();
        Ok((b.ok() == Some(*b"H43")).then(|| Probe::new(Confidence::Id, "ID registers 'H43'")))
    },
    ..BASE
};

fn adxl_read(d: &mut Dev) -> R<Readout> {
    let fmt = d.u8(0x31)?;
    let b = d.block(0x32, 6)?;
    let scale = if fmt & 0x08 != 0 { 0.0039 } else { (2u32 << (fmt & 3)) as f64 / 512.0 }; // g per LSB
    Ok(Readout::values(xyz("a", "Accel", |i| s16(b[i * 2] as u16 | (b[i * 2 + 1] as u16) << 8) as f64 * scale, 3, "g")))
}

const ADXL345: ChipDef = ChipDef {
    id: "adxl345",
    name: "ADXL345",
    what: "3-axis accelerometer",
    addrs: &[(0x1d, 0x1d), (0x53, 0x53)],
    probe: |d, _| Ok((d.u8(0x00).ok() == Some(0xe5)).then(|| Probe::new(Confidence::Id, "DEVID 0xE5"))),
    read: Some(|d, _| {
        if d.u8(0x2d)? & 0x08 == 0 {
            return Ok(Readout::stale("In standby (POWER_CTL.Measure = 0). Measure wakes it briefly."));
        }
        adxl_read(d)
    }),
    measure_note: "sets POWER_CTL.Measure, reads, restores standby",
    measure: Some(|d, _| {
        let pc = d.u8(0x2d)?;
        if pc & 0x08 == 0 {
            d.wb(0x2d, pc | 0x08)?;
            d.sleep(30.0);
        }
        let r = adxl_read(d);
        let g = if pc & 0x08 == 0 { d.wb(0x2d, pc) } else { Ok(()) };
        finally(r, g)
    }),
    ..BASE
};

fn veml_read(d: &mut Dev, cfg: u16) -> R<Readout> {
    let als = d.u16le(0x04)?;
    let white = d.u16le(0x05)?;
    Ok(Readout::values(vec![
        Val::unit("lux", "Illuminance", r2(veml7700_lux(als, cfg), 2), "lx", 2),
        Val::new("white", "White (raw)", white as f64),
    ]))
}

const VEML7700: ChipDef = ChipDef {
    id: "veml7700",
    name: "VEML7700",
    what: "Ambient light (lux)",
    addrs: &[(0x10, 0x10)],
    probe: |d, _| match d.u16le(0x07) {
        Ok(id) if id & 0xff == 0x81 => Ok(Some(Probe::new(Confidence::Id, format!("ID 0x{}", hex_lower(id as u32))))),
        _ => Ok(None),
    },
    read: Some(|d, _| {
        let cfg = d.u16le(0x00)?;
        if cfg & 1 != 0 {
            return Ok(Readout::stale("Shut down (ALS_SD = 1). Measure powers it up for one integration."));
        }
        veml_read(d, cfg)
    }),
    measure_note: "clears ALS_SD, waits one integration, restores the config",
    measure: Some(|d, _| {
        let cfg = d.u16le(0x00)?;
        if cfg & 1 != 0 {
            d.write_word(0x00, cfg & !1)?;
            d.sleep(5.0 + veml7700_it(cfg) * 1.2); // power-up + one integration with margin
        }
        let r = veml_read(d, cfg & !1);
        let g = if cfg & 1 != 0 { d.write_word(0x00, cfg) } else { Ok(()) };
        finally(r, g)
    }),
    ..BASE
};

const AK8963: ChipDef = ChipDef {
    id: "ak8963",
    name: "AK8963",
    what: "Magnetometer (inside MPU-9250, visible in bypass mode)",
    addrs: &[(0x0c, 0x0d)],
    probe: |d, _| Ok((d.u8(0x00).ok() == Some(0x48)).then(|| Probe::new(Confidence::Id, "WIA 0x48"))),
    ..BASE
};

const HTU21D: ChipDef = ChipDef {
    id: "htu21d",
    name: "HTU21D / Si7021",
    what: "Temperature / humidity",
    addrs: &[(0x40, 0x40)],
    // Runs first at 0x40: HTU21D/Si70xx are command-based (0xFE = soft reset), so the TI ID
    // probe (pointer 0xFE) must not reach them. They NACK pointer 0x00, which every
    // register-based chip at 0x40 (INA2xx, PCA9685) accepts; then "read user register" (0xE7)
    // confirms.
    probe: |d, _| {
        if d.u16be(0x00).is_ok() {
            return Ok(None);
        }
        let Ok(ur) = d.u8(0xe7) else { return Ok(None) };
        let mut variant = "HTU21D / Si7021";
        if d.plain()
            && let Ok(id) = d.write_read(&[0xfc, 0xc9], 6)
        {
            variant = match id.first() {
                Some(0x0d) => "Si7013",
                Some(0x14) => "Si7020",
                Some(0x15) => "Si7021",
                Some(0x32) => "HTU21D",
                _ => variant,
            };
        }
        Ok(Some(Probe::new(Confidence::Likely, format!("User register 0x{}", hex_lower(ur as u32))).variant(variant)))
    },
    measure_note: "sends 0xF3 / 0xF5 (no-hold temperature and humidity measurements)",
    measure: Some(|d, _| {
        let mut get = |cmd: u8, ms: f64| -> R<f64> {
            d.write(&[cmd])?;
            d.sleep(ms);
            let b = d.read(3)?;
            if b.len() < 3 || crc8sens(&b[..2], 0x00) != b[2] {
                return fail("CRC mismatch");
            }
            Ok((((b[0] as u32) << 8 | b[1] as u32) & 0xfffc) as f64)
        };
        let t = -46.85 + (175.72 * get(0xf3, 55.0)?) / 65536.0;
        let h = -6.0 + (125.0 * get(0xf5, 20.0)?) / 65536.0;
        Ok(Readout::values(vec![deg("t", "Temperature", Some(t), 2), rh(h.clamp(0.0, 100.0))]))
    }),
    ..BASE
};

const TI_INA: ChipDef = ChipDef {
    id: "ti",
    name: "TI INA226",
    what: "Current / power monitor",
    addrs: &[(0x40, 0x4f)],
    probe: |d, _| {
        if d.u16be(0xfe).ok() != Some(0x5449) {
            return Ok(None);
        }
        let die = d.u16be(0xff).unwrap_or(0);
        let v: String = match die {
            0x2260 => "INA226".into(),
            0x2270 => "INA260".into(),
            0x3220 => "INA3221".into(),
            0x1050 => "HDC1080".into(),
            _ => format!("TI part (die 0x{})", hex_lower(die as u32)),
        };
        Ok(Some(
            Probe::new(Confidence::Id, format!("Manufacturer 0x5449 (TI), die 0x{}", hex_lower(die as u32)))
                .variant(v)
                .data(ProbeData { die: Some(die), ..Default::default() }),
        ))
    },
    read: Some(ti_read),
    measure_note: "HDC1080 only: writes pointer 0x00 (trigger), reads 4 bytes",
    measure: Some(|d, s| {
        if s.probe.data.die != Some(0x1050) {
            return ti_read(d, s);
        }
        d.write(&[0x00])?;
        d.sleep(20.0);
        let b = d.read(4)?;
        let t = (((at(&b, 0) as u32) << 8 | at(&b, 1) as u32) as f64 / 65536.0) * 165.0 - 40.0;
        let h = (((at(&b, 2) as u32) << 8 | at(&b, 3) as u32) as f64 / 65536.0) * 100.0;
        Ok(Readout::values(vec![deg("t", "Temperature", Some(t), 2), rh(h)]))
    }),
    opts: &[SHUNT],
    ..BASE
};

const SHUNT: Opt = Opt { key: "shunt", label: "Shunt resistor", kind: "number", default: 0.1, choices: &[], unit: Some("Ω") };

fn ti_read(d: &mut Dev, s: &mut ChipState) -> R<Readout> {
    match s.probe.data.die {
        Some(0x2260) => {
            let shunt = d.i16be(0x01)? as f64 * 2.5e-6;
            let bus = d.u16be(0x02)? as f64 * 1.25e-3;
            let i = shunt / s.opt("shunt", 0.1);
            Ok(Readout::values(vec![
                Val::unit("v", "Bus", r2(bus, 3), "V", 3),
                Val::unit("i", "Current", r2(i * 1000.0, 2), "mA", 2),
                Val::unit("p", "Power", r2(bus * i * 1000.0, 1), "mW", 1),
                Val::unit("vs", "Shunt", r2(shunt * 1e3, 4), "mV", 4),
            ]))
        }
        Some(0x2270) => {
            let i = d.i16be(0x01)? as f64 * 1.25e-3;
            let bus = d.u16be(0x02)? as f64 * 1.25e-3;
            let p = d.u16be(0x03)? as f64 * 10e-3;
            Ok(Readout::values(vec![
                Val::unit("v", "Bus", r2(bus, 3), "V", 3),
                Val::unit("i", "Current", r2(i * 1000.0, 1), "mA", 1),
                Val::unit("p", "Power", r2(p * 1000.0, 0), "mW", 0),
            ]))
        }
        Some(0x3220) => {
            let mut vals = Vec::new();
            let rs = s.opt("shunt", 0.1);
            for ch in 0..3u8 {
                let sh = (d.i16be(1 + ch * 2)? >> 3) as f64 * 40e-6;
                let bus = (d.i16be(2 + ch * 2)? >> 3) as f64 * 8e-3;
                vals.push(Val::unit(format!("v{}", ch + 1), format!("CH{} bus", ch + 1), r2(bus, 3), "V", 3));
                vals.push(Val::unit(format!("i{}", ch + 1), format!("CH{} current", ch + 1), r2((sh / rs) * 1000.0, 1), "mA", 1));
            }
            Ok(Readout::values(vals))
        }
        _ => Ok(Readout::note("Identified by ID registers; no decoder for this part yet.")),
    }
}

/// The config word as it goes on the wire for an SMBus word write (MSB first).
fn be_word(v: u16) -> u16 {
    v.swap_bytes()
}

const ADS1X15: ChipDef = ChipDef {
    id: "ads1x15",
    name: "ADS1115",
    what: "4-channel ADC",
    addrs: &[(0x48, 0x4b)],
    probe: |d, _| {
        let (Ok(cfg), Ok(lo), Ok(hi)) = (d.u16be(0x01), d.u16be(0x02), d.u16be(0x03)) else { return Ok(None) };
        if lo == 0x8000 && (hi == 0x7fff || hi == 0x7ff0) {
            return Ok(Some(Probe::new(Confidence::Likely, "Comparator thresholds at power-on defaults.").variant(if hi == 0x7ff0 {
                "ADS1015"
            } else {
                "ADS1115"
            })));
        }
        if cfg & 0x7fff == 0x0583 {
            return Ok(Some(Probe::new(Confidence::Likely, "Config register at its power-on default.").variant("ADS1115")));
        }
        Ok(None)
    },
    read: Some(|d, s| {
        let cfg = d.u16be(0x01)?;
        let v15 = s.probe.variant.as_deref() == Some("ADS1015");
        if cfg & 0x0100 != 0 {
            return Ok(Readout {
                values: vec![Val::new("cfg", "Config", format!("0x{}", hex_lower(cfg as u32)))],
                stale: Some(true),
                note: Some(
                    "Single-shot mode: the conversion register holds the last result only. Measure converts all four inputs.".into(),
                ),
                ..Default::default()
            });
        }
        let raw = d.u16be(0x00)?;
        Ok(Readout::values(vec![
            Val::unit("v", ADS_MUX[((cfg >> 12) & 7) as usize], r2(ads_volts(raw, cfg, v15), 4), "V", 4),
            Val::new("fsr", "Range", format!("±{} V", js_num_str(ADS_FSR[((cfg >> 9) & 7) as usize]))),
        ]))
    }),
    measure_note: "writes the config register for 4 single-shot conversions (AIN0–3 vs GND, ±4.096 V), then restores it",
    measure: Some(|d, s| {
        let orig = d.u16be(0x01)?;
        let v15 = s.probe.variant.as_deref() == Some("ADS1015");
        let run = |d: &mut Dev| -> R<Vec<Val>> {
            let mut vals = Vec::new();
            for ch in 0..4u16 {
                // OS=1, MUX=1xx (single-ended), PGA=001 (±4.096), MODE=1, DR=100, comparator off.
                let cfg = 0x8000 | ((4 + ch) << 12) | (1 << 9) | 0x0100 | (4 << 5) | 0x0003;
                d.write_word(0x01, be_word(cfg))?;
                for _ in 0..20 {
                    d.sleep(3.0);
                    if d.u16be(0x01)? & 0x8000 != 0 {
                        break;
                    }
                }
                vals.push(Val::unit(format!("ain{ch}"), format!("AIN{ch}"), r2(ads_volts(d.u16be(0x00)?, cfg, v15), 4), "V", 4));
            }
            Ok(vals)
        };
        let r = run(d);
        let g = d.write_word(0x01, be_word(orig));
        finally(r, g).map(Readout::values)
    }),
    ..BASE
};

const TMP102: ChipDef = ChipDef {
    id: "tmp102",
    name: "TMP102",
    what: "Temperature",
    addrs: &[(0x48, 0x4b)],
    probe: |d, _| {
        let (Ok(cfg), Ok(lo)) = (d.u16be(0x01), d.u16be(0x02)) else { return Ok(None) };
        Ok((cfg & 0x6000 == 0x6000 && cfg & 0x000f == 0 && lo & 0x0f == 0)
            .then(|| Probe::new(Confidence::Likely, format!("Config 0x{} (R1/R0 read as 1)", hex_lower(cfg as u32)))))
    },
    read: Some(|d, _| {
        let cfg = d.u16be(0x01)?;
        let raw = d.i16be(0x00)?;
        let t = if cfg & 0x0010 != 0 { (raw >> 3) as f64 * 0.0625 } else { (raw >> 4) as f64 * 0.0625 };
        let sd = cfg & 0x0100 != 0;
        Ok(Readout {
            values: vec![deg("t", "Temperature", Some(t), 4)],
            stale: Some(sd),
            note: sd.then(|| "Shutdown mode: value is from the last conversion.".into()),
            ..Default::default()
        })
    }),
    ..BASE
};

const LM75: ChipDef = ChipDef {
    id: "lm75",
    name: "LM75",
    what: "Temperature",
    addrs: &[(0x48, 0x4f)],
    probe: |d, _| {
        let (Ok(cfg), Ok(hyst), Ok(os)) = (d.u8(0x01), d.u16be(0x02), d.u16be(0x03)) else { return Ok(None) };
        if cfg & 0xe0 != 0 || hyst & 0x7f != 0 || os & 0x7f != 0 {
            return Ok(None);
        }
        let def = hyst == 0x4b00 && os == 0x5000;
        Ok(Some(if def {
            Probe::new(Confidence::Likely, "Thresholds at power-on defaults (75/80 °C).")
        } else {
            Probe::new(
                Confidence::Guess,
                format!("Thresholds {}/{} °C", js_num_str(s16(hyst) as f64 / 256.0), js_num_str(s16(os) as f64 / 256.0)),
            )
        }))
    },
    read: Some(|d, _| Ok(Readout::values(vec![deg("t", "Temperature", Some(d.i16be(0x00)? as f64 / 256.0), 3)]))),
    ..BASE
};

const INA219: ChipDef = ChipDef {
    id: "ina219",
    name: "INA219",
    what: "Current / power monitor",
    addrs: &[(0x40, 0x4f)],
    // No ID register. Tried last in this range, after everything that has one.
    probe: |d, _| {
        let (Ok(cfg), Ok(bus)) = (d.u16be(0x00), d.u16be(0x02)) else { return Ok(None) };
        if cfg & 0x8000 != 0 || bus & 0x0004 != 0 {
            return Ok(None);
        }
        Ok(Some(if cfg == 0x399f {
            Probe::new(Confidence::Likely, "Config at power-on default 0x399F.")
        } else {
            Probe::new(Confidence::Guess, format!("Config 0x{} (INA219 has no ID register)", hex_lower(cfg as u32)))
        }))
    },
    read: Some(|d, s| {
        let shunt = d.i16be(0x01)? as f64 * 10e-6;
        let b = d.u16be(0x02)?;
        let bus = (b >> 3) as f64 * 4e-3;
        let i = shunt / s.opt("shunt", 0.1);
        Ok(Readout {
            values: vec![
                Val::unit("v", "Bus", r2(bus, 3), "V", 3),
                Val::unit("i", "Current", r2(i * 1000.0, 2), "mA", 2),
                Val::unit("p", "Power", r2(bus * i * 1000.0, 1), "mW", 1),
                Val::unit("vs", "Shunt", r2(shunt * 1e3, 3), "mV", 3),
            ],
            note: (b & 1 != 0).then(|| "Overflow: the shunt voltage exceeds the configured range.".into()),
            ..Default::default()
        })
    }),
    opts: &[SHUNT],
    ..BASE
};

fn tsl_read(d: &mut Dev) -> R<Readout> {
    let timing = d.u8(0x81)?;
    let ch0 = d.u16le(0xac)?;
    let ch1 = d.u16le(0xae)?;
    let lux = tsl2561_lux(ch0, ch1, timing & 3, timing & 0x10 != 0);
    let l = match lux {
        Some(l) => Val::unit("lux", "Illuminance", r2(l, 1), "lx", 1),
        None => Val { d: Some(1), ..Val::new("lux", "Illuminance", "saturated") },
    };
    Ok(Readout::values(vec![l, Val::new("ch0", "Broadband", ch0 as f64), Val::new("ch1", "IR", ch1 as f64)]))
}

const TSL2561: ChipDef = ChipDef {
    id: "tsl2561",
    name: "TSL2561",
    what: "Ambient light (lux)",
    addrs: &[(0x29, 0x29), (0x39, 0x39), (0x49, 0x49)],
    probe: |d, _| {
        let Ok(id) = d.u8(0x8a) else { return Ok(None) };
        let part = id >> 4;
        if ![0, 1, 4, 5].contains(&part) {
            return Ok(None);
        }
        Ok(Some(Probe::new(Confidence::Id, format!("ID 0x{}", hex_lower(id as u32))).variant(if part & 1 != 0 {
            "TSL2561"
        } else {
            "TSL2560"
        })))
    },
    read: Some(|d, _| {
        if d.u8(0x80)? & 3 != 3 {
            return Ok(Readout::stale("Powered down. Measure powers it up for one integration."));
        }
        tsl_read(d)
    }),
    measure_note: "powers the chip up (CONTROL=0x03), waits one integration, powers it back down",
    measure: Some(|d, _| {
        let was = d.u8(0x80)? & 3;
        if was != 3 {
            d.wb(0x80, 0x03)?;
            let t = d.u8(0x81)? & 3;
            d.sleep([14.0, 101.0, 402.0, 402.0][t as usize] + 20.0);
        }
        let r = tsl_read(d);
        let g = if was != 3 { d.wb(0x80, 0x00) } else { Ok(()) };
        finally(r, g)
    }),
    ..BASE
};

fn tcs_read(d: &mut Dev) -> R<Readout> {
    let b = d.block(0xb4, 8)?;
    let w = |i: usize| b[i] as u32 | (b[i + 1] as u32) << 8;
    let (c, r, g, bl) = (w(0), w(2), w(4), w(6));
    let hexc = if c != 0 {
        let h = |x: u32| format!("{:02x}", (js_round((x as f64 / c as f64) * 255.0) as u32).min(255));
        format!("#{}{}{}", h(r), h(g), h(bl))
    } else {
        "#000000".into()
    };
    Ok(Readout::values(vec![
        Val::new("c", "Clear", c as f64),
        Val::new("r", "Red", r as f64),
        Val::new("g", "Green", g as f64),
        Val::new("b", "Blue", bl as f64),
        Val::new("rgb", "Colour", hexc),
    ]))
}

const TCS34725: ChipDef = ChipDef {
    id: "tcs34725",
    name: "TCS34725",
    what: "RGB colour sensor",
    addrs: &[(0x29, 0x29)],
    probe: |d, _| match d.u8(0x92) {
        Ok(id @ (0x44 | 0x4d)) => Ok(Some(Probe::new(Confidence::Id, format!("ID 0x{}", hex_lower(id as u32))).variant(if id == 0x44 {
            "TCS34725"
        } else {
            "TCS34727"
        }))),
        _ => Ok(None),
    },
    read: Some(|d, _| {
        if d.u8(0x80)? & 3 != 3 {
            return Ok(Readout::stale("RGBC not enabled. Measure enables it for one integration."));
        }
        tcs_read(d)
    }),
    measure_note: "sets ENABLE=PON|AEN, waits one integration, restores ENABLE",
    measure: Some(|d, _| {
        let en = d.u8(0x80)?;
        if en & 3 != 3 {
            d.wb(0x80, en | 0x01)?;
            d.sleep(3.0);
            d.wb(0x80, en | 0x03)?;
            let atime = d.u8(0x81)?;
            d.sleep((256 - atime as u32) as f64 * 2.4 + 10.0);
        }
        let r = tcs_read(d);
        let g = if en & 3 != 3 { d.wb(0x80, en) } else { Ok(()) };
        finally(r, g)
    }),
    ..BASE
};

const TSL2591: ChipDef = ChipDef {
    id: "tsl2591",
    name: "TSL2591",
    what: "Ambient light",
    addrs: &[(0x29, 0x29)],
    probe: |d, _| Ok((d.u8(0xb2).ok() == Some(0x50)).then(|| Probe::new(Confidence::Id, "ID 0x50"))),
    ..BASE
};

const VL53L0X: ChipDef = ChipDef {
    id: "vl53l0x",
    name: "VL53L0X",
    what: "Time-of-flight distance",
    addrs: &[(0x29, 0x29)],
    probe: |d, _| {
        Ok((d.u8(0xc0).ok() == Some(0xee))
            .then(|| Probe::new(Confidence::Id, "Model ID 0xEE. Ranging needs ST's init sequence (not implemented).")))
    },
    ..BASE
};

const APDS9960: ChipDef = ChipDef {
    id: "apds9960",
    name: "APDS-9960",
    what: "Gesture / proximity / colour",
    addrs: &[(0x39, 0x39)],
    probe: |d, _| match d.u8(0x92) {
        Ok(id @ (0xab | 0xa8)) => Ok(Some(Probe::new(Confidence::Id, format!("ID 0x{}", hex_lower(id as u32))))),
        _ => Ok(None),
    },
    ..BASE
};

const SHT4X: ChipDef = ChipDef {
    id: "sht4x",
    name: "SHT4x",
    what: "Temperature / humidity",
    addrs: &[(0x44, 0x46)],
    // Single-byte commands; 0x89 = read serial number (no state change).
    probe: |d, _| {
        if !d.plain() || d.write(&[0x89]).is_err() {
            return Ok(None);
        }
        d.sleep(2.0);
        match d.read(6).ok().and_then(|b| sens_words(&b, 0xff)) {
            Some(w) if w.len() == 2 => Ok(Some(Probe::new(Confidence::Id, format!("Serial {}", (w[0] as u32) << 16 | w[1] as u32)))),
            _ => Ok(None),
        }
    },
    measure_note: "sends 0xFD (high-precision measurement)",
    measure: Some(|d, _| {
        d.write(&[0xfd])?;
        d.sleep(10.0);
        match sens_words(&d.read(6)?, 0xff) {
            Some(w) if w.len() >= 2 => Ok(Readout::values(vec![deg("t", "Temperature", Some(sht3x_t(w[0])), 2), rh(sht4x_h(w[1]))])),
            _ => fail("SHT4x CRC mismatch"),
        }
    }),
    ..BASE
};

const SHT3X: ChipDef = ChipDef {
    id: "sht3x",
    name: "SHT3x",
    what: "Temperature / humidity",
    addrs: &[(0x44, 0x45)],
    // 16-bit commands; 0xF32D = read status register.
    probe: |d, _| {
        if !d.plain() || d.write(&[0xf3, 0x2d]).is_err() {
            return Ok(None);
        }
        d.sleep(1.0);
        match d.read(3).ok().and_then(|b| sens_words(&b, 0xff)) {
            Some(w) => Ok(Some(Probe::new(
                Confidence::Likely,
                format!("Status 0x{} (CRC valid)", w.first().map(|x| hex_lower(*x as u32)).unwrap_or("undefined".into())),
            ))),
            None => Ok(None),
        }
    },
    measure_note: "sends 0x2400 (single-shot, high repeatability)",
    measure: Some(|d, _| {
        d.write(&[0x24, 0x00])?;
        d.sleep(20.0);
        match sens_words(&d.read(6)?, 0xff) {
            Some(w) if w.len() >= 2 => Ok(Readout::values(vec![deg("t", "Temperature", Some(sht3x_t(w[0])), 2), rh(sht3x_h(w[1]))])),
            _ => fail("SHT3x CRC mismatch"),
        }
    }),
    ..BASE
};

const PCA9685: ChipDef = ChipDef {
    id: "pca9685",
    name: "PCA9685",
    what: "16-channel PWM / servo driver",
    addrs: &[(0x40, 0x7f)],
    probe: |d, _| {
        let r = (|| Ok::<_, I2cError>([d.u8(0x00)?, d.u8(0x01)?, d.u8(0x02)?, d.u8(0x03)?, d.u8(0x04)?, d.u8(0x05)?, d.u8(0xfe)?]))();
        let Ok([m1, m2, s1, s2, s3, ac, pre]) = r else { return Ok(None) };
        if m2 & 0xe0 != 0 || [s1, s2, s3, ac].iter().any(|x| x & 1 != 0) || pre < 3 {
            return Ok(None);
        }
        let defaults = [s1 == 0xe2, s2 == 0xe4, s3 == 0xe8, ac == 0xe0].iter().filter(|x| **x).count();
        if defaults < 2 {
            return Ok(None);
        }
        Ok(Some(Probe::new(Confidence::Likely, format!("MODE1 0x{}, sub-addresses at defaults ({defaults}/4)", hex_lower(m1 as u32)))))
    },
    read: Some(|d, _| {
        let m1 = d.u8(0x00)?;
        let pre = d.u8(0xfe)?;
        let regs = if m1 & 0x20 != 0 { d.block(0x06, 64)? } else { (0x06..=0x45).map(|r| d.u8(r)).collect::<Result<Vec<_>, _>>()? };
        let channels = (0..16u8)
            .map(|ch| {
                let b = &regs[ch as usize * 4..ch as usize * 4 + 4];
                let on = b[0] as u16 | ((b[1] & 0x0f) as u16) << 8;
                let off = b[2] as u16 | ((b[3] & 0x0f) as u16) << 8;
                let (full_on, full_off) = (b[1] & 0x10 != 0, b[3] & 0x10 != 0);
                let duty = if full_off {
                    0.0
                } else if full_on {
                    100.0
                } else {
                    r2(((off as i32 - on as i32 + 4096) % 4096) as f64 / 4096.0 * 100.0, 1)
                };
                Channel {
                    ch,
                    duty,
                    on,
                    off,
                    full: if full_off {
                        Some("off")
                    } else if full_on {
                        Some("on")
                    } else {
                        None
                    },
                }
            })
            .collect();
        Ok(Readout {
            values: vec![
                Val::unit("freq", "PWM frequency", r2(25e6 / (4096.0 * (pre as f64 + 1.0)), 1), "Hz", 1),
                Val::new("sleep", "Oscillator", if m1 & 0x10 != 0 { "sleeping" } else { "running" }),
            ],
            channels: Some(channels),
            note: Some("Frequency assumes the internal 25 MHz oscillator.".into()),
            ..Default::default()
        })
    }),
    ..BASE
};

const AT24: ChipDef = ChipDef {
    id: "at24",
    name: "EEPROM (24Cxx)",
    what: "Serial EEPROM",
    addrs: &[(0x50, 0x57)],
    probe: |d, _| Ok(d.u8(0x00).ok().map(|_| Probe::new(Confidence::Likely, "EEPROMs have no ID register; recognised by address."))),
    read: Some(|d, s| {
        let two = s.opt("size", 256.0) > 2048.0;
        let head = if two { if d.plain() { Some(d.write_read(&[0, 0], 64)?) } else { None } } else { Some(d.block(0, 64)?) };
        let Some(head) = head else { return Ok(Readout::note("2-byte-address EEPROMs need a plain-I²C adapter.")) };
        let mut values = Vec::new();
        if let Some(hat) = parse_hat_eeprom(&head) {
            let names: Vec<&str> =
                [hat.vendor.as_deref(), hat.product.as_deref()].into_iter().flatten().filter(|s| !s.is_empty()).collect();
            let v = if names.is_empty() { format!("v{}, {} atoms", hat.version, hat.atoms) } else { names.join(" · ") };
            values.push(Val::new("hat", "Pi HAT EEPROM", v));
        }
        let blank = head.iter().all(|b| *b == 0xff);
        let first =
            if blank { "blank (all 0xFF)".into() } else { head.iter().take(16).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ") };
        values.push(Val::new("first", "First bytes", first));
        Ok(Readout { values, hex: Some(HexDump { start: 0, bytes: head }), ..Default::default() })
    }),
    poll: false,
    opts: &[Opt {
        key: "size",
        label: "Size",
        kind: "select",
        default: 256.0,
        choices: &[
            (128.0, "24C01 (128 B)"),
            (256.0, "24C02 (256 B)"),
            (4096.0, "24C32 (4 KiB, 2-byte address)"),
            (8192.0, "24C64 (8 KiB)"),
            (16384.0, "24C128 (16 KiB)"),
            (32768.0, "24C256 (32 KiB)"),
            (65536.0, "24C512 (64 KiB)"),
        ],
        unit: None,
    }],
    ..BASE
};

const CCS811: ChipDef = ChipDef {
    id: "ccs811",
    name: "CCS811",
    what: "eCO₂ / TVOC gas sensor",
    addrs: &[(0x5a, 0x5b)],
    probe: |d, _| Ok((d.u8(0x20).ok() == Some(0x81)).then(|| Probe::new(Confidence::Id, "HW_ID 0x81"))),
    ..BASE
};

const MLX90614: ChipDef = ChipDef {
    id: "mlx90614",
    name: "MLX90614",
    what: "Infrared thermometer",
    addrs: &[(0x5a, 0x5a)],
    probe: |d, _| {
        let (Ok(ta), Ok(ad)) = (d.u16le(0x06), d.u16le(0x2e)) else { return Ok(None) };
        let t = ta as f64 * 0.02 - 273.15;
        Ok(((ad & 0x7f) as u8 == d.addr && t > -40.0 && t < 125.0)
            .then(|| Probe::new(Confidence::Likely, "SMBus address register matches; ambient in range.")))
    },
    read: Some(|d, _| {
        let obj = d.u16le(0x07)? as f64 * 0.02 - 273.15;
        let amb = d.u16le(0x06)? as f64 * 0.02 - 273.15;
        Ok(Readout::values(vec![deg("tobj", "Object", Some(obj), 2), deg("ta", "Ambient", Some(amb), 2)]))
    }),
    ..BASE
};

const MAX1704X: ChipDef = ChipDef {
    id: "max1704x",
    name: "MAX17048",
    what: "Li-ion fuel gauge",
    addrs: &[(0x36, 0x36)],
    probe: |d, _| {
        let Ok(ver) = d.u16be(0x08) else { return Ok(None) };
        let note = format!("Version 0x{}", hex_lower(ver as u32));
        if ver & 0xfff0 == 0x0010 {
            return Ok(Some(Probe::new(Confidence::Likely, note).variant("MAX17048/49")));
        }
        if ver == 2 || ver == 3 {
            return Ok(Some(Probe::new(Confidence::Likely, note).variant("MAX17043/44")));
        }
        Ok(None)
    },
    read: Some(|d, s| {
        let v = d.u16be(0x02)? as f64 * 78.125e-6;
        let soc = d.u16be(0x04)? as f64 / 256.0;
        let mut vals = vec![Val::unit("v", "Cell", r2(v, 3), "V", 3), Val::unit("soc", "Charge", r2(soc, 1), "%", 1)];
        if s.probe.variant.as_deref().is_some_and(|v| v.starts_with("MAX17048")) {
            vals.push(Val::unit("rate", "Rate", r2(s16(d.u16be(0x16)?) as f64 * 0.208, 1), "%/h", 1));
        }
        Ok(Readout::values(vals))
    }),
    ..BASE
};

fn scd_read(d: &mut Dev, s: &mut ChipState) -> R<Readout> {
    d.write(&[0xe4, 0xb8])?;
    d.sleep(2.0);
    let Some(rdy) = sens_words(&d.read(3)?, 0xff).filter(|w| !w.is_empty()) else { return fail("SCD4x CRC mismatch") };
    if rdy[0] & 0x07ff == 0 {
        let last = s.cache.last.clone().unwrap_or_default();
        return Ok(Readout { stale: Some(true), note: Some("No new sample yet (periodic mode produces one every 5 s).".into()), ..last });
    }
    d.write(&[0xec, 0x05])?;
    d.sleep(2.0);
    let Some(w) = sens_words(&d.read(9)?, 0xff).filter(|w| w.len() >= 3) else { return fail("SCD4x CRC mismatch") };
    let out = Readout::values(vec![
        Val::unit("co2", "CO₂", w[0] as f64, "ppm", 0),
        deg("t", "Temperature", Some(sht3x_t(w[1])), 2),
        rh(sht3x_h(w[2])),
    ]);
    s.cache.last = Some(out.clone());
    Ok(out)
}

fn scd_periodic(s: &ChipState) -> bool {
    s.probe.data.periodic == Some(true) || s.cache.periodic == Some(true)
}

const SCD4X: ChipDef = ChipDef {
    id: "scd4x",
    name: "SCD4x",
    what: "CO₂ / temperature / humidity",
    addrs: &[(0x62, 0x62)],
    // After mcp4725 (pure read) has ruled out a DAC at this address: SCD4x NACKs a bare read.
    probe: |d, _| {
        if !d.plain() {
            return Ok(None);
        }
        if d.write(&[0x36, 0x82]).is_ok() {
            d.sleep(2.0);
            if let Some(w) = d.read(9).ok().and_then(|b| sens_words(&b, 0xff))
                && w.len() == 3
            {
                let serial: String = w.iter().map(|x| format!("{x:04x}")).collect();
                return Ok(Some(
                    Probe::new(Confidence::Id, format!("Serial 0x{serial}"))
                        .data(ProbeData { periodic: Some(false), ..Default::default() }),
                ));
            }
        }
        // In periodic mode only a few commands are accepted; get_data_ready_status is one.
        if d.write(&[0xe4, 0xb8]).is_err() {
            return Ok(None);
        }
        d.sleep(2.0);
        match d.read(3).ok().and_then(|b| sens_words(&b, 0xff)) {
            Some(w) if w.len() == 1 => Ok(Some(
                Probe::new(Confidence::Likely, "Periodic measurement running.")
                    .data(ProbeData { periodic: Some(true), ..Default::default() }),
            )),
            _ => Ok(None),
        }
    },
    read: Some(|d, s| {
        // Reading a result is non-destructive state-wise (it only clears data-ready), so allow it
        // when another agent or a previous auto-measure left periodic mode on.
        if !scd_periodic(s) {
            return Ok(Readout::stale("Idle. Measure takes a single shot (5 s, SCD41 only); auto-measure runs periodic mode."));
        }
        scd_read(d, s)
    }),
    measure_note: "SCD41 single shot (0x219D, ~5 s) when idle; reads the latest sample when periodic mode is on",
    measure: Some(|d, s| {
        if !scd_periodic(s) {
            d.write(&[0x21, 0x9d])?;
            d.sleep(5100.0);
        }
        scd_read(d, s)
    }),
    auto_start: Some(|d, s| {
        d.write(&[0x21, 0xb1])?;
        s.cache.periodic = Some(true);
        Ok(())
    }),
    auto_stop: Some(|d, s| {
        d.write(&[0x3f, 0x86])?;
        d.sleep(500.0);
        s.cache.periodic = Some(false);
        s.probe.data.periodic = Some(false);
        Ok(())
    }),
    ..BASE
};

fn mpu_name(id: u8) -> Option<&'static str> {
    Some(match id {
        0x68 => "MPU-6050",
        0x70 => "MPU-6500",
        0x71 => "MPU-9250",
        0x73 => "MPU-9255",
        0x19 => "MPU-6886",
        0x12 => "ICM-20602",
        0x98 => "ICM-20689",
        0xaf => "ICM-20608",
        _ => return None,
    })
}

fn mpu_read(d: &mut Dev, s: &ChipState) -> R<Readout> {
    let b = d.block(0x3b, 14)?;
    let w = |i: usize| s16((b[i] as u16) << 8 | b[i + 1] as u16) as f64;
    let afs = (d.u8(0x1c)? >> 3) & 3;
    let gfs = (d.u8(0x1b)? >> 3) & 3;
    let acc = (16384u32 >> afs) as f64;
    let gs = 131.0 / (1u32 << gfs) as f64;
    let t = if s.probe.data.id == Some(0x68) { w(6) / 340.0 + 36.53 } else { w(6) / 333.87 + 21.0 };
    let mut values = xyz("a", "Accel", |i| w(i * 2) / acc, 3, "g");
    values.extend(xyz("g", "Gyro", |i| w(8 + i * 2) / gs, 2, "°/s"));
    values.push(deg("t", "Die temperature", Some(t), 1));
    Ok(Readout::values(values))
}

const MPU: ChipDef = ChipDef {
    id: "mpu",
    name: "MPU-6050",
    what: "6-axis IMU",
    addrs: &[(0x68, 0x69)],
    probe: |d, _| {
        let Ok(id) = d.u8(0x75) else { return Ok(None) };
        let Some(name) = mpu_name(id) else { return Ok(None) };
        Ok(Some(
            Probe::new(Confidence::Id, format!("WHO_AM_I 0x{}", hex_lower(id as u32)))
                .variant(name)
                .data(ProbeData { id: Some(id), ..Default::default() }),
        ))
    },
    read: Some(|d, s| {
        if d.u8(0x6b)? & 0x40 != 0 {
            return Ok(Readout::stale("Asleep (PWR_MGMT_1.SLEEP, the power-on state). Measure wakes it briefly."));
        }
        mpu_read(d, s)
    }),
    measure_note: "clears PWR_MGMT_1.SLEEP, reads, restores PWR_MGMT_1",
    measure: Some(|d, s| {
        let pm = d.u8(0x6b)?;
        if pm & 0x40 != 0 {
            d.wb(0x6b, 0x01)?;
            d.sleep(100.0);
        }
        let r = mpu_read(d, s);
        let g = if pm & 0x40 != 0 { d.wb(0x6b, pm) } else { Ok(()) };
        finally(r, g)
    }),
    ..BASE
};

fn icm_read(d: &mut Dev) -> R<Readout> {
    let b = d.block(0x2d, 14)?;
    let w = |i: usize| s16((b[i] as u16) << 8 | b[i + 1] as u16) as f64;
    let mut values = xyz("a", "Accel", |i| w(i * 2) / 16384.0, 3, "g");
    values.extend(xyz("g", "Gyro", |i| w(6 + i * 2) / 131.0, 2, "°/s"));
    values.push(deg("t", "Die temperature", Some((w(12) - 21.0) / 333.87 + 21.0), 1));
    Ok(Readout {
        values,
        note: Some("Scales assume the power-on ranges (±2 g, ±250 °/s); range settings live in bank 2.".into()),
        ..Default::default()
    })
}

const ICM20948: ChipDef = ChipDef {
    id: "icm20948",
    name: "ICM-20948",
    what: "9-axis IMU",
    addrs: &[(0x68, 0x69)],
    probe: |d, _| Ok((d.u8(0x00).ok() == Some(0xea)).then(|| Probe::new(Confidence::Id, "WHO_AM_I 0xEA"))),
    read: Some(|d, _| {
        if (d.u8(0x7f)? >> 4) & 3 != 0 {
            return Ok(Readout::stale("Register bank ≠ 0 (someone else is using it); not switching banks without a Measure."));
        }
        if d.u8(0x06)? & 0x40 != 0 {
            return Ok(Readout::stale("Asleep (power-on state). Measure wakes it briefly."));
        }
        icm_read(d)
    }),
    measure_note: "selects bank 0, clears PWR_MGMT_1.SLEEP, reads, restores both",
    measure: Some(|d, _| {
        let bank = d.u8(0x7f)?;
        if bank & 0x30 != 0 {
            d.wb(0x7f, 0)?;
        }
        let pm = d.u8(0x06)?;
        if pm & 0x40 != 0 {
            d.wb(0x06, 0x01)?;
            d.sleep(50.0);
        }
        let r = icm_read(d);
        let g = (|| {
            if pm & 0x40 != 0 {
                d.wb(0x06, pm)?;
            }
            if bank & 0x30 != 0 {
                d.wb(0x7f, bank)?;
            }
            Ok(())
        })();
        finally(r, g)
    }),
    ..BASE
};

const RTC: ChipDef = ChipDef {
    id: "rtc",
    name: "DS3231",
    what: "Real-time clock",
    addrs: &[(0x68, 0x68)],
    probe: |d, _| {
        let Ok(r) = d.block(0x00, 0x13) else { return Ok(None) };
        if r.len() < 0x13 || !is_bcd(r[0] & 0x7f) || !is_bcd(r[1]) || !is_bcd(r[4]) || !is_bcd(r[5] & 0x1f) || !is_bcd(r[6]) {
            return Ok(None);
        }
        if rtc_decode(&r).0.is_none() {
            return Ok(None);
        }
        let ds3231 = r[0x12] & 0x3f == 0 && r[0x0f] & 0x70 == 0 && s8(r[0x11]) > -40 && s8(r[0x11]) < 85;
        Ok(Some(
            Probe::new(Confidence::Likely, "Time registers hold valid BCD.")
                .variant(if ds3231 { "DS3231" } else { "DS1307 (or compatible)" })
                .data(ProbeData { ds3231, ..Default::default() }),
        ))
    },
    read: Some(|d, s| {
        let ds3231 = s.probe.data.ds3231;
        let r = d.block(0x00, if ds3231 { 0x13 } else { 0x08 })?;
        let (date, halted, _) = rtc_decode(&r);
        let mut vals = vec![Val::new("time", "RTC time (UTC)", date.map(iso_seconds).unwrap_or_else(|| "invalid".into()))];
        if let Some(ms) = date {
            vals.push(Val::unit("drift", "vs system clock", r2((ms - d.io.now_ms()) as f64 / 1000.0, 0), "s", 0));
        }
        let mut note = None;
        if ds3231 {
            vals.push(deg("t", "Temperature", Some(s8(r[0x11]) as f64 + (r[0x12] >> 6) as f64 * 0.25), 2));
            if r[0x0f] & 0x80 != 0 {
                note = Some("Oscillator-stop flag set: the clock lost power at some point; time may be wrong.".into());
            }
        } else if halted {
            note = Some("Clock halted (CH bit set): the oscillator isn't running.".into());
        }
        Ok(Readout { values: vals, note, ..Default::default() })
    }),
    ..BASE
};

fn bme280_read(d: &mut Dev, s: &mut ChipState) -> R<Readout> {
    let hum = s.probe.data.id == Some(0x60);
    if s.cache.cal.is_none() {
        let a = d.block(0x88, 26)?;
        let h = if hum { Some(d.block(0xe1, 7)?) } else { None };
        s.cache.cal = Some(bme280_cal(&a, h.as_deref()));
    }
    let cal = s.cache.cal.unwrap();
    let b = d.block(0xf7, if hum { 8 } else { 6 })?;
    let adc_p = (b[0] as u32) << 12 | (b[1] as u32) << 4 | (b[2] >> 4) as u32;
    let adc_t = (b[3] as u32) << 12 | (b[4] as u32) << 4 | (b[5] >> 4) as u32;
    if adc_t == 0x80000 {
        return Ok(Readout::stale("No conversion since power-on. Measure runs one."));
    }
    let adc_h = hum.then(|| ((b[6] as u32) << 8 | b[7] as u32) as f64);
    let (t, p, h) = bme280_comp(&cal, adc_t as f64, adc_p as f64, adc_h);
    let mut vals = vec![deg("t", "Temperature", Some(t), 2)];
    if let Some(h) = h {
        vals.push(rh(h));
    }
    if let Some(p) = p {
        vals.push(Val::unit("p", "Pressure", r2(p / 100.0, 2), "hPa", 2));
    }
    Ok(Readout::values(vals))
}

fn bme680_read(d: &mut Dev, s: &mut ChipState, measure: bool) -> R<Readout> {
    if s.cache.cal680.is_none() {
        let lo = d.block(0x00, 5)?;
        let a = d.block(0x8a, 23)?;
        let e = d.block(0xe1, 16)?;
        let reg = |r: u8| -> u8 {
            if r < 5 {
                at(&lo, r as usize)
            } else if r >= 0xe1 {
                at(&e, (r - 0xe1) as usize)
            } else {
                at(&a, r.wrapping_sub(0x8a) as usize)
            }
        };
        s.cache.cal680 = Some(bme680_cal(reg));
    }
    let c = s.cache.cal680.unwrap();
    if measure {
        d.wb(0x72, 0x01)?; // osrs_h ×1
        d.wb(0x5a, bme680_heat_res(&c, 320.0, s.cache.last_t.unwrap_or(25.0)))?; // res_heat_0
        d.wb(0x64, 0x65)?; // gas_wait_0 = 150 ms
        d.wb(0x71, 0x10)?; // run_gas, heater profile 0
        d.wb(0x74, (2 << 5) | (1 << 2) | 1)?; // osrs_t ×2, osrs_p ×1, forced
        d.sleep(200.0);
        for _ in 0..20 {
            if d.u8(0x1d)? & 0x80 != 0 {
                break;
            }
            d.sleep(10.0);
        }
    }
    let b = d.block(0x1d, 15)?;
    let adc_p = (b[2] as u32) << 12 | (b[3] as u32) << 4 | (b[4] >> 4) as u32;
    let adc_t = (b[5] as u32) << 12 | (b[6] as u32) << 4 | (b[7] >> 4) as u32;
    let adc_h = (b[8] as u32) << 8 | b[9] as u32;
    if adc_t == 0x80000 {
        return Ok(Readout::stale("No conversion yet. Measure runs one (BME680 has no free-running mode)."));
    }
    let gas_valid = b[14] & 0x20 != 0 && b[14] & 0x10 != 0;
    let gas = gas_valid.then(|| (((b[13] as u32) << 2 | (b[14] >> 6) as u32) as f64, (b[14] & 0x0f) as usize));
    let (t, p, h, g) = bme680_comp(&c, adc_t as f64, adc_p as f64, adc_h as f64, gas);
    s.cache.last_t = Some(t);
    let mut vals = vec![deg("t", "Temperature", Some(t), 2), rh(h), Val::unit("p", "Pressure", r2(p / 100.0, 2), "hPa", 2)];
    if let Some(g) = g {
        vals.push(Val::unit("gas", "Gas resistance", r2(g / 1000.0, 1), "kΩ", 1));
    }
    Ok(Readout { values: vals, stale: Some(!measure && b[0] & 0x80 == 0), ..Default::default() })
}

fn bmp180_measure(d: &mut Dev, s: &mut ChipState) -> R<Readout> {
    if s.cache.cal180.is_none() {
        s.cache.cal180 = Some(bmp180_cal(&d.block(0xaa, 22)?));
    }
    d.wb(0xf4, 0x2e)?;
    d.sleep(5.0);
    let ut = d.u16be(0xf6)? as i64;
    d.wb(0xf4, 0x34)?;
    d.sleep(8.0);
    let b = d.block(0xf6, 3)?;
    let up = ((b[0] as i64) << 16 | (b[1] as i64) << 8 | b[2] as i64) >> 8;
    let (t, p) = bmp180_comp(&s.cache.cal180.unwrap(), ut, up, 0);
    Ok(Readout::values(vec![deg("t", "Temperature", Some(t), 1), Val::unit("p", "Pressure", r2(p as f64 / 100.0, 2), "hPa", 2)]))
}

fn bosch_name(id: u8) -> Option<&'static str> {
    Some(match id {
        0x60 => "BME280",
        0x56..=0x58 => "BMP280",
        0x61 => "BME680",
        0x55 => "BMP180",
        _ => return None,
    })
}

const BOSCH: ChipDef = ChipDef {
    id: "bosch",
    name: "BME280",
    what: "Temperature / humidity / pressure",
    addrs: &[(0x76, 0x77)],
    probe: |d, _| {
        let Ok(id) = d.u8(0xd0) else { return Ok(None) };
        let Some(name) = bosch_name(id) else { return Ok(None) };
        Ok(Some(
            Probe::new(Confidence::Id, format!("Chip ID 0x{}", hex_lower(id as u32)))
                .variant(name)
                .data(ProbeData { id: Some(id), ..Default::default() }),
        ))
    },
    read: Some(|d, s| {
        let id = s.probe.data.id;
        if id == Some(0x55) {
            return Ok(Readout::stale("BMP180 only converts on command. Use Measure."));
        }
        if id == Some(0x61) {
            return bme680_read(d, s, false);
        }
        if d.u8(0xf4)? & 3 != 3 {
            let last = bme280_read(d, s)?;
            return Ok(Readout {
                stale: Some(true),
                note: Some("Sleep/forced mode: values are from the last conversion. Measure runs a forced conversion.".into()),
                ..last
            });
        }
        bme280_read(d, s)
    }),
    measure_note: "forced-mode conversion (writes ctrl_hum/ctrl_meas; BME680 also runs the gas heater at 320 °C for 150 ms)",
    measure: Some(|d, s| {
        let id = s.probe.data.id;
        if id == Some(0x55) {
            return bmp180_measure(d, s);
        }
        if id == Some(0x61) {
            return bme680_read(d, s, true);
        }
        let meas = d.u8(0xf4)?;
        if meas & 3 == 3 {
            return bme280_read(d, s); // already in normal mode
        }
        if id == Some(0x60) {
            d.wb(0xf2, 0x01)?; // osrs_h ×1 (latched by the ctrl_meas write)
        }
        d.wb(0xf4, (1 << 5) | (1 << 2) | 1)?; // osrs_t ×1, osrs_p ×1, forced
        d.sleep(10.0);
        for _ in 0..10 {
            if d.u8(0xf3)? & 0x08 == 0 {
                break;
            }
            d.sleep(5.0);
        }
        bme280_read(d, s)
    }),
    ..BASE
};

const BMP3XX: ChipDef =
    ChipDef {
        id: "bmp3xx",
        name: "BMP388",
        what: "Pressure / temperature",
        addrs: &[(0x76, 0x77)],
        probe: |d, _| match d.u8(0x00) {
            Ok(id @ (0x50 | 0x60)) => Ok(Some(
                Probe::new(Confidence::Id, format!("Chip ID 0x{} (decoder not implemented)", hex_lower(id as u32)))
                    .variant(if id == 0x50 { "BMP388" } else { "BMP390" }),
            )),
            _ => Ok(None),
        },
        ..BASE
    };

const DPS310: ChipDef = ChipDef {
    id: "dps310",
    name: "DPS310",
    what: "Pressure / temperature",
    addrs: &[(0x76, 0x77)],
    probe: |d, _| Ok((d.u8(0x0d).ok() == Some(0x10)).then(|| Probe::new(Confidence::Id, "Product ID 0x10 (decoder not implemented)"))),
    ..BASE
};

/// At 0x76/0x77 a BME/BMP is far likelier than a re-strapped mux.
const TCA9548_HI: ChipDef = ChipDef { addrs: &[(0x76, 0x77)], ..TCA9548 };

/// Probe order. Read the crate docs before changing it.
pub static CHIPS: &[ChipDef] = &[
    // Pure-read probes for command-style chips first.
    SSD1306,
    AHT2X,
    EXPANDER,
    MCP4725,
    BH1750,
    PCA9685_ALLCALL,
    TCA9548,
    // Register-based chips with ID registers.
    MCP9808,
    LIS3DH,
    DS2482,
    HMC5883,
    ADXL345,
    VEML7700,
    AK8963,
    HTU21D, // must precede everything else at 0x40 (see its comment)
    TI_INA,
    ADS1X15,
    TMP102,
    LM75,
    TSL2561, // after LM75/ADS at 0x49 (an LM75 THYST byte can look like a TSL2560 ID)
    TCS34725,
    TSL2591,
    VL53L0X,
    APDS9960,
    SHT4X,
    SHT3X,
    PCA9685,
    INA219, // no ID register: last in 0x40–0x4F
    AT24,
    CCS811,
    MLX90614,
    MAX1704X,
    SCD4X,
    MPU,
    ICM20948,
    RTC,
    BOSCH,
    BMP3XX,
    DPS310,
    TCA9548_HI,
];

/// The first definition with this id.
pub fn chip_by_id(id: &str) -> Option<&'static ChipDef> {
    CHIPS.iter().find(|c| c.id == id)
}

/// Well-known residents of an address, for "unknown device" hints.
pub fn address_hints(addr: u8) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    let mut add = |n: &'static str| {
        if !names.contains(&n) {
            names.push(n);
        }
    };
    for c in CHIPS {
        if c.has_addr(addr) && c.id != "pca9685-allcall" {
            add(c.name);
        }
    }
    let extra: &[&'static str] = match addr {
        0x27 => &["HD44780 LCD backpack (PCF8574)"],
        0x3f => &["HD44780 LCD backpack (PCF8574A)"],
        0x50 => &["Pi HAT ID EEPROM", "DDR SPD"],
        0x51 => &["PCF8563 RTC"],
        0x57 => &["AT24C32 on DS3231 modules", "MAX30102"],
        0x5c => &["AM2320"],
        0x60 => &["Si5351", "MPL3115A2", "ATECC608"],
        0x68 => &["PCF8523 RTC", "BMI160"],
        0x77 => &["MS5611"],
        _ => &[],
    };
    for n in extra {
        add(n);
    }
    names
}

/// What identification found at one address.
pub struct Ident {
    pub chip: &'static ChipDef,
    pub probe: Probe,
}

/// Identify one address. Probes run in [`CHIPS`] order; the first match wins. Bus errors
/// inside a probe mean "not this chip"; anything else is passed up.
pub fn identify(io: &mut dyn I2cIo, addr: u8, ctx: &ProbeCtx) -> R<Option<Ident>> {
    let mut d = Dev::new(io, addr);
    for chip in CHIPS {
        if !chip.has_addr(addr) {
            continue;
        }
        match (chip.probe)(&mut d, ctx) {
            Ok(Some(p)) => return Ok(Some(Ident { chip: chip_by_id(chip.id).unwrap_or(chip), probe: p })),
            Ok(None) | Err(ChipError::Bus(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// Bus-level hints once every address is identified (pairs that explain each other).
pub fn cross_hints(found: &BTreeMap<u8, &'static str>) -> BTreeMap<u8, &'static str> {
    let mut out = BTreeMap::new();
    if found.get(&0x68) == Some(&"rtc") && found.contains_key(&0x57) {
        out.insert(0x57, "Next to a DS3231: this is usually the module's AT24C32 (4 KiB, 2-byte addresses). Set the size accordingly.");
    }
    if found.get(&0x68) == Some(&"mpu") && (found.contains_key(&0x0c) || found.contains_key(&0x0d)) {
        out.insert(0x0c, "AK8963 magnetometer of an MPU-9250 (I²C bypass enabled).");
    }
    out
}

#[cfg(test)]
mod tests;
