//! Flashing (serial.ts): building esptool/avrdude command lines and parsing their output
//! into progress, phase, chip facts and an error.

use std::sync::OnceLock;

use benchbob_core::HttpError;
use regex::Regex;
use serde_json::{Map, Value};

pub struct AvrPreset {
    pub key: &'static str,
    pub label: &'static str,
    pub part: &'static str,
    pub programmer: &'static str,
    pub baud: u32,
}

pub const AVR_PRESETS: &[AvrPreset] = &[
    AvrPreset { key: "uno", label: "Arduino Uno (optiboot, 115200)", part: "atmega328p", programmer: "arduino", baud: 115200 },
    AvrPreset { key: "nano", label: "Arduino Nano (new bootloader, 115200)", part: "atmega328p", programmer: "arduino", baud: 115200 },
    AvrPreset { key: "nano-old", label: "Arduino Nano (old bootloader, 57600)", part: "atmega328p", programmer: "arduino", baud: 57600 },
    AvrPreset { key: "mega", label: "Arduino Mega 2560 (wiring, 115200)", part: "atmega2560", programmer: "wiring", baud: 115200 },
];

pub const ESP_CHIPS: &[&str] = &["auto", "esp32", "esp32s2", "esp32s3", "esp32c2", "esp32c3", "esp32c6", "esp32h2", "esp32p4", "esp8266"];

#[derive(Debug, Clone)]
pub struct FlashFile {
    pub offset: u64,
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct FlashOpts {
    pub tool: String,
    pub action: String,
    pub files: Vec<FlashFile>,
    /// None (or NaN) for the default.
    pub baud: Option<f64>,
    pub chip: Option<String>,
    pub preset: Option<String>,
}

/// `"0x" + n.toString(16).padStart(5, "0")`.
pub fn hex(n: u64) -> String {
    format!("0x{n:05x}")
}

fn baud_str(b: Option<f64>, dflt: u32) -> String {
    match b {
        Some(v) if v != 0.0 && !v.is_nan() => benchbob_core::http::js_string(&benchbob_core::http::num(v)),
        _ => dflt.to_string(),
    }
}

pub fn flash_command(port: &str, o: &FlashOpts) -> Result<Vec<String>, HttpError> {
    let s = |x: &str| x.to_string();
    match o.tool.as_str() {
        "esptool" => {
            let chip = o.chip.as_deref().filter(|c| !c.is_empty()).unwrap_or("auto");
            if !ESP_CHIPS.contains(&chip) {
                return Err(HttpError::bad_request(format!("Unknown chip \"{chip}\" (one of {})", ESP_CHIPS.join(", "))));
            }
            let baud = baud_str(o.baud, if o.action == "write" { 460800 } else { 115200 });
            let mut base = vec![s("/usr/bin/esptool"), s("--chip"), s(chip), s("--port"), s(port), s("--baud"), baud];
            match o.action.as_str() {
                "info" => base.push(s("flash_id")),
                "erase" => base.push(s("erase_flash")),
                _ => {
                    if o.files.is_empty() {
                        return Err(HttpError::bad_request("No firmware files to write"));
                    }
                    base.extend([s("write_flash"), s("-z")]);
                    for f in &o.files {
                        base.extend([hex(f.offset), f.path.clone()]);
                    }
                }
            }
            Ok(base)
        }
        "avrdude" => {
            let key = o.preset.as_deref().filter(|c| !c.is_empty()).unwrap_or("uno");
            let Some(pr) = AVR_PRESETS.iter().find(|p| p.key == key) else {
                return Err(HttpError::bad_request(format!(
                    "Unknown avrdude preset \"{}\" (one of {})",
                    o.preset.as_deref().unwrap_or("undefined"),
                    AVR_PRESETS.iter().map(|p| p.key).collect::<Vec<_>>().join(", ")
                )));
            };
            let mut base = vec![
                s("/usr/bin/avrdude"),
                s("-p"),
                s(pr.part),
                s("-c"),
                s(pr.programmer),
                s("-P"),
                s(port),
                s("-b"),
                baud_str(o.baud, pr.baud),
            ];
            match o.action.as_str() {
                "info" => {} // connects and reads the signature
                "erase" => base.push(s("-e")),
                _ => {
                    let Some(f) = o.files.first() else { return Err(HttpError::bad_request("No firmware file to write")) };
                    if o.files.len() > 1 {
                        return Err(HttpError::bad_request("avrdude takes one file"));
                    }
                    // -D: the bootloader erases page by page; a chip erase isn't possible through it.
                    let fmt = if f.name.to_lowercase().ends_with(".hex") { "i" } else { "r" };
                    base.extend([s("-D"), s("-U"), format!("flash:w:{}:{fmt}", f.path)]);
                }
            }
            Ok(base)
        }
        other => Err(HttpError::bad_request(format!(
            "Unknown flash tool \"{}\" (esptool or avrdude)",
            if other.is_empty() { "undefined" } else { other }
        ))),
    }
}

struct Res {
    connecting: Regex,
    chip: Regex,
    detecting: Regex,
    features: Regex,
    crystal: Regex,
    mac: Regex,
    manufacturer: Regex,
    device: Regex,
    size: Regex,
    erasing: Regex,
    erased: Regex,
    writing_phase: Regex,
    writing_at: Regex,
    wrote: Regex,
    verified: Regex,
    resetting: Regex,
    fatal: Regex,
    exception: Regex,
    bar: Regex,
    signature: Regex,
    avr_verified: Regex,
    avr_error: Regex,
    avr_error2: Regex,
}

fn res() -> &'static Res {
    static R: OnceLock<Res> = OnceLock::new();
    R.get_or_init(|| {
        let r = |s: &str| Regex::new(s).unwrap();
        Res {
            connecting: r("^Connecting"),
            chip: r("^Chip is (.+)"),
            detecting: r(r"^Detecting chip type\.\.\. (.+)"),
            features: r("^Features: (.+)"),
            crystal: r("^Crystal is (.+)"),
            mac: r("^MAC: (.+)"),
            manufacturer: r(r"^Manufacturer: ([A-Za-z0-9_]+)"),
            device: r(r"^Device: ([A-Za-z0-9_]+)"),
            size: r(r"^Detected flash size: (\S+)"),
            erasing: r("^Erasing flash"),
            erased: r("^Chip erase completed"),
            writing_phase: r(r"^(Compressed \d+ bytes|Flash will be erased)"),
            writing_at: r(r"(?i)^Writing at 0x[0-9a-f]+\.\.\. \((\d+) ?%\)"),
            wrote: r(r"^Wrote \d+ bytes"),
            verified: r("^Hash of data verified"),
            resetting: r("^(Hard resetting|Leaving)"),
            fatal: r("A fatal error occurred: (.+)"),
            exception: r("^([A-Za-z0-9_]+(?:Error|Exception)): (.+)"),
            bar: r(r"^(Reading|Writing) \|"),
            signature: r(r"(?i)Device signature = (0x[0-9a-f]+)(?: \(probably ([A-Za-z0-9_]+)\))?"),
            avr_verified: r("bytes of flash verified"),
            avr_error: r("^avrdude(?: OS)? error: (.+)"),
            avr_error2: r("(?i)^avrdude: (.*(?:not responding|can't open|failed|error).*)$"),
        }
    })
}

