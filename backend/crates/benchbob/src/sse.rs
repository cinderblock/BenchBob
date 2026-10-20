//! `/api/events`: Server-Sent Events. Every client gets the full status every 2 s and a
//! `periph` event when a driver changes.
//!
//! Gzipped where the browser accepts it: one gzip stream per client, sync-flushed after
//! every message so each arrives at once, and repeats of a nearly identical ~20 KB status
//! cost a few hundred bytes on the wire instead of the whole thing every 2 s.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{HeaderValue, header};
use axum::response::Response;
use bytes::Bytes;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::Value;
use tokio::sync::mpsc;

struct Client {
    id: u64,
    tx: mpsc::UnboundedSender<Bytes>,
    gz: Option<GzEncoder<Vec<u8>>>,
}

impl Client {
    fn send(&mut self, msg: &[u8]) -> bool {
        let out = match &mut self.gz {
            None => Bytes::copy_from_slice(msg),
            Some(e) => {
                if e.write_all(msg).is_err() || e.flush().is_err() {
                    return false;
                }
                Bytes::from(std::mem::take(e.get_mut()))
            }
        };
        self.tx.send(out).is_ok()
    }
}

#[derive(Default)]
pub struct Sse {
    clients: Mutex<Vec<Client>>,
    next: AtomicU64,
}

/// One SSE message: `event: name` (if any) and `data: <json>`.
pub fn message(data: &Value, event: Option<&str>) -> Vec<u8> {
    let mut s = String::new();
    if let Some(e) = event {
        s.push_str("event: ");
        s.push_str(e);
        s.push('\n');
    }
    s.push_str("data: ");
    s.push_str(&serde_json::to_string(data).unwrap_or_default());
    s.push_str("\n\n");
    s.into_bytes()
}

impl Sse {
    pub fn count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    /// Add a client; returns its id and the response that streams to it.
    pub fn subscribe(&self, gzip: bool) -> (u64, Response) {
        let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let gz = gzip.then(|| GzEncoder::new(Vec::new(), Compression::default()));
        self.clients.lock().unwrap().push(Client { id, tx, gz });
        let stream = futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx).map(|o| o.map(Ok::<Bytes, std::io::Error>)));
        let mut r = Response::new(Body::from_stream(stream));
        let h = r.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        h.insert("X-Accel-Buffering", HeaderValue::from_static("no"));
        if gzip {
            h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        }
        (id, r)
    }

    /// Send to one client (its first status).
    pub fn send_to(&self, id: u64, data: &Value, event: Option<&str>) {
        let msg = message(data, event);
        let mut c = self.clients.lock().unwrap();
        if let Some(i) = c.iter().position(|x| x.id == id)
            && !c[i].send(&msg)
        {
            c.remove(i);
        }
    }

    /// Send to everyone; clients whose connection is gone are dropped.
    pub fn broadcast(&self, data: &Value, event: Option<&str>) {
        let msg = message(data, event);
        self.clients.lock().unwrap().retain_mut(|c| c.send(&msg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[tokio::test]
    async fn gzip_stream_decodes_message_by_message() {
        let sse = Sse::default();
        let (id, r) = sse.subscribe(true);
        sse.send_to(id, &serde_json::json!({ "a": 1 }), None);
        sse.broadcast(&serde_json::json!({ "b": 2 }), Some("periph"));
        assert_eq!(sse.count(), 1);
        drop(r);
        // The receiver is gone: the next broadcast drops the client.
        sse.broadcast(&serde_json::json!({}), None);
        assert_eq!(sse.count(), 0);
    }

    #[tokio::test]
    async fn each_flush_is_decodable_on_its_own() {
        let sse = Sse::default();
        let (id, r) = sse.subscribe(true);
        sse.send_to(id, &serde_json::json!({ "a": 1 }), None);
        sse.broadcast(&serde_json::json!({ "b": 2 }), Some("periph"));
        drop(sse);
        let body = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        let mut d = flate2::read::MultiGzDecoder::new(&body[..]);
        let mut s = String::new();
        // No gzip trailer (the stream never ended): read what was flushed.
        let _ = d.read_to_string(&mut s);
        assert_eq!(s, "data: {\"a\":1}\n\nevent: periph\ndata: {\"b\":2}\n\n");
    }
}
