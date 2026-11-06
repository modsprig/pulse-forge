//! Linux-only stack-sampling recorder (wraps pf_linux::perf).
#![cfg(target_os = "linux")]

//! Stack-sampling recorder (roadmap P10: PF-1001/1002 pipeline integration)
//! — wraps `pf_linux::perf::ProcessSampler` (one event per thread, with
//! bounded thread-lifecycle rescan) in a bounded collector thread.
//!
//! Contract:
//! - samples drain into a **bounded** shared vec (max_samples cap; when the
//!   cap is hit the recorder sets `truncated` and drops honestly — never
//!   unbounded memory);
//! - the sampling thread runs until the stop flag or the session cancel
//!   fires, then final-drains and disables the event;
//! - kernel frames are filtered upstream (exclude_kernel + parser skip
//!   accounting) — stored samples are user-space only;
//! - LOST records are surfaced (never normalized away).

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pf_linux::perf::{EventSource, ProcessSampler};

/// One sampled stack (user-space frames only).
#[derive(Clone, Debug)]
pub struct StackSample {
    /// perf sample timestamp (kernel clock, ns).
    pub time_ns: u64,
    pub tid: u32,
    /// User-space callchain, innermost first. Kernel frames were filtered
    /// with accounting (`kernel_frames_skipped`).
    pub ips: Vec<u64>,
}

/// Sendable wrapper: the sampler owns raw mapped pointers, but ownership
/// moves wholly into the drain thread (no concurrent access — mappings
/// and fds are used only by the owning thread).
struct SendSampler(ProcessSampler);

// SAFETY: the mappings and fds are owned wholly by the drain thread once
// moved in — no concurrent access.
#[allow(unsafe_code)]
unsafe impl Send for SendSampler {}

/// Handle to a running stack sampler. Dropping without `join` stops the
/// thread lazily (the stop flag is shared).
pub struct StackRecorder {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    samples: Arc<Mutex<Vec<StackSample>>>,
    lost: Arc<AtomicU64>,
    kernel_skipped: Arc<AtomicU64>,
    truncated: Arc<AtomicBool>,
    pub source: EventSource,
}

