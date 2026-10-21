//! Telnet + RFC 2217 (Com Port Control) server session (lib/rfc2217.ts).
//!
//! One instance per TCP client. Transport-agnostic: `feed()` takes bytes from the socket and
//! returns the serial payload with the Telnet layer stripped; everything the protocol wants
//! to say back is queued as separate sends (`take_sent()`). The serial side is reached
//! through [`PortHooks`], so this file has no I/O of its own and is unit-tested with a fake
//! port.
//!
//! Written against RFC 854/855 (Telnet option negotiation, in the "Q method" spirit of RFC
//! 1143: only answer when an option's state actually changes, so two peers can never loop)
//! and RFC 2217. The reference client is pyserial's rfc2217:// (3.5), which:
//!  - requires its WILL COM-PORT-OPTION to be answered with DO;
//!  - waits up to 3 s for every SET-* reply and checks that the reply value *starts with*
//!    the value it asked for, so replies must echo the effective setting promptly;
//!  - polls the modem state by sending NOTIFY-MODEMSTATE (7) and expects a 107 back.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::termios::{Flow, ModemLines, Parity, Queue, SerialConfig};

pub const IAC: u8 = 255;
pub const DONT: u8 = 254;
pub const DO: u8 = 253;
pub const WONT: u8 = 252;
pub const WILL: u8 = 251;
pub const SB: u8 = 250;
pub const SE: u8 = 240;
pub const OPT_BINARY: u8 = 0;
pub const OPT_ECHO: u8 = 1;
pub const OPT_SGA: u8 = 3;
pub const OPT_COM_PORT: u8 = 44;

// Client -> server sub-option codes; the server answers with code + 100.
pub mod cpo {
    pub const SIGNATURE: u8 = 0;
    pub const SET_BAUDRATE: u8 = 1;
    pub const SET_DATASIZE: u8 = 2;
    pub const SET_PARITY: u8 = 3;
    pub const SET_STOPSIZE: u8 = 4;
    pub const SET_CONTROL: u8 = 5;
    pub const NOTIFY_LINESTATE: u8 = 6;
    pub const NOTIFY_MODEMSTATE: u8 = 7;
    pub const FLOWCONTROL_SUSPEND: u8 = 8;
    pub const FLOWCONTROL_RESUME: u8 = 9;
    pub const SET_LINESTATE_MASK: u8 = 10;
    pub const SET_MODEMSTATE_MASK: u8 = 11;
    pub const PURGE_DATA: u8 = 12;
}

fn parity_code(p: Parity) -> u8 {
    match p {
        Parity::None => 1,
        Parity::Odd => 2,
        Parity::Even => 3,
        Parity::Mark => 4,
        Parity::Space => 5,
    }
}

fn code_parity(c: u8) -> Option<&'static str> {
    Some(match c {
        1 => "none",
        2 => "odd",
        3 => "even",
        4 => "mark",
        5 => "space",
        _ => return None,
    })
}

pub const MODEM_CTS: u8 = 0x10;
pub const MODEM_DSR: u8 = 0x20;
pub const MODEM_RI: u8 = 0x40;
pub const MODEM_CD: u8 = 0x80;

/// The serial port, as a session sees it.
pub trait PortHooks {
    fn config(&self) -> SerialConfig;
    /// Apply a partial change (`{ baud: 9600 }`); the effective config, or why it was refused.
    fn set_config(&mut self, c: &Value) -> Result<SerialConfig, String>;
    fn lines(&self) -> Option<ModemLines>;
    fn set_lines(&mut self, dtr: Option<bool>, rts: Option<bool>);
    fn break_state(&self) -> bool;
    fn set_break(&mut self, on: bool);
    fn purge(&mut self, which: Queue);
    /// The client asked us to pause (true) or resume (false) sending it serial data.
    fn suspend(&mut self, _on: bool) {}
    fn log(&mut self, _msg: &str) {}
}

/// Double every IAC so serial bytes can't be mistaken for Telnet commands.
pub fn escape_iac(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    for &b in data {
        out.push(b);
        if b == IAC {
            out.push(IAC);
        }
    }
    out
}

