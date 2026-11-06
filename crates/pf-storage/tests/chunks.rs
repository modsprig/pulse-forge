// Test arithmetic on synthetic series bounded by explicit caps.
#![allow(clippy::arithmetic_side_effects)]

//! Durable chunk store tests (PF-502/504/505/506/508/509): round-trips at
//! scale, bounded ranged queries, CRC corruption salvage, crash simulation
//! via `.partial` files, spike-preserving envelopes, append continuation.

use pf_storage::{ChunkReader, ChunkWriter, EnvelopeField};
use pf_types::CounterSnapshot;

fn snap(mono_ns: u64, ws: u64) -> CounterSnapshot {
    CounterSnapshot {
        mono_ns,
        cpu_user_ns: Some(mono_ns / 100),
        cpu_kernel_ns: Some(0),
        resident_bytes: Some(ws),
        resident_peak_bytes: Some(ws),
        private_commit_bytes: Some(ws / 2),
        private_commit_peak_bytes: Some(ws / 2),
        io_read_ops: Some(mono_ns % 97),
        io_write_ops: Some(mono_ns % 89),
        io_read_bytes: Some(mono_ns % 101 * 4096),
        io_write_bytes: Some(mono_ns % 103 * 4096),
        thread_count: Some(2 + (mono_ns % 5) as u32),
        handle_count: Some(100 + (mono_ns % 50) as u32),
        page_faults: Some(mono_ns % 7),
    }
}

