//! MCP stacks tools e2e (PF-905) on Linux: real daemon + real `pulseforge mcp`
//! bridge + real bench-spin workload. Verifies `profile_top_functions` and
//! `profile_get_stack` end-to-end against the known-hotspot fixture, with
//! bounded I/O and symbol-coverage quality fields.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pf_types::json::{self, JsonValue};

fn exe() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_pulseforge"))
}

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pf-mcp-stacks-{tag}-{}-{}",
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
        self.next_id = self.next_id.saturating_add(1);
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
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_content_json(reply: &JsonValue) -> JsonValue {
    let text = reply
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    json::parse(&text).unwrap_or_else(|e| panic!("tool text is JSON: {e}: {text}"))
}

fn tool_is_error(reply: &JsonValue) -> bool {
    reply
        .get("result")
        .and_then(|r| r.get("isError"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

#[test]
fn mcp_stacks_tools_answer_with_the_known_hotspot() {
    let data = fresh_dir("stacks");
    let _daemon = DaemonGuard(
        Command::new(exe())
            .args(["daemon"])
            .env("PULSEFORGE_DATA", &data)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon"),
    );
    std::thread::sleep(Duration::from_millis(900));

    let mut bridge = Bridge::spawn(&data);
    bridge.request(
        "initialize",
        Some(
            json::parse(
                r#"{"protocolVersion":"2025-06-18","clientInfo":{"name":"mcp-stacks-e2e","version":"0.0"}}"#,
            )
            .unwrap(),
        ),
    );
    bridge.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    // tools/list: the new stacks tools are advertised; the launch tool
    // stays absent (ADR-008).
    let tools_reply = bridge.request("tools/list", None);
    let names: Vec<String> = tools_reply
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .expect("tools array")
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    assert!(
        names.contains(&"profile_top_functions".to_string()),
        "{names:?}"
    );
    assert!(
        names.contains(&"profile_get_stack".to_string()),
        "{names:?}"
    );
    assert!(!names.contains(&"profile_start".to_string()), "{names:?}");

    // Record the known-hotspot workload to completion.
    let mut workload = Command::new(exe())
        .args(["bench-spin", "--ms", "15000"])
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn workload");
    let attach = bridge.tool_call(
        "profile_attach",
        json::parse(&format!(
            r#"{{"pid":{},"duration_ms":3000,"interval_ms":150}}"#,
            workload.id()
        ))
        .unwrap(),
    );
    assert!(!tool_is_error(&attach), "{attach:?}");
    let sid = parse_content_json(&attach)
        .get("session_id")
        .and_then(|v| v.as_str())
        .expect("session id")
        .to_string();

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "session never completed");
        std::thread::sleep(Duration::from_millis(300));
        let status = bridge.tool_call(
            "profile_status",
            json::parse(&format!(r#"{{"session_id":"{sid}"}}"#)).unwrap(),
        );
        let state = parse_content_json(&status)
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if matches!(
            state.as_str(),
            "COMPLETED" | "FAILED" | "CANCELLED" | "RECOVERED_PARTIAL"
        ) {
            assert_eq!(state, "COMPLETED");
            break;
        }
    }
    let _ = workload.kill();
    let _ = workload.wait();

    // profile_top_functions: bounded, with coverage quality, known hotspot.
    let top = bridge.tool_call(
        "profile_top_functions",
        json::parse(&format!(r#"{{"session_id":"{sid}","limit":16}}"#)).unwrap(),
    );
    assert!(!tool_is_error(&top), "{top:?}");
    let top_json = parse_content_json(&top);
    assert_eq!(
        top_json.get("schema").and_then(|v| v.as_str()),
        Some("pulseforge.top-functions/0")
    );
    let samples = top_json
        .get("samples")
        .and_then(|v| v.as_u64())
        .expect("samples field");
    assert!(samples > 0);
    let rows = top_json
        .get("top_functions")
        .and_then(|v| v.as_array())
        .expect("top_functions");
    assert!(!rows.is_empty() && rows.len() <= 16, "{} rows", rows.len());
    let top_fn = rows[0]
        .get("function")
        .and_then(|f| f.as_str())
        .expect("function")
        .to_string();
    eprintln!("E2E-MCP-TOP {top_fn} w={:?}", rows[0].get("weight"));
    assert!(
        top_fn.contains("pf_test_support") || top_fn.contains("spin_ms"),
        "top function must be the known hotspot, got {top_fn}"
    );
    assert!(
        top_json.get("frame_identity_coverage").is_some(),
        "coverage quality field present (number or null)"
    );

    // profile_get_stack: callchains for the top function, outermost-first.
    let stack = bridge.tool_call(
        "profile_get_stack",
        json::parse(&format!(
            r#"{{"session_id":"{sid}","function":"{top_fn}","limit":4}}"#
        ))
        .unwrap(),
    );
    assert!(!tool_is_error(&stack), "{stack:?}");
    let stack_json = parse_content_json(&stack);
    assert_eq!(
        stack_json.get("schema").and_then(|v| v.as_str()),
        Some("pulseforge.stack-samples/0")
    );
    assert_eq!(
        stack_json.get("function").and_then(|v| v.as_str()),
        Some(top_fn.as_str())
    );
    let matches_total = stack_json
        .get("matches_total")
        .and_then(|v| v.as_u64())
        .expect("matches_total");
    assert!(matches_total > 0, "the top function appears in samples");
    let stacks = stack_json
        .get("samples")
        .and_then(|v| v.as_array())
        .expect("samples");
    assert!(
        !stacks.is_empty() && stacks.len() <= 4,
        "{} stacks",
        stacks.len()
    );
    let frames = stacks[0]
        .get("frames")
        .and_then(|f| f.as_array())
        .expect("frames");
    assert!(!frames.is_empty());
    assert!(
        frames
            .iter()
            .any(|f| f.get("function").and_then(|x| x.as_str()) == Some(top_fn.as_str())),
        "requested function appears in the returned callchain"
    );

    // Honesty: a counters-only/unknown session gets a typed error, not zeros.
    let missing = bridge.tool_call(
        "profile_top_functions",
        json::parse(r#"{"session_id":"00000000-0000-7000-8000-000000000000"}"#).unwrap(),
    );
    assert!(
        tool_is_error(&missing),
        "unknown session must be a typed error: {missing:?}"
    );
}