pub fn modem_byte(l: Option<ModemLines>) -> u8 {
    let Some(l) = l else { return 0 };
    (if l.cts { MODEM_CTS } else { 0 })
        | (if l.dsr { MODEM_DSR } else { 0 })
        | (if l.ri { MODEM_RI } else { 0 })
        | (if l.dcd { MODEM_CD } else { 0 })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum M {
    Data,
    Iac,
    Neg,
    Sub,
    SubIac,
}

// Options we'll agree to. ECHO lets an interactive `telnet host port` switch to character
// mode and leave echoing to the device, which is what ser2net does too.
const WE_OK: [u8; 4] = [OPT_BINARY, OPT_SGA, OPT_ECHO, OPT_COM_PORT];
const THEY_OK: [u8; 3] = [OPT_BINARY, OPT_SGA, OPT_COM_PORT];

pub struct Rfc2217Session {
    // Per-option state for each side: "we" = this server performs it, "they" = the client does.
    we: HashMap<u8, bool>,
    they: HashMap<u8, bool>,
    mode: M,
    neg_cmd: u8,
    sub: Vec<u8>,
    last_cr: bool,
    modem_mask: u8,
    line_mask: u8,
    last_modem: Option<u8>,
    sent: Vec<Vec<u8>>,
    /// True once the client has agreed to COM-PORT-OPTION in either direction.
    pub rfc2217: bool,
    pub signature: String,
}

impl Default for Rfc2217Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Rfc2217Session {
    pub fn new() -> Self {
        Self {
            we: HashMap::new(),
            they: HashMap::new(),
            mode: M::Data,
            neg_cmd: 0,
            sub: Vec::new(),
            last_cr: false,
            modem_mask: 0xff,
            line_mask: 0,
            last_modem: None,
            sent: Vec::new(),
            rfc2217: false,
            signature: "BenchBob".into(),
        }
    }

    /// What the session queued to send the client, one entry per send, oldest first.
    pub fn take_sent(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.sent)
    }

    fn send(&mut self, b: Vec<u8>) {
        self.sent.push(b);
    }

    /// Our opening offer. Harmless for clients that don't speak Telnet; they see a few bytes once.
    pub fn start(&mut self) {
        self.offer(WILL, OPT_SGA);
        self.offer(WILL, OPT_ECHO);
        self.offer(WILL, OPT_BINARY);
        self.offer(DO, OPT_BINARY);
        self.offer(WILL, OPT_COM_PORT);
    }

    fn offer(&mut self, cmd: u8, opt: u8) {
        // Record the request as granted-pending; if refused, the DONT/WONT resets it.
        if cmd == WILL { &mut self.we } else { &mut self.they }.insert(opt, true);
        self.send(vec![IAC, cmd, opt]);
    }

    /// Client bytes in; serial payload out.
    pub fn feed(&mut self, chunk: &[u8], port: &mut dyn PortHooks) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk.len());
        for &b in chunk {
            match self.mode {
                M::Data => {
                    if b == IAC {
                        self.mode = M::Iac;
                    } else {
                        // NVT: a non-binary client sends CR as "CR NUL". Drop the NUL.
                        if b == 0 && self.last_cr && self.they.get(&OPT_BINARY) != Some(&true) {
                            self.last_cr = false;
                            continue;
                        }
                        self.last_cr = b == 13;
                        out.push(b);
                    }
                }
                M::Iac => {
                    if b == IAC {
                        out.push(IAC);
                        self.last_cr = false;
                        self.mode = M::Data;
                    } else if b == SB {
                        self.sub.clear();
                        self.mode = M::Sub;
                    } else if matches!(b, DO | DONT | WILL | WONT) {
                        self.neg_cmd = b;
                        self.mode = M::Neg;
                    } else {
                        // NOP, AYT, BRK…: nothing useful to do for a serial port.
                        self.mode = M::Data;
                    }
                }
                M::Neg => {
                    self.negotiate(self.neg_cmd, b, port);
                    self.mode = M::Data;
                }
                M::Sub => {
                    if b == IAC {
                        self.mode = M::SubIac;
                    } else {
                        self.sub.push(b);
                    }
                }
                M::SubIac => {
                    if b == IAC {
                        self.sub.push(IAC);
                        self.mode = M::Sub;
                    } else if b == SE {
                        self.mode = M::Data;
                        let s = std::mem::take(&mut self.sub);
                        self.subnegotiation(&s, port);
                    } else {
                        // Malformed (IAC x inside SB). Abandon the sub-negotiation rather than desync.
                        self.mode = M::Data;
                    }
                }
            }
        }
        out
    }

    fn negotiate(&mut self, cmd: u8, opt: u8, port: &mut dyn PortHooks) {
        let (map, ok_set, yes, no): (&mut HashMap<u8, bool>, &[u8], u8, u8) =
            if cmd == DO || cmd == DONT { (&mut self.we, &WE_OK, WILL, WONT) } else { (&mut self.they, &THEY_OK, DO, DONT) };
        let want = cmd == DO || cmd == WILL;
        let ok = want && ok_set.contains(&opt);
        let prev = map.get(&opt).copied();
        let cur = prev == Some(true);
        let answer = ok != cur || (want && !ok && prev.is_none());
        map.insert(opt, ok);
        if answer {
            // Only answer on a state change (or the first refusal), never re-acknowledge.
            self.send(vec![IAC, if ok { yes } else { no }, opt]);
        }
        if opt == OPT_COM_PORT && (self.we.get(&opt) == Some(&true) || self.they.get(&opt) == Some(&true)) && !self.rfc2217 {
            self.rfc2217 = true;
            port.log("client speaks RFC 2217");
            self.notify_modem(true, port);
        }
    }

    fn reply(&mut self, code: u8, value: &[u8]) {
        let f = sub_frame(code + 100, value);
        self.send(f);
    }

    fn subnegotiation(&mut self, s: &[u8], port: &mut dyn PortHooks) {
        if s.len() < 2 || s[0] != OPT_COM_PORT {
            return;
        }
        let code = s[1];
        let v = &s[2..];
        let try_cfg = |c: Value, port: &mut dyn PortHooks| match port.set_config(&c) {
            Ok(cfg) => cfg,
            Err(e) => {
                port.log(&format!("rejected {}: {e}", serde_json::to_string(&c).unwrap_or_default()));
                port.config()
            }
        };
        match code {
            cpo::SIGNATURE => {
                if v.is_empty() {
                    let sig = self.signature.clone().into_bytes();
                    self.reply(code, &sig);
                }
            }
            cpo::SET_BAUDRATE => {
                let req = if v.len() >= 4 { u32::from_be_bytes([v[0], v[1], v[2], v[3]]) } else { 0 };
                let c = if req != 0 { try_cfg(json!({ "baud": req }), port) } else { port.config() };
                self.reply(code, &c.baud.to_be_bytes());
            }
            cpo::SET_DATASIZE => {
                let req = v.first().copied().unwrap_or(0);
                let c = if req != 0 { try_cfg(json!({ "dataBits": req }), port) } else { port.config() };
                self.reply(code, &[c.data_bits]);
            }
            cpo::SET_PARITY => {
                let c = match v.first().copied().and_then(code_parity) {
                    Some(p) => try_cfg(json!({ "parity": p }), port),
                    None => port.config(),
                };
                self.reply(code, &[parity_code(c.parity)]);
            }
            cpo::SET_STOPSIZE => {
                // 1 = 1, 2 = 2, 3 = 1.5. Linux has no 1.5 (except CSTOPB at 5 bits), so 1.5 maps
                // to 2 and the reply says so; pyserial then reports the mismatch to its user.
                let req = match v.first() {
                    Some(1) => 1,
                    Some(2 | 3) => 2,
                    _ => 0,
                };
                let c = if req != 0 { try_cfg(json!({ "stopBits": req }), port) } else { port.config() };
                self.reply(code, &[c.stop_bits]);
            }
            cpo::SET_CONTROL => {
                let r = Self::control(v.first().copied().unwrap_or(0), port);
                self.reply(code, &[r]);
            }
            // pyserial uses this as a poll.
            cpo::NOTIFY_MODEMSTATE => self.notify_modem(true, port),
            cpo::NOTIFY_LINESTATE => self.send(sub_frame(106, &[0])),
            cpo::FLOWCONTROL_SUSPEND => port.suspend(true),
            cpo::FLOWCONTROL_RESUME => port.suspend(false),
            cpo::SET_LINESTATE_MASK => {
                self.line_mask = v.first().copied().unwrap_or(0);
                let m = self.line_mask;
                self.reply(code, &[m]);
            }
            cpo::SET_MODEMSTATE_MASK => {
                self.modem_mask = v.first().copied().unwrap_or(0);
                let m = self.modem_mask;
                self.reply(code, &[m]);
            }
            cpo::PURGE_DATA => {
                let which = match v.first() {
                    Some(1) => Some(Queue::Rx),
                    Some(2) => Some(Queue::Tx),
                    Some(3) => Some(Queue::Both),
                    _ => None,
                };
                if let Some(w) = which {
                    port.purge(w);
                }
                self.reply(code, &[v.first().copied().unwrap_or(0)]);
            }
            _ => {}
        }
    }

    /// SET-CONTROL: apply and return the value to echo (requests answer with the current state).
    fn control(c: u8, port: &mut dyn PortHooks) -> u8 {
        let l = port.lines();
        let cfg = port.config();
        let flow_code = match cfg.flow {
            Flow::RtsCts => 3,
            Flow::XonXoff => 2,
            Flow::None => 1,
        };
        let set_flow = |f: &str, port: &mut dyn PortHooks| {
            let _ = port.set_config(&json!({ "flow": f }));
        };
        match c {
            0 => flow_code,
            1 => {
                set_flow("none", port);
                c
            }
            2 => {
                set_flow("xonxoff", port);
                c
            }
            3 => {
                set_flow("rtscts", port);
                c
            }
            4 => {
                if port.break_state() {
                    5
                } else {
                    6
                }
            }
            5 | 6 => {
                port.set_break(c == 5);
                c
            }
            7 => {
                if l.is_some_and(|l| l.dtr) {
                    8
                } else {
                    9
                }
            }
            8 | 9 => {
                port.set_lines(Some(c == 8), None);
                c
            }
            10 => {
                if l.is_some_and(|l| l.rts) {
                    11
                } else {
                    12
                }
            }
            11 | 12 => {
                port.set_lines(None, Some(c == 11));
                c
            }
            13 => match cfg.flow {
                Flow::RtsCts => 16,
                Flow::XonXoff => 15,
                Flow::None => 14,
            },
            14..=16 => {
                // Inbound flow control can't differ from outbound on Linux; apply the same setting.
                set_flow(
                    match c {
                        16 => "rtscts",
                        15 => "xonxoff",
                        _ => "none",
                    },
                    port,
                );
                c
            }
            // DCD/DTR/DSR flow control (17-19) aren't supported by the tty layer: report "none".
            _ => 1,
        }
    }

    /// Call when modem lines change (periodically is fine): sends NOTIFY-MODEMSTATE on a change.
    pub fn notify_modem(&mut self, force: bool, port: &dyn PortHooks) {
        if !self.rfc2217 {
            return;
        }
        let state = modem_byte(port.lines());
        let prev = self.last_modem.unwrap_or(0);
        if !force && Some(state) == self.last_modem {
            return;
        }
        let delta = state ^ prev;
        let mut v = state
            | (if delta & MODEM_CTS != 0 { 1 } else { 0 })
            | (if delta & MODEM_DSR != 0 { 2 } else { 0 })
            | (if delta & MODEM_RI != 0 { 4 } else { 0 })
            | (if delta & MODEM_CD != 0 { 8 } else { 0 });
        self.last_modem = Some(state);
        v &= if force { 0xff } else { self.modem_mask };
        if !force && v == 0 {
            return;
        }
        self.send(sub_frame(107, &[v]));
    }
}

