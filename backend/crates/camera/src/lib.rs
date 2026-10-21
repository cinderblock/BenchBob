//! The camera panel, ported from `peripherals/drivers/camera.ts`.
//!
//! USB webcams and HDMI capture sticks (anything UVC). Each camera gets a `ustreamer` child,
//! started only when someone looks (the dashboard's live view, a snapshot, an agent) and
//! stopped once nobody has for a minute, so an idle camera costs nothing and its LED stays
//! off. The MJPEG stream and snapshots are proxied through BenchBob's own port; a per-camera
//! "share directly" switch instead binds ustreamer to the LAN (8080 + BENCHBOB_PORT_OFFSET,
//! counting up) for tools that want it raw.
//!
//! CSI cameras (libcamera) are listed when present, but not streamed: ustreamer can't drive
//! them. `rpicam-hello` may be missing, which only means no CSI list.
//!
//! Testing without hardware: `BENCHBOB_CAMERA_SIMULATE=/dev/video99` (any path) adds a fake
//! MJPEG camera "sim"; ustreamer can't open it and serves its NO SIGNAL placeholder frame,
//! which exercises the start-on-demand, proxy, snapshot and idle-stop paths.

pub mod parse;
pub mod upstream;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderValue, Method, header};
use axum::response::Response;
use benchbob_core::driver::ToolResult;
use benchbob_core::http::{HttpError, js_number, js_string, json as reply, num, truthy};
use benchbob_core::paths::{env, port_offset};
use benchbob_core::proc::{Proc, SIGTERM, Split};
use benchbob_core::run::{Opts, run};
use benchbob_core::timeline::now_ms;
use benchbob_core::udev::{Props, UdevEvent};
use benchbob_core::util::{base64_encode, hostname, iso, prop, usb_name};
use benchbob_core::{Ctx, Driver, Json, Reply, Route, StopReason, Tool};
use bytes::Bytes;
use futures_util::StreamExt;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio::process::Command;
use tokio::sync::watch;

use crate::parse::{
    Control, Format, FrameSize, StreamChoice, Want, camera_key, camera_kind, caps_can_capture, choose_stream, is_usb_capture_node,
    is_user_control, parse_controls, parse_device_caps, parse_formats, parse_libcamera_list, ustreamer_format,
};
use crate::upstream::Addr;

const IDLE_STOP_MS: u64 = 60_000;
/// ustreamer's own default, so tools' examples work unchanged.
const SHARE_PORT_BASE: u32 = 8080;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Clone)]
pub struct Config {
    /// `/tmp/benchbob-camera-<PORT>`: the private unix sockets.
    pub run_dir: PathBuf,
    /// `BENCHBOB_CAMERA_SIMULATE`
    pub sim: String,
    pub port_offset: u16,
    /// This server's port, for the URLs agents get.
    pub web_port: u16,
    pub ustreamer: PathBuf,
    pub v4l2_ctl: PathBuf,
    pub rpicam_hello: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        let port = env("PORT");
        Self {
            run_dir: PathBuf::from(format!("/tmp/benchbob-camera-{}", port.clone().unwrap_or_else(|| "80".into()))),
            sim: env("BENCHBOB_CAMERA_SIMULATE").unwrap_or_default(),
            port_offset: port_offset(),
            web_port: port.and_then(|p| benchbob_core::http::parse_int(&p)).filter(|p| *p != 0).map(|p| p as u16).unwrap_or(80),
            ustreamer: "ustreamer".into(),
            v4l2_ctl: "v4l2-ctl".into(),
            rpicam_hello: "rpicam-hello".into(),
        }
    }
}

type Ready = watch::Receiver<Option<Result<(), HttpError>>>;

struct Streamer {
    sid: u64,
    proc: Arc<Proc>,
    addr: Addr,
    /// The TCP port when shared.
    port: Option<u32>,
    shared: bool,
    ready: Ready,
    /// From ustreamer's /state: proxied viewers + direct ones.
    clients: f64,
    fps: Value,
    online: bool,
}

struct Cam {
    /// "video0": the /dev name; what URLs and tools use.
    id: String,
    /// Stable settings key (serial, else USB port path).
    key: String,
    dev: String,
    devpath: String,
    name: String,
    vendor: String,
    vid: String,
    pid: String,
    serial: String,
    kind: &'static str,
    /// "1-1.3"
    usb_port: String,
    driver: String,
    formats: Vec<Format>,
    controls: Vec<Control>,
    stream: Option<StreamChoice>,
    streamer: Option<Streamer>,
    /// Streams currently piped through BenchBob.
    proxied: u32,
    last_use: u64,
    error: String,
    last: Option<(Bytes, u64)>,
}

#[derive(Default)]
struct St {
    cams: IndexMap<String, Cam>,
    csi: Vec<Value>,
}

struct Inner {
    cfg: Config,
    ctx: OnceLock<Ctx>,
    st: Mutex<St>,
    next_sid: AtomicU64,
    rescan_gen: AtomicU64,
    epoch: AtomicU64,
}

pub struct Camera {
    inner: Arc<Inner>,
}

impl Default for Camera {
    fn default() -> Self {
        Self::new()
    }
}

impl Camera {
    pub fn new() -> Self {
        Self::with_config(Config::from_env())
    }
    pub fn with_config(cfg: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                cfg,
                ctx: OnceLock::new(),
                st: Mutex::new(St::default()),
                next_sid: AtomicU64::new(1),
                rescan_gen: AtomicU64::new(0),
                epoch: AtomicU64::new(0),
            }),
        }
    }
}

/// Auto switches first: an absolute exposure or temperature is refused while its auto is on.
fn auto_first(names: &mut [String]) {
    names.sort_by_key(|n| !n.contains("auto"));
}

impl Inner {
    fn ctx(&self) -> &Ctx {
        self.ctx.get().expect("camera driver not initialised")
    }

    // ---------- settings ----------

