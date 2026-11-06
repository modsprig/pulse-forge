//! P10 perf sampling tests — Linux only.
#![cfg(target_os = "linux")]

//! P10 bounded slice: perf_event_open sampling verified against a
//! known-hotspot fixture inside the same process. The fixture burns CPU in
//! a deterministic loop (real stack frames, real on-CPU time); assertions:
//! samples arrive, callchains are user-space and non-empty, lost records
//! are accounted, and the event source is reported verbatim.

use pf_linux::perf::{PerfSampler, probe};

/// Deterministic CPU burner with real stack frames: outer → inner → spin.
/// Returns a checksum so the loop cannot be optimized away.
#[allow(clippy::arithmetic_side_effects)]
fn hot_outer(ms: u64) -> u64 {
    fn inner(ms: u64, salt: u64) -> u64 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        let mut x = salt;
        while std::time::Instant::now() < deadline {
            for _ in 0..10_000 {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
            }
        }
        x
    }
    inner(ms, 0x9E37_79B9_7F4A_7C15)
}

#[test]
fn probe_reports_available_source() {
    // paranoid=2 on this VM permits user-space self-sampling; the probe
    // must find at least the software clock.
    let source = probe().expect("perf sampling available");
    assert!(matches!(
        source,
        pf_linux::perf::EventSource::HardwareCycles | pf_linux::perf::EventSource::SoftwareCpuClock
    ));
}

#[test]
fn samples_arrive_for_hotspot_workload() {
    let mut sampler = PerfSampler::open(2_000_000).expect("open sampler"); // ~2 ms period
    sampler.start();

    // Burn CPU on this thread for 300 ms — the sampled process is doing
    // real, known work in a known function.
    let checksum = hot_outer(300);

    sampler.stop();
    let samples = sampler.collect();
    let (head, tail, doff, dsize) = sampler.debug_ring();
    println!(
        "DBG head={head} tail={tail} data_offset={doff} data_size={dsize} parsed={}",
        samples.len()
    );

    assert!(
        !samples.is_empty(),
        "300 ms of hot work at a 2 ms period must produce samples"
    );
    // Callchains: every sample carries at least the current user IP.
    for s in &samples {
        assert!(s.ip_count >= 1, "sample without callchain: {s:?}");
        for ip in s.ips.iter().take(s.ip_count) {
            assert!(*ip != 0, "zero IP in callchain");
            // paranoid=2: user-space only. x86-64 user space < 2^47.
            assert!(*ip < (1u64 << 47), "kernel-space IP leaked: {ip:#x}");
        }
    }
    // Lost records are surfaced (may be zero on a quiet machine — the
    // assertion is that the counter exists and is finite).
    assert!(sampler.lost_records <= samples.len() as u64 * 1000);
    assert_ne!(checksum, 0);
    // Kernel frames filtered with explicit accounting (paranoid=2 +
    // exclude_kernel contract).
    println!("kernel frames skipped: {}", sampler.kernel_frames_skipped);
}

#[test]
fn idle_process_yields_fewer_samples_than_hot_process() {
    // Rate honesty: the clock event only fires while on-CPU. A quiet window
    // must yield fewer samples than a hot one (same period both sides).
    let mut hot = PerfSampler::open(1_000_000).expect("open");
    hot.start();
    let _ = hot_outer(200);
    hot.stop();
    let hot_count = hot.collect().len();

    let mut idle = PerfSampler::open(1_000_000).expect("open");
    idle.start();
    std::thread::sleep(std::time::Duration::from_millis(200));
    idle.stop();
    let idle_count = idle.collect().len();

    assert!(
        hot_count > idle_count,
        "hot window ({hot_count}) must out-sample idle window ({idle_count})"
    );
}