fn sub_frame(code: u8, value: &[u8]) -> Vec<u8> {
    let mut body = vec![OPT_COM_PORT, code];
    body.extend_from_slice(value);
    let body = escape_iac(&body);
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&[IAC, SB]);
    out.extend_from_slice(&body);
    out.extend_from_slice(&[IAC, SE]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::termios::{DEFAULT_CONFIG, normalize_config};

    #[derive(Default)]
    struct Fake {
        cfg: SerialConfig,
        dtr: bool,
        rts: bool,
        cts: bool,
        brk: bool,
        purged: Vec<Queue>,
        suspended: bool,
    }

    impl PortHooks for Fake {
        fn config(&self) -> SerialConfig {
            self.cfg
        }
        fn set_config(&mut self, c: &Value) -> Result<SerialConfig, String> {
            self.cfg = normalize_config(c, &self.cfg)?;
            Ok(self.cfg)
        }
        fn lines(&self) -> Option<ModemLines> {
            Some(ModemLines { dtr: self.dtr, rts: self.rts, cts: self.cts, dsr: false, dcd: true, ri: false })
        }
        fn set_lines(&mut self, dtr: Option<bool>, rts: Option<bool>) {
            if let Some(d) = dtr {
                self.dtr = d;
            }
            if let Some(r) = rts {
                self.rts = r;
            }
        }
        fn break_state(&self) -> bool {
            self.brk
        }
        fn set_break(&mut self, on: bool) {
            self.brk = on;
        }
        fn purge(&mut self, which: Queue) {
            self.purged.push(which);
        }
        fn suspend(&mut self, on: bool) {
            self.suspended = on;
        }
    }

    fn fake() -> (Rfc2217Session, Fake) {
        (Rfc2217Session::new(), Fake { cfg: DEFAULT_CONFIG, dtr: true, rts: true, ..Default::default() })
    }

    fn sub(v: &[u8]) -> Vec<u8> {
        let mut out = vec![IAC, SB, OPT_COM_PORT];
        out.extend_from_slice(v);
        out.extend_from_slice(&[IAC, SE]);
        out
    }

    fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn opening_offer() {
        let (mut s, _) = fake();
        s.start();
        assert_eq!(
            s.take_sent(),
            vec![
                vec![IAC, WILL, OPT_SGA],
                vec![IAC, WILL, OPT_ECHO],
                vec![IAC, WILL, OPT_BINARY],
                vec![IAC, DO, OPT_BINARY],
                vec![IAC, WILL, OPT_COM_PORT]
            ]
        );
    }

    #[test]
    fn pyserials_opening_requests_get_exactly_the_answers_it_needs() {
        let (mut s, mut p) = fake();
        s.start();
        s.take_sent();
        // pyserial: DO ECHO, WILL SGA, DO SGA, WILL COM-PORT, DO COM-PORT, then WILL BINARY / DO BINARY in reply to our offers
        let data = s.feed(
            &[
                IAC,
                DO,
                OPT_ECHO,
                IAC,
                WILL,
                OPT_SGA,
                IAC,
                DO,
                OPT_SGA,
                IAC,
                WILL,
                OPT_COM_PORT,
                IAC,
                DO,
                OPT_COM_PORT,
                IAC,
                WILL,
                OPT_BINARY,
                IAC,
                DO,
                OPT_BINARY,
            ],
            &mut p,
        );
        assert!(data.is_empty());
        let sent = s.take_sent();
        // Only the client's WILLs we hadn't offered as DO need a reply (SGA, COM-PORT); then the forced modem state.
        assert_eq!(sent[0], vec![IAC, DO, OPT_SGA]);
        assert_eq!(sent[1], vec![IAC, DO, OPT_COM_PORT]);
        assert_eq!(sent[2], sub(&[107, 0x88])); // CD set, plus its delta bit on the first report
        assert_eq!(sent.len(), 3);
        assert!(s.rfc2217);
    }

    #[test]
    fn unknown_options_are_refused_once() {
        let (mut s, mut p) = fake();
        s.feed(&[IAC, DO, 24, IAC, WILL, 31, IAC, DO, 24], &mut p);
        assert_eq!(s.take_sent(), vec![vec![IAC, WONT, 24], vec![IAC, DONT, 31]]);
    }

    #[test]
    fn set_baudrate_applies_and_echoes_0_queries() {
        let (mut s, mut p) = fake();
        s.feed(&sub(&[1, 0x00, 0x01, 0x24, 0xf8]), &mut p); // 0x0124f8 = 75000
        assert_eq!(p.cfg.baud, 75000);
        assert_eq!(s.take_sent().pop().unwrap(), sub(&[101, 0x00, 0x01, 0x24, 0xf8]));
        s.feed(&sub(&[1, 0, 0, 0, 0]), &mut p);
        assert_eq!(s.take_sent().pop().unwrap(), sub(&[101, 0x00, 0x01, 0x24, 0xf8]));
    }

    #[test]
    fn datasize_parity_stopsize() {
        let (mut s, mut p) = fake();
        s.feed(&cat(&[sub(&[2, 7]), sub(&[3, 3]), sub(&[4, 2])]), &mut p);
        assert_eq!((p.cfg.data_bits, p.cfg.parity, p.cfg.stop_bits), (7, Parity::Even, 2));
        assert_eq!(s.take_sent(), vec![sub(&[102, 7]), sub(&[103, 3]), sub(&[104, 2])]);
        s.feed(&cat(&[sub(&[3, 5]), sub(&[4, 3])]), &mut p); // space parity; 1.5 stop -> 2
        assert_eq!(s.take_sent(), vec![sub(&[103, 5]), sub(&[104, 2])]);
    }

    #[test]
    fn rejected_setting_answers_with_the_current_value() {
        let (mut s, mut p) = fake();
        s.feed(&sub(&[2, 9]), &mut p);
        assert_eq!(s.take_sent().pop().unwrap(), sub(&[102, 8]));
    }

    #[test]
    fn set_control_dtr_rts_break_flow_and_queries() {
        let (mut s, mut p) = fake();
        s.feed(&cat(&[sub(&[5, 9]), sub(&[5, 11]), sub(&[5, 5]), sub(&[5, 3]), sub(&[5, 7]), sub(&[5, 4]), sub(&[5, 0])]), &mut p);
        assert!(!p.dtr);
        assert!(p.rts);
        assert!(p.brk);
        assert_eq!(p.cfg.flow, Flow::RtsCts);
        assert_eq!(
            s.take_sent(),
            vec![sub(&[105, 9]), sub(&[105, 11]), sub(&[105, 5]), sub(&[105, 3]), sub(&[105, 9]), sub(&[105, 5]), sub(&[105, 3])]
        );
    }

    #[test]
    fn purge_data_and_masks() {
        let (mut s, mut p) = fake();
        s.feed(&cat(&[sub(&[12, 3]), sub(&[11, 0x30]), sub(&[10, 0])]), &mut p);
        assert_eq!(p.purged, vec![Queue::Both]);
        assert_eq!(s.take_sent(), vec![sub(&[112, 3]), sub(&[111, 0x30]), sub(&[110, 0])]);
    }

    #[test]
    fn notify_modemstate_poll_and_change_notifications_with_deltas() {
        let (mut s, mut p) = fake();
        s.feed(&[IAC, WILL, OPT_COM_PORT], &mut p);
        s.take_sent();
        s.feed(&sub(&[7]), &mut p);
        assert_eq!(s.take_sent().pop().unwrap(), sub(&[107, 0x80]));
        p.cts = true;
        s.notify_modem(false, &p);
        assert_eq!(s.take_sent().pop().unwrap(), sub(&[107, 0x90 | 0x01]));
        s.notify_modem(false, &p); // no change, nothing sent
        assert!(s.take_sent().is_empty());
    }

    #[test]
    fn iac_escaping_both_ways_including_inside_a_subnegotiation_value() {
        assert_eq!(escape_iac(&[1, 255, 2, 255]), vec![1, 255, 255, 2, 255, 255]);
        let (mut s, mut p) = fake();
        assert_eq!(s.feed(&[0x41, IAC, IAC, 0x42], &mut p), vec![0x41, 255, 0x42]);
        // baud 0x0000ffff: each 0xff byte arrives doubled
        s.feed(&[IAC, SB, OPT_COM_PORT, 1, 0, 0, 0xff, 0xff, 0xff, 0xff, IAC, SE], &mut p);
        assert_eq!(p.cfg.baud, 65535);
        assert_eq!(s.take_sent().pop().unwrap(), vec![IAC, SB, OPT_COM_PORT, 101, 0, 0, 0xff, 0xff, 0xff, 0xff, IAC, SE]);
    }

    #[test]
    fn parser_survives_frames_split_across_reads() {
        let (mut s, mut p) = fake();
        let mut bytes = vec![0x61];
        bytes.extend(sub(&[1, 0, 0, 0x25, 0x80]));
        bytes.push(0x62);
        let mut out = Vec::new();
        for b in bytes {
            out.extend(s.feed(&[b], &mut p));
        }
        assert_eq!(out, vec![0x61, 0x62]);
        assert_eq!(p.cfg.baud, 9600);
    }

    #[test]
    fn nvt_cr_nul_is_collapsed_only_when_the_client_isnt_in_binary_mode() {
        let (mut a, mut pa) = fake();
        assert_eq!(a.feed(&[13, 0, 0x41], &mut pa), vec![13, 0x41]);
        let (mut b, mut pb) = fake();
        b.start();
        b.feed(&[IAC, WILL, OPT_BINARY], &mut pb);
        assert_eq!(b.feed(&[13, 0, 0x41], &mut pb), vec![13, 0, 0x41]);
    }

    #[test]
    fn flow_control_suspend_resume() {
        let (mut s, mut p) = fake();
        s.feed(&sub(&[8]), &mut p);
        assert!(p.suspended);
        s.feed(&sub(&[9]), &mut p);
        assert!(!p.suspended);
    }

    #[test]
    fn signature_query_and_line_state() {
        let (mut s, mut p) = fake();
        s.feed(&cat(&[sub(&[0]), sub(&[6, 0])]), &mut p);
        let mut sig = vec![100];
        sig.extend_from_slice(b"BenchBob");
        assert_eq!(s.take_sent(), vec![sub(&sig), sub(&[106, 0])]);
    }
}
