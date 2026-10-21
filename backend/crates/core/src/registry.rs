//! The driver registry: starts and stops drivers, feeds them udev events, routes HTTP and
//! WebSocket requests to them, lists their MCP tools and docs, and assembles their status
//! (core.ts's registry, routing and status sections).
//!
//! Drivers are registered by the binary in a fixed list (Rust has no runtime import), but
//! the registry itself knows nothing about any of them.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;

use axum::http::Method;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex as AsyncMutex;

use crate::driver::{Ctx, Driver, Hub, Route, Socket, SocketHandler, StopReason, Tool};
use crate::http::{HttpError, match_path};
use crate::settings::Settings;

pub struct Loaded {
    pub driver: Arc<dyn Driver>,
    pub ctx: Ctx,
    started: AsyncMutex<bool>,
    started_flag: std::sync::atomic::AtomicBool,
    pub routes: Vec<Route>,
    pub sockets: Vec<Socket>,
    pub tools: Vec<Arc<Tool>>,
}

impl Loaded {
    pub fn started(&self) -> bool {
        self.started_flag.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// One row of `peripherals.list` in the status.
#[derive(Clone, Debug)]
pub struct ListItem {
    pub id: String,
    pub order: i32,
    pub entry: Value,
    pub state: Option<Value>,
}

pub struct Registry {
    hub: Arc<Hub>,
    loaded: Vec<Arc<Loaded>>,
    stop_tx: tokio::sync::watch::Sender<bool>,
}

/// A route that matched a request.
pub struct RouteMatch {
    pub loaded: Arc<Loaded>,
    pub index: usize,
    pub params: HashMap<String, String>,
}

impl RouteMatch {
    pub fn route(&self) -> &Route {
        &self.loaded.routes[self.index]
    }
}

impl Registry {
    pub fn new(settings_path: PathBuf) -> Self {
        let (stop_tx, _) = tokio::sync::watch::channel(false);
        Self { hub: Arc::new(Hub::new(Settings::load(settings_path))), loaded: Vec::new(), stop_tx }
    }

    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }

    pub fn register(&mut self, driver: Arc<dyn Driver>) {
        let id = driver.id();
        assert!(!self.loaded.iter().any(|l| l.driver.id() == id), "duplicate driver id {id}");
        self.hub.settings.own(id);
        let ctx = Ctx::new(id, self.hub.clone());
        let l = Loaded {
            routes: driver.routes(),
            sockets: driver.sockets(),
            tools: driver.tools().into_iter().map(Arc::new).collect(),
            driver,
            ctx,
            started: AsyncMutex::new(false),
            started_flag: Default::default(),
        };
        self.loaded.push(Arc::new(l));
    }

    pub fn ids(&self) -> Vec<&'static str> {
        self.loaded.iter().map(|l| l.driver.id()).collect()
    }

    pub fn get(&self, id: &str) -> Option<&Arc<Loaded>> {
        self.loaded.iter().find(|l| l.driver.id() == id)
    }

    /// Load the udev database, follow hotplug, and start every enabled driver
    /// concurrently (one slow init mustn't hold up the rest).
    pub async fn start(&self) {
        self.hub.udev.load().await;
        let loaded = self.loaded.clone();
        let dispatch = Arc::new(move |ev: crate::udev::UdevEvent| {
            for l in &loaded {
                if !l.started() {
                    continue;
                }
                if std::panic::catch_unwind(AssertUnwindSafe(|| l.driver.udev(&ev))).is_err() {
                    tracing::error!("[{}] udev handler panicked", l.driver.id());
                }
            }
        });
        tokio::spawn(self.hub.udev.clone().monitor(dispatch, self.stop_tx.subscribe()));
        futures_util::future::join_all(self.loaded.iter().map(|l| self.start_driver(l))).await;
    }

    async fn start_driver(&self, l: &Arc<Loaded>) {
        let mut started = l.started.lock().await;
        if *started || (l.driver.listed() && self.hub.settings.is_disabled(l.driver.id())) {
            return;
        }
        *started = true;
        l.started_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Err(e) = l.driver.init(l.ctx.clone()).await {
            tracing::error!("[{}] init failed: {e:#}", l.driver.id());
            self.hub.timeline.add(l.driver.id(), &format!("Driver failed to start: {e:#}"), None);
        }
    }

    async fn stop_driver(&self, l: &Arc<Loaded>, reason: StopReason) {
        let mut started = l.started.lock().await;
        if !*started {
            return;
        }
        *started = false;
        l.started_flag.store(false, std::sync::atomic::Ordering::SeqCst);
        l.driver.stop(reason).await;
        l.ctx.kill_children();
    }

    /// Enable or disable a driver (`POST /api/periph/driver`).
    pub async fn set_disabled(&self, id: &str, disabled: bool) -> Result<(), HttpError> {
        let l = self.get(id).filter(|l| l.driver.listed()).ok_or_else(|| HttpError::new(404, format!("No driver \"{id}\"")))?.clone();
        self.hub.settings.set_disabled(id, disabled);
        if disabled {
            self.stop_driver(&l, StopReason::Disable).await;
        } else {
            self.start_driver(&l).await;
        }
        self.hub.changed();
        Ok(())
    }

