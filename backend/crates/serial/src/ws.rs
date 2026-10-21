//! /ws/serial/:id: the live port. Binary frames are raw bytes both ways; JSON text frames
//! are control and status (serial.ts "WebSocket").

use std::sync::Arc;

use benchbob_core::http::{js_number, js_string, truthy};
use benchbob_core::timeline::now_ms;
use benchbob_core::{HttpError, WsIn, WsSession};
use serde_json::{Value, json};

use crate::Inner;
use crate::port::{Lock, Port, WsEntry, http};
use crate::ring::RING_CAP;
use crate::util::base64_encode;

pub const WS_DOC: &str = "Live serial port. Binary frames are raw bytes both ways: what you send is written to the port, what you receive was read from it. Text frames from the server are JSON: {type:'hello', port, cursor}, {type:'history', chunks:[[t, base64]...]}, {type:'status', port}, {type:'lines', lines:{dtr,rts,cts,dsr,dcd,ri}}, {type:'flash', job, line?}, {type:'error', message}, {type:'result', for, ...}. Text frames to the server that are a JSON object with a `type` are control: {type:'write', text|hex|base64, eol?}, {type:'config', baud?, dataBits?, parity?, stopBits?, flow?, save?}, {type:'lines', dtr?, rts?}, {type:'break', ms?}, {type:'reset', mode:'esp'|'esp-boot'|'dtr'|'touch1200'}, {type:'open'}, {type:'close'}, {type:'lock', owner?}, {type:'unlock'}, {type:'status'}. Any other text frame is written to the port as UTF-8 (handy with websocat). Query: history=<bytes> of recent rx to replay first (default 65536; 0 for none), ts=1 to get history as JSON with timestamps instead of one binary frame, quiet=1 for no JSON at all (pure byte pipe), open=1 to open the port for as long as this connection lasts.";

fn error_msg(m: &str) -> String {
    json!({ "type": "error", "message": m }).to_string()
}

/// A JSON line value as JavaScript's `lines.dtr !== undefined ? !!lines.dtr : leave`.
pub(crate) fn line_arg(v: Option<&Value>) -> Option<bool> {
    v.map(truthy)
}