/// Streaming parser for esptool/avrdude output.
pub struct FlashProgress {
    pub tool: String,
    pub n_files: usize,
    pub action: String,
    pub progress: f64,
    pub phase: String,
    pub info: Map<String, Value>,
    pub error: String,
    buf: String,
    files_done: usize,
    avr_stage: u8, // 1 signature read, 2 write, 3 verify
}

impl FlashProgress {
    pub fn new(tool: &str, n_files: usize, action: &str) -> Self {
        Self {
            tool: tool.into(),
            n_files,
            action: action.into(),
            progress: 0.0,
            phase: "starting".into(),
            info: Map::new(),
            error: String::new(),
            buf: String::new(),
            files_done: 0,
            avr_stage: 0,
        }
    }

    fn set(&mut self, k: &str, v: &str) {
        self.info.insert(k.into(), Value::String(v.into()));
    }

    /// Feed raw output; returns the lines it completed. esptool rewrites progress with \r
    /// when on a tty, so both end a line.
    pub fn feed(&mut self, text: &str) -> Vec<String> {
        self.buf.push_str(text);
        let mut out = Vec::new();
        while let Some(i) = self.buf.find(['\r', '\n']) {
            let line = self.buf[..i].to_string();
            self.buf.drain(..=i);
            if !line.trim().is_empty() {
                self.line(&line);
                out.push(line);
            }
        }
        // avrdude draws its bar a '#' at a time with no line end until it finishes.
        if self.tool == "avrdude" && res().bar.is_match(&self.buf) {
            let b = self.buf.clone();
            self.avr_bar(&b, false);
        }
        out
    }

