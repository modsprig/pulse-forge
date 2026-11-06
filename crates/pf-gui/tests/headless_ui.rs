//! Headless UI tests (PF-701..703 testability contract): the REAL
//! `PulseForgeApp::update` runs over egui contexts without a window or
//! GPU surface — the same discipline the PF-008 spike validated. Asserted:
//! navigation, virtualized table layout, chart painting with dense series,
//! budgeted pass times, and no panics across view transitions.

// Test loops with bounded counters.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::field_reassign_with_default,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::sync::Arc;

use egui::{Pos2, Rect, Vec2};
use pf_gui::DaemonClient;
use pf_gui::{FakeDaemon, PulseForgeApp, SessionRow, SessionSnapshot, View};
use pf_types::{CounterSnapshot, PfError, SessionId};

fn dense_samples(n: usize) -> Vec<CounterSnapshot> {
    (0..n)
        .map(|i| {
            let i = i as u64;
            CounterSnapshot {
                mono_ns: i * 250_000,
                cpu_user_ns: Some(i * 240_000),
                cpu_kernel_ns: Some(i * 10_000),
                resident_bytes: Some(4_000_000 + (i % 300) * 4096),
                io_read_bytes: Some(i * 4096),
                io_write_bytes: Some(i * 2048),
                thread_count: Some(4),
                ..CounterSnapshot::default()
            }
        })
        .collect()
}

fn app_with(fake: FakeDaemon) -> PulseForgeApp {
    pf_gui::testing::hermetic_data_root();
    PulseForgeApp::new(Arc::new(fake))
}

fn run_frames(app: &mut PulseForgeApp, frames: usize) -> Vec<f64> {
    let ctx = egui::Context::default();
    let mut times = Vec::with_capacity(frames);
    for f in 0..frames {
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1180.0, 780.0)));
        input.time = Some(f as f64 * 0.016);
        let t0 = std::time::Instant::now();
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
fn home_renders_session_list_and_attach_form() {
    let mut fake = FakeDaemon::default();
    fake.sessions = (0..40)
        .map(|i| SessionRow {
            id: SessionId::now_v7(),
            state: if i % 3 == 0 {
                "COMPLETED".into()
            } else {
                "RECORDING".into()
            },
            samples: i,
            image: format!("app{i}.exe"),
            stop_reason: None,
        })
        .collect();
    let mut app = app_with(fake);
    let times = run_frames(&mut app, 20);
    assert_eq!(app.view, View::Home);
    assert_eq!(app.home.sessions.len(), 40);
    // Budget: steady-state home paint far under 25 ms (frame 1 is the
    // one-time egui warm-up, not paint cost).
    let budget = p95(times[1..].to_vec());
    assert!(budget < 25.0, "home p95 {budget:?} exceeds budget");
}

#[test]
fn dashboard_renders_dense_charts_within_budget() {
    let id = SessionId::now_v7();
    let mut fake = FakeDaemon::default();
    fake.samples_result = dense_samples(100_000);
    fake.status_result = Ok(SessionSnapshot {
        id,
        state: "RECORDING".into(),
        samples: 100_000,
        session_dir: None,
        warnings: vec!["ENVIRONMENT: cpu counters read zero here".into()],
    });
    let mut app = app_with(fake);
    app.open_dashboard_for_tests(id);
    let times = run_frames(&mut app, 30);
    // Refreshes now compute off-thread: poll (bounded) until the payload
    // lands instead of assuming it arrived within the frame window.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while app.dashboard.as_ref().is_none_or(|d| d.samples == 0)
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = run_frames(&mut app, 1);
    }
    let dash = app.dashboard.as_ref().expect("dashboard model");
    // Series actually built from the dense data.
    assert_eq!(dash.resident_bytes.points.len(), 100_000);
    assert_eq!(dash.io_bytes.points.len(), 100_000);
    // CPU rate series: one fewer point (first-sample unavailability).
    assert_eq!(dash.cpu_core_pct.points.len(), 99_999);
    // Warnings surface (honesty in the UI).
    assert!(dash.warnings.iter().any(|w| w.contains("ENVIRONMENT")));
    // Budget at the PF-008 scale: pass+tessellation p95 < 25 ms.
    // Debug builds run hotter than the release spike; the enforced bound
    // here is the spike's release result + generous debug margin.
    let budget = p95(times);
    assert!(
        budget < 60.0,
        "dashboard p95 {budget:?} exceeds debug budget"
    );
}

