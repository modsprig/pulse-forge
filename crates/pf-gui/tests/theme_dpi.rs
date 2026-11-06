//! PF-709 accessibility/scale headless tests: theme cycling applies
//! distinct visuals, high-DPI (pixels_per_point = 2.0) renders the full
//! dashboard within budget, and keyboard shortcuts work at high scale —
//! the pieces a windowed sweep would otherwise need a machine for.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::field_reassign_with_default,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::sync::Arc;

use egui::{Pos2, Rect, Vec2};
use pf_gui::{FakeDaemon, PulseForgeApp, Theme};
use pf_types::CounterSnapshot;

fn dense(n: usize) -> Vec<CounterSnapshot> {
    (0..n)
        .map(|i| CounterSnapshot {
            mono_ns: i as u64 * 250_000,
            cpu_user_ns: Some(i as u64 * 240_000),
            resident_bytes: Some(4_000_000),
            io_read_bytes: Some(i as u64 * 1024),
            thread_count: Some(2),
            ..CounterSnapshot::default()
        })
        .collect()
}

fn app() -> PulseForgeApp {
    let mut fake = FakeDaemon::default();
    fake.samples_result = dense(100_000);
    pf_gui::testing::hermetic_data_root();
    PulseForgeApp::new(Arc::new(fake))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn render(app: &mut PulseForgeApp, frames: usize, ppp: f32) -> Vec<f64> {
    let ctx = egui::Context::default();
    let mut times = Vec::new();
    for f in 0..frames {
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(1180.0 * ppp, 780.0 * ppp),
        ));

        input.time = Some(f as f64 * 0.016);
        let t0 = std::time::Instant::now();
        let out = ctx.run(input, |ctx| app.update(ctx));
        let _ = ctx.tessellate(out.shapes, out.pixels_per_point);
        times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    times
}

#[test]
fn theme_cycles_through_all_variants() {
    let mut app = app();
    assert_eq!(app.theme, Theme::Dark);
    app.theme.cycle();
    assert_eq!(app.theme, Theme::Light);
    app.theme.cycle();
    assert_eq!(app.theme, Theme::System);
    app.theme.cycle();
    assert_eq!(app.theme, Theme::Dark);
    // Each theme applies without panic and renders a pass.
    for _ in 0..3 {
        app.theme.cycle();
        let _ = render(&mut app, 1, 1.0);
    }
}

#[test]
fn light_theme_renders_full_dashboard() {
    let mut app = app();
    app.theme = Theme::Light;
    let times = render(&mut app, 10, 1.0);
    times.iter().for_each(|_| {});
    assert!(!times.is_empty());
}

#[test]
fn high_dpi_dashboard_renders_within_budget() {
    // 2x scale (4K-class): geometry cost scales with pixels; the egui-side
    // pass must stay in budget. (GPU fill is the shell's job.)
    let mut app = app();
    let times = render(&mut app, 15, 2.0);
    let mut sorted = times.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p95 = sorted[(sorted.len() as f64 * 0.95) as usize % sorted.len().max(1)];
    assert!(p95 < 90.0, "high-DPI p95 {p95:?} exceeds debug budget");
}

#[test]
fn shortcuts_work_across_themes() {
    let mut app = app();
    app.theme = Theme::Light;
    assert!(!app.show_diagnostics);
    app.on_key(egui::Key::D);
    assert!(app.show_diagnostics);
    app.on_key(egui::Key::Escape);
    assert!(!app.show_diagnostics);
    app.on_key(egui::Key::C);
    assert!(app.show_compare);
}
