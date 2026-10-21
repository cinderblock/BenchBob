//! One monitoring socket plus aggregates per interface (can.ts's `Hub`): the per-ID table,
//! the frame ring, decoded errors, rates, WebSocket clients, captures, periodic jobs and the
//! candump recording.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use benchbob_core::{HttpError, WsClient};
use indexmap::IndexMap;
use serde_json::Value;
use tokio::sync::oneshot;

use crate::frame::{self, CanFrame, DecodedError, IdFilter};
use crate::json::{O, num};
use crate::sock::CanSocket;
use crate::{Iface, Task};

pub const RING_MAX: usize = 2000;
pub const ID_ROWS_MAX: usize = 512;
pub const ERRS_MAX: usize = 50;
pub const REC_MAX_BYTES: u64 = 256 * 1024 * 1024; // a runaway recording must not fill the root fs again

/// The wire shape used by the WebSocket, captures and the log. ts = Unix seconds.
#[derive(Clone, Debug)]
pub struct OutFrame {
    pub id: u32,
    pub ext: bool,
    pub rtr: bool,
    pub fd: bool,
    pub brs: bool,
    pub esi: bool,
    pub err: bool,
    pub len: u8,
    pub data: String,
    pub ts: f64,
    pub tx: bool,
    /// Decoded error.
    pub text: Option<String>,
}

/// A JavaScript number for serde: integral values as integers, NaN as null.
pub struct JsNum(pub f64);

impl serde::Serialize for JsNum {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let f = self.0;
        if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
            s.serialize_i64(f as i64)
        } else if f.is_finite() {
            s.serialize_f64(f)
        } else {
            s.serialize_none()
        }
    }
}

/// `{ type, ...frame }`: a frame with a leading type tag (the WebSocket's `frame` message).
pub struct Tagged<'a>(pub &'static str, pub &'a OutFrame);

impl OutFrame {
    fn fields<S: serde::ser::SerializeStruct>(&self, st: &mut S) -> Result<(), S::Error> {
        st.serialize_field("id", &self.id)?;
        st.serialize_field("ext", &self.ext)?;
        st.serialize_field("rtr", &self.rtr)?;
        st.serialize_field("fd", &self.fd)?;
        st.serialize_field("brs", &self.brs)?;
        st.serialize_field("len", &self.len)?;
        st.serialize_field("data", &self.data)?;
        st.serialize_field("ts", &JsNum(self.ts))?;
        st.serialize_field("dir", if self.tx { "tx" } else { "rx" })?;
        // toOut() adds these after the fact, so they come last.
        if self.esi {
            st.serialize_field("esi", &true)?;
        }
        if self.err {
            st.serialize_field("err", &true)?;
        }
        if let Some(t) = &self.text {
            st.serialize_field("text", t)?;
        }
        Ok(())
    }

    pub fn json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

impl serde::Serialize for OutFrame {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("OutFrame", 12)?;
        self.fields(&mut st)?;
        st.end()
    }
}

impl serde::Serialize for Tagged<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Frame", 13)?;
        st.serialize_field("type", self.0)?;
        self.1.fields(&mut st)?;
        st.end()
    }
}

/// One row of the per-ID table, as idRows() makes it.
#[derive(serde::Serialize)]
pub struct IdRowJson {
    pub id: u32,
    pub ext: bool,
    pub fd: bool,
    pub rtr: bool,
    pub len: u8,
    pub count: u64,
    pub period: JsNum,
    pub data: String,
    pub hot: u32,
    pub last: JsNum,
    pub dir: &'static str,
}

#[derive(serde::Serialize)]
pub struct Berr<'a> {
    pub tx: &'a Value,
    pub rx: &'a Value,
}

/// snapshot(): the stats view of a hub, optionally tagged `type: "stats"`.
#[derive(serde::Serialize)]
pub struct Snap<'a> {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub ty: Option<&'static str>,
    pub iface: &'a str,
    pub up: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub berr: Option<Berr<'a>>,
    pub rates: Value,
    pub totals: Value,
    /// Frames the kernel dropped because we fell behind.
    pub drops: u32,
    #[serde(rename = "idCount")]
    pub id_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<IdRowJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<Value>>,
    pub jobs: Vec<Value>,
    pub recording: Value,
}

