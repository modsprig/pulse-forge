//! PF-1003 per-thread sampling verification: `perf_event_open(pid=tgid)`
//! measures only the group-leader thread, so whole-process coverage needs
//! one event per tid. Both fixture threads spin from t0 — the process
//! sampler must report BOTH tids. Thread lifecycle: a thread spawned AFTER
//! open must appear on the next rescan.

#![cfg(target_os = "linux")]
#![allow(clippy::arithmetic_side_effects, clippy::cast_possible_wrap)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use pf_linux::perf::ProcessSampler;

fn compile(tag: &str, src: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pf-threads-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let src_path = dir.join("fixture.rs");
    std::fs::write(&src_path, src).unwrap();
    let out = dir.join("fixture");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let st = Command::new(&rustc)
        .arg("--edition")
        .arg("2021")
        .arg("-C")
        .arg("opt-level=2")
        .arg("-o")
        .arg(&out)
        .arg(&src_path)
        .status()
        .unwrap_or_else(|e| panic!("run {rustc}: {e}"));
    assert!(st.success(), "fixture compilation failed");
    out
}

fn spin_two_threads_src() -> &'static str {
    r#"
fn main() {
    let worker = std::thread::spawn(|| {
        let end = std::time::Instant::now() + std::time::Duration::from_millis(4000);
        let mut x = 0u64;
        while std::time::Instant::now() < end { x = x.wrapping_add(1); }
        x
    });
    let end = std::time::Instant::now() + std::time::Duration::from_millis(4000);
    let mut x = 0u64;
    while std::time::Instant::now() < end { x = x.wrapping_add(1); }
    let _ = worker.join();
    std::process::exit(if x == u64::MAX { 1 } else { 0 });
}
"#
}

fn late_thread_src() -> &'static str {
    r#"
fn main() {
    // Leader idles; a second, hot thread appears ~600 ms in.
    std::thread::sleep(std::time::Duration::from_millis(600));
    let h = std::thread::spawn(|| {
        let end = std::time::Instant::now() + std::time::Duration::from_millis(2500);
        let mut x = 0u64;
        while std::time::Instant::now() < end { x = x.wrapping_add(1); }
        x
    });
    let _ = h.join();
}
"#
}

#[test]
fn process_sampler_covers_both_hot_threads() {
    let bin = compile("two", spin_two_threads_src());
    let mut child = Command::new(&bin)
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn fixture");
    std::thread::sleep(Duration::from_millis(150));

    let mut sampler = ProcessSampler::open_for_process(child.id() as i32, 1_000_000).expect("open");
    assert!(
        sampler.threads_sampled() >= 2,
        "both hot threads must be open at start, got {}",
        sampler.threads_sampled()
    );
    sampler.start();
    std::thread::sleep(Duration::from_millis(600));
    sampler.stop();
    let samples = sampler.collect();
    let mut tids: Vec<u32> = samples.iter().map(|s| s.tid).collect();
    tids.sort_unstable();
    tids.dedup();
    eprintln!("E2E-THREADS tids={tids:?} samples={}", samples.len());
    assert!(
        !samples.is_empty(),
        "600 ms of hot spinning must produce samples"
    );
    assert!(
        tids.len() >= 2,
        "both spinning threads must be sampled, got tids {tids:?}"
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(bin.parent().unwrap());
}

#[test]
fn rescan_discovers_threads_born_after_open() {
    let bin = compile("late", late_thread_src());
    let mut child = Command::new(&bin)
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn fixture");
    std::thread::sleep(Duration::from_millis(100));

    // Opened while only the (idle) leader exists.
    let mut sampler = ProcessSampler::open_for_process(child.id() as i32, 1_000_000).expect("open");
    sampler.start();

    // Poll-rescan until the late thread is discovered (bounded).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while sampler.threads_sampled() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "rescan never discovered the late thread"
        );
        std::thread::sleep(Duration::from_millis(200));
        sampler.rescan();
    }
    // Sample the window where the late thread is hot.
    std::thread::sleep(Duration::from_millis(600));
    sampler.stop();
    let samples = sampler.collect();
    assert!(!samples.is_empty(), "the hot late thread must be sampled");
    let leader_tid = child.id();
    // The leader only sleeps, so every sample must come from the thread
    // that rescan discovered AFTER open — proof of lifecycle coverage.
    let non_leader: Vec<u32> = {
        let mut t: Vec<u32> = samples.iter().map(|s| s.tid).collect();
        t.sort_unstable();
        t.dedup();
        t
    };
    eprintln!(
        "E2E-LATE-TID tids={non_leader:?} leader={leader_tid} samples={}",
        samples.len()
    );
    assert!(
        non_leader.iter().all(|t| *t != leader_tid),
        "idle leader must produce no samples"
    );
    assert!(
        !non_leader.is_empty(),
        "samples must come from the discovered late thread"
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(bin.parent().unwrap());
}

