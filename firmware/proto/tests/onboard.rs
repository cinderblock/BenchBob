//! Onboard LEDs: the hold-to-light endpoint, its status view and MCP tool.

use benchbob_proto::api::*;
use benchbob_proto::json::{self, Buf, Value};

static LEDS: [OnboardLed; 2] =
    [OnboardLed { name: "LED", color: "green", normally: "Wi-Fi status" }, OnboardLed { name: "PWR", color: "red", normally: "power" }];
static INFO: BoardInfo = BoardInfo {
    board: "mock",
    board_name: "Mock Board",
    chip: "MOCK1",
    firmware_version: "0.1.0",
    build: "0.1.0+test",
    pins: &[],
    i2c: &[],
    leds: None,
    onboard_leds: &LEDS,
};

struct Mock {
    now: u64,
    holds: Vec<(usize, Option<u64>)>,
    info: &'static BoardInfo,
}

impl Node for Mock {
    fn info(&self) -> &'static BoardInfo {
        self.info
    }
    fn status(&mut self) -> Status<'_> {
        Status {
            hostname: "m",
            uptime_ms: self.now,
            mac: [0; 6],
            wifi_state: WifiState::Connected,
            ssid: "",
            rssi: None,
            ip: None,
            heap: None,
        }
    }
    fn gpio_read(&mut self, _gpio: u8) -> PinState {
        unreachable!()
    }
    fn gpio_write(&mut self, _gpio: u8, _mode: Option<(Mode, bool)>, _pull: Option<Pull>) {}
    fn i2c_probe(&mut self, _bus: u8, _addr: u8, _read: bool) -> Result<bool, I2cFault> {
        Ok(false)
    }
    fn i2c_write_read(&mut self, _bus: u8, _addr: u8, _wr: &[u8], _rd: &mut [u8]) -> Result<(), I2cFault> {
        Ok(())
    }
    fn hold_onboard_led(&mut self, i: usize, until_ms: Option<u64>) {
        self.holds.push((i, until_ms));
    }
}

struct H {
    api: Api,
    node: Mock,
    out: Vec<u8>,
}

impl H {
    fn new(info: &'static BoardInfo) -> Self {
        H { api: Api::new(), node: Mock { now: 1000, holds: vec![], info }, out: vec![0; 8192] }
    }
    fn req(&mut self, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut scratch = vec![0u8; 4096];
        let req = Request { method, path, query: "", host: None, body: body.as_bytes() };
        let mut buf = Buf::new(&mut self.out);
        let r = self.api.handle(&mut self.node, &req, &mut buf, &mut scratch);
        (r.status, buf.as_str().to_string())
    }
}

fn parse(s: &str) -> Value<'_> {
    json::parse(s).unwrap()
}

fn held(body: &str) -> Vec<bool> {
    let v = parse(body);
    let leds = v.get("onboardLeds").unwrap();
    leds.items().map(|l| l.get("held").unwrap().as_bool().unwrap()).collect()
}

#[test]
fn status_lists_onboard_leds() {
    let mut h = H::new(&INFO);
    let (st, body) = h.req("GET", "/api/status", "");
    assert_eq!(st, 200);
    let v = parse(&body);
    let l = v.get("onboardLeds").unwrap().items().next().unwrap();
    assert!(l.get("name").unwrap().str_eq("LED"));
    assert!(l.get("color").unwrap().str_eq("green"));
    assert_eq!(held(&body), [false, false]);
}

#[test]
fn hold_lights_all_until_the_deadline_then_release() {
    let mut h = H::new(&INFO);
    let (st, body) = h.req("POST", "/api/onboard-leds", r#"{"ms":500}"#);
    assert_eq!(st, 200, "{body}");
    assert_eq!(held(&body), [true, true]);
    assert_eq!(h.node.holds, [(0, Some(1500)), (1, Some(1500))]);
    // Not gated by output control (it's off).
    h.node.now = 1600;
    assert_eq!(held(&h.req("GET", "/api/status", "").1), [false, false]);
    h.node.holds.clear();
    h.req("POST", "/api/onboard-leds", r#"{"id":1}"#);
    assert_eq!(h.node.holds, [(1, Some(2600))]);
    h.req("POST", "/api/onboard-leds", r#"{"on":false}"#);
    assert_eq!(h.node.holds[1..], [(0, None), (1, None)]);
    assert_eq!(held(&h.req("GET", "/api/status", "").1), [false, false]);
}

#[test]
fn hold_refuses_bad_input() {
    let mut h = H::new(&INFO);
    assert_eq!(h.req("POST", "/api/onboard-leds", r#"{"id":2}"#).0, 400);
    assert_eq!(h.req("POST", "/api/onboard-leds", r#"{"ms":60000}"#).0, 400);
    assert_eq!(h.req("GET", "/api/onboard-leds", "").0, 405);
    assert!(h.node.holds.is_empty());
}

#[test]
fn mcp_tool_and_docs_only_where_there_are_leds() {
    let mut h = H::new(&INFO);
    let (_, body) =
        h.req("POST", "/mcp", r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"onboard_led","arguments":{"ms":3000}}}"#);
    assert!(body.contains(r#""held":true"#), "{body}");
    assert_eq!(h.node.holds, [(0, Some(4000)), (1, Some(4000))]);
    assert!(h.req("GET", "/api", "").1.contains("POST /api/onboard-leds"));

    static NONE: BoardInfo = BoardInfo { onboard_leds: &[], ..INFO };
    let mut h = H::new(&NONE);
    let (_, list) = h.req("POST", "/mcp", r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    assert!(!list.contains("onboard_led"));
    assert!(!h.req("GET", "/api", "").1.contains("/api/onboard-leds"));
    assert_eq!(h.req("POST", "/api/onboard-leds", "").0, 404);
}

#[test]
fn hold_deadline_wraps() {
    let h = Hold::new();
    assert!(!h.held(0));
    h.set(Some(500));
    assert!(h.held(499));
    assert!(!h.held(500));
    // Across the u32 wrap of the uptime.
    let near = u32::MAX as u64 - 100;
    h.set(Some(near + 300));
    assert!(h.held(near));
    assert!(h.held(near + 299));
    assert!(!h.held(near + 300));
    h.set(None);
    assert!(!h.held(near));
}
