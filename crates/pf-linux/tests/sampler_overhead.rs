//! Observer-overhead study for the STACK SAMPLER itself (`#[ignore]`d;
//! run with `--ignored --nocapture`). The observation study (PF-210)
//! measured counters-only recording; this isolates the marginal cost the
//! per-thread perf sampler adds to the OBSERVED process: a deterministic
//! spinner's wall time with the sampler attached vs not.
//!
//! Method: the same fixed-iteration spin runs in a worker thread while
//! the main thread either idles (bare cell) or drives the sampler's
//! production cadence (open → start → 100 ms collect/rescan drain →
//! stop). Medians over 7 reps; printed for the ledger, never asserted
//! against (roadmap §16). Linux-only (perf_event_open).

#![cfg(target_os = "linux")]
#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::time::{Duration, Instant};

use pf_linux::perf::ProcessSampler;

const ITERATIONS: u64 = 1_500_000_000; // ~0.5 s of deterministic spin
const REPS: usize = 7;
const DRAIN_MS: u64 = 100; // the production drain cadence

/// Deterministic CPU spin (same shape as pf-test-support's spin_ms):
/// fixed iterations, wrapping checksum, blackboxed against elision.
fn spin(iterations: u64) -> u64 {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    for i in 0..iterations {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(i | 1);
    }
    std::hint::black_box(x)
}