    fn get_cfg(&self, key: &str) -> Map<String, Value> {
        self.ctx().setting("cameras").and_then(|c| c.get(key).and_then(Value::as_object).cloned()).unwrap_or_default()
    }

    fn set_cfg(&self, key: &str, patch: Map<String, Value>) {
        let mut all = self.ctx().setting("cameras").and_then(|v| v.as_object().cloned()).unwrap_or_default();
        let mut e = all.get(key).and_then(Value::as_object).cloned().unwrap_or_default();
        for (k, v) in patch {
            e.insert(k, v);
        }
        all.insert(key.into(), Value::Object(e));
        self.ctx().set_setting("cameras", Value::Object(all));
    }

    /// The saved share port, else the first from 8080 (+ offset) no camera has saved.
    fn share_port(&self, st: &St, key: &str) -> u32 {
        let all = self.ctx().setting("cameras").unwrap_or(Value::Null);
        let saved = |k: &str| all.get(k).and_then(|c| c.get("sharePort")).filter(|v| truthy(v)).map(js_number);
        if let Some(p) = saved(key) {
            return p as u32;
        }
        let used: Vec<f64> = st.cams.values().filter_map(|c| saved(&c.key)).collect();
        let mut port = SHARE_PORT_BASE + self.cfg.port_offset as u32;
        while used.contains(&(port as f64)) {
            port += 1;
        }
        port
    }

    // ---------- discovery ----------

    async fn v4l2(&self, args: &[&str], ms: u64) -> benchbob_core::run::Output {
        let prog = self.cfg.v4l2_ctl.display().to_string();
        let mut c = vec![prog.as_str()];
        c.extend_from_slice(args);
        run(&c, Opts::timeout_ms(ms)).await
    }

    async fn read_controls(&self, dev: &str) -> Vec<Control> {
        parse_controls(&self.v4l2(&["-d", dev, "--list-ctrls-menus"], 5000).await.out)
    }

    fn set_controls_of(&self, id: &str, controls: Vec<Control>) {
        if let Some(c) = lock(&self.st).cams.get_mut(id) {
            c.controls = controls;
        }
    }

    async fn probe(&self, p: &Props) -> Option<Cam> {
        let dev = prop(p, "DEVNAME").to_string();
        // Older udev marks a UVC camera's metadata node `:capture:` too; its own Device Caps settle it.
        let caps = parse_device_caps(&self.v4l2(&["-d", &dev, "-D"], 5000).await.out);
        if !caps_can_capture(&caps.caps) {
            return None;
        }
        let usb = self.ctx().ancestor(prop(p, "DEVPATH"), "usb", Some("usb_device")).unwrap_or_default();
        let (vendor, model) = usb_name(&usb);
        let or = |a: &str, b: &str| if a.is_empty() { b.to_string() } else { a.to_string() };
        let vid = or(prop(&usb, "ID_VENDOR_ID"), prop(p, "ID_VENDOR_ID"));
        let pid = or(prop(&usb, "ID_MODEL_ID"), prop(p, "ID_MODEL_ID"));
        let name = [caps.card.as_str(), model.as_str(), prop(p, "ID_V4L_PRODUCT"), dev.as_str()]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        let mut merged = usb.clone();
        for (k, v) in p {
            merged.insert(k.clone(), v.clone());
        }
        let key = camera_key(&merged);
        let formats = parse_formats(&self.v4l2(&["-d", &dev, "--list-formats-ext"], 5000).await.out);
        let saved = self.get_cfg(&key).get("stream").map(Want::from_json);
        let stream = choose_stream(&formats, saved.as_ref());
        let mut c = Cam {
            id: dev.get(5..).unwrap_or("").to_string(),
            key,
            devpath: prop(p, "DEVPATH").into(),
            kind: camera_kind(&vid, &pid, &format!("{name} {model}")),
            name,
            vendor,
            vid,
            pid,
            serial: or(prop(p, "ID_SERIAL_SHORT"), prop(&usb, "ID_SERIAL_SHORT")),
            usb_port: prop(&usb, "DEVPATH").rsplit('/').next().unwrap_or("").into(),
            driver: caps.driver,
            formats,
            controls: Vec::new(),
            stream,
            streamer: None,
            proxied: 0,
            last_use: 0,
            error: String::new(),
            last: None,
            dev,
        };
        c.controls = self.apply_saved_controls(&c.key, &c.dev).await;
        Some(c)
    }

    /// Cameras forget their controls on every replug; put back what the user chose.
    async fn apply_saved_controls(&self, key: &str, dev: &str) -> Vec<Control> {
        let saved = self.get_cfg(key).get("controls").and_then(Value::as_object).cloned().unwrap_or_default();
        let controls = self.read_controls(dev).await;
        let mut names: Vec<String> = saved.keys().filter(|n| controls.iter().any(|x| x.name == **n)).cloned().collect();
        auto_first(&mut names);
        for n in &names {
            let a = format!("{n}={}", js_string(&saved[n]));
            self.v4l2(&["-d", dev, "-c", &a], 3000).await;
        }
        if names.is_empty() { controls } else { self.read_controls(dev).await }
    }

    fn sim_cam(&self) -> Cam {
        let formats = vec![Format {
            fourcc: "MJPG".into(),
            description: "Motion-JPEG, compressed".into(),
            compressed: true,
            sizes: vec![FrameSize { w: 640, h: 480, fps: vec![30.0, 15.0] }],
            stepwise: None,
        }];
        Cam {
            id: "sim".into(),
            key: "sim".into(),
            dev: self.cfg.sim.clone(),
            devpath: String::new(),
            name: "Simulated camera".into(),
            vendor: "BenchBob".into(),
            vid: "0000".into(),
            pid: "0000".into(),
            serial: String::new(),
            kind: "webcam",
            usb_port: "-".into(),
            driver: "simulated".into(),
            stream: choose_stream(&formats, None),
            formats,
            controls: Vec::new(),
            streamer: None,
            proxied: 0,
            last_use: 0,
            error: String::new(),
            last: None,
        }
    }

