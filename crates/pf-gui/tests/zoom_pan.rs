//! PF-704 headless tests: zoom/pan viewport math, envelope-backed series
//! scoped to the window, spike preservation through the P5 envelope path,
//! and render budget while zoomed.

#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use egui::{Pos2, Rect, Vec2};
use pf_gui::{DashboardModel, FakeDaemon, PulseForgeApp, View};
use pf_types::{CounterSnapshot, SessionId};

const MS: u64 = 1_000_000;

/// 10 000 samples over 10 s; one 512 MiB spike at t=5 s, baseline 4 MiB.
fn spike_series() -> Vec<CounterSnapshot> {
    (0..10_000u64)
        .map(|i| {
            let t = i * MS;
            let ws = if (4900..=5100).contains(&(t / MS)) {
                512 * 1024 * 1024
            } else {
                4 * 1024 * 1024
            };
            CounterSnapshot {
                mono_ns: t,
                cpu_user_ns: Some(t / 100),
                cpu_kernel_ns: Some(0),
                resident_bytes: Some(ws),
                io_read_bytes: Some(i * 1024),
                io_write_bytes: Some(0),
                thread_count: Some(2),
                ..CounterSnapshot::default()
            }
        })
        .collect()
}

fn model_with_data() -> (DashboardModel, FakeDaemon) {
    let id = SessionId::now_v7();
    let mut fake = FakeDaemon::default();
    fake.samples_result = spike_series();
    let mut d = DashboardModel::new(id);
    d.refresh(&fake);
    (d, fake)
}

#[test]
fn zoom_clamps_to_data_range_and_min_span() {
    let (mut d, _) = model_with_data();
    assert_eq!(d.data_start_ns, Some(0));
    assert_eq!(d.data_end_ns, Some(9_999 * MS));

    // Zoom deep: span clamps at 1 ms minimum, stays inside data.
    for _ in 0..40 {
        d.zoom_by(2.0);
    }
    let (s, e) = d.effective_view();
    assert!(e - s >= 1_000_000, "min span 1 ms, got {}", e - s);
    assert!(e <= 9_999 * MS, "clamped inside data: {s}..{e}");
    assert!(d.zoomed);

    // Reset restores the full range.
    d.reset_zoom();
    assert!(!d.zoomed);
    let (s, e) = d.effective_view();
    assert_eq!((s, e), (0, 9_999 * MS));
}

#[test]
fn pan_shifts_and_clamps() {
    let (mut d, _) = model_with_data();
    d.zoom_by(4.0); // 9999 ms / 4 = 2499.75 ms window
    let (s0, e0) = d.effective_view();
    let span = e0 - s0;
    assert_eq!(span, 2_499_750_000, "quarter of the 9999 ms range");

    // Pan right by 25% of span, twice.
    d.pan_by(0.25);
    d.pan_by(0.25);
    let (s1, e1) = d.effective_view();
    assert_eq!(e1 - s1, span, "span preserved");
    assert!(s1 > s0, "moved right: {s0} -> {s1}");

    // Pan far right: clamps at data end.
    for _ in 0..20 {
        d.pan_by(0.25);
    }
    let (s2, e2) = d.effective_view();
    assert_eq!(e2, 9_999 * MS, "clamped at data end");
    assert_eq!(e2 - s2, span);
}

#[test]
fn zoomed_envelope_scopes_to_window_and_preserves_spike() {
    let (mut d, fake) = model_with_data();

    // Zoom into the spike window (4.5 s..5.5 s).
    d.view_start_ns = Some(4_500 * MS);
    d.view_end_ns = Some(5_500 * MS);
    d.zoomed = true;
    d.refresh(&fake);

    // Envelope path (resident): max must show the 512 MiB spike even
    // though only bucket minima are in `points` (spike preserved).
    assert_eq!(d.resident_bytes.max, Some(512.0 * 1024.0 * 1024.0));
    assert_eq!(d.resident_bytes.min, Some(4.0 * 1024.0 * 1024.0));
    // All bucket points sit inside the window.
    assert!(
        d.resident_bytes
            .points
            .iter()
            .all(|(t, _)| *t >= 4_500 * MS && *t <= 5_500 * MS)
    );

    // Zoom elsewhere: no spike visible.
    d.view_start_ns = Some(0);
    d.view_end_ns = Some(1_000 * MS);
    d.refresh(&fake);
    assert_eq!(d.resident_bytes.max, Some(4.0 * 1024.0 * 1024.0));
}

#[test]
fn full_range_never_uses_envelope_path() {
    let (mut d, _) = model_with_data();
    d.refresh(&fake_client());
    // Raw series: 10k points (not bucketed).
    assert_eq!(d.resident_bytes.points.len(), 10_000);
    assert!(!d.zoomed);
    // fake_client() helper not needed here; using the same fake's samples.
}

fn fake_client() -> FakeDaemon {
    let mut f = FakeDaemon::default();
    f.samples_result = spike_series();
    f
}

// Render *budgets* are measured on the GPU reference machine (roadmap
// §16: never gate on a single unrepeatable number on a non-reference
// host). WSL2 renders via software (no GPU), so the budget assertion is
// ignored there with this named reason — the correctness assertions above
// (series built, envelopes scoped) still run everywhere.
#[test]
#[cfg_attr(
    target_os = "linux",
    ignore = "WSL2 software rendering is not the GPU reference machine;               render budgets are measured on the reference host (roadmap §16)"
)]
fn zoomed_dashboard_renders_within_budget() {
    let id = SessionId::now_v7();
    let mut fake = FakeDaemon::default();
    fake.samples_result = spike_series();
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::new(Arc::new(fake));
    app.open_dashboard_for_tests(id);
    app.view = View::Dashboard(id);
    if let Some(d) = app.dashboard.as_mut() {
        d.zoom_by(8.0);
    }
    let ctx = egui::Context::default();
    let mut times = Vec::new();
    for f in 0..20 {
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 780.0)));
        input.time = Some(f as f64 * 0.016);
        let t0 = std::time::Instant::now();
        let out = ctx.run(input, |ctx| app.update(ctx));
        let _ = ctx.tessellate(out.shapes, 1.0);
        times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p95 = times[(times.len() as f64 * 0.95) as usize % times.len()];
    assert!(
        p95 < 60.0,
        "zoomed dashboard p95 {p95:?} exceeds debug budget"
    );
}