pub fn to_out(f: &CanFrame) -> OutFrame {
    // The kernel flags our own echoed frames (MSG_CONFIRM): they made it onto the bus.
    OutFrame {
        id: f.id,
        ext: f.ext,
        rtr: f.rtr,
        fd: f.fd,
        brs: f.brs,
        esi: f.esi,
        err: f.err,
        len: f.len,
        data: frame::to_hex(&f.data),
        ts: (f.ts * 1000.0 + 0.5).floor() / 1e6,
        tx: f.own,
        text: None,
    }
}

pub struct IdRow {
    pub id: u32,
    pub ext: bool,
    pub fd: bool,
    pub rtr: bool,
    pub len: u8,
    pub count: u64,
    /// ms
    pub last: f64,
    /// ms, smoothed
    pub period: f64,
    pub data: Vec<u8>,
    pub changed_at: Vec<f64>,
    pub tx: bool,
}

pub struct Job {
    pub id: u64,
    pub frame: CanFrame,
    pub period_ms: f64,
    pub sent: u64,
    pub errors: u64,
    pub last_err: Option<String>,
    pub since: u64,
    pub _task: Task,
}

impl Job {
    pub fn json(&self) -> Value {
        O::new()
            .k("id", self.id)
            .k("frame", frame::candump_body(&self.frame))
            .n("periodMs", self.period_ms)
            .k("sent", self.sent)
            .k("errors", self.errors)
            .o("lastErr", self.last_err.clone())
            .k("since", self.since)
            .v()
    }
}

pub struct Capture {
    pub filters: Option<Vec<IdFilter>>,
    pub frames: Vec<OutFrame>,
    pub max: usize,
    pub done: Option<oneshot::Sender<()>>,
}

pub struct WsState {
    pub client: WsClient,
    pub filters: Option<Vec<IdFilter>>,
    pub frames: bool,
    pub batch: bool,
    pub stats: bool,
    pub q: Vec<OutFrame>,
    pub dropped: u64,
}

pub struct Rec {
    pub file: String,
    pub path: PathBuf,
    pub writer: std::io::BufWriter<std::fs::File>,
    pub frames: u64,
    pub bytes: u64,
    pub since: u64,
}

#[derive(Clone, Copy, Default)]
pub struct Totals {
    pub rx: u64,
    pub tx: u64,
    pub err: u64,
}

impl Totals {
    pub fn json(&self) -> Value {
        O::new().k("rx", self.rx).k("tx", self.tx).k("err", self.err).v()
    }
}

#[derive(Clone, Copy, Default)]
pub struct Rates {
    pub fps: f64,
    pub txps: f64,
    pub errps: f64,
    pub load: Option<f64>,
}

impl Rates {
    pub fn json(&self) -> Value {
        O::new().n("fps", self.fps).n("txps", self.txps).n("errps", self.errps).k("load", self.load.map(num).unwrap_or(Value::Null)).v()
    }
}

pub struct Window {
    pub rx: u64,
    pub tx: u64,
    pub err: u64,
    pub bus_time: f64,
    pub t: u64,
}

pub struct Hub {
    pub name: String,
    pub sock: Option<CanSocket>,
    /// Bumped on every open, so a stale reader's close callback is ignored.
    pub sock_gen: u64,
    pub open_err: String,
    /// By (ext, id): frameKey() without building a string per frame.
    pub ids: HashMap<(bool, u32), IdRow>,
    pub ring: Vec<OutFrame>,
    pub errors: Vec<(DecodedError, String)>,
    pub tot: Totals,
    pub win: Window,
    pub rates: Rates,
    pub clients: IndexMap<u64, WsState>,
    pub captures: HashMap<u64, Capture>,
    pub jobs: IndexMap<u64, Job>,
    pub rec: Option<Rec>,
}

fn now() -> u64 {
    benchbob_core::timeline::now_ms()
}

pub fn decoded_json(d: &DecodedError, iface: &str) -> Value {
    O::new()
        .k("ts", num(d.ts))
        .k("classes", d.classes.clone())
        .k("text", d.text.clone())
        .k("busOff", d.bus_off)
        .o("tec", d.tec)
        .o("rec", d.rec)
        .k("iface", iface)
        .v()
}