#[test]
fn navigation_home_to_dashboard_and_back() {
    let mut fake = FakeDaemon::default();
    fake.attach_result = Ok(SessionId::now_v7());
    let mut app = app_with(fake);
    run_frames(&mut app, 2);

    // Fill the form via the view model (the UI buttons call exactly this).
    app.home.attach_pid = "4180".into();
    let client = app_client(&app);
    let opened = app.home.submit_attach(client.as_ref());
    let id = opened.expect("attach opens dashboard");
    app.view = View::Dashboard(id);
    app.dashboard = Some(pf_gui::DashboardModel::new(id));
    run_frames(&mut app, 2);
    assert!(matches!(app.view, View::Dashboard(_)));

    // Back to home.
    app.view = View::Home;
    run_frames(&mut app, 2);
    assert_eq!(app.view, View::Home);
}

// Helper: re-borrow the client Arc from the app for view-model calls.
fn app_client(app: &PulseForgeApp) -> Arc<dyn DaemonClient + Send + Sync> {
    app.client_for_tests()
}

#[test]
fn daemon_down_surfaces_status_not_crash() {
    struct DownDaemon;
    impl DaemonClient for DownDaemon {
        fn capabilities(&self) -> Result<pf_types::json::JsonValue, PfError> {
            Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
        }
        fn list_sessions(&self) -> Result<Vec<SessionRow>, PfError> {
            Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
        }
        fn list_processes(&self, _: u32) -> Result<Vec<pf_gui::ProcessRow>, PfError> {
            Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
        }
        fn start_attach(&self, _: u32, _: u64, _: u64, _: &str) -> Result<SessionId, PfError> {
            Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
        }
        fn start_run(
            &self,
            _exe: &str,
            _args: &[String],
            _duration_ms: u64,
            _interval_ms: u64,
            _label: &str,
        ) -> Result<SessionId, PfError> {
            Err(PfError::new(
                pf_types::ErrorCode::InvalidArgument,
                "not wired in this fake",
            ))
        }
        fn status(&self, _: SessionId) -> Result<SessionSnapshot, PfError> {
            Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
        }
        fn stop(&self, _: SessionId) -> Result<String, PfError> {
            Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
        }
        fn delete_session(&self, _: SessionId) -> Result<u64, PfError> {
            Err(pf_types::PfError::new(
                pf_types::ErrorCode::Internal,
                "no deletes in this fake",
            ))
        }

        fn samples(&self, _: SessionId) -> Result<Vec<CounterSnapshot>, PfError> {
            Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
        }
        fn envelope(
            &self,
            _: SessionId,
            _: pf_storage::EnvelopeField,
            _: u64,
            _: u64,
            _: usize,
        ) -> Result<Vec<pf_storage::EnvelopeBucket>, PfError> {
            Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
        }
        fn compare(
            &self,
            _: SessionId,
            _: SessionId,
        ) -> Result<pf_types::json::JsonValue, PfError> {
            Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
        }
        fn storage_health(&self) -> Result<pf_gui::StorageHealth, PfError> {
            Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
        }
    }
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::new(Arc::new(DownDaemon));
    let times = run_frames(&mut app, 10);
    // The honest terminal state is the auto-start outcome (the attempt
    // fails fast: no daemon binary next to the test binary).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !app.home.status_line.contains("auto-start failed")
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = run_frames(&mut app, 1);
    }
    assert!(
        app.home.status_line.contains("auto-start failed"),
        "{}",
        app.home.status_line
    );
    // Attach failure is a visible typed outcome, not a crash.
    app.home.attach_pid = "1".into();
    let client = app_client(&app);
    app.home.submit_attach(client.as_ref());
    assert!(
        matches!(app.home.last_outcome, pf_gui::ActionOutcome::Failed(ref t) if t.contains("TIMEOUT"))
    );
    // The paint budget is STEADY-STATE: frame 1 is the one-time egui
    // warm-up (font atlas + cold page-ins — on WSL/drvfs this alone can
    // read 30-40 ms), which says nothing about paint cost.
    eprintln!("daemon-down frames: {times:?}");
    let budget = p95(times[1..].to_vec());
    assert!(budget < 25.0, "daemon-down budget {budget:?} ms");
}

