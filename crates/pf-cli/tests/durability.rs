//! P5 durability gate: samples recorded through the daemon survive a
//! daemon kill mid-recording, in sealed chunks, and remain queryable after
//! restart (PF-506 — "crash recovery returns valid completed chunks").

#![cfg(windows)]

use std::process::{Command, Stdio};
use std::time::Duration;

fn exe() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_pulseforge"))
}

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pf-dur-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn killed_daemon_leaves_queryable_sealed_chunks() {
    let data = fresh_dir("kill");
    let mut daemon = Command::new(exe())
        .args(["daemon"])
        .env("PULSEFORGE_DATA", &data)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");

    let mut workload = Command::new(exe())
        .args(["bench-spin", "--ms", "60000"])
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn workload");

    // Start a recording through the agent (client exits immediately).
    let mut started = None;
    for _ in 0..20 {
        let out = Command::new(exe())
            .args([
                "agent",
                "start",
                "--pid",
                &workload.id().to_string(),
                "--duration-ms",
                "30000",
                "--interval-ms",
                "150",
            ])
            .env("PULSEFORGE_DATA", &data)
            .output()
            .expect("agent start");
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(line) = text.trim().lines().last() {
                if let Ok(v) = pf_types::json::parse(line) {
                    started = v
                        .get("session_id")
                        .and_then(|s| s.as_str())
                        .map(str::to_string);
                    if started.is_some() {
                        break;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let sid = started.expect("recording started");

    // Let the flush period (1 s) seal at least two chunks.
    std::thread::sleep(Duration::from_millis(2600));

    // KILL the daemon hard, mid-recording.
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = workload.kill();
    let _ = workload.wait();

    // Locate the session dir and open the chunk store directly.
    let sessions = data.join("sessions");
    let session_dir = std::fs::read_dir(&sessions)
        .expect("sessions dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&sid[..13]))
        })
        .expect("session dir survived the kill");

    let reader = pf_storage::ChunkReader::open(&session_dir).expect("chunks readable");
    assert!(
        reader.total_records() >= 10,
        "sealed samples must survive: {}",
        reader.total_records()
    );
    assert!(reader.chunk_count() >= 1);
    // Time range is sane and inside the recording window.
    let (min_ns, max_ns) = reader.time_range().expect("time range");
    assert!(min_ns <= max_ns);
    // Ranged query works on the salvaged data.
    let page = reader.range(0, u64::MAX, 1000).unwrap();
    assert!(!page.snapshots.is_empty());
    // Values are real (working set of a live process).
    assert!(
        page.snapshots
            .iter()
            .all(|s| s.resident_bytes.unwrap_or(0) > 0)
    );

    // The manifest is non-terminal (RECORDING); a daemon restart rewrites
    // it to RECOVERED_PARTIAL and the chunks remain readable.
    let manifest = std::fs::read_to_string(session_dir.join("manifest.json")).unwrap();
    let v = pf_types::json::parse(manifest.trim()).unwrap();
    assert_eq!(
        v.get("state").and_then(|s| s.as_str()),
        Some("RECORDING"),
        "killed mid-recording manifest must be non-terminal"
    );

    let mut daemon2 = Command::new(exe())
        .args(["daemon"])
        .env("PULSEFORGE_DATA", &data)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("restart daemon");
    std::thread::sleep(Duration::from_millis(1200));
    let manifest = std::fs::read_to_string(session_dir.join("manifest.json")).unwrap();
    let v = pf_types::json::parse(manifest.trim()).unwrap();
    assert_eq!(
        v.get("state").and_then(|s| s.as_str()),
        Some("RECOVERED_PARTIAL"),
        "restart must recover the orphan"
    );
    // Chunks still queryable after recovery.
    let reader2 = pf_storage::ChunkReader::open(&session_dir).unwrap();
    assert_eq!(reader2.total_records(), reader.total_records());

    let _ = daemon2.kill();
    let _ = daemon2.wait();
    let _ = std::fs::remove_dir_all(&data);
}