    pub fn end(&mut self) -> Vec<String> {
        let rest = self.buf.trim().to_string();
        self.buf.clear();
        if rest.is_empty() {
            return vec![];
        }
        self.line(&rest);
        vec![rest]
    }

    fn line(&mut self, l: &str) {
        let r = res();
        let cap = |re: &Regex, i: usize| re.captures(l).and_then(|c| c.get(i).map(|m| m.as_str().to_string()));
        if self.tool == "esptool" {
            if r.connecting.is_match(l) {
                self.phase = "connecting".into();
            } else if let Some(m) = cap(&r.chip, 1) {
                self.set("chip", &m);
                self.phase = "connected".into();
            } else if let Some(m) = cap(&r.detecting, 1) {
                self.set("chipType", &m);
            } else if let Some(m) = cap(&r.features, 1) {
                self.set("features", &m);
            } else if let Some(m) = cap(&r.crystal, 1) {
                self.set("crystal", &m);
            } else if let Some(m) = cap(&r.mac, 1) {
                self.set("mac", &m);
            } else if let Some(m) = cap(&r.manufacturer, 1) {
                self.set("flashManufacturer", &m);
            } else if let Some(m) = cap(&r.device, 1) {
                self.set("flashDevice", &m);
            } else if let Some(m) = cap(&r.size, 1) {
                self.set("flashSize", &m);
            } else if r.erasing.is_match(l) {
                self.phase = "erasing".into();
            } else if r.erased.is_match(l) {
                self.progress = 100.0;
            } else if r.writing_phase.is_match(l) {
                self.phase = "writing".into();
            } else if let Some(m) = cap(&r.writing_at, 1) {
                self.phase = "writing".into();
                let pct: f64 = m.parse().unwrap_or(0.0);
                self.progress = ((self.files_done as f64 + pct / 100.0) / self.n_files as f64 * 100.0).min(100.0);
            } else if r.wrote.is_match(l) {
                self.files_done += 1;
                self.progress = (self.files_done as f64 / self.n_files as f64 * 100.0).min(100.0);
            } else if r.verified.is_match(l) {
                self.set("verified", "yes");
            } else if r.resetting.is_match(l) {
                self.phase = "resetting".into();
            } else if let Some(m) = cap(&r.fatal, 1) {
                self.error = m;
            } else if let Some(c) = r.exception.captures(l)
                && self.error.is_empty()
            {
                self.error = format!("{}: {}", &c[1], &c[2]);
            }
            if self.action != "write" && self.phase == "connected" && self.action == "info" {
                self.progress = self.progress.max(50.0);
            }
        } else if r.bar.is_match(l) {
            self.avr_bar(l, true);
        } else if let Some(c) = r.signature.captures(l) {
            self.set("signature", &c[1]);
            if let Some(p) = c.get(2) {
                self.set("part", p.as_str());
            }
        } else if r.avr_verified.is_match(l) {
            self.progress = 100.0;
            self.set("verified", "yes");
        } else if let Some(m) = cap(&r.avr_error, 1).or_else(|| cap(&r.avr_error2, 1)) {
            self.error = m;
        }
    }