#[test]
fn diagnostics_drawer_surfaces_storage_and_capability_state() {
    use pf_gui::StorageHealth;
    let mut fake = FakeDaemon::default();
    fake.storage_health = StorageHealth {
        data_root: "C:/diag-test".into(),
        writable: false,
        session_count: 7,
        total_bytes: 123_456,
        counters_available: true,
        counters_reason: String::new(),
        stacks_available: false,
        stacks_reason: "NOT_IMPLEMENTED".into(),
        os_version: "test-os 1.0".into(),
        logical_cpus: Some(8),
    };
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::new(Arc::new(fake));
    // Drawer starts closed; toggling requests an async refresh and opens.
    assert!(!app.show_diagnostics);
    app.toggle_diagnostics();
    assert!(app.show_diagnostics);
    // The payload lands off-thread: poll (bounded) until applied.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while app.diagnostics.health.is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = run_frames(&mut app, 1);
    }
    assert!(app.diagnostics.health.is_some());
    let h = app.diagnostics.health.as_ref().unwrap();
    assert!(!h.writable, "NOT WRITABLE must be surfaced");
    assert_eq!(h.session_count, 7);
    assert!(h.counters_available);
    assert!(!h.stacks_available);
    // Summary line is text-complete (PF-709 non-color cues).
    let line = app.diagnostics.summary_line();
    assert!(line.contains("NOT WRITABLE"), "{line}");
    assert!(line.contains("counters available"));
    assert!(line.contains("stacks unavailable"));
    // Render pass with the drawer open must not panic and stay in budget.
    let times = run_frames(&mut app, 10);
    let budget = p95(times);
    assert!(budget < 25.0, "drawer p95 {budget:?} exceeds budget");
    // Esc closes.
    app.on_key(egui::Key::Escape);
    assert!(!app.show_diagnostics);
}

#[test]
fn diagnostics_drawer_survives_daemon_down() {
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::new(Arc::new(DownDaemon));
    app.toggle_diagnostics();
    assert!(app.show_diagnostics);
    // The typed error lands off-thread: poll (bounded) until applied.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while app.diagnostics.error.is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = run_frames(&mut app, 1);
    }
    // Error state is text-complete, never a crash.
    assert!(app.diagnostics.error.is_some());
    let line = app.diagnostics.summary_line();
    assert!(line.contains("daemon down"), "{line}");
    run_frames(&mut app, 5);
}

// Shared DownDaemon for the drawer test.
struct DownDaemon;
impl DaemonClient for DownDaemon {
    fn capabilities(&self) -> Result<pf_types::json::JsonValue, PfError> {
        Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
    }
    fn list_sessions(&self) -> Result<Vec<SessionRow>, PfError> {
        Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
    }
    fn list_processes(&self, _: u32) -> Result<Vec<pf_gui::ProcessRow>, PfError> {
        Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
    }
    fn start_attach(&self, _: u32, _: u64, _: u64, _: &str) -> Result<SessionId, PfError> {
        Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
    }
    fn start_run(
        &self,
        _exe: &str,
        _args: &[String],
        _duration_ms: u64,
        _interval_ms: u64,
        _label: &str,
    ) -> Result<SessionId, PfError> {
        Err(PfError::new(
            pf_types::ErrorCode::InvalidArgument,
            "not wired in this fake",
        ))
    }
    fn status(&self, _: SessionId) -> Result<SessionSnapshot, PfError> {
        Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
    }
    fn stop(&self, _: SessionId) -> Result<String, PfError> {
        Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
    }
    fn delete_session(&self, _: SessionId) -> Result<u64, PfError> {
        Err(pf_types::PfError::new(
            pf_types::ErrorCode::Internal,
            "no deletes in this fake",
        ))
    }

