//! A minimal DNS client for the DNS check: one A query sent straight to one nameserver
//! (UDP, then TCP if the answer was truncated), so the timing is that server's, not a
//! cache's. What `new Resolver({ timeout: 2000, tries: 1 })` + `resolve4` did in the Bun
//! server (c-ares), with the same error codes.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

/// Build a recursive A/IN query for `name`.
pub fn query(id: u16, name: &str) -> Result<Vec<u8>, &'static str> {
    let mut q = Vec::with_capacity(32 + name.len());
    q.extend(id.to_be_bytes());
    q.extend([0x01, 0x00]); // RD
    q.extend([0, 1, 0, 0, 0, 0, 0, 0]); // QDCOUNT 1
    let name = name.strip_suffix('.').unwrap_or(name);
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err("EBADNAME");
        }
        q.push(label.len() as u8);
        q.extend(label.as_bytes());
    }
    q.push(0);
    q.extend([0, 1, 0, 1]); // A, IN
    Ok(q)
}

fn skip_name(b: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let l = *b.get(i)? as usize;
        if l == 0 {
            return Some(i + 1);
        }
        if l & 0xc0 == 0xc0 {
            b.get(i + 1)?;
            return Some(i + 2);
        }
        i += 1 + l;
    }
}

pub enum Answer {
    Addrs(Vec<String>),
    Truncated,
}

/// Parse an answer to [`query`]: the A records, or the c-ares error code.
pub fn parse(b: &[u8], id: u16) -> Result<Answer, &'static str> {
    if b.len() < 12 || u16::from_be_bytes([b[0], b[1]]) != id {
        return Err("EBADRESP");
    }
    if b[2] & 0x02 != 0 {
        return Ok(Answer::Truncated);
    }
    match b[3] & 0x0f {
        0 => {}
        1 => return Err("EFORMERR"),
        2 => return Err("ESERVFAIL"),
        3 => return Err("ENOTFOUND"),
        4 => return Err("ENOTIMP"),
        5 => return Err("EREFUSED"),
        _ => return Err("EBADRESP"),
    }
    let qd = u16::from_be_bytes([b[4], b[5]]);
    let an = u16::from_be_bytes([b[6], b[7]]);
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(b, i).ok_or("EBADRESP")? + 4;
    }
    let mut out = Vec::new();
    for _ in 0..an {
        i = skip_name(b, i).ok_or("EBADRESP")?;
        let h = b.get(i..i + 10).ok_or("EBADRESP")?;
        let (typ, class, len) =
            (u16::from_be_bytes([h[0], h[1]]), u16::from_be_bytes([h[2], h[3]]), u16::from_be_bytes([h[8], h[9]]) as usize);
        i += 10;
        let rd = b.get(i..i + len).ok_or("EBADRESP")?;
        if typ == 1 && class == 1 && len == 4 {
            out.push(format!("{}.{}.{}.{}", rd[0], rd[1], rd[2], rd[3]));
        }
        i += len;
    }
    if out.is_empty() {
        return Err("ENODATA");
    }
    Ok(Answer::Addrs(out))
}

/// A nameserver from resolv.conf: an IPv4 or IPv6 address, IPv6 with an optional %scope.
pub fn server_addr(s: &str) -> Option<SocketAddr> {
    let (addr, scope) = s.split_once('%').map(|(a, z)| (a, Some(z))).unwrap_or((s, None));
    match addr.parse::<IpAddr>().ok()? {
        IpAddr::V4(v4) => Some(SocketAddr::new(IpAddr::V4(v4), 53)),
        IpAddr::V6(v6) => {
            let scope_id = match scope {
                None => 0,
                Some(z) => z.parse::<u32>().ok().or_else(|| {
                    let c = std::ffi::CString::new(z).ok()?;
                    // SAFETY: a valid NUL-terminated string.
                    let i = unsafe { libc::if_nametoindex(c.as_ptr()) };
                    (i != 0).then_some(i)
                })?,
            };
            Some(SocketAddr::V6(std::net::SocketAddrV6::new(v6, 53, 0, scope_id)))
        }
    }
}

fn io_code(e: &std::io::Error) -> &'static str {
    match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "ECONNREFUSED",
        std::io::ErrorKind::TimedOut => "ETIMEOUT",
        _ => "ECONNREFUSED",
    }
}

fn random_id() -> u16 {
    let mut b = [0u8; 2];
    // SAFETY: getrandom writes at most b.len() bytes.
    let n = unsafe { libc::getrandom(b.as_mut_ptr().cast(), b.len(), 0) };
    if n == 2 { u16::from_ne_bytes(b) } else { (std::process::id() as u16) ^ (benchbob_core::timeline::now_ms() as u16) }
}