    /// Stop every driver (service shutdown) and the udev monitor.
    pub async fn shutdown(&self) {
        let _ = self.stop_tx.send(true);
        futures_util::future::join_all(self.loaded.iter().map(|l| self.stop_driver(l, StopReason::Shutdown))).await;
        self.hub.settings.flush();
    }

    /// Every driver's list row and state, in dashboard order (stable for equal orders).
    pub fn status_items(&self) -> Vec<ListItem> {
        let mut items: Vec<ListItem> = self
            .loaded
            .iter()
            .filter(|l| l.driver.listed())
            .map(|l| {
                let d = &l.driver;
                let st = if l.started() {
                    match std::panic::catch_unwind(AssertUnwindSafe(|| d.status())) {
                        Ok(s) => s,
                        Err(_) => Some(json!({ "error": "status() panicked" })),
                    }
                } else {
                    None
                };
                let active = st.as_ref().and_then(|s| s.get("active")).is_some_and(crate::http::truthy);
                let summary = st.as_ref().and_then(|s| s.get("summary")).and_then(Value::as_str).unwrap_or("").to_string();
                ListItem {
                    id: d.id().into(),
                    order: d.order(),
                    entry: json!({ "id": d.id(), "title": d.title(), "description": d.description(), "disabled": self.hub.settings.is_disabled(d.id()), "active": active, "summary": summary }),
                    state: st,
                }
            })
            .collect();
        items.sort_by_key(|i| i.order);
        items
    }

    /// `{ list, state }` as core.ts `status()` returns it.
    pub fn status(&self) -> Value {
        assemble(self.status_items())
    }

    /// The route for this request: the most literal match wins (fewest :params), so
    /// "/api/gpio/reboot" isn't swallowed by "/api/gpio/:gpio".
    pub fn find_route(&self, method: &Method, path: &str) -> Option<RouteMatch> {
        let mut best: Option<(RouteMatch, usize)> = None;
        for l in &self.loaded {
            for (i, r) in l.routes.iter().enumerate() {
                if r.method != *method {
                    continue;
                }
                let Some(params) = match_path(&r.path, path) else { continue };
                let n = params.len();
                if best.as_ref().is_none_or(|(_, bn)| n < *bn) {
                    best = Some((RouteMatch { loaded: l.clone(), index: i, params }, n));
                }
            }
        }
        best.map(|(m, _)| m)
    }

    /// Whether any driver (started or not) has a route at this path, for any method.
    pub fn has_path(&self, path: &str) -> bool {
        self.loaded.iter().any(|l| l.routes.iter().any(|r| match_path(&r.path, path).is_some()))
    }

    /// The WebSocket handler for a /ws/ path, among started drivers.
    pub fn find_socket(&self, path: &str) -> Option<(SocketHandler, HashMap<String, String>)> {
        for l in &self.loaded {
            if !l.started() {
                continue;
            }
            for s in &l.sockets {
                if let Some(p) = match_path(&s.path, path) {
                    return Some((s.handler.clone(), p));
                }
            }
        }
        None
    }

    /// Whether any driver, started or not, owns this WebSocket path.
    pub fn owns_socket(&self, path: &str) -> bool {
        self.loaded.iter().any(|l| l.sockets.iter().any(|s| match_path(&s.path, path).is_some()))
    }

    /// MCP tools of started drivers, with their driver ids.
    pub fn tools(&self) -> Vec<(&'static str, Arc<Tool>)> {
        self.loaded.iter().filter(|l| l.started()).flat_map(|l| l.tools.iter().map(move |t| (l.driver.id(), t.clone()))).collect()
    }

    /// Every tool name any driver defines, started or not.
    pub fn all_tool_names(&self) -> Vec<String> {
        self.loaded.iter().flat_map(|l| l.tools.iter().map(|t| t.name.clone())).collect()
    }

    /// Endpoint docs contributed by drivers, merged into GET /api.
    pub fn docs(&self) -> Map<String, Value> {
        let mut out = Map::new();
        for l in &self.loaded {
            for r in &l.routes {
                if let Some(d) = &r.doc {
                    out.insert(format!("{} {}", r.method, r.path), d.clone());
                }
            }
            for s in &l.sockets {
                if let Some(d) = &s.doc {
                    out.insert(format!("WS {}", s.path), Value::String(d.clone()));
                }
            }
        }
        out
    }
}

/// `{ list: [...entries], state: { id: status } }` from ordered items.
pub fn assemble(items: Vec<ListItem>) -> Value {
    let mut list = Vec::new();
    let mut state = Map::new();
    for i in items {
        list.push(i.entry);
        if let Some(s) = i.state {
            state.insert(i.id, s);
        }
    }
    json!({ "list": list, "state": state })
}