    fn samples(&self, _: SessionId) -> Result<Vec<CounterSnapshot>, PfError> {
        Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
    }
    fn compare(&self, _: SessionId, _: SessionId) -> Result<pf_types::json::JsonValue, PfError> {
        Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
    }
    fn envelope(
        &self,
        _: SessionId,
        _: pf_storage::EnvelopeField,
        _: u64,
        _: u64,
        _: usize,
    ) -> Result<Vec<pf_storage::EnvelopeBucket>, PfError> {
        Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
    }
    fn storage_health(&self) -> Result<pf_gui::StorageHealth, PfError> {
        Err(PfError::new(pf_types::ErrorCode::Timeout, "daemon down"))
    }
}

#[test]
fn keyboard_shortcuts_toggle_diagnostics() {
    // PF-709: D toggles the drawer, Escape closes it (text cues carry the
    // meaning; color is never the only channel).
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::new(Arc::new(FakeDaemon::default()));
    assert!(!app.show_diagnostics);
    app.on_key(egui::Key::D);
    assert!(app.show_diagnostics);
    app.on_key(egui::Key::D);
    assert!(!app.show_diagnostics);
    // Escape on a closed drawer is a no-op (does not open it).
    app.on_key(egui::Key::Escape);
    assert!(!app.show_diagnostics);
    app.on_key(egui::Key::D);
    app.on_key(egui::Key::Escape);
    assert!(!app.show_diagnostics, "Escape closes an open drawer");
}

#[test]
fn process_picker_lists_filters_and_fills_attach_form() {
    use pf_gui::ProcessRow;
    let mut fake = FakeDaemon::default();
    fake.process_rows = (0..50)
        .map(|i| ProcessRow {
            pid: 1000 + i,
            parent_pid: Some(4),
            image: if i % 2 == 0 {
                format!("app{i}.exe")
            } else {
                format!("svc{i}.exe")
            },
            window_title: None,

            cpu_pct: None,
            rss_bytes: None,
        })
        .collect();
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::new(Arc::new(fake));
    run_frames(&mut app, 5);
    assert_eq!(app.home.processes.len(), 50);

    // Filter by substring.
    let rows = app.home.filter_processes("app");
    assert_eq!(rows.len(), 25);
    let rows = app.home.filter_processes("1005");
    assert_eq!(rows.len(), 1);
    let rows = app.home.filter_processes("zzz");
    assert_eq!(rows.len(), 0);

    // Picking a row fills the attach form (the picker button calls this).
    let picked = app.home.processes[3].clone();
    app.home.pick_process(&picked);
    assert_eq!(app.home.attach_pid, "1003");
    // The form then validates and submits through the same path as before.
    assert!(app.home.validate_attach_form().is_ok());
    run_frames(&mut app, 3);
}

/// Regression for the user-reported "(Not Responding)" hang: a daemon whose
/// every call takes ~120 ms must never block a UI frame — periodic refreshes
/// run on a worker thread and land through the non-blocking apply slot, and
/// the pipe-connect absence budget is short enough that a dead daemon fails
/// fast instead of stalling the message loop for seconds.
struct SlowDaemon(FakeDaemon);
impl DaemonClient for SlowDaemon {
    fn capabilities(&self) -> Result<pf_types::json::JsonValue, PfError> {
        self.0.capabilities()
    }
    fn list_sessions(&self) -> Result<Vec<SessionRow>, PfError> {
        std::thread::sleep(std::time::Duration::from_millis(120));
        self.0.list_sessions()
    }
    fn list_processes(&self, max: u32) -> Result<Vec<pf_gui::ProcessRow>, PfError> {
        std::thread::sleep(std::time::Duration::from_millis(120));
        self.0.list_processes(max)
    }
    fn start_attach(&self, pid: u32, d: u64, i: u64, l: &str) -> Result<SessionId, PfError> {
        std::thread::sleep(std::time::Duration::from_millis(120));
        self.0.start_attach(pid, d, i, l)
    }
    fn start_run(
        &self,
        exe: &str,
        args: &[String],
        duration_ms: u64,
        interval_ms: u64,
        label: &str,
    ) -> Result<SessionId, PfError> {
        self.0.start_run(exe, args, duration_ms, interval_ms, label)
    }
    fn status(&self, id: SessionId) -> Result<SessionSnapshot, PfError> {
        std::thread::sleep(std::time::Duration::from_millis(120));
        self.0.status(id)
    }
    fn stop(&self, id: SessionId) -> Result<String, PfError> {
        std::thread::sleep(std::time::Duration::from_millis(120));
        self.0.stop(id)
    }
    fn delete_session(&self, _: SessionId) -> Result<u64, PfError> {
        Err(pf_types::PfError::new(
            pf_types::ErrorCode::Internal,
            "no deletes in this fake",
        ))
    }