    async fn rescan(self: &Arc<Self>) {
        let ctx = self.ctx();
        let nodes = ctx.devices(Some(&|p: &Props| {
            prop(p, "SUBSYSTEM") == "video4linux"
                && is_usb_capture_node(p, ctx.ancestor(prop(p, "DEVPATH"), "usb", Some("usb_device")).as_ref())
        }));
        let mut seen: Vec<String> = Vec::new();
        for p in &nodes {
            let id = prop(p, "DEVNAME").get(5..).unwrap_or("").to_string();
            seen.push(id.clone());
            let same = lock(&self.st).cams.get(&id).is_some_and(|c| c.devpath == prop(p, "DEVPATH"));
            if same {
                continue;
            }
            let Some(c) = self.probe(p).await else { continue };
            ctx.event(
                &format!("Camera connected: {} ({}{})", c.name, c.dev, if c.kind == "hdmi" { ", HDMI capture" } else { "" }),
                Some(json!({ "id": id, "vid": c.vid, "pid": c.pid })),
            );
            let share = truthy(self.get_cfg(&c.key).get("share").unwrap_or(&Value::Null));
            let old = lock(&self.st).cams.insert(id.clone(), c);
            // The same node name for a different device: the old streamer can't serve it.
            if let Some(mut o) = old
                && let Some(s) = o.streamer.take()
            {
                s.proc.kill(SIGTERM);
            }
            if share {
                let me = self.clone();
                let id2 = id.clone();
                tokio::spawn(async move {
                    if let Err(e) = me.ensure_streamer(&id2).await
                        && let Some(c) = lock(&me.st).cams.get_mut(&id2)
                    {
                        c.error = e.message;
                    }
                });
            }
        }
        if !self.cfg.sim.is_empty() {
            seen.push("sim".into());
            let mut st = lock(&self.st);
            if !st.cams.contains_key("sim") {
                let c = self.sim_cam();
                st.cams.insert("sim".into(), c);
            }
        }
        let gone: Vec<String> = lock(&self.st).cams.keys().filter(|k| !seen.contains(k)).cloned().collect();
        for id in gone {
            drop(self.stop_streamer(&id)); // the kill is sent now; not waiting for the exit
            if let Some(c) = lock(&self.st).cams.shift_remove(&id) {
                ctx.event(&format!("Camera disconnected: {} ({})", c.name, c.dev), Some(json!({ "id": id })));
            }
        }
        ctx.changed();
    }