impl Hub {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            sock: None,
            sock_gen: 0,
            open_err: String::new(),
            ids: HashMap::new(),
            ring: Vec::new(),
            errors: Vec::new(),
            tot: Totals::default(),
            win: Window { rx: 0, tx: 0, err: 0, bus_time: 0.0, t: now() },
            rates: Rates::default(),
            clients: IndexMap::new(),
            captures: HashMap::new(),
            jobs: IndexMap::new(),
            rec: None,
        }
    }

    pub fn close_sock(&mut self) {
        self.sock = None;
    }

    pub fn drops(&self) -> u32 {
        self.sock.as_ref().map(|s| s.drops()).unwrap_or(0)
    }

    pub fn send(&self, i: Option<&Iface>, f: &CanFrame) -> Result<(), HttpError> {
        let Some(i) = i else { return Err(HttpError::new(404, format!("No CAN interface {}", self.name))) };
        let Some(sock) = self.sock.as_ref().filter(|_| i.up) else { return Err(HttpError::new(409, format!("{} is down", self.name))) };
        if f.fd && !i.fd_on {
            return Err(HttpError::bad_request(format!(
                "{} is not in CAN FD mode{}",
                self.name,
                if i.fd_capable { " (enable FD in its config)" } else { " (the controller doesn't support FD)" }
            )));
        }
        if i.listen_only() {
            return Err(HttpError::new(409, format!("{} is listen-only", self.name)));
        }
        sock.send(f).map_err(|e| HttpError::new(503, e))
    }

    /// Returns the reason when a recording had to stop (size limit).
    pub fn on_frames(&mut self, i: Option<&Iface>, frames: Vec<CanFrame>) -> Option<(String, u64)> {
        let now = now() as f64;
        let mut rec_stop = None;
        for f in &frames {
            let mut o = to_out(f);
            if f.err {
                self.tot.err += 1;
                self.win.err += 1;
                let d = frame::decode_error(f);
                o.text = Some(d.text.clone());
                self.errors.push((d, self.name.clone()));
                if self.errors.len() > ERRS_MAX {
                    let n = self.errors.len() - ERRS_MAX;
                    self.errors.drain(..n);
                }
            } else {
                if o.tx {
                    self.tot.tx += 1;
                    self.win.tx += 1;
                } else {
                    self.tot.rx += 1;
                    self.win.rx += 1;
                }
                if let Some(br) = i.and_then(|i| i.bitrate) {
                    self.win.bus_time += frame::frame_time(f, br, i.and_then(|i| i.dbitrate));
                }
                self.update_id(f, o.tx, now);
            }
            if self.rec.is_some() && rec_stop.is_none() {
                rec_stop = self.record(f);
            }
            for c in self.captures.values_mut() {
                if c.frames.len() < c.max && frame::match_filters(f.id, f.ext, f.err, c.filters.as_deref()) {
                    c.frames.push(o.clone());
                    if c.frames.len() >= c.max
                        && let Some(d) = c.done.take()
                    {
                        let _ = d.send(());
                    }
                }
            }
            for ws in self.clients.values_mut() {
                // Errors pass only when unfiltered.
                if !ws.frames || !frame::match_filters(f.id, f.ext, f.err, ws.filters.as_deref()) {
                    continue;
                }
                if ws.batch {
                    if ws.q.len() < 1000 {
                        ws.q.push(o.clone());
                    } else {
                        ws.dropped += 1;
                    }
                } else if ws.client.buffered() < 1 << 20 {
                    ws.client.send(serde_json::to_string(&Tagged("frame", &o)).unwrap());
                } else {
                    ws.dropped += 1;
                }
            }
            self.ring.push(o);
        }
        if self.ring.len() > RING_MAX * 3 / 2 {
            let n = self.ring.len() - RING_MAX;
            self.ring.drain(..n);
        }
        rec_stop
    }

    fn update_id(&mut self, f: &CanFrame, tx: bool, now: f64) {
        let k = (f.ext, f.id);
        let r = match self.ids.get_mut(&k) {
            Some(r) => {
                let dt = f.ts - r.last;
                r.period = if r.period != 0.0 { r.period * 0.8 + dt * 0.2 } else { dt };
                let n = r.data.len().max(f.data.len());
                if r.changed_at.len() < n {
                    r.changed_at.resize(n, 0.0);
                }
                for b in 0..n {
                    if r.data.get(b) != f.data.get(b) {
                        r.changed_at[b] = now;
                    }
                }
                r.changed_at.truncate(f.data.len());
                r.data = f.data.clone();
                r
            }
            None => {
                if self.ids.len() >= 8192 {
                    return; // fuzzing/random ids: the table stops growing, counters don't
                }
                self.ids.insert(
                    k,
                    IdRow {
                        id: f.id,
                        ext: f.ext,
                        fd: f.fd,
                        rtr: f.rtr,
                        len: f.len,
                        count: 0,
                        last: 0.0,
                        period: 0.0,
                        data: f.data.clone(),
                        changed_at: vec![0.0; f.data.len()],
                        tx,
                    },
                );
                self.ids.get_mut(&k).unwrap()
            }
        };
        r.count += 1;
        r.last = f.ts;
        r.len = f.len;
        r.fd = f.fd;
        r.rtr = f.rtr;
        r.tx = tx;
    }

    pub fn tick(&mut self, i: Option<&Iface>) {
        let now = now();
        let dt = (now.saturating_sub(self.win.t) as f64 / 1000.0).max(0.2);
        let round = |x: f64| (x + 0.5).floor();
        self.rates = Rates {
            fps: round((self.win.rx + self.win.tx) as f64 / dt),
            txps: round(self.win.tx as f64 / dt),
            errps: round(self.win.err as f64 / dt),
            load: i.and_then(|i| i.bitrate).map(|_| (round(self.win.bus_time / dt * 1000.0) / 10.0).min(100.0)),
        };
        self.win = Window { rx: 0, tx: 0, err: 0, bus_time: 0.0, t: now };
        if let Some(r) = &mut self.rec {
            let _ = r.writer.flush();
        }
    }

    pub fn clear(&mut self) {
        self.ids.clear();
        self.ring.clear();
        self.errors.clear();
        self.tot = Totals::default();
    }

    /// The per-ID table, standard ids first, then by id.
    pub fn id_rows(&self, limit: usize) -> Vec<IdRowJson> {
        let now = now() as f64;
        let mut rows: Vec<&IdRow> = self.ids.values().collect();
        rows.sort_unstable_by_key(|r| (r.ext, r.id));
        rows.into_iter()
            .take(limit)
            .map(|r| {
                let mut hot: u32 = 0;
                for b in 0..r.changed_at.len().min(31) {
                    if now - r.changed_at[b] < 1000.0 {
                        hot |= 1 << b;
                    }
                }
                IdRowJson {
                    id: r.id,
                    ext: r.ext,
                    fd: r.fd,
                    rtr: r.rtr,
                    len: r.len,
                    count: r.count,
                    period: JsNum((r.period * 10.0 + 0.5).floor() / 10.0),
                    data: frame::to_hex(&r.data),
                    hot,
                    last: JsNum((r.last + 0.5).floor()),
                    dir: if r.tx { "tx" } else { "rx" },
                }
            })
            .collect()
    }

    pub fn rec_json(&self) -> Value {
        match &self.rec {
            Some(r) => O::new().k("file", r.file.clone()).k("frames", r.frames).k("bytes", r.bytes).k("since", r.since).v(),
            None => Value::Null,
        }
    }

    pub fn snap<'a>(&'a self, i: Option<&'a Iface>, full: bool, ty: Option<&'static str>) -> Snap<'a> {
        let n = self.errors.len().saturating_sub(20);
        Snap {
            ty,
            iface: &self.name,
            up: i.is_some_and(|i| i.up),
            state: i.and_then(|i| i.state.as_ref()),
            berr: i.and_then(|i| i.berr.as_ref()).map(|(tx, rx)| Berr { tx, rx }),
            rates: self.rates.json(),
            totals: self.tot.json(),
            drops: self.drops(),
            id_count: self.ids.len(),
            ids: full.then(|| self.id_rows(ID_ROWS_MAX)),
            errors: full.then(|| self.errors[n..].iter().map(|(d, i)| decoded_json(d, i)).collect()),
            jobs: self.jobs.values().map(Job::json).collect(),
            recording: self.rec_json(),
        }
    }

    pub fn snapshot(&self, i: Option<&Iface>, full: bool) -> Value {
        serde_json::to_value(self.snap(i, full, None)).unwrap_or(Value::Null)
    }

    // ----- recording (candump -L format) -----

    /// Returns (reason, frames) when the size cap stopped it.
    fn record(&mut self, f: &CanFrame) -> Option<(String, u64)> {
        let r = self.rec.as_mut()?;
        let line = frame::candump_line(f, &self.name) + "\n";
        let _ = r.writer.write_all(line.as_bytes());
        r.frames += 1;
        r.bytes += line.len() as u64;
        if r.bytes > REC_MAX_BYTES {
            return Some((format!("size limit {} MB reached", REC_MAX_BYTES >> 20), r.frames));
        }
        None
    }

    /// Stop recording: Some({file, frames, bytes}) if it was.
    pub fn stop_recording(&mut self) -> Option<Rec> {
        let mut r = self.rec.take()?;
        let _ = r.writer.flush();
        Some(r)
    }
}