    fn samples(&self, id: SessionId) -> Result<Vec<CounterSnapshot>, PfError> {
        std::thread::sleep(std::time::Duration::from_millis(120));
        self.0.samples(id)
    }
    fn compare(&self, a: SessionId, b: SessionId) -> Result<pf_types::json::JsonValue, PfError> {
        self.0.compare(a, b)
    }
    fn envelope(
        &self,
        id: SessionId,
        field: pf_storage::EnvelopeField,
        from: u64,
        to: u64,
        buckets: usize,
    ) -> Result<Vec<pf_storage::EnvelopeBucket>, PfError> {
        self.0.envelope(id, field, from, to, buckets)
    }
    fn storage_health(&self) -> Result<pf_gui::StorageHealth, PfError> {
        self.0.storage_health()
    }
}

#[test]
fn frames_stay_responsive_while_daemon_slow() {
    let mut fake = FakeDaemon::default();
    fake.sessions = vec![SessionRow {
        id: SessionId::now_v7(),
        state: "RECORDING".into(),
        samples: 1,
        image: "slow.exe".into(),
        stop_reason: None,
    }];
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::new(Arc::new(SlowDaemon(fake)));
    // Six frames while the worker fetches (~240 ms of daemon latency): no
    // frame may block on it.
    let times = run_frames(&mut app, 6);
    for (i, ms) in times.iter().enumerate() {
        assert!(
            *ms < 100.0,
            "frame {i} blocked for {ms} ms on a slow daemon"
        );
    }
    // The worker finishes off-thread; a later frame applies the payload.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = run_frames(&mut app, 1);
    assert_eq!(
        app.home.sessions.len(),
        1,
        "async refresh payload was never applied"
    );
    assert!(
        app.home.status_line.contains("1 session(s)"),
        "{}",
        app.home.status_line
    );
}

#[test]
fn zoom_request_refresh_now_never_blocks() {
    // The zoom/pan buttons used to call a blocking refresh on the UI
    // thread — with a slow daemon every zoom click stalled the frame.
    // The forced async request must return immediately and still land.
    let id = SessionId::now_v7();
    let mut fake = FakeDaemon::default();
    fake.samples_result = dense_samples(1_000);
    fake.status_result = Ok(SessionSnapshot {
        id,
        state: "RECORDING".into(),
        samples: 1_000,
        session_dir: None,
        warnings: vec![],
    });
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::new(Arc::new(SlowDaemon(fake)));
    app.open_dashboard_for_tests(id);
    // Frame 1 requests; the worker (~240 ms of daemon latency) finishes
    // during the sleep; the next frame applies the payload.
    let _ = run_frames(&mut app, 2);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = run_frames(&mut app, 1);
    let client = app_client(&app);
    let t0 = std::time::Instant::now();
    if let Some(d) = app.dashboard.as_mut() {
        d.zoom_by(2.0);
        d.request_refresh_now(client);
    }
    let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
    assert!(elapsed < 100.0, "zoom request blocked for {elapsed} ms");
    // The zoomed payload still lands off-thread.
    std::thread::sleep(std::time::Duration::from_millis(400));
    let _ = run_frames(&mut app, 1);
    let dash = app.dashboard.as_ref().expect("dashboard model");
    assert!(dash.zoomed, "zoom state must hold");
    assert_eq!(dash.samples, 1_000, "refreshed payload must land");
}

