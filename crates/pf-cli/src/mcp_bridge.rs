//! MCP stdio bridge onto the recording daemon (roadmap P6).
//!
//! Contract implemented here (architecture §12, ADR-008/014):
//! - **stdio only**; stdout carries JSON-RPC messages exclusively (logs go
//!   to stderr) — verified by tests;
//! - tools are the counters-safe set: `profile_capabilities`,
//!   `profile_list_processes`, `profile_list_sessions`, `profile_attach`,
//!   `profile_status`, `profile_stop`, `profile_summary`,
//!   `profile_diagnostics`;
//! - **`profile_start` (launch) is NOT advertised** — execution from an
//!   agent host requires an explicit trust policy that does not exist yet
//!   (ADR-008). It is absent from `tools/list`, not merely discouraged;
//! - every tool response is bounded (process list ≤ 100 rows, sessions ≤
//!   100, summaries one JSON object);
//! - unknown methods → −32601, malformed JSON → −32700, bad arguments →
//!   typed tool error; the connection survives every error;
//! - each tool call opens a fresh, handshaked daemon connection (v1
//!   simplicity; connection pooling arrives with the daemon-era rewrite).

use std::io::{BufRead, Write};

use pf_types::json::{self, JsonValue};
use pf_types::{ErrorCode, PfError};

const PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_LINE_BYTES: usize = 1024 * 1024; // 1 MiB inbound line cap
const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024; // default target per §12.1

/// Tool metadata: name, description, input schema.
struct ToolDef {
    name: &'static str,
    description: &'static str,
    input_schema: JsonValue,
}

fn obj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn schema(properties: Vec<(&str, &str, &str, JsonValue)>) -> JsonValue {
    // (name, (type, description, required?))
    let props: Vec<(String, JsonValue)> = properties
        .iter()
        .map(|(n, t, d, _req)| {
            (
                n.to_string(),
                obj(vec![
                    ("type", JsonValue::Str(t.to_string())),
                    ("description", JsonValue::Str(d.to_string())),
                ]),
            )
        })
        .collect();
    let required: Vec<JsonValue> = properties
        .iter()
        .filter(|(_, _, _, req)| matches!(req, JsonValue::Bool(true)))
        .map(|(n, _, _, _)| JsonValue::Str(n.to_string()))
        .collect();
    obj(vec![
        ("type", JsonValue::Str("object".into())),
        ("properties", JsonValue::Object(props)),
        ("required", JsonValue::Array(required)),
        ("additionalProperties", JsonValue::Bool(false)),
    ])
}

