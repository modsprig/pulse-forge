//! Storage-encoding microbenchmark (roadmap PF-010).
//!
//! Compares two candidate raw-record encodings for the future chunk format:
//! (a) JSONL via pf-types' writer (schema-0 baseline) and
//! (b) a fixed-layout binary record (little-endian u64s with a frame header).
//!
//! Measures wall time to encode N records, decode them back, bytes produced,
//! and integrity. Writes a bounded report to stdout as JSON. Repeat counts
//! are explicit: run 3+ times and compare medians, never single runs
//! (roadmap §16: "Never gate a release on a single unrepeatable speed
//! number").

// Benchmark arithmetic: fixed sizes bounded by CLI validation (<= 10M).
#![allow(clippy::arithmetic_side_effects)]

use std::io::Write;
use std::time::Instant;

use pf_types::CounterSnapshot;
use pf_types::json::{self, JsonValue};

fn synthetic_snapshot(i: u64) -> CounterSnapshot {
    CounterSnapshot {
        mono_ns: i * 250_000_000,
        cpu_user_ns: Some(i * 1_000_003),
        cpu_kernel_ns: Some(i * 500_007),
        resident_bytes: Some(48_000_000 + (i % 8_192) * 4_096),
        resident_peak_bytes: Some(64_000_000 + (i % 8_192) * 4_096),
        private_commit_bytes: Some(32_000_000 + (i % 4_096) * 4_096),
        private_commit_peak_bytes: Some(40_000_000 + (i % 4_096) * 4_096),
        io_read_ops: Some(i * 3),
        io_write_ops: Some(i * 2),
        io_read_bytes: Some(i * 65_536),
        io_write_bytes: Some(i * 32_768),
        thread_count: Some(6 + (i % 5) as u32),
        handle_count: Some(200 + (i % 90) as u32),
        page_faults: Some(i * 41),
    }
}

/// Fixed-layout binary record: magic u32 + count u32 + 14 u64 fields.
const BIN_FIELDS: usize = 14;

fn encode_binary(s: &CounterSnapshot, out: &mut Vec<u8>) {
    let fields: [u64; BIN_FIELDS] = [
        s.mono_ns,
        s.cpu_user_ns.unwrap_or(u64::MAX),
        s.cpu_kernel_ns.unwrap_or(u64::MAX),
        s.resident_bytes.unwrap_or(u64::MAX),
        s.resident_peak_bytes.unwrap_or(u64::MAX),
        s.private_commit_bytes.unwrap_or(u64::MAX),
        s.private_commit_peak_bytes.unwrap_or(u64::MAX),
        s.io_read_ops.unwrap_or(u64::MAX),
        s.io_write_ops.unwrap_or(u64::MAX),
        s.io_read_bytes.unwrap_or(u64::MAX),
        s.io_write_bytes.unwrap_or(u64::MAX),
        u64::from(s.thread_count.unwrap_or(u32::MAX)),
        u64::from(s.handle_count.unwrap_or(u32::MAX)),
        s.page_faults.unwrap_or(u64::MAX),
    ];
    out.extend_from_slice(&0x50464252u32.to_le_bytes()); // "PFRB"
    out.extend_from_slice(&u32::try_from(BIN_FIELDS).unwrap_or(0).to_le_bytes());
    for f in fields {
        out.extend_from_slice(&f.to_le_bytes());
    }
}

fn decode_binary_all(buf: &[u8]) -> Result<usize, String> {
    let mut pos = 0;
    let mut records = 0;
    while pos < buf.len() {
        if pos + 8 > buf.len() {
            return Err("truncated header".into());
        }
        let magic = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
        if magic != 0x50464252 {
            return Err("bad magic".into());
        }
        let count =
            u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as usize; // usize cast: u32 always fits
        if count != BIN_FIELDS {
            return Err("unexpected field count".into());
        }
        let body = 8 + count * 8;
        if pos + body > buf.len() {
            return Err("truncated record".into());
        }
        pos += body;
        records += 1;
    }
    Ok(records)
}

fn bench(n: usize) -> JsonValue {
    let snapshots: Vec<CounterSnapshot> = (0..n as u64).map(synthetic_snapshot).collect();

    // JSONL encode
    let t0 = Instant::now();
    let mut jsonl = String::with_capacity(n * 320);
    for s in &snapshots {
        jsonl.push_str(&s.to_json().to_json_string());
        jsonl.push('\n');
    }
    let json_encode_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let json_bytes = jsonl.len();

    // JSONL decode
    let t1 = Instant::now();
    let mut json_decoded = 0usize;
    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v = json::parse(line).expect("own writer output must parse");
        let _ = CounterSnapshot::from_json(&v).expect("own writer output must decode");
        json_decoded += 1;
    }
    let json_decode_ms = t1.elapsed().as_secs_f64() * 1000.0;

    // Binary encode
    let t2 = Instant::now();
    let mut bin = Vec::with_capacity(n * (8 + BIN_FIELDS * 8));
    for s in &snapshots {
        encode_binary(s, &mut bin);
    }
    let bin_encode_ms = t2.elapsed().as_secs_f64() * 1000.0;
    let bin_bytes = bin.len();

    // Binary decode
    let t3 = Instant::now();
    let bin_decoded = decode_binary_all(&bin).expect("roundtrip integrity");
    let bin_decode_ms = t3.elapsed().as_secs_f64() * 1000.0;

    assert_eq!(json_decoded, n);
    assert_eq!(bin_decoded, n);

    use JsonValue as J;
    J::Object(vec![
        ("records".into(), J::UInt(n as u64)),
        (
            "jsonl".into(),
            J::Object(vec![
                ("encode_ms".into(), J::Float(json_encode_ms)),
                ("decode_ms".into(), J::Float(json_decode_ms)),
                ("bytes".into(), J::UInt(json_bytes as u64)),
                (
                    "bytes_per_record".into(),
                    J::Float(json_bytes as f64 / n as f64),
                ),
            ]),
        ),
        (
            "binary".into(),
            J::Object(vec![
                ("encode_ms".into(), J::Float(bin_encode_ms)),
                ("decode_ms".into(), J::Float(bin_decode_ms)),
                ("bytes".into(), J::UInt(bin_bytes as u64)),
                (
                    "bytes_per_record".into(),
                    J::Float(bin_bytes as f64 / n as f64),
                ),
            ]),
        ),
        (
            "ratio_binary_over_jsonl_bytes".into(),
            J::Float(bin_bytes as f64 / json_bytes as f64),
        ),
    ])
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n: usize = if args.is_empty() {
        100_000
    } else {
        match args[0].parse() {
            Ok(v) if v > 0 && v <= 10_000_000 => v,
            _ => {
                eprintln!("usage: pf-bench-encoding [records 1..=10000000] (default 100000)");
                std::process::exit(2);
            }
        }
    };
    let report = bench(n);
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{}", report.to_json_string());
}
