//! `/api/leds` and the `leds_*` MCP tools against a mock board, plus the board-facing side
//! (`Leds::job`, `render`, `report`).

use benchbob_pixels::timing::{PioFit, Timing};
use benchbob_proto::api::leds::MAX_STRINGS;
use benchbob_proto::api::*;
use benchbob_proto::json::{self, Buf, Value};

static PINS: [PinInfo; 6] = [
    PinInfo { gpio: 2, header: Some(4), func: None, note: None },
    PinInfo { gpio: 3, header: Some(5), func: None, note: None },
    PinInfo { gpio: 4, header: Some(6), func: Some("I2C0 SDA"), note: None },
    PinInfo { gpio: 5, header: Some(7), func: Some("I2C0 SCL"), note: None },
    PinInfo { gpio: 6, header: Some(9), func: None, note: None },
    PinInfo { gpio: 7, header: Some(10), func: None, note: None },
];

fn pio_timing(t: &Timing) -> Result<(), &'static str> {
    PioFit::find(t).map(|_| ()).ok_or("can't be made with the PIO program")
}

static LEDS: LedsInfo =
    LedsInfo { max_strings: 3, max_pixels: 300, one_wire: true, clocked: true, check_timing: pio_timing, max_clock_hz: 10_000_000 };
static INFO: BoardInfo = BoardInfo {
    board: "mock",
    board_name: "Mock Board",
    chip: "MOCK1",
    firmware_version: "0.1.0",
    build: "0.1.0+test",
    pins: &PINS,
    i2c: &[],
    leds: Some(&LEDS),
    onboard_leds: &[],
};

struct Mock;

impl Node for Mock {
    fn info(&self) -> &'static BoardInfo {
        &INFO
    }
    fn status(&mut self) -> Status<'_> {
        Status {
            hostname: "mock",
            uptime_ms: 1000,
            mac: [0; 6],
            wifi_state: WifiState::Connected,
            ssid: "lab",
            rssi: None,
            ip: None,
            heap: None,
        }
    }
    fn gpio_read(&mut self, gpio: u8) -> PinState {
        assert!(![4, 5].contains(&gpio), "read of reserved pin {gpio}");
        PinState { mode: Mode::Input, level: false, pull: Pull::None }
    }
    fn gpio_write(&mut self, _: u8, _: Option<(Mode, bool)>, _: Option<Pull>) {}
    fn i2c_probe(&mut self, _: u8, _: u8, _: bool) -> Result<bool, I2cFault> {
        Ok(false)
    }
    fn i2c_write_read(&mut self, _: u8, _: u8, _: &[u8], _: &mut [u8]) -> Result<(), I2cFault> {
        Err(I2cFault::Nack)
    }
}

struct Harness {
    api: Api,
}

struct Resp {
    status: u16,
    body: String,
}

impl Resp {
    fn json(&self) -> Value<'_> {
        json::parse(&self.body).unwrap_or_else(|e| panic!("bad JSON {e:?}: {}", self.body))
    }
    fn ok(self) -> Self {
        assert_eq!(self.status, 200, "{}", self.body);
        self
    }
}