fn tools() -> Vec<ToolDef> {
    use JsonValue as J;
    vec![
        ToolDef {
            name: "profile_capabilities",
            description: "Report what this machine can profile right now: OS, counters/stack availability with reasons, CPU count.",
            input_schema: schema(vec![]),
        },
        ToolDef {
            name: "profile_list_processes",
            description: "List visible processes (bounded to 100 rows): pid, parent pid, image name. Paths may be sensitive.",
            input_schema: schema(vec![(
                "limit",
                "integer",
                "Max rows (1..=100, default 100)",
                J::Bool(false),
            )]),
        },
        ToolDef {
            name: "profile_list_sessions",
            description: "List recording sessions known to the daemon (bounded to 100, newest last).",
            input_schema: schema(vec![]),
        },
        ToolDef {
            name: "profile_attach",
            description: "Attach counters recording to an existing PID (the target is never signalled). Duration is bounded (1..=3600000 ms).",
            input_schema: schema(vec![
                ("pid", "integer", "Process id to attach to", J::Bool(true)),
                (
                    "duration_ms",
                    "integer",
                    "Recording budget in ms (default 10000)",
                    J::Bool(false),
                ),
                (
                    "interval_ms",
                    "integer",
                    "Poll interval ms (100..=5000, default 250)",
                    J::Bool(false),
                ),
                (
                    "label",
                    "string",
                    "Free-form label for the session",
                    J::Bool(false),
                ),
            ]),
        },
        ToolDef {
            name: "profile_status",
            description: "Session state, sample count, stop reason, session directory.",
            input_schema: schema(vec![(
                "session_id",
                "string",
                "Session id from profile_attach",
                J::Bool(true),
            )]),
        },
        ToolDef {
            name: "profile_stop",
            description: "Stop a recording (never the target process). Idempotent.",
            input_schema: schema(vec![("session_id", "string", "Session id", J::Bool(true))]),
        },
        ToolDef {
            name: "profile_summary",
            description: "Metric aggregates for a session (units + availability states). CPU stacks are not claimed: this is counters data.",
            input_schema: schema(vec![("session_id", "string", "Session id", J::Bool(true))]),
        },
        ToolDef {
            name: "profile_top_functions",
            description: "Top functions by exclusive/inclusive sample weight from a session's sampled CPU stacks (Linux perf sessions). Bounded rows; includes symbol-coverage quality fields. Errors honestly for counters-only sessions.",
            input_schema: schema(vec![
                (
                    "session_id",
                    "string",
                    "Session id from profile_attach",
                    J::Bool(true),
                ),
                (
                    "limit",
                    "integer",
                    "Max rows per list (1..=64, default 32)",
                    J::Bool(false),
                ),
            ]),
        },
        ToolDef {
            name: "profile_get_stack",
            description: "Sample callchains containing a given function from a session's sampled CPU stacks (bounded count and depth, outermost-first). Includes symbol-coverage quality fields.",
            input_schema: schema(vec![
                (
                    "session_id",
                    "string",
                    "Session id from profile_attach",
                    J::Bool(true),
                ),
                (
                    "function",
                    "string",
                    "Function symbol name as reported by profile_top_functions",
                    J::Bool(true),
                ),
                (
                    "limit",
                    "integer",
                    "Max sample stacks returned (1..=16, default 8)",
                    J::Bool(false),
                ),
            ]),
        },
        ToolDef {
            name: "profile_diagnostics",
            description: "Daemon/backend health: capabilities, protocol version, launch-tool policy.",
            input_schema: schema(vec![]),
        },
        ToolDef {
            name: "profile_compare",
            description: "Observed-delta comparison of two recorded sessions by id (wall time, CPU, memory, I/O). Single pairs are NOT statistically significant; use repeated bench runs for distributions.",
            input_schema: schema(vec![
                (
                    "baseline_session_id",
                    "string",
                    "Session id of the baseline",
                    J::Bool(true),
                ),
                (
                    "candidate_session_id",
                    "string",
                    "Session id of the candidate",
                    J::Bool(true),
                ),
                (
                    "force",
                    "boolean",
                    "Compare even when scopes are incompatible (result is flagged non-comparable)",
                    J::Bool(false),
                ),
            ]),
        },
    ]
}

fn rpc_result(id: &JsonValue, result: JsonValue) -> String {
    obj(vec![
        ("jsonrpc", JsonValue::Str("2.0".into())),
        ("id", id.clone()),
        ("result", result),
    ])
    .to_json_string()
}

fn rpc_error(id: &JsonValue, code: i64, message: &str) -> String {
    obj(vec![
        ("jsonrpc", JsonValue::Str("2.0".into())),
        ("id", id.clone()),
        (
            "error",
            obj(vec![
                ("code", JsonValue::Int(code)),
                ("message", JsonValue::Str(message.to_string())),
            ]),
        ),
    ])
    .to_json_string()
}

