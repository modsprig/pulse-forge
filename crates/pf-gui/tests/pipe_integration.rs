//! Real-daemon integration test for the GUI stack (PF-706 live path): the
//! actual named-pipe client (`PipeDaemonClient`) drives the actual view
//! models against a spawned daemon — no window, all real processes.

#![cfg(windows)]

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use pf_gui::{DashboardModel, HomeModel, PipeDaemonClient};

fn exe() -> std::path::PathBuf {
    // pf-gui's own binary isn't the daemon; locate the pf-cli binary in the
    // shared target dir. Cross-package tests do not rebuild it, so build it
    // here (incremental; a no-op when fresh) — spawning a stale daemon
    // binary silently breaks protocol-behavior tests.
    let status = std::process::Command::new("cargo")
        .args(["build", "-q", "-p", "pf-cli", "--bin", "pulseforge"])
        .status()
        .expect("run cargo build for pf-cli");
    assert!(
        status.success(),
        "building pulseforge for the GUI e2e failed"
    );
    for candidate in [
        "../../target/debug/pulseforge.exe",
        "../../../target/debug/pulseforge.exe",
    ] {
        let p = std::path::PathBuf::from(candidate);
        if p.is_file() {
            return std::path::absolute(&p).unwrap_or(p);
        }
    }
    panic!("pulseforge.exe not found after build");
}

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pf-gui-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct DaemonGuard(std::process::Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn gui_stack_drives_a_real_recording_end_to_end() {
    let data = fresh_dir("e2e");
    let daemon = Command::new(exe())
        .args(["daemon"])
        .env("PULSEFORGE_DATA", &data)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    let _guard = DaemonGuard(daemon);
    std::thread::sleep(Duration::from_millis(1200));

    // The GUI's real client (named pipe + handshake per call).
    let client =
        Arc::new(PipeDaemonClient::connect_with_root(data.clone()).expect("client connects"));

    // Home refresh sees the empty session list.
    let mut home = HomeModel::default();
    home.refresh(client.as_ref());
    assert_eq!(home.sessions.len(), 0, "{:?}", home.status_line);

    // Attach to a real workload through the same flow the Start button uses.
    let mut workload = Command::new(exe())
        .args(["bench-spin", "--ms", "60000"])
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn workload");
    home.attach_pid = workload.id().to_string();
    home.attach_duration_ms = "8000".into();
    home.attach_interval_ms = "150".into();
    home.attach_label = "gui-e2e".into();
    let id = home.submit_attach(client.as_ref()).expect("attach started");

    // Dashboard model observes live progress over the real pipe. Samples
    // become visible through the chunk store's 1 s flush period (ADR-013:
    // incremental durability), so poll like a real UI would.
    let mut dash = DashboardModel::new(id);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        dash.refresh(client.as_ref());
        if dash.samples >= 2 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no samples within flush window"
        );
        assert_eq!(
            dash.state, "RECORDING",
            "state while waiting: {}",
            dash.state
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    assert_eq!(dash.state, "RECORDING");
    assert!(dash.samples >= 2, "live samples visible: {}", dash.samples);
    assert!(dash.warnings.iter().all(|w| !w.contains("status failed")));

    // Stop from the dashboard flow; idempotent repeat.
    dash.stop(client.as_ref());
    dash.stop(client.as_ref());
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        dash.refresh(client.as_ref());
        if dash.state == "COMPLETED" || dash.state == "STOPPING" || dash.state == "FINALIZING" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "state: {}",
            dash.state
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // Home refresh now lists the finished session; the search finds it.
    home.refresh(client.as_ref());
    assert_eq!(home.sessions.len(), 1, "{:?}", home.status_line);
    let found = home.filter_rows(
        id.as_hyphenated()
            .chars()
            .take(8)
            .collect::<String>()
            .as_str(),
    );
    assert_eq!(found.len(), 1);

    let _ = workload.kill();
    let _ = workload.wait();
    let _ = std::fs::remove_dir_all(&data);
}
