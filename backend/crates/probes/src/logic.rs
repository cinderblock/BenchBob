//! Pure logic (`peripherals/lib/probes.ts`): the device table, OpenOCD command lines, log
//! parsing, TCL wrapping, memory, firmware kinds, UF2, dfu-util progress and the client
//! snippets, all unit-tested with the TypeScript's fixtures.

use std::sync::OnceLock;

use benchbob_core::http::num;
use benchbob_core::udev::Props;
use benchbob_core::util::{js_round, prop};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

// ---------- device table ----------

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeType {
    /// "stlink", "jlink", "cmsis-dap", "ftdi", "esp-usb-jtag", "bmp", "wch-link", "atmel"
    pub kind: &'static str,
    pub name: &'static str,
    /// OpenOCD interface script, relative to the scripts dir; None when OpenOCD can't drive it.
    pub iface: Option<&'static str>,
    pub transports: Vec<&'static str>,
    /// Shown on the card: caveats, what the device also is.
    pub note: Option<&'static str>,
    /// Black Magic Probe: the GDB server runs on the probe itself.
    pub gdb_built_in: bool,
    /// An ESP32 with the built-in USB-JTAG/serial: its ROM bootloader is serial (esptool).
    pub esp_serial: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BootType {
    /// "rp2-bootsel" | "stm32-dfu"
    pub kind: &'static str,
    pub name: &'static str,
    pub chip: &'static str,
    /// The CLI that talks to it natively.
    pub tool: &'static str,
    /// Debian package providing it.
    pub pkg: &'static str,
}

fn stlink_name(pid: &str) -> Option<&'static str> {
    Some(match pid {
        "3744" => "ST-LINK/V1",
        "3748" => "ST-LINK/V2",
        "374b" | "3752" => "ST-LINK/V2-1",
        "374d" => "STLINK-V3MINIE",
        "374e" | "374f" | "3753" | "3754" => "STLINK-V3",
        "3757" => "STLINK-V3PWR",
        _ => return None,
    })
}

const ARM: [&str; 2] = ["swd", "jtag"];

fn pt(kind: &'static str, name: &'static str, iface: Option<&'static str>, transports: &[&'static str]) -> ProbeType {
    ProbeType { kind, name, iface, transports: transports.to_vec(), note: None, gdb_built_in: false, esp_serial: false }
}

/// Look a USB device up by VID:PID (lower-case hex) and its strings.
pub fn probe_type(vid: &str, pid: &str, strings: &[String]) -> Option<ProbeType> {
    let vid = vid.to_lowercase();
    let pid = pid.to_lowercase();
    let note = |mut t: ProbeType, n: &'static str| {
        t.note = Some(n);
        t
    };
    if vid == "0483"
        && let Some(n) = stlink_name(&pid)
    {
        return Some(pt("stlink", n, Some("interface/stlink.cfg"), &ARM));
    }
    // SEGGER's VID is only used for J-Links (PIDs vary by model and USB mode).
    if vid == "1366" {
        return Some(pt("jlink", "SEGGER J-Link", Some("interface/jlink.cfg"), &ARM));
    }
    Some(match (vid.as_str(), pid.as_str()) {
        ("2e8a", "000c") => pt("cmsis-dap", "Raspberry Pi Debug Probe", Some("interface/cmsis-dap.cfg"), &["swd"]),
        ("0d28", "0204") => pt("cmsis-dap", "DAPLink", Some("interface/cmsis-dap.cfg"), &ARM),
        ("1d50", "6018") => {
            let mut t =
                note(pt("bmp", "Black Magic Probe", None, &ARM), "Runs its own GDB server on its first serial port; OpenOCD isn't needed.");
            t.gdb_built_in = true;
            t
        }
        ("1a86", "8010") => note(
            pt("wch-link", "WCH-Link (RISC-V mode)", None, &[]),
            "RISC-V mode needs WCH's OpenOCD fork, which isn't installed. Switch the probe to DAP mode (ARM targets) to use it here.",
        ),
        ("1a86", "8012") => pt("wch-link", "WCH-LinkE (DAP mode)", Some("interface/cmsis-dap.cfg"), &ARM),
        ("03eb", "2141") => note(
            pt("atmel", "Atmel-ICE", Some("interface/cmsis-dap.cfg"), &ARM),
            "SAM (ARM) targets only; AVR debugWIRE/UPDI isn't OpenOCD's.",
        ),
        ("03eb", "2111") => note(pt("atmel", "Atmel EDBG", Some("interface/cmsis-dap.cfg"), &ARM), "SAM (ARM) targets only."),
        ("03eb", "2145") => note(pt("atmel", "Atmel mEDBG", Some("interface/cmsis-dap.cfg"), &ARM), "SAM (ARM) targets only."),
        ("0403", "6010") => note(
            pt("ftdi", "FTDI FT2232H", Some("interface/ftdi/esp32_devkitj_v1.cfg"), &["jtag"]),
            "Assumed to be an ESP-Prog (channel A = JTAG, B = UART). Other FT2232H boards need their own interface/ftdi/*.cfg layout.",
        ),
        ("303a", "1001") => {
            let mut t = note(
                pt("esp-usb-jtag", "ESP32 built-in USB-JTAG", Some("interface/esp_usb_jtag.cfg"), &["jtag"]),
                "An ESP32-S3/C3/C6 with its own USB. JTAG debugging is here; serial flashing (esptool, ROM bootloader) is in the Serial panel.",
            );
            t.esp_serial = true;
            t
        }
        _ => {
            // Any other CMSIS-DAP: the spec requires "CMSIS-DAP" in the product or interface string.
            static DAP: OnceLock<Regex> = OnceLock::new();
            if strings.iter().any(|s| re(&DAP, r"(?i)CMSIS[-_ ]?DAP").is_match(s)) {
                return Some(note(pt("cmsis-dap", "CMSIS-DAP probe", Some("interface/cmsis-dap.cfg"), &ARM), "CMSIS-DAP"));
            }
            return None;
        }
    })
}

pub fn boot_type(vid: &str, pid: &str) -> Option<BootType> {
    let b = |kind, name, chip, tool, pkg| BootType { kind, name, chip, tool, pkg };
    match (vid.to_lowercase().as_str(), pid.to_lowercase().as_str()) {
        ("2e8a", "0003") => Some(b("rp2-bootsel", "RP2040 BOOTSEL", "rp2040", "picotool", "picotool")),
        ("2e8a", "000f") => Some(b("rp2-bootsel", "RP2350 BOOTSEL", "rp2350", "picotool", "picotool")),
        ("0483", "df11") => Some(b("stm32-dfu", "STM32 DFU bootloader", "stm32", "dfu-util", "dfu-util")),
        _ => None,
    }
}