#[test]
fn start_click_never_blocks_and_failure_lands() {
    // The Start button used to run its daemon round-trip ON the UI thread
    // (~1.2 s with a dead daemon: create + start). Validation stays sync,
    // the IPC goes off-thread, and the typed failure still lands.
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::new(Arc::new(DownDaemon));
    app.home.attach_pid = "4242".into();
    let client = app_client(&app);
    let t0 = std::time::Instant::now();
    app.home.submit_attach_async(client);
    let dispatch_ms = t0.elapsed().as_secs_f64() * 1000.0;
    assert!(
        dispatch_ms < 100.0,
        "start dispatch blocked for {dispatch_ms} ms"
    );
    // The typed outcome lands off-thread.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while matches!(app.home.last_outcome, pf_gui::ActionOutcome::None)
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = run_frames(&mut app, 1);
    }
    assert!(
        matches!(app.home.last_outcome, pf_gui::ActionOutcome::Failed(ref t) if t.contains("TIMEOUT")),
        "{}",
        app.home.status_line
    );
    assert!(
        app.home.status_line.contains("attach failed"),
        "{}",
        app.home.status_line
    );
}

#[test]
fn successful_start_navigates_when_outcome_lands() {
    // A real start: the outcome arrives a few frames after the click and
    // the app navigates to the new session's dashboard exactly then.
    let mut fake = FakeDaemon::default();
    let id = SessionId::now_v7();
    fake.attach_result = Ok(id);
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::new(Arc::new(fake));
    app.home.attach_pid = "7".into();
    let client = app_client(&app);
    app.home.submit_attach_async(client);
    assert_eq!(app.view, View::Home, "no navigation before the outcome");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while app.view == View::Home && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = run_frames(&mut app, 1);
    }
    assert_eq!(app.view, View::Dashboard(id), "start must navigate");
    assert!(
        matches!(app.home.last_outcome, pf_gui::ActionOutcome::Started(_)),
        "{}",
        app.home.status_line
    );
}

#[test]
fn daemon_down_triggers_autostart_attempt_with_honest_failure() {
    // On-demand daemon: an unreachable daemon must trigger an auto-start
    // attempt OFF the UI thread. Next to the TEST binary there is no
    // `pulseforge` exe, so the attempt fails fast and honestly — the
    // manual instruction stays visible, the frame never blocks.
    pf_gui::testing::hermetic_data_root();
    let mut app = PulseForgeApp::new(Arc::new(DownDaemon));
    let times = run_frames(&mut app, 2);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    // Poll for the TERMINAL state ("auto-start failed"), not the
    // in-flight marker ("auto-starting…" — set before the worker runs).
    while !app.home.status_line.contains("auto-start failed")
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = run_frames(&mut app, 1);
    }
    assert!(
        app.home.status_line.contains("auto-start failed"),
        "{}",
        app.home.status_line
    );
    for (i, ms) in times.iter().enumerate() {
        assert!(*ms < 100.0, "frame {i} blocked for {ms} ms");
    }
    // The honest failure keeps the manual instruction reachable.
    assert!(
        app.home.status_line.contains("pulseforge daemon"),
        "{}",
        app.home.status_line
    );
}

#[test]
fn daemon_down_error_is_not_printed_twice() {
    // Both list calls fail identically when the pipe is gone; the status
    // line used to concatenate the same TIMEOUT text twice. Checked
    // against the pure compute (the UI's terminal status now shows the
    // auto-start outcome instead of this error wall).
    let client: Arc<dyn DaemonClient + Send + Sync> = Arc::new(DownDaemon);
    let refresh = pf_gui::viewmod::HomeModel::compute_home_refresh(client.as_ref());
    let needle = "daemon down";
    let occurrences = refresh.status_line.matches(needle).count();
    assert_eq!(
        occurrences, 1,
        "duplicate error text: {}",
        refresh.status_line
    );
    assert!(refresh.daemon_unreachable, "timeout must flag unreachable");
}

#[test]
fn process_list_beyond_100_is_reachable() {
    // The picker used to request only 100 rows; on a machine with more
    // processes the rest were invisible AND unsearchable (the filter runs
    // over the fetched rows). The limit is now the protocol maximum.
    let mut fake = FakeDaemon::default();
    fake.process_rows = (0..250)
        .map(|i| pf_gui::ProcessRow {
            pid: i,
            parent_pid: None,
            image: format!("proc{i}.exe"),
            window_title: None,

            cpu_pct: None,
            rss_bytes: None,
        })
        .collect();
    let mut app = app_with(fake);
    // Sync refresh path: the payload must carry every row.
    let client = app_client(&app);
    app.home.refresh(client.as_ref());
    assert_eq!(
        app.home.processes.len(),
        250,
        "rows beyond 100 were dropped"
    );
    // The tail is searchable (this failed when only 100 were fetched).
    let tail = app.home.filter_processes("proc240");
    assert_eq!(tail.len(), 1, "tail process unreachable via search");
}