/// Run the spin on a worker thread and return its wall time.
fn spin_wall() -> Duration {
    let handle = std::thread::spawn(|| spin(ITERATIONS));
    let t0 = Instant::now();
    let _ = handle.join().expect("spinner joins");
    t0.elapsed()
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[test]
#[ignore = "timing study: run with `cargo test -p pf-linux --test sampler_overhead -- --ignored --nocapture`"]
fn stack_sampler_marginal_overhead() {
    // Warm-up rep (page cache, scheduler settle) — not recorded.
    let _ = spin_wall();

    let mut bare = Vec::with_capacity(REPS);
    for _ in 0..REPS {
        bare.push(spin_wall());
    }

    let mut observed = Vec::with_capacity(REPS);
    let mut samples_total = 0u64;
    for _ in 0..REPS {
        // The production path: one event per discovered thread, the main
        // thread drives the 100 ms drain cadence (rescan + collect) while
        // the spinner spins — the drain work is part of the observer
        // overhead being measured.
        let pid = i32::try_from(std::process::id()).unwrap_or(1);
        let mut sampler = ProcessSampler::open_for_process(pid, 1_000_000)
            .expect("sampler opens (perf_event_paranoid permitting)");
        sampler.start();
        let handle = std::thread::spawn(|| spin(ITERATIONS));
        let t0 = Instant::now();
        while !handle.is_finished() {
            std::thread::sleep(Duration::from_millis(DRAIN_MS));
            sampler.rescan();
            samples_total += sampler.collect().len() as u64;
        }
        let _ = handle.join().expect("spinner joins");
        let wall = t0.elapsed();
        sampler.stop();
        observed.push(wall);
        eprintln!(
            "rep: threads_sampled={} unsampled={} lost={}",
            sampler.threads_sampled(),
            sampler.threads_unsampled,
            sampler.lost_records
        );
    }

    let bare_med = median(bare);
    let obs_med = median(observed);
    let delta_ns = obs_med.as_nanos().saturating_sub(bare_med.as_nanos()) as f64;
    let delta_pct = delta_ns / bare_med.as_nanos() as f64 * 100.0;
    eprintln!(
        "sampler_overhead: bare median {:.1} ms, observed median {:.1} ms, \
         delta {:.1} ms ({delta_pct:.1}%), samples drained {samples_total}",
        bare_med.as_secs_f64() * 1000.0,
        obs_med.as_secs_f64() * 1000.0,
        delta_ns / 1_000_000.0,
    );

    // Third cell: a 10x coarser period — the overhead must scale with
    // sample rate, and the ledger needs the curve, not one point.
    let mut coarse = Vec::with_capacity(REPS);
    let mut coarse_samples = 0u64;
    for _ in 0..REPS {
        let pid = i32::try_from(std::process::id()).unwrap_or(1);
        let mut sampler =
            ProcessSampler::open_for_process(pid, 10_000_000).expect("coarse sampler opens");
        sampler.start();
        let handle = std::thread::spawn(|| spin(ITERATIONS));
        let t0 = Instant::now();
        while !handle.is_finished() {
            std::thread::sleep(Duration::from_millis(DRAIN_MS));
            sampler.rescan();
            coarse_samples += sampler.collect().len() as u64;
        }
        let _ = handle.join().expect("spinner joins");
        let wall = t0.elapsed();
        sampler.stop();
        coarse.push(wall);
    }

    let coarse_med = median(coarse);
    let cdelta = coarse_med.as_nanos().saturating_sub(bare_med.as_nanos()) as f64;
    let cdelta_pct = cdelta / bare_med.as_nanos() as f64 * 100.0;
    let spin_s = 0.55; // the spin's approximate wall time
    eprintln!(
        "sampler_overhead: coarse(10M period) median {:.1} ms, delta {:.1} ms \
         ({cdelta_pct:.1}%), ~{:.0}k samples/s (vs {:.0}k/s at 1M period)",
        coarse_med.as_secs_f64() * 1000.0,
        cdelta / 1_000_000.0,
        coarse_samples as f64 / REPS as f64 / spin_s / 1000.0,
        samples_total as f64 / REPS as f64 / spin_s / 1000.0,
    );

    // Duration sweep: does the delta amortize down as sessions get
    // longer? On this shared machine the calibration is systematically
    // off (~4x — code-shape-dependent codegen between calibration and
    // worker calls) and the deltas are variance-dominated; the sweep is
    // INCONCLUSIVE and the numbers below are raw evidence, not a curve.
    // Robust calibration: 3 reps of the SAME call shape, median — a
    // single cold rep mis-estimated badly on a noisy machine.
    let per_iter_ns = {
        let mut calib = Vec::with_capacity(3);
        for _ in 0..3 {
            let t0 = Instant::now();
            let _ = spin(ITERATIONS);
            calib.push(t0.elapsed().as_nanos() as f64 / ITERATIONS as f64);
        }
        calib.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        calib[1]
    };
    for dur_s in [0.5f64, 2.0, 8.0] {
        let iters = u64::try_from((dur_s * 1_000_000_000.0 / per_iter_ns) as i64).unwrap_or(1);
        let mut bare_d = Vec::with_capacity(5);
        for _ in 0..5 {
            let h = std::thread::spawn(move || spin(iters));
            let t0 = Instant::now();
            let _ = h.join().expect("joins");
            bare_d.push(t0.elapsed());
        }
        let mut obs_d = Vec::with_capacity(5);
        for _ in 0..5 {
            let pid = i32::try_from(std::process::id()).unwrap_or(1);
            let mut sampler =
                ProcessSampler::open_for_process(pid, 1_000_000).expect("sampler opens");
            sampler.start();
            let h = std::thread::spawn(move || spin(iters));
            let t0 = Instant::now();
            while !h.is_finished() {
                std::thread::sleep(Duration::from_millis(DRAIN_MS));
                sampler.rescan();
                let _ = sampler.collect();
            }
            let _ = h.join().expect("joins");
            let wall = t0.elapsed();
            sampler.stop();
            obs_d.push(wall);
        }
        let b = median(bare_d);
        let o = median(obs_d);
        let d = o.as_nanos().saturating_sub(b.as_nanos()) as f64;
        let pct = d / b.as_nanos() as f64 * 100.0;
        eprintln!(
            "sampler_overhead: nominal {:.1} s (actual bare {:.1} s) — bare {:.1} ms, \
             observed {:.1} ms, delta {:.1} ms ({pct:.1}%)",
            dur_s,
            b.as_secs_f64(),
            b.as_secs_f64() * 1000.0,
            o.as_secs_f64() * 1000.0,
            d / 1_000_000.0,
        );
    }
}

/// Thread-count sweep (PF-1202-adjacent measurement): does the sampler's
/// fixed delta grow with the number of observed THREADS (per-event cost:
/// one perf event per thread + per-thread ring) or stay flat (per-session
/// cost)? Production targets run 8-32 threads; the answer changes the
/// story from "~12% at short sessions" to a per-thread multiplier.
#[test]
#[ignore = "timing study: run with `cargo test -p pf-linux --test sampler_overhead thread_sweep -- --ignored --nocapture`"]
fn sampler_overhead_thread_sweep() {
    // Each configuration: N spinner threads each spinning for ~0.5 s of
    // wall (iterations split across threads), bare vs observed (sampler
    // attached, production 100 ms drain cadence). 5 reps per cell.
    let wall_iters = 1_500_000_000u64; // ~0.55 s single-thread equivalent

    for spinners in [1usize, 4, 16] {
        // EVERY thread spins the FULL length concurrently — the wall stays
        // ~constant across configs and every spinner lives through the
        // rescan window (short-lived threads fall in the honest 100 ms
        // blind window and are never sampled).
        let per_thread = wall_iters;

        // Warm-up.
        let hs: Vec<_> = (0..spinners)
            .map(|_| std::thread::spawn(move || spin(per_thread)))
            .collect();
        for h in hs {
            let _ = h.join();
        }

        let mut bare = Vec::with_capacity(5);
        for _ in 0..5 {
            let hs: Vec<_> = (0..spinners)
                .map(|_| std::thread::spawn(move || spin(per_thread)))
                .collect();
            let t0 = Instant::now();
            for h in hs {
                let _ = h.join().expect("joins");
            }
            bare.push(t0.elapsed());
        }

        let mut observed = Vec::with_capacity(5);
        let mut sampled = 0u64;
        for _ in 0..5 {
            let pid = i32::try_from(std::process::id()).unwrap_or(1);
            let mut sampler =
                ProcessSampler::open_for_process(pid, 1_000_000).expect("sampler opens");
            sampler.start();
            let hs: Vec<_> = (0..spinners)
                .map(|_| std::thread::spawn(move || spin(per_thread)))
                .collect();
            let t0 = Instant::now();
            while hs.iter().any(|h| !h.is_finished()) {
                std::thread::sleep(Duration::from_millis(DRAIN_MS));
                sampler.rescan();
                sampled += sampler.collect().len() as u64;
                if t0.elapsed() > Duration::from_secs(3) {
                    break;
                }
            }
            for h in hs {
                let _ = h.join().expect("joins");
            }
            let wall = t0.elapsed();
            sampler.stop();
            observed.push(wall);
        }

        let b = median(bare);
        let o = median(observed);
        let d = o.as_nanos().saturating_sub(b.as_nanos()) as f64;
        let pct = d / b.as_nanos() as f64 * 100.0;
        eprintln!(
            "thread_sweep: spinners={spinners} events={} — bare {:.1} ms, \
             observed {:.1} ms, delta {:.1} ms ({pct:.1}%), samples {sampled}",
            spinners + 1,
            b.as_secs_f64() * 1000.0,
            o.as_secs_f64() * 1000.0,
            d / 1_000_000.0,
        );
    }
}
