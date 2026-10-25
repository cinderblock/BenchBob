//! A node on your PC: the firmware's API, MCP and web UI handling (this crate, as the
//! boards run it) over std networking, against a pretend board. For working on the web
//! UI's MCU build, or a client, without flashing anything:
//!
//! ```sh
//! (cd web && bun run build:mcu)
//! cargo run --example mock_node            # http://127.0.0.1:8099/
//! cargo run --example mock_node -- 0.0.0.0:8099 path/to/dist-mcu
//! ```
//!
//! The UI is read from `web/dist-mcu` at startup and served uncompressed (the firmware
//! also stores a gzipped copy; that path is covered by `web`'s unit tests). The board has
//! eight free GPIOs and two I²C devices: a BME280 at 0x76 (chip id 0x60 at 0xD0) and an
//! EEPROM at 0x50. GP7's input level follows the clock, so the GPIO panel visibly polls.

use benchbob_proto::api::*;
use benchbob_proto::http::{self, HeadError};
use benchbob_proto::json::Buf;
use benchbob_proto::web::{self, Asset};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const fn pin(gpio: u8, header: u8) -> PinInfo {
    PinInfo { gpio, header: Some(header), func: None, note: None }
}

static PINS: [PinInfo; 10] = [
    pin(0, 1),
    pin(1, 2),
    pin(2, 4),
    pin(3, 5),
    PinInfo { gpio: 4, header: Some(6), func: Some("I2C0 SDA"), note: None },
    PinInfo { gpio: 5, header: Some(7), func: Some("I2C0 SCL"), note: None },
    pin(6, 9),
    PinInfo { gpio: 7, header: Some(10), func: None, note: Some("mock: input level follows the clock") },
    pin(8, 11),
    PinInfo { gpio: 9, header: Some(12), func: None, note: Some("strapping pin") },
];
static BUSES: [I2cBusInfo; 1] = [I2cBusInfo { bus: 0, label: "I2C0 (GP4/GP5)", sda: 4, scl: 5, hz: 100_000, can_quick: false }];

struct Mock {
    started: Instant,
    pins: BTreeMap<u8, PinState>,
    /// Address -> 256 registers.
    devs: BTreeMap<u8, Vec<u8>>,
}

impl Mock {
    fn new() -> Self {
        let mut bme = vec![0u8; 256];
        bme[0xD0] = 0x60;
        let mut devs = BTreeMap::new();
        devs.insert(0x76, bme);
        devs.insert(0x50, (0..=255).collect());
        Mock { started: Instant::now(), pins: BTreeMap::new(), devs }
    }
}

impl Node for Mock {
    fn info(&self) -> &'static BoardInfo {
        info()
    }
    fn status(&mut self) -> Status<'_> {
        Status {
            hostname: "benchbob-mock",
            uptime_ms: self.started.elapsed().as_millis() as u64,
            mac: [2, 0, 0, 0xa, 0xb, 0xc],
            wifi_state: WifiState::Connected,
            ssid: "mock",
            rssi: Some(-52),
            ip: Some([127, 0, 0, 1]),
            heap: None,
        }
    }
    fn gpio_read(&mut self, gpio: u8) -> PinState {
        let mut s = *self.pins.get(&gpio).unwrap_or(&PinState { mode: Mode::Input, level: false, pull: Pull::None });
        if gpio == 7 && s.mode == Mode::Input {
            s.level = self.started.elapsed().as_secs() % 2 == 1;
        } else if s.mode == Mode::Input {
            s.level = s.pull == Pull::Up;
        }
        s
    }
    fn gpio_write(&mut self, gpio: u8, mode: Option<(Mode, bool)>, pull: Option<Pull>) {
        let mut s = self.gpio_read(gpio);
        if let Some((m, l)) = mode {
            s.mode = m;
            s.level = l;
        }
        if let Some(p) = pull {
            s.pull = p;
        }
        self.pins.insert(gpio, s);
    }
    fn i2c_probe(&mut self, _bus: u8, addr: u8, _read: bool) -> Result<bool, I2cFault> {
        Ok(self.devs.contains_key(&addr))
    }
    fn i2c_write_read(&mut self, _bus: u8, addr: u8, wr: &[u8], rd: &mut [u8]) -> Result<(), I2cFault> {
        let regs = self.devs.get(&addr).ok_or(I2cFault::Nack)?;
        let start = wr.first().copied().unwrap_or(0) as usize;
        for (i, b) in rd.iter_mut().enumerate() {
            *b = regs[(start + i) % 256];
        }
        Ok(())
    }
    fn hold_onboard_led(&mut self, i: usize, until_ms: Option<u64>) {
        match until_ms {
            Some(t) => println!("onboard LED {i}: on until {t} ms"),
            None => println!("onboard LED {i}: released"),
        }
    }
}

fn info() -> &'static BoardInfo {
    static INFO: std::sync::OnceLock<BoardInfo> = std::sync::OnceLock::new();
    INFO.get_or_init(|| BoardInfo {
        board: "mock",
        board_name: "Mock node (host)",
        chip: "host",
        firmware_version: benchbob_proto::VERSION,
        build: BUILD.get().map(String::as_str).unwrap_or(benchbob_proto::VERSION),
        pins: &PINS,
        i2c: &BUSES,
        leds: Some(&LEDS),
        onboard_leds: &[OnboardLed { name: "LED", color: "green", normally: "Wi-Fi status (pretend)" }],
    })
}

