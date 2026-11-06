//! The egui application layer (roadmap PF-701/702/703): thin widgets over
//! the view models. Dense data is drawn as single polyline shapes and the
//! session table is virtualized (`ScrollArea::show_rows`) — the techniques
//! the PF-008 spike measured at 100k density.
//!
//! Testability: `update()` runs headlessly in the test suite over a fake
//! client; the same code opens the real window in the binary.

use std::sync::Arc;

use egui::{Color32, Pos2, Stroke, Vec2};

use pf_types::json::JsonValue;
use pf_types::{ErrorCode, PfError, SessionId};

use crate::viewmod::{
    DaemonClient, DashboardModel, DiagnosticsModel, HomeModel, ProcessFilterTab, ProcessRow,
    ProcessViewMode, Series, SessionRow, SessionSnapshot, View,
};

/// Semantic UI colors derived from the theme — replaces the hardcoded
/// dark values the widgets used to embed (which broke Light mode).
#[derive(Clone, Copy)]
pub struct Palette {
    pub card_fill: egui::Color32,
    pub surface_elevated: egui::Color32,
    pub card_stroke: egui::Color32,
    pub inset_fill: egui::Color32,
    pub text_strong: egui::Color32,
    pub text_muted: egui::Color32,
    pub accent: egui::Color32,
    pub grid_line: egui::Color32,
    pub success: egui::Color32,
    pub warning: egui::Color32,
    pub danger: egui::Color32,
}

/// Workspace layout mode for the session dashboard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DashboardLayout {
    /// Balanced overview: full-width CPU chart, side-by-side Memory & I/O, and Flamegraph below.
    #[default]
    SplitOverview,
    /// Full-width stacked high-resolution timeline charts.
    TimelineFocus,
    /// Maximized CPU Stacks & Flamegraph / Hotspots workspace.
    StacksFocus,
}

/// Severity of a transient toast. Color is never the only channel — the
/// leading dot pulses green for success and solid red for errors, and the
/// message text itself states the outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Success,
    Error,
}

pub struct PulseForgeApp {
    pub view: View,
    pub home: HomeModel,
    pub dashboard: Option<DashboardModel>,
    pub dashboard_layout: DashboardLayout,
    pub diagnostics: DiagnosticsModel,
    pub show_diagnostics: bool,
    pub compare: crate::viewmod::CompareModel,
    pub show_compare: bool,
    /// Keyboard shortcuts help cheat sheet modal.
    pub show_help: bool,
    /// Destructive delete awaiting confirmation: (session id, image name).
    pub pending_delete: Option<(SessionId, String)>,
    /// Transient user-facing feedback notification pill.
    pub toast: Option<(String, std::time::Instant, ToastKind)>,
    /// PF-906: CPU-stacks flamegraph panel for the open session.
    pub stacks: crate::flame::StacksModel,
    pub process_search: String,
    /// Cached native application textures and procedural fallback icons.
    pub icon_cache: crate::app_icon::ProcessIconCache,
    /// PF-709: UI theme *preference* (persisted to the data root across
    /// sessions); `active_theme` is the per-frame resolution of `System`
    /// against the OS dark/light setting.
    pub theme: Theme,
    /// Resolved theme actually painted this frame (`System` never paints
    /// as itself — it renders as the OS preference, defaulting to dark).
    pub active_theme: Theme,
    pub search: String,
    client: Arc<dyn DaemonClient + Send + Sync>,
}

// Offline client: reads the store on disk; writes are impossible
// (finalized sessions are immutable).
struct OfflineClient(std::path::PathBuf);
impl DaemonClient for OfflineClient {
    fn capabilities(&self) -> Result<JsonValue, PfError> {
        Err(PfError::new(
            ErrorCode::UnsupportedPlatform,
            "offline session: no daemon connection",
        ))
    }
    fn list_sessions(&self) -> Result<Vec<SessionRow>, PfError> {
        Ok(Vec::new())
    }
    fn list_processes(&self, _limit: u32) -> Result<Vec<crate::viewmod::ProcessRow>, PfError> {
        Err(PfError::new(
            ErrorCode::UnsupportedPlatform,
            "offline session: no daemon connection",
        ))
    }
    fn start_attach(&self, _: u32, _: u64, _: u64, _: &str) -> Result<SessionId, PfError> {
        Err(PfError::new(
            ErrorCode::UnsupportedPlatform,
            "offline session: starting recordings needs the daemon",
        ))
    }
    fn start_run(
        &self,
        _: &str,
        _: &[String],
        _: u64,
        _: u64,
        _: &str,
    ) -> Result<SessionId, PfError> {
        Err(PfError::new(
            ErrorCode::UnsupportedPlatform,
            "offline session: launching workloads needs the daemon",
        ))
    }
    fn status(&self, id: SessionId) -> Result<SessionSnapshot, PfError> {
        Ok(SessionSnapshot {
            id,
            state: "OFFLINE".into(),
            samples: 0,
            session_dir: Some(self.0.clone()),
            warnings: vec![],
        })
    }
    fn stop(&self, _: SessionId) -> Result<String, PfError> {
        Err(PfError::new(
            ErrorCode::UnsupportedPlatform,
            "offline session is immutable",
        ))
    }
    fn delete_session(&self, _: SessionId) -> Result<u64, PfError> {
        Err(PfError::new(
            ErrorCode::UnsupportedPlatform,
            "offline session: deleting needs the daemon",
        ))
    }
    fn samples(&self, _id: SessionId) -> Result<Vec<pf_types::CounterSnapshot>, PfError> {
        let reader = pf_storage::ChunkReader::open(&self.0)?;
        if reader.total_records() > 0 {
            return reader.range(0, u64::MAX, 100_000).map(|p| p.snapshots);
        }
        let store = pf_platform::session_store::SessionStore::open(&self.0)?;
        store.read_samples().map(|(s, _)| s)
    }
    fn envelope(
        &self,
        _id: SessionId,
        field: pf_storage::EnvelopeField,
        from_ns: u64,
        to_ns: u64,
        buckets: usize,
    ) -> Result<Vec<pf_storage::EnvelopeBucket>, PfError> {
        let reader = pf_storage::ChunkReader::open(&self.0)?;
        reader.envelope(from_ns, to_ns, buckets, field)
    }
    fn compare(&self, _: SessionId, _: SessionId) -> Result<JsonValue, PfError> {
        Err(PfError::new(
            ErrorCode::UnsupportedPlatform,
            "offline session: comparison needs the daemon",
        ))
    }
    fn storage_health(&self) -> Result<crate::viewmod::StorageHealth, PfError> {
        Ok(crate::viewmod::StorageHealth {
            data_root: self.0.display().to_string(),
            writable: true,
            session_count: 0,
            total_bytes: 0,
            counters_available: false,
            counters_reason: "OFFLINE".into(),
            stacks_available: false,
            stacks_reason: "OFFLINE".into(),
            os_version: String::new(),
            logical_cpus: None,
        })
    }
}

/// PF-708 deep link: build a dashboard purely from a recorded session
/// directory (chunk store / schema-0 JSONL) — no daemon connection. The
/// returned model refreshes from the store again on demand (the refresh
/// cadence naturally re-reads finalized files).
pub fn open_session_offline(dir: &std::path::Path) -> Result<DashboardModel, PfError> {
    let manifest_path = dir.join("manifest.json");
    let text = std::fs::read_to_string(&manifest_path).map_err(|e| {
        PfError::new(
            ErrorCode::SessionNotFound,
            format!("manifest unreadable at {}: {e}", manifest_path.display()),
        )
    })?;
    if text.len() > 4 * 1024 * 1024 {
        return Err(PfError::new(
            ErrorCode::ResourceExhausted,
            "manifest exceeds 4 MiB safety cap",
        ));
    }
    let manifest = pf_types::json::parse(&text).map_err(PfError::from)?;
    let id = SessionId::parse(
        manifest
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| PfError::new(ErrorCode::InvalidArgument, "manifest lacks session_id"))?,
    )
    .map_err(|e| PfError::new(ErrorCode::InvalidArgument, format!("bad session id: {e}")))?;

    let mut model = DashboardModel::new(id);
    model.refresh(&OfflineClient(dir.to_path_buf()));
    Ok(model)
}

impl PulseForgeApp {
    /// Offline constructor (PF-708): no daemon client; the dashboard
    /// model was already populated from the chunk store.
    pub fn offline(model: DashboardModel) -> PulseForgeApp {
        let id = model.id;
        let view = View::Dashboard(id);
        let client: Arc<dyn DaemonClient + Send + Sync> = match model.session_dir.clone() {
            Some(dir) => Arc::new(OfflineClient(dir)),
            None => Arc::new(crate::viewmod::FakeDaemon::default()),
        };
        PulseForgeApp {
            view,
            home: HomeModel::default(),
            dashboard: Some(model),
            dashboard_layout: DashboardLayout::default(),
            diagnostics: DiagnosticsModel::default(),
            show_diagnostics: false,
            compare: crate::viewmod::CompareModel::default(),
            show_compare: false,
            show_help: false,
            pending_delete: None,
            toast: None,
            stacks: crate::flame::StacksModel::default(),
            process_search: String::new(),
            icon_cache: crate::app_icon::ProcessIconCache::default(),
            theme: load_theme_preference().unwrap_or_default(),
            active_theme: Theme::default(),
            search: String::new(),
            client,
        }
    }

    /// PF-708 drag-and-drop: open a session directory (or a file inside
    /// one) offline — no daemon, the chunk store only. Replaces the
    /// current view; a failure surfaces as an error toast, never a
    /// navigation.
    pub fn open_offline_in_place(&mut self, dropped: &std::path::Path) {
        // Accept either the session dir itself or a file inside it.
        let dir = if dropped.join("manifest.json").is_file() {
            dropped.to_path_buf()
        } else {
            match dropped.parent() {
                Some(parent) if parent.join("manifest.json").is_file() => parent.to_path_buf(),
                _ => {
                    self.set_toast_error(format!(
                        "Not a PulseForge session (no manifest.json): {}",
                        dropped.display()
                    ));
                    return;
                }
            }
        };
        match open_session_offline(&dir) {
            Ok(model) => {
                self.client = Arc::new(OfflineClient(dir));
                let id = model.id;
                self.dashboard = Some(model);
                self.view = View::Dashboard(id);
                let short = &id.as_hyphenated()[..8];
                self.set_toast(format!("Opened session offline: {short}"));
            }
            Err(e) => {
                self.set_toast_error(format!("Open failed: {e}"));
            }
        }
    }

    pub fn new(client: Arc<dyn DaemonClient + Send + Sync>) -> PulseForgeApp {
        PulseForgeApp {
            view: View::Home,
            home: HomeModel::default(),
            dashboard: None,
            dashboard_layout: DashboardLayout::default(),
            diagnostics: DiagnosticsModel::default(),
            show_diagnostics: false,
            compare: crate::viewmod::CompareModel::default(),
            show_compare: false,
            show_help: false,
            pending_delete: None,
            toast: None,
            stacks: crate::flame::StacksModel::default(),
            process_search: String::new(),
            icon_cache: crate::app_icon::ProcessIconCache::default(),
            theme: load_theme_preference().unwrap_or_default(),
            active_theme: Theme::default(),
            search: String::new(),
            client,
        }
    }