    fn schedule_rescan(self: &Arc<Self>) {
        let g = self.rescan_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let me = self.clone();
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        // A UVC camera arrives as several udev events (usb device, interfaces, two video nodes).
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(700)).await;
            if me.rescan_gen.load(Ordering::SeqCst) == g {
                me.rescan().await;
            }
        });
    }

    // ---------- ustreamer ----------

    /// Start the camera's ustreamer if it isn't running; resolves once it answers /state.
    async fn ensure_streamer(self: &Arc<Self>, id: &str) -> Result<(), HttpError> {
        let mut ready = self.start_streamer(id)?;
        loop {
            if let Some(r) = ready.borrow_and_update().clone() {
                return r;
            }
            if ready.changed().await.is_err() {
                return Err(HttpError::new(503, "ustreamer stopped"));
            }
        }
    }

    fn start_streamer(self: &Arc<Self>, id: &str) -> Result<Ready, HttpError> {
        let mut st = lock(&self.st);
        let share_port = st.cams.get(id).map(|c| self.share_port(&st, &c.key));
        let has_video31 = !self.ctx().devices(Some(&|p: &Props| prop(p, "DEVNAME") == "/dev/video31")).is_empty();
        let c = st.cams.get_mut(id).ok_or_else(|| HttpError::new(404, format!("No camera \"{id}\"")))?;
        c.last_use = now_ms();
        if let Some(s) = &c.streamer {
            return Ok(s.ready.clone());
        }
        let Some(stream) = c.stream.clone() else {
            return Err(HttpError::new(409, format!("{} offers no format ustreamer can stream", c.name)));
        };
        let cfg = self.get_cfg(&c.key);
        let shared = cfg.get("share").is_some_and(truthy);
        let fmt = ustreamer_format(&stream.fourcc).unwrap_or("MJPEG");
        let compressed = fmt == "MJPEG" || fmt == "JPEG";
        // MJPEG from the camera is forwarded as-is. Raw frames go to the Pi's JPEG block
        // (/dev/video31) when it's there; the CPU encoder is the fallback.
        let encoder = if compressed {
            "HW"
        } else if has_video31 {
            "M2M-IMAGE"
        } else {
            "CPU"
        };
        let mut args: Vec<String> = [
            "--device",
            &c.dev,
            "--format",
            fmt,
            "--resolution",
            &format!("{}x{}", js_string(&num(stream.w)), js_string(&num(stream.h))),
            "--desired-fps",
            &js_string(&num(stream.fps)),
            "--encoder",
            encoder,
            "--exit-on-parent-death",
            "--process-name-prefix",
            &format!("benchbob-{}", c.id),
            "--no-log-colors",
        ]
        .map(String::from)
        .to_vec();
        let (addr, port) = if shared {
            let p = share_port.unwrap_or(SHARE_PORT_BASE);
            args.extend(["--host".into(), "0.0.0.0".into(), "--port".into(), p.to_string()]);
            (Addr::Tcp(p as u16), Some(p))
        } else {
            let sock = self.cfg.run_dir.join(format!("{}.sock", c.id));
            args.extend(["--unix".into(), sock.display().to_string(), "--unix-rm".into(), "--unix-mode".into(), "660".into()]);
            (Addr::Unix(sock), None)
        };
        c.error.clear();
        let stderr: Arc<Mutex<Vec<String>>> = Default::default();
        let buf = stderr.clone();
        let on_line = Arc::new(move |l: &str| {
            if l.trim().is_empty() {
                return;
            }
            let mut b = lock(&buf);
            b.push(l.trim().to_string());
            if b.len() > 30 {
                let n = b.len() - 30;
                b.drain(..n);
            }
        });
        let proc = Proc::spawn_with(self.ctx(), Command::new(&self.cfg.ustreamer).args(&args), Split::Lf, on_line, false)
            .map_err(|e| HttpError::new(500, spawn_error(&e, &self.cfg.ustreamer)))?;
        let sid = self.next_sid.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = watch::channel(None);
        let key = c.key.clone();
        c.streamer = Some(Streamer {
            sid,
            proc: proc.clone(),
            addr: addr.clone(),
            port,
            shared,
            ready: rx.clone(),
            clients: 0.0,
            fps: json!(0),
            online: false,
        });
        drop(st);
        if shared && !cfg.get("sharePort").is_some_and(truthy) {
            let mut m = Map::new();
            m.insert("sharePort".into(), json!(port));
            self.set_cfg(&key, m);
        }
        // Exit: forget it, and say why when it gave up by itself.
        let me = self.clone();
        let id2 = id.to_string();
        tokio::spawn(async move {
            let code = proc.wait().await;
            {
                let mut st = lock(&me.st);
                let Some(c) = st.cams.get_mut(&id2) else { return };
                if !c.streamer.as_ref().is_some_and(|s| s.sid == sid) {
                    return;
                }
                c.streamer = None;
                if code != 0 {
                    // ustreamer logs "-- ERROR [...] ..." lines; the last one says why it gave up.
                    let lines = lock(&stderr).clone();
                    c.error = lines
                        .iter()
                        .rev()
                        .find(|l| l.to_lowercase().contains("error"))
                        .or(lines.last())
                        .cloned()
                        .unwrap_or_else(|| format!("ustreamer exited ({code})"));
                }
            }
            me.ctx().changed();
        });
        // Ready once /state answers.
        let me = self.clone();
        let id2 = id.to_string();
        tokio::spawn(async move {
            for _ in 0..50 {
                let alive = lock(&me.st).cams.get(&id2).and_then(|c| c.streamer.as_ref().map(|s| s.sid == sid)).unwrap_or(false);
                if !alive {
                    let e = lock(&me.st).cams.get(&id2).map(|c| c.error.clone()).filter(|e| !e.is_empty());
                    let _ = tx.send(Some(Err(HttpError::new(503, e.unwrap_or_else(|| "ustreamer stopped".into())))));
                    return;
                }
                if let Ok((200, body)) = upstream::get_all(&addr, "/state").await {
                    me.read_state(&id2, sid, &body);
                    me.ctx().changed();
                    let _ = tx.send(Some(Ok(())));
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let _ = tx.send(Some(Err(HttpError::new(504, "ustreamer didn't start within 5 s"))));
        });
        self.ctx().changed();
        Ok(rx)
    }

    fn read_state(&self, id: &str, sid: u64, body: &[u8]) {
        let Some(j) = std::str::from_utf8(body).ok().and_then(benchbob_core::util::parse_json) else { return };
        let mut st = lock(&self.st);
        let Some(s) = st.cams.get_mut(id).and_then(|c| c.streamer.as_mut()).filter(|s| s.sid == sid) else { return };
        let r = &j["result"];
        s.clients = r["stream"]["clients"].as_f64().unwrap_or(0.0);
        s.fps = match &r["source"]["captured_fps"] {
            Value::Null => json!(0),
            v => v.clone(),
        };
        s.online = truthy(&r["source"]["online"]);
    }

    /// Stop the camera's ustreamer; the returned future resolves once it has exited, so a
    /// restart doesn't find the device or port still busy.
    fn stop_streamer(&self, id: &str) -> impl std::future::Future<Output = ()> + use<> {
        let proc = lock(&self.st).cams.get_mut(id).and_then(|c| c.streamer.take()).map(|s| s.proc);
        if let Some(p) = &proc {
            p.kill(SIGTERM);
            self.ctx().changed();
        }
        async move {
            if let Some(p) = proc {
                p.wait().await;
            }
        }
    }

    /// Every 10 s: refresh viewer counts and stop streamers nobody has used for a minute.
    async fn idle_sweep(&self) {
        let list: Vec<(String, u64, Addr)> =
            lock(&self.st).cams.values().filter_map(|c| c.streamer.as_ref().map(|s| (c.id.clone(), s.sid, s.addr.clone()))).collect();
        for (id, sid, addr) in list {
            if let Ok((200, body)) = upstream::get_all(&addr, "/state").await {
                self.read_state(&id, sid, &body);
            }
            let stop = {
                let mut st = lock(&self.st);
                let Some(c) = st.cams.get_mut(&id) else { continue };
                let Some(s) = &c.streamer else { continue };
                if s.clients > 0.0 || c.proxied > 0 {
                    c.last_use = now_ms();
                }
                // A shared stream is for other tools, so it stays up until sharing is turned off.
                !s.shared && now_ms().saturating_sub(c.last_use) > IDLE_STOP_MS
            };
            if stop {
                self.ctx().log(&format!("{id}: idle for a minute, stopping ustreamer"));
                drop(self.stop_streamer(&id)); // the kill is sent now; not waiting for the exit
            }
        }
        self.ctx().changed();
    }

    // ---------- snapshots and streams ----------

    fn addr_of(&self, id: &str) -> Result<Addr, HttpError> {
        let st = lock(&self.st);
        let c = st.cams.get(id).ok_or_else(|| HttpError::new(404, format!("No camera \"{id}\"")))?;
        c.streamer
            .as_ref()
            .map(|s| s.addr.clone())
            .ok_or_else(|| HttpError::new(503, if c.error.is_empty() { "ustreamer stopped".to_string() } else { c.error.clone() }))
    }

    async fn snapshot(self: &Arc<Self>, id: &str) -> Result<(Bytes, u64), HttpError> {
        self.ensure_streamer(id).await?;
        let addr = self.addr_of(id)?;
        // Right after start ustreamer answers 503 until the camera delivers a first frame.
        for _ in 0..40 {
            let (status, body) = upstream::get_all(&addr, "/snapshot").await.map_err(|e| HttpError::new(500, e.to_string()))?;
            if status == 200 {
                let t = now_ms();
                let mut st = lock(&self.st);
                if let Some(c) = st.cams.get_mut(id) {
                    c.last = Some((body.clone(), t));
                    c.last_use = t;
                }
                return Ok((body, t));
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let name = lock(&self.st).cams.get(id).map(|c| c.name.clone()).unwrap_or_default();
        Err(HttpError::new(504, format!("{name} hasn't delivered a frame (no signal?)")))
    }

    /// Pipe ustreamer's multipart MJPEG through, counting viewers so the idle sweep knows.
    async fn stream(self: &Arc<Self>, id: &str) -> Result<Response, HttpError> {
        self.ensure_streamer(id).await?;
        let addr = self.addr_of(id)?;
        let r = upstream::get(&addr, "/stream").await.map_err(|e| HttpError::new(500, e.to_string()))?;
        if r.status() != 200 {
            return Err(HttpError::new(502, format!("ustreamer: {}", r.status().as_u16())));
        }
        let ctype = r.headers().get(header::CONTENT_TYPE).cloned().unwrap_or_else(|| HeaderValue::from_static("multipart/x-mixed-replace"));
        if let Some(c) = lock(&self.st).cams.get_mut(id) {
            c.proxied += 1;
        }
        // Dropping the body (the viewer left) drops the upstream connection and the guard.
        let guard = ViewerGuard { inner: self.clone(), id: id.to_string() };
        let body = http_body_util::BodyStream::new(r.into_body()).filter_map(move |f| {
            let _keep = &guard;
            let out = match f {
                Ok(frame) => frame.into_data().ok().map(Ok::<Bytes, std::io::Error>),
                Err(e) => Some(Err(std::io::Error::other(e.to_string()))),
            };
            std::future::ready(out)
        });
        self.ctx().changed();
        let mut resp = Response::new(Body::from_stream(body));
        resp.headers_mut().insert(header::CONTENT_TYPE, ctype);
        resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        Ok(resp)
    }

    // ---------- API helpers ----------

    fn cam_exists(&self, id: &str) -> Result<(), HttpError> {
        let st = lock(&self.st);
        if st.cams.contains_key(id) {
            return Ok(());
        }
        let have: Vec<&String> = st.cams.keys().collect();
        Err(HttpError::new(
            404,
            format!(
                "No camera \"{id}\"{}",
                if have.is_empty() {
                    " (none connected)".to_string()
                } else {
                    format!(" (have: {})", have.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
                }
            ),
        ))
    }

    fn public_cam(&self, st: &St, c: &Cam) -> Value {
        let cfg = self.get_cfg(&c.key);
        let s = c.streamer.as_ref();
        let share = cfg.get("share").is_some_and(truthy);
        let share_port = match (share, s.and_then(|s| s.port)) {
            (true, Some(p)) => p,
            _ => self.share_port(st, &c.key),
        };
        json!({
            "id": c.id,
            "dev": c.dev,
            "name": c.name,
            "vendor": c.vendor,
            "vid": c.vid,
            "pid": c.pid,
            "serial": c.serial,
            "kind": c.kind,
            "usbPort": c.usb_port,
            "driver": c.driver,
            "stream": c.stream.as_ref().map(StreamChoice::to_json),
            "streaming": s.is_some(),
            "viewers": s.map(|s| num(s.clients.max(c.proxied as f64))).unwrap_or(json!(0)),
            "fps": s.map(|s| if truthy(&s.fps) { s.fps.clone() } else { json!(0) }).unwrap_or(json!(0)),
            "signal": s.map(|s| Value::Bool(s.online)).unwrap_or(Value::Null),
            "share": share,
            "sharePort": share_port,
            "lastSnapshot": c.last.as_ref().map(|l| l.1).unwrap_or(0),
            "error": c.error,
        })
    }

    fn list(&self) -> Vec<Value> {
        let st = lock(&self.st);
        st.cams.values().map(|c| self.public_cam(&st, c)).collect()
    }

    fn public(&self, id: &str) -> Value {
        let st = lock(&self.st);
        st.cams.get(id).map(|c| self.public_cam(&st, c)).unwrap_or(Value::Null)
    }

    fn base_url(&self) -> String {
        let p = self.cfg.web_port;
        format!("http://{}{}", hostname(), if p == 80 { String::new() } else { format!(":{p}") })
    }

    fn user_controls(&self, id: &str) -> Value {
        let st = lock(&self.st);
        let list = st.cams.get(id).map(|c| c.controls.iter().filter(|x| is_user_control(x)).map(Control::to_json).collect::<Vec<_>>());
        Value::Array(list.unwrap_or_default())
    }

    async fn set_controls(&self, id: &str, values: &Value) -> Result<Value, HttpError> {
        // `typeof values === "object"`: an array passes too, with its indices as names.
        let values: Map<String, Value> = match values {
            Value::Object(m) => m.clone(),
            Value::Array(a) => a.iter().enumerate().map(|(i, v)| (i.to_string(), v.clone())).collect(),
            _ => return Err(HttpError::bad_request("Expected { name: value, … }")),
        };
        let (dev, key, name) = {
            let st = lock(&self.st);
            let c = st.cams.get(id).ok_or_else(|| HttpError::new(404, format!("No camera \"{id}\"")))?;
            (c.dev.clone(), c.key.clone(), c.name.clone())
        };
        let controls = self.read_controls(&dev).await;
        self.set_controls_of(id, controls.clone());
        let mut saved = self.get_cfg(&key).get("controls").and_then(Value::as_object).cloned().unwrap_or_default();
        let mut order: Vec<String> = values.keys().cloned().collect();
        auto_first(&mut order);
        for n in order {
            if !controls.iter().any(|x| x.name == n) {
                return Err(HttpError::bad_request(format!("{name} has no control \"{n}\"")));
            }
            let v = benchbob_core::util::js_round(js_number(&values[&n]));
            if !v.is_finite() {
                return Err(HttpError::bad_request(format!("{n}: not a number")));
            }
            let v = v as i64;
            let a = format!("{n}={v}");
            let r = self.v4l2(&["-d", &dev, "-c", &a], 3000).await;
            if r.code != 0 {
                return Err(HttpError::bad_request(format!("{n}={v}: {}", if r.err.is_empty() { r.out } else { r.err })));
            }
            saved.insert(n, json!(v));
        }
        let mut m = Map::new();
        m.insert("controls".into(), Value::Object(saved));
        self.set_cfg(&key, m);
        // Flipping an auto switch changes which other controls are "inactive".
        let controls = self.read_controls(&dev).await;
        self.set_controls_of(id, controls);
        Ok(self.user_controls(id))
    }

    async fn config(self: &Arc<Self>, id: &str, b: &Value) -> Result<Value, HttpError> {
        let (key, cur, formats, name) = {
            let st = lock(&self.st);
            let c = st.cams.get(id).ok_or_else(|| HttpError::new(404, format!("No camera \"{id}\"")))?;
            (c.key.clone(), c.stream.clone(), c.formats.clone(), c.name.clone())
        };
        let mut patch = Map::new();
        let t = |k: &str| b.get(k).is_some_and(truthy);
        if t("fourcc") || t("w") || t("h") || t("fps") {
            let pick = |k: &str, cur: Option<Value>| match b.get(k) {
                Some(v) if !v.is_null() => v.clone(),
                _ => cur.unwrap_or(Value::Null),
            };
            let n = |v: Value| if v.is_null() { f64::NAN } else { js_number(&v) };
            let want = Want {
                fourcc: pick("fourcc", cur.as_ref().map(|s| json!(s.fourcc))),
                w: n(pick("w", cur.as_ref().map(|s| num(s.w)))),
                h: n(pick("h", cur.as_ref().map(|s| num(s.h)))),
                fps: n(pick("fps", cur.as_ref().map(|s| num(s.fps)))),
            };
            let chosen = choose_stream(&formats, Some(&want));
            let fc = if want.fourcc.is_null() { "undefined".to_string() } else { js_string(&want.fourcc) };
            let ok = chosen.as_ref().is_some_and(|p| Value::String(p.fourcc.clone()) == want.fourcc && p.w == want.w && p.h == want.h);
            if !ok {
                let f = |x: f64| if x.is_nan() { "NaN".to_string() } else { js_string(&num(x)) };
                return Err(HttpError::bad_request(format!("{name} doesn't offer {fc} {}x{}", f(want.w), f(want.h))));
            }
            let pick = chosen.unwrap();
            patch.insert("stream".into(), pick.to_json());
            if let Some(c) = lock(&self.st).cams.get_mut(id) {
                c.stream = Some(pick);
            }
        }
        if let Some(p) = b.get("sharePort").filter(|v| !v.is_null()) {
            let port = js_number(p);
            if port.fract() != 0.0 || !(1024.0..=65535.0).contains(&port) {
                return Err(HttpError::bad_request("sharePort must be 1024–65535"));
            }
            patch.insert("sharePort".into(), num(port));
        }
        if let Some(s) = b.get("share").filter(|v| !v.is_null()) {
            patch.insert("share".into(), truthy(s).into());
        }
        self.set_cfg(&key, patch);
        // Restart with the new settings; a shared camera comes straight back up.
        let was_running = lock(&self.st).cams.get(id).is_some_and(|c| c.streamer.is_some());
        self.stop_streamer(id).await;
        if (self.get_cfg(&key).get("share").is_some_and(truthy) || was_running)
            && let Err(e) = self.ensure_streamer(id).await
            && let Some(c) = lock(&self.st).cams.get_mut(id)
        {
            c.error = e.message;
        }
        self.ctx().changed();
        Ok(json!({ "ok": true, "camera": self.public(id) }))
    }

    fn status(&self) -> Value {
        let list = self.list();
        let csi = lock(&self.st).csi.clone();
        let live: Vec<&Value> = list.iter().filter(|c| c["streaming"] == true).collect();
        let viewers: f64 = live.iter().map(|c| c["viewers"].as_f64().unwrap_or(0.0)).sum();
        let names = list.iter().map(|c| js_string(&c["name"])).collect::<Vec<_>>().join(", ");
        let summary = if list.is_empty() {
            if csi.is_empty() {
                "No camera".to_string()
            } else {
                format!("CSI: {}", csi.iter().map(|x| js_string(&x["sensor"])).collect::<Vec<_>>().join(", "))
            }
        } else if !live.is_empty() {
            format!("{names} · live, {} viewer{}", js_string(&num(viewers)), if viewers == 1.0 { "" } else { "s" })
        } else {
            names
        };
        json!({ "active": !list.is_empty() || !csi.is_empty(), "summary": summary, "cameras": list, "csi": csi })
    }
}

struct ViewerGuard {
    inner: Arc<Inner>,
    id: String,
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        if let Some(c) = lock(&self.inner.st).cams.get_mut(&self.id) {
            c.proxied = c.proxied.saturating_sub(1);
            c.last_use = now_ms();
        }
        self.inner.ctx().changed();
    }
}

fn spawn_error(e: &std::io::Error, prog: &std::path::Path) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("Executable not found in $PATH: \"{}\"", prog.display())
    } else {
        format!("{}: {e}", prog.display())
    }
}

fn jpeg_response(jpeg: Bytes, extra: &[(&'static str, String)]) -> Response {
    let mut r = Response::new(Body::from(jpeg));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    for (k, v) in extra {
        if let Ok(v) = HeaderValue::from_str(v) {
            h.insert(*k, v);
        }
    }
    r
}

fn route<F, Fut>(i: &Arc<Inner>, method: Method, path: &str, doc: Json, f: F) -> Route
where
    F: Fn(Arc<Inner>, benchbob_core::Request) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = benchbob_core::RouteResult> + Send + 'static,
{
    let i = i.clone();
    Route::new(method, path, Some(doc), move |r| f(i.clone(), r))
}

#[async_trait]
impl Driver for Camera {
    fn id(&self) -> &'static str {
        "camera"
    }
    fn title(&self) -> &'static str {
        "Camera"
    }
    fn order(&self) -> i32 {
        60
    }
    fn description(&self) -> &'static str {
        "USB webcams and HDMI capture sticks: live MJPEG view, snapshots, image controls"
    }

    async fn init(&self, ctx: Ctx) -> anyhow::Result<()> {
        let i = &self.inner;
        let _ = i.ctx.set(ctx);
        let _ = std::fs::create_dir_all(&i.cfg.run_dir);
        i.rescan().await;
        let epoch = i.epoch.load(Ordering::SeqCst);
        let me = i.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if me.epoch.load(Ordering::SeqCst) != epoch {
                    break;
                }
                me.idle_sweep().await;
            }
        });
        // libcamera enumeration takes a second or two; nothing waits on it.
        let me = i.clone();
        tokio::spawn(async move {
            let prog = me.cfg.rpicam_hello.display().to_string();
            let r = run(&[&prog, "--list-cameras"], Opts::timeout_ms(15_000)).await;
            let csi = parse_libcamera_list(&format!("{}\n{}", r.out, r.err));
            let any = !csi.is_empty();
            lock(&me.st).csi = csi;
            if any {
                me.ctx().changed();
            }
        });
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) {
        let i = &self.inner;
        i.epoch.fetch_add(1, Ordering::SeqCst);
        let ids: Vec<String> = lock(&i.st).cams.keys().cloned().collect();
        for id in ids {
            drop(i.stop_streamer(&id)); // the kill is sent now; not waiting for the exit
        }
        lock(&i.st).cams.clear();
    }

    fn udev(&self, ev: &UdevEvent) {
        if prop(&ev.props, "SUBSYSTEM") == "video4linux" {
            self.inner.schedule_rescan();
        }
    }

    fn status(&self) -> Option<Json> {
        self.inner.ctx.get()?;
        Some(self.inner.status())
    }

    fn routes(&self) -> Vec<Route> {
        let i = &self.inner;
        vec![
            route(
                i,
                Method::GET,
                "/api/camera",
                json!(
                    "List cameras: id (e.g. video0), name, kind (webcam | hdmi), chosen stream format, whether ustreamer is running, viewers; plus CSI cameras seen by libcamera (listed only)"
                ),
                |i, _r| async move {
                    let csi = lock(&i.st).csi.clone();
                    reply(json!({ "cameras": i.list(), "csi": csi }))
                },
            ),
            route(
                i,
                Method::GET,
                "/api/camera/:id",
                json!(
                    "One camera in detail: every format/resolution/fps it offers and its adjustable controls (brightness, exposure, white balance, focus…) with ranges and current values"
                ),
                |i, r| async move {
                    let id = r.param("id").to_string();
                    i.cam_exists(&id)?;
                    let dev = lock(&i.st).cams.get(&id).map(|c| c.dev.clone()).unwrap_or_default();
                    let controls = i.read_controls(&dev).await;
                    i.set_controls_of(&id, controls);
                    let mut v = i.public(&id);
                    let formats: Vec<Value> =
                        lock(&i.st).cams.get(&id).map(|c| c.formats.iter().map(Format::to_json).collect()).unwrap_or_default();
                    v["formats"] = Value::Array(formats);
                    v["controls"] = i.user_controls(&id);
                    reply(v)
                },
            ),
            route(
                i,
                Method::GET,
                "/api/camera/:id/stream",
                json!(
                    "Live MJPEG (multipart/x-mixed-replace) — use as an <img> src. Starts the camera on demand; it stops a minute after the last viewer leaves"
                ),
                |i, r| async move {
                    let id = r.param("id").to_string();
                    i.cam_exists(&id)?;
                    Ok(Reply::Response(i.stream(&id).await?))
                },
            ),
            route(
                i,
                Method::GET,
                "/api/camera/:id/snapshot",
                json!("A fresh JPEG frame (starts the camera if needed). ?download=1 adds a Content-Disposition filename"),
                |i, r| async move {
                    let id = r.param("id").to_string();
                    i.cam_exists(&id)?;
                    let (jpeg, t) = i.snapshot(&id).await?;
                    let mut extra = Vec::new();
                    if r.query("download").is_some_and(|v| !v.is_empty()) {
                        let stamp = iso(t as i64).replace([':', '.'], "-");
                        extra.push(("content-disposition", format!("attachment; filename=\"{id}-{stamp}.jpg\"")));
                    }
                    Ok(Reply::Response(jpeg_response(jpeg, &extra)))
                },
            ),
            route(
                i,
                Method::GET,
                "/api/camera/:id/last.jpg",
                json!("The most recent snapshot taken (by anyone), without waking the camera. 404 if none yet"),
                |i, r| async move {
                    let id = r.param("id").to_string();
                    i.cam_exists(&id)?;
                    let last = lock(&i.st).cams.get(&id).and_then(|c| c.last.clone());
                    let Some((jpeg, t)) = last else { return Err(HttpError::new(404, "No snapshot taken yet")) };
                    Ok(Reply::Response(jpeg_response(jpeg, &[("x-taken-at", iso(t as i64))])))
                },
            ),
            route(
                i,
                Method::POST,
                "/api/camera/:id/controls",
                json!({ "body": { "brightness": 140, "auto_exposure": 1, "exposure_time_absolute": 300 }, "description": "Set image controls (v4l2 names from GET /api/camera/:id). Remembered per camera and re-applied when it's plugged in again" }),
                |i, r| async move {
                    let id = r.param("id").to_string();
                    i.cam_exists(&id)?;
                    let b = r.json()?;
                    reply(json!({ "ok": true, "controls": i.set_controls(&id, &b).await? }))
                },
            ),
            route(
                i,
                Method::POST,
                "/api/camera/:id/controls/reset",
                json!("Put every control back to the camera's default and forget the saved values"),
                |i, r| async move {
                    let id = r.param("id").to_string();
                    i.cam_exists(&id)?;
                    let (dev, key) = lock(&i.st).cams.get(&id).map(|c| (c.dev.clone(), c.key.clone())).unwrap_or_default();
                    let controls = i.read_controls(&dev).await;
                    let mut defaults = Map::new();
                    for x in controls.iter().filter(|x| is_user_control(x)) {
                        if let Some(d) = x.get("default") {
                            defaults.insert(x.name.clone(), num(d));
                        }
                    }
                    i.set_controls_of(&id, controls);
                    let controls = i.set_controls(&id, &Value::Object(defaults)).await?;
                    let mut m = Map::new();
                    m.insert("controls".into(), json!({}));
                    i.set_cfg(&key, m);
                    reply(json!({ "ok": true, "controls": controls }))
                },
            ),
            route(
                i,
                Method::POST,
                "/api/camera/:id/config",
                json!({ "body": { "fourcc": "MJPG", "w": 1280, "h": 720, "fps": 30, "share": false, "sharePort": 8080 }, "description": "Stream format/resolution/fps, and 'share directly': ustreamer on 0.0.0.0:sharePort (its own /stream, /snapshot, /state) for tools that want it without BenchBob in the path. A shared stream stays running until sharing is turned off" }),
                |i, r| async move {
                    let id = r.param("id").to_string();
                    i.cam_exists(&id)?;
                    let b = r.json()?;
                    reply(i.config(&id, &b).await?)
                },
            ),
            route(
                i,
                Method::POST,
                "/api/camera/:id/stop",
                json!("Stop the camera's ustreamer now (viewers are disconnected; it restarts on the next view)"),
                |i, r| async move {
                    let id = r.param("id").to_string();
                    i.cam_exists(&id)?;
                    drop(i.stop_streamer(&id)); // the kill is sent now; not waiting for the exit
                    reply(json!({ "ok": true }))
                },
            ),
        ]
    }

    fn tools(&self) -> Vec<Tool> {
        let i = self.inner.clone();
        let list_tool = {
            let i = i.clone();
            Tool::new(
                "camera_list",
                "List cameras attached to BenchBob (USB webcams, HDMI capture sticks; CSI cameras are listed but can't be streamed). Returns ids for camera_snapshot, the stream format, and the URLs of the live MJPEG stream and snapshot endpoints.",
                json!({ "type": "object", "properties": {} }),
                move |_a| {
                    let i = i.clone();
                    async move {
                        let base = i.base_url();
                        let cams: Vec<Value> = i
                            .list()
                            .into_iter()
                            .map(|mut c| {
                                let id = js_string(&c["id"]);
                                c["streamUrl"] = format!("{base}/api/camera/{id}/stream").into();
                                c["snapshotUrl"] = format!("{base}/api/camera/{id}/snapshot").into();
                                c
                            })
                            .collect();
                        let csi = lock(&i.st).csi.clone();
                        Ok(json!({ "cameras": cams, "csi": csi }))
                    }
                },
            )
        };
        let snap = Tool {
            name: "camera_snapshot".into(),
            description: "Capture a JPEG from a camera now and return the image itself, plus a URL to fetch it again (valid until the next snapshot), its size and time. Use camera_list first to get the id.".into(),
            input_schema: json!({ "type": "object", "properties": { "id": { "type": "string", "description": "Camera id from camera_list, e.g. video0. Optional when exactly one camera is connected." } } }),
            handler: Arc::new(move |a: Value| {
                let i = i.clone();
                Box::pin(async move {
                    let id = match a.get("id").filter(|v| truthy(v)) {
                        Some(v) => js_string(v),
                        None => {
                            let st = lock(&i.st);
                            if st.cams.len() == 1 {
                                st.cams.keys().next().cloned().unwrap_or_default()
                            } else if st.cams.is_empty() {
                                return Err(HttpError::bad_request("No camera connected"));
                            } else {
                                let ids: Vec<&str> = st.cams.keys().map(String::as_str).collect();
                                return Err(HttpError::bad_request(format!("Several cameras; pass id ({})", ids.join(", "))));
                            }
                        }
                    };
                    i.cam_exists(&id)?;
                    let (jpeg, t) = i.snapshot(&id).await?;
                    let (name, stream) = lock(&i.st).cams.get(&id).map(|c| (c.name.clone(), c.stream.clone())).unwrap_or_default();
                    let mut meta = Map::new();
                    meta.insert("url".into(), format!("{}/api/camera/{id}/last.jpg?t={t}", i.base_url()).into());
                    meta.insert("path".into(), format!("/api/camera/{id}/last.jpg").into());
                    meta.insert("camera".into(), name.into());
                    meta.insert("bytes".into(), jpeg.len().into());
                    if let Some(s) = &stream {
                        meta.insert("width".into(), num(s.w));
                        meta.insert("height".into(), num(s.h));
                    }
                    meta.insert("takenAt".into(), iso(t as i64).into());
                    let meta = Value::Object(meta);
                    // Agents can look at the picture directly instead of fetching the URL.
                    Ok(ToolResult::Content {
                        content: vec![
                            json!({ "type": "image", "data": base64_encode(&jpeg), "mimeType": "image/jpeg" }),
                            json!({ "type": "text", "text": serde_json::to_string_pretty(&meta).unwrap_or_default() }),
                        ],
                        structured: Some(meta),
                    })
                })
            }),
        };
        vec![list_tool, snap]
    }
}

#[cfg(test)]
mod tests;
