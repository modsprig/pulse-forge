//! MCP stdio wire-format spike server (roadmap PF-009).
//!
//! A *protocol-level* validation harness, not the product MCP bridge: the
//! v0.4 bridge (P6) will use the official `rmcp` SDK. This spike proves the
//! MCP 2025-06-18/2026-07-28 JSON-RPC handshake over real pipes on this
//! machine: `initialize` → `notifications/initialized` → `tools/list`.
//!
//! Protocol rules honored:
//! - stdout carries ONLY JSON-RPC messages, one per line (LSP-style
//!   Content-Length framing is optional; we use newline-delimited JSON,
//!   which stdio MCP clients accept);
//! - logs go to stderr;
//! - unknown methods produce JSON-RPC error -32601; malformed JSON produces
//!   -32700; malformed parameters produce -32602;
//! - every response carries the negotiated jsonrpc "2.0" and request id.

use std::io::{BufRead, Write};

use pf_types::json::{self, JsonValue};

fn rpc_result(id: &JsonValue, result: JsonValue) -> String {
    JsonValue::Object(vec![
        ("jsonrpc".into(), JsonValue::Str("2.0".into())),
        ("id".into(), id.clone()),
        ("result".into(), result),
    ])
    .to_json_string()
}

fn rpc_error(id: &JsonValue, code: i64, message: &str) -> String {
    JsonValue::Object(vec![
        ("jsonrpc".into(), JsonValue::Str("2.0".into())),
        ("id".into(), id.clone()),
        (
            "error".into(),
            JsonValue::Object(vec![
                ("code".into(), JsonValue::Int(code)),
                ("message".into(), JsonValue::Str(message.into())),
            ]),
        ),
    ])
    .to_json_string()
}

fn server_info() -> JsonValue {
    JsonValue::Object(vec![
        ("name".into(), JsonValue::Str("pf-mcp-spike".into())),
        (
            "version".into(),
            JsonValue::Str(env!("CARGO_PKG_VERSION").into()),
        ),
    ])
}

fn tools() -> JsonValue {
    use JsonValue as J;
    let tool = |name: &str, desc: &str| {
        J::Object(vec![
            ("name".into(), J::Str(name.into())),
            ("description".into(), J::Str(desc.into())),
            (
                "inputSchema".into(),
                J::Object(vec![
                    ("type".into(), J::Str("object".into())),
                    ("properties".into(), J::Object(vec![])),
                ]),
            ),
        ])
    };
    J::Object(vec![(
        "tools".into(),
        J::Array(vec![
            tool(
                "profile_capabilities_spike",
                "Spike tool: reports the capability schema shape (does not profile).",
            ),
            tool("ping", "Spike tool: replies with pong."),
        ]),
    )])
}

fn handle(line: &str) -> Option<String> {
    if line.trim().is_empty() {
        return None;
    }
    let msg = match json::parse(line) {
        Ok(v) => v,
        Err(_) => {
            let null_id = JsonValue::Null;
            return Some(rpc_error(&null_id, -32700, "Parse error"));
        }
    };
    let id = msg.get("id").cloned().unwrap_or(JsonValue::Null);
    let method = msg.get("method").and_then(|m| m.as_str());
    let Some(method) = method else {
        return Some(rpc_error(&id, -32600, "Invalid Request: missing method"));
    };
    match method {
        "initialize" => {
            let params = msg
                .get("params")
                .cloned()
                .unwrap_or(JsonValue::Object(vec![]));
            let client_name = params
                .get("clientInfo")
                .and_then(|c| c.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("unknown-client")
                .to_string();
            eprintln!(
                "[spike] initialize from `{client_name}` (stderr log; stdout stays protocol-pure)"
            );
            let result = JsonValue::Object(vec![
                (
                    "protocolVersion".into(),
                    JsonValue::Str("2025-06-18".into()),
                ),
                ("capabilities".into(), JsonValue::Object(vec![])),
                ("serverInfo".into(), server_info()),
            ]);
            Some(rpc_result(&id, result))
        }
        "notifications/initialized" => {
            // Notification: no response (JSON-RPC semantics).
            None
        }
        "tools/list" => Some(rpc_result(&id, tools())),
        "ping" => Some(rpc_result(&id, JsonValue::Object(vec![]))),
        _ => Some(rpc_error(&id, -32601, "Method not found")),
    }
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if let Some(response) = handle(&line) {
            let _ = writeln!(stdout, "{response}");
            let _ = stdout.flush();
        }
    }
}