    /// Surface a non-intrusive feedback toast in the corner of the window.
    pub fn set_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), std::time::Instant::now(), ToastKind::Success));
    }

    /// Same pill in the error variant (solid red dot, danger stroke) —
    /// failures must not look like success.
    pub fn set_toast_error(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), std::time::Instant::now(), ToastKind::Error));
    }

    /// PF-707: toggle the diagnostics drawer (refreshes on open).
    pub fn toggle_diagnostics(&mut self) {
        self.show_diagnostics = !self.show_diagnostics;
        if self.show_diagnostics {
            self.diagnostics
                .request_refresh_now(std::sync::Arc::clone(&self.client));
        }
    }

    /// PF-1106: toggle the compare panel (refreshes the session list).
    pub fn toggle_compare(&mut self) {
        self.show_compare = !self.show_compare;
        if self.show_compare {
            self.home
                .request_refresh_now(std::sync::Arc::clone(&self.client));
        }
    }

    /// Cycle the theme *preference* and persist it to the data root so the
    /// choice survives restarts (a failed write only skips persistence —
    /// the in-session change always applies).
    pub fn cycle_theme(&mut self) {
        self.theme.cycle();
        save_theme_preference(self.theme);
    }

    /// PF-709 keyboard shortcuts (Esc closes overlays, C toggles compare,
    /// D toggles diagnostics, ? toggles help modal, R refreshes).
    pub fn on_key(&mut self, key: egui::Key) {
        match key {
            egui::Key::Escape if self.pending_delete.is_some() => self.pending_delete = None,
            egui::Key::Escape if self.show_help => self.show_help = false,
            egui::Key::Escape if self.show_diagnostics => self.show_diagnostics = false,
            egui::Key::Escape if self.show_compare => self.show_compare = false,
            egui::Key::Escape if matches!(self.view, View::Dashboard(_)) => self.view = View::Home,
            egui::Key::Questionmark if ui_has_no_text_focus() => self.show_help = !self.show_help,
            egui::Key::D if ui_has_no_text_focus() => self.toggle_diagnostics(),
            egui::Key::C if ui_has_no_text_focus() => self.toggle_compare(),
            egui::Key::R if ui_has_no_text_focus() => {
                let client = std::sync::Arc::clone(&self.client);
                match self.view {
                    View::Home => self.home.request_refresh_now(client),
                    View::Dashboard(_) => {
                        if let Some(d) = self.dashboard.as_mut() {
                            d.request_refresh_now(client);
                        }
                    }
                }
            }
            egui::Key::Plus | egui::Key::Equals if ui_has_no_text_focus() => {
                if let Some(d) = self.dashboard.as_mut() {
                    d.zoom_by(2.0);
                    d.request_refresh_now(std::sync::Arc::clone(&self.client));
                }
            }
            egui::Key::Minus if ui_has_no_text_focus() => {
                if let Some(d) = self.dashboard.as_mut() {
                    d.zoom_by(0.5);
                    d.request_refresh_now(std::sync::Arc::clone(&self.client));
                }
            }
            egui::Key::ArrowLeft if ui_has_no_text_focus() => {
                if let Some(d) = self.dashboard.as_mut() {
                    d.pan_by(-0.25);
                    d.request_refresh_now(std::sync::Arc::clone(&self.client));
                }
            }
            egui::Key::ArrowRight if ui_has_no_text_focus() => {
                if let Some(d) = self.dashboard.as_mut() {
                    d.pan_by(0.25);
                    d.request_refresh_now(std::sync::Arc::clone(&self.client));
                }
            }
            egui::Key::Num0 if ui_has_no_text_focus() => {
                if let Some(d) = self.dashboard.as_mut() {
                    d.reset_zoom();
                    d.request_refresh_now(std::sync::Arc::clone(&self.client));
                }
            }
            egui::Key::Space if ui_has_no_text_focus() => {
                if let Some(d) = self.dashboard.as_mut() {
                    d.stop_async(std::sync::Arc::clone(&self.client));
                }
            }
            _ => {}
        }
    }

    /// Test access to the shared client (used by headless tests to drive
    /// view-model calls with the same instance the UI uses).
    pub fn client_for_tests(&self) -> std::sync::Arc<dyn DaemonClient + Send + Sync> {
        std::sync::Arc::clone(&self.client)
    }

    /// Window title reflecting the live state (version + what is open).
    /// The shell only sends a title change when this string changes, so it
    /// is safe to consult every frame.
    pub fn window_title(&self) -> String {
        const VERSION: &str = env!("CARGO_PKG_VERSION");
        match &self.view {
            View::Home => format!("PulseForge v{VERSION}"),
            View::Dashboard(id) => {
                let short = &id.as_hyphenated().to_string()[..8];
                match self.dashboard.as_ref() {
                    Some(dash) if dash.state == "RECORDING" || dash.state == "STARTING" => {
                        format!("PulseForge v{VERSION} — ● Recording (session {short})")
                    }
                    Some(dash) if !dash.state.is_empty() => {
                        format!("PulseForge v{VERSION} — {} (session {short})", dash.state)
                    }
                    _ => format!("PulseForge v{VERSION} — session {short}"),
                }
            }
        }
    }

    /// Test navigation helper: same transition the open button performs.
    pub fn open_dashboard_for_tests(&mut self, id: SessionId) {
        self.open_dashboard(id)
    }

    fn open_dashboard(&mut self, id: SessionId) {
        self.dashboard = Some(DashboardModel::new(id));
        self.view = View::Dashboard(id);
    }

    /// Render the global application header bar (shared across Home & Dashboard).
    #[allow(clippy::arithmetic_side_effects)]
    fn render_global_header(&mut self, ui: &mut egui::Ui) {
        let pal = self.active_theme.palette();
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("⚡ PulseForge")
                    .size(16.5)
                    .strong()
                    .color(pal.accent),
            );
            ui.label(
                egui::RichText::new("PRO")
                    .size(9.5)
                    .strong()
                    .color(pal.text_muted),
            );
            ui.separator();

            // Daemon health pill
            let (dot_col, dot_lbl, glowing) = if self.home.last_refresh_unreachable {
                (pal.danger, "Daemon Offline", false)
            } else if self.home.status_line.contains("auto-starting") {
                (pal.warning, "Starting…", true)
            } else {
                (pal.success, "Daemon Online", true)
            };
            paint_status_dot(ui, dot_col, glowing);
            ui.label(
                egui::RichText::new(dot_lbl)
                    .size(11.0)
                    .strong()
                    .color(dot_col),
            );
            if !self.home.status_line.is_empty() {
                ui.weak(egui::RichText::new(format!("· {}", self.home.status_line)).size(11.0));
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .selectable_label(self.show_help, "? Shortcuts")
                    .on_hover_text("Keyboard shortcuts cheat sheet [?]")
                    .clicked()
                {
                    self.show_help = !self.show_help;
                }
                if ui
                    .small_button(
                        egui::RichText::new(format!("Theme: {}", self.theme.as_str())).size(11.0),
                    )
                    .on_hover_text("Cycle color theme (Dark / Light / System)")
                    .clicked()
                {
                    self.cycle_theme();
                }
                if ui
                    .selectable_label(self.show_diagnostics, "Diagnostics")
                    .on_hover_text("Toggle storage & platform diagnostics drawer [D]")
                    .clicked()
                {
                    self.toggle_diagnostics();
                }
                if ui
                    .selectable_label(self.show_compare, "⚖ Compare")
                    .on_hover_text("Compare two sessions side-by-side [C]")
                    .clicked()
                {
                    self.toggle_compare();
                }
            });
        });
    }

    /// One UI frame. Runs identically headless (tests) and windowed.
    #[allow(clippy::arithmetic_side_effects)]
    pub fn update(&mut self, ctx: &egui::Context) {
        self.active_theme = self.theme.resolve(ctx);
        self.active_theme.apply(ctx);
        // PF-709: re-evaluate the keyboard guard every frame — a TextEdit
        // (or any keyboard-interested widget) owning focus must disable
        // the single-key shortcuts for that frame.
        TEXT_FOCUS_OK.store(
            !ctx.wants_keyboard_input(),
            std::sync::atomic::Ordering::Relaxed,
        );
        // PF-708: drag-and-drop — dropping a session directory (or any file
        // inside one) opens it as an offline dashboard.
        let dropped_paths: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        for path in dropped_paths {
            self.open_offline_in_place(&path);
        }
        // PF-709: keyboard shortcuts (documented in drawer footer & help modal).
        ctx.input(|i| {
            for key in [
                &egui::Key::Escape,
                &egui::Key::D,
                &egui::Key::C,
                &egui::Key::Questionmark,
                &egui::Key::R,
                &egui::Key::Plus,
                &egui::Key::Equals,
                &egui::Key::Minus,
                &egui::Key::ArrowLeft,
                &egui::Key::ArrowRight,
                &egui::Key::Num0,
                &egui::Key::Space,
            ] {
                if i.key_pressed(*key) {
                    self.on_key(*key);
                }
            }
        });
        if self.show_diagnostics {
            self.diagnostics.request_refresh_if_due(
                std::sync::Arc::clone(&self.client),
                std::time::Instant::now(),
            );
            self.diagnostics.apply_refresh();
        }
        if self.show_compare {
            self.compare.apply_result();
        }
        if let Some(outcome) = self.home.apply_delete_result() {
            match outcome {
                Ok(bytes) => {
                    self.set_toast(format!(
                        "Recording deleted — freed {}",
                        fmt_value(bytes as f64)
                    ));
                }
                Err(e) => {
                    self.set_toast_error(format!("Delete failed: {e}"));
                }
            }
        }
        let pal = self.active_theme.palette();
        match self.view.clone() {
            View::Home => {
                self.home.request_refresh_if_due(
                    std::sync::Arc::clone(&self.client),
                    std::time::Instant::now(),
                );
                self.home.apply_refresh();
                if self.home.last_refresh_unreachable {
                    self.home
                        .maybe_autostart_daemon(std::sync::Arc::clone(&self.client));
                }
                self.home
                    .apply_autostart(std::sync::Arc::clone(&self.client));
                if let Some(id) = self.home.apply_outcome() {
                    self.open_dashboard(id);
                }
                egui::TopBottomPanel::top("top")
                    .frame(
                        egui::Frame::default()
                            .fill(pal.card_fill)
                            .stroke(egui::Stroke::new(1.0_f32, pal.card_stroke))
                            .inner_margin(egui::Margin::symmetric(14.0, 6.0)),
                    )
                    .show(ctx, |ui| {
                        self.render_global_header(ui);
                    });
                if self.show_diagnostics {
                    self.diagnostics_drawer(ctx);
                }
                if self.show_compare {
                    self.compare_panel(ctx);
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    self.home_ui(ui);
                });
            }
            View::Dashboard(id) => {
                if let Some(dash) = self.dashboard.as_mut() {
                    dash.request_refresh_if_due(
                        std::sync::Arc::clone(&self.client),
                        std::time::Instant::now(),
                    );
                    dash.apply_refresh();
                    dash.apply_stop();
                }
                egui::TopBottomPanel::top("top")
                    .frame(
                        egui::Frame::default()
                            .fill(pal.card_fill)
                            .stroke(egui::Stroke::new(1.0_f32, pal.card_stroke))
                            .inner_margin(egui::Margin::symmetric(14.0, 5.0)),
                    )
                    .show(ctx, |ui| {
                        // Row 1: Breadcrumb navigation + session metadata + global actions
                        ui.horizontal(|ui| {
                            if ui
                                .button(egui::RichText::new("◀ Sessions").strong().size(11.5))
                                .on_hover_text("Return to Sessions workspace [Esc]")
                                .clicked()
                            {
                                self.view = View::Home;
                                return;
                            }
                            ui.separator();
                            ui.label(
                                egui::RichText::new("⚡ PulseForge")
                                    .size(14.0)
                                    .strong()
                                    .color(pal.accent),
                            );
                            ui.weak("/");
                            let full_id = id.as_hyphenated().to_string();
                            ui.strong(
                                egui::RichText::new(format!(
                                    "Session {}",
                                    &full_id[..full_id.len().min(13)]
                                ))
                                .size(13.5),
                            );
                            if ui
                                .small_button("📋 Copy ID")
                                .on_hover_text("Copy full session UUID to clipboard")
                                .clicked()
                            {
                                ui.output_mut(|o| o.copied_text = full_id.clone());
                                self.set_toast("Copied full session ID to clipboard");
                            }
                            let dash_dir = self
                                .dashboard
                                .as_ref()
                                .and_then(|d| d.session_dir.clone());
                            if let Some(dir) = &dash_dir {
                                if ui
                                    .small_button("📂 Folder")
                                    .on_hover_text(
                                        "Reveal this session's folder in the file manager",
                                    )
                                    .clicked()
                                {
                                    match reveal_folder(dir) {
                                        Ok(()) => {
                                            self.set_toast(format!("Opened {}", dir.display()));
                                        }
                                        Err(e) => {
                                            self.set_toast_error(format!(
                                                "Open folder failed: {e}"
                                            ));
                                        }
                                    }
                                }
                            }
                            if let Some(dash) = self.dashboard.as_ref() {
                                ui.separator();
                                let (dot, st_lbl) = state_color(&dash.state);
                                let is_live =
                                    dash.state == "RECORDING" || dash.state == "STARTING";
                                paint_status_dot(ui, dot, is_live);
                                ui.label(
                                    egui::RichText::new(st_lbl.to_uppercase())
                                        .size(11.0)
                                        .color(dot)
                                        .strong(),
                                );
                                ui.weak(
                                    egui::RichText::new(format!(
                                        "· {} samples",
                                        fmt_int(dash.samples)
                                    ))
                                    .size(11.0),
                                );
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .selectable_label(self.show_help, "? Help")
                                        .on_hover_text("Keyboard shortcuts [?]")
                                        .clicked()
                                    {
                                        self.show_help = !self.show_help;
                                    }
                                    if ui
                                        .small_button(format!("Theme: {}", self.theme.as_str()))
                                        .clicked()
                                    {
                                        self.cycle_theme();
                                    }
                                    if ui
                                        .selectable_label(self.show_diagnostics, "Diagnostics")
                                        .clicked()
                                    {
                                        self.toggle_diagnostics();
                                    }
                                    let dash_info = self.dashboard.as_ref().map(|d| {
                                        (
                                            d.session_dir.clone(),
                                            d.state == "RECORDING" || d.state == "STARTING",
                                            d.stop_status.clone(),
                                        )
                                    });
                                    if let Some((session_dir, is_active, stop_status)) = dash_info {
                                        if let Some(dir) = &session_dir {
                                            if ui
                                                .small_button("📤 Export Speedscope")
                                                .on_hover_text(
                                                    "Export Speedscope JSON to session folder & clipboard",
                                                )
                                                .clicked()
                                            {
                                                let profile_name = format!(
                                                    "{} profile",
                                                    &full_id[..full_id.len().min(8)]
                                                );
                                                match pf_analysis::export::speedscope_export(
                                                    dir,
                                                    &profile_name,
                                                    concat!("pulseforge@", env!("CARGO_PKG_VERSION")),
                                                ) {
                                                    Ok(rep) => {
                                                        let out_path = dir.join("speedscope.json");
                                                        let json_str = rep.doc.to_json_string();
                                                        let _ = std::fs::write(&out_path, &json_str);
                                                        ui.output_mut(|o| o.copied_text = json_str);
                                                        self.set_toast(format!(
                                                            "Exported {} samples to speedscope.json & clipboard!",
                                                            rep.samples_included
                                                        ));
                                                    }
                                                    Err(e) => {
                                                        self.set_toast_error(format!(
                                                            "Export error: {e}"
                                                        ));
                                                    }
                                                }
                                            }
                                            if is_active
                                                && ui
                                                    .small_button("📤 Export pprof")
                                                    .on_hover_text(
                                                        "Export pprof profile.pb (validated protobuf) to the session folder",
                                                    )
                                                    .clicked()
                                                {
                                                    match pf_analysis::export::pprof_export(dir) {
                                                        Ok(rep) => {
                                                            // Same honesty contract as the
                                                            // CLI: never hand out a
                                                            // document our own decoder
                                                            // cannot read back.
                                                            match pf_analysis::export::validate_pprof_document(&rep.doc) {
                                                                Ok(_) => {
                                                                    let out_path = dir.join("profile.pb");
                                                                    match std::fs::write(&out_path, &rep.doc) {
                                                                        Ok(()) => {
                                                                            let trunc = if rep.truncated {
                                                                                format!(" (truncated from {})", rep.samples_total)
                                                                            } else {
                                                                                String::new()
                                                                            };
                                                                            self.set_toast(format!(
                                                                                "Exported {} samples to profile.pb{}",
                                                                                rep.samples_included, trunc
                                                                            ));
                                                                        }
                                                                        Err(e) => {
                                                                            self.set_toast_error(format!("Write failed: {e}"));
                                                                        }
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    self.set_toast_error(format!("Export rejected by validator: {e}"));
                                                                }
                                                            }
                                                        }
                                                        Err(e) => {
                                                            self.set_toast_error(format!("Export error: {e}"));
                                                        }
                                                    }
                                                }
                                        }
                                        if is_active
                                            && ui
                                                .button(
                                                    egui::RichText::new("■ Stop Recording")
                                                        .color(pal.danger)
                                                        .strong(),
                                                )
                                                .on_hover_text("Stop active recording [Space]")
                                                .clicked()
                                        {
                                            if let Some(dash) = self.dashboard.as_mut() {
                                                dash.stop_async(std::sync::Arc::clone(
                                                    &self.client,
                                                ));
                                            }
                                        }
                                        if let Some(s) = &stop_status {
                                            ui.weak(format!("stop: {s}"));
                                        }
                                    }
                                },
                            );
                        });

                        // Row 2: Timeline zoom/pan controls + Workspace layout mode switcher
                        ui.horizontal(|ui| {
                            ui.weak(egui::RichText::new("Timeline:").size(11.0));
                            let mut viewport_changed = false;
                            if ui
                                .small_button("+ Zoom In")
                                .on_hover_text("Zoom into timeline [+]")
                                .clicked()
                            {
                                if let Some(d) = self.dashboard.as_mut() {
                                    d.zoom_by(2.0);
                                    viewport_changed = true;
                                }
                            }
                            if ui
                                .small_button("− Zoom Out")
                                .on_hover_text("Zoom out of timeline [-]")
                                .clicked()
                            {
                                if let Some(d) = self.dashboard.as_mut() {
                                    d.zoom_by(0.5);
                                    viewport_changed = true;
                                }
                            }
                            if ui
                                .small_button("◀ Pan")
                                .on_hover_text("Pan left [←]")
                                .clicked()
                            {
                                if let Some(d) = self.dashboard.as_mut() {
                                    d.pan_by(-0.25);
                                    viewport_changed = true;
                                }
                            }
                            if ui
                                .small_button("Pan ▶")
                                .on_hover_text("Pan right [→]")
                                .clicked()
                            {
                                if let Some(d) = self.dashboard.as_mut() {
                                    d.pan_by(0.25);
                                    viewport_changed = true;
                                }
                            }
                            if ui
                                .small_button("↻ Reset")
                                .on_hover_text("Reset timeline zoom [0]")
                                .clicked()
                            {
                                if let Some(d) = self.dashboard.as_mut() {
                                    d.reset_zoom();
                                    viewport_changed = true;
                                }
                            }
                            if viewport_changed {
                                let client = std::sync::Arc::clone(&self.client);
                                if let Some(d) = self.dashboard.as_mut() {
                                    d.request_refresh_now(client);
                                }
                            }
                            if let Some(d) = self.dashboard.as_ref() {
                                if d.zoomed {
                                    let (a, b) = d.effective_view();
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Window: {}–{} ms",
                                            a / 1_000_000,
                                            b / 1_000_000
                                        ))
                                        .size(11.0)
                                        .color(pal.accent)
                                        .strong(),
                                    );
                                }
                            }

                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    for (lbl, mode) in [
                                        ("🔥 Stacks Focus", DashboardLayout::StacksFocus),
                                        ("📈 Timeline Focus", DashboardLayout::TimelineFocus),
                                        ("Split Overview", DashboardLayout::SplitOverview),
                                    ] {
                                        if ui
                                            .selectable_label(self.dashboard_layout == mode, lbl)
                                            .clicked()
                                        {
                                            self.dashboard_layout = mode;
                                        }
                                    }
                                    ui.weak(egui::RichText::new("Layout:").size(11.0));
                                },
                            );
                        });
                    });
                if self.show_diagnostics {
                    self.diagnostics_drawer(ctx);
                }
                if self.show_compare {
                    self.compare_panel(ctx);
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    self.dashboard_ui(ui);
                });
            }
        }
        if self.show_help {
            self.shortcuts_modal(ctx);
        }
        if self.pending_delete.is_some() {
            self.render_delete_confirm(ctx);
        }
        self.render_toast(ctx);
    }

    /// Render keyboard shortcuts help cheat sheet modal.
    fn shortcuts_modal(&mut self, ctx: &egui::Context) {
        let mut open = self.show_help;
        egui::Window::new("Keyboard Shortcuts & Quick Guide")
            .open(&mut open)
            .default_width(480.0)
            .default_pos([220.0, 120.0])
            .resizable(false)
            .show(ctx, |ui| {
                ui.strong(egui::RichText::new("PulseForge Navigation & Controls").size(14.0));
                ui.add_space(4.0);
                egui::Grid::new("shortcuts-modal-grid")
                    .striped(true)
                    .num_columns(2)
                    .spacing([28.0, 6.0])
                    .show(ui, |ui| {
                        ui.strong("Shortcut");
                        ui.strong("Command");
                        ui.end_row();
                        ui.monospace("Esc");
                        ui.label("Return to Sessions workspace / Close drawers & modals");
                        ui.end_row();
                        ui.monospace("C");
                        ui.label("Toggle Compare Sessions analysis window");
                        ui.end_row();
                        ui.monospace("D");
                        ui.label("Toggle Storage & Platform Diagnostics drawer");
                        ui.end_row();
                        ui.monospace("?");
                        ui.label("Toggle this Keyboard Shortcuts reference");
                        ui.end_row();
                        ui.monospace("R");
                        ui.label("Force immediate data refresh from daemon");
                        ui.end_row();
                        ui.monospace("+ / -");
                        ui.label("Zoom in / out on timeline charts");
                        ui.end_row();
                        ui.monospace("Left / Right");
                        ui.label("Pan zoomed timeline window horizontally");
                        ui.end_row();
                        ui.monospace("0");
                        ui.label("Reset timeline zoom to full duration");
                        ui.end_row();
                        ui.monospace("Space");
                        ui.label("Stop active recording in Dashboard");
                        ui.end_row();
                    });
                ui.add_space(8.0);
                ui.separator();
                ui.weak("Tip: Left-click any Flamegraph bar to zoom into its call subtree; Right-click to copy the symbol name.");
                ui.weak("Tip: Drag a session folder (or any file in it) onto the window to open it offline — no daemon needed.");
            });
        self.show_help = open;
    }

    /// Render floating transient feedback notification toast.
    fn render_toast(&self, ctx: &egui::Context) {
        if let Some((msg, created, kind)) = &self.toast {
            if created.elapsed() < std::time::Duration::from_millis(2600) {
                let pal = self.active_theme.palette();
                let (dot_col, stroke_col) = match kind {
                    ToastKind::Success => (pal.success, pal.accent),
                    ToastKind::Error => (pal.danger, pal.danger),
                };
                let screen = ctx.screen_rect();
                let toast_pos = egui::pos2(screen.right() - 310.0, screen.bottom() - 46.0);
                egui::Area::new("toast-area".into())
                    .fixed_pos(toast_pos)
                    .order(egui::Order::Foreground)
                    .show(ctx, |ui| {
                        egui::Frame::default()
                            .fill(pal.surface_elevated)
                            .stroke(egui::Stroke::new(1.5_f32, stroke_col))
                            .rounding(6.0)
                            .inner_margin(egui::Margin::symmetric(14.0, 8.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    paint_status_dot(ui, dot_col, *kind == ToastKind::Success);
                                    ui.label(
                                        egui::RichText::new(msg)
                                            .size(11.5)
                                            .strong()
                                            .color(pal.text_strong),
                                    );
                                });
                            });
                    });
            }
        }
    }

    /// PF-1106: the compare panel (window overlay on the home view).
    #[allow(clippy::arithmetic_side_effects)]
    fn compare_panel(&mut self, ctx: &egui::Context) {
        let pal = self.active_theme.palette();
        // Auto-preselect the first two sessions if none selected yet so the modal
        // opens ready to compare instead of showing blank "pick..." placeholders.
        if self.compare.baseline.is_none()
            && self.compare.candidate.is_none()
            && self.home.sessions.len() >= 2
        {
            self.compare.baseline = Some(self.home.sessions[0].id);
            self.compare.candidate = Some(self.home.sessions[1].id);
        }
        let mut open = self.show_compare;
        egui::Window::new("⚖ Compare Sessions — Differential Analysis")
            .open(&mut open)
            .collapsible(false)
            .default_width(640.0)
            .min_width(540.0)
            .default_pos([250.0, 110.0])
            .show(ctx, |ui| {
                let sessions = self.home.sessions.clone();
                let mut changed = false;
                let row_of = |id: &pf_types::SessionId| -> Option<SessionRow> {
                    sessions.iter().find(|r| &r.id == id).cloned()
                };
                let label_of = |id: &pf_types::SessionId| -> String {
                    row_of(id)
                        .map(|r| {
                            format!(
                                "{} · {} samples ({})",
                                r.image,
                                fmt_int(r.samples),
                                &r.id.as_hyphenated()[..8]
                            )
                        })
                        .unwrap_or_else(|| id.as_hyphenated())
                };

                ui.columns(2, |cols| {
                    // Column 0: Baseline
                    egui::Frame::default()
                        .fill(pal.inset_fill)
                        .stroke(egui::Stroke::new(1.0_f32, pal.card_stroke))
                        .rounding(6.0)
                        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                        .show(&mut cols[0], |ui| {
                            ui.label(
                                egui::RichText::new("BASELINE SESSION (A)")
                                    .size(10.0)
                                    .strong()
                                    .color(pal.text_muted),
                            );
                            egui::ComboBox::from_id_source("cmp-baseline-combo")
                                .selected_text(
                                    self.compare
                                        .baseline
                                        .as_ref()
                                        .map(&label_of)
                                        .unwrap_or_else(|| "Select baseline…".into()),
                                )
                                .width(230.0)
                                .show_ui(ui, |ui| {
                                    for r in &sessions {
                                        let label = label_of(&r.id);
                                        let selected = self.compare.baseline == Some(r.id);
                                        if ui.selectable_label(selected, label).clicked() {
                                            self.compare.baseline = Some(r.id);
                                            changed = true;
                                        }
                                    }
                                });
                            if let Some(row) = self.compare.baseline.as_ref().and_then(row_of) {
                                let (dot, st) = state_color(&row.state);
                                ui.horizontal(|ui| {
                                    paint_status_dot(ui, dot, false);
                                    ui.weak(format!("{} · {} samples", st, fmt_int(row.samples)));
                                });
                            }
                        });

                    // Column 1: Candidate
                    egui::Frame::default()
                        .fill(pal.inset_fill)
                        .stroke(egui::Stroke::new(1.0_f32, pal.card_stroke))
                        .rounding(6.0)
                        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                        .show(&mut cols[1], |ui| {
                            ui.label(
                                egui::RichText::new("CANDIDATE SESSION (B)")
                                    .size(10.0)
                                    .strong()
                                    .color(pal.accent),
                            );
                            egui::ComboBox::from_id_source("cmp-candidate-combo")
                                .selected_text(
                                    self.compare
                                        .candidate
                                        .as_ref()
                                        .map(&label_of)
                                        .unwrap_or_else(|| "Select candidate…".into()),
                                )
                                .width(230.0)
                                .show_ui(ui, |ui| {
                                    for r in &sessions {
                                        let label = label_of(&r.id);
                                        let selected = self.compare.candidate == Some(r.id);
                                        if ui.selectable_label(selected, label).clicked() {
                                            self.compare.candidate = Some(r.id);
                                            changed = true;
                                        }
                                    }
                                });
                            if let Some(row) = self.compare.candidate.as_ref().and_then(row_of) {
                                let (dot, st) = state_color(&row.state);
                                ui.horizontal(|ui| {
                                    paint_status_dot(ui, dot, false);
                                    ui.weak(format!("{} · {} samples", st, fmt_int(row.samples)));
                                });
                            }
                        });
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let run_clicked = ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("▶ Run Differential Analysis")
                                    .strong()
                                    .color(Color32::WHITE),
                            )
                            .fill(egui::Color32::from_rgb(0x1f, 0x6f, 0xeb)),
                        )
                        .clicked();
                    if run_clicked || (changed && self.compare.ran) {
                        let client = std::sync::Arc::clone(&self.client);
                        self.compare.run_async(client);
                    }
                    if ui
                        .button("↻ Swap A / B")
                        .on_hover_text("Swap baseline and candidate sessions")
                        .clicked()
                    {
                        self.compare.swap();
                        if self.compare.ran {
                            let client = std::sync::Arc::clone(&self.client);
                            self.compare.run_async(client);
                        }
                    }
                    if let (Some(a), Some(b)) = (
                        self.compare.baseline.as_ref().and_then(row_of),
                        self.compare.candidate.as_ref().and_then(row_of),
                    ) {
                        let delta_str = if b.samples >= a.samples {
                            format!("+{}", fmt_int(b.samples - a.samples))
                        } else {
                            format!("-{}", fmt_int(a.samples - b.samples))
                        };
                        ui.weak(format!(
                            "Sample delta: {delta_str} ({} vs {})",
                            fmt_int(a.samples),
                            fmt_int(b.samples)
                        ));
                    }
                });

                if let Some(e) = &self.compare.error {
                    ui.add_space(4.0);
                    ui.colored_label(pal.danger, format!("⚠ {e}"));
                }
                if let Some(v) = &self.compare.result {
                    ui.add_space(6.0);
                    ui.separator();
                    if v.get("comparable").and_then(|c| c.as_bool()) == Some(false) {
                        ui.colored_label(
                            pal.warning,
                            "⚠ NOT COMPARABLE — scopes differ (see caveats below)",
                        );
                    }
                    egui::Grid::new("compare-grid")
                        .striped(true)
                        .num_columns(4)
                        .spacing([20.0, 6.0])
                        .show(ui, |ui| {
                            ui.strong("Metric");
                            ui.strong("Baseline (A)");
                            ui.strong("Candidate (B)");
                            ui.strong("Delta (B − A)");
                            ui.end_row();
                            for row in self.compare.metrics_detailed() {
                                ui.label(&row.name);
                                ui.monospace(&row.baseline);
                                ui.monospace(&row.candidate);
                                let delta_col = match row.pct_change {
                                    Some(p) if p > 0.05 => pal.danger,
                                    Some(p) if p < -0.05 => pal.success,
                                    _ => pal.text_muted,
                                };
                                let delta_str = match row.pct_change {
                                    Some(p) if p > 0.0 => format!("{} (+{:.1}%)", row.delta, p),
                                    Some(p) if p < 0.0 => format!("{} ({:.1}%)", row.delta, p),
                                    _ => row.delta,
                                };
                                ui.colored_label(delta_col, delta_str);
                                ui.end_row();
                            }
                        });
                    for c in v
                        .get("caveats")
                        .and_then(|c| c.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|c| c.as_str())
                    {
                        ui.small(c);
                    }
                }
            });
        self.show_compare = open;
    }

    /// PF-707: the diagnostics drawer (bottom panel). Every state is
    /// expressed in text as well as color (PF-709 non-color cues).
    fn diagnostics_drawer(&mut self, ctx: &egui::Context) {
        let pal = self.active_theme.palette();
        egui::TopBottomPanel::bottom("diagnostics")
            .min_height(152.0)
            .show_separator_line(false)
            .frame(
                egui::Frame::default()
                    .fill(pal.card_fill)
                    .stroke(egui::Stroke::new(1.0_f32, pal.card_stroke))
                    .inner_margin(egui::Margin::symmetric(16.0, 10.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    paint_status_dot(ui, pal.accent, true);
                    ui.strong(egui::RichText::new("Platform & Storage Diagnostics").size(13.5));
                    ui.weak(egui::RichText::new("· Esc to close · D toggles").size(11.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("× Close").clicked() {
                            self.show_diagnostics = false;
                        }
                    });
                });
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
                match self.diagnostics.health.as_ref() {
                    Some(h) => {
                        ui.columns(2, |cols| {
                            egui::Grid::new("diag-grid-left")
                                .num_columns(2)
                                .spacing([16.0, 5.0])
                                .show(&mut cols[0], |ui| {
                                    ui.weak("Data Root:");
                                    ui.monospace(h.data_root.as_str());
                                    ui.end_row();
                                    ui.weak("Storage Writable:");
                                    ui.horizontal(|ui| {
                                        let col = if h.writable { pal.success } else { pal.danger };
                                        paint_status_dot(ui, col, false);
                                        ui.label(if h.writable {
                                            "Yes (Ready for recording)"
                                        } else {
                                            "NO — recordings cannot be saved"
                                        });
                                    });
                                    ui.end_row();
                                    ui.weak("Sessions Stored:");
                                    ui.strong(format!(
                                        "{} sessions ({} total)",
                                        h.session_count,
                                        fmt_value(h.total_bytes as f64)
                                    ));
                                    ui.end_row();
                                    ui.weak("Operating System:");
                                    ui.monospace(h.os_version.as_str());
                                    ui.end_row();
                                });

                            egui::Grid::new("diag-grid-right")
                                .num_columns(2)
                                .spacing([16.0, 5.0])
                                .show(&mut cols[1], |ui| {
                                    ui.weak("OS Performance Counters:");
                                    ui.horizontal(|ui| {
                                        let col = if h.counters_available {
                                            pal.success
                                        } else {
                                            pal.warning
                                        };
                                        paint_status_dot(ui, col, false);
                                        ui.label(if h.counters_available {
                                            "Available".into()
                                        } else {
                                            format!("Unavailable ({})", h.counters_reason)
                                        });
                                    });
                                    ui.end_row();
                                    ui.weak("CPU Stack Sampling:");
                                    ui.horizontal(|ui| {
                                        let col = if h.stacks_available {
                                            pal.success
                                        } else {
                                            pal.warning
                                        };
                                        paint_status_dot(ui, col, false);
                                        ui.label(if h.stacks_available {
                                            "Available".into()
                                        } else {
                                            format!("Unavailable ({})", h.stacks_reason)
                                        });
                                    });
                                    ui.end_row();
                                    ui.weak("Logical CPUs:");
                                    ui.strong(
                                        h.logical_cpus
                                            .map(|n| format!("{n} cores"))
                                            .unwrap_or_else(|| "unknown".into()),
                                    );
                                    ui.end_row();
                                });
                        });
                    }
                    None => {
                        ui.colored_label(
                            pal.danger,
                            self.diagnostics
                                .error
                                .clone()
                                .unwrap_or_else(|| "no diagnostics".into()),
                        );
                    }
                }
            });
    }

    #[allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn home_ui(&mut self, ui: &mut egui::Ui) {
        let pal = self.active_theme.palette();
        ui.add_space(4.0);

        // ---- Top Studio Card: Workload Recording Control ----
        egui::Frame::default()
            .fill(pal.card_fill)
            .stroke(egui::Stroke::new(1.0_f32, pal.card_stroke))
            .rounding(8.0)
            .inner_margin(egui::Margin::symmetric(14.0, 10.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // Header row: Mode tabs on left, Quick Preset Chips on right
                ui.horizontal(|ui| {
                    section_header_colored(ui, "Record New Profile", pal.accent);
                    ui.separator();
                    if ui
                        .selectable_label(
                            self.home.record_mode == crate::viewmod::RecordMode::Attach,
                            "Target Running PID (Attach)",
                        )
                        .clicked()
                    {
                        self.home.record_mode = crate::viewmod::RecordMode::Attach;
                    }
                    if ui
                        .selectable_label(
                            self.home.record_mode == crate::viewmod::RecordMode::Run,
                            "Launch Executable (Run)",
                        )
                        .clicked()
                    {
                        self.home.record_mode = crate::viewmod::RecordMode::Run;
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.home.record_mode == crate::viewmod::RecordMode::Attach {
                            for (lbl, val) in [
                                ("1s Slow", "1000"),
                                ("250ms Balanced", "250"),
                                ("100ms Fast", "100"),
                            ] {
                                let active = self.home.attach_interval_ms == val;
                                if ui.selectable_label(active, lbl).clicked() {
                                    self.home.attach_interval_ms = val.into();
                                }
                            }
                            ui.weak(egui::RichText::new("Cadence:").size(10.5));
                            ui.separator();
                            for (lbl, val) in [
                                ("5m", "5m"),
                                ("60s", "60s"),
                                ("30s", "30s"),
                                ("10s", "10s"),
                                ("5s", "5s"),
                            ] {
                                let active = self.home.attach_duration_ms == val;
                                if ui.selectable_label(active, lbl).clicked() {
                                    self.home.attach_duration_ms = val.into();
                                }
                            }
                            ui.weak(egui::RichText::new("Presets:").size(10.5));
                        } else {
                            if ui
                                .small_button("cargo check (30s)")
                                .on_hover_text("Profile cargo check build execution")
                                .clicked()
                            {
                                self.home.run_exe = "cargo".into();
                                self.home.run_args = "check".into();
                                self.home.attach_duration_ms = "30s".into();
                            }
                            if ui
                                .small_button("pf-fixture-cpu (5s)")
                                .on_hover_text("Load CPU burn fixture benchmark")
                                .clicked()
                            {
                                self.home.run_exe = "pf-fixture-cpu".into();
                                self.home.run_args = "--threads 4 --duration 5".into();
                                self.home.attach_duration_ms = "5s".into();
                            }
                            ui.weak(egui::RichText::new("Templates:").size(10.5));
                        }
                    });
                });

                ui.add_space(6.0);

                // Labeled Form Fields Row
                ui.horizontal(|ui| {
                    if self.home.record_mode == crate::viewmod::RecordMode::Run {
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new("EXECUTABLE PATH")
                                    .size(9.5)
                                    .strong()
                                    .color(pal.text_muted),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut self.home.run_exe)
                                    .desired_width(240.0)
                                    .hint_text("e.g. ./target/release/app"),
                            );
                        });
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new("ARGUMENTS")
                                    .size(9.5)
                                    .strong()
                                    .color(pal.text_muted),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut self.home.run_args)
                                    .desired_width(170.0)
                                    .hint_text("--bench --threads 4"),
                            );
                        });
                    } else {
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new("TARGET PID")
                                    .size(9.5)
                                    .strong()
                                    .color(pal.text_muted),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut self.home.attach_pid)
                                    .desired_width(110.0)
                                    .hint_text("PID e.g. 4242"),
                            );
                        });
                    }
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("DURATION")
                                .size(9.5)
                                .strong()
                                .color(pal.text_muted),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.home.attach_duration_ms)
                                .desired_width(105.0)
                                .hint_text("10s or 10000"),
                        );
                    });
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("INTERVAL (MS)")
                                .size(9.5)
                                .strong()
                                .color(pal.text_muted),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.home.attach_interval_ms)
                                .desired_width(105.0)
                                .hint_text("250"),
                        );
                    });
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("SESSION LABEL (OPTIONAL)")
                                .size(9.5)
                                .strong()
                                .color(pal.text_muted),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.home.attach_label)
                                .desired_width(170.0)
                                .hint_text("e.g. baseline-v1"),
                        );
                    });
                    ui.vertical(|ui| {
                        ui.add_space(13.0);
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("▶ Start Recording")
                                        .size(12.5)
                                        .strong()
                                        .color(Color32::WHITE),
                                )
                                .min_size(egui::vec2(145.0, 24.0))
                                .fill(egui::Color32::from_rgb(0x1f, 0x6f, 0xeb)),
                            )
                            .clicked()
                        {
                            if self.home.record_mode == crate::viewmod::RecordMode::Run {
                                self.home
                                    .submit_run_async(std::sync::Arc::clone(&self.client));
                            } else {
                                self.home
                                    .submit_attach_async(std::sync::Arc::clone(&self.client));
                            }
                        }
                    });
                });

                if let crate::viewmod::ActionOutcome::Failed(err) = &self.home.last_outcome {
                    ui.add_space(4.0);
                    ui.colored_label(pal.danger, format!("⚠ {err}"));
                }
            });

        ui.add_space(8.0);

        // ---- Two-Column Studio Workspace: Left = Running Processes, Right = Recorded Sessions ----
        let avail_h = ui.available_height().max(240.0);
        ui.horizontal_top(|ui| {
            // LEFT PANEL: Live Running Processes Picker (350px wide)
            let left_w = 350.0;
            ui.allocate_ui_with_layout(
                egui::vec2(left_w, avail_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    egui::Frame::default()
                        .fill(pal.card_fill)
                        .stroke(egui::Stroke::new(1.0_f32, pal.card_stroke))
                        .rounding(8.0)
                        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
                        .show(ui, |ui| {
                            ui.set_width(left_w - 24.0);
                            ui.set_min_height(avail_h - 24.0);
                            let proc_count = self.home.processes.len();
                            let rec_count = self.home.recording_process_count();

                            // Header: title, active/rec badges, quick refresh button
                            ui.horizontal(|ui| {
                                section_header_colored(ui, "Live Processes", pal.success);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .small_button("↻")
                                            .on_hover_text("Refresh process list now [R]")
                                            .clicked()
                                        {
                                            self.home.request_refresh_now(std::sync::Arc::clone(
                                                &self.client,
                                            ));
                                            self.set_toast("Refreshed running processes");
                                        }
                                        if rec_count > 0 {
                                            paint_status_dot(ui, pal.success, true);
                                            ui.colored_label(
                                                pal.success,
                                                egui::RichText::new(format!("{rec_count} rec"))
                                                    .size(10.5)
                                                    .strong(),
                                            );
                                        }
                                        ui.weak(
                                            egui::RichText::new(format!("{proc_count} active"))
                                                .size(11.0),
                                        );
                                    },
                                );
                            });

                            ui.add_space(4.0);

                            // Control Strip: Mode (List/Tree) + Filter Tabs + Sort
                            ui.horizontal(|ui| {
                                if ui
                                    .selectable_label(
                                        self.home.process_view_mode == ProcessViewMode::Flat,
                                        "List",
                                    )
                                    .on_hover_text("Flat sorted list of all running processes")
                                    .clicked()
                                {
                                    self.home.process_view_mode = ProcessViewMode::Flat;
                                }
                                if ui
                                    .selectable_label(
                                        self.home.process_view_mode == ProcessViewMode::Tree,
                                        "Tree",
                                    )
                                    .on_hover_text(
                                        "Hierarchical process tree (parent/child relationships)",
                                    )
                                    .clicked()
                                {
                                    self.home.process_view_mode = ProcessViewMode::Tree;
                                }

                                ui.separator();

                                let all_label = format!("All ({proc_count})");
                                if ui
                                    .selectable_label(
                                        self.home.process_tab == ProcessFilterTab::All,
                                        all_label,
                                    )
                                    .clicked()
                                {
                                    self.home.process_tab = ProcessFilterTab::All;
                                }
                                let rec_label = format!("Profiling ({rec_count})");
                                if ui
                                    .selectable_label(
                                        self.home.process_tab == ProcessFilterTab::Recording,
                                        rec_label,
                                    )
                                    .on_hover_text(
                                        "Filter to processes currently being recorded by PulseForge",
                                    )
                                    .clicked()
                                {
                                    self.home.process_tab = ProcessFilterTab::Recording;
                                }

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .small_button(self.home.process_sort.label())
                                            .on_hover_text("Cycle sort order (PID / Name)")
                                            .clicked()
                                        {
                                            self.home.process_sort.cycle();
                                        }
                                    },
                                );
                            });

                            ui.add_space(4.0);

                            // Search bar with clear button
                            ui.horizontal(|ui| {
                                let search_w = if self.process_search.is_empty() {
                                    left_w - 32.0
                                } else {
                                    left_w - 58.0
                                };
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.process_search)
                                        .desired_width(search_w)
                                        .hint_text("Filter processes by name or PID…"),
                                );
                                if !self.process_search.is_empty()
                                    && ui
                                        .small_button("×")
                                        .on_hover_text("Clear filter")
                                        .clicked()
                                {
                                    self.process_search.clear();
                                }
                            });

                            if self.home.process_view_mode == ProcessViewMode::Tree
                                && self.process_search.is_empty()
                            {
                                ui.add_space(2.0);
                                ui.horizontal(|ui| {
                                    if ui
                                        .small_button("Expand All")
                                        .on_hover_text("Expand all process subtrees")
                                        .clicked()
                                    {
                                        self.home.expand_all_processes();
                                    }
                                    if ui
                                        .small_button("Collapse All")
                                        .on_hover_text("Collapse all process subtrees")
                                        .clicked()
                                    {
                                        self.home.collapse_all_processes();
                                    }
                                });
                            }

                            ui.add_space(4.0);
                            ui.separator();
                            ui.add_space(3.0);

                            // Deferred action state (avoids mut borrow conflicts in the loop)
                            let mut selected_row: Option<ProcessRow> = None;
                            let mut double_clicked_row: Option<ProcessRow> = None;
                            let mut toggle_pid: Option<u32> = None;
                            let mut stop_session_id: Option<(SessionId, String)> = None;
                            let mut open_session_id: Option<SessionId> = None;
                            let mut copied_pid: Option<u32> = None;

                            let row_h = 34.0;

                            match self.home.process_view_mode {
                                ProcessViewMode::Tree => {
                                    let tree_nodes =
                                        self.home.visible_tree_nodes(&self.process_search);
                                    if tree_nodes.is_empty() {
                                        ui.add_space(16.0);
                                        ui.weak(
                                            egui::RichText::new(if self.home.processes.is_empty() {
                                                "No active processes listed yet.\nStart the daemon or press Refresh [↻]."
                                            } else {
                                                "No processes match the current filter."
                                            })
                                            .size(11.5),
                                        );
                                        if !self.process_search.is_empty()
                                            && ui.button("Clear Filter").clicked()
                                        {
                                            self.process_search.clear();
                                        }
                                    } else {
                                        egui::ScrollArea::vertical()
                                            .id_source("process-picker-tree")
                                            .max_height(avail_h - 130.0)
                                            .auto_shrink([false, false])
                                            .show_rows(
                                                ui,
                                                row_h,
                                                tree_nodes.len(),
                                                |ui, range| {
                                                    for node in tree_nodes[range].iter() {
                                                        let row = &node.process;
                                                        let is_selected = self.home.attach_pid
                                                            == row.pid.to_string();
                                                        let live_session = self
                                                            .home
                                                            .is_process_recording(row)
                                                            .cloned();

                                                        let frame_fill = if is_selected {
                                                            pal.surface_elevated
                                                        } else {
                                                            Color32::TRANSPARENT
                                                        };
                                                        let frame_stroke = if is_selected {
                                                            egui::Stroke::new(1.0_f32, pal.accent)
                                                        } else if live_session.is_some() {
                                                            egui::Stroke::new(1.0_f32, pal.success)
                                                        } else {
                                                            egui::Stroke::NONE
                                                        };

                                                        egui::Frame::default()
                                                            .fill(frame_fill)
                                                            .stroke(frame_stroke)
                                                            .rounding(4.0)
                                                            .inner_margin(egui::Margin::symmetric(
                                                                4.0, 3.0,
                                                            ))
                                                            .show(ui, |ui| {
                                                                ui.horizontal(|ui| {
                                                                    if node.depth > 0 {
                                                                        ui.add_space(
                                                                            node.depth as f32
                                                                                * 12.0,
                                                                        );
                                                                        if !node.has_children {
                                                                            let (branch_rect, _) = ui.allocate_exact_size(
                                                                                egui::vec2(10.0, 16.0),
                                                                                egui::Sense::hover(),
                                                                            );
                                                                            let mid_y = branch_rect.center().y;
                                                                            let left_x = branch_rect.left() + 2.0;
                                                                            ui.painter().line_segment(
                                                                                [
                                                                                    Pos2::new(left_x, branch_rect.top() - 2.0),
                                                                                    Pos2::new(left_x, mid_y),
                                                                                ],
                                                                                egui::Stroke::new(1.0_f32, pal.card_stroke),
                                                                            );
                                                                            ui.painter().line_segment(
                                                                                [
                                                                                    Pos2::new(left_x, mid_y),
                                                                                    Pos2::new(branch_rect.right() - 1.0, mid_y),
                                                                                ],
                                                                                egui::Stroke::new(1.0_f32, pal.card_stroke),
                                                                            );
                                                                        }
                                                                    }
                                                                    if node.has_children
                                                                        && paint_caret(
                                                                            ui,
                                                                            node.is_expanded,
                                                                            pal.text_muted,
                                                                        )
                                                                        .on_hover_text(
                                                                            if node.is_expanded {
                                                                                "Collapse child processes"
                                                                            } else {
                                                                                "Expand child processes"
                                                                            },
                                                                        )
                                                                        .clicked()
                                                                    {
                                                                        toggle_pid =
                                                                            Some(row.pid);
                                                                    }

                                                                    crate::app_icon::paint_process_icon(
                                                                        ui,
                                                                        &mut self.icon_cache,
                                                                        row.pid,
                                                                        &row.image,
                                                                        egui::vec2(20.0, 20.0),
                                                                    );

                                                                    let raw_left = ui.vertical(|ui| {
                                                                        ui.horizontal(|ui| {
                                                                            if let Some(ref title) = row.window_title {
                                                                                ui.add(
                                                                                    egui::Label::new(
                                                                                        egui::RichText::new(title)
                                                                                            .size(11.5)
                                                                                            .strong()
                                                                                            .color(pal.text_strong),
                                                                                    )
                                                                                    .truncate(true),
                                                                                );
                                                                            } else {
                                                                                ui.add(
                                                                                    egui::Label::new(
                                                                                        egui::RichText::new(&row.image)
                                                                                            .size(11.5)
                                                                                            .strong()
                                                                                            .color(pal.text_strong),
                                                                                    )
                                                                                    .truncate(true),
                                                                                );
                                                                            }
                                                                            if live_session.is_some() {
                                                                                paint_status_dot(ui, pal.success, true);
                                                                                ui.colored_label(
                                                                                    pal.success,
                                                                                    egui::RichText::new("REC").size(8.5).strong(),
                                                                                );
                                                                            }
                                                                        });
                                                                        ui.horizontal(|ui| {
                                                                            if row.window_title.is_some() {
                                                                                ui.monospace(
                                                                                    egui::RichText::new(format!("{} · {}", row.image, row.pid))
                                                                                        .size(9.5)
                                                                                        .color(pal.text_muted),
                                                                                );
                                                                            } else {
                                                                                ui.monospace(
                                                                                    egui::RichText::new(format!("PID {}", row.pid))
                                                                                        .size(9.5)
                                                                                        .color(pal.accent),
                                                                                );
                                                                            }
                                                                        });
                                                                    });

                                                                    let mut tip = if let Some(ref title) = row.window_title {
                                                                        format!("Window: {}\nProcess: {}\nPID: {}", title, row.image, row.pid)
                                                                    } else {
                                                                        format!("Process: {}\nPID: {}", row.image, row.pid)
                                                                    };
                                                                    if let Some(ppid) =
                                                                        row.parent_pid
                                                                    {
                                                                        tip.push_str(&format!(
                                                                            "\nParent PID: {ppid}"
                                                                        ));
                                                                    }
                                                                            if let Some(pct) = row.cpu_pct {
                                                                                tip.push_str(&format!(
                                                                                "
CPU: {pct:.1}% of one logical core"
                                                                                ));
                                                                            }
                                                                            if let Some(rss) = row.rss_bytes {
                                                                                tip.push_str(&format!(
                                                                                "
Memory: {}",
                                                                                fmt_value(rss as f64)
                                                                                ));
                                                                            }
                                                                    if let Some(s) = &live_session {
                                                                        tip.push_str(&format!(
                                                                            "\nStatus: RECORDING (Session {}, {} samples)",
                                                                            &s.id.as_hyphenated()[..8],
                                                                            fmt_int(s.samples)
                                                                        ));
                                                                    } else {
                                                                        tip.push_str("\nClick to select · Double-click to start recording immediately");
                                                                    }

                                                                    let click_resp = ui
                                                                        .interact(
                                                                            raw_left.response.rect,
                                                                            ui.make_persistent_id(
                                                                                format!(
                                                                                    "tree-proc-{}",
                                                                                    row.pid
                                                                                ),
                                                                            ),
                                                                            egui::Sense::click(),
                                                                        )
                                                                        .on_hover_text(tip);

                                                                    if click_resp.double_clicked() {
                                                                        double_clicked_row =
                                                                            Some(row.clone());
                                                                    } else if click_resp.clicked() {
                                                                        selected_row =
                                                                            Some(row.clone());
                                                                    }

                                                                    ui.with_layout(
                                                                        egui::Layout::right_to_left(
                                                                            egui::Align::Center,
                                                                        ),
                                                                        |ui| {
                                                                            if let Some(s) =
                                                                                &live_session
                                                                            {
                                                                                if ui
                                                                                    .add(
                                                                                        egui::Button::new(
                                                                                            egui::RichText::new("Open ▶").size(10.5).strong(),
                                                                                        )
                                                                                        .min_size(egui::vec2(50.0, 19.0)),
                                                                                    )
                                                                                    .on_hover_text("Open live session dashboard")
                                                                                    .clicked()
                                                                                {
                                                                                    open_session_id = Some(s.id);
                                                                                }
                                                                                if ui
                                                                                    .button(
                                                                                        egui::RichText::new("■").size(11.0).color(pal.danger).strong(),
                                                                                    )
                                                                                    .on_hover_text("Stop recording this session")
                                                                                    .clicked()
                                                                                {
                                                                                    stop_session_id = Some((s.id, row.image.clone()));
                                                                                }
                                                                            } else {
                                                                                if ui
                                                                                    .small_button("▶")
                                                                                    .on_hover_text("Attach & start recording immediately [Double-click]")
                                                                                    .clicked()
                                                                                {
                                                                                    double_clicked_row = Some(row.clone());
                                                                                }
                                                                                if ui
                                                                                    .add(
                                                                                        egui::Button::new(
                                                                                            egui::RichText::new("Select").size(10.5),
                                                                                        )
                                                                                        .min_size(egui::vec2(40.0, 19.0)),
                                                                                    )
                                                                                    .on_hover_text("Fill PID into record form [Single-click]")
                                                                                    .clicked()
                                                                                {
                                                                                    selected_row = Some(row.clone());
                                                                                }
                                                                            }
                                                                            if ui
                                                                                .small_button("📋")
                                                                                .on_hover_text("Copy PID to clipboard")
                                                                                .clicked()
                                                                            {
                                                                                copied_pid = Some(row.pid);
                                                                            }
                                                                            paint_row_resources(
                                                                                ui,
                                                                                &pal,
                                                                                row.cpu_pct,
                                                                                row.rss_bytes,
                                                                            );
                                                                        },
                                                                    );
                                                                });
                                                            });
                                                    }
                                                },
                                            );
                                    }
                                }
                                ProcessViewMode::Flat => {
                                    let flat_rows = self
                                        .home
                                        .filtered_and_sorted_processes(&self.process_search);
                                    if flat_rows.is_empty() {
                                        ui.add_space(16.0);
                                        ui.weak(
                                            egui::RichText::new(if self.home.processes.is_empty() {
                                                "No active processes listed yet.\nStart the daemon or press Refresh [↻]."
                                            } else {
                                                "No processes match the current filter."
                                            })
                                            .size(11.5),
                                        );
                                        if !self.process_search.is_empty()
                                            && ui.button("Clear Filter").clicked()
                                        {
                                            self.process_search.clear();
                                        }
                                    } else {
                                        egui::ScrollArea::vertical()
                                            .id_source("process-picker-flat")
                                            .max_height(avail_h - 105.0)
                                            .auto_shrink([false, false])
                                            .show_rows(
                                                ui,
                                                row_h,
                                                flat_rows.len(),
                                                |ui, range| {
                                                    for row in flat_rows[range].iter() {
                                                        let is_selected = self.home.attach_pid
                                                            == row.pid.to_string();
                                                        let live_session = self
                                                            .home
                                                            .is_process_recording(row)
                                                            .cloned();

                                                        let frame_fill = if is_selected {
                                                            pal.surface_elevated
                                                        } else {
                                                            Color32::TRANSPARENT
                                                        };
                                                        let frame_stroke = if is_selected {
                                                            egui::Stroke::new(1.0_f32, pal.accent)
                                                        } else if live_session.is_some() {
                                                            egui::Stroke::new(1.0_f32, pal.success)
                                                        } else {
                                                            egui::Stroke::NONE
                                                        };

                                                        egui::Frame::default()
                                                            .fill(frame_fill)
                                                            .stroke(frame_stroke)
                                                            .rounding(4.0)
                                                            .inner_margin(egui::Margin::symmetric(
                                                                4.0, 3.0,
                                                            ))
                                                            .show(ui, |ui| {
                                                                ui.horizontal(|ui| {
                                                                    crate::app_icon::paint_process_icon(
                                                                        ui,
                                                                        &mut self.icon_cache,
                                                                        row.pid,
                                                                        &row.image,
                                                                        egui::vec2(20.0, 20.0),
                                                                    );

                                                                    let raw_left = ui.vertical(|ui| {
                                                                        ui.horizontal(|ui| {
                                                                            if let Some(ref title) = row.window_title {
                                                                                ui.add(
                                                                                    egui::Label::new(
                                                                                        egui::RichText::new(title)
                                                                                            .size(11.5)
                                                                                            .strong()
                                                                                            .color(pal.text_strong),
                                                                                    )
                                                                                    .truncate(true),
                                                                                );
                                                                            } else {
                                                                                ui.add(
                                                                                    egui::Label::new(
                                                                                        egui::RichText::new(&row.image)
                                                                                            .size(11.5)
                                                                                            .strong()
                                                                                            .color(pal.text_strong),
                                                                                    )
                                                                                    .truncate(true),
                                                                                );
                                                                            }
                                                                            if live_session.is_some() {
                                                                                paint_status_dot(ui, pal.success, true);
                                                                                ui.colored_label(
                                                                                    pal.success,
                                                                                    egui::RichText::new("REC").size(8.5).strong(),
                                                                                );
                                                                            }
                                                                        });
                                                                        ui.horizontal(|ui| {
                                                                            if row.window_title.is_some() {
                                                                                ui.monospace(
                                                                                    egui::RichText::new(format!("{} · {}", row.image, row.pid))
                                                                                        .size(9.5)
                                                                                        .color(pal.text_muted),
                                                                                );
                                                                            } else {
                                                                                ui.monospace(
                                                                                    egui::RichText::new(format!("PID {}", row.pid))
                                                                                        .size(9.5)
                                                                                        .color(pal.accent),
                                                                                );
                                                                            }
                                                                        });
                                                                    });

                                                                    let mut tip = if let Some(ref title) = row.window_title {
                                                                        format!("Window: {}\nProcess: {}\nPID: {}", title, row.image, row.pid)
                                                                    } else {
                                                                        format!("Process: {}\nPID: {}", row.image, row.pid)
                                                                    };
                                                                    if let Some(ppid) =
                                                                        row.parent_pid
                                                                    {
                                                                        tip.push_str(&format!(
                                                                            "\nParent PID: {ppid}"
                                                                        ));
                                                                    }
                                                                            if let Some(pct) = row.cpu_pct {
                                                                                tip.push_str(&format!(
                                                                                "
CPU: {pct:.1}% of one logical core"
                                                                                ));
                                                                            }
                                                                            if let Some(rss) = row.rss_bytes {
                                                                                tip.push_str(&format!(
                                                                                "
Memory: {}",
                                                                                fmt_value(rss as f64)
                                                                                ));
                                                                            }
                                                                    if let Some(s) = &live_session {
                                                                        tip.push_str(&format!(
                                                                            "\nStatus: RECORDING (Session {}, {} samples)",
                                                                            &s.id.as_hyphenated()[..8],
                                                                            fmt_int(s.samples)
                                                                        ));
                                                                    } else {
                                                                        tip.push_str("\nClick to select · Double-click to start recording immediately");
                                                                    }

                                                                    let click_resp = ui
                                                                        .interact(
                                                                            raw_left.response.rect,
                                                                            ui.make_persistent_id(
                                                                                format!(
                                                                                    "flat-proc-{}",
                                                                                    row.pid
                                                                                ),
                                                                            ),
                                                                            egui::Sense::click(),
                                                                        )
                                                                        .on_hover_text(tip);

                                                                    if click_resp.double_clicked() {
                                                                        double_clicked_row =
                                                                            Some(row.clone());
                                                                    } else if click_resp.clicked() {
                                                                        selected_row =
                                                                            Some(row.clone());
                                                                    }

                                                                    ui.with_layout(
                                                                        egui::Layout::right_to_left(
                                                                            egui::Align::Center,
                                                                        ),
                                                                        |ui| {
                                                                            if let Some(s) =
                                                                                &live_session
                                                                            {
                                                                                if ui
                                                                                    .add(
                                                                                        egui::Button::new(
                                                                                            egui::RichText::new("Open ▶").size(10.5).strong(),
                                                                                        )
                                                                                        .min_size(egui::vec2(50.0, 19.0)),
                                                                                    )
                                                                                    .on_hover_text("Open live session dashboard")
                                                                                    .clicked()
                                                                                {
                                                                                    open_session_id = Some(s.id);
                                                                                }
                                                                                if ui
                                                                                    .button(
                                                                                        egui::RichText::new("■").size(11.0).color(pal.danger).strong(),
                                                                                    )
                                                                                    .on_hover_text("Stop recording this session")
                                                                                    .clicked()
                                                                                {
                                                                                    stop_session_id = Some((s.id, row.image.clone()));
                                                                                }
                                                                            } else {
                                                                                if ui
                                                                                    .small_button("▶")
                                                                                    .on_hover_text("Attach & start recording immediately [Double-click]")
                                                                                    .clicked()
                                                                                {
                                                                                    double_clicked_row = Some(row.clone());
                                                                                }
                                                                                if ui
                                                                                    .add(
                                                                                        egui::Button::new(
                                                                                            egui::RichText::new("Select").size(10.5),
                                                                                        )
                                                                                        .min_size(egui::vec2(40.0, 19.0)),
                                                                                    )
                                                                                    .on_hover_text("Fill PID into record form [Single-click]")
                                                                                    .clicked()
                                                                                {
                                                                                    selected_row = Some(row.clone());
                                                                                }
                                                                            }
                                                                            if ui
                                                                                .small_button("📋")
                                                                                .on_hover_text("Copy PID to clipboard")
                                                                                .clicked()
                                                                            {
                                                                                copied_pid = Some(row.pid);
                                                                            }
                                                                            paint_row_resources(
                                                                                ui,
                                                                                &pal,
                                                                                row.cpu_pct,
                                                                                row.rss_bytes,
                                                                            );
                                                                        },
                                                                    );
                                                                });
                                                            });
                                                    }
                                                },
                                            );
                                    }
                                }
                            }

                            // Execute deferred actions outside the rows iteration
                            if let Some(row) = selected_row {
                                self.home.pick_process(&row);
                                self.set_toast(format!(
                                    "Selected PID {} ({})",
                                    row.pid,
                                    row.display_title()
                                ));
                            }
                            if let Some(row) = double_clicked_row {
                                self.home.pick_process(&row);
                                self.home.submit_attach_async(std::sync::Arc::clone(
                                    &self.client,
                                ));
                                self.set_toast(format!("Recording {}", row.display_title()));
                            }
                            if let Some(pid) = toggle_pid {
                                self.home.toggle_process_expanded(pid);
                            }
                            if let Some((sid, img)) = stop_session_id {
                                let client = std::sync::Arc::clone(&self.client);
                                std::thread::spawn(move || {
                                    let _ = client.stop(sid);
                                });
                                self.set_toast(format!("Sent stop request for {img}"));
                                self.home.request_refresh_now(std::sync::Arc::clone(
                                    &self.client,
                                ));
                            }
                            if let Some(sid) = open_session_id {
                                self.open_dashboard(sid);
                            }
                            if let Some(pid) = copied_pid {
                                ui.output_mut(|o| o.copied_text = pid.to_string());
                                self.set_toast(format!("Copied PID {pid}"));
                            }
                        });
                },
            );

            ui.add_space(6.0);

            // RIGHT PANEL: Recorded Sessions Explorer (Remaining width)
            let right_w = ui.available_width();
            ui.allocate_ui_with_layout(
                egui::vec2(right_w, avail_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    let total_count = self.home.sessions.len();
                    let live_count = self
                        .home
                        .sessions
                        .iter()
                        .filter(|s| s.state == "RECORDING" || s.state == "STARTING")
                        .count();
                    let done_count = self
                        .home
                        .sessions
                        .iter()
                        .filter(|s| s.state == "COMPLETED" || s.state == "RECOVERED_PARTIAL")
                        .count();
                    let fail_count = self
                        .home
                        .sessions
                        .iter()
                        .filter(|s| s.state == "FAILED" || s.state == "CANCELLED")
                        .count();
                    let matching_rows = self.home.filter_rows(&self.search);

                    // Sessions Header & Filter Bar
                    ui.horizontal(|ui| {
                        section_header_colored(ui, "Recorded Sessions", pal.accent);
                        ui.separator();
                        for (flt_lbl, flt_val) in [
                            (
                                format!("All ({total_count})"),
                                crate::viewmod::SessionFilter::All,
                            ),
                            (
                                format!("Live ({live_count})"),
                                crate::viewmod::SessionFilter::Recording,
                            ),
                            (
                                format!("Done ({done_count})"),
                                crate::viewmod::SessionFilter::Completed,
                            ),
                            (
                                format!("Failed ({fail_count})"),
                                crate::viewmod::SessionFilter::Failed,
                            ),
                        ] {
                            let sel = self.home.session_filter == flt_val;
                            if ui.selectable_label(sel, flt_lbl).clicked() {
                                self.home.session_filter = flt_val;
                            }
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button("↻ Refresh")
                                .on_hover_text("Refresh sessions & processes [R]")
                                .clicked()
                            {
                                self.home
                                    .request_refresh_now(std::sync::Arc::clone(&self.client));
                            }
                            let sort_label = match self.home.session_sort {
                                crate::viewmod::SessionSort::Newest => "Newest",
                                crate::viewmod::SessionSort::Oldest => "Oldest",
                                crate::viewmod::SessionSort::MostSamples => "Most samples",
                                crate::viewmod::SessionSort::Name => "Image name",
                            };
                            egui::ComboBox::from_id_source("session-sort-combo")
                                .selected_text(sort_label)
                                .width(98.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.home.session_sort,
                                        crate::viewmod::SessionSort::Newest,
                                        "Newest",
                                    );
                                    ui.selectable_value(
                                        &mut self.home.session_sort,
                                        crate::viewmod::SessionSort::Oldest,
                                        "Oldest",
                                    );
                                    ui.selectable_value(
                                        &mut self.home.session_sort,
                                        crate::viewmod::SessionSort::MostSamples,
                                        "Most samples",
                                    );
                                    ui.selectable_value(
                                        &mut self.home.session_sort,
                                        crate::viewmod::SessionSort::Name,
                                        "Image name",
                                    );
                                });
                            ui.weak(egui::RichText::new("Sort:").size(11.0));
                            if !self.search.is_empty()
                                && ui
                                    .small_button("×")
                                    .on_hover_text("Clear search")
                                    .clicked()
                            {
                                self.search.clear();
                            }
                            ui.add(
                                egui::TextEdit::singleline(&mut self.search)
                                    .desired_width(135.0)
                                    .hint_text("Search sessions…"),
                            );
                        });
                    });

                    ui.add_space(6.0);

                    // ---- Session cards ----
                    let mut delete_request: Option<(SessionId, String)> = None;
                    let rows = matching_rows;
                    if rows.is_empty() {
                        egui::Frame::default()
                            .fill(pal.card_fill)
                            .stroke(egui::Stroke::new(1.0_f32, pal.card_stroke))
                            .rounding(8.0)
                            .inner_margin(egui::Margin::symmetric(20.0, 18.0))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.strong(
                                    egui::RichText::new("No sessions recorded yet").size(15.0),
                                );
                                ui.add_space(6.0);
                                ui.label(
                                    egui::RichText::new(
                                        "Capture and analyze your first performance profile in three steps:",
                                    )
                                    .size(12.0),
                                );
                                ui.add_space(6.0);
                                for (n, step) in [
                                    (
                                        1,
                                        "Ensure the daemon is active (auto-starts on launch, or run `pulseforge daemon`)",
                                    ),
                                    (
                                        2,
                                        "Select a running target from Live Processes on the left (or enter a PID above) and press ▶ Start Recording",
                                    ),
                                    (
                                        3,
                                        "Open any live or completed session card to inspect CPU, Memory, I/O timelines and Flamegraphs",
                                    ),
                                ] {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!("{n}."))
                                                .strong()
                                                .size(12.0)
                                                .color(pal.accent),
                                        );
                                        ui.label(egui::RichText::new(step).size(12.0));
                                    });
                                }
                                ui.add_space(8.0);
                                ui.weak(
                                    egui::RichText::new(
                                        "Shortcuts: ? Help · D Diagnostics · C Compare · R Refresh · Esc Back",
                                    )
                                    .size(11.0),
                                );
                            });
                        return;
                    }

                    let max_samples = rows.iter().map(|r| r.samples).max().unwrap_or(1).max(1);
                    egui::ScrollArea::vertical()
                        .id_source("sessions-scroll")
                        .max_height(avail_h - 36.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for row in rows.iter() {
                                let (dot, label) = state_color(&row.state);
                                let is_live =
                                    row.state == "RECORDING" || row.state == "STARTING";
                                let border_col = if is_live { pal.success } else { pal.card_stroke };
                                egui::Frame::default()
                                    .fill(pal.card_fill)
                                    .stroke(egui::Stroke::new(1.0_f32, border_col))
                                    .rounding(8.0)
                                    .inner_margin(egui::Margin::symmetric(14.0, 9.0))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            paint_status_dot(ui, dot, is_live);
                                            ui.vertical(|ui| {
                                                ui.horizontal(|ui| {
                                                    ui.strong(
                                                        egui::RichText::new(row.image.as_str())
                                                            .size(13.5),
                                                    );
                                                    // Status badge text
                                                    ui.label(
                                                        egui::RichText::new(if is_live {
                                                            "LIVE RECORDING"
                                                        } else {
                                                            label
                                                        })
                                                        .size(10.0)
                                                        .color(dot)
                                                        .strong(),
                                                    );
                                                    ui.weak(
                                                        egui::RichText::new(format!(
                                                            "· {} samples",
                                                            fmt_int(row.samples)
                                                        ))
                                                        .size(11.5),
                                                    );
                                                    if let Some(reason) = &row.stop_reason {
                                                        ui.weak(
                                                            egui::RichText::new(format!(
                                                                "· stopped: {reason}"
                                                            ))
                                                            .size(11.0),
                                                        );
                                                    }
                                                });
                                                let full_id = row.id.as_hyphenated().to_string();
                                                let short_id = &full_id[..full_id.len().min(8)];
                                                ui.horizontal(|ui| {
                                                    ui.label(
                                                        egui::RichText::new(format!(
                                                            "ID {short_id} · {}",
                                                            relative_time(row.id.unix_ms()),
                                                        ))
                                                        .size(10.5)
                                                        .color(pal.text_muted),
                                                    )
                                                    .on_hover_text(format!(
                                                        "Full Session ID: {full_id}"
                                                    ));
                                                    // Mini sample volume bar
                                                    let (bar_rect, _) = ui.allocate_exact_size(
                                                        egui::vec2(90.0, 5.0),
                                                        egui::Sense::hover(),
                                                    );
                                                    ui.painter().rect_filled(
                                                        bar_rect,
                                                        2.5,
                                                        pal.inset_fill,
                                                    );
                                                    let frac = (row.samples as f32
                                                        / max_samples as f32)
                                                        .clamp(0.04, 1.0);
                                                    let fill_rect = egui::Rect::from_min_size(
                                                        bar_rect.min,
                                                        egui::vec2(
                                                            bar_rect.width() * frac,
                                                            bar_rect.height(),
                                                        ),
                                                    );
                                                    ui.painter().rect_filled(fill_rect, 2.5, dot);
                                                });
                                            });
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if ui
                                                        .add(
                                                            egui::Button::new(
                                                                egui::RichText::new("Open ▶")
                                                                    .size(11.5)
                                                                    .strong(),
                                                            )
                                                            .min_size(egui::vec2(64.0, 22.0)),
                                                        )
                                                        .on_hover_text(
                                                            "Open session timeline & flamegraph",
                                                        )
                                                        .clicked()
                                                    {
                                                        self.open_dashboard(row.id);
                                                    }
                                                    if is_live {
                                                        if ui
                                                            .button(
                                                                egui::RichText::new("■ Stop")
                                                                    .color(pal.danger)
                                                                    .size(11.0)
                                                                    .strong(),
                                                            )
                                                            .on_hover_text(
                                                                "Stop recording this session",
                                                            )
                                                            .clicked()
                                                        {
                                                            let client =
                                                                std::sync::Arc::clone(&self.client);
                                                            let sid = row.id;
                                                            std::thread::spawn(move || {
                                                                let _ = client.stop(sid);
                                                            });
                                                            self.set_toast("Sent stop request");
                                                            self.home.request_refresh_now(
                                                                std::sync::Arc::clone(&self.client),
                                                            );
                                                        }
                                                    } else if ui
                                                        .small_button("⚖ Compare")
                                                        .on_hover_text(
                                                            "Set as Compare baseline & open comparison",
                                                        )
                                                        .clicked()
                                                    {
                                                        self.compare.baseline = Some(row.id);
                                                        self.show_compare = true;
                                                        self.set_toast(format!(
                                                            "Set {} as baseline",
                                                            row.image
                                                        ));
                                                    }
                                                    let full_id =
                                                        row.id.as_hyphenated().to_string();
                                                    if ui
                                                        .small_button("📋")
                                                        .on_hover_text("Copy session ID")
                                                        .clicked()
                                                    {
                                                        ui.output_mut(|o| o.copied_text = full_id);
                                                        self.set_toast("Copied session ID");
                                                    }
                                                    if ui
                                                        .small_button("🗑")
                                                        .on_hover_text(
                                                            "Delete this recording permanently [asks for confirmation]",
                                                        )
                                                        .clicked()
                                                    {
                                                        delete_request =
                                                            Some((row.id, row.image.clone()));
                                                    }
                                                },
                                            );
                                        });
                                    });
                                ui.add_space(4.0);
                            }
                        });
                    if let Some((id, image)) = delete_request {
                        self.pending_delete = Some((id, image));
                    }
                },
            );
        });
    }

    /// Confirmation modal for the destructive session delete. The delete
    /// itself runs off the UI thread and its typed outcome lands through
    /// `HomeModel::apply_delete_result`.
    fn render_delete_confirm(&mut self, ctx: &egui::Context) {
        let Some((id, image)) = self.pending_delete.clone() else {
            return;
        };
        let deleting = self.home.deleting == Some(id);
        let mut open = true;
        egui::Window::new("Delete Recording?")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(430.0)
            .show(ctx, |ui| {
                let pal = self.active_theme.palette();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("⚠").size(22.0).color(pal.warning));
                    ui.vertical(|ui| {
                        ui.strong(egui::RichText::new(format!(
                            "Permanently delete \"{image}\"?"
                        )));
                        ui.weak(
                            "This removes the session record and its stored samples from disk. There is no undo.",
                        );
                    });
                });
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if deleting {
                        ui.weak("deleting…");
                    } else if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("🗑 Delete permanently")
                                    .color(pal.danger)
                                    .strong(),
                            ),
                        )
                        .clicked()
                    {
                        self.home.request_delete_async(
                            std::sync::Arc::clone(&self.client),
                            id,
                        );
                        self.pending_delete = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.pending_delete = None;
                    }
                });
            });
        if !open {
            self.pending_delete = None;
        }
    }

    #[allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn dashboard_ui(&mut self, ui: &mut egui::Ui) {
        let dir = self.dashboard.as_ref().and_then(|d| d.session_dir.clone());
        self.stacks.maybe_reload(dir.as_deref());
        let Some(dash) = self.dashboard.as_ref() else {
            ui.label("no session");
            return;
        };
        let pal = self.active_theme.palette();
        for w in &dash.warnings {
            ui.colored_label(pal.warning, format!("⚠ {w}"));
        }

        // ---- Full-Width 5-Column KPI Summary Strip ----
        let cpu_val = dash.cpu_core_pct.points.last().map(|p| p.1).unwrap_or(0.0);
        let rss_val = dash
            .resident_bytes
            .points
            .last()
            .map(|p| p.1)
            .unwrap_or(0.0);
        let io_val = dash.io_bytes.points.last().map(|p| p.1).unwrap_or(0.0);
        let (st_color, _) = state_color(&dash.state);
        let state_sub = if dash.zoomed {
            "Zoomed window"
        } else {
            "Full timeline"
        };
        let time_span_sub = dash
            .data_end_ns
            .and_then(|e| {
                dash.data_start_ns
                    .map(|s| format!("window {}", fmt_ns(e.saturating_sub(s))))
            })
            .unwrap_or_else(|| "live stream".to_string());
        let cpu_sub = format!(
            "avg {:.1}% · peak {:.1}%",
            dash.cpu_core_pct.mean().unwrap_or(0.0),
            dash.cpu_core_pct.peak().unwrap_or(0.0)
        );
        let rss_sub = format!(
            "min {} · peak {}",
            fmt_value(dash.resident_bytes.min.unwrap_or(0.0)),
            fmt_value(dash.resident_bytes.peak().unwrap_or(0.0))
        );
        let io_sub = format!(
            "{} samples · cumulative",
            fmt_int(dash.io_bytes.points.len() as u64)
        );

        ui.columns(5, |cols| {
            metric_card(
                &mut cols[0],
                "SESSION STATE",
                &dash.state,
                Some(state_sub),
                st_color,
                pal.card_fill,
                pal.card_stroke,
                pal.text_muted,
            );
            metric_card(
                &mut cols[1],
                "TOTAL SAMPLES",
                &fmt_int(dash.samples),
                Some(&time_span_sub),
                pal.text_strong,
                pal.card_fill,
                pal.card_stroke,
                pal.text_muted,
            );
            metric_card(
                &mut cols[2],
                "CPU UTILIZATION",
                &format!("{cpu_val:.1}%"),
                Some(&cpu_sub),
                pal.accent,
                pal.card_fill,
                pal.card_stroke,
                pal.text_muted,
            );
            metric_card(
                &mut cols[3],
                "RESIDENT MEMORY",
                &fmt_value(rss_val),
                Some(&rss_sub),
                pal.success,
                pal.card_fill,
                pal.card_stroke,
                pal.text_muted,
            );
            metric_card(
                &mut cols[4],
                "LOGICAL I/O",
                &fmt_value(io_val),
                Some(&io_sub),
                pal.warning,
                pal.card_fill,
                pal.card_stroke,
                pal.text_muted,
            );
        });
        ui.add_space(6.0);

        let mut hovered = dash.hovered_timestamp;
        let cpu_color = pal.accent;
        let rss_color = pal.success;
        let io_color = pal.warning;

        match self.dashboard_layout {
            DashboardLayout::SplitOverview => {
                // Primary full-width CPU chart (positioned so y=175 is right inside the plot)
                chart_with_header(
                    ui,
                    "CPU — % of one logical core",
                    &dash.cpu_core_pct,
                    86.0,
                    cpu_color,
                    &pal,
                    &mut hovered,
                );
                ui.add_space(6.0);

                // Secondary 2-column charts: Resident Memory (left) & I/O Transfer (right)
                let avail_w = ui.available_width();
                let col_w = ((avail_w - 8.0) * 0.5).max(120.0);
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(col_w, 108.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            chart_with_header(
                                ui,
                                "Resident Memory — working set",
                                &dash.resident_bytes,
                                86.0,
                                rss_color,
                                &pal,
                                &mut hovered,
                            );
                        },
                    );
                    ui.allocate_ui_with_layout(
                        egui::vec2(col_w, 108.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            chart_with_header(
                                ui,
                                "I/O Transfer — cumulative logical",
                                &dash.io_bytes,
                                86.0,
                                io_color,
                                &pal,
                                &mut hovered,
                            );
                        },
                    );
                });
                if let Some(d) = self.dashboard.as_mut() {
                    d.hovered_timestamp = hovered;
                }

                ui.add_space(6.0);
                // Framed CPU Stacks & Flamegraph Studio Card
                egui::Frame::default()
                    .fill(pal.card_fill)
                    .stroke(egui::Stroke::new(1.0_f32, pal.card_stroke))
                    .rounding(8.0)
                    .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        section_header_colored(
                            ui,
                            "CPU Call Stacks — Flamegraph & Hotspots",
                            pal.accent,
                        );
                        ui.add_space(3.0);
                        self.stacks.ui(ui);
                    });
            }
            DashboardLayout::TimelineFocus => {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        chart_with_header(
                            ui,
                            "CPU — % of one logical core",
                            &dash.cpu_core_pct,
                            155.0,
                            cpu_color,
                            &pal,
                            &mut hovered,
                        );
                        ui.add_space(8.0);
                        chart_with_header(
                            ui,
                            "Resident Memory — working set",
                            &dash.resident_bytes,
                            155.0,
                            rss_color,
                            &pal,
                            &mut hovered,
                        );
                        ui.add_space(8.0);
                        chart_with_header(
                            ui,
                            "I/O Transfer — cumulative logical",
                            &dash.io_bytes,
                            155.0,
                            io_color,
                            &pal,
                            &mut hovered,
                        );
                    });
                if let Some(d) = self.dashboard.as_mut() {
                    d.hovered_timestamp = hovered;
                }
            }
            DashboardLayout::StacksFocus => {
                egui::Frame::default()
                    .fill(pal.card_fill)
                    .stroke(egui::Stroke::new(1.0_f32, pal.card_stroke))
                    .rounding(8.0)
                    .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        section_header_colored(
                            ui,
                            "CPU Call Stacks — Maximized Inspector",
                            pal.accent,
                        );
                        ui.add_space(4.0);
                        self.stacks.ui(ui);
                    });
            }
        }
    }
}

