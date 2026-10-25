//! The REST and MCP surface against a mock board.

use benchbob_proto::api::*;
use benchbob_proto::json::{self, Buf, Kind, Value};
use std::collections::BTreeMap;

static PINS: [PinInfo; 4] = [
    PinInfo { gpio: 2, header: Some(4), func: None, note: None },
    PinInfo { gpio: 3, header: Some(5), func: None, note: Some("strapping pin") },
    PinInfo { gpio: 4, header: Some(6), func: Some("I2C0 SDA"), note: None },
    PinInfo { gpio: 5, header: Some(7), func: Some("I2C0 SCL"), note: None },
];
static BUSES: [I2cBusInfo; 1] = [I2cBusInfo { bus: 0, label: "I2C0 (GP4/GP5)", sda: 4, scl: 5, hz: 100_000, can_quick: false }];
static INFO: BoardInfo = BoardInfo { board: "mock", board_name: "Mock Board", chip: "MOCK1", firmware_version: "0.1.0", build: "0.1.0+test", pins: &PINS, i2c: &BUSES, leds: None, onboard_leds: &[] };

#[derive(Default)]
struct Mock {
    pins: BTreeMap<u8, PinState>,
    /// Devices: address -> registers.
    devs: BTreeMap<u8, Vec<u8>>,
    probes: Vec<(u8, bool)>,
    stuck_at: Option<u8>,
}

impl Mock {
    fn new() -> Self {
        let mut m = Mock::default();
        m.devs.insert(0x76, (0..=255).collect());
        m.devs.insert(0x50, vec![0xAB; 256]);
        m
    }
}

impl Node for Mock {
    fn info(&self) -> &'static BoardInfo {
        &INFO
    }
    fn status(&mut self) -> Status<'_> {
        Status { hostname: "benchbob-mock", uptime_ms: 12_345, mac: [2, 0, 0, 0xa, 0xb, 0xc], wifi_state: WifiState::Connected, ssid: "lab", rssi: Some(-52), ip: Some([10, 0, 0, 9]), heap: None }
    }
    fn gpio_read(&mut self, gpio: u8) -> PinState {
        assert!(PINS.iter().any(|p| p.gpio == gpio && p.func.is_none()), "read of reserved pin {gpio}");
        *self.pins.get(&gpio).unwrap_or(&PinState { mode: Mode::Input, level: false, pull: Pull::None })
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
    fn i2c_probe(&mut self, _bus: u8, addr: u8, read: bool) -> Result<bool, I2cFault> {
        self.probes.push((addr, read));
        if self.stuck_at == Some(addr) {
            return Err(I2cFault::Timeout);
        }
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
}

struct Harness {
    api: Api,
    node: Mock,
}

struct Resp {
    status: u16,
    reply: Reply,
    body: String,
}

impl Resp {
    fn json(&self) -> Value<'_> {
        json::parse(&self.body).unwrap_or_else(|e| panic!("bad JSON {e:?}: {}", self.body))
    }
}

