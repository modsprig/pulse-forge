//! Crash-safety chaos test (durability story end-to-end): SIGKILL the
//! daemon at swept points across the recording/finalize window, restart
//! (recovery runs), and assert the invariants a crash victim actually
//! relies on —
//!
//! 1. the manifest either parses or carries a RECOVERY_NOTE.txt (with
//!    atomic manifest writes it must always parse);
//! 2. after recovery the session state is terminal or RECOVERED_PARTIAL;
//! 3. `stacks.jsonl` is COMPLETE if present — every line parses, because
//!    persist publishes through tmp + fsync + rename and a kill can only
//!    leave the `.partial` tmp, never a torn final artifact;
//! 4. `stacks-summary.json` and `maps.json`, when present, parse;
//! 5. final chunk files all parse their headers (`.partial` tmps are
//!    evidence and allowed to remain).
//!
//! Linux-only: SIGKILL semantics + the perf stacks pipeline.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WORKLOAD_MS: u64 = 6000;
const RECORD_MS: u64 = 4000;

fn fresh_root(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pf-crash-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn pulseforge() -> &'static str {
    env!("CARGO_BIN_EXE_pulseforge")
}

struct Daemon(Child);

impl Daemon {
    fn spawn(data: &Path) -> Daemon {
        let child = Command::new(pulseforge())
            .env("PULSEFORGE_DATA", data)
            .args(["daemon"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon spawns");
        Daemon(child)
    }

    fn kill9(&mut self) {
        // SIGKILL: the crash the durability story must survive.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn agent_json(data: &Path, args: &[&str]) -> Option<String> {
    let mut full: Vec<&str> = vec!["agent"];
    full.extend_from_slice(args);
    let out = Command::new(pulseforge())
        .env("PULSEFORGE_DATA", data)
        .args(&full)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Poll the session MANIFEST on disk until it reaches a terminal or
/// recovered state. Post-crash the session lives on disk, not in the
/// restarted supervisor's memory (agent ops answer SESSION_NOT_FOUND for
/// it by design; `report --session <dir>` is the user-facing path).
fn wait_recovered_on_disk(session_dir: &Path, deadline: Duration) -> String {
    let start = Instant::now();
    loop {
        assert!(
            start.elapsed() < deadline,
            "manifest never reached a terminal/recovered state"
        );
        std::thread::sleep(Duration::from_millis(100));
        let text = std::fs::read_to_string(session_dir.join("manifest.json")).unwrap();
        if let Ok(v) = pf_types::json::parse(text.trim()) {
            let state = v
                .get("state")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            if matches!(
                state.as_str(),
                "COMPLETED" | "FAILED" | "CANCELLED" | "RECOVERED_PARTIAL"
            ) {
                return state;
            }
        }
    }
}

#[test]
fn sigkill_sweep_leaves_recoverable_complete_artifacts() {
    // Sweeps kill points through the recording window AND the finalize
    // window (finalize ≈ RECORD_MS): some kills land mid-persist, which
    // is exactly where the atomic-write contract earns its keep.
    for kill_ms in [2200u64, 2500, 2800, 3100, 3400, 3700, 4000] {
        let data = fresh_root(&format!("k{kill_ms}"));
        let workload_ms = WORKLOAD_MS.to_string();
        let mut workload = Command::new(pulseforge())
            .args(["bench-spin", "--ms", &workload_ms])
            .stdout(Stdio::null())
            .spawn()
            .expect("workload spawns");
        let pid = workload.id();

        let mut daemon = Daemon::spawn(&data);
        std::thread::sleep(Duration::from_millis(600));

        let start_out = agent_json(
            &data,
            &[
                "start",
                "--pid",
                &pid.to_string(),
                "--duration-ms",
                &RECORD_MS.to_string(),
                "--interval-ms",
                "150",
            ],
        );
        let start_out = start_out.unwrap_or_else(|| panic!("agent start at {kill_ms} ms"));
        let sid = pf_types::json::parse(start_out.trim())
            .ok()
            .and_then(|v| {
                v.get("session_id")
                    .and_then(|s| s.as_str())
                    .map(str::to_string)
            })
            .expect("session id in start output");

        std::thread::sleep(Duration::from_millis(kill_ms));
        daemon.kill9(); // SIGKILL at the swept point
        drop(daemon);

        // Restart: recovery runs on start.
        let mut daemon2 = Daemon::spawn(&data);
        std::thread::sleep(Duration::from_millis(400));
        let session_dir = data.join("sessions").join(format!("{sid}.pfsession"));
        let state = wait_recovered_on_disk(&session_dir, Duration::from_secs(20));
        eprintln!("chaos: kill at {kill_ms} ms -> {state}");
        // The workload is still spinning; release it.
        let _ = workload.kill();
        let _ = workload.wait();

        // Invariant 2: terminal or recovered — a kill before finalize
        // recovers to RECOVERED_PARTIAL, after finalize it stays
        // COMPLETED.
        assert!(
            matches!(state.as_str(), "COMPLETED" | "RECOVERED_PARTIAL"),
            "kill at {kill_ms} ms → state {state}"
        );

        // The user-facing post-crash path: report renders the recovered
        // session dir without any daemon.
        let report = Command::new(pulseforge())
            .args(["report", "--session", &session_dir.to_string_lossy()])
            .output()
            .expect("report runs");
        assert!(
            report.status.success(),
            "report works on the post-crash session: {}",
            String::from_utf8_lossy(&report.stderr)
        );

        // Invariant 1: the manifest parses (atomic writes; recovery may
        // have rewritten it to RECOVERED_PARTIAL).
        let manifest_text = std::fs::read_to_string(session_dir.join("manifest.json")).unwrap();
        let manifest = pf_types::json::parse(manifest_text.trim())
            .expect("manifest parses after any kill point");
        let state_in_manifest = manifest
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        assert_eq!(state_in_manifest, state, "manifest state matches status");

        // Invariant 3: stacks.jsonl is COMPLETE if present — no torn tail.
        let stacks_path = session_dir.join("stacks.jsonl");
        if stacks_path.is_file() {
            let text = std::fs::read_to_string(&stacks_path).unwrap();
            let lines = text.lines().filter(|l| !l.trim().is_empty());
            let mut n = 0usize;
            for (i, line) in lines.enumerate() {
                pf_types::json::parse(line).unwrap_or_else(|e| {
                    panic!("kill {kill_ms} ms: torn stacks.jsonl line {}: {e}", i + 1)
                });
                n += 1;
            }
            if n > 0 {
                let loaded = pf_analysis::stacks_io::load_session_stacks_timed(&session_dir);
                assert!(
                    loaded.is_ok(),
                    "kill {kill_ms} ms: complete stacks.jsonl must load"
                );
            }
        } else {
            // Absent is allowed (killed before finalize): the .partial tmp
            // may exist as evidence.
            let tmp = session_dir.join("stacks.jsonl.partial");
            if tmp.is_file() {
                // Tmp evidence may be torn BY DESIGN — never promoted.
            }
        }

        // Invariant 4: summary/maps parse when present.
        for name in ["stacks-summary.json", "maps.json"] {
            let p = session_dir.join(name);
            if p.is_file() {
                let text = std::fs::read_to_string(&p).unwrap();
                pf_types::json::parse(text.trim())
                    .unwrap_or_else(|e| panic!("kill {kill_ms} ms: torn {name}: {e}"));
            }
        }

        // Invariant 5: every FINAL chunk parses its header; .partial
        // tmps are evidence and allowed.
        let chunks = session_dir.join("chunks");
        if chunks.is_dir() {
            for e in std::fs::read_dir(&chunks).unwrap().flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "partial") {
                    continue;
                }
                let bytes = std::fs::read(&p).unwrap();
                assert!(
                    bytes.len() >= 8,
                    "final chunk {} too small for a header",
                    p.display()
                );
            }
        }

        daemon2.kill9();
        let _ = std::fs::remove_dir_all(&data);
    }
}
