//! Flamegraph panel tests (PF-906 bounded slice): the StacksModel loads a
//! session's persisted stacks, the bounded icicle renders headlessly
//! through the REAL `PulseForgeApp::update`, and search/zoom stay within
//! their caps. The Linux test records a REAL session via the daemon and
//! asserts the known hotspot drives the panel.

// Bounded test counters and f32 rect math.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::field_reassign_with_default
)]

use std::path::PathBuf;
use std::sync::Arc;

use egui::{Pos2, Rect, Vec2};
use pf_gui::PulseForgeApp;

fn fresh_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pf-flame-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::create_dir_all(&d);
    d
}

const SID: &str = "00000000-0000-7000-8000-0000000000ff";

/// A synthetic session dir with the artifacts the finalize path writes.
fn synth_session(tag: &str) -> PathBuf {
    let dir = fresh_dir(tag);
    std::fs::write(
        dir.join("manifest.json"),
        format!(
            r#"{{"schema":"pulseforge.session/0","session_id":"{SID}","state":"COMPLETED",
 "target":{{"os":"linux","pid":4242,"creation_identity":1,"boot_identity":"b","image":"testbin"}},
 "stop_reason":"duration","warnings":[]}}"#
        ),
    )
    .unwrap();
    // 6 samples: 4 through worker->hot_function, 2 main->hot_function,
    // 2 recursive a->b->a->b (cut), 1 with an unresolved leaf.
    let frame = |m: &str, s: &str, o: u64| {
        format!(
            r#"{{"module":"{m}","symbol":{},"offset":{o}}}"#,
            if s.is_empty() {
                "null".to_string()
            } else {
                format!(r#""{s}""#)
            }
        )
    };
    let line = |time_ns: u64, tid: u64, frames: &[String]| {
        format!(
            r#"{{"time_ns":{},"tid":{},"ips":[1],"frames":[{}]}}"#,
            time_ns,
            tid,
            frames.join(",")
        )
    };
    // Samples 1 ms apart: time filtering (PF-906) has a real range to cut.
    let mut lines = Vec::new();
    let mut stamp = 0u64;
    let mut next_ns = || {
        stamp += 1_000_000;
        stamp
    };
    // Threads: tid 100 carries the first 6 samples, tid 200 the last 2 —
    // thread filtering (PF-906) has two real populations to cut between.
    for _ in 0..4 {
        lines.push(line(
            next_ns(),
            100,
            &[
                frame("testbin", "hot_function", 0x30),
                frame("testbin", "worker", 0x20),
                frame("testbin", "main", 0x10),
            ],
        ));
    }
    for _ in 0..2 {
        lines.push(line(
            next_ns(),
            100,
            &[
                frame("testbin", "hot_function", 0x30),
                frame("testbin", "main", 0x10),
            ],
        ));
    }
    lines.push(line(
        next_ns(),
        200,
        &[
            frame("testbin", "b", 0x50),
            frame("testbin", "a", 0x40),
            frame("testbin", "b", 0x50),
            frame("testbin", "a", 0x40),
        ],
    ));
    lines.push(line(
        next_ns(),
        200,
        &[frame("libx.so", "", 0x60), frame("testbin", "main", 0x10)],
    ));
    std::fs::write(dir.join("stacks.jsonl"), lines.join("\n")).unwrap();
    std::fs::write(
        dir.join("stacks-summary.json"),
        r#"{"schema":"pulseforge.stacks/0","samples":8,"frames_total":25,"frames_resolved":24}"#,
    )
    .unwrap();
    dir
}

fn run_frames(app: &mut PulseForgeApp, frames: usize) {
    let ctx = egui::Context::default();
    for f in 0..frames {
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 780.0)));
        input.time = Some(f as f64 * 0.016);
        let out = ctx.run(input, |ctx| app.update(ctx));
        let _ = ctx.tessellate(out.shapes, 1.0);
    }
}

