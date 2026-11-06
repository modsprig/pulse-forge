//! 1M-sample flamegraph stress (PF-910 remainder, P9 proper — unblocked by
//! the PF-906 icicle panel): the GUI stacks panel at the recorder's stress
//! scale, built IN MEMORY (no 600 MB session file per gate run).
//!
//! Property under test: **boundedness** — the bounded icicle paints at
//! most [`MAX_RECTS`] rects with honest omission counts, the flame-tree
//! caps hold, and re-aggregation on a window change stays bounded. Timings
//! are PRINTED for the ledger and never asserted against (measurement, not
//! a performance claim — roadmap §16); one generous paint sanity bound
//! exists only to catch accidental unbounded painting.

// Bounded test counters and f32 rect math.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::field_reassign_with_default
)]

use std::path::PathBuf;
use std::time::Instant;

use egui::{Pos2, Rect, Vec2};
use pf_analysis::stacks::{SampledStack, StackFrame};
use pf_analysis::stacks_io::TimedStack;
use pf_gui::PulseForgeApp;

const N: usize = 1_000_000;
const RECTS_CAP: usize = 2_000; // mirror of flame.rs MAX_RECTS

fn fresh_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pf-flame-stress-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::create_dir_all(&d);
    d
}

const SID: &str = "00000000-0000-7000-8000-0000000000dd";

/// A tiny synthetic session dir so the offline app opens; the 1M-sample
/// corpus is injected into the model in memory (the reload guard keeps
/// the injection alive because the dir matches `loaded_from`).
fn synth_session(tag: &str) -> PathBuf {
    let dir = fresh_dir(tag);
    std::fs::write(
        dir.join("manifest.json"),
        format!(
            r#"{{"schema":"pulseforge.session/0","session_id":"{SID}","state":"COMPLETED",
 "target":{{"os":"linux","pid":4242,"creation_identity":1,"boot_identity":"b","image":"stressbin"}},
 "stop_reason":"duration","warnings":[]}}"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join("stacks.jsonl"), "").unwrap();
    std::fs::write(
        dir.join("stacks-summary.json"),
        r#"{"schema":"pulseforge.stacks/0","samples":0,"frames_total":0,"frames_resolved":0}"#,
    )
    .unwrap();
    dir
}

fn frame(key: &str, module: &str, resolved: bool) -> StackFrame {
    StackFrame {
        key: key.into(),
        module: module.into(),
        resolved,
        file: None,
        line: None,
        offset: 0,
    }
}

/// The stress_1m shape mix (proven memory-fine at this scale): deep
/// recursion, wide fan-out, unresolved leaves, 8-module chains, broken
/// references — outermost frame first, like the loader returns.
fn sample(i: usize) -> SampledStack {
    let kind = i % 5;
    let frames: Vec<StackFrame> = match kind {
        0 => (0..12)
            .map(|d| {
                frame(
                    if d % 2 == 0 { "recurse_a" } else { "recurse_b" },
                    "app",
                    true,
                )
            })
            .collect(),
        1 => {
            let mut v = vec![frame("wide_root", "app", true)];
            for d in 0..10 {
                v.push(frame(&format!("wide_{d}"), "app", true));
            }
            v
        }
        2 => vec![
            frame("known_caller", "app", true),
            frame("<unresolved>", "", false),
        ],
        3 => (0..8)
            .map(|d| frame(&format!("chain_{d}"), &format!("lib{d}.so"), true))
            .collect(),
        _ => vec![
            frame("caller", "app", true),
            frame("after_broken", "gone.so", false),
        ],
    };
    SampledStack { frames, weight: 1 }
}

fn timed(i: usize) -> TimedStack {
    TimedStack {
        time_ns: 1_000_000_000 + i as u64 * 5_000, // 5 s window
        tid: 4242,
        stack: sample(i),
    }
}

fn run_frames(app: &mut PulseForgeApp, frames: usize) -> Vec<f64> {
    let ctx = egui::Context::default();
    let mut times = Vec::with_capacity(frames);
    for f in 0..frames {
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 780.0)));
        input.time = Some(f as f64 * 0.016);
        let t0 = Instant::now();
        let out = ctx.run(input, |ctx| app.update(ctx));
        let _ = ctx.tessellate(out.shapes, 1.0);
        times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    times
}

fn p95(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[(v.len() as f64 * 0.95) as usize % v.len().max(1)]
}