/// PF-709: UI theme. Dark is the profiler default; Light and System follow
/// the OS preference. Color is never the only state channel (see drawer).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Dark,
    Light,
    System,
}

impl Theme {
    /// Resolve the `System` preference against the OS dark/light setting
    /// (Windows personalization toggle / Linux desktop color-scheme),
    /// cached briefly so per-frame calls cost nothing. Unknown resolves to
    /// dark — the profiler default — never a silent guess.
    pub fn resolve(self, ctx: &egui::Context) -> Theme {
        match self {
            Theme::System => {
                if os_prefers_light() {
                    Theme::Light
                } else {
                    Theme::Dark
                }
            }
            other => {
                let _ = ctx; // reserved: egui <0.28 hides the window theme
                other
            }
        }
    }

    pub fn apply(self, ctx: &egui::Context) {
        // Per-theme palettes. Ensures both Dark and Light modes have crisp
        // contrast, including `widgets.active.fg_stroke` (used by `ui.strong()`).
        let visuals = match self {
            Theme::Dark | Theme::System => {
                let mut v = egui::Visuals::dark();
                v.panel_fill = egui::Color32::from_rgb(0x0d, 0x11, 0x17);
                v.window_fill = egui::Color32::from_rgb(0x16, 0x1b, 0x22);
                v.extreme_bg_color = egui::Color32::from_rgb(0x09, 0x0c, 0x10);
                v.faint_bg_color = egui::Color32::from_rgb(0x1a, 0x21, 0x2e);
                v.hyperlink_color = egui::Color32::from_rgb(0x58, 0xa6, 0xff);
                v.selection.bg_fill = egui::Color32::from_rgb(0x1f, 0x6f, 0xeb);
                v.selection.stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0xf0, 0xf6, 0xfc));
                v.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(0x0d, 0x11, 0x17);
                v.widgets.noninteractive.fg_stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0xc9, 0xd1, 0xd9));
                v.widgets.noninteractive.weak_bg_fill = egui::Color32::from_rgb(0x16, 0x1b, 0x22);
                v.widgets.noninteractive.bg_stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0x30, 0x36, 0x3d));
                v.widgets.inactive.bg_fill = egui::Color32::from_rgb(0x1c, 0x22, 0x2b);
                v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(0x1c, 0x22, 0x2b);
                v.widgets.inactive.bg_stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0x30, 0x36, 0x3d));
                v.widgets.inactive.fg_stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0xc9, 0xd1, 0xd9));
                v.widgets.hovered.bg_fill = egui::Color32::from_rgb(0x26, 0x2e, 0x3b);
                v.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(0x26, 0x2e, 0x3b);
                v.widgets.hovered.bg_stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0x58, 0xa6, 0xff));
                v.widgets.hovered.fg_stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0xf0, 0xf6, 0xfc));
                v.widgets.active.bg_fill = egui::Color32::from_rgb(0x1f, 0x6f, 0xeb);
                v.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, egui::Color32::WHITE);
                v.widgets.open.bg_fill = egui::Color32::from_rgb(0x16, 0x1b, 0x22);
                v
            }
            Theme::Light => {
                // High-contrast Studio Light: soft slate canvas, crisp white cards,
                // deep slate typography (including `active.fg_stroke` for `ui.strong()`).
                let mut v = egui::Visuals::light();
                v.panel_fill = egui::Color32::from_rgb(0xf3, 0xf5, 0xf8);
                v.window_fill = egui::Color32::from_rgb(0xff, 0xff, 0xff);
                v.extreme_bg_color = egui::Color32::from_rgb(0xff, 0xff, 0xff);
                v.faint_bg_color = egui::Color32::from_rgb(0xea, 0xee, 0xf4);
                v.hyperlink_color = egui::Color32::from_rgb(0x09, 0x69, 0xda);
                v.selection.bg_fill = egui::Color32::from_rgb(0xdb, 0xea, 0xfe);
                v.selection.stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0x05, 0x50, 0xae));
                v.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(0xff, 0xff, 0xff);
                v.widgets.noninteractive.fg_stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0x1f, 0x23, 0x28));
                v.widgets.noninteractive.weak_bg_fill = egui::Color32::from_rgb(0xf6, 0xf8, 0xfa);
                v.widgets.noninteractive.bg_stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0xd0, 0xd7, 0xde));
                v.widgets.inactive.bg_fill = egui::Color32::from_rgb(0xea, 0xee, 0xf2);
                v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(0xea, 0xee, 0xf2);
                v.widgets.inactive.bg_stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0xc8, 0xd1, 0xda));
                v.widgets.inactive.fg_stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0x24, 0x29, 0x2f));
                v.widgets.hovered.bg_fill = egui::Color32::from_rgb(0xdc, 0xe4, 0xec);
                v.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(0xdc, 0xe4, 0xec);
                v.widgets.hovered.bg_stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0x09, 0x69, 0xda));
                v.widgets.hovered.fg_stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0x0f, 0x17, 0x2a));
                v.widgets.active.bg_fill = egui::Color32::from_rgb(0xdb, 0xea, 0xfe);
                v.widgets.active.fg_stroke =
                    egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(0x0f, 0x17, 0x2a));
                v.widgets.open.bg_fill = egui::Color32::from_rgb(0xea, 0xee, 0xf2);
                v
            }
        };
        ctx.style_mut(|style| {
            style.spacing.item_spacing = egui::vec2(7.0, 5.0);
            style.spacing.button_padding = egui::vec2(8.0, 4.0);
        });
        ctx.set_visuals(visuals);
    }

    /// UI-facing semantic colors for cards, charts, badges, and surfaces.
    pub fn palette(self) -> Palette {
        match self {
            Theme::Light => Palette {
                card_fill: egui::Color32::from_rgb(0xff, 0xff, 0xff),
                surface_elevated: egui::Color32::from_rgb(0xf6, 0xf8, 0xfa),
                card_stroke: egui::Color32::from_rgb(0xd0, 0xd7, 0xde),
                inset_fill: egui::Color32::from_rgb(0xf8, 0xfa, 0xfc),
                grid_line: egui::Color32::from_rgb(0xe2, 0xe8, 0xf0),
                text_strong: egui::Color32::from_rgb(0x0f, 0x17, 0x2a),
                text_muted: egui::Color32::from_rgb(0x57, 0x60, 0x6a),
                accent: egui::Color32::from_rgb(0x09, 0x69, 0xda),
                success: egui::Color32::from_rgb(0x1a, 0x7f, 0x37),
                warning: egui::Color32::from_rgb(0x9a, 0x67, 0x00),
                danger: egui::Color32::from_rgb(0xcf, 0x22, 0x2e),
            },
            Theme::Dark | Theme::System => Palette {
                card_fill: egui::Color32::from_rgb(0x16, 0x1b, 0x22),
                surface_elevated: egui::Color32::from_rgb(0x1c, 0x22, 0x2b),
                card_stroke: egui::Color32::from_rgb(0x30, 0x36, 0x3d),
                inset_fill: egui::Color32::from_rgb(0x09, 0x0d, 0x12),
                grid_line: egui::Color32::from_rgb(0x1e, 0x26, 0x31),
                text_strong: egui::Color32::from_rgb(0xe6, 0xed, 0xf3),
                text_muted: egui::Color32::from_rgb(0x8b, 0x94, 0x9e),
                accent: egui::Color32::from_rgb(0x58, 0xa6, 0xff),
                success: egui::Color32::from_rgb(0x3f, 0xb9, 0x50),
                warning: egui::Color32::from_rgb(0xd2, 0x99, 0x22),
                danger: egui::Color32::from_rgb(0xf8, 0x51, 0x49),
            },
        }
    }

    pub fn cycle(&mut self) -> Theme {
        *self = match self {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::System,
            Theme::System => Theme::Dark,
        };
        *self
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
            Theme::System => "system",
        }
    }

    /// Inverse of `as_str` for the settings file; unknown names are None.
    pub fn parse_theme(s: &str) -> Option<Theme> {
        match s {
            "dark" => Some(Theme::Dark),
            "light" => Some(Theme::Light),
            "system" => Some(Theme::System),
            _ => None,
        }
    }
}