/// `\xNN` escapes as `String.fromCharCode` decodes them (no trim, unlike udevDecode).
fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' && b[i + 2].is_ascii_hexdigit() && b[i + 3].is_ascii_hexdigit() {
            out.push(u8::from_str_radix(&s[i + 2..i + 4], 16).unwrap_or(0) as char);
            i += 4;
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Strings udev knows for a usb_device: the product string in its raw and encoded forms.
pub fn usb_strings(p: &Props) -> Vec<String> {
    [prop(p, "ID_MODEL").to_string(), dec(prop(p, "ID_MODEL_ENC")), prop(p, "PRODUCT_NAME").to_string()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect()
}

/// Settings key and URL id: stable across replugs (serial), else the physical port.
pub fn stable_id(p: &Props) -> String {
    let raw = [prop(p, "ID_SERIAL"), prop(p, "ID_PATH"), prop(p, "DEVPATH")].into_iter().find(|s| !s.is_empty()).unwrap_or("unknown");
    static BAD: OnceLock<Regex> = OnceLock::new();
    let s = re(&BAD, r"[^A-Za-z0-9_.-]+").replace_all(raw, "_");
    s.chars().take(80).collect()
}

/// The USB port path OpenOCD's `adapter usb location` wants: DEVPATH's last part, "1-1.3".
pub fn usb_location(devpath: &str) -> Option<String> {
    static LOC: OnceLock<Regex> = OnceLock::new();
    let last = devpath.rsplit('/').next().unwrap_or("");
    re(&LOC, r"^\d+-[\d.]+$").is_match(last).then(|| last.to_string())
}

/// How to make OpenOCD open *this* probe when several of a kind are plugged in: the serial
/// (survives moving ports), except ST-Link/V2's binary one, which udev mangles: its port.
pub fn pin_command(kind: &str, pid: &str, serial: &str, devpath: &str) -> Option<String> {
    let clean = (4..=64).contains(&serial.len())
        && serial.bytes().all(|c| (0x21..=0x7e).contains(&c))
        && !serial.bytes().any(|c| b"{}[]$\"\\;".contains(&c));
    let binary_serial = kind == "stlink" && (pid == "3748" || pid == "3744");
    if clean && !binary_serial {
        return Some(format!("adapter serial {serial}"));
    }
    usb_location(devpath).map(|l| format!("adapter usb location {l}"))
}

// ---------- targets ----------

/// Target presets offered first; only those whose cfg exists are shown.
pub const TARGET_PRESETS: [(&str, &str); 16] = [
    ("target/stm32f0x.cfg", "STM32F0"),
    ("target/stm32f1x.cfg", "STM32F1"),
    ("target/stm32f4x.cfg", "STM32F4"),
    ("target/stm32g0x.cfg", "STM32G0"),
    ("target/stm32g4x.cfg", "STM32G4"),
    ("target/stm32l4x.cfg", "STM32L4"),
    ("target/stm32h7x.cfg", "STM32H7"),
    ("target/rp2040.cfg", "RP2040"),
    ("target/rp2350.cfg", "RP2350"),
    ("target/nordic/nrf52.cfg", "nRF52"),
    ("target/at91samdXX.cfg", "SAMD21"),
    ("target/atsame5x.cfg", "SAMD51/E5x"),
    ("target/esp32.cfg", "ESP32"),
    ("target/esp32s3.cfg", "ESP32-S3"),
    ("target/esp32c3.cfg", "ESP32-C3"),
    ("target/esp32c6.cfg", "ESP32-C6"),
];

/// Built-in pseudo-target: any Cortex-M over SWD, for "what's on the other end?".
pub const GENERIC_TARGET: &str = "generic-cortex-m";
/// No target script: just the adapter, for configuring targets by hand over telnet.
pub const NO_TARGET: &str = "none";

/// Xtensa/RISC-V ESP targets are JTAG-only; everything else here defaults to SWD.
pub fn default_transport(t: &ProbeType, target: &str) -> &'static str {
    if target.contains("esp32") || !t.transports.contains(&"swd") { "jtag" } else { "swd" }
}

/// Sensible adapter speed (kHz) for a probe/target pair.
pub fn default_speed(t: &ProbeType, target: &str) -> u32 {
    if t.kind == "esp-usb-jtag" {
        40000
    } else if target.contains("esp32") {
        5000
    } else {
        4000
    }
}

/// The GDB binary a client would use for a target.
pub fn gdb_for(target: &str) -> String {
    static ESP: OnceLock<Regex> = OnceLock::new();
    if let Some(m) = re(&ESP, r"esp32(s2|s3|c\d+|h\d+)?").captures(target) {
        let v = m.get(1).map(|g| g.as_str()).unwrap_or("");
        return if v.starts_with('c') || v.starts_with('h') { "riscv32-esp-elf-gdb".into() } else { format!("xtensa-esp32{v}-elf-gdb") };
    }
    if target.contains("riscv") {
        return "riscv64-unknown-elf-gdb".into();
    }
    "arm-none-eabi-gdb".into()
}

// ---------- OpenOCD command line ----------

pub const READY_MARK: &str = "BENCHBOB-READY";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ports {
    pub gdb: u32,
    pub telnet: u32,
    pub tcl: u32,
}

impl Ports {
    pub fn to_json(&self) -> Value {
        json!({ "gdb": self.gdb, "telnet": self.telnet, "tcl": self.tcl })
    }
}

/// None = disabled (one-shot sessions only open TCL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPorts {
    pub gdb: Option<u32>,
    pub telnet: Option<u32>,
    pub tcl: u32,
}

impl SessionPorts {
    pub fn to_json(&self) -> Value {
        json!({ "gdb": self.gdb, "telnet": self.telnet, "tcl": self.tcl })
    }
}

#[derive(Debug, Clone)]
pub struct OcdOptions {
    pub iface: String,
    /// "target/x.cfg", GENERIC_TARGET or NO_TARGET
    pub target: String,
    pub transport: String,
    /// kHz
    pub speed: f64,
    /// "adapter serial …" / "adapter usb location …"
    pub pin: Option<String>,
    /// "0.0.0.0" for a LAN-visible server, "127.0.0.1" for one-shot work.
    pub bindto: String,
    pub ports: SessionPorts,
}