/// Tool-level error carried inside a successful JSON-RPC response (the
/// MCP pattern: tool errors are content, not transport errors).
fn tool_error(id: &JsonValue, code: ErrorCode, message: &str) -> String {
    let err = obj(vec![
        ("code", JsonValue::Str(code.as_str().to_string())),
        ("message", JsonValue::Str(message.to_string())),
    ]);
    rpc_result(
        id,
        obj(vec![
            (
                "content",
                JsonValue::Array(vec![JsonValue::Object(vec![
                    ("type".into(), JsonValue::Str("text".into())),
                    ("text".into(), JsonValue::Str(message.to_string())),
                ])]),
            ),
            ("isError", JsonValue::Bool(true)),
            ("structuredError", err),
        ]),
    )
}

fn text_result(id: &JsonValue, text: String) -> String {
    // Enforce the response budget: hard-truncate with an explicit marker
    // (never silently).
    let text = if text.len() > MAX_TOOL_RESULT_BYTES {
        let mut cut = text[..MAX_TOOL_RESULT_BYTES].to_string();
        cut.push_str("…[truncated at 64 KiB budget]");
        cut
    } else {
        text
    };
    rpc_result(
        id,
        obj(vec![
            (
                "content",
                JsonValue::Array(vec![obj(vec![
                    ("type", JsonValue::Str("text".into())),
                    ("text", JsonValue::Str(text)),
                ])]),
            ),
            ("isError", JsonValue::Bool(false)),
        ]),
    )
}

struct DaemonClient {
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
    data_root: std::path::PathBuf,
}

impl DaemonClient {
    fn new() -> Result<DaemonClient, PfError> {
        let data_root = pf_platform::paths::data_root()
            .map_err(|e| PfError::new(ErrorCode::StorageIo, format!("data root: {e}")))?;
        Ok(DaemonClient { data_root })
    }

    /// One handshaked request round-trip per call (v1 policy: simple,
    /// stateless, reconnect-tolerant).
    fn call(&self, op: &str, args: JsonValue) -> Result<JsonValue, PfError> {
        #[cfg(windows)]
        {
            use pf_daemon::ipc;
            let name = pf_windows::pipe::daemon_pipe_name(&self.data_root);
            let mut stream = pf_windows::pipe::connect(&name)?;
            ipc::write_frame(&mut stream, &ipc::client_handshake("pulseforge-mcp"))?;
            let ack = ipc::read_frame(&mut stream)?;
            if ack.get("type").and_then(|v| v.as_str()) != Some("handshake_ok") {
                return Err(PfError::new(
                    ErrorCode::BackendFailure,
                    "daemon rejected the MCP bridge handshake",
                ));
            }
            ipc::write_frame(&mut stream, &ipc::request(1, op, args))?;
            let reply = ipc::read_frame(&mut stream)?;
            if reply.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                Ok(reply.get("result").cloned().unwrap_or(JsonValue::Null))
            } else {
                let code = reply
                    .get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("BACKEND_FAILURE");
                let msg = reply
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("daemon request failed");
                Err(PfError::new(parse_daemon_code(code), msg))
            }
        }
        #[cfg(target_os = "linux")]
        {
            use pf_daemon::ipc;
            let socket = pf_linux::uds::daemon_uds_path(&self.data_root);
            let mut stream = pf_linux::uds::connect(&socket)?;
            ipc::write_frame(&mut stream, &ipc::client_handshake("pulseforge-mcp"))?;
            let ack = ipc::read_frame(&mut stream)?;
            if ack.get("type").and_then(|v| v.as_str()) != Some("handshake_ok") {
                return Err(PfError::new(
                    ErrorCode::BackendFailure,
                    "daemon rejected the MCP bridge handshake",
                ));
            }
            ipc::write_frame(&mut stream, &ipc::request(1, op, args))?;
            let reply = ipc::read_frame(&mut stream)?;
            if reply.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                Ok(reply.get("result").cloned().unwrap_or(JsonValue::Null))
            } else {
                let code = reply
                    .get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("BACKEND_FAILURE");
                let msg = reply
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("daemon request failed");
                Err(PfError::new(parse_daemon_code(code), msg))
            }
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = (op, args);
            Err(PfError::new(
                ErrorCode::UnsupportedPlatform,
                "the MCP bridge requires the Windows named-pipe or Linux UDS daemon transport",
            ))
        }
    }
}

