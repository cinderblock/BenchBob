//! `GET /api`: the endpoint list, with the board this is. Example URLs name the host the
//! caller reached us by, so they paste and run as-is.

use serde_json::{Map, Value, json};

/// This server's own endpoints (the drivers' come from the registry).
pub fn own_endpoints() -> Map<String, Value> {
    let v = json!({
        "GET /api": "This documentation",
        "GET /api/status": "System status snapshot (same data as SSE). board: { id, name, model, tested, capabilities } says which board file BenchBob picked and what it supports; sensors: thermal/hwmon/power_supply readings",
        "GET /api/events": "SSE stream, sends status every ~2s",
        "GET /api/boards": "Every board definition BenchBob loaded (TOML files in /usr/share/benchbob/boards and /etc/benchbob/boards.d), which one this machine matched, and files that failed to load",
    });
    v.as_object().unwrap().clone()
}

/// The registry's own endpoints (core.ts `docs()` head).
pub fn periph_endpoints() -> Map<String, Value> {
    let v = json!({
        "POST /api/periph/driver": { "body": { "id": "gpio", "disabled": true }, "description": "Enable or disable a peripheral driver (disabled drivers release their devices and disappear from the dashboard)" },
        "GET /api/periph/timeline": "Hardware event timeline: USB plug/unplug, kernel USB messages, driver events",
        "POST /mcp": "Model Context Protocol endpoint (Streamable HTTP, stateless JSON). Every peripheral operation below is also an MCP tool.",
    });
    v.as_object().unwrap().clone()
}

/// Assemble the document: the endpoints, the board, and which server this is.
pub fn assemble(endpoints: Map<String, Value>, board: Value, backend: Value, origin: &str) -> String {
    let doc = json!({
        "name": "BenchBob API",
        "version": "1.0",
        "appVersion": benchbob_core::paths::version(),
        "base": "http://benchbob.local",
        "endpoints": endpoints,
        "examples": benchbob_drives::examples(),
        "board": board,
        "backend": backend,
    });
    serde_json::to_string(&doc).unwrap_or_default().replace("http://benchbob.local", origin)
}
