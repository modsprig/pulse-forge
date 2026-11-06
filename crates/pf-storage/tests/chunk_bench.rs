//! Timing bench for the chunk store's recording and read-back paths
//! (`#[ignore]`d; run with `--ignored --nocapture`). The writer is the
//! live recording path (every counter snapshot); the reader feeds the GUI
//! dashboard charts and the report. Numbers are printed for the ledger
//! and never asserted against (roadmap §16).

#![allow(clippy::arithmetic_side_effects)]

use std::time::Instant;

use pf_storage::{ChunkReader, ChunkWriter};
use pf_types::CounterSnapshot;

const N: usize = 100_000;

fn synthetic_snapshot(i: u64) -> CounterSnapshot {
    CounterSnapshot {
        mono_ns: i * 250_000_000,
        cpu_user_ns: Some(i * 1_000_003),
        cpu_kernel_ns: Some(i * 500_007),
        resident_bytes: Some(48_000_000 + (i % 8_192) * 4_096),
        resident_peak_bytes: Some(64_000_000),
        private_commit_bytes: Some(32_000_000 + (i % 4_096) * 4_096),
        private_commit_peak_bytes: Some(40_000_000),
        io_read_ops: Some(i * 3),
        io_write_ops: Some(i * 2),
        io_read_bytes: Some(i * 65_536),
        io_write_bytes: Some(i * 32_768),
        thread_count: Some(6),
        handle_count: Some(200),
        page_faults: Some(i * 41),
    }
}

fn median(v: &[f64]) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

#[test]
#[ignore = "timing bench: run with `cargo test -p pf-storage --test chunk_bench -- --ignored --nocapture`"]
fn chunk_write_read_throughput() {
    let dir = std::env::temp_dir().join(format!("pf-chunk-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create bench dir");

    // Write: 100k snapshots through a rotating writer with the daemon's
    // 1 s flush period.
    let mut write_ms = Vec::new();
    let mut bytes = 0u64;
    for _ in 0..5 {
        let sub = dir.join(format!("w{}", write_ms.len()));
        std::fs::create_dir_all(&sub).expect("create session dir");
        let t0 = Instant::now();
        let mut w = ChunkWriter::open(&sub).expect("writer opens");
        for i in 0..N as u64 {
            w.push(&synthetic_snapshot(i)).expect("push");
        }
        w.seal().expect("seal");
        write_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        bytes = std::fs::read_dir(sub.join("chunks"))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
                    .sum()
            })
            .unwrap_or(0);
    }

    // Read back: full-range page + envelope on a fresh reader.
    let main = dir.join("w0");
    let mut page_ms = Vec::new();
    let mut env_ms = Vec::new();
    for _ in 0..5 {
        let r = ChunkReader::open(&main).expect("reader opens");
        assert_eq!(r.total_records(), N);
        let t0 = Instant::now();
        let page = r.range(0, u64::MAX, 1_000_000).expect("range query");
        page_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(page.snapshots.len(), N);
        let t0 = Instant::now();
        let env = r
            .envelope(0, u64::MAX, 64, pf_storage::EnvelopeField::ResidentBytes)
            .expect("envelope");
        env_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(env.len(), 64);
    }

    eprintln!(
        "chunk_bench: write {N} records median {:.1} ms ({:.0}k rec/s, {:.1} MB), \
         full-range read median {:.1} ms ({:.0}k rec/s), envelope(64) median {:.2} ms",
        median(&write_ms),
        N as f64 / (median(&write_ms) / 1000.0) / 1000.0,
        bytes as f64 / 1_048_576.0,
        median(&page_ms),
        N as f64 / (median(&page_ms) / 1000.0) / 1000.0,
        median(&env_ms),
    );

    let _ = std::fs::remove_dir_all(&dir);
}