#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
fn parse_daemon_code(code: &str) -> ErrorCode {
    use ErrorCode::*;
    match code {
        "INVALID_ARGUMENT" => InvalidArgument,
        "PERMISSION_DENIED" => PermissionDenied,
        "CAPABILITY_UNAVAILABLE" => CapabilityUnavailable,
        "TARGET_EXITED" => TargetExited,
        "IDENTITY_MISMATCH" => IdentityMismatch,
        "SESSION_NOT_FOUND" => SessionNotFound,
        "PARTIAL_DATA" => PartialData,
        "RESOURCE_EXHAUSTED" => ResourceExhausted,
        "TIMEOUT" => Timeout,
        "STORAGE_IO" => StorageIo,
        "UNSUPPORTED_PLATFORM" => UnsupportedPlatform,
        _ => BackendFailure,
    }
}

fn require_u64(args: &JsonValue, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a non-negative integer")),
    }
}

fn require_str<'a>(args: &'a JsonValue, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a string")),
    }
}

/// Execute one tool call; the result is the MCP response line.
fn call_tool(id: &JsonValue, name: &str, args: &JsonValue) -> String {
    let client = match DaemonClient::new() {
        Ok(c) => c,
        Err(e) => return tool_error(id, e.code, &e.to_string()),
    };
    let result: Result<JsonValue, PfError> = match name {
        "profile_capabilities" => client.call("capabilities", JsonValue::Object(vec![])),
        "profile_diagnostics" => client.call("capabilities", JsonValue::Object(vec![])).map(|caps| {
            obj(vec![
                ("daemon_capabilities", caps),
                ("bridge_protocol", JsonValue::Str(PROTOCOL_VERSION.into())),
                (
                    "launch_tool",
                    JsonValue::Str("disabled by default (ADR-008: agent execution needs an explicit trust policy)".into()),
                ),
            ])
        }),
        "profile_list_processes" => {
            let limit = match require_u64(args, "limit") {
                Ok(v) => v.unwrap_or(100).min(100),
                Err(e) => return tool_error(id, ErrorCode::InvalidArgument, &e),
            };
            client.call(
                "list_processes",
                obj(vec![("limit", JsonValue::UInt(limit))]),
            )
        }
        "profile_list_sessions" => client.call("list_sessions", JsonValue::Object(vec![])).map(|s| {
            let mut arr = s.as_array().map(|a| a.to_vec()).unwrap_or_default();
            if arr.len() > 100 {
                arr.truncate(100);
                arr.push(JsonValue::Str("…[truncated to 100 sessions]".into()));
            }
            JsonValue::Array(arr)
        }),
        "profile_attach" => {
            let pid = match require_u64(args, "pid") {
                Ok(Some(p)) => p,
                Ok(None) => {
                    return tool_error(
                        id,
                        ErrorCode::InvalidArgument,
                        "`pid` is required for profile_attach",
                    )
                }
                Err(e) => return tool_error(id, ErrorCode::InvalidArgument, &e),
            };
            let duration = match require_u64(args, "duration_ms") {
                Ok(v) => v.unwrap_or(10_000),
                Err(e) => return tool_error(id, ErrorCode::InvalidArgument, &e),
            };
            if !(1..=3_600_000).contains(&duration) {
                return tool_error(
                    id,
                    ErrorCode::InvalidArgument,
                    "duration_ms must be within 1..=3600000",
                );
            }
            let interval = match require_u64(args, "interval_ms") {
                Ok(v) => v.unwrap_or(250),
                Err(e) => return tool_error(id, ErrorCode::InvalidArgument, &e),
            };
            if !(100..=5_000).contains(&interval) {
                return tool_error(
                    id,
                    ErrorCode::InvalidArgument,
                    "interval_ms must be within 100..=5000",
                );
            }
            let label = match require_str(args, "label") {
                Ok(v) => v.map(str::to_string),
                Err(e) => return tool_error(id, ErrorCode::InvalidArgument, &e),
            };
            // Attach = create + start in one call; the daemon owns it from
            // here on, surviving this bridge process.
            client
                .call(
                    "create_session",
                    obj(vec![("pid", JsonValue::UInt(pid))]),
                )
                .and_then(|created| {
                    let sid = created
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            PfError::new(ErrorCode::BackendFailure, "no session id from daemon")
                        })?
                        .to_string();
                    let mut start_args = vec![
                        ("session_id".to_string(), JsonValue::Str(sid.clone())),
                        ("duration_ms".to_string(), JsonValue::UInt(duration)),
                        ("interval_ms".to_string(), JsonValue::UInt(interval)),
                    ];
                    if let Some(l) = label {
                        start_args.push(("label".to_string(), JsonValue::Str(l)));
                    }
                    client
                        .call("start_recording", JsonValue::Object(start_args))
                        .map(|started| {
                            obj(vec![
                                ("session_id", JsonValue::Str(sid)),
                                ("state", started.get("state").cloned().unwrap_or(JsonValue::Null)),
                                (
                                    "note",
                                    JsonValue::Str(
                                        "counters recording; the daemon owns it and it survives disconnects; stop with profile_stop"
                                            .into(),
                                    ),
                                ),
                            ])
                        })
                })
        }
        "profile_compare" => {
            let a = match require_str(args, "baseline_session_id") {
                Ok(Some(s)) => s.to_string(),
                Ok(None) => {
                    return tool_error(
                        id,
                        ErrorCode::InvalidArgument,
                        "`baseline_session_id` is required",
                    )
                }
                Err(e) => return tool_error(id, ErrorCode::InvalidArgument, &e),
            };
            let b = match require_str(args, "candidate_session_id") {
                Ok(Some(s)) => s.to_string(),
                Ok(None) => {
                    return tool_error(
                        id,
                        ErrorCode::InvalidArgument,
                        "`candidate_session_id` is required",
                    )
                }
                Err(e) => return tool_error(id, ErrorCode::InvalidArgument, &e),
            };
            let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            client.call(
                "compare_sessions",
                obj(vec![
                    ("baseline", JsonValue::Str(a)),
                    ("candidate", JsonValue::Str(b)),
                    ("force", JsonValue::Bool(force)),
                ]),
            )
        }
        "profile_top_functions" => {
            let sid = match require_str(args, "session_id") {
                Ok(Some(s)) => s.to_string(),
                Ok(None) => {
                    return tool_error(
                        id,
                        ErrorCode::InvalidArgument,
                        "`session_id` is required",
                    )
                }
                Err(e) => return tool_error(id, ErrorCode::InvalidArgument, &e),
            };
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(32);
            client.call(
                "query_top_functions",
                obj(vec![
                    ("session_id", JsonValue::Str(sid)),
                    ("limit", JsonValue::UInt(limit)),
                ]),
            )
        }
        "profile_get_stack" => {
            let sid = match require_str(args, "session_id") {
                Ok(Some(s)) => s.to_string(),
                Ok(None) => {
                    return tool_error(
                        id,
                        ErrorCode::InvalidArgument,
                        "`session_id` is required",
                    )
                }
                Err(e) => return tool_error(id, ErrorCode::InvalidArgument, &e),
            };
            let function = match require_str(args, "function") {
                Ok(Some(s)) => s.to_string(),
                Ok(None) => {
                    return tool_error(
                        id,
                        ErrorCode::InvalidArgument,
                        "`function` is required",
                    )
                }
                Err(e) => return tool_error(id, ErrorCode::InvalidArgument, &e),
            };
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(8);
            client.call(
                "get_stack",
                obj(vec![
                    ("session_id", JsonValue::Str(sid)),
                    ("function", JsonValue::Str(function)),
                    ("limit", JsonValue::UInt(limit)),
                ]),
            )
        }
        "profile_status" | "profile_stop" | "profile_summary" => {
            let sid = match require_str(args, "session_id") {
                Ok(Some(s)) => s.to_string(),
                Ok(None) => {
                    return tool_error(
                        id,
                        ErrorCode::InvalidArgument,
                        "`session_id` is required",
                    )
                }
                Err(e) => return tool_error(id, ErrorCode::InvalidArgument, &e),
            };
            let op = match name {
                "profile_status" => "get_session",
                "profile_stop" => "stop_recording",
                _ => "query_metrics",
            };
            client.call(
                op,
                obj(vec![("session_id", JsonValue::Str(sid))]),
            )
        }
        other => {
            return tool_error(
                id,
                ErrorCode::InvalidArgument,
                &format!("unknown tool `{other}`"),
            )
        }
    };
    match result {
        Ok(v) => text_result(id, v.to_json_string()),
        Err(e) => tool_error(id, e.code, &e.to_string()),
    }
}

