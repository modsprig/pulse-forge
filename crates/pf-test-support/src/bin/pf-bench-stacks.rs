//! Stack-pipeline microbenchmark (hot-path pass): measures the four paths
//! every stacks session touches —
//! 1. `write_jsonl`   — the persist write shape (`format!`-built lines,
//!    matching pf-daemon's stacks.jsonl writer, minus fsync);
//! 2. `load_stacks`   — the REAL `load_session_stacks_timed` (disk →
//!    pf_types::json parse → frame conversion → flip), i.e. the CLI/GUI
//!    open path;
//! 3. `aggregate`     — StackTrie top-down aggregation of the loaded set;
//! 4. `aggregate_bottom_up` — the PF-907 direction.
//!
//! Synthetic workload: `samples` stacks drawn from a Zipf-weighted set of
//! `shapes` distinct stack shapes over `fns` distinct function names —
//! heavy repetition on a few hot shapes, like real CPU profiles.
//!
//! Measures the median of `runs` repetitions (default 5); single-run
//! numbers are never reported (roadmap §16). Writes a bounded JSON report
//! to stdout. Sample counts are explicit and bounded (<= 5_000_000).

// Benchmark arithmetic: sizes bounded by CLI validation.
#![allow(clippy::arithmetic_side_effects)]

use std::io::Write;
use std::time::Instant;

use pf_analysis::stacks::{SampledStack, StackTrie};
use pf_analysis::stacks_io;
use pf_types::json::JsonValue as J;

const SCHEMA: &str = "pulseforge.bench-stacks/0";

/// One synthetic stack shape: module+symbol pairs, outermost first.
struct Shape {
    frames: Vec<(String, String)>, // (module, symbol)
}

fn synth_shapes(shape_count: usize, fn_count: usize) -> Vec<Shape> {
    let mut shapes = Vec::with_capacity(shape_count);
    for s in 0..shape_count {
        let depth = 8 + (s * 7) % 18; // 8..=25 frames
        let mut frames = Vec::with_capacity(depth);
        for d in 0..depth {
            let f = (s * 31 + d * 17) % fn_count;
            let m = (s + d) % 4;
            let module = match m {
                0 => "/usr/bin/pf-fixture-cpu",
                1 => "/usr/lib/x86_64-linux-gnu/libstd-abc.so",
                2 => "/usr/lib/x86_64-linux-gnu/libfoo.so.1",
                _ => "/usr/lib/x86_64-linux-gnu/libbar.so.2",
            };
            frames.push((module.to_string(), format!("fn_{f}")));
        }
        shapes.push(Shape { frames });
    }
    shapes
}

/// Zipf-ish (s=0.9) share per shape, normalized; shape 0 is the hottest.
fn shape_weights(shape_count: usize) -> Vec<f64> {
    let raw: Vec<f64> = (0..shape_count)
        .map(|i| 1.0 / (i as f64 + 1.0).powf(0.9))
        .collect();
    let total: f64 = raw.iter().sum();
    raw.iter().map(|w| w / total).collect()
}

fn synth_samples(samples: usize, shapes: &[Shape], weights: &[f64]) -> Vec<SampledStack> {
    let mut out = Vec::with_capacity(samples);
    // Deterministic LCG picks shapes by cumulative weight.
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let cum: Vec<f64> = weights
        .iter()
        .scan(0.0, |acc, w| {
            *acc += w;
            Some(*acc)
        })
        .collect();
    for i in 0..samples {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let pick = (state >> 11) as f64 / (1u64 << 53) as f64;
        let idx = cum
            .iter()
            .position(|c| *c > pick)
            .unwrap_or(shapes.len() - 1);
        let shape = &shapes[idx];
        let frames: Vec<pf_analysis::stacks::StackFrame> = shape
            .frames
            .iter()
            .map(|(m, sym)| pf_analysis::stacks::StackFrame {
                key: sym.clone(),
                module: m.clone(),
                resolved: true,
                file: Some("src/hot.rs".to_string()),
                line: Some(u32::try_from((i % 400) + 1).unwrap_or(1)),
                offset: (i % 4096) as u64,
            })
            .collect();
        out.push(SampledStack { frames, weight: 1 });
    }
    out
}