    fn avr_bar(&mut self, l: &str, done: bool) {
        let writing = l.starts_with("Writing");
        if writing {
            self.avr_stage = 2;
        } else if self.avr_stage == 0 || (self.avr_stage == 1 && !done && self.phase != "reading") {
            self.avr_stage = self.avr_stage.max(1);
        }
        if !writing && self.avr_stage >= 2 {
            self.avr_stage = 3;
        }
        let hashes = l.split('|').nth(1).unwrap_or("").matches('#').count();
        let pct = (hashes as f64 * 2.0).min(100.0);
        if self.action != "write" {
            self.phase = "reading".into();
            self.progress = pct;
            return;
        }
        let (lo, hi, phase) = match self.avr_stage {
            1 => (0.0, 5.0, "connecting"),
            2 => (5.0, 75.0, "writing"),
            _ => (75.0, 100.0, "verifying"),
        };
        self.phase = phase.into();
        self.progress = self.progress.max(lo + (hi - lo) * pct / 100.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(f: &[(u64, &str, &str)]) -> Vec<FlashFile> {
        f.iter().map(|(o, p, n)| FlashFile { offset: *o, path: p.to_string(), name: n.to_string() }).collect()
    }

    fn opts(tool: &str, action: &str) -> FlashOpts {
        FlashOpts { tool: tool.into(), action: action.into(), ..Default::default() }
    }

    #[test]
    fn esptool_commands() {
        assert_eq!(
            flash_command("/dev/ttyUSB0", &opts("esptool", "info")).unwrap(),
            ["/usr/bin/esptool", "--chip", "auto", "--port", "/dev/ttyUSB0", "--baud", "115200", "flash_id"]
        );
        let o = FlashOpts {
            chip: Some("esp32".into()),
            files: files(&[(0x1000, "/t/b.bin", "b.bin"), (0x10000, "/t/a.bin", "a.bin")]),
            ..opts("esptool", "write")
        };
        assert_eq!(
            flash_command("/dev/ttyUSB0", &o).unwrap()[1..],
            [
                "--chip",
                "esp32",
                "--port",
                "/dev/ttyUSB0",
                "--baud",
                "460800",
                "write_flash",
                "-z",
                "0x01000",
                "/t/b.bin",
                "0x10000",
                "/t/a.bin"
            ]
        );
        let o = FlashOpts { baud: Some(921600.0), ..opts("esptool", "erase") };
        assert_eq!(flash_command("/dev/ttyACM0", &o).unwrap()[6..], ["921600", "erase_flash"]);
        let o = FlashOpts { chip: Some("z80".into()), ..opts("esptool", "info") };
        assert!(flash_command("/dev/x", &o).unwrap_err().message.contains("Unknown chip"));
        assert!(flash_command("/dev/x", &opts("esptool", "write")).unwrap_err().message.contains("No firmware"));
        assert_eq!(
            flash_command("/dev/x", &opts("stm32flash", "info")).unwrap_err().message,
            "Unknown flash tool \"stm32flash\" (esptool or avrdude)"
        );
    }

    #[test]
    fn avrdude_presets() {
        let f = files(&[(0, "/t/0-blink.hex", "blink.hex")]);
        let o = FlashOpts { preset: Some("uno".into()), files: f.clone(), ..opts("avrdude", "write") };
        assert_eq!(
            flash_command("/dev/ttyUSB0", &o).unwrap(),
            [
                "/usr/bin/avrdude",
                "-p",
                "atmega328p",
                "-c",
                "arduino",
                "-P",
                "/dev/ttyUSB0",
                "-b",
                "115200",
                "-D",
                "-U",
                "flash:w:/t/0-blink.hex:i"
            ]
        );
        let o = FlashOpts { preset: Some("nano-old".into()), files: f.clone(), ..opts("avrdude", "write") };
        assert_eq!(flash_command("/dev/ttyUSB0", &o).unwrap()[7..9], ["-b", "57600"]);
        let o = FlashOpts { preset: Some("mega".into()), files: files(&[(0, "/t/x.bin", "x.bin")]), ..opts("avrdude", "write") };
        let mega = flash_command("/dev/ttyACM0", &o).unwrap();
        assert_eq!(mega[1..5], ["-p", "atmega2560", "-c", "wiring"]);
        assert_eq!(mega.last().unwrap(), "flash:w:/t/x.bin:r");
        let o = FlashOpts { preset: Some("due".into()), files: f, ..opts("avrdude", "write") };
        assert!(flash_command("/dev/x", &o).unwrap_err().message.contains("preset"));
    }

    const ESPTOOL_OK: &str = "esptool.py v4.7.0
Serial port /dev/ttyUSB0
Connecting....
Chip is ESP32-D0WD-V3 (revision v3.1)
Features: WiFi, BT, Dual Core, 240MHz, VRef calibration in efuse, Coding Scheme None
Crystal is 40MHz
MAC: 24:6f:28:aa:bb:cc
Uploading stub...
Running stub...
Stub running...
Changing baud rate to 460800
Changed.
Configuring flash size...
Flash will be erased from 0x00001000 to 0x00007fff...
Flash will be erased from 0x00010000 to 0x000fffff...
Compressed 26640 bytes to 16691...
Writing at 0x00001000... (50 %)\rWriting at 0x000076a1... (100 %)
Wrote 26640 bytes (16691 compressed) at 0x00001000 in 0.8 seconds (effective 263.0 kbit/s)...
Hash of data verified.
Compressed 1000000 bytes to 500000...
Writing at 0x00010000... (3 %)
";

    #[test]
    fn esptool_progress_across_two_files() {
        let mut fp = FlashProgress::new("esptool", 2, "write");
        let lines = fp.feed(ESPTOOL_OK);
        assert!(lines.contains(&"Writing at 0x000076a1... (100 %)".to_string()));
        assert_eq!(fp.info["chip"], "ESP32-D0WD-V3 (revision v3.1)");
        assert_eq!(fp.info["mac"], "24:6f:28:aa:bb:cc");
        assert_eq!(fp.info["crystal"], "40MHz");
        assert_eq!(fp.info["verified"], "yes");
        assert_eq!(fp.phase, "writing");
        assert!((fp.progress - 51.5).abs() < 0.05);
        fp.feed(
            "Writing at 0x000f0000... (100 %)\nWrote 1000000 bytes\nHash of data verified.\n\nLeaving...\nHard resetting via RTS pin...\n",
        );
        assert_eq!(fp.progress, 100.0);
        assert_eq!(fp.phase, "resetting");
        assert_eq!(fp.error, "");
    }

    #[test]
    fn esptool_flash_id_info() {
        let mut fp = FlashProgress::new("esptool", 1, "info");
        fp.feed("Connecting...\nDetecting chip type... ESP32-C3\nChip is ESP32-C3 (QFN32) (revision v0.4)\nMAC: 58:cf:79:00:00:01\nManufacturer: 20\nDevice: 4016\nDetected flash size: 4MB\n");
        assert_eq!(fp.info["chipType"], "ESP32-C3");
        assert_eq!(fp.info["flashManufacturer"], "20");
        assert_eq!(fp.info["flashDevice"], "4016");
        assert_eq!(fp.info["flashSize"], "4MB");
        assert_eq!(fp.progress, 50.0);
        // Insertion order, as the TypeScript object has it.
        assert_eq!(fp.info.keys().collect::<Vec<_>>(), ["chipType", "chip", "mac", "flashManufacturer", "flashDevice", "flashSize"]);
    }

    #[test]
    fn esptool_errors_fixtures_captured_on_desky() {
        let mut a = FlashProgress::new("esptool", 1, "write");
        a.feed("esptool.py v4.7.0\nSerial port /dev/ttyNOPE\n\nA fatal error occurred: Could not open /dev/ttyNOPE, the port is busy or doesn't exist.\n([Errno 2] could not open port /dev/ttyNOPE)\n");
        assert_eq!(a.error, "Could not open /dev/ttyNOPE, the port is busy or doesn't exist.");
        let mut b = FlashProgress::new("esptool", 1, "write");
        b.feed("Traceback (most recent call last):\n  File \"/usr/bin/esptool\", line 37, in <module>\nOSError: [Errno 25] Inappropriate ioctl for device\n");
        assert_eq!(b.error, "OSError: [Errno 25] Inappropriate ioctl for device");
    }

    #[test]
    fn avrdude_bar_parsing_partial_and_complete() {
        let mut fp = FlashProgress::new("avrdude", 1, "write");
        fp.feed("avrdude: AVR device initialized and ready to accept instructions\n\nReading | ################################################## | 100% 0.00 s\n\navrdude: Device signature = 0x1e950f (probably m328p)\n");
        assert_eq!(fp.info["signature"], "0x1e950f");
        assert_eq!(fp.info["part"], "m328p");
        assert_eq!(fp.progress, 5.0);
        fp.feed("avrdude: writing 924 bytes flash ...\n\nWriting | #########################");
        assert_eq!(fp.phase, "writing");
        assert!((fp.progress - 40.0).abs() < 0.5);
        fp.feed("######################### | 100% 0.15 s\n\navrdude: 924 bytes of flash written\n\nReading | ##########");
        assert_eq!(fp.phase, "verifying");
        assert!((fp.progress - 80.0).abs() < 0.5);
        fp.feed(
            "######################################## | 100% 0.12 s\n\navrdude: 924 bytes of flash verified\n\navrdude done.  Thank you.\n",
        );
        assert_eq!(fp.progress, 100.0);
        assert_eq!(fp.error, "");
    }

    #[test]
    fn avrdude_errors_fixture_captured_on_desky() {
        let mut fp = FlashProgress::new("avrdude", 1, "write");
        fp.feed("avrdude OS error: ioctl(\"TIOCMGET\"): Inappropriate ioctl for device\navrdude error: programmer is not responding\navrdude warning: attempt 1 of 10: not in sync: resp=0x00\navrdude error: unable to open programmer arduino on port /dev/pts/1\n\navrdude done.  Thank you.\n");
        assert_eq!(fp.error, "unable to open programmer arduino on port /dev/pts/1");
    }
}