impl Harness {
    fn new() -> Self {
        Harness { api: Api::new(), node: Mock::new() }
    }
    fn req(&mut self, method: &str, path: &str, body: &str) -> Resp {
        let mut out = vec![0u8; 8192];
        let mut scratch = vec![0u8; 4096];
        let mut buf = Buf::new(&mut out);
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let r = Request { method, path, query, host: Some("node.test"), body: body.as_bytes() };
        let reply = self.api.handle(&mut self.node, &r, &mut buf, &mut scratch);
        Resp { status: reply.status, reply, body: buf.as_str().to_string() }
    }
    fn mcp(&mut self, body: &str) -> Resp {
        let r = self.req("POST", "/mcp", body);
        assert!(r.reply.mcp || r.status != 200);
        r
    }
    fn tool(&mut self, name: &str, args: &str) -> Resp {
        self.mcp(&format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#))
    }
}

fn s(v: Option<Value<'_>>) -> String {
    let v = v.expect("missing");
    let mut b = [0u8; 512];
    v.as_str_in(&mut b).map(|x| x.to_string()).unwrap_or_else(|| v.raw().to_string())
}

#[test]
fn docs_and_status() {
    let mut h = Harness::new();
    let r = h.req("GET", "/api", "");
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(s(v.get("name")), "BenchBob node API");
    assert_eq!(s(v.get("appVersion")), "0.1.0");
    assert_eq!(s(v.get("base")), "http://node.test");
    assert!(v.get("endpoints").unwrap().get("POST /mcp").is_some());
    assert_eq!(h.req("GET", "/api/", "").status, 200);

    let r = h.req("GET", "/api/status", "");
    let v = r.json();
    assert_eq!(v.get("uptime").unwrap().raw(), "12.345");
    assert_eq!(s(v.get("version")), "0.1.0");
    assert_eq!(s(v.get("build")), "0.1.0+test");
    let net = v.get("network").unwrap();
    assert_eq!(net.get("rssi").unwrap().as_i64(), Some(-52));
    assert_eq!(s(net.get("ip")), "10.0.0.9");
    assert_eq!(s(net.get("mac")), "02:00:00:0a:0b:0c");
    assert!(v.get("heap").unwrap().is_null());
}

#[test]
fn not_found_and_method_not_allowed() {
    let mut h = Harness::new();
    // `/` is the web UI, served before the API sees the request.
    assert_eq!(h.req("GET", "/", "").status, 404);
    let r = h.req("GET", "/nope", "");
    assert_eq!(r.status, 404);
    assert_eq!(r.json().get("ok").unwrap().as_bool(), Some(false));
    let r = h.req("DELETE", "/api/status", "");
    assert_eq!((r.status, r.reply.allow), (405, Some("GET, POST")));
    assert_eq!(h.req("GET", "/mcp", "").status, 405);
    assert_eq!(h.req("POST", "/api/gpio/2", "{bad").status, 400);
}

#[test]
fn gpio_output_needs_confirmed_enable() {
    let mut h = Harness::new();
    let v = h.req("GET", "/api/gpio", "");
    let pins = v.json().get("pins").unwrap();
    assert_eq!(pins.items().count(), 4);
    let sda = pins.items().nth(2).unwrap();
    assert_eq!(s(sda.get("func")), "I2C0 SDA");
    assert_eq!(s(sda.get("refusal")), "used by I2C0 SDA");
    assert!(sda.get("level").unwrap().is_null());

    // Off by default.
    let r = h.req("POST", "/api/gpio/2", r#"{"mode":"high"}"#);
    assert_eq!(r.status, 403);
    assert!(h.node.pins.is_empty());
    // Enabling without confirm is refused.
    assert_eq!(h.req("POST", "/api/gpio/output-control", r#"{"enabled":true}"#).status, 400);
    assert!(!h.api.output_enabled());
    let r = h.req("POST", "/api/gpio/output-control", r#"{"enabled":true,"confirm":true}"#);
    assert_eq!(r.body, r#"{"outputEnabled":true}"#);

    let r = h.req("POST", "/api/gpio/GP2", r#"{"mode":"HIGH","pull":"down"}"#);
    assert_eq!(r.status, 200, "{}", r.body);
    let v = r.json();
    assert_eq!((s(v.get("func")), v.get("level").unwrap().as_i64(), s(v.get("pull"))), ("output".into(), Some(1), "down".into()));
    assert_eq!(h.req("POST", "/api/gpio/4", r#"{"mode":"low"}"#).status, 409);
    assert_eq!(h.req("POST", "/api/gpio/9", r#"{"mode":"low"}"#).status, 404);
    assert_eq!(h.req("POST", "/api/gpio/2", r#"{"mode":"sideways"}"#).status, 400);
    assert_eq!(h.req("POST", "/api/gpio/2", r#"{}"#).status, 400);
    // Disabling needs no confirm.
    assert_eq!(h.req("POST", "/api/gpio/output-control", r#"{"enabled":false}"#).status, 200);
    assert_eq!(h.req("POST", "/api/gpio/2", r#"{"mode":"low"}"#).status, 403);
}

#[test]
fn i2c_scan_uses_read_byte_without_quick_write() {
    let mut h = Harness::new();
    let r = h.req("POST", "/api/i2c/0/scan", "");
    assert_eq!(r.status, 200, "{}", r.body);
    let devs: Vec<String> = r.json().get("devices").unwrap().items().map(|d| s(d.get("hex"))).collect();
    assert_eq!(devs, ["0x50", "0x76"]);
    assert_eq!(h.node.probes.len(), 0x77 - 0x08 + 1);
    assert!(h.node.probes.iter().all(|&(_, read)| read), "bus can't quick-write: every probe is a read");
    // Last scan is remembered.
    let r = h.req("GET", "/api/i2c", "");
    assert_eq!(r.json().get("buses").unwrap().items().next().unwrap().get("devices").unwrap().items().count(), 2);
    assert_eq!(h.req("POST", "/api/i2c/i2c-0/scan", "").status, 200);
    assert_eq!(h.req("POST", "/api/i2c/3/scan", "").status, 404);
}

#[test]
fn i2c_scan_stops_on_a_stuck_bus() {
    let mut h = Harness::new();
    h.node.stuck_at = Some(0x20);
    let r = h.req("POST", "/api/i2c/0/scan", "");
    assert_eq!(r.status, 502);
    assert!(r.body.contains("0x20"), "{}", r.body);
}

#[test]
fn i2c_raw_read_shapes() {
    let mut h = Harness::new();
    let r = h.req("POST", "/api/i2c/0/0x76/raw/read", r#"{"register":"0xD0"}"#);
    assert_eq!(r.body, r#"{"register":208,"bytes":[208]}"#);
    let r = h.req("POST", "/api/i2c/0/118/raw/read", r#"{"register":16,"size":"word"}"#);
    assert_eq!(r.body, r#"{"register":16,"word":4368,"wordBigEndian":4113,"bytes":[16,17]}"#);
    let r = h.req("POST", "/api/i2c/0/0x76/raw/read", r#"{"register":1,"size":"block","length":3}"#);
    assert_eq!(r.body, r#"{"register":1,"bytes":[1,2,3]}"#);
    let r = h.req("POST", "/api/i2c/0/0x76/raw/read", r#"{"length":2}"#);
    assert_eq!(r.body, r#"{"bytes":[0,1]}"#);
    assert_eq!(h.req("POST", "/api/i2c/0/0x76/raw/read", r#"{"size":"word"}"#).status, 400);
    assert_eq!(h.req("POST", "/api/i2c/0/0x76/raw/read", r#"{"register":1,"size":"block","length":33}"#).status, 400);
    assert_eq!(h.req("POST", "/api/i2c/0/0x02/raw/read", "").status, 400);
    assert_eq!(h.req("POST", "/api/i2c/0/0x10/raw/read", "").status, 502);
}

#[test]
fn mcp_initialize_and_list() {
    let mut h = Harness::new();
    let r = h.mcp(r#"{"jsonrpc":"2.0","id":"abc","method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{}}}"#);
    let v = r.json();
    assert_eq!(v.get("id").unwrap().raw(), r#""abc""#);
    let res = v.get("result").unwrap();
    assert_eq!(s(res.get("protocolVersion")), "2025-03-26");
    assert_eq!(s(res.get("serverInfo").unwrap().get("name")), "benchbob-node");

    // Notifications get 202 and no body.
    let r = h.mcp(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    assert_eq!((r.status, r.body.as_str()), (202, ""));

    let r = h.mcp(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    let v = r.json();
    let names: Vec<String> = v.get("result").unwrap().get("tools").unwrap().items().map(|t| s(t.get("name"))).collect();
    assert_eq!(names, ["gpio_read", "gpio_write", "i2c_buses", "i2c_scan", "i2c_read"]);
    for t in v.get("result").unwrap().get("tools").unwrap().items() {
        assert_eq!(t.get("inputSchema").unwrap().kind(), Kind::Object);
    }

    let r = h.mcp(r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#);
    assert_eq!(r.json().get("error").unwrap().get("code").unwrap().as_i64(), Some(-32601));
    let r = h.mcp("not json");
    assert_eq!(r.status, 400);
    assert_eq!(r.json().get("error").unwrap().get("code").unwrap().as_i64(), Some(-32700));
}

#[test]
fn mcp_batches() {
    let mut h = Harness::new();
    let r = h.mcp(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"},{"jsonrpc":"2.0","id":2,"method":"ping"}]"#);
    assert_eq!(r.body, r#"[{"jsonrpc":"2.0","id":1,"result":{}},{"jsonrpc":"2.0","id":2,"result":{}}]"#);
    let r = h.mcp(r#"[{"jsonrpc":"2.0","method":"a"}]"#);
    assert_eq!(r.status, 202);
}

#[test]
fn mcp_tools() {
    let mut h = Harness::new();
    let r = h.tool("i2c_scan", r#"{"bus":"i2c-0"}"#);
    let res = r.json().get("result").unwrap();
    assert!(res.get("isError").is_none(), "{}", r.body);
    let sc = res.get("structuredContent").unwrap();
    let addrs: Vec<String> = sc.get("devices").unwrap().items().map(|d| s(d.get("address"))).collect();
    assert_eq!(addrs, ["0x50", "0x76"]);
    // The text content is the same JSON.
    let text = s(res.get("content").unwrap().items().next().unwrap().get("text"));
    assert_eq!(text, sc.raw());

    let r = h.tool("i2c_read", r#"{"bus":0,"address":"0x50","register":0,"size":"byte"}"#);
    assert_eq!(r.json().get("result").unwrap().get("structuredContent").unwrap().raw(), r#"{"register":0,"bytes":[171]}"#);

    let r = h.tool("i2c_buses", "{}");
    let res = r.json().get("result").unwrap();
    assert!(res.get("structuredContent").is_none(), "arrays go in text only, like the Linux server");
    let text = s(res.get("content").unwrap().items().next().unwrap().get("text"));
    assert_eq!(json::parse(&text).unwrap().items().next().unwrap().get("devices").unwrap().as_i64(), Some(2));

    let r = h.tool("gpio_read", r#"{"gpios":[3]}"#);
    let pins = r.json().get("result").unwrap().get("structuredContent").unwrap().get("pins").unwrap();
    assert_eq!(pins.items().count(), 1);
    assert_eq!(s(pins.items().next().unwrap().get("note")), "strapping pin");

    // gpio_write is refused (as a tool error, not a protocol error) until enabled.
    let r = h.tool("gpio_write", r#"{"gpio":2,"mode":"high"}"#);
    let res = r.json().get("result").unwrap();
    assert_eq!(res.get("isError").unwrap().as_bool(), Some(true));
    assert!(s(res.get("content").unwrap().items().next().unwrap().get("text")).contains("output control is off"));
    h.req("POST", "/api/gpio/output-control", r#"{"enabled":true,"confirm":true}"#);
    let r = h.tool("gpio_write", r#"{"gpio":2,"mode":"high"}"#);
    assert!(r.json().get("result").unwrap().get("isError").is_none(), "{}", r.body);
    assert!(h.node.pins[&2].level);

    let r = h.tool("nope", "{}");
    assert_eq!(r.json().get("error").unwrap().get("code").unwrap().as_i64(), Some(-32602));
}

#[test]
fn small_buffer_reports_overflow() {
    let mut node = Mock::new();
    let mut api = Api::new();
    let mut out = [0u8; 96];
    let mut scratch = [0u8; 64];
    let mut buf = Buf::new(&mut out);
    let r = Request { method: "GET", path: "/api", query: "", host: None, body: b"" };
    let reply = api.handle(&mut node, &r, &mut buf, &mut scratch);
    assert_eq!(reply.status, 500);
    assert!(json::parse(buf.as_str()).is_ok());
}