pub(crate) async fn session(inner: Arc<Inner>, mut s: WsSession) {
    let id = s.params.get("id").cloned().unwrap_or_default();
    let Some(p) = inner.port(&id) else {
        s.client.send(error_msg(&format!("No serial port \"{id}\"")));
        s.client.close_with(4004, "no such port");
        return;
    };
    let q = |k: &str| s.query.iter().rev().find(|(x, _)| x == k).map(|(_, v)| v.clone());
    let quiet = q("quiet").as_deref() == Some("1");
    let cid = s.client.id();
    let open = q("open").as_deref() == Some("1");
    {
        let mut st = p.lock();
        st.ws.push(WsEntry { client: s.client.clone(), quiet, lease: false, token: String::new() });
        if !quiet {
            s.client.send(json!({ "type": "hello", "port": inner.port_status(&p, &st, None), "cursor": st.ring.end }).to_string());
        }
        let h = match q("history") {
            None => 65536.0,
            Some(v) => js_number(&Value::String(v)),
        };
        let hist = if h.is_nan() { 0.0 } else { h.clamp(0.0, RING_CAP as f64) } as usize;
        if hist > 0 {
            let chunks = st.ring.tail(hist);
            if q("ts").as_deref() == Some("1") && !quiet {
                let list: Vec<Value> = chunks.iter().map(|(t, d)| json!([t, base64_encode(d)])).collect();
                s.client.send(json!({ "type": "history", "chunks": list }).to_string());
            } else if !chunks.is_empty() {
                s.client.send_binary(chunks.iter().flat_map(|c| c.1.iter().copied()).collect::<Vec<u8>>());
            }
        }
        if open {
            if let Some(w) = st.ws.iter_mut().find(|w| w.client.id() == cid) {
                w.lease = true;
            }
            st.leases += 1;
            if let Err(e) = inner.ensure_open_locked(&p, &mut st) {
                s.client.send(error_msg(&e.message));
            }
        }
    }
    inner.changed();

    while let Some(m) = s.incoming.recv().await {
        let token = || p.lock().ws.iter().find(|w| w.client.id() == cid).map(|w| w.token.clone()).unwrap_or_default();
        let fail = |e: HttpError| {
            s.client.send(error_msg(&e.message));
        };
        let text = match m {
            WsIn::Binary(b) => {
                if let Err(e) = inner.write(&p, b.to_vec(), Some(&token())) {
                    fail(e);
                }
                continue;
            }
            WsIn::Text(t) => t,
        };
        let msg = serde_json::from_str::<Value>(&text).ok().filter(|j| j.get("type").is_some_and(Value::is_string));
        let Some(m) = msg else {
            if let Err(e) = inner.write(&p, text.into_bytes(), Some(&token())) {
                fail(e);
            }
            continue;
        };
        let ty = m["type"].as_str().unwrap_or("").to_string();
        let ok = |extra: Value| {
            let mut r = json!({ "type": "result", "for": ty, "ok": true });
            if let (Value::Object(o), Value::Object(e)) = (&mut r, extra) {
                o.extend(e);
            }
            s.client.send(r.to_string());
        };
        let r: Result<(), HttpError> = match ty.as_str() {
            "write" => crate::util::payload(&m).and_then(|d| inner.write(&p, d, Some(&token()))),
            "config" => {
                inner.apply_config(&p, &m, m.get("save") != Some(&Value::Bool(false))).map(|c| ok(json!({ "config": c.to_json() })))
            }
            "lines" => inner.set_lines(&p, line_arg(m.get("dtr")), line_arg(m.get("rts"))),
            "break" | "reset" => {
                // These take a while: run them aside so the byte stream keeps flowing.
                let (inner, p, client, m, ty) = (inner.clone(), p.clone(), s.client.clone(), m.clone(), ty.clone());
                tokio::spawn(async move {
                    let r = if ty == "break" {
                        let ms = js_number(m.get("ms").unwrap_or(&Value::Null));
                        inner.send_break(&p, if ms == 0.0 || ms.is_nan() { 250.0 } else { ms }).await
                    } else {
                        inner.reset(&p, m.get("mode")).await.map(|_| {
                            client.send(json!({ "type": "result", "for": "reset", "ok": true }).to_string());
                        })
                    };
                    if let Err(e) = r {
                        client.send(error_msg(&e.message));
                    }
                });
                Ok(())
            }
            "open" => {
                p.lock().want = true;
                inner.ensure_open(&p)
            }
            "close" => {
                let mut st = p.lock();
                st.want = false;
                inner.close_fd_locked(&p, &mut st, "closed");
                Ok(())
            }
            "lock" => lock(&inner, &p, cid, &m),
            "unlock" => {
                let mut st = p.lock();
                let mine = st.ws.iter().find(|w| w.client.id() == cid).map(|w| w.token.clone()).unwrap_or_default();
                if st.lock.as_ref().is_some_and(|l| l.token == mine || m.get("force").is_some_and(truthy)) {
                    st.lock = None;
                }
                inner.notify(&p, &st);
                Ok(())
            }
            "status" => {
                let st = p.lock();
                s.client.send(json!({ "type": "status", "port": inner.port_status(&p, &st, None) }).to_string());
                Ok(())
            }
            other => Err(HttpError::new(500, format!("Unknown message type \"{other}\""))),
        };
        if let Err(e) = r {
            fail(e);
        }
    }

    let mut st = p.lock();
    let Some(i) = st.ws.iter().position(|w| w.client.id() == cid) else { return };
    let w = st.ws.remove(i);
    if st.lock.as_ref().is_some_and(|l| l.token == w.token) {
        st.lock = None;
    }
    if w.lease {
        inner.release(&p, &mut st);
    }
    inner.notify(&p, &st);
}

fn lock(inner: &Inner, p: &Port, cid: u64, m: &Value) -> Result<(), HttpError> {
    let mut st = p.lock();
    let mine = st.ws.iter().find(|w| w.client.id() == cid).map(|w| w.token.clone()).unwrap_or_default();
    if let Some(l) = &st.lock
        && l.token != mine
    {
        return Err(http(423, format!("Locked by {}", l.owner)));
    }
    let token = uuid::Uuid::new_v4().to_string();
    let owner = match m.get("owner").filter(|v| truthy(v)) {
        Some(o) => js_string(o),
        None => "a dashboard tab".into(),
    };
    st.lock = Some(Lock { owner, token: token.clone(), since: now_ms() });
    if let Some(w) = st.ws.iter_mut().find(|w| w.client.id() == cid) {
        w.token = token;
    }
    inner.notify(p, &st);
    Ok(())
}
