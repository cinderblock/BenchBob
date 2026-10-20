//! The parts of BenchBob every panel shares, ported from `peripherals/core.ts` and
//! `paths.ts`:
//!
//! - [`driver`]: the [`Driver`] trait a panel implements, with its routes, WebSockets and
//!   MCP tools;
//! - [`registry`]: discovers nothing (drivers are registered by the binary) but starts and
//!   stops them, routes requests to them and assembles their status;
//! - [`settings`]: `peripherals.json`, merged on write so another process (the Bun server
//!   in proxy mode) can keep its own drivers' entries in the same file;
//! - [`udev`]: the udev device database, kept live from `udevadm monitor`;
//! - [`mcp`]: the stateless Streamable HTTP JSON-RPC endpoint;
//! - [`http`], [`run`], [`paths`]: request/response helpers, child processes, state dir.
//!
//! Contract every driver keeps (as in the TypeScript server): `status()` is called every
//! ~2 s for the SSE stream and must be cheap (cached state, never a process); anything
//! long-lived is released in `stop()`; call `ctx.changed()` after a change worth showing
//! right away.

pub mod driver;
pub mod http;
pub mod mcp;
pub mod paths;
pub mod registry;
pub mod run;
pub mod settings;
pub mod timeline;
pub mod udev;

pub use driver::{Child, Ctx, Driver, Route, Socket, StopReason, Tool, ToolResult, WsClient, WsIn, WsSession};
pub use http::{HttpError, Reply, Request, RouteResult, json};
pub use registry::Registry;

/// A JSON value; every API body is one.
pub type Json = serde_json::Value;