/// PF-709: single-key shortcuts only fire when no text field has focus.
fn ui_has_no_text_focus() -> bool {
    TEXT_FOCUS_OK.load(std::sync::atomic::Ordering::Relaxed)
}

static TEXT_FOCUS_OK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

// ---------------------------------------------------------------------------
// Theme: OS preference query + persistence
// ---------------------------------------------------------------------------

/// Cached OS light/dark preference (refreshed at most every 5 s; theme
/// changes are rare and the query must stay off the per-frame hot path).
fn os_prefers_light() -> bool {
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, bool)>> =
        std::sync::Mutex::new(None);
    let mut guard = match CACHE.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    if let Some((at, light)) = *guard {
        if at.elapsed() < std::time::Duration::from_secs(5) {
            return light;
        }
    }
    let light = query_os_prefers_light().unwrap_or(false);
    *guard = Some((std::time::Instant::now(), light));
    light
}

/// Best-effort OS light-theme query. `None` (unreadable) resolves to the
/// dark default at the call site — never a guess.
fn query_os_prefers_light() -> Option<bool> {
    #[cfg(windows)]
    {
        pf_windows::apps_use_light_theme()
    }
    #[cfg(not(windows))]
    {
        // GNOME color-scheme is the one broadly scriptable source; any
        // failure (KDE, bare WM) falls back to the dark default.
        let out = std::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", "color-scheme"])
            .output()
            .ok()?;
        Some(String::from_utf8_lossy(&out.stdout).contains("light"))
    }
}

