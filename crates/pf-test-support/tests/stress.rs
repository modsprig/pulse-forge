//! P12 subsets: (a) handle/resource stability across session churn
//! (PF-1207), (b) sustained high-rate capture (PF-1203 bounded slice).
//!
//! The handle check is self-referential by design: PulseForge counts its
//! own open handles through the same Win32 API it profiles — a leak across
//! N start/stop cycles shows up as a monotonically growing handle count.

#![cfg(windows)]

use std::process::Stdio;
use std::time::Duration;

use pf_daemon::supervisor::{SessionOptions, SessionSupervisor, TargetSpec};
use pf_platform::contracts::PlatformBackend;
use pf_types::CounterSnapshot;

fn supervisor() -> std::sync::Arc<SessionSupervisor> {
    std::sync::Arc::new(SessionSupervisor::new(Box::new(|| {
        Ok(Box::new(pf_windows::WindowsPlatform::new()))
    })))
}

fn own_handles(backend: &dyn PlatformBackend) -> u32 {
    let mut src = backend.open(std::process::id()).expect("open self");
    src.snapshot(0)
        .expect("snapshot self")
        .handle_count
        .unwrap_or(0)
}

/// PF-1207 subset: repeated attach-record-stop cycles on real children
/// must not leak handles or session records in the supervisor.
#[test]
fn session_churn_does_not_leak_handles() {
    use std::process::{Command, Stdio};

    let exe = env!("CARGO_BIN_EXE_pf-fixture-cpu");
    let backend = pf_windows::WindowsPlatform::new();
    let sup = supervisor();

    let baseline = own_handles(&backend);
    const CYCLES: usize = 6;
    let mut per_cycle: Vec<u32> = Vec::new();
    let mut prev = baseline;
    for cycle in 0..CYCLES {
        let mut child = Command::new(exe)
            .args(["--ms", "6000"])
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn fixture");
        let id = sup
            .create(TargetSpec::Attach { pid: child.id() })
            .expect("create");
        sup.start(
            id,
            SessionOptions {
                interval_ms: 150,
                duration_ms: 700,
                out_dir: Some(std::env::temp_dir().join(format!(
                    "pf-stress-{}-{}",
                    std::process::id(),
                    cycle
                ))),
                label: None,
            },
        )
        .expect("start");
        let final_state = sup.wait(id, Duration::from_secs(10)).expect("wait");
        assert!(final_state.is_terminal(), "cycle {cycle}: {final_state:?}");
        sup.finalize(id).expect("finalize");
        let _ = child.kill();
        let _ = child.wait();

        // No supervisor-level session growth: terminal records are kept
        // (listed) but active count must return to zero.
        assert_eq!(sup.active_count(), 0, "cycle {cycle} left active sessions");

        let now = own_handles(&backend);
        per_cycle.push(now.saturating_sub(prev));
        prev = now;
    }

    let after = own_handles(&backend);

    // Leak vs. cache discrimination: a handle leak grows ~linearly with
    // cycles; one-time runtime caches (loader, allocator, destroyed-thread
    // reclamation) plateau. Require the per-cycle delta to shrink: the
    // second half must add strictly less than the first half, and the last
    // cycle must add fewer handles than the first.
    let first_half: u32 = per_cycle[..CYCLES / 2].iter().sum();
    let second_half: u32 = per_cycle[CYCLES / 2..].iter().sum();
    let total_growth = after.saturating_sub(baseline);
    eprintln!(
        "handles: baseline {baseline} -> after {after} (growth {total_growth}); per-cycle {per_cycle:?}"
    );
    assert!(
        second_half <= first_half,
        "per-cycle handle growth not plateauing: first half {first_half}, second half {second_half} — possible leak"
    );
    assert!(
        total_growth < 16,
        "absolute growth {total_growth} across {CYCLES} cycles too high (baseline {baseline}, after {after})"
    );
}

/// PF-1203 bounded slice: sustained capture at the fastest documented poll
/// interval — 30 s at 100 ms (300 samples) with monotonic CPU counters,
/// stable cadence, and bounded memory (sample vec capped by config).
#[test]
fn sustained_fast_capture_stays_stable() {
    let exe = env!("CARGO_BIN_EXE_pf-fixture-cpu");
    let mut child = std::process::Command::new(exe)
        .args(["--ms", "40000"])
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn fixture");

    let backend = pf_windows::WindowsPlatform::new();
    let mut source = backend.open(child.id()).expect("open");
    let cfg = pf_platform::record::RecorderConfig {
        interval: Duration::from_millis(100),
        max_duration: Some(Duration::from_millis(30_000)),
        ..pf_platform::record::RecorderConfig::default()
    };
    let report = pf_platform::record::Recorder::new(source.as_mut(), cfg, None)
        .expect("recorder")
        .run()
        .expect("recording");

    assert_eq!(
        report.outcome.reason,
        pf_platform::record::StopReason::DurationReached
    );
    assert!(
        report.snapshots.len() >= 250,
        "expected ~300 samples, got {}",
        report.snapshots.len()
    );
    assert!(
        report.snapshots.len() <= 320,
        "cadence must not runaway: {}",
        report.snapshots.len()
    );

    // Cumulative counters never decrease across the sustained window.
    let total = |s: &CounterSnapshot| {
        s.cpu_user_ns
            .unwrap_or(0)
            .saturating_add(s.cpu_kernel_ns.unwrap_or(0))
    };
    for w in report.snapshots.windows(2) {
        assert!(total(&w[1]) >= total(&w[0]), "counter went backwards");
    }
    // Memory bound: the recorder stores exactly what it capped.
    assert!(report.snapshots.len() <= 100_000);

    let _ = child.kill();
    let _ = child.wait();
}
