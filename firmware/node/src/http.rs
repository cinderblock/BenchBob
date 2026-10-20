//! A connection-per-request HTTP/1.1 server over embassy-net. Routing and responses are
//! `benchbob_proto::api`; this only moves bytes. Run several of these tasks to serve
//! requests concurrently (they share the board through a mutex).

use benchbob_proto::api::{Api, Node, Reply, Request};
use benchbob_proto::http::{self, HeadError};
use benchbob_proto::json::Buf;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::Duration;
use embedded_io_async::Write;

/// What the HTTP tasks share: the API state and the board.
pub struct Shared<N> {
    pub api: Api,
    pub node: N,
}

impl<N> Shared<N> {
    pub const fn new(node: N) -> Self {
        Shared { api: Api::new(), node }
    }
}

pub const REQUEST_MAX: usize = 4096;
pub const RESPONSE_MAX: usize = 8192;

/// Buffers for one server task (about 19 KiB), kept in a static by the caller.
pub struct Buffers {
    rx: [u8; 1536],
    tx: [u8; 2048],
    req: [u8; REQUEST_MAX],
    out: [u8; RESPONSE_MAX],
    scratch: [u8; 4096],
}

impl Buffers {
    pub const fn new() -> Self {
        Buffers { rx: [0; 1536], tx: [0; 2048], req: [0; REQUEST_MAX], out: [0; RESPONSE_MAX], scratch: [0; 4096] }
    }
}

impl Default for Buffers {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn serve<M: RawMutex, N: Node>(stack: Stack<'_>, shared: &Mutex<M, Shared<N>>, port: u16, bufs: &mut Buffers) -> ! {
    let Buffers { rx, tx, req, out, scratch } = bufs;
    loop {
        let mut sock = TcpSocket::new(stack, rx, tx);
        sock.set_timeout(Some(Duration::from_secs(10)));
        if sock.accept(port).await.is_err() {
            continue;
        }
        if let Err(e) = connection(&mut sock, shared, req, out, scratch).await {
            warn!("http: {}", e);
        }
        sock.close();
        let _ = sock.flush().await;
    }
}

async fn connection<M: RawMutex, N: Node>(sock: &mut TcpSocket<'_>, shared: &Mutex<M, Shared<N>>, req: &mut [u8], out: &mut [u8], scratch: &mut [u8]) -> Result<(), &'static str> {
    // Read until the head is complete.
    let mut n = 0;
    let (head_len, content_length) = loop {
        if n == req.len() {
            return respond_error(sock, 431, "request head too large").await;
        }
        let got = sock.read(&mut req[n..]).await.map_err(|_| "read failed")?;
        if got == 0 {
            return Err("closed before the request");
        }
        n += got;
        match http::parse_head(&req[..n]) {
            Ok(Some(h)) => break (h.len, h.content_length),
            Ok(None) => {}
            Err(HeadError::BadRequest) => return respond_error(sock, 400, "bad request").await,
        }
    };
    let total = head_len + content_length;
    if total > req.len() {
        return respond_error(sock, 413, "body too large").await;
    }
    while n < total {
        let got = sock.read(&mut req[n..total]).await.map_err(|_| "read failed")?;
        if got == 0 {
            return Err("closed mid-body");
        }
        n += got;
    }
    // Parse again over the full buffer (the head borrows it).
    let h = http::parse_head(&req[..total]).ok().flatten().ok_or("head vanished")?;
    let request = Request { method: h.method, path: h.path, query: h.query, host: h.host, body: &req[head_len..total] };
    let mut body = Buf::new(out);
    let reply: Reply = {
        let mut g = shared.lock().await;
        let Shared { api, node } = &mut *g;
        api.handle(node, &request, &mut body, scratch)
    };
    let mut head_mem = [0u8; 256];
    let mut head = Buf::new(&mut head_mem);
    let mut extra: [(&str, &str); 2] = [("", ""); 2];
    let mut k = 0;
    if reply.mcp {
        extra[k] = ("MCP-Protocol-Version", benchbob_proto::api::MCP_PROTOCOL);
        k += 1;
    }
    if let Some(a) = reply.allow {
        extra[k] = ("Allow", a);
        k += 1;
    }
    http::response_head(reply.status, reply.content_type, body.len(), &extra[..k], &mut head);
    sock.write_all(head.as_bytes()).await.map_err(|_| "write failed")?;
    sock.write_all(body.as_bytes()).await.map_err(|_| "write failed")?;
    sock.flush().await.map_err(|_| "flush failed")
}

async fn respond_error(sock: &mut TcpSocket<'_>, status: u16, msg: &str) -> Result<(), &'static str> {
    let mut mem = [0u8; 384];
    let mut b = Buf::new(&mut mem);
    let mut body_mem = [0u8; 128];
    let mut body = Buf::new(&mut body_mem);
    benchbob_proto::json::Json::new(&mut body).obj().kv_bool("ok", false).kv_str("message", msg).end_obj();
    http::response_head(status, "application/json", body.len(), &[], &mut b);
    b.push(body.as_bytes());
    sock.write_all(b.as_bytes()).await.map_err(|_| "write failed")?;
    sock.flush().await.map_err(|_| "flush failed")
}