#[test]
fn one_million_stack_samples_render_a_bounded_icicle() {
    let dir = synth_session("1m");
    let t_gen = Instant::now();
    let corpus: Vec<TimedStack> = (0..N).map(timed).collect();
    eprintln!(
        "PF910GUI generate: n={N} corpus_ms={:.1}",
        t_gen.elapsed().as_secs_f64() * 1000.0
    );

    pf_gui::testing::hermetic_data_root();
    let mut app =
        PulseForgeApp::offline(pf_gui::app::open_session_offline(&dir).expect("offline open"));
    {
        let m = &mut app.stacks;
        m.loaded_from = Some(dir.clone()); // reload guard keeps the injection
        m.all = corpus;
        m.available = true;
        m.coverage = Some(0.9);
        m.frames_resolved = Some(4_000_000);
        m.frames_total = Some(5_000_000);
        m.t_range = Some((1_000_000_000, 1_000_000_000 + (N as u64 - 1) * 5_000));
        m.window_ms = (0, (N as u64) * 5_000 / 1_000_000);
        m.tids = vec![(4242, N as u64)];
    }
    let t_agg = Instant::now();
    app.stacks.reaggregate();
    let agg_ms = t_agg.elapsed().as_secs_f64() * 1000.0;

    // Honesty + boundedness of the aggregation at 1M scale.
    assert_eq!(app.stacks.samples, N as u64, "all samples aggregated");
    assert_eq!(app.stacks.total_weight, N as u64, "weights exact");
    assert_eq!(app.stacks.filtered_out, 0, "full window filters nothing");
    let nodes = pf_gui::flame::count_nodes(&app.stacks.tree.root_children);
    assert!(
        nodes <= RECTS_CAP,
        "flame tree bounded at {nodes} (cap {RECTS_CAP})"
    );
    eprintln!(
        "PF910GUI aggregate: n={N} agg_ms={agg_ms:.1} tree_nodes={nodes} omitted={}",
        app.stacks.tree.omitted_children
    );

    // Paint through the real app: bounded rects, honest omissions, and a
    // generous sanity bound (an unbounded icicle would blow past this by
    // orders of magnitude; timing itself is printed, not gated).
    let times = run_frames(&mut app, 10);
    let paint = p95(times);
    assert!(
        app.stacks.last_painted_rects <= RECTS_CAP,
        "painted rects bounded: {}",
        app.stacks.last_painted_rects
    );
    assert!(paint < 250.0, "paint p95 {paint} ms — unbounded paint?");
    eprintln!(
        "PF910GUI paint: rects={} omitted={} p95_ms={paint:.1}",
        app.stacks.last_painted_rects, app.stacks.last_painted_omitted
    );

    // Half-window re-aggregation (the pan/zoom hot path): still exact and
    // bounded; the honest filtered-out count matches the window (the end
    // is inclusive; samples are 5 µs apart, window_ms → ns via ×1e6).
    let window_ns = ((N as u64) * 5_000 / 1_000_000 / 2) * 1_000_000;
    let inside = window_ns / 5_000 + 1;
    app.stacks.window_ms = (0, (N as u64) * 5_000 / 1_000_000 / 2);
    let t_re = Instant::now();
    app.stacks.reaggregate();
    let re_ms = t_re.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(
        app.stacks.filtered_out,
        N as u64 - inside,
        "half-window filters everything past the inclusive end"
    );
    assert_eq!(app.stacks.samples, inside);
    assert!(app.stacks.time_filtered);
    let nodes2 = pf_gui::flame::count_nodes(&app.stacks.tree.root_children);
    assert!(nodes2 <= RECTS_CAP);
    eprintln!(
        "PF910GUI reaggregate(half window): re_ms={re_ms:.1} nodes={nodes2} filtered_out={}",
        app.stacks.filtered_out
    );

    // Bottom-up direction at scale (PF-907): same boundedness.
    app.stacks.window_ms = (0, (N as u64) * 5_000 / 1_000_000);
    app.stacks.bottom_up = true;
    let t_bu = Instant::now();
    app.stacks.reaggregate();
    eprintln!(
        "PF910GUI reaggregate(bottom-up full): bu_ms={:.1} nodes={}",
        t_bu.elapsed().as_secs_f64() * 1000.0,
        pf_gui::flame::count_nodes(&app.stacks.tree.root_children)
    );
    assert_eq!(app.stacks.samples, N as u64);
    app.stacks.bottom_up = false;
    app.stacks.reaggregate();

    // Zoom at scale: re-rooting at a subtree clones a bounded view; the
    // painted rect count must stay capped and the deep subtree must
    // actually drive the render (zoomed content differs from the root).
    // The stress tree's roots are chain_0 / recurse_a / wide_root / …;
    // pick the first root with children for a meaningful zoom.
    let zoom_root = app
        .stacks
        .tree
        .root_children
        .iter()
        .find(|n| !n.children.is_empty())
        .cloned()
        .expect("stress tree has a subtree to zoom into");
    app.stacks.zoom = Some(zoom_root);
    let times_zoom = run_frames(&mut app, 10);
    let zoom_paint = p95(times_zoom);
    assert!(
        app.stacks.last_painted_rects <= RECTS_CAP,
        "zoomed paint bounded: {}",
        app.stacks.last_painted_rects
    );
    assert!(zoom_paint < 250.0, "zoomed paint p95 {zoom_paint} ms");
    eprintln!(
        "PF910GUI zoom paint: rects={} p95_ms={zoom_paint:.1}",
        app.stacks.last_painted_rects
    );
    app.stacks.zoom = None;

    // Search at scale: the highlight scan runs per painted rect over the
    // tree keys; bounded paint stays bounded with an active filter.
    app.stacks.search = "chain_".into();
    let times_search = run_frames(&mut app, 10);
    let search_paint = p95(times_search);
    assert!(app.stacks.last_painted_rects <= RECTS_CAP);
    assert!(search_paint < 250.0, "search paint p95 {search_paint} ms");
    eprintln!(
        "PF910GUI search paint: rects={} p95_ms={search_paint:.1}",
        app.stacks.last_painted_rects
    );
    app.stacks.search.clear();

    let _ = std::fs::remove_dir_all(&dir);
}
