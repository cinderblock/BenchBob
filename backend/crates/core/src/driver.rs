//! The driver contract (core.ts `Driver`): one device class per driver, each with routes,
//! WebSockets and MCP tools, mounted by the registry so adding a class never touches the
//! server.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::Method;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::Json;
use crate::http::{HttpError, Request, RouteResult};
use crate::settings::Settings;
use crate::timeline::Timeline;
use crate::udev::{Props, UdevDb, UdevEvent};

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The user turned the driver off.
    Disable,
    /// The service is stopping or restarting.
    Shutdown,
}

#[async_trait]
pub trait Driver: Send + Sync + 'static {
    fn id(&self) -> &'static str;
    fn title(&self) -> &'static str;
    /// Section order on the dashboard (lower first).
    fn order(&self) -> i32 {
        100
    }
    /// One-line description for the driver settings list.
    fn description(&self) -> &'static str {
        ""
    }
    async fn init(&self, _ctx: Ctx) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&self, _reason: StopReason) {}
    /// Hotplug: called for every udev event after the device DB is updated.
    fn udev(&self, _ev: &UdevEvent) {}
    /// Cheap, cached state for the 2 s status stream. `active` / `summary` keys are read.
    fn status(&self) -> Option<Json> {
        None
    }
    fn routes(&self) -> Vec<Route> {
        Vec::new()
    }
    fn sockets(&self) -> Vec<Socket> {
        Vec::new()
    }
    fn tools(&self) -> Vec<Tool> {
        Vec::new()
    }
    /// False for a part of the server that isn't a dashboard panel (drives and flashing,
    /// which live in server.ts in the Bun server): it has no row in `peripherals`, always
    /// runs, and can't be disabled. It still gets routes, docs, MCP tools and a Ctx.
    fn listed(&self) -> bool {
        true
    }
    /// The driver itself, for the binary to reach its own API (the drives crate adds
    /// top-level `/api/status` fields this way).
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }
}

// ---------- routes ----------

pub type RouteHandler = Arc<dyn Fn(Request) -> BoxFut<RouteResult> + Send + Sync>;

pub struct Route {
    pub method: Method,
    /// "/api/serial/:id/config": `:name` segments become params.
    pub path: String,
    pub doc: Option<Json>,
    pub handler: RouteHandler,
}

impl Route {
    pub fn new<F, Fut>(method: Method, path: &str, doc: Option<Json>, f: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = RouteResult> + Send + 'static,
    {
        Self { method, path: path.into(), doc, handler: Arc::new(move |r| Box::pin(f(r))) }
    }
}

// ---------- WebSockets ----------

static NEXT_WS: AtomicU64 = AtomicU64::new(1);

pub enum WsOut {
    Text(String),
    Close,
}

pub enum WsIn {
    Text(String),
    Binary(Bytes),
}

/// The sending half of one WebSocket connection; cheap to clone and keep in a set.
#[derive(Clone)]
pub struct WsClient {
    id: u64,
    tx: mpsc::UnboundedSender<WsOut>,
}

impl WsClient {
    pub fn new(tx: mpsc::UnboundedSender<WsOut>) -> Self {
        Self { id: NEXT_WS.fetch_add(1, Ordering::Relaxed), tx }
    }
    pub fn id(&self) -> u64 {
        self.id
    }
    /// Queue a text message; false once the connection is gone.
    pub fn send(&self, text: impl Into<String>) -> bool {
        self.tx.send(WsOut::Text(text.into())).is_ok()
    }
    pub fn close(&self) {
        let _ = self.tx.send(WsOut::Close);
    }
}

/// One connection, handed to the socket's handler, which runs for as long as it is open.
pub struct WsSession {
    pub client: WsClient,
    pub params: HashMap<String, String>,
    pub query: Vec<(String, String)>,
    pub incoming: mpsc::UnboundedReceiver<WsIn>,
}

pub type SocketHandler = Arc<dyn Fn(WsSession) -> BoxFut<()> + Send + Sync>;

pub struct Socket {
    /// "/ws/serial/:id"
    pub path: String,
    pub doc: Option<String>,
    pub handler: SocketHandler,
}

impl Socket {
    pub fn new<F, Fut>(path: &str, doc: Option<&str>, f: F) -> Self
    where
        F: Fn(WsSession) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self { path: path.into(), doc: doc.map(Into::into), handler: Arc::new(move |s| Box::pin(f(s))) }
    }
}

// ---------- MCP tools ----------

pub enum ToolResult {
    /// Sent as one JSON text block (and as structuredContent when it's an object).
    Value(Json),
    /// MCP content blocks as-is (images, several text blocks), plus structuredContent.
    Content { content: Vec<Json>, structured: Option<Json> },
}

pub type ToolHandler = Arc<dyn Fn(Json) -> BoxFut<Result<ToolResult, HttpError>> + Send + Sync>;

