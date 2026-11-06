//! End-to-end stack-sampling pipeline test (PF-1001/1002) over the REAL
//! daemon on Linux: the perf sampler runs alongside the counter recorder,
//! and the finalize path persists `stacks.jsonl` + `stacks-summary.json`
//! into the session dir, with the summary also embedded in `report.json`.
//!
//! Known-hotspot verification: the recorded target is our own `bench-spin`
//! (whose hot loop is `pf_test_support::spin_ms`), so the top leaf symbol
//! must resolve into that function — not libc, not `lang_start`.

#![cfg(target_os = "linux")]
// Test index casts are bounded by validated frame counts.
#![allow(clippy::cast_possible_truncation)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn exe() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_pulseforge"))
}

fn fresh_env_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pf-stacks-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_daemon(data_root: &std::path::Path) -> Result<DaemonGuard, String> {
    let err_path = data_root.join("daemon.stderr");
    let child = Command::new(exe())
        .args(["daemon"])
        .env("PULSEFORGE_DATA", data_root)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&err_path).unwrap())
        .spawn()
        .map_err(|e| format!("spawn daemon: {e}"))?;
    Ok(DaemonGuard(child))
}

fn agent_json(data_root: &std::path::Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(exe())
        .args(args)
        .env("PULSEFORGE_DATA", data_root)
        .output()
        .expect("run agent");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        text.push_str(
            "
--stderr--
",
        );
        text.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    (out.status.success(), text)
}

fn parse_last_json(text: &str) -> pf_types::json::JsonValue {
    for line in text.trim().lines().rev() {
        if let Ok(v) = pf_types::json::parse(line) {
            if v.as_object_pairs().is_some() {
                return v;
            }
        }
    }
    panic!("no JSON in agent output: {text}");
}

fn daemon_stderr(data_root: &std::path::Path) -> String {
    std::fs::read_to_string(data_root.join("daemon.stderr")).unwrap_or_default()
}

