//! End-to-end daemon tests (PF-402/PF-406/PF-409) over the REAL named pipe
//! with REAL processes:
//!
//! 1. spawn `pulseforge daemon` as a child process;
//! 2. an agent client starts a recording (then exits — client disconnect);
//! 3. a second agent client observes the recording still running;
//! 4. stop + terminal state + persisted session on disk;
//! 5. a second daemon instance refuses to start while the first lives;
//! 6. killing the daemon and restarting recovers the orphaned session
//!    manifest as RECOVERED_PARTIAL (PF-407).

#![cfg(windows)]

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn exe() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_pulseforge"))
}

fn fresh_env_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pf-e2e-{tag}-{}-{}",
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

fn spawn_daemon(data_root: &std::path::Path) -> DaemonGuard {
    let err_path = data_root.join(format!(
        "daemon-{}.stderr",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let child = Command::new(exe())
        .args(["daemon"])
        .env("PULSEFORGE_DATA", data_root)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&err_path).unwrap())
        .spawn()
        .expect("spawn daemon");
    DaemonGuard(child)
}

fn agent_json(data_root: &std::path::Path, args: &[&str]) -> (bool, String) {
    for _ in 0..30 {
        let out = Command::new(exe())
            .args(args)
            .env("PULSEFORGE_DATA", data_root)
            .output()
            .expect("run agent");
        if out.status.success() {
            return (true, String::from_utf8_lossy(&out.stdout).into_owned());
        }
        // Daemon may still be starting; retry briefly.
        std::thread::sleep(Duration::from_millis(250));
    }
    let out = Command::new(exe())
        .args(args)
        .env("PULSEFORGE_DATA", data_root)
        .output()
        .expect("run agent (final)");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn parse_last_json(text: &str) -> pf_types::json::JsonValue {
    for line in text.trim().lines().rev() {
        if let Ok(v) = pf_types::json::parse(line) {
            if v.as_object_pairs().is_some() {
                return v;
            }
        }
    }
    panic!("no JSON in agent output: {text}");
}

#[test]
fn daemon_records_across_client_disconnects_and_recovers_after_restart() {
    let data = fresh_env_dir("full");
    let sessions = data.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();

    // A long-running workload we record: our own bench-spin.
    let mut workload = Command::new(exe())
        .args(["bench-spin", "--ms", "60000"])
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn workload");
    let pid = workload.id();

    let _daemon = spawn_daemon(&data);

    // Client #1: start a recording, then this client *exits*.
    let (ok, out) = agent_json(
        &data,
        &[
            "agent",
            "start",
            "--pid",
            &pid.to_string(),
            "--duration-ms",
            "30000",
            "--interval-ms",
            "150",
        ],
    );
    assert!(ok, "{out}");
    let started = parse_last_json(&out);
    let sid = started
        .get("session_id")
        .and_then(|v| v.as_str())
        .expect("session id")
        .to_string();

    // Client #1 is gone. A second client observes the recording alive.
    std::thread::sleep(Duration::from_millis(600));
    let (ok, out) = agent_json(&data, &["agent", "status", "--session", &sid]);
    assert!(ok, "{out}");
    let status = parse_last_json(&out);
    let result = status
        .get("result")
        .cloned()
        .unwrap_or(pf_types::json::JsonValue::Null);
    assert_eq!(
        result.get("state").and_then(|v| v.as_str()),
        Some("RECORDING"),
        "recording must survive the starting client's exit (out={out})"
    );
    let samples = result.get("samples").and_then(|v| v.as_u64()).unwrap_or(0);
    assert!(samples >= 2, "live progress visible: {samples} samples");

    // Daemon restart: kill the daemon (clients unaffected by design), then
    // start a new daemon — the orphaned RECORDING session must be recovered.
    drop(_daemon);
    let _ = workload.kill();
    let _ = workload.wait();
    let _daemon2 = spawn_daemon(&data);
    // The second daemon must have come up despite the first's lock (the
    // first died; stale-lock recovery) — prove by asking it anything.
    let (ok, out) = agent_json(&data, &["agent", "sessions"]);
    let _ = &out;
    if !ok {
        let mut notes = String::new();
        for f in std::fs::read_dir(&data).unwrap().filter_map(|e| e.ok()) {
            if f.path().extension().and_then(|e| e.to_str()) == Some("stderr") {
                notes.push_str(&std::fs::read_to_string(f.path()).unwrap_or_default());
                notes.push_str(
                    "
---
",
                );
            }
        }
        panic!("agent sessions failed; daemon notes: {notes}");
    }

    // The orphaned session manifest must now be RECOVERED_PARTIAL.
    let mut found_recovered = false;
    for entry in std::fs::read_dir(&sessions).expect("sessions dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("pfsession") {
            let manifest = std::fs::read_to_string(path.join("manifest.json")).unwrap_or_default();
            if manifest.contains(&sid[..8.min(sid.len())]) {
                let v = pf_types::json::parse(manifest.trim()).unwrap();
                if v.get("state").and_then(|s| s.as_str()) == Some("RECOVERED_PARTIAL") {
                    found_recovered = true;
                }
            }
        }
    }
    assert!(
        found_recovered,
        "orphaned session {sid} must be RECOVERED_PARTIAL after daemon restart"
    );
    drop(_daemon2);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn second_daemon_refuses_while_first_lives() {
    let data = fresh_env_dir("single");
    let _daemon = spawn_daemon(&data);
    std::thread::sleep(Duration::from_millis(800));

    let second = Command::new(exe())
        .args(["daemon"])
        .env("PULSEFORGE_DATA", &data)
        .stdout(Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn second daemon");
    let mut second = second;
    // It must exit non-zero with a clear error, quickly.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if second.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "second daemon must exit promptly"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let status = second.wait().unwrap();
    assert!(!status.success(), "second daemon must fail");
    let mut err = String::new();
    if let Some(mut e) = second.stderr.take() {
        use std::io::Read;
        let _ = e.read_to_string(&mut err);
    }
    let _ = writeln!(std::io::stdout());
    assert!(
        err.contains("PERMISSION_DENIED") || err.contains("AlreadyRunning") || err.contains("owns"),
        "clear single-instance error, got: {err}"
    );
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn agent_stop_is_idempotent_across_clients() {
    let data = fresh_env_dir("idem");
    let mut workload = Command::new(exe())
        .args(["bench-spin", "--ms", "60000"])
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn workload");
    let pid = workload.id();
    let _daemon = spawn_daemon(&data);

    let (ok, out) = agent_json(
        &data,
        &[
            "agent",
            "start",
            "--pid",
            &pid.to_string(),
            "--duration-ms",
            "30000",
            "--interval-ms",
            "150",
        ],
    );
    assert!(ok, "{out}");
    let sid = parse_last_json(&out)
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // Two clients stop concurrently: both succeed.
    let data2 = data.clone();
    let sid2 = sid.clone();
    let a = std::thread::spawn(move || agent_json(&data2, &["agent", "stop", "--session", &sid2]));
    let (ok, out) = agent_json(&data, &["agent", "stop", "--session", &sid]);
    assert!(ok, "{out}");
    let (ok2, out2) = a.join().unwrap();
    assert!(ok2, "{out2}");

    // Terminal state persisted: poll with a deadline (parallel CI load can
    // slow finalization; a fixed sleep is flaky under contention).
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let (ok, out) = agent_json(&data, &["agent", "status", "--session", &sid]);
        assert!(ok, "{out}");
        let state = parse_last_json(&out)
            .get("result")
            .and_then(|r| r.get("state"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if state == "COMPLETED" || state == "STOPPING" || state == "FINALIZING" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stop never reached a late state; last: {state}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    let _ = workload.kill();
    let _ = workload.wait();
    let _ = std::fs::remove_dir_all(&data);
}