/// Arguments for `openocd`. OpenOCD runs -f/-c strictly in sequence, so the transport is
/// chosen after the adapter driver exists but before the target script (which builds its DAP
/// for whatever transport is active), and the speed comes after the target script because
/// most targets set their own.
pub fn openocd_args(o: &OcdOptions) -> Vec<String> {
    let mut a: Vec<String> = vec!["-f".into(), o.iface.clone(), "-c".into(), format!("transport select {}", o.transport)];
    let c = |a: &mut Vec<String>, s: String| {
        a.push("-c".into());
        a.push(s);
    };
    if o.target == GENERIC_TARGET {
        // ARM's JTAG-DP has a 4-bit IR; SWD has no IR at all.
        c(&mut a, if o.transport == "swd" { "swd newdap chip cpu -enable".into() } else { "jtag newtap chip cpu -irlen 4 -enable".into() });
        c(&mut a, "dap create chip.dap -chain-position chip.cpu".into());
        c(&mut a, "target create chip.cpu cortex_m -dap chip.dap".into());
    } else if o.target != NO_TARGET {
        a.push("-f".into());
        a.push(o.target.clone());
    }
    c(&mut a, format!("adapter speed {}", js_round(o.speed)));
    if let Some(p) = &o.pin {
        c(&mut a, p.clone());
    }
    c(&mut a, format!("bindto {}", o.bindto));
    // This build (0.12+dev) deprecates gdb_port/telnet_port/tcl_port for the spaced forms.
    let port = |p: Option<u32>| p.map(|x| x.to_string()).unwrap_or_else(|| "disabled".into());
    c(&mut a, format!("gdb port {}", port(o.ports.gdb)));
    c(&mut a, format!("telnet port {}", port(o.ports.telnet)));
    c(&mut a, format!("tcl port {}", o.ports.tcl));
    // Explicit init so a dead probe fails here (exit 1, before any port opens); the marker
    // then tells the driver init succeeded. The TCL listener opens right after it.
    c(&mut a, "init".into());
    c(&mut a, format!("echo {READY_MARK}"));
    a
}

/// Quote one argument for display in a copy-pasteable shell command.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"_./:=@+-".contains(&c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

// ---------- OpenOCD log parsing ----------

#[derive(Debug, Clone, Default)]
pub struct OcdState {
    /// "STLINK V2J37S7 (API v2)", "CMSIS-DAPv2 FW 2.0.0"
    pub adapter: String,
    pub voltage: Option<f64>,
    pub dpidr: String,
    /// "stm32f4x.cpu: Cortex-M4 r0p1"
    pub cpus: Vec<String>,
    pub examined: bool,
    pub flash: String,
    pub listening: IndexMap<String, u32>,
    pub gdb_clients: u32,
    /// Newest last, a handful.
    pub errors: Vec<String>,
    pub hint: String,
}

const HINTS: [(&str, &str); 11] = [
    (
        r"LIBUSB_ERROR_ACCESS|ermission denied",
        "No permission to open the probe's USB device: the openocd package's udev rules give it to group plugdev; replug the probe after installing them.",
    ),
    (r"(?i)LIBUSB_ERROR_BUSY|resource busy", "The probe is busy: another OpenOCD, GDB server or IDE on BenchBob has it open."),
    (
        r"Invalid serial number",
        "OpenOCD rejected the serial number used to pick this probe (J-Link serials must be numeric). Unplug other probes of this kind, or report it.",
    ),
    (
        r"unable to find a matching CMSIS-DAP device|^Error: open failed|No J-Link device found|could not find or open device|unable to open ftdi device|LIBUSB_ERROR_NO_DEVICE|No device selected",
        "OpenOCD couldn't open the probe: it was unplugged, something else holds it, or it re-enumerated with a different serial.",
    ),
    (r"missing IR length", "The target script needs SWD but JTAG was selected (or the other way round). Change the transport."),
    (
        r"all ones|all zeroes|IR capture error|scan chain interrogation failed|Unexpected idcode",
        "The JTAG chain doesn't answer: check TCK/TMS/TDI/TDO and GND wiring, that the target is powered, and try a lower adapter speed.",
    ),
    (
        r"target voltage may be too low",
        "The probe sees little or no voltage on VTref: power the target and connect its supply to the probe's VTref/VCC pin.",
    ),
    (
        r"[Ll]ocked|read.?out protection|RDP|flash is protected|protected sector",
        "The chip's flash is locked/read-protected. A mass erase (or the vendor's unlock command) clears it, wiping the firmware.",
    ),
    (r"lockup|double fault|HardFault", "The firmware crashed into a fault/lockup. Reset-and-halt, then reflash."),
    (
        r"(?i)Address already in use|Couldn't bind|couldn't bind",
        "One of the ports is already taken (another OpenOCD?). Pick different ports in the settings.",
    ),
    (
        r"init mode failed|unable to connect to the target|Error connecting DP|cannot read IDR|SWD ack not OK|DAP init failed|[Ee]xamination failed|Target not examined|Failed to read memory|timed out while waiting for target|DP initialisation failed",
        "The probe works but the target doesn't answer. Check SWDIO/SWCLK and GND wiring, that the target is powered, try a lower adapter speed, or wire NRST and connect under reset if the firmware sleeps or reuses the SWD pins.",
    ),
];

pub fn hint_for(line: &str) -> &'static str {
    static RES: OnceLock<Vec<Regex>> = OnceLock::new();
    let res = RES.get_or_init(|| HINTS.iter().map(|(r, _)| Regex::new(r).unwrap()).collect());
    res.iter().zip(HINTS.iter()).find(|(r, _)| r.is_match(line)).map(|(_, (_, h))| *h).unwrap_or("")
}

