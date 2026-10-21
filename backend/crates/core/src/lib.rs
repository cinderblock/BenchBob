//! The parts of BenchBob every panel shares, ported from `peripherals/core.ts` and
//! `paths.ts`:
//!
//! - [`driver`]: the [`Driver`] trait a panel implements, with its routes, WebSockets and
//!   MCP tools;
//! - [`registry`]: discovers nothing (drivers are registered by the binary) but starts and
//!   stops them, routes requests to them and assembles their status;
//! - [`settings`]: `peripherals.json`, merged on write so entries for drivers this build
//!   doesn't run survive;
//! - [`udev`]: the udev device database, kept live from `udevadm monitor`;
//! - [`mcp`]: the stateless Streamable HTTP JSON-RPC endpoint;
//! - [`http`], [`run`], [`paths`]: request/response helpers, child processes, state dir;
//! - [`proc`]: long-running children read line by line (the on-demand tools' servers);
//! - [`child`]: how children end when dropped: SIGTERM, a grace period, then SIGKILL (never a
//!   bare SIGKILL, which would orphan the program a `sudo` runs);
//! - [`util`]: udev strings and USB names, JavaScript numbers and dates, base64.
//!
//! Contract every driver keeps (as in the TypeScript server): `status()` is called every
//! ~2 s for the SSE stream and must be cheap (cached state, never a process); anything
//! long-lived is released in `stop()`; call `ctx.changed()` after a change worth showing
//! right away.

pub mod child;
pub mod driver;
pub mod http;
pub mod mcp;
pub mod paths;
pub mod proc;
pub mod registry;
pub mod run;
pub mod settings;
pub mod timeline;
pub mod udev;
pub mod util;

pub use driver::{Child, Ctx, Driver, Route, Socket, StopReason, Tool, ToolResult, WsClient, WsIn, WsSession};
pub use http::{HttpError, Reply, Request, RouteResult, json};
pub use registry::Registry;

/// A JSON value; every API body is one.
pub type Json = serde_json::Value;