async fn udp(server: SocketAddr, q: &[u8], id: u16) -> Result<Answer, &'static str> {
    let bind: SocketAddr = if server.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
    let sock = UdpSocket::bind(bind).await.map_err(|e| io_code(&e))?;
    sock.connect(server).await.map_err(|e| io_code(&e))?;
    sock.send(q).await.map_err(|e| io_code(&e))?;
    let mut buf = vec![0u8; 4096];
    loop {
        let n = sock.recv(&mut buf).await.map_err(|e| io_code(&e))?;
        // Someone else's answer (or a stray): keep waiting for ours.
        if n >= 2 && u16::from_be_bytes([buf[0], buf[1]]) != id {
            continue;
        }
        return parse(&buf[..n], id);
    }
}

async fn tcp(server: SocketAddr, q: &[u8], id: u16) -> Result<Answer, &'static str> {
    let mut s = TcpStream::connect(server).await.map_err(|e| io_code(&e))?;
    let mut msg = (q.len() as u16).to_be_bytes().to_vec();
    msg.extend(q);
    s.write_all(&msg).await.map_err(|e| io_code(&e))?;
    let mut len = [0u8; 2];
    s.read_exact(&mut len).await.map_err(|_| "EBADRESP")?;
    let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
    s.read_exact(&mut buf).await.map_err(|_| "EBADRESP")?;
    parse(&buf, id)
}

/// The IPv4 addresses `server` gives for `name`, within `timeout` (one try, as c-ares with
/// `tries: 1`), or the c-ares error code (ENOTFOUND, ENODATA, ETIMEOUT, ESERVFAIL…).
pub async fn resolve4(server: &str, name: &str, timeout: Duration) -> Result<Vec<String>, String> {
    let addr = server_addr(server).ok_or_else(|| "EBADSTR".to_string())?;
    let id = random_id();
    let q = query(id, name).map_err(str::to_string)?;
    let work = async {
        match udp(addr, &q, id).await? {
            Answer::Addrs(a) => Ok(a),
            Answer::Truncated => match tcp(addr, &q, id).await? {
                Answer::Addrs(a) => Ok(a),
                Answer::Truncated => Err("EBADRESP"),
            },
        }
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(r) => r.map_err(str::to_string),
        Err(_) => Err("ETIMEOUT".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_and_answers() {
        let q = query(0x1234, "example.com.").unwrap();
        assert_eq!(&q[..4], &[0x12, 0x34, 0x01, 0x00]);
        assert_eq!(&q[12..], b"\x07example\x03com\x00\x00\x01\x00\x01");
        assert!(query(1, "a..b").is_err());
        // An answer: the question, a CNAME, then two A records behind compression pointers.
        let mut r = q.clone();
        r[2] = 0x81;
        r[3] = 0x80;
        r[7] = 3;
        r.extend(b"\xc0\x0c\x00\x05\x00\x01\x00\x00\x00\x3c\x00\x06\x03www\xc0\x0c");
        r.extend(b"\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x3c\x00\x04\x5d\xb8\xd7\x0e");
        r.extend(b"\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x3c\x00\x04\x01\x02\x03\x04");
        match parse(&r, 0x1234) {
            Ok(Answer::Addrs(a)) => assert_eq!(a, ["93.184.215.14", "1.2.3.4"]),
            _ => panic!(),
        }
        let mut nx = q.clone();
        nx[2] = 0x81;
        nx[3] = 0x83;
        assert_eq!(parse(&nx, 0x1234).err(), Some("ENOTFOUND"));
        let mut empty = q.clone();
        empty[2] = 0x81;
        empty[3] = 0x80;
        assert_eq!(parse(&empty, 0x1234).err(), Some("ENODATA"));
        assert_eq!(parse(&empty, 0x9999).err(), Some("EBADRESP"));
        let mut tc = q;
        tc[2] = 0x83;
        assert!(matches!(parse(&tc, 0x1234), Ok(Answer::Truncated)));
    }

    #[test]
    fn servers() {
        assert_eq!(server_addr("10.255.0.1").unwrap().to_string(), "10.255.0.1:53");
        assert_eq!(server_addr("2001:db8::1").unwrap().to_string(), "[2001:db8::1]:53");
        assert_eq!(server_addr("fe80::1%3").unwrap().to_string(), "[fe80::1%3]:53");
        assert!(server_addr("nope").is_none());
    }

    #[tokio::test]
    async fn timeout_is_etimeout() {
        // TEST-NET-1 never answers.
        let r = resolve4("192.0.2.1", "example.com", Duration::from_millis(200)).await;
        assert!(matches!(r.as_ref().map_err(String::as_str), Err("ETIMEOUT") | Err("ECONNREFUSED")), "{r:?}");
    }
}