#[test]
fn sequential_sub_second_threads_are_all_sampled() {
    // Three workers, each spinning only 400 ms, started SEQUENTIALLY —
    // each lives for fewer than the old 1 s rescan cadence, but well over
    // the 100 ms tick the sampler uses now. All three tids must appear.
    let src = r#"
fn main() {
    for _ in 0..3 {
        let h = std::thread::spawn(|| {
            let end = std::time::Instant::now() + std::time::Duration::from_millis(400);
            let mut x = 0u64;
            while std::time::Instant::now() < end { x = x.wrapping_add(1); }
            x
        });
        h.join().unwrap();
    }
}
"#;
    let bin = compile("short", src);
    let mut child = Command::new(&bin)
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn fixture");
    std::thread::sleep(Duration::from_millis(100));

    let mut sampler = ProcessSampler::open_for_process(child.id() as i32, 1_000_000).expect("open");
    sampler.start();

    // Drain for the fixture's lifetime, rescanning every tick.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut all_tids: Vec<u32> = Vec::new();
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        sampler.rescan();
        for s in sampler.collect() {
            all_tids.push(s.tid);
        }
        let _ = child.try_wait();
        if child.try_wait().unwrap().is_some() {
            // Final drain after exit.
            sampler.rescan();
            for s in sampler.collect() {
                all_tids.push(s.tid);
            }
            break;
        }
    }
    let _ = child.wait();
    all_tids.sort_unstable();
    all_tids.dedup();
    eprintln!("E2E-SHORT-THREADS distinct tids={all_tids:?}");
    assert!(
        all_tids.len() >= 3,
        "three sequential sub-second workers must all be sampled, got {all_tids:?}"
    );
    let _ = std::fs::remove_dir_all(bin.parent().unwrap());
}

#[test]
fn cpu_attribution_observes_migration_behavior() {
    // PF-1003 remainder examination: PERF_SAMPLE_CPU records the CPU each
    // sample ran on. A multi-threaded fixture on a multi-CPU machine shows
    // per-tid CPU sets; the evidence is reported honestly (migration
    // behavior is a scheduling fact, not an assertion).
    let src = r#"
fn main() {
    let workers: Vec<_> = (0..2)
        .map(|_| {
            std::thread::spawn(|| {
                let end = std::time::Instant::now() + std::time::Duration::from_millis(3500);
                let mut x = 0u64;
                while std::time::Instant::now() < end { x = x.wrapping_add(1); }
                x
            })
        })
        .collect();
    let end = std::time::Instant::now() + std::time::Duration::from_millis(3500);
    let mut x = 0u64;
    while std::time::Instant::now() < end { x = x.wrapping_add(1); }
    for w in workers { let _ = w.join(); }
    std::process::exit(if x == u64::MAX { 1 } else { 0 });
}
"#;
    let bin = compile("cpus", src);
    let mut child = Command::new(&bin)
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn fixture");
    std::thread::sleep(Duration::from_millis(150));

    let mut sampler = ProcessSampler::open_for_process(child.id() as i32, 1_000_000).expect("open");
    sampler.start();

    // Sample the hot window, collecting (tid, cpu) pairs.
    let mut tid_cpus: std::collections::HashMap<u32, std::collections::HashSet<u32>> =
        std::collections::HashMap::new();
    let mut total = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        sampler.rescan();
        for s in sampler.collect() {
            tid_cpus.entry(s.tid).or_default().insert(s.cpu);
            total += 1;
        }
    }
    sampler.stop();
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        total > 500,
        "hot 3 s window must produce samples, got {total}"
    );
    // The evidence examination: per-tid CPU sets and migrations.
    let mut migrations = 0usize;
    for (tid, cpus) in &tid_cpus {
        if cpus.len() > 1 {
            migrations += 1;
        }
        eprintln!(
            "E2E-CPU tid={tid} cpus={:?} samples_spanning={}",
            {
                let mut v: Vec<u32> = cpus.iter().copied().collect();
                v.sort_unstable();
                v
            },
            cpus.len()
        );
    }
    eprintln!(
        "E2E-CPU total samples={total} tids={} migrating_tids={migrations}",
        tid_cpus.len()
    );
    // Sanity only (scheduling is a scheduling fact): every recorded cpu is
    // a plausible CPU id and at least one thread was sampled.
    assert!(!tid_cpus.is_empty());
    for cpus in tid_cpus.values() {
        for c in cpus {
            assert!(*c < 1024, "cpu id {c} implausible");
        }
    }
    let _ = std::fs::remove_dir_all(bin.parent().unwrap());
}
