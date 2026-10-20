//! Model Context Protocol over Streamable HTTP, stateless (core.ts's MCP section).
//!
//! Enough of the protocol for agents to discover and call tools: initialize, ping,
//! tools/list, tools/call. No sessions or server-initiated messages, so every request is
//! answered with plain JSON and GET (the SSE side channel) is refused, as the spec allows.
//!
//! The tools come from a [`ToolSource`]: the registry's drivers, or (in proxy mode) those
//! merged with the Bun server's.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use serde_json::{Value, json};

use crate::driver::{Tool, ToolResult};
use crate::http::{json_response, text_response, truthy};

pub const PROTOCOL: &str = "2025-06-18";

pub const INSTRUCTIONS: &str = "BenchBob is a bench server: a Raspberry Pi or another Linux box (GET /api says which board, and whether it has a mapped pin header). These tools reach the peripherals plugged into it (serial ports, CAN, I²C, 1-Wire, GPIO, Wi-Fi, USB, radios). Call a *_list tool first to find device ids.";

#[async_trait]
pub trait ToolSource: Send + Sync {
    /// `[{ name, description, inputSchema }]`
    async fn list(&self) -> Vec<Value>;
    /// The `tools/call` result, or None for an unknown tool.
    async fn call(&self, name: &str, args: Value) -> Option<Value>;
}

/// A tool's `{ name, description, inputSchema }` for tools/list.
pub fn describe(t: &Tool) -> Value {
    json!({ "name": t.name, "description": t.description, "inputSchema": t.input_schema })
}

/// Run a native tool and shape its result as a tools/call result.
pub async fn call_tool(t: &Tool, args: Value) -> Value {
    match (t.handler)(args).await {
        Ok(ToolResult::Content { content, structured }) => {
            let mut r = json!({ "content": content });
            if let Some(s) = structured {
                r["structuredContent"] = s;
            }
            r
        }
        Ok(ToolResult::Value(v)) => {
            let text = match &v {
                Value::String(s) => s.clone(),
                // JSON.stringify(result, null, 2)
                other => serde_json::to_string_pretty(other).unwrap_or_default(),
            };
            let mut r = json!({ "content": [{ "type": "text", "text": text }] });
            if v.is_object() {
                r["structuredContent"] = v;
            }
            r
        }
        Err(e) => json!({ "content": [{ "type": "text", "text": e.message }], "isError": true }),
    }
}