/// Serialize one sample as a stacks.jsonl line — the SAME shape pf-daemon's
/// persist writer emits (frames callee-first, ips list, inlined flags).
fn synth_line(sample: &SampledStack, time_ns: u64, tid: u32) -> String {
    let mut ips = String::new();
    for (i, _fr) in sample.frames.iter().enumerate() {
        if i > 0 {
            ips.push(',');
        }
        ips.push_str(&format!("{}", 0x400000 + i as u64 * 16));
    }
    let mut frames = String::new();
    for (i, fr) in sample.frames.iter().enumerate().rev() {
        if i != sample.frames.len() - 1 {
            frames.push(',');
        }
        let inlined = i != sample.frames.len() - 1;
        frames.push_str(&format!(
            "{{\"module\":\"{}\",\"symbol\":\"{}\",\"offset\":{},\"inlined\":{},\"file\":\"{}\",\"line\":{}}}",
            fr.module,
            fr.key,
            fr.offset,
            inlined,
            fr.file.as_deref().unwrap_or(""),
            fr.line.unwrap_or(0)
        ));
    }
    format!(
        "{{\"time_ns\":{},\"tid\":{},\"ips\":[{}],\"frames\":[{}]}}\n",
        time_ns, tid, ips, frames
    )
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

fn bench(samples: usize, shapes: usize, runs: usize) -> J {
    let shapes_v = synth_shapes(shapes, 64);
    let weights = shape_weights(shapes);
    let samples_v = synth_samples(samples, &shapes_v, &weights);
    let depth_avg =
        samples_v.iter().map(|s| s.frames.len()).sum::<usize>() as f64 / samples_v.len() as f64;

    // Pre-build the stacks.jsonl text (untimed) for the load bench.
    let dir = std::env::temp_dir().join(format!("pf-bench-stacks-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut text = String::with_capacity(samples * 1024);
    for (i, s) in samples_v.iter().enumerate() {
        text.push_str(&synth_line(s, (i as u64 + 1) * 100_000_000, 4242));
    }
    std::fs::write(dir.join("stacks.jsonl"), &text).expect("write synthetic stacks.jsonl");
    let bytes = text.len();
    drop(text);

    // 1. persist write shape — mirrors pf-daemon's CURRENT writer: one
    // reused line scratch written into directly, streamed through a
    // 1 MiB BufWriter (fsync excluded). Frames emit callee-first with
    // inlined flags, like the real stacks.jsonl.
    let write_path = dir.join("bench-write.jsonl");
    let mut write_ms = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t0 = Instant::now();
        let f = std::fs::File::create(&write_path).expect("create bench write file");
        let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
        let mut line = String::with_capacity(4_096);
        for (i, s) in samples_v.iter().enumerate() {
            line.clear();
            use std::fmt::Write as FmtWrite;
            let _ = write!(
                line,
                "{{\"time_ns\":{},\"tid\":{},\"ips\":[",
                (i as u64 + 1) * 100_000_000,
                4242u32
            );
            for (k, _fr) in s.frames.iter().enumerate() {
                if k > 0 {
                    line.push(',');
                }
                let _ = write!(line, "{}", 0x400000u64 + k as u64 * 16);
            }
            line.push_str("],\"frames\":[");
            let depth = s.frames.len().saturating_sub(1);
            for (k, fr) in s.frames.iter().enumerate().rev() {
                if k != s.frames.len() - 1 {
                    line.push(',');
                }
                let _ = write!(
                    line,
                    "{{\"module\":\"{}\",\"symbol\":\"{}\",\"offset\":{},\"inlined\":{},\"file\":\"src/hot.rs\",\"line\":{}}}",
                    fr.module,
                    fr.key,
                    fr.offset,
                    k < depth,
                    fr.line.unwrap_or(0)
                );
            }
            line.push_str("]}\n");
            let _ = w.write_all(line.as_bytes());
        }
        let _ = w.flush();
        write_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    let _ = std::fs::remove_file(&write_path);

    // 2. REAL load path (disk read + JSON parse + conversion).
    let loaded = stacks_io::load_session_stacks_timed(&dir).expect("load synthetic session");
    assert_eq!(loaded.len(), samples);
    let mut load_ms = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t0 = Instant::now();
        let l = stacks_io::load_session_stacks_timed(&dir).expect("load synthetic session");
        load_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(l.len(), samples);
    }

    // 2b. JSON-parse-only floor for the same file (what remains in the
    // loader after conversion work is squeezed out).
    let file_text = std::fs::read_to_string(dir.join("stacks.jsonl")).expect("read back");
    let mut parse_ms = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t0 = Instant::now();
        let mut lines = 0usize;
        for l in file_text.lines() {
            if l.trim().is_empty() {
                continue;
            }
            let v = pf_types::json::parse(l).expect("own writer output must parse");
            lines += 1;
            let _ = v;
        }
        parse_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(lines, samples);
    }

    // Borrowed view for the aggregation benches (clone outside the timers).
    let flat: Vec<SampledStack> = loaded.iter().map(|t| t.stack.clone()).collect();

    // 3. top-down aggregation of the loaded set.
    let mut agg_ms = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t0 = Instant::now();
        let mut trie = StackTrie::new();
        trie.aggregate(&flat);
        agg_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(trie.sample_count(), samples);
    }

    // 4. bottom-up aggregation (PF-907).
    let mut bu_ms = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t0 = Instant::now();
        let mut trie = StackTrie::new();
        trie.aggregate_bottom_up(&flat);
        bu_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(trie.sample_count(), samples);
    }

    let _ = std::fs::remove_dir_all(&dir);

    J::Object(vec![
        ("schema".into(), J::Str(SCHEMA.into())),
        ("samples".into(), J::UInt(samples as u64)),
        ("shapes".into(), J::UInt(shapes as u64)),
        (
            "depth_avg".into(),
            J::Float((depth_avg * 100.0).round() / 100.0),
        ),
        ("file_bytes".into(), J::UInt(bytes as u64)),
        ("runs".into(), J::UInt(runs as u64)),
        (
            "median_ms".into(),
            J::Object(vec![
                (
                    "write_jsonl".into(),
                    J::Float((median(write_ms) * 100.0).round() / 100.0),
                ),
                (
                    "load_stacks".into(),
                    J::Float((median(load_ms) * 100.0).round() / 100.0),
                ),
                (
                    "parse_json_only".into(),
                    J::Float((median(parse_ms) * 100.0).round() / 100.0),
                ),
                (
                    "aggregate".into(),
                    J::Float((median(agg_ms) * 100.0).round() / 100.0),
                ),
                (
                    "aggregate_bottom_up".into(),
                    J::Float((median(bu_ms) * 100.0).round() / 100.0),
                ),
            ]),
        ),
    ])
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (samples, shapes) = match args.len() {
        0 => (18_000, 256),
        1 => match args[0].parse() {
            Ok(v) if v > 0 && v <= 5_000_000 => (v, 256),
            _ => {
                eprintln!("usage: pf-bench-stacks [samples 1..=5000000] (default 18000)");
                std::process::exit(2);
            }
        },
        2 => match (args[0].parse(), args[1].parse()) {
            (Ok(s), Ok(sh)) if s > 0 && s <= 5_000_000 && sh > 0 && sh <= 100_000 => (s, sh),
            _ => {
                eprintln!("usage: pf-bench-stacks [samples 1..=5000000] [shapes 1..=100000]");
                std::process::exit(2);
            }
        },
        _ => {
            eprintln!("usage: pf-bench-stacks [samples 1..=5000000] [shapes 1..=100000]");
            std::process::exit(2);
        }
    };
    let report = bench(samples, shapes, 5);
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{}", report.to_json_string());
}
