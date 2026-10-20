//! Improv Wi-Fi over serial (https://www.improv-wifi.com/serial/).
//!
//! Packet: `IMPROV`, version (1), type, data length, data, checksum (the low byte of the
//! sum of every byte before it). Devices end each packet with `\n`, as the reference SDK
//! does, so a terminal shows them one per line. Anything that isn't a packet (log text,
//! a stray newline) is skipped by the parser.

pub const HEADER: &[u8; 6] = b"IMPROV";
pub const VERSION: u8 = 1;
/// Largest packet: 9 header bytes, 255 data bytes, checksum, newline.
pub const MAX_PACKET: usize = 9 + 255 + 2;

pub const TYPE_CURRENT_STATE: u8 = 0x01;
pub const TYPE_ERROR_STATE: u8 = 0x02;
pub const TYPE_RPC: u8 = 0x03;
pub const TYPE_RPC_RESULT: u8 = 0x04;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Stopped = 0x00,
    Authorized = 0x02,
    Provisioning = 0x03,
    Provisioned = 0x04,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ErrorCode {
    None = 0x00,
    InvalidRpc = 0x01,
    UnknownRpc = 0x02,
    UnableToConnect = 0x03,
    BadHostname = 0x05,
    Unknown = 0xFF,
}

pub const CMD_WIFI_SETTINGS: u8 = 0x01;
pub const CMD_CURRENT_STATE: u8 = 0x02;
pub const CMD_DEVICE_INFO: u8 = 0x03;
pub const CMD_SCAN: u8 = 0x04;
pub const CMD_HOSTNAME: u8 = 0x05;
pub const CMD_DEVICE_NAME: u8 = 0x06;
pub const CMD_NETWORK_STATE: u8 = 0x07;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command<'a> {
    WifiSettings { ssid: &'a [u8], password: &'a [u8] },
    CurrentState,
    DeviceInfo,
    Scan,
    /// Get (None) or set the hostname.
    Hostname(Option<&'a [u8]>),
    DeviceName(Option<&'a [u8]>),
    NetworkState,
    Unknown(u8),
}

impl Command<'_> {
    pub fn id(&self) -> u8 {
        match self {
            Command::WifiSettings { .. } => CMD_WIFI_SETTINGS,
            Command::CurrentState => CMD_CURRENT_STATE,
            Command::DeviceInfo => CMD_DEVICE_INFO,
            Command::Scan => CMD_SCAN,
            Command::Hostname(_) => CMD_HOSTNAME,
            Command::DeviceName(_) => CMD_DEVICE_NAME,
            Command::NetworkState => CMD_NETWORK_STATE,
            Command::Unknown(c) => *c,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packet<'a> {
    Rpc(Command<'a>),
    /// A well-formed packet of a type devices don't receive (state, error, result).
    Other(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    Checksum,
    Version,
    /// An RPC whose inner lengths don't add up.
    InvalidRpc,
}

/// An optional length-prefixed string (get/set commands).
fn optional(p: &[u8]) -> Result<Option<&[u8]>, ParseError> {
    match p {
        [] => Ok(None),
        [n, rest @ ..] if *n as usize == rest.len() => Ok(Some(rest)),
        _ => Err(ParseError::InvalidRpc),
    }
}

/// Parse the data of an RPC packet: command, length, payload.
pub fn parse_rpc(data: &[u8]) -> Result<Command<'_>, ParseError> {
    if data.len() < 2 || data[1] as usize != data.len() - 2 {
        return Err(ParseError::InvalidRpc);
    }
    let (cmd, p) = (data[0], &data[2..]);
    Ok(match cmd {
        CMD_WIFI_SETTINGS => {
            let sl = *p.first().ok_or(ParseError::InvalidRpc)? as usize;
            let ssid = p.get(1..1 + sl).ok_or(ParseError::InvalidRpc)?;
            let pl = *p.get(1 + sl).ok_or(ParseError::InvalidRpc)? as usize;
            let password = p.get(2 + sl..2 + sl + pl).ok_or(ParseError::InvalidRpc)?;
            Command::WifiSettings { ssid, password }
        }
        CMD_CURRENT_STATE => Command::CurrentState,
        CMD_DEVICE_INFO => Command::DeviceInfo,
        CMD_SCAN => Command::Scan,
        CMD_HOSTNAME => Command::Hostname(optional(p)?),
        CMD_DEVICE_NAME => Command::DeviceName(optional(p)?),
        CMD_NETWORK_STATE => Command::NetworkState,
        other => Command::Unknown(other),
    })
}

/// Byte-at-a-time packet assembler.
pub struct Parser {
    buf: [u8; MAX_PACKET],
    len: usize,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub const fn new() -> Self {
        Parser { buf: [0; MAX_PACKET], len: 0 }
    }

    /// Feed one byte. When it completes a packet, returns it (or why it was rejected).
    pub fn push(&mut self, b: u8) -> Option<Result<Packet<'_>, ParseError>> {
        if self.len < HEADER.len() {
            if b == HEADER[self.len] {
                self.buf[self.len] = b;
                self.len += 1;
            } else {
                // Restart; this byte may begin a new header.
                self.len = 0;
                if b == HEADER[0] {
                    self.buf[0] = b;
                    self.len = 1;
                }
            }
            return None;
        }
        self.buf[self.len] = b;
        self.len += 1;
        if self.len < 9 {
            return None;
        }
        let total = 9 + self.buf[8] as usize + 1;
        if self.len < total {
            return None;
        }
        self.len = 0;
        let pkt = &self.buf[..total];
        let sum = pkt[..total - 1].iter().fold(0u8, |a, &x| a.wrapping_add(x));
        if sum != pkt[total - 1] {
            return Some(Err(ParseError::Checksum));
        }
        if pkt[6] != VERSION {
            return Some(Err(ParseError::Version));
        }
        let data = &self.buf[9..total - 1];
        Some(match self.buf[7] {
            TYPE_RPC => parse_rpc(data).map(Packet::Rpc),
            t => Ok(Packet::Other(t)),
        })
    }
}

/// Write a packet (with checksum and trailing newline) into `out`; returns its length.
/// `out` must hold `data.len() + 11` bytes.
pub fn encode(ty: u8, data: &[u8], out: &mut [u8]) -> usize {
    let n = 9 + data.len();
    out[..6].copy_from_slice(HEADER);
    out[6] = VERSION;
    out[7] = ty;
    out[8] = data.len() as u8;
    out[9..n].copy_from_slice(data);
    out[n] = out[..n].iter().fold(0u8, |a, &x| a.wrapping_add(x));
    out[n + 1] = b'\n';
    n + 2
}

pub fn encode_state(s: State, out: &mut [u8]) -> usize {
    encode(TYPE_CURRENT_STATE, &[s as u8], out)
}

pub fn encode_error(e: ErrorCode, out: &mut [u8]) -> usize {
    encode(TYPE_ERROR_STATE, &[e as u8], out)
}

/// An RPC result: command, total length, then each string length-prefixed. Strings that
/// would overflow the 255-byte packet are cut off (whole strings only).
pub fn encode_result(cmd: u8, strings: &[&str], out: &mut [u8]) -> usize {
    let mut data = [0u8; 255];
    let mut n = 2;
    for s in strings {
        let s = &s.as_bytes()[..s.len().min(255)];
        if n + 1 + s.len() > data.len() {
            break;
        }
        data[n] = s.len() as u8;
        data[n + 1..n + 1 + s.len()].copy_from_slice(s);
        n += 1 + s.len();
    }
    data[0] = cmd;
    data[1] = (n - 2) as u8;
    encode(TYPE_RPC_RESULT, &data[..n], out)
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    fn client_packet(ty: u8, data: &[u8]) -> Vec<u8> {
        let mut out = [0u8; MAX_PACKET];
        let n = encode(ty, data, &mut out);
        out[..n].to_vec()
    }

    fn rpc(cmd: u8, payload: &[u8]) -> Vec<u8> {
        let mut d = std::vec![cmd, payload.len() as u8];
        d.extend_from_slice(payload);
        client_packet(TYPE_RPC, &d)
    }

    #[test]
    fn known_bytes() {
        // Request current state, as the SDK sends it: IMPROV 01 03 02 02 00 cs \n
        let p = rpc(CMD_CURRENT_STATE, &[]);
        let sum = b"IMPROV".iter().map(|&b| b as u32).sum::<u32>() + 1 + 3 + 2 + 2;
        assert_eq!(p, [b"IMPROV".as_slice(), &[1, 3, 2, 2, 0, sum as u8, b'\n']].concat());
    }

    #[test]
    fn parses_wifi_settings_amid_noise() {
        let mut stream = b"boot log line\nIMPR".to_vec();
        stream.extend(rpc(CMD_WIFI_SETTINGS, &[3, b'l', b'a', b'b', 2, b'p', b'w']));
        let mut p = Parser::new();
        let mut got = None;
        for b in stream {
            if let Some(Ok(Packet::Rpc(Command::WifiSettings { ssid, password }))) = p.push(b) {
                got = Some((ssid.to_vec(), password.to_vec()));
            }
        }
        assert_eq!(got, Some((b"lab".to_vec(), b"pw".to_vec())));
    }

    #[test]
    fn rejects_bad_checksum_and_rpc() {
        let mut bad = rpc(CMD_DEVICE_INFO, &[]);
        let last = bad.len() - 2;
        bad[last] ^= 1;
        let mut p = Parser::new();
        let r: Vec<_> = bad.iter().filter_map(|&b| p.push(b).map(|r| r.map(|_| ()))).collect();
        assert_eq!(r, [Err(ParseError::Checksum)]);
        // Inner length disagrees with the packet length.
        assert_eq!(parse_rpc(&[CMD_WIFI_SETTINGS, 3, 9, b'a', 0]), Err(ParseError::InvalidRpc));
        assert_eq!(parse_rpc(&[CMD_WIFI_SETTINGS, 2, 5, b'a']), Err(ParseError::InvalidRpc));
        assert_eq!(parse_rpc(&[CMD_HOSTNAME, 0]), Ok(Command::Hostname(None)));
        assert_eq!(parse_rpc(&[CMD_HOSTNAME, 3, 2, b'h', b'i']), Ok(Command::Hostname(Some(b"hi"))));
        assert_eq!(parse_rpc(&[0x42, 0]), Ok(Command::Unknown(0x42)));
    }

    #[test]
    fn result_encoding() {
        let mut out = [0u8; MAX_PACKET];
        let n = encode_result(CMD_DEVICE_INFO, &["BenchBob node", "0.1.0", "RP2350", "bb"], &mut out);
        let p = &out[..n];
        // 4 strings: 14 + 6 + 7 + 3 = 30 bytes, plus command and length.
        assert_eq!(&p[..9], b"IMPROV\x01\x04\x20");
        assert_eq!(&p[9..12], &[CMD_DEVICE_INFO, 30, 13]);
        assert_eq!(&p[12..25], b"BenchBob node");
        assert_eq!(p[n - 1], b'\n');
        let sum = p[..n - 2].iter().fold(0u8, |a, &x| a.wrapping_add(x));
        assert_eq!(p[n - 2], sum);
        // An empty result (end of a scan list).
        let n = encode_result(CMD_SCAN, &[], &mut out);
        assert_eq!(&out[7..11], &[TYPE_RPC_RESULT, 2, CMD_SCAN, 0]);
        assert_eq!(n, 13);
    }
}
