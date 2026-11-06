//! PF-009: drive the MCP spike server over real stdio pipes and assert the
//! handshake, tool listing, error semantics and protocol purity of stdout.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use pf_types::json::{self, JsonValue};

fn spawn_server() -> std::process::Child {
    let exe = env!("CARGO_BIN_EXE_pf-mcp-spike-server");
    Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn spike server")
}

fn send(stdin: &mut std::process::ChildStdin, line: &str) {
    writeln!(stdin, "{line}").expect("write to server");
    stdin.flush().expect("flush to server");
}

fn recv(stdout: &mut BufReader<std::process::ChildStdout>) -> JsonValue {
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read response");
    json::parse(line.trim()).expect("response is valid JSON")
}

#[test]
fn initialize_handshake_negotiates_and_lists_tools() {
    let mut child = spawn_server();
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"spike-test","version":"0.0.0"}}}"#,
    );
    let init = recv(&mut reader);
    assert_eq!(init.get("jsonrpc").and_then(|v| v.as_str()), Some("2.0"));
    assert_eq!(init.get("id").and_then(|v| v.as_i64()), Some(1));
    let result = init.get("result").expect("result present");
    assert_eq!(
        result.get("protocolVersion").and_then(|v| v.as_str()),
        Some("2025-06-18")
    );
    assert_eq!(
        result
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str()),
        Some("pf-mcp-spike")
    );

    // Notification: must produce NO response line.
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );

    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let tools = recv(&mut reader);
    let tool_list = tools
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .expect("tools array");
    assert!(tool_list.len() >= 2);
    let names: Vec<&str> = tool_list
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(names.contains(&"ping"));
    assert!(names.contains(&"profile_capabilities_spike"));

    drop(stdin);
    let status = child.wait().expect("server exits on EOF");
    assert!(status.success());
}

#[test]
fn malformed_json_and_unknown_methods_produce_typed_errors() {
    let mut child = spawn_server();
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    send(&mut stdin, "{not json");
    let parse_err = recv(&mut reader);
    assert_eq!(
        parse_err
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(-32700)
    );

    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":9,"method":"shenanigans"}"#,
    );
    let method_err = recv(&mut reader);
    assert_eq!(
        method_err
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_i64()),
        Some(-32601)
    );

    drop(stdin);
    assert!(child.wait().map(|s| s.success()).unwrap_or(false));
}

#[test]
fn stdout_carries_only_protocol_lines() {
    // Every line the server ever prints on stdout must be a JSON object;
    // initialize writes a stderr log at the same time, so this also checks
    // separation.
    let mut child = spawn_server();
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"purity-check"}}}"#,
    );
    let mut line = String::new();
    reader.read_line(&mut line).expect("one response line");
    let v = json::parse(line.trim()).expect("stdout line is JSON");
    assert!(
        v.as_object_pairs().is_some(),
        "stdout line is a JSON object"
    );

    drop(stdin);
    let _ = child.wait();
}