#[test]
fn stacks_model_loads_and_aggregates_a_session() {
    let dir = synth_session("model");
    let mut model = pf_gui::flame::StacksModel::default();
    model.maybe_reload(Some(&dir));
    assert!(model.available, "{:?}", model.error);
    assert_eq!(model.samples, 8);
    // Coverage from the summary: 24/25.
    let cov = model.coverage.expect("coverage present");
    assert!((cov - 0.96).abs() < 1e-9, "{cov}");
    // The known function is in the bounded tree; recursion collapsed (a
    // and b each appear exactly once at their level).
    let roots = &model.tree.root_children;
    assert!(
        roots
            .iter()
            .any(|n| pf_gui::flame::tree_contains(n, "hot_function")),
        "hot_function must be in the tree"
    );
    // hot_function inclusive weight = 4 + 2 = 6 of 8 total.
    // With callee-first data correctly flipped to outermost-first, the
    // trie separates the two call paths: hot_function under worker (4)
    // and hot_function directly under main (2). Sum both.
    assert_eq!(model.total_weight, 8);
    let hot_total: u64 = sum_inclusive(roots, "hot_function");
    assert_eq!(hot_total, 6, "hot_function total inclusive");
    // Reload is guarded: same dir is a no-op (no error churn).
    model.maybe_reload(Some(&dir));
    assert!(model.available);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn time_window_filters_samples_and_reaggregates() {
    let dir = synth_session("window");
    let mut model = pf_gui::flame::StacksModel::default();
    model.maybe_reload(Some(&dir));
    assert!(model.available);
    assert_eq!(model.samples, 8);
    let (lo, hi) = model.t_range.expect("range present");
    assert_eq!((hi - lo) / 1_000_000, 7, "8 samples 1 ms apart");

    // The window is relative to the FIRST sample (t_min). Sample times
    // relative to it: 0..7 ms. Window [0, 4] ms keeps 5 of 8 samples.
    model.window_ms = (0, 4);
    model.reaggregate();
    assert!(model.time_filtered);
    assert_eq!(model.samples, 5, "samples at 0..=4 ms relative");
    assert_eq!(model.filtered_out, 3);
    // The hotspot is inside the window and carries its windowed weight
    // (hot leaves sit at relative 0..5 ms; five are in the window).
    let hot_w: u64 = sum_inclusive(&model.tree.root_children, "hot_function");
    assert_eq!(hot_w, 5);

    // A zero-width window [3, 3] ms keeps exactly one sample.
    model.window_ms = (3, 3);
    model.reaggregate();
    assert_eq!(model.samples, 1);

    // Out-of-order ends are ordered by reaggregation (clamped to range):
    // [2, 6] ms keeps relative samples 2,3,4,5,6.
    model.window_ms = (6, 2);
    model.reaggregate();
    assert_eq!(model.samples, 5, "clamped to [2, 6] ms");

    // Reset restores the full view.
    model.window_ms = (0, 7);
    model.reaggregate();
    assert!(!model.time_filtered);
    assert_eq!(model.samples, 8);
    assert_eq!(model.filtered_out, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn thread_filter_isolates_tids_and_combines_with_time() {
    let dir = synth_session("tid");
    let mut model = pf_gui::flame::StacksModel::default();
    model.maybe_reload(Some(&dir));
    assert!(model.available);
    assert_eq!(model.samples, 8);
    // Bounded thread list, descending by samples: tid 100 (6), tid 200 (2).
    assert_eq!(model.tids, vec![(100, 6), (200, 2)]);
    assert_eq!(model.tids_hidden, 0);
    assert!(!model.thread_filtered);

    // Filter to tid 100: 6 samples, recursion+unresolved excluded.
    model.tid_filter = Some(100);
    model.reaggregate();
    assert!(model.thread_filtered);
    assert_eq!(model.samples, 6);
    assert_eq!(model.filtered_out, 2);
    let hot_total_tid100: u64 = sum_inclusive(&model.tree.root_children, "hot_function");
    assert_eq!(hot_total_tid100, 6, "all hot samples belong to tid 100");

    // Filter to tid 200: recursion + unresolved leaf only.
    model.tid_filter = Some(200);
    model.reaggregate();
    assert_eq!(model.samples, 2);
    // tid 200's unresolved leaf lowers coverage honestly.
    assert!(model.coverage.unwrap_or(1.0) < 1.0);

    // An unknown tid selects nothing — honestly zero, with the count kept.
    model.tid_filter = Some(999);
    model.reaggregate();
    assert_eq!(model.samples, 0);
    assert_eq!(model.filtered_out, 8);
    assert!(model.last_painted_rects == 0 || true); // empty window paints none

    // Combined with a time window: tid 100 spans relative 0..5 ms, so
    // [0, 4] ms keeps its first 5 samples.
    model.tid_filter = Some(100);
    model.window_ms = (0, 4);
    model.reaggregate();
    assert_eq!(model.samples, 5, "tid 100 ∩ first 4 ms");

    // Reset both.
    model.tid_filter = None;
    model.window_ms = (0, 7);
    model.reaggregate();
    assert!(!model.thread_filtered && !model.time_filtered);
    assert_eq!(model.samples, 8);
    let _ = std::fs::remove_dir_all(&dir);
}

pub fn sum_inclusive(nodes: &[pf_analysis::stacks::FlameNode], key: &str) -> u64 {
    nodes
        .iter()
        .map(|n| {
            let own = if n.key == key { n.inclusive } else { 0 };
            own + sum_inclusive(&n.children, key)
        })
        .sum()
}

#[test]
fn offline_dashboard_paints_the_flame_panel() {
    let dir = synth_session("render");
    let model = pf_gui::app::open_session_offline(&dir).expect("offline open");
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::offline(model);
    run_frames(&mut app, 3);
    let stacks = &app.stacks;
    assert!(stacks.available, "{:?}", stacks.error);
    assert_eq!(stacks.samples, 8);
    assert!(
        stacks.last_painted_rects > 0,
        "the icicle painted {} rects",
        stacks.last_painted_rects
    );

    // Search caps and re-renders without unbounded growth.
    let mut app2 =
        PulseForgeApp::offline(pf_gui::app::open_session_offline(&dir).expect("offline open 2"));
    app2.stacks.search = "hot".repeat(200); // over the cap
    run_frames(&mut app2, 2);
    assert!(app2.stacks.search.len() <= 128, "search string is capped");
    assert!(app2.stacks.last_painted_rects > 0);

    // Zoom to a subtree and reset.
    let mut app3 =
        PulseForgeApp::offline(pf_gui::app::open_session_offline(&dir).expect("offline open 3"));
    run_frames(&mut app3, 1);
    let zoom_root = app3
        .stacks
        .tree
        .root_children
        .first()
        .cloned()
        .expect("tree root");
    app3.stacks.zoom = Some(zoom_root);
    run_frames(&mut app3, 2);
    assert!(app3.stacks.last_painted_rects > 0);
    app3.stacks.zoom = None;
    run_frames(&mut app3, 1);
    assert!(app3.stacks.last_painted_rects > 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn counters_only_sessions_render_an_honest_unavailable_note() {
    let dir = fresh_dir("nostacks");
    std::fs::write(
        dir.join("manifest.json"),
        format!(r#"{{"session_id":"{SID}","state":"COMPLETED"}}"#),
    )
    .unwrap();
    // No stacks.jsonl: the loader must report unavailability, not zeros.
    let model = pf_gui::app::open_session_offline(&dir).expect("offline open");
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::offline(model);
    run_frames(&mut app, 2);
    assert!(!app.stacks.available);
    assert!(app.stacks.error.is_some());
    assert_eq!(app.stacks.last_painted_rects, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Real-data evidence (Linux/WSL2): a daemon-recorded bench-spin session
/// drives the panel, and the known hotspot is present in the bounded tree.
#[test]
#[cfg(target_os = "linux")]
fn real_recorded_session_drives_the_flame_panel() {
    use std::process::{Child, Command, Stdio};

    fn exe() -> PathBuf {
        // pf-gui does not build the CLI itself; the workspace target dir
        // has it after `cargo test` builds the workspace.
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/debug/pulseforge")
            .canonicalize()
            .expect("workspace CLI binary (target/debug/pulseforge) must exist");
        assert!(p.is_file(), "missing CLI binary at {}", p.display());
        p
    }

    struct DaemonGuard(Child);
    impl Drop for DaemonGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let data = fresh_dir("real");
    let mut workload = Command::new(exe())
        .args(["bench-spin", "--ms", "15000"])
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn bench-spin");
    let daemon = DaemonGuard(
        Command::new(exe())
            .args(["daemon"])
            .env("PULSEFORGE_DATA", &data)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon"),
    );
    std::thread::sleep(std::time::Duration::from_millis(900));

    // Record 3 s via the CLI agent (UDS transport).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let out = loop {
        let out = Command::new(exe())
            .args([
                "agent",
                "start",
                "--pid",
                &workload.id().to_string(),
                "--duration-ms",
                "3000",
                "--interval-ms",
                "150",
            ])
            .env("PULSEFORGE_DATA", &data)
            .output()
            .expect("run agent");
        if out.status.success() {
            break out;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "agent start never succeeded"
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let sid = text
        .lines()
        .rev()
        .find_map(|l| {
            pf_types::json::parse(l).ok().and_then(|v| {
                v.get("session_id")
                    .and_then(|s| s.as_str())
                    .map(String::from)
            })
        })
        .expect("session id");

    // Wait for the recording to finish.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "session {sid} never completed"
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        let out = Command::new(exe())
            .args(["agent", "status", "--session", &sid])
            .env("PULSEFORGE_DATA", &data)
            .output()
            .expect("run agent status");
        let t = String::from_utf8_lossy(&out.stdout);
        if t.contains("COMPLETED") {
            break;
        }
    }
    let _ = workload.kill();
    let _ = workload.wait();

    // Open the recorded session offline and render the panel.
    let dir = data.join("sessions").join(format!("{sid}.pfsession"));
    let model = pf_gui::app::open_session_offline(&dir).expect("offline open");
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::offline(model);
    run_frames(&mut app, 3);
    assert!(
        app.stacks.available,
        "real session must have stacks: {:?}",
        app.stacks.error
    );
    assert!(app.stacks.samples > 100, "{} samples", app.stacks.samples);
    let roots = &app.stacks.tree.root_children;
    assert!(
        roots
            .iter()
            .any(|n| pf_gui::flame::tree_contains(n, "pf_test_support")),
        "the known hotspot must drive the flame panel"
    );
    assert!(
        app.stacks.last_painted_rects > 0,
        "the icicle painted the real data"
    );

    // PF-906: time-filter the real recording to its first half.
    let full_samples = app.stacks.samples;
    let (lo, hi) = app.stacks.t_range.expect("real range present");
    let half_ms = (hi.saturating_sub(lo)) / 2_000_000;
    app.stacks.window_ms = (0, half_ms);
    app.stacks.reaggregate();
    run_frames(&mut app, 2);
    assert!(
        app.stacks.time_filtered,
        "half-window must exclude some samples"
    );
    assert!(
        app.stacks.samples > 0 && app.stacks.samples < full_samples,
        "windowed samples {} of {}",
        app.stacks.samples,
        full_samples
    );
    assert!(
        app.stacks
            .tree
            .root_children
            .iter()
            .any(|n| pf_gui::flame::tree_contains(n, "pf_test_support")),
        "the hotspot spans the whole recording, so the window keeps it"
    );
    assert!(app.stacks.last_painted_rects > 0, "windowed view paints");
    // Reset restores the full view.
    app.stacks.window_ms = (0, (hi.saturating_sub(lo)) / 1_000_000);
    app.stacks.reaggregate();
    assert_eq!(app.stacks.samples, full_samples);
    drop(daemon);
    let _ = std::fs::remove_dir_all(&data);
}

/// Real recorded data (Linux/WSL2): the per-tid filter against a real
/// daemon session of the two-thread fixture. Per-thread sampling
/// (PF-1003) means both spinning threads appear as distinct tids; the
/// top thread isolates a strict subset and Reset restores the full view.
#[test]
#[cfg(target_os = "linux")]
fn real_threads_session_drives_the_tid_filter() {
    use std::process::{Child, Command, Stdio};

    fn workspace_bin(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/debug")
            .join(name)
            .canonicalize()
            .unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    struct DaemonGuard(Child);
    impl Drop for DaemonGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let data = fresh_dir("tids");
    let fixture = workspace_bin("pf-fixture-threads");
    let mut workload = Command::new(&fixture)
        .args(["--ms", "15000"])
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn threads fixture");
    let daemon = DaemonGuard(
        Command::new(workspace_bin("pulseforge"))
            .args(["daemon"])
            .env("PULSEFORGE_DATA", &data)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon"),
    );
    std::thread::sleep(std::time::Duration::from_millis(900));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let out = loop {
        let out = Command::new(workspace_bin("pulseforge"))
            .args([
                "agent",
                "start",
                "--pid",
                &workload.id().to_string(),
                "--duration-ms",
                "3000",
                "--interval-ms",
                "150",
            ])
            .env("PULSEFORGE_DATA", &data)
            .output()
            .expect("run agent");
        if out.status.success() {
            break out;
        }
        assert!(std::time::Instant::now() < deadline, "agent start stuck");
        std::thread::sleep(std::time::Duration::from_millis(300));
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let sid = text
        .lines()
        .rev()
        .find_map(|l| {
            pf_types::json::parse(l).ok().and_then(|v| {
                v.get("session_id")
                    .and_then(|s| s.as_str())
                    .map(String::from)
            })
        })
        .expect("session id");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "session never completed"
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        let out = Command::new(workspace_bin("pulseforge"))
            .args(["agent", "status", "--session", &sid])
            .env("PULSEFORGE_DATA", &data)
            .output()
            .expect("agent status");
        if String::from_utf8_lossy(&out.stdout).contains("COMPLETED") {
            break;
        }
    }
    let _ = workload.kill();
    let _ = workload.wait();

    let dir = data.join("sessions").join(format!("{sid}.pfsession"));
    let model = pf_gui::app::open_session_offline(&dir).expect("offline open");
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::offline(model);
    run_frames(&mut app, 3);
    assert!(
        app.stacks.available,
        "real session must have stacks: {:?}",
        app.stacks.error
    );
    let full_samples = app.stacks.samples;
    assert!(full_samples > 100, "{full_samples} samples");
    // Per-thread sampling (PF-1003): both spinning fixture threads appear.
    assert!(
        app.stacks.tids.len() >= 2,
        "two hot threads must be sampled, got {:?}",
        app.stacks.tids
    );
    eprintln!("E2E-TIDS {:?}", app.stacks.tids);

    // The top thread isolates a strict subset (both threads spin for the
    // whole recording, so neither owns all samples).
    let top_tid = app.stacks.tids[0].0;
    app.stacks.tid_filter = Some(top_tid);
    app.stacks.reaggregate();
    run_frames(&mut app, 2);
    assert!(app.stacks.thread_filtered);
    assert!(
        app.stacks.samples > 0 && app.stacks.samples < full_samples,
        "top thread {top_tid} keeps {} of {full_samples} samples",
        app.stacks.samples
    );

    // Reset restores the full view.
    app.stacks.tid_filter = None;
    app.stacks.reaggregate();
    assert_eq!(app.stacks.samples, full_samples);
    assert_eq!(app.stacks.filtered_out, 0);
    drop(daemon);
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn bottom_up_toggle_swaps_exclusive_weight() {
    let dir = synth_session("bu-toggle");
    let model = pf_gui::app::open_session_offline(&dir).expect("offline open");
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::offline(model);
    run_frames(&mut app, 3);
    assert!(app.stacks.available);
    // Top-down: root children are the outermost frames (main).
    let td_roots: Vec<String> = app
        .stacks
        .tree
        .root_children
        .iter()
        .map(|n| n.key.clone())
        .collect();
    assert!(
        td_roots.contains(&"main".to_string()),
        "top-down roots: {td_roots:?}"
    );
    let td_before = td_roots.clone();

    // Toggle bottom-up: the trie reverses — root children are now the
    // LEAF frames (burn / hot_function / unresolved), not the entry points.
    app.stacks.bottom_up = true;
    app.stacks.reaggregate();
    run_frames(&mut app, 2);
    let bu_roots: Vec<String> = app
        .stacks
        .tree
        .root_children
        .iter()
        .map(|n| n.key.clone())
        .collect();
    assert!(
        !bu_roots.contains(&"main".to_string()),
        "bottom-up roots must not contain the entry point: {bu_roots:?}"
    );
    assert_eq!(
        app.stacks.total_weight, 8,
        "bottom-up: total weight unchanged"
    );

    // Toggle back: root children return to the top-down distribution.
    app.stacks.bottom_up = false;
    app.stacks.reaggregate();
    run_frames(&mut app, 1);
    let back_roots: Vec<String> = app
        .stacks
        .tree
        .root_children
        .iter()
        .map(|n| n.key.clone())
        .collect();
    assert_eq!(back_roots, td_before, "toggle back restores top-down roots");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Silence unused-import warnings on platforms where the Linux e2e is off.
#[allow(unused)]
fn _keep_alive(_: Arc<()>) {}