/// Fold one OpenOCD output line into the running state (0.12's "[target]" prefix and the
/// older "target:" one). True when something user-visible changed.
pub fn parse_ocd_line(st: &mut OcdState, line: &str) -> bool {
    static CPU: OnceLock<Regex> = OnceLock::new();
    static EXAM: OnceLock<Regex> = OnceLock::new();
    static VOLT: OnceLock<Regex> = OnceLock::new();
    static VTARGET: OnceLock<Regex> = OnceLock::new();
    static STLINK: OnceLock<Regex> = OnceLock::new();
    static DAPV: OnceLock<Regex> = OnceLock::new();
    static DAPFW: OnceLock<Regex> = OnceLock::new();
    static JLINK: OnceLock<Regex> = OnceLock::new();
    static ESP: OnceLock<Regex> = OnceLock::new();
    static DPIDR: OnceLock<Regex> = OnceLock::new();
    static TAP: OnceLock<Regex> = OnceLock::new();
    static FLASH: OnceLock<Regex> = OnceLock::new();
    static FPROBE: OnceLock<Regex> = OnceLock::new();
    static LISTEN: OnceLock<Regex> = OnceLock::new();
    static GDBSRV: OnceLock<Regex> = OnceLock::new();
    static WARN: OnceLock<Regex> = OnceLock::new();
    let line = line.trim_end();
    if let Some(m) = re(&CPU, r"^Info : (?:\[([^\]]+)\]\s*|(\S+): )(.+?) processor detected").captures(line) {
        let s = format!("{}: {}", m.get(1).or(m.get(2)).map(|g| g.as_str()).unwrap_or(""), &m[3]);
        if !st.cpus.contains(&s) {
            st.cpus.push(s);
        }
        st.examined = true;
        return true;
    }
    if let Some(m) = re(&EXAM, r"^Info : \[([^\]]+)\] Examination succeed").captures(line) {
        let pre = format!("{}:", &m[1]);
        if !st.cpus.iter().any(|c| c.starts_with(&pre)) {
            st.cpus.push(format!("{}: examined", &m[1]));
        }
        st.examined = true;
        return true;
    }
    if let Some(m) = re(&VOLT, r"Target voltage: ([\d.]+)").captures(line).or_else(|| re(&VTARGET, r"VTarget = ([\d.]+) ?V").captures(line))
    {
        let v = js_round(benchbob_core::http::parse_float(&m[1]).unwrap_or(f64::NAN) * 100.0) / 100.0;
        st.voltage = Some(v);
        if v < 1.0 {
            st.hint = hint_for("target voltage may be too low").into();
        }
        return true;
    }
    if let Some(m) = re(&STLINK, r"^Info : (STLINK \S+ \(API v\d\))").captures(line) {
        st.adapter = m[1].into();
        return true;
    }
    if let Some(m) = re(&DAPV, r"Using CMSIS-DAPv(\d)").captures(line) {
        st.adapter = format!("CMSIS-DAPv{}", &m[1]);
        return true;
    }
    if let Some(m) = re(&DAPFW, r"CMSIS-DAP: FW Version = (\S+)").captures(line) {
        st.adapter = format!("{} FW {}", if st.adapter.is_empty() { "CMSIS-DAP" } else { &st.adapter }, &m[1]);
        return true;
    }
    if let Some(m) = re(&JLINK, r"^Info : (J-Link .+?) compiled").captures(line) {
        st.adapter = m[1].into();
        return true;
    }
    if re(&ESP, r"^Info : esp_usb_jtag: Device found\. Base speed (\d+)KHz").is_match(line) {
        st.adapter = "ESP USB-JTAG".into();
        return true;
    }
    if let Some(m) = re(&DPIDR, r"(?i)(?:SWD|JTAG) DPIDR (0x[0-9a-f]+)").captures(line) {
        st.dpidr = m[1].into();
        return true;
    }
    if let Some(m) = re(&TAP, r"^Info : JTAG tap: (\S+) tap/device found: (0x[0-9a-f]+)").captures(line) {
        if st.dpidr.is_empty() {
            st.dpidr = format!("{} {}", &m[1], &m[2]);
        }
        return true;
    }
    if let Some(m) = re(&FLASH, r"[Ff]lash size = (.+)$").captures(line).or_else(|| re(&FPROBE, r"Flash Probe: (\d+ bytes)").captures(line))
    {
        st.flash = m[1].trim().into();
        return true;
    }
    if let Some(m) = re(&LISTEN, r"Listening on port (\d+) for (gdb|telnet|tcl) connections").captures(line) {
        st.listening.insert(m[2].into(), m[1].parse().unwrap_or(0));
        return true;
    }
    if let Some(m) = re(&GDBSRV, r"starting gdb server for \S+ on (\d+)").captures(line) {
        st.listening.entry("gdb".into()).or_insert(m[1].parse().unwrap_or(0));
        return true;
    }
    if line.contains("accepting 'gdb' connection") {
        st.gdb_clients += 1;
        return true;
    }
    if line.contains("dropped 'gdb' connection") {
        st.gdb_clients = st.gdb_clients.saturating_sub(1);
        return true;
    }
    if line.starts_with("Error: ") || re(&WARN, r"(?i)^Warn : .*examination failed").is_match(line) {
        st.errors.push(line.into());
        if st.errors.len() > 6 {
            st.errors.remove(0);
        }
        let h = hint_for(line);
        if !h.is_empty() {
            st.hint = h.into();
        }
        return true;
    }
    false
}

// ---------- TCL RPC ----------
//
// OpenOCD's TCL server takes a command terminated by 0x1a and answers with the Tcl result
// terminated by 0x1a. An error comes back as bare message text, indistinguishable from a
// result, so every command is wrapped in `catch` and tagged. `capture` collects what the
// command prints (most OpenOCD commands print rather than return).

pub const TCL_EOM: char = '\x1a';
const OK: &str = "@@OK@@";
const ERR: &str = "@@ERR@@";

pub fn tcl_wrap(cmd: &str, capture: bool) -> String {
    let body = if capture { format!("capture {{{cmd}}}") } else { cmd.to_string() };
    format!("if {{[catch {{{body}}} __r]}} {{set __r \"{ERR}$__r\"}} else {{set __r \"{OK}$__r\"}}")
}

/// The command's output, or the Tcl error message.
pub fn tcl_unwrap(reply: &str) -> Result<String, String> {
    if let Some(r) = reply.strip_prefix(OK) {
        return Ok(r.into());
    }
    let fail = |s: &str| if s.trim().is_empty() { "OpenOCD command failed".to_string() } else { s.trim().to_string() };
    if let Some(r) = reply.strip_prefix(ERR) {
        return Err(fail(r));
    }
    // Unwrapped (unbalanced braces in the command make Tcl reject the wrapper itself).
    Err(fail(reply))
}

/// Braces must balance for the command to sit inside the catch wrapper.
pub fn tcl_balanced(cmd: &str) -> bool {
    let b = cmd.as_bytes();
    let mut d = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'{' => d += 1,
            b'}' => {
                d -= 1;
                if d < 0 {
                    return false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    d == 0 && !cmd.contains(TCL_EOM)
}

// ---------- memory ----------

/// A number from JSON or text: decimal or 0x hex, `_` separators allowed; None otherwise.
pub fn parse_num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::String(s) => parse_num_str(s),
        _ => None,
    }
}

pub fn parse_num_str(s: &str) -> Option<f64> {
    if s.is_empty() {
        return None;
    }
    let t: String = s.trim().replace('_', "");
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        if !h.is_empty() && h.bytes().all(|c| c.is_ascii_hexdigit()) {
            return u128::from_str_radix(h, 16).ok().map(|n| n as f64);
        }
        return None;
    }
    if !t.is_empty() && t.bytes().all(|c| c.is_ascii_digit()) {
        return t.parse::<f64>().ok();
    }
    None
}

/// `read_memory` returns a Tcl list of hex/decimal words.
pub fn parse_read_memory(out: &str) -> Vec<u64> {
    out.split_whitespace().map(|w| parse_num_str(w).unwrap_or(0.0) as u64).collect()
}

pub fn hexdump(bytes: &[u64], base: u64) -> String {
    bytes
        .chunks(16)
        .enumerate()
        .map(|(i, row)| {
            let hex = row.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
            let asc: String = row.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as u8 as char } else { '.' }).collect();
            format!("{:08x}  {hex:<47}  {asc}", base + i as u64 * 16)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------- firmware files ----------

/// What OpenOCD will see: it sniffs ELF by magic and ihex/srec by first character.
pub fn firmware_kind(buf: &[u8], name: &str) -> &'static str {
    let lname = name.to_lowercase();
    if buf.starts_with(b"\x7fELF") {
        return "elf";
    }
    if buf.len() >= 8 && u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) == UF2_MAGIC0 {
        return "uf2";
    }
    if buf.first() == Some(&b':') || lname.ends_with(".hex") {
        return "hex";
    }
    if (buf.first() == Some(&b'S') && buf.get(1).is_some_and(|c| c.is_ascii_digit()))
        || lname.ends_with(".srec")
        || lname.ends_with(".s19")
        || lname.ends_with(".mot")
    {
        return "srec";
    }
    "bin"
}