#[test]
fn daemon_session_persists_sampled_stacks_and_report_embeds_summary() {
    let data = fresh_env_dir("stacks");

    // Known-hotspot workload: bench-spin's hot loop is spin_ms.
    let mut workload = Command::new(exe())
        .args(["bench-spin", "--ms", "15000"])
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn bench-spin workload");
    let pid = workload.id();

    let daemon = spawn_daemon(&data);
    let daemon = match daemon {
        Ok(d) => d,
        Err(e) => {
            let _ = workload.kill();
            panic!("{e}");
        }
    };

    // Start a 3 s recording against the spinning workload.
    let (ok, out) = agent_json(
        &data,
        &[
            "agent",
            "start",
            "--pid",
            &pid.to_string(),
            "--duration-ms",
            "3000",
            "--interval-ms",
            "150",
        ],
    );
    if !ok {
        let _ = workload.kill();
        let _ = workload.wait();
        panic!(
            "agent start failed: {out}\n--- daemon stderr ---\n{}",
            daemon_stderr(&data)
        );
    }
    let started = parse_last_json(&out);
    let sid = started
        .get("session_id")
        .and_then(|v| v.as_str())
        .expect("session id")
        .to_string();

    // Poll until the session reaches a terminal state.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut final_state = String::new();
    loop {
        if Instant::now() > deadline {
            let _ = workload.kill();
            let _ = workload.wait();
            panic!(
                "session {sid} never reached a terminal state; last={final_state}\n--- daemon stderr ---\n{}",
                daemon_stderr(&data)
            );
        }
        std::thread::sleep(Duration::from_millis(300));
        let (ok, out) = agent_json(&data, &["agent", "status", "--session", &sid]);
        if !ok {
            continue; // transient; deadline bounds this
        }
        let status = parse_last_json(&out);
        let result = status
            .get("result")
            .cloned()
            .unwrap_or(pf_types::json::JsonValue::Null);
        let state = result
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match state.as_str() {
            "COMPLETED" | "FAILED" | "CANCELLED" | "RECOVERED_PARTIAL" => {
                final_state = state;
                break;
            }
            _ => final_state = state,
        }
    }
    assert_eq!(final_state, "COMPLETED", "recording must complete cleanly");
    let _ = workload.kill();
    let _ = workload.wait();

    // Session dir with the stack artifacts.
    let session_dir = data.join("sessions").join(format!("{sid}.pfsession"));
    assert!(
        session_dir.join("manifest.json").exists(),
        "session dir missing: {}",
        session_dir.display()
    );

    // 1. stacks.jsonl: one JSON object per line, bounded, populated.
    let stacks_text = std::fs::read_to_string(session_dir.join("stacks.jsonl"))
        .expect("stacks.jsonl must exist after a completed recording");
    let lines: Vec<&str> = stacks_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert!(
        lines.len() >= 50,
        "3 s at ~1 kHz must leave at least 50 persisted samples, got {}",
        lines.len()
    );
    for line in lines.iter().take(5) {
        let v = pf_types::json::parse(line).expect("stacks.jsonl line parses");
        assert!(v.get("time_ns").and_then(|x| x.as_u64()).is_some());
        assert!(v.get("tid").and_then(|x| x.as_u64()).is_some());
        let ips = v.get("ips").and_then(|x| x.as_array()).expect("ips array");
        assert!(!ips.is_empty(), "each sample carries a callchain");
        let frames = v
            .get("frames")
            .and_then(|x| x.as_array())
            .expect("resolved frames persisted");
        assert!(
            frames.len() >= ips.len(),
            "flattened chains: >= one frame per raw ip ({} vs {})",
            frames.len(),
            ips.len()
        );
        let mut with_lines = 0usize;
        for fr in frames {
            // Every chain entry carries the inline provenance flag.
            assert!(
                fr.get("inlined").and_then(|x| x.as_bool()).is_some(),
                "inlined flag present: {fr:?}"
            );
            // Dev-profile binaries carry .debug_line: frames attribute to
            // real source files (PF-1005 line remainder).
            if fr
                .get("file")
                .and_then(|x| x.as_str())
                .is_some_and(|f| !f.is_empty())
                && fr.get("line").and_then(|x| x.as_u64()).unwrap_or(0) > 0
            {
                with_lines += 1;
            }
        }
        assert!(
            with_lines > 0,
            "at least one persisted frame must carry file:line"
        );
    }
    // maps.json snapshot: durable module map for re-resolution.
    let maps_text = std::fs::read_to_string(session_dir.join("maps.json")).expect("maps.json");
    let maps = pf_types::json::parse(maps_text.trim()).expect("maps.json parses");
    let maps_arr = maps.as_array().expect("maps array");
    assert!(
        maps_arr.iter().any(|m| m
            .get("pathname")
            .and_then(|p| p.as_str())
            .is_some_and(|p| p.ends_with("pulseforge"))),
        "the workload binary must appear in the persisted module map"
    );

    // 2. stacks-summary.json: honest counters + the known-hotspot leaf.
    let summary_text = std::fs::read_to_string(session_dir.join("stacks-summary.json"))
        .expect("stacks-summary.json must exist");
    let summary = pf_types::json::parse(summary_text.trim()).expect("summary parses");
    assert_eq!(
        summary.get("schema").and_then(|v| v.as_str()),
        Some("pulseforge.stacks/0")
    );
    let samples = summary
        .get("samples")
        .and_then(|v| v.as_u64())
        .expect("samples field");
    assert_eq!(samples, lines.len() as u64, "summary matches the raw lines");
    assert_eq!(
        summary.get("truncated").and_then(|v| v.as_bool()),
        Some(false),
        "3 s at the 20k cap must not truncate"
    );
    assert!(
        summary
            .get("kernel_frames_filtered")
            .and_then(|v| v.as_u64())
            .is_some(),
        "kernel-frame accounting present"
    );
    let frames_total = summary
        .get("frames_total")
        .and_then(|v| v.as_u64())
        .expect("frames_total present");
    let frames_resolved = summary
        .get("frames_resolved")
        .and_then(|v| v.as_u64())
        .expect("frames_resolved present");
    // Honest per-thread coverage (PF-1003): the single-threaded bench-spin
    // target yields exactly one tid with samples.
    let threads_with_samples = summary
        .get("threads_with_samples")
        .and_then(|v| v.as_u64())
        .expect("threads_with_samples present");
    assert_eq!(
        threads_with_samples, 1,
        "bench-spin is single-threaded: one tid with samples"
    );
    assert!(frames_total > 0);
    assert!(
        frames_resolved * 2 > frames_total,
        "most frames resolve in a debug build ({frames_resolved}/{frames_total})"
    );

    let top = summary
        .get("top_leaf_symbols")
        .and_then(|v| v.as_array())
        .expect("top_leaf_symbols array");
    assert!(
        !top.is_empty(),
        "leaf symbols must resolve against the maps snapshot"
    );
    let top_sym = top[0]
        .get("symbol")
        .and_then(|v| v.as_str())
        .expect("symbol string")
        .to_string();
    eprintln!("E2E-TOP-LEAF {top_sym} count={:?}", top[0].get("count"));
    assert!(
        top_sym.contains("spin_ms") || top_sym.contains("pf_test_support"),
        "top leaf symbol must be the known hotspot, got {top_sym}"
    );
    assert!(
        !top_sym.contains("__libc_start") && !top_sym.contains("lang_start"),
        "self-time attribution must not crown root frames, got {top_sym}"
    );

    // 3. report.json (daemon-written) embeds the stacks summary.
    let report_text =
        std::fs::read_to_string(session_dir.join("report.json")).expect("report.json");
    let report = pf_types::json::parse(report_text.trim()).expect("report parses");
    let embedded = report.get("stacks").expect("report.json must embed stacks");
    assert_eq!(
        embedded.get("samples").and_then(|v| v.as_u64()),
        Some(samples),
        "embedded summary matches the standalone one"
    );

    // 4. `pulseforge report` renders the same summary from disk.
    let (ok, out) = agent_json(
        &data,
        &["report", "--session", session_dir.to_str().unwrap()],
    );
    assert!(ok, "report command failed: {out}");
    let rendered = parse_last_json(&out);
    assert!(
        rendered.get("stacks").is_some(),
        "`pulseforge report` must include the stacks section"
    );
    assert_eq!(
        rendered
            .get("stacks")
            .and_then(|s| s.get("samples"))
            .and_then(|v| v.as_u64()),
        Some(samples),
        "rendered summary matches the daemon-written one"
    );

    // 5. `pulseforge stacks` aggregates the persisted callchains (PF-904):
    //    top EXCLUSIVE function is the known hotspot, tree is bounded.
    let (ok, out) = agent_json(
        &data,
        &["stacks", "--session", session_dir.to_str().unwrap()],
    );
    assert!(ok, "stacks command failed: {out}");
    let agg = parse_last_json(&out);
    assert_eq!(
        agg.get("schema").and_then(|v| v.as_str()),
        Some("pulseforge.stacks-analysis/0")
    );
    let a = agg.get("aggregation").expect("aggregation object");
    assert_eq!(
        a.get("samples").and_then(|v| v.as_u64()),
        Some(samples),
        "every persisted sample is aggregated"
    );
    let exc = a
        .get("top_exclusive")
        .and_then(|v| v.as_array())
        .expect("top_exclusive");
    assert!(!exc.is_empty(), "exclusive weights exist");
    let top_fn = exc[0]
        .get("function")
        .and_then(|f| f.as_str())
        .expect("function name")
        .to_string();
    eprintln!("E2E-TOP-EXCLUSIVE {top_fn} w={:?}", exc[0].get("weight"));
    assert!(
        top_fn.contains("pf_test_support") || top_fn.contains("spin_ms"),
        "top exclusive function must be the known hotspot, got {top_fn}"
    );
    let tree = a.get("call_tree").expect("call tree");
    let tree_kids = tree
        .get("children")
        .and_then(|c| c.as_array())
        .expect("tree children");
    assert!(!tree_kids.is_empty(), "call tree renders");
    // Bounded output: top lists never exceed --limit (default 32).
    assert!(
        a.get("top_inclusive")
            .and_then(|v| v.as_array())
            .map(|x| x.len())
            .unwrap_or(0)
            <= 32
    );
    // Coverage honesty: present and in (0, 1].
    let cov = a
        .get("frame_identity_coverage")
        .and_then(|v| v.as_f64())
        .expect("coverage present");
    assert!(cov > 0.0 && cov <= 1.0, "coverage sane: {cov}");

    // 6. `pulseforge export-stacks` emits a spec-conformant speedscope
    //    document whose top frame (by summed sample weight) is the known
    //    hotspot — weight 1 per sample, unit "none" (never relabeled time).
    let export_path = data.join("export.speedscope.json");
    let (ok, out) = agent_json(
        &data,
        &[
            "export-stacks",
            "--session",
            session_dir.to_str().unwrap(),
            "--out",
            export_path.to_str().unwrap(),
        ],
    );
    assert!(ok, "export-stacks failed: {out}");
    let text = std::fs::read_to_string(&export_path).expect("export file written");
    let doc = pf_types::json::parse(text.trim()).expect("speedscope document parses");
    assert_eq!(
        doc.get("$schema")
            .and_then(|v| v.as_str())
            .map(|s| s.ends_with("file-format-schema.json")),
        Some(true),
        "$schema present"
    );
    let frames = doc
        .get("shared")
        .and_then(|s| s.get("frames"))
        .and_then(|f| f.as_array())
        .expect("shared.frames");
    assert!(!frames.is_empty());
    for f in frames {
        assert!(f.get("name").and_then(|n| n.as_str()).is_some());
    }
    let profile = doc
        .get("profiles")
        .and_then(|p| p.as_array())
        .and_then(|p| p.first())
        .cloned()
        .expect("one profile");
    assert_eq!(
        profile.get("type").and_then(|v| v.as_str()),
        Some("sampled")
    );
    assert_eq!(profile.get("unit").and_then(|v| v.as_str()), Some("none"));
    let samples = profile
        .get("samples")
        .and_then(|v| v.as_array())
        .expect("samples");
    let weights = profile
        .get("weights")
        .and_then(|v| v.as_array())
        .expect("weights");
    assert_eq!(samples.len(), weights.len(), "one weight per sample");
    assert!(!samples.is_empty());
    let name_of = |i: u64| -> String {
        frames[i as usize]
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string()
    };
    // Exclusive (leaf-only) weight per frame: root frames appear in every
    // stack, so INCLUSIVE sums legitimately crown main/libc. The top LEAF
    // frame of the spinning binary must be the known hotspot.
    let mut leaf_weight: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    let mut end_value = 0u64;
    for (smp, w) in samples.iter().zip(weights.iter()) {
        let idxs = smp.as_array().expect("frame index array");
        assert!(!idxs.is_empty(), "samples are non-empty");
        let weight = w.as_u64().expect("weight");
        end_value += weight;
        for i in idxs {
            let idx = i.as_u64().expect("frame index");
            assert!((idx as usize) < frames.len(), "frame index {idx} in range");
        }
        let leaf = idxs[idxs.len() - 1].as_u64().expect("leaf index");
        *leaf_weight.entry(leaf).or_insert(0) += weight;
    }
    assert_eq!(
        profile.get("endValue").and_then(|v| v.as_u64()),
        Some(end_value),
        "endValue equals summed weights"
    );
    let (best_idx, best_w) = leaf_weight
        .iter()
        .max_by_key(|(_, w)| **w)
        .map(|(k, v)| (*k, *v))
        .expect("weighted leaf frames");
    let best_name = name_of(best_idx);
    eprintln!("E2E-EXPORT-TOP {best_name} w={best_w}");
    assert!(
        best_name.contains("pf_test_support") || best_name.contains("spin_ms"),
        "top exported leaf frame must be the known hotspot, got {best_name}"
    );

    // 7. `pulseforge export-pprof` emits a profile.proto document that our
    //    spec-conformance decoder reads back — top leaf = known hotspot.
    let pprof_path = data.join("export.pb");
    let (ok, out) = agent_json(
        &data,
        &[
            "export-pprof",
            "--session",
            session_dir.to_str().unwrap(),
            "--out",
            pprof_path.to_str().unwrap(),
        ],
    );
    assert!(ok, "export-pprof failed: {out}");
    let doc = std::fs::read(&pprof_path).expect("pprof document written");
    let validation = pf_analysis::export::validate_pprof_document(&doc).expect("spec conformance");
    assert!(
        validation.samples >= 1,
        "validated samples {} > 0",
        validation.samples
    );
    let (top_fn, top_w) = validation
        .by_leaf
        .iter()
        .max_by_key(|(_, w)| **w)
        .map(|(k, w)| (k.clone(), *w))
        .expect("weighted leaf frames");
    eprintln!("E2E-PPROF-TOP {top_fn} w={top_w}");
    assert!(
        top_fn.contains("pf_test_support") || top_fn.contains("spin_ms"),
        "pprof top leaf must be the known hotspot, got {top_fn}"
    );

    drop(daemon);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn function_shift_compare_reports_gains_and_losses() {
    // Two synthetic sessions in the same data root: a "baseline" whose
    // hot function dominates, and a "candidate" where a different
    // function gained share. `stacks --compare-session` must report the
    // shift honestly (gains, losses, unchanged).
    let data = std::env::temp_dir().join(format!(
        "pf-shift-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::create_dir_all(&data);
    let _ = std::fs::create_dir_all(&data);
    let mk = |name: &str, hex_id: &str, hot: &str, hot_n: usize, other: &str, other_n: usize| {
        let dir = data.join(format!("{name}.pfsession"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{"session_id":"01010101-0000-7000-8000-{hex_id:0>12}","state":"COMPLETED"}}"#
            ),
        )
        .unwrap();
        let frame = |m: &str, s: &str| {
            format!(
                r#"{{"module":"{m}","symbol":"{s}","offset":8,"inlined":false,"file":null,"line":null}}"#
            )
        };
        let mut lines = Vec::new();
        for _ in 0..hot_n {
            lines.push(format!(
                r#"{{"time_ns":1,"tid":1,"cpu":0,"ips":[1],"frames":[{},{}]}}"#,
                frame("app", hot),
                frame("app", "main")
            ));
        }
        for _ in 0..other_n {
            lines.push(format!(
                r#"{{"time_ns":2,"tid":1,"cpu":0,"ips":[1],"frames":[{},{}]}}"#,
                frame("app", other),
                frame("app", "main")
            ));
        }
        std::fs::write(dir.join("stacks.jsonl"), lines.join("\n")).unwrap();
        dir
    };
    let base = mk("baseline", "0000000000ba", "cold_fn", 50, "warm_fn", 50);
    let cand = mk("candidate", "0000000000ca", "cold_fn", 20, "new_hot", 80);

    let (ok, out) = agent_json(
        &data,
        &[
            "stacks",
            "--session",
            cand.to_str().unwrap(),
            "--limit",
            "16",
            "--compare-session",
            base.to_str().unwrap(),
        ],
    );
    assert!(ok, "compare run failed: {out}");
    let v = pf_types::json::parse(out.trim())
        .ok()
        .filter(|x| x.as_object_pairs().is_some())
        .expect("JSON output");
    let shifts = v
        .get("function_shifts")
        .and_then(|s| s.as_array())
        .expect("function_shifts array");
    assert!(!shifts.is_empty());
    let find = |name: &str| {
        shifts
            .iter()
            .find(|s| s.get("function").and_then(|f| f.as_str()) == Some(name))
            .expect(name)
            .clone()
    };
    let gained = find("new_hot");
    assert_eq!(gained.get("baseline").and_then(|x| x.as_u64()), Some(0));
    assert_eq!(gained.get("candidate").and_then(|x| x.as_u64()), Some(80));
    assert_eq!(gained.get("delta").and_then(|x| x.as_i64()), Some(80));
    let lost = find("warm_fn");
    assert_eq!(lost.get("baseline").and_then(|x| x.as_u64()), Some(50));
    assert_eq!(lost.get("candidate").and_then(|x| x.as_u64()), Some(0));
    assert_eq!(lost.get("delta").and_then(|x| x.as_i64()), Some(-50));
    let stable = find("cold_fn");
    assert_eq!(stable.get("delta").and_then(|x| x.as_i64()), Some(-30));
    let _ = std::fs::remove_dir_all(&data);
}
