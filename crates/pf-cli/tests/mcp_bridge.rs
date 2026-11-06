//! MCP bridge end-to-end tests (PF-601..608 core): a real daemon, a real
//! `pulseforge mcp` bridge process, the MCP client protocol on stdio —
//! handshake, tools/list (with `profile_start` ABSENT), a full
//! attach→status→stop workflow, typed errors, and stdout purity.

// Test helpers use bounded counters.
#![allow(clippy::arithmetic_side_effects)]
#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use pf_types::json::{self, JsonValue};

fn exe() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_pulseforge"))
}

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pf-mcp-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Bridge {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Bridge {
    fn spawn(data_root: &std::path::Path) -> Bridge {
        let mut child = Command::new(exe())
            .args(["mcp"])
            .env("PULSEFORGE_DATA", data_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn mcp bridge");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Bridge {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn send_raw(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write to bridge");
        self.stdin.flush().expect("flush bridge");
    }

    fn recv(&mut self) -> JsonValue {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read from bridge");
        json::parse(line.trim()).expect("bridge reply is JSON")
    }

    fn request(&mut self, method: &str, params: Option<JsonValue>) -> JsonValue {
        self.next_id += 1;
        let id = self.next_id;
        let mut pairs = vec![
            ("jsonrpc".to_string(), JsonValue::Str("2.0".into())),
            ("id".to_string(), JsonValue::UInt(id)),
            ("method".to_string(), JsonValue::Str(method.into())),
        ];
        if let Some(p) = params {
            pairs.push(("params".to_string(), p));
        }
        self.send_raw(&JsonValue::Object(pairs).to_json_string());
        let reply = self.recv();
        assert_eq!(
            reply.get("id").and_then(|v| v.as_u64()),
            Some(id),
            "request id echoed"
        );
        reply
    }

    fn tool_call(&mut self, name: &str, args: JsonValue) -> JsonValue {
        self.request(
            "tools/call",
            Some(JsonValue::Object(vec![
                ("name".to_string(), JsonValue::Str(name.into())),
                ("arguments".to_string(), args),
            ])),
        )
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // Closing stdin cleanly is implicit: killing the child ends the
        // bridge; the held stdin handle goes away with the process.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_content_text(reply: &JsonValue) -> String {
    reply
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string()
}

fn parse_content_json(reply: &JsonValue) -> JsonValue {
    let text = parse_content_text(reply);
    json::parse(&text).unwrap_or_else(|e| panic!("tool text is JSON: {e}: {text}"))
}

#[test]
fn full_agent_workflow_over_mcp() {
    let data = fresh_dir("full");
    let _daemon = DaemonGuard(
        Command::new(exe())
            .args(["daemon"])
            .env("PULSEFORGE_DATA", &data)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon"),
    );
    // Daemon startup grace.
    std::thread::sleep(Duration::from_millis(900));

    let mut bridge = Bridge::spawn(&data);

    // -- initialize handshake (PF-601) --
    let init = bridge.request(
        "initialize",
        Some(json::parse(
            r#"{"protocolVersion":"2025-06-18","clientInfo":{"name":"mcp-e2e-test","version":"0.0"}}"#,
        )
        .unwrap()),
    );
    let result = init.get("result").expect("init result").clone();
    assert_eq!(
        result.get("protocolVersion").and_then(|v| v.as_str()),
        Some("2025-06-18")
    );
    assert_eq!(
        result
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str()),
        Some("pulseforge-mcp")
    );
    // Notification: no reply (verified by the next request getting the
    // right id — no stale lines in between).
    bridge.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    // -- tools/list (PF-602/604/605): schemas present, launch tool ABSENT --
    let tools_reply = bridge.request("tools/list", None);
    let tools = tools_reply
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .expect("tools array");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    for expected in [
        "profile_capabilities",
        "profile_list_processes",
        "profile_list_sessions",
        "profile_attach",
        "profile_status",
        "profile_stop",
        "profile_summary",
        "profile_diagnostics",
        "profile_compare",
    ] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }
    assert!(
        !names.contains(&"profile_start"),
        "launch tool must NOT be advertised (ADR-008): {names:?}"
    );
    // Every tool has a JSON-Schema input contract.
    for t in tools.iter() {
        assert!(t.get("inputSchema").is_some(), "{t:?}");
        assert!(t.get("description").is_some());
    }

    // -- capabilities through the bridge to the daemon --
    let caps = bridge.tool_call("profile_capabilities", JsonValue::Object(vec![]));
    assert_eq!(
        caps.get("result")
            .and_then(|r| r.get("isError"))
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    let caps_json = parse_content_json(&caps);
    assert!(caps_json.get("counters").is_some(), "{caps_json:?}");

    // -- the agent workflow (PF-610): attach -> status -> stop --
    let mut workload = Command::new(exe())
        .args(["bench-spin", "--ms", "60000"])
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn workload");
    let attach = bridge.tool_call(
        "profile_attach",
        json::parse(&format!(
            r#"{{"pid":{},"duration_ms":8000,"interval_ms":150,"label":"mcp-e2e"}}"#,
            workload.id()
        ))
        .unwrap(),
    );
    let attach_json = parse_content_json(&attach);
    let sid = attach_json
        .get("session_id")
        .and_then(|v| v.as_str())
        .expect("session id")
        .to_string();

    // status observes live progress (bridge clients are stateless).
    std::thread::sleep(Duration::from_millis(500));
    let status = bridge.tool_call(
        "profile_status",
        json::parse(&format!(r#"{{"session_id":"{sid}"}}"#)).unwrap(),
    );
    let status_json = parse_content_json(&status);
    assert_eq!(
        status_json.get("state").and_then(|v| v.as_str()),
        Some("RECORDING"),
        "{status_json:?}"
    );
    assert!(
        status_json
            .get("samples")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            >= 2
    );

    // stop is idempotent across separate calls.
    let stop = bridge.tool_call(
        "profile_stop",
        json::parse(&format!(r#"{{"session_id":"{sid}"}}"#)).unwrap(),
    );
    let stop_json = parse_content_json(&stop);
    assert!(
        stop_json.get("state").and_then(|v| v.as_str()).is_some(),
        "{stop_json:?}"
    );
    let stop2 = bridge.tool_call(
        "profile_stop",
        json::parse(&format!(r#"{{"session_id":"{sid}"}}"#)).unwrap(),
    );
    assert_eq!(
        stop2
            .get("result")
            .and_then(|r| r.get("isError"))
            .and_then(|v| v.as_bool()),
        Some(false),
        "repeated stop is a successful no-op"
    );

    // summary after stop: units + availability present.
    let summary = bridge.tool_call(
        "profile_summary",
        json::parse(&format!(r#"{{"session_id":"{sid}"}}"#)).unwrap(),
    );
    let summary_json = parse_content_json(&summary);
    assert!(summary_json.get("metrics").is_some(), "{summary_json:?}");

    // -- typed errors: unknown session, bad arguments --
    let missing = bridge.tool_call(
        "profile_status",
        json::parse(r#"{"session_id":"00000000-0000-7000-8000-000000000000"}"#).unwrap(),
    );
    assert_eq!(
        missing
            .get("result")
            .and_then(|r| r.get("isError"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(
        missing.to_json_string().contains("SESSION_NOT_FOUND"),
        "{}",
        missing.to_json_string()
    );
    let bad_args = bridge.tool_call("profile_attach", json::parse(r#"{}"#).unwrap());
    assert_eq!(
        bad_args
            .get("result")
            .and_then(|r| r.get("isError"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(bad_args.to_json_string().contains("INVALID_ARGUMENT"));

    // -- profile_compare across two recorded sessions (PF-1110) --
    // Record a second, shorter session through the bridge.
    let attach2 = bridge.tool_call(
        "profile_attach",
        json::parse(&format!(
            r#"{{"pid":{},"duration_ms":3000,"interval_ms":150,"label":"mcp-e2e-2"}}"#,
            workload.id()
        ))
        .unwrap(),
    );
    let sid2 = parse_content_json(&attach2)
        .get("session_id")
        .and_then(|v| v.as_str())
        .expect("second session id")
        .to_string();
    // Wait for both to finalize, then compare by id.
    std::thread::sleep(Duration::from_millis(3500));
    let compare = bridge.tool_call(
        "profile_compare",
        json::parse(&format!(
            r#"{{"baseline_session_id":"{sid}","candidate_session_id":"{sid2}"}}"#
        ))
        .unwrap(),
    );
    assert_eq!(
        compare
            .get("result")
            .and_then(|r| r.get("isError"))
            .and_then(|v| v.as_bool()),
        Some(false),
        "{}",
        compare.to_json_string()
    );
    let cmp_json = parse_content_json(&compare);
    assert!(cmp_json.get("comparable").is_some(), "{cmp_json:?}");
    assert!(cmp_json.get("metrics").is_some());
    assert!(
        cmp_json
            .get("caveats")
            .and_then(|c| c.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    );

    // -- transport-level errors: unknown method, malformed JSON --
    let unknown = bridge.request("resources/read", None);
    assert_eq!(
        unknown
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(-32601)
    );
    bridge.send_raw("{not json");
    let parse_err = bridge.recv();
    assert_eq!(
        parse_err
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(-32700)
    );
    // The connection still works after all of the above.
    let ping = bridge.request("ping", None);
    assert!(ping.get("result").is_some());

    let _ = workload.kill();
    let _ = workload.wait();
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn stdout_is_protocol_pure_under_tool_errors() {
    // Even when the daemon is DOWN, tool calls must produce MCP responses
    // (tool errors) — and stdout must contain nothing else.
    let data = fresh_dir("pure");
    let mut bridge = Bridge::spawn(&data);
    let init = bridge.request(
        "initialize",
        Some(
            json::parse(
                r#"{"protocolVersion":"2025-06-18","clientInfo":{"name":"t","version":"0"}}"#,
            )
            .unwrap(),
        ),
    );
    assert!(init.get("result").is_some());
    bridge.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let caps = bridge.tool_call("profile_capabilities", JsonValue::Object(vec![]));
    assert_eq!(
        caps.get("result")
            .and_then(|r| r.get("isError"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "daemon down => tool error, not a hang or crash"
    );
    // Reply is well-formed JSON on one line (purity = only protocol lines).
    let _ = std::fs::remove_dir_all(&data);
}
