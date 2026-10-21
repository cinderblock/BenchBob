//! HTTP to a ustreamer child: over its private unix socket, or TCP when it's shared.
//! One connection per request, as `fetch` did; dropping a streamed body hangs up, which is
//! what lets ustreamer count the viewer gone.

use std::path::PathBuf;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Clone, Debug)]
pub enum Addr {
    Unix(PathBuf),
    Tcp(u16),
}

fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

async fn handshake<S>(s: S) -> std::io::Result<SendRequest<Empty<Bytes>>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(s)).await.map_err(io_err)?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(sender)
}

pub async fn get(addr: &Addr, path: &str) -> std::io::Result<Response<Incoming>> {
    let mut sender = match addr {
        Addr::Unix(p) => handshake(tokio::net::UnixStream::connect(p).await?).await?,
        Addr::Tcp(port) => handshake(tokio::net::TcpStream::connect(("127.0.0.1", *port)).await?).await?,
    };
    let req = Request::get(path).header(hyper::header::HOST, "localhost").body(Empty::<Bytes>::new()).map_err(io_err)?;
    sender.send_request(req).await.map_err(io_err)
}

/// GET and read the whole body: (status, bytes).
pub async fn get_all(addr: &Addr, path: &str) -> std::io::Result<(u16, Bytes)> {
    let r = get(addr, path).await?;
    let status = r.status().as_u16();
    let body = r.into_body().collect().await.map_err(io_err)?.to_bytes();
    Ok((status, body))
}