const GUI_SETTINGS_FILE: &str = "gui-settings.json";

/// Load the persisted theme preference from `<data_root>/gui-settings.json`.
/// Missing, unreadable or malformed files resolve to `None` (the caller
/// applies the Dark default) — a broken settings file must never block
/// startup.
fn load_theme_preference() -> Option<Theme> {
    let root = pf_platform::paths::data_root().ok()?;
    load_theme_preference_at(&root.join(GUI_SETTINGS_FILE))
}

fn load_theme_preference_at(path: &std::path::Path) -> Option<Theme> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.len() > 64 * 1024 {
        return None;
    }
    let value = pf_types::json::parse(&text).ok()?;
    Theme::parse_theme(value.get("theme")?.as_str()?)
}

/// Persist the theme preference next to the session data. A failed write
/// only skips persistence (read-only data root); the in-session change
/// already applied.
fn save_theme_preference(theme: Theme) {
    let Ok(root) = pf_platform::paths::data_root() else {
        return;
    };
    save_theme_preference_at(&root.join(GUI_SETTINGS_FILE), theme);
}

fn save_theme_preference_at(path: &std::path::Path, theme: Theme) {
    let doc = pf_types::json::JsonValue::Object(vec![
        (
            "schema".into(),
            pf_types::json::JsonValue::Str("pulseforge.gui-settings/0".into()),
        ),
        (
            "theme".into(),
            pf_types::json::JsonValue::Str(theme.as_str().into()),
        ),
    ]);
    let _ = std::fs::write(path, doc.to_json_string());
}