/// Pretend LED hardware: every timing the RP2350's PIO program can make, both kinds.
static LEDS: LedsInfo = LedsInfo {
    max_strings: 4,
    max_pixels: 1024,
    one_wire: true,
    clocked: true,
    check_timing: |t| benchbob_pixels::timing::PioFit::find(t).map(|_| ()).ok_or("the PIO program can't make this timing"),
    max_clock_hz: 10_000_000,
};

/// What a board's LED task does, minus the hardware: claim (always works here) and render.
fn fake_led_task(api: &mut Api) {
    let mut frame = vec![0u8; 4 * benchbob_proto::api::leds::MAX_PIXELS];
    for i in 0..benchbob_proto::api::leds::MAX_STRINGS {
        if let Some(job) = api.leds.job(i) {
            api.leds.report(i, job.config_gen, Ok(()));
            api.leds.render(i, 0, &mut frame);
        }
    }
}

static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// web/dist-mcu as assets, the page stamped as the firmware's build.rs does. The build id
/// here is the version plus the startup time, so restarting the mock reloads open pages.
fn load(dist: &Path) -> &'static [Asset] {
    let build = BUILD.get_or_init(|| format!("{}+mock{}", benchbob_proto::VERSION, std::process::id()));
    let mut out = Vec::new();
    let mut stack = vec![dist.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e} (build it: cd web && bun run build:mcu)", d.display())) {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let rel = format!("/{}", p.strip_prefix(dist).unwrap().to_string_lossy().replace('\\', "/"));
            let mut bytes = std::fs::read(&p).unwrap();
            if rel == "/index.html" {
                bytes = String::from_utf8_lossy(&bytes).replace("__BUILD_VERSION__", build).into_bytes();
            }
            let bytes: &'static [u8] = bytes.leak();
            let mime = match rel.rsplit_once('.').map(|(_, x)| x) {
                Some("html") => "text/html; charset=utf-8",
                Some("js") => "text/javascript; charset=utf-8",
                Some("css") => "text/css; charset=utf-8",
                _ => "application/octet-stream",
            };
            let etag: &'static str = format!("\"{}-{}\"", build, bytes.len()).leak();
            let immutable = rel.starts_with("/assets/");
            // `gz` is never sent: requests are looked up as if they didn't take gzip.
            out.push(Asset { path: rel.leak(), mime, gz: bytes, raw: bytes, etag, immutable });
        }
    }
    out.leak()
}

type Shared = Arc<Mutex<(Api, Mock)>>;

fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:8099".into());
    let dist = args.next().map(PathBuf::from).unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist-mcu"));
    let assets = load(&dist);
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| panic!("bind {addr}: {e}"));
    println!("mock node {} on http://{addr}/ (UI from {})", info().build, dist.display());
    let shared: Shared = Arc::new(Mutex::new((Api::new(), Mock::new())));
    for s in listener.incoming().flatten() {
        let shared = shared.clone();
        std::thread::spawn(move || {
            if let Err(e) = connection(s, &shared, assets) {
                eprintln!("http: {e}");
            }
        });
    }
}

/// node/src/http.rs's `connection`, over a std socket.
fn connection(mut sock: TcpStream, shared: &Shared, assets: &'static [Asset]) -> Result<(), String> {
    let mut req = vec![0u8; 4096];
    let mut n = 0;
    let (head_len, content_length) = loop {
        let got = sock.read(&mut req[n..]).map_err(|e| e.to_string())?;
        if got == 0 {
            return Err("closed before the request".into());
        }
        n += got;
        match http::parse_head(&req[..n]) {
            Ok(Some(h)) => break (h.len, h.content_length),
            Ok(None) if n == req.len() => return Err("request head too large".into()),
            Ok(None) => {}
            Err(HeadError::BadRequest) => return Err("bad request".into()),
        }
    };
    let total = head_len + content_length;
    if total > req.len() {
        return Err("body too large".into());
    }
    while n < total {
        let got = sock.read(&mut req[n..total]).map_err(|e| e.to_string())?;
        if got == 0 {
            return Err("closed mid-body".into());
        }
        n += got;
    }
    let h = http::parse_head(&req[..total]).ok().flatten().ok_or("head vanished")?;
    let mut head_mem = [0u8; 512];
    let mut head = Buf::new(&mut head_mem);
    if h.method == "OPTIONS" {
        http::preflight(&h, &mut head);
        return sock.write_all(head.as_bytes()).map_err(|e| e.to_string());
    }
    if let Some(s) = web::lookup(assets, h.method, h.path, None, h.if_none_match) {
        s.write_head(&mut head);
        sock.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
        return sock.write_all(s.body()).map_err(|e| e.to_string());
    }
    let request = Request { method: h.method, path: h.path, query: h.query, host: h.host, body: &req[head_len..total] };
    let mut out = vec![0u8; 8192];
    let mut scratch = vec![0u8; 4096];
    let mut body = Buf::new(&mut out);
    let reply = {
        let mut g = shared.lock().unwrap();
        let (api, node) = &mut *g;
        let reply = api.handle(node, &request, &mut body, &mut scratch);
        fake_led_task(api);
        reply
    };
    let mut extra: Vec<(&str, &str)> = Vec::new();
    if reply.mcp {
        extra.push(("MCP-Protocol-Version", MCP_PROTOCOL));
    }
    if let Some(a) = reply.allow {
        extra.push(("Allow", a));
    }
    http::response_head(reply.status, reply.content_type, body.len(), &extra, &mut head);
    sock.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    sock.write_all(body.as_bytes()).map_err(|e| e.to_string())
}