async fn one(msg: &Value, src: &dyn ToolSource) -> Option<Value> {
    // A message without an id is a notification (notifications/initialized…).
    let id = msg.get("id")?.clone();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let reply = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let fail = |code: i32, message: String| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    Some(match method {
        "initialize" => {
            let pv = params.get("protocolVersion").filter(|v| truthy(v)).cloned().unwrap_or_else(|| Value::from(PROTOCOL));
            reply(json!({
                "protocolVersion": pv,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "benchbob", "version": "1.0" },
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => reply(json!({})),
        "tools/list" => reply(json!({ "tools": src.list().await })),
        "tools/call" => {
            let name = params.get("name").cloned().unwrap_or(Value::Null);
            let args = params.get("arguments").filter(|v| truthy(v)).cloned().unwrap_or_else(|| json!({}));
            match name.as_str() {
                Some(n) => match src.call(n, args).await {
                    Some(r) => reply(r),
                    None => fail(-32602, format!("Unknown tool: {n}")),
                },
                None => fail(-32602, format!("Unknown tool: {}", crate::http::js_string(&name).replace("null", "undefined"))),
            }
        }
        other => {
            fail(-32601, format!("Method not found: {}", if msg.get("method").is_some() { other.to_string() } else { "undefined".into() }))
        }
    })
}

/// Answer one HTTP request to /mcp.
pub async fn handle(method: &Method, body: &[u8], src: &dyn ToolSource) -> Response {
    if method == Method::GET {
        let mut r = text_response(405, "This MCP server is stateless; use POST");
        r.headers_mut().insert(header::ALLOW, HeaderValue::from_static("POST"));
        return r;
    }
    if method == Method::DELETE {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
        return r;
    }
    if method != Method::POST {
        return text_response(405, "Method Not Allowed");
    }
    let Ok(msg) = serde_json::from_slice::<Value>(body) else {
        return json_response(400, &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "Parse error" } }));
    };
    let out = match &msg {
        Value::Array(list) => {
            let mut out = Vec::new();
            for m in list {
                if let Some(r) = one(m, src).await {
                    out.push(r);
                }
            }
            (!out.is_empty()).then_some(Value::Array(out))
        }
        m => one(m, src).await,
    };
    let mut r = match out {
        Some(v) => json_response(200, &v),
        None => {
            let mut r = Response::new(Body::empty());
            *r.status_mut() = StatusCode::ACCEPTED;
            r
        }
    };
    r.headers_mut().insert("MCP-Protocol-Version", HeaderValue::from_static(PROTOCOL));
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpError;
    use std::sync::Arc;

    struct Src(Vec<Arc<Tool>>);

    #[async_trait]
    impl ToolSource for Src {
        async fn list(&self) -> Vec<Value> {
            self.0.iter().map(|t| describe(t)).collect()
        }
        async fn call(&self, name: &str, args: Value) -> Option<Value> {
            let t = self.0.iter().find(|t| t.name == name)?;
            Some(call_tool(t, args).await)
        }
    }

    fn src() -> Src {
        Src(vec![
            Arc::new(Tool::new("echo", "Echo", json!({ "type": "object" }), |a| async move { Ok(json!({ "got": a })) })),
            Arc::new(Tool::new("fail", "Fail", json!({ "type": "object" }), |_| async move { Err(HttpError::new(409, "nope")) })),
        ])
    }

    async fn post(body: Value) -> (u16, Option<Value>) {
        let r = handle(&Method::POST, body.to_string().as_bytes(), &src()).await;
        let status = r.status().as_u16();
        let b = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&b).ok())
    }

    #[tokio::test]
    async fn initialize_list_call() {
        let (s, v) = post(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} })).await;
        assert_eq!(s, 200);
        assert_eq!(v.unwrap()["result"]["protocolVersion"], PROTOCOL);
        let (_, v) = post(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
        assert_eq!(v.unwrap()["result"]["tools"][1]["name"], "fail");
        let (_, v) =
            post(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "echo", "arguments": { "x": 1 } } })).await;
        let v = v.unwrap();
        assert_eq!(v["result"]["structuredContent"]["got"]["x"], 1);
        assert_eq!(v["result"]["content"][0]["text"], "{\n  \"got\": {\n    \"x\": 1\n  }\n}");
        let (_, v) = post(json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "fail" } })).await;
        assert_eq!(v.unwrap()["result"], json!({ "content": [{ "type": "text", "text": "nope" }], "isError": true }));
        let (_, v) = post(json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "nope" } })).await;
        assert_eq!(v.unwrap()["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn notifications_batches_and_errors() {
        let (s, v) = post(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
        assert_eq!((s, v), (202, None));
        let (s, v) = post(json!([{ "jsonrpc": "2.0", "id": 1, "method": "ping" }, { "jsonrpc": "2.0", "method": "x" }])).await;
        assert_eq!(s, 200);
        assert_eq!(v.unwrap().as_array().unwrap().len(), 1);
        let r = handle(&Method::POST, b"{not json", &src()).await;
        assert_eq!(r.status().as_u16(), 400);
        let r = handle(&Method::GET, b"", &src()).await;
        assert_eq!(r.status().as_u16(), 405);
        let (_, v) = post(json!({ "jsonrpc": "2.0", "id": 9, "method": "nope" })).await;
        assert_eq!(v.unwrap()["error"]["message"], "Method not found: nope");
    }
}