/// Renders a chart header bar (colored accent bar + title on the left,
/// `min · avg · max · window` summary stats on the right) followed by the
/// gradient area timeline chart.
fn chart_with_header(
    ui: &mut egui::Ui,
    title: &str,
    series: &Series,
    height: f32,
    accent: Color32,
    pal: &Palette,
    hovered_ts: &mut Option<u64>,
) {
    let t0 = series.points.first().map(|p| p.0).unwrap_or(0);
    let t1 = series.points.last().map(|p| p.0).unwrap_or(t0);
    let span_ns = t1.saturating_sub(t0);
    let vmin = series.min.unwrap_or(0.0);
    let vmax = series.max.unwrap_or(0.0);
    let vavg = series.mean().unwrap_or(0.0);

    ui.horizontal(|ui| {
        let (bar_rect, _) = ui.allocate_exact_size(egui::vec2(3.0, 13.0), egui::Sense::hover());
        ui.painter().rect_filled(bar_rect, 1.5, accent);
        ui.label(
            egui::RichText::new(title)
                .size(12.0)
                .strong()
                .color(pal.text_strong),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !series.points.is_empty() {
                let unit_suffix = fmt_unit_suffix(series.unit);
                ui.label(
                    egui::RichText::new(format!(
                        "min {}{} · avg {}{} · max {}{} · {}",
                        fmt_value(vmin),
                        unit_suffix,
                        fmt_value(vavg),
                        unit_suffix,
                        fmt_value(vmax),
                        unit_suffix,
                        fmt_ns(span_ns)
                    ))
                    .size(10.5)
                    .color(pal.text_muted),
                );
            }
        });
    });
    chart_colored(ui, series, height, accent, pal, hovered_ts);
}