impl Harness {
    fn new() -> Self {
        Harness { api: Api::new() }
    }
    fn req(&mut self, method: &str, path: &str, body: &str) -> Resp {
        let mut out = vec![0u8; 8192];
        let mut scratch = vec![0u8; 4096];
        let mut buf = Buf::new(&mut out);
        let r = Request { method, path, query: "", host: None, body: body.as_bytes() };
        let reply = self.api.handle(&mut Mock, &r, &mut buf, &mut scratch);
        Resp { status: reply.status, body: buf.as_str().to_string() }
    }
    fn tool(&mut self, name: &str, args: &str) -> Resp {
        self.req(
            "POST",
            "/mcp",
            &format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#),
        )
    }
    fn output(&mut self, on: bool) {
        self.req("POST", "/api/gpio/output-control", &format!(r#"{{"enabled":{on},"confirm":true}}"#)).ok();
    }
    /// Slot `i`'s next frame, as the board's LED task would get it.
    fn frame(&mut self, i: usize) -> Vec<u8> {
        let mut out = vec![0u8; 4 * 1024];
        let n = self.api.leds.render(i, 1000, &mut out);
        out.truncate(n);
        out
    }
}

fn s(v: Option<Value<'_>>) -> String {
    let v = v.expect("missing");
    let mut b = [0u8; 512];
    v.as_str_in(&mut b).map(|x| x.to_string()).unwrap_or_else(|| v.raw().to_string())
}

fn n(v: Option<Value<'_>>) -> i64 {
    v.and_then(|v| v.as_i64()).expect("not a number")
}

#[test]
fn capabilities_and_defaults() {
    let mut h = Harness::new();
    let r = h.req("GET", "/api/leds", "").ok();
    let v = r.json();
    let caps = v.get("capabilities").unwrap();
    assert_eq!(n(caps.get("maxStrings")), 3);
    assert_eq!(n(caps.get("maxPixels")), 300);
    assert_eq!(caps.get("chips").unwrap().items().count(), 7);
    let pins: Vec<i64> = caps.get("pins").unwrap().items().map(|p| p.as_i64().unwrap()).collect();
    assert_eq!(pins, [2, 3, 6, 7]);
    assert_eq!(v.get("strings").unwrap().items().count(), 0);

    let r = h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":2,"pixels":60}"#).ok();
    let v = r.json();
    assert_eq!(n(v.get("id")), 1);
    assert_eq!(s(v.get("name")), "strip 1");
    assert_eq!(s(v.get("order")), "GRB");
    assert_eq!(s(v.get("kind")), "one-wire");
    assert_eq!(n(v.get("timing").unwrap().get("t1hNs")), 800);
    assert_eq!(s(v.get("state")), "off");
    assert_eq!(s(v.get("source")), "pattern");
    assert_eq!(s(v.get("pattern").unwrap().get("name")), "rainbow");

    let v = h.req("POST", "/api/leds", r#"{"chip":"apa102","pin":6,"clockPin":7,"pixels":10,"name":"bench"}"#).ok();
    let v = v.json();
    assert_eq!(s(v.get("order")), "BGR");
    assert_eq!(n(v.get("clockHz")), 2_000_000);
    assert!(v.get("timing").unwrap().is_null());
}

#[test]
fn pins_are_claimed() {
    let mut h = Harness::new();
    h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":2,"pixels":10,"name":"desk"}"#).ok();
    let r = h.req("GET", "/api/gpio", "").ok();
    let v = r.json();
    let pin2 = v.get("pins").unwrap().items().find(|p| p.get("gpio").unwrap().as_i64() == Some(2)).unwrap();
    assert_eq!(s(pin2.get("func")), "LED string desk");
    assert!(pin2.get("level").unwrap().is_null());
    h.output(true);
    let r = h.req("POST", "/api/gpio/2", r#"{"mode":"high"}"#);
    assert_eq!(r.status, 409, "{}", r.body);
    assert!(r.body.contains("LED string desk"));

    assert_eq!(h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":2,"pixels":10}"#).status, 409);
    assert_eq!(h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":4,"pixels":10}"#).status, 409);
    assert_eq!(h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":9,"pixels":10}"#).status, 404);
    // Clocked strings claim both pins.
    h.req("POST", "/api/leds", r#"{"chip":"ws2801","pin":3,"clockPin":6,"pixels":10}"#).ok();
    assert_eq!(h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":6,"pixels":10}"#).status, 409);

    // Removing frees them.
    h.req("DELETE", "/api/leds/1", "").ok();
    h.req("POST", "/api/gpio/2", r#"{"mode":"high"}"#).ok();
    assert_eq!(h.req("DELETE", "/api/leds/1", "").status, 404);
}

#[test]
fn validation() {
    let mut h = Harness::new();
    let r = h.req("POST", "/api/leds", r#"{"chip":"neopixel","pin":2,"pixels":10}"#);
    assert_eq!(r.status, 400);
    assert!(r.body.contains("ws2812b") && r.body.contains("apa102"), "{}", r.body);
    assert_eq!(h.req("POST", "/api/leds", r#"{"pin":2,"pixels":10}"#).status, 400);
    assert_eq!(h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":2,"pixels":301}"#).status, 400);
    assert_eq!(h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":2,"pixels":10,"order":"RGBX"}"#).status, 400);
    // Clocked: needs a distinct clock pin.
    assert_eq!(h.req("POST", "/api/leds", r#"{"chip":"apa102","pin":2,"pixels":10}"#).status, 400);
    assert_eq!(h.req("POST", "/api/leds", r#"{"chip":"apa102","pin":2,"clockPin":2,"pixels":10}"#).status, 400);
    assert_eq!(h.req("POST", "/api/leds", r#"{"chip":"apa102","pin":2,"clockPin":3,"pixels":10,"clockHz":50000000}"#).status, 400);
    // Presets fix the timing; custom takes it, and it has to be possible.
    let r = h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":2,"pixels":10,"timing":{"t0hNs":300}}"#);
    assert_eq!(r.status, 400);
    assert!(r.body.contains("custom"));
    let r = h.req("POST", "/api/leds", r#"{"chip":"custom","pin":2,"pixels":10,"timing":{"t0hNs":900,"t1hNs":400}}"#);
    assert_eq!(r.status, 400, "{}", r.body);
    let r = h
        .req(
            "POST",
            "/api/leds",
            r#"{"chip":"custom","pin":2,"pixels":10,"timing":{"t0hNs":350,"t1hNs":700,"periodNs":1200,"resetUs":80}}"#,
        )
        .ok();
    assert_eq!(n(r.json().get("timing").unwrap().get("periodNs")), 1200);
    // The board's limit on strings.
    h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":3,"pixels":10}"#).ok();
    h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":6,"pixels":10}"#).ok();
    let r = h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":7,"pixels":10}"#);
    assert_eq!(r.status, 409, "{}", r.body);
    const { assert!(3 <= MAX_STRINGS) };
}

#[test]
fn update_keeps_what_is_not_given() {
    let mut h = Harness::new();
    h.req("POST", "/api/leds", r#"{"chip":"sk6812","pin":2,"pixels":10,"order":"GRBW","brightness":100}"#).ok();
    let r = h.req("POST", "/api/leds/1", r#"{"pixels":20,"power":{"maxMa":500}}"#).ok();
    let v = r.json();
    assert_eq!(n(v.get("pixels")), 20);
    assert_eq!(s(v.get("order")), "GRBW");
    assert_eq!(n(v.get("brightness")), 100);
    assert_eq!(n(v.get("power").unwrap().get("maPerChannel")), 20);
    assert_eq!(n(v.get("power").unwrap().get("maxMa")), 500);
    let r = h.req("POST", "/api/leds/1", r#"{"power":null}"#).ok();
    assert!(r.json().get("power").unwrap().is_null());
    // Moving to another pin is checked like a new one.
    assert_eq!(h.req("POST", "/api/leds/1", r#"{"pin":4}"#).status, 409);
    h.req("POST", "/api/leds/1", r#"{"pin":3}"#).ok();
    assert_eq!(h.req("POST", "/api/leds/7", r#"{"pin":3}"#).status, 404);
    assert_eq!(h.req("PUT", "/api/leds/1", "").status, 405);
}

#[test]
fn rendering_follows_output_control() {
    let mut h = Harness::new();
    h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":2,"pixels":3,"gamma":false}"#).ok();
    h.req("POST", "/api/leds/1/pixels", r#"{"pixels":"ff0000 00ff00 000080"}"#).ok();
    let job = h.api.leds.job(0).unwrap();
    assert!(!job.output && !job.animated);
    // Output control off: black.
    assert_eq!(h.frame(0), [0; 9]);
    h.output(true);
    let job2 = h.api.leds.job(0).unwrap();
    assert_ne!(job2.content_gen, job.content_gen, "output control redraws");
    assert_eq!(job2.config_gen, job.config_gen);
    // GRB wire order.
    assert_eq!(h.frame(0), [0, 255, 0, 255, 0, 0, 0, 0, 128]);
    // A moving pattern animates only while output control is on.
    h.req("POST", "/api/leds/1/pattern", r#"{"name":"chase","speed":5}"#).ok();
    assert!(h.api.leds.job(0).unwrap().animated);
    h.output(false);
    assert!(!h.api.leds.job(0).unwrap().animated);
    assert_eq!(h.frame(0), [0; 9]);
}

#[test]
fn pixels_and_patterns() {
    let mut h = Harness::new();
    h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":2,"pixels":4,"order":"RGB","gamma":false}"#).ok();
    h.output(true);
    h.req("POST", "/api/leds/1/pixels", r##"{"pixels":["#010203","#040506"],"start":2}"##).ok();
    assert_eq!(h.frame(0), [0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6]);
    h.req("POST", "/api/leds/1/pixels", r##"{"fill":"#ffffff","start":1,"count":1}"##).ok();
    assert_eq!(h.frame(0), [0, 0, 0, 255, 255, 255, 1, 2, 3, 4, 5, 6]);
    assert_eq!(h.req("POST", "/api/leds/1/pixels", r#"{"pixels":"ff00"}"#).status, 400);
    assert_eq!(h.req("POST", "/api/leds/1/pixels", r#"{"pixels":"ff0000ff0000ff0000ff0000ff0000"}"#).status, 400);
    assert_eq!(h.req("POST", "/api/leds/1/pixels", r#"{"pixels":"zz0000"}"#).status, 400);

    let r = h.req("POST", "/api/leds/1/pattern", r##"{"name":"solid","color":"#102030"}"##).ok();
    assert_eq!(s(r.json().get("pattern").unwrap().get("color")), "#102030");
    assert_eq!(h.frame(0), [0x10, 0x20, 0x30].repeat(4));
    // Same name again: keeps its parameters, changes only what's given.
    h.req("POST", "/api/leds/1/pattern", r#"{"name":"chase"}"#).ok();
    let r = h.req("POST", "/api/leds/1/pattern", r#"{"speed":7}"#).ok();
    let p = r.json();
    let p = p.get("pattern").unwrap();
    assert_eq!((n(p.get("speed")), n(p.get("width"))), (7, 3));
    let r = h.req("POST", "/api/leds/1/pattern", r#"{"name":"disco"}"#);
    assert_eq!(r.status, 400);
    assert!(r.body.contains("order-test"));
    h.req("POST", "/api/leds/1/pattern", r#"{"name":"order-test"}"#).ok();
    assert_eq!(h.frame(0), [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
}

#[test]
fn power_limit_and_reported_state() {
    let mut h = Harness::new();
    h.req("POST", "/api/leds", r#"{"chip":"ws2812b","pin":2,"pixels":10,"power":{"maPerChannel":20,"idleMa":1,"maxMa":100}}"#).ok();
    h.output(true);
    h.req("POST", "/api/leds/1/pattern", r##"{"name":"solid","color":"#ffffff"}"##).ok();
    let f = h.frame(0);
    assert!(f.iter().all(|&b| b < 255), "{f:?}");
    let job = h.api.leds.job(0).unwrap();
    h.api.leds.report(0, job.config_gen, Ok(()));
    let r = h.req("GET", "/api/leds", "").ok();
    let v = r.json();
    let st = v.get("strings").unwrap().items().next().unwrap();
    assert_eq!(s(st.get("state")), "running");
    assert!(n(st.get("estimateMa")) <= 100);
    // A stale report is ignored; a current error shows.
    h.api.leds.report(0, job.config_gen.wrapping_sub(1), Err("nope"));
    h.req("POST", "/api/leds/1", r#"{"pixels":5}"#).ok();
    let job = h.api.leds.job(0).unwrap();
    h.api.leds.report(0, job.config_gen, Err("no PIO state machine left"));
    let r = h.req("GET", "/api/leds", "").ok();
    let v = r.json();
    let st = v.get("strings").unwrap().items().next().unwrap();
    assert_eq!(s(st.get("state")), "error");
    assert_eq!(s(st.get("error")), "no PIO state machine left");
}

#[test]
fn mcp_tools() {
    let mut h = Harness::new();
    let r = h.req("POST", "/mcp", r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).ok();
    for t in ["leds_list", "leds_configure", "leds_pattern", "leds_set_pixels"] {
        assert!(r.body.contains(t), "{t}");
    }
    let r = h.tool("leds_configure", r#"{"chip":"ws2812b","pin":2,"pixels":8,"name":"shelf"}"#).ok();
    assert!(!r.body.contains("isError"), "{}", r.body);
    let v = r.json();
    let sc = v.get("result").unwrap().get("structuredContent").unwrap();
    assert_eq!(n(sc.get("id")), 1);
    let r = h.tool("leds_pattern", r#"{"id":1,"name":"ruler"}"#).ok();
    assert!(r.body.contains(r#"\"name\":\"ruler\""#), "{}", r.body);
    let r = h.tool("leds_set_pixels", r##"{"id":1,"fill":"#00ff00"}"##).ok();
    assert!(r.body.contains(r#"\"source\":\"pixels\""#), "{}", r.body);
    let r = h.tool("leds_list", "{}").ok();
    assert!(r.body.contains("shelf"));
    let r = h.tool("leds_configure", r#"{"id":1,"delete":true}"#).ok();
    assert!(r.body.contains("removed"));
    let r = h.tool("leds_pattern", r#"{"id":1,"name":"ruler"}"#).ok();
    assert!(r.body.contains("isError"), "{}", r.body);
}

#[test]
fn board_without_leds() {
    static NO: BoardInfo = BoardInfo { leds: None, ..INFO };
    struct Plain;
    impl Node for Plain {
        fn info(&self) -> &'static BoardInfo {
            &NO
        }
        fn status(&mut self) -> Status<'_> {
            Mock.status_static()
        }
        fn gpio_read(&mut self, _: u8) -> PinState {
            PinState { mode: Mode::Input, level: false, pull: Pull::None }
        }
        fn gpio_write(&mut self, _: u8, _: Option<(Mode, bool)>, _: Option<Pull>) {}
        fn i2c_probe(&mut self, _: u8, _: u8, _: bool) -> Result<bool, I2cFault> {
            Ok(false)
        }
        fn i2c_write_read(&mut self, _: u8, _: u8, _: &[u8], _: &mut [u8]) -> Result<(), I2cFault> {
            Ok(())
        }
    }
    let mut api = Api::new();
    let mut out = vec![0u8; 8192];
    let mut scratch = vec![0u8; 4096];
    let mut buf = Buf::new(&mut out);
    let r = Request { method: "GET", path: "/api/leds", query: "", host: None, body: b"" };
    assert_eq!(api.handle(&mut Plain, &r, &mut buf, &mut scratch).status, 200);
    assert!(buf.as_str().contains(r#""capabilities":null"#));
    let mut buf = Buf::new(&mut out);
    let r = Request { method: "POST", path: "/api/leds", query: "", host: None, body: br#"{"chip":"ws2812b","pin":2,"pixels":1}"# };
    assert_eq!(api.handle(&mut Plain, &r, &mut buf, &mut scratch).status, 404);
}

impl Mock {
    fn status_static(&self) -> Status<'static> {
        Status {
            hostname: "mock",
            uptime_ms: 1000,
            mac: [0; 6],
            wifi_state: WifiState::Connected,
            ssid: "lab",
            rssi: None,
            ip: None,
            heap: None,
        }
    }
}
