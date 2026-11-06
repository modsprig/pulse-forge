//! CLI-level integration tests: drive the real `pulseforge` binary the way a
//! user would, including the launch-and-record loop and offline report
//! re-rendering (v0.1 exit gate).

use std::process::Command;

fn exe() -> std::path::PathBuf {
    // Same-package integration tests get CARGO_BIN_EXE_ for their own bin.
    let p = std::path::PathBuf::from(env!("CARGO_BIN_EXE_pulseforge"));
    assert!(p.is_file(), "missing {}", p.display());
    p
}

fn run(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(exe())
        .args(args)
        .env(
            "PULSEFORGE_DATA",
            std::env::temp_dir().join("pf-cli-test-data"),
        )
        .output()
        .expect("run pulseforge");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn parse_json(text: &str) -> pf_types::json::JsonValue {
    let trimmed = text.trim();
    // Prefer the last line that parses as an object (record prints
    // session/report/summary lines before nothing; capabilities/probes emit
    // a single JSON line).
    for line in trimmed.lines().rev() {
        if let Ok(v) = pf_types::json::parse(line) {
            if v.as_object_pairs().is_some() {
                return v;
            }
        }
    }
    pf_types::json::parse(trimmed).expect("output must contain JSON")
}

#[test]
fn capabilities_reports_honest_backend_state() {
    let (ok, stdout, _) = run(&["capabilities"]);
    assert!(ok);
    let json = parse_json(&stdout);
    assert_eq!(
        json.get("tool").and_then(|v| v.as_str()),
        Some("pulseforge")
    );
    let counters = json.get("counters").expect("counters block");
    assert_eq!(
        counters.get("available").and_then(|v| v.as_bool()),
        Some(true),
        "Windows counters backend must be compiled in and live"
    );
    let stacks = json.get("cpu_stack_sampling").expect("stacks block");
    assert_eq!(
        stacks.get("available").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert!(
        !stacks
            .get("reason_code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
    );
}

#[test]
fn version_and_doctor_work() {
    let (ok, stdout, _) = run(&["version"]);
    assert!(ok);
    assert!(stdout.trim().starts_with("pulseforge "));
    let (ok, stdout, _) = run(&["doctor"]);
    assert!(ok);
    let json = parse_json(&stdout);
    assert!(json.get("data_root").is_some());
}

#[test]
fn bad_usage_is_a_clean_typed_error() {
    let (ok, _, stderr) = run(&["record", "--pid"]);
    assert!(!ok);
    assert!(stderr.contains("INVALID_ARGUMENT"), "{stderr}");
    let (ok, _, _) = run(&["nonsense"]);
    assert!(!ok);
    let (ok, _, _) = run(&["record"]);
    assert!(!ok);
}

/// Unique output dir per test run so parallel test binaries never collide.
fn fresh_out_root(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir()
        .join(format!("pf-cli-{tag}-{}-{nanos}", std::process::id()))
        .to_str()
        .unwrap()
        .to_string()
}

fn record_bench_spin(out_root: &str) -> (bool, String, String) {
    // Launch our own binary in bench-spin mode as the workload: direct
    // argv launch, no shell — exactly what --run guarantees.
    run(&[
        "record",
        "--run",
        exe().to_str().unwrap(),
        "--duration-ms",
        "1200",
        "--out",
        out_root,
        "--",
        "bench-spin",
        "--ms",
        "1500",
    ])
}

fn find_session_dir(out_root: &str) -> std::path::PathBuf {
    std::fs::read_dir(out_root)
        .expect("out root created")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e == "pfsession")
        })
        .expect("one .pfsession dir")
}

#[test]
fn record_launch_and_report_roundtrip() {
    let out_root = fresh_out_root("record");
    let (ok, stdout, stderr) = record_bench_spin(&out_root);
    assert!(ok, "{stderr}\n{stdout}");
    assert!(stdout.contains("session:"));

    let session = find_session_dir(&out_root);
    assert!(session.join("manifest.json").is_file());
    assert!(session.join("samples.jsonl").is_file());
    assert!(session.join("report.json").is_file());

    // Offline re-render must succeed and agree on identity.
    let (ok, stdout, _) = run(&["report", "--session", session.to_str().unwrap()]);
    assert!(ok);
    let json = parse_json(&stdout);
    let manifest_text = std::fs::read_to_string(session.join("manifest.json")).unwrap();
    let manifest = pf_types::json::parse(&manifest_text).unwrap();
    let session_id_m = manifest.get("session_id").and_then(|v| v.as_str()).unwrap();
    assert_eq!(
        json.get("session_id").and_then(|v| v.as_str()),
        Some(session_id_m),
        "report and manifest must agree on identity"
    );
    assert!(json.get("samples").and_then(|v| v.as_u64()).unwrap_or(0) >= 3);
    let _ = std::fs::remove_dir_all(&out_root);
}

#[test]
fn attach_record_never_kills_target() {
    let mut spin = Command::new(exe())
        .args(["bench-spin", "--ms", "6000"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn bench-spin");
    let pid = spin.id().to_string();
    let out_root = fresh_out_root("attach");
    let (ok, stdout, stderr) = run(&[
        "record",
        "--pid",
        &pid,
        "--duration-ms",
        "900",
        "--out",
        &out_root,
    ]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("session:"));
    let alive = spin.try_wait().expect("try_wait").is_none();
    assert!(alive, "attached target must survive the recording");
    let _ = spin.kill();
    let _ = spin.wait();
    let _ = std::fs::remove_dir_all(&out_root);
}

#[test]
fn probe_counters_reports_structured_json() {
    let (ok, stdout, stderr) = run(&["probe", "counters"]);
    assert!(ok, "{stderr}");
    let json = parse_json(&stdout);
    assert_eq!(json.get("mode").and_then(|v| v.as_str()), Some("counters"));
    assert!(json.get("metrics").is_some());
}

#[test]
fn probe_etw_reports_structured_steps_on_windows_unsupported_on_linux() {
    let (ok, stdout, stderr) = run(&["probe", "etw"]);
    if cfg!(windows) {
        assert!(ok, "{stderr}");
        let json = parse_json(&stdout);
        assert!(json.get("conclusion").is_some());
        assert!(json.get("steps").and_then(|s| s.as_array()).unwrap().len() >= 3);
    } else {
        // Linux: ETW does not exist — the honest answer is a typed
        // UNSUPPORTED_PLATFORM error, not a fake success.
        assert!(!ok, "probe etw must not succeed on Linux");
        assert!(stderr.contains("UNSUPPORTED_PLATFORM"), "{stderr}");
    }
}

#[test]
fn report_missing_dir_is_clean_error() {
    let (ok, _, stderr) = run(&["report", "--session", "Z:/no/such/session"]);
    assert!(!ok);
    assert!(stderr.contains("SESSION_NOT_FOUND"), "{stderr}");
}

#[test]
fn stdout_is_pure_json_for_machine_commands() {
    // Machine-facing commands must emit exactly one JSON document — no
    // stray logs or prompts (foundation for the future MCP bridge).
    for cmd in ["capabilities", "doctor"] {
        let out = Command::new(exe()).arg(cmd).output().expect("run");
        pf_types::json::parse(String::from_utf8_lossy(&out.stdout).trim())
            .unwrap_or_else(|e| panic!("{cmd} stdout must be pure JSON: {e}"));
    }
}