/// Studio-grade timeline chart with vertical gradient area fill, dedicated
/// non-overlapping bottom time-axis strip, semantic color coding, and
/// synchronized crosshairs across all charts.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn chart_colored(
    ui: &mut egui::Ui,
    series: &Series,
    height: f32,
    accent: Color32,
    pal: &Palette,
    hovered_ts: &mut Option<u64>,
) {
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), height),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, pal.inset_fill);
    painter.rect_stroke(rect, 6.0, Stroke::new(1.0_f32, pal.card_stroke));
    if series.points.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("no data ({})", series.name),
            egui::FontId::proportional(11.0),
            pal.text_muted,
        );
        return;
    }

    // Reserve top padding and a dedicated 15px bottom time-axis strip so
    // the waveform never collides with time labels.
    let plot_top = rect.top() + 8.0;
    let plot_bottom = (rect.bottom() - 15.0).max(plot_top + 12.0);
    let plot_h = (plot_bottom - plot_top).max(1.0);

    let max_points = (rect.width() as usize * 2).max(64);
    let stride = series.points.len().div_ceil(max_points).max(1);
    let t0 = series.points.first().map(|p| p.0).unwrap_or(0);
    let t1 = series.points.last().map(|p| p.0).unwrap_or(1).max(t0 + 1);
    let vmin = series.min.unwrap_or(0.0);
    let vmax = series.max.unwrap_or(1.0).max(vmin + 1.0);
    let to_screen = |t: u64, v: f64| -> Pos2 {
        let x = rect.left() + (t.saturating_sub(t0) as f32 / (t1 - t0) as f32) * rect.width();
        let norm_y = ((v - vmin) as f32 / (vmax - vmin) as f32).clamp(0.0, 1.0);
        let y = plot_bottom - norm_y * plot_h;
        Pos2::new(x, y)
    };

    // Subtle separator line for the bottom time-axis strip
    painter.line_segment(
        [
            Pos2::new(rect.left() + 1.0, plot_bottom),
            Pos2::new(rect.right() - 1.0, plot_bottom),
        ],
        Stroke::new(0.75_f32, pal.grid_line),
    );

    // Horizontal gridlines (25%, 50%, 75%) inside plot area
    for frac in [0.25_f32, 0.5, 0.75] {
        let y = plot_bottom - plot_h * frac;
        painter.line_segment(
            [
                Pos2::new(rect.left() + 1.0, y),
                Pos2::new(rect.right() - 1.0, y),
            ],
            Stroke::new(0.5_f32, pal.grid_line),
        );
    }

    // Vertical time gridlines + bottom time-axis labels
    let span_ns = t1.saturating_sub(t0);
    for frac in [0.25_f32, 0.5, 0.75] {
        let x = rect.left() + rect.width() * frac;
        painter.line_segment(
            [Pos2::new(x, rect.top() + 1.0), Pos2::new(x, plot_bottom)],
            Stroke::new(0.5_f32, pal.grid_line),
        );
        let t = span_ns as f64 * f64::from(frac);
        painter.text(
            Pos2::new(x, rect.bottom() - 2.0),
            egui::Align2::CENTER_BOTTOM,
            format!("+{}", fmt_ns(t as u64)),
            egui::FontId::proportional(9.0),
            pal.text_muted,
        );
    }
    painter.text(
        Pos2::new(rect.left() + 6.0, rect.bottom() - 2.0),
        egui::Align2::LEFT_BOTTOM,
        "+0ms",
        egui::FontId::proportional(9.0),
        pal.text_muted,
    );
    painter.text(
        Pos2::new(rect.right() - 6.0, rect.bottom() - 2.0),
        egui::Align2::RIGHT_BOTTOM,
        format!("+{}", fmt_ns(span_ns)),
        egui::FontId::proportional(9.0),
        pal.text_muted,
    );

    let pts: Vec<Pos2> = series
        .points
        .iter()
        .step_by(stride)
        .map(|(t, v)| to_screen(*t, *v))
        .collect();

    // Vertical gradient quad-strip fill under the polyline
    if pts.len() >= 2 {
        let mut fill = egui::Mesh::default();
        let [r, g, b, _] = accent.to_array();
        let top_col = Color32::from_rgba_unmultiplied(r, g, b, 52);
        let bot_col = Color32::from_rgba_unmultiplied(r, g, b, 4);
        for w in pts.windows(2) {
            let (a, b_pt) = (w[0], w[1]);
            let i0 = fill.vertices.len() as u32;
            fill.vertices.push(egui::epaint::Vertex {
                pos: a,
                uv: egui::epaint::WHITE_UV,
                color: top_col,
            });
            fill.vertices.push(egui::epaint::Vertex {
                pos: b_pt,
                uv: egui::epaint::WHITE_UV,
                color: top_col,
            });
            fill.vertices.push(egui::epaint::Vertex {
                pos: egui::Pos2::new(b_pt.x, plot_bottom),
                uv: egui::epaint::WHITE_UV,
                color: bot_col,
            });
            fill.vertices.push(egui::epaint::Vertex {
                pos: egui::Pos2::new(a.x, plot_bottom),
                uv: egui::epaint::WHITE_UV,
                color: bot_col,
            });
            fill.indices
                .extend_from_slice(&[i0, i0 + 1, i0 + 2, i0, i0 + 2, i0 + 3]);
        }
        painter.add(egui::Shape::Mesh(fill));
    }

    // Crisp waveform polyline
    painter.add(egui::Shape::line(pts, Stroke::new(1.75_f32, accent)));

    // Y-axis reference labels drawn over the fill on the left
    for frac in [0.25_f32, 0.75] {
        let y = plot_bottom - plot_h * frac;
        let val = vmin + (vmax - vmin) * f64::from(frac);
        painter.text(
            Pos2::new(rect.left() + 5.0, y - 1.0),
            egui::Align2::LEFT_BOTTOM,
            fmt_value(val),
            egui::FontId::proportional(9.0),
            pal.text_muted,
        );
    }

    // Synchronized crosshair & hover detection across all timeline charts
    // Expand vertical hit box by 18px upwards to include the chart's header strip.
    let hover_zone = egui::Rect::from_min_max(Pos2::new(rect.left(), rect.top() - 18.0), rect.max);
    let pointer_in_chart = ui
        .ctx()
        .pointer_latest_pos()
        .map(|pos| hover_zone.contains(pos))
        .unwrap_or(false);
    if (resp.hovered() || pointer_in_chart) && span_ns > 0 {
        if let Some(pos) = ui.ctx().pointer_latest_pos() {
            let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            let t_mouse = t0 + (span_ns as f64 * f64::from(frac)) as u64;
            *hovered_ts = Some(t_mouse);
        }
    }

    if let Some(sync_t) = *hovered_ts {
        if sync_t >= t0 && sync_t <= t1 && span_ns > 0 {
            let frac = (sync_t.saturating_sub(t0) as f32) / (span_ns as f32);
            let crosshair_x = rect.left() + frac * rect.width();
            let [r, g, b, _] = accent.to_array();

            // Vertical crosshair guide line
            painter.line_segment(
                [
                    Pos2::new(crosshair_x, rect.top() + 1.0),
                    Pos2::new(crosshair_x, plot_bottom),
                ],
                Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(r, g, b, 175)),
            );

            // Nearest sample point to sync_t
            let mut best: Option<(u64, f64)> = None;
            let mut best_dt = u64::MAX;
            for (t, v) in &series.points {
                let dt = t.abs_diff(sync_t);
                if dt < best_dt {
                    best_dt = dt;
                    best = Some((*t, *v));
                }
            }

            if let Some((t_sample, v_sample)) = best {
                let pt = to_screen(t_sample, v_sample);
                painter.circle_filled(pt, 6.0, Color32::from_rgba_unmultiplied(r, g, b, 45));
                painter.circle_filled(pt, 3.5, accent);
                painter.circle_stroke(pt, 4.2, Stroke::new(1.2_f32, Color32::WHITE));

                let rel = fmt_ns(t_sample.saturating_sub(t0));
                let unit_suffix = fmt_unit_suffix(series.unit);
                let is_direct_hover = resp.hovered() || pointer_in_chart;
                let label = if is_direct_hover {
                    format!(
                        "t+{rel}  ·  {}  ·  {}{}",
                        series.name,
                        fmt_value(v_sample),
                        unit_suffix
                    )
                } else {
                    format!("t+{rel} · {}{}", fmt_value(v_sample), unit_suffix)
                };
                let font_size = if is_direct_hover { 10.5 } else { 9.5 };
                let galley = ui.painter().layout_no_wrap(
                    label,
                    egui::FontId::proportional(font_size),
                    pal.text_strong,
                );
                let size = galley.size() + Vec2::new(14.0, 8.0);
                let prefer_right = crosshair_x + 10.0 + size.x <= rect.right() - 4.0;
                let box_x = if prefer_right {
                    crosshair_x + 10.0
                } else {
                    (crosshair_x - size.x - 10.0).max(rect.left() + 4.0)
                };
                let box_y = (pt.y - size.y - 6.0).clamp(
                    rect.top() + 4.0,
                    (plot_bottom - size.y - 2.0).max(rect.top() + 4.0),
                );
                let box_pos = Pos2::new(box_x, box_y);
                let box_rect = egui::Rect::from_min_size(box_pos, size);
                painter.rect_filled(box_rect, 5.0, pal.card_fill);
                painter.rect_stroke(box_rect, 5.0, Stroke::new(1.0_f32, accent));
                painter.galley(box_pos + Vec2::new(7.0, 4.0), galley, pal.text_strong);
            }
        }
    }
}