impl StackRecorder {
    /// Open a sampler for `pid` and spawn the drain thread. `period_ns` is
    /// the sample period for the clock event (1_000_000 ⇒ ~1000/s).
    /// `cancel` is the session cancel flag: the sampler also stops when the
    /// counter recording ends.
    pub fn open(
        pid: i32,
        period_ns: u64,
        max_samples: usize,
        cancel: Arc<AtomicBool>,
    ) -> Result<StackRecorder, pf_types::PfError> {
        // PF-1003: per-thread sampling — perf_event_open(pid=tgid) covers
        // only the group-leader thread, so every thread gets its own event
        // (bounded); the drain thread re-scans for thread lifecycle.
        let sampler = pf_linux::perf::ProcessSampler::open_for_process(pid, period_ns)?;
        let source = sampler.source;
        let samples: Arc<Mutex<Vec<StackSample>>> = Arc::new(Mutex::new(Vec::new()));
        let lost = Arc::new(AtomicU64::new(0));
        let kernel_skipped = Arc::new(AtomicU64::new(0));
        let truncated = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let sampler = SendSampler(sampler);
        let max = max_samples;
        let thread = std::thread::Builder::new()
            .name("pf-stack-drain".into())
            .spawn({
                let stop = Arc::clone(&stop);
                let samples = Arc::clone(&samples);
                let lost = Arc::clone(&lost);
                let kernel_skipped = Arc::clone(&kernel_skipped);
                let truncated = Arc::clone(&truncated);
                move || {
                    let mut sampler = sampler;
                    let sampler = &mut sampler.0;
                    sampler.start();
                    loop {
                        if stop.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                        // Every tick (100 ms): pick up threads born after
                        // open and drop exited ones. The blind window for a
                        // born-and-died thread is one interval; 100 ms keeps
                        // sub-second threads coverable (a bounded read_dir
                        // per tick; misses are counted honestly).
                        sampler.rescan();
                        for s in sampler.collect() {
                            let mut v = match samples.lock() {
                                Ok(v) => v,
                                Err(_) => return,
                            };
                            if v.len() >= max {
                                truncated.store(true, Ordering::Relaxed);
                                continue; // bounded: drop honestly, keep counting
                            }
                            v.push(StackSample {
                                time_ns: s.time_ns,
                                tid: s.tid,
                                ips: s.ips[..s.ip_count].to_vec(),
                            });
                        }
                        // ProcessSampler totals fold deltas once — store,
                        // never re-add.
                        lost.store(sampler.lost_records, Ordering::Relaxed);
                        kernel_skipped.store(sampler.kernel_frames_skipped, Ordering::Relaxed);
                    }
                    // Final drain after the loop (captures the tail window).
                    for s in sampler.collect() {
                        if let Ok(mut v) = samples.lock() {
                            if v.len() < max {
                                v.push(StackSample {
                                    time_ns: s.time_ns,
                                    tid: s.tid,
                                    ips: s.ips[..s.ip_count].to_vec(),
                                });
                            } else {
                                truncated.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                    sampler.stop();
                    let _ = sampler.collect(); // fold the tail counters
                    lost.store(sampler.lost_records, Ordering::Relaxed);
                    kernel_skipped.store(sampler.kernel_frames_skipped, Ordering::Relaxed);
                }
            })
            .map_err(|e| {
                pf_types::PfError::new(
                    pf_types::ErrorCode::BackendFailure,
                    format!("stack drain thread spawn failed: {e}"),
                )
            })?;

        Ok(StackRecorder {
            stop,
            thread: Some(thread),
            samples,
            lost,
            kernel_skipped,
            truncated,
            source,
        })
    }

    /// Request stop (the drain thread final-drains and exits).
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn samples(&self) -> Arc<Mutex<Vec<StackSample>>> {
        Arc::clone(&self.samples)
    }

    pub fn lost(&self) -> u64 {
        self.lost.load(Ordering::Relaxed)
    }

    pub fn kernel_skipped(&self) -> u64 {
        self.kernel_skipped.load(Ordering::Relaxed)
    }

    pub fn truncated(&self) -> bool {
        self.truncated.load(Ordering::Relaxed)
    }

    /// Join the drain thread; returns the collected samples.
    pub fn join(&mut self) -> Vec<StackSample> {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.samples.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

/// Minimal JSON string escaping for the persistence writer (paths can
/// contain quotes/backslashes; control bytes are escaped honestly).
struct JStr<'a>(&'a str);

impl JStr<'_> {
    /// Escape directly into a caller-owned buffer (no intermediate String
    /// per frame on the hot persist path).
    fn push_escaped(out: &mut String, s: &str) {
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    let _ = std::fmt::write(out, format_args!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }

    fn json(&self) -> String {
        let mut out = String::with_capacity(self.0.len().saturating_add(2));
        Self::push_escaped(&mut out, self.0);
        out
    }
}

/// Write `bytes` to `path` atomically: tmp file → fsync → rename. A
/// daemon killed mid-write leaves a `.partial` tmp file, never a torn
/// final artifact — the loader rejects unparseable lines, so a torn
/// `stacks.jsonl` would otherwise take the whole session's stacks
/// analysis down. (Same pattern as the recovery manifest rewrite.)
// Tmp-path arithmetic is a with_extension suffix; rename replaces the
// destination (MoveFileEx REPLACE_EXISTING on Windows).
#[allow(clippy::arithmetic_side_effects)]
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), pf_types::PfError> {
    let tmp = path.with_extension("partial");
    {
        let mut f = fs::File::create(&tmp).map_err(pf_types::PfError::from)?;
        f.write_all(bytes).map_err(pf_types::PfError::from)?;
        f.sync_all().map_err(pf_types::PfError::from)?;
    }
    fs::rename(&tmp, path).map_err(pf_types::PfError::from)?;
    Ok(())
}

/// Persist sampled stacks: `stacks.jsonl` (raw) + `stacks-summary.json`
/// (leaf-attribution summary resolved against the maps snapshot captured
/// at recording start). Bounded: at most max_samples lines.
///
/// Returns the persisted sample count and the summary object (also
/// embedded into `report.json`). When the sampler produced no evidence at
/// all (no samples, no lost records — e.g. the sampler never opened), no
/// files are written and the summary is `None`: unavailable is never
/// rendered as zero.
pub fn persist_stacks(
    dir: &Path,
    samples: &[StackSample],
    lost: u64,
    kernel_skipped: u64,
    truncated: bool,
    maps: &[pf_linux::symbols::MapsEntry],
) -> Result<(usize, Option<pf_types::json::JsonValue>), pf_types::PfError> {
    use pf_types::json::JsonValue;

    if samples.is_empty() && lost == 0 && !truncated {
        return Ok((0, None));
    }

    // Resolve EVERY frame of every sample against the start-of-session maps
    // snapshot so the session dir is self-contained (the target's address
    // space is gone by the time anyone analyzes the recording). One batched
    // resolve over the deduped, bounded IP set; per-IP results map back.
    let all_ips: Vec<u64> = {
        let mut seen = std::collections::HashSet::new();
        let mut v = Vec::new();
        for s in samples {
            for ip in &s.ips {
                if seen.insert(*ip) {
                    v.push(*ip);
                }
            }
        }
        v
    };
    let mut symcache = std::collections::HashMap::new();
    // PF-1005: chains carry DWARF inline expansion (innermost-first);
    // absent debug info yields single-entry chains.
    let chains_all = pf_linux::symbols::resolve_ip_chains(&all_ips, maps, &mut symcache);
    let mut ip_to_chain: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    for (i, (ip, chain)) in all_ips.iter().zip(chains_all.iter()).enumerate() {
        if chain.is_some() {
            ip_to_chain.insert(*ip, i);
        }
    }
    let mut frames_total = 0u64;
    let mut frames_resolved = 0u64;
    let mut inlined_frames = 0u64;
    let mut line_resolved = 0u64;

    // stacks.jsonl: one JSON object per line — raw `ips` for fidelity plus
    // resolved `frames` ({module, symbol|null, offset, inlined}) for
    // aggregation. Frames are FLATTENED chains: each IP contributes its
    // inlinee entries (innermost-first) then its enclosing frame.
    // Streamed through a buffered writer from one reused line scratch —
    // a session-sized string never accumulates in memory.
    use std::fmt::Write as FmtWrite;
    let stacks_path = dir.join("stacks.jsonl");
    let stacks_tmp = stacks_path.with_extension("partial");
    let stacks_file = fs::File::create(&stacks_tmp).map_err(pf_types::PfError::from)?;
    let mut w = std::io::BufWriter::with_capacity(1 << 20, stacks_file);
    let mut line = String::with_capacity(4_096);
    let mut tids: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for s in samples {
        tids.insert(s.tid);
        line.clear();
        let _ = write!(
            line,
            "{{\"time_ns\":{},\"tid\":{},\"ips\":[",
            s.time_ns, s.tid
        );
        for (i, ip) in s.ips.iter().enumerate() {
            if i > 0 {
                line.push(',');
            }
            let _ = write!(line, "{ip}");
        }
        line.push_str("],\"frames\":[");
        let mut sep = "";
        for ip in &s.ips {
            let chain: &[pf_linux::symbols::ResolvedFrame] = match ip_to_chain.get(ip) {
                Some(&i) => chains_all[i].as_deref().unwrap_or(&[]),
                None => &[],
            };
            let depth = chain.len().saturating_sub(1);
            if chain.is_empty() {
                frames_total = frames_total.saturating_add(1);
                line.push_str(sep);
                sep = ",";
                line.push_str("{\"module\":null,\"symbol\":null,\"offset\":0,\"inlined\":false}");
            }
            for (i, fr) in chain.iter().enumerate() {
                frames_total = frames_total.saturating_add(1);
                if i < depth {
                    inlined_frames = inlined_frames.saturating_add(1);
                }
                if !fr.symbol.is_empty() {
                    frames_resolved = frames_resolved.saturating_add(1);
                }
                if fr.file.is_some() && fr.line.unwrap_or(0) > 0 {
                    line_resolved = line_resolved.saturating_add(1);
                }
                line.push_str(sep);
                sep = ",";
                line.push_str("{\"module\":");
                JStr::push_escaped(&mut line, &fr.module);
                line.push_str(",\"symbol\":");
                if fr.symbol.is_empty() {
                    line.push_str("null");
                } else {
                    JStr::push_escaped(&mut line, &fr.symbol);
                }
                let _ = write!(
                    line,
                    ",\"offset\":{},\"inlined\":{},\"file\":",
                    fr.offset,
                    i < depth
                );
                match (&fr.file, fr.line) {
                    (Some(f), Some(l)) if l > 0 => {
                        JStr::push_escaped(&mut line, f);
                        let _ = write!(line, ",\"line\":{l}");
                    }
                    _ => line.push_str("null,\"line\":null"),
                }
                line.push('}');
            }
        }
        line.push_str("]}\n");
        w.write_all(line.as_bytes())
            .map_err(pf_types::PfError::from)?;
    }
    w.flush().map_err(pf_types::PfError::from)?;
    let stacks_file = w.into_inner().map_err(|e| {
        pf_types::PfError::new(
            pf_types::ErrorCode::StorageIo,
            format!("stacks.jsonl close: {e}"),
        )
    })?;
    stacks_file.sync_all().map_err(pf_types::PfError::from)?;
    // The final artifact appears atomically: a crash before this rename
    // leaves only the .partial tmp as evidence, never a torn file.
    fs::rename(&stacks_tmp, &stacks_path).map_err(pf_types::PfError::from)?;

    // Leaf attribution against the maps snapshot. PF-1005: the leaf is
    // the INNERMOST chain entry — an inlined leaf names the inlinee, not
    // its enclosing function. Leaf IPs are a subset of the batch resolved
    // above, so the chains are REUSED (no second resolution pass).
    let mut symbol_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut resolved_count = 0usize;
    let mut leaf_count = 0usize;
    for s in samples {
        let Some(&leaf_ip) = s.ips.first() else {
            continue;
        };
        if leaf_ip >= 0xffff_8000_0000_0000 {
            continue; // kernel leaf: honestly unresolved
        }
        leaf_count = leaf_count.saturating_add(1);
        let chain: &[pf_linux::symbols::ResolvedFrame] = match ip_to_chain.get(&leaf_ip) {
            Some(&i) => chains_all[i].as_deref().unwrap_or(&[]),
            None => &[],
        };
        if let Some(innermost) = chain.first() {
            if !innermost.symbol.is_empty() {
                let c = symbol_counts.entry(innermost.symbol.clone()).or_insert(0);
                *c = c.saturating_add(1);
                resolved_count = resolved_count.saturating_add(1);
            }
        }
    }
    // Everything else (None or empty symbol) is honestly unresolved.
    let unresolved = leaf_count.saturating_sub(resolved_count);
    let mut top: Vec<(&String, &usize)> = symbol_counts.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    let top_json: Vec<JsonValue> = top
        .iter()
        .take(32)
        .map(|(sym, c)| {
            JsonValue::Object(vec![
                ("symbol".to_string(), JsonValue::Str((*sym).clone())),
                ("count".to_string(), JsonValue::UInt(**c as u64)),
            ])
        })
        .collect();

    let summary = JsonValue::Object(vec![
        (
            "schema".to_string(),
            JsonValue::Str("pulseforge.stacks/0".into()),
        ),
        ("samples".to_string(), JsonValue::UInt(samples.len() as u64)),
        ("lost".to_string(), JsonValue::UInt(lost)),
        ("truncated".to_string(), JsonValue::Bool(truncated)),
        (
            "kernel_frames_filtered".to_string(),
            JsonValue::UInt(kernel_skipped),
        ),
        ("frames_total".to_string(), JsonValue::UInt(frames_total)),
        (
            "frames_resolved".to_string(),
            JsonValue::UInt(frames_resolved),
        ),
        (
            "inlined_frames".to_string(),
            JsonValue::UInt(inlined_frames),
        ),
        ("line_resolved".to_string(), JsonValue::UInt(line_resolved)),
        (
            "threads_with_samples".to_string(),
            JsonValue::UInt(tids.len() as u64),
        ),
        (
            "leaf_symbols_resolved".to_string(),
            JsonValue::UInt(resolved_count as u64),
        ),
        (
            "leaf_unresolved".to_string(),
            JsonValue::UInt(unresolved as u64),
        ),
        ("top_leaf_symbols".to_string(), JsonValue::Array(top_json)),
    ]);
    // The module map snapshot itself: lets future tools re-resolve raw IPs
    // (e.g. after improved symbolization) without the target process.
    let mut map_pairs: Vec<String> = Vec::with_capacity(maps.len());
    for m in maps {
        map_pairs.push(format!(
            "{{\"start\":{},\"end\":{},\"pathname\":{}}}",
            m.start,
            m.end,
            JStr(&m.pathname).json()
        ));
    }
    let maps_path = dir.join("maps.json");
    write_atomic(
        &maps_path,
        format!(
            "[{}]
",
            map_pairs.join(",")
        )
        .as_bytes(),
    )?;

    let summary_path = dir.join("stacks-summary.json");
    write_atomic(&summary_path, summary.to_json_string().as_bytes())?;
    Ok((samples.len(), Some(summary)))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn persisted_lines_are_valid_json_and_self_describing() {
        let dir = std::env::temp_dir().join(format!("pf-stacks-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let samples = vec![
            StackSample {
                time_ns: 1_000,
                tid: 42,
                ips: vec![0x1000, 0x2000, 0x3000],
            },
            StackSample {
                time_ns: 2_000,
                tid: 42,
                ips: vec![0x1000],
            },
        ];
        let maps = vec![pf_linux::symbols::MapsEntry {
            start: 0x400000,
            end: 0x500000,
            pathname: "/tmp/pf/test-bin".to_string(),
        }];
        // No real ELF at that path: resolution returns None/empty honestly —
        // the persisted JSON must still be well-formed.
        let (count, summary) = persist_stacks(&dir, &samples, 7, 3, false, &maps).expect("persist");
        assert_eq!(count, 2);
        let text = std::fs::read_to_string(dir.join("stacks.jsonl")).unwrap();
        let mut lines = 0;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            lines += 1;
            let v = pf_types::json::parse(line)
                .unwrap_or_else(|e| panic!("line {lines} unparseable: {e}\n{line}"));
            let ips = v.get("ips").and_then(|x| x.as_array()).unwrap();
            let frames = v.get("frames").and_then(|x| x.as_array()).unwrap();
            assert_eq!(ips.len(), frames.len());
        }
        assert_eq!(lines, 2);
        let maps_text = std::fs::read_to_string(dir.join("maps.json")).unwrap();
        let mv = pf_types::json::parse(maps_text.trim()).unwrap();
        assert_eq!(mv.as_array().map(|a| a.len()), Some(1));
        let sv = summary.expect("summary for non-empty samples");
        assert_eq!(
            sv.get("frames_total").and_then(|x| x.as_u64()),
            Some(4),
            "3 + 1 ips counted"
        );
        assert_eq!(
            sv.get("frames_resolved").and_then(|x| x.as_u64()),
            Some(0),
            "no ELF on disk: nothing claims resolution"
        );
        // Atomic-write contract: success leaves NO .partial residue and
        // all three final artifacts present.
        let residue: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "partial"))
            .collect();
        assert!(
            residue.is_empty(),
            "no .partial tmp may survive a successful persist: {residue:?}"
        );
        for final_name in ["stacks.jsonl", "maps.json", "stacks-summary.json"] {
            assert!(
                dir.join(final_name).is_file(),
                "{final_name} present after atomic persist"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