#[test]
fn delete_flow_confirms_dispatches_and_lands_typed_outcome() {
    // Destructive delete: the card button only ARMS the confirmation
    // modal; the modal's confirm dispatches off the UI thread; the typed
    // outcome (bytes freed) lands through the per-frame poll.
    pf_gui::testing::hermetic_data_root();
    let mut fake = FakeDaemon::default();
    let sid = SessionId::now_v7();
    fake.sessions = vec![pf_gui::SessionRow {
        id: sid,
        state: "COMPLETED".into(),
        samples: 12,
        image: "junk.exe".into(),
        stop_reason: None,
    }];
    let mut app = PulseForgeApp::new(Arc::new(fake));
    run_frames(&mut app, 2);

    // Arming: the modal is armed, nothing dispatched yet.
    app.pending_delete = Some((sid, "junk.exe".into()));
    assert_eq!(app.pending_delete, Some((sid, "junk.exe".into())));
    assert_eq!(app.home.deleting, None, "no delete in flight yet");

    // Confirm — the modal dispatches, then disarms itself (the same two
    // steps render_delete_confirm performs on click).
    app.home.request_delete_async(app.client_for_tests(), sid);
    app.pending_delete = None;
    assert_eq!(app.pending_delete, None);
    let mut deleted = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while deleted.is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
        deleted = app.home.apply_delete_result();
    }
    let freed = deleted
        .expect("delete outcome must land")
        .expect("delete ok");
    assert!(freed > 0, "fake reports freed bytes: {freed}");
    assert_eq!(app.home.deleting, None, "in-flight marker cleared");
}

#[test]
fn esc_cancels_delete_confirmation_without_deleting() {
    pf_gui::testing::hermetic_data_root();
    let fake = FakeDaemon::default();
    let sid = SessionId::now_v7();
    let mut app = PulseForgeApp::new(Arc::new(fake));
    app.pending_delete = Some((sid, "doomed.exe".into()));
    app.on_key(egui::Key::Escape);
    assert_eq!(app.pending_delete, None, "Esc disarms the modal");
    assert_eq!(app.home.deleting, None, "nothing was dispatched");
}

#[test]
fn dropping_a_session_dir_opens_it_offline() {
    pf_gui::testing::hermetic_data_root();
    // Minimal session dir: manifest only — the offline dashboard renders
    // honestly empty series when no chunks exist.
    let dir = std::env::temp_dir().join(format!(
        "pf-drop-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("manifest.json"),
        r#"{"schema":"pulseforge.session/0","session_id":"00000000-0000-7000-8000-0000000000aa","state":"COMPLETED","target":{"os":"linux","pid":4242,"creation_identity":1,"boot_identity":"b","image":"dropped.bin"},"stop_reason":"duration","warnings":[]}"#,
    )
    .unwrap();

    let fake = FakeDaemon::default();
    let mut app = PulseForgeApp::new(Arc::new(fake));
    run_frames(&mut app, 1);
    assert_eq!(app.view, View::Home);

    // One frame carrying the drop (a file INSIDE the dir also works).
    let ctx = egui::Context::default();
    let mut input = egui::RawInput::default();
    input.screen_rect = Some(egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::Vec2::new(1180.0, 780.0),
    ));
    input.dropped_files = vec![egui::DroppedFile {
        path: Some(dir.join("manifest.json")),
        name: "manifest.json".into(),
        ..Default::default()
    }];
    let _ = ctx.run(input, |ctx| app.update(ctx));

    let expected = pf_types::SessionId::parse("00000000-0000-7000-8000-0000000000aa")
        .expect("fixture id parses");
    assert_eq!(app.view, View::Dashboard(expected), "drop must navigate");
    assert!(
        app.window_title().contains("OFFLINE") && app.window_title().contains("00000000"),
        "offline dashboard reflected in the title: {}",
        app.window_title()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