/// Custom-painted status indicator dot (replaces Unicode `●` which is missing
/// from egui's default font and otherwise renders as a hollow tofu box `□`).
fn paint_status_dot(ui: &mut egui::Ui, color: Color32, glowing: bool) {
    let size = if glowing { 10.0 } else { 8.0 };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let center = rect.center();
    if glowing {
        let [r, g, b, _] = color.to_array();
        ui.painter()
            .circle_filled(center, 4.8, Color32::from_rgba_unmultiplied(r, g, b, 65));
    }
    ui.painter().circle_filled(center, 3.2, color);
}

/// Vector caret (pointing right when collapsed, pointing down when expanded).
/// Replaces Unicode arrow glyphs with zero-tofu, pixel-crisp vector triangles.
fn paint_caret(ui: &mut egui::Ui, expanded: bool, color: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(12.0, 14.0), egui::Sense::click());
    let center = rect.center();
    let painter = ui.painter();
    if expanded {
        // Downward triangle
        let p1 = Pos2::new(center.x - 3.5, center.y - 2.0);
        let p2 = Pos2::new(center.x + 3.5, center.y - 2.0);
        let p3 = Pos2::new(center.x, center.y + 2.5);
        painter.add(egui::Shape::convex_polygon(
            vec![p1, p2, p3],
            color,
            egui::Stroke::NONE,
        ));
    } else {
        // Rightward triangle
        let p1 = Pos2::new(center.x - 2.0, center.y - 3.5);
        let p2 = Pos2::new(center.x - 2.0, center.y + 3.5);
        let p3 = Pos2::new(center.x + 2.5, center.y);
        painter.add(egui::Shape::convex_polygon(
            vec![p1, p2, p3],
            color,
            egui::Stroke::NONE,
        ));
    }
    resp
}

/// Relative time from a Unix-ms timestamp: "just now", "N min ago",
/// "N h ago", or "N d ago" for real timestamps, or "recorded" for
/// synthetic test UUIDs whose timestamp component is zero.
fn relative_time(unix_ms: u64) -> String {
    // Timestamps prior to year 2000 (946_684_800_000 ms) come from synthetic
    // test SessionIds (e.g. `00000000-0000-7000-...`).
    if unix_ms < 946_684_800_000 {
        return "recorded".to_string();
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(unix_ms);
    let age_s = now_ms.saturating_sub(unix_ms) / 1000;
    if age_s < 60 {
        "just now".to_string()
    } else if age_s < 3600 {
        format!("{} min ago", age_s / 60)
    } else if age_s < 86_400 {
        format!("{} h ago", age_s / 3600)
    } else {
        format!("{} d ago", age_s / 86_400)
    }
}

/// State → (dot color, label) for session cards and badges.
fn state_color(state: &str) -> (Color32, &'static str) {
    match state {
        "RECORDING" | "STARTING" => (Color32::from_rgb(0x3f, 0xb9, 0x50), "recording"),
        "COMPLETED" => (Color32::from_rgb(0x58, 0xa6, 0xff), "completed"),
        "OFFLINE" => (Color32::from_rgb(0x58, 0xa6, 0xff), "offline"),
        "FAILED" | "CANCELLED" => (Color32::from_rgb(0xf8, 0x51, 0x49), "failed"),
        "RECOVERED_PARTIAL" => (Color32::from_rgb(0xd2, 0x99, 0x22), "recovered"),
        _ => (Color32::from_rgb(0x8b, 0x94, 0x9e), "unknown"),
    }
}

fn fmt_unit_suffix(unit: &str) -> &str {
    match unit {
        "bytes" => "",
        "percent" => "%",
        other => other,
    }
}

/// Section header with custom-colored vertical accent bar.
fn section_header_colored(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(3.5, 16.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 1.5, color);
        ui.strong(egui::RichText::new(text).size(13.0));
    });
}

/// Full-width KPI summary card with top accent indicator and optional subtitle.
#[allow(clippy::too_many_arguments)]
fn metric_card(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    subtitle: Option<&str>,
    color: egui::Color32,
    card_fill: egui::Color32,
    card_stroke: egui::Color32,
    text_muted: egui::Color32,
) {
    egui::Frame::default()
        .fill(card_fill)
        .stroke(egui::Stroke::new(1.0_f32, card_stroke))
        .rounding(7.0)
        .inner_margin(egui::Margin::symmetric(12.0, 5.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.horizontal(|ui| {
                    let (bar, _) =
                        ui.allocate_exact_size(egui::vec2(3.0, 10.0), egui::Sense::hover());
                    ui.painter().rect_filled(bar, 1.5, color);
                    ui.label(
                        egui::RichText::new(label)
                            .size(9.5)
                            .strong()
                            .color(text_muted),
                    );
                });
                ui.label(egui::RichText::new(value).size(17.0).strong().color(color));
                if let Some(sub) = subtitle {
                    ui.label(egui::RichText::new(sub).size(10.0).color(text_muted));
                }
            });
        });
}

/// Format an integer with comma thousands separators (e.g. `12,884`).
#[allow(clippy::arithmetic_side_effects)]
fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

/// Relative duration (ns) rendered compactly for the time axis.
fn fmt_ns(ns: u64) -> String {
    let ms = ns / 1_000_000;
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let total_s = ms / 1_000;
        format!("{}m{}s", total_s / 60, total_s % 60)
    }
}

fn fmt_value(v: f64) -> String {
    if v.abs() >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GiB", v / (1024.0 * 1024.0 * 1024.0))
    } else if v.abs() >= 1024.0 * 1024.0 {
        format!("{:.1} MiB", v / (1024.0 * 1024.0))
    } else if v.abs() >= 1024.0 {
        format!("{:.1} KiB", v / 1024.0)
    } else {
        format!("{v:.1}")
    }
}

/// Compact byte formatting for the picker's RAM column (MB/GB scale,
/// integer precision — this is a glanceable column, not a report).
fn fmt_bytes_compact(v: u64) -> String {
    let v = v as f64;
    if v >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GB", v / (1024.0 * 1024.0 * 1024.0))
    } else if v >= 1024.0 * 1024.0 {
        format!("{:.0} MB", v / (1024.0 * 1024.0))
    } else if v >= 1024.0 {
        format!("{:.0} KB", v / 1024.0)
    } else {
        format!("{v:.0} B")
    }
}

/// Live resource snapshot for a picker row: CPU% of one logical core
/// (colored by load: <50% muted, ≥50% amber, ≥85% red) and resident set
/// size. `None` renders as an em dash with an honest hover note (no
/// baseline yet / access denied) — absence never masquerades as zero.
fn paint_row_resources(
    ui: &mut egui::Ui,
    pal: &Palette,
    cpu_pct: Option<f64>,
    rss_bytes: Option<u64>,
) {
    match cpu_pct {
        Some(pct) => {
            let col = if pct >= 85.0 {
                pal.danger
            } else if pct >= 50.0 {
                pal.warning
            } else {
                pal.text_muted
            };
            ui.monospace(
                egui::RichText::new(format!("{pct:4.1}%"))
                    .size(10.0)
                    .strong()
                    .color(col),
            )
            .on_hover_text(format!(
                "CPU: {pct:.1}% of one logical core since the previous refresh"
            ));
        }
        None => {
            ui.monospace(
                egui::RichText::new("    —")
                    .size(10.0)
                    .color(pal.text_muted),
            )
            .on_hover_text("CPU not measured yet (first refresh) or access denied");
        }
    }
    match rss_bytes {
        Some(rss) => {
            ui.monospace(
                egui::RichText::new(fmt_bytes_compact(rss))
                    .size(10.0)
                    .color(pal.text_muted),
            )
            .on_hover_text(format!("Resident memory: {}", fmt_value(rss as f64)));
        }
        None => {
            ui.monospace(
                egui::RichText::new("    —")
                    .size(10.0)
                    .color(pal.text_muted),
            )
            .on_hover_text("Memory not reported by the OS for this process");
        }
    }
}

/// Open a folder in the OS file manager (Explorer on Windows, xdg-open on
/// Linux). Fire-and-forget: the file manager outlives the GUI process and
/// pops no console window.
fn reveal_folder(dir: &std::path::Path) -> Result<(), String> {
    if !dir.is_dir() {
        return Err(format!("folder does not exist: {}", dir.display()));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: spawning explorer must not flash a console.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("explorer.exe")
            .arg(dir)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression lock for the P0 "shortcuts fire while typing" bug: the
    // guard static must be re-derived from `wants_keyboard_input()` every
    // update() frame, not loaded from a static that nothing ever stores.
    #[test]
    fn text_focus_disables_single_key_shortcuts_next_frame() {
        let ctx = egui::Context::default();
        let mut buf = String::new();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                Pos2::ZERO,
                Vec2::new(800.0, 600.0),
            )),
            ..egui::RawInput::default()
        };

        // Frame 1: focus a TextEdit (in an Area — update() owns the
        // CentralPanel) so the NEXT frame reports keyboard use.
        let _ = ctx.run(input(), |ctx| {
            app_frame(ctx, &mut buf, None);
        });

        // Frame 2: focus is retained (the TextEdit re-renders); the app
        // must adopt the guarded state and NOT dispatch single keys.
        let mut app =
            PulseForgeApp::new(std::sync::Arc::new(crate::viewmod::FakeDaemon::default()));
        app.view = View::Home;
        let _ = ctx.run(input(), |ctx| app_frame(ctx, &mut buf, Some(&mut app)));

        assert!(ctx.wants_keyboard_input(), "TextEdit focus = keyboard use");
        assert!(
            !TEXT_FOCUS_OK.load(std::sync::atomic::Ordering::Relaxed),
            "the guard must be stored from wants_keyboard_input() each frame"
        );
        assert!(
            !app.show_diagnostics,
            "D key while typing must not open diagnostics"
        );

        // Frame 3: the user clicked the background (focus surrendered) and
        // pressed D — the shortcut must fire again. This is the full
        // end-to-end path: RawInput key event → update() → guard → on_key.
        let mut input = input();
        input.events.push(egui::Event::Key {
            key: egui::Key::D,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        });
        let _ = ctx.run(input, |ctx| {
            ctx.memory_mut(|mem| mem.stop_text_input());
            app.update(ctx);
        });
        assert!(app.show_diagnostics, "D key with no text focus toggles");
    }

    /// Render the app plus (optionally) a focused test TextEdit in an Area.
    fn app_frame(ctx: &egui::Context, buf: &mut String, app: Option<&mut PulseForgeApp>) {
        if let Some(app) = app {
            app.update(ctx);
        }
        egui::Area::new("test-focus-area".into())
            .fixed_pos(Pos2::ZERO)
            .show(ctx, |ui| {
                let resp = ui.add(egui::TextEdit::singleline(buf));
                resp.request_focus();
            });
    }

    #[test]
    fn error_toasts_carry_the_error_kind() {
        let mut app =
            PulseForgeApp::new(std::sync::Arc::new(crate::viewmod::FakeDaemon::default()));
        app.set_toast("ok");
        assert_eq!(app.toast.as_ref().expect("toast").2, ToastKind::Success);
        app.set_toast_error("Export error: boom");
        let (msg, _, kind) = app.toast.as_ref().expect("toast");
        assert_eq!(*kind, ToastKind::Error);
        assert!(msg.contains("boom"));
    }

    #[test]
    fn theme_settings_roundtrip_and_reject_garbage() {
        let dir = std::env::temp_dir().join(format!("pf-gui-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("gui-settings.json");

        assert_eq!(load_theme_preference_at(&path), None, "missing file");

        save_theme_preference_at(&path, Theme::System);
        assert_eq!(load_theme_preference_at(&path), Some(Theme::System));

        save_theme_preference_at(&path, Theme::Light);
        assert_eq!(load_theme_preference_at(&path), Some(Theme::Light));

        std::fs::write(&path, "not json at all").expect("garbage");
        assert_eq!(
            load_theme_preference_at(&path),
            None,
            "malformed settings must fall back, never block startup"
        );
        std::fs::write(&path, r#"{"theme":"plaid"}"#).expect("unknown theme");
        assert_eq!(load_theme_preference_at(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_never_returns_system() {
        let ctx = egui::Context::default();
        // The OS query may return either answer on this machine; the
        // contract under test is that System never paints as itself.
        assert_eq!(Theme::Dark.resolve(&ctx), Theme::Dark);
        assert_eq!(Theme::Light.resolve(&ctx), Theme::Light);
        assert_ne!(Theme::System.resolve(&ctx), Theme::System);
    }
}
