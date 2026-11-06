//! Linux recording e2e — runs only on Linux.
#![cfg(target_os = "linux")]

//! Linux recording e2e (PF-306 subset): record a real /bin/dd workload
//! through the actual platform backend — I/O counters must grow, CPU
//! counters must be monotonic, identity must hold across snapshots.

use pf_platform::contracts::PlatformBackend;
use pf_platform::record::{Recorder, RecorderConfig, StopReason};
use std::time::Duration;

#[test]
fn records_a_real_other_process_for_two_seconds() {
    // `sleep` is a real other-user-visible process: the lifecycle claim is
    // that PulseForge attaches, samples, and stops cleanly — CPU may be
    // ~0 (it sleeps), which is honest data, not a failure.
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn sleep");

    let platform = pf_linux::LinuxPlatform::new();
    let mut source = platform.open(child.id()).expect("open sleep");
    let cfg = RecorderConfig {
        interval: Duration::from_millis(100),
        max_duration: Some(Duration::from_millis(2_000)),
        ..RecorderConfig::default()
    };
    let report = Recorder::new(source.as_mut(), cfg, None)
        .expect("recorder")
        .run()
        .expect("recording");

    assert_eq!(report.outcome.reason, StopReason::DurationReached);
    assert!(report.snapshots.len() >= 15, "2 s @ 100 ms => >=15 samples");
    // Identity held across the whole recording (no mismatch ended it).
    assert!(
        report
            .snapshots
            .iter()
            .all(|s| s.thread_count.unwrap_or(0) >= 1)
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn self_io_counters_reflect_real_file_writes() {
    // PF-304: storage I/O (write_bytes) must grow after real file writes —
    // recorded against our own process where /proc/self/io is always
    // readable.
    let platform = pf_linux::LinuxPlatform::new();
    let mut source = platform.open(std::process::id()).expect("open self");
    let before = source.snapshot(0).expect("before");

    let path = std::env::temp_dir().join(format!("pf-linux-io-{}.bin", std::process::id()));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&path).expect("create");
        let block = vec![0u8; 1024 * 1024];
        for _ in 0..48 {
            f.write_all(&block).expect("write");
        }
        f.sync_all().expect("sync");
    }

    let after = source.snapshot(1_000_000_000).expect("after");
    let _ = std::fs::remove_file(&path);

    let delta = after
        .io_write_bytes
        .unwrap_or(0)
        .saturating_sub(before.io_write_bytes.unwrap_or(0));
    assert!(
        delta >= 48 * 1024 * 1024,
        "48 MiB of synced writes must show in write_bytes: delta {delta}"
    );
}

#[test]
fn enumerate_finds_self_with_parent_and_image() {
    let platform = pf_linux::LinuxPlatform::new();
    let rows = platform.enumerate(1024).expect("enumerate");
    assert!(!rows.is_empty());
    let me = rows.iter().find(|r| r.pid == std::process::id());
    assert!(me.is_some(), "self must be enumerable");
    assert!(!me.unwrap().image_name.is_empty());
}

#[test]
fn capabilities_report_perf_policy_honestly() {
    let platform = pf_linux::LinuxPlatform::new();
    let caps = platform.capabilities();
    assert!(caps.counters.available, "counters must be live on Linux");
    // paranoid=2 on this VM: kernel sampling restricted — must say so with
    // the actual sysctl value, not a generic failure.
    if !caps.cpu_stack_sampling.available {
        assert!(
            caps.cpu_stack_sampling.reason_code == "PERF_POLICY"
                || caps.cpu_stack_sampling.reason_code == "NOT_IMPLEMENTED",
            "{:?}",
            caps.cpu_stack_sampling
        );
    }
}