fn temp_session(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pf-chunks-{tag}-{}-{}",
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
fn roundtrip_small_session() {
    let dir = temp_session("small");
    let mut w = ChunkWriter::open(&dir).unwrap();
    for i in 0..100u64 {
        w.push(&snap(i * 250_000_000, 4096 * (i + 1))).unwrap();
    }
    w.seal().unwrap();
    let r = ChunkReader::open(&dir).unwrap();
    assert_eq!(r.chunk_count(), 1);
    assert_eq!(r.total_records(), 100);
    assert_eq!(r.time_range(), Some((0, 99 * 250_000_000)));
    let page = r.range(0, u64::MAX, 1000).unwrap();
    assert_eq!(page.snapshots.len(), 100);
    assert!(!page.truncated);
    // Spot-check values survive exactly.
    assert_eq!(page.snapshots[7].resident_bytes, Some(4096 * 8));
    assert_eq!(page.snapshots[7].cpu_user_ns, Some(7 * 250_000_000 / 100));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rotation_and_append_continuation() {
    let dir = temp_session("rotate");
    let records = pf_storage::MAX_RECORDS_PER_CHUNK as u64;
    {
        let mut w = ChunkWriter::open(&dir).unwrap();
        for i in 0..records {
            w.push(&snap(i, 1024)).unwrap();
        }
        w.seal().unwrap();
        assert_eq!(w.chunks_written(), 1);
    }
    // A NEW writer (new process simulation) continues after the sealed index.
    {
        let mut w = ChunkWriter::open(&dir).unwrap();
        for i in records..records * 2 {
            w.push(&snap(i, 1024)).unwrap();
        }
        w.seal().unwrap();
        assert_eq!(w.chunks_written(), 1);
    }
    let r = ChunkReader::open(&dir).unwrap();
    assert_eq!(r.chunk_count(), 2, "two sealed chunks");
    assert_eq!(r.total_records() as u64, records * 2);
    let page = r.range(0, u64::MAX, 10).unwrap();
    assert!(page.truncated);
    assert_eq!(page.total_in_range as u64, records * 2);
    // Stride sampling must include both extremes.
    assert_eq!(page.snapshots.first().map(|s| s.mono_ns), Some(0));
    assert_eq!(
        page.snapshots.last().map(|s| s.mono_ns),
        Some(records * 2 - 1)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ranged_query_returns_exact_window_and_respects_cap() {
    let dir = temp_session("range");
    let mut w = ChunkWriter::open(&dir).unwrap();
    for i in 0..1000u64 {
        w.push(&snap(i * 1_000_000, 4096)).unwrap();
    }
    w.seal().unwrap();
    let r = ChunkReader::open(&dir).unwrap();

    let page = r.range(100_000_000, 200_000_000, 10_000).unwrap();
    assert_eq!(page.snapshots.len(), 101); // inclusive bounds
    assert_eq!(page.snapshots[0].mono_ns, 100_000_000);
    assert_eq!(page.snapshots[100].mono_ns, 200_000_000);

    let capped = r.range(0, u64::MAX, 10).unwrap();
    assert!(capped.truncated);
    assert_eq!(capped.snapshots.len(), 10, "cap respected exactly");
    assert_eq!(capped.total_in_range, 1000);

    assert!(r.range(0, u64::MAX, 0).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn crc_corruption_is_surfaced_not_swallowed() {
    let dir = temp_session("corrupt");
    let mut w = ChunkWriter::open(&dir).unwrap();
    for i in 0..50u64 {
        w.push(&snap(i * 1_000, 4096)).unwrap();
    }
    w.seal().unwrap();
    let chunk_path = dir.join("chunks/metrics-0000000001.pfc");
    let original = std::fs::read(&chunk_path).unwrap();

    // Flip one payload byte near the end (inside records).
    let mut corrupted = original.clone();
    let mid = corrupted.len() / 2;
    corrupted[mid] ^= 0xFF;
    std::fs::write(&chunk_path, &corrupted).unwrap();

    let r = ChunkReader::open(&dir).unwrap();
    assert_eq!(r.corrupt_chunk_count(), 1);
    assert!(r.total_records() < 50, "corrupt chunk excluded from counts");
    assert!(
        r.salvage_notes().iter().any(|n| n.contains("CRC mismatch")),
        "{:?}",
        r.salvage_notes()
    );
    let page = r.range(0, u64::MAX, 100).unwrap();
    assert!(page.snapshots.is_empty(), "corrupt data must not be served");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn partial_files_are_evidence_not_data() {
    let dir = temp_session("partial");
    let mut w = ChunkWriter::open(&dir).unwrap();
    for i in 0..10u64 {
        w.push(&snap(i * 1_000, 4096)).unwrap();
    }
    w.seal().unwrap();
    // Simulate a crash mid-write: a stray .partial next to sealed chunks.
    std::fs::write(
        dir.join("chunks/metrics-0000000002.pfc.partial"),
        b"\x52\x42\x46\x50garbage-not-a-chunk",
    )
    .unwrap();

    let r = ChunkReader::open(&dir).unwrap();
    assert_eq!(r.partial_file_count(), 1);
    assert_eq!(r.chunk_count(), 1);
    assert_eq!(r.total_records(), 10, "sealed chunk still fully readable");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn truncated_chunk_file_is_rejected_as_partial() {
    let dir = temp_session("truncated");
    let mut w = ChunkWriter::open(&dir).unwrap();
    for i in 0..10u64 {
        w.push(&snap(i * 1_000, 4096)).unwrap();
    }
    w.seal().unwrap();
    let chunk_path = dir.join("chunks/metrics-0000000001.pfc");
    let original = std::fs::read(&chunk_path).unwrap();
    // Cut the file mid-record: CRC over fewer bytes mismatches.
    std::fs::write(&chunk_path, &original[..original.len() - 20]).unwrap();

    let r = ChunkReader::open(&dir).unwrap();
    assert_eq!(r.corrupt_chunk_count(), 1, "truncation must be detected");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn envelope_preserves_spikes_and_gaps() {
    let dir = temp_session("envelope");
    let mut w = ChunkWriter::open(&dir).unwrap();
    // Baseline 4 MiB samples, one 512 MiB spike at t=50M, and a full gap
    // covering the last bucket (t >= 75M).
    for i in 0..100u64 {
        let t = i * 1_000_000;
        if i >= 75 {
            continue; // gap: the entire last bucket
        }
        let ws = if i == 50 {
            512 * 1024 * 1024
        } else {
            4 * 1024 * 1024
        };
        w.push(&snap(t, ws)).unwrap();
    }
    w.seal().unwrap();
    let r = ChunkReader::open(&dir).unwrap();
    let buckets = r
        .envelope(0, 99_000_000, 4, EnvelopeField::ResidentBytes)
        .unwrap();
    assert_eq!(buckets.len(), 4);
    // Spike lives in bucket 2 (t=50M of [50M,75M)); min/max must show it.
    let spike_bucket = &buckets[2];
    assert_eq!(spike_bucket.max, Some(512.0 * 1024.0 * 1024.0));
    assert_eq!(spike_bucket.min, Some(4.0 * 1024.0 * 1024.0));
    // Average-only downsampling would hide the spike at 4 MiB.
    // Gap bucket: count 0, no values.
    let gap = &buckets[3];
    assert_eq!(gap.count, 0);
    assert!(gap.max.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn writer_rejects_unordered_snapshots() {
    let dir = temp_session("order");
    let mut w = ChunkWriter::open(&dir).unwrap();
    w.push(&snap(1_000, 10)).unwrap();
    w.push(&snap(500, 10)).unwrap(); // out of order
    let err = w.seal().unwrap_err();
    assert_eq!(err.code, pf_types::ErrorCode::InvalidArgument);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hundred_thousand_record_roundtrip_is_exact_and_bounded() {
    let dir = temp_session("100k");
    let mut w = ChunkWriter::open(&dir).unwrap();
    for i in 0..100_000u64 {
        w.push(&snap(i * 250_000, 4096)).unwrap();
    }
    w.seal().unwrap();
    assert!(
        w.chunks_written() >= 12,
        "rotation expected: {}",
        w.chunks_written()
    );
    let r = ChunkReader::open(&dir).unwrap();
    assert_eq!(r.total_records(), 100_000);
    // Bounded query must not materialize everything.
    let page = r.range(0, u64::MAX, 500).unwrap();
    assert_eq!(page.snapshots.len(), 500, "cap respected exactly");
    assert_eq!(page.total_in_range, 100_000);
    // Narrow window query returns exact values.
    let win = r.range(10_000 * 250_000, 10_003 * 250_000, 1_000).unwrap();
    assert_eq!(win.snapshots.len(), 4);
    assert_eq!(win.snapshots[2].resident_bytes, Some(4096));
    let _ = std::fs::remove_dir_all(&dir);
}

/// PF-509: 1M-record write/query/read scale. Runs in release test builds
/// only (debug takes minutes); CI's test job runs debug — the gate still
/// executes via `cargo test --release` in the cross-check job.
#[test]
#[cfg(not(debug_assertions))]
fn million_record_scale_is_bounded_and_exact() {
    let dir = temp_session("1m");
    let total: u64 = 1_000_000;
    {
        let mut w = ChunkWriter::open(&dir).unwrap();
        for i in 0..total {
            // Cheap synthetic values; the point is scale, not content.
            let mut s = snap(i * 250, 4096);
            s.cpu_user_ns = Some(i);
            w.push(&s).unwrap();
        }
        w.seal().unwrap();
    }
    let r = ChunkReader::open(&dir).unwrap();
    assert_eq!(r.total_records() as u64, total);
    assert_eq!(r.corrupt_chunk_count(), 0);
    // Bounded query: memory stays capped, stride keeps both ends.
    let page = r.range(0, u64::MAX, 1_000).unwrap();
    assert_eq!(page.snapshots.len(), 1_000);
    assert!(page.truncated);
    assert_eq!(page.total_in_range as u64, total);
    assert_eq!(page.snapshots.first().map(|s| s.mono_ns), Some(0));
    assert_eq!(
        page.snapshots.last().map(|s| s.mono_ns),
        Some((total - 1) * 250)
    );
    // Narrow window: exact values, no full scan cost visible to the caller.
    let win = r.range(500_000 * 250, 500_004 * 250, 10_000).unwrap();
    assert_eq!(win.snapshots.len(), 5);
    assert_eq!(win.snapshots[3].cpu_user_ns, Some(500_003));
    let _ = std::fs::remove_dir_all(&dir);
}