// ---------- UF2 ----------

pub const UF2_MAGIC0: u32 = 0x0a32_4655;
const UF2_MAGIC1: u32 = 0x9e5d_5157;
const UF2_MAGIC_END: u32 = 0x0ab1_6f30;
const UF2_FLAG_FAMILY: u32 = 0x2000;

pub fn uf2_family(id: u32) -> Option<&'static str> {
    Some(match id {
        0xe48bff56 => "RP2040",
        0xe48bff57 => "RP2XXX absolute",
        0xe48bff58 => "RP2XXX data",
        0xe48bff59 => "RP2350 ARM-S",
        0xe48bff5a => "RP2350 RISC-V",
        0xe48bff5b => "RP2350 ARM-NS",
        0x68ed2b88 => "SAMD21",
        0x55114460 => "SAMD51",
        0xada52840 => "nRF52840",
        0x1c5f21b0 => "ESP32",
        0xc47e5767 => "ESP32-S3",
        0xd42ba06c => "ESP32-C3",
        0x57755a57 => "STM32F4",
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Uf2Info {
    pub blocks: usize,
    pub family_ids: Vec<u32>,
    pub families: Vec<String>,
    pub start: u64,
    pub end: u64,
    pub payload: u64,
}

impl Uf2Info {
    /// The fields in parseUf2's order, for `{ ok, ...info, … }`.
    pub fn fields(&self) -> Map<String, Value> {
        let v = json!({
            "blocks": self.blocks, "familyIds": self.family_ids, "families": self.families,
            "start": self.start, "end": self.end, "payload": self.payload,
        });
        v.as_object().cloned().unwrap_or_default()
    }
}

/// Validate a UF2 file: every 512-byte block carries both start magics and the end magic.
pub fn parse_uf2(buf: &[u8]) -> Result<Uf2Info, String> {
    if buf.is_empty() || !buf.len().is_multiple_of(512) {
        return Err(format!("Not a UF2 file: {} bytes isn't a whole number of 512-byte blocks", buf.len()));
    }
    let u = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    let mut fams: Vec<u32> = Vec::new();
    let (mut start, mut end, mut payload) = (u64::MAX, 0u64, 0u64);
    for o in (0..buf.len()).step_by(512) {
        if u(o) != UF2_MAGIC0 || u(o + 4) != UF2_MAGIC1 || u(o + 508) != UF2_MAGIC_END {
            return Err(format!("Not a UF2 file: bad magic in block {}", o / 512));
        }
        let flags = u(o + 8);
        let (addr, size) = (u(o + 12) as u64, u(o + 16) as u64);
        // "Not main flash" blocks are comments/extra data.
        if flags & 1 != 0 {
            continue;
        }
        if flags & UF2_FLAG_FAMILY != 0 && !fams.contains(&u(o + 28)) {
            fams.push(u(o + 28));
        }
        start = start.min(addr);
        end = end.max(addr + size);
        payload += size;
    }
    Ok(Uf2Info {
        blocks: buf.len() / 512,
        families: fams.iter().map(|f| uf2_family(*f).map(String::from).unwrap_or_else(|| format!("0x{f:x}"))).collect(),
        family_ids: fams,
        start: if start == u64::MAX { 0 } else { start },
        end,
        payload,
    })
}

/// Families a BOOTSEL device will accept (others are silently ignored by the bootrom).
pub fn uf2_accepts(chip: &str, family_ids: &[u32]) -> bool {
    if family_ids.is_empty() {
        return true;
    }
    let ok: &[u32] = if chip == "rp2040" { &[0xe48bff56] } else { &[0xe48bff57, 0xe48bff58, 0xe48bff59, 0xe48bff5a, 0xe48bff5b] };
    family_ids.iter().any(|f| ok.contains(f))
}

/// INFO_UF2.TXT: a first line naming the bootloader, then "Key: value" lines.
pub fn parse_info_uf2(text: &str) -> Value {
    let mut fields = Map::new();
    let mut bootloader = String::new();
    for l in text.split('\n').map(|l| l.trim_end_matches('\r').trim()).filter(|l| !l.is_empty()) {
        match l.split_once(':') {
            Some((k, v)) if !k.is_empty() => {
                fields.insert(k.trim().into(), v.trim().into());
            }
            _ => {
                if bootloader.is_empty() {
                    bootloader = l.into();
                }
            }
        }
    }
    json!({ "bootloader": bootloader, "fields": fields })
}

// ---------- dfu-util ----------

/// dfu-util redraws a bar with \r: "Download\t[=====    ]  36%    45056 bytes".
pub fn parse_dfu_progress(chunk: &str) -> Option<(String, u32)> {
    static BAR: OnceLock<Regex> = OnceLock::new();
    re(&BAR, r"(Erase|Download|Upload)\s*\[[= ]*\]\s*(\d+)%")
        .captures_iter(chunk)
        .last()
        .map(|m| (m[1].to_string(), m[2].parse().unwrap_or(0)))
}

// ---------- client snippets ----------

pub fn client_snippets(host: &str, ports: &Ports, target: &str, id: &str, web: &str) -> Value {
    let gdb = gdb_for(target);
    let ep = format!("{host}:{}", ports.gdb);
    let cortex = json!({ "name": "BenchBob probe", "type": "cortex-debug", "request": "launch", "servertype": "external", "gdbTarget": ep, "executable": "${workspaceFolder}/build/app.elf", "runToEntryPoint": "main" });
    let platformio = [
        "debug_tool = custom".to_string(),
        format!("debug_port = {ep}"),
        "debug_server =".to_string(),
        "upload_protocol = custom".to_string(),
        format!("upload_command = curl -fsS --data-binary @$SOURCE \"{web}/api/probes/{id}/flash?name=$SOURCE\""),
    ]
    .join(
        "
",
    );
    json!({
        "gdb": format!("{gdb} -ex \"target extended-remote {ep}\" build/app.elf"),
        "telnet": format!("telnet {host} {}", ports.telnet),
        "cortexDebug": serde_json::to_string_pretty(&cortex).unwrap_or_default(),
        "platformio": platformio,
    })
}

/// A JS number as JSON (for voltages and such).
pub fn jnum(x: Option<f64>) -> Value {
    x.map(num).unwrap_or(Value::Null)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // REAL = captured from openocd 0.12.0+dev-snapshot (2026-02-16) on desky with no probe
    // attached. SYNTHETIC = written from OpenOCD's source/typical sessions.
    const REAL_CMSIS_NO_ADAPTER: &str = "Open On-Chip Debugger 0.12.0+dev-snapshot (2026-02-16-16:07)
Licensed under GNU GPL v2
Warn : Transport \"swd\" was already selected
Info : [rp2040.core0] Hardware thread awareness created
Info : [rp2040.core1] Hardware thread awareness created
adapter speed: 4000 kHz
Error: unable to find a matching CMSIS-DAP device";
    const REAL_STLINK_NO_ADAPTER: &str = "adapter speed: 4000 kHz
Error: open failed";
    const REAL_JLINK_BAD_SERIAL: &str = "adapter speed: 4000 kHz
Error: Invalid serial number: ABC123";
    const REAL_JLINK_WRONG_TRANSPORT: &str =
        "Warn : DEPRECATED: auto-selecting transport \"jtag\". Use 'transport select jtag' to suppress this message.
newtap: nrf52.cpu missing IR length";
    const REAL_DUMMY_JTAG: &str = "Info : TAP chip.cpu does not have valid IDCODE (idcode=0x0)
Warn : Unexpected idcode after end of chain: 21 0xfffff800
Error: double-check your JTAG setup (interface, speed, ...)
Error: Trying to use configured scan chain anyway...
Error: chip.cpu: IR capture error; saw 0x0f not 0x01
Warn : Bypassing JTAG setup events due to errors
Error: Invalid ACK (7) in DAP response
Error: JTAG-DP STICKY ERROR";
    pub(crate) const REAL_DUMMY_READY: &str = "Warn : gdb services need one or more targets defined
BENCHBOB-READY
Info : Listening on port 16667 for tcl connections
Info : telnet server disabled";
    const SYN_STLINK_OK: &str = "Info : Listening on port 6666 for tcl connections
Info : Listening on port 4444 for telnet connections
Info : clock speed 2000 kHz
Info : STLINK V2J37S7 (API v2) VID:PID 0483:3748
Info : Target voltage: 3.238278
Info : [stm32f4x.cpu] Cortex-M4 r0p1 processor detected
Info : [stm32f4x.cpu] target has 6 breakpoints, 4 watchpoints
Info : starting gdb server for stm32f4x.cpu on 3333
Info : Listening on port 3333 for gdb connections
Info : accepting 'gdb' connection on tcp/3333
Info : device id = 0x10076413
Info : flash size = 1024 KiB";
    const SYN_STLINK_NO_TARGET: &str = "Info : STLINK V2J37S7 (API v2) VID:PID 0483:3748
Info : Target voltage: 0.003182
Error: target voltage may be too low for reliable debugging
Error: init mode failed (unable to connect to the target)";
    const SYN_CMSIS_OK: &str = "Info : Using CMSIS-DAPv2 interface with VID:PID=0x2e8a:0x000c, serial=E6614C311B597D32
Info : CMSIS-DAP: SWD supported
Info : CMSIS-DAP: FW Version = 2.0.0
Info : CMSIS-DAP: Interface Initialised (SWD)
Info : SWD DPIDR 0x0bc12477, DLPIDR 0x00000001
Info : rp2040.core0: Cortex-M0+ r0p1 processor detected
Info : [rp2040.core1] Cortex-M0+ r0p1 processor detected
Info : RP2040 Flash Probe: 2097152 bytes @0x10000000, in 32 sectors";
    const SYN_CMSIS_NO_TARGET: &str = "Info : Using CMSIS-DAPv2 interface with VID:PID=0x2e8a:0x000c, serial=E6614C311B597D32
Info : CMSIS-DAP: FW Version = 2.0.0
Error: Error connecting DP: cannot read IDR";

    fn feed(log: &str) -> OcdState {
        let mut st = OcdState::default();
        for l in log.split('\n') {
            parse_ocd_line(&mut st, l);
        }
        st
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn probe_type_st_link_variants() {
        assert_eq!(probe_type("0483", "3748", &[]).unwrap().name, "ST-LINK/V2");
        assert_eq!(probe_type("0483", "374B", &[]).unwrap().name, "ST-LINK/V2-1");
        assert_eq!(probe_type("0483", "374e", &[]).unwrap().kind, "stlink");
        // DFU is a bootloader, not a probe.
        assert!(probe_type("0483", "df11", &[]).is_none());
    }

    #[test]
    fn probe_type_jlink_by_vendor() {
        for pid in ["0101", "0105", "1015", "1020", "1050"] {
            assert_eq!(probe_type("1366", pid, &[]).unwrap().iface, Some("interface/jlink.cfg"));
        }
    }

    #[test]
    fn probe_type_cmsis_dap_by_id_and_by_string() {
        assert_eq!(probe_type("2e8a", "000c", &[]).unwrap().name, "Raspberry Pi Debug Probe");
        assert_eq!(probe_type("0d28", "0204", &[]).unwrap().kind, "cmsis-dap");
        assert_eq!(probe_type("c251", "f001", &s(&["LPC-Link CMSIS-DAP"])).unwrap().kind, "cmsis-dap");
        assert!(probe_type("c251", "f001", &s(&["Keyboard"])).is_none());
    }

    #[test]
    fn probe_type_others() {
        assert_eq!(probe_type("0403", "6010", &[]).unwrap().iface, Some("interface/ftdi/esp32_devkitj_v1.cfg"));
        assert_eq!(probe_type("303a", "1001", &[]).unwrap().transports, ["jtag"]);
        assert!(probe_type("1d50", "6018", &[]).unwrap().gdb_built_in);
        assert_eq!(probe_type("1a86", "8010", &[]).unwrap().iface, None);
        assert_eq!(probe_type("1a86", "8012", &[]).unwrap().iface, Some("interface/cmsis-dap.cfg"));
        for pid in ["2141", "2111", "2145"] {
            assert_eq!(probe_type("03eb", pid, &[]).unwrap().kind, "atmel");
        }
    }

    #[test]
    fn bootloaders() {
        assert_eq!(boot_type("2e8a", "0003").unwrap().chip, "rp2040");
        assert_eq!(boot_type("2e8a", "000f").unwrap().chip, "rp2350");
        assert_eq!(boot_type("0483", "df11").unwrap().tool, "dfu-util");
        assert!(boot_type("2e8a", "000c").is_none());
    }

    #[test]
    fn udev_strings_from_fixture_props() {
        let p: Props =
            [("ID_MODEL", "Debugprobe_on_Pico__CMSIS-DAP_"), ("ID_MODEL_ENC", "Debugprobe\\x20on\\x20Pico\\x20\\x28CMSIS-DAP\\x29")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        assert!(usb_strings(&p).contains(&"Debugprobe on Pico (CMSIS-DAP)".to_string()));
        assert_eq!(probe_type("1234", "5678", &usb_strings(&p)).unwrap().kind, "cmsis-dap");
    }

    #[test]
    fn ids_and_pinning() {
        let p = |kv: &[(&str, &str)]| -> Props { kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
        assert_eq!(
            stable_id(&p(&[("ID_SERIAL", "STMicroelectronics_STM32_STLink_066DFF38"), ("ID_PATH", "x")])),
            "STMicroelectronics_STM32_STLink_066DFF38"
        );
        assert_eq!(
            stable_id(&p(&[("ID_PATH", "platform-fd500000.pcie-pci-0000:01:00.0-usb-0:1.3")])),
            "platform-fd500000.pcie-pci-0000_01_00.0-usb-0_1.3"
        );
        assert_eq!(
            usb_location("/devices/platform/scb/fd500000.pcie/pci0000:00/0000:00:00.0/0000:01:00.0/usb1/1-1/1-1.3").as_deref(),
            Some("1-1.3")
        );
        assert_eq!(usb_location("/devices/virtual/foo"), None);
        let dp = "/devices/x/usb1/1-1/1-1.2";
        assert_eq!(pin_command("cmsis-dap", "000c", "E6614C311B597D32", dp).as_deref(), Some("adapter serial E6614C311B597D32"));
        // ST-Link/V2's serial is binary; udev's version is useless to OpenOCD.
        assert_eq!(pin_command("stlink", "3748", "U_U_U_", dp).as_deref(), Some("adapter usb location 1-1.2"));
        assert_eq!(pin_command("jlink", "0101", "", dp).as_deref(), Some("adapter usb location 1-1.2"));
        assert_eq!(pin_command("jlink", "0101", "evil}{;", dp).as_deref(), Some("adapter usb location 1-1.2"));
    }

    fn opts(iface: &str, target: &str, transport: &str, speed: f64, pin: Option<&str>, bindto: &str, ports: SessionPorts) -> OcdOptions {
        OcdOptions {
            iface: iface.into(),
            target: target.into(),
            transport: transport.into(),
            speed,
            pin: pin.map(String::from),
            bindto: bindto.into(),
            ports,
        }
    }

    #[test]
    fn openocd_args_server() {
        let a = openocd_args(&opts(
            "interface/stlink.cfg",
            "target/stm32f4x.cfg",
            "swd",
            4000.0,
            Some("adapter serial 066D"),
            "0.0.0.0",
            SessionPorts { gdb: Some(3333), telnet: Some(4444), tcl: 6666 },
        ));
        assert_eq!(
            a,
            [
                "-f",
                "interface/stlink.cfg",
                "-c",
                "transport select swd",
                "-f",
                "target/stm32f4x.cfg",
                "-c",
                "adapter speed 4000",
                "-c",
                "adapter serial 066D",
                "-c",
                "bindto 0.0.0.0",
                "-c",
                "gdb port 3333",
                "-c",
                "telnet port 4444",
                "-c",
                "tcl port 6666",
                "-c",
                "init",
                "-c",
                "echo BENCHBOB-READY"
            ]
        );
    }

    #[test]
    fn openocd_args_one_shot_generic_and_no_target() {
        let a = openocd_args(&opts(
            "interface/cmsis-dap.cfg",
            GENERIC_TARGET,
            "swd",
            1000.0,
            None,
            "127.0.0.1",
            SessionPorts { gdb: None, telnet: None, tcl: 40000 },
        ));
        for x in
            ["swd newdap chip cpu -enable", "target create chip.cpu cortex_m -dap chip.dap", "gdb port disabled", "telnet port disabled"]
        {
            assert!(a.iter().any(|y| y == x), "{x}");
        }
        assert!(!a.iter().any(|x| x.starts_with("target/")));
        let a = openocd_args(&opts(
            "interface/dummy.cfg",
            NO_TARGET,
            "jtag",
            1000.0,
            None,
            "127.0.0.1",
            SessionPorts { gdb: None, telnet: None, tcl: 1 },
        ));
        assert!(!a.iter().any(|x| x.starts_with("target")));
    }

    #[test]
    fn defaults_and_quoting() {
        let st = probe_type("0483", "3748", &[]).unwrap();
        let esp = probe_type("303a", "1001", &[]).unwrap();
        assert_eq!(default_transport(&st, "target/stm32f1x.cfg"), "swd");
        assert_eq!(default_transport(&st, "target/esp32.cfg"), "jtag");
        assert_eq!(default_transport(&esp, "target/esp32s3.cfg"), "jtag");
        assert_eq!(default_speed(&esp, "target/esp32s3.cfg"), 40000);
        assert_eq!(gdb_for("target/esp32s3.cfg"), "xtensa-esp32s3-elf-gdb");
        assert_eq!(gdb_for("target/esp32.cfg"), "xtensa-esp32-elf-gdb");
        assert_eq!(gdb_for("target/esp32c3.cfg"), "riscv32-esp-elf-gdb");
        assert_eq!(gdb_for("target/rp2040.cfg"), "arm-none-eabi-gdb");
        assert_eq!(shell_quote("-c"), "-c");
        assert_eq!(shell_quote("gdb port 3333"), "'gdb port 3333'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn real_logs() {
        let st = feed(REAL_CMSIS_NO_ADAPTER);
        assert_eq!(st.errors, ["Error: unable to find a matching CMSIS-DAP device"]);
        assert!(st.hint.contains("couldn't open the probe"));
        assert!(st.cpus.is_empty());
        assert!(feed(REAL_STLINK_NO_ADAPTER).hint.contains("couldn't open the probe"));
        assert!(feed(REAL_JLINK_BAD_SERIAL).hint.contains("serial number"));
        assert!(hint_for(REAL_JLINK_WRONG_TRANSPORT.split('\n').nth(1).unwrap()).contains("transport"));
        let st = feed(REAL_DUMMY_JTAG);
        assert_eq!(st.errors.len(), 5);
        assert!(st.hint.contains("target doesn't answer") || st.hint.contains("JTAG chain"));
        let st = feed(REAL_DUMMY_READY);
        assert_eq!(st.listening.iter().map(|(k, v)| (k.as_str(), *v)).collect::<Vec<_>>(), [("tcl", 16667)]);
    }

    #[test]
    fn synthetic_logs() {
        let st = feed(SYN_STLINK_OK);
        assert_eq!(st.adapter, "STLINK V2J37S7 (API v2)");
        assert_eq!(st.voltage, Some(3.24));
        assert_eq!(st.cpus, ["stm32f4x.cpu: Cortex-M4 r0p1"]);
        assert_eq!(
            st.listening.iter().map(|(k, v)| (k.as_str(), *v)).collect::<Vec<_>>(),
            [("tcl", 6666), ("telnet", 4444), ("gdb", 3333)]
        );
        assert_eq!(st.gdb_clients, 1);
        assert_eq!(st.flash, "1024 KiB");
        assert!(st.errors.is_empty());
        assert_eq!(st.hint, "");
        let st = feed(SYN_STLINK_NO_TARGET);
        assert_eq!(st.voltage, Some(0.0));
        assert_eq!(st.errors.len(), 2);
        assert!(st.hint.contains("target doesn't answer"));
        let st = feed(SYN_CMSIS_OK);
        assert_eq!(st.adapter, "CMSIS-DAPv2 FW 2.0.0");
        assert_eq!(st.dpidr, "0x0bc12477");
        assert_eq!(st.cpus, ["rp2040.core0: Cortex-M0+ r0p1", "rp2040.core1: Cortex-M0+ r0p1"]);
        assert_eq!(st.flash, "2097152 bytes");
        assert!(feed(SYN_CMSIS_NO_TARGET).hint.contains("SWDIO/SWCLK"));
    }

    #[test]
    fn hints() {
        assert!(hint_for("Error: libusb_open() failed with LIBUSB_ERROR_ACCESS").contains("plugdev"));
        assert!(hint_for("Error: Couldn't bind tcl to socket on port 6666: Address already in use").contains("ports"));
        assert!(hint_for("Error: JTAG scan chain interrogation failed: all ones").contains("JTAG chain"));
        assert!(hint_for("Error: [stm32f4x.cpu] clearing lockup after double fault").contains("fault"));
        assert_eq!(hint_for("Info : nothing"), "");
    }

    #[test]
    fn tcl_wrap_unwrap_balanced() {
        assert_eq!(
            tcl_wrap("targets", true),
            r#"if {[catch {capture {targets}} __r]} {set __r "@@ERR@@$__r"} else {set __r "@@OK@@$__r"}"#
        );
        assert_eq!(tcl_unwrap("@@OK@@hello"), Ok("hello".into()));
        assert_eq!(tcl_unwrap("@@ERR@@invalid command name \"foo\""), Err("invalid command name \"foo\"".into()));
        assert!(tcl_unwrap("missing close-brace").is_err());
        assert!(tcl_balanced("program {/tmp/a b.elf} verify"));
        assert!(!tcl_balanced("echo {"));
        assert!(!tcl_balanced("echo }{"));
        assert!(tcl_balanced("echo \\{"));
        assert!(!tcl_balanced("echo \x1a"));
    }

    #[test]
    fn parse_num_and_hexdump() {
        let n = |s: &str| parse_num(Some(&Value::String(s.into())));
        assert_eq!(n("0x08000000"), Some(134217728.0));
        assert_eq!(n("1024"), Some(1024.0));
        assert_eq!(n("0x2000_0000"), Some(536870912.0));
        assert_eq!(n("12abc"), None);
        assert_eq!(n(""), None);
        assert_eq!(parse_num(Some(&json!(7))), Some(7.0));
        let mut b = vec![0x48, 0x69, 0, 0xff];
        b.extend([0x41; 14]);
        let d = hexdump(&b, 0x20000000);
        assert_eq!(d.split('\n').next().unwrap(), "20000000  48 69 00 ff 41 41 41 41 41 41 41 41 41 41 41 41  Hi..AAAAAAAAAAAA");
        assert_eq!(d.split('\n').nth(1).unwrap(), format!("20000010  41 41{}  AA", " ".repeat(42)));
        assert_eq!(parse_read_memory("0x41 0x42 0x0"), [0x41, 0x42, 0]);
    }

    pub(crate) fn make_uf2(n: usize, family: Option<u32>, addr: u32) -> Vec<u8> {
        let mut buf = vec![0u8; 512 * n];
        for i in 0..n {
            let o = i * 512;
            let mut put = |off: usize, v: u32| buf[o + off..o + off + 4].copy_from_slice(&v.to_le_bytes());
            put(0, 0x0a324655);
            put(4, 0x9e5d5157);
            put(8, if family.is_some() { 0x2000 } else { 0 });
            put(12, addr + i as u32 * 256);
            put(16, 256);
            put(20, i as u32);
            put(24, n as u32);
            put(28, family.unwrap_or(0));
            put(508, 0x0ab16f30);
        }
        buf
    }

    #[test]
    fn firmware_kinds() {
        assert_eq!(firmware_kind(&[0x7f, 0x45, 0x4c, 0x46, 1], ""), "elf");
        assert_eq!(firmware_kind(b":020000040800F2\n", ""), "hex");
        assert_eq!(firmware_kind(b"S00F0000", ""), "srec");
        assert_eq!(firmware_kind(&[0, 0x20, 0, 0x20], "a.bin"), "bin");
        assert_eq!(firmware_kind(&make_uf2(1, Some(0xe48bff56), 0x10000000), ""), "uf2");
        assert_eq!(firmware_kind(b"xx", "FW.HEX"), "hex");
    }

    #[test]
    fn uf2() {
        let u = parse_uf2(&make_uf2(4, Some(0xe48bff56), 0x10000000)).unwrap();
        assert_eq!(
            u,
            Uf2Info {
                blocks: 4,
                family_ids: vec![0xe48bff56],
                families: vec!["RP2040".into()],
                start: 0x10000000,
                end: 0x10000400,
                payload: 1024
            }
        );
        assert!(parse_uf2(&[0u8; 100]).unwrap_err().contains("512-byte"));
        let mut bad = make_uf2(2, None, 0x10000000);
        bad[512 + 508] = 0;
        assert!(parse_uf2(&bad).unwrap_err().contains("block 1"));
        assert!(uf2_accepts("rp2040", &[0xe48bff56]));
        assert!(!uf2_accepts("rp2040", &[0xe48bff59]));
        assert!(uf2_accepts("rp2350", &[0xe48bff59, 0xe48bff58]));
        assert!(uf2_accepts("rp2350", &[]));
        // Real RP2040 bootrom content.
        assert_eq!(
            parse_info_uf2("UF2 Bootloader v3.0\r\nModel: Raspberry Pi RP2\r\nBoard-ID: RPI-RP2\r\n"),
            json!({ "bootloader": "UF2 Bootloader v3.0", "fields": { "Model": "Raspberry Pi RP2", "Board-ID": "RPI-RP2" } })
        );
        assert_eq!(
            parse_dfu_progress("Erase   \t[=====     ]  45%\rDownload\t[==        ]  12%        8192 bytes"),
            Some(("Download".into(), 12))
        );
        assert_eq!(parse_dfu_progress("Opening DFU capable USB device..."), None);
    }

    #[test]
    fn snippets() {
        let s = client_snippets(
            "benchbob.local",
            &Ports { gdb: 3333, telnet: 4444, tcl: 6666 },
            "target/stm32f4x.cfg",
            "abc",
            "http://benchbob.local",
        );
        assert_eq!(s["gdb"], "arm-none-eabi-gdb -ex \"target extended-remote benchbob.local:3333\" build/app.elf");
        assert_eq!(s["telnet"], "telnet benchbob.local 4444");
        assert_eq!(serde_json::from_str::<Value>(s["cortexDebug"].as_str().unwrap()).unwrap()["gdbTarget"], "benchbob.local:3333");
        assert!(s["platformio"].as_str().unwrap().contains("debug_port = benchbob.local:3333"));
        assert!(s["platformio"].as_str().unwrap().contains("http://benchbob.local/api/probes/abc/flash"));
    }
}