fn handle_line(line: &str) -> Option<String> {
    if line.trim().is_empty() {
        return None;
    }
    let msg = match json::parse(line) {
        Ok(v) => v,
        Err(_) => return Some(rpc_error(&JsonValue::Null, -32700, "Parse error")),
    };
    let id = msg.get("id").cloned().unwrap_or(JsonValue::Null);
    let method = msg.get("method").and_then(|m| m.as_str());
    let Some(method) = method else {
        return Some(rpc_error(&id, -32600, "Invalid Request: missing method"));
    };
    match method {
        "initialize" => Some(rpc_result(
            &id,
            obj(vec![
                ("protocolVersion", JsonValue::Str(PROTOCOL_VERSION.into())),
                ("capabilities", obj(vec![])),
                (
                    "serverInfo",
                    obj(vec![
                        ("name", JsonValue::Str("pulseforge-mcp".into())),
                        ("version", JsonValue::Str(env!("CARGO_PKG_VERSION").into())),
                    ]),
                ),
            ]),
        )),
        "notifications/initialized" | "notifications/cancelled" => None,
        "ping" => Some(rpc_result(&id, obj(vec![]))),
        "tools/list" => {
            let defs: Vec<JsonValue> = tools()
                .iter()
                .map(|t| {
                    obj(vec![
                        ("name", JsonValue::Str(t.name.to_string())),
                        ("description", JsonValue::Str(t.description.to_string())),
                        ("inputSchema", t.input_schema.clone()),
                    ])
                })
                .collect();
            Some(rpc_result(
                &id,
                obj(vec![("tools", JsonValue::Array(defs))]),
            ))
        }
        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or(JsonValue::Null);
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(JsonValue::Object(vec![]));
            Some(call_tool(&id, name, &args))
        }
        other => Some(rpc_error(
            &id,
            -32601,
            &format!("Method not found: {other}"),
        )),
    }
}

/// Entry point for `pulseforge mcp`: newline-delimited JSON-RPC on stdio.
pub fn run() -> Result<(), PfError> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut line = String::new();
    let mut reader = stdin.lock();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF: host closed the pipe
            Ok(_) => {}
            Err(e) => {
                eprintln!("mcp: stdin error: {e}");
                break;
            }
        }
        if line.len() > MAX_LINE_BYTES {
            let _ = writeln!(
                stdout,
                "{}",
                rpc_error(&JsonValue::Null, -32600, "Request too large")
            );
            continue;
        }
        if let Some(reply) = handle_line(line.trim_end()) {
            if writeln!(stdout, "{reply}")
                .and_then(|_| stdout.flush())
                .is_err()
            {
                break; // host went away
            }
        }
    }
    Ok(())
}