pub struct Tool {
    /// Unique across drivers, e.g. "serial_write".
    pub name: String,
    pub description: String,
    /// JSON Schema (type: "object").
    pub input_schema: Json,
    pub handler: ToolHandler,
}

impl Tool {
    pub fn new<F, Fut>(name: &str, description: &str, input_schema: Json, f: F) -> Self
    where
        F: Fn(Json) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Json, HttpError>> + Send + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            handler: Arc::new(move |a| {
                let fut = f(a);
                Box::pin(async move { fut.await.map(ToolResult::Value) })
            }),
        }
    }
}

// ---------- Ctx ----------

pub type ChangedFn = Arc<dyn Fn() + Send + Sync>;

/// Shared by every driver's Ctx.
pub struct Hub {
    pub udev: Arc<UdevDb>,
    pub timeline: Timeline,
    pub settings: Settings,
    changed_cb: Mutex<Option<ChangedFn>>,
    changed_pending: AtomicBool,
}

impl Hub {
    pub fn new(settings: Settings) -> Self {
        Self {
            udev: Arc::new(UdevDb::default()),
            timeline: Timeline::default(),
            settings,
            changed_cb: Mutex::new(None),
            changed_pending: AtomicBool::new(false),
        }
    }

    pub fn on_changed(&self, f: ChangedFn) {
        *self.changed_cb.lock().unwrap() = Some(f);
    }

    /// Push a status refresh soon (debounced 150 ms).
    pub fn changed(self: &Arc<Self>) {
        if self.changed_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            self.changed_pending.store(false, Ordering::SeqCst);
            return;
        };
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            me.changed_pending.store(false, Ordering::SeqCst);
            let cb = me.changed_cb.lock().unwrap().clone();
            if let Some(cb) = cb {
                cb();
            }
        });
    }
}

/// What a driver gets at init.
#[derive(Clone)]
pub struct Ctx {
    pub id: &'static str,
    pub hub: Arc<Hub>,
    /// Children spawned through this Ctx, killed when the driver stops.
    pub children: Arc<Mutex<std::collections::HashSet<u32>>>,
}

/// A child process started with [`Ctx::spawn`]: killed when the driver stops, and when
/// this handle is dropped.
pub struct Child {
    pub child: tokio::process::Child,
    pid: Option<u32>,
    set: Arc<Mutex<std::collections::HashSet<u32>>>,
}

impl Drop for Child {
    fn drop(&mut self) {
        if let Some(p) = self.pid {
            self.set.lock().unwrap().remove(&p);
        }
    }
}

impl Ctx {
    pub fn new(id: &'static str, hub: Arc<Hub>) -> Self {
        Self { id, hub, children: Default::default() }
    }

    /// Spawn a child that is killed on stop()/shutdown (and when its handle is dropped).
    pub fn spawn(&self, cmd: &mut tokio::process::Command) -> std::io::Result<Child> {
        let child = cmd.kill_on_drop(true).spawn()?;
        let pid = child.id();
        if let Some(p) = pid {
            self.children.lock().unwrap().insert(p);
        }
        Ok(Child { child, pid, set: self.children.clone() })
    }

    /// SIGTERM every child still running (the registry calls this after stop()).
    pub fn kill_children(&self) {
        let pids: Vec<u32> = self.children.lock().unwrap().drain().collect();
        for p in pids {
            #[cfg(unix)]
            // SAFETY: kill(2) on a pid we spawned and haven't seen reaped.
            unsafe {
                libc::kill(p as libc::pid_t, libc::SIGTERM);
            }
            #[cfg(not(unix))]
            let _ = p;
        }
    }

    /// Current udev database, optionally filtered. Kept live by the hotplug monitor.
    pub fn devices(&self, filter: Option<&dyn Fn(&Props) -> bool>) -> Vec<Props> {
        self.hub.udev.devices(filter)
    }
    /// The nearest ancestor of `devpath` with the given subsystem (and devtype).
    pub fn ancestor(&self, devpath: &str, subsystem: &str, devtype: Option<&str>) -> Option<Props> {
        self.hub.udev.ancestor(devpath, subsystem, devtype)
    }
    /// Push a status refresh to dashboards now (debounced).
    pub fn changed(&self) {
        self.hub.changed();
    }
    /// Add a line to the hardware timeline.
    pub fn event(&self, text: &str, detail: Option<Json>) {
        self.hub.timeline.add(self.id, text, detail);
        self.hub.changed();
    }
    /// A persisted per-driver setting (peripherals.json).
    pub fn setting(&self, key: &str) -> Option<Json> {
        self.hub.settings.get(self.id, key)
    }
    pub fn set_setting(&self, key: &str, value: Json) {
        self.hub.settings.set(self.id, key, value);
    }
    pub fn log(&self, msg: &str) {
        tracing::info!("[{}] {msg}", self.id);
    }
}
